use std::{
    sync::mpsc::{Receiver, TryRecvError, TrySendError, sync_channel},
    thread,
};

use eframe::egui;
use godovoy_plan::{
    Fallible,
    api::{ApiServer, DEFAULT_PORT},
    storage::{ApiSettings, SharedStore, random_bytes},
};

#[derive(Clone, Copy)]
pub(super) enum Action {
    StartSaved,
    Enable,
    Rotate,
    Disable,
    CloseListener,
}

impl Action {
    fn progress(self) -> &'static str {
        match self {
            Self::StartSaved | Self::Enable => "Запускаем локальный API…",
            Self::Rotate => "Заменяем токены…",
            Self::Disable => "Выключаем локальный API…",
            Self::CloseListener => "Закрываем локальное подключение…",
        }
    }
}

struct Request {
    action: Action,
    port: u16,
    server: Option<ApiServer>,
}

struct Completed {
    server: Option<ApiServer>,
    result: Fallible<()>,
}

struct Job {
    action: Action,
    completed: Receiver<Completed>,
}

pub(super) struct ApiControls {
    server: Option<ApiServer>,
    job: Option<Job>,
    failed: bool,
    pub port: u16,
    pub message: Option<(String, bool)>,
}

impl ApiControls {
    pub(super) fn busy(&self) -> bool {
        self.job.is_some()
    }

    pub(super) fn failed(&self) -> bool {
        self.failed
    }

    pub(super) fn new(store: &SharedStore, settings: Option<&ApiSettings>) -> Self {
        let mut controls = Self {
            server: None,
            job: None,
            failed: false,
            port: settings.map_or(DEFAULT_PORT, |value| value.port),
            message: None,
        };
        if settings.is_some_and(|value| value.enabled) {
            controls.start(store, Action::StartSaved, None);
        }
        controls
    }

    pub(super) fn running_port(&self) -> Option<u16> {
        self.server.as_ref().map(ApiServer::port)
    }

    pub(super) fn progress(&self) -> Option<&'static str> {
        self.job.as_ref().map(|job| job.action.progress())
    }

    pub(super) fn start(
        &mut self,
        store: &SharedStore,
        action: Action,
        context: Option<&egui::Context>,
    ) {
        if self.job.is_some() {
            return;
        }
        self.failed = false;
        self.message = None;
        let (order, work) = sync_channel::<Request>(1);
        let (reply, completed) = sync_channel(1);
        let store = store.clone();
        let context = context.cloned();
        // Transfer an existing listener only after the worker exists. If
        // spawning fails, the window keeps the running listener and its port.
        let worker = thread::Builder::new()
            .name("api-settings".into())
            .spawn(move || {
                if let Ok(request) = work.recv() {
                    let _ = reply.send(apply(&store, request));
                    if let Some(context) = context {
                        context.request_repaint();
                    }
                }
            });
        if worker.is_err() {
            self.fail("Не удалось запустить операцию с локальным API.");
            return;
        }
        let request = Request {
            action,
            port: self.port,
            server: self.server.take(),
        };
        if let Err(TrySendError::Full(request) | TrySendError::Disconnected(request)) =
            order.try_send(request)
        {
            self.server = request.server;
            self.fail("Не удалось передать настройки локального API.");
            return;
        }
        self.job = Some(Job { action, completed });
    }

    pub(super) fn poll(&mut self) {
        let Some(job) = &self.job else { return };
        let action = job.action;
        let response = match job.completed.try_recv() {
            Ok(response) => response,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.job = None;
                self.fail("Операция с локальным API прервана.");
                return;
            }
        };
        self.job = None;
        self.server = response.server;
        match response.result {
            Ok(()) if matches!(action, Action::Rotate) => {
                self.message = Some(("Токены заменены. Обновите их в интеграциях.".into(), false));
            }
            Ok(()) => {}
            Err(error) => self.fail(error.to_string()),
        }
    }

    fn fail(&mut self, text: impl Into<String>) {
        self.failed = true;
        self.message = Some((text.into(), true));
    }
}

