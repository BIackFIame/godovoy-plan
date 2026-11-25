use std::{
    collections::BTreeSet,
    net::{Ipv4Addr, SocketAddrV4, TcpListener},
    sync::Arc,
    thread,
    time::Duration,
};

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, Query, Request, State, rejection::JsonRejection},
    http::{Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, put},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{Datelike, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tokio::{
    sync::{Semaphore, oneshot},
    task::JoinSet,
};

use crate::{
    Fallible,
    model::{
        Calendar, Category, Event, ExternalSource, Id, MAX_PLANS, Occurrence, PLAN_LIMIT_ERROR,
        Record, Task, YearChange,
    },
    storage::{
        ImportResultKind, LocalState, ProfileSnapshot, ProfileUsage, SharedStore, Store,
        StoreReader, valid_source,
    },
};

mod bridge;
mod request;
pub use request::{ApiKeyKind, ApiMethod, ApiReply, ApiSession, request_secure};

pub use bridge::{
    BRIDGE_RUN_LIMIT, BridgeOutcome, BridgeRun, ImportTransferError, import_secure,
    import_secure_tracked, report_bridge_run,
};

pub const DEFAULT_PORT: u16 = 43857;
pub const BODY_LIMIT: usize = 512 * 1024;
pub use crate::import_report::BATCH_LIMIT;
const MAX_CONNECTIONS: usize = 12;

#[derive(Clone)]
struct ApiState {
    store: SharedStore,
    reader: StoreReader,
    writer: Arc<Semaphore>,
    report_writer: Arc<Semaphore>,
}

impl ApiState {
    fn new(store: SharedStore) -> Self {
        let reader = store.lock().unwrap().reader();
        Self {
            store,
            reader,
            writer: Arc::new(Semaphore::new(1)),
            report_writer: Arc::new(Semaphore::new(1)),
        }
    }

    async fn write<T: Send + 'static>(
        self,
        operation: impl FnOnce(SharedStore) -> ApiResult<T> + Send + 'static,
    ) -> ApiResult<T> {
        let unavailable = || {
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Запись в профиль недоступна.",
            )
        };
        let permit = self
            .writer
            .acquire_owned()
            .await
            .map_err(|_| unavailable())?;
        // The worker owns the permit even if the client disconnects or times out.
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            operation(self.store)
        })
        .await
        .map_err(|_| unavailable())?
    }
}

