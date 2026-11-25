use std::{
    collections::BTreeSet,
    net::SocketAddrV4,
    path::PathBuf,
    sync::{
        Arc, Weak,
        mpsc::{Receiver, TryRecvError, sync_channel},
    },
    thread,
    time::{Duration, Instant},
};

use chrono::{Datelike, Days, Local, Months, NaiveDate, TimeZone};
use eframe::egui::{
    self, Align, Button, ComboBox, DragValue, Frame, Layout, Margin, Panel, RichText, ScrollArea,
    Stroke, TextEdit, Ui, Vec2, vec2,
};
use godovoy_plan::{
    Fallible,
    api::{self, BridgeOutcome},
    exchange::{EXCHANGE_EXTENSION, export_file, import_file},
    model::{Calendar, Category, Event, Id, Occurrence, Record, Revision, Task, YearChange},
    network::{Config, Network, SYNC_PORT},
    planning::{self, AgendaItem, Horizon, Milestone, OwnerFilter, Reminder},
    storage::{
        BackupEntry, BridgeRunStatus, CALENDAR_MESSAGE_LIMIT, ImportResultKind, ImportStatus,
        LocalState, ProfileSizeLimit, ProfileUsage, ReminderRecovery, ReminderStore, RestoreResult,
        SharedStore, Store, StoreReader, Workspace,
    },
};
use serde::{Deserialize, Serialize};

use crate::{
    autostart,
    chart::{draw_marker, legend_marker, year_wheel},
    file_picker::{pick_exchange_to_open, pick_exchange_to_save, reveal_exchange_file},
    notifications::{Delivery, TestStatus},
    theme::{
        self, ACCENT, ACTION_TEXT, BACK, BORDER, DUE_TODAY, GREEN, INK, MUTED, NAV,
        PAGE_TITLE_SIZE, RED, SECTION_TITLE_SIZE, TINT, brand, card, dialog, heading_font, marker,
        muted, nav, page_frame, primary, quiet, section_heading, semibold, title, view_option,
        workspace_brand,
    },
};

mod api_controls;
mod drafts;
mod editor_fields;
use editor_fields::editor_fields;
mod hosted;
mod people;
mod reminder_work;
use api_controls::{Action as ApiAction, ApiControls};
use drafts::{Drafts, MAX_NOTE_DRAFTS};
use people::{DeviceAssignments, employee_options};
use reminder_work::{ReminderWork, Status as ReminderWorkStatus};

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Calendar,
    Tasks,
    Agenda,
    Reminders,
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
enum Scope {
    ThisYear,
    Future,
}

struct Editor {
    id: Id,
    existing: Option<(i32, Revision)>,
    date_input: String,
    initial_date_input: String,
    event: Event,
    original: Event,
    scope: Scope,
    initial_scope: Scope,
    error: Option<String>,
    confirm_remove: bool,
    confirm_discard: bool,
    focus_title: bool,
    focus_new_task: bool,
}

impl Editor {
    fn new(year: i32, today: NaiveDate) -> Self {
        let date = if today.year() == year {
            today
        } else {
            NaiveDate::from_ymd_opt(year, 1, 1).unwrap()
        };
        Self::with_event(
            Id::new_v4(),
            None,
            date,
            Event {
                title: String::new(),
                description: String::new(),
                category: Category::Campaign,
                date,
                yearly: false,
                preparation_days: 0,
                owner: None,
                tasks: Vec::new(),
            },
            Scope::Future,
        )
    }

    fn edit(occurrence: Occurrence<'_>) -> Self {
        let scope = if occurrence.event.yearly {
            Scope::ThisYear
        } else {
            Scope::Future
        };
        Self::with_event(
            occurrence.id,
            Some((occurrence.year, occurrence.revision)),
            occurrence.date,
            occurrence.event.clone(),
            scope,
        )
    }

    fn from_template(occurrence: Occurrence<'_>) -> Self {
        let mut event = occurrence.event.clone();
        event.date = occurrence.date;
        Self::new_copy(event, occurrence.date.format("%d.%m.%Y").to_string())
    }

    fn into_new_copy(mut self) -> Self {
        if let Ok(date) = self
            .parsed_date()
            .or_else(|_| NaiveDate::parse_from_str(&self.initial_date_input, "%d.%m.%Y"))
            && (1900..=9999).contains(&date.year())
        {
            self.event.date = date;
        }
        Self::new_copy(self.event, self.date_input)
    }

    fn new_copy(mut event: Event, date_input: String) -> Self {
        let mut draft = Self::new(event.date.year(), event.date);
        for task in &mut event.tasks {
            task.id = Id::new_v4();
        }
        draft.event = event;
        draft.date_input = date_input;
        draft
    }

