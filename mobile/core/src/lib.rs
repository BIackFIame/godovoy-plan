mod alerts;
mod calendar;
mod editor;
mod ffi;
mod staging;

use chrono::NaiveDate;
use godovoy_plan::{
    Fallible, exchange,
    model::{Category, Id},
    network::{Config, Network},
    storage::{SharedStore, Store},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    io,
    net::SocketAddrV4,
    path::{Path, PathBuf},
};

#[derive(Serialize)]
struct Success<T> {
    ok: bool,
    data: T,
}

fn success(data: impl Serialize) -> Fallible<String> {
    Ok(serde_json::to_string(&Success { ok: true, data })?)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TaskInput {
    title: String,
    days_before: u16,
}

#[derive(Deserialize)]
#[serde(
    tag = "op",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum Command {
    State {
        year: i32,
        if_token: Option<Id>,
        #[serde(default)]
        accept_sync_stamp: bool,
        #[serde(default)]
        accept_check_patch: bool,
        #[serde(default)]
        accept_index_check_patch: bool,
        #[serde(default)]
        accept_checklist_tuples: bool,
        #[serde(default)]
        accept_agenda_index: bool,
    },
    AgendaCard {
        base_token: Id,
        year: i32,
        event_year: i32,
        event_id: Id,
    },
    ReminderPage {
        base_token: Id,
        year: i32,
        offset: usize,
        limit: usize,
    },
    CreateWorkspace {
        name: String,
        employee: String,
    },
    CreateEvent {
        title: String,
        date: NaiveDate,
        yearly: bool,
        preparation_days: u16,
        owner_id: Option<Id>,
        category: Category,
        checklist: Vec<TaskInput>,
        #[serde(default)]
        description: String,
    },
    EditEvent(editor::Edit),
    DeleteEvent(editor::Delete),
    Check {
        view_year: Option<i32>,
        base_token: Option<Id>,
        event_id: Id,
        year: i32,
        task_id: Id,
        done: bool,
    },
    Note {
        view_year: Option<i32>,
        base_token: Option<Id>,
        event_id: Id,
        year: i32,
        text: String,
        base: Option<String>,
    },
    PairingRequest,
    Invite {
        request: String,
    },
    Join {
        code: String,
        employee: String,
        address: Option<SocketAddrV4>,
    },
    Rejoin {
        code: String,
        address: Option<SocketAddrV4>,
    },
    LinkEmployeeDevice {
        device_id: Id,
        employee_id: Id,
    },
    Revoke {
        device_ids: BTreeSet<Id>,
    },
    Renew,
    Peer {
        address: SocketAddrV4,
    },
    Pause {
        paused: bool,
    },
    Background,
    Foreground,
    Export,
    Import {
        path: PathBuf,
    },
}

fn short_name(s: String) -> Fallible<String> {
    let s = s.trim();
    if s.is_empty() || s.chars().count() > 80 {
        return Err("Введите имя длиной от 1 до 80 символов.".into());
    }
    Ok(s.to_owned())
}

fn year_check(year: i32) -> Fallible<()> {
    if !(1900..=9999).contains(&year) {
        return Err("Год должен быть в диапазоне 1900–9999.".into());
    }
    Ok(())
}

struct Session {
    store: SharedStore,
    network: Option<Network>,
    network_error: Option<String>,
    last_state: Option<calendar::LastState>,
}

impl Session {
    fn load(initial: &Path, background: bool) -> Fallible<Self> {
        let active = Store::active_directory(initial)?;
        let store = if active == initial {
            Store::open(initial)?
        } else {
            Store::open_existing(&active)?
        };
        let mut session = Self {
            store,
            network: None,
            network_error: None,
            last_state: None,
        };
        if session.lan_enabled() {
            session.connect(background);
        }
        Ok(session)
    }

    fn lan_enabled(&self) -> bool {
        self.store.lock().unwrap().state().lan_enabled()
    }

    fn connect(&mut self, background: bool) {
        #[cfg(not(test))]
        let mut config = Config::default();
        #[cfg(test)]
        let mut config = Config {
            listen: SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, 0),
            discovery_port: None,
        };
        #[cfg(target_os = "ios")]
        {
            config.discovery_port = None;
        }
        let result =
            Network::start_with_pause(self.store.clone(), config, background).or_else(|e| {
                if e.downcast_ref::<io::Error>()
                    .is_some_and(|e| e.kind() == io::ErrorKind::AddrInUse)
                {
                    config.listen.set_port(0);
                    Network::start_with_pause(self.store.clone(), config, background)
                } else {
                    Err(e)
                }
            });
        match result {
            Ok(net) => {
                self.network = Some(net);
                self.network_error = None;
            }
            Err(e) => self.network_error = Some(format!("Локальная сеть недоступна. {e}")),
        }
    }

    fn net(&self) -> Fallible<&Network> {
        self.network.as_ref().ok_or_else(|| {
            self.network_error
                .as_deref()
                .unwrap_or("Локальная сеть недоступна.")
                .into()
        })
    }

    fn verify_access(&self) -> Fallible<()> {
        let store = self
            .store
            .lock()
            .map_err(|_| "Профиль требуется открыть заново.")?;
        let state = store.state();
        if let Some(w) = &state.workspace
            && (w.is_revoked(state.device) || !w.calendar.members.contains_key(&state.device))
        {
            return Err("Доступ этого устройства к календарю закрыт.".into());
        }
        Ok(())
    }
}

