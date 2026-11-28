//! In-memory validators only. Both credentials and policy are checked before lookup.
use super::super::actions::{CalendarView, EventView, OverviewFormat, PublicYear, ViewContext, YearRows};
use axum::{
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use blake2::{Blake2b, Digest, digest::consts::U32};
use chrono::{Datelike, NaiveDate};
use godovoy_plan::{
    Fallible,
    model::Id,
    storage::online::{CalendarFile, Data},
};
use serde_json::value::{RawValue, to_raw_value};
use std::{
    collections::VecDeque,
    io::{BufWriter, Write},
    sync::Mutex,
};

const MAX_VIEWS: usize = 128;
const MIN_SHARED_SOURCE: usize = 128 * 1024;
pub(super) const MAX_SHARED_SOURCE: usize = 2 * 1024 * 1024;
const MAX_SHARED_ROWS: usize = 1536 * 1024;
const ANNUAL_BLOCK_ROWS: usize = 32;

struct PublicRows {
    key: PublicYear,
    json: Option<Box<RawValue>>,
}

pub(super) struct OriginalSource {
    raw_digest: InputDigest,
    bytes: Vec<u8>,
    public_rows: Option<PublicRows>,
    overview_digest: Option<OverviewDigest>,
}

pub(super) struct WriteInput {
    pub(super) calendar: Data,
    pub(super) original: Option<OriginalSource>,
    pub(super) before: Option<InputDigest>,
}

pub(super) struct ReceiptCommit {
    pub(super) raw_digest: InputDigest,
    pub(super) bytes: Option<Vec<u8>>,
    pub(super) annual: Option<OverviewDigest>,
}

#[derive(Default)]
enum SourceCache {
    #[default]
    Empty,
    Full(DecodedSource),
    Large(super::large_reads::LargeReads),
}

impl SourceCache {
    fn full(&self) -> Option<&DecodedSource> {
        match self { Self::Full(source) => Some(source), _ => None }
    }
    fn full_mut(&mut self) -> Option<&mut DecodedSource> {
        match self { Self::Full(source) => Some(source), _ => None }
    }
    fn take_full(&mut self) -> Option<DecodedSource> {
        match std::mem::take(self) { Self::Full(source) => Some(source), _ => None }
    }
    fn clear_full_for(&mut self, len: usize) {
        if !matches!(self, Self::Large(source) if source.byte_len() == len) {
            *self = Self::Empty;
        }
    }
    fn large_read<T>(
        &mut self, file: CalendarFile<'_>, query: super::large_reads::Query,
        read: impl FnOnce(&Data) -> T,
    ) -> Fallible<T> {
        let mut large = match std::mem::take(self) {
            Self::Large(previous) => previous,
            _ => Default::default(),
        };
        let result = large.read(file, query, read);
        *self = Self::Large(large);
        result
    }
}

struct DecodedSource {
    raw_digest: InputDigest,
    bytes: Vec<u8>,
    calendar: Data,
    public_rows: Option<PublicRows>,
    overview_digest: Option<OverviewDigest>,
}

pub(super) struct OverviewDigest {
    key: PublicYear,
    row_count: usize,
    blocks: Vec<InputDigest>,
    root: InputDigest,
}

impl OverviewDigest {
    fn build(key: PublicYear, rows: &[EventView<'_>]) -> Fallible<Self> {
        let mut writer = BufWriter::new(InputHasher::new());
        let blocks = rows
            .chunks(ANNUAL_BLOCK_ROWS)
            .enumerate()
            .map(|(index, block)| Self::block(&mut writer, index, block))
            .collect::<Fallible<Vec<_>>>()?;
        Ok(Self {
            key,
            row_count: rows.len(),
            root: Self::root(rows.len(), &blocks),
            blocks,
        })
    }

    fn block(
        writer: &mut BufWriter<InputHasher>,
        index: usize,
        rows: &[EventView<'_>],
    ) -> Fallible<InputDigest> {
        writer.get_mut().update(b"godovoy-overview-block-v1\0");
        writer.get_mut().update((index as u64).to_be_bytes());
        serde_json::to_writer(&mut *writer, rows)?;
        writer.flush()?;
        Ok(std::mem::take(writer.get_mut()).finalize().into())
    }

    fn root(row_count: usize, blocks: &[InputDigest]) -> InputDigest {
        let mut hash = InputHasher::new();
        hash.update(b"godovoy-overview-root-v1\0");
        hash.update((row_count as u64).to_be_bytes());
        for block in blocks {
            hash.update(block);
        }
        hash.finalize().into()
    }

    fn matches(&self, key: PublicYear, row_count: usize) -> bool {
        self.key == key && self.row_count == row_count
    }

    fn refresh_row(&mut self, rows: &[EventView<'_>], index: usize) -> Fallible<()> {
        let block_index = index / ANNUAL_BLOCK_ROWS;
        let start = block_index * ANNUAL_BLOCK_ROWS;
        let end = (start + ANNUAL_BLOCK_ROWS).min(self.row_count);
        let block = Self::block(
            &mut BufWriter::new(InputHasher::new()),
            block_index,
            &rows[start..end],
        )?;
        if self.blocks[block_index] != block {
            self.blocks[block_index] = block;
            self.root = Self::root(self.row_count, &self.blocks);
        }
        Ok(())
    }
}

impl DecodedSource {
    fn load(
        slot: &mut SourceCache,
        bytes: Vec<u8>,
        raw_digest: InputDigest,
    ) -> Fallible<&mut Self> {
        // Every caller holds the vault lock. Compare its current full source exactly;
        // the returned validator and personalized fields are always computed for this credential.
        if slot.full().is_some_and(|source| source.bytes == bytes) {
            // Release the duplicate source before allocating the response and event rows.
            drop(bytes);
        } else {
            *slot = SourceCache::Empty;
            let calendar = serde_json::from_slice(&bytes)?;
            *slot = SourceCache::Full(DecodedSource {
                raw_digest,
                bytes,
                calendar,
                public_rows: None,
                overview_digest: None,
            });
        }
        slot.full_mut()
            .ok_or_else(|| "Не удалось прочитать календарь.".into())
    }
}

type InputDigest = [u8; 32];
// Apple Silicon provides SHA-256 instructions on every supported device.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(super) type InputHasher = sha2::Sha256;
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub(super) type InputHasher = Blake2b<U32>;

#[derive(Copy, Clone, Eq, PartialEq)]
struct ReadHint {
    credential: Id,
    size: usize,
    modified: std::time::SystemTime,
}

#[derive(Clone)]
struct CachedView {
    tag: HeaderValue,
    // A write receipt can establish a tag before any complete response is sent.
    body_len: Option<usize>,
}

struct Entry {
    digest: InputDigest,
    view: CachedView,
    hint: Option<ReadHint>,
}

#[derive(Clone, Copy)]
pub(super) struct EventTransition {
    pub before: InputDigest,
    pub after: InputDigest,
    pub event: Id,
}

pub(super) struct Views {
    transition: Mutex<Option<EventTransition>>,
    nonce: Id,
    source: Mutex<SourceCache>,
    entries: Mutex<VecDeque<Entry>>,
}

impl Default for Views {
    fn default() -> Self {
        Self {
            transition: Mutex::default(),
            nonce: Id::new_v4(),
            source: Mutex::default(),
            entries: Mutex::new(VecDeque::new()),
        }
    }
}

impl Views {
    pub(super) fn transition(&self, change: Option<EventTransition>) {
        if let Ok(mut slot) = self.transition.lock() {
            *slot = change;
        }
    }

    pub(super) fn source_key(context: &InputHasher, raw: InputDigest) -> InputDigest {
        context.clone().chain_update(raw).finalize().into()
    }

    pub(super) fn input_hasher(
        credential: Id,
        context: &ViewContext<'_>,
        today: NaiveDate,
    ) -> Fallible<InputHasher> {
        let mut source = InputHasher::new();
        source.update(credential.as_bytes());
        source.update(today.num_days_from_ce().to_be_bytes());
        source.update(context.policy_revision.to_be_bytes());
        source.update([context.overview_format as u8]);
        if context.packed_cards {
            source.update(b"packed-cards-v1\0");
        }
        serde_json::to_writer(&mut source, context)?;
        Ok(source)
    }

    pub(super) fn lookup(&self, key: &InputDigest) -> Fallible<Option<HeaderValue>> {
        Ok(self.lookup_and_refresh(key, None)?.map(|view| view.tag))
    }

    fn lookup_and_refresh(
        &self,
        key: &InputDigest,
        hint: Option<ReadHint>,
    ) -> Fallible<Option<CachedView>> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| "Ошибка проверки версии календаря.")?;
        Ok(entries
            .iter_mut()
            .find(|entry| &entry.digest == key)
            .map(|entry| {
                if hint.is_some() {
                    entry.hint = hint;
                }
                entry.view.clone()
            }))
    }

    pub(super) fn remember(&self, key: InputDigest, tag: HeaderValue) -> Fallible<()> {
        self.store(
            key,
            CachedView {
                tag,
                body_len: None,
            },
            None,
        )
    }

    fn store(&self, key: InputDigest, view: CachedView, hint: Option<ReadHint>) -> Fallible<()> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| "Ошибка проверки версии календаря.")?;
        if let Some(entry) = entries.iter_mut().find(|entry| entry.digest == key) {
            if view.body_len.is_some() {
                entry.view = view;
                entry.hint = hint;
            }
            return Ok(());
        }
        if entries.len() == MAX_VIEWS {
            entries.pop_front();
        }
        entries.push_back(Entry {
            digest: key,
            view,
            hint,
        });
        Ok(())
    }

    fn tag_hasher(&self, credential: Id) -> Blake2b<U32> {
        Blake2b::new()
            .chain_update(self.nonce.as_bytes())
            .chain_update(credential.as_bytes())
    }

    fn tag(digest: Blake2b<U32>) -> Fallible<HeaderValue> {
        let digest = digest.finalize();
        Ok(HeaderValue::from_str(&format!("\"{digest:x}\""))?)
    }

    pub(super) fn view_tag(
        &self,
        credential: Id,
        view: &CalendarView<'_>,
        today: NaiveDate,
    ) -> Fallible<HeaderValue> {
        self.view_tag_with_source(credential, view, today, &mut None)
    }

    pub(super) fn view_tag_with_source(
        &self,
        credential: Id,
        view: &CalendarView<'_>,
        today: NaiveDate,
        memo: &mut Option<OverviewDigest>,
    ) -> Fallible<HeaderValue> {
        let mut digest = self.tag_hasher(credential);
        if let Some(key) = view.annual_digest_key() {
            // Share only a digest of the authorized annual rows. Personal rows,
            // credential, local date and policy still participate in every tag.
            let rows = view.annual_rows();
            let annual = match memo {
                Some(entry) if entry.matches(key, rows.len()) => entry.root,
                slot => slot.insert(OverviewDigest::build(key, rows)?).root,
            };
            if view.tuple_rows() {
                digest.update(b"tuple-v2\0");
            }
            digest.update(b"overview-v1-annual\0");
            digest.update(today.num_days_from_ce().to_be_bytes());
            digest.update(view.policy_revision().to_be_bytes());
            digest.update(annual);
            let mut writer = BufWriter::new(&mut digest);
            serde_json::to_writer(&mut writer, &view.personal_view())?;
            writer.flush()?;
        } else {
            if let Some(assignee) = view.compact_assignee() {
                // Scoped rows never share an annual digest with another employee.
                // Bind the compact form to the complete authorized card contents.
                digest.update(b"overview-v1-assigned-tuple\0");
                digest.update(assignee.as_bytes());
                digest.update(today.num_days_from_ce().to_be_bytes());
                digest.update(view.policy_revision().to_be_bytes());
            }
            let mut writer = BufWriter::new(&mut digest);
            view.write_full(&mut writer)?;
            writer.flush()?;
        }
        Self::tag(digest)
    }

    pub(super) fn view_tag_after_receipt_commit(
        &self,
        credential: Id,
        view: &CalendarView<'_>,
        today: NaiveDate,
        event: Id,
        memo: &mut Option<OverviewDigest>,
    ) -> Fallible<HeaderValue> {
        if let Some(mut prior) = memo.take()
            && view
                .annual_digest_key()
                .is_some_and(|key| prior.matches(key, view.annual_rows().len()))
            && let Some(index) = view.annual_row_index(event)
        {
            // Only Note and Check use receipts. They edit one plan's records
            // without changing the visible annual row order or membership.
            prior.refresh_row(view.annual_rows(), index)?;
            *memo = Some(prior);
        }
        self.view_tag_with_source(credential, view, today, memo)
    }

    fn with_source<T>(
        &self,
        bytes: Vec<u8>,
        raw_digest: InputDigest,
        read: impl FnOnce(&mut DecodedSource) -> Fallible<T>,
    ) -> Fallible<T> {
        let mut cached = self.source.lock().map_err(|_| "Ошибка чтения календаря.")?;
        if (MIN_SHARED_SOURCE..=MAX_SHARED_SOURCE).contains(&bytes.len()) {
            return read(DecodedSource::load(&mut cached, bytes, raw_digest)?);
        }
        cached.clear_full_for(bytes.len());
        drop(cached);
        let data = serde_json::from_slice(&bytes)?;
        drop(bytes);
        read(&mut DecodedSource {
            raw_digest,
            bytes: Vec::new(),
            calendar: data,
            public_rows: None,
            overview_digest: None,
        })
    }

    pub(super) fn read_mcp<T>(
        &self, file: CalendarFile<'_>, query: super::large_reads::Query,
        read: impl FnOnce(&Data) -> T,
    ) -> Fallible<T> {
        if file.byte_len() <= MAX_SHARED_SOURCE {
            return self.read_calendar(file, read);
        }
        let mut cached = self.source.lock().map_err(|_| "Ошибка чтения календаря.")?;
        cached.large_read(file, query, read)
    }

    pub(super) fn read_calendar<T>(
        &self,
        mut file: CalendarFile<'_>,
        read: impl FnOnce(&Data) -> T,
    ) -> Fallible<T> {
        let mut cached = self.source.lock().map_err(|_| "Ошибка чтения календаря.")?;
        if let Some(source) = cached.full()
            && file.matches_snapshot(&source.bytes)?
        {
            return Ok(read(&source.calendar));
        }
        *cached = SourceCache::Empty;
        let bytes = file.into_bytes()?;
        if (MIN_SHARED_SOURCE..=MAX_SHARED_SOURCE).contains(&bytes.len()) {
            let raw = InputHasher::digest(&bytes).into();
            return Ok(read(&DecodedSource::load(&mut cached, bytes, raw)?.calendar));
        }
        drop(cached);
        let calendar = serde_json::from_slice(&bytes)?;
        drop(bytes);
        Ok(read(&calendar))
    }

    pub(super) fn detail(
        &self,
        credential: Id,
        context: ViewContext<'_>,
        today: NaiveDate,
        mut file: CalendarFile<'_>,
        base: HeaderValue,
        target: (Id, i32),
    ) -> Fallible<Response> {
        if context.overview_format == OverviewFormat::Full {
            return Ok(StatusCode::PRECONDITION_FAILED.into_response());
        }
        let scope = Self::input_hasher(credential, &context, today)?;
        // An annual overview contains every occurrence of its year. Adjacent-year
        // personal tasks still require the ordinary visibility check below.
        if let Some(year) = context.public_rows_key()
            && year.year == target.1
            && (MIN_SHARED_SOURCE..=MAX_SHARED_SOURCE).contains(&file.byte_len())
        {
            let cached = self.source.lock().map_err(|_| "Ошибка чтения календаря.")?;
            if let Some(source) = cached.full()
                && source.overview_digest.as_ref().is_some_and(|digest| digest.key == year)
                && self.lookup(&Self::source_key(&scope, source.raw_digest))?.as_ref() == Some(&base)
                && file.matches_snapshot(&source.bytes)?
            {
                let Some(event) = source.calendar.calendar.event(target.0, target.1) else {
                    return Ok(StatusCode::NOT_FOUND.into_response());
                };
                let row = super::super::actions::occurrence(&source.calendar, event, year.allow_free_text);
                let mut response = json_body(serde_json::to_vec(&row)?);
                response.headers_mut().insert("x-godovoy-state-tag", base);
                return Ok(response);
            }
        }
        let bytes = file.into_bytes()?;
        let raw = InputHasher::digest(&bytes).into();
        let key = Self::source_key(&scope, raw);
        let known = self.lookup(&key)?;
        if known.as_ref().is_some_and(|tag| *tag != base) {
            return Ok(StatusCode::PRECONDITION_FAILED.into_response());
        }
        self.with_source(bytes, raw, |source| {
            let view = context.render(&source.calendar, today);
            if !view.is_overview()
                || (known.is_none()
                    && self.view_tag_with_source(
                        credential,
                        &view,
                        today,
                        &mut source.overview_digest,
                    )? != base)
            {
                return Ok(StatusCode::PRECONDITION_FAILED.into_response());
            }
            let Some(event) = view.event(target.0, target.1) else {
                return Ok(StatusCode::NOT_FOUND.into_response());
            };
            let mut response = json_body(serde_json::to_vec(event)?);
            response.headers_mut().insert("x-godovoy-state-tag", base);
            Ok(response)
        })
    }

    pub(super) fn data_for_write(
        &self,
        mut file: CalendarFile<'_>,
        needs_receipt: bool,
    ) -> Fallible<WriteInput> {
        // The vault lock covers comparison, mutation and the durable commit.
        let prior = self.source.lock().ok().and_then(|mut slot| slot.take_full());
        if let Some(source) = prior
            && file.matches_snapshot(&source.bytes)?
        {
            return Ok(WriteInput {
                before: Some(source.raw_digest),
                calendar: source.calendar,
                original: Some(OriginalSource {
                    raw_digest: source.raw_digest,
                    bytes: source.bytes,
                    public_rows: source.public_rows,
                    overview_digest: source.overview_digest,
                }),
            });
        }
        let bytes = file.into_bytes()?;
        Ok(WriteInput {
            calendar: serde_json::from_slice(&bytes)?,
            before: needs_receipt.then(|| InputHasher::digest(&bytes).into()),
            original: None,
        })
    }

    pub(super) fn commit_for_receipt(
        lock: &godovoy_plan::storage::online::LockedVault<'_>,
        data: &Data,
        original: Option<OriginalSource>,
    ) -> Fallible<ReceiptCommit> {
        // data_for_write admits the previous digest only after comparing all
        // bytes under this lock. Export it for updating only after durable save.
        let (mut bytes, annual) = match original {
            Some(OriginalSource {
                mut bytes,
                overview_digest,
                ..
            }) => {
                bytes.clear();
                (Some(bytes), overview_digest)
            }
            None => (None, None),
        };
        let mut hash = InputHasher::new();
        lock.save_data_observed(data, |chunk| {
            hash.update(chunk);
            if let Some(buffer) = bytes.as_mut() {
                if chunk.len() > MAX_SHARED_SOURCE - buffer.len()
                    || buffer.try_reserve_exact(chunk.len()).is_err()
                {
                    bytes = None;
                } else {
                    buffer.extend_from_slice(chunk);
                }
            }
        })?;
        Ok(ReceiptCommit {
            raw_digest: hash.finalize().into(),
            bytes: bytes.filter(|b| {
                b.len() >= MIN_SHARED_SOURCE
                    && b.capacity() <= MAX_SHARED_SOURCE
                    && b.capacity() <= b.len() * 2
            }),
            annual,
        })
    }

    pub(super) fn commit_without_receipt(
        &self,
        lock: &godovoy_plan::storage::online::LockedVault<'_>,
        data: Data,
        original: Option<OriginalSource>,
        notify: impl FnOnce(&Data),
    ) -> Fallible<()> {
        // Even an error syncing the directory can follow a completed rename.
        // A general command cannot extend the preceding single-event receipt.
        self.transition(None);
        let committed = match original {
            Some(source) => Some(Self::commit_for_receipt(lock, &data, Some(source))?),
            None => {
                lock.save_data(&data)?;
                None
            }
        };
        notify(&data);
        if let Some(committed) = committed {
            // A general command may change row membership and ordering.
            self.retain_committed(committed.bytes, committed.raw_digest, data, None);
        }
        Ok(())
    }

    pub(super) fn retain_committed(
        &self,
        bytes: Option<Vec<u8>>,
        raw_digest: InputDigest,
        calendar: Data,
        overview_digest: Option<OverviewDigest>,
    ) {
        if let Some(bytes) = bytes
            && let Ok(mut slot) = self.source.lock()
        {
            *slot = SourceCache::Full(DecodedSource {
                raw_digest,
                bytes,
                calendar,
                overview_digest,
                public_rows: None,
            });
        }
    }

    pub(super) fn restore_after_failure(&self, original: Option<OriginalSource>) {
        // A rejected command can already have changed its in-memory Data.
        // Only the original, verified source can repopulate the read cache.
        if let Some(OriginalSource {
            raw_digest,
            bytes,
            public_rows,
            overview_digest,
        }) = original
            && let Ok(calendar) = serde_json::from_slice(&bytes)
            && let Ok(mut slot) = self.source.lock()
        {
            *slot = SourceCache::Full(DecodedSource {
                raw_digest,
                bytes,
                calendar,
                public_rows,
                overview_digest,
            });
        }
    }

    fn encode_shared_body(
        source: &mut DecodedSource,
        context: ViewContext<'_>,
        today: NaiveDate,
        expected_len: Option<usize>,
    ) -> Fallible<Vec<u8>> {
        let mut body = Vec::with_capacity(expected_len.unwrap_or(0));
        // A restricted request always follows its normal row filter. A second unrestricted
        // read of the same year and text policy may reuse the shared event array.
        let public_key = context.public_rows_key();
        if let Some(key) = public_key
            && let Some(saved) = &mut source.public_rows
            && saved.key == key
        {
            let rows = match &mut saved.json {
                Some(rows) => rows,
                slot @ None => slot.insert(to_raw_value(&context.event_rows(&source.calendar))?),
            };
            if expected_len.is_none() {
                // Leave room for this reader's roster and tasks. An exact reservation for
                // the shared array always forces a second allocation for the envelope.
                body.reserve((rows.get().len() + 1).next_power_of_two());
            }
            serde_json::to_writer(
                &mut body,
                &context.with_rows(&source.calendar, today, &**rows),
            )?;
            return Ok(body);
        }
        serde_json::to_writer(&mut body, &context.render(&source.calendar, today))?;
        if let Some(key) = public_key {
            // The complete first response proves the size limit for its year array.
            source.public_rows =
                (body.len() <= MAX_SHARED_ROWS).then_some(PublicRows { key, json: None });
        }
        Ok(body)
    }

    fn render_employee_view(
        &self,
        source: &mut DecodedSource,
        context: ViewContext<'_>,
        today: NaiveDate,
        credential: Id,
        condition: &[HeaderValue],
        hint: Option<ReadHint>,
    ) -> Fallible<Response> {
        let accepts_page_delta = context.accepts_page_delta;
        let scope = Self::input_hasher(credential, &context, today)?;
        let key = Self::source_key(&scope, source.raw_digest);
        let known = self.lookup_and_refresh(&key, hint)?;
        let view = context.render(&source.calendar, today);
        let year_rows = view.visible_year_rows();
        let body = if view.is_overview() {
            None
        } else {
            // The offered representation stayed full. Serialize it
            // once, as on the legacy path, and hash those same bytes.
            let expected_len = known.as_ref().and_then(|saved| saved.body_len);
            let mut body = Vec::with_capacity(expected_len.unwrap_or(0));
            view.write_full(&mut body)?;
            Some(body)
        };
        let tag = match (known, &body) {
            (Some(view), _) => view.tag,
            (None, Some(body)) => {
                Self::tag(self.tag_hasher(credential).chain_update(body))?
            }
            (None, None) => self.view_tag_with_source(
                credential,
                &view,
                today,
                &mut source.overview_digest,
            )?,
        };
        self.store(
            key,
            CachedView {
                tag: tag.clone(),
                body_len: body.as_ref().map(Vec::len),
            },
            hint,
        )?;
        if condition.is_empty() && view.is_overview()
            && let Some((prior, supplied)) = view.continuation()
        {
            let previous = response_tag(&tag, prior)?;
            if previous == supplied && let Some(bytes) = view.encode_continuation(prior)? {
                let target = response_tag(&tag, year_rows)?;
                let body_tag = Self::tag(Blake2b::new().chain_update(b"godovoy-continuation-v1\0")
                    .chain_update(previous.as_bytes()).chain_update(target.as_bytes()))?;
                let mut response = tagged(json_body(bytes), body_tag);
                let headers = response.headers_mut();
                headers.insert("x-godovoy-continued", previous);
                if year_rows != YearRows::All {
                    headers.insert("x-godovoy-page-tag", target);
                }
                headers.insert("x-godovoy-state-tag", tag);
                return Ok(response);
            }
        }
        if !condition.is_empty() && matches_tag(condition, &response_tag(&tag, year_rows)?) {
            tagged_view(StatusCode::NOT_MODIFIED.into_response(), tag, year_rows)
        } else if accepts_page_delta && let Some(limit) = year_rows.prefix_size()
            && let [base] = condition
            && let Some(change) = self.transition.lock().ok().and_then(|last| *last)
            && change.after == source.raw_digest
            && let Some(previous) = self.lookup(&Self::source_key(&scope, change.before))?
            && response_tag(&previous, year_rows)? == *base
            && view.annual_row_index(change.event).is_some_and(|i| i < usize::from(limit))
        {
            // Only a committed Check or Note records a transition. Neither changes
            // the annual order or membership. Encode the caller's visible prefix and tasks.
            let mut response = json_body(view.encode_change(change.event, None)?);
            let headers = response.headers_mut();
            headers.insert("x-godovoy-delta-base", base.clone());
            headers.insert("x-godovoy-page-tag", response_tag(&tag, year_rows)?);
            headers.insert("x-godovoy-state-tag", tag);
            headers.insert(header::VARY, HeaderValue::from_static(VARY));
            Ok(response)
        } else if let Some(body) = body {
            tagged_view(json_body(body), tag, year_rows)
        } else {
            render_state(&view, tag, year_rows)
        }
    }

    pub(super) fn response(
        &self,
        credential: Id,
        context: ViewContext<'_>,
        today: NaiveDate,
        mut file: CalendarFile<'_>,
        condition: &[HeaderValue],
        accept_delta: bool,
    ) -> Fallible<Response> {
        let year_rows = context.year_rows;
        // This internal fingerprint may change when another employee edits a private
        // event. Only the digest of the caller's rendered representation leaves the host.
        let hint = file.modified().map(|modified| ReadHint {
            credential,
            size: file.byte_len(),
            modified,
        });
        // A matching hint only chooses streaming. Every 304 still verifies the full
        // file, current credential, permissions, local date and rendered-view tag.
        if (condition.is_empty() || year_rows != YearRows::All)
            && (MIN_SHARED_SOURCE..=MAX_SHARED_SOURCE).contains(&file.byte_len())
        {
            let mut cached = self.source.lock().map_err(|_| "Ошибка чтения календаря.")?;
            if let Some(source) = cached.full_mut()
                && file.matches_snapshot(&source.bytes)?
            {
                // The same lock protects the bytes just verified and their decoded document.
                if context.overview_format == OverviewFormat::Full && !context.assigned_only {
                    let scope = Self::input_hasher(credential, &context, today)?;
                    let key = Self::source_key(&scope, source.raw_digest);
                    let known = self.lookup_and_refresh(&key, hint)?;
                    let expected_len = known.as_ref().and_then(|v| v.body_len);
                    let body = Self::encode_shared_body(source, context, today, expected_len)?;
                    drop(cached);
                    return self.full_response(credential, key, known, hint, condition, body);
                }
                return self.render_employee_view(source, context, today, credential, condition, hint);
            }
        }
        let scope = Self::input_hasher(credential, &context, today)?;
        if year_rows == YearRows::All && let ([condition], Some(hint)) = (condition, hint) {
            let candidate = self
                .entries
                .lock()
                .map_err(|_| "Ошибка проверки версии календаря.")?
                .iter()
                .rev()
                .find(|entry| entry.hint == Some(hint) && entry.view.tag == condition)
                .map(|entry| entry.digest);
            if let Some(expected) = candidate {
                let cached = self.source.lock().map_err(|_| "Ошибка чтения календаря.")?;
                let verified =
                    if let Some(source) = cached.full().filter(|s| s.bytes.len() == hint.size) {
                        Self::source_key(&scope, source.raw_digest) == expected
                            && file.matches_bytes(&source.bytes)?
                    } else {
                        Self::source_key(&scope, file.fingerprint(InputHasher::new())?) == expected
                    };
                if verified {
                    return Ok(tagged(
                        StatusCode::NOT_MODIFIED.into_response(),
                        condition.clone(),
                    ));
                }
            }
        }
        let data = file.into_bytes()?;
        let raw = InputHasher::digest(&data).into();
        let key = Self::source_key(&scope, raw);
        let known = self.lookup_and_refresh(&key, hint)?;
        if year_rows == YearRows::All && let Some(view) = &known
            && matches_tag(condition, &view.tag)
        {
            return Ok(tagged(
                StatusCode::NOT_MODIFIED.into_response(),
                view.tag.clone(),
            ));
        }
        if year_rows == YearRows::All && accept_delta && let [base] = condition {
            let transition = self.transition.lock().ok().and_then(|last| *last);
            if let Some(change) = transition
                && change.after == raw
                && let Some(previous) = self.lookup(&Self::source_key(&scope, change.before))?
                && previous == *base
            {
                return self.with_source(data, raw, |source| {
                    let view = context.render(&source.calendar, today);
                    let tag = match &known {
                        Some(view) => view.tag.clone(),
                        None => self.view_tag_with_source(
                            credential,
                            &view,
                            today,
                            &mut source.overview_digest,
                        )?,
                    };
                    self.remember(key, tag.clone())?;
                    if tag == previous {
                        return Ok(tagged(StatusCode::NOT_MODIFIED.into_response(), tag));
                    }
                    let receipt = view.changed_event(change.event);
                    if receipt.has_visible_rows() {
                        let mut response = if view.is_overview() {
                            json_body(view.encode_change(change.event, None)?)
                        } else {
                            axum::Json(receipt).into_response()
                        };
                        response
                            .headers_mut()
                            .insert("x-godovoy-delta-base", previous);
                        response.headers_mut().insert("x-godovoy-state-tag", tag);
                        response
                            .headers_mut()
                            .insert(header::VARY, HeaderValue::from_static(VARY));
                        return Ok(response);
                    }
                    // Absence is ambiguous for an adjacent-year task. Send this
                    // employee's complete representation without naming a hidden event.
                    render_state(&view, tag, YearRows::All)
                });
            }
        }
        if context.overview_format != OverviewFormat::Full {
            return self.with_source(data, raw, |source| {
                self.render_employee_view(source, context, today, credential, condition, hint)
            });
        }
        let expected_len = known.as_ref().and_then(|view| view.body_len);
        // Keep the parsed document and rows alive through validator generation on a
        // full uncached read, as on the original calendar response path.
        let calendar;
        let view;
        let body = if (MIN_SHARED_SOURCE..=MAX_SHARED_SOURCE).contains(&data.len()) {
            let mut entry = self.source.lock().map_err(|_| "Ошибка чтения календаря.")?;
            let source = DecodedSource::load(&mut entry, data, raw)?;
            Self::encode_shared_body(source, context, today, expected_len)?
        } else {
            self.source.lock().map_err(|_| "Ошибка чтения календаря.")?.clear_full_for(data.len());
            calendar = serde_json::from_slice(&data)?;
            drop(data);
            view = context.render(&calendar, today);
            if let Some(len) = expected_len {
                let mut body = Vec::with_capacity(len);
                serde_json::to_writer(&mut body, &view)?;
                body
            } else {
                serde_json::to_vec(&view)?
            }
        };
        self.full_response(credential, key, known, hint, condition, body)
    }

    fn full_response(
        &self,
        credential: Id,
        key: InputDigest,
        known: Option<CachedView>,
        hint: Option<ReadHint>,
        condition: &[HeaderValue],
        body: Vec<u8>,
    ) -> Fallible<Response> {
        let tag = if let Some(CachedView {
            tag,
            body_len: Some(_),
        }) = &known
        {
            tag.clone()
        } else {
            let tag = match known {
                Some(cached) => cached.tag,
                None => Self::tag(self.tag_hasher(credential).chain_update(&body))?,
            };
            self.store(
                key,
                CachedView {
                    tag: tag.clone(),
                    body_len: Some(body.len()),
                },
                hint,
            )?;
            tag
        };
        let response = if matches_tag(condition, &tag) {
            StatusCode::NOT_MODIFIED.into_response()
        } else {
            ([(header::CONTENT_TYPE, "application/json")], body).into_response()
        };
        Ok(tagged(response, tag))
    }
}

