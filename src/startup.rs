use std::{
    net::{Ipv4Addr, SocketAddrV4},
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, TryRecvError, TrySendError, sync_channel},
};

use chrono::Local;
use eframe::egui::{self, Align, Layout, Ui, Vec2, ViewportCommand, vec2};
use godovoy_plan::{
    APP_ID, APP_NAME, Fallible,
    network::{Config, Network},
    storage::{
        BackupEntry, ProfileOpenIssue, ProfileSizeLimit, RestoreResult, RestoredProfile,
        SharedStore, Store,
    },
};

use crate::{
    theme::{self, ACCENT, BACK, RED, brand, card, dialog, muted, primary, title},
    ui::{App, LocalWorkStatus, RestoreRequest, spawn_worker},
};

const APP_SIZE: Vec2 = vec2(1340.0, 850.0);
const APP_MIN_SIZE: Vec2 = vec2(800.0, 500.0);
const RECOVERY_SIZE: Vec2 = vec2(710.0, 400.0);
const RECOVERY_MIN_SIZE: Vec2 = vec2(600.0, 360.0);
const PICKER_SIZE: Vec2 = vec2(RECOVERY_SIZE.x, 510.0);
const PICKER_MIN_SIZE: Vec2 = vec2(RECOVERY_MIN_SIZE.x, 470.0);

pub fn run(
    directory: PathBuf,
    config: Config,
    at_login: bool,
    standard_profile: bool,
) -> Fallible<()> {
    let (directory, initial) = match Store::active_directory(&directory) {
        Ok(active) => {
            let opened = open_profile(&active, active != directory);
            (active, opened)
        }
        Err(error) => (directory, Err(error)),
    };
    if at_login
        && initial
            .as_ref()
            .err()
            .and_then(|error| error.downcast_ref())
            == Some(&ProfileOpenIssue::Locked)
    {
        return Ok(());
    }
    let starts_minimized = at_login && initial.is_ok();
    let (size, min_size) = if initial.is_ok() {
        (APP_SIZE, APP_MIN_SIZE)
    } else {
        (RECOVERY_SIZE, RECOVERY_MIN_SIZE)
    };
    let viewport = egui::ViewportBuilder::default()
        .with_title(APP_NAME)
        .with_app_id(APP_ID)
        .with_active(!starts_minimized)
        .with_inner_size(size)
        .with_min_inner_size(min_size)
        .with_icon(crate::app_icon::window_icon());
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |context| {
            theme::setup(&context.egui_ctx);
            let mut desktop = Desktop {
                larger_font_atlas: None,
                directory,
                config,
                standard_profile,
                app: None,
                issue: None,
                existing_required: false,
                copied_path: false,
                picker: None,
                restored: None,
                restore_work: None,
                exit: ExitState::Idle,
                exit_warning: None,
            };
            desktop.start(initial);
            if let Some(error) = &desktop.issue {
                eprintln!("{APP_NAME}: {}", godovoy_plan::terminal_message(error));
            }
            if starts_minimized {
                context
                    .egui_ctx
                    .send_viewport_cmd(ViewportCommand::Minimized(true));
            }
            Ok(Box::new(desktop))
        }),
    )
    .map_err(|error| error.to_string().into())
}

fn open_profile(directory: &Path, existing_required: bool) -> Fallible<SharedStore> {
    if existing_required {
        Store::open_existing(directory)
    } else {
        Store::open(directory)
    }
}

pub(crate) fn start_network(store: SharedStore, mut config: Config) -> Result<Network, String> {
    match Network::start(store.clone(), config) {
        Ok(network) => Ok(network),
        Err(error) if *config.listen.ip() == Ipv4Addr::UNSPECIFIED => {
            config.listen = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0);
            Network::start(store.clone(), config).map_err(|second| format!("{error}. {second}"))
        }
        Err(error) => Err(error.to_string()),
    }
}

struct BackupPicker {
    copies: Vec<BackupEntry>,
    selected: usize,
    confirm: bool,
    error: Option<String>,
}

#[derive(Default)]
enum PickerAction {
    #[default]
    Wait,
    Back,
    Apply(BackupEntry),
}

