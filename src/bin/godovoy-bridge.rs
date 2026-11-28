#![forbid(unsafe_code)]

use std::{
    env,
    ffi::OsString,
    fmt,
    fs::File,
    io::{self, BufReader, Read, Write},
    process::ExitCode,
    thread,
    time::{Duration, Instant},
};

use axum::http::Uri;
use chrono::{Datelike, Local, NaiveDate};
use godovoy_plan::{
    Fallible,
    api::{
        BATCH_LIMIT, BODY_LIMIT, BRIDGE_RUN_LIMIT, BridgeOutcome, BridgeRun, DEFAULT_PORT,
        EventInput, ImportBatch, ImportItem, ImportTransferError, decode_token,
        import_secure_tracked, report_bridge_run,
    },
    model::Category,
};
use serde::Deserialize;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

const MAPPING_LIMIT: u64 = 64 * 1024;
const MAPPING_TOO_LARGE: &str = "Файл сопоставления превышает 64 КиБ.";

enum Format {
    Json,
    Bitrix,
}

struct Config {
    source_url: String,
    source: String,
    api_token: [u8; 32],
    source_token: Option<String>,
    api_port: u16,
    format: Format,
    mapping: Option<Mapping>,
    every: Option<Duration>,
    bitrix_period: BitrixPeriod,
}

enum BitrixPeriod {
    CurrentYear,
    Fixed { from: NaiveDate, to: NaiveDate },
}

impl BitrixPeriod {
    fn from_args(from: Option<NaiveDate>, to: Option<NaiveDate>, year: i32) -> Fallible<Self> {
        if from.is_none() && to.is_none() {
            return Ok(Self::CurrentYear);
        }
        let (year_start, year_end) = Self::CurrentYear.bounds(year);
        let from = from.unwrap_or(year_start);
        let to = to.unwrap_or(year_end);
        if from > to {
            return Err("Начало периода должно быть раньше конца.".into());
        }
        Ok(Self::Fixed { from, to })
    }

    fn bounds(&self, year: i32) -> (NaiveDate, NaiveDate) {
        match *self {
            Self::CurrentYear => (
                NaiveDate::from_ymd_opt(year, 1, 1).unwrap(),
                NaiveDate::from_ymd_opt(year, 12, 31).unwrap(),
            ),
            Self::Fixed { from, to } => (from, to),
        }
    }

    fn request_body(&self, today: NaiveDate) -> serde_json::Value {
        let (from, to) = self.bounds(today.year());
        serde_json::json!({ "type": "company_calendar", "ownerId": 0, "from": from, "to": to })
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", godovoy_plan::terminal_message(&error.to_string()));
            ExitCode::FAILURE
        }
    }
}

fn next_text(args: &mut impl Iterator<Item = OsString>, missing: &'static str) -> Fallible<String> {
    args.next()
        .ok_or(missing)?
        .into_string()
        .map_err(|_| "Значение аргумента должно быть текстом.".into())
}

fn single_request(mut args: impl Iterator<Item = OsString>) -> Fallible<()> {
    use godovoy_plan::api::{ApiKeyKind, ApiMethod, request_secure};
    let method = match next_text(&mut args, "Укажите метод запроса.")?.as_str() {
        "GET" => ApiMethod::Get,
        "POST" => ApiMethod::Post,
        "PUT" => ApiMethod::Put,
        _ => return Err("Поддерживаются GET, POST и PUT.".into()),
    };
    let path = next_text(&mut args, "Укажите путь /v1/…")?;
    let mut port = DEFAULT_PORT;
    let mut kind = ApiKeyKind::Main;
    while let Some(option) = args.next() {
        match option.to_str() {
            Some("--import-key") => kind = ApiKeyKind::Import,
            Some("--api-port") => port = next_text(&mut args, "Укажите порт.")?.parse()?,
            _ => return Err("Неизвестный аргумент запроса.".into()),
        }
    }
    let key =
        decode_token(&env::var("GODOVOY_API_TOKEN").map_err(|_| "Задайте GODOVOY_API_TOKEN.")?)?;
    let mut body = Vec::new();
    if method != ApiMethod::Get {
        std::io::stdin()
            .lock()
            .take(BODY_LIMIT as u64 + 1)
            .read_to_end(&mut body)?;
        if body.is_empty() || body.len() > BODY_LIMIT {
            return Err("Ожидается JSON до 512 КиБ на stdin.".into());
        }
    }
    let reply = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(request_secure(port, &key, kind, method, &path, &body))?;
    use std::io::Write;
    std::io::stdout().lock().write_all(&reply.body)?;
    if !(200..300).contains(&reply.status) {
        return Err(format!("API вернул код {}.", reply.status).into());
    }
    Ok(())
}

