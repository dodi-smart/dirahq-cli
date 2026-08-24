//! Repo-visibility probing for telemetry (WP3): resolves whether a GitHub or
//! GitLab remote is public or private, on a best-effort, cache-first basis.
//!
//! This never sits on the telemetry ingestion hot path (see
//! [`crate::telemetry_sync::ingest`]'s integration): a cache hit answers
//! instantly, and a miss answers [`RepoVisibility::Unknown`] immediately for
//! the event being ingested right now, while a detached background probe
//! fills the cache so the NEXT event from that repo carries the real answer.
//! Nothing ever retro-updates an already-stored row.
//!
//! No auth headers, no cookies, ever — every probe request is exactly what
//! any anonymous caller of the public API would send, so it can only ever
//! reveal what that API already hands out to nobody-in-particular.

use dira_core::telemetry::repo_facts::{RepoHostClass, RepoVisibility};
use reqwest::header::USER_AGENT;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Real GitHub REST API base. Overridable in tests via [`probe`]'s
/// `base_github` parameter, which is why every call site threads it through
/// rather than hardcoding it at the request site.
pub(crate) const GITHUB_API_BASE: &str = "https://api.github.com";
/// Real GitLab API base. Overridable in tests via [`probe`]'s `base_gitlab`
/// parameter — see [`GITHUB_API_BASE`].
pub(crate) const GITLAB_API_BASE: &str = "https://gitlab.com";

/// Per-request timeout for a visibility probe. The shared `AppState::http`
/// client carries no default timeout by design (see its doc comment), so
/// every request built here sets this explicitly. Short in tests so a
/// deliberately-slow mock response can exercise the timeout path without
/// slowing the suite by the production value.
#[cfg(not(test))]
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(test)]
const PROBE_TIMEOUT: Duration = Duration::from_millis(150);

/// How long a confident answer is trusted before re-probing: `Public`/
/// `Private` from a real 200/404, and the `Unknown` we give Bitbucket/
/// self-hosted without ever asking (there is nothing transient about "WP3
/// has no probe for this forge").
const LONG_TTL: Duration = Duration::from_secs(24 * 3600);
/// How long an `Unknown` from a rate limit, network error, timeout, or
/// unexpected status is trusted. Deliberately short: a 429/403 window is
/// typically minutes, not a day, and caching it as long as a confident answer
/// would freeze the wrong verdict for every event from that repo until the
/// entry expired on its own.
const SHORT_TTL: Duration = Duration::from_secs(10 * 60);

/// Cache capacity. Small and size-capped — this memoizes the handful of
/// repos one install actually works in, not a general-purpose store.
const CACHE_CAP: usize = 256;

/// How long to trust a resolved [`RepoVisibility`] before re-probing.
/// `Unknown` only ever comes out of a GitHub/GitLab probe when something went
/// wrong (a rate limit, an error status, a timeout, a network failure) — a
/// real 200/404 always resolves to `Public`/`Private` instead — so every
/// `Unknown` from an actual probe is transient by construction and gets
/// [`SHORT_TTL`]. `Public`/`Private` are confident answers and get
/// [`LONG_TTL`]. Bitbucket/self-hosted's synchronous `Unknown` (never
/// probed at all) is cached with [`LONG_TTL`] directly by [`resolve`],
/// bypassing this function entirely — there's nothing transient about it.
fn cache_ttl_for(visibility: RepoVisibility) -> Duration {
    match visibility {
        RepoVisibility::Public | RepoVisibility::Private => LONG_TTL,
        RepoVisibility::Unknown => SHORT_TTL,
    }
}

struct CacheEntry {
    visibility: RepoVisibility,
    inserted_at: Instant,
    expires_at: Instant,
}

