//! Loopback HTTP ingress for harness http-hooks.
//!
//! **Hot path rule:** authenticate → normalize → non-blocking enqueue → 200 OK.
//! No git resolution, no DB write, no accounting happens here — all of that is
//! done by the writer task off the response path, so the agent loop never waits.
//!
//! The route is harness-generic: `POST /hooks/{harness}` dispatches the body
//! through the [`dira_sources`] registry, so adding a harness needs no change
//! here. `/hooks/claude` is just `:harness=claude`.

use crate::state::{AppState, EventMsg};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use time::OffsetDateTime;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/hooks/{harness}", post(hook))
        .with_state(state)
}

/// Receive one harness hook. Returns 200 the instant it's enqueued (or safely
/// dropped/ignored). The `harness` path segment selects the source.
async fn hook(
    State(state): State<AppState>,
    Path(harness): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    // Defense-in-depth ingress guard: reject any request carrying a cross-origin
    // `Origin`. A browser attaches `Origin` on cross-site `fetch`, so a malicious
    // page that learned the loopback port can't drive this endpoint; legitimate
    // hook posters (the harness, `curl`) don't set `Origin` at all. The bearer
    // below stays the primary gate — this only closes the drive-by-browser hole.
    if !origin_allowed(&headers) {
        return StatusCode::FORBIDDEN;
    }
    if !authorized(&headers, &state.bearer) {
        return StatusCode::UNAUTHORIZED;
    }
    let Some((norm, harness_kind)) = dira_sources::normalize_for(&harness, payload) else {
        // Unknown harness or unknown/ignored hook — ack so the harness doesn't retry.
        return StatusCode::OK;
    };

    // Stamp arrival time here (cheap); enrichment happens in the writer.
    let msg = EventMsg::Hook {
        norm,
        harness: harness_kind,
        at: OffsetDateTime::now_utc(),
    };

    // Non-blocking: if the queue is full we drop rather than stall the agent loop.
    match state.tx.try_send(msg) {
        Ok(()) => StatusCode::OK,
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            tracing::warn!("ingest queue full; dropped a hook event");
            StatusCode::OK
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// Bearer check against the `Authorization: Bearer <token>` header, using a
/// constant-time comparison so a timing side-channel can't be used to recover the
/// token byte-by-byte.
fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| constant_time_eq(t.as_bytes(), expected.as_bytes()))
        .unwrap_or(false)
}

/// Constant-time byte-slice equality. Tiny manual implementation (avoids pulling
/// in a crate). The scan length is fixed by `expected` (the secret), not by the
/// attacker-supplied input, so the running time doesn't reveal how many leading
/// bytes matched. A length mismatch is folded into the accumulator (not
/// short-circuited), so it changes the *result* without changing the timing
/// profile. Note: like all such helpers this leaks the secret's *length*, which
/// for a fixed-length ULID bearer is not sensitive.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    // A non-zero seed whenever the lengths differ, so a mismatch is guaranteed
    // regardless of byte content; 0 when the lengths are equal.
    let mut diff: u8 = if a.len() == b.len() { 0 } else { 1 };
    // Iterate over the secret's length so the loop count is independent of the
    // input. Index `a` modulo its length (0 when empty) so we never panic; the
    // length seed already forces a mismatch when the lengths differ.
    for (i, &be) in b.iter().enumerate() {
        let ae = if a.is_empty() { 0 } else { a[i % a.len()] };
        diff |= ae ^ be;
    }
    diff == 0
}

/// True unless the request carries a cross-origin `Origin` header. A missing
/// `Origin` (the common case — CLIs and the harness don't send one) is allowed;
/// an `Origin` is allowed only when it points at loopback (`localhost` /
/// `127.0.0.1` / `[::1]`, any scheme/port). Anything else is a cross-site browser
/// request and is rejected.
fn origin_allowed(headers: &HeaderMap) -> bool {
    let Some(origin) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    else {
        return true; // no Origin → not a browser cross-site request
    };
    is_loopback_origin(origin)
}

/// Whether an `Origin` value's host is loopback. Parses out the host between the
/// `scheme://` and any `:port`, tolerating IPv6 brackets.
fn is_loopback_origin(origin: &str) -> bool {
    // Strip the scheme.
    let after_scheme = origin
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(origin);
    // Host is up to the first `/` (there shouldn't be a path on an Origin, but be safe).
    let authority = after_scheme.split('/').next().unwrap_or(after_scheme);
    // Drop an IPv6 bracketed host's port, or an IPv4/host's `:port`.
    let host = if let Some(rest) = authority.strip_prefix('[') {
        // `[::1]:port` → `::1`
        rest.split(']').next().unwrap_or(rest)
    } else {
        authority.split(':').next().unwrap_or(authority)
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::{AUTHORIZATION, ORIGIN};

    #[test]
    fn constant_time_eq_matches_only_on_equal_bytes() {
        assert!(constant_time_eq(b"secret-token", b"secret-token"));
        assert!(!constant_time_eq(b"secret-token", b"secret-tokeX"));
        assert!(!constant_time_eq(b"short", b"longer-token"));
        assert!(!constant_time_eq(b"longer-token", b"short"));
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(!constant_time_eq(b"x", b""));
    }

    #[test]
    fn authorized_requires_exact_bearer() {
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, "Bearer s3cr3t".parse().unwrap());
        assert!(authorized(&h, "s3cr3t"));
        assert!(!authorized(&h, "s3cr3X"));
        assert!(!authorized(&h, "s3cr3t-longer"));

        // Missing / malformed headers are unauthorized.
        let empty = HeaderMap::new();
        assert!(!authorized(&empty, "s3cr3t"));
        let mut wrong_scheme = HeaderMap::new();
        wrong_scheme.insert(AUTHORIZATION, "Basic s3cr3t".parse().unwrap());
        assert!(!authorized(&wrong_scheme, "s3cr3t"));
    }

    #[test]
    fn origin_allowed_only_for_missing_or_loopback() {
        // No Origin (CLI / harness poster) → allowed.
        assert!(origin_allowed(&HeaderMap::new()));

        for ok in [
            "http://localhost",
            "http://localhost:8765",
            "http://127.0.0.1:8765",
            "https://localhost",
            "http://[::1]:8765",
        ] {
            let mut h = HeaderMap::new();
            h.insert(ORIGIN, ok.parse().unwrap());
            assert!(origin_allowed(&h), "expected {ok} to be allowed");
        }
        for bad in [
            "http://evil.example",
            "https://evil.example:8765",
            "http://127.0.0.1.evil.com",
            "null",
        ] {
            let mut h = HeaderMap::new();
            h.insert(ORIGIN, bad.parse().unwrap());
            assert!(!origin_allowed(&h), "expected {bad} to be rejected");
        }
    }
}
