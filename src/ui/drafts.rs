use super::*;

pub(super) const MAX_NOTE_DRAFTS: usize = 33;
const CHECKPOINT_INTERVAL: Duration = Duration::from_millis(400);

#[derive(Default)]
pub(super) struct Drafts {
    saved: Option<Document>,
    next_check: Option<Instant>,
    warning: Option<String>,
    retry_write: bool,
    blocked: bool,
    restored: bool,
}

#[derive(Serialize, Deserialize)]
struct Document {
    device: Id,
    workspace: Id,
    editor: Option<EventDraft>,
    notes: Vec<NoteDraft>,
}

#[derive(Serialize, Deserialize)]
struct EventDraft {
    id: Id,
    existing: Option<(i32, Revision)>,
    date_input: String,
    initial_date_input: String,
    event: Event,
    original: Event,
    scope: Scope,
    initial_scope: Scope,
}

struct LiveDrafts<'a> {
    device: Id,
    workspace: Id,
    editor: Option<&'a Editor>,
    parked: &'a [NoteDraft],
    active: Option<ActiveNote<'a>>,
}

struct ActiveNote<'a> {
    event: (Id, i32),
    text: &'a str,
    baseline: &'a str,
    merging: bool,
}

impl LiveDrafts<'_> {
    fn matches(&self, saved: &Document) -> bool {
        if self.device != saved.device || self.workspace != saved.workspace {
            return false;
        }
        let same_editor = match (self.editor, &saved.editor) {
            (None, None) => true,
            (Some(live), Some(saved)) => saved.matches_editor(live),
            _ => false,
        };
        same_editor
            && match &self.active {
                None => self.parked == saved.notes.as_slice(),
                Some(active) => saved.notes.split_last().is_some_and(|(last, parked)| {
                    self.parked == parked
                        && last.event == active.event
                        && last.text == active.text
                        && last.baseline == active.baseline
                        && last.merging == active.merging
                }),
            }
    }

    fn document(self) -> Document {
        let mut notes = self.parked.to_vec();
        if let Some(active) = self.active {
            notes.push(NoteDraft {
                event: active.event,
                text: active.text.to_owned(),
                baseline: active.baseline.to_owned(),
                merging: active.merging,
            });
        }
        Document {
            device: self.device,
            workspace: self.workspace,
            editor: self.editor.map(EventDraft::from),
            notes,
        }
    }
}

impl From<&Editor> for EventDraft {
    fn from(editor: &Editor) -> Self {
        Self {
            id: editor.id,
            existing: editor.existing,
            date_input: editor.date_input.clone(),
            initial_date_input: editor.initial_date_input.clone(),
            event: editor.event.clone(),
            original: editor.original.clone(),
            scope: editor.scope,
            initial_scope: editor.initial_scope,
        }
    }
}

impl EventDraft {
    fn matches_editor(&self, editor: &Editor) -> bool {
        self.id == editor.id
            && self.existing == editor.existing
            && self.date_input == editor.date_input
            && self.initial_date_input == editor.initial_date_input
            && self.event == editor.event
            && self.original == editor.original
            && self.scope == editor.scope
            && self.initial_scope == editor.initial_scope
    }

    fn editor(&self, calendar: &Calendar) -> Option<Editor> {
        let editor = Editor {
            id: self.id,
            existing: self.existing,
            date_input: self.date_input.clone(),
            initial_date_input: self.initial_date_input.clone(),
            event: self.event.clone(),
            original: self.original.clone(),
            scope: self.scope,
            initial_scope: self.initial_scope,
            error: None,
            confirmation: None,
            focus_title: true,
            focus_new_task: false,
        };
        let committed = match editor.existing {
            None => calendar.plans.contains_key(&editor.id),
            Some((year, expected)) => calendar.event(editor.id, year).is_some_and(|current| {
                current.revision != expected
                    && editor
                        .validated()
                        .is_ok_and(|(_, event)| current.event == &event)
                    && (editor.scope == Scope::ThisYear) == current.overridden
            }),
        };
        (editor.has_changes() && !committed).then_some(editor)
    }
}

impl Document {
    fn matches(&self, state: &LocalState) -> bool {
        state.device == self.device
            && state
                .workspace
                .as_ref()
                .is_some_and(|workspace| workspace.id == self.workspace)
    }

