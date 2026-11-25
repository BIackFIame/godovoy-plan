#[path = "admission.rs"]
mod admission;
#[path = "mcp.rs"]
mod mcp;
#[path = "standby.rs"]
mod standby;
use admission::{Admission, Discovery, credential_hint};
use standby::Mode;

use super::actions::{self, Command};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use godovoy_plan::{
    Fallible,
    model::Id,
    storage::online::{Access, Data, GrantPurpose, LockedVault, OnlineVault, record},
};
use hyper::server::conn::http1;
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use serde::Deserialize;
use serde_json::json;
use std::{
    net::Ipv4Addr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{net::TcpListener, sync::Semaphore};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryPoint {
    Office,
    Remote,
}

#[derive(Clone)]
pub struct Host {
    vault: OnlineVault,
    origin: String,
    authority: String,
    entry: EntryPoint,
    mode: Mode,
    operations: Arc<Semaphore>,
    discovery: Arc<Mutex<Discovery>>,
    new_checks: Arc<Semaphore>,
}
impl Host {
    pub fn new(vault: OnlineVault, origin: &str, entry: Option<EntryPoint>) -> Fallible<Self> {
        let uri = origin.parse::<Uri>()?;
        let authority = uri.authority().ok_or("Нужен origin с именем узла.")?;
        let local = matches!(authority.host(), "127.0.0.1" | "localhost" | "[::1]");
        let entry = entry.unwrap_or(if local {
            EntryPoint::Office
        } else {
            EntryPoint::Remote
        });
        let allowed_scheme =
            uri.scheme_str() == Some("https") || (local && uri.scheme_str() == Some("http"));
        if authority.as_str().contains('@')
            || authority.host().is_empty()
            || authority.as_str().ends_with(':')
            || uri.path() != "/" && !uri.path().is_empty()
            || uri.query().is_some()
            || !allowed_scheme
        {
            return Err(
                "Для удалённого узла укажите HTTPS origin без пути, параметров и учётных данных."
                    .into(),
            );
        }
        if entry == EntryPoint::Remote && uri.scheme_str() != Some("https") {
            return Err("Для внешнего входа нужен явный --origin https://…".into());
        }
        let origin = format!("{}://{}", uri.scheme_str().unwrap_or_default(), authority);
        Ok(Self {
            vault,
            origin,
            authority: authority.as_str().into(),
            entry,
            mode: Mode::Primary,
            operations: Arc::new(Semaphore::new(16)),
            discovery: Arc::new(Mutex::new(Discovery::new(Instant::now()))),
            new_checks: Arc::new(Semaphore::new(4)),
        })
    }
    fn enable_standby(&mut self) -> Fallible<()> {
        if self.entry != EntryPoint::Office {
            return Err("Резервный узел поддерживает только офисный вход.".into());
        }
        self.mode = Mode::standby(&self.vault.lock()?)?;
        Ok(())
    }
}
fn standby_unavailable() -> Response {
    let mut response = problem(
        StatusCode::SERVICE_UNAVAILABLE,
        "Вход закрыт. Проверьте состояние резервного окна.",
    );
    response
        .headers_mut()
        .insert("x-godovoy-standby", HeaderValue::from_static("unavailable"));
    response
}
fn problem(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error":message}))).into_response()
}
fn discovery_limited() -> Response {
    let mut response = problem(
        StatusCode::TOO_MANY_REQUESTS,
        "Проверка новых ключей занята. Повторите запрос через секунду.",
    );
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    response
}
fn protection_denied(message: &str, code: &'static str) -> Response {
    let mut response = problem(StatusCode::FORBIDDEN, message);
    response
        .headers_mut()
        .insert("x-godovoy-access", HeaderValue::from_static(code));
    response
}
fn unavailable() -> Response {
    problem(
        StatusCode::SERVICE_UNAVAILABLE,
        "Хранилище узла недоступно.",
    )
}
async fn headers(State(host): State<Host>, req: Request, next: Next) -> Response {
    let h = req.headers();
    let authority = h.get(header::HOST).and_then(|h| h.to_str().ok());
    let valid_origin = h
        .get(header::ORIGIN)
        .is_none_or(|v| v.as_bytes() == host.origin.as_bytes());
    let cross_site = h
        .get("sec-fetch-site")
        .is_some_and(|v| v.as_bytes() == b"cross-site");
    let absolute_authority = req
        .uri()
        .authority()
        .is_some_and(|a| !a.as_str().eq_ignore_ascii_case(&host.authority));
    let mut response = if !authority.is_some_and(|a| a.eq_ignore_ascii_case(&host.authority))
        || absolute_authority
        || !valid_origin
        || (cross_site && req.uri().path() != "/")
    {
        problem(
            StatusCode::FORBIDDEN,
            "Недопустимый адрес или источник запроса.",
        )
    } else {
        next.run(req).await
    };
    let h = response.headers_mut();
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, private, max-age=0"),
    );
    h.insert("pragma", HeaderValue::from_static("no-cache"));
    h.insert("content-security-policy",HeaderValue::from_static("default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; form-action 'none'; base-uri 'none'"));
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("same-origin"),
    );
    response
}
async fn with_auth<F>(
    host: Host,
    headers: HeaderMap,
    purpose: GrantPurpose,
    write: bool,
    action: F,
) -> Response
where
    F: FnOnce(&LockedVault<'_>, &Access, Id, Id, bool, &Mode) -> Response + Send + 'static,
{
    let Some((token, presented_id, hint)) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|v| v.len() == 80)
        .and_then(|value| {
            let (id, hint) = credential_hint(value, purpose)?;
            Some((zeroize::Zeroizing::new(value.to_owned()), id, hint))
        })
    else {
        return problem(
            StatusCode::UNAUTHORIZED,
            "Для доступа нужен действующий ключ.",
        );
    };
    let admission = match host.discovery.lock() {
        Ok(mut gate) => gate.admit(&hint, Instant::now()),
        Err(_) => return unavailable(),
    };
    let new_check = match admission {
        Admission::Known => None,
        Admission::New => match host.new_checks.clone().try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => return discovery_limited(),
        },
        Admission::Limited => return discovery_limited(),
    };
    let Ok(slot) = host.operations.clone().try_acquire_owned() else {
        return problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "Узел занят, повторите запрос.",
        );
    };
    tokio::task::spawn_blocking(move || {
        let _slot = slot;
        let Ok(lock) = host.vault.lock() else {
            return unavailable();
        };
        let Ok(mut acl) = lock.access() else {
            return unavailable();
        };
        let Some((id, grant)) = acl.authorize_for(&token, purpose) else {
            let record_denial = match host.discovery.lock() {
                Ok(mut gate) => gate.reject(&hint, Instant::now()),
                Err(_) => return unavailable(),
            };
            if record_denial
                && lock
                    .audit_security(
                        "authorization_denied",
                        acl.grants
                            .contains_key(&presented_id)
                            .then_some(presented_id),
                        None,
                        None,
                    )
                    .is_err()
            {
                return unavailable();
            }
            return problem(
                StatusCode::UNAUTHORIZED,
                "Ключ недействителен или доступ отозван.",
            );
        };
        let employee = grant.employee;
        let (read_only, allow_remote) = match purpose {
            GrantPurpose::Employee => (grant.read_only, grant.allow_remote),
            GrantPurpose::Mcp => {
                let Some(rights) = acl.mcp_permissions(grant) else {
                    return problem(StatusCode::UNAUTHORIZED, "Доступ по ключу MCP отозван.");
                };
                (rights.read_only, rights.allow_remote)
            }
        };
        if purpose == GrantPurpose::Mcp {
            // Only the authorization view of this request is narrowed. Persistent employee rights stay separate.
            acl.assigned_only.insert(employee);
        }
        let Ok(deadline) = host.mode.check(&lock) else {
            if lock
                .audit_security("standby_access_denied", Some(id), Some(employee), None)
                .is_err()
            {
                return unavailable();
            }
            return standby_unavailable();
        };
        let settings = acl.protection().settings;
        if host.entry == EntryPoint::Remote && !settings.remote_allowed() {
            if lock
                .audit_security("protection_remote_denied", Some(id), Some(employee), None)
                .is_err()
            {
                return unavailable();
            }
            return protection_denied("Внешний вход отключён администратором.", "remote-disabled");
        }
        if host.entry == EntryPoint::Remote && !allow_remote {
            if lock
                .audit_security("remote_access_denied", Some(id), Some(employee), None)
                .is_err()
            {
                return unavailable();
            }
            let mut response = problem(
                StatusCode::FORBIDDEN,
                "Этот ключ действует только в офисной сети.",
            );
            response
                .headers_mut()
                .insert("x-godovoy-access", HeaderValue::from_static("office-only"));
            return response;
        }
        if write && read_only {
            if lock
                .audit_security("write_denied", Some(id), Some(employee), None)
                .is_err()
            {
                return unavailable();
            }
            return problem(StatusCode::FORBIDDEN, "Разрешено только чтение.");
        };
        match host.discovery.lock() {
            Ok(mut gate) => gate.remember(hint),
            Err(_) => return unavailable(),
        }
        drop(new_check);
        let mut response = action(&lock, &acl, id, employee, !read_only, &host.mode);
        if response.status().is_success() {
            let scope = if acl.assigned_only.contains(&employee) {
                "true"
            } else {
                "false"
            };
            response
                .headers_mut()
                .insert("x-godovoy-assigned-only", HeaderValue::from_static(scope));
            let enabled = if settings.free_text_allowed() {
                "true"
            } else {
                "false"
            };
            response.headers_mut().insert(
                "x-godovoy-allow-free-text",
                HeaderValue::from_static(enabled),
            );
        }
        if response.status().is_success()
            && let Some(deadline) = deadline
        {
            response
                .headers_mut()
                .insert("x-godovoy-standby-until", HeaderValue::from(deadline));
        }
        response
    })
    .await
    .unwrap_or_else(|_| unavailable())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Year {
    year: i32,
}
async fn state(
    State(host): State<Host>,
    headers: HeaderMap,
    Query(query): Query<Year>,
) -> Response {
    if !(1900..=9999).contains(&query.year) {
        return problem(StatusCode::BAD_REQUEST, "Год должен быть от 1900 до 9999.");
    }
    with_auth(
        host,
        headers,
        GrantPurpose::Employee,
        false,
        move |lock, acl, id, employee, edit, mode| match lock.data() {
            Ok(data) => {
                if lock
                    .audit_security("state_requested", Some(id), Some(employee), None)
                    .is_err()
                {
                    return unavailable();
                }
                let response =
                    Json(actions::state(&data, acl, query.year, employee, edit)).into_response();
                if matches!(mode, Mode::Standby(_)) && mode.check(lock).is_err() {
                    return standby_unavailable();
                }
                response
            }
            Err(_) => unavailable(),
        },
    )
    .await
}
async fn session(State(host): State<Host>, headers: HeaderMap) -> Response {
    with_auth(
        host,
        headers,
        GrantPurpose::Employee,
        false,
        |_, _, _, _, _, _| StatusCode::NO_CONTENT.into_response(),
    )
    .await
}
async fn command(State(host): State<Host>, headers: HeaderMap, body: Bytes) -> Response {
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(';').next())
        .is_none_or(|s| !s.trim().eq_ignore_ascii_case("application/json"))
    {
        return problem(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Требуется application/json.",
        );
    }
    with_auth(
        host,
        headers,
        GrantPurpose::Employee,
        true,
        move |lock, acl, credential, employee, _, mode| {
            let Ok(cmd) = serde_json::from_slice::<Command>(&body) else {
                return problem(StatusCode::BAD_REQUEST, "Недопустимый формат команды.");
            };
            let Ok(data) = lock.data() else {
                return unavailable();
            };
            commit_command(lock, acl, credential, employee, mode, cmd, data)
        },
    )
    .await
}
async fn with_mcp_auth<F>(host: Host, headers: HeaderMap, write: bool, action: F) -> Response
where
    F: FnOnce(&LockedVault<'_>, &Access, Id, Id, bool, &Mode) -> Response + Send + 'static,
{
    with_auth(host, headers, GrantPurpose::Mcp, write, action).await
}

fn commit_command(
    lock: &LockedVault<'_>,
    acl: &Access,
    credential: Id,
    employee: Id,
    mode: &Mode,
    cmd: Command,
    mut data: Data,
) -> Response {
    if !acl.protection().settings.free_text_allowed() && cmd.uses_free_text() {
        if lock
            .audit_security(
                "protection_free_text_denied",
                Some(credential),
                Some(employee),
                None,
            )
            .is_err()
        {
            return unavailable();
        }
        return protection_denied(
            "Описания и заметки отключены администратором.",
            "free-text-disabled",
        );
    }
    if acl.assigned_only.contains(&employee) && !cmd.allowed_for_assignee(&data, employee) {
        if lock
            .audit_security(
                "employee_scope_denied",
                Some(credential),
                Some(employee),
                None,
            )
            .is_err()
        {
            return unavailable();
        }
        return protection_denied(
            "Действие недоступно в персональном календаре.",
            "assigned-only",
        );
    }
    match actions::apply(cmd, &mut data, acl) {
        Err(e) => problem(
            if e.conflict {
                StatusCode::CONFLICT
            } else {
                StatusCode::UNPROCESSABLE_ENTITY
            },
            e.message,
        ),
        Ok((kind, event)) => {
            if let Err(e) = data.calendar.validate() {
                return problem(StatusCode::UNPROCESSABLE_ENTITY, e);
            }
            let audit_kind = match kind {
                "create" => "create_requested",
                "edit" => "edit_requested",
                "delete" => "delete_requested",
                "check" => "check_requested",
                "note" => "note_requested",
                _ => return unavailable(),
            };
            if lock
                .audit_security(audit_kind, Some(credential), Some(employee), Some(event))
                .is_err()
            {
                return unavailable();
            }
            record(&mut data.audit, kind, credential, employee, Some(event));
            if matches!(mode, Mode::Standby(_)) && mode.check(lock).is_err() {
                return standby_unavailable();
            }
            match lock.save_data(&data) {
                Ok(()) => StatusCode::NO_CONTENT.into_response(),
                Err(_) => unavailable(),
            }
        }
    }
}
pub fn router(host: Host) -> Router {
    Router::new()
        .route("/", get(|| async { Html(include_str!("index.html")) }))
        .route(
            "/calendar.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("calendar.js"),
                )
            }),
        )
        .route(
            "/calendar.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("calendar.css"),
                )
            }),
        )
        .route("/mcp", post(mcp::handle))
        .route("/v1/session", get(session))
        .route("/v1/state", get(state))
        .route("/v1/command", post(command))
        .layer(DefaultBodyLimit::max(128 * 1024))
        .layer(middleware::from_fn_with_state(host.clone(), headers))
        .with_state(host)
}
pub async fn serve(
    vault: OnlineVault,
    port: u16,
    origin: Option<String>,
    entry: Option<EntryPoint>,
    standby: bool,
) -> Fallible<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    let addr = listener.local_addr()?;
    let origin = origin.unwrap_or_else(|| format!("http://{addr}"));
    let mut host = Host::new(vault, &origin, entry)?;
    if standby {
        host.enable_standby()?;
    }
    let scope = match host.entry {
        EntryPoint::Office => "офисный",
        EntryPoint::Remote => "внешний",
    };
    let app = router(host);
    eprintln!("Сервер слушает {addr}. Вход {scope}. Адрес клиента {origin}");
    serve_listener(listener, app).await
}
pub(super) async fn serve_listener(listener: TcpListener, app: Router) -> Fallible<()> {
    let admissions = Arc::new(Semaphore::new(32));
    loop {
        let (socket, _) = listener.accept().await?;
        let Ok(slot) = admissions.clone().try_acquire_owned() else {
            continue;
        };
        let service = TowerToHyperService::new(app.clone());
        tokio::spawn(async move {
            let _slot = slot;
            let mut http = http1::Builder::new();
            http.timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(5))
                .keep_alive(false)
                .max_buf_size(32 * 1024);
            let connection = http.serve_connection(TokioIo::new(socket), service);
            let _ = tokio::time::timeout(Duration::from_secs(20), connection).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Method,
    };
    use godovoy_plan::{model::Calendar, storage::Store};
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use tower::ServiceExt;

    struct Fixture {
        _tmp: TempDir,
        vault: OnlineVault,
        app: Router,
        key: String,
        credential: Id,
        employee: Id,
    }
    impl Fixture {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let vault = OnlineVault::init(
                &tmp.path().join("host"),
                "Компания".into(),
                Calendar::default(),
                Default::default(),
            )
            .unwrap();
            let (credential, employee, key) =
                vault.lock().unwrap().grant("Анна", None, false).unwrap();
            let host = Host::new(
                vault.clone(),
                "https://calendar.test",
                Some(EntryPoint::Office),
            )
            .unwrap();
            Self {
                _tmp: tmp,
                vault,
                app: router(host),
                key,
                credential,
                employee,
            }
        }
        async fn request(&self, path: &str, key: &str, body: Option<Value>) -> (StatusCode, Value) {
            let mut req = Request::builder()
                .uri(path)
                .header("Host", "calendar.test")
                .header("Authorization", format!("Bearer {key}"));
            let bytes = if let Some(body) = body {
                req = req
                    .method(Method::POST)
                    .header(header::CONTENT_TYPE, "application/json");
                body.to_string()
            } else {
                String::new()
            };
            let res = self
                .app
                .clone()
                .oneshot(req.body(Body::from(bytes)).unwrap())
                .await
                .unwrap();
            assert!(
                res.headers()[header::CACHE_CONTROL]
                    .to_str()
                    .unwrap()
                    .contains("no-store")
            );
            let status = res.status();
            let bytes = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
            (
                status,
                serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            )
        }
        async fn cmd(&self, v: Value) -> StatusCode {
            self.request("/v1/command", &self.key, Some(v)).await.0
        }
        async fn state(&self) -> Value {
            let (s, data) = self.request("/v1/state?year=2028", &self.key, None).await;
            assert_eq!(s, StatusCode::OK, "{data}");
            data
        }
        fn create(&self) -> Value {
            json!({"op":"createEvent","title":"Поздравления","description":"","category":"Partners","ownerId":self.employee,
            "date":"2028-02-29","yearly":true,"preparationDays":30,"checklist":[{"title":"Список","daysBefore":10}]})
        }
    }
    #[tokio::test]
    async fn known_employee_can_work_and_be_revoked_while_new_checks_are_busy() {
        let mut f = Fixture::new();
        let (_, _, second) = f.vault.lock().unwrap().grant("Борис", None, false).unwrap();
        let host = Host::new(
            f.vault.clone(),
            "https://calendar.test",
            Some(EntryPoint::Office),
        )
        .unwrap();
        let checks = host.new_checks.clone();
        f.app = router(host);
        let path = "/v1/session";
        assert_eq!(
            f.request(path, &f.key, None).await.0,
            StatusCode::NO_CONTENT
        );
        let busy = checks
            .acquire_many(checks.available_permits() as u32)
            .await
            .unwrap();
        assert_eq!(
            f.request(path, &second, None).await.0,
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            f.request(path, &f.key, None).await.0,
            StatusCode::NO_CONTENT
        );
        f.vault.lock().unwrap().revoke(f.credential).unwrap();
        assert_eq!(
            f.request(path, &f.key, None).await.0,
            StatusCode::UNAUTHORIZED
        );
        drop(busy);
        assert_eq!(
            f.request(path, &second, None).await.0,
            StatusCode::NO_CONTENT
        );
    }

    #[tokio::test]
    async fn protection_applies_to_existing_sessions_grants_and_minimizes_both_event_lists() {
        use chrono::Datelike;
        use godovoy_plan::storage::online::{GrantPermissions, ProtectionSettings};
        let mut f = Fixture::new();
        let today = chrono::Local::now().date_naive();
        let year = today.year();
        let query = format!("/v1/state?year={year}");
        let (_, _, remote_key) = f
            .vault
            .lock()
            .unwrap()
            .grant_with_permissions(
                "Анна",
                Some(f.employee),
                GrantPermissions {
                    read_only: true,
                    allow_remote: true,
                },
            )
            .unwrap();
        let mut values = f.create();
        values["date"] = json!(today);
        values["description"] = json!("private-description");
        assert_eq!(f.cmd(values.clone()).await, StatusCode::NO_CONTENT);
        let previous = f.request(&query, &f.key, None).await.1;
        let event = &previous["events"][0];
        let id = &event["id"];
        assert_eq!(
            f.cmd(json!({"op":"note","eventId":id,"year":year,"base":"","text":"private-note"}))
                .await,
            StatusCode::NO_CONTENT
        );
        let restricted = ProtectionSettings {
            belarus: true,
            ..Default::default()
        };
        f.vault
            .lock()
            .unwrap()
            .set_protection(0, restricted)
            .unwrap();
        assert!(
            f.vault
                .lock()
                .unwrap()
                .grant_with_permissions(
                    "Борис",
                    None,
                    GrantPermissions {
                        read_only: false,
                        allow_remote: true
                    }
                )
                .is_err()
        );
        let (_, state) = f.request(&query, &remote_key, None).await;
        assert_eq!(state["canEdit"], false);
        assert_eq!(state["allowFreeText"], false);
        assert!(!state["tasks"].as_array().unwrap().is_empty());
        for list in ["events", "tasks"] {
            assert_eq!(state[list][0]["description"], "");
            assert_eq!(state[list][0]["note"], "");
        }
        assert!(!state.to_string().contains("private-"));
        f.vault.lock().unwrap().begin_standby(5).unwrap();
        let mut standby = Host::new(
            f.vault.clone(),
            "https://calendar.test",
            Some(EntryPoint::Office),
        )
        .unwrap();
        standby.enable_standby().unwrap();
        let primary = std::mem::replace(&mut f.app, router(standby));
        let request = Request::builder()
            .uri("/v1/session")
            .header(header::HOST, "calendar.test")
            .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
            .body(Body::empty())
            .unwrap();
        let response = f.app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(response.headers()["x-godovoy-allow-free-text"], "false");
        assert!(response.headers().contains_key("x-godovoy-standby-until"));
        assert!(
            !f.request(&query, &f.key, None)
                .await
                .1
                .to_string()
                .contains("private-")
        );
        assert_eq!(f.cmd(values.clone()).await, StatusCode::FORBIDDEN);
        f.vault.lock().unwrap().end_standby().unwrap();
        f.app = primary;
        assert_eq!(f.cmd(values.clone()).await, StatusCode::FORBIDDEN);
        assert_eq!(
            f.cmd(json!({"op":"note","eventId":id,"year":year,"base":"private-note","text":""}))
                .await,
            StatusCode::FORBIDDEN
        );
        values["op"] = json!("editEvent");
        values["eventId"] = id.clone();
        values["year"] = json!(year);
        values["revision"] = event["revision"].clone();
        values["scope"] = json!("year");
        values["title"] = json!("Новое название");
        values["description"] = json!("");
        values["checklist"][0]["id"] = event["checklist"][0]["id"].clone();
        assert_eq!(f.cmd(values).await, StatusCode::NO_CONTENT);
        let remote = Host::new(
            f.vault.clone(),
            "https://calendar.test",
            Some(EntryPoint::Remote),
        )
        .unwrap();
        let office = std::mem::replace(&mut f.app, router(remote));
        for path in ["/v1/session", query.as_str(), "/v1/command"] {
            let request = Request::builder()
                .uri(path)
                .method(if path.ends_with("command") {
                    Method::POST
                } else {
                    Method::GET
                })
                .header(header::HOST, "calendar.test")
                .header(header::AUTHORIZATION, format!("Bearer {remote_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap();
            let response = f.app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert_eq!(response.headers()["x-godovoy-access"], "remote-disabled");
        }
        f.vault
            .lock()
            .unwrap()
            .set_protection(
                1,
                ProtectionSettings {
                    russia: true,
                    allow_remote: true,
                    allow_free_text: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let restored = f.request(&query, &remote_key, None).await.1;
        assert_eq!(restored["allowFreeText"], true);
        assert_eq!(restored["events"][0]["title"], "Новое название");
        assert_eq!(restored["events"][0]["description"], "private-description");
        assert_eq!(restored["events"][0]["note"], "private-note");
        assert_eq!(
            f.request("/v1/command", &remote_key, Some(f.create()))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        f.app = office;
        assert_eq!(
            f.request("/v1/session", &f.key, None).await.0,
            StatusCode::NO_CONTENT
        );
    }

    #[tokio::test]
    async fn restricted_employee_sees_and_updates_only_own_current_occurrences_on_every_key() {
        use godovoy_plan::storage::online::ProtectionSettings;
        let mut f = Fixture::new();
        let (_, colleague, colleague_key) =
            f.vault.lock().unwrap().grant("Борис", None, false).unwrap();
        let (_, _, reader) = f
            .vault
            .lock()
            .unwrap()
            .grant("Анна", Some(f.employee), true)
            .unwrap();
        let mut owned = f.create();
        owned["title"] = json!("mine");
        assert_eq!(f.cmd(owned.clone()).await, StatusCode::NO_CONTENT);
        let mut foreign = f.create();
        foreign["title"] = json!("private-to-colleague");
        foreign["ownerId"] = json!(colleague);
        assert_eq!(f.cmd(foreign).await, StatusCode::NO_CONTENT);
        let before = f.state().await;
        let events = before["events"].as_array().unwrap();
        let mine = events.iter().find(|e| e["title"] == "mine").unwrap();
        let other = events
            .iter()
            .find(|e| e["title"] == "private-to-colleague")
            .unwrap();
        let check = |event: &Value| json!({"op":"check","eventId":event["id"],"taskId":event["checklist"][0]["id"],"year":2028,"done":true});
        let note = |id: &Value, year| json!({"op":"note","eventId":id,"year":year,"text":"готово","base":""});
        assert!(!before["assignedOnly"].as_bool().unwrap());
        f.vault
            .lock()
            .unwrap()
            .set_employee_scope(f.employee, true)
            .unwrap();
        let (_, _, future) = f
            .vault
            .lock()
            .unwrap()
            .grant("Анна", Some(f.employee), false)
            .unwrap();
        for key in [&f.key, &reader, &future] {
            let state = f.request("/v1/state?year=2028", key, None).await.1;
            assert_eq!(state["assignedOnly"], true);
            assert_eq!(state["events"].as_array().unwrap().len(), 1);
            assert_eq!(state["employees"].as_array().unwrap().len(), 1);
            assert_eq!(state["employees"][0]["id"], json!(f.employee));
            assert!(!state.to_string().contains("private-to-colleague"));
            assert!(!state.to_string().contains(&colleague.to_string()));
        }
        assert_eq!(
            f.request("/v1/state?year=2028", &colleague_key, None)
                .await
                .1["events"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            f.request("/v1/command", &reader, Some(check(mine))).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(f.cmd(check(mine)).await, StatusCode::NO_CONTENT);
        assert_eq!(f.cmd(note(&mine["id"], 2028)).await, StatusCode::NO_CONTENT);
        let denied = f
            .request("/v1/command", &future, Some(note(&other["id"], 2028)))
            .await;
        let unknown = f
            .request(
                "/v1/command",
                &future,
                Some(note(&json!(Id::new_v4()), 2028)),
            )
            .await;
        assert_eq!(denied, unknown);
        assert_eq!(denied.0, StatusCode::FORBIDDEN);
        assert_eq!(f.cmd(check(other)).await, StatusCode::FORBIDDEN);
        assert_eq!(f.cmd(owned.clone()).await, StatusCode::FORBIDDEN);
        let mut change = owned.clone();
        change["op"] = json!("editEvent");
        change["eventId"] = mine["id"].clone();
        change["year"] = json!(2028);
        change["revision"] = mine["revision"].clone();
        change["scope"] = json!("year");
        change["ownerId"] = json!(colleague);
        assert_eq!(f.cmd(change.clone()).await, StatusCode::FORBIDDEN);
        let delete = json!({"op":"deleteEvent","eventId":mine["id"],"year":2028,"revision":mine["revision"],"scope":"future"});
        assert_eq!(f.cmd(delete).await, StatusCode::FORBIDDEN);
        // The shared editor reassigns just one occurrence. Other years stay accessible.
        assert_eq!(
            f.request("/v1/command", &colleague_key, Some(change))
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(f.cmd(note(&mine["id"], 2028)).await, StatusCode::FORBIDDEN);
        assert_eq!(f.cmd(note(&mine["id"], 2029)).await, StatusCode::NO_CONTENT);
        f.vault
            .lock()
            .unwrap()
            .set_protection(
                0,
                ProtectionSettings {
                    russia: true,
                    ..Default::default()
                },
            )
            .unwrap();
        f.vault.lock().unwrap().begin_standby(5).unwrap();
        assert!(
            f.vault
                .lock()
                .unwrap()
                .set_employee_scope(f.employee, false)
                .is_err()
        );
        let mut standby = Host::new(
            f.vault.clone(),
            "https://calendar.test",
            Some(EntryPoint::Office),
        )
        .unwrap();
        standby.enable_standby().unwrap();
        let primary = std::mem::replace(&mut f.app, router(standby));
        let request = Request::builder()
            .uri("/v1/session")
            .header(header::HOST, "calendar.test")
            .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
            .body(Body::empty())
            .unwrap();
        let response = f.app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.headers()["x-godovoy-assigned-only"], "true");
        assert!(response.headers().contains_key("x-godovoy-standby-until"));
        assert_eq!(f.state().await["events"], json!([]));
        assert_eq!(f.cmd(note(&mine["id"], 2029)).await, StatusCode::FORBIDDEN);
        f.vault.lock().unwrap().end_standby().unwrap();
        f.app = primary;
        f.vault
            .lock()
            .unwrap()
            .set_employee_scope(f.employee, false)
            .unwrap();
        assert_eq!(f.state().await["events"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn editing_future_dates_under_minimization_preserves_hidden_year_and_template_descriptions()
     {
        use godovoy_plan::storage::online::ProtectionSettings;
        let f = Fixture::new();
        let mut fields = f.create();
        fields["description"] = json!("future-only");
        assert_eq!(f.cmd(fields.clone()).await, StatusCode::NO_CONTENT);
        let event = f.state().await["events"][0].clone();
        fields["op"] = json!("editEvent");
        fields["scope"] = json!("year");
        fields["eventId"] = event["id"].clone();
        fields["year"] = json!(2028);
        fields["revision"] = event["revision"].clone();
        fields["checklist"][0]["id"] = event["checklist"][0]["id"].clone();
        fields["description"] = json!("only-this-year");
        assert_eq!(f.cmd(fields.clone()).await, StatusCode::NO_CONTENT);
        f.vault
            .lock()
            .unwrap()
            .set_protection(
                0,
                ProtectionSettings {
                    russia: true,
                    ..Default::default()
                },
            )
            .unwrap();
        fields["revision"] = f.state().await["events"][0]["revision"].clone();
        fields["scope"] = json!("future");
        fields["description"] = json!("");
        fields["title"] = json!("Updated");
        assert_eq!(f.cmd(fields).await, StatusCode::NO_CONTENT);
        f.vault
            .lock()
            .unwrap()
            .set_protection(1, ProtectionSettings::default())
            .unwrap();
        assert_eq!(
            f.state().await["events"][0]["description"],
            "only-this-year"
        );
        let future = f.request("/v1/state?year=2029", &f.key, None).await.1;
        assert_eq!(future["events"][0]["description"], "future-only");
        assert_eq!(future["events"][0]["title"], "Updated");
    }

    #[tokio::test]
    async fn standby_epoch_blocks_both_primary_entries_and_keeps_office_edits_and_revocations() {
        let mut f = Fixture::new();
        let new_host =
            |entry| Host::new(f.vault.clone(), "https://calendar.test", Some(entry)).unwrap();
        let mut standby = new_host(EntryPoint::Office);
        let mut remote = new_host(EntryPoint::Remote);
        assert!(standby.enable_standby().is_err());
        let window = f.vault.lock().unwrap().begin_standby(10).unwrap();
        assert!(remote.enable_standby().is_err());
        standby.enable_standby().unwrap();
        let remote = router(remote);
        for app in [f.app.clone(), remote] {
            for (path, method) in [
                ("/v1/session", Method::GET),
                ("/v1/state?year=2028", Method::GET),
                ("/v1/command", Method::POST),
            ] {
                let request = Request::builder()
                    .uri(path)
                    .method(method)
                    .header(header::HOST, "calendar.test")
                    .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(f.create().to_string()))
                    .unwrap();
                let response = app.clone().oneshot(request).await.unwrap();
                assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
                assert_eq!(response.headers()["x-godovoy-standby"], "unavailable");
            }
        }
        let primary = std::mem::replace(&mut f.app, router(standby));
        let request = Request::builder()
            .uri("/v1/session")
            .header(header::HOST, "calendar.test")
            .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
            .body(Body::empty())
            .unwrap();
        let response = f.app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            response.headers()["x-godovoy-standby-until"]
                .to_str()
                .unwrap(),
            window.deadline.to_string()
        );
        assert_eq!(f.cmd(f.create()).await, StatusCode::NO_CONTENT);
        let saved = f.state().await;
        f.vault.lock().unwrap().end_standby().unwrap();
        assert_eq!(
            f.request("/v1/session", &f.key, None).await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        let old_standby = std::mem::replace(&mut f.app, primary);
        assert_eq!(f.state().await, saved);
        let (_, _, other) = f.vault.lock().unwrap().grant("Борис", None, false).unwrap();
        f.vault.lock().unwrap().begin_standby(2).unwrap();
        f.app = old_standby;
        assert_eq!(
            f.request("/v1/session", &other, None).await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        let mut next = new_host(EntryPoint::Office);
        next.enable_standby().unwrap();
        f.app = router(next);
        f.vault.lock().unwrap().revoke(f.credential).unwrap();
        assert_eq!(
            f.request("/v1/session", &f.key, None).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            f.request("/v1/session", &other, None).await.0,
            StatusCode::NO_CONTENT
        );
    }

    #[tokio::test]
    async fn entry_policy_is_fixed_by_the_host_and_shared_vault_grants_apply_on_each_request() {
        use godovoy_plan::storage::online::GrantPermissions;
        let mut f = Fixture::new();
        for local in [
            "http://127.0.0.1:45838",
            "http://localhost",
            "https://[::1]",
        ] {
            assert_eq!(
                Host::new(f.vault.clone(), local, None).unwrap().entry,
                EntryPoint::Office
            );
        }
        assert!(
            Host::new(
                f.vault.clone(),
                "http://127.0.0.1:45838",
                Some(EntryPoint::Remote)
            )
            .is_err()
        );
        let (remote_id, _, remote_key) = f
            .vault
            .lock()
            .unwrap()
            .grant_with_permissions(
                "Анна",
                Some(f.employee),
                GrantPermissions {
                    read_only: false,
                    allow_remote: true,
                },
            )
            .unwrap();
        let (_, _, reader) = f
            .vault
            .lock()
            .unwrap()
            .grant_with_permissions(
                "Анна",
                Some(f.employee),
                GrantPermissions {
                    read_only: true,
                    allow_remote: true,
                },
            )
            .unwrap();
        assert_eq!(
            f.request("/v1/session", &remote_key, None).await.0,
            StatusCode::NO_CONTENT
        );
        let remote_host = Host::new(f.vault.clone(), "https://calendar.test", None).unwrap();
        assert_eq!(remote_host.entry, EntryPoint::Remote);
        let office = std::mem::replace(&mut f.app, router(remote_host));
        for (url, body) in [
            ("/v1/session", None),
            ("/v1/state?year=2028", None),
            ("/v1/command", Some(f.create())),
        ] {
            assert_eq!(f.request(url, &f.key, body).await.0, StatusCode::FORBIDDEN);
        }
        let session_status = |key: &str| {
            let req = Request::builder()
                .uri("/v1/session")
                .header(header::HOST, "calendar.test")
                .header("x-forwarded-for", "127.0.0.1")
                .header("x-godovoy-access", "office")
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .body(Body::empty())
                .unwrap();
            f.app.clone().oneshot(req)
        };
        let response = session_status(&f.key).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(response.headers()["x-godovoy-access"], "office-only");
        let response = session_status("invalid").await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(!response.headers().contains_key("x-godovoy-access"));
        assert_eq!(
            f.request("/v1/state?year=2028", &reader, None).await.0,
            StatusCode::OK
        );
        assert_eq!(
            f.request("/v1/command", &reader, Some(f.create())).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            f.request("/v1/command", &remote_key, Some(f.create()))
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        f.vault.lock().unwrap().revoke(remote_id).unwrap();
        assert_eq!(
            f.request("/v1/session", &remote_key, None).await.0,
            StatusCode::UNAUTHORIZED
        );
        f.app = office;
        assert_eq!(
            f.request("/v1/session", &remote_key, None).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(f.state().await["events"].as_array().unwrap().len(), 1);
        let journal = std::fs::read_dir(f._tmp.path().join("host"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
            .unwrap();
        let journal = std::fs::read_to_string(journal).unwrap();
        let denials: Vec<Value> = journal
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|r| r["action"] == "remote_access_denied")
            .collect();
        assert_eq!(denials.len(), 4);
        assert!(
            denials
                .iter()
                .all(|r| r["credential"] == json!(f.credential)
                    && r["employee"] == json!(f.employee))
        );
        assert!(!journal.contains(&f.key));
    }

    #[tokio::test]
    async fn live_revoke_and_read_only_controls_are_checked_on_every_operation() {
        let f = Fixture::new();
        let (_, _, reader) = f
            .vault
            .lock()
            .unwrap()
            .grant("Анна", Some(f.employee), true)
            .unwrap();
        assert_eq!(
            f.request("/v1/session", "invalid", None).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            f.request("/v1/session", &reader, None).await.0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            f.request("/v1/command", &reader, Some(f.create())).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(f.cmd(f.create()).await, StatusCode::NO_CONTENT);
        let dir = f._tmp.path().join("host");
        assert!(Store::open(&dir).is_err());
        let admin = OnlineVault::open(&dir).unwrap();
        admin.lock().unwrap().revoke(f.credential).unwrap();
        for path in ["/v1/session", "/v1/state?year=2028"] {
            assert_eq!(
                f.request(path, &f.key, None).await.0,
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(f.cmd(f.create()).await, StatusCode::UNAUTHORIZED);
        let state = f.request("/v1/state?year=2028", &reader, None).await.1;
        assert_eq!(state["canEdit"], false);
        assert_eq!(state["events"].as_array().unwrap().len(), 1);
        assert!(!state.to_string().contains(&f.key));
        assert!(!state.to_string().contains("digest"));
        assert!(
            !std::fs::read_to_string(dir.join("online-access-v1.json"))
                .unwrap()
                .contains(f.key.split_once('.').unwrap().1)
        );
    }
    #[tokio::test]
    async fn serialized_updates_preserve_checklists_and_detect_stale_notes_and_edits() {
        let f = Fixture::new();
        assert_eq!(f.cmd(f.create()).await, StatusCode::NO_CONTENT);
        let event = f.state().await["events"][0].clone();
        let id = &event["id"];
        let task = &event["checklist"][0]["id"];
        assert_eq!(
            f.cmd(json!({"op":"check","year":2028,"eventId":id,"taskId":task,"done":true}))
                .await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            f.cmd(json!({"op":"note","year":2028,"eventId":id,"text":"Готово","base":""}))
                .await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            f.cmd(json!({"op":"note","year":2028,"eventId":id,"text":"Возврат","base":""}))
                .await,
            StatusCode::CONFLICT
        );
        let mut edit = f.create();
        edit["op"] = json!("editEvent");
        edit["title"] = json!("Обновлено");
        edit["revision"] = event["revision"].clone();
        edit["year"] = json!(2028);
        edit["eventId"] = id.clone();
        edit["scope"] = json!("future");
        edit["checklist"][0]["id"] = task.clone();
        assert_eq!(f.cmd(edit.clone()).await, StatusCode::NO_CONTENT);
        assert_eq!(f.cmd(edit).await, StatusCode::CONFLICT);
        let event = f.state().await["events"][0].clone();
        assert_eq!(event["checklist"][0]["done"], true);
        assert_eq!(event["note"], "Готово");
        assert_eq!(f.cmd(json!({"op":"deleteEvent","year":2028,"eventId":id,"revision":event["revision"],"scope":"year"})).await,StatusCode::NO_CONTENT);
        assert_eq!(f.state().await["events"], json!([]));
        assert_eq!(
            f.request("/v1/state?year=2032", &f.key, None).await.1["events"][0]["date"],
            "2032-02-29"
        );
    }
    #[tokio::test]
    async fn failed_security_journal_denies_reads_writes_and_admin_success() {
        let f = Fixture::new();
        let dir = f._tmp.path().join("host");
        let log = dir.join(format!(
            "security-{}.jsonl",
            chrono::Utc::now().format("%Y-%m")
        ));
        std::fs::write(&log, b"unfinished synthetic record").unwrap();
        assert_eq!(
            f.request("/v1/state?year=2028", &f.key, None).await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(f.cmd(f.create()).await, StatusCode::SERVICE_UNAVAILABLE);
        let lock = f.vault.lock().unwrap();
        assert!(lock.data().unwrap().calendar.plans.is_empty());
        assert!(lock.revoke(f.credential).is_err());
        assert!(lock.access().unwrap().authorize(&f.key).is_some());
    }

    #[tokio::test]
    async fn rejects_untrusted_authorities_origins_and_oversized_requests() {
        let f = Fixture::new();
        for (host, origin) in [
            ("elsewhere.test", "https://calendar.test"),
            ("calendar.test", "https://elsewhere.test"),
            ("calendar.test", "null"),
        ] {
            let req = Request::builder()
                .uri("/v1/session")
                .header(header::HOST, host)
                .header(header::ORIGIN, origin)
                .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                f.app.clone().oneshot(req).await.unwrap().status(),
                StatusCode::FORBIDDEN
            );
        }
        let huge = json!({"op":"note","text":"x".repeat(150*1024)});
        assert_eq!(f.cmd(huge).await, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(Host::new(f.vault.clone(), "http://calendar.test", None).is_err());
        assert!(Host::new(f.vault.clone(), "https://user@calendar.test", None).is_err());
    }
}
