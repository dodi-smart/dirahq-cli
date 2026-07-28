//! Shared 429 (rate-limit) response parsing (WP-B6).
//!
//! The cloud pairs a standard `Retry-After` header with a typed
//! `{"error":"rate_limited","retryAfterSecs":n}` JSON body on every
//! device→cloud route (ingest/presence/billing) — see
//! `cloud/src/lib/rate-limit.ts::rateLimitedResponse`. Header first (cheap,
//! doesn't need the body read), the JSON body as a fallback for whichever
//! caller can't/didn't read headers.

/// Parse a `Retry-After` header value (seconds form only — the cloud never
/// sends the HTTP-date form) into a second count. `None` on anything that
/// doesn't parse as a plain non-negative integer.
pub fn parse_retry_after_secs(header_value: &str) -> Option<u64> {
    header_value.trim().parse::<u64>().ok()
}

/// Parse the cloud's typed rate-limit body for `retryAfterSecs`, tolerant of
/// any other shape (a non-JSON or differently-shaped body simply yields `None`
/// — callers fall back to their own default backoff/cadence).
pub fn parse_retry_after_body(body: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("retryAfterSecs").and_then(|n| n.as_u64()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_form_parses_plain_seconds() {
        assert_eq!(parse_retry_after_secs("30"), Some(30));
        assert_eq!(parse_retry_after_secs(" 5 "), Some(5));
        assert_eq!(parse_retry_after_secs("0"), Some(0));
    }

    #[test]
    fn header_form_rejects_non_numeric_or_http_date() {
        // The cloud never sends the HTTP-date form, but a header parser must
        // not panic or misparse it as a small number.
        assert_eq!(
            parse_retry_after_secs("Wed, 21 Oct 2026 07:28:00 GMT"),
            None
        );
        assert_eq!(parse_retry_after_secs(""), None);
        assert_eq!(parse_retry_after_secs("abc"), None);
    }

    #[test]
    fn body_form_parses_typed_rate_limited_error() {
        assert_eq!(
            parse_retry_after_body(r#"{"error":"rate_limited","retryAfterSecs":42}"#),
            Some(42)
        );
    }

    #[test]
    fn body_form_tolerates_missing_or_garbage() {
        assert_eq!(parse_retry_after_body(r#"{"error":"rate_limited"}"#), None);
        assert_eq!(parse_retry_after_body("not json"), None);
        assert_eq!(parse_retry_after_body(""), None);
    }
}
