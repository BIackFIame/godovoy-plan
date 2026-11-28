//! Fixed application files only. Calendar data, credentials and API responses
//! never enter this compression path.
use axum::{
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};

pub(crate) struct Asset {
    media_type: &'static str,
    identity: &'static [u8],
    gzip: &'static [u8],
    brotli: &'static [u8],
}

macro_rules! asset {
    ($file:literal, $mime:literal) => {
        Asset {
            media_type: $mime,
            identity: include_bytes!($file),
            brotli: include_bytes!(concat!(env!("OUT_DIR"), "/", $file, ".br")),
            gzip: include_bytes!(concat!(env!("OUT_DIR"), "/", $file, ".gz")),
        }
    };
}

macro_rules! generated_asset {
    ($file:literal, $mime:literal) => {
        Asset {
            media_type: $mime,
            identity: include_bytes!(concat!(env!("OUT_DIR"), "/", $file)),
            gzip: include_bytes!(concat!(env!("OUT_DIR"), "/", $file, ".gz")),
            brotli: include_bytes!(concat!(env!("OUT_DIR"), "/", $file, ".br")),
        }
    };
}
pub(crate) const INDEX: Asset = generated_asset!("index.html", "text/html; charset=utf-8");
pub(crate) const WARM: Asset = generated_asset!("warm.html", "text/html; charset=utf-8");
pub(crate) const SCRIPT_PATH: &str = env!("GODOVOY_SCRIPT_PATH");
pub(crate) const PRELOAD: HeaderValue = HeaderValue::from_static(concat!(
    "<", env!("GODOVOY_SCRIPT_PATH"), ">; rel=preload; as=script; integrity=\"", env!("GODOVOY_SCRIPT_INTEGRITY"), "\""
));

pub(crate) fn calendar_policy() -> HeaderValue {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use sha2::{Digest, Sha256};
    use std::{borrow::Cow, sync::LazyLock};

    fn html_digest(text: &str) -> String {
        // HTML parsing normalizes line endings before the browser verifies CSP.
        let text = if text.contains('\r') {
            Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n"))
        } else {
            Cow::Borrowed(text)
        };
        STANDARD.encode(Sha256::digest(text.as_bytes()))
    }

    static POLICY: LazyLock<HeaderValue> = LazyLock::new(|| {
        let gate = html_digest(include_str!("login-gate.js"));
        let script = html_digest(include_str!("calendar.min.js"));
        let style = html_digest(include_str!("calendar.css"));
        format!("default-src 'none'; script-src 'sha256-{gate}' 'sha256-{script}'; style-src 'sha256-{style}'; connect-src 'self'; frame-ancestors 'none'; form-action 'none'; base-uri 'none'")
            .parse().expect("static calendar policy")
    });
    POLICY.clone()
}
pub(crate) const CALENDAR_JS: Asset = generated_asset!("calendar.js", "text/javascript; charset=utf-8");
pub(crate) const CALENDAR_CSS: Asset = asset!("calendar.css", "text/css; charset=utf-8");
pub(crate) const ADMIN: Asset = asset!("admin.html", "text/html; charset=utf-8");
pub(crate) const ADMIN_JS: Asset = asset!("admin.js", "text/javascript; charset=utf-8");
pub(crate) const ADMIN_CSS: Asset = asset!("admin.css", "text/css; charset=utf-8");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Encoding {
    Brotli,
    Gzip,
    Identity,
}

fn weight(parameter: &str) -> Option<u16> {
    let (key, value) = parameter.trim().split_once('=')?;
    if !key.eq_ignore_ascii_case("q") {
        return None;
    }
    let (integer, fraction) = value.split_once('.').unwrap_or((value, ""));
    if fraction.len() > 3 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut digits = 0;
    for (byte, place) in fraction.bytes().zip([100, 10, 1]) {
        digits += u16::from(byte - b'0') * place;
    }
    match integer {
        "0" => Some(digits),
        "1" if digits == 0 => Some(1000),
        _ => None,
    }
}

fn preferred(headers: &HeaderMap) -> Option<Encoding> {
    let (mut gzip, mut identity, mut wildcard): (Option<u16>, Option<u16>, Option<u16>) =
        (None, None, None);
    let mut brotli: Option<u16> = None;
    for field in headers.get_all(header::ACCEPT_ENCODING) {
        for part in field.to_str().unwrap_or_default().split(',') {
            let (name, quality) = match part.split_once(';') {
                Some((name, parameter)) => (name, weight(parameter).unwrap_or(0)),
                None => (part, 1000),
            };
            let slot = match name.trim() {
                name if name.eq_ignore_ascii_case("gzip")
                    || name.eq_ignore_ascii_case("x-gzip") =>
                {
                    &mut gzip
                }
                name if name.eq_ignore_ascii_case("identity") => &mut identity,
                name if name.eq_ignore_ascii_case("br") => &mut brotli,
                "*" => &mut wildcard,
                _ => continue,
            };
            // In a contradictory duplicate, honor the more restrictive preference.
            *slot = Some(slot.unwrap_or(quality).min(quality));
        }
    }
    let compressed = gzip.or(wildcard).unwrap_or(0);
    let br_weight = brotli.or(wildcard).unwrap_or(0);
    if br_weight > 0 && br_weight >= compressed && br_weight >= identity.unwrap_or(0) {
        Some(Encoding::Brotli)
    } else if compressed > 0 && compressed >= identity.unwrap_or(0) {
        Some(Encoding::Gzip)
    } else if identity.unwrap_or(if wildcard == Some(0) { 0 } else { 1000 }) > 0 {
        Some(Encoding::Identity)
    } else {
        None
    }
}