fn run() -> Fallible<()> {
    if std::env::args_os()
        .skip(1)
        .eq([std::ffi::OsStr::new("--version")])
    {
        println!("godovoy-bridge {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let mut arguments = env::args_os().skip(1);
    if arguments.next().is_some_and(|s| s == "--request") {
        return single_request(arguments);
    }
    let mut args = env::args_os().skip(1);
    let mut port = DEFAULT_PORT;
    let mut source = None;
    let mut format = Format::Json;
    let mut mapping = None;
    let mut every = None;
    let mut from = None;
    let mut to = None;
    while let Some(arg) = args.next() {
        match arg.to_str().ok_or("Неизвестная кодировка аргумента.")? {
            "--source" => source = Some(next_text(&mut args, "Укажите источник.")?),
            "--api-port" => port = next_text(&mut args, "Укажите порт.")?.parse()?,
            "--format" => match next_text(&mut args, "Укажите формат.")?.as_str() {
                "json" => format = Format::Json,
                "bitrix-calendar" => format = Format::Bitrix,
                _ => return Err("Формат должен быть json или bitrix-calendar.".into()),
            },
            "--mapping" => {
                let file = File::open(args.next().ok_or("Укажите файл сопоставления полей.")?)?;
                if file.metadata()?.len() > MAPPING_LIMIT {
                    return Err(MAPPING_TOO_LARGE.into());
                }
                mapping = Some(read_mapping(file)?);
            }
            "--every-seconds" => {
                let seconds = next_text(&mut args, "Укажите интервал.")?.parse::<u64>()?;
                if !(60..=86400).contains(&seconds) {
                    return Err("Интервал должен быть от 60 до 86400 секунд.".into());
                }
                every = Some(Duration::from_secs(seconds));
            }
            "--from" => from = Some(next_text(&mut args, "Укажите дату начала.")?.parse()?),
            "--to" => to = Some(next_text(&mut args, "Укажите дату конца.")?.parse()?),
            "--help" | "-h" => {
                println!(
                    "godovoy-bridge --request GET|POST|PUT /v1/... [--import-key] [--api-port <port>]\n               POST/PUT: JSON на stdin; ответ на stdout.\n\ngodovoy-bridge --source <name> [--format json|bitrix-calendar] [--api-port <port>] [--mapping <file.json>]\n               [--every-seconds <60..86400>] [--from <YYYY-MM-DD>] [--to <YYYY-MM-DD>]\n\nВ окружении: GODOVOY_API_TOKEN, GODOVOY_SOURCE_URL, опционально GODOVOY_SOURCE_TOKEN"
                );
                return Ok(());
            }
            _ => return Err("Неизвестный аргумент. Используйте --help.".into()),
        }
    }
    let source = source
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 48
                && value.bytes().all(|c| {
                    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'-' | b'_')
                })
        })
        .ok_or("Укажите --source: до 48 строчных ASCII-букв, цифр, дефисов или подчёркиваний.")?;
    let source_url = env::var("GODOVOY_SOURCE_URL").map_err(|_| "Укажите GODOVOY_SOURCE_URL.")?;
    validate_source(&source_url)?;
    let bitrix_period = BitrixPeriod::from_args(from, to, Local::now().year())?;
    if matches!(format, Format::Bitrix) && mapping.is_some() {
        return Err("Сопоставление полей доступно в режиме json.".into());
    }
    let config = Config {
        source_url,
        source,
        api_token: decode_token(
            &env::var("GODOVOY_API_TOKEN").map_err(|_| "Укажите GODOVOY_API_TOKEN.")?,
        )?,
        source_token: env::var("GODOVOY_SOURCE_TOKEN").ok(),
        api_port: port,
        format,
        mapping,
        every,
        bitrix_period,
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(20)))
        .tls_config(
            TlsConfig::builder()
                .provider(TlsProvider::NativeTls)
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .into();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    loop {
        let (report, result) = pull(&agent, &runtime, &config);
        if config.every.is_some()
            && let Err(error) = &result
        {
            eprintln!("{}", godovoy_plan::terminal_message(&error.to_string()));
        }
        let report_started = Instant::now();
        let _ = runtime.block_on(report_bridge_run(
            config.api_port,
            &config.api_token,
            &config.source,
            report,
        ));
        if let Some(interval) = config.every {
            thread::sleep(interval.saturating_sub(report_started.elapsed()));
        } else {
            return result.map_err(Into::into);
        }
    }
}

