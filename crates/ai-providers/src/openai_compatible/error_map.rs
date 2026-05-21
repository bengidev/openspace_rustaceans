//! HTTP status → [`AiError`] mapping for the industry-standard
//! chat-completions wire format.
//!
//! Pulled out of [`super::provider`] so the categorisation rules live
//! in one place — every status the issue's acceptance criteria pin
//! (400, 401, 429 with/without `Retry-After`, 500) is exercised by
//! the unit tests at the bottom of this file.
//!
//! Mapping rules:
//!
//! - **400** → [`AiError::InvalidRequest`]. Carries the upstream
//!   `error.message` when the body parses; otherwise the raw body
//!   preview.
//! - **401 / 403** → [`AiError::Auth`]. The two statuses are
//!   collapsed because every "credential refused" outcome is the
//!   same recovery (re-enter the API key).
//! - **404** → [`AiError::ModelNotFound`] when the wire format's
//!   error type advertises a missing-model condition; otherwise
//!   [`AiError::InvalidRequest`]. This slice does not parse the
//!   `type` field, so 404 is conservatively mapped to
//!   `InvalidRequest`.
//! - **408 / 504** → [`AiError::Network`] (transient transport-side
//!   timeout from the upstream).
//! - **429** → [`AiError::RateLimited`] with `retry_after` populated
//!   from the `Retry-After` header when present.
//! - **5xx (any other)** → [`AiError::Network`] without re-encoding.
//!   The agent loop's retry layer handles back-off.
//!
//! The `Retry-After` header parser supports both shapes the HTTP
//! spec defines: a decimal seconds value (`Retry-After: 30`) and an
//! HTTP-date (`Retry-After: Wed, 21 Oct 2026 07:28:00 GMT`). The
//! latter is converted to a duration relative to "now"; if "now" is
//! already past the date the duration is `Some(Duration::ZERO)`
//! rather than `None` so callers can distinguish a "no header" case
//! from a "header advertised an expired window" case.

use std::time::Duration;

use openspace_shared::ai::error::AiError;
use reqwest::header::HeaderMap;

use super::wire::ErrorEnvelope;

/// Map a non-2xx response into the categorised [`AiError`] variant
/// the agent loop expects.
pub(super) fn map_status(status: u16, headers: &HeaderMap, body: &[u8]) -> AiError {
    match status {
        400 => AiError::InvalidRequest(extract_message(body, "bad request")),
        401 | 403 => AiError::Auth(extract_message(body, "credentials rejected")),
        404 => AiError::InvalidRequest(extract_message(body, "endpoint or model not found")),
        408 | 504 => AiError::Network(format!(
            "upstream timeout (HTTP {status}): {}",
            extract_message(body, "no body")
        )),
        429 => AiError::RateLimited {
            retry_after: parse_retry_after(headers),
        },
        500..=599 => AiError::Network(format!(
            "server error (HTTP {status}): {}",
            extract_message(body, "no body")
        )),
        _ => AiError::Other(format!(
            "unexpected status {status}: {}",
            extract_message(body, "no body")
        )),
    }
}

/// Pull the human-readable message out of the upstream error
/// envelope, falling back to a UTF-8 preview of the raw body when
/// the envelope does not parse. The fallback is bounded at 512
/// bytes so a wayward HTML error page does not flood the agent
/// loop's tool-result surface.
pub(super) fn extract_message(body: &[u8], default: &str) -> String {
    if body.is_empty() {
        return default.to_string();
    }
    if let Ok(env) = serde_json::from_slice::<ErrorEnvelope>(body) {
        return env.error.message;
    }
    let take = body.len().min(512);
    String::from_utf8_lossy(&body[..take]).into_owned()
}