impl BackupPicker {
    fn show(&mut self, ui: &mut Ui) -> PickerAction {
        let mut action = PickerAction::Wait;
        let chosen = self.copies.get(self.selected).filter(|_| self.confirm);
        recovery_panel(ui, |ui| {
            title(
                ui,
                if chosen.is_some_and(BackupEntry::is_restore_point) {
                    "Вернуть прежний календарь"
                } else {
                    "Восстановить календарь"
                },
                theme::PAGE_TITLE_SIZE,
            );
            if self.copies.is_empty() {
                ui.label("В профиле пока нет копий для восстановления.");
                muted(
                    ui,
                    "Если у вас сохранена папка backups, верните её в папку профиля и откройте этот экран снова.",
                );
            } else if let Some(chosen) = chosen {
                ui.label(egui::RichText::new(backup_label(chosen)).strong());
                ui.label("Обмен с коллегами будет на паузе до ручного возобновления. Пауза сохраняется после перезапуска.");
                muted(
                    ui,
                    "После возобновления могут вернуться свежие правки коллег. Интеграции могут продолжить импорт.",
                );
                muted(
                    ui,
                    "Исходный файл сохранится отдельно, если он есть. В нём могут быть ключ синхронизации и токены API.",
                );
            } else {
                muted(ui, "Выберите копию, которую хотите восстановить.");
                egui::ScrollArea::vertical()
                    .max_height(160.0)
                    .show(ui, |ui| {
                        for (index, copy) in self.copies.iter().enumerate() {
                            ui.selectable_value(&mut self.selected, index, backup_label(copy));
                        }
                    });
            }
            if let Some(error) = &self.error {
                ui.colored_label(RED, error);
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if let Some(chosen) = chosen {
                        if ui
                            .button(egui::RichText::new("Восстановить копию").color(RED))
                            .clicked()
                        {
                            action = PickerAction::Apply(chosen.clone());
                        }
                        if ui.button("К списку копий").clicked() {
                            self.confirm = false;
                            self.error = None;
                        }
                    } else if ui
                        .add_enabled(!self.copies.is_empty(), primary("Продолжить"))
                        .clicked()
                    {
                        self.confirm = true;
                        self.error = None;
                    }
                    if ui.button("Отмена").clicked() {
                        action = PickerAction::Back;
                    }
                });
            });
        });
        action
    }
}

fn backup_label(entry: &BackupEntry) -> String {
    let kind = if entry.is_restore_point() {
        "Точка возврата"
    } else {
        "Резервная копия"
    };
    let date = entry
        .created
        .with_timezone(&Local)
        .format("%d.%m.%Y, %H:%M:%S");
    format!("{kind} · {date}")
}

struct RestoreWork {
    completed: Receiver<RestoreCompletion>,
    recovery_window: bool,
}

struct RestoreCompletion {
    previous: Option<Box<App>>,
    outcome: Fallible<RestoredProfile>,
}

fn restore_calendar(
    directory: &Path,
    copy: &BackupEntry,
    previous: &mut Option<Box<App>>,
) -> Fallible<RestoredProfile> {
    if let Some(app) = previous.as_ref() {
        let (store, result) = app.restore_open_backup(copy)?;
        // Draining the old listeners can wait for active requests.
        drop(previous.take());
        Ok(RestoredProfile {
            result,
            store: Ok(store),
        })
    } else {
        Store::restore_and_open_backup(directory, copy)
    }
}

#[derive(Clone, Copy)]
enum ExitState {
    Idle,
    WaitingForRestore,
    WaitingForLocalWork {
        started: LocalWorkStatus,
    },
    Confirm {
        previous_focus: Option<egui::Id>,
        focus_continue: bool,
    },
    ClosingApi,
    Exiting,
}

struct Desktop {
    larger_font_atlas: Option<usize>,
    directory: PathBuf,
    config: Config,
    standard_profile: bool,
    app: Option<Box<App>>,
    issue: Option<String>,
    existing_required: bool,
    copied_path: bool,
    picker: Option<BackupPicker>,
    restored: Option<RestoreResult>,
    restore_work: Option<RestoreWork>,
    exit: ExitState,
    exit_warning: Option<String>,
}

impl Desktop {
    fn start(&mut self, store: Fallible<SharedStore>) {
        match store {
            Ok(store) => {
                let mut app = App::open(store, self.config, self.standard_profile);
                if let Some(result) = self.restored.take() {
                    app.report_restore(result);
                }
                self.app = Some(Box::new(app));
                self.issue = None;
            }
            Err(error) => {
                self.existing_required |= error.is::<ProfileOpenIssue>();
                self.issue = Some(error.to_string());
            }
        }
    }