    fn within_limits(&self) -> bool {
        let mut events = BTreeSet::new();
        self.notes.len() <= MAX_NOTE_DRAFTS
            && self.notes.iter().all(|draft| {
                (1900..=9999).contains(&draft.event.1)
                    && draft.text.chars().count() <= MAX_NOTE_CHARS
                    && draft.baseline.chars().count() <= MAX_NOTE_CHARS
                    && events.insert(draft.event)
            })
            && self.editor.as_ref().is_none_or(|draft| {
                draft
                    .existing
                    .is_none_or(|(year, _)| (1900..=9999).contains(&year))
                    && draft.date_input.chars().count() <= 32
                    && draft.initial_date_input.chars().count() <= 32
                    && [&draft.event, &draft.original]
                        .into_iter()
                        .all(draft_event_within_limits)
            })
    }
}

fn draft_event_within_limits(event: &Event) -> bool {
    let mut task_ids = BTreeSet::new();
    (1900..=9999).contains(&event.date.year())
        && event.title.chars().count() <= MAX_TITLE_CHARS
        && event.description.chars().count() <= MAX_DESCRIPTION_CHARS
        && event.preparation_days <= MAX_PREPARATION_DAYS
        && event.tasks.len() <= godovoy_plan::model::MAX_TASKS
        && event.tasks.iter().all(|task| {
            task.title.chars().count() <= MAX_TITLE_CHARS
                && task.days_before <= event.preparation_days
                && task_ids.insert(task.id)
        })
}

impl App {
    pub(super) fn restore_local_drafts(&mut self) {
        let snapshot = self.reader.snapshot();
        let loaded: Fallible<Option<Document>> = self.store.lock().unwrap().load_ui_drafts();
        let document = match loaded {
            Ok(Some(document)) if document.matches(&snapshot.state) && document.within_limits() => {
                document
            }
            Ok(None) => return,
            Ok(Some(_)) => {
                self.drafts.blocked = true;
                self.drafts.warning =
                    Some("Локальные черновики принадлежат другому профилю или повреждены.".into());
                return;
            }
            Err(error) => {
                self.drafts.blocked = true;
                self.drafts.warning =
                    Some(format!("Не удалось открыть локальные черновики. {error}"));
                return;
            }
        };
        let Some(workspace) = &snapshot.state.workspace else {
            return;
        };
        self.editor = document
            .editor
            .as_ref()
            .and_then(|draft| draft.editor(&workspace.calendar));
        self.note_drafts = document
            .notes
            .iter()
            .filter(|draft| {
                draft.text != draft.baseline
                    && workspace
                        .calendar
                        .follow_event(draft.event.0, draft.event.1)
                        .is_none_or(|event| event.note() != draft.text.trim())
            })
            .cloned()
            .collect();
        self.follow_note_drafts(&workspace.calendar);
        if let Some((id, year)) = self.note_drafts.iter().rev().find_map(|draft| {
            workspace
                .calendar
                .event(draft.event.0, draft.event.1)
                .map(|_| draft.event)
        }) {
            self.open_event(year, id);
        }
        self.drafts.restored = self.has_unsaved_changes();
        self.drafts.saved = Some(document);
    }