fn read_mapping(reader: impl Read) -> Fallible<Mapping> {
    let mut bounded = reader.take(MAPPING_LIMIT + 1);
    let parsed = serde_json::from_reader(BufReader::new(&mut bounded));
    if bounded.limit() == 0 {
        return Err(MAPPING_TOO_LARGE.into());
    }
    parsed.map_err(|error| {
        if error.is_io() {
            "Не удалось прочитать файл сопоставления.".into()
        } else {
            format!(
                "Проверьте файл сопоставления. Строка {}, столбец {}.",
                error.line(),
                error.column()
            )
            .into()
        }
    })
}

fn validate_source(url: &str) -> Fallible<()> {
    let uri: Uri = url.parse().map_err(|_| "Проверьте адрес источника.")?;
    if uri.authority().is_some_and(|a| a.as_str().contains('@')) {
        return Err("Используйте отдельный токен для источника.".into());
    }
    let local = matches!(uri.host(), Some("127.0.0.1" | "[::1]"));
    if uri.host().is_some()
        && (uri.scheme_str() == Some("https") || (local && uri.scheme_str() == Some("http")))
    {
        Ok(())
    } else {
        Err("Источнику нужен HTTPS. HTTP разрешён для тестового сервера на loopback.".into())
    }
}

#[derive(Debug)]
enum PollFailure {
    InvalidRequest,
    HttpStatus(u16),
    RedirectRefused,
    SourceUnavailable,
    InvalidSource(&'static str),
    MappingField { row: usize, field: &'static str },
    Transfer(ImportTransferError),
}

impl PollFailure {
    fn from_source_error(error: ureq::Error) -> Self {
        match error {
            ureq::Error::StatusCode(code) => Self::HttpStatus(code),
            ureq::Error::RedirectFailed | ureq::Error::TooManyRedirects => Self::RedirectRefused,
            ureq::Error::BodyExceedsLimit(_) => {
                Self::InvalidSource("Ответ источника превышает 2 МиБ.")
            }
            _ => Self::SourceUnavailable,
        }
    }

    fn outcome(&self) -> BridgeOutcome {
        match self {
            Self::HttpStatus(_) | Self::RedirectRefused => BridgeOutcome::SourceRejected,
            Self::SourceUnavailable => BridgeOutcome::SourceUnavailable,
            Self::InvalidRequest | Self::InvalidSource(_) | Self::MappingField { .. } => {
                BridgeOutcome::SourceInvalid
            }
            Self::Transfer(ImportTransferError::Rejected { .. }) => BridgeOutcome::BatchRejected,
            Self::Transfer(ImportTransferError::Unavailable) => BridgeOutcome::ApiUnavailable,
            Self::Transfer(ImportTransferError::ConfirmationLost) => {
                BridgeOutcome::ConfirmationLost
            }
        }
    }
}

impl fmt::Display for PollFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest => formatter.write_str("Проверьте адрес и заголовки источника."),
            Self::HttpStatus(status) => write!(formatter, "Источник вернул HTTP {status}."),
            Self::RedirectRefused | Self::SourceUnavailable => formatter.write_str(
                "Источник недоступен или не прошёл проверку HTTPS. Проверьте подключение и настройки.",
            ),
            Self::InvalidSource(message) => formatter.write_str(message),
            Self::MappingField { row, field } => {
                write!(formatter, "Запись {row}: проверьте поле {field} в сопоставлении.")
            }
            Self::Transfer(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for PollFailure {}

fn pull(
    agent: &ureq::Agent,
    runtime: &tokio::runtime::Runtime,
    config: &Config,
) -> (BridgeRun, Result<(), PollFailure>) {
    let (fetched, batches) = match prepare_poll(agent, config) {
        Ok(prepared) => prepared,
        Err(error) => {
            return (
                BridgeRun {
                    fetched: 0,
                    confirmed: 0,
                    changed: 0,
                    outcome: error.outcome(),
                },
                Err(error),
            );
        }
    };
    let (report, result) = transfer_batches(fetched, batches, |batch| {
        runtime.block_on(import_secure_tracked(
            config.api_port,
            &config.api_token,
            &config.source,
            batch,
        ))
    });
    if result.is_ok() {
        println!(
            "Получено событий: {fetched}. Создано или обновлено: {}.",
            report.changed
        );
    }
    (report, result)
}

fn prepare_poll(
    agent: &ureq::Agent,
    config: &Config,
) -> Result<(u16, Vec<ImportBatch>), PollFailure> {
    let post_body = match config.format {
        Format::Json => config.mapping.as_ref().and_then(|m| m.post_body.clone()),
        Format::Bitrix => Some(config.bitrix_period.request_body(Local::now().date_naive())),
    };
    let method = if post_body.is_some() { "POST" } else { "GET" };
    let mut request = ureq::http::Request::builder()
        .method(method)
        .uri(&config.source_url);
    if let Some(token) = &config.source_token {
        let auth = config.mapping.as_ref().map_or(Auth::Bearer, |m| m.auth);
        request = match auth {
            Auth::Bearer => request.header("Authorization", format!("Bearer {token}")),
            Auth::AuthorizationRaw => request.header("Authorization", token),
            Auth::XApiKey => request.header("X-Api-Key", token),
        };
    }
    let bytes = if let Some(body) = post_body {
        request = request.header("Content-Type", "application/json");
        serde_json::to_vec(&body).map_err(|_| PollFailure::InvalidRequest)?
    } else {
        Vec::new()
    };
    let request = request
        .body(bytes)
        .map_err(|_| PollFailure::InvalidRequest)?;
    // Errors omit the source URL, which can contain a webhook credential.
    let mut response = agent.run(request).map_err(PollFailure::from_source_error)?;
    if !response.status().is_success() {
        return Err(PollFailure::HttpStatus(response.status().as_u16()));
    }
    let body = response
        .body_mut()
        .with_config()
        .limit(2 * 1024 * 1024)
        .read_to_vec()
        .map_err(PollFailure::from_source_error)?;
    let items = match (&config.format, config.mapping.as_ref()) {
        (Format::Json, Some(mapping)) => {
            let document = serde_json::from_slice(&body)
                .map_err(|_| PollFailure::InvalidSource("Ответ источника не является JSON."))?;
            mapped_records(mapping, &document)?
        }
        (Format::Json, None) => {
            serde_json::from_slice::<ImportBatch>(&body)
                .map_err(|_| PollFailure::InvalidSource("Ответ источника имеет неверный формат."))?
                .events
        }
        (Format::Bitrix, _) => {
            let response = serde_json::from_slice(&body).map_err(|_| {
                PollFailure::InvalidSource("Ответ календаря Bitrix24 имеет неверный формат.")
            })?;
            bitrix_records(response)?
        }
    };
    prepare_batches(items)
}

// Count the bytes emitted by the same JSON serializer used by the transfer.
struct JsonLength(usize);

impl Write for JsonLength {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn prepare_batches(items: Vec<ImportItem>) -> Result<(u16, Vec<ImportBatch>), PollFailure> {
    let count = u16::try_from(items.len())
        .ok()
        .filter(|count| *count <= BRIDGE_RUN_LIMIT)
        .ok_or(PollFailure::InvalidSource(
            "Один проход может импортировать до 2000 событий.",
        ))?;
    let mut batches = Vec::new();
    let mut batch = Vec::new();
    let mut size = 16;
    for item in items {
        let mut length = JsonLength(0);
        serde_json::to_writer(&mut length, &item)
            .map_err(|_| PollFailure::InvalidSource("Не удалось подготовить событие к импорту."))?;
        let bytes = length.0.saturating_add(1);
        if bytes >= BODY_LIMIT - 16 {
            return Err(PollFailure::InvalidSource(
                "Одно событие превышает размер пакета API.",
            ));
        }
        if batch.len() == BATCH_LIMIT || size + bytes >= BODY_LIMIT {
            batches.push(ImportBatch {
                events: std::mem::take(&mut batch),
            });
            size = 16;
        }
        size += bytes;
        batch.push(item);
    }
    if !batch.is_empty() {
        batches.push(ImportBatch { events: batch });
    }
    Ok((count, batches))
}

fn transfer_batches(
    fetched: u16,
    batches: Vec<ImportBatch>,
    mut send: impl FnMut(&ImportBatch) -> Result<usize, ImportTransferError>,
) -> (BridgeRun, Result<(), PollFailure>) {
    let mut report = BridgeRun {
        fetched,
        confirmed: 0,
        changed: 0,
        outcome: BridgeOutcome::Complete,
    };
    for batch in batches {
        // prepare_batches bounds the total size of all batches by fetched.
        let count = batch.events.len() as u16;
        let result = send(&batch).and_then(|written| {
            u16::try_from(written)
                .ok()
                .filter(|written| *written <= count)
                .ok_or(ImportTransferError::ConfirmationLost)
        });
        match result {
            Ok(written) => {
                report.confirmed += count;
                report.changed += written;
            }
            Err(error) => {
                let error = PollFailure::Transfer(error);
                report.outcome = error.outcome();
                return (report, Err(error));
            }
        }
    }
    (report, Ok(()))
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
enum Auth {
    #[default]
    Bearer,
    AuthorizationRaw,
    XApiKey,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Mapping {
    items: String,
    id: String,
    title: String,
    date: String,
    description: Option<String>,
    category: Option<Category>,
    preparation_days: Option<u16>,
    yearly: Option<bool>,
    #[serde(default)]
    auth: Auth,
    post_body: Option<serde_json::Value>,
}

fn source_date(value: &str) -> Option<NaiveDate> {
    let date = value.get(..10)?;
    NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .or_else(|_| NaiveDate::parse_from_str(date, "%d.%m.%Y"))
        .ok()
}

fn source_id(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(id) => Some(id.clone()),
        serde_json::Value::Number(id) => Some(id.to_string()),
        _ => None,
    }
}

fn mapped_records(
    mapping: &Mapping,
    document: &serde_json::Value,
) -> Result<Vec<ImportItem>, PollFailure> {
    let items = document
        .pointer(&mapping.items)
        .and_then(serde_json::Value::as_array)
        .ok_or(PollFailure::InvalidSource(
            "Путь items должен указывать на массив записей.",
        ))?;
    if items.len() > usize::from(BRIDGE_RUN_LIMIT) {
        return Err(PollFailure::InvalidSource(
            "Один проход может импортировать до 2000 событий.",
        ));
    }
    items
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let field = |path| row.pointer(path).and_then(serde_json::Value::as_str);
            let invalid = |field| PollFailure::MappingField {
                row: index + 1,
                field,
            };
            let external_id = row
                .pointer(&mapping.id)
                .and_then(source_id)
                .ok_or_else(|| invalid("id"))?;
            Ok(ImportItem {
                external_id,
                event: EventInput {
                    title: field(&mapping.title)
                        .ok_or_else(|| invalid("title"))?
                        .into(),
                    date: field(&mapping.date)
                        .and_then(source_date)
                        .ok_or_else(|| invalid("date"))?,
                    description: mapping
                        .description
                        .as_deref()
                        .and_then(field)
                        .map(str::to_owned),
                    category: mapping.category,
                    preparation_days: mapping.preparation_days,
                    yearly: mapping.yearly,
                    owner_id: None,
                    checklist: None,
                },
            })
        })
        .collect()
}

#[derive(Deserialize)]
struct BitrixResponse {
    result: Vec<BitrixEvent>,
}

#[derive(Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
struct BitrixEvent {
    private_event: BitrixPrivacy,
    id: serde_json::Value,
    name: String,
    date_from: Option<String>,
    date_from_iso: Option<String>,
    description: Option<String>,
    rrule: Option<serde_json::Value>,
}

#[derive(Deserialize)]
enum BitrixPrivacy {
    #[serde(rename = "Y")]
    Private,
    #[serde(rename = "N", alias = "")]
    Public,
}

fn bitrix_records(response: BitrixResponse) -> Result<Vec<ImportItem>, PollFailure> {
    let mut records = std::collections::BTreeMap::new();
    for row in response.result {
        if matches!(row.private_event, BitrixPrivacy::Private) {
            continue;
        }
        let id = source_id(&row.id).ok_or(PollFailure::InvalidSource(
            "Bitrix24 вернул событие без ID.",
        ))?;
        let date = row
            .date_from_iso
            .as_deref()
            .into_iter()
            .chain(row.date_from.as_deref())
            .find_map(source_date)
            .ok_or(PollFailure::InvalidSource(
                "Неподдерживаемый формат даты события Bitrix24.",
            ))?;
        let external_id = if row
            .rrule
            .is_some_and(|v| !v.is_null() && v != false && v != "")
        {
            format!("{id}-{date}")
        } else {
            id
        };
        records.insert(
            external_id.clone(),
            ImportItem {
                external_id,
                event: EventInput {
                    title: row.name.chars().take(160).collect(),
                    date,
                    category: Some(Category::Operations),
                    description: row
                        .description
                        .map(|description| description.chars().take(4000).collect()),
                    yearly: None,
                    owner_id: None,
                    preparation_days: None,
                    checklist: None,
                },
            },
        );
    }
    Ok(records.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_errors_do_not_echo_input_and_have_a_read_limit() {
        let redacted = "private-synthetic-field";
        let invalid = format!(r#"{{"{redacted}":"value"}}"#);
        let wrong_type = format!(
            r#"{{"items":"items","id":"id","title":"title","date":"date","yearly":"{redacted}"}}"#
        );
        for invalid in [invalid, wrong_type] {
            let error = read_mapping(invalid.as_bytes()).err().unwrap().to_string();
            assert!(error.starts_with("Проверьте файл сопоставления. Строка 1, столбец "));
            assert!(!error.contains(redacted));
        }

        let mut mapping = br#"{"items":"items","id":"id","title":"title","date":"date"}"#.to_vec();
        mapping.resize(MAPPING_LIMIT as usize, b' ');
        assert!(read_mapping(mapping.as_slice()).is_ok());
        mapping.push(b' ');
        assert_eq!(
            read_mapping(mapping.as_slice()).err().unwrap().to_string(),
            MAPPING_TOO_LARGE
        );
    }

    #[test]
    fn an_empty_poll_is_complete_without_sending_a_batch() {
        let (fetched, batches) = prepare_batches(Vec::new()).unwrap();
        let (report, result) = transfer_batches(fetched, batches, |_| {
            panic!("empty poll must not send a batch")
        });
        assert!(result.is_ok());
        assert!(matches!(report.outcome, BridgeOutcome::Complete));
        assert_eq!(
            (report.fetched, report.confirmed, report.changed),
            (0, 0, 0)
        );
    }

    #[test]
    fn batch_boundaries_count_encoded_json_bytes() {
        fn item(target_bytes: usize) -> ImportItem {
            let mut row: ImportItem = serde_json::from_value(serde_json::json!({
                "externalId": "№1",
                "event": {
                    "title": "Проверить \"акт\" 🗓",
                    "date": "2026-12-01",
                    "description": "Первая строка\nВторая\t\\\u{0000}",
                    "checklist": [{"key": "é", "title": "Сверить сумму", "daysBefore": 2}]
                }
            }))
            .unwrap();
            let fixed = serde_json::to_vec(&row).unwrap().len();
            row.event.title.push_str(&"x".repeat(target_bytes - fixed));
            assert_eq!(serde_json::to_vec(&row).unwrap().len(), target_bytes);
            row
        }
        let first = 1024;
        let second = BODY_LIMIT - 16 - 2 - first;
        let (_, fitting) = prepare_batches(vec![item(first), item(second - 1)]).unwrap();
        assert_eq!(fitting.len(), 1);
        let (_, split) = prepare_batches(vec![item(first), item(second)]).unwrap();
        assert_eq!(
            split
                .iter()
                .map(|part| part.events.len())
                .collect::<Vec<_>>(),
            [1, 1]
        );
        assert!(prepare_batches(vec![item(BODY_LIMIT - 18)]).is_ok());

        // A later oversized item rejects the poll even after a complete batch.
        let mut rows: Vec<_> = (0..=BATCH_LIMIT).map(|_| item(first)).collect();
        rows.push(item(BODY_LIMIT - 17));
        assert!(matches!(
            prepare_batches(rows),
            Err(PollFailure::InvalidSource(
                "Одно событие превышает размер пакета API."
            ))
        ));
    }

    #[test]
    fn a_failed_second_batch_preserves_confirmed_counts() {
        for (error, expected) in [
            (
                ImportTransferError::Rejected { status: 422 },
                BridgeOutcome::BatchRejected,
            ),
            (
                ImportTransferError::Unavailable,
                BridgeOutcome::ApiUnavailable,
            ),
            (
                ImportTransferError::ConfirmationLost,
                BridgeOutcome::ConfirmationLost,
            ),
        ] {
            let items = (0..=BATCH_LIMIT)
                .map(|index| {
                    serde_json::from_value(serde_json::json!({
                        "externalId": format!("qa-{index}"),
                        "event": {"title": "Событие", "date": "2026-12-01"},
                    }))
                    .unwrap()
                })
                .collect();
            let (fetched, batches) = prepare_batches(items).unwrap();
            let mut replies = [Ok(2), Err(error)].into_iter();
            let (report, result) = transfer_batches(fetched, batches, |_| replies.next().unwrap());
            assert!(result.is_err());
            assert_eq!(report.outcome, expected);
            assert_eq!(
                (
                    usize::from(report.fetched),
                    usize::from(report.confirmed),
                    report.changed,
                ),
                (BATCH_LIMIT + 1, BATCH_LIMIT, 2)
            );
        }
    }

    #[test]
    fn source_errors_are_classified_without_raw_diagnostics() {
        let status = PollFailure::from_source_error(ureq::Error::StatusCode(401));
        assert!(matches!(status.outcome(), BridgeOutcome::SourceRejected));
        assert_eq!(status.to_string(), "Источник вернул HTTP 401.");
        let unavailable = PollFailure::from_source_error(ureq::Error::ConnectionFailed);
        assert!(matches!(
            unavailable.outcome(),
            BridgeOutcome::SourceUnavailable
        ));
        let limit = PollFailure::from_source_error(ureq::Error::BodyExceedsLimit(2 * 1024 * 1024));
        assert!(matches!(limit.outcome(), BridgeOutcome::SourceInvalid));
    }

    #[test]
    fn bitrix_default_period_changes_when_the_local_year_changes() {
        let december = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
        let january = NaiveDate::from_ymd_opt(2027, 1, 1).unwrap();
        let period = BitrixPeriod::from_args(None, None, december.year()).unwrap();
        assert_eq!(period.request_body(december)["from"], "2026-01-01");
        assert_eq!(period.request_body(december)["to"], "2026-12-31");
        assert_eq!(period.request_body(january)["from"], "2027-01-01");
        assert_eq!(period.request_body(january)["to"], "2027-12-31");
    }

    #[test]
    fn bitrix_explicit_period_stays_fixed_even_with_one_boundary() {
        let from = NaiveDate::from_ymd_opt(2026, 8, 12).unwrap();
        let to = NaiveDate::from_ymd_opt(2027, 5, 6).unwrap();
        let today = NaiveDate::from_ymd_opt(2027, 1, 1).unwrap();
        for (begin, end, expected_from, expected_to) in [
            (Some(from), Some(to), "2026-08-12", "2027-05-06"),
            (Some(from), None, "2026-08-12", "2026-12-31"),
            (None, Some(to), "2026-01-01", "2027-05-06"),
        ] {
            let period = BitrixPeriod::from_args(begin, end, 2026).unwrap();
            let body = period.request_body(today);
            assert_eq!(body["from"], expected_from);
            assert_eq!(body["to"], expected_to);
        }
        assert!(BitrixPeriod::from_args(Some(to), Some(from), 2026).is_err());
    }

    #[test]
    fn applies_custom_mapping_to_nested_server_records() {
        let config = serde_json::json!({"items":"/result/items","id":"/id","title":"/fields/name",
            "date":"/fields/deadline","description":"/fields/about","category":"Campaign","auth":"xApiKey"});
        let upstream = serde_json::json!({"result":{"items":[{"id":123,"fields":{"name":"Акция",
            "deadline":"2026-12-07T11:00:00+03:00","about":"Описание"}}]}});
        let mapping: Mapping = serde_json::from_value(config).unwrap();
        let records = mapped_records(&mapping, &upstream).unwrap();
        assert_eq!(records[0].external_id, "123");
        assert_eq!(records[0].event.date.to_string(), "2026-12-07");
        assert_eq!(records[0].event.category, Some(Category::Campaign));
    }

    #[test]
    fn bitrix_requires_explicit_visibility_before_import() {
        let private =
            serde_json::json!({"ID":"private", "NAME":"private detail", "PRIVATE_EVENT":"Y"});
        let public = serde_json::json!({"ID":"public", "NAME":"Meeting", "DATE_FROM":"2026-12-07", "PRIVATE_EVENT":""});
        let input = serde_json::json!({"result":[private,public]});
        let rows = bitrix_records(serde_json::from_value(input.clone()).unwrap()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].external_id, "public");
        for flag in [
            serde_json::Value::Null,
            serde_json::json!("unknown"),
            serde_json::json!(false),
        ] {
            let mut invalid = input.clone();
            invalid["result"][1]["PRIVATE_EVENT"] = flag;
            assert!(serde_json::from_value::<BitrixResponse>(invalid).is_err());
        }
        let mut missing = input;
        missing["result"][1]
            .as_object_mut()
            .unwrap()
            .remove("PRIVATE_EVENT");
        assert!(serde_json::from_value::<BitrixResponse>(missing).is_err());
    }

    #[test]
    fn converts_company_calendar_dates_and_stable_ids() {
        let wire = serde_json::json!({"result":[
            {"PRIVATE_EVENT":"N","ID":"123","NAME":"Планирование","DATE_FROM":"07.12.2026 10:30:00","DESCRIPTION":"Описание"},
            {"PRIVATE_EVENT":"N","ID":456,"NAME":"Повтор","DATE_FROM":"2026-12-15T10:30:00+03:00","RRULE":{"FREQ":"WEEKLY"}},
            {"PRIVATE_EVENT":"","ID":456,"NAME":"Повтор","DATE_FROM_ISO":"2026-12-22T10:30:00+03:00","RRULE":{"FREQ":"WEEKLY"}}
        ]});
        let items = bitrix_records(serde_json::from_value(wire).unwrap()).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].external_id, "123");
        assert_eq!(items[0].event.description.as_deref(), Some("Описание"));
        assert_eq!(items[2].external_id, "456-2026-12-22");
    }
}