    fn try_open(&mut self, ctx: &egui::Context) {
        match Store::active_directory(&self.directory) {
            Ok(active) => {
                self.existing_required |= active != self.directory;
                self.directory = active;
            }
            Err(error) => {
                self.issue = Some(error.to_string());
                return;
            }
        }
        let store = open_profile(&self.directory, self.existing_required);
        self.start(store);
        if self.app.is_some() {
            resize(ctx, APP_SIZE, APP_MIN_SIZE);
        }
        ctx.request_repaint();
    }

    fn begin_picker(&mut self, ctx: &egui::Context, request: RestoreRequest) {
        let (copies, mut error) = match Store::restore_choices(&self.directory) {
            Ok(copies) => (copies, None),
            Err(error) => (Vec::new(), Some(error.to_string())),
        };
        let selected = match (request, error.is_none()) {
            (RestoreRequest::ReturnTo(point), true) => {
                let selected = copies.iter().position(|candidate| *candidate == point);
                if selected.is_none() {
                    error =
                        Some("Точка возврата больше недоступна. Выберите копию из списка.".into());
                }
                selected
            }
            _ => None,
        };
        self.picker = Some(BackupPicker {
            copies,
            selected: selected.unwrap_or(0),
            confirm: selected.is_some(),
            error,
        });
        if self.app.is_none() {
            resize(ctx, PICKER_SIZE, PICKER_MIN_SIZE);
        }
        ctx.request_repaint();
    }

    fn picker_ui(&mut self, ui: &mut Ui) {
        let Some(picker) = &mut self.picker else {
            return;
        };
        match picker.show(ui) {
            PickerAction::Wait => {}
            PickerAction::Back => {
                self.picker = None;
                if self.app.is_none() {
                    resize(ui.ctx(), RECOVERY_SIZE, RECOVERY_MIN_SIZE);
                }
                ui.ctx().request_repaint();
            }
            PickerAction::Apply(copy) => self.apply_restore(ui.ctx(), copy),
        }
    }

    fn apply_restore(&mut self, ctx: &egui::Context, copy: BackupEntry) {
        let (send_app, receive_app) = sync_channel::<Option<Box<App>>>(1);
        let directory = self.directory.clone();
        let completed = match spawn_worker("calendar-restore", ctx, move || {
            let Ok(mut previous) = receive_app.recv() else {
                return None;
            };
            let outcome = restore_calendar(&directory, &copy, &mut previous);
            Some(RestoreCompletion { previous, outcome })
        }) {
            Ok(completed) => completed,
            Err(_) => {
                self.restore_error("Не удалось запустить восстановление.".into());
                ctx.request_repaint();
                return;
            }
        };
        let recovery_window = self.app.is_none();
        // Move the live profile only after the worker has been created.
        if let Err(TrySendError::Disconnected(app) | TrySendError::Full(app)) =
            send_app.try_send(self.app.take())
        {
            self.app = app;
            self.restore_error("Не удалось начать восстановление.".into());
            ctx.request_repaint();
            return;
        }
        self.existing_required = true;
        self.restore_work = Some(RestoreWork {
            completed,
            recovery_window,
        });
        ctx.request_repaint();
    }

    fn poll_restore(&mut self, ctx: &egui::Context) {
        let Some(work) = &self.restore_work else {
            return;
        };
        let completion = match work.completed.try_recv() {
            Ok(completion) => Some(completion),
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => None,
        };
        let recovery_window = work.recovery_window;
        self.restore_work = None;
        let close_after_restore = matches!(self.exit, ExitState::WaitingForRestore);
        self.exit = ExitState::Idle;
        if let Some(RestoreCompletion { previous, outcome }) = completion {
            self.app = previous;
            match outcome {
                Ok(RestoredProfile { store, result }) => {
                    self.picker = None;
                    if close_after_restore && store.is_ok() {
                        drop(store);
                        self.finish_exit(ctx);
                    } else {
                        self.restored = Some(result);
                        self.start(store);
                        if self.app.is_some() && recovery_window {
                            resize(ctx, APP_SIZE, APP_MIN_SIZE);
                        }
                    }
                }
                Err(error) => self.restore_error(restore_failure_message(error)),
            }
        } else {
            self.picker = None;
            self.issue = Some("Восстановление прервано. Попробуйте открыть календарь.".into());
            resize(ctx, RECOVERY_SIZE, RECOVERY_MIN_SIZE);
        }
        ctx.request_repaint();
    }