    fn live_drafts(&self) -> Option<LiveDrafts<'_>> {
        if !self.has_unsaved_changes() {
            return None;
        }
        let state = self.reader.snapshot().state;
        let workspace = state.workspace.as_ref()?.id;
        Some(LiveDrafts {
            device: state.device,
            workspace,
            editor: self.editor.as_ref().filter(|editor| editor.has_changes()),
            parked: &self.note_drafts,
            active: self
                .note_event
                .filter(|_| self.note != self.note_baseline)
                .map(|event| ActiveNote {
                    event,
                    text: &self.note,
                    baseline: &self.note_baseline,
                    merging: self.note_merging,
                }),
        })
    }

    pub fn flush_local_drafts(&mut self) -> Fallible<()> {
        self.flush_draft_changes().map(|_| ())
    }

    fn flush_draft_changes(&mut self) -> Fallible<bool> {
        let next = self.live_drafts();
        if self.drafts.blocked {
            if next.is_none() {
                return Ok(false);
            }
            return Err(self.drafts.warning.clone().unwrap_or_default().into());
        }
        let unchanged = match (&next, &self.drafts.saved) {
            (None, None) => true,
            (Some(live), Some(saved)) => live.matches(saved),
            _ => false,
        };
        if !unchanged || self.drafts.retry_write {
            let next = next.map(LiveDrafts::document);
            if next.as_ref().is_some_and(|drafts| !drafts.within_limits()) {
                let error = "Локальные черновики превышают допустимый размер.";
                self.drafts.warning = Some(error.into());
                return Err(error.into());
            }
            self.persist_local_drafts(next.as_ref())?;
            self.drafts.saved = next;
            return Ok(true);
        }
        Ok(false)
    }

    fn persist_local_drafts(&mut self, next: Option<&Document>) -> Fallible<()> {
        let result = self.store.lock().unwrap().save_ui_drafts(next);
        // A failed directory flush can leave the new value visible. Reconcile even if
        // the employee types the previous value again, or discards an unsaved draft.
        self.drafts.retry_write = result.is_err();
        self.drafts.warning = result
            .as_ref()
            .err()
            .map(|error| format!("Не удалось сохранить локальные черновики. {error}"));
        result
    }

    pub fn discard_local_drafts(&mut self) -> Fallible<()> {
        if !self.drafts.blocked && (self.drafts.saved.is_some() || self.drafts.retry_write) {
            self.persist_local_drafts(None)?;
        }
        self.drafts.saved = None;
        self.editor = None;
        self.take_saved_note("");
        self.note_event = None;
        self.note_drafts.clear();
        Ok(())
    }

    pub(super) fn checkpoint_local_drafts(&mut self, ctx: &egui::Context) {
        if self.drafts.blocked
            || (!self.drafts.retry_write
                && !self.has_unsaved_changes()
                && self.drafts.saved.is_none())
        {
            return;
        }
        let now = Instant::now();
        if self.drafts.next_check.is_none_or(|next| next <= now) {
            self.drafts.next_check = match self.flush_draft_changes() {
                Ok(false) => None,
                Ok(true) => Some(now + CHECKPOINT_INTERVAL),
                Err(_) => Some(now + Duration::from_secs(2)),
            };
        }
        if let Some(next) = self.drafts.next_check {
            ctx.request_repaint_after(next.saturating_duration_since(now));
        }
    }

    fn follow_note_drafts(&mut self, calendar: &Calendar) {
        let mut active = self.note_event.filter(|_| self.note != self.note_baseline);
        if let Some(key) = active
            && let Some(destination) = moved_note_key(calendar, key)
            && self
                .note_drafts
                .iter()
                .all(|draft| draft.event != destination)
        {
            self.note_event = Some(destination);
            active = Some(destination);
        }
        // Prefer an exact key. Otherwise use the order of restored drafts.
        for index in (0..self.note_drafts.len()).rev() {
            let Some(destination) = moved_note_key(calendar, self.note_drafts[index].event) else {
                continue;
            };
            if active == Some(destination)
                || self
                    .note_drafts
                    .iter()
                    .any(|draft| draft.event == destination)
            {
                continue;
            }
            self.note_drafts[index].event = destination;
            if self.note_event == Some(destination) {
                self.note_event = None;
            }
        }
    }

    pub(super) fn park_active_note(&mut self) {
        let Some(event) = self.note_event.filter(|_| self.note != self.note_baseline) else {
            return;
        };
        self.note_drafts.push(NoteDraft {
            event,
            text: std::mem::take(&mut self.note),
            baseline: std::mem::take(&mut self.note_baseline),
            merging: self.note_merging,
        });
        self.note_event = None;
        self.note_merging = false;
    }

    pub(super) fn draft_banner(&mut self, ui: &mut Ui, calendar: &Calendar) {
        self.follow_note_drafts(calendar);
        if self.note_event.is_some_and(|event| {
            self.note != self.note_baseline && calendar.event(event.0, event.1).is_none()
        }) {
            self.park_active_note();
        }
        if let Some(warning) = &self.drafts.warning {
            ui.colored_label(RED, warning);
        } else if self.drafts.restored && self.has_unsaved_changes() {
            muted(ui, "Локальные черновики восстановлены.");
        }
        let active = self.note_event.filter(|_| self.note != self.note_baseline);
        let count = self.note_drafts.len() + usize::from(active.is_some());
        if count == 0 {
            return;
        }

        let mut open = None;
        let mut remove = None;
        egui::CollapsingHeader::new(format!("Черновики заметок · {count} из {MAX_NOTE_DRAFTS}"))
            .id_salt("local-note-drafts")
            .show(ui, |ui| {
                ScrollArea::vertical()
                    .id_salt("local-note-draft-rows")
                    .max_height(136.0)
                    .show(ui, |ui| {
                        let drafts = active
                            .into_iter()
                            .map(|key| (key, self.note.as_str()))
                            .chain(
                                self.note_drafts
                                    .iter()
                                    .map(|draft| (draft.event, draft.text.as_str())),
                            );
                        for (key, text) in drafts {
                            let (id, year) = key;
                            let event = calendar.follow_event(id, year);
                            let name = event
                                .map_or("Недоступное событие", |event| {
                                    event.event.title.as_str()
                                });
                            ui.push_id(key, |ui| {
                                ui.horizontal_wrapped(|ui| {
                                    let label = match event {
                                        Some(moved) if moved.year != year => {
                                            format!("{name} · {year} → {}", moved.year)
                                        }
                                        _ => format!("{name} · {year}"),
                                    };
                                    ui.add(egui::Label::new(&label).truncate())
                                        .on_hover_text(label);
                                    if event.is_some_and(|event| event.year == year)
                                        && ui.button("Открыть").clicked()
                                    {
                                        open = Some(key);
                                    }
                                    if ui.button("Скопировать черновик").clicked()
                                    {
                                        ui.ctx().copy_text(text.to_owned());
                                    }
                                    if ui.button("Удалить черновик").clicked() {
                                        remove = Some(key);
                                    }
                                });
                                let preview = text
                                    .lines()
                                    .map(str::trim)
                                    .find(|line| !line.is_empty())
                                    .unwrap_or("Без текста");
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(preview).color(MUTED).size(12.0),
                                    )
                                    .truncate(),
                                );
                            });
                        }
                    });
            });
        if let Some(key) = remove {
            self.remove_note_draft(calendar, key);
        } else if let Some((id, year)) = open {
            self.open_event(year, id);
        }
    }

    fn remove_note_draft(&mut self, calendar: &Calendar, key: (Id, i32)) {
        if self.note_event == Some(key) && self.note != self.note_baseline {
            let saved = calendar.event(key.0, key.1).map_or("", Occurrence::note);
            self.replace_note_draft(saved);
        } else if let Some(index) = self.note_drafts.iter().position(|draft| draft.event == key) {
            let draft = self.note_drafts.remove(index);
            if self.flush_local_drafts().is_err() {
                self.note_drafts.insert(index, draft);
            }
        }
    }
}

