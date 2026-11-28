//! Temporary OS-operator console. Never mount its router on either calendar listener.
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use crate::http::{add_common_security_headers, problem};
use godovoy_plan::{
    Fallible,
    model::Id,
    storage::{
        online::{
            GrantPermissions, GrantPurpose, LockedVault, OnlineVault, OperatorInput,
            OperatorRecord, ProtectionConflict, ProtectionSettings,
        },
        random_bytes,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    net::Ipv4Addr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::{net::TcpListener, sync::Semaphore};
use zeroize::Zeroizing;

const LIFETIME: Duration = Duration::from_secs(15 * 60);
#[derive(Clone)]
struct Console {
    vault: OnlineVault,
    authority: String,
    origin: String,
    secret: Arc<Zeroizing<String>>,
    expires: Instant,
    permits: Arc<Semaphore>,
    backup_root: Option<PathBuf>,
    backup: Arc<Mutex<BackupJob>>,
}
async fn guard(State(c): State<Console>, req: Request, next: Next) -> Response {
    let h = req.headers();
    let unsafe_origin = h
        .get(header::ORIGIN)
        .is_some_and(|v| v.as_bytes() != c.origin.as_bytes());
    let cross = h
        .get("sec-fetch-site")
        .is_some_and(|v| v.as_bytes() == b"cross-site");
    let bad_target = req.uri().query().is_some()
        || req
            .uri()
            .authority()
            .is_some_and(|a| a.as_str() != c.authority);
    let is_write = req.method() != Method::GET && req.method() != Method::HEAD;
    let mut res = if h
        .get(header::HOST)
        .is_none_or(|v| v.as_bytes() != c.authority.as_bytes())
        || unsafe_origin
        || cross
        || bad_target
        || is_write && h.get(header::ORIGIN).is_none()
    {
        problem(StatusCode::FORBIDDEN, "Недопустимый адрес запроса.")
    } else if Instant::now() >= c.expires {
        problem(StatusCode::UNAUTHORIZED, "Срок работы консоли истёк.")
    } else if req.uri().path().starts_with("/v1/") && !authorized(h, &c.secret) {
        problem(
            StatusCode::UNAUTHORIZED,
            "Введите временный ключ из терминала.",
        )
    } else {
        next.run(req).await
    };
    let h = res.headers_mut();
    for (name, value) in [
        ("cache-control", "no-store, private, max-age=0"),
        (
            "content-security-policy",
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
        ),
        ("cross-origin-opener-policy", "same-origin"),
    ] {
        h.insert(name, HeaderValue::from_static(value));
    }
    add_common_security_headers(h);
    res
}
fn authorized(headers: &HeaderMap, secret: &str) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|s| bool::from(s.as_bytes().ct_eq(secret.as_bytes())))
}
async fn locked(
    c: Console,
    action: impl FnOnce(&LockedVault<'_>) -> Fallible<Value> + Send + 'static,
) -> Response {
    let Ok(permit) = c.permits.clone().try_acquire_owned() else {
        return problem(StatusCode::SERVICE_UNAVAILABLE, "Консоль занята.");
    };
    let result = tokio::task::spawn_blocking(move || -> Fallible<Value> {
        let _permit = permit;
        let lock = c.vault.lock()?;
        if Instant::now() >= c.expires {
            return Err("Срок работы консоли истёк.".into());
        }
        action(&lock)
    })
    .await;
    match result {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(e)) if e.is::<ProtectionConflict>() => {
            problem(StatusCode::CONFLICT, "Данные изменились. Обновите форму.")
        }
        Ok(Err(e)) if e.is::<OperatorInput>() => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Проверьте идентификатор, длину и содержимое полей.",
        ),
        _ => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "Операция недоступна. Проверьте состояние хранилища, журнала и резервного режима.",
        ),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingChange {
    revision: u64,
    settings: ProtectionSettings,
}
async fn settings(State(c): State<Console>) -> Response {
    locked(c, |lock| Ok(serde_json::to_value(lock.protection()?)?)).await
}
async fn apply_settings(State(c): State<Console>, Json(change): Json<SettingChange>) -> Response {
    locked(c, move |lock| {
        Ok(serde_json::to_value(
            lock.set_protection(change.revision, change.settings)?,
        )?)
    })
    .await
}
async fn operator(State(c): State<Console>) -> Response {
    locked(c, |lock| Ok(serde_json::to_value(lock.operator_record()?)?)).await
}
async fn apply_operator(State(c): State<Console>, Json(input): Json<OperatorRecord>) -> Response {
    locked(c, move |lock| {
        Ok(serde_json::to_value(lock.save_operator_record(input)?)?)
    })
    .await
}
async fn report(State(c): State<Console>) -> Response {
    locked(c, |lock| {
        lock.audit_security("operator_report_requested", None, None, None)?;
        lock.protection_report()
    })
    .await
}
async fn employees(State(c): State<Console>) -> Response {
    locked(c, |lock| {
        let directory = lock.employee_directory()?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let directory = {
            let mut directory = directory;
            directory["revokeSpace"] = json!(match lock.revoke_space_ready() {
                Ok(true) => "ready",
                Ok(false) => "absent",
                Err(_) => "unavailable",
            });
            directory
        };
        Ok(directory)
    }).await
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn prepare_revoke_space(State(c): State<Console>) -> Response {
    locked(c, |lock| {
        lock.prepare_revoke_space()?;
        Ok(json!({"revokeSpace": "ready"}))
    }).await
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct McpGrantRequest {
    allow_write: bool,
    allow_remote: bool,
}
async fn issue_mcp(
    State(c): State<Console>,
    Path(employee): Path<Id>,
    Json(request): Json<McpGrantRequest>,
) -> Response {
    locked(c, move |lock| {
        let acl = lock.access()?;
        let name = acl.employees.get(&employee).ok_or(OperatorInput)?;
        let (credential, _, token) = lock.grant_mcp(
            name,
            employee,
            GrantPermissions {
                read_only: !request.allow_write,
                allow_remote: request.allow_remote,
            },
        )?;
        Ok(json!({"id":credential, "token":token}))
    })
    .await
}
async fn revoke_mcp(
    State(c): State<Console>,
    Path((employee, credential)): Path<(Id, Id)>,
    Json(_): Json<Confirm>,
) -> Response {
    locked(c, move |lock| {
        let acl = lock.access()?;
        if !acl
            .grants
            .get(&credential)
            .is_some_and(|g| g.employee == employee && g.purpose == GrantPurpose::Mcp)
        {
            return Err(OperatorInput.into());
        }
        lock.revoke(credential)?;
        Ok(json!({"revoked":1}))
    })
    .await
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NameChange {
    previous_name: String,
    name: String,
}
async fn edit_name(
    State(c): State<Console>,
    Path(id): Path<Id>,
    Json(change): Json<NameChange>,
) -> Response {
    locked(c, move |lock| {
        lock.correct_employee_name(id, &change.previous_name, &change.name)?;
        Ok(json!({"updated":true}))
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Confirm {}
async fn revoke(State(c): State<Console>, Path(id): Path<Id>, Json(_): Json<Confirm>) -> Response {
    locked(c, move |lock| {
        Ok(json!({"revoked":lock.revoke_employee_keys(id)?}))
    })
    .await
}
#[derive(Clone, Copy, Default, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum JobStatus {
    #[default]
    Idle,
    Running,
    Verified,
    Failed,
}
#[derive(Default)]
struct BackupJob {
    status: JobStatus,
    finished_at: Option<String>,
}
impl Console {
    fn backup_response(&self) -> Response {
        match self.backup.lock() {
            Ok(job)=>Json(json!({"available":self.backup_root.is_some(),"status":job.status,"finishedAt":job.finished_at})).into_response(),
            Err(_)=>problem(StatusCode::SERVICE_UNAVAILABLE,"Проверка статуса временно недоступна."),
        }
    }
}
async fn backup_status(State(c): State<Console>) -> Response {
    c.backup_response()
}
async fn start_backup(State(c): State<Console>, Json(_): Json<Confirm>) -> Response {
    let Some(root) = c.backup_root.clone() else {
        return problem(
            StatusCode::FORBIDDEN,
            "Каталог резервных копий не выбран в консоли ОС.",
        );
    };
    {
        let Ok(mut job) = c.backup.lock() else {
            return problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "Проверка статуса временно недоступна.",
            );
        };
        if job.status == JobStatus::Running {
            return problem(StatusCode::CONFLICT, "Копия уже создаётся.");
        }
        job.status = JobStatus::Running;
        job.finished_at = None;
    }
    let dest = root.join(format!(
        "calendar-{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
        Id::new_v4()
    ));
    let job = c.backup.clone();
    tokio::spawn(async move {
        let result =
            tokio::task::spawn_blocking(move || c.vault.create_verified_backup(&dest)).await;
        if let Ok(mut state) = job.lock() {
            state.status = if matches!(result, Ok(Ok(_))) {
                JobStatus::Verified
            } else {
                JobStatus::Failed
            };
            state.finished_at = Some(chrono::Utc::now().to_rfc3339());
        }
    });
    (
        StatusCode::ACCEPTED,
        Json(json!({"available":true,"status":"running","finishedAt":null})),
    )
        .into_response()
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EmployeeScope {
    assigned_only: bool,
}
async fn update_scope(
    State(c): State<Console>,
    Path(id): Path<Id>,
    Json(scope): Json<EmployeeScope>,
) -> Response {
    locked(c, move |lock| {
        lock.set_employee_scope(id, scope.assigned_only)?;
        Ok(json!({"assignedOnly":scope.assigned_only}))
    })
    .await
}
fn router(c: Console) -> Router {
    let routes = Router::new();
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let routes = routes.route("/v1/revoke-space", post(prepare_revoke_space));
    routes
        .route(
            "/",
            get(|request: Request| async move { crate::assets::ADMIN.serve(request.headers()) }),
        )
        .route(
            "/admin.js",
            get(|request: Request| async move { crate::assets::ADMIN_JS.serve(request.headers()) }),
        )
        .route(
            "/admin.css",
            get(|request: Request| async move { crate::assets::ADMIN_CSS.serve(request.headers()) }),
        )
        .route("/v1/settings", get(settings).put(apply_settings))
        .route("/v1/operator", get(operator).put(apply_operator))
        .route("/v1/report", get(report))
        .route("/v1/backup", get(backup_status).post(start_backup))
        .route("/v1/employees", get(employees))
        .route("/v1/employees/{id}/mcp", post(issue_mcp))
        .route(
            "/v1/employees/{id}/mcp/{credential}/revoke",
            post(revoke_mcp),
        )
        .route("/v1/employees/{id}/name", put(edit_name))
        .route("/v1/employees/{id}/scope", put(update_scope))
        .route("/v1/employees/{id}/revoke", post(revoke))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(middleware::from_fn_with_state(c.clone(), guard))
        .with_state(c)
}
pub(super) async fn serve(vault: OnlineVault, backup_root: Option<PathBuf>) -> Fallible<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let authority = listener.local_addr()?.to_string();
    let origin = format!("http://{authority}");
    let random = Zeroizing::new(random_bytes::<32>()?);
    let secret = Arc::new(Zeroizing::new(URL_SAFE_NO_PAD.encode(*random)));
    vault
        .lock()?
        .audit_security("operator_console_opened", None, None, None)?;
    println!(
        "Локальная консоль ИТ (15 минут): {origin}\nВременный ключ консоли:\n{}",
        secret.as_str()
    );
    let c = Console {
        vault,
        authority,
        origin,
        secret,
        backup_root,
        backup: Default::default(),
        expires: Instant::now() + LIFETIME,
        permits: Arc::new(Semaphore::new(4)),
    };
    match tokio::time::timeout(LIFETIME, super::host::serve_listener(listener, router(c))).await {
        Ok(result) => result,
        Err(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use godovoy_plan::model::Calendar;
    use tempfile::TempDir;
    use tower::ServiceExt;

    struct Fixture {
        _dir: TempDir,
        console: Console,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = TempDir::new().unwrap();
            let vault = OnlineVault::init(
                &dir.path().join("vault"),
                "Календарь".into(),
                Calendar::default(),
                Default::default(),
            )
            .unwrap();
            Self {
                _dir: dir,
                console: Console {
                    vault,
                    authority: "127.0.0.1:45880".into(),
                    origin: "http://127.0.0.1:45880".into(),
                    secret: Arc::new(Zeroizing::new("test-admin-secret".into())),
                    expires: Instant::now() + LIFETIME,
                    permits: Arc::new(Semaphore::new(4)),
                    backup_root: None,
                    backup: Default::default(),
                },
            }
        }
        async fn request(
            &self,
            method: Method,
            path: &str,
            token: &str,
            origin: Option<&str>,
            body: Value,
        ) -> (StatusCode, Value) {
            let mut r = Request::builder()
                .method(method)
                .uri(path)
                .header(header::HOST, &self.console.authority)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {token}"));
            if let Some(origin) = origin {
                r = r.header(header::ORIGIN, origin);
            }
            let response = router(self.console.clone())
                .oneshot(r.body(Body::from(body.to_string())).unwrap())
                .await
                .unwrap();
            assert!(
                response
                    .headers()
                    .get(header::CACHE_CONTROL)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .contains("no-store")
            );
            let code = response.status();
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
        }
    }
    #[tokio::test]
    async fn assistant_keys_are_issued_only_by_admin_and_revoked_individually() {
        let f = Fixture::new();
        let admin = "test-admin-secret";
        let origin = Some(f.console.origin.as_str());
        let ((parent, anna, personal), (_, boris, _)) = {
            let lock = f.console.vault.lock().unwrap();
            (
                lock.grant("Анна", None, false).unwrap(),
                lock.grant("Борис", None, false).unwrap(),
            )
        };
        let issue = format!("/v1/employees/{anna}/mcp");
        let request = json!({"allowWrite":false,"allowRemote":false});
        assert_eq!(
            f.request(Method::POST, &issue, &personal, origin, request.clone())
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            f.request(Method::POST, &issue, admin, None, request.clone())
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        let (code, issued) = f
            .request(Method::POST, &issue, admin, origin, request)
            .await;
        assert_eq!(code, StatusCode::OK);
        let credential = issued["id"].as_str().unwrap();
        let token = issued["token"].as_str().unwrap();
        let (_, directory) = f
            .request(Method::GET, "/v1/employees", admin, origin, Value::Null)
            .await;
        assert!(!directory.to_string().contains(token));
        let person = directory["employees"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == anna.to_string())
            .unwrap();
        assert_eq!(person["mcpKeys"][0]["id"], credential);
        assert_eq!(person["mcpKeys"][0]["readOnly"], true);
        for path in [
            format!("/v1/employees/{anna}/mcp/{parent}/revoke"),
            format!("/v1/employees/{boris}/mcp/{credential}/revoke"),
        ] {
            assert_eq!(
                f.request(Method::POST, &path, admin, origin, json!({}))
                    .await
                    .0,
                StatusCode::UNPROCESSABLE_ENTITY
            );
        }
        let revoke = format!("/v1/employees/{anna}/mcp/{credential}/revoke");
        assert_eq!(
            f.request(Method::POST, &revoke, admin, origin, json!({}))
                .await
                .0,
            StatusCode::OK
        );
        let acl = f.console.vault.lock().unwrap().access().unwrap();
        assert!(acl.authorize_for(token, GrantPurpose::Mcp).is_none());
        assert!(acl.authorize(&personal).is_some());
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn preparing_disk_space_requires_the_local_operator_and_its_origin() {
        let fixture = Fixture::new();
        let origin = Some(fixture.console.origin.as_str());
        let admin = fixture.console.secret.as_str();
        let path = "/v1/revoke-space";
        let (_, _, employee) = fixture.console.vault.lock().unwrap()
            .grant("Сотрудник", None, false).unwrap();

        for (token, request_origin, status) in [
            (employee.as_str(), origin, StatusCode::UNAUTHORIZED),
            (admin, None, StatusCode::FORBIDDEN),
            (admin, Some("https://other.invalid"), StatusCode::FORBIDDEN),
        ] {
            assert_eq!(fixture.request(Method::POST, path, token, request_origin, Value::Null).await.0, status);
            assert!(!fixture.console.vault.lock().unwrap().revoke_space_ready().unwrap());
        }
        let result = fixture.request(Method::POST, path, admin, origin, Value::Null).await;
        assert_eq!(result, (StatusCode::OK, json!({"revokeSpace": "ready"})));
        let directory = fixture.request(Method::GET, "/v1/employees", admin, None, Value::Null).await;
        assert_eq!(directory.1["revokeSpace"], "ready");
    }

    #[tokio::test]
    async fn admin_secrets_are_separate_and_writes_require_local_origin() {
        let mut f = Fixture::new();
        let (_, _, employee_token) = f
            .console
            .vault
            .lock()
            .unwrap()
            .grant("Борис", None, false)
            .unwrap();
        for path in [
            "/v1/settings",
            "/v1/employees",
            "/v1/report",
            "/v1/operator",
            "/v1/backup",
        ] {
            assert_eq!(
                f.request(Method::GET, path, &employee_token, None, Value::Null)
                    .await
                    .0,
                StatusCode::UNAUTHORIZED
            );
        }
        let change = json!({"revision":0,"settings":{"russia":true,"belarus":true,"allowRemote":false,"allowFreeText":false}});
        let token = "test-admin-secret";
        for origin in [None, Some("https://unrelated.invalid")] {
            assert_eq!(
                f.request(Method::PUT, "/v1/settings", token, origin, change.clone())
                    .await
                    .0,
                StatusCode::FORBIDDEN
            );
        }
        assert_eq!(
            f.request(
                Method::PUT,
                "/v1/settings",
                token,
                Some(&f.console.origin),
                change.clone()
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            f.request(
                Method::PUT,
                "/v1/settings",
                token,
                Some(&f.console.origin),
                change
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        let (status, report) = f
            .request(Method::GET, "/v1/report", token, None, Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !report.to_string().contains("Борис") && !report.to_string().contains(&employee_token)
        );
        f.console.expires = Instant::now();
        assert_eq!(
            f.request(Method::GET, "/v1/report", token, None, Value::Null)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    #[tokio::test]
    async fn corrects_live_registry_and_revokes_every_key_for_one_employee() {
        let f = Fixture::new();
        let token = "test-admin-secret";
        let origin = Some(f.console.origin.as_str());
        let (_, id, first) = f
            .console
            .vault
            .lock()
            .unwrap()
            .grant("Аня", None, false)
            .unwrap();
        let (_, _, second) = f
            .console
            .vault
            .lock()
            .unwrap()
            .grant("Аня", Some(id), true)
            .unwrap();
        let (_, _, other) = f
            .console
            .vault
            .lock()
            .unwrap()
            .grant("Ольга", None, false)
            .unwrap();
        let path = format!("/v1/employees/{id}/name");
        let change = json!({"previousName":"Аня","name":"Анна"});
        assert_eq!(
            f.request(Method::PUT, &path, token, origin, change.clone())
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            f.request(Method::PUT, &path, token, origin, change).await.0,
            StatusCode::CONFLICT
        );
        let path = format!("/v1/employees/{id}/revoke");
        let (status, v) = f
            .request(Method::POST, &path, token, origin, json!({}))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["revoked"], 2);
        let access = f.console.vault.lock().unwrap().access().unwrap();
        assert_eq!(access.employees[&id], "Анна");
        assert!(
            access.authorize(&first).is_none()
                && access.authorize(&second).is_none()
                && access.authorize(&other).is_some()
        );
    }
}