/// Read `Retry-After` and convert to a [`Duration`].
///
/// The parser tolerates the two shapes the HTTP spec allows
/// (decimal seconds; HTTP-date) and a third shape some upstream
/// implementations emit in the wild (a floating-point seconds
/// value). The HTTP-date branch uses [`chrono`] so we do not have
/// to hand-roll RFC 1123 parsing — `chrono::DateTime::parse_from_rfc2822`
/// covers the canonical shape and the few wire variants seen in
/// practice.
pub(super) fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let trimmed = raw.trim();

    // Integer seconds — the canonical shape.
    if let Ok(secs) = trimmed.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    // Floating-point seconds — non-standard but observed.
    if let Ok(secs) = trimmed.parse::<f64>() {
        if secs.is_finite() && secs >= 0.0 {
            return Some(Duration::from_secs_f64(secs));
        }
    }
    // HTTP-date — chrono parses RFC 2822 / RFC 1123 directly.
    if let Ok(when) = chrono::DateTime::parse_from_rfc2822(trimmed) {
        let now = chrono::Utc::now();
        let delta = when.with_timezone(&chrono::Utc) - now;
        return Some(delta.to_std().unwrap_or(Duration::ZERO));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    fn headers_with_retry_after(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            reqwest::header::RETRY_AFTER,
            HeaderValue::from_str(value).expect("static header value"),
        );
        h
    }

    #[test]
    fn maps_400_to_invalid_request_and_extracts_envelope_message() {
        let body =
            br#"{"error":{"message":"temperature out of range","type":"invalid_request_error"}}"#;
        let err = map_status(400, &HeaderMap::new(), body);
        assert!(
            matches!(err, AiError::InvalidRequest(ref m) if m == "temperature out of range"),
            "got {err:?}"
        );
    }

    #[test]
    fn maps_401_and_403_to_auth() {
        for status in [401u16, 403u16] {
            let err = map_status(status, &HeaderMap::new(), b"{}");
            assert!(matches!(err, AiError::Auth(_)), "{status} -> {err:?}");
        }
    }

    #[test]
    fn maps_429_with_retry_after_seconds() {
        let headers = headers_with_retry_after("42");
        let err = map_status(429, &headers, b"");
        assert!(
            matches!(
                err,
                AiError::RateLimited { retry_after: Some(d) } if d == Duration::from_secs(42)
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn maps_429_without_retry_after_to_none() {
        let err = map_status(429, &HeaderMap::new(), b"");
        assert!(
            matches!(err, AiError::RateLimited { retry_after: None }),
            "got {err:?}"
        );
    }

    #[test]
    fn maps_500_to_network() {
        let err = map_status(500, &HeaderMap::new(), b"oops");
        assert!(
            matches!(err, AiError::Network(ref m) if m.contains("500")),
            "got {err:?}"
        );
    }

    #[test]
    fn extract_message_falls_back_to_body_preview() {
        let msg = extract_message(b"<html>boom</html>", "default");
        assert!(msg.contains("boom"), "got {msg:?}");
    }

    #[test]
    fn extract_message_returns_default_for_empty_body() {
        assert_eq!(extract_message(b"", "fallback"), "fallback");
    }

    #[test]
    fn parse_retry_after_handles_floating_point() {
        let headers = headers_with_retry_after("1.5");
        let parsed = parse_retry_after(&headers).expect("parsed");
        assert!(parsed >= Duration::from_millis(1500));
        assert!(parsed <= Duration::from_millis(1600));
    }

    #[test]
    fn parse_retry_after_handles_http_date_in_the_future() {
        // Date a long way in the future — the delta must be positive
        // even after chrono's internal arithmetic.
        let headers = headers_with_retry_after("Wed, 21 Oct 2099 07:28:00 GMT");
        let parsed = parse_retry_after(&headers).expect("parsed");
        assert!(parsed > Duration::from_secs(60));
    }

    #[test]
    fn parse_retry_after_handles_expired_http_date_as_zero() {
        let headers = headers_with_retry_after("Wed, 21 Oct 1970 07:28:00 GMT");
        let parsed = parse_retry_after(&headers).expect("parsed");
        assert_eq!(parsed, Duration::ZERO);
    }
}