fn apply(store: &SharedStore, request: Request) -> Completed {
    let Request {
        action,
        port,
        mut server,
    } = request;
    let result = match action {
        Action::StartSaved | Action::Enable => {
            let result = ApiServer::start(store.clone(), port)
                .map_err(|error| format!("Не удалось открыть локальный порт. {error}").into())
                .and_then(|listener| {
                    if matches!(action, Action::Enable) {
                        save_enabled(store, port)?;
                    }
                    Ok(listener)
                });
            result.map(|listener| server = Some(listener))
        }
        Action::Rotate => rotate_tokens(store),
        Action::Disable => save_disabled(store, port).map(|()| drop(server.take())),
        Action::CloseListener => {
            drop(server.take());
            Ok(())
        }
    };
    Completed { server, result }
}

fn save_enabled(store: &SharedStore, port: u16) -> Fallible<()> {
    store.lock().unwrap().transaction(|state| {
        let existing = state.api.as_ref();
        let token = existing.map_or_else(random_bytes, |settings| Ok(settings.token))?;
        let import_token = existing
            .and_then(|settings| settings.import_token)
            .map_or_else(random_bytes, Ok)?;
        state.api = Some(ApiSettings {
            token,
            import_token: Some(import_token),
            port,
            enabled: true,
        });
        Ok(())
    })
}

fn save_disabled(store: &SharedStore, port: u16) -> Fallible<()> {
    // An admitted write checks this state under the same Store lock immediately
    // before its transaction. Close and drain the listener only after commit.
    store.lock().unwrap().transaction(|state| {
        if let Some(settings) = &mut state.api {
            settings.enabled = false;
            settings.port = port;
        }
        Ok(())
    })
}