fn moved_note_key(calendar: &Calendar, key: (Id, i32)) -> Option<(Id, i32)> {
    let event = calendar.follow_event(key.0, key.1)?;
    (event.year != key.1).then_some((event.id, event.year))
}

#[cfg(test)]
mod tests {
    use super::*;
    use godovoy_plan::storage::Store;
    use std::{fs, path::Path};

    fn open_team(path: &Path) -> App {
        let store = Store::open(path).unwrap();
        if store.lock().unwrap().state().workspace.is_none() {
            store
                .lock()
                .unwrap()
                .transaction(|state| {
                    state.create_workspace("Тестовая команда".into(), "Анна".into())
                })
                .unwrap();
        }
        App::new(store, Err("Network inactive in test".into()), false)
    }

    fn planned(today: NaiveDate) -> Editor {
        let mut editor = Editor::new(today.year(), today);
        editor.event.title = "Тестовое событие".into();
        editor
    }

    fn moved_import(app: &App, old_year: i32, new_year: i32) -> Id {
        let id = Id::new_v4();
        let date = NaiveDate::from_ymd_opt(new_year, 1, 4).unwrap();
        let event = planned(date).event;
        app.store
            .lock()
            .unwrap()
            .transaction(|state| {
                let revision = state.revision()?;
                let plan = state.workspace_mut()?.calendar.plans.entry(id).or_default();
                plan.source = Some(godovoy_plan::model::ExternalSource {
                    name: "probe".into(),
                    external_id: id.to_string(),
                });
                plan.templates.insert(old_year, Record::new(revision, None));
                plan.templates
                    .insert(new_year, Record::new(revision, Some(event.into())));
                Ok(())
            })
            .unwrap();
        id
    }