    fn restore_error(&mut self, error: String) {
        if let Some(picker) = &mut self.picker {
            picker.error = Some(error);
        }
    }

    fn restore_progress(&mut self, ui: &mut Ui) {
        recovery_panel(ui, |ui| {
            title(ui, "Восстанавливаем календарь", theme::PAGE_TITLE_SIZE);
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(16.0).color(ACCENT));
                muted(ui, "Проверяем копию и сохраняем прежний календарь.");
            });
            if matches!(self.exit, ExitState::WaitingForRestore) {
                ui.label("Приложение закроется после завершения восстановления.");
                if ui.add(primary("Остаться в приложении")).clicked() {
                    self.exit = ExitState::Idle;
                }
            }
        });
    }

    fn recovery_ui(&mut self, ui: &mut Ui) {
        let mut retry = false;
        let mut choose = false;
        recovery_panel(ui, |ui| {
            title(ui, "Не удалось открыть календарь", theme::PAGE_TITLE_SIZE);
            if let Some(error) = &self.issue {
                ui.label(error);
            }
            ui.add_space(7.0);
            muted(ui, "Папка профиля");
            ui.add(egui::Label::new(self.directory.to_string_lossy()).wrap());
            ui.separator();
            ui.horizontal_wrapped(|ui| {
                retry = ui.add(primary("Повторить попытку")).clicked();
                choose = ui.button("Восстановить из копии…").clicked();
                if ui.button("Закрыть").clicked() {
                    ui.ctx().send_viewport_cmd(ViewportCommand::Close);
                }
            });
            if ui.button("Скопировать путь").clicked() {
                ui.ctx()
                    .copy_text(self.directory.to_string_lossy().into_owned());
                self.copied_path = true;
            }
            if self.copied_path {
                muted(ui, "Путь скопирован.");
            }
        });
        if choose {
            self.begin_picker(ui.ctx(), RestoreRequest::Choose);
        } else if retry {
            self.try_open(ui.ctx());
        }
    }

    fn confirm_exit(
        &mut self,
        ctx: &egui::Context,
        previous_focus: Option<egui::Id>,
        focus_continue: bool,
    ) {
        let mut keep = false;
        let mut save_drafts = false;
        let mut discard = false;
        let modal = egui::Modal::new(egui::Id::new("confirm-exit"))
            .frame(dialog(ctx))
            .backdrop_color(theme::MODAL_BACKDROP)
            .show(ctx, |ui| {
                ui.set_width(540.0);
                title(ui, "Закрыть приложение?", theme::DIALOG_TITLE_SIZE);
                muted(
                    ui,
                    "Сохраните черновики, чтобы продолжить работу при следующем запуске.",
                );
                if let Some(warning) = &self.exit_warning {
                    ui.colored_label(RED, warning);
                }
                ui.separator();
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let proceed = ui.add(primary("Продолжить работу"));
                    if focus_continue {
                        proceed.request_focus();
                    }
                    keep = proceed.clicked();
                    save_drafts = ui.button("Выйти с черновиками").clicked();
                    discard = ui
                        .button(egui::RichText::new("Удалить черновики и выйти").color(RED))
                        .clicked();
                });
            });
        if save_drafts || discard {
            if let Some(app) = &mut self.app {
                let result = if discard {
                    app.discard_local_drafts()
                } else {
                    app.flush_local_drafts()
                };
                if let Err(error) = result {
                    self.exit_warning = Some(error.to_string());
                    ctx.request_repaint();
                    return;
                }
            }
            self.finish_exit(ctx);
        } else if keep || modal.should_close() {
            self.exit_warning = None;
            self.exit = ExitState::Idle;
            if let Some(previous_focus) = previous_focus {
                ctx.memory_mut(|memory| memory.request_focus(previous_focus));
            }
            ctx.request_repaint();
        } else if focus_continue {
            self.exit = ExitState::Confirm {
                previous_focus,
                focus_continue: false,
            };
        }
    }

    fn finish_exit(&mut self, ctx: &egui::Context) {
        if let Some(app) = &mut self.app
            && app.close_api_listener(ctx)
        {
            self.exit = if app.local_work_status().api.busy {
                ExitState::ClosingApi
            } else {
                ExitState::Idle
            };
            ctx.request_repaint();
        } else {
            self.exit = ExitState::Exiting;
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }
}