struct Mobile {
    initial: PathBuf,
    session: Option<Session>,
    background_pause: Option<bool>,
}

impl Mobile {
    fn open(initial: PathBuf) -> Fallible<Self> {
        if !initial.is_absolute() || initial.file_name().is_none() {
            return Err("Требуется приватная папка приложения.".into());
        }
        let session = Session::load(&initial, false)?;
        staging::prepare(&initial)?;
        Ok(Self {
            initial,
            session: Some(session),
            background_pause: None,
        })
    }

    fn refresh(&mut self) -> Fallible<&mut Session> {
        if let Some(session) = &self.session
            && session
                .store
                .lock()
                .map_err(|_| "Профиль требуется открыть заново.")?
                .successor_directory()
                .is_some()
        {
            self.session = None;
        }
        if self.session.is_none() {
            self.session = Some(Session::load(
                &self.initial,
                self.background_pause.is_some(),
            )?);
        }
        self.session
            .as_mut()
            .ok_or_else(|| "Профиль недоступен.".into())
    }

    #[cfg(test)]
    fn request(&mut self, input: serde_json::Value) -> Fallible<String> {
        self.execute(serde_json::from_value(input).map_err(|_| "Неверные параметры запроса.")?)
    }

    fn execute(&mut self, cmd: Command) -> Fallible<String> {
        let session = self.refresh()?;
        if !matches!(
            &cmd,
            Command::State { .. } | Command::AgendaCard { .. } | Command::ReminderPage { .. }
        ) {
            session.verify_access()?;
        }
        let session = self.session.as_mut().ok_or("Профиль недоступен.")?;
        if matches!(
            &cmd,
            Command::PairingRequest
                | Command::Join { .. }
                | Command::Rejoin { .. }
                | Command::Invite { .. }
                | Command::Peer { .. }
                | Command::Pause { paused: false }
        ) && session.network.is_none()
        {
            if self.background_pause.is_some() && !matches!(cmd, Command::Pause { .. }) {
                return Err("Для подключения откройте календарь на экране.".into());
            }
            session.store.lock().unwrap().enable_lan()?;
            if self.background_pause.is_none() {
                session.connect(false);
                session.net()?;
            }
        }
        match cmd {
            Command::State {
                year,
                if_token,
                accept_sync_stamp,
                accept_check_patch,
                accept_index_check_patch,
                accept_checklist_tuples,
                accept_agenda_index,
            } => {
                return calendar::state(
                    session,
                    year,
                    if_token,
                    accept_sync_stamp,
                    calendar::CheckPatch {
                        full: accept_check_patch,
                        indexed: accept_index_check_patch,
                    },
                    accept_checklist_tuples,
                    accept_agenda_index,
                );
            }
            Command::AgendaCard {
                base_token,
                year,
                event_year,
                event_id,
            } => {
                return calendar::agenda_card(session, base_token, year, event_year, event_id);
            }
            Command::ReminderPage {
                base_token,
                year,
                offset,
                limit,
            } => {
                return calendar::reminder_page(session, base_token, year, offset, limit);
            }
            Command::EditEvent(edit) => {
                editor::edit_event(&session.store, session.last_state.as_ref(), edit)?
            }
            Command::DeleteEvent(delete) => {
                editor::delete_event(&session.store, session.last_state.as_ref(), delete)?
            }
            Command::CreateWorkspace { name, employee } => {
                session.store.lock().unwrap().transaction(|s| {
                    s.create_workspace(short_name(name)?, short_name(employee)?)
                })?;
            }
            Command::CreateEvent {
                title,
                date,
                yearly,
                preparation_days,
                owner_id,
                category,
                checklist,
                description,
            } => {
                use godovoy_plan::model::{Event, Task};
                let tasks = checklist
                    .into_iter()
                    .map(|t| Task {
                        id: Id::new_v4(),
                        title: t.title.trim().to_owned(),
                        days_before: t.days_before,
                    })
                    .collect();
                calendar::create(
                    &session.store,
                    Event {
                        title: title.trim().to_owned(),
                        date,
                        yearly,
                        preparation_days,
                        owner: owner_id,
                        category,
                        tasks,
                        description,
                    },
                )?;
            }
            Command::Check {
                view_year,
                base_token,
                event_id,
                year,
                task_id,
                done,
            } => {
                if let Some(token) = base_token {
                    calendar::check_from_view(
                        session,
                        token,
                        view_year.unwrap_or(year),
                        event_id,
                        year,
                        task_id,
                        done,
                    )?;
                } else if view_year.is_some() {
                    return Err(
                        "Для отметки в открытом календаре требуется актуальный снимок.".into(),
                    );
                } else {
                    calendar::check(&session.store, event_id, year, task_id, done)?;
                }
            }
            Command::Note {
                view_year,
                base_token,
                event_id,
                year,
                text,
                base,
            } => calendar::note(
                session,
                view_year,
                base_token,
                event_id,
                year,
                &text,
                base.as_deref(),
            )?,
            Command::PairingRequest => return success(session.net()?.pairing_request()),
            Command::Invite { request } => {
                return success(session.net()?.make_invite(request.trim())?);
            }
            Command::Join {
                code,
                employee,
                address,
            } => session
                .net()?
                .join_async(code.trim().to_owned(), short_name(employee)?, address)
                .recv()??,
            Command::Rejoin { code, address } => {
                session
                    .net()?
                    .rejoin_async(code.trim().to_owned(), address)
                    .recv()??;
                self.refresh()?;
            }
            Command::LinkEmployeeDevice {
                device_id,
                employee_id,
            } => {
                session
                    .store
                    .lock()
                    .map_err(|_| "Профиль недоступен.")?
                    .link_employee_device(device_id, employee_id)?;
            }
            Command::Revoke { device_ids } => {
                session.net()?.revoke_devices_async(device_ids).recv()??;
                self.refresh()?;
            }
            Command::Renew => {
                session.net()?.renew_access_async().recv()??;
                self.refresh()?;
            }
            Command::Peer { address } => session.net()?.remember_peer(address)?,
            Command::Pause { paused } => {
                if let Some(preference) = self.background_pause.as_mut() {
                    // A preference change cannot restart the transport while in background.
                    session.store.lock().unwrap().set_sync_paused(paused)?;
                    *preference = paused;
                } else if let Some(network) = session.network.as_ref() {
                    network.set_profile_paused(paused)?;
                } else {
                    session.store.lock().unwrap().set_sync_paused(paused)?;
                }
            }
            Command::Background => {
                if self.background_pause.is_none() {
                    self.background_pause = Some(session.network.as_ref().map_or_else(
                        || session.store.lock().unwrap().sync_paused(),
                        Network::paused,
                    ));
                    if let Some(net) = &session.network {
                        net.set_paused(true);
                    }
                }
            }
            Command::Foreground => {
                if session.network.is_none()
                    && (session.lan_enabled() || session.network_error.is_some())
                {
                    session.connect(false);
                }
                if let Some(previous) = self.background_pause.take()
                    && let Some(net) = &session.network
                {
                    net.set_paused(previous || session.store.lock().unwrap().sync_paused());
                }
            }
            Command::Export => {
                let output =
                    staging::directory(&self.initial)?.join(format!("{}.yrplan", Id::new_v4()));
                exchange::export_file(&session.store, &output)?;
                return success(output);
            }
            Command::Import { path } => {
                let path = staging::input_path(&self.initial, &path)?;
                exchange::import_file(&session.store, &path)?;
            }
        }
        success(())
    }
}

#[cfg(test)]
mod tests;
