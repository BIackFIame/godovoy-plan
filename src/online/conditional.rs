//! In-memory validators only. Both credentials and policy are checked before lookup.
use super::super::actions::{CalendarView, ViewContext};
use axum::{
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use blake2::{Blake2b, Digest, digest::consts::U32};
use chrono::{Datelike, NaiveDate};
use godovoy_plan::{Fallible, model::Id, storage::online::CalendarFile};
use std::{
    collections::VecDeque,
    io::{BufWriter, Write},
    sync::Mutex,
};

const MAX_VIEWS: usize = 128;
type InputDigest = [u8; 32];

#[derive(Copy, Clone, Eq, PartialEq)]
struct ReadHint {
    credential: Id,
    size: usize,
    modified: std::time::SystemTime,
}

struct Entry {
    digest: InputDigest,
    tag: HeaderValue,
    hint: Option<ReadHint>,
}

pub(super) struct Views {
    nonce: Id,
    entries: Mutex<VecDeque<Entry>>,
}

impl Default for Views {
    fn default() -> Self {
        Self {
            nonce: Id::new_v4(),
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
    ) -> Fallible<Blake2b<U32>> {
        let mut source = Blake2b::<U32>::new();
        source.update(credential.as_bytes());
        source.update(today.num_days_from_ce().to_be_bytes());
        serde_json::to_writer(&mut source, context)?;
        Ok(source)
    }

    pub(super) fn lookup(&self, key: &InputDigest) -> Fallible<Option<HeaderValue>> {
        self.lookup_and_refresh(key, None)
    }

    fn lookup_and_refresh(
        &self,
        key: &InputDigest,
        hint: Option<ReadHint>,
    ) -> Fallible<Option<HeaderValue>> {
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
                entry.tag.clone()
            }))
    }

    pub(super) fn remember(&self, key: InputDigest, tag: HeaderValue) -> Fallible<()> {
        self.store(key, tag, None)
    }

    fn store(&self, key: InputDigest, tag: HeaderValue, hint: Option<ReadHint>) -> Fallible<()> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| "Ошибка проверки версии календаря.")?;
        if entries.len() == MAX_VIEWS {
            entries.pop_front();
        }
        entries.push_back(Entry {
            digest: key,
            tag,
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
                .find(|entry| entry.hint == Some(hint) && entry.tag == condition)
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
        if let Some(tag) = &known
            && matches_tag(condition, tag)
        {
            return Ok(tagged(
                StatusCode::NOT_MODIFIED.into_response(),
                tag.clone(),
            ));
        }
        let calendar = serde_json::from_slice(&data)?;
        drop(data);
        let body = serde_json::to_vec(&context.render(&calendar, today))?;
        let tag = if let Some(tag) = known {
            tag
        } else {
            let tag = Self::tag(self.tag_hasher(credential).chain_update(&body))?;
            self.store(key, tag.clone(), hint)?;
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
