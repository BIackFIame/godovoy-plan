use std::{
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{Receiver, TryRecvError},
    },
};

use chrono::Local;
use eframe::egui::{self, Align, Button, Frame, Layout, Margin, Panel, ScrollArea, Ui, vec2};
use godovoy_plan::{
    Fallible,
    storage::{BackupEntry, RestoreResult, SharedStore, Store},
};

use super::{feedback, spawn_worker};
use crate::theme::{self, ACCENT, MUTED, NAV, RED, dialog, muted, primary, quiet, title};

pub(super) struct Backups {
    open: bool,
    folder: Option<PathBuf>,
    latest: Result<Option<BackupEntry>, String>,
    creation: BackupCreation,
    status: Option<(String, bool)>,
    restore_result: Option<RestoreResult>,
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

impl Backups {
    pub(super) fn new(folder: Option<PathBuf>) -> Self {
        Self {
            open: false,
            folder,
            latest: Ok(None),
            creation: BackupCreation::Idle,
            status: None,
            restore_result: None,
        }
    }

    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    pub(super) fn busy(&self) -> bool {
        matches!(self.creation, BackupCreation::Writing(_))
    }

    pub(super) fn failed(&self) -> bool {
        matches!(self.creation, BackupCreation::Finished { error: true, .. })
    }

    pub(super) fn reject_restore(&mut self, reason: &str) {
        self.status = Some((reason.into(), true));
    }

    pub(super) fn open_window(&mut self) {
        self.latest = self
            .folder
            .as_deref()
            .and_then(|folder| folder.parent())
            .ok_or_else(|| "Не удалось определить папку копий.".to_owned())
            .and_then(|profile| {
                Store::backups(profile)
                    .map(|copies| copies.into_iter().next())
                    .map_err(|error| format!("Не удалось прочитать список копий. {error}"))
            });
        self.status = None;
        self.open = true;
        if let BackupCreation::Finished { notice, .. } = &mut self.creation {
            *notice = false;
        }
    }

    fn create(&mut self, ctx: &egui::Context, store: &SharedStore) {
        if self.busy() {
            return;
        }
        let store = Arc::clone(store);
        let completed = spawn_worker("calendar-backup", ctx, move || {
            let result = Store::create_shared_backup(&store);
            drop(store);
            Some(result)
        });
        self.status = None;
        match completed {
            Ok(completed) => self.creation = BackupCreation::Writing(completed),
            Err(error) => self.finish(Err(error.into())),
        }
        ctx.request_repaint();
    }

    pub(super) fn poll(&mut self) {
        let BackupCreation::Writing(completed) = &self.creation else {
            return;
        };
        let result = match completed.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("Создание копии прервано.".into()),
        };
        self.finish(result);
    }

    fn finish(&mut self, result: Fallible<BackupEntry>) {
        let (message, error) = match result {
            Ok(copy) => {
                let latest = self.latest.as_mut().ok().and_then(Option::take);
                self.latest = Ok(latest.max(Some(copy)));
                ("Копия создана.".into(), false)
            }
            Err(error) => (format!("Не удалось создать копию. {error}"), true),
        };
        self.creation = BackupCreation::Finished {
            message,
            error,
            notice: !self.open,
        };
    }

    pub(super) fn show_notice(&mut self, ui: &mut Ui) {
        let BackupCreation::Finished { error, notice, .. } = &mut self.creation else {
            return;
        };
        if !*notice || self.open {
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
            self.open_window();
        }
    }

    pub(super) fn report_restore(&mut self, result: RestoreResult) {
        self.open = true;
        self.status = None;
        self.restore_result = Some(result);
    }