/// Caches resolved repo visibility, keyed by the SALTED `repo_hash` —
/// never the plaintext canonical ref, so the cache carries the same privacy
/// property as everything else in the telemetry pipeline (see
/// `dira_core::telemetry::repo_facts`). One instance lives on `AppState` for
/// the daemon's whole life.
///
/// Bounds two independent things:
/// - **Answers**: a `repo_hash -> (visibility, expiry)` map, capped at
///   [`CACHE_CAP`] entries (oldest inserted evicted first on overflow) and
///   TTL'd per [`Self::insert`]'s caller.
/// - **In-flight probes**: a `repo_hash` set, so a burst of events for the
///   same not-yet-cached repo spawns at most one probe rather than one per
///   event — see [`Self::try_start_probe`] / [`Self::finish_probe`].
#[derive(Default)]
pub struct VisibilityCache {
    entries: Mutex<HashMap<String, CacheEntry>>,
    in_flight: Mutex<HashSet<String>>,
}

impl VisibilityCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// A cached, still-fresh answer for `repo_hash`, or `None` on a miss
    /// (never probed, or the entry expired).
    pub(crate) fn get(&self, repo_hash: &str) -> Option<RepoVisibility> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = entries.get(repo_hash)?;
        (entry.expires_at > Instant::now()).then_some(entry.visibility)
    }

    /// Record `visibility` for `repo_hash`, valid for `ttl`. If the cache is
    /// full and `repo_hash` is a genuinely new key, the oldest-inserted entry
    /// is evicted first — an update to an existing key never grows the map,
    /// so it never triggers eviction.
    pub(crate) fn insert(&self, repo_hash: String, visibility: RepoVisibility, ttl: Duration) {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if !entries.contains_key(&repo_hash) && entries.len() >= CACHE_CAP {
            if let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, e)| e.inserted_at)
                .map(|(k, _)| k.clone())
            {
                entries.remove(&oldest);
            }
        }
        entries.insert(
            repo_hash,
            CacheEntry {
                visibility,
                inserted_at: now,
                expires_at: now + ttl,
            },
        );
    }

    /// Claim the right to run a probe for `repo_hash`: `true` if no probe for
    /// it is already in flight (and it is now marked as such), `false` if one
    /// already is — the caller must not spawn a second. Always pair a `true`
    /// with a later [`Self::finish_probe`] call once that probe completes, so
    /// a future miss can probe again.
    pub(crate) fn try_start_probe(&self, repo_hash: &str) -> bool {
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        in_flight.insert(repo_hash.to_string())
    }

    /// Release the in-flight claim taken by [`Self::try_start_probe`].
    pub(crate) fn finish_probe(&self, repo_hash: &str) {
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        in_flight.remove(repo_hash);
    }
}

