use std::{
    collections::BTreeSet,
    net::SocketAddrV4,
    sync::mpsc::{Receiver, TryRecvError},
    time::Instant,
};

use chrono::{Local, TimeZone};
use eframe::egui::{self, Align, Button, Layout, RichText, ScrollArea, TextEdit, Ui};
use godovoy_plan::{
    Fallible,
    model::Id,
    network::{Network, SYNC_PORT},
    storage::{LocalState, ProfileUsage, Workspace},
};

use super::{
    App, DesktopTransport, feedback, owner_name, people::DeviceAssignments, settings_window,
};
use crate::theme::{self, RED, SECTION_TITLE_SIZE, muted, primary, title};

#[derive(Default)]
pub(super) struct SyncSettings {
    pub(super) open: bool,
    address: String,
    pub(super) join_code: String,
    pairing_request: String,
    message: Option<(String, bool)>,
    confirming: bool,
    revocations: BTreeSet<Id>,
    devices: DeviceAssignments,
    job: Option<ConnectionJob>,
}

enum ConnectionJob {
    Join(Receiver<Fallible<()>>),
    UpdateAccess(Receiver<Fallible<()>>),
}

enum AccessAction {
    Renew,
    Rejoin,
}

impl SyncSettings {
    pub(super) fn busy(&self) -> bool {
        self.job.is_some()
    }

    pub(super) fn changing_access(&self) -> bool {
        matches!(self.job, Some(ConnectionJob::UpdateAccess(_)))
    }

    pub(super) fn address_field(&mut self, ui: &mut Ui, width: f32) -> egui::Response {
        ui.add(
            TextEdit::singleline(&mut self.address)
                .hint_text("192.168.1.15:45736")
                .margin(theme::field_margin(ui))
                .desired_width(width)
                .char_limit(40),
        )
    }

    fn peer_address(&self) -> Fallible<Option<SocketAddrV4>> {
        if self.address.trim().is_empty() {
            Ok(None)
        } else {
            parse_address(&self.address).map(Some)
        }
    }

