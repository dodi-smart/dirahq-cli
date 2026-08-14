//! Shared retry pacing: 429 (rate-limit) response parsing (WP-B6) and the one
//! exponential-backoff ladder every retrying caller waits on.
//!
//! The cloud pairs a standard `Retry-After` header with a typed
//! `{"error":"rate_limited","retryAfterSecs":n}` JSON body on every
//! device→cloud route (ingest/presence/billing) — see
//! `cloud/src/lib/rate-limit.ts::rateLimitedResponse`. Header first (cheap,
//! doesn't need the body read), the JSON body as a fallback for whichever
//! caller can't/didn't read headers.
//!
//! Everything here is pure timing — no payload ever passes through it, so it
//! stays clear of D-0001's content-free wire invariant.

use std::time::Duration;

/// Parse a `Retry-After` header value (seconds form only — the cloud never
/// sends the HTTP-date form) into a second count. `None` on anything that
/// doesn't parse as a plain non-negative integer.
pub fn parse_retry_after_secs(header_value: &str) -> Option<u64> {
    header_value.trim().parse::<u64>().ok()
}

/// An exponential-backoff ladder: `seed`, doubling, capped at `max`.
///
/// One implementation for both retrying callers (DIRASH-0031). They differ in
/// how long they are willing to wait, and in whether they give up at all —
/// `dirad::sync` retries the cloud indefinitely in the background on a 2s/300s
/// ladder, while `dira update` is an interactive command a human is watching
/// and gives up after a bounded number of attempts on a 500ms/4s one — but the
/// shape of the wait is identical. Two copies of it had already drifted (one
/// capped only the `Retry-After` branch, the other capped both) precisely
/// because nothing forced them to agree.
///
/// The attempt budget deliberately does NOT live here: "how patient is this
/// ladder" and "how many times is this worth trying at all" are different
/// questions, and only the caller knows the second one.
///
/// Pure, so the cap and the override-vs-ladder choice stay unit-testable
/// without a network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// First wait after a failed attempt; doubles from there.
    pub seed: Duration,
    /// Ceiling on any single wait, including a server-supplied `Retry-After`.
    pub max: Duration,
}

impl Backoff {
    /// The next wait after `current`: [`Self::seed`] from zero, then doubling,
    /// capped at [`Self::max`].
    pub fn next(&self, current: Duration) -> Duration {
        let next = if current.is_zero() {
            self.seed
        } else {
            current * 2
        };
        next.min(self.max)
    }

    /// The wait before retrying a transient failure: the server's `Retry-After`
    /// when it sent one, else the ladder off `current`.
    ///
    /// Both branches are capped at [`Self::max`]. The ladder half is redundant
    /// today because [`Self::next`] already caps — but writing it
    /// unconditionally is what stops the two arms drifting apart again, and a
    /// misbehaving or hostile `Retry-After` must never be able to wedge a
    /// caller indefinitely.
    pub fn transient_wait(&self, retry_after: Option<Duration>, current: Duration) -> Duration {
        retry_after
            .unwrap_or_else(|| self.next(current))
            .min(self.max)
    }
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

    /// The daemon's ladder, so the shared type is exercised on both callers'
    /// real numbers rather than on invented ones.
    const SYNC: Backoff = Backoff {
        seed: Duration::from_secs(2),
        max: Duration::from_secs(300),
    };

    /// The updater's ladder — an interactive command, capped in seconds.
    const UPDATE: Backoff = Backoff {
        seed: Duration::from_millis(500),
        max: Duration::from_secs(4),
    };

    #[test]
    fn the_ladder_seeds_then_doubles_then_caps() {
        assert_eq!(SYNC.next(Duration::ZERO), Duration::from_secs(2));
        assert_eq!(SYNC.next(Duration::from_secs(2)), Duration::from_secs(4));
        assert_eq!(SYNC.next(Duration::from_secs(4)), Duration::from_secs(8));
        assert_eq!(SYNC.next(Duration::from_secs(256)), SYNC.max);
        // Already at the cap, and past it, both stay at the cap.
        assert_eq!(SYNC.next(SYNC.max), SYNC.max);
        assert_eq!(SYNC.next(Duration::from_secs(10_000)), SYNC.max);
    }

    #[test]
    fn each_caller_keeps_its_own_seed_and_cap() {
        assert_eq!(UPDATE.next(Duration::ZERO), Duration::from_millis(500));
        assert_eq!(
            UPDATE.next(Duration::from_millis(500)),
            Duration::from_secs(1)
        );
        assert_eq!(UPDATE.next(Duration::from_secs(2)), Duration::from_secs(4));
        assert_eq!(UPDATE.next(Duration::from_secs(4)), UPDATE.max);
    }

    #[test]
    fn retry_after_overrides_the_ladder_but_is_still_capped() {
        // Honoured when it is shorter than the cap...
        assert_eq!(
            SYNC.transient_wait(Some(Duration::from_secs(30)), Duration::from_secs(2)),
            Duration::from_secs(30)
        );
        // ...and clamped when a server (or an attacker) sends something absurd.
        assert_eq!(
            SYNC.transient_wait(Some(Duration::from_secs(86_400)), Duration::ZERO),
            SYNC.max
        );
        assert_eq!(
            UPDATE.transient_wait(Some(Duration::from_secs(3_600)), Duration::ZERO),
            UPDATE.max
        );
    }

    #[test]
    fn without_a_retry_after_the_wait_is_the_ladder() {
        assert_eq!(
            SYNC.transient_wait(None, Duration::ZERO),
            SYNC.next(Duration::ZERO)
        );
        assert_eq!(
            SYNC.transient_wait(None, Duration::from_secs(8)),
            Duration::from_secs(16)
        );
    }

    /// The drift this type exists to remove: the two former implementations
    /// disagreed on whether the ladder branch was capped, and were equivalent
    /// only because the ladder happened to be pre-capped. Assert the property
    /// directly so neither arm can regress.
    #[test]
    fn both_arms_are_capped_whatever_the_input() {
        for ladder in [SYNC, UPDATE] {
            for current in [
                Duration::ZERO,
                Duration::from_millis(1),
                ladder.max,
                Duration::from_secs(100_000),
            ] {
                assert!(ladder.next(current) <= ladder.max);
                assert!(ladder.transient_wait(None, current) <= ladder.max);
                assert!(
                    ladder.transient_wait(Some(Duration::from_secs(999_999)), current)
                        <= ladder.max
                );
            }
        }
    }
}