// egui uses the renderer's reported maximum as its initial RGBA glyph atlas width.
// Keep its native limit available if many glyphs or large type need more room.
fn font_atlas_limit(
    renderer_limit: Option<usize>,
    larger: &mut Option<usize>,
    crowded: impl FnOnce() -> bool,
) -> Option<usize> {
    const INITIAL_LIMIT: usize = 4096;
    if let Some(limit) = renderer_limit {
        *larger = (limit > INITIAL_LIMIT).then_some(limit);
        Some(limit.min(INITIAL_LIMIT))
    } else if larger.is_some() && crowded() {
        larger.take()
    } else {
        None
    }
}

impl eframe::App for Desktop {
    fn raw_input_hook(&mut self, ctx: &egui::Context, input: &mut egui::RawInput) {
        input.max_texture_side =
            font_atlas_limit(input.max_texture_side, &mut self.larger_font_atlas, || {
                ctx.fonts(|fonts| fonts.font_atlas_fill_ratio() > 0.6)
            });
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        BACK.to_normalized_gamma_f32()
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(app) = &mut self.app
            && let Err(error) = app.flush_local_drafts()
        {
            eprintln!(
                "{APP_NAME}: Не удалось сохранить локальные черновики. {}",
                godovoy_plan::terminal_message(&error.to_string())
            );
        }
    }

    fn logic(&mut self, context: &egui::Context, frame: &mut eframe::Frame) {
        let close_requested = context.input(|input| input.viewport().close_requested());
        if self.restore_work.is_some() {
            if close_requested {
                self.exit = ExitState::WaitingForRestore;
                context.send_viewport_cmd(ViewportCommand::CancelClose);
            }
            self.poll_restore(context);
            return;
        }
        if close_requested && self.app.is_none() {
            self.exit = ExitState::Exiting;
            return;
        }
        let backup_was_writing = close_requested
            && self
                .app
                .as_ref()
                .is_some_and(|app| app.local_work_status().backup.busy);
        if let Some(app) = &mut self.app {
            app.logic(context, frame);
        }
        if let Some(directory) = self.app.as_mut().and_then(|app| app.renewed_profile.take()) {
            drop(self.app.take());
            self.directory = directory;
            self.existing_required = true;
            self.try_open(context);
            if let Some(app) = self.app.as_mut() {
                app.show_renewed_access();
            }
            return;
        }
        let waiting = match self.exit {
            ExitState::WaitingForLocalWork { started } => Some(started),
            _ => None,
        };
        if (waiting.is_some() || (close_requested && matches!(self.exit, ExitState::Idle)))
            && let Some(app) = self.app.as_deref()
        {
            let current = app.local_work_status();
            let started =
                waiting.unwrap_or_else(|| current.with_backup_was_writing(backup_was_writing));
            if current.any_busy() {
                if waiting.is_none() {
                    self.exit = ExitState::WaitingForLocalWork { started };
                }
            } else if (backup_was_writing && current.backup.failed) || started.failed_for(current) {
                self.exit = ExitState::Idle;
            } else if app.has_unsaved_changes() {
                self.exit = ExitState::Confirm {
                    previous_focus: context.memory(|memory| memory.focused()),
                    focus_continue: true,
                };
            } else {
                self.finish_exit(context);
            }
        }
        if matches!(self.exit, ExitState::ClosingApi)
            && let Some(app) = self.app.as_deref()
        {
            let api = app.local_work_status().api;
            if !api.busy {
                if api.failed {
                    self.exit = ExitState::Idle;
                } else {
                    self.finish_exit(context);
                }
            }
        }
        let keep_open = matches!(
            self.exit,
            ExitState::WaitingForLocalWork { .. }
                | ExitState::Confirm { .. }
                | ExitState::ClosingApi
        ) || (matches!(self.exit, ExitState::Idle) && self.app.is_some());
        if close_requested && keep_open {
            context.send_viewport_cmd(ViewportCommand::CancelClose);
        }
    }

    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        if self.restore_work.is_some() {
            self.restore_progress(ui);
            return;
        }
        match self.exit {
            ExitState::WaitingForLocalWork { started } => {
                let ctx = ui.ctx();
                let caption = started.caption();
                let modal = egui::Modal::new(egui::Id::new("exit-after-local-work"))
                    .frame(dialog(ctx))
                    .backdrop_color(theme::MODAL_BACKDROP)
                    .show(ctx, |ui| {
                        title(ui, caption, theme::DIALOG_TITLE_SIZE);
                        muted(ui, "Приложение закроется после завершения операции.");
                        ui.add(egui::Spinner::new());
                        ui.add(primary("Остаться в приложении")).clicked()
                    });
                if modal.inner || modal.should_close() {
                    self.exit = ExitState::Idle;
                    ctx.request_repaint();
                }
                return;
            }
            ExitState::Confirm {
                previous_focus,
                focus_continue,
            } => {
                self.confirm_exit(ui.ctx(), previous_focus, focus_continue);
                return;
            }
            ExitState::ClosingApi => {
                let ctx = ui.ctx();
                egui::Modal::new(egui::Id::new("close-local-api"))
                    .frame(dialog(ctx))
                    .backdrop_color(theme::MODAL_BACKDROP)
                    .show(ctx, |ui| {
                        title(
                            ui,
                            "Закрываем локальное подключение",
                            theme::DIALOG_TITLE_SIZE,
                        );
                        ui.spinner();
                    });
                return;
            }
            ExitState::Exiting => return,
            ExitState::Idle | ExitState::WaitingForRestore => {}
        }
        if self.picker.is_some() {
            self.picker_ui(ui);
        } else if let Some(app) = &mut self.app {
            app.ui(ui, frame);
            if let Some(request) = app.take_restore_request() {
                self.begin_picker(ui.ctx(), request);
            }
        } else {
            self.recovery_ui(ui);
        }
    }
}