    pub(super) fn show(
        &mut self,
        ctx: &egui::Context,
        store: &SharedStore,
    ) -> Option<RestoreRequest> {
        if !self.open {
            return None;
        }
        let mut close = false;
        let mut request = None;
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
                    .show(ui, |ui| request = self.content(ui, ctx, store));
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
                                request = Some(RestoreRequest::ReturnTo(point.clone()));
                            }
                        } else {
                            close = ui.button("Закрыть").clicked();
                        }
                    },
                );
            });
        if close || dialog.should_close() {
            self.open = false;
            self.restore_result = None;
        }
        request
    }

    fn content(
        &mut self,
        ui: &mut Ui,
        ctx: &egui::Context,
        store: &SharedStore,
    ) -> Option<RestoreRequest> {
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
            if let Some((message, error)) = &self.status {
                feedback(ui, message, *error);
            }
            return None;
        }
        let mut request = None;
        muted(
            ui,
            "Копия содержит календарь и ключ синхронизации. Токены API исключены. Храните копию в закрытой папке.",
        );
        ui.add_space(5.0);
        match &self.latest {
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
            let enabled = !self.busy();
            if ui.add_enabled(enabled, primary("Создать копию")).clicked() {
                self.create(ctx, store);
            }
            if ui
                .add_enabled(enabled, Button::new("Восстановить из копии…"))
                .clicked()
            {
                request = Some(RestoreRequest::Choose);
            }
        });
        if self.busy() {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(16.0).color(ACCENT));
                muted(ui, "Сохраняем резервную копию…");
            });
        }
        if let BackupCreation::Finished { message, error, .. } = &self.creation {
            feedback(ui, message, *error);
        }
        if let Some((message, error)) = &self.status {
            feedback(ui, message, *error);
        }
        ui.separator();
        if let Some(folder) = &self.folder {
            muted(ui, "Папка копий");
            ui.add(egui::Label::new(folder.to_string_lossy()).truncate())
                .on_hover_text(folder.display().to_string());
            if ui.button("Скопировать путь").clicked() {
                ctx.copy_text(folder.to_string_lossy().into_owned());
                self.status = Some(("Путь скопирован.".into(), false));
            }
        }
        request
    }
}

#[cfg(test)]
mod tests {
    use super::super::App;
    use super::*;
    use std::{
        thread,
        time::{Duration, Instant},
    };

    #[test]
    fn restore_result_keeps_source_files_and_warning_separate_from_backup_status() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("profile")).unwrap();
        let mut app = App::new(store, Err("Без сети".into()), false);
        let original = directory
            .path()
            .join("calendar-before-restore-original.json");
        let in_memory = directory
            .path()
            .join("calendar-open-before-restore-original.json");
        app.report_restore(RestoreResult {
            original: Some(original.clone()),
            working_snapshot: Some(in_memory.clone()),
            restore_point: None,
            restore_point_warning: true,
        });
        assert!(app.backups.open);
        assert!(app.backups.status.is_none());
        let result = app.backups.restore_result.as_ref().unwrap();
        assert_eq!(result.original.as_ref(), Some(&original));
        assert_eq!(result.working_snapshot.as_ref(), Some(&in_memory));
        assert!(result.restore_point_warning);
        assert!(result.restore_point.is_none());
    }

    #[test]
    fn latest_backup_updates_after_creation_and_on_reopening_the_window() {
        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path().join("profile");
        let open = || {
            App::new(
                Store::open(&profile).unwrap(),
                Err("Без сети".into()),
                false,
            )
        };
        let mut app = open();
        app.store
            .lock()
            .unwrap()
            .transaction(|state| state.create_workspace("Команда".into(), "Анна".into()))
            .unwrap();
        app.backups.open_window();
        assert_eq!(app.backups.latest, Ok(None));
        app.backups.create(&egui::Context::default(), &app.store);
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.backups.busy() {
            assert!(Instant::now() < deadline, "Создание копии не завершилось");
            app.backups.poll();
            thread::sleep(Duration::from_millis(1));
        }
        let copy = app.backups.latest.as_ref().unwrap().clone().unwrap();
        assert!(matches!(
            &app.backups.creation,
            BackupCreation::Finished { message, error: false, notice: false }
                if message == "Копия создана."
        ));
        drop(app);

        let mut app = open();
        app.backups.open_window();
        assert_eq!(app.backups.latest, Ok(Some(copy)));
        assert_eq!(app.backups.status, None);
        let folder = Store::backup_folder(&profile);
        std::fs::rename(&folder, profile.join("backups-displaced")).unwrap();
        std::fs::write(&folder, "Вместо каталога").unwrap();
        app.backups.open_window();
        assert!(app.backups.latest.is_err());
        app.backups.open = false;
        app.backups.finish(Err("Нет доступа к папке.".into()));
        assert!(matches!(
            &app.backups.creation,
            BackupCreation::Finished {
                error: true,
                notice: true,
                ..
            }
        ));
        app.backups.open_window();
        assert!(matches!(
            &app.backups.creation,
            BackupCreation::Finished { message, error: true, notice: false }
                if message.contains("Нет доступа к папке.")
        ));
    }
}