const VARY: &str = "Authorization, X-Godovoy-Delta, X-Godovoy-View, X-Godovoy-Rows, X-Godovoy-Assigned-View, X-Godovoy-Cards, X-Godovoy-Page, X-Godovoy-Continue, X-Godovoy-Base, If-None-Match";

pub(super) fn json_body(body: Vec<u8>) -> Response {
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

fn render_state(view: &CalendarView<'_>, tag: HeaderValue, year_rows: YearRows) -> Fallible<Response> {
    let mut response = json_body(view.encode()?);
    if view.is_overview() {
        // Intermediaries may weaken a compressed representation's HTTP ETag.
        response
            .headers_mut()
            .insert("x-godovoy-state-tag", tag.clone());
    }
    tagged_view(response, tag, year_rows)
}

fn first_page_tag(snapshot: &HeaderValue) -> Fallible<HeaderValue> {
    Views::tag(Blake2b::new().chain_update(b"godovoy-first-page-v1\0").chain_update(snapshot.as_bytes()))
}

pub(super) fn response_tag(snapshot: &HeaderValue, year_rows: YearRows) -> Fallible<HeaderValue> {
    match year_rows {
        YearRows::All => Ok(snapshot.clone()),
        YearRows::First => first_page_tag(snapshot),
        rows => {
            let size = rows.prefix_size().ok_or("Неизвестный размер страницы календаря.")?;
            Views::tag(Blake2b::new()
                .chain_update(b"godovoy-prefix-v1\0")
                .chain_update(size.to_be_bytes())
                .chain_update(snapshot.as_bytes()))
        }
    }
}

fn tagged_view(mut response: Response, snapshot: HeaderValue, year_rows: YearRows) -> Fallible<Response> {
    let tag = response_tag(&snapshot, year_rows)?;
    if year_rows != YearRows::All {
        response.headers_mut().insert("x-godovoy-state-tag", snapshot);
    }
    Ok(tagged(response, tag))
}

fn tagged(mut response: Response, tag: HeaderValue) -> Response {
    response.headers_mut().insert(header::ETAG, tag);
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static(VARY));
    response
}