    fn original_issue(&self, calendar: &Calendar) -> Option<&'static str> {
        let (year, expected) = self.existing?;
        match calendar.event(self.id, year) {
            Some(current) if current.revision == expected => None,
            Some(_) => Some("Исходное событие изменилось."),
            None if calendar.follow_event(self.id, year).is_some() => {
                Some("Исходное событие перенесено на другой год.")
            }
            None => Some("Исходное событие больше недоступно."),
        }
    }

    fn with_event(
        id: Id,
        existing: Option<(i32, Revision)>,
        date: NaiveDate,
        event: Event,
        scope: Scope,
    ) -> Self {
        let date_input = date.format("%d.%m.%Y").to_string();
        Self {
            id,
            existing,
            initial_date_input: date_input.clone(),
            date_input,
            original: event.clone(),
            event,
            scope,
            initial_scope: scope,
            error: None,
            confirm_remove: false,
            confirm_discard: false,
            focus_title: existing.is_none(),
            focus_new_task: false,
        }
    }

    fn has_changes(&self) -> bool {
        self.date_input.trim() != self.initial_date_input
            || self.event != self.original
            || self.scope != self.initial_scope
    }

    fn parsed_date(&self) -> Result<NaiveDate, chrono::ParseError> {
        NaiveDate::parse_from_str(self.date_input.trim(), "%d.%m.%Y")
            .or_else(|_| NaiveDate::parse_from_str(self.date_input.trim(), "%Y-%m-%d"))
    }

    fn validated(&self) -> Fallible<(i32, Event)> {
        let date = self
            .parsed_date()
            .map_err(|_| "Введите дату в формате ДД.ММ.ГГГГ.")?;
        if !(1900..=9999).contains(&date.year()) {
            return Err("Укажите год от 1900 до 9999.".into());
        }
        if self.existing.is_some_and(|(year, _)| date.year() != year) {
            return Err("Для этого события можно менять день и месяц в выбранном году.".into());
        }
        let mut event = self.event.clone();
        event.title = event.title.trim().to_owned();
        if event.title.is_empty() || event.title.chars().count() > 160 {
            return Err("Введите название длиной от 1 до 160 символов.".into());
        }
        if self.date_input.trim() != self.initial_date_input || self.existing.is_none() {
            event.date = date;
        }
        for task in &mut event.tasks {
            task.title = task.title.trim().to_owned();
            if task.title.is_empty() || task.title.chars().count() > 160 {
                return Err("Заполните названия всех пунктов чек-листа, до 160 символов.".into());
            }
            if task.days_before > event.preparation_days {
                return Err("Пункт чек-листа не может быть раньше начала подготовки.".into());
            }
        }
        event.validate()?;
        Ok((date.year(), event))
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct NoteDraft {
    event: (Id, i32),
    text: String,
    baseline: String,
    merging: bool,
}

#[derive(Clone)]
pub(crate) struct CalendarRow {
    pub(crate) id: Id,
    pub(crate) date: NaiveDate,
    pub(crate) title: String,
    pub(crate) category: Category,
}

struct CalendarView {
    snapshot: Weak<Workspace>,
    year: i32,
    search: String,
    category: Option<Category>,
    rows: Arc<[CalendarRow]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TaskRow {
    event: Id,
    year: i32,
    task: Option<usize>,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum TaskFilter {
    All,
    TodayAndTomorrow,
    Overdue,
}

struct TaskRows {
    filter: TaskFilter,
    rows: Arc<[TaskRow]>,
    open_count: usize,
}

struct PersonalView {
    device: Id,
    snapshot: Weak<Workspace>,
    receipts: Weak<BTreeSet<Reminder>>,
    today: NaiveDate,
    events: Vec<(Id, i32)>,
    task_rows: Option<TaskRows>,
    pending: Arc<[Reminder]>,
    open_count: usize,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum AgendaPeriod {
    Upcoming(Horizon),
    Overdue,
}

struct AgendaView {
    snapshot: Weak<Workspace>,
    today: NaiveDate,
    period: AgendaPeriod,
    owner: OwnerFilter,
    rows: Arc<[AgendaItem]>,
}

struct Startup {
    executable: PathBuf,
    status: autostart::Status,
}

enum CapacityPrompt {
    Open {
        previous_focus: Option<egui::Id>,
        focus_close: bool,
    },
    RestoreFocus(egui::Id),
}

#[derive(Default)]
struct FileExchange {
    path: String,
    dropped: Option<PathBuf>,
    exported: Option<PathBuf>,
    job: Option<Receiver<Fallible<ExchangeResult>>>,
    status: Option<(String, bool)>,
}

#[derive(Clone, Copy)]
enum ExchangeOperation {
    Export,
    Import,
    Reveal,
}

enum ExchangeRequest {
    PickExport,
    PickImport,
    Use(ExchangeOperation, PathBuf),
}

enum ExchangeResult {
    Exported(PathBuf),
    Imported { changed: bool },
    Revealed,
}

#[derive(Default)]
enum BackupCreation {
    #[default]
    Idle,
    Writing(Receiver<Fallible<BackupEntry>>),
    Finished {
        message: String,
        error: bool,
        notice: bool,
    },
}

pub enum RestoreRequest {
    Choose,
    ReturnTo(BackupEntry),
}

enum DesktopTransport {
    Waiting(Config),
    Started(Result<Network, String>),
}
impl DesktopTransport {
    fn start(&mut self, store: &SharedStore) {
        if let Self::Waiting(config) = self {
            *self = Self::Started(crate::startup::start_network(store.clone(), *config));
        }
    }
    fn enable(&mut self, store: &SharedStore) -> Fallible<&Network> {
        store.lock().unwrap().enable_lan()?;
        self.start(store);
        self.as_ref().map_err(Into::into)
    }
    fn as_ref(&self) -> Result<&Network, &str> {
        match self {
            Self::Waiting(_) => Err("Обмен ещё не включён."),
            Self::Started(result) => result.as_ref().map_err(String::as_str),
        }
    }
}

pub struct App {
    store: SharedStore,
    reader: StoreReader,
    network: DesktopTransport,
    page: Page,
    year: i32,
    selected: Option<Id>,
    reveal_event: bool,
    search: String,
    search_focus: bool,
    category: Option<Category>,
    show_circle: bool,
    integrations_open: bool,
    confirm_token_change: bool,
    api_controls: ApiControls,
    note_event: Option<(Id, i32)>,
    note: String,
    note_baseline: String,
    note_merging: bool,
    note_drafts: Vec<NoteDraft>,
    editor: Option<Editor>,
    capacity_prompt: Option<CapacityPrompt>,
    drafts: Drafts,
    backup_open: bool,
    backup_folder: Option<PathBuf>,
    latest_backup: Result<Option<BackupEntry>, String>,
    backup_creation: BackupCreation,
    backup_status: Option<(String, bool)>,
    restore_result: Option<RestoreResult>,
    restore_requested: Option<RestoreRequest>,
    connections: bool,
    confirm_access_renewal: bool,
    selected_revocations: BTreeSet<Id>,
    device_assignments: DeviceAssignments,
    access_work: bool,
    pub renewed_profile: Option<std::path::PathBuf>,
    sync_message: Option<(String, bool)>,
    file_exchange: FileExchange,
    address: String,
    join_code: String,
    pairing_request: String,
    company: String,
    employee: String,
    hosted_entry: hosted::HostedEntry,
    join_mode: bool,
    onboarding_focus: bool,
    join_result: Option<Receiver<Fallible<()>>>,
    message: Option<(String, bool)>,
    startup: Result<Startup, String>,
    startup_error: Option<String>,
    calendar_view: Option<CalendarView>,
    personal: Option<PersonalView>,
    task_filter: TaskFilter,
    agenda: Option<AgendaView>,
    agenda_overdue: bool,
    horizon: Horizon,
    owner_filter: OwnerFilter,
    notification: Delivery,
    next_reminder_check: Instant,
    reminder_store: Arc<ReminderStore>,
    reminder_work: ReminderWork,
}

impl App {
    pub fn open(store: SharedStore, config: Config, standard_profile: bool) -> Self {
        let mut transport = DesktopTransport::Waiting(config);
        if store.lock().unwrap().state().lan_enabled() {
            transport.start(&store);
        }
        Self::from_transport(store, transport, standard_profile)
    }

    #[cfg(test)]
    fn new(store: SharedStore, network: Result<Network, String>, standard_profile: bool) -> Self {
        Self::from_transport(store, DesktopTransport::Started(network), standard_profile)
    }

    fn from_transport(
        store: SharedStore,
        network: DesktopTransport,
        standard_profile: bool,
    ) -> Self {
        let startup = autostart::executable(standard_profile)
            .and_then(|executable| {
                let status = autostart::status(&executable)?;
                Ok(Startup { executable, status })
            })
            .map_err(|error| error.to_string());
        let (reader, backup_folder, reminder_store) = {
            let guard = store.lock().unwrap();
            (
                guard.reader(),
                guard.backup_directory(),
                guard.reminder_store(),
            )
        };
        let api_controls = ApiControls::new(&store, reader.snapshot().state.api.as_ref());
        let mut app = Self {
            store,
            reader,
            network,
            page: Page::Calendar,
            year: Local::now().year(),
            selected: None,
            reveal_event: false,
            search: String::new(),
            search_focus: false,
            category: None,
            show_circle: true,
            integrations_open: false,
            confirm_token_change: false,
            api_controls,
            note_event: None,
            note: String::new(),
            note_baseline: String::new(),
            note_merging: false,
            note_drafts: Vec::new(),
            editor: None,
            capacity_prompt: None,
            drafts: Drafts::default(),
            backup_open: false,
            backup_folder,
            latest_backup: Ok(None),
            backup_creation: BackupCreation::default(),
            backup_status: None,
            restore_result: None,
            restore_requested: None,
            connections: false,
            confirm_access_renewal: false,
            selected_revocations: BTreeSet::new(),
            device_assignments: DeviceAssignments::default(),
            access_work: false,
            renewed_profile: None,
            sync_message: None,
            file_exchange: FileExchange::default(),
            address: String::new(),
            join_code: String::new(),
            pairing_request: String::new(),
            company: String::new(),
            employee: String::new(),
            hosted_entry: hosted::HostedEntry::default(),
            join_mode: false,
            onboarding_focus: true,
            join_result: None,
            message: None,
            startup,
            startup_error: None,
            calendar_view: None,
            personal: None,
            task_filter: TaskFilter::All,
            agenda: None,
            agenda_overdue: false,
            horizon: Horizon::Days30,
            owner_filter: OwnerFilter::Everyone,
            notification: Delivery::default(),
            next_reminder_check: Instant::now(),
            reminder_store,
            reminder_work: ReminderWork::default(),
        };
        app.restore_local_drafts();
        app
    }

    fn show_onboarding(&mut self, ui: &mut Ui, today: NaiveDate) {
        let enter_pressed = ui.input(|input| input.key_pressed(egui::Key::Enter));
        egui::CentralPanel::default()
            .frame(Frame::new().fill(BACK).inner_margin(48))
            .show_inside(ui, |ui| {
                ScrollArea::vertical()
                    .id_salt("welcome-content")
                    .show(ui, |ui| {
                        brand(ui);
                        ui.add_space(36.0);
                        ui.columns(2, |columns| {
                            let ui = &mut columns[0];
                            ui.set_max_width(445.0);
                            title(
                                ui,
                                "Годовой календарь\nкоманды",
                                37.0,
                            );
                            muted(ui, "События, сроки подготовки и задачи в одном календаре.");
                            ui.add_space(12.0);
                            let waiting = self.join_result.is_some();
                            card().show(ui, |ui| {
                                ui.add_enabled_ui(!waiting, |ui| {
                                    ui.horizontal(|ui| {
                                        for (join, label) in [(false, "Новый календарь"), (true, "Присоединиться")] {
                                            if view_option(ui, label, self.join_mode == join) && self.join_mode != join {
                                                self.join_mode = join;
                                                self.message = None;
                                                self.onboarding_focus = true;
                                            }
                                        }
                                    });
                                });
                                if self.join_mode {
                                    muted(
                                        ui,
                                        "Передайте коллеге запрос этого устройства. Оставьте приложение открытым, пока не получите приглашение.",
                                    );
                                    if ui.add_enabled(!waiting, quiet("Скопировать запрос устройства")).clicked() {
                                        match self.network.enable(&self.store) {
                                            Ok(network) => {
                                                ui.ctx().copy_text(network.pairing_request());
                                                self.message = Some(("Запрос устройства скопирован.".into(), false));
                                            }
                                            Err(error) => self.message = Some((error.to_string(), true)),
                                        }
                                    }
                                } else {
                                    muted(ui, "После создания календаря вы сможете пригласить коллег.");
                                }
                                let name = ui.label("Ваше имя");
                                let employee = ui
                                    .add_enabled(
                                        !waiting,
                                        TextEdit::singleline(&mut self.employee)
                                            .hint_text("Например, Анна")
                                            .margin(theme::field_margin(ui))
                                            .char_limit(80)
                                            .desired_width(f32::INFINITY),
                                    )
                                    .labelled_by(name.id);
                                if std::mem::take(&mut self.onboarding_focus) {
                                    employee.request_focus();
                                }

                                let second_field = if self.join_mode {
                                    let caption = ui.label("Код приглашения");
                                    ui.add_enabled(
                                        !waiting,
                                        TextEdit::singleline(&mut self.join_code)
                                            .hint_text("Вставьте код от коллеги")
                                            .margin(theme::field_margin(ui))
                                            .char_limit(300)
                                            .desired_width(f32::INFINITY),
                                    )
                                    .labelled_by(caption.id)
                                } else {
                                    let caption = ui.label("Название календаря");
                                    ui.add(
                                        TextEdit::singleline(&mut self.company)
                                            .hint_text("Например, Студия Север")
                                            .margin(theme::field_margin(ui))
                                            .char_limit(100)
                                            .desired_width(f32::INFINITY),
                                    )
                                    .labelled_by(caption.id)
                                };
                                let required = if self.join_mode {
                                    &self.join_code
                                } else {
                                    &self.company
                                };
                                let can_submit = !waiting
                                    && !self.employee.trim().is_empty()
                                    && !required.trim().is_empty();
                                let button = if waiting {
                                    "Подключаемся…"
                                } else if self.join_mode {
                                    "Присоединиться к календарю"
                                } else {
                                    "Создать календарь"
                                };
                                let clicked = ui.add_enabled(can_submit, primary(button)).clicked();
                                let enter = enter_pressed && second_field.lost_focus();
                                if enter_pressed && employee.lost_focus() {
                                    second_field.request_focus();
                                }
                                if clicked || (can_submit && enter) {
                                    if self.join_mode {
                                        self.begin_join();
                                    } else {
                                        self.create();
                                    }
                                }
                                if self.join_mode {
                                    muted(
                                        ui,
                                        "Компьютер коллеги должен быть доступен в локальной сети.",
                                    );
                                    ui.collapsing(
                                        "Указать сетевой адрес вручную",
                                        |ui| {
                                            ui.add(
                                                TextEdit::singleline(&mut self.address)
                                                    .hint_text("192.168.1.15:45736")
                                                    .margin(theme::field_margin(ui))
                                                    .desired_width(f32::INFINITY)
                                                    .char_limit(40),
                                            );
                                        },
                                    );
                                }
                                self.draw_message(ui);
                            });
                            ui.add_space(8.0);
                            self.hosted_entry.show(ui);
                            let ui = &mut columns[1];
                            ui.add_space(10.0);
                            year_wheel(ui, today.year(), today, &[], None);
                        });
                    });
            });
    }

    fn create(&mut self) {
        if self.join_result.is_some() {
            return;
        }
        let company = self.company.trim().to_owned();
        let employee = self.employee.trim().to_owned();
        if company.is_empty() || employee.is_empty() {
            self.fail("Укажите своё имя и название календаря.");
            return;
        }
        let result = self
            .store
            .lock()
            .unwrap()
            .transaction(|state| state.create_workspace(company, employee));
        self.report(result, None);
    }

    fn begin_join(&mut self) {
        let result = (|| -> Fallible<_> {
            if self.join_code.trim().is_empty() || self.employee.trim().is_empty() {
                return Err("Введите ваше имя и код приглашения.".into());
            }
            let address = if self.address.trim().is_empty() {
                None
            } else {
                Some(parse_address(&self.address)?)
            };
            let network = self.network.enable(&self.store)?;
            Ok(network.join_async(self.join_code.clone(), self.employee.clone(), address))
        })();
        match result {
            Ok(receiver) => {
                self.message = None;
                self.join_result = Some(receiver);
            }
            Err(error) => self.fail(error.to_string()),
        }
    }

    fn app_bar(
        &mut self,
        ui: &mut Ui,
        workspace: &Workspace,
        count: usize,
        pending: usize,
        near_limit: bool,
    ) {
        Panel::top("main-navigation")
            .exact_size(54.0)
            .show_separator_line(true)
            .frame(
                Frame::new()
                    .fill(NAV)
                    .inner_margin(Margin::symmetric(theme::PAGE_MARGIN.left, 8)),
            )
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    workspace_brand(ui, &workspace.name);
                    ui.add_space(12.0);
                    if nav(ui, "Календарь", self.page == Page::Calendar) {
                        self.page = Page::Calendar;
                    }
                    let tasks = navigation_count("Мои задачи", count);
                    if nav(ui, &tasks, self.page == Page::Tasks) {
                        self.page = Page::Tasks;
                    }
                    if nav(ui, "Обзор", self.page == Page::Agenda) {
                        self.page = Page::Agenda;
                    }
                    let caption = navigation_count("Напоминания", pending);
                    if nav(ui, &caption, self.page == Page::Reminders) {
                        self.page = Page::Reminders;
                        self.message = None;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if nav(ui, "Интеграции", self.integrations_open) {
                            self.integrations_open = true;
                        }
                        let sync_label =
                            if matches!(self.network, DesktopTransport::Started(Err(_))) {
                                "Синхронизация · сбой"
                            } else if near_limit {
                                "Синхронизация · лимит"
                            } else {
                                "Синхронизация"
                            };
                        if nav(ui, sync_label, self.connections) {
                            self.connections = true;
                            self.message = None;
                        }
                        ui.add_space(7.0);
                        if ui.ctx().content_rect().width() >= 1150.0
                            && let Ok(network) = self.network.as_ref()
                        {
                            let paused = network.paused();
                            let connected = !paused && network.has_recent_peer();
                            let caption = if paused {
                                "На паузе"
                            } else if connected {
                                "В сети"
                            } else {
                                "Автономно"
                            };
                            muted(ui, caption);
                            marker(ui, if connected { GREEN } else { MUTED });
                        }
                    });
                });
            });
    }

    fn calendar_rows(&mut self, workspace: &Arc<Workspace>) -> Arc<[CalendarRow]> {
        let calendar = &workspace.calendar;
        let snapshot = Arc::downgrade(workspace);
        let search = search_key(&self.search);
        if let Some(view) = &self.calendar_view
            && view.snapshot.ptr_eq(&snapshot)
            && view.year == self.year
            && view.category == self.category
            && view.search == search
        {
            return Arc::clone(&view.rows);
        }
        let matches = |category, title: &str| {
            self.category.is_none_or(|selected| selected == category)
                && (search.is_empty() || search_key(title).contains(&search))
        };
        let rows: Arc<[CalendarRow]> = if let Some(previous) = &self.calendar_view
            && previous.snapshot.ptr_eq(&snapshot)
            && previous.year == self.year
            && (previous.category.is_none() || previous.category == self.category)
            && search.contains(&previous.search)
        {
            previous
                .rows
                .iter()
                .filter(|row| matches(row.category, &row.title))
                .cloned()
                .collect()
        } else {
            let mut rows: Vec<_> = calendar
                .occurrences(self.year)
                .filter(|entry| matches(entry.event.category, &entry.event.title))
                .map(|event| CalendarRow {
                    id: event.id,
                    date: event.date,
                    title: event.event.title.clone(),
                    category: event.event.category,
                })
                .collect();
            rows.sort_unstable_by_key(|row| (row.date, row.id));
            rows.into()
        };
        self.calendar_view = Some(CalendarView {
            snapshot,
            year: self.year,
            search,
            category: self.category,
            rows: Arc::clone(&rows),
        });
        rows
    }

    fn calendar(
        &mut self,
        ui: &mut Ui,
        workspace: &Arc<Workspace>,
        today: NaiveDate,
        usage: ProfileUsage,
    ) {
        let calendar = &workspace.calendar;
        if let Some(id) = self.selected
            && self.calendar_view.as_ref().is_some_and(|view| {
                view.year == self.year && view.rows.iter().any(|row| row.id == id)
            })
            && let Some(event) = calendar.follow_event(id, self.year)
            && event.year != self.year
        {
            self.open_event(event.year, id);
        }
        let displayed_year = self.year;
        let events = self.calendar_rows(workspace);
        let selected = self
            .update_selection(&events, today)
            .and_then(|id| calendar.event(id, displayed_year));
        let inspector_width = if ui.ctx().content_rect().width() < 1160.0 {
            308.0
        } else {
            338.0
        };
        Panel::right("details")
            .exact_size(inspector_width)
            .resizable(false)
            .show_separator_line(false)
            .frame(
                Frame::new()
                    .fill(BACK)
                    .inner_margin(crate::theme::PAGE_MARGIN),
            )
            .show_inside(ui, |ui| {
                let pane = ui.max_rect() + crate::theme::PAGE_MARGIN;
                ui.painter().line_segment(
                    [pane.left_top(), pane.left_bottom()],
                    Stroke::new(1.0, BORDER),
                );
                ScrollArea::vertical()
                    .id_salt("event-details")
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.details(ui, calendar, selected, today, usage));
            });

        egui::CentralPanel::default()
            .frame(page_frame())
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        title(ui, "План на год", PAGE_TITLE_SIZE);
                        muted(ui, format!("Сегодня, {}", short_date(today)));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add(primary("Новое событие"))
                            .on_hover_text("Новое событие · Ctrl/⌘+N")
                            .clicked()
                        {
                            self.request_new_event(ui.ctx(), usage, |year| {
                                Editor::new(year, today)
                            });
                        }
                        if ui.add(quiet("Резервные копии")).clicked() {
                            self.open_backup_window();
                        }
                    });
                });
                ui.add_space(8.0);
                self.calendar_controls(ui, today);
                self.draw_message(ui);
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);

                let updated = self.calendar_rows(workspace);
                if !Arc::ptr_eq(&events, &updated) {
                    self.update_selection(&updated, today);
                }
                let events = updated;
                let width = ui.available_width();
                if self.show_circle && width >= 555.0 {
                    let dial = (width * 0.43).clamp(245.0, 368.0);
                    let gap = 20.0;
                    let height = ui.available_height();
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = gap;
                        ui.allocate_ui_with_layout(
                            vec2(dial, height),
                            Layout::top_down(Align::Min),
                            |ui| {
                                ui.set_width(dial);
                                self.dial(ui, &events, today);
                            },
                        );
                        let width = width - dial - gap;
                        ui.allocate_ui_with_layout(
                            vec2(width, height),
                            Layout::top_down(Align::Min),
                            |ui| {
                                ui.set_width(width);
                                self.event_list(ui, &events, today, usage);
                            },
                        );
                    });
                } else {
                    if self.show_circle {
                        self.dial(ui, &events, today);
                        ui.add_space(8.0);
                    }
                    self.event_list(ui, &events, today, usage);
                }
            });
    }

    fn update_selection(&mut self, events: &[CalendarRow], today: NaiveDate) -> Option<Id> {
        let selected = events
            .iter()
            .find(|event| Some(event.id) == self.selected)
            .or_else(|| events.iter().find(|event| event.date >= today))
            .or_else(|| events.last())
            .map(|event| event.id);
        if self.selected != selected {
            self.reveal_event = true;
        }
        self.selected = selected;
        selected
    }

    fn dial(&mut self, ui: &mut Ui, events: &[CalendarRow], today: NaiveDate) {
        ui.horizontal(|ui| title(ui, "Годовой круг", SECTION_TITLE_SIZE));
        if let Some(id) = year_wheel(ui, self.year, today, events, self.selected) {
            self.selected = Some(id);
            self.reveal_event = true;
            ui.ctx().request_repaint();
        }
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 9.0;
            for category in Category::ALL {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 3.0;
                    legend_marker(ui, category);
                    ui.label(RichText::new(category.label()).size(11.5).color(MUTED));
                });
            }
        });
    }

    fn calendar_controls(&mut self, ui: &mut Ui, today: NaiveDate) {
        ui.horizontal_wrapped(|ui| {
            Frame::new()
                .fill(BACK)
                .stroke(Stroke::new(1.0, BORDER))
                .corner_radius(5)
                .inner_margin(Margin::symmetric(3, 1))
                .show(ui, |ui| {
                    ui.spacing_mut().interact_size.y = crate::theme::COMPACT_HEIGHT;
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 3.0;
                        if ui
                            .add_enabled(
                                self.year > 1900,
                                Button::new("‹").frame_when_inactive(false).min_size(vec2(
                                    crate::theme::COMPACT_HEIGHT,
                                    crate::theme::COMPACT_HEIGHT,
                                )),
                            )
                            .on_hover_text("Предыдущий год")
                            .clicked()
                        {
                            self.year -= 1;
                        }
                        ui.label(semibold(self.year.to_string(), 14.0, INK));
                        if ui
                            .add_enabled(
                                self.year < 9999,
                                Button::new("›").frame_when_inactive(false).min_size(vec2(
                                    crate::theme::COMPACT_HEIGHT,
                                    crate::theme::COMPACT_HEIGHT,
                                )),
                            )
                            .on_hover_text("Следующий год")
                            .clicked()
                        {
                            self.year += 1;
                        }
                    });
                });
            if self.year != today.year() && ui.add(quiet("Этот год")).clicked() {
                self.year = today.year();
            }
            if view_option(ui, "Круг и список", self.show_circle) {
                self.show_circle = true;
            }
            if view_option(ui, "Список", !self.show_circle) {
                self.show_circle = false;
            }
            ui.add_space(8.0);

            let response = ui.add(
                TextEdit::singleline(&mut self.search)
                    .id(ui.make_persistent_id("event-search"))
                    .hint_text("Найти событие")
                    .margin(crate::theme::field_margin(ui))
                    .desired_width(162.0)
                    .char_limit(120),
            );
            response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "Поиск событий")
            });
            if std::mem::take(&mut self.search_focus) {
                response.request_focus();
            }
            if !self.search.is_empty()
                && ui
                    .add(Button::new("×").frame_when_inactive(false))
                    .on_hover_text("Очистить поиск")
                    .clicked()
            {
                self.search.clear();
            }
            ComboBox::from_id_salt("category-filter")
                .width(132.0)
                .height(220.0)
                .selected_text(self.category.map_or("Все категории", Category::label))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.category, None, "Все категории");
                    for category in Category::ALL {
                        ui.selectable_value(&mut self.category, Some(category), category.label());
                    }
                });
        });
    }

    fn event_list(
        &mut self,
        ui: &mut Ui,
        events: &[CalendarRow],
        today: NaiveDate,
        usage: ProfileUsage,
    ) {
        ui.horizontal(|ui| {
            title(ui, "События", SECTION_TITLE_SIZE);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                muted(ui, event_count(events.len()))
            });
        });
        if events.is_empty() {
            if !self.search.is_empty() || self.category.is_some() {
                muted(ui, "События не найдены");
                if ui.add(quiet("Сбросить фильтры")).clicked() {
                    self.search.clear();
                    self.category = None;
                }
            } else {
                muted(ui, "На этот год пока нет событий");
                if ui.add(quiet("Добавить событие")).clicked() {
                    self.request_new_event(ui.ctx(), usage, |year| Editor::new(year, today));
                }
            }
            return;
        }
        let date_width = 46.0;
        let row_height = 46.0;
        let height = ui.available_height().max(140.0);
        let mut scroll = ScrollArea::vertical()
            .id_salt("event-list")
            .max_height(height)
            .auto_shrink([false, false]);
        let visible_rows = (height / row_height).floor().max(1.0) as usize;
        let month_start = |index: usize| {
            let month = events[index].date.month();
            events[..index].partition_point(|row| row.date.month() < month)
        };
        // Leave enough room to bring December (or the last populated month) to the
        // top, even when it has only a few dates. Short lists already fit in full.
        let extra_rows = if events.len() > visible_rows {
            (visible_rows + 1).saturating_sub(events.len() - month_start(events.len() - 1))
        } else {
            0
        };
        if std::mem::take(&mut self.reveal_event)
            && let Some(index) = events
                .iter()
                .position(|event| Some(event.id) == self.selected)
        {
            let first = month_start(index).max((index + 1).saturating_sub(visible_rows));
            scroll = scroll.vertical_scroll_offset(first as f32 * row_height);
        }
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            scroll.show_rows(ui, row_height, events.len() + extra_rows, |ui, range| {
                for event in &events[range.start.min(events.len())..range.end.min(events.len())] {
                    let active = self.selected == Some(event.id);
                    let (rect, response) = ui.allocate_exact_size(
                        vec2(ui.available_width(), row_height),
                        egui::Sense::click(),
                    );
                    let date = short_date(event.date);
                    let label = format!("{date}, {}, {}", event.title, event.category.label());
                    let response = response.on_hover_text(&label);
                    response.widget_info(|| {
                        egui::WidgetInfo::selected(
                            egui::WidgetType::SelectableLabel,
                            true,
                            active,
                            &label,
                        )
                    });
                    let painter = ui.painter_at(rect);
                    let row_fill = rect.shrink2(vec2(0.0, 2.0));
                    if active {
                        painter.rect_filled(row_fill, 4, TINT);
                        painter.line_segment(
                            [
                                row_fill.left_top() + vec2(1.5, 4.0),
                                row_fill.left_bottom() + vec2(1.5, -4.0),
                            ],
                            Stroke::new(2.0, ACCENT),
                        );
                    } else if response.hovered() {
                        painter.rect_filled(row_fill, 4, BACK);
                    }
                    if response.has_focus() {
                        painter.rect_stroke(
                            row_fill,
                            4,
                            Stroke::new(2.0, ACCENT),
                            egui::StrokeKind::Inside,
                        );
                    }
                    let date_center = rect.left_center() + vec2(22.0, 0.0);
                    painter.text(
                        date_center - vec2(0.0, 6.0),
                        egui::Align2::CENTER_CENTER,
                        event.date.day().to_string(),
                        heading_font(16.0),
                        if active { ACTION_TEXT } else { INK },
                    );
                    painter.text(
                        date_center + vec2(0.0, 10.0),
                        egui::Align2::CENTER_CENTER,
                        crate::chart::MONTHS[event.date.month0() as usize],
                        egui::FontId::proportional(11.0),
                        MUTED,
                    );
                    draw_marker(
                        &painter,
                        rect.left_center() + vec2(date_width + 10.0, 0.0),
                        4.0,
                        event.category,
                    );
                    let text_x = date_width + 20.0;
                    let font = if active {
                        heading_font(14.0)
                    } else {
                        egui::FontId::proportional(14.0)
                    };
                    let mut job = egui::text::LayoutJob::simple(
                        event.title.clone(),
                        font,
                        INK,
                        (rect.width() - text_x - 7.0).max(35.0),
                    );
                    job.wrap.max_rows = 2;
                    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
                    painter.galley(
                        rect.left_center() + vec2(text_x, -galley.size().y * 0.5),
                        galley,
                        INK,
                    );
                    if response.clicked() {
                        self.selected = Some(event.id);
                        ui.ctx().request_repaint();
                    }
                }
            });
        });
    }

    fn details(
        &mut self,
        ui: &mut Ui,
        calendar: &Calendar,
        selected: Option<Occurrence<'_>>,
        today: NaiveDate,
        usage: ProfileUsage,
    ) {
        let Some(event) = selected else {
            title(ui, "Событие", SECTION_TITLE_SIZE);
            muted(ui, "Выберите событие в календаре");
            return;
        };
        ui.horizontal(|ui| {
            legend_marker(ui, event.event.category);
            ui.label(
                RichText::new(event.event.category.label())
                    .size(12.0)
                    .color(MUTED),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(
                        Button::new(RichText::new("Редактировать").size(13.0).color(ACTION_TEXT))
                            .min_size(vec2(0.0, crate::theme::CONTROL_HEIGHT)),
                    )
                    .clicked()
                {
                    self.editor = Some(Editor::edit(event));
                }
            });
        });
        title(ui, &event.event.title, 19.0);
        let owner = owner_name(calendar, event.event.owner);
        let date = short_date(event.date);
        let when = match (event.event.yearly, event.overridden) {
            (true, true) => format!("{date} · Изменено на этот год"),
            (true, false) => format!("{date} · Ежегодно"),
            _ => date,
        };
        muted(ui, when);
        if event.event.owner.is_some() {
            muted(ui, format!("Ответственный · {owner}"));
        } else {
            muted(ui, owner);
        }
        if let Some(source) = calendar.plans[&event.id].source.as_ref() {
            muted(ui, format!("Источник · {}", source.name));
        }
        ui.separator();

        let total = event.event.tasks.len();
        let done = event.completed();
        let count = if total > 0 {
            format!("{done} из {total}")
        } else {
            String::new()
        };
        section_heading(ui, "Чек-лист", &count);
        muted(
            ui,
            format!(
                "Начало подготовки · {}",
                short_date(event.preparation_date())
            ),
        );
        if total == 0 {
            muted(ui, "Пока нет пунктов");
        } else if done > 0 {
            ui.add(
                egui::ProgressBar::new(done as f32 / total as f32)
                    .fill(GREEN)
                    .desired_height(3.0),
            );
        }
        for task in &event.event.tasks {
            let mut checked = event.checked(task.id);
            let changed = ui
                .push_id(task.id, |ui| {
                    task_checkbox(ui, &mut checked, task, event, today)
                })
                .inner;
            if changed {
                self.check(event.id, event.year, task.id, checked);
            }
        }

        ui.add_space(5.0);
        ui.separator();
        if !event.event.description.is_empty() {
            ui.label(&event.event.description);
        }
        if ui.add(quiet("Создать по образцу")).clicked() {
            self.request_new_event(ui.ctx(), usage, |_| Editor::from_template(event));
        }
        ui.separator();
        title(ui, "Дневник", SECTION_TITLE_SIZE);
        let updated = event.note();
        self.load_note((event.id, event.year), updated);
        let conflict = self.note_baseline != updated;
        if conflict {
            ui.colored_label(RED, "Сохранённая заметка изменилась.");
        }
        if conflict || self.note_merging {
            muted(ui, "Сохранённый текст");
            ScrollArea::vertical()
                .id_salt(("saved-note", event.id, event.year))
                .max_height(108.0)
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(if updated.is_empty() {
                            "Пусто"
                        } else {
                            updated
                        })
                        .wrap()
                        .selectable(!updated.is_empty()),
                    );
                });
            muted(ui, "Ваш черновик");
        }
        let draft_limit =
            self.note == self.note_baseline && self.note_drafts.len() >= MAX_NOTE_DRAFTS;
        if draft_limit {
            muted(
                ui,
                "Сохраните или удалите один из черновиков, чтобы начать новую заметку.",
            );
        }
        let response = ui.add_enabled(
            !draft_limit,
            TextEdit::multiline(&mut self.note)
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .margin(Margin::same(8))
                .char_limit(4000)
                .hint_text("Результаты и замечания"),
        );
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "Заметка к событию")
        });
        if conflict || self.note_merging {
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add(quiet("Заменить черновик"))
                    .on_hover_text("Заменить ваш черновик сохранённым текстом")
                    .clicked()
                {
                    self.replace_note_draft(updated);
                } else if conflict {
                    if ui.add(quiet("Объединить вручную")).clicked() {
                        self.begin_note_merge(updated);
                        response.request_focus();
                    }
                } else if ui
                    .add_enabled(self.note != updated, primary("Сохранить итог"))
                    .clicked()
                {
                    self.save_note(event.id, event.year, updated);
                }
            });
        } else {
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(self.note != updated, quiet("Сохранить заметку"))
                    .clicked()
                {
                    self.save_note(event.id, event.year, updated);
                }
                if self.note != self.note_baseline && ui.button("Удалить черновик").clicked()
                {
                    self.replace_note_draft(updated);
                }
            });
        }
    }

    fn load_note(&mut self, key: (Id, i32), current: &str) {
        if self.note_event != Some(key) {
            if let Some(event) = self.note_event
                && self.note != self.note_baseline
            {
                self.note_drafts.push(NoteDraft {
                    event,
                    text: std::mem::take(&mut self.note),
                    baseline: std::mem::take(&mut self.note_baseline),
                    merging: self.note_merging,
                });
            }
            self.note_event = Some(key);
            if let Some(index) = self.note_drafts.iter().position(|draft| draft.event == key) {
                let draft = self.note_drafts.swap_remove(index);
                self.note = draft.text;
                self.note_baseline = draft.baseline;
                self.note_merging = draft.merging;
            } else {
                self.take_saved_note(current);
            }
        }
        if (self.note_baseline != current || self.note_merging)
            && (self.note == current || self.note == self.note_baseline)
        {
            self.take_saved_note(current);
        }
    }

    fn take_saved_note(&mut self, text: &str) {
        self.note = text.to_owned();
        self.note_baseline = self.note.clone();
        self.note_merging = false;
    }

    fn replace_note_draft(&mut self, saved: &str) {
        let previous = (
            self.note.clone(),
            self.note_baseline.clone(),
            self.note_merging,
        );
        self.take_saved_note(saved);
        if self.flush_local_drafts().is_err() {
            (self.note, self.note_baseline, self.note_merging) = previous;
        } else {
            self.message = None;
        }
    }

    fn begin_note_merge(&mut self, saved: &str) {
        self.note_baseline = saved.to_owned();
        self.note_merging = true;
        self.message = None;
    }

    fn save_note(&mut self, id: Id, year: i32, current_note: &str) {
        const CHANGED: &str = "Сохранённая заметка изменилась. Сравните её с вашим черновиком.";
        let expected = self.note_baseline.as_str();
        if current_note != expected {
            self.fail(CHANGED);
            return;
        }
        let note = self.note.trim().to_owned();
        let result = self.store.lock().unwrap().transaction(|state| {
            let event = state.follow_event(id, year)?;
            if event.note() != expected {
                return Err(CHANGED.into());
            }
            if event.note() == note {
                return Ok(());
            }
            let active_year = event.year;
            let revision = state.revision()?;
            state
                .workspace_mut()?
                .calendar
                .plans
                .get_mut(&id)
                .unwrap()
                .years
                .entry(active_year)
                .or_default()
                .note = Some(Record::new(revision, note.clone()));
            Ok(())
        });
        if result.is_ok() {
            self.take_saved_note(&note);
            let _ = self.flush_local_drafts();
        }
        self.report(result, Some("Заметка сохранена."));
    }

    fn check(&mut self, id: Id, year: i32, task: Id, done: bool) {
        let result = self.store.lock().unwrap().transaction(|state| {
            let event = state.follow_event(id, year)?;
            if !event.event.tasks.iter().any(|item| item.id == task) {
                return Err("Пункт чек-листа был изменён.".into());
            }
            if event.checked(task) == done {
                return Ok(());
            }
            let active_year = event.year;
            let revision = state.revision()?;
            state
                .workspace_mut()?
                .calendar
                .plans
                .get_mut(&id)
                .unwrap()
                .years
                .entry(active_year)
                .or_default()
                .checks
                .insert(task, Record::new(revision, done));
            Ok(())
        });
        self.report(result, None);
    }

    fn tasks(&mut self, ui: &mut Ui, today: NaiveDate, calendar: &Calendar) {
        egui::CentralPanel::default()
            .frame(page_frame())
            .show_inside(ui, |ui| {
                let sheet = egui::Rect::from_min_size(
                    ui.cursor().min,
                    vec2(ui.available_width().min(840.0), ui.available_height()),
                );
                let mut sheet_ui = ui.new_child(egui::UiBuilder::new().max_rect(sheet));
                let ui = &mut sheet_ui;
                title(ui, "Мои задачи", PAGE_TITLE_SIZE);
                ui.horizontal_wrapped(|ui| {
                    for (filter, label) in [
                        (TaskFilter::All, "Все"),
                        (TaskFilter::TodayAndTomorrow, "Сегодня и завтра"),
                        (TaskFilter::Overdue, "Просрочено"),
                    ] {
                        if view_option(ui, label, self.task_filter == filter) {
                            self.task_filter = filter;
                        }
                    }
                });
                let filter = self.task_filter;
                let Some(view) = &mut self.personal else {
                    return;
                };
                if view
                    .task_rows
                    .as_ref()
                    .is_some_and(|rows| rows.filter != filter)
                {
                    view.task_rows = None;
                }
                let cached = view.task_rows.get_or_insert_with(|| {
                    let rows = task_rows(calendar, &view.events, view.open_count, today, filter);
                    let open_count = if filter == TaskFilter::All {
                        view.open_count
                    } else {
                        rows.iter().filter(|row| row.task.is_some()).count()
                    };
                    TaskRows {
                        filter,
                        rows: rows.into(),
                        open_count,
                    }
                });
                let open_count = cached.open_count;
                let rows = Arc::clone(&cached.rows);
                if open_count != 0 {
                    muted(ui, format!("Открытые пункты · {open_count}"));
                } else if filter == TaskFilter::All {
                    muted(ui, "События, за подготовку которых вы отвечаете");
                }
                self.draw_message(ui);
                ui.add_space(12.0);

                if rows.is_empty() {
                    card().show(ui, |ui| {
                        let heading = match filter {
                            TaskFilter::All => "Открытых задач нет",
                            TaskFilter::TodayAndTomorrow => "На сегодня и завтра задач нет",
                            TaskFilter::Overdue => "Просроченных задач нет",
                        };
                        title(ui, heading, 20.0);
                        if filter == TaskFilter::All {
                            muted(ui, "Чек-листы появятся здесь в день начала подготовки.");
                        }
                    });
                    return;
                }
                ui.spacing_mut().item_spacing.y = 0.0;
                ScrollArea::vertical()
                    .id_salt(("my-task-rows", filter))
                    .auto_shrink([false, false])
                    .show_rows(ui, LIST_ROW_HEIGHT, rows.len(), |ui, range| {
                        let mut group: Option<Occurrence<'_>> = None;
                        for row in &rows[range] {
                            let event = match group {
                                Some(event) if event.id == row.event && event.year == row.year => {
                                    event
                                }
                                _ => {
                                    let Some(event) = calendar.event(row.event, row.year) else {
                                        continue;
                                    };
                                    group = Some(event);
                                    event
                                }
                            };
                            let width = ui.available_width();
                            let band = ui
                                .allocate_ui_with_layout(
                                    vec2(width, LIST_ROW_HEIGHT),
                                    Layout::left_to_right(Align::Center),
                                    |ui| {
                                        ui.set_min_size(vec2(width, LIST_ROW_HEIGHT));
                                        ui.style_mut().wrap_mode =
                                            Some(egui::TextWrapMode::Truncate);
                                        if let Some(index) = row.task {
                                            if let Some(task) = event.event.tasks.get(index) {
                                                let mut checked = false;
                                                ui.add_space(16.0);
                                                let deadline = task_deadline(
                                                    event.due(task),
                                                    event.date,
                                                    today,
                                                );
                                                let changed = ui
                                                    .push_id(
                                                        (event.id, event.year, task.id),
                                                        |ui| {
                                                            let title_width =
                                                                (width - 214.0).max(100.0);
                                                            let checkbox = ui
                                                                .allocate_ui_with_layout(
                                                                    vec2(title_width, 36.0),
                                                                    Layout::left_to_right(
                                                                        Align::Center,
                                                                    ),
                                                                    |ui| {
                                                                        ui.spacing_mut()
                                                                            .icon_spacing = 7.0;
                                                                        ui.checkbox(
                                                                            &mut checked,
                                                                            semibold(
                                                                                &task.title,
                                                                                15.0,
                                                                                INK,
                                                                            ),
                                                                        )
                                                                    },
                                                                )
                                                                .inner
                                                                .on_hover_text(&task.title);
                                                            checkbox.widget_info(|| {
                                                                egui::WidgetInfo::selected(
                                                                    egui::WidgetType::Checkbox,
                                                                    ui.is_enabled(),
                                                                    checked,
                                                                    format!(
                                                                        "{}, {}",
                                                                        task.title,
                                                                        deadline.text()
                                                                    ),
                                                                )
                                                            });
                                                            checkbox.changed()
                                                        },
                                                    )
                                                    .inner;
                                                ui.with_layout(
                                                    Layout::right_to_left(Align::Center),
                                                    |ui| {
                                                        ui.add_space(10.0);
                                                        ui.label(deadline.size(13.0));
                                                    },
                                                );
                                                if changed {
                                                    self.check(
                                                        event.id, event.year, task.id, checked,
                                                    );
                                                }
                                            }
                                        } else {
                                            legend_marker(ui, event.event.category);
                                            let title_width = (width - 204.0).max(100.0);
                                            ui.allocate_ui_with_layout(
                                                vec2(title_width, 34.0),
                                                Layout::left_to_right(Align::Center),
                                                |ui| {
                                                    let link = ui
                                                        .add(
                                                            egui::Button::new(semibold(
                                                                &event.event.title,
                                                                14.0,
                                                                ACTION_TEXT,
                                                            ))
                                                            .frame_when_inactive(false)
                                                            .truncate(),
                                                        )
                                                        .on_hover_text(format!(
                                                            "Открыть событие «{}»",
                                                            event.event.title
                                                        ));
                                                    if link.clicked() {
                                                        self.open_event(event.year, event.id);
                                                    }
                                                },
                                            );
                                            ui.with_layout(
                                                Layout::right_to_left(Align::Center),
                                                |ui| {
                                                    ui.add_space(10.0);
                                                    muted(
                                                        ui,
                                                        format!(
                                                            "Событие · {}",
                                                            short_date(event.date)
                                                        ),
                                                    );
                                                },
                                            );
                                        }
                                    },
                                )
                                .response
                                .rect;
                            row_rule(ui, band);
                        }
                    });
            });
    }

    fn open_event(&mut self, year: i32, id: Id) {
        self.year = year;
        self.selected = Some(id);
        self.search.clear();
        self.category = None;
        self.reveal_event = true;
        self.page = Page::Calendar;
    }

    fn request_new_event(
        &mut self,
        ctx: &egui::Context,
        usage: ProfileUsage,
        make_editor: impl FnOnce(i32) -> Editor,
    ) {
        if self.editor.is_some() || self.capacity_prompt.is_some() {
            return;
        }
        if usage.plans >= usage.plan_limit {
            self.capacity_prompt = Some(CapacityPrompt::Open {
                previous_focus: ctx.memory(|memory| memory.focused()),
                focus_close: true,
            });
        } else {
            self.editor = Some(make_editor(self.year));
        }
    }

    fn capacity_window(&mut self, ctx: &egui::Context, usage: ProfileUsage) {
        let id = egui::Id::new("calendar-capacity");
        let (previous_focus, mut focus_close) = match self.capacity_prompt.take() {
            Some(CapacityPrompt::Open {
                previous_focus,
                focus_close,
            }) => (previous_focus, focus_close),
            Some(CapacityPrompt::RestoreFocus(previous)) => {
                // Egui keeps the modal layer active for the following frame.
                match ctx.memory(|memory| memory.top_modal_layer()) {
                    None => ctx.memory_mut(|memory| memory.request_focus(previous)),
                    Some(layer) if layer.id == id => {
                        self.capacity_prompt = Some(CapacityPrompt::RestoreFocus(previous));
                    }
                    Some(_) => return,
                }
                ctx.request_repaint();
                return;
            }
            None => return,
        };
        let modal = egui::Modal::new(id)
            .frame(dialog(ctx))
            .backdrop_color(theme::MODAL_BACKDROP)
            .show(ctx, |ui| {
                ui.set_width(440.0);
                title(
                    ui,
                    "Достигнут лимит записей календаря",
                    theme::DIALOG_TITLE_SIZE,
                );
                Self::capacity_details(ui, usage);
                muted(ui, "Существующие события по-прежнему можно редактировать.");
                ui.separator();
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let close = ui.add(primary("Закрыть"));
                    if std::mem::take(&mut focus_close) {
                        close.request_focus();
                    }
                    close.clicked()
                })
                .inner
            });
        if modal.inner || modal.should_close() {
            self.capacity_prompt = previous_focus.map(CapacityPrompt::RestoreFocus);
            ctx.request_repaint();
        } else {
            self.capacity_prompt = Some(CapacityPrompt::Open {
                previous_focus,
                focus_close,
            });
        }
    }

    fn capacity_details(ui: &mut Ui, usage: ProfileUsage) {
        ui.label(format!("Записи · {} из {}", usage.plans, usage.plan_limit));
        muted(
            ui,
            "В лимит входят записи об удалениях для обмена с коллегами.",
        );
        const MIB: f64 = 1024.0 * 1024.0;
        ui.label(format!(
            "Файл календаря · {:.2} из {:.2} МиБ",
            usage.bytes as f64 / MIB,
            usage.byte_limit as f64 / MIB,
        ));
        let free = usage.byte_limit.saturating_sub(usage.bytes);
        if free <= usage.byte_limit / 10 {
            let remaining = if free < 1024 {
                format!("{free} байт")
            } else {
                format!("{} КиБ", free / 1024)
            };
            muted(ui, format!("До предела размера файла · {remaining}"));
            muted(
                ui,
                format!(
                    "Данные для обмена · до {:.0} МиБ",
                    CALENDAR_MESSAGE_LIMIT as f64 / MIB,
                ),
            );
        }
    }

    fn save_event(&self, editor: &Editor, remove: bool) -> Fallible<i32> {
        let (year, event) = if remove {
            (
                editor.existing.ok_or("Событие ещё не создано.")?.0,
                editor.event.clone(),
            )
        } else {
            editor.validated()?
        };
        self.store.lock().unwrap().transaction(|state| {
            let revision = state.revision()?;
            let workspace = state.workspace_mut()?;
            if !remove {
                let previous_owner = editor
                    .existing
                    .and_then(|(year, _)| workspace.calendar.event(editor.id, year))
                    .and_then(|current| current.event.owner);
                workspace.check_assignment(previous_owner, event.owner)?;
            }
            let calendar = &mut workspace.calendar;
            if editor.existing.is_none() && calendar.plans.contains_key(&editor.id) {
                return Err("Это событие уже сохранено. Откройте его в календаре.".into());
            }
            if let Some(issue) = editor.original_issue(calendar) {
                return Err(issue.into());
            }
            let plan = calendar.plans.entry(editor.id).or_default();
            if !remove && event.yearly && plan.imported_single_has_other_dates() {
                return Err("Для ежегодного повтора создайте отдельное событие по образцу.".into());
            }
            if editor.existing.is_some() && editor.scope == Scope::ThisYear {
                let change = if remove {
                    YearChange::Cancel
                } else {
                    YearChange::Edit(event.into())
                };
                plan.years.entry(year).or_default().change = Some(Record::new(revision, change));
            } else {
                plan.templates.insert(
                    year,
                    Record::new(revision, if remove { None } else { Some(event.into()) }),
                );
                if let Some(record) = plan.years.get_mut(&year) {
                    record.change = Some(Record::new(revision, YearChange::UseTemplate));
                }
            }
            Ok(year)
        })
    }

    fn edit_window(&mut self, ctx: &egui::Context, workspace: &Workspace, usage: ProfileUsage) {
        let calendar = &workspace.calendar;
        let Some(mut editor) = self.editor.take() else {
            return;
        };
        let mut save = None;
        let mut copy_as_new = false;
        let mut close = false;
        let mut discard = false;
        let existing = editor.existing.is_some();
        let original_issue = editor.original_issue(calendar);
        let blocked_creation =
            usage.plans >= usage.plan_limit && (!existing || original_issue.is_some());
        let chrome_height = if original_issue.is_some() || blocked_creation {
            276.0
        } else {
            224.0
        };
        if original_issue.is_some() {
            editor.confirm_remove = false;
        }
        let name = if existing {
            "Изменить событие"
        } else {
            "Новое событие"
        };

        let modal = egui::Modal::new(egui::Id::new("event-editor"))
            .frame(dialog(ctx))
            .backdrop_color(theme::MODAL_BACKDROP)
            .show(ctx, |ui| {
                ui.set_width((ctx.content_rect().width() - 70.0).clamp(300.0, 620.0));
                if editor.confirm_discard || editor.confirm_remove {
                    let heading = if editor.confirm_discard {
                        "Не сохранять изменения?"
                    } else {
                        "Удалить событие?"
                    };
                    title(ui, heading, theme::DIALOG_TITLE_SIZE);
                    if editor.confirm_discard {
                        muted(ui, "Изменения в этом редакторе будут потеряны.");
                    } else if editor.scope == Scope::ThisYear {
                        muted(ui, "Событие исчезнет из календаря за выбранный год.");
                    } else {
                        muted(ui, "Событие исчезнет за выбранный и следующие годы до следующей версии шаблона.");
                    }
                    if let Some(error) = &editor.error {
                        ui.colored_label(RED, error);
                    }
                    ui.separator();
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let action = if editor.confirm_discard { "Не сохранять" } else { "Удалить событие" };
                        if ui.button(RichText::new(action).color(RED)).clicked() {
                            if editor.confirm_discard {
                                discard = true;
                            } else {
                                save = Some(true);
                            }
                        }
                        if ui.add(primary("Вернуться к событию")).clicked() {
                            editor.confirm_remove = false;
                            editor.confirm_discard = false;
                            editor.error = None;
                        }
                    });
                    return;
                }
                ui.horizontal(|ui| {
                    title(ui, name, theme::DIALOG_TITLE_SIZE);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let dismiss = ui.add(Button::new("×").frame_when_inactive(false)
                            .min_size(Vec2::splat(theme::CONTROL_HEIGHT)))
                            .on_hover_text("Закрыть редактор");
                        dismiss.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Закрыть редактор"));
                        if dismiss.clicked()
                        {
                            close = true;
                        }
                    });
                });
                ui.separator();
                ScrollArea::vertical()
                    .auto_shrink([false, true])
                    .max_height((ctx.content_rect().height() - chrome_height).max(180.0))
                    .show(ui, |ui| editor_fields(ui, &mut editor, workspace));
                if let Some(issue) = original_issue {
                    ui.colored_label(RED, issue);
                    if !blocked_creation {
                        muted(ui, "Можно перенести введённые данные в новый черновик.");
                    }
                } else if let Some(error) = &editor.error
                    && !blocked_creation
                {
                    ui.colored_label(RED, error);
                }
                if blocked_creation {
                    ui.colored_label(
                        RED,
                        format!("Лимит записей календаря · {} из {}", usage.plans, usage.plan_limit),
                    );
                    muted(ui, "Введённые данные остаются в черновике.");
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.add_enabled(editor.event.tasks.len() < 100, quiet("+ Добавить пункт"))
                        .clicked()
                    {
                        editor.event.tasks.push(Task {
                            id: Id::new_v4(),
                            title: String::new(),
                            days_before: 14.min(editor.event.preparation_days),
                        });
                        editor.focus_new_task = true;
                        ui.ctx().request_repaint();
                    }
                    if existing && original_issue.is_none() && ui.button("Удалить…").clicked() {
                        editor.confirm_remove = true;
                        editor.error = None;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let action = if original_issue.is_some() {
                            "Сохранить как новое"
                        } else if existing {
                            "Сохранить"
                        } else {
                            "Создать событие"
                        };
                        if ui.add_enabled(!blocked_creation, primary(action)).clicked() {
                            if original_issue.is_some() {
                                copy_as_new = true;
                            } else {
                                save = Some(false);
                            }
                        }
                        if ui.button("Отмена").clicked() {
                            close = true;
                        }
                    });
                });
            });
        if modal.is_top_modal && !modal.any_popup_open {
            let escape =
                ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
            if escape {
                if editor.confirm_discard || editor.confirm_remove {
                    editor.confirm_discard = false;
                    editor.confirm_remove = false;
                    editor.error = None;
                } else {
                    close = true;
                }
            } else if !editor.confirm_discard
                && !editor.confirm_remove
                && original_issue.is_none()
                && !blocked_creation
                && ctx.input_mut(|input| {
                    input.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter)
                })
            {
                save = Some(false);
            }
        }
        if copy_as_new {
            self.editor = Some(editor.into_new_copy());
            ctx.request_repaint();
            return;
        }
        if let Some(remove) = save {
            match self.save_event(&editor, remove) {
                Ok(year) => {
                    self.open_event(year, editor.id);
                    if remove {
                        self.selected = None;
                    }
                    self.message = None;
                    let _ = self.flush_local_drafts();
                    self.next_reminder_check = Instant::now();
                    ctx.request_repaint();
                    return;
                }
                Err(error) => {
                    editor.error = Some(if !remove && error.is::<ProfileSizeLimit>() {
                        format!("{error} Попробуйте сократить описание или чек-лист.")
                    } else {
                        error.to_string()
                    });
                    if editor.event.title.trim().is_empty() {
                        editor.focus_title = true;
                    }
                    ctx.request_repaint();
                }
            }
        }
        if discard {
            if let Err(error) = self.flush_local_drafts() {
                editor.error = Some(format!("Не удалось удалить локальный черновик. {error}"));
                self.editor = Some(editor);
            }
            ctx.request_repaint();
            return;
        }
        if close && !editor.has_changes() {
            ctx.request_repaint();
            return;
        }
        if close {
            editor.confirm_discard = true;
            editor.error = None;
            ctx.request_repaint();
        }
        self.editor = Some(editor);
    }

    pub fn take_restore_request(&mut self) -> Option<RestoreRequest> {
        self.restore_requested.take()
    }

    pub fn has_unsaved_changes(&self) -> bool {
        self.editor.as_ref().is_some_and(Editor::has_changes) || self.has_unsaved_notes()
    }

    pub fn file_exchange_busy(&self) -> bool {
        self.file_exchange.job.is_some()
    }

    pub fn file_exchange_failed(&self) -> bool {
        self.file_exchange
            .status
            .as_ref()
            .is_some_and(|(_, error)| *error)
    }

    pub fn reminder_work_busy(&self) -> bool {
        self.reminder_work.busy()
    }

    pub fn reminder_work_failed(&self) -> bool {
        matches!(self.reminder_work.status(), ReminderWorkStatus::Failed(_))
    }

    pub fn backup_creation_busy(&self) -> bool {
        matches!(self.backup_creation, BackupCreation::Writing(_))
    }

    pub fn backup_creation_failed(&self) -> bool {
        matches!(
            self.backup_creation,
            BackupCreation::Finished { error: true, .. }
        )
    }

    fn has_unsaved_notes(&self) -> bool {
        self.note != self.note_baseline || !self.note_drafts.is_empty()
    }

    pub fn restore_open_backup(
        &self,
        backup: &BackupEntry,
    ) -> Fallible<(SharedStore, RestoreResult)> {
        if self.backup_creation_busy() {
            return Err("Дождитесь создания резервной копии.".into());
        }
        if self.file_exchange.job.is_some() {
            return Err("Дождитесь завершения обмена через файл.".into());
        }
        if self.reminder_work_busy() {
            return Err("Дождитесь завершения работы с отметками напоминаний.".into());
        }
        if self.api_controls_busy() {
            return Err("Дождитесь завершения работы с локальным API.".into());
        }
        let mut store = self.store.lock().unwrap();
        let result = store.restore_open_backup(backup)?;
        // Keep writers blocked until the live network sees the saved pause.
        if let Ok(network) = self.network.as_ref() {
            network.set_paused(true);
        }
        Ok((Arc::clone(&self.store), result))
    }

    fn open_backup_window(&mut self) {
        self.latest_backup = self
            .backup_folder
            .as_deref()
            .and_then(|folder| folder.parent())
            .ok_or_else(|| "Не удалось определить папку копий.".to_owned())
            .and_then(|profile| {
                Store::backups(profile)
                    .map(|copies| copies.into_iter().next())
                    .map_err(|error| format!("Не удалось прочитать список копий. {error}"))
            });
        self.backup_status = None;
        self.backup_open = true;
        if let BackupCreation::Finished { notice, .. } = &mut self.backup_creation {
            *notice = false;
        }
    }

    fn create_profile_backup(&mut self, ctx: &egui::Context) {
        if self.backup_creation_busy() {
            return;
        }
        let (send, completed) = sync_channel(1);
        let store = Arc::clone(&self.store);
        let worker_ctx = ctx.clone();
        let worker = thread::Builder::new()
            .name("calendar-backup".into())
            .spawn(move || {
                let result = Store::create_shared_backup(&store);
                drop(store);
                let _ = send.send(result);
                worker_ctx.request_repaint();
            });
        self.backup_status = None;
        match worker {
            Ok(_) => self.backup_creation = BackupCreation::Writing(completed),
            Err(error) => self.finish_backup_creation(Err(error.into())),
        }
        ctx.request_repaint();
    }

    fn poll_backup_creation(&mut self) {
        let BackupCreation::Writing(completed) = &self.backup_creation else {
            return;
        };
        let result = match completed.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("Создание копии прервано.".into()),
        };
        self.finish_backup_creation(result);
    }

    fn finish_backup_creation(&mut self, result: Fallible<BackupEntry>) {
        let (message, error) = match result {
            Ok(copy) => {
                let latest = self.latest_backup.as_mut().ok().and_then(Option::take);
                self.latest_backup = Ok(latest.max(Some(copy)));
                ("Копия создана.".into(), false)
            }
            Err(error) => (format!("Не удалось создать копию. {error}"), true),
        };
        self.backup_creation = BackupCreation::Finished {
            message,
            error,
            notice: !self.backup_open,
        };
    }

    fn backup_creation_notice(&mut self, ui: &mut Ui) {
        let BackupCreation::Finished { error, notice, .. } = &mut self.backup_creation else {
            return;
        };
        if !*notice || self.backup_open {
            return;
        }
        let mut open = false;
        let mut dismiss = false;
        Panel::bottom("backup-creation-notice")
            .frame(
                Frame::new()
                    .fill(NAV)
                    .inner_margin(Margin::symmetric(theme::PAGE_MARGIN.left, 4)),
            )
            .show_separator_line(true)
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    if *error {
                        ui.colored_label(RED, "Не удалось создать копию.");
                    } else {
                        ui.colored_label(MUTED, "Копия создана.");
                    }
                    open = ui.add(quiet("Резервные копии")).clicked();
                    dismiss = ui.add(quiet("Закрыть сообщение")).clicked();
                });
            });
        if dismiss {
            *notice = false;
        }
        if open {
            self.open_backup_window();
        }
    }

    pub fn report_restore(&mut self, result: RestoreResult) {
        self.backup_open = true;
        self.backup_status = None;
        self.restore_result = Some(result);
    }

    fn request_restore(&mut self, request: RestoreRequest) {
        let blocked = if self.backup_creation_busy() {
            Some("Дождитесь создания резервной копии.")
        } else if self.file_exchange_busy() {
            Some("Дождитесь завершения обмена через файл.")
        } else if self.reminder_work_busy() {
            Some("Дождитесь завершения работы с отметками напоминаний.")
        } else if self.api_controls_busy() {
            Some("Дождитесь завершения работы с локальным API.")
        } else if self.has_unsaved_notes() {
            Some("Сохраните черновики в дневнике перед восстановлением.")
        } else {
            None
        };
        if let Some(message) = blocked {
            self.backup_status = Some((message.into(), true));
        } else {
            self.restore_requested = Some(request);
        }
    }

    fn backup_window(&mut self, ctx: &egui::Context) {
        if !self.backup_open {
            return;
        }
        let mut close = false;
        let restored = self.restore_result.is_some();
        let dialog = egui::Modal::new(egui::Id::new("profile-backups"))
            .frame(dialog(ctx))
            .backdrop_color(theme::MODAL_BACKDROP)
            .show(ctx, |ui| {
                ui.set_width(if restored { 460.0 } else { 500.0 });
                title(
                    ui,
                    if restored {
                        "Календарь восстановлен"
                    } else {
                        "Резервные копии"
                    },
                    theme::DIALOG_TITLE_SIZE,
                );
                ScrollArea::vertical()
                    .id_salt(("backup-content", restored))
                    .max_height((ctx.content_rect().height() - 152.0).max(100.0))
                    .show(ui, |ui| self.backup_content(ui, ctx));
                ui.separator();
                ui.allocate_ui_with_layout(
                    vec2(ui.available_width(), theme::CONTROL_HEIGHT),
                    Layout::right_to_left(Align::Center),
                    |ui| {
                        if restored {
                            close = ui.add(primary("Готово")).clicked();
                            if let Some(point) = self
                                .restore_result
                                .as_ref()
                                .and_then(|result| result.restore_point.as_ref())
                                && ui.button("Вернуть прежний календарь…").clicked()
                            {
                                self.request_restore(RestoreRequest::ReturnTo(point.clone()));
                            }
                        } else {
                            close = ui.button("Закрыть").clicked();
                        }
                    },
                );
            });
        if close || dialog.should_close() {
            self.backup_open = false;
            self.restore_result = None;
        }
    }

    fn backup_content(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        if let Some(result) = &self.restore_result {
            ui.label("Обмен с коллегами на паузе до ручного возобновления.");
            if result.restore_point_warning {
                ui.colored_label(
                    RED,
                    "Точку возврата создать не удалось. Прежние данные сохранены в файлах профиля.",
                );
            }
            let original = result.original.as_ref();
            let working = result.working_snapshot.as_ref();
            if let Some(path) = original.or(working) {
                ui.add_space(5.0);
                muted(
                    ui,
                    "Файлы могут содержать ключ синхронизации и токены API. Храните их в закрытой папке.",
                );
                egui::CollapsingHeader::new("Файлы прежнего профиля")
                    .id_salt(path)
                    .show(ui, |ui| {
                        for (name, path) in [
                            ("Прежний файл на диске", original),
                            ("Прежнее открытое состояние", working),
                        ] {
                            if let Some(path) = path {
                                ui.horizontal_wrapped(|ui| {
                                    let label = ui.label(name);
                                    if ui
                                        .button("Скопировать путь")
                                        .labelled_by(label.id)
                                        .clicked()
                                    {
                                        ctx.copy_text(path.display().to_string());
                                    }
                                });
                                let filename = path.file_name().unwrap_or(path.as_os_str());
                                ui.add(egui::Label::new(filename.to_string_lossy()).wrap())
                                    .on_hover_text(path.display().to_string());
                            }
                        }
                        muted(ui, "Для обмена с коллегами используйте файл .yrplan.");
                    });
            }
            if let Some((message, error)) = &self.backup_status {
                ui.colored_label(if *error { RED } else { MUTED }, message);
            }
            return;
        }
        muted(
            ui,
            "Копия содержит календарь и ключ синхронизации. Токены API исключены. Храните копию в закрытой папке.",
        );
        ui.add_space(5.0);
        match &self.latest_backup {
            Ok(Some(copy)) => {
                ui.label(format!(
                    "Последняя копия · {}",
                    copy.created.with_timezone(&Local).format("%d.%m.%Y %H:%M"),
                ))
                .on_hover_text("Файл проверяется перед восстановлением.");
            }
            Ok(None) => {
                muted(ui, "Копий пока нет.");
            }
            Err(error) => {
                ui.colored_label(RED, error);
            }
        }
        ui.add_space(5.0);
        ui.horizontal(|ui| {
            let enabled = !self.backup_creation_busy();
            if ui.add_enabled(enabled, primary("Создать копию")).clicked() {
                self.create_profile_backup(ctx);
            }
            if ui
                .add_enabled(enabled, Button::new("Восстановить из копии…"))
                .clicked()
            {
                self.request_restore(RestoreRequest::Choose);
            }
        });
        if self.backup_creation_busy() {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(16.0).color(ACCENT));
                muted(ui, "Сохраняем резервную копию…");
            });
        }
        if let BackupCreation::Finished { message, error, .. } = &self.backup_creation {
            ui.colored_label(if *error { RED } else { MUTED }, message);
        }
        if let Some((message, error)) = &self.backup_status {
            ui.colored_label(if *error { RED } else { MUTED }, message);
        }
        ui.separator();
        if let Some(folder) = &self.backup_folder {
            muted(ui, "Папка копий");
            ui.add(egui::Label::new(folder.to_string_lossy()).truncate())
                .on_hover_text(folder.display().to_string());
            if ui.button("Скопировать путь").clicked() {
                ctx.copy_text(folder.to_string_lossy().into_owned());
                self.backup_status = Some(("Путь скопирован.".into(), false));
            }
        }
    }

    fn file_exchange_section(&mut self, ui: &mut Ui) -> Option<ExchangeRequest> {
        let mut request = None;
        egui::CollapsingHeader::new("Обмен через файл")
            .default_open(self.network.as_ref().is_err())
            .show(ui, |ui| {
                let exchange = &mut self.file_exchange;
                let working = exchange.job.is_some();
                if !working {
                    let dropped = ui.input_mut(|input| std::mem::take(&mut input.raw.dropped_files));
                    match dropped.as_slice() {
                        [] => {}
                        [file] if file.path.is_some() => {
                            exchange.dropped.clone_from(&file.path);
                            exchange.status = None;
                        }
                        _ => {
                            exchange.status = Some(("Перетащите один файл обмена.".into(), true));
                        }
                    }
                }
                muted(ui, "Файл .yrplan можно передать уже подключённому коллеге на флешке, через AirDrop на Mac или через Bluetooth в Windows.");
                if let Some(path) = exchange.dropped.clone() {
                    let name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy();
                    muted(ui, format!("Выбран файл · {name}"));
                    ui.horizontal_wrapped(|ui| {
                        if ui.add_enabled(!working, primary("Принять выбранный файл")).clicked() {
                            request = Some(ExchangeRequest::Use(ExchangeOperation::Import, path));
                            exchange.dropped = None;
                        }
                        if ui.add_enabled(!working, quiet("Сбросить")).clicked() {
                            exchange.dropped = None;
                        }
                    });
                }
                let native_picker = cfg!(any(windows, target_os = "macos"));
                if native_picker {
                    ui.horizontal_wrapped(|ui| {
                        if ui.add_enabled(!working, Button::new("Сохранить файл…")).clicked() {
                            request = Some(ExchangeRequest::PickExport);
                        }
                        if ui.add_enabled(!working, Button::new("Открыть файл…")).clicked() {
                            request = Some(ExchangeRequest::PickImport);
                        }
                    });
                }
                if exchange.dropped.is_none() {
                    muted(ui, "Полученный файл можно перетащить в это окно.");
                }
                egui::CollapsingHeader::new("Указать путь вручную")
                    .default_open(!native_picker)
                    .show(ui, |ui| {
                        let label = ui.label("Путь к файлу .yrplan");
                        ui.add_enabled(
                            !working,
                            TextEdit::singleline(&mut exchange.path)
                                .margin(theme::field_margin(ui))
                                .desired_width(f32::INFINITY),
                        )
                        .labelled_by(label.id);
                        let enabled = !working && !exchange.path.trim().is_empty();
                        ui.horizontal_wrapped(|ui| {
                            if ui.add_enabled(enabled, Button::new("Сохранить по пути")).clicked() {
                                request = Some(ExchangeRequest::Use(
                                    ExchangeOperation::Export,
                                    PathBuf::from(&exchange.path),
                                ));
                            }
                            if ui.add_enabled(enabled, Button::new("Открыть по пути")).clicked() {
                                request = Some(ExchangeRequest::Use(
                                    ExchangeOperation::Import,
                                    PathBuf::from(&exchange.path),
                                ));
                            }
                        });
                    });
                if let Some((message, error)) = &exchange.status {
                    ui.colored_label(if *error { RED } else { MUTED }, message);
                }
                if let Some(path) = &exchange.exported {
                    let name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy();
                    let label = format!("Последний сохранённый файл · {name}");
                    ui.add(egui::Label::new(RichText::new(label).color(MUTED)).truncate())
                        .on_hover_text(path.display().to_string());
                    if native_picker && ui.add_enabled(!working, quiet("Показать в папке")).clicked() {
                        request = Some(ExchangeRequest::Use(ExchangeOperation::Reveal, path.clone()));
                    }
                }
            });
        request
    }

    fn start_file_exchange(&mut self, ctx: &egui::Context, request: ExchangeRequest) {
        if self.file_exchange.job.is_some() {
            return;
        }
        let (operation, path) = match request {
            ExchangeRequest::PickExport => (ExchangeOperation::Export, pick_exchange_to_save()),
            ExchangeRequest::PickImport => (ExchangeOperation::Import, pick_exchange_to_open()),
            ExchangeRequest::Use(operation, path) => (operation, Some(path)),
        };
        let Some(mut path) = path else {
            return;
        };
        if !path.is_absolute() {
            self.file_exchange.status = Some(("Укажите полный путь к файлу.".into(), true));
            ctx.request_repaint();
            return;
        }
        if matches!(operation, ExchangeOperation::Export) && path.extension().is_none() {
            path.set_extension(EXCHANGE_EXTENSION);
        }
        let store = self.store.clone();
        let worker_ctx = ctx.clone();
        let (sender, receiver) = sync_channel(1);
        match thread::Builder::new()
            .name("file-exchange".into())
            .spawn(move || {
                let result = match operation {
                    ExchangeOperation::Export => export_file(&store, &path)
                        .map(|_| ExchangeResult::Exported(path))
                        .map_err(|error| format!("Не удалось сохранить файл. {error}").into()),
                    ExchangeOperation::Import => import_file(&store, &path)
                        .map(|changed| ExchangeResult::Imported { changed })
                        .map_err(|error| format!("Не удалось открыть файл. {error}").into()),
                    ExchangeOperation::Reveal => reveal_exchange_file(&path)
                        .map(|()| ExchangeResult::Revealed)
                        .map_err(|error| format!("Не удалось показать файл. {error}").into()),
                };
                let _ = sender.send(result);
                worker_ctx.request_repaint();
            }) {
            Ok(_) => {
                self.file_exchange.job = Some(receiver);
                let status = match operation {
                    ExchangeOperation::Export => "Сохраняем файл…",
                    ExchangeOperation::Import => "Принимаем изменения из файла…",
                    ExchangeOperation::Reveal => "Открываем папку файла…",
                };
                self.file_exchange.status = Some((status.into(), false));
            }
            Err(_) => {
                self.file_exchange.status =
                    Some(("Не удалось начать обмен через файл.".into(), true));
            }
        }
        ctx.request_repaint();
    }

    fn poll_file_exchange(&mut self) {
        let Some(result) = self.file_exchange.job.as_ref().map(Receiver::try_recv) else {
            return;
        };
        let status = match result {
            Ok(Ok(ExchangeResult::Exported(path))) => {
                self.file_exchange.exported = Some(path);
                ("Файл сохранён. Передайте его коллеге.".into(), false)
            }
            Ok(Ok(ExchangeResult::Imported { changed: true })) => {
                self.next_reminder_check = Instant::now();
                ("Изменения из файла добавлены в календарь.".into(), false)
            }
            Ok(Ok(ExchangeResult::Imported { changed: false })) => {
                ("Календарь уже содержит эти изменения.".into(), false)
            }
            Ok(Ok(ExchangeResult::Revealed)) => ("Папка файла открыта.".into(), false),
            Ok(Err(error)) => (error.to_string(), true),
            Err(TryRecvError::Disconnected) => ("Обмен через файл прерван.".into(), true),
            Err(TryRecvError::Empty) => return,
        };
        self.file_exchange.job = None;
        self.file_exchange.status = Some(status);
    }

    pub fn show_renewed_access(&mut self) {
        self.connections = true;
        self.sync_message = Some(("Открыт календарь с новым ключом.".into(), false));
    }

    fn sync_window(
        &mut self,
        ctx: &egui::Context,
        state: &LocalState,
        workspace: &Workspace,
        usage: ProfileUsage,
    ) {
        if !self.connections {
            return;
        }
        let mut open = true;
        let mut address_result = None;
        let mut file_request = None;
        let busy = self.file_exchange_busy()
            || self.reminder_work_busy()
            || self.backup_creation_busy()
            || self.api_controls_busy()
            || self.join_result.is_some();
        let mut renew = false;
        let mut rejoin = false;
        let mut link = None;

        settings_window(ctx, "Синхронизация", &mut open, |ui| {
            ui.set_min_width(440.0);
            self.hosted_entry.show(ui);
            ui.separator();
            title(ui, "Пригласить коллегу", SECTION_TITLE_SIZE);
            if let Ok(network) = self.network.as_ref() {
                let mut paused = network.paused();
                let status = network.status();
                if let Some(id) = workspace.access_manager.filter(|id| *id != state.device) {
                    let name = workspace
                        .calendar
                        .members
                        .get(&id)
                        .map_or("Управляющий компьютер", |m| {
                            m.value.as_str()
                        });
                    muted(
                        ui,
                        format!(
                            "За приглашением обратитесь к сотруднику {name}. ID управляющего компьютера · {id}"
                        ),
                    );
                } else {
                    muted(
                        ui,
                        "Попросите коллегу открыть «Присоединиться» и прислать запрос устройства.",
                    );
                    let label = ui.label("Запрос устройства коллеги");
                    ui.add(
                        TextEdit::singleline(&mut self.pairing_request)
                            .desired_width(f32::INFINITY)
                            .char_limit(120)
                            .hint_text("Вставьте запрос от коллеги")
                            .margin(theme::field_margin(ui)),
                    )
                    .labelled_by(label.id);
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .add_enabled(
                                !paused && !self.pairing_request.trim().is_empty(),
                                primary("Создать и скопировать код"),
                            )
                            .clicked()
                        {
                            match network.make_invite(&self.pairing_request) {
                                Ok(code) => {
                                    ctx.copy_text(code);
                                    self.pairing_request.clear();
                                    self.sync_message = Some((
                                        "Приглашение скопировано. Передайте его коллеге.".into(),
                                        false,
                                    ));
                                }
                                Err(error) => address_result = Some(Err(error)),
                            }
                        }
                    });
                    muted(
                        ui,
                        "Код действует 15 минут и подходит только устройству, от которого вы получили запрос.",
                    );
                }
                if let Some(error) = status.merge_error {
                    ui.colored_label(RED, error);
                }
                if let Some(warning) = status.warning {
                    ui.colored_label(RED, warning);
                }
                if let Some(warning) = status.route_warning {
                    ui.colored_label(RED, warning);
                }
                if let Some((message, error)) = &self.sync_message {
                    ui.colored_label(if *error { RED } else { MUTED }, message);
                }
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    if paused {
                        muted(ui, "Синхронизация на паузе");
                    } else if status.recent_peers > 0 {
                        ui.label(format!("Устройств на связи · {}", status.recent_peers));
                    } else {
                        muted(ui, "Других устройств пока нет на связи");
                    }
                    if ui.checkbox(&mut paused, "Приостановить обмен").changed() {
                        self.sync_message = network.set_profile_paused(paused).err().map(|error| {
                            (format!("Не удалось сохранить режим обмена. {error}"), true)
                        });
                    }
                });
                ui.collapsing("Подключение по адресу и состояние сети", |ui| {
                    let time = status.last_sync.or(state.last_sync)
                        .and_then(|ts| Local.timestamp_opt(ts, 0).single());
                    muted(ui, time.map_or_else(|| "Обмена ещё не было".into(), |time|
                        format!("Последний обмен · {}", time.format("%d.%m.%Y %H:%M"))
                    ));
                    if let Some(address) = network.address() {
                        muted(ui, format!("Адрес этого устройства · {address}"));
                    }
                    ui.horizontal(|ui| {
                        let address = ui.label("Адрес коллеги");
                        ui.add(TextEdit::singleline(&mut self.address)
                            .hint_text("192.168.1.15:45736").margin(theme::field_margin(ui)).desired_width(180.0).char_limit(40))
                            .labelled_by(address.id);
                        if ui.add_enabled(!self.address.trim().is_empty(), Button::new("Добавить адрес")).clicked() {
                            address_result = Some(parse_address(&self.address)
                                .and_then(|address| network.remember_peer(address))
                                .map(|()| "Адрес добавлен. Приложение попробует связаться с ним."));
                        }
                    });
                    if !state.peers.is_empty() {
                        muted(ui, format!("Сохранённые адреса · {}", state.peers.len()));
                        ScrollArea::vertical()
                            .id_salt("saved-peer-addresses")
                            .max_height(104.0)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                for &address in &state.peers {
                                    ui.push_id(address, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.label(address.to_string());
                                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                                let remove = ui.button("Удалить адрес");
                                                remove.widget_info(|| egui::WidgetInfo::labeled(
                                                    egui::WidgetType::Button, true, format!("Удалить адрес {address}"),
                                                ));
                                                if remove.clicked() {
                                                    address_result = Some(network.remove_saved_address(address)
                                                        .map(|()| "Адрес удалён из сохранённого списка."));
                                                }
                                            });
                                        });
                                    });
                                }
                            });
                    }
                });
            } else if matches!(self.network, DesktopTransport::Waiting(_)) {
                if ui
                    .add(primary("Включить обмен по локальной сети"))
                    .clicked()
                    && let Err(error) = self.network.enable(&self.store)
                {
                    self.sync_message = Some((error.to_string(), true));
                }
            } else if let Err(error) = self.network.as_ref() {
                ui.colored_label(RED, format!("Обмен по сети недоступен. {error}"));
            }
            ui.separator();
            file_request = self.file_exchange_section(ui);
            let near_limit = usage.near_limit();
            if near_limit {
                let warning = if usage.plans >= usage.plan_limit {
                    "Достигнут лимит записей календаря."
                } else {
                    "Календарь близок к лимиту."
                };
                ui.colored_label(RED, warning);
            }
            egui::CollapsingHeader::new("Ёмкость профиля")
                .id_salt(("profile-capacity", near_limit))
                .default_open(near_limit)
                .show(ui, |ui| Self::capacity_details(ui, usage));
            ui.separator();
            if let Ok(network) = self.network.as_ref() {
                ui.collapsing("Доступ к календарю", |ui| {
                    muted(ui, format!("ID этого устройства · {}", state.device));
                    if workspace.access_manager.is_none_or(|id| id == state.device) {
                    if self.confirm_access_renewal {
                        if self.selected_revocations.is_empty() {
                            ui.label("Перевыпустить ключ для всего календаря?");
                        } else {
                            ui.label("Отозвать доступ выбранных устройств?");
                            for id in &self.selected_revocations {
                                if let Some(member) = workspace.calendar.members.get(id) {
                                    ui.label(format!("{} · {id}", member.value));
                                }
                            }
                        }
                        if workspace.access_manager.is_none() {
                            muted(ui, "Этот компьютер станет управляющим. В дальнейшем приглашения и отзыв доступа будут доступны только на нём.");
                        }
                        muted(ui, "Остальным компьютерам понадобятся новые приглашения. История и назначения сохранятся. Прежние копии останутся у их владельцев.");
                        ui.horizontal_wrapped(|ui| {
                            let action = if self.selected_revocations.is_empty() { "Перевыпустить ключ" } else { "Отозвать доступ" };
                            if ui.add_enabled(!busy, Button::new(RichText::new(action).color(RED))).clicked() { renew = true; }
                            if ui.button("Отмена").clicked() { self.confirm_access_renewal = false; }
                        });
                    } else {
                        muted(ui, "При потере компьютера отметьте его в списке. Сверьте ID, если имена совпадают.");
                        ScrollArea::vertical().id_salt("device-access-list").max_height(164.0).show(ui, |ui| {
                            for (&id, member) in &workspace.calendar.members {
                                if id == state.device { continue; }
                                if workspace.is_revoked(id) {
                                    muted(ui, format!("{} · {id} · доступ отозван", member.value));
                                } else {
                                    let mut chosen = self.selected_revocations.contains(&id);
                                    if ui.checkbox(&mut chosen, format!("{} · {id}", member.value)).changed() {
                                        if chosen { self.selected_revocations.insert(id); } else { self.selected_revocations.remove(&id); }
                                    }
                                }
                            }
                        });
                        let action = if self.selected_revocations.is_empty() { "Перевыпустить доступ…" } else { "Отозвать доступ устройств…" };
                        if ui.add_enabled(!busy, Button::new(action)).clicked() {
                            self.confirm_access_renewal = true;
                        }
                    }
                    }
                    ui.separator();
                    ui.label("Коллега уже перевыпустил ключ");
                    muted(ui, "Отправьте коллеге запрос этого компьютера. Вставьте полученный код до закрытия приложения. Ваши сохранённые правки будут объединены с календарём команды.");
                    if ui.button("Скопировать запрос устройства").clicked() {
                        ctx.copy_text(network.pairing_request());
                        self.sync_message = Some(("Запрос скопирован.".into(), false));
                    }
                    let code_label = ui.label("Новый код приглашения");
                    ui.add(TextEdit::singleline(&mut self.join_code).password(true).char_limit(300)).labelled_by(code_label.id);
                    if ui.add_enabled(!busy && !self.join_code.trim().is_empty(), Button::new("Подключиться по новому коду")).clicked() { rejoin = true; }
                });
                ui.separator();
            }
            if workspace.access_manager.is_none_or(|id| id == state.device) {
                ui.collapsing("Устройства сотрудников", |ui| {
                    link = self
                        .device_assignments
                        .show(ui, workspace, state.device, busy);
                });
                ui.separator();
            }
            title(ui, "Подключённые устройства", SECTION_TITLE_SIZE);
            ScrollArea::vertical().max_height(135.0).show(ui, |ui| {
                for (&id, member) in &workspace.calendar.members {
                    let status = if id == state.device {
                        " · вы"
                    } else if workspace.is_revoked(id) {
                        " · доступ отозван"
                    } else if workspace.access_manager == Some(state.device) {
                        if state.admitted_this_generation.contains(&id) {
                            " · запрос принят"
                        } else {
                            " · ожидает подключения"
                        }
                    } else {
                        ""
                    };
                    let employee = workspace.calendar.person_for(id);
                    let name = if employee == id {
                        member.value.clone()
                    } else {
                        format!(
                            "{} · {}",
                            owner_name(&workspace.calendar, Some(employee)),
                            member.value
                        )
                    };
                    ui.label(format!("{name}{status}"))
                        .on_hover_text(id.to_string());
                }
            });
        });
        if let Some((device, employee)) = link {
            let result = self
                .store
                .lock()
                .unwrap()
                .link_employee_device(device, employee);
            self.device_assignments.finish(result);
            self.next_reminder_check = Instant::now();
            ctx.request_repaint();
        }
        if renew || rejoin {
            let result = (|| -> Fallible<_> {
                self.flush_local_drafts()?;
                let network = self.network.as_ref()?;
                if renew {
                    if self.selected_revocations.is_empty() {
                        Ok(network.renew_access_async())
                    } else {
                        Ok(network.revoke_devices_async(self.selected_revocations.clone()))
                    }
                } else {
                    let address = if self.address.trim().is_empty() {
                        None
                    } else {
                        Some(parse_address(&self.address)?)
                    };
                    Ok(network.rejoin_async(self.join_code.trim().to_owned(), address))
                }
            })();
            match result {
                Ok(job) => {
                    self.join_result = Some(job);
                    self.access_work = true;
                }
                Err(e) => self.sync_message = Some((e.to_string(), true)),
            }
            self.confirm_access_renewal = false;
        }
        self.connections = open;
        if !open {
            self.confirm_access_renewal = false;
            self.selected_revocations.clear();
            self.device_assignments = DeviceAssignments::default();
            self.sync_message = None;
        }
        if let Some(result) = address_result {
            self.sync_message = Some(match result {
                Ok(message) => (message.into(), false),
                Err(error) => (error.to_string(), true),
            });
            ctx.request_repaint();
        }
        if let Some(request) = file_request {
            self.start_file_exchange(ctx, request);
        }
    }

    fn integration_window(&mut self, ctx: &egui::Context, state: &LocalState) {
        if !self.integrations_open {
            return;
        }
        if self.api_controls.running_port().is_some()
            && !ctx.input(|input| input.viewport().minimized == Some(true))
        {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        let mut open = true;
        let mut action = None;
        settings_window(ctx, "Интеграции", &mut open, |ui| {
            title(ui, "Локальный API", SECTION_TITLE_SIZE);
            muted(
                ui,
                "Интеграции подключаются через godovoy-bridge по защищённому каналу.",
            );
            if let Some(progress) = self.api_controls.progress() {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new());
                    muted(ui, progress);
                });
            } else if let Some(port) = self.api_controls.running_port() {
                ui.colored_label(GREEN, "API включён");
                ui.label(RichText::new(format!("Защищённый порт {port}")).strong());
                ui.horizontal_wrapped(|ui| {
                    if let Some(settings) = state.api.as_ref() {
                        if ui.button("Копировать основной токен").clicked() {
                            ctx.copy_text(api::encode_token(&settings.token));
                            self.api_controls.message =
                                Some(("Основной токен скопирован.".into(), false));
                        }
                        if let Some(key) = settings.import_token
                            && ui.button("Копировать токен импорта").clicked()
                        {
                            ctx.copy_text(api::encode_token(&key));
                            self.api_controls.message =
                                Some(("Токен импорта скопирован.".into(), false));
                        }
                    }
                    if ui.button("Сменить токены…").clicked() {
                        self.confirm_token_change = true;
                    }
                    if ui.button("Выключить").clicked() {
                        action = Some(ApiAction::Disable);
                    }
                });
                if self.confirm_token_change {
                    ui.separator();
                    muted(
                        ui,
                        "Старые токены перестанут действовать. В подключённых системах потребуется ввести новые.",
                    );
                    ui.horizontal(|ui| {
                        if ui.button("Отмена").clicked() {
                            self.confirm_token_change = false;
                        }
                        if ui
                            .button(RichText::new("Заменить токены").color(RED))
                            .clicked()
                        {
                            self.confirm_token_change = false;
                            action = Some(ApiAction::Rotate);
                        }
                    });
                }
            } else {
                let configured = state.api.as_ref().is_some_and(|settings| settings.enabled);
                muted(
                    ui,
                    if configured {
                        "API не запустился"
                    } else {
                        "API выключен"
                    },
                );
                let label = if configured {
                    "Повторить запуск"
                } else {
                    "Включить API"
                };
                if ui.add(primary(label)).clicked() {
                    action = Some(ApiAction::Enable);
                }
                if configured && ui.button("Выключить").clicked() {
                    action = Some(ApiAction::Disable);
                }
                ui.collapsing("Настройки подключения", |ui| {
                    ui.horizontal(|ui| {
                        let port = ui.label("Порт");
                        ui.add(DragValue::new(&mut self.api_controls.port).range(1024..=65535))
                            .labelled_by(port.id);
                    });
                });
            }
            if let Some((message, error)) = &self.api_controls.message {
                ui.colored_label(if *error { RED } else { MUTED }, message);
            }
            Self::import_status_section(
                ui,
                &self.reader.import_status(),
                self.reader.import_status_warning(),
            );
        });
        self.integrations_open = open;
        if !open {
            self.confirm_token_change = false;
            if self
                .api_controls
                .message
                .as_ref()
                .is_some_and(|(_, error)| !error)
            {
                self.api_controls.message = None;
            }
        }
        if let Some(action) = action {
            self.api_controls.start(&self.store, action, Some(ctx));
        }
    }

    fn import_status_section(ui: &mut Ui, statuses: &[ImportStatus], warning: bool) {
        ui.separator();
        title(ui, "Журнал импорта", SECTION_TITLE_SIZE);
        if warning {
            ui.colored_label(
                RED,
                "Файл журнала импорта недоступен. Новые записи могут остаться только в памяти.",
            );
        }
        if statuses.is_empty() {
            muted(ui, "На этом компьютере пока нет записей об импорте.");
            return;
        }
        ScrollArea::vertical()
            .id_salt("integration-import-status")
            .max_height(230.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (index, status) in statuses.iter().enumerate() {
                    if index != 0 {
                        ui.separator();
                    }
                    ui.label(semibold(&status.source, 14.0, INK));
                    if let Some(bridge_run) = &status.bridge_run {
                        Self::bridge_run_summary(ui, bridge_run);
                    }
                    if let Some(success) = &status.success {
                        muted(
                            ui,
                            format!(
                                "Пачка принята через API · {} · событий {}, изменено {}",
                                success.at.with_timezone(&Local).format("%d.%m.%Y %H:%M"),
                                success.received,
                                success.changed,
                            ),
                        );
                    }
                    if let Some(failure) = &status.failure {
                        ui.colored_label(
                            RED,
                            format!(
                                "Отказ локального API · {}",
                                failure.at.with_timezone(&Local).format("%d.%m.%Y %H:%M"),
                            ),
                        );
                        let message = match failure.kind {
                            ImportResultKind::InvalidBatch => {
                                "Некорректная пачка. Проверьте формат и поля."
                            }
                            ImportResultKind::CalendarRejected => {
                                "Календарь отклонил запись. Проверьте состояние профиля и лимиты."
                            }
                        };
                        muted(ui, message);
                    }
                }
            });
    }

    fn bridge_run_summary(ui: &mut Ui, status: &BridgeRunStatus) {
        let run = &status.run;
        let (outcome, show_counts, failed) = match run.outcome {
            BridgeOutcome::Complete if run.fetched == 0 => ("Выгрузка пуста", false, false),
            BridgeOutcome::Complete => ("Завершён", true, false),
            BridgeOutcome::SourceUnavailable => ("Источник недоступен", false, true),
            BridgeOutcome::SourceRejected => ("Источник отклонил запрос", false, true),
            BridgeOutcome::SourceInvalid => ("Источник вернул некорректные данные", false, true),
            BridgeOutcome::BatchRejected => ("Локальный API отклонил пачку", true, true),
            BridgeOutcome::ApiUnavailable => ("Локальный API недоступен", true, true),
            BridgeOutcome::ConfirmationLost => ("Ответ от локального API потерян", true, true),
        };
        let label = format!(
            "Запуск моста · {} · {outcome}",
            status.at.with_timezone(&Local).format("%d.%m.%Y %H:%M"),
        );
        if failed {
            ui.colored_label(RED, label);
        } else {
            muted(ui, label);
        }
        if show_counts {
            let mut counts = format!(
                "Подтверждено {} из {}, изменено {}",
                run.confirmed, run.fetched, run.changed,
            );
            if matches!(run.outcome, BridgeOutcome::ConfirmationLost) {
                counts.push_str(". Последняя пачка могла быть записана.");
            }
            muted(ui, counts);
        }
    }

    fn report(&mut self, result: Fallible<()>, success: Option<&str>) {
        match result {
            Ok(()) => self.message = success.map(|value| (value.to_owned(), false)),
            Err(error) => self.fail(error.to_string()),
        }
    }

    fn fail(&mut self, message: impl Into<String>) {
        self.message = Some((message.into(), true));
    }

    fn draw_message(&self, ui: &mut Ui) {
        if let Some((message, is_error)) = &self.message {
            ui.colored_label(if *is_error { RED } else { MUTED }, message);
        }
    }

    fn agenda_page(
        &mut self,
        ui: &mut Ui,
        state: &Arc<LocalState>,
        workspace: &Arc<Workspace>,
        today: NaiveDate,
    ) {
        egui::CentralPanel::default()
            .frame(page_frame())
            .show_inside(ui, |ui| {
                title(ui, "Сроки команды", PAGE_TITLE_SIZE);
                ui.horizontal_wrapped(|ui| {
                    for (overdue, label) in [(false, "Ближайшие"), (true, "Просрочено")]
                    {
                        if view_option(ui, label, self.agenda_overdue == overdue) {
                            self.agenda_overdue = overdue;
                        }
                    }
                    ui.separator();
                    if !self.agenda_overdue {
                        for horizon in Horizon::ALL {
                            if view_option(
                                ui,
                                &format!("{} дней", horizon.days()),
                                self.horizon == horizon,
                            ) {
                                self.horizon = horizon;
                            }
                        }
                    }
                    ComboBox::from_id_salt("agenda-owner")
                        .width(190.0)
                        .selected_text(filter_name(&workspace.calendar, self.owner_filter))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.owner_filter,
                                OwnerFilter::Everyone,
                                "Все ответственные",
                            );
                            ui.selectable_value(
                                &mut self.owner_filter,
                                OwnerFilter::Unassigned,
                                "Без ответственного",
                            );
                            for (id, label) in employee_options(workspace, true) {
                                if ui.selectable_label(
                                    matches!(self.owner_filter, OwnerFilter::Member(member) if workspace.calendar.person_for(member) == id),
                                    label,
                                ).clicked() {
                                    self.owner_filter = OwnerFilter::Member(id);
                                }
                            }
                        });
                });
                let period = if self.agenda_overdue {
                    muted(
                        ui,
                        format!(
                            "Незавершённые пункты за последние {} дней",
                            planning::OVERDUE_DAYS
                        ),
                    );
                    AgendaPeriod::Overdue
                } else {
                    let end = today
                        .checked_add_days(Days::new(self.horizon.days() - 1))
                        .unwrap_or(NaiveDate::MAX);
                    muted(
                        ui,
                        format!(
                            "{} – {} · Подготовка, сроки пунктов и события",
                            short_date(today),
                            short_date(end)
                        ),
                    );
                    AgendaPeriod::Upcoming(self.horizon)
                };
                ui.separator();
                const WEEK_WIDTH: f32 = 106.0;
                const DATE_WIDTH: f32 = 85.0;
                const OWNER_WIDTH: f32 = 158.0;
                let table_width = ui.available_width() - ui.spacing().scroll.allocated_width();
                let title_width = (table_width
                    - WEEK_WIDTH
                    - DATE_WIDTH
                    - OWNER_WIDTH
                    - 3.0 * ui.spacing().item_spacing.x)
                    .max(140.0);
                ui.horizontal(|ui| {
                    for (label, width) in [
                        ("Неделя с", WEEK_WIDTH),
                        ("Дата", DATE_WIDTH),
                        ("Событие и этап", title_width),
                        ("Ответственный", OWNER_WIDTH),
                    ] {
                        agenda_cell(
                            ui,
                            width,
                            18.0,
                            RichText::new(label).size(12.0).color(MUTED),
                        );
                    }
                });
                let snapshot = Arc::downgrade(workspace);
                if !self.agenda.as_ref().is_some_and(|view| {
                    view.snapshot.ptr_eq(&snapshot)
                        && view.today == today
                        && view.period == period
                        && view.owner == self.owner_filter
                }) {
                    let rows = match period {
                        AgendaPeriod::Upcoming(horizon) => {
                            planning::agenda(&workspace.calendar, today, horizon, self.owner_filter)
                        }
                        AgendaPeriod::Overdue => {
                            planning::overdue(&workspace.calendar, today, self.owner_filter)
                        }
                    };
                    self.agenda = Some(AgendaView {
                        snapshot,
                        today,
                        period,
                        owner: self.owner_filter,
                        rows: rows.into(),
                    });
                }
                let Some(rows) = self.agenda.as_ref().map(|view| Arc::clone(&view.rows)) else {
                    return;
                };
                if rows.is_empty() {
                    muted(
                        ui,
                        match period {
                            AgendaPeriod::Upcoming(_) => "На выбранный период нет сроков.",
                            AgendaPeriod::Overdue => "За этот период нет просроченных пунктов.",
                        },
                    );
                    return;
                }
                let overdue = period == AgendaPeriod::Overdue;
                let display_date = |date: NaiveDate| {
                    if overdue && date.year() != today.year() {
                        date.format("%d.%m.%Y").to_string()
                    } else {
                        short_date(date)
                    }
                };
                ui.spacing_mut().item_spacing.y = 0.0;
                ScrollArea::vertical()
                    .id_salt((period, self.owner_filter))
                    .auto_shrink([false, false])
                    .show_rows(ui, LIST_ROW_HEIGHT, rows.len(), |ui, range| {
                        for index in range {
                            let row = rows[index];
                            let Some((event, caption, date_color)) =
                                resolve_item(state, row, today)
                            else {
                                continue;
                            };
                            let week_start = row.date.week(chrono::Weekday::Mon).first_day();
                            let first = index == 0 || rows[index - 1].date < week_start;
                            let band = ui
                                .allocate_ui_with_layout(
                                    vec2(table_width, LIST_ROW_HEIGHT),
                                    Layout::left_to_right(Align::Center),
                                    |ui| {
                                        ui.set_min_size(vec2(table_width, LIST_ROW_HEIGHT));
                                        agenda_cell(
                                            ui,
                                            WEEK_WIDTH,
                                            36.0,
                                            RichText::new(if first {
                                                display_date(week_start)
                                            } else {
                                                String::new()
                                            })
                                            .size(12.0)
                                            .color(MUTED),
                                        );
                                        agenda_cell(
                                            ui,
                                            DATE_WIDTH,
                                            36.0,
                                            RichText::new(display_date(row.date))
                                                .size(13.0)
                                                .color(date_color),
                                        );
                                        if item_button(
                                            ui,
                                            title_width,
                                            &event.event.title,
                                            &caption,
                                        )
                                        .clicked()
                                        {
                                            self.open_event(row.year, row.event);
                                        }
                                        agenda_cell(
                                            ui,
                                            OWNER_WIDTH,
                                            36.0,
                                            RichText::new(owner_name(
                                                &workspace.calendar,
                                                event.event.owner,
                                            ))
                                            .size(13.0)
                                            .color(MUTED),
                                        );
                                    },
                                )
                                .response
                                .rect;
                            row_rule(ui, band);
                        }
                    });
            });
    }

    fn reminders_page(
        &mut self,
        ui: &mut Ui,
        state: &LocalState,
        pending: &[Reminder],
        today: NaiveDate,
    ) {
        egui::CentralPanel::default()
            .frame(page_frame())
            .show_inside(ui, |ui| {
                title(ui, "Напоминания", PAGE_TITLE_SIZE);
                muted(ui, "Начало подготовки и сроки незавершённых пунктов");
                if let Ok(startup) = &mut self.startup {
                    let action = if startup.status == autostart::Status::NeedsUpdate {
                        muted(ui, "В автозапуске указан прежний путь к приложению.");
                        ui.horizontal_wrapped(|ui| {
                            if ui.button("Обновить путь").clicked() {
                                Some(true)
                            } else if ui.button("Выключить автозапуск").clicked() {
                                Some(false)
                            } else {
                                None
                            }
                        }).inner
                    } else {
                        let mut enabled = startup.status == autostart::Status::Enabled;
                        ui.checkbox(&mut enabled, "Запускать при входе в систему")
                            .changed().then_some(enabled)
                    };
                    if let Some(enabled) = action {
                        self.startup_error = match autostart::set(&startup.executable, enabled) {
                            Ok(()) => {
                                startup.status = if enabled { autostart::Status::Enabled } else { autostart::Status::Off };
                                None
                            }
                            Err(error) => Some(error.to_string()),
                        };
                    }
                } else if let Err(reason) = &self.startup {
                    muted(ui, reason);
                }
                if let Some(error) = &self.startup_error {
                    ui.colored_label(RED, format!("Не удалось изменить автозапуск. {error}"));
                }
                if !pending.is_empty() && self.notification.has_error() {
                    muted(
                        ui,
                        "Последняя попытка отправки не удалась. Приложение повторит её автоматически.",
                    );
                }
                ui.horizontal_wrapped(|ui| {
                    if ui.add_enabled(!self.notification.busy(), Button::new("Проверить уведомления")).clicked() {
                        self.notification.test(Instant::now());
                    }
                    if let Some(status) = self.notification.test_status {
                        let message = match status {
                            TestStatus::Sending => "Отправляем пробное уведомление…",
                            TestStatus::Requested => "Проверьте, появилось ли пробное уведомление.",
                            TestStatus::Failed => "Не удалось отправить. Проверьте настройки уведомлений ОС.",
                        };
                        muted(ui, message);
                    }
                });
                let issue = self.reader.snapshot().reminder_issue;
                if let Some(issue) = issue {
                    ui.colored_label(RED, issue.message());
                    if issue.can_recover() && ui.add_enabled(!self.reminder_work_busy(),
                        Button::new("Начать отметки заново")).clicked()
                    {
                        self.reminder_work.repair(&self.reminder_store, ui.ctx());
                    }
                }
                match self.reminder_work.status() {
                    ReminderWorkStatus::Idle => {}
                    ReminderWorkStatus::Repairing => {
                        muted(ui, "Восстанавливаем отметки…");
                    }
                    ReminderWorkStatus::Saving => {
                        muted(ui, "Сохраняем отметки…");
                    }
                    ReminderWorkStatus::Recovered(ReminderRecovery::Reset) => {
                        ui.colored_label(GREEN, "Отметки начаты заново. Исходный файл сохранён в профиле.");
                    }
                    ReminderWorkStatus::Recovered(ReminderRecovery::Reloaded) => {
                        ui.colored_label(GREEN, "Отметки загружены из профиля.");
                    }
                    ReminderWorkStatus::Failed(error) => {
                        ui.colored_label(RED, error);
                    }
                }
                self.draw_message(ui);
                ui.separator();
                if pending.is_empty() {
                    muted(ui, "Непрочитанных напоминаний нет.");
                    return;
                }
                let can_acknowledge = issue.is_none() && !matches!(self.reminder_work.status(), ReminderWorkStatus::Repairing);
                if ui
                    .add_enabled(can_acknowledge, Button::new("Отметить все прочитанными"))
                    .clicked()
                {
                    self.reminder_work.enqueue(pending, pending, today);
                    ui.ctx().request_repaint();
                }
                let mut acknowledged = None;
                let mut opened = None;
                const DATE_WIDTH: f32 = 96.0;
                const ACTION_WIDTH: f32 = 132.0;
                ui.spacing_mut().item_spacing.y = 0.0;
                ScrollArea::vertical()
                    .id_salt("local-reminders")
                    .auto_shrink([false, false])
                    .show_rows(ui, LIST_ROW_HEIGHT, pending.len(), |ui, range| {
                        for &row in &pending[range] {
                            let Some((event, caption, date_color)) = resolve_item(state, row, today) else {
                                continue;
                            };
                            let width = ui.available_width();
                            let band = ui
                                .allocate_ui_with_layout(
                                    vec2(width, LIST_ROW_HEIGHT),
                                    Layout::left_to_right(Align::Center),
                                    |ui| {
                                        ui.set_min_size(vec2(width, LIST_ROW_HEIGHT));
                                        let date = RichText::new(short_date(row.date))
                                            .size(13.0)
                                            .color(date_color);
                                        agenda_cell(ui, DATE_WIDTH, crate::theme::CONTROL_HEIGHT, date);
                                        let item_width = ui.available_width() - ui.spacing().item_spacing.x - ACTION_WIDTH;
                                        if item_button(
                                            ui,
                                            item_width,
                                            &event.event.title,
                                            &caption,
                                        )
                                        .clicked()
                                        {
                                            opened = Some(row);
                                        }
                                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                            if ui
                                                .add_enabled(can_acknowledge && !self.reminder_work.submitted(&row), quiet("Прочитано"))
                                                .on_hover_text("Отметить напоминание прочитанным")
                                                .clicked()
                                            {
                                                acknowledged = Some(row);
                                            }
                                        });
                                    },
                                )
                                .response
                                .rect;
                            row_rule(ui, band);
                        }
                    });
                if let Some(row) = opened {
                    if can_acknowledge {
                        self.reminder_work.enqueue(&[row], pending, today);
                        ui.ctx().request_repaint();
                    }
                    self.open_event(row.year, row.event);
                } else if let Some(row) = acknowledged {
                    self.reminder_work.enqueue(&[row], pending, today);
                    ui.ctx().request_repaint();
                }
            });
    }

    fn personal_view(
        &mut self,
        state: &Arc<LocalState>,
        receipts: &Arc<BTreeSet<Reminder>>,
        today: NaiveDate,
    ) {
        let Some(workspace) = &state.workspace else {
            self.personal = None;
            return;
        };
        let snapshot = Arc::downgrade(workspace);
        let receipt_snapshot = Arc::downgrade(receipts);
        if self.personal.as_ref().is_some_and(|view| {
            view.snapshot.ptr_eq(&snapshot)
                && view.device == state.device
                && view.receipts.ptr_eq(&receipt_snapshot)
                && view.today == today
        }) {
            return;
        }
        let events = planning::personal_events(&workspace.calendar, today, state.device);
        let pending = planning::due_reminders(&events, today)
            .into_iter()
            .filter(|reminder| !receipts.contains(reminder))
            .collect::<Vec<_>>();
        self.personal = Some(PersonalView {
            device: state.device,
            snapshot,
            receipts: receipt_snapshot,
            today,
            events: events.iter().map(|event| (event.id, event.year)).collect(),
            task_rows: None,
            pending: pending.into(),
            open_count: events
                .iter()
                .map(|event| event.event.tasks.len() - event.completed())
                .sum(),
        });
    }

    fn remind(
        &mut self,
        state: &Arc<LocalState>,
        receipts: &Arc<BTreeSet<Reminder>>,
        today: NaiveDate,
    ) {
        let now = Instant::now();
        self.next_reminder_check = now + Duration::from_secs(20);
        if !self.notification.ready(now) {
            return;
        }
        self.personal_view(state, receipts, today);
        let Some(view) = &self.personal else { return };
        let new = self.notification.new_reminders(&view.pending);
        self.notification.reminders(new, now);
    }
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        BACK.to_normalized_gamma_f32()
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_file_exchange();
        self.poll_api_controls();
        self.poll_backup_creation();
        if let Some(error) = self.reminder_work.poll(&self.reminder_store, ctx)
            && self.page != Page::Reminders
        {
            self.fail(error);
        }
        self.notification.poll(Instant::now());
        let join = self.join_result.as_ref().map(Receiver::try_recv);
        match join {
            Some(Ok(result)) => {
                self.join_result = None;
                if self.access_work {
                    self.renewed_profile = self.store.lock().unwrap().successor_directory();
                    self.sync_message = result.err().map(|e| {
                        let message = if self.network.as_ref().is_ok_and(|n| n.paused()) {
                            format!("{e} Прежняя синхронизация на паузе. Проверьте данные и повторите действие.")
                        } else { e.to_string() };
                        (message, true)
                    });
                    self.access_work = false;
                } else {
                    self.report(result, None);
                }
                self.next_reminder_check = Instant::now();
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.join_result = None;
                self.access_work = false;
                self.fail("Подключение было прервано.");
            }
            _ => {}
        }
        if Instant::now() >= self.next_reminder_check {
            let snapshot = self.reader.snapshot();
            self.remind(
                &snapshot.state,
                &snapshot.reminder_receipts,
                Local::now().date_naive(),
            );
        }
        let delay = if self.join_result.is_some()
            || self.file_exchange.job.is_some()
            || self.reminder_work_busy()
            || self.api_controls_busy()
            || self.backup_creation_busy()
            || self.notification.busy()
        {
            Duration::from_millis(250)
        } else if (self
            .network
            .as_ref()
            .is_ok_and(Network::can_sync_automatically)
            || self.api_controls.running_port().is_some())
            && !ctx.input(|input| input.viewport().minimized == Some(true))
        {
            // Other processes or peers can change the visible calendar.
            Duration::from_secs(2)
        } else {
            // egui subtracts a predicted frame from repaint delays. Compensate
            // when waiting for an absolute deadline to avoid spinning just before it.
            let frame_lead = ctx
                .input(|input| Duration::try_from_secs_f32(input.predicted_dt).unwrap_or_default());
            self.next_reminder_check
                .saturating_duration_since(Instant::now())
                .saturating_add(frame_lead)
        };
        ctx.request_repaint_after(delay);
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        if self.access_work {
            ui.spinner();
            ui.label("Обновляем доступ к календарю…");
            return;
        }
        let snapshot = self.reader.snapshot();
        let state = snapshot.state;
        let receipts = snapshot.reminder_receipts;
        let today = Local::now().date_naive();
        let Some(workspace) = &state.workspace else {
            self.show_onboarding(ui, today);
            return;
        };
        if self.editor.is_none()
            && self.capacity_prompt.is_none()
            && !self.connections
            && !self.integrations_open
            && !self.backup_open
        {
            if ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::N)) {
                self.request_new_event(ui.ctx(), snapshot.usage, |year| Editor::new(year, today));
            }
            if ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::F)) {
                self.page = Page::Calendar;
                self.search_focus = true;
            }
        }
        self.personal_view(&state, &receipts, today);
        let (open_count, pending) = self.personal.as_ref().map_or_else(
            || (0, Arc::from([])),
            |view| (view.open_count, Arc::clone(&view.pending)),
        );
        self.app_bar(
            ui,
            workspace,
            open_count,
            pending.len(),
            snapshot.usage.near_limit(),
        );
        self.backup_creation_notice(ui);
        self.draft_banner(ui, &workspace.calendar);
        match self.page {
            Page::Calendar => self.calendar(ui, workspace, today, snapshot.usage),
            Page::Tasks => self.tasks(ui, today, &workspace.calendar),
            Page::Agenda => self.agenda_page(ui, &state, workspace, today),
            Page::Reminders => self.reminders_page(ui, &state, &pending, today),
        }
        self.edit_window(ui.ctx(), workspace, snapshot.usage);
        self.capacity_window(ui.ctx(), snapshot.usage);
        self.sync_window(ui.ctx(), &state, workspace, snapshot.usage);
        self.integration_window(ui.ctx(), &state);
        self.backup_window(ui.ctx());
        self.checkpoint_local_drafts(ui.ctx());
    }
}

