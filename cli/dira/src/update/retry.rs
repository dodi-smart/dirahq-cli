//! Bounded retry policy for the updater's network calls.
//!
//! The backoff ladder itself is `dira_core::sync::Backoff`, shared with
//! `dirad::sync` (DIRASH-0031). This module used to carry a deliberate local
//! *copy* of it, on the reasoning that `dira` does not depend on `dirad` and
//! the two want different caps — both still true, but neither needs a second
//! implementation: the caps are values, and both crates already depend on
//! `dira_core`. The two copies had in fact drifted (this one capped only the
//! `Retry-After` branch), which is what the shared type removes.
//!
//! What stays here is what is genuinely the updater's: an attempt budget (the
//! daemon retries indefinitely; an interactive command must give up), a
//! per-attempt timeout, the classification of which failures are worth another
//! try at all, and [`with_retry`] — the one loop both of `dira update`'s
//! network hops run on.
//!
//! [`with_retry`] is deliberately scoped to this crate. DIRASH-0031 rejected
//! sharing a *loop* with `dirad::sync`, whose typed `SyncError` arms have no
//! analogue here; sharing one between this crate's own two GETs is a different
//! question, and the answer is yes — see [`Attempt`].
//!
//! Everything except [`with_retry`] is pure, so the classification rules and
//! the ladder are unit-testable without a network.

use std::future::Future;
use std::time::Duration;

use anyhow::Result;

/// Ceiling on establishing the TCP+TLS connection, shared by every request the
/// updater makes.
pub(super) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Ceiling on the gap between successful reads of a response body — an
/// *inactivity* timeout, applied client-wide via `ClientBuilder::read_timeout`
/// (reqwest 0.12.9+; this workspace pins 0.12.28). It resets on every read
/// that makes progress, so unlike a total-transfer timeout it is safe to
/// share across every call this client makes regardless of payload size: a
/// connection that has stalled is stalled whether it was serving a small
/// JSON response or a ~20MB archive. This is what actually bounds the *hang*
/// case; [`Policy::download`]'s `timeout` field is a much longer backstop on
/// the whole request, not the thing that catches a stall.
pub(super) const READ_TIMEOUT: Duration = Duration::from_secs(30);

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
/// The seconds-only parse itself lives in `dira_core::sync` — shared with the
/// daemon's rate-limit handling, and already tested there (including that the
/// legal-but-unused HTTP-date form yields `None` rather than a misparsed small
/// number).
pub(super) fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()
        .and_then(dira_core::sync::parse_retry_after_secs)
        .map(Duration::from_secs)
}

/// How hard to try, how long to wait between tries, and how long one try may
/// take. A value rather than constants so tests can run the identical loop on
/// a millisecond ladder instead of adding seconds of real sleeping to the
/// suite — the production numbers live in [`Policy::download`].
#[derive(Debug, Clone, Copy)]
pub(super) struct Policy {
    /// Total attempts, i.e. the first try plus this many minus one retries.
    pub attempts: u32,
    /// How long to wait between tries. The ladder itself is
    /// [`dira_core::sync::Backoff`], shared with the daemon (DIRASH-0031); the
    /// seed and cap below are this caller's — an interactive command a human
    /// is watching, so capped in seconds rather than the daemon's minutes.
    pub backoff: dira_core::sync::Backoff,
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
    /// cases.
    ///
    /// `timeout` here is a per-attempt *total* backstop, not the thing that
    /// bounds a stall — that job belongs to the client-wide [`READ_TIMEOUT`]
    /// set once in `update::run` (an inactivity timeout that resets on every
    /// read making progress). This field used to be the *only* bound, at
    /// 120s, and that was the regression: `reqwest`'s per-request
    /// `.timeout()` covers the WHOLE request including the body read, so a
    /// genuinely working but merely slow link — anything under roughly
    /// 1.4 Mbps for a ~20MB artifact — blew the budget and retried, then blew
    /// it again, deterministically exhausting all 4 attempts on a download
    /// that was never stalled, only slow. 600s is sized to still comfortably
    /// fit the largest published artifact (~20MB) down to about 0.3 Mbps,
    /// while staying short enough that a connection which genuinely never
    /// progresses doesn't hang an interactive command indefinitely —
    /// `READ_TIMEOUT` normally catches that case first, at 30s of no
    /// progress; this is the backstop for whatever `READ_TIMEOUT` doesn't
    /// (e.g. a link so marginal that a few bytes trickle in every 29s,
    /// forever).
    pub(super) const fn download() -> Self {
        Self {
            attempts: 4,
            backoff: dira_core::sync::Backoff {
                seed: Duration::from_millis(500),
                max: Duration::from_secs(4),
            },
            timeout: Duration::from_secs(600),
        }
    }