/// Percent-encode one path segment per RFC 3986's unreserved set
/// (`A-Za-z0-9-._~`). GitLab's nested-group project path joins segments with
/// a literal `%2F`, so any `/`-or-otherwise-reserved byte *inside* a segment
/// must itself be encoded first or it would be indistinguishable from a
/// group separator.
fn percent_encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Everything in `canonical` after the host segment (`host/owner/repo` ->
/// `owner/repo`, `host/group/sub/repo` -> `group/sub/repo`).
fn path_after_host(canonical: &str) -> &str {
    canonical.split_once('/').map_or("", |(_, rest)| rest)
}

/// The daemon's `User-Agent` on every visibility probe. GitHub's REST API
/// rejects anonymous requests that carry none at all, and the shared
/// `AppState::http` client sets no default headers (by design — see its doc
/// comment), so every request built here sets this explicitly.
fn user_agent() -> String {
    format!("dirad/{}", env!("CARGO_PKG_VERSION"))
}

/// Probe a canonical remote's visibility against the real forge API (or, in
/// tests, `base_github`/`base_gitlab` pointed at a mock). Stateless — it
/// never touches a [`VisibilityCache`]; see [`resolve`] for the cache- and
/// in-flight-aware entry point [`crate::telemetry_sync::ingest`] actually
/// uses.
///
/// - GitHub (`github.com/owner/repo`): `GET {base_github}/repos/{owner}/{repo}`.
/// - GitLab (`gitlab.com/owner/repo`, possibly nested —
///   `gitlab.com/group/sub/repo`): `GET
///   {base_gitlab}/api/v4/projects/{group%2Fsub%2Frepo}` — every path segment
///   after the host is percent-encoded and the segments are joined with
///   `%2F`, matching GitLab's namespaced-path project lookup.
/// - Bitbucket/self-hosted: `Unknown`, with no request at all — WP3 has no
///   probe for these forges yet.
///
/// Status mapping (see [`cache_ttl_for`] for the TTL each maps to when
/// [`resolve`] caches it):
/// - `200` -> `Public`.
/// - `404` -> `Private`. Deliberately ambiguous: both forges also 404 a URL
///   that doesn't exist at all (a typo'd owner, or a renamed/deleted repo).
///   Treating that the same as "private" is the conservative reading — the
///   alternative (assuming `Public`) risks mislabeling a private repo public.
/// - `403`/`429` (forbidden/rate-limited) -> `Unknown`.
/// - Any other status, network error, or timeout -> `Unknown`.
///
/// No auth headers, no cookies, ever.
pub(crate) async fn probe(
    http: &reqwest::Client,
    base_github: &str,
    base_gitlab: &str,
    host_class: RepoHostClass,
    canonical: &str,
) -> RepoVisibility {
    match host_class {
        RepoHostClass::Bitbucket | RepoHostClass::SelfHosted => RepoVisibility::Unknown,
        RepoHostClass::GitHub => probe_github(http, base_github, canonical).await,
        RepoHostClass::GitLab => probe_gitlab(http, base_gitlab, canonical).await,
    }
}

async fn probe_github(
    http: &reqwest::Client,
    base_github: &str,
    canonical: &str,
) -> RepoVisibility {
    let owner_repo = path_after_host(canonical);
    let url = format!("{}/repos/{owner_repo}", base_github.trim_end_matches('/'));
    request_visibility(http, &url).await
}

async fn probe_gitlab(
    http: &reqwest::Client,
    base_gitlab: &str,
    canonical: &str,
) -> RepoVisibility {
    let encoded_path = path_after_host(canonical)
        .split('/')
        .map(percent_encode_segment)
        .collect::<Vec<_>>()
        .join("%2F");
    let url = format!(
        "{}/api/v4/projects/{encoded_path}",
        base_gitlab.trim_end_matches('/')
    );
    request_visibility(http, &url).await
}

async fn request_visibility(http: &reqwest::Client, url: &str) -> RepoVisibility {
    let resp = http
        .get(url)
        .header(USER_AGENT, user_agent())
        .timeout(PROBE_TIMEOUT)
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("repo visibility probe: request failed: {e}");
            return RepoVisibility::Unknown;
        }
    };
    match resp.status().as_u16() {
        200 => RepoVisibility::Public,
        404 => RepoVisibility::Private,
        403 | 429 => RepoVisibility::Unknown,
        other => {
            tracing::debug!("repo visibility probe: unexpected status {other}");
            RepoVisibility::Unknown
        }
    }
}