fn settings_window(ctx: &egui::Context, name: &str, open: &mut bool, body: impl FnOnce(&mut Ui)) {
    egui::Window::new(name)
        .open(open)
        .frame(dialog(ctx))
        .default_width(480.0)
        .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
        .collapsible(false)
        .resizable([true, false])
        .show(ctx, |ui| {
            ScrollArea::vertical()
                .auto_shrink([false, true])
                .max_height((ctx.content_rect().height() - 132.0).max(100.0))
                .show(ui, body);
        });
}

fn task_checkbox(
    ui: &mut Ui,
    checked: &mut bool,
    task: &Task,
    event: Occurrence<'_>,
    today: NaiveDate,
) -> bool {
    ui.spacing_mut().interact_size.y = 24.0;
    ui.spacing_mut().item_spacing.y = 1.0;
    let color = if *checked { MUTED } else { INK };
    let checkbox = ui
        .checkbox(checked, RichText::new(&task.title).color(color))
        .on_hover_text(&task.title);
    if !*checked {
        let deadline = task_deadline(event.due(task), event.date, today);
        checkbox.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::WidgetType::Checkbox,
                ui.is_enabled(),
                false,
                format!("{}, {}", task.title, deadline.text()),
            )
        });
        ui.spacing_mut().interact_size.y = 15.0;
        ui.horizontal(|ui| {
            ui.add_space(ui.spacing().icon_width + ui.spacing().icon_spacing);
            ui.label(deadline);
        });
    }
    checkbox.changed()
}