    /// The policy for the `.sha256` companion file — the same ladder, but a
    /// per-attempt timeout sized to ~100 bytes rather than to the archive.
    ///
    /// Sharing the archive's 600s budget would mean a stalled checksum fetch
    /// burning 4 × 600s of dead wall clock on top of whatever the archive
    /// already spent, for a file that arrives in one packet.
    pub(super) const fn checksum() -> Self {
        Self {
            timeout: API_TIMEOUT,
            ..Self::download()
        }
    }

    /// The policy for GitHub Releases API calls.
    ///
    /// Three attempts rather than the download's four: this hop is a small
    /// JSON GET, and it runs on the foreground path of `--check` as well as
    /// `update`, where a long ladder is worse than a miss. It is also the hop
    /// most likely to see a 429 rather than a dead socket — anonymous API
    /// calls are rate-limited to 60/hr per IP, which on a shared corporate
    /// egress can be exhausted by other people entirely, and a `Retry-After`
    /// on that response is honoured (capped) instead of the ladder.
    pub(super) const fn api() -> Self {
        Self {
            attempts: 3,
            timeout: API_TIMEOUT,
            ..Self::download()
        }
    }
}

/// A failed attempt, tagged with whether another one could plausibly succeed.
pub(super) enum Attempt {
    /// Deterministic — surface it immediately, unchanged.
    Fatal(anyhow::Error),
    /// Transient. `retry_after` carries a server-supplied delay when there was
    /// a response to read one from (a 429); a dead connection has none.
    Transient {
        err: anyhow::Error,
        retry_after: Option<Duration>,
    },
}

impl Attempt {
    /// Pair a classified disposition with the error that produced it. One
    /// constructor for every failure point in every attempt body, so the
    /// retryable-vs-fatal decision is spelled out once rather than per site.
    pub(super) fn new(
        disposition: Disposition,
        err: anyhow::Error,
        retry_after: Option<Duration>,
    ) -> Self {
        match disposition {
            Disposition::Fatal => Attempt::Fatal(err),
            Disposition::Retry => Attempt::Transient { err, retry_after },
        }
    }

    /// A transport failure — no response, so never a `Retry-After`.
    pub(super) fn from_transport(e: reqwest::Error, context: String) -> Self {
        let disposition = classify_transport(&e);
        Self::new(disposition, anyhow::Error::new(e).context(context), None)
    }
}

