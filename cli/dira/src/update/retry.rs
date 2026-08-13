//! Bounded retry policy for the updater's network calls.
//!
//! Deliberately a local mirror of `dirad::sync`'s `next_backoff` /
//! `transient_wait` ladder rather than a shared helper: `dira` does not depend
//! on `dirad` (see `cli/dira/Cargo.toml`), and the two want different caps
//! anyway. The daemon retries in the background and can afford `MAX_BACKOFF`
//! of 300s; `dira update` is an interactive command a human is watching, so
//! this ladder is capped in seconds. Keeping the *shape* identical is the
//! point — anyone who has read one can read the other.
//!
//! Everything here is pure, so the classification rules and the cap are
//! unit-testable without a network (mirrors `sync.rs`'s tests for the same
//! helpers).

use std::time::Duration;

/// Ceiling on establishing the TCP+TLS connection, shared by every request the
/// updater makes.
pub(super) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Ceiling on a GitHub Releases API call. Short — these are small JSON
/// responses, and `--check` runs speculatively from a detached background
/// refresh where a long hang is worse than a miss.
pub(super) const API_TIMEOUT: Duration = Duration::from_secs(30);

/// What to do about a failed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Disposition {
    /// Transient — worth another attempt.
    Retry,
    /// Deterministic — another attempt would fail identically.
    Fatal,
}

/// Classify an HTTP status on a download response.
///
/// 429 and 5xx are the server telling us to come back; every other
/// non-success is deterministic and must fail immediately. In particular a
/// 404 is never retried — the caller special-cases it with a clearer message
/// ("asset not found on that release"), and hammering it would only delay
/// that. 401/403 are equally pointless to repeat: the credential (or the
/// anonymous rate limit) will not change between attempts.
pub(super) fn classify_status(status: reqwest::StatusCode) -> Disposition {
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        Disposition::Retry
    } else {
        Disposition::Fatal
    }
}

/// Classify a transport-level `reqwest` error.
///
/// This is the case the whole module exists for: `connection closed before
/// message completed` — the stream dying part-way through a body that had
/// already started arriving. Connect failures, timeouts, and truncated bodies
/// are all worth another attempt.
///
/// Two are not. A builder error is a bug in our own request construction, and
/// a redirect error means the redirect budget was exhausted — a property of
/// the server's configuration, identical next time.
pub(super) fn classify_transport(err: &reqwest::Error) -> Disposition {
    if err.is_builder() || err.is_redirect() {
        Disposition::Fatal
    } else {
        Disposition::Retry
    }
}

/// Read a `Retry-After` delay from response headers.
///
/// Only the delta-seconds form is honoured. The HTTP-date form is legal but
/// GitHub does not send it, and parsing dates here would mean trusting the
/// client's clock to compute a delay that [`Policy::transient_wait`] caps at a
/// few seconds regardless — not worth the dependency or the skew bug.
pub(super) fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let secs: u64 = raw.trim().parse().ok()?;
    Some(Duration::from_secs(secs))
}

/// How hard to try, how long to wait between tries, and how long one try may
/// take. A value rather than constants so tests can run the identical loop on
/// a millisecond ladder instead of adding seconds of real sleeping to the
/// suite — the production numbers live in [`Policy::download`].
#[derive(Debug, Clone, Copy)]
pub(super) struct Policy {
    /// Total attempts, i.e. the first try plus this many minus one retries.
    pub attempts: u32,
    /// First wait after a failed attempt; doubles from there.
    pub seed: Duration,
    /// Ceiling on any single wait, including a server-supplied `Retry-After`,
    /// so a misbehaving header can't wedge an interactive command.
    pub max_backoff: Duration,
    /// Ceiling on one attempt, applied per attempt rather than per download —
    /// so the retry budget multiplies it.
    pub timeout: Duration,
}

impl Policy {
    /// The production policy for artifact downloads.
    ///
    /// Four attempts is sized against the failure this exists for: a single
    /// mid-stream abort on a lossy link, which clears on the very next
    /// attempt. More attempts would mostly add latency to genuinely-down
    /// cases. The 120s per-attempt timeout is generous because the largest
    /// published artifact is ~20MB and this must not fire on a slow-but-working
    /// link — its job is bounding the *hang* case (a connection that stalls
    /// instead of closing), which before this had no bound at all.
    pub(super) const fn download() -> Self {
        Self {
            attempts: 4,
            seed: Duration::from_millis(500),
            max_backoff: Duration::from_secs(4),
            timeout: Duration::from_secs(120),
        }
    }

    /// The exponential ladder: [`Self::seed`] seed, doubling, capped at
    /// [`Self::max_backoff`]. Mirrors `dirad::sync::next_backoff`.
    pub(super) fn next_backoff(&self, current: Duration) -> Duration {
        let next = if current.is_zero() {
            self.seed
        } else {
            current * 2
        };
        next.min(self.max_backoff)
    }

    /// The wait before retrying a transient failure: the server's
    /// `Retry-After` when present, else the ladder off `current` — either way
    /// capped at [`Self::max_backoff`]. Mirrors `dirad::sync::transient_wait`.
    pub(super) fn transient_wait(
        &self,
        retry_after: Option<Duration>,
        current: Duration,
    ) -> Duration {
        retry_after
            .unwrap_or_else(|| self.next_backoff(current))
            .min(self.max_backoff)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
    use reqwest::StatusCode;

    #[test]
    fn server_errors_and_429_are_retried() {
        for status in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::TOO_MANY_REQUESTS,
        ] {
            assert_eq!(
                classify_status(status),
                Disposition::Retry,
                "{status} should be retried"
            );
        }
    }

    /// The regression guard for the rule in this module's doc: a missing asset
    /// is deterministic, and retrying it only delays the clear message.
    #[test]
    fn client_errors_are_never_retried() {
        for status in [
            StatusCode::NOT_FOUND,
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::BAD_REQUEST,
            StatusCode::GONE,
        ] {
            assert_eq!(
                classify_status(status),
                Disposition::Fatal,
                "{status} must not be retried"
            );
        }
    }

    #[test]
    fn backoff_ladder_seeds_doubles_and_caps() {
        let p = Policy::download();
        assert_eq!(p.next_backoff(Duration::ZERO), p.seed);
        assert_eq!(p.next_backoff(p.seed), p.seed * 2);
        assert_eq!(
            p.next_backoff(Duration::from_secs(1)),
            Duration::from_secs(2)
        );
        // ...and never grows past the cap, however long the ladder runs.
        assert_eq!(p.next_backoff(p.max_backoff), p.max_backoff);
        assert_eq!(p.next_backoff(Duration::from_secs(600)), p.max_backoff);
    }

    #[test]
    fn retry_after_overrides_the_ladder_but_is_still_capped() {
        let p = Policy::download();
        assert_eq!(
            p.transient_wait(Some(Duration::from_secs(1)), Duration::from_secs(2)),
            Duration::from_secs(1)
        );
        assert_eq!(p.transient_wait(None, Duration::ZERO), p.seed);
        // A huge Retry-After must not wedge an interactive command.
        assert_eq!(
            p.transient_wait(Some(Duration::from_secs(999_999)), Duration::ZERO),
            p.max_backoff
        );
    }

    #[test]
    fn retry_after_parses_delta_seconds_only() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("7"));
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(7)));

        // The HTTP-date form is deliberately ignored, not mis-parsed as 0.
        let mut headers = HeaderMap::new();
        headers.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT"),
        );
        assert_eq!(parse_retry_after(&headers), None);

        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
    }
}