fn task_alert(due: NaiveDate, today: NaiveDate) -> Option<(&'static str, egui::Color32)> {
    match due.cmp(&today) {
        std::cmp::Ordering::Less => Some(("Просрочено", RED)),
        std::cmp::Ordering::Equal => Some(("Сегодня", DUE_TODAY)),
        std::cmp::Ordering::Greater => None,
    }
}

fn task_deadline(due: NaiveDate, event: NaiveDate, today: NaiveDate) -> RichText {
    let date = deadline_label(due, event);
    let (caption, color) = match task_alert(due, today) {
        Some((status, color)) => (format!("{status} · {date}"), color),
        None => (format!("До {date}"), MUTED),
    };
    RichText::new(caption).size(12.0).color(color)
}

fn deadline_label(date: NaiveDate, event: NaiveDate) -> String {
    if date.year() == event.year() {
        short_date(date)
    } else {
        format!("{} {}", short_date(date), date.year())
    }
}

fn date_picker(
    ui: &mut Ui,
    label: &str,
    selected: NaiveDate,
    min: NaiveDate,
    max: NaiveDate,
) -> Option<NaiveDate> {
    if min > max {
        muted(ui, "Недопустимый диапазон дат");
        return None;
    }
    let response = ui.button(label).on_hover_text("Выбрать дату");
    let month_id = response.id.with("month");
    let selected_month = selected.clamp(min, max).with_day(1).unwrap();
    if response.clicked() {
        ui.ctx()
            .data_mut(|data| data.insert_temp(month_id, selected_month));
    }
    let mut chosen = None;
    egui::Popup::menu(&response)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            const GUTTER: f32 = 4.0;
            let cell = theme::CONTROL_HEIGHT;
            let width = 7.0 * cell + 6.0 * GUTTER;
            ui.set_width(width);
            ui.spacing_mut().button_padding.x = GUTTER;
            let mut month = ui
                .ctx()
                .data(|data| data.get_temp(month_id))
                .unwrap_or(selected_month);
            ui.horizontal(|ui| {
                let previous = month
                    .checked_sub_months(Months::new(1))
                    .filter(|value| *value >= min.with_day(1).unwrap());
                let next = month
                    .checked_add_months(Months::new(1))
                    .filter(|value| *value <= max);
                if ui
                    .add_enabled(
                        previous.is_some(),
                        Button::new("‹").min_size(Vec2::splat(cell)),
                    )
                    .on_hover_text("Предыдущий месяц")
                    .clicked()
                    && let Some(previous) = previous
                {
                    month = previous;
                }
                ui.add_sized(
                    [width - 2.0 * (cell + ui.spacing().item_spacing.x), cell],
                    egui::Label::new(
                        RichText::new(format!(
                            "{} {}",
                            MONTH_NAMES[month.month0() as usize],
                            month.year()
                        ))
                        .strong(),
                    )
                    .halign(Align::Center),
                );
                if ui
                    .add_enabled(next.is_some(), Button::new("›").min_size(Vec2::splat(cell)))
                    .on_hover_text("Следующий месяц")
                    .clicked()
                    && let Some(next) = next
                {
                    month = next;
                }
            });
            ui.ctx().data_mut(|data| data.insert_temp(month_id, month));
            egui::Grid::new(month_id)
                .min_col_width(cell)
                .min_row_height(0.0)
                .spacing(Vec2::splat(GUTTER))
                .show(ui, |ui| {
                    for caption in ["Пн", "Вт", "Ср", "Чт", "Пт", "Сб", "Вс"] {
                        ui.add_sized(
                            [cell, 20.0],
                            egui::Label::new(RichText::new(caption).color(MUTED))
                                .halign(Align::Center),
                        );
                    }
                    ui.end_row();
                    let start = month - Days::new(month.weekday().num_days_from_monday().into());
                    for index in 0..42 {
                        let day = start + Days::new(index);
                        if day.month() == month.month() {
                            if ui
                                .add_enabled(
                                    (min..=max).contains(&day),
                                    Button::selectable(day == selected, day.day().to_string())
                                        .min_size(Vec2::splat(cell)),
                                )
                                .on_hover_text(day.format("%d.%m.%Y").to_string())
                                .clicked()
                            {
                                chosen = Some(day);
                                ui.close();
                            }
                        } else {
                            ui.allocate_space(Vec2::splat(cell));
                        }
                        if index % 7 == 6 {
                            ui.end_row();
                        }
                    }
                });
        });
    chosen
}

