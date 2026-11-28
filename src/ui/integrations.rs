use std::time::Duration;

use chrono::Local;
use eframe::egui::{self, DragValue, RichText, ScrollArea, Ui};
use godovoy_plan::{
    api::{self, BridgeOutcome},
    storage::{
        ApiSettings, BridgeRunStatus, ImportResultKind, ImportStatus, LocalState, SharedStore,
        StoreReader,
    },
};

use super::{
    App,
    api_controls::{Action as ApiAction, ApiControls},
    feedback, settings_window,
};
use crate::theme::{GREEN, INK, RED, SECTION_TITLE_SIZE, muted, primary, semibold, title};

pub(super) struct Integrations {
    open: bool,
    confirm_token_change: bool,
    controls: ApiControls,
}

impl Integrations {
    pub(super) fn work_status(&self) -> super::WorkStatus {
        super::WorkStatus {
            busy: self.controls.busy(),
            failed: self.controls.failed(),
        }
    }

    pub(super) fn new(store: &SharedStore, settings: Option<&ApiSettings>) -> Self {
        Self {
            open: false,
            confirm_token_change: false,
            controls: ApiControls::new(store, settings),
        }
    }

    pub(super) fn open(&mut self) {
        self.open = true;
    }

    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    pub(super) fn needs_refresh(&self) -> bool {
        self.open && self.controls.running_port().is_some()
    }

    pub(super) fn show(
        &mut self,
        ctx: &egui::Context,
        state: &LocalState,
        store: &SharedStore,
        reader: &StoreReader,
    ) {
        if !self.open {
            return;
        }
        if self.controls.running_port().is_some()
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
            if let Some(progress) = self.controls.progress() {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new());
                    muted(ui, progress);
                });
            } else if let Some(port) = self.controls.running_port() {
                ui.colored_label(GREEN, "API включён");
                ui.label(RichText::new(format!("Защищённый порт {port}")).strong());
                ui.horizontal_wrapped(|ui| {
                    if let Some(settings) = state.api.as_ref() {
                        if ui.button("Копировать основной токен").clicked() {
                            ctx.copy_text(api::encode_token(&settings.token));
                            self.controls.message =
                                Some(("Основной токен скопирован.".into(), false));
                        }
                        if let Some(key) = settings.import_token
                            && ui.button("Копировать токен импорта").clicked()
                        {
                            ctx.copy_text(api::encode_token(&key));
                            self.controls.message =
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
                        ui.add(DragValue::new(&mut self.controls.port).range(1024..=65535))
                            .labelled_by(port.id);
                    });
                });
            }
            if let Some((message, error)) = &self.controls.message {
                feedback(ui, message, *error);
            }
            Self::import_status_section(
                ui,
                &reader.import_status(),
                reader.import_status_warning(),
            );
        });
        self.open = open;
        if !open {
            self.confirm_token_change = false;
            if self
                .controls
                .message
                .as_ref()
                .is_some_and(|(_, error)| !error)
            {
                self.controls.message = None;
            }
        }
        if let Some(action) = action {
            self.controls.start(store, action, Some(ctx));
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
}

impl App {
    pub fn poll_api_controls(&mut self) {
        self.integrations.controls.poll();
    }

    // True means exit must wait for this job or handle its start failure.
    // The saved state stays enabled so the next launch restores the listener.
    pub fn close_api_listener(&mut self, context: &egui::Context) -> bool {
        if self.integrations.controls.busy() {
            return true;
        }
        if self.integrations.controls.running_port().is_none() {
            return false;
        }
        self.integrations
            .controls
            .start(&self.store, ApiAction::CloseListener, Some(context));
        true
    }
}
