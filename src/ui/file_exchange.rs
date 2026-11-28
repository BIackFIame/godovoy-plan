use std::{
    path::PathBuf,
    sync::mpsc::{Receiver, TryRecvError},
};

use eframe::egui::{self, Button, RichText, TextEdit, Ui};
use godovoy_plan::{
    Fallible,
    exchange::{EXCHANGE_EXTENSION, export_file, import_file},
    storage::SharedStore,
};

use super::{feedback, spawn_worker};
use crate::{
    file_picker::{pick_exchange_to_open, pick_exchange_to_save, reveal_exchange_file},
    theme::{self, MUTED, muted, primary, quiet},
};

#[derive(Default)]
pub(super) struct FileExchange {
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

pub(super) enum ExchangeRequest {
    PickExport,
    PickImport,
    Export(PathBuf),
    Import(PathBuf),
    Reveal(PathBuf),
}

enum ExchangeResult {
    Exported(PathBuf),
    Imported { changed: bool },
    Revealed,
}

impl FileExchange {
    pub(super) fn busy(&self) -> bool {
        self.job.is_some()
    }

    pub(super) fn failed(&self) -> bool {
        self.status.as_ref().is_some_and(|(_, error)| *error)
    }

    pub(super) fn section(
        &mut self,
        ui: &mut Ui,
        network_unavailable: bool,
    ) -> Option<ExchangeRequest> {
        let mut request = None;
        egui::CollapsingHeader::new("Обмен через файл")
            .default_open(network_unavailable)
            .show(ui, |ui| {
                let working = self.busy();
                if !working {
                    let dropped = ui.input_mut(|input| std::mem::take(&mut input.raw.dropped_files));
                    match dropped.as_slice() {
                        [] => {}
                        [file] if file.path.is_some() => {
                            self.dropped.clone_from(&file.path);
                            self.status = None;
                        }
                        _ => {
                            self.status = Some(("Перетащите один файл обмена.".into(), true));
                        }
                    }
                }
                muted(ui, "Файл .yrplan можно передать уже подключённому коллеге на флешке, через AirDrop на Mac или через Bluetooth в Windows.");
                if let Some(path) = self.dropped.clone() {
                    let name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy();
                    muted(ui, format!("Выбран файл · {name}"));
                    ui.horizontal_wrapped(|ui| {
                        if ui.add_enabled(!working, primary("Принять выбранный файл")).clicked() {
                            request = Some(ExchangeRequest::Import(path));
                            self.dropped = None;
                        }
                        if ui.add_enabled(!working, quiet("Сбросить")).clicked() {
                            self.dropped = None;
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
                if self.dropped.is_none() {
                    muted(ui, "Полученный файл можно перетащить в это окно.");
                }
                egui::CollapsingHeader::new("Указать путь вручную")
                    .default_open(!native_picker)
                    .show(ui, |ui| {
                        let label = ui.label("Путь к файлу .yrplan");
                        ui.add_enabled(
                            !working,
                            TextEdit::singleline(&mut self.path)
                                .margin(theme::field_margin(ui))
                                .desired_width(f32::INFINITY),
                        )
                        .labelled_by(label.id);
                        let enabled = !working && !self.path.trim().is_empty();
                        ui.horizontal_wrapped(|ui| {
                            if ui.add_enabled(enabled, Button::new("Сохранить по пути")).clicked() {
                                request = Some(ExchangeRequest::Export(PathBuf::from(&self.path)));
                            }
                            if ui.add_enabled(enabled, Button::new("Открыть по пути")).clicked() {
                                request = Some(ExchangeRequest::Import(PathBuf::from(&self.path)));
                            }
                        });
                    });
                if let Some((message, error)) = &self.status {
                    feedback(ui, message, *error);
                }
                if let Some(path) = &self.exported {
                    let name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy();
                    let label = format!("Последний сохранённый файл · {name}");
                    ui.add(egui::Label::new(RichText::new(label).color(MUTED)).truncate())
                        .on_hover_text(path.display().to_string());
                    if native_picker && ui.add_enabled(!working, quiet("Показать в папке")).clicked() {
                        request = Some(ExchangeRequest::Reveal(path.clone()));
                    }
                }
            });
        request
    }

    pub(super) fn start(
        &mut self,
        ctx: &egui::Context,
        store: &SharedStore,
        request: ExchangeRequest,
    ) {
        if self.busy() {
            return;
        }
        let (operation, path) = match request {
            ExchangeRequest::PickExport => (ExchangeOperation::Export, pick_exchange_to_save()),
            ExchangeRequest::PickImport => (ExchangeOperation::Import, pick_exchange_to_open()),
            ExchangeRequest::Export(path) => (ExchangeOperation::Export, Some(path)),
            ExchangeRequest::Import(path) => (ExchangeOperation::Import, Some(path)),
            ExchangeRequest::Reveal(path) => (ExchangeOperation::Reveal, Some(path)),
        };
        let Some(mut path) = path else {
            return;
        };
        if !path.is_absolute() {
            self.status = Some(("Укажите полный путь к файлу.".into(), true));
            ctx.request_repaint();
            return;
        }
        if matches!(operation, ExchangeOperation::Export) && path.extension().is_none() {
            path.set_extension(EXCHANGE_EXTENSION);
        }
        let store = store.clone();
        match spawn_worker("file-exchange", ctx, move || {
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
            Some(result)
        }) {
            Ok(receiver) => {
                self.job = Some(receiver);
                let status = match operation {
                    ExchangeOperation::Export => "Сохраняем файл…",
                    ExchangeOperation::Import => "Принимаем изменения из файла…",
                    ExchangeOperation::Reveal => "Открываем папку файла…",
                };
                self.status = Some((status.into(), false));
            }
            Err(_) => {
                self.status = Some(("Не удалось начать обмен через файл.".into(), true));
            }
        }
        ctx.request_repaint();
    }

    /// A changed import invalidates the next reminder check in the app.
    #[must_use]
    pub(super) fn poll(&mut self) -> bool {
        let Some(result) = self.job.as_ref().map(Receiver::try_recv) else {
            return false;
        };
        let changed = matches!(&result, Ok(Ok(ExchangeResult::Imported { changed: true })));
        let status = match result {
            Ok(Ok(ExchangeResult::Exported(path))) => {
                self.exported = Some(path);
                ("Файл сохранён. Передайте его коллеге.".into(), false)
            }
            Ok(Ok(ExchangeResult::Imported { changed: true })) => {
                ("Изменения из файла добавлены в календарь.".into(), false)
            }
            Ok(Ok(ExchangeResult::Imported { changed: false })) => {
                ("Календарь уже содержит эти изменения.".into(), false)
            }
            Ok(Ok(ExchangeResult::Revealed)) => ("Папка файла открыта.".into(), false),
            Ok(Err(error)) => (error.to_string(), true),
            Err(TryRecvError::Disconnected) => ("Обмен через файл прерван.".into(), true),
            Err(TryRecvError::Empty) => return false,
        };
        self.job = None;
        self.status = Some(status);
        changed
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::sync_channel;

    use super::*;

    #[test]
    fn completed_import_invalidates_reminders_once_and_only_when_it_changed_data() {
        let mut exchange = FileExchange::default();
        for changed in [true, false] {
            let (send, completed) = sync_channel(1);
            exchange.job = Some(completed);
            assert!(!exchange.poll());
            assert!(exchange.busy());
            send.send(Ok(ExchangeResult::Imported { changed })).unwrap();
            assert_eq!(exchange.poll(), changed);
            assert!(!exchange.busy());
            assert!(!exchange.failed());
            assert!(!exchange.poll());
        }
        let (send, completed) = sync_channel(1);
        exchange.job = Some(completed);
        drop(send);
        assert!(!exchange.poll());
        assert!(!exchange.busy());
        assert!(exchange.failed());
    }
}
