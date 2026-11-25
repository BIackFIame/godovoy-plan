use std::{
    collections::BTreeSet,
    fs::File,
    io::{self, BufReader, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{Fallible, Id, LocalState, check_profile_file, save};
use crate::import_report::BridgeRun;

const MAX_SOURCES: usize = 32;
const FILE_LIMIT: usize = 16 * 1024;
const REPEAT_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImportStatus {
    pub source: String,
    pub success: Option<ImportSuccess>,
    pub failure: Option<ImportFailure>,
    #[serde(skip)]
    pub bridge_run: Option<BridgeRunStatus>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeRunStatus {
    pub at: DateTime<Utc>,
    pub run: BridgeRun,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RunEntry {
    source: String,
    status: BridgeRunStatus,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImportSuccess {
    pub at: DateTime<Utc>,
    pub received: u16,
    pub changed: u16,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImportFailure {
    pub at: DateTime<Utc>,
    pub kind: ImportResultKind,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ImportResultKind {
    InvalidBatch,
    CalendarRejected,
}

#[derive(Default)]
pub(super) struct View {
    entries: Arc<[ImportStatus]>,
    combined: Arc<[ImportStatus]>,
    warning: bool,
    runs: Vec<RunEntry>,
    run_warning: bool,
}

pub(crate) struct ImportLog {
    path: PathBuf,
    blocked: bool,
    run_path: PathBuf,
    runs_blocked: bool,
    pub(super) view: Arc<RwLock<View>>,
    last_write: Mutex<Option<Instant>>,
    run_write: Mutex<()>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Document<T> {
    device: Id,
    workspace: Id,
    sources: T,
}

pub(crate) fn valid_source(source: &str) -> bool {
    !source.is_empty()
        && source.len() <= 48
        && source
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_')
}

impl ImportLog {
    pub(super) fn open(path: PathBuf, state: &LocalState) -> Self {
        let (mut view, blocked) = match load(&path, state) {
            Ok(entries) => (
                View {
                    entries: entries.into(),
                    ..View::default()
                },
                false,
            ),
            Err(_) => (
                View {
                    warning: true,
                    ..View::default()
                },
                true,
            ),
        };
        let run_path = path.with_file_name("bridge-runs-v1.json");
        let runs_blocked = match load_entries::<RunEntry>(&run_path, state).and_then(|entries| {
            validate_entries(
                &entries,
                |entry| &entry.source,
                |entry| entry.status.run.valid(),
            )?;
            Ok(entries)
        }) {
            Ok(entries) => {
                view.runs = entries;
                false
            }
            Err(_) => {
                view.run_warning = true;
                true
            }
        };
        view.refresh_statuses();
        Self {
            path,
            blocked,
            run_path,
            runs_blocked,
            view: Arc::new(RwLock::new(view)),
            last_write: Mutex::new(None),
            run_write: Mutex::new(()),
        }
    }

    pub(crate) fn record(
        &self,
        device: Id,
        workspace: Id,
        source: &str,
        outcome: Result<(u16, u16), ImportResultKind>,
    ) {
        let mut last_write = self.last_write.lock().unwrap();
        let now = Utc::now();
        let view = self.view.read().unwrap();
        let mut entries = view.entries.to_vec();
        let previous_warning = view.warning;
        drop(view);
        let mut entry = entries
            .iter()
            .position(|entry| entry.source == source)
            .map_or_else(
                || ImportStatus {
                    source: source.into(),
                    success: None,
                    failure: None,
                    bridge_run: None,
                },
                |index| entries.remove(index),
            );
        let last_was_failure = entry
            .failure
            .is_some_and(|failure| entry.success.is_none_or(|success| failure.at >= success.at));
        let repeated = match outcome {
            Ok((received, changed)) => {
                let repeated = changed == 0
                    && !last_was_failure
                    && entry.success.is_some_and(|success| {
                        success.received == received && success.changed == changed
                    });
                entry.success = Some(ImportSuccess {
                    at: now,
                    received,
                    changed,
                });
                repeated
            }
            Err(kind) => {
                let repeated =
                    last_was_failure && entry.failure.is_some_and(|failure| failure.kind == kind);
                entry.failure = Some(ImportFailure { at: now, kind });
                repeated
            }
        };
        entries.insert(0, entry);
        entries.truncate(MAX_SOURCES);
        let warning = if self.blocked {
            true
        } else if repeated && last_write.is_some_and(|at| at.elapsed() < REPEAT_INTERVAL) {
            previous_warning
        } else {
            *last_write = Some(Instant::now());
            check_profile_file(&self.path)
                .and_then(|()| {
                    save(
                        &self.path,
                        &Document {
                            device,
                            workspace,
                            sources: &entries,
                        },
                        FILE_LIMIT,
                    )
                })
                .is_err()
        };
        let mut view = self.view.write().unwrap();
        view.entries = entries.into();
        view.warning = warning;
        view.refresh_statuses();
    }

    pub(crate) fn record_run(
        &self,
        device: Id,
        workspace: Id,
        source: &str,
        run: BridgeRun,
    ) -> bool {
        let _write_guard = self.run_write.lock().unwrap();
        let status = BridgeRunStatus {
            at: Utc::now(),
            run,
        };
        let mut entries = {
            let view = self.view.read().unwrap();
            view.runs
                .iter()
                .filter(|entry| entry.source != source)
                .map(|entry| RunEntry {
                    source: entry.source.clone(),
                    status: entry.status,
                })
                .collect::<Vec<_>>()
        };
        entries.insert(
            0,
            RunEntry {
                source: source.into(),
                status,
            },
        );
        entries.truncate(MAX_SOURCES);
        let warning = self.runs_blocked
            || check_profile_file(&self.run_path)
                .and_then(|()| {
                    save(
                        &self.run_path,
                        &Document {
                            device,
                            workspace,
                            sources: &entries,
                        },
                        FILE_LIMIT,
                    )
                })
                .is_err();
        let mut view = self.view.write().unwrap();
        view.runs = entries;
        view.run_warning = warning;
        view.refresh_statuses();
        !warning
    }
}

impl View {
    pub(super) fn statuses(&self) -> Arc<[ImportStatus]> {
        Arc::clone(&self.combined)
    }

    fn refresh_statuses(&mut self) {
        if self.runs.is_empty() {
            self.combined = Arc::clone(&self.entries);
            return;
        }
        let mut statuses = self.entries.to_vec();
        for entry in &self.runs {
            if let Some(status) = statuses
                .iter_mut()
                .find(|status| status.source == entry.source)
            {
                status.bridge_run = Some(entry.status);
            } else {
                statuses.push(ImportStatus {
                    source: entry.source.clone(),
                    success: None,
                    failure: None,
                    bridge_run: Some(entry.status),
                });
            }
        }
        statuses.sort_by_cached_key(|status| {
            std::cmp::Reverse(
                status
                    .success
                    .map(|item| item.at)
                    .into_iter()
                    .chain(status.failure.map(|item| item.at))
                    .chain(status.bridge_run.map(|item| item.at))
                    .max(),
            )
        });
        self.combined = statuses.into();
    }

    pub(super) fn has_warning(&self) -> bool {
        self.warning || self.run_warning
    }
}

fn load(path: &Path, state: &LocalState) -> Fallible<Vec<ImportStatus>> {
    let entries = load_entries(path, state)?;
    validate_entries(
        &entries,
        |entry: &ImportStatus| &entry.source,
        |entry| {
            (entry.success.is_some() || entry.failure.is_some())
                && entry.success.is_none_or(|success| {
                    success.received as usize <= crate::import_report::BATCH_LIMIT
                        && success.changed <= success.received
                })
        },
    )?;
    Ok(entries)
}

fn load_entries<T: serde::de::DeserializeOwned>(
    path: &Path,
    state: &LocalState,
) -> Fallible<Vec<T>> {
    check_profile_file(path)?;
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    if file.metadata()?.len() > FILE_LIMIT as u64 {
        return Err("Файл результатов импорта слишком большой.".into());
    }
    let mut limited = file.take(FILE_LIMIT as u64 + 1);
    let document: Document<Vec<T>> = serde_json::from_reader(BufReader::new(&mut limited))?;
    if limited.limit() == 0
        || document.device != state.device
        || state
            .workspace
            .as_ref()
            .is_none_or(|space| space.id != document.workspace)
        || document.sources.len() > MAX_SOURCES
    {
        return Err("Файл результатов импорта повреждён или относится к другому профилю.".into());
    }
    Ok(document.sources)
}

fn validate_entries<T>(
    entries: &[T],
    source: impl Fn(&T) -> &str,
    valid: impl Fn(&T) -> bool,
) -> Fallible<()> {
    let mut names = BTreeSet::new();
    if entries.iter().any(|entry| {
        let source = source(entry);
        !valid_source(source) || !names.insert(source) || !valid(entry)
    }) {
        return Err("Файл результатов импорта повреждён.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Store;

    #[test]
    fn profile_open_preserves_damaged_or_foreign_import_log() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile");
        let store = Store::open(&profile).unwrap();
        store
            .lock()
            .unwrap()
            .transaction(|state| state.create_workspace("Команда".into(), "Анна".into()))
            .unwrap();
        let active = store.lock().unwrap().snapshot();
        let device = active.device;
        let workspace = active.workspace.as_ref().unwrap().id;
        drop(store);
        let file = profile.join("imports-v1.json");
        let foreign = serde_json::to_vec(&Document {
            device: Id::new_v4(),
            workspace,
            sources: [ImportStatus {
                source: "foreign".into(),
                success: Some(ImportSuccess {
                    at: Utc::now(),
                    received: 1,
                    changed: 1,
                }),
                failure: None,
                bridge_run: None,
            }],
        })
        .unwrap();
        for bytes in [&b"{ broken"[..], foreign.as_slice()] {
            std::fs::write(&file, bytes).unwrap();
            let store = Store::open_existing(&profile).unwrap();
            let store = store.lock().unwrap();
            assert_eq!(store.state(), &*active);
            let reader = store.reader();
            assert!(reader.import_status().is_empty());
            assert!(reader.import_status_warning());
            store
                .import_log()
                .record(device, workspace, "local", Ok((1, 0)));
            assert_eq!(reader.import_status()[0].source, "local");
            assert!(reader.import_status_warning());
            assert_eq!(std::fs::read(&file).unwrap(), bytes);
        }
    }
}