fn restore_failure_message(error: Box<dyn std::error::Error + Send + Sync>) -> String {
    match error.downcast_ref::<ProfileOpenIssue>() {
        Some(ProfileOpenIssue::InvalidCalendar) => {
            "Эту копию не удалось прочитать. Выберите другую копию.".into()
        }
        Some(ProfileOpenIssue::CalendarTooLarge) => {
            "Выбранная копия превышает допустимый размер.".into()
        }
        Some(ProfileOpenIssue::CalendarMissing) => "Файл выбранной копии отсутствует.".into(),
        _ if error.is::<ProfileSizeLimit>() => {
            "Копия вместе с текущими настройками превышает допустимый размер. Выберите меньшую копию.".into()
        }
        _ => error.to_string(),
    }
}

fn resize(ctx: &egui::Context, size: Vec2, minimum: Vec2) {
    ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(minimum));
    let size = ctx.input(|i| {
        i.viewport()
            .monitor_size
            .map_or(size, |monitor| size.min(monitor))
    });
    ctx.send_viewport_cmd(ViewportCommand::InnerSize(size));
}

fn recovery_panel(ui: &mut Ui, body: impl FnOnce(&mut Ui)) {
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(BACK).inner_margin(32))
        .show_inside(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("recovery-content")
                .show(ui, |ui| {
                    let width = ui.available_width().min(660.0);
                    ui.allocate_ui_with_layout(
                        vec2(width, 0.0),
                        Layout::top_down(Align::Min),
                        |ui| {
                            brand(ui);
                            ui.add_space(24.0);
                            card().show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                body(ui);
                            });
                        },
                    );
                });
        });
}

#[cfg(test)]
mod profile_tests {
    #[test]
    fn missing_replacement_profile_is_not_created() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("replacement");
        assert!(super::open_profile(&missing, true).is_err());
        assert!(!missing.join("calendar.json").exists());
        assert!(super::open_profile(&missing, false).is_ok());
    }
}

#[cfg(test)]
mod atlas_tests {
    use super::font_atlas_limit;

    #[test]
    fn respects_renderer_limit_and_rechecks_only_while_expansion_is_possible() {
        for native in [1024, 2048, 4096, 8192, 16384] {
            let mut larger = None;
            let unexpected = || panic!("the initial GPU limit needs no font lookup");
            assert_eq!(
                font_atlas_limit(Some(native), &mut larger, unexpected),
                Some(native.min(4096))
            );
            if native > 4096 {
                assert_eq!(font_atlas_limit(None, &mut larger, || false), None);
                assert_eq!(font_atlas_limit(None, &mut larger, || true), Some(native));
            }
            assert_eq!(font_atlas_limit(None, &mut larger, unexpected), None);
        }
    }

    #[test]
    fn new_renderer_replaces_the_previous_expansion_limit() {
        let mut larger = Some(16384);
        assert_eq!(
            font_atlas_limit(Some(2048), &mut larger, || true),
            Some(2048)
        );
        assert_eq!(font_atlas_limit(None, &mut larger, || true), None);
    }
}
