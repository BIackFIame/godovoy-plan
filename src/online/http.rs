use axum::{
    Json,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::json;

pub(crate) fn problem(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": message}))).into_response()
}

pub(crate) fn add_common_security_headers(headers: &mut HeaderMap) {
    for (name, value) in [
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("x-frame-options", "DENY"),
        ("cross-origin-resource-policy", "same-origin"),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
}

pub(crate) fn is_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("application/json"))
}

pub(crate) fn is_strong_state_tag(tag: &str) -> bool {
    tag.strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .is_some_and(|digest| {
            digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_media_type_keeps_header_map_parsing_rules() {
        let mut headers = HeaderMap::new();
        assert!(!is_json_content_type(&headers));

        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("Application/JSON ; charset=utf-8"),
        );
        assert!(is_json_content_type(&headers));

        headers.append(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        assert!(is_json_content_type(&headers));

        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json, text/plain"),
        );
        assert!(!is_json_content_type(&headers));
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        assert!(!is_json_content_type(&headers));
    }

    #[test]
    fn strong_state_tag_requires_quoted_64_digit_hex() {
        let tag = format!("\"{}\"", "aB09".repeat(16));
        assert!(is_strong_state_tag(&tag));
        assert!(!is_strong_state_tag(""));
        assert!(!is_strong_state_tag(&format!("W/{tag}")));
        assert!(!is_strong_state_tag(&format!("\"{}\"", "a".repeat(63))));
        assert!(!is_strong_state_tag(&format!("\"{}\"", "g".repeat(64))));
        assert!(!is_strong_state_tag(&format!("{tag}extra")));
    }

    #[test]
    fn common_security_headers_leave_listener_policies_alone() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-security-policy",
            HeaderValue::from_static("default-src 'none'"),
        );
        headers.insert(
            "cross-origin-opener-policy",
            HeaderValue::from_static("same-origin"),
        );

        add_common_security_headers(&mut headers);

        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(headers["referrer-policy"], "no-referrer");
        assert_eq!(headers["x-frame-options"], "DENY");
        assert_eq!(headers["cross-origin-resource-policy"], "same-origin");
        assert_eq!(headers["content-security-policy"], "default-src 'none'");
        assert_eq!(headers["cross-origin-opener-policy"], "same-origin");
    }
}