fn rotate_tokens(store: &SharedStore) -> Fallible<()> {
    store.lock().unwrap().transaction(|state| {
        let settings = state
            .api
            .as_mut()
            .filter(|settings| settings.enabled)
            .ok_or("Локальный API выключен.")?;
        settings.token = random_bytes()?;
        settings.import_token = Some(random_bytes()?);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        net::{Ipv4Addr, TcpListener},
        time::{Duration, Instant},
    };

    use godovoy_plan::storage::Store;

    use super::*;

    fn profile() -> (tempfile::TempDir, SharedStore) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("profile")).unwrap();
        (directory, store)
    }

    fn wait(controls: &mut ApiControls) {
        let deadline = Instant::now() + Duration::from_secs(4);
        while controls.job.is_some() && Instant::now() < deadline {
            controls.poll();
            thread::sleep(Duration::from_millis(5));
        }
        assert!(controls.job.is_none(), "API operation did not complete");
    }

    fn settings(store: &SharedStore) -> Option<ApiSettings> {
        store.lock().unwrap().state().api.clone()
    }

    fn health(port: u16, token: &[u8; 32]) -> Fallible<()> {
        use godovoy_plan::api::{ApiKeyKind, ApiMethod, request_secure};
        let reply = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(request_secure(
                port,
                token,
                ApiKeyKind::Main,
                ApiMethod::Get,
                "/v1/health",
                &[],
            ))?;
        if reply.status != 200 {
            return Err("API недоступен.".into());
        }
        Ok(())
    }

    #[test]
    fn upgrading_disables_old_bearer_keys_and_persists_new_noise_keys() {
        let (directory, store) = profile();
        let path = directory.path().join("profile");
        store
            .lock()
            .unwrap()
            .transaction(|s| s.create_workspace("Команда".into(), "Анна".into()))
            .unwrap();
        drop(store);
        let file = path.join("calendar.json");
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        legacy.as_object_mut().unwrap().remove("secure_api_v1");
        legacy["api"] = serde_json::json!({"enabled": true, "port": 43857, "token": vec![11u8;32], "import_token":vec![22u8;32]});
        fs::write(&file, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let store = Store::open_existing(&path).unwrap();
        assert!(settings(&store).is_none());
        save_enabled(&store, 43857).unwrap();
        let fresh = settings(&store).unwrap();
        assert_ne!(fresh.token, [11; 32]);
        assert_ne!(fresh.import_token, Some([22; 32]));
        drop(store);
        let persisted = Store::open_existing(&path).unwrap();
        assert_eq!(settings(&persisted), Some(fresh));
        assert!(persisted.lock().unwrap().state().workspace.is_some());
    }

    #[test]
    fn rotate_and_disable_keep_settings_and_close_listener_in_order() {
        let (_directory, store) = profile();
        let mut controls = ApiControls::new(&store, None);
        controls.port = 0;
        controls.start(&store, Action::Enable, None);
        wait(&mut controls);
        let port = controls.running_port().unwrap();
        let first = settings(&store).unwrap();
        assert!(first.enabled);
        health(port, &first.token).unwrap();

        controls.start(&store, Action::Rotate, None);
        wait(&mut controls);
        let rotated = settings(&store).unwrap();
        assert_ne!(first.token, rotated.token);
        assert_ne!(first.import_token, rotated.import_token);
        assert_eq!(controls.running_port(), Some(port));
        assert!(health(port, &first.token).is_err());
        health(port, &rotated.token).unwrap();

        controls.start(&store, Action::CloseListener, None);
        wait(&mut controls);
        assert_eq!(controls.running_port(), None);
        assert!(settings(&store).unwrap().enabled);
        assert!(TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok());

        let mut reopened = ApiControls::new(&store, Some(&rotated));
        wait(&mut reopened);
        let reopened_port = reopened.running_port().unwrap();
        health(reopened_port, &rotated.token).unwrap();
        reopened.start(&store, Action::Disable, None);
        wait(&mut reopened);
        assert_eq!(reopened.running_port(), None);
        let disabled = settings(&store).unwrap();
        assert!(!disabled.enabled);
        assert_eq!(disabled.token, rotated.token);
        assert!(TcpListener::bind((Ipv4Addr::LOCALHOST, reopened_port)).is_ok());
    }

    #[test]
    fn bind_failure_keeps_profile_unchanged() {
        let (_directory, store) = profile();
        let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut controls = ApiControls::new(&store, None);
        controls.port = occupied.local_addr().unwrap().port();

        controls.start(&store, Action::Enable, None);
        wait(&mut controls);
        assert!(controls.failed);
        assert!(controls.running_port().is_none());
        assert!(settings(&store).is_none());

        drop(occupied);
        controls.start(&store, Action::Enable, None);
        wait(&mut controls);
        assert!(!controls.failed);
        assert_eq!(controls.running_port(), Some(controls.port));
    }

    #[test]
    fn failed_disable_keeps_listener_and_credentials() {
        let (directory, store) = profile();
        let mut controls = ApiControls::new(&store, None);
        controls.port = 0;
        controls.start(&store, Action::Enable, None);
        wait(&mut controls);
        let port = controls.running_port().unwrap();
        let current = settings(&store).unwrap();

        let directory = directory.path().join("profile");
        let calendar = directory.join("calendar.json");
        let preserved = directory.join("test-original.json");
        fs::rename(&calendar, &preserved).unwrap();
        fs::create_dir(&calendar).unwrap();
        controls.start(&store, Action::Disable, None);
        wait(&mut controls);
        assert!(controls.failed);
        assert_eq!(controls.running_port(), Some(port));
        let after = settings(&store).unwrap();
        assert!(after.enabled);
        assert_eq!(after.token, current.token);
        assert_eq!(after.import_token, current.import_token);
        health(port, &current.token).unwrap();

        fs::remove_dir(&calendar).unwrap();
        fs::rename(&preserved, &calendar).unwrap();
        controls.start(&store, Action::Disable, None);
        wait(&mut controls);
        assert!(!controls.failed);
        assert_eq!(controls.running_port(), None);
        assert!(!settings(&store).unwrap().enabled);
    }

    #[test]
    fn dispatch_does_not_wait_for_the_profile_writer() {
        let (_directory, store) = profile();
        let worker_store = store.clone();
        let held = store.lock().unwrap();
        let mut controls = thread::scope(|scope| {
            let (send, receive) = sync_channel(1);
            scope.spawn(move || {
                let mut controls = ApiControls::new(&worker_store, None);
                controls.port = 0;
                controls.start(&worker_store, Action::Enable, None);
                let _ = send.send(controls);
            });
            let started = receive.recv_timeout(Duration::from_secs(1));
            drop(held);
            started.expect("dispatch blocked waiting for the Store lock")
        });
        assert!(controls.job.is_some());
        wait(&mut controls);
        assert!(controls.running_port().is_some());
    }
}