    fn network_section(
        &mut self,
        ui: &mut Ui,
        network: &Network,
        state: &LocalState,
        workspace: &Workspace,
    ) -> Option<Fallible<&'static str>> {
        let mut paused = network.paused();
        let status = network.status();
        let invite_error = self.invitation(ui, network, workspace, state.device, paused);
        for warning in [status.merge_error, status.warning, status.route_warning]
            .into_iter()
            .flatten()
        {
            ui.colored_label(RED, warning);
        }
        if let Some((message, error)) = &self.message {
            feedback(ui, message, *error);
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
                self.message = network
                    .set_profile_paused(paused)
                    .err()
                    .map(|error| (format!("Не удалось сохранить режим обмена. {error}"), true));
            }
        });
        self.addresses(ui, network, state, status.last_sync.or(state.last_sync))
            .or(invite_error)
    }

    fn invitation(
        &mut self,
        ui: &mut Ui,
        network: &Network,
        workspace: &Workspace,
        device: Id,
        paused: bool,
    ) -> Option<Fallible<&'static str>> {
        let mut error_result = None;
        if let Some(id) = workspace.access_manager.filter(|id| *id != device) {
            let name = workspace
                .calendar
                .members
                .get(&id)
                .map_or("Управляющий компьютер", |member| {
                    member.value.as_str()
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
                            ui.ctx().copy_text(code);
                            self.pairing_request.clear();
                            self.message = Some((
                                "Приглашение скопировано. Передайте его коллеге.".into(),
                                false,
                            ));
                        }
                        Err(error) => error_result = Some(Err(error)),
                    }
                }
            });
            muted(
                ui,
                "Код действует 15 минут и подходит только устройству, от которого вы получили запрос.",
            );
        }
        error_result
    }

    fn addresses(
        &mut self,
        ui: &mut Ui,
        network: &Network,
        state: &LocalState,
        last_sync: Option<i64>,
    ) -> Option<Fallible<&'static str>> {
        let mut address_result = None;
        ui.collapsing("Подключение по адресу и состояние сети", |ui| {
            let time = last_sync
                .and_then(|ts| Local.timestamp_opt(ts, 0).single());
            muted(ui, time.map_or_else(|| "Обмена ещё не было".into(), |time|
                format!("Последний обмен · {}", time.format("%d.%m.%Y %H:%M"))
            ));
            if let Some(address) = network.address() {
                muted(ui, format!("Адрес этого устройства · {address}"));
            }
            ui.horizontal(|ui| {
                let address = ui.label("Адрес коллеги");
                self.address_field(ui, 180.0).labelled_by(address.id);
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
        address_result
    }

    fn revocation_controls(
        &mut self,
        ui: &mut Ui,
        workspace: &Workspace,
        device: Id,
        busy: bool,
    ) -> bool {
        let mut renew = false;
        if self.confirming {
            if self.revocations.is_empty() {
                ui.label("Перевыпустить ключ для всего календаря?");
            } else {
                ui.label("Отозвать доступ выбранных устройств?");
                for id in &self.revocations {
                    if let Some(member) = workspace.calendar.members.get(id) {
                        ui.label(format!("{} · {id}", member.value));
                    }
                }
            }
            if workspace.access_manager.is_none() {
                muted(
                    ui,
                    "Этот компьютер станет управляющим. В дальнейшем приглашения и отзыв доступа будут доступны только на нём.",
                );
            }
            muted(
                ui,
                "Остальным компьютерам понадобятся новые приглашения. История и назначения сохранятся. Прежние копии останутся у их владельцев.",
            );
            ui.horizontal_wrapped(|ui| {
                let action = if self.revocations.is_empty() {
                    "Перевыпустить ключ"
                } else {
                    "Отозвать доступ"
                };
                if ui
                    .add_enabled(!busy, Button::new(RichText::new(action).color(RED)))
                    .clicked()
                {
                    renew = true;
                }
                if ui.button("Отмена").clicked() {
                    self.confirming = false;
                }
            });
        } else {
            muted(
                ui,
                "При потере компьютера отметьте его в списке. Сверьте ID, если имена совпадают.",
            );
            ScrollArea::vertical()
                .id_salt("device-access-list")
                .max_height(164.0)
                .show(ui, |ui| {
                    for (&id, member) in &workspace.calendar.members {
                        if id == device {
                            continue;
                        }
                        if workspace.is_revoked(id) {
                            muted(ui, format!("{} · {id} · доступ отозван", member.value));
                        } else {
                            let mut chosen = self.revocations.contains(&id);
                            if ui
                                .checkbox(&mut chosen, format!("{} · {id}", member.value))
                                .changed()
                            {
                                if chosen {
                                    self.revocations.insert(id);
                                } else {
                                    self.revocations.remove(&id);
                                }
                            }
                        }
                    }
                });
            let action = if self.revocations.is_empty() {
                "Перевыпустить доступ…"
            } else {
                "Отозвать доступ устройств…"
            };
            if ui.add_enabled(!busy, Button::new(action)).clicked() {
                self.confirming = true;
            }
        }
        renew
    }

    fn access(
        &mut self,
        ui: &mut Ui,
        network: &Network,
        workspace: &Workspace,
        device: Id,
        can_manage: bool,
        busy: bool,
    ) -> Option<AccessAction> {
        let mut action = None;
        ui.collapsing("Доступ к календарю", |ui| {
            muted(ui, format!("ID этого устройства · {device}"));
            if can_manage && self.revocation_controls(ui, workspace, device, busy) {
                action = Some(AccessAction::Renew);
            }
            ui.separator();
            ui.label("Коллега уже перевыпустил ключ");
            muted(ui, "Отправьте коллеге запрос этого компьютера. Вставьте полученный код до закрытия приложения. Ваши сохранённые правки будут объединены с календарём команды.");
            if ui.button("Скопировать запрос устройства").clicked() {
                ui.ctx().copy_text(network.pairing_request());
                self.message = Some(("Запрос скопирован.".into(), false));
            }
            let code_label = ui.label("Новый код приглашения");
            ui.add(TextEdit::singleline(&mut self.join_code).password(true).char_limit(300))
                .labelled_by(code_label.id);
            if ui.add_enabled(
                !busy && !self.join_code.trim().is_empty(),
                Button::new("Подключиться по новому коду"),
            ).clicked() {
                action.get_or_insert(AccessAction::Rejoin);
            }
        });
        action
    }
}

fn connected_devices(ui: &mut Ui, state: &LocalState, workspace: &Workspace) {
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
}

impl App {
    pub(super) fn begin_join(&mut self) {
        let result = (|| -> Fallible<_> {
            if self.sync.join_code.trim().is_empty() || self.employee.trim().is_empty() {
                return Err("Введите ваше имя и код приглашения.".into());
            }
            let address = self.sync.peer_address()?;
            let network = self.network.enable(&self.store)?;
            Ok(network.join_async(self.sync.join_code.clone(), self.employee.clone(), address))
        })();
        match result {
            Ok(receiver) => {
                self.message = None;
                self.sync.job = Some(ConnectionJob::Join(receiver));
            }
            Err(error) => self.fail(error.to_string()),
        }
    }

    pub fn show_renewed_access(&mut self) {
        self.sync.open = true;
        self.sync.message = Some(("Открыт календарь с новым ключом.".into(), false));
    }

    pub(super) fn sync_window(
        &mut self,
        ctx: &egui::Context,
        state: &LocalState,
        workspace: &Workspace,
        usage: ProfileUsage,
    ) {
        if !self.sync.open {
            return;
        }
        let mut open = true;
        let mut address_result = None;
        let mut file_request = None;
        let busy = self.sync.busy() || self.profile_work_in_progress().is_some();
        let mut access = None;
        let can_manage = workspace.access_manager.is_none_or(|id| id == state.device);
        let mut link = None;

        settings_window(ctx, "Синхронизация", &mut open, |ui| {
            ui.set_min_width(440.0);
            self.hosted_entry.show(ui);
            ui.separator();
            title(ui, "Пригласить коллегу", SECTION_TITLE_SIZE);
            match &self.network {
                DesktopTransport::Started(Ok(network)) => {
                    address_result = self.sync.network_section(ui, network, state, workspace);
                }
                DesktopTransport::Started(Err(error)) => {
                    ui.colored_label(RED, format!("Обмен по сети недоступен. {error}"));
                }
                DesktopTransport::Waiting(_) => {
                    if ui
                        .add(primary("Включить обмен по локальной сети"))
                        .clicked()
                        && let Err(error) = self.network.enable(&self.store)
                    {
                        self.sync.message = Some((error.to_string(), true));
                    }
                }
            }
            ui.separator();
            file_request = self
                .file_exchange
                .section(ui, self.network.as_ref().is_err());
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
                access = self
                    .sync
                    .access(ui, network, workspace, state.device, can_manage, busy);
                ui.separator();
            }
            if can_manage {
                ui.collapsing("Устройства сотрудников", |ui| {
                    link = self.sync.devices.show(ui, workspace, state.device, busy);
                });
                ui.separator();
            }
            connected_devices(ui, state, workspace);
        });
        if let Some((device, employee)) = link {
            let result = self
                .store
                .lock()
                .unwrap()
                .link_employee_device(device, employee);
            self.sync.devices.finish(result);
            self.next_reminder_check = Instant::now();
            ctx.request_repaint();
        }
        if let Some(action) = access {
            self.begin_access_change(action);
        }
        if !open {
            self.sync.open = false;
            self.sync.confirming = false;
            self.sync.revocations.clear();
            self.sync.devices = DeviceAssignments::default();
            self.sync.message = None;
        }
        if let Some(result) = address_result {
            self.sync.message = Some(match result {
                Ok(message) => (message.into(), false),
                Err(error) => (error.to_string(), true),
            });
            ctx.request_repaint();
        }
        if let Some(request) = file_request {
            self.file_exchange.start(ctx, &self.store, request);
        }
    }

    fn begin_access_change(&mut self, action: AccessAction) {
        let result = (|| -> Fallible<_> {
            self.flush_local_drafts()?;
            let network = self.network.as_ref()?;
            Ok(match action {
                AccessAction::Renew if self.sync.revocations.is_empty() => {
                    network.renew_access_async()
                }
                AccessAction::Renew => network.revoke_devices_async(self.sync.revocations.clone()),
                AccessAction::Rejoin => network.rejoin_async(
                    self.sync.join_code.trim().to_owned(),
                    self.sync.peer_address()?,
                ),
            })
        })();
        match result {
            Ok(job) => self.sync.job = Some(ConnectionJob::UpdateAccess(job)),
            Err(error) => self.sync.message = Some((error.to_string(), true)),
        }
        self.sync.confirming = false;
    }

    pub(super) fn poll_connection(&mut self) {
        let Some(job) = &self.sync.job else {
            return;
        };
        let (ConnectionJob::Join(receiver) | ConnectionJob::UpdateAccess(receiver)) = job;
        match receiver.try_recv() {
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => self.fail("Подключение было прервано."),
            Ok(result) => {
                match job {
                    ConnectionJob::Join(_) => self.report(result, None),
                    ConnectionJob::UpdateAccess(_) => {
                        self.renewed_profile = self.store.lock().unwrap().successor_directory();
                        self.sync.message = result.err().map(|error| {
                            let message = if self.network.as_ref().is_ok_and(|n| n.paused()) {
                                format!("{error} Прежняя синхронизация на паузе. Проверьте данные и повторите действие.")
                            } else { error.to_string() };
                            (message,true)
                        });
                    }
                }
                self.next_reminder_check = Instant::now();
            }
        }
        self.sync.job = None;
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

#[cfg(test)]
mod tests {
    use super::*;
    use godovoy_plan::storage::Store;
    use std::{sync::mpsc, time::Duration};

    #[test]
    fn optional_peer_address_distinguishes_discovery_from_invalid_input() {
        let mut settings = SyncSettings::default();
        settings.address.push_str("  ");
        assert_eq!(settings.peer_address().unwrap(), None);
        settings.address = " 192.168.1.15 ".into();
        let address: SocketAddrV4 = format!("192.168.1.15:{SYNC_PORT}").parse().unwrap();
        assert_eq!(settings.peer_address().unwrap(), Some(address));
        settings.address = "192.168.1.15:45800".into();
        assert_eq!(settings.peer_address().unwrap().unwrap().port(), 45800);
        settings.address = "192.168.1.15:abc".into();
        assert!(settings.peer_address().is_err());
    }

    #[test]
    fn completion_uses_the_message_and_busy_state_of_the_started_operation() {
        let profile = tempfile::tempdir().unwrap();
        let mut app = App::new(
            Store::open(&profile.path().join("profile")).unwrap(),
            Err("Без сети".into()),
            false,
        );
        for updating in [true, false] {
            let (send, receive) = mpsc::channel();
            app.sync.job = Some(if updating {
                ConnectionJob::UpdateAccess(receive)
            } else {
                ConnectionJob::Join(receive)
            });
            app.message = None;
            app.sync.message = None;
            let next_check = Instant::now() + Duration::from_secs(60);
            app.next_reminder_check = next_check;
            app.poll_connection();
            assert!(app.sync.busy());
            assert_eq!(app.sync.changing_access(), updating);
            assert_eq!(app.next_reminder_check, next_check);
            send.send(Err("Ошибка соединения".into())).unwrap();
            app.poll_connection();
            assert!(!app.sync.busy());
            assert!(!app.sync.changing_access());
            let expected = Some(("Ошибка соединения".to_owned(), true));
            if updating {
                assert_eq!(app.sync.message, expected);
                assert!(app.message.is_none());
            } else {
                assert_eq!(app.message, expected);
                assert!(app.sync.message.is_none());
            }
            assert!(app.next_reminder_check < next_check);
        }
    }
}