const MONTH_NAMES: [&str; 12] = [
    "Январь",
    "Февраль",
    "Март",
    "Апрель",
    "Май",
    "Июнь",
    "Июль",
    "Август",
    "Сентябрь",
    "Октябрь",
    "Ноябрь",
    "Декабрь",
];

fn search_key(text: &str) -> String {
    let lower = text.trim().to_lowercase();
    if lower.contains('ё') {
        lower.replace('ё', "е")
    } else {
        lower
    }
}

fn task_rows(
    calendar: &Calendar,
    events: &[(Id, i32)],
    open_count: usize,
    today: NaiveDate,
    filter: TaskFilter,
) -> Vec<TaskRow> {
    let tomorrow = today.succ_opt().unwrap_or(today);
    let mut rows = if filter == TaskFilter::All {
        Vec::with_capacity(events.len() + open_count)
    } else {
        Vec::new()
    };
    let mut open = Vec::new();
    for &(id, year) in events {
        let Some(event) = calendar.event(id, year) else {
            continue;
        };
        let header = TaskRow {
            event: id,
            year,
            task: None,
        };
        open.clear();
        open.extend(
            event
                .event
                .tasks
                .iter()
                .enumerate()
                .filter_map(|(index, task)| {
                    if event.checked(task.id) {
                        return None;
                    }
                    let due = event.due(task);
                    let matches = match filter {
                        TaskFilter::All => true,
                        TaskFilter::TodayAndTomorrow => (today..=tomorrow).contains(&due),
                        TaskFilter::Overdue => due < today,
                    };
                    matches.then_some((due, index))
                }),
        );
        if filter != TaskFilter::All && open.is_empty() {
            continue;
        }
        rows.push(header);
        open.sort_unstable();
        rows.extend(open.iter().map(|&(_, index)| TaskRow {
            task: Some(index),
            ..header
        }));
    }
    rows
}