/// The cache- and in-flight-aware entry point [`crate::telemetry_sync::ingest`]
/// calls. Never blocks on the network:
///
/// - Cache hit (still fresh): returns it immediately.
/// - Bitbucket/self-hosted: answered — and cached with [`LONG_TTL`] — entirely
///   synchronously, since [`probe`] never issues a request for these forges
///   anyway; no point spawning a task for a foregone conclusion.
/// - Cache miss on GitHub/GitLab: returns [`RepoVisibility::Unknown`] for the
///   CALLER's event — the "first event says unknown" half of the documented
///   ingest behavior — and, unless a probe for this `repo_hash` is already
///   running, spawns a detached task that probes the real answer and fills
///   the cache, so the NEXT event from this repo gets the truth. A burst of
///   events for the same uncached repo before that probe resolves spawns at
///   most one probe: [`VisibilityCache::try_start_probe`] makes every event
///   after the first in the burst a no-op spawn.
///
/// Nothing here ever retro-updates an already-stored event row — see
/// [`crate::telemetry_sync::ingest`]'s doc comment.
///
/// `base_github`/`base_gitlab` are threaded through (rather than this
/// function hardcoding [`GITHUB_API_BASE`]/[`GITLAB_API_BASE`] itself) purely
/// for tests — every production call site passes the real constants.
pub(crate) fn resolve(
    cache: &Arc<VisibilityCache>,
    http: &reqwest::Client,
    base_github: &str,
    base_gitlab: &str,
    host_class: RepoHostClass,
    canonical: &str,
    repo_hash: &str,
) -> RepoVisibility {
    if let Some(v) = cache.get(repo_hash) {
        return v;
    }
    if matches!(
        host_class,
        RepoHostClass::Bitbucket | RepoHostClass::SelfHosted
    ) {
        cache.insert(repo_hash.to_string(), RepoVisibility::Unknown, LONG_TTL);
        return RepoVisibility::Unknown;
    }
    if cache.try_start_probe(repo_hash) {
        let cache = cache.clone();
        let http = http.clone();
        let base_github = base_github.to_string();
        let base_gitlab = base_gitlab.to_string();
        let canonical = canonical.to_string();
        let repo_hash = repo_hash.to_string();
        tokio::spawn(async move {
            let visibility = probe(&http, &base_github, &base_gitlab, host_class, &canonical).await;
            cache.insert(repo_hash.clone(), visibility, cache_ttl_for(visibility));
            cache.finish_probe(&repo_hash);
        });
    }
    RepoVisibility::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MockCloud, MockResp};
    use dira_core::telemetry::event::TelemetryEvent;

    /// Poll `f` until it returns `Some`, or panic after `deadline`. Used to
    /// wait on a detached background probe without a fixed sleep.
    async fn poll_until<T>(deadline: Duration, mut f: impl FnMut() -> Option<T>) -> T {
        let start = Instant::now();
        loop {
            if let Some(v) = f() {
                return v;
            }
            if start.elapsed() > deadline {
                panic!("condition never became true within {deadline:?}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn github_200_maps_to_public() {
        let cloud = MockCloud::start(&["/repos/acme/api"]).await;
        cloud.push("/repos/acme/api", MockResp::ok("{}"));
        let http = reqwest::Client::new();
        let vis = probe(
            &http,
            cloud.base_url(),
            "http://unused.invalid",
            RepoHostClass::GitHub,
            "github.com/acme/api",
        )
        .await;
        assert_eq!(vis, RepoVisibility::Public);
    }

    #[tokio::test]
    async fn github_404_maps_to_private() {
        let cloud = MockCloud::start(&["/repos/acme/api"]).await;
        cloud.push("/repos/acme/api", MockResp::status(404, ""));
        let http = reqwest::Client::new();
        let vis = probe(
            &http,
            cloud.base_url(),
            "http://unused.invalid",
            RepoHostClass::GitHub,
            "github.com/acme/api",
        )
        .await;
        assert_eq!(vis, RepoVisibility::Private);
    }

    #[tokio::test]
    async fn github_500_maps_to_unknown() {
        let cloud = MockCloud::start(&["/repos/acme/api"]).await;
        cloud.push("/repos/acme/api", MockResp::status(500, "boom"));
        let http = reqwest::Client::new();
        let vis = probe(
            &http,
            cloud.base_url(),
            "http://unused.invalid",
            RepoHostClass::GitHub,
            "github.com/acme/api",
        )
        .await;
        assert_eq!(vis, RepoVisibility::Unknown);
    }

    #[tokio::test]
    async fn github_429_maps_to_unknown() {
        let cloud = MockCloud::start(&["/repos/acme/api"]).await;
        cloud.push("/repos/acme/api", MockResp::status(429, "slow down"));
        let http = reqwest::Client::new();
        let vis = probe(
            &http,
            cloud.base_url(),
            "http://unused.invalid",
            RepoHostClass::GitHub,
            "github.com/acme/api",
        )
        .await;
        assert_eq!(vis, RepoVisibility::Unknown);
    }

    #[tokio::test]
    async fn a_slow_response_times_out_to_unknown() {
        let cloud = MockCloud::start(&["/repos/acme/api"]).await;
        cloud.push(
            "/repos/acme/api",
            MockResp::ok("{}").with_delay(Duration::from_secs(5)),
        );
        let http = reqwest::Client::new();
        let started = Instant::now();
        let vis = probe(
            &http,
            cloud.base_url(),
            "http://unused.invalid",
            RepoHostClass::GitHub,
            "github.com/acme/api",
        )
        .await;
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the probe's own timeout must fire well before the mock's 5s delay"
        );
        assert_eq!(vis, RepoVisibility::Unknown);
    }

    #[tokio::test]
    async fn a_network_error_maps_to_unknown() {
        let http = reqwest::Client::new();
        // Port 0 can never accept a connection — an immediate, deterministic
        // network error without depending on any particular port being closed.
        let vis = probe(
            &http,
            "http://127.0.0.1:0",
            "http://unused.invalid",
            RepoHostClass::GitHub,
            "github.com/acme/api",
        )
        .await;
        assert_eq!(vis, RepoVisibility::Unknown);
    }

    #[tokio::test]
    async fn gitlab_200_maps_to_public() {
        let cloud = MockCloud::start(&["/api/v4/projects/acme%2Fapi"]).await;
        cloud.push("/api/v4/projects/acme%2Fapi", MockResp::ok("{}"));
        let http = reqwest::Client::new();
        let vis = probe(
            &http,
            "http://unused.invalid",
            cloud.base_url(),
            RepoHostClass::GitLab,
            "gitlab.com/acme/api",
        )
        .await;
        assert_eq!(vis, RepoVisibility::Public);
    }

    #[tokio::test]
    async fn gitlab_nested_group_path_is_percent_encoded_and_joined() {
        // If the encoding/join logic ever produced the wrong path, this
        // request would miss the registered route (axum's fallback 404) and
        // be read back as `Private` instead of the `Public` we queued —
        // failing this assertion.
        let cloud = MockCloud::start(&["/api/v4/projects/group%2Fsub%2Frepo"]).await;
        cloud.push("/api/v4/projects/group%2Fsub%2Frepo", MockResp::ok("{}"));
        let http = reqwest::Client::new();
        let vis = probe(
            &http,
            "http://unused.invalid",
            cloud.base_url(),
            RepoHostClass::GitLab,
            "gitlab.com/group/sub/repo",
        )
        .await;
        assert_eq!(
            vis,
            RepoVisibility::Public,
            "a nested gitlab path must resolve to the %2F-joined project path"
        );
    }

    #[tokio::test]
    async fn gitlab_404_maps_to_private() {
        let cloud = MockCloud::start(&["/api/v4/projects/acme%2Fapi"]).await;
        cloud.push("/api/v4/projects/acme%2Fapi", MockResp::status(404, ""));
        let http = reqwest::Client::new();
        let vis = probe(
            &http,
            "http://unused.invalid",
            cloud.base_url(),
            RepoHostClass::GitLab,
            "gitlab.com/acme/api",
        )
        .await;
        assert_eq!(vis, RepoVisibility::Private);
    }

    #[tokio::test]
    async fn bitbucket_and_self_hosted_never_issue_a_request() {
        let http = reqwest::Client::new();
        // Bases point at addresses that would error/hang if ever contacted;
        // reaching the assertion at all (fast, no timeout) proves the match
        // arm short-circuited before building a request.
        for host_class in [RepoHostClass::Bitbucket, RepoHostClass::SelfHosted] {
            let vis = probe(
                &http,
                "http://127.0.0.1:0",
                "http://127.0.0.1:0",
                host_class,
                "bitbucket.org/acme/api",
            )
            .await;
            assert_eq!(vis, RepoVisibility::Unknown);
        }
    }

    #[test]
    fn ttl_choice_is_short_for_unknown_and_long_for_a_confident_answer() {
        assert_eq!(cache_ttl_for(RepoVisibility::Public), LONG_TTL);
        assert_eq!(cache_ttl_for(RepoVisibility::Private), LONG_TTL);
        assert_eq!(
            cache_ttl_for(RepoVisibility::Unknown),
            SHORT_TTL,
            "an Unknown from an actual probe only ever means a rate limit, error, or timeout — \
             a real 200/404 always resolves to Public/Private instead — so it must never be \
             cached as long as a confident answer"
        );
    }

    #[tokio::test]
    async fn resolve_caches_after_the_spawned_probe_fills_it_and_a_second_lookup_hits_it() {
        let cloud = MockCloud::start(&["/repos/acme/api"]).await;
        cloud.push("/repos/acme/api", MockResp::ok("{}"));
        let http = reqwest::Client::new();
        let cache = Arc::new(VisibilityCache::new());
        let repo_hash = "deadbeef";

        // Miss: Unknown immediately, probe spawned in the background.
        let first = resolve(
            &cache,
            &http,
            cloud.base_url(),
            "http://unused.invalid",
            RepoHostClass::GitHub,
            "github.com/acme/api",
            repo_hash,
        );
        assert_eq!(first, RepoVisibility::Unknown);

        // Wait for the detached probe to land, rather than sleeping a fixed
        // amount.
        let warmed = poll_until(Duration::from_secs(1), || cache.get(repo_hash)).await;
        assert_eq!(warmed, RepoVisibility::Public);
        assert_eq!(cloud.requests("/repos/acme/api").len(), 1);

        // Second lookup, now warm: served from the cache, no second request.
        let second = resolve(
            &cache,
            &http,
            cloud.base_url(),
            "http://unused.invalid",
            RepoHostClass::GitHub,
            "github.com/acme/api",
            repo_hash,
        );
        assert_eq!(second, RepoVisibility::Public);
        assert_eq!(
            cloud.requests("/repos/acme/api").len(),
            1,
            "a fresh cache hit must not issue a second request"
        );
    }

    #[tokio::test]
    async fn resolve_never_spawns_a_second_probe_while_one_is_in_flight() {
        // A response slow enough that several `resolve` calls land before it
        // answers, so if bounding failed we'd see more than one request.
        let cloud = MockCloud::start(&["/repos/acme/api"]).await;
        cloud.push(
            "/repos/acme/api",
            MockResp::ok("{}").with_delay(Duration::from_millis(300)),
        );
        let http = reqwest::Client::new();
        let cache = Arc::new(VisibilityCache::new());
        let repo_hash = "deadbeef";

        for _ in 0..5 {
            let v = resolve(
                &cache,
                &http,
                cloud.base_url(),
                "http://unused.invalid",
                RepoHostClass::GitHub,
                "github.com/acme/api",
                repo_hash,
            );
            assert_eq!(v, RepoVisibility::Unknown);
        }

        poll_until(Duration::from_secs(2), || cache.get(repo_hash)).await;
        assert_eq!(
            cloud.requests("/repos/acme/api").len(),
            1,
            "a burst of misses for the same repo must spawn at most one probe"
        );
    }

    #[tokio::test]
    async fn resolve_answers_bitbucket_and_self_hosted_synchronously_with_no_request() {
        let http = reqwest::Client::new();
        let cache = Arc::new(VisibilityCache::new());

        let vis = resolve(
            &cache,
            &http,
            "http://127.0.0.1:0",
            "http://127.0.0.1:0",
            RepoHostClass::Bitbucket,
            "bitbucket.org/acme/api",
            "deadbeef",
        );
        assert_eq!(vis, RepoVisibility::Unknown);
        // Cached immediately — no spawned probe needed for a forge we never ask.
        assert_eq!(cache.get("deadbeef"), Some(RepoVisibility::Unknown));
    }

    #[test]
    fn cache_evicts_the_oldest_entry_once_full() {
        let cache = VisibilityCache::new();
        for i in 0..CACHE_CAP {
            cache.insert(format!("hash-{i}"), RepoVisibility::Public, LONG_TTL);
        }
        assert!(cache.get("hash-0").is_some());

        // One more insert past capacity evicts the very first one inserted.
        cache.insert("hash-new".to_string(), RepoVisibility::Private, LONG_TTL);
        assert!(
            cache.get("hash-0").is_none(),
            "the oldest entry must be evicted once the cache is full"
        );
        assert_eq!(cache.get("hash-new"), Some(RepoVisibility::Private));
    }

    #[test]
    fn cache_expires_entries_past_their_ttl() {
        let cache = VisibilityCache::new();
        cache.insert(
            "hash".to_string(),
            RepoVisibility::Public,
            Duration::from_millis(0),
        );
        // A zero-length TTL is already expired relative to `Instant::now()`
        // measured microseconds later.
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(cache.get("hash"), None);
    }

    /// End-to-end through `telemetry_sync::ingest`: the first event for a
    /// never-before-seen repo stores `Unknown` (the cache miss path), and —
    /// once the detached probe has warmed the cache — a second event for the
    /// SAME repo stores the real, probed visibility. Neither the first row
    /// nor the cache is ever retro-updated; only the second ingest's own
    /// write differs.
    #[tokio::test]
    async fn ingest_reports_unknown_first_then_the_real_visibility_once_warm() {
        let cloud = MockCloud::start(&["/api/v1/pulse", "/repos/acme/api"]).await;
        cloud.push("/repos/acme/api", MockResp::ok("{}"));
        let store = dira_core::Store::open_in_memory().await.unwrap();
        let config = dira_core::Config {
            cloud_url: Some(cloud.base_url().to_string()),
            telemetry: dira_core::config::TelemetryKnobs { enabled: true },
            ..Default::default()
        };
        let (mut state, ..) = crate::build_state(store, config).await.unwrap();
        // `ingest` must resolve visibility against the mock, never the real
        // forge — point its probe base at the same mock cloud serves.
        state.github_api_base = Arc::from(cloud.base_url());

        let make_wire = |n: u64| {
            TelemetryEvent::CommandExecuted {
                command: "status",
                duration_ms: n,
                success: true,
                error_kind: None,
                repo: None,
            }
            .into_wire("2026-01-01T00:00:00Z".into(), "0.0.0-test")
        };

        crate::telemetry_sync::ingest(
            &state,
            make_wire(1),
            Some("github.com/acme/api".to_string()),
        )
        .await;

        let after_first = state.store.telemetry_max_event_id().await.unwrap().unwrap();
        let rows = state
            .store
            .telemetry_events_since(None, &after_first, 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        let first: dira_core::telemetry::wire::TelemetryEventWire =
            serde_json::from_str(&rows[0].props_json).unwrap();
        assert_eq!(
            first.repo_visibility.as_deref(),
            Some("unknown"),
            "the very first event for an unseen repo must report unknown, never guess"
        );
        let repo_hash = first.repo_hash.clone().unwrap();

        // Wait for the background probe `ingest` kicked off to warm the cache.
        poll_until(Duration::from_secs(1), || {
            state.visibility_cache.get(&repo_hash)
        })
        .await;

        crate::telemetry_sync::ingest(
            &state,
            make_wire(2),
            Some("github.com/acme/api".to_string()),
        )
        .await;
        let after_second = state.store.telemetry_max_event_id().await.unwrap().unwrap();
        let rows2 = state
            .store
            .telemetry_events_since(Some(&after_first), &after_second, 10)
            .await
            .unwrap();
        assert_eq!(rows2.len(), 1);
        let second: dira_core::telemetry::wire::TelemetryEventWire =
            serde_json::from_str(&rows2[0].props_json).unwrap();
        assert_eq!(
            second.repo_visibility.as_deref(),
            Some("public"),
            "once the cache is warm, the next event for the same repo carries the real answer"
        );

        // The first row is untouched — nothing retro-updates it.
        let rows_again = state
            .store
            .telemetry_events_since(None, &after_first, 10)
            .await
            .unwrap();
        let first_again: dira_core::telemetry::wire::TelemetryEventWire =
            serde_json::from_str(&rows_again[0].props_json).unwrap();
        assert_eq!(first_again.repo_visibility.as_deref(), Some("unknown"));
    }
}
