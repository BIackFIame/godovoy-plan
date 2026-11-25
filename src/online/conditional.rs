//! In-memory validators only. Both credentials and policy are checked before lookup.
use super::super::actions::{CalendarView, PublicYear, ViewContext};
use axum::{
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use blake2::{Blake2b, Digest, digest::consts::U32};
use chrono::{Datelike, NaiveDate};
use godovoy_plan::{
    Fallible,
    model::Id,
    storage::online::{CalendarFile, Data},
};
use serde_json::value::{RawValue, to_raw_value};
use std::{
    collections::VecDeque,
    io::{BufWriter, Write},
    sync::Mutex,
};

const MAX_VIEWS: usize = 128;
const MIN_SHARED_SOURCE: usize = 128 * 1024;
const MAX_SHARED_SOURCE: usize = 1536 * 1024;
const MAX_SHARED_ROWS: usize = MAX_SHARED_SOURCE;

struct PublicRows {
    key: PublicYear,
    json: Option<Box<RawValue>>,
}

struct DecodedSource {
    bytes: Vec<u8>,
    calendar: Data,
    public_rows: Option<PublicRows>,
}

type InputDigest = [u8; 32];
// Apple Silicon provides SHA-256 instructions on every supported device.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
type InputHasher = sha2::Sha256;
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
type InputHasher = Blake2b<U32>;

#[derive(Copy, Clone, Eq, PartialEq)]
struct ReadHint {
    credential: Id,
    size: usize,
    modified: std::time::SystemTime,
}

#[derive(Clone)]
struct CachedView {
    tag: HeaderValue,
    // A write receipt can establish a tag before any complete response is sent.
    body_len: Option<usize>,
}

struct Entry {
    digest: InputDigest,
    view: CachedView,
    hint: Option<ReadHint>,
}

pub(super) struct Views {
    nonce: Id,
    source: Mutex<Option<DecodedSource>>,
    entries: Mutex<VecDeque<Entry>>,
}

impl Default for Views {
    fn default() -> Self {
        Self {
            nonce: Id::new_v4(),
            source: Mutex::default(),
            entries: Mutex::new(VecDeque::new()),
        }
    }
}

impl Views {
    pub(super) fn input(
        credential: Id,
        context: &ViewContext<'_>,
        today: NaiveDate,
        data: &[u8],
    ) -> Fallible<InputDigest> {
        let mut source = Self::input_hasher(credential, context, today)?;
        source.update(data);
        Ok(source.finalize().into())
    }

    pub(super) fn input_hasher(
        credential: Id,
        context: &ViewContext<'_>,
        today: NaiveDate,
    ) -> Fallible<InputHasher> {
        let mut source = InputHasher::new();
        source.update(credential.as_bytes());
        source.update(today.num_days_from_ce().to_be_bytes());
        serde_json::to_writer(&mut source, context)?;
        Ok(source)
    }

    pub(super) fn lookup(&self, key: &InputDigest) -> Fallible<Option<HeaderValue>> {
        Ok(self.lookup_and_refresh(key, None)?.map(|view| view.tag))
    }

    fn lookup_and_refresh(
        &self,
        key: &InputDigest,
        hint: Option<ReadHint>,
    ) -> Fallible<Option<CachedView>> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| "Ошибка проверки версии календаря.")?;
        Ok(entries
            .iter_mut()
            .find(|entry| &entry.digest == key)
            .map(|entry| {
                if hint.is_some() {
                    entry.hint = hint;
                }
                entry.view.clone()
            }))
    }

    pub(super) fn remember(&self, key: InputDigest, tag: HeaderValue) -> Fallible<()> {
        self.store(
            key,
            CachedView {
                tag,
                body_len: None,
            },
            None,
        )
    }

    fn store(&self, key: InputDigest, view: CachedView, hint: Option<ReadHint>) -> Fallible<()> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| "Ошибка проверки версии календаря.")?;
        if let Some(entry) = entries.iter_mut().find(|entry| entry.digest == key) {
            if view.body_len.is_some() {
                entry.view = view;
                entry.hint = hint;
            }
            return Ok(());
        }
        if entries.len() == MAX_VIEWS {
            entries.pop_front();
        }
        entries.push_back(Entry {
            digest: key,
            view,
            hint,
        });
        Ok(())
    }

    fn tag_hasher(&self, credential: Id) -> Blake2b<U32> {
        Blake2b::new()
            .chain_update(self.nonce.as_bytes())
            .chain_update(credential.as_bytes())
    }

    fn tag(digest: Blake2b<U32>) -> Fallible<HeaderValue> {
        let digest = digest.finalize();
        Ok(HeaderValue::from_str(&format!("\"{digest:x}\""))?)
    }

    pub(super) fn view_tag(
        &self,
        credential: Id,
        view: &CalendarView<'_>,
    ) -> Fallible<HeaderValue> {
        let mut digest = self.tag_hasher(credential);
        {
            let mut writer = BufWriter::new(&mut digest);
            serde_json::to_writer(&mut writer, view)?;
            writer.flush()?;
        }
        Self::tag(digest)
    }

    fn encode_shared_body(
        &self,
        bytes: Vec<u8>,
        context: ViewContext<'_>,
        today: NaiveDate,
        expected_len: Option<usize>,
    ) -> Fallible<Vec<u8>> {
        let mut body = Vec::with_capacity(expected_len.unwrap_or(0));
        let mut entry = self.source.lock().map_err(|_| "Ошибка чтения календаря.")?;
        // Every caller holds the vault lock. Compare its current full source exactly;
        // the returned validator and personalized fields are always computed for this credential.
        if entry.as_ref().is_some_and(|source| source.bytes == bytes) {
            // Release the duplicate source before allocating the response and event rows.
            drop(bytes);
        } else {
            *entry = None;
            let calendar = serde_json::from_slice(&bytes)?;
            *entry = Some(DecodedSource {
                bytes,
                calendar,
                public_rows: None,
            });
        }
        let source = entry.as_mut().ok_or("Не удалось прочитать календарь.")?;
        // A restricted request always follows its normal row filter. A second unrestricted
        // read of the same year and text policy may reuse the shared event array.
        let public_key = context.public_rows_key();
        if let Some(key) = public_key
            && let Some(saved) = &mut source.public_rows
            && saved.key == key
        {
            let rows = match &mut saved.json {
                Some(rows) => rows,
                slot @ None => slot.insert(to_raw_value(&context.event_rows(&source.calendar))?),
            };
            if expected_len.is_none() {
                // Leave room for this reader's roster and tasks. An exact reservation for
                // the shared array always forces a second allocation for the envelope.
                body.reserve((rows.get().len() + 1).next_power_of_two());
            }
            serde_json::to_writer(
                &mut body,
                &context.with_rows(&source.calendar, today, &**rows),
            )?;
            return Ok(body);
        }
        serde_json::to_writer(&mut body, &context.render(&source.calendar, today))?;
        if let Some(key) = public_key {
            // The complete first response proves the size limit for its year array.
            source.public_rows =
                (body.len() <= MAX_SHARED_ROWS).then_some(PublicRows { key, json: None });
        }
        Ok(body)
    }

    pub(super) fn response(
        &self,
        credential: Id,
        context: ViewContext<'_>,
        today: NaiveDate,
        mut file: CalendarFile<'_>,
        condition: &[HeaderValue],
    ) -> Fallible<Response> {
        // This internal fingerprint may change when another employee edits a private
        // event. Only the digest of the caller's rendered representation leaves the host.
        let hint = file.modified().map(|modified| ReadHint {
            credential,
            size: file.byte_len(),
            modified,
        });
        // A matching hint only chooses streaming. Every 304 still verifies the full
        // file, current credential, permissions, local date and rendered-view tag.
        if let ([condition], Some(hint)) = (condition, hint) {
            let candidate = self
                .entries
                .lock()
                .map_err(|_| "Ошибка проверки версии календаря.")?
                .iter()
                .rev()
                .find(|entry| entry.hint == Some(hint) && entry.view.tag == condition)
                .map(|entry| entry.digest);
            if let Some(expected) = candidate {
                let source = Self::input_hasher(credential, &context, today)?;
                if file.fingerprint(source)? == expected {
                    return Ok(tagged(
                        StatusCode::NOT_MODIFIED.into_response(),
                        condition.clone(),
                    ));
                }
            }
        }
        let data = file.into_bytes()?;
        let key = Self::input(credential, &context, today, &data)?;
        let known = self.lookup_and_refresh(&key, hint)?;
        if let Some(view) = &known
            && matches_tag(condition, &view.tag)
        {
            return Ok(tagged(
                StatusCode::NOT_MODIFIED.into_response(),
                view.tag.clone(),
            ));
        }
        let expected_len = known.as_ref().and_then(|view| view.body_len);
        // Keep the parsed document and rows alive through validator generation on a
        // full uncached read, as on the original calendar response path.
        let calendar;
        let view;
        let body = if (MIN_SHARED_SOURCE..=MAX_SHARED_SOURCE).contains(&data.len()) {
            self.encode_shared_body(data, context, today, expected_len)?
        } else {
            *self.source.lock().map_err(|_| "Ошибка чтения календаря.")? = None;
            calendar = serde_json::from_slice(&data)?;
            drop(data);
            view = context.render(&calendar, today);
            if let Some(len) = expected_len {
                let mut body = Vec::with_capacity(len);
                serde_json::to_writer(&mut body, &view)?;
                body
            } else {
                serde_json::to_vec(&view)?
            }
        };
        let tag = if let Some(CachedView {
            tag,
            body_len: Some(_),
        }) = &known
        {
            tag.clone()
        } else {
            let tag = match known {
                Some(cached) => cached.tag,
                None => Self::tag(self.tag_hasher(credential).chain_update(&body))?,
            };
            self.store(
                key,
                CachedView {
                    tag: tag.clone(),
                    body_len: Some(body.len()),
                },
                hint,
            )?;
            tag
        };
        let response = if matches_tag(condition, &tag) {
            StatusCode::NOT_MODIFIED.into_response()
        } else {
            ([(header::CONTENT_TYPE, "application/json")], body).into_response()
        };
        Ok(tagged(response, tag))
    }
}