const LIST_ROW_HEIGHT: f32 = 48.0;

fn row_rule(ui: &Ui, rect: egui::Rect) {
    ui.painter().line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(1.0, BORDER),
    );
}

fn agenda_cell(ui: &mut Ui, width: f32, height: f32, text: RichText) {
    ui.allocate_ui_with_layout(
        vec2(width, height),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.set_width(width);
            ui.add_space(8.0);
            ui.add(egui::Label::new(text).truncate());
        },
    );
}

fn item_button(ui: &mut Ui, width: f32, title: &str, caption: &str) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(vec2(width.max(140.0), 44.0), egui::Sense::click());
    let full_label = format!("{title} · {caption}");
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &full_label));
    let painter = ui.painter_at(rect);
    if response.hovered() {
        painter.rect_filled(rect, 5, BACK);
    }
    if response.has_focus() {
        painter.rect_stroke(rect, 5, Stroke::new(2.0, ACCENT), egui::StrokeKind::Inside);
    }
    for (text, font, color, y) in [
        (title, heading_font(14.0), INK, 3.0),
        (caption, egui::FontId::proportional(12.0), MUTED, 23.0),
    ] {
        let mut job =
            egui::text::LayoutJob::simple(text.to_owned(), font, color, rect.width() - 16.0);
        job.wrap.max_rows = 1;
        let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
        painter.galley(rect.min + vec2(8.0, y), galley, color);
    }
    response.on_hover_text(full_label)
}