impl Asset {
    pub(crate) fn serve(&self, headers: &HeaderMap) -> Response {
        let mut response = match preferred(headers) {
            Some(encoding) => {
                let (body, coding) = match encoding {
                    Encoding::Identity => (self.identity, None),
                    Encoding::Gzip => (self.gzip, Some("gzip")),
                    Encoding::Brotli => (self.brotli, Some("br")),
                };
                let mut response = ([(header::CONTENT_TYPE, self.media_type)], body).into_response();
                if let Some(coding) = coding {
                    response.headers_mut().insert(header::CONTENT_ENCODING, HeaderValue::from_static(coding));
                }
                response
            }
            None => StatusCode::NOT_ACCEPTABLE.into_response(),
        };
        response
            .headers_mut()
            .insert(header::VARY, HeaderValue::from_static("accept-encoding"));
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn calendar_document_matches_its_restricted_content_policy() {
        use base64::{Engine, engine::general_purpose::STANDARD};
        use sha2::{Digest, Sha256};

        let document = std::str::from_utf8(INDEX.identity).unwrap().replace("\r\n", "\n").replace('\r', "\n");
        let policy = calendar_policy();
        let directives: Vec<_> = policy.to_str().unwrap().split("; ").collect();
        for (tag, directive) in [("script", "script-src"), ("style", "style-src")] {
            let opening = format!("<{tag}>");
            let expected_count = if tag == "script" { 2 } else { 1 };
            let closing = format!("</{tag}>");
            let hashes: Vec<_> = document.split(&opening).skip(1).map(|tail| {
                let (content, _) = tail.split_once(&closing).unwrap();
                format!("'sha256-{}'", STANDARD.encode(Sha256::digest(content.as_bytes())))
            }).collect();
            assert_eq!(hashes.len(), expected_count);
            assert!(directives.contains(&format!("{directive} {}", hashes.join(" ")).as_str()));
        }
        let script = std::str::from_utf8(CALENDAR_JS.identity).unwrap();
        assert!(!script.contains('\r'));
        let digest = Sha256::digest(script.as_bytes());
        let integrity = format!("sha256-{}", STANDARD.encode(digest));
        assert_eq!(SCRIPT_PATH, format!("/static/calendar-{digest:x}.js"));
        assert!(policy.to_str().unwrap().contains(&format!("'{integrity}'")));
        let warm = std::str::from_utf8(WARM.identity).unwrap();
        assert!(warm.contains(&format!(r#"<script id="calendar-program" src="{SCRIPT_PATH}" integrity="{integrity}"></script>"#)));
        assert!(!warm.contains(script));
        assert!(!document.contains("src=\"/calendar.js\""));
        assert!(!document.contains("href=\"/calendar.css\""));
    }

    #[test]
    fn static_representations_are_exact_and_negotiated() {
        for asset in [INDEX, WARM, CALENDAR_JS, CALENDAR_CSS, ADMIN, ADMIN_JS, ADMIN_CSS] {
            let mut body = Vec::new();
            flate2::read::GzDecoder::new(asset.gzip)
                .read_to_end(&mut body)
                .unwrap();
            assert_eq!(body, asset.identity);
        }
        for (value, wanted) in [
            ("", Some(Encoding::Identity)),
            ("br, gzip, deflate", Some(Encoding::Brotli)),
            ("gzip;", Some(Encoding::Identity)),
            ("x-gzip", Some(Encoding::Gzip)),
            ("*;q=0.7, gzip;q=0", Some(Encoding::Brotli)),
            ("identity;q=0.7, gzip;q=0.6", Some(Encoding::Identity)),
            ("gzip;q=0.7, identity;q=0.6", Some(Encoding::Gzip)),
            ("gzip;q=bogus, *;q=1", Some(Encoding::Brotli)),
            ("*;q=0, identity;q=1", Some(Encoding::Identity)),
            ("gzip;q=1, *;q=0", Some(Encoding::Gzip)),
            ("gzip;q=0.2, GZIP;q=0", Some(Encoding::Identity)),
            ("*;q=0", None),
            ("br, identity;q=0", Some(Encoding::Brotli)),
            ("br;q=0, gzip;q=0, identity;q=0", None),
            ("br;q=0.3, gzip;q=0.5", Some(Encoding::Gzip)),
            ("BR;q=0.2, br;q=0, identity;q=0.1", Some(Encoding::Identity)),
        ] {
            let mut headers = HeaderMap::new();
            headers.append(header::ACCEPT_ENCODING, value.parse().unwrap());
            assert_eq!(preferred(&headers), wanted, "{value}");
        }
        let mut headers = HeaderMap::new();
        assert_eq!(preferred(&headers), Some(Encoding::Identity));
        headers.append(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
        headers.append(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("GZIP;Q=1.000"),
        );
        assert_eq!(preferred(&headers), Some(Encoding::Brotli));
    }
}