fn matches_tag(condition: &[HeaderValue], current: &HeaderValue) -> bool {
    if let [one] = condition
        && one.as_bytes().trim_ascii() == b"*"
    {
        return true;
    }
    let mut matched = false;
    for line in condition {
        let mut rest = line.as_bytes().trim_ascii();
        while !rest.is_empty() {
            if let Some(next) = rest.strip_prefix(b",") {
                rest = next.trim_ascii_start();
                continue;
            }
            let opaque = rest.strip_prefix(b"W/").unwrap_or(rest);
            let Some(value) = opaque.strip_prefix(b"\"") else {
                return false;
            };
            let Some(end) = value.iter().position(|&b| b == b'"') else {
                return false;
            };
            if value[..end].iter().any(|&b| b < 0x21 || b == 0x7f) {
                return false;
            }
            matched |= &opaque[..end + 2] == current.as_bytes();
            rest = value[end + 1..].trim_ascii_start();
            if !rest.is_empty() && !rest.starts_with(b",") {
                return false;
            }
        }
    }
    matched
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_digest_matches_full_recomputation_at_boundaries() {
        use super::super::super::actions;
        use godovoy_plan::storage::online::OnlineVault;
        use serde_json::json;

        let temporary = tempfile::tempdir().unwrap();
        let vault = OnlineVault::init(
            &temporary.path().join("office"),
            "QA".into(),
            Default::default(),
            Default::default(),
        )
        .unwrap();
        let lock = vault.lock().unwrap();
        let (credential, employee, _) = lock.grant("Сотрудник", None, false).unwrap();
        let access = lock.access().unwrap();
        let today = NaiveDate::from_ymd_opt(2028, 4, 1).unwrap();
        let mut original = lock.data().unwrap();
        for number in 0..97 {
            let event = serde_json::from_value(json!({
                "op": "createEvent",
                "title": format!("Событие {number}"),
                "description": format!("Описание {number}"),
                "date": "2028-04-04",
                "category": "Team",
                "yearly": false,
                "ownerId": null,
                "preparationDays": 7,
                "checklist": [{"title":"Проверить", "daysBefore":3}]
            }))
            .unwrap();
            actions::apply(event, &mut original, &access)
                .unwrap();
        }
        let context =
            |format| ViewContext::new(&access, 2028, employee, true).offer_overview(format);
        let occurrences = original.calendar.year(2028);
        let original_bytes = serde_json::to_vec(&original).unwrap();
        let views = Views::default();
        for format in [OverviewFormat::NamedRows, OverviewFormat::TupleRows] {
            for index in [0, 31, 32, 63, 64, 95, 96] {
                let old = context(format).render(&original, today);
                let mut memo = None;
                let before = views
                    .view_tag_with_source(credential, &old, today, &mut memo)
                    .unwrap();
                let blocks = memo.as_ref().unwrap().blocks.clone();
                assert_eq!(blocks.len(), 4);
                let event = occurrences[index];
                let mut changed: Data = serde_json::from_slice(&original_bytes).unwrap();
                for command in [
                    json!({"op":"note","eventId":event.id,"year":2028,"base":"","text":"Новая заметка"}),
                    json!({"op":"check","eventId":event.id,"year":2028,"taskId":event.event.tasks[0].id,"done":true}),
                ] {
                    actions::apply(
                        serde_json::from_value(command).unwrap(),
                        &mut changed,
                        &access,
                    )
                    .unwrap();
                    let view = context(format).render(&changed, today);
                    let updated = views
                        .view_tag_after_receipt_commit(
                            credential, &view, today, event.id, &mut memo,
                        )
                        .unwrap();
                    assert_eq!(updated, views.view_tag(credential, &view, today).unwrap());
                    assert_ne!(updated, before);
                    let difference: Vec<_> = blocks
                        .iter()
                        .zip(&memo.as_ref().unwrap().blocks)
                        .enumerate()
                        .filter_map(|(i, (a, b))| (a != b).then_some(i))
                        .collect();
                    assert_eq!(difference, [index / ANNUAL_BLOCK_ROWS]);
                }
            }
            let mut memo = None;
            let prior = context(format).render(&original, today);
            let before = views
                .view_tag_with_source(credential, &prior, today, &mut memo)
                .unwrap();
            let event = occurrences[0].id;
            let mut changed: Data = serde_json::from_slice(&original_bytes).unwrap();
            let note = json!({"op":"note","eventId":event,"year":2028,"base":"","text":"Изменено"});
            actions::apply(serde_json::from_value(note).unwrap(), &mut changed, &access)
                .unwrap();
            let fresh = context(format).render(&changed, today);
            let updated = views
                .view_tag_after_receipt_commit(credential, &fresh, today, Id::new_v4(), &mut memo)
                .unwrap();
            assert_eq!(updated, views.view_tag(credential, &fresh, today).unwrap());
            assert_ne!(updated, before);
        }
    }

    #[test]
    fn block_digest_rebuilds_for_text_policy_or_different_year() {
        use super::super::super::actions;
        use godovoy_plan::storage::online::{OnlineVault, ProtectionSettings};
        use serde_json::json;

        let temporary = tempfile::tempdir().unwrap();
        let vault = OnlineVault::init(
            &temporary.path().join("office"),
            "QA".into(),
            Default::default(),
            Default::default(),
        )
        .unwrap();
        let lock = vault.lock().unwrap();
        let (credential, employee, _) = lock.grant("Сотрудник", None, false).unwrap();
        let original_access = lock.access().unwrap();
        let mut data = lock.data().unwrap();
        let today = NaiveDate::from_ymd_opt(2028, 4, 1).unwrap();
        let create = json!({
            "op":"createEvent","title":"Годовой план","description":"Локальная заметка",
            "date":"2028-04-04","category":"Team","yearly":true,"ownerId":null,
            "preparationDays":7,"checklist":[{"title":"Проверить","daysBefore":3}]
        });
        for _ in 0..96 {
            actions::apply(
                serde_json::from_value(create.clone()).unwrap(),
                &mut data,
                &original_access,
            )
            .unwrap();
        }
        let id = *data.calendar.plans.keys().next().unwrap();
        let view = |access, year| {
            ViewContext::new(access, year, employee, true)
                .offer_overview(OverviewFormat::TupleRows)
                .render(&data, today)
        };
        let views = Views::default();
        let mut memo = None;
        views
            .view_tag_with_source(credential, &view(&original_access, 2028), today, &mut memo)
            .unwrap();
        let before = memo.as_ref().unwrap().root;
        let future = view(&original_access, 2029);
        let tag = views
            .view_tag_after_receipt_commit(credential, &future, today, id, &mut memo)
            .unwrap();
        assert_eq!(tag, views.view_tag(credential, &future, today).unwrap());
        assert_ne!(before, memo.as_ref().unwrap().root);

        lock.set_protection(
            0,
            ProtectionSettings {
                russia: true,
                allow_free_text: false,
                ..Default::default()
            },
        )
        .unwrap();
        let narrowed = lock.access().unwrap();
        let private = view(&narrowed, 2029);
        let before = memo.as_ref().unwrap().root;
        let tag = views
            .view_tag_after_receipt_commit(credential, &private, today, id, &mut memo)
            .unwrap();
        assert_eq!(tag, views.view_tag(credential, &private, today).unwrap());
        assert_ne!(before, memo.as_ref().unwrap().root);
    }

    #[test]
    fn response_size_survives_a_duplicate_receipt_and_expires_with_its_digest() {
        let views = Views::default();
        let digest = [0; 32];
        let tag = HeaderValue::from_static("\"year\"");
        let body_len = |key| {
            views
                .lookup_and_refresh(&key, None)
                .unwrap()
                .unwrap()
                .body_len
        };
        views.remember(digest, tag.clone()).unwrap();
        assert_eq!(body_len(digest), None);
        views
            .store(
                digest,
                CachedView {
                    tag: tag.clone(),
                    body_len: Some(780),
                },
                None,
            )
            .unwrap();
        views.remember(digest, tag.clone()).unwrap();
        for i in 1..MAX_VIEWS {
            views.remember([i as u8; 32], tag.clone()).unwrap();
        }
        assert_eq!(body_len(digest), Some(780));
        assert_eq!(body_len([1; 32]), None);
        views.remember([255; 32], tag).unwrap();
        assert!(views.lookup(&digest).unwrap().is_none());
    }
    #[test]
    fn committed_source_matches_the_file_and_obeys_admission_limits() {
        use godovoy_plan::storage::online::{Audit, OnlineVault};
        let tmp = tempfile::tempdir().unwrap();
        let vault = OnlineVault::init(
            &tmp.path().join("office"),
            "QA".into(),
            Default::default(),
            Default::default(),
        )
        .unwrap();
        let lock = vault.lock().unwrap();
        let mut data = lock.data().unwrap();
        let id = Id::new_v4();
        data.audit.push_back(Audit {
            at: 0,
            action: String::new(),
            credential: id,
            employee: id,
            event: None,
        });
        let envelope = serde_json::to_vec(&data).unwrap().len();
        for (len, capacity, retained) in [
            (MIN_SHARED_SOURCE - 1, Some(MIN_SHARED_SOURCE), false),
            (MIN_SHARED_SOURCE, None, false),
            (MIN_SHARED_SOURCE, Some(MIN_SHARED_SOURCE), true),
            (MIN_SHARED_SOURCE, Some(MAX_SHARED_SOURCE), false),
            (MAX_SHARED_SOURCE, Some(MAX_SHARED_SOURCE), true),
            (MAX_SHARED_SOURCE + 1, Some(MAX_SHARED_SOURCE), false),
        ] {
            data.audit[0].action = "a".repeat(len - envelope);
            let prior = capacity.map(|size| OriginalSource {
                raw_digest: [0; 32],
                bytes: Vec::with_capacity(size),
                public_rows: None,
                overview_digest: None,
            });
            let ReceiptCommit { raw_digest: digest, bytes, annual } =
                Views::commit_for_receipt(&lock, &data, prior).unwrap();
            assert!(annual.is_none());
            let disk = lock.data_bytes().unwrap();
            assert_eq!(disk.len(), len);
            assert_eq!(digest.as_slice(), InputHasher::digest(&disk).as_slice());
            assert_eq!(bytes.is_some(), retained);
            if let Some(bytes) = bytes {
                assert_eq!(bytes, disk);
            }
        }
    }

    #[test]
    fn write_without_view_receipt_keeps_only_the_new_durable_source() {
        use godovoy_plan::storage::online::{Audit, OnlineVault};
        let tmp = tempfile::tempdir().unwrap();
        let directory = tmp.path().join("office");
        let vault = OnlineVault::init(
            &directory,
            "QA".into(),
            Default::default(),
            Default::default(),
        )
        .unwrap();
        let lock = vault.lock().unwrap();
        let views = Views::default();
        let id = Id::new_v4();
        let mut data = lock.data().unwrap();
        data.audit.push_back(Audit {
            at: 0,
            action: "a".repeat(MIN_SHARED_SOURCE),
            credential: id,
            employee: id,
            event: None,
        });
        lock.save_data(&data).unwrap();
        views
            .read_calendar(lock.open_data().unwrap(), |_| ())
            .unwrap();
        let WriteInput {
            mut calendar,
            original,
            ..
        } = views
            .data_for_write(lock.open_data().unwrap(), false)
            .unwrap();
        let mut prior = original.unwrap();
        let year = PublicYear {
            year: 2026,
            allow_free_text: true,
        };
        prior.public_rows = Some(PublicRows {
            key: year,
            json: None,
        });
        prior.overview_digest = Some(OverviewDigest {
            key: year,
            row_count: 1,
            blocks: vec![[1; 32]],
            root: [1; 32],
        });
        let transition = Some(EventTransition {
            event: id,
            before: [1; 32],
            after: [2; 32],
        });
        views.transition(transition);
        calendar.audit[0].at = 1;
        views
            .commit_without_receipt(&lock, calendar, Some(prior), |data| {
                assert_eq!(
                    lock.data_bytes().unwrap(),
                    serde_json::to_vec(data).unwrap()
                );
            })
            .unwrap();
        {
            let source = views.source.lock().unwrap();
            let saved = source.full().unwrap();
            assert_eq!(saved.calendar.audit[0].at, 1);
            assert_eq!(saved.bytes, lock.data_bytes().unwrap());
            assert!(saved.overview_digest.is_none() && saved.public_rows.is_none());
            assert!(views.transition.lock().unwrap().is_none());
        }
        let WriteInput {
            calendar, original, ..
        } = views
            .data_for_write(lock.open_data().unwrap(), false)
            .unwrap();
        let path = directory.join("online-calendar-v1.json");
        let backup = directory.join("backup");
        std::fs::rename(&path, &backup).unwrap();
        std::fs::create_dir(&path).unwrap();
        views.transition(transition);
        assert!(
            views
                .commit_without_receipt(&lock, calendar, original, |_| panic!(
                    "failed save notified"
                ))
                .is_err()
        );
        assert!(
            views.source.lock().unwrap().full().is_none() && views.transition.lock().unwrap().is_none()
        );
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(backup, path).unwrap();
        views
            .read_calendar(lock.open_data().unwrap(), |data| {
                assert_eq!(data.audit[0].at, 1)
            })
            .unwrap();
    }

    #[test]
    fn decoded_reads_compare_the_last_byte_and_keep_only_bounded_sources() {
        use godovoy_plan::storage::online::OnlineVault;
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("office");
        let vault = OnlineVault::init(&directory, "QA".into(), Default::default(), Default::default()).unwrap();
        let lock = vault.lock().unwrap();
        let path = directory.join("online-calendar-v1.json");
        let views = Views::default();
        let mut document = lock.data_bytes().unwrap();
        let valid_len = document.len();
        for (length, retained) in [
            (MIN_SHARED_SOURCE - 1, false),
            (MIN_SHARED_SOURCE, true),
            (MAX_SHARED_SOURCE, true),
            (MAX_SHARED_SOURCE + 1, false),
        ] {
            document.truncate(valid_len);
            document.resize(length, b' ');
            std::fs::write(&path, &document).unwrap();
            for _ in 0..2 {
                views.read_calendar(lock.open_data().unwrap(), |_| ()).unwrap();
                assert_eq!(views.source.lock().unwrap().full().is_some(), retained);
            }
            document[length - 1] = b'!';
            std::fs::write(&path, &document).unwrap();
            assert!(views.read_calendar(lock.open_data().unwrap(), |_| panic!("stale read")).is_err());
            assert!(views.source.lock().unwrap().full().is_none());
        }
    }

    #[test]
    fn commands_consume_only_the_matching_decoded_source() {
        use godovoy_plan::storage::online::OnlineVault;
        let tmp = tempfile::tempdir().unwrap();
        let directory = tmp.path().join("office");
        let vault = OnlineVault::init(&directory, "QA".into(), Default::default(), Default::default()).unwrap();
        let lock = vault.lock().unwrap();
        let path = directory.join("online-calendar-v1.json");
        let employee = Id::new_v4();
        let document = |name| {
            serde_json::to_vec(&serde_json::json!({
                "calendar":{"members":{employee.to_string():{
                    "revision":{"sequence":1,"device":employee},"value":name
                }},"plans":{}},
                "audit":[]
            }))
            .unwrap()
        };
        let initial = document("Anna");
        let changed = document("Olga");
        assert_eq!(initial.len(), changed.len());
        let views = Views::default();
        let warm = || {
            *views.source.lock().unwrap() = SourceCache::Full(DecodedSource {
                raw_digest: InputHasher::digest(&initial).into(),
                calendar: serde_json::from_slice(&initial).unwrap(),
                bytes: initial.clone(),
                public_rows: None,
                overview_digest: None,
            });
        };
        for (bytes, name) in [(&initial, "Anna"), (&changed, "Olga")] {
            warm();
            std::fs::write(&path, bytes).unwrap();
            let WriteInput { calendar: mut data, original, before } =
                views.data_for_write(lock.open_data().unwrap(), true).unwrap();
            assert_eq!(before.unwrap().as_slice(), InputHasher::digest(bytes).as_slice());
            assert_eq!(data.calendar.members[&employee].value, name);
            assert!(views.source.lock().unwrap().full().is_none());
            data.calendar.members.get_mut(&employee).unwrap().value = "Unsaved".into();
            views.restore_after_failure(original);
            let cache = views.source.lock().unwrap();
            if bytes == &initial {
                assert_eq!(
                    cache.full().unwrap().calendar.calendar.members[&employee].value,
                    "Anna"
                );
            } else {
                assert!(cache.full().is_none());
            }
        }
        std::fs::write(&path, &changed).unwrap();
        let cold = views.data_for_write(lock.open_data().unwrap(), false).unwrap();
        assert!(cold.original.is_none() && cold.before.is_none());
        warm();
        std::fs::write(&path, b"{").unwrap();
        assert!(views.data_for_write(lock.open_data().unwrap(), false).is_err());
        assert!(views.source.lock().unwrap().full().is_none());
    }

    #[test]
    fn shared_year_rows_respect_current_source_employee_and_text_policy() {
        use godovoy_plan::storage::online::Access;
        use serde_json::{Value, json};
        let alice = Id::new_v4();
        let bob = Id::new_v4();
        let event = Id::new_v4();
        let date = NaiveDate::from_ymd_opt(2026, 11, 25).unwrap();
        let mut document = json!({"calendar":{"members":{},"plans":{event.to_string():{
            "templates":{"2026":{"revision":{"sequence":1,"device":alice},"value":{
                "title":"A","description":"private text","category":"Team","date":date,
                "yearly":true,"preparation_days":0,"owner":alice,"tasks":[]}}},"years":{}
        }}},"audit":[]});
        let mut rules = json!({"name":"QA","employees":{},"grants":{},"audit":[]});
        let views = Views::default();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("vault");
        let vault = godovoy_plan::storage::online::OnlineVault::init(
            &path,
            "QA".into(),
            Default::default(),
            Default::default(),
        )
        .unwrap();
        let lock = vault.lock().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let compare = |doc: &Value, acl: &Value, employee: Id, year: i32, size: usize| {
            let acl: Access = serde_json::from_value(acl.clone()).unwrap();
            let ctx = || ViewContext::new(&acl, year, employee, false);
            let mut bytes = serde_json::to_vec(doc).unwrap();
            bytes.resize(size, b' ');
            let data = serde_json::from_slice(&bytes).unwrap();
            let expected = serde_json::to_vec(&ctx().render(&data, date)).unwrap();
            std::fs::write(path.join("online-calendar-v1.json"), bytes).unwrap();
            let response = views
                .response(employee, ctx(), date, lock.open_data().unwrap(), &[], false)
                .unwrap();
            let actual = runtime
                .block_on(axum::body::to_bytes(
                    response.into_body(),
                    MAX_SHARED_SOURCE * 2,
                ))
                .unwrap();
            assert_eq!(&actual[..], expected);
            serde_json::from_slice::<Value>(&actual).unwrap()
        };
        for id in [alice, bob, alice] {
            let v = compare(&document, &rules, id, 2026, MIN_SHARED_SOURCE);
            assert_eq!(v["events"][0]["description"], "private text");
        }
        rules["assigned_only"] = json!([bob]);
        assert_eq!(
            compare(&document, &rules, bob, 2026, MIN_SHARED_SOURCE)["events"],
            json!([])
        );
        rules["protection"] = json!({"version":1,"state":{"revision":1,"settings":{
            "russia":true,"belarus":false,"allowRemote":false,"allowFreeText":false
        }},"changeId":Id::new_v4(),"pendingAudit":false});
        for yr in [2026, 2026, 2027, 2026] {
            let v = compare(&document, &rules, alice, yr, MIN_SHARED_SOURCE);
            assert_eq!(v["events"][0]["description"], "");
            assert_eq!(v["events"][0]["year"], yr);
        }
        document["calendar"]["plans"][event.to_string()]["templates"]["2026"]["value"]["title"] =
            json!("B");
        assert_eq!(
            compare(&document, &rules, alice, 2026, MIN_SHARED_SOURCE)["events"][0]["title"],
            "B"
        );
        compare(&document, &rules, alice, 2026, MAX_SHARED_SOURCE + 1);
        assert!(views.source.lock().unwrap().full().is_none());
    }
}