    #[test]
    fn moved_import_relinks_active_drafts_and_retains_other_drafts_on_collision() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = open_team(&temp.path().join("profile"));
        let active = moved_import(&app, 2026, 2027);
        let collision = moved_import(&app, 2026, 2027);
        app.load_note((active, 2026), "База активной заметки");
        app.note = "Активный черновик".into();
        app.note_merging = true;
        for (year, text) in [
            (2026, "Черновик до переноса"),
            (2027, "Черновик после переноса"),
        ] {
            app.note_drafts.push(NoteDraft {
                event: (collision, year),
                text: text.into(),
                baseline: String::new(),
                merging: false,
            });
        }
        let snapshot = app.reader.snapshot();
        let calendar = &snapshot.state.workspace.as_ref().unwrap().calendar;
        let parked = app.note_drafts.clone();
        app.follow_note_drafts(calendar);
        assert_eq!(app.note_event, Some((active, 2027)));
        assert_eq!(app.note, "Активный черновик");
        assert_eq!(app.note_baseline, "База активной заметки");
        assert!(app.note_merging);
        assert!(app.note_drafts == parked);

        app.load_note((collision, 2027), "");
        assert_eq!(app.note, "Черновик после переноса");
        app.replace_note_draft("");
        app.follow_note_drafts(calendar);
        assert_eq!(app.note_event, None);
        app.load_note((collision, 2027), "");
        assert_eq!(app.note, "Черновик до переноса");
        assert!(
            app.note_drafts
                .iter()
                .any(|draft| draft.event == (active, 2027))
        );
    }

    #[test]
    fn moved_import_restores_a_draft_with_the_new_year_and_the_same_merge_base() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("profile");
        let mut app = open_team(&path);
        let event = moved_import(&app, 2027, 2026);
        app.load_note((event, 2027), "База объединения");
        app.note = "Черновик до переноса назад".into();
        app.note_merging = true;
        app.flush_local_drafts().unwrap();
        drop(app);

        let mut reopened = open_team(&path);
        assert_eq!(reopened.selected, Some(event));
        assert_eq!(reopened.year, 2026);
        assert_eq!(reopened.note_drafts.len(), 1);
        assert_eq!(reopened.note_drafts[0].event, (event, 2026));
        reopened.load_note((event, 2026), "");
        assert_eq!(reopened.note, "Черновик до переноса назад");
        assert_eq!(reopened.note_baseline, "База объединения");
        assert!(reopened.note_merging);
        reopened.flush_local_drafts().unwrap();
        let saved: Document = reopened
            .store
            .lock()
            .unwrap()
            .load_ui_drafts()
            .unwrap()
            .unwrap();
        assert_eq!(saved.notes[0].event, (event, 2026));
    }

    #[test]
    fn deleting_one_note_draft_preserves_other_years_and_the_saved_note() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("profile");
        let mut app = open_team(&path);
        let today = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap();
        let mut editor = planned(today);
        editor.event.yearly = true;
        let year = app.save_event(&editor, false).unwrap();
        let missing = (Id::new_v4(), year);
        let parked = (editor.id, year);
        let active = (editor.id, year + 1);
        app.load_note(missing, "");
        app.note = "К удалённому событию".into();
        app.load_note(parked, "");
        app.note = "К прошлому году".into();
        app.load_note(active, "");
        app.note = "Записано в календаре".into();
        app.save_note(active.0, active.1, "");
        app.note = "Активный черновик".into();
        app.flush_local_drafts().unwrap();

        let saved_file = path.join("calendar.json");
        let before = fs::read(&saved_file).unwrap();
        let snapshot = app.reader.snapshot();
        let calendar = &snapshot.state.workspace.as_ref().unwrap().calendar;
        app.remove_note_draft(calendar, parked);
        assert_eq!(app.note, "Активный черновик");
        assert_eq!(app.note_drafts.len(), 1);
        assert_eq!(app.note_drafts[0].event, missing);

        app.remove_note_draft(calendar, active);
        assert_eq!(app.note, "Записано в календаре");
        assert_eq!(app.note, app.note_baseline);
        assert_eq!(fs::read(&saved_file).unwrap(), before);
        drop(app);

        let reopened = open_team(&path);
        assert_eq!(reopened.note_drafts.len(), 1);
        assert_eq!(reopened.note_drafts[0].event, missing);
        assert_eq!(reopened.note_drafts[0].text, "К удалённому событию");
    }

    #[test]
    fn failed_checkpoint_is_reconciled_even_after_reverting_to_no_draft() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join("profile");
        let mut app = open_team(&profile);
        let file = profile.join("drafts-v1.json");
        fs::create_dir(&file).unwrap();
        app.editor = Some(planned(NaiveDate::from_ymd_opt(2026, 11, 23).unwrap()));
        assert!(app.flush_local_drafts().is_err());
        let visible = app.live_drafts().unwrap().document();
        fs::remove_dir(&file).unwrap();
        // Simulate the visible replacement which may precede a failed directory flush.
        app.store
            .lock()
            .unwrap()
            .save_ui_drafts(Some(&visible))
            .unwrap();
        app.editor = None;
        assert!(app.flush_draft_changes().unwrap());
        assert!(!file.exists());
        assert!(!app.drafts.retry_write);
        assert!(!app.flush_draft_changes().unwrap());
    }

    #[test]
    fn reopens_incomplete_event_and_note_with_original_baseline() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("profile");
        let today = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap();
        let mut app = open_team(&path);
        let event = planned(today);
        let year = app.save_event(&event, false).unwrap();
        app.load_note((event.id, year), "");
        app.note = "Мой черновик".into();
        let mut editor = Editor::new(year, today);
        editor.date_input = "12.".into();
        editor.event.tasks.push(Task {
            id: Id::new_v4(),
            title: String::new(),
            days_before: 0,
        });
        app.editor = Some(editor);
        app.flush_local_drafts().unwrap();
        app.store
            .lock()
            .unwrap()
            .transaction(|state| {
                let revision = state.revision()?;
                state
                    .workspace_mut()?
                    .calendar
                    .plans
                    .get_mut(&event.id)
                    .unwrap()
                    .years
                    .entry(year)
                    .or_default()
                    .note = Some(Record::new(revision, "Правка коллеги".into()));
                Ok(())
            })
            .unwrap();
        drop(app);

        let mut reopened = open_team(&path);
        let draft = reopened.editor.as_ref().unwrap();
        assert!(draft.event.title.is_empty() && draft.event.tasks[0].title.is_empty());
        assert_eq!(draft.date_input, "12.");
        reopened.load_note((event.id, year), "Правка коллеги");
        assert_eq!(
            (&*reopened.note, &*reopened.note_baseline),
            ("Мой черновик", "")
        );
        reopened.save_note(event.id, year, "Правка коллеги");
        assert_eq!(reopened.note, "Мой черновик");
        reopened.discard_local_drafts().unwrap();
        assert!(!path.join("drafts-v1.json").exists());
        drop(reopened);
        assert!(!open_team(&path).has_unsaved_changes());
    }

    #[test]
    fn completed_new_event_is_not_restored_or_overwritten_after_interruption() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("profile");
        let today = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap();
        let mut app = open_team(&path);
        app.editor = Some(planned(today));
        app.flush_local_drafts().unwrap();
        let editor = app.editor.as_ref().unwrap();
        let year = app.save_event(editor, false).unwrap();
        assert!(app.save_event(editor, false).is_err());
        let id = editor.id;
        drop(app);

        let mut reopened = open_team(&path);
        assert!(reopened.editor.is_none());
        assert!(
            reopened
                .reader
                .snapshot()
                .state
                .workspace
                .as_ref()
                .unwrap()
                .calendar
                .event(id, year)
                .is_some()
        );
        reopened.flush_local_drafts().unwrap();
        assert!(!path.join("drafts-v1.json").exists());
    }

    #[test]
    fn damaged_drafts_are_preserved_and_calendar_remains_available() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("profile");
        drop(open_team(&path));
        let file = path.join("drafts-v1.json");
        fs::write(&file, b"{ damaged").unwrap();
        let mut app = open_team(&path);
        assert!(app.drafts.warning.is_some());
        let today = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap();
        app.editor = Some(planned(today));
        assert!(app.flush_local_drafts().is_err());
        assert!(app.save_event(app.editor.as_ref().unwrap(), false).is_ok());
        assert_eq!(fs::read(file).unwrap(), b"{ damaged");
    }

    #[test]
    fn draft_with_invalid_scheduling_fields_is_not_opened() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("profile");
        let mut app = open_team(&path);
        let today = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap();
        app.editor = Some(planned(today));
        let mut document = app.live_drafts().unwrap().document();
        document.editor.as_mut().unwrap().event.preparation_days = 367;
        app.store
            .lock()
            .unwrap()
            .save_ui_drafts(Some(&document))
            .unwrap();
        drop(app);
        let file = path.join("drafts-v1.json");
        let original = fs::read(&file).unwrap();

        let reopened = open_team(&path);
        assert!(reopened.editor.is_none());
        assert!(reopened.drafts.warning.is_some());
        assert!(reopened.save_event(&planned(today), false).is_ok());
        assert_eq!(fs::read(file).unwrap(), original);
    }
}