/// Run `once` until it succeeds, until it fails deterministically, or until
/// `policy.attempts` is exhausted, waiting on the ladder in between.
///
/// `what` names the operation for the mid-retry line and the final context
/// ("download", "release lookup"); `target` is the URL both messages cite.
///
/// The unit of retry is deliberately "make the request AND fully read the
/// body", not "send the request". #113's reported failure — `connection closed
/// before message completed` — surfaces from the body read, *after* `send()`
/// has already returned `Ok`, so a driver wrapped around `send()` alone would
/// not have retried the very failure this exists for. Both callers therefore
/// do their whole read inside `once`.
///
/// Status→error mapping stays in `once` rather than being baked in here: the
/// download wants a bespoke 404 message ("asset not found on that release"),
/// the API hop wants a typed `Unauthorized` the token fallback can downcast,
/// and neither is expressible as a shared rule.
pub(super) async fn with_retry<T, Fut>(
    policy: Policy,
    what: &str,
    target: &str,
    mut once: impl FnMut() -> Fut,
) -> Result<T>
where
    Fut: Future<Output = std::result::Result<T, Attempt>>,
{
    debug_assert!(
        policy.attempts > 0,
        "a zero-attempt policy would never call `once` and never return"
    );
    let mut backoff = Duration::ZERO;
    let mut attempt = 1;

    loop {
        match once().await {
            Ok(value) => return Ok(value),
            Err(Attempt::Fatal(err)) => return Err(err),
            // `>=`, not `==`: `==` is exact for every policy constructed today,
            // but it is a silent infinite-retry trap for any future policy this
            // loop is entered with `attempts` already past. `>=` is correct
            // either way and costs nothing.
            Err(Attempt::Transient { err, .. }) if attempt >= policy.attempts => {
                return Err(err.context(format!(
                    "{what} failed after {} attempts: {target}",
                    policy.attempts
                )));
            }
            Err(Attempt::Transient { err, retry_after }) => {
                backoff = policy.backoff.transient_wait(retry_after, backoff);
                // To stderr, not stdout: `dira update`'s stdout is its progress
                // narrative, and this is a hiccup being handled, not progress.
                // Saying it out loud beats a long unexplained pause. `{err:#}`
                // (not `{err}`) so the anyhow context chain — e.g. "connection
                // closed before message completed" — actually reaches the line
                // a user sees mid-retry, instead of just the outer wrapper.
                eprintln!(
                    "dira update: {what} attempt {attempt}/{} failed ({err:#}) — retrying in {:.1}s",
                    policy.attempts,
                    backoff.as_secs_f32()
                );
                tokio::time::sleep(backoff).await;
                attempt += 1;
            }
        }
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

    /// The ladder's own seed/double/cap behaviour is `dira_core::sync`'s and
    /// tested there. What matters here is that the updater still carries the
    /// numbers it is supposed to — the shared type made these a value, and a
    /// value is exactly what a careless edit can change without breaking a
    /// compile.
    #[test]
    fn the_updater_keeps_its_interactive_ladder() {
        let p = Policy::download();
        assert_eq!(p.backoff.seed, Duration::from_millis(500));
        assert_eq!(p.backoff.max, Duration::from_secs(4));
        assert_eq!(p.backoff.next(Duration::ZERO), p.backoff.seed);
        assert_eq!(p.backoff.next(p.backoff.seed), Duration::from_secs(1));
        assert_eq!(p.backoff.next(Duration::from_secs(600)), p.backoff.max);
    }

    #[test]
    fn retry_after_overrides_the_ladder_but_is_still_capped() {
        let p = Policy::download();
        assert_eq!(
            p.backoff
                .transient_wait(Some(Duration::from_secs(1)), Duration::from_secs(2)),
            Duration::from_secs(1)
        );
        assert_eq!(
            p.backoff.transient_wait(None, Duration::ZERO),
            p.backoff.seed
        );
        // A huge Retry-After must not wedge an interactive command.
        assert_eq!(
            p.backoff
                .transient_wait(Some(Duration::from_secs(999_999)), Duration::ZERO),
            p.backoff.max
        );
    }

    /// The seconds/HTTP-date parse rules are `dira_core::sync`'s and tested
    /// there; this covers only the header extraction layered on top.
    #[test]
    fn retry_after_reads_the_header_when_present() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("7"));
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(7)));

        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
    }

    /// The checksum is a ~100-byte file; it must not inherit the archive's
    /// per-attempt budget, or a stall costs minutes instead of seconds.
    #[test]
    fn the_checksum_policy_keeps_the_ladder_but_shortens_the_timeout() {
        let (archive, sha) = (Policy::download(), Policy::checksum());
        assert!(sha.timeout < archive.timeout);
        assert_eq!(sha.attempts, archive.attempts);
        assert_eq!(sha.backoff, archive.backoff);
    }
}