pub struct ApiServer {
    port: u16,
    stop: Option<oneshot::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl ApiServer {
    pub fn start(store: SharedStore, port: u16) -> Fallible<Self> {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let (stop, shutdown) = oneshot::channel();
        let state = ApiState::new(store);
        let worker = thread::Builder::new()
            .name("calendar-api".into())
            .spawn(move || {
                runtime.block_on(serve(listener, state, shutdown));
            })?;
        Ok(Self {
            port,
            stop: Some(stop),
            worker: Some(worker),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for ApiServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

async fn serve(listener: TcpListener, state: ApiState, mut shutdown: oneshot::Receiver<()>) {
    let Ok(listener) = tokio::net::TcpListener::from_std(listener) else {
        return;
    };
    let routes = router(state.clone());
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            accepted = listener.accept() => match accepted {
                Ok((stream, address)) if address.ip().is_loopback() && connections.len() < MAX_CONNECTIONS => {
                    let state = state.clone();
                    let routes = routes.clone();
                    connections.spawn(async move {
                        let _ = tokio::time::timeout(Duration::from_secs(10), bridge::serve(stream, state, routes)).await;
                    });
                }
                Ok(_) => {}
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }
    connections.shutdown().await;
}

fn router(state: ApiState) -> Router {
    Router::new()
        .route(
            "/v1/health",
            get(|| async { Json(serde_json::json!({ "version": 1, "status": "ok" })) }),
        )
        .route("/v1/events", get(list).post(create))
        .route("/v1/events/{id}", get(event))
        .route("/v1/members", get(members))
        .route("/v1/capacity", get(capacity))
        .route("/v1/imports/{source}", put(import))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(middleware::from_fn_with_state(
            state.reader.clone(),
            authenticate,
        ))
        .with_state(state)
}

pub fn encode_token(key: &[u8; 32]) -> String {
    URL_SAFE_NO_PAD.encode(key)
}

#[derive(Clone, Copy)]
enum Credential {
    Admin([u8; 32]),
    Import([u8; 32]),
}

impl Credential {
    fn require_current(self, state: &LocalState) -> ApiResult<()> {
        let settings = active_settings(state)?;
        let valid = match self {
            Self::Admin(key) => key.ct_eq(&settings.token),
            Self::Import(key) => settings
                .import_token
                .map_or_else(|| 0.into(), |token| key.ct_eq(&token)),
        };
        if bool::from(valid) {
            Ok(())
        } else {
            Err(unauthorized())
        }
    }
}

fn active_settings(state: &LocalState) -> ApiResult<&crate::storage::ApiSettings> {
    state
        .api
        .as_ref()
        .filter(|settings| settings.enabled)
        .ok_or_else(|| error(StatusCode::SERVICE_UNAVAILABLE, "Локальный API выключен."))
}

fn unauthorized() -> ApiError {
    error(
        StatusCode::UNAUTHORIZED,
        "Требуется действующий ключ с правом на эту операцию.",
    )
}

pub fn decode_token(value: &str) -> Fallible<[u8; 32]> {
    if value.len() != 43 {
        return Err("Токен должен содержать 43 символа base64url.".into());
    }
    let mut token = [0; 32];
    if URL_SAFE_NO_PAD.decode_slice(value, &mut token)? != token.len() {
        return Err("Неверная длина токена.".into());
    }
    Ok(token)
}

async fn authenticate(
    State(reader): State<StoreReader>,
    mut request: Request,
    next: Next,
) -> Response {
    let snapshot = reader.snapshot();
    let Some(&credential) = request.extensions().get::<Credential>() else {
        return unauthorized().into_response();
    };
    if matches!(credential, Credential::Import(_))
        && (request.method() != Method::PUT || !request.uri().path().starts_with("/v1/imports/"))
    {
        return unauthorized().into_response();
    }
    if let Err(error) = credential.require_current(&snapshot.state) {
        return error.into_response();
    }
    request.extensions_mut().insert(snapshot);
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}

type ApiError = (StatusCode, Json<serde_json::Value>);
type ApiResult<T> = Result<T, ApiError>;

fn error(status: StatusCode, message: impl Into<String>) -> ApiError {
    (status, Json(serde_json::json!({ "error": message.into() })))
}
fn invalid(message: impl Into<String>) -> ApiError {
    error(StatusCode::UNPROCESSABLE_ENTITY, message)
}
fn body<T>(json: Result<Json<T>, JsonRejection>) -> ApiResult<T> {
    json.map(|Json(value)| value).map_err(|rejection| {
        error(
            rejection.status(),
            "Проверьте JSON, Content-Type и размер запроса.",
        )
    })
}

#[derive(Deserialize)]
struct Year {
    year: Option<i32>,
    offset: Option<usize>,
    limit: Option<usize>,
}
impl Year {
    fn value(&self) -> ApiResult<i32> {
        let year = self.year.unwrap_or_else(|| Local::now().year());
        if (1900..=9999).contains(&year) {
            Ok(year)
        } else {
            Err(invalid("Год должен быть от 1900 до 9999."))
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EventView {
    id: Id,
    year: i32,
    title: String,
    description: String,
    category: Category,
    date: NaiveDate,
    yearly: bool,
    owner_id: Option<Id>,
    employee_id: Option<Id>,
    preparation_days: u16,
    preparation_date: NaiveDate,
    checklist: Vec<TaskView>,
    diary: String,
    source: Option<ExternalSource>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskView {
    id: Id,
    title: String,
    due_date: NaiveDate,
    days_before: u16,
    done: bool,
}

fn view(event: Occurrence<'_>, calendar: &Calendar) -> EventView {
    EventView {
        id: event.id,
        year: event.year,
        title: event.event.title.clone(),
        description: event.event.description.clone(),
        category: event.event.category,
        date: event.date,
        yearly: event.event.yearly,
        owner_id: event.event.owner,
        employee_id: event.event.owner.map(|id| calendar.person_for(id)),
        preparation_days: event.event.preparation_days,
        preparation_date: event.preparation_date(),
        checklist: event
            .event
            .tasks
            .iter()
            .map(|task| TaskView {
                id: task.id,
                title: task.title.clone(),
                due_date: event.due(task),
                days_before: task.days_before,
                done: event.checked(task.id),
            })
            .collect(),
        diary: event.note().to_owned(),
        source: calendar.plans[&event.id].source.clone(),
    }
}

async fn list(
    Extension(snapshot): Extension<ProfileSnapshot>,
    Query(query): Query<Year>,
) -> ApiResult<Json<Vec<EventView>>> {
    let year = query.value()?;
    let calendar = &snapshot
        .state
        .workspace
        .as_ref()
        .ok_or_else(no_workspace)?
        .calendar;
    let max = query.limit.unwrap_or(100);
    if !(1..=200).contains(&max) {
        return Err(invalid("limit должен быть от 1 до 200."));
    }
    Ok(Json(
        calendar
            .year(year)
            .into_iter()
            .skip(query.offset.unwrap_or(0))
            .take(max)
            .map(|event| view(event, calendar))
            .collect(),
    ))
}

async fn event(
    Extension(snapshot): Extension<ProfileSnapshot>,
    Path(id): Path<Id>,
    Query(query): Query<Year>,
) -> ApiResult<Json<EventView>> {
    let year = query.value()?;
    let calendar = &snapshot
        .state
        .workspace
        .as_ref()
        .ok_or_else(no_workspace)?
        .calendar;
    let occurrence = calendar
        .event(id, year)
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Событие не найдено."))?;
    Ok(Json(view(occurrence, calendar)))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Member {
    id: Id,
    employee_id: Id,
    name: String,
    revoked: bool,
}

async fn members(Extension(snapshot): Extension<ProfileSnapshot>) -> ApiResult<Json<Vec<Member>>> {
    let workspace = snapshot.state.workspace.as_ref().ok_or_else(no_workspace)?;
    Ok(Json(
        workspace
            .calendar
            .members
            .iter()
            .map(|(&id, entry)| Member {
                id,
                name: entry.value.clone(),
                revoked: workspace.is_revoked(id),
                employee_id: workspace.calendar.person_for(id),
            })
            .collect(),
    ))
}

async fn capacity(Extension(snapshot): Extension<ProfileSnapshot>) -> Json<ProfileUsage> {
    Json(snapshot.usage)
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventInput {
    pub title: String,
    pub date: NaiveDate,
    pub category: Option<Category>,
    pub description: Option<String>,
    pub yearly: Option<bool>,
    pub owner_id: Option<Id>,
    pub preparation_days: Option<u16>,
    pub checklist: Option<Vec<TaskInput>>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskInput {
    pub key: String,
    pub title: String,
    pub days_before: u16,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportItem {
    pub external_id: String,
    pub event: EventInput,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImportBatch {
    pub events: Vec<ImportItem>,
}

fn build_event(
    input: EventInput,
    id: Id,
    existing: Option<&Event>,
    state: &LocalState,
) -> Fallible<Event> {
    let workspace = state
        .workspace
        .as_ref()
        .ok_or("Сначала подключитесь к пространству.")?;
    let previous_owner = existing.and_then(|event| event.owner);
    let owner = input.owner_id.or(previous_owner);
    workspace.check_assignment(previous_owner, owner)?;
    // Preserve the original owner when importing updates to a historical event.
    let owner = if owner == previous_owner {
        previous_owner
    } else {
        owner.map(|id| workspace.calendar.person_for(id))
    };
    let tasks = if let Some(input) = input.checklist {
        input
            .into_iter()
            .map(|task| {
                if task.key.is_empty() || task.key.len() > 80 {
                    return Err("Ключ пункта должен содержать от 1 до 80 байт.".into());
                }
                Ok(Task {
                    id: Id::new_v5(&id, task.key.as_bytes()),
                    title: task.title.trim().to_owned(),
                    days_before: task.days_before,
                })
            })
            .collect::<Fallible<_>>()?
    } else {
        existing.map_or_else(Vec::new, |event| event.tasks.clone())
    };
    let event = Event {
        title: input.title.trim().to_owned(),
        date: input.date,
        category: input
            .category
            .or_else(|| existing.map(|event| event.category))
            .unwrap_or_default(),
        description: input
            .description
            .unwrap_or_else(|| existing.map_or_else(String::new, |e| e.description.clone())),
        yearly: input
            .yearly
            .or_else(|| existing.map(|e| e.yearly))
            .unwrap_or(false),
        owner,
        preparation_days: input
            .preparation_days
            .or_else(|| existing.map(|e| e.preparation_days))
            .unwrap_or(30),
        tasks,
    };
    event.validate()?;
    Ok(event)
}

fn no_workspace() -> ApiError {
    error(StatusCode::CONFLICT, "Сначала подключитесь к пространству.")
}

async fn create(
    State(api): State<ApiState>,
    Extension(credential): Extension<Credential>,
    input: Result<Json<EventInput>, JsonRejection>,
) -> ApiResult<impl IntoResponse> {
    let input = body(input)?;
    let event = api
        .write(move |store| create_event(&store, input, credential))
        .await?;
    Ok((StatusCode::CREATED, Json(event)))
}

fn create_event(
    store: &SharedStore,
    input: EventInput,
    credential: Credential,
) -> ApiResult<EventView> {
    let id = Id::new_v4();
    let year = input.date.year();
    let mut store = store.lock().unwrap();
    credential.require_current(store.state())?;
    if store
        .state()
        .workspace
        .as_ref()
        .is_some_and(|space| space.calendar.plans.len() >= MAX_PLANS)
    {
        return Err(invalid(PLAN_LIMIT_ERROR));
    }
    store
        .transaction(|state| {
            let event = build_event(input, id, None, state)?;
            let revision = state.revision()?;
            let calendar = &mut state.workspace_mut()?.calendar;
            calendar
                .plans
                .entry(id)
                .or_default()
                .templates
                .insert(year, Record::new(revision, Some(event.into())));
            let event = calendar
                .event(id, year)
                .ok_or("Не удалось прочитать созданное событие.")?;
            Ok(view(event, calendar))
        })
        .map_err(|e| invalid(e.to_string()))
}

async fn import(
    State(api): State<ApiState>,
    Path(source): Path<String>,
    Extension(credential): Extension<Credential>,
    input: Result<Json<ImportBatch>, JsonRejection>,
) -> ApiResult<Json<Vec<ImportResult>>> {
    let batch = body(input)?;
    api.write(move |store| apply_import(&store, &source, batch, credential))
        .await
        .map(Json)
}

fn apply_import(
    store: &SharedStore,
    source: &str,
    batch: ImportBatch,
    credential: Credential,
) -> ApiResult<Vec<ImportResult>> {
    if !valid_source(source) {
        return Err(invalid(
            "Источник должен содержать 1–48 строчных букв ASCII, цифр, дефисов или подчёркиваний.",
        ));
    }
    let mut store = store.lock().unwrap();
    credential.require_current(store.state())?;
    let result = match validate_import(&batch) {
        Ok(()) => commit_import(&mut store, source, batch)
            .map_err(|error| (error, ImportResultKind::CalendarRejected)),
        Err(error) => Err((error, ImportResultKind::InvalidBatch)),
    };
    let identity = store
        .state()
        .workspace
        .as_ref()
        .map(|workspace| (store.state().device, workspace.id));
    let log = store.import_log();
    drop(store);
    if let Some((device, workspace)) = identity {
        let outcome = result
            .as_ref()
            .map(|rows| {
                (
                    rows.len() as u16,
                    rows.iter().filter(|row| row.changed).count() as u16,
                )
            })
            .map_err(|(_, kind)| *kind);
        log.record(device, workspace, source, outcome);
    }
    result.map_err(|(error, _)| error)
}

fn validate_import(batch: &ImportBatch) -> ApiResult<()> {
    if batch.events.len() > BATCH_LIMIT {
        return Err(invalid(format!(
            "За один запрос можно передать до {BATCH_LIMIT} событий."
        )));
    }
    let mut unique = BTreeSet::new();
    if batch.events.iter().any(|item| {
        item.external_id.trim().is_empty()
            || item.external_id.len() > 160
            || item.external_id.chars().any(char::is_control)
            || !unique.insert(&item.external_id)
    }) {
        return Err(invalid(
            "Внешние идентификаторы должны быть уникальными в запросе и содержать от 1 до 160 байт.",
        ));
    }
    Ok(())
}

fn commit_import(
    store: &mut Store,
    source: &str,
    batch: ImportBatch,
) -> ApiResult<Vec<ImportResult>> {
    let (results, updates) =
        prepare_import(store.state(), source, batch).map_err(|error| invalid(error.to_string()))?;
    if !updates.is_empty() {
        store
            .transaction(|state| {
                let revision = state.revision()?;
                let plans = &mut state.workspace_mut()?.calendar.plans;
                for update in updates {
                    let year = update.event.date.year();
                    let plan = plans.entry(update.id).or_default();
                    plan.source = Some(ExternalSource {
                        name: source.to_owned(),
                        external_id: update.external_id,
                    });
                    if let Some(old_year) = update.old_year.filter(|old_year| *old_year != year) {
                        plan.templates.insert(old_year, Record::new(revision, None));
                        if let Some(record) = plan.years.get_mut(&old_year)
                            && matches!(
                                record.change.as_ref().map(|record| &record.value),
                                Some(YearChange::Edit(_))
                            )
                        {
                            record.change = Some(Record::new(revision, YearChange::Cancel));
                        }
                        plan.years.entry(year).or_default().change =
                            Some(Record::new(revision, YearChange::UseTemplate));
                    }
                    plan.templates
                        .insert(year, Record::new(revision, Some(update.event.into())));
                }
                Ok(())
            })
            .map_err(|error| invalid(error.to_string()))?;
    }
    Ok(results)
}

struct ImportUpdate {
    id: Id,
    external_id: String,
    event: Event,
    old_year: Option<i32>,
}

fn prepare_import(
    state: &LocalState,
    source: &str,
    batch: ImportBatch,
) -> Fallible<(Vec<ImportResult>, Vec<ImportUpdate>)> {
    let space = state
        .workspace
        .as_ref()
        .ok_or("Сначала подключитесь к пространству.")?;
    let mut results = Vec::with_capacity(batch.events.len());
    let mut updates = Vec::new();
    let mut remaining = MAX_PLANS.saturating_sub(space.calendar.plans.len());
    for item in batch.events {
        let id = Id::new_v5(
            &space.id,
            format!("{source}\0{}", item.external_id).as_bytes(),
        );
        let plan = space.calendar.plans.get(&id);
        if plan.is_none() {
            remaining = remaining.checked_sub(1).ok_or(PLAN_LIMIT_ERROR)?;
        }
        let previous = plan.and_then(|plan| plan.import_template());
        let event = build_event(item.event, id, previous.map(|(_, event)| event), state)?;
        if event.yearly && plan.is_some_and(|plan| plan.imported_single_has_other_dates()) {
            return Err("Для ежегодного повтора перенесённого события используйте новый внешний идентификатор.".into());
        }
        let year = event.date.year();
        let changed = previous != Some((year, &event));
        if previous
            .is_some_and(|(old, old_event)| old != year && (old_event.yearly || event.yearly))
        {
            return Err(
                "Перенос ежегодного шаблона в другой год требует нового внешнего идентификатора."
                    .into(),
            );
        }
        results.push(ImportResult {
            id,
            external_id: item.external_id.clone(),
            year,
            changed,
        });
        if changed {
            updates.push(ImportUpdate {
                id,
                external_id: item.external_id,
                event,
                old_year: previous.map(|(year, _)| year),
            });
        }
    }
    Ok((results, updates))
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ImportResult {
    id: Id,
    external_id: String,
    year: i32,
    changed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{ApiSettings, Store};

    #[tokio::test]
    async fn mutations_require_current_credentials() {
        let profile = tempfile::tempdir().unwrap();
        let store = Store::open(&profile.path().join("profile")).unwrap();
        store
            .lock()
            .unwrap()
            .transaction(|state| {
                state.create_workspace("Тест".into(), "Анна".into())?;
                state.api = Some(ApiSettings {
                    token: [11; 32],
                    import_token: Some([22; 32]),
                    port: 0,
                    enabled: true,
                });
                Ok(())
            })
            .unwrap();
        let rejected_create = create(
            State(ApiState::new(store.clone())),
            Extension(Credential::Admin([33; 32])),
            Ok(Json(
                serde_json::from_value(serde_json::json!({
                    "title": "Встреча",
                    "date": "2026-12-15",
                }))
                .unwrap(),
            )),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(rejected_create.0, StatusCode::UNAUTHORIZED);
        let rejected_import = apply_import(
            &store,
            "test",
            ImportBatch { events: Vec::new() },
            Credential::Import([44; 32]),
        )
        .err()
        .unwrap();
        assert_eq!(rejected_import.0, StatusCode::UNAUTHORIZED);
        assert!(
            store
                .lock()
                .unwrap()
                .state()
                .workspace
                .as_ref()
                .unwrap()
                .calendar
                .plans
                .is_empty()
        );
    }
}
