//! Bounded, lossless working sets for repeated MCP queries on a large calendar.
use super::actions;
use flate2::{Compression, bufread::DeflateDecoder, write::DeflateEncoder};
use godovoy_plan::{
    Fallible,
    model::{Id, Occurrence},
    storage::online::{CalendarFile, Data, same_calendar_bytes},
};
use std::{
    collections::{BTreeSet, VecDeque},
    io::{self, BufWriter, Read, Write},
};

// Leave one quarter of the existing source budget for indices, buffers and the decoder.
const RETAINED_BYTES: usize = super::conditional::MAX_SHARED_SOURCE * 3 / 4;
const MAX_SNAPSHOT: usize = RETAINED_BYTES - MAX_PROJECTION;
const MAX_PROJECTION: usize = super::conditional::MAX_SHARED_SOURCE / 16;
const WORKING_BYTES: usize = super::conditional::MAX_SHARED_SOURCE / 2;
const SLOTS: usize = 64;
const WARMUP: u8 = 4;
const _: () = assert!(MAX_SNAPSHOT + MAX_PROJECTION == RETAINED_BYTES);

#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg_attr(test, derive(Debug))]
pub(super) enum Query {
    Get {
        employee: Id,
        year: i32,
        event: Id,
    },
    List {
        employee: Id,
        year: i32,
        offset: usize,
        limit: usize,
    },
}

pub(super) fn assigned(data: &Data, employee: Id, year: i32) -> Vec<Occurrence<'_>> {
    actions::sorted_occurrences(data, year, Some(employee))
}

impl Query {
    fn restrict(self, data: &mut Data) -> bool {
        match self {
            Self::Get {
                employee,
                year,
                event,
            } => {
                let allowed = data
                    .calendar
                    .event(event, year)
                    .is_some_and(|e| actions::assigned_to(data, e, employee));
                if !allowed {
                    return false;
                }
                let Some(selected) = data.calendar.plans.remove(&event) else {
                    return false;
                };
                data.calendar.plans.clear();
                data.calendar.plans.insert(event, selected);
            }
            Self::List {
                employee,
                year,
                offset,
                limit,
            } => {
                // The ordinary handler still applies its original cursor, limit and lookahead.
                let count = offset.saturating_add(limit).saturating_add(1);
                let selected: BTreeSet<_> = assigned(data, employee, year)
                    .into_iter()
                    .take(count)
                    .map(|event| event.id)
                    .collect();
                data.calendar.plans.retain(|id, _| selected.contains(id));
            }
        }
        // Neither read tool returns the calendar's append-only audit history.
        data.audit.clear();
        true
    }
}

struct Bounded {
    bytes: Vec<u8>,
    limit: usize,
}
impl Bounded {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }
}
impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit - self.bytes.len() {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        self.bytes
            .try_reserve_exact(bytes.len())
            .map_err(io::Error::other)?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Snapshot {
    compressed: Box<[u8]>,
    projections: VecDeque<(Query, Box<[u8]>)>,
}
impl Snapshot {
    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let mut enc = DeflateEncoder::new(Bounded::new(MAX_SNAPSHOT), Compression::fast());
        enc.write_all(bytes).ok()?;
        let compressed = enc.finish().ok()?.bytes.into_boxed_slice();
        Some(Self {
            compressed,
            projections: VecDeque::new(),
        })
    }
    fn insert(&mut self, query: Query, working: Box<[u8]>) {
        let mut bytes = working.len()
            + self
                .projections
                .iter()
                .map(|(_, value)| value.len())
                .sum::<usize>();
        let budget = WORKING_BYTES.min(RETAINED_BYTES - self.compressed.len());
        while self.projections.len() >= SLOTS || bytes > budget {
            let Some((_, oldest)) = self.projections.pop_front() else {
                return;
            };
            bytes -= oldest.len();
        }
        self.projections.push_back((query, working));
    }

    fn matches(&self, bytes: &[u8]) -> bool {
        let mut input = DeflateDecoder::new(&*self.compressed);
        let mut chunk = [0; 16 * 1024];
        let mut offset = 0;
        while let Ok(n) = input.read(&mut chunk) {
            if n == 0 {
                return offset == bytes.len();
            }
            if bytes
                .get(offset..offset + n)
                .is_none_or(|current| !same_calendar_bytes(current, &chunk[..n]))
            {
                return false;
            }
            offset += n;
        }
        false
    }
}

