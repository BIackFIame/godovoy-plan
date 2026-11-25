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
    storage::online::{Access, LockedVault, OnlineVault, record},
};
use hyper::server::conn::http1;
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use serde::Deserialize;
use serde_json::json;
use std::{net::Ipv4Addr, sync::Arc, time::Duration};
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
    operations: Arc<Semaphore>,
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
            operations: Arc::new(Semaphore::new(16)),
        })
    }
}
fn problem(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error":message}))).into_response()
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
async fn with_auth<F>(host: Host, headers: HeaderMap, write: bool, action: F) -> Response
where
    F: FnOnce(&LockedVault<'_>, &Access, Id, Id, bool) -> Response + Send + 'static,
{
    let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|v| v.len() == 80)
        .map(|value| zeroize::Zeroizing::new(value.to_owned()))
    else {
        return problem(
            StatusCode::UNAUTHORIZED,
            "Для доступа нужен действующий ключ.",
        );
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
        let Ok(acl) = lock.access() else {
            return unavailable();
        };
        let Some((id, grant)) = acl.authorize(&token) else {
            if let Some((prefix, secret)) = token.split_once('.')
                && let Ok(id) = Id::parse_str(prefix)
                && secret.len() == 43
                && secret
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
                && lock
                    .audit_security(
                        "authorization_denied",
                        acl.grants.contains_key(&id).then_some(id),
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
        if host.entry == EntryPoint::Remote && !grant.allow_remote {
            if lock
                .audit_security("remote_access_denied", Some(id), Some(grant.employee), None)
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
        if write && grant.read_only {
            if lock
                .audit_security("write_denied", Some(id), Some(grant.employee), None)
                .is_err()
            {
                return unavailable();
            }
            return problem(StatusCode::FORBIDDEN, "Разрешено только чтение.");
        };
        action(&lock, &acl, id, grant.employee, !grant.read_only)
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
        false,
        move |lock, acl, id, employee, edit| match lock.data() {
            Ok(data) => {
                if lock
                    .audit_security("state_requested", Some(id), Some(employee), None)
                    .is_err()
                {
                    return unavailable();
                }
                Json(actions::state(&data, acl, query.year, employee, edit)).into_response()
            }
            Err(_) => unavailable(),
        },
    )
    .await
}
async fn session(State(host): State<Host>, headers: HeaderMap) -> Response {
    with_auth(host, headers, false, |_, _, _, _, _| {
        StatusCode::NO_CONTENT.into_response()
    })
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
        true,
        move |lock, acl, credential, employee, _| {
            let Ok(cmd) = serde_json::from_slice::<Command>(&body) else {
                return problem(StatusCode::BAD_REQUEST, "Недопустимый формат команды.");
            };
            let Ok(mut data) = lock.data() else {
                return unavailable();
            };
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
                    match lock.save_data(&data) {
                        Ok(()) => StatusCode::NO_CONTENT.into_response(),
                        Err(_) => unavailable(),
                    }
                }
            }
        },
    )
    .await
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
) -> Fallible<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    let addr = listener.local_addr()?;
    let origin = origin.unwrap_or_else(|| format!("http://{addr}"));
    let host = Host::new(vault, &origin, entry)?;
    let scope = match host.entry {
        EntryPoint::Office => "офисный",
        EntryPoint::Remote => "внешний",
    };
    let app = router(host);
    eprintln!("Сервер слушает {addr}. Вход {scope}. Адрес клиента {origin}");
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