fn filter_name(calendar: &Calendar, filter: OwnerFilter) -> &str {
    match filter {
        OwnerFilter::Everyone => "Все ответственные",
        OwnerFilter::Unassigned => "Без ответственного",
        OwnerFilter::Member(id) => owner_name(calendar, Some(id)),
    }
}

fn resolve_item(
    state: &LocalState,
    row: AgendaItem,
    today: NaiveDate,
) -> Option<(Occurrence<'_>, String, egui::Color32)> {
    let event = state
        .workspace
        .as_ref()?
        .calendar
        .event(row.event, row.year)?;
    let (caption, color) = match row.kind {
        Milestone::Preparation => ("Начало подготовки".to_owned(), MUTED),
        Milestone::Event => ("Событие".to_owned(), MUTED),
        Milestone::Task(id) => {
            let task = event.event.tasks.iter().find(|task| task.id == id)?;
            let (status, color) = task_alert(row.date, today).unwrap_or(("Срок", MUTED));
            (format!("{status} · {}", task.title), color)
        }
    };
    Some((event, caption, color))
}

fn owner_name(calendar: &Calendar, id: Option<Id>) -> &str {
    match id {
        Some(id) => calendar.person_name(id).unwrap_or("Неизвестный сотрудник"),
        None => "Без ответственного",
    }
}

fn parse_address(input: &str) -> Fallible<SocketAddrV4> {
    let input = input.trim();
    let result = if input.contains(':') {
        input.parse()
    } else {
        format!("{input}:{SYNC_PORT}").parse()
    };
    result.map_err(|_| "Введите IPv4-адрес, например 192.168.1.15:45736.".into())
}

pub fn short_date(date: NaiveDate) -> String {
    const MONTH_NAMES: [&str; 12] = [
        "января",
        "февраля",
        "марта",
        "апреля",
        "мая",
        "июня",
        "июля",
        "августа",
        "сентября",
        "октября",
        "ноября",
        "декабря",
    ];
    format!("{} {}", date.day(), MONTH_NAMES[date.month0() as usize])
}

pub fn event_count(count: usize) -> String {
    let unit = match (count % 10, count % 100) {
        (_, 11..=14) => "событий",
        (1, _) => "событие",
        (2..=4, _) => "события",
        _ => "событий",
    };
    format!("{count} {unit}")
}

fn navigation_count(label: &str, count: usize) -> String {
    if count == 0 {
        label.to_owned()
    } else {
        format!("{label} · {count}")
    }
}

#[cfg(test)]
mod design_interactions;

#[cfg(test)]
mod transport_tests;