#[derive(Default)]
pub(super) struct LargeReads {
    len: usize,
    modified: Option<std::time::SystemTime>,
    reads: u8,
    incompressible: bool,
    recent: VecDeque<Query>,
    refused: VecDeque<Query>,
    snapshot: Option<Snapshot>,
}

fn remember(recent: &mut VecDeque<Query>, query: Query) {
    if let Some(at) = recent.iter().position(|entry| *entry == query) {
        recent.remove(at);
    }
    if recent.len() == SLOTS {
        recent.pop_front();
    }
    recent.push_back(query);
}

impl LargeReads {
    pub(super) fn byte_len(&self) -> usize {
        self.len
    }

    pub(super) fn read<T>(
        &mut self,
        mut file: CalendarFile<'_>,
        query: Query,
        render: impl FnOnce(&Data) -> T,
    ) -> Fallible<T> {
        let len = file.byte_len();
        let modified = file.modified();
        if self.len != len || self.modified != modified {
            *self = Self {
                len,
                modified,
                ..Self::default()
            };
        }
        if let Some(snapshot) = &mut self.snapshot
            && let Some(at) = snapshot
                .projections
                .iter()
                .position(|(key, _)| *key == query)
        {
            if file.matches_stream(DeflateDecoder::new(&*snapshot.compressed))?
                && let Some(entry) = snapshot.projections.remove(at)
            {
                let data = serde_json::from_slice(&entry.1)?;
                snapshot.projections.push_back(entry);
                return Ok(render(&data));
            }
            *self = Self {
                len,
                modified,
                ..Self::default()
            };
        }
        self.reads = self.reads.saturating_add(1);
        let admit = self.reads >= WARMUP
            && !self.incompressible
            && self.recent.contains(&query)
            && !self.refused.contains(&query);
        remember(&mut self.recent, query);
        let raw = file.into_bytes()?;
        if !admit
            || self
                .snapshot
                .as_ref()
                .is_some_and(|prior| !prior.matches(&raw))
        {
            if admit {
                *self = Self {
                    len,
                    modified,
                    ..Self::default()
                };
            }
            let data = serde_json::from_slice(&raw)?;
            drop(raw);
            return Ok(render(&data));
        }
        let mut data = serde_json::from_slice(&raw)?;
        let working = if query.restrict(&mut data) {
            let mut out = BufWriter::new(Bounded::new(MAX_PROJECTION));
            serde_json::to_writer(&mut out, &data)
                .ok()
                .and_then(|()| out.into_inner().ok())
                .map(|buffer| buffer.bytes.into_boxed_slice())
        } else {
            None
        };
        if let Some(working) = working {
            // The complete source was validated. Its permitted working set fits the budget.
            if self.snapshot.is_none() {
                self.snapshot = Snapshot::from_bytes(&raw);
                self.incompressible = self.snapshot.is_none();
            }
            if let Some(snapshot) = &mut self.snapshot {
                snapshot.insert(query, working);
            }
        } else {
            remember(&mut self.refused, query);
        }
        drop(raw);
        // Rendering includes the current access policy, security journal and final standby fence.
        Ok(render(&data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use godovoy_plan::storage::online::OnlineVault;

    #[test]
    fn count_and_total_bytes_bound_calendar_projections_independently() {
        let mut snapshot = Snapshot::from_bytes(b"office").unwrap();
        let employee = Id::new_v4();
        let key = |offset| Query::List {
            employee,
            year: 2026,
            offset,
            limit: 12,
        };
        for i in 0..=SLOTS {
            snapshot.insert(key(i), vec![0; 8].into_boxed_slice());
        }
        assert_eq!(snapshot.projections.len(), SLOTS);
        assert_eq!(snapshot.projections.front().map(|(k, _)| *k), Some(key(1)));
        for i in 0..WORKING_BYTES / MAX_PROJECTION {
            snapshot.insert(
                key(SLOTS + 1 + i),
                vec![0; MAX_PROJECTION].into_boxed_slice(),
            );
        }
        assert_eq!(snapshot.projections.len(), WORKING_BYTES / MAX_PROJECTION);
        assert_eq!(
            snapshot
                .projections
                .iter()
                .map(|(_, data)| data.len())
                .sum::<usize>(),
            WORKING_BYTES
        );
    }

    #[test]
    fn a_less_compressible_snapshot_shares_the_same_total_budget() {
        let mut snapshot = Snapshot {
            compressed: vec![0; MAX_SNAPSHOT].into_boxed_slice(),
            projections: VecDeque::new(),
        };
        let employee = Id::new_v4();
        for offset in 0..3 {
            snapshot.insert(
                Query::List {
                    employee,
                    year: 2026,
                    offset,
                    limit: 25,
                },
                vec![0; MAX_PROJECTION].into_boxed_slice(),
            );
        }
        assert_eq!(snapshot.projections.len(), 1);
        assert_eq!(
            snapshot.compressed.len() + snapshot.projections[0].1.len(),
            RETAINED_BYTES
        );
    }

    #[test]
    fn only_recurring_queries_occupy_the_bounded_working_set() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("office");
        let vault = OnlineVault::init(
            &directory,
            "QA".into(),
            Default::default(),
            Default::default(),
        )
        .unwrap();
        let lock = vault.lock().unwrap();
        let mut raw = lock.data_bytes().unwrap();
        raw.resize(super::super::conditional::MAX_SHARED_SOURCE + 1, b' ');
        let path = directory.join("online-calendar-v1.json");
        std::fs::write(&path, &raw).unwrap();
        let employee = Id::new_v4();
        let list = |offset| Query::List {
            employee,
            year: 2026,
            offset,
            limit: 12,
        };
        let mut cache = LargeReads::default();
        let absent = Query::Get {
            employee,
            year: 2026,
            event: Id::new_v4(),
        };
        for _ in 0..6 {
            cache
                .read(lock.open_data().unwrap(), absent, |_| ())
                .unwrap();
        }
        assert!(cache.snapshot.is_none());
        for i in 0..16 {
            cache
                .read(lock.open_data().unwrap(), list(i), |_| ())
                .unwrap();
        }
        assert!(cache.snapshot.is_none());
        for i in 0..16 {
            for _ in 0..2 {
                cache
                    .read(lock.open_data().unwrap(), list(i), |_| ())
                    .unwrap();
            }
        }
        let snapshot = cache.snapshot.as_ref().unwrap();
        assert_eq!(snapshot.projections.len(), 16);
        assert!(snapshot.projections.iter().any(|(key, _)| *key == list(0)));
        assert!(snapshot.compressed.len() <= MAX_SNAPSHOT);
        assert!(
            snapshot
                .projections
                .iter()
                .all(|(_, value)| value.len() <= MAX_PROJECTION)
        );
        // Simulate a filesystem preserving the old modification time across the change.
        *raw.last_mut().unwrap() = b'!';
        std::fs::write(&path, &raw).unwrap();
        cache.modified = lock.open_data().unwrap().modified();
        assert!(
            cache
                .read(lock.open_data().unwrap(), list(15), |_| panic!(
                    "stale data"
                ))
                .is_err()
        );
        assert!(cache.snapshot.is_none());
    }
}
