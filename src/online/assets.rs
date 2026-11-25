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
}

macro_rules! asset {
    ($file:literal, $mime:literal) => {
        Asset {
            media_type: $mime,
            identity: include_bytes!($file),
            gzip: include_bytes!(concat!(env!("OUT_DIR"), "/", $file, ".gz")),
        }
    };
}

pub(crate) const INDEX: Asset = asset!("index.html", "text/html; charset=utf-8");
pub(crate) const CALENDAR_JS: Asset = asset!("calendar.js", "text/javascript; charset=utf-8");
pub(crate) const CALENDAR_CSS: Asset = asset!("calendar.css", "text/css; charset=utf-8");
pub(crate) const ADMIN: Asset = asset!("admin.html", "text/html; charset=utf-8");
pub(crate) const ADMIN_JS: Asset = asset!("admin.js", "text/javascript; charset=utf-8");
pub(crate) const ADMIN_CSS: Asset = asset!("admin.css", "text/css; charset=utf-8");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Encoding {
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
                "*" => &mut wildcard,
                _ => continue,
            };
            // In a contradictory duplicate, honor the more restrictive preference.
            *slot = Some(slot.unwrap_or(quality).min(quality));
        }
    }
    let compressed = gzip.or(wildcard).unwrap_or(0);
    if compressed > 0 && compressed >= identity.unwrap_or(0) {
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
            Some(Encoding::Gzip) => (
                [
                    (header::CONTENT_TYPE, self.media_type),
                    (header::CONTENT_ENCODING, "gzip"),
                ],
                self.gzip,
            )
                .into_response(),
            Some(Encoding::Identity) => {
                ([(header::CONTENT_TYPE, self.media_type)], self.identity).into_response()
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
    fn static_representations_are_exact_and_negotiated() {
        for asset in [INDEX, CALENDAR_JS, CALENDAR_CSS, ADMIN, ADMIN_JS, ADMIN_CSS] {
            let mut body = Vec::new();
            flate2::read::GzDecoder::new(asset.gzip)
                .read_to_end(&mut body)
                .unwrap();
            assert_eq!(body, asset.identity);
        }
        for (value, wanted) in [
            ("", Some(Encoding::Identity)),
            ("br, gzip, deflate", Some(Encoding::Gzip)),
            ("gzip;", Some(Encoding::Identity)),
            ("x-gzip", Some(Encoding::Gzip)),
            ("*;q=0.7, gzip;q=0", Some(Encoding::Identity)),
            ("identity;q=0.7, gzip;q=0.6", Some(Encoding::Identity)),
            ("gzip;q=0.7, identity;q=0.6", Some(Encoding::Gzip)),
            ("gzip;q=bogus, *;q=1", Some(Encoding::Identity)),
            ("*;q=0, identity;q=1", Some(Encoding::Identity)),
            ("gzip;q=1, *;q=0", Some(Encoding::Gzip)),
            ("gzip;q=0.2, GZIP;q=0", Some(Encoding::Identity)),
            ("*;q=0", None),
            ("br, identity;q=0", None),
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
        assert_eq!(preferred(&headers), Some(Encoding::Gzip));
    }
}