fn tagged(mut response: Response, tag: HeaderValue) -> Response {
    response.headers_mut().insert(header::ETAG, tag);
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Authorization"));
    response
}

fn matches_tag(condition: &[HeaderValue], current: &HeaderValue) -> bool {
    if let [one] = condition
        && one.as_bytes().trim_ascii() == b"*"
    {
        return true;
    }
    let mut matched = false;
    for line in condition {
        let mut rest = line.as_bytes().trim_ascii();
        while !rest.is_empty() {
            if let Some(next) = rest.strip_prefix(b",") {
                rest = next.trim_ascii_start();
                continue;
            }
            let opaque = rest.strip_prefix(b"W/").unwrap_or(rest);
            let Some(value) = opaque.strip_prefix(b"\"") else {
                return false;
            };
            let Some(end) = value.iter().position(|&b| b == b'"') else {
                return false;
            };
            if value[..end].iter().any(|&b| b < 0x21 || b == 0x7f) {
                return false;
            }
            matched |= &opaque[..end + 2] == current.as_bytes();
            rest = value[end + 1..].trim_ascii_start();
            if !rest.is_empty() && !rest.starts_with(b",") {
                return false;
            }
        }
    }
    matched
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_size_survives_a_duplicate_receipt_and_expires_with_its_digest() {
        let views = Views::default();
        let digest = [0; 32];
        let tag = HeaderValue::from_static("\"year\"");
        let body_len = |key| {
            views
                .lookup_and_refresh(&key, None)
                .unwrap()
                .unwrap()
                .body_len
        };
        views.remember(digest, tag.clone()).unwrap();
        assert_eq!(body_len(digest), None);
        views
            .store(
                digest,
                CachedView {
                    tag: tag.clone(),
                    body_len: Some(780),
                },
                None,
            )
            .unwrap();
        views.remember(digest, tag.clone()).unwrap();
        for i in 1..MAX_VIEWS {
            views.remember([i as u8; 32], tag.clone()).unwrap();
        }
        assert_eq!(body_len(digest), Some(780));
        assert_eq!(body_len([1; 32]), None);
        views.remember([255; 32], tag).unwrap();
        assert!(views.lookup(&digest).unwrap().is_none());
    }
    #[test]
    fn shared_year_rows_respect_current_source_employee_and_text_policy() {
        use godovoy_plan::storage::online::Access;
        use serde_json::{Value, json};
        let alice = Id::new_v4();
        let bob = Id::new_v4();
        let event = Id::new_v4();
        let date = NaiveDate::from_ymd_opt(2026, 11, 25).unwrap();
        let mut document = json!({"calendar":{"members":{},"plans":{event.to_string():{
            "templates":{"2026":{"revision":{"sequence":1,"device":alice},"value":{
                "title":"A","description":"private text","category":"Team","date":date,
                "yearly":true,"preparation_days":0,"owner":alice,"tasks":[]}}},"years":{}
        }}},"audit":[]});
        let mut rules = json!({"name":"QA","employees":{},"grants":{},"audit":[]});
        let views = Views::default();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("vault");
        let vault = godovoy_plan::storage::online::OnlineVault::init(
            &path,
            "QA".into(),
            Default::default(),
            Default::default(),
        )
        .unwrap();
        let lock = vault.lock().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let compare = |doc: &Value, acl: &Value, employee: Id, year: i32, size: usize| {
            let acl: Access = serde_json::from_value(acl.clone()).unwrap();
            let ctx = || ViewContext::new(&acl, year, employee, false);
            let mut bytes = serde_json::to_vec(doc).unwrap();
            bytes.resize(size, b' ');
            let data = serde_json::from_slice(&bytes).unwrap();
            let expected = serde_json::to_vec(&ctx().render(&data, date)).unwrap();
            std::fs::write(path.join("online-calendar-v1.json"), bytes).unwrap();
            let response = views
                .response(employee, ctx(), date, lock.open_data().unwrap(), &[])
                .unwrap();
            let actual = runtime
                .block_on(axum::body::to_bytes(
                    response.into_body(),
                    MAX_SHARED_SOURCE * 2,
                ))
                .unwrap();
            assert_eq!(&actual[..], expected);
            serde_json::from_slice::<Value>(&actual).unwrap()
        };
        for id in [alice, bob, alice] {
            let v = compare(&document, &rules, id, 2026, MIN_SHARED_SOURCE);
            assert_eq!(v["events"][0]["description"], "private text");
        }
        rules["assigned_only"] = json!([bob]);
        assert_eq!(
            compare(&document, &rules, bob, 2026, MIN_SHARED_SOURCE)["events"],
            json!([])
        );
        rules["protection"] = json!({"version":1,"state":{"revision":1,"settings":{
            "russia":true,"belarus":false,"allowRemote":false,"allowFreeText":false
        }},"changeId":Id::new_v4(),"pendingAudit":false});
        for yr in [2026, 2026, 2027, 2026] {
            let v = compare(&document, &rules, alice, yr, MIN_SHARED_SOURCE);
            assert_eq!(v["events"][0]["description"], "");
            assert_eq!(v["events"][0]["year"], yr);
        }
        document["calendar"]["plans"][event.to_string()]["templates"]["2026"]["value"]["title"] =
            json!("B");
        assert_eq!(
            compare(&document, &rules, alice, 2026, MIN_SHARED_SOURCE)["events"][0]["title"],
            "B"
        );
        compare(&document, &rules, alice, 2026, MAX_SHARED_SOURCE + 1);
        assert!(views.source.lock().unwrap().is_none());
    }
}
