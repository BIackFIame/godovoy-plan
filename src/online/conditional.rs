//! In-memory validators only. Both credentials and policy are checked before lookup.
use super::super::actions::{CalendarView, ViewContext};
use axum::{
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use blake2::{Blake2b, Digest, digest::consts::U32};
use chrono::{Datelike, NaiveDate};
use godovoy_plan::{Fallible, model::Id};
use std::{
    collections::VecDeque,
    io::{BufWriter, Write},
    sync::Mutex,
};

const MAX_VIEWS: usize = 128;
type InputDigest = [u8; 32];

pub(super) struct Views {
    nonce: Id,
    entries: Mutex<VecDeque<(InputDigest, HeaderValue)>>,
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
        let entries = self
            .entries
            .lock()
            .map_err(|_| "Ошибка проверки версии календаря.")?;
        Ok(entries
            .iter()
            .find(|(digest, _)| digest == key)
            .map(|(_, tag)| tag.clone()))
    }

    pub(super) fn remember(&self, key: InputDigest, tag: HeaderValue) -> Fallible<()> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| "Ошибка проверки версии календаря.")?;
        if entries.len() == MAX_VIEWS {
            entries.pop_front();
        }
        entries.push_back((key, tag));
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
        data: Vec<u8>,
        condition: &[HeaderValue],
    ) -> Fallible<Response> {
        // This internal fingerprint may change when another employee edits a private
        // event. Only the digest of the caller's rendered representation leaves the host.
        let key = Self::input(credential, &context, today, &data)?;
        let known = self.lookup(&key)?;
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
            self.remember(key, tag.clone())?;
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
