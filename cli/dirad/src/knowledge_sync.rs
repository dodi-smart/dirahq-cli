//! Knowledge sync task (M2): ship zavet decisions, specs, trailer refs, guard
//! events, and per-repo coverage stats to `POST /api/v1/knowledge`.
//!
//! A deliberately smaller sibling of [`crate::sync`]: same debounce/backstop
//! shape, same signing rule, same cursor-after-2xx discipline — but its own
//! endpoint, its own four cursors, and its own **consent gate**: nothing runs
//! unless `[sync] knowledge` is `metadata` or `full` (default `off`), and the
//! cloud additionally enforces the workspace's tier. Two channel-specific
//! error paths:
//! - `knowledge_disabled` (403): the workspace opted out — back off at the
//!   ceiling with a clear health kind; nothing is wrong locally.
//! - `content_not_allowed` (400): the batch carried content the workspace has
//!   not opted into — strip the same window down to the metadata tier
//!   ([`dira_contract::KnowledgeBatch::strip_content`]) and retry once. Sync
//!   never wedges; content never lands without both consents.
//!
//! A 404 means the cloud predates the endpoint — treated as quietly skipped
//! (health kind `endpoint_missing`), not an error worth logging loudly.

use crate::state::AppState;
use crate::sync::{
    next_backoff, record_channel_health, retry_after_from_headers, transient_wait, HealthChannel,
    MAX_BACKOFF,
};
use dira_contract::{KnowledgeEnvelope, KnowledgeRepoStats, SCHEMA_VERSION};
use dira_core::identity;
use dira_core::signing::DeviceKey;
use dira_core::store::{ZavetDecisionRow, ZavetSpecRow};
use dira_core::sync::knowledge::{
    build_knowledge_batches, knowledge_batch_id, parse_knowledge_response, KnowledgeChunk,
    KnowledgeTier, KNOWLEDGE_CHUNK_ITEMS, META_KNOWLEDGE_DECISION_CURSOR,
    META_KNOWLEDGE_GUARD_CURSOR, META_KNOWLEDGE_HEALTH, META_KNOWLEDGE_SPEC_CURSOR,
    META_KNOWLEDGE_STATS_PREFIX, META_KNOWLEDGE_TRAILER_CURSOR,
};
use dira_core::{config::KnowledgeSyncMode, project};
use std::collections::HashSet;
use std::time::Duration as StdDuration;
use tokio::sync::mpsc;
use tokio::time::{sleep, sleep_until, Instant};

/// Debounce window: coalesce a burst of triggers into one flush.
const DEBOUNCE: StdDuration = StdDuration::from_secs(3);
/// Backstop cadence — slower than attestations; knowledge moves at commit
/// speed, not event speed.
const BACKSTOP: StdDuration = StdDuration::from_secs(120);
/// HTTP timeout for a single knowledge chunk POST.
const HTTP_TIMEOUT: StdDuration = StdDuration::from_secs(30);
/// Ceiling on the rolling window for the per-repo coverage/capture snapshot.
///
/// A ceiling, not a fixed window: the effective window is clamped to the period
/// since the repo adopted `.zavet/` (see [`effective_window_days`]).
const STATS_WINDOW_DAYS: u32 = 90;
/// Minimum interval between recomputations of one repo's stats snapshot (the
/// git pass walks 90 days of history — cheap, but not per-flush cheap).
const STATS_TTL: time::Duration = time::Duration::hours(6);

/// Handle to the knowledge sync task. Cloneable; shares the trigger channel.
#[derive(Clone)]
pub struct KnowledgeSyncHandle {
    /// Non-blocking trigger; capture and guard-event ingress `try_send(())`
    /// here. A full channel is fine — the backstop covers a missed nudge.
    pub trigger: mpsc::Sender<()>,
}

/// Create the trigger channel + handle before `AppState` exists (the handle is
/// a field of `AppState`), mirroring [`crate::sync::channel`].
pub fn channel() -> (KnowledgeSyncHandle, mpsc::Receiver<()>) {
    let (trigger, rx) = mpsc::channel::<()>(1);
    (KnowledgeSyncHandle { trigger }, rx)
}

pub fn spawn(state: AppState, rx: mpsc::Receiver<()>) {
    tokio::spawn(run(state, rx));
}

async fn run(state: AppState, mut rx: mpsc::Receiver<()>) {
    let mut backstop_at =
        Instant::now() + crate::jitter::jittered(BACKSTOP, crate::jitter::DEFAULT_FRAC);
    let mut backoff = StdDuration::ZERO;
    let mut consecutive_failures: u32 = 0;

    loop {
        tokio::select! {
            recv = rx.recv() => {
                if recv.is_none() {
                    break;
                }
                sleep(DEBOUNCE).await;
                while rx.try_recv().is_ok() {}
            }
            _ = sleep_until(backstop_at) => {
                backstop_at = Instant::now()
                    + crate::jitter::jittered(BACKSTOP, crate::jitter::DEFAULT_FRAC);
            }
        }

        let Some(device_key) = state.device_key().await else {
            continue;
        };
        match flush_knowledge(&state, &device_key, &state.http).await {
            Ok(Outcome::Synced) | Ok(Outcome::Nothing) => {
                backoff = StdDuration::ZERO;
                consecutive_failures = 0;
                record_health(&state, None, 0, 0).await;
            }
            Ok(Outcome::Skipped(kind)) => {
                backoff = StdDuration::ZERO;
                consecutive_failures = 0;
                record_health(&state, Some(kind), 0, 0).await;
            }
            Err(err) => {
                // One shared failure tail: each arm only picks its health kind
                // and wait (logging at its own level); the health/backoff
                // bookkeeping below is identical for all of them.
                let (kind, wait) = match &err {
                    KError::Transient {
                        message,
                        retry_after,
                    } => {
                        let wait = transient_wait(*retry_after, backoff);
                        tracing::warn!(
                            "knowledge sync: transient failure, backing off {wait:?}: {message}"
                        );
                        ("transient", wait)
                    }
                    KError::ReLinkRequired => {
                        tracing::error!(
                            "knowledge sync: cloud rejected device (unknown_device) — re-link \
                             required"
                        );
                        ("unknown_device", StdDuration::ZERO)
                    }
                    KError::SignatureRejected => {
                        // The attestation task owns the pending-key self-heal
                        // (WP-B1b); a rotation it promotes invalidates the
                        // shared cached key, so this task recovers on its next
                        // tick without duplicating that machinery here. Until
                        // then, ceiling backoff.
                        tracing::error!(
                            "knowledge sync: signature rejected — waiting for the attestation \
                             task's key recovery (or `dira device link`)"
                        );
                        ("signature_rejected", MAX_BACKOFF)
                    }
                    KError::ContentNotAllowed => {
                        // Consumed inside `flush_knowledge` (the downgrade
                        // retry); reaching here would be a bug — treat as fatal.
                        tracing::warn!(
                            "knowledge sync: content_not_allowed escaped the downgrade path"
                        );
                        ("fatal", next_backoff(backoff))
                    }
                    KError::Disabled => {
                        // Workspace-level opt-out — not a local failure; long
                        // quiet backoff.
                        tracing::info!(
                            "knowledge sync: workspace has knowledge sync disabled \
                             (knowledge_disabled) — enable it in the dashboard's Connections \
                             screen to start receiving"
                        );
                        ("knowledge_disabled", MAX_BACKOFF)
                    }
                    KError::SchemaSkew(body) => {
                        tracing::error!(
                            "knowledge sync: cloud rejected our contract version — upgrade \
                             daemon or cloud: {body}"
                        );
                        ("schema_skew", MAX_BACKOFF)
                    }
                    KError::Fatal(e) => {
                        let wait = next_backoff(backoff);
                        tracing::warn!("knowledge sync: error, backing off {wait:?}: {e}");
                        ("fatal", wait)
                    }
                };
                backoff = wait;
                // A workspace opt-out is not a local failure; everything else is.
                consecutive_failures = if matches!(err, KError::Disabled) {
                    0
                } else {
                    consecutive_failures + 1
                };
                record_health(&state, Some(kind), consecutive_failures, backoff.as_secs()).await;
                sleep(backoff).await;
            }
        }
    }
}

#[cfg_attr(test, derive(Debug))]
enum Outcome {
    Synced,
    Nothing,
    /// Not running, with the health kind saying why (`"off"`, `"skipped"`,
    /// `"endpoint_missing"`).
    Skipped(&'static str),
}

#[cfg_attr(test, derive(Debug))]
enum KError {
    Transient {
        message: String,
        retry_after: Option<StdDuration>,
    },
    ReLinkRequired,
    SignatureRejected,
    /// 400 `content_not_allowed`: the batch carried content the workspace has
    /// not opted into. Consumed inside [`flush_knowledge`] (the metadata
    /// downgrade-and-retry); never escapes to the run loop.
    ContentNotAllowed,
    /// 403 `knowledge_disabled`: the workspace opted out.
    Disabled,
    SchemaSkew(String),
    Fatal(String),
}

/// One knowledge flush. Re-reads the knob + linkage every call so config and
/// `dira device link` take effect without a restart.
async fn flush_knowledge(
    state: &AppState,
    device_key: &DeviceKey,
    client: &reqwest::Client,
) -> Result<Outcome, KError> {
    // 1. The consent gate, then configuration + linkage.
    let tier = match state.config.sync.knowledge {
        KnowledgeSyncMode::Off => return Ok(Outcome::Skipped("off")),
        KnowledgeSyncMode::Metadata => KnowledgeTier::Metadata,
        KnowledgeSyncMode::Full => KnowledgeTier::Full,
    };
    let Some(cloud_url) = state.config.cloud_url.clone() else {
        return Ok(Outcome::Skipped("skipped"));
    };
    let device_id = match identity::device_id(&state.store).await {
        Ok(Some(id)) => id,
        Ok(None) => return Ok(Outcome::Skipped("skipped")),
        Err(e) => return Err(KError::Fatal(format!("read device_id: {e}"))),
    };

    // 2. Snapshot the four cursors.
    let decision_seq = meta_i64(state, META_KNOWLEDGE_DECISION_CURSOR).await?;
    let spec_seq = meta_i64(state, META_KNOWLEDGE_SPEC_CURSOR).await?;
    let trailer_rowid = meta_i64(state, META_KNOWLEDGE_TRAILER_CURSOR).await?;
    let guard_id = state
        .store
        .meta_get(META_KNOWLEDGE_GUARD_CURSOR)
        .await
        .map_err(|e| KError::Fatal(format!("read guard cursor: {e}")))?
        .unwrap_or_default();

    // 3. Load the windows (bounded; the next flush continues where this ends).
    let limit = KNOWLEDGE_CHUNK_ITEMS as i64;
    let decisions = state
        .store
        .zavet_decisions_since(decision_seq, limit)
        .await
        .map_err(|e| KError::Fatal(format!("load decisions: {e}")))?;
    let specs = state
        .store
        .zavet_specs_since(spec_seq, limit)
        .await
        .map_err(|e| KError::Fatal(format!("load specs: {e}")))?;
    let trailers = state
        .store
        .zavet_trailers_since(trailer_rowid, limit * 4)
        .await
        .map_err(|e| KError::Fatal(format!("load trailers: {e}")))?;
    let guard_events = state
        .store
        .zavet_guard_events_since(&guard_id, limit * 10)
        .await
        .map_err(|e| KError::Fatal(format!("load guard events: {e}")))?;
    let repo_stats = compute_repo_stats(state).await;

    let now = crate::heartbeat::fmt_rfc3339(time::OffsetDateTime::now_utc());
    let chunks = build_knowledge_batches(
        &device_id,
        &now,
        tier,
        &decisions,
        &specs,
        &trailers,
        &guard_events,
        repo_stats,
    );
    if chunks.is_empty() {
        return Ok(Outcome::Nothing);
    }

    match post_chunks(state, device_key, client, &cloud_url, &chunks).await {
        Err(KError::ContentNotAllowed) if tier == KnowledgeTier::Full => {
            // The workspace accepts metadata only — downgrade THIS window in
            // place ([`KnowledgeBatch::strip_content`]) and retry once. The
            // batch id is tier-sensitive, so it is recomputed after the strip;
            // the stats snapshot is dropped from the retry (it never was part
            // of the window's identity — the next TTL pass re-sends it).
            // Re-sent metadata-only chunks that overlap already-accepted ones
            // dedup by batch id / natural keys.
            tracing::warn!(
                "knowledge sync: workspace has not opted into content — re-sending this \
                 window at the metadata tier (set [sync] knowledge = \"metadata\" locally, \
                 or opt the workspace in, to silence this)"
            );
            let stripped: Vec<KnowledgeChunk> = chunks
                .iter()
                .map(|chunk| {
                    let mut chunk = chunk.clone();
                    chunk.batch.strip_content();
                    chunk.batch.repo_stats.clear();
                    chunk.batch.batch_id = knowledge_batch_id(&chunk.batch);
                    chunk
                })
                .collect();
            match post_chunks(state, device_key, client, &cloud_url, &stripped).await {
                Err(KError::ContentNotAllowed) => Err(metadata_rejected()),
                other => other,
            }
        }
        Err(KError::ContentNotAllowed) => Err(metadata_rejected()),
        other => other,
    }
}

/// The cloud answered `content_not_allowed` to a batch that already carried no
/// content — nothing left to strip, so surface it as a plain fatal error.
fn metadata_rejected() -> KError {
    KError::Fatal("cloud answered content_not_allowed to a metadata-tier batch".into())
}

async fn post_chunks(
    state: &AppState,
    device_key: &DeviceKey,
    client: &reqwest::Client,
    cloud_url: &str,
    chunks: &[KnowledgeChunk],
) -> Result<Outcome, KError> {
    let url = format!("{}/api/v1/knowledge", cloud_url.trim_end_matches('/'));
    for chunk in chunks {
        let sig = device_key
            .sign_payload(&chunk.batch)
            .map_err(|e| KError::Fatal(format!("sign: {e}")))?;
        let envelope = KnowledgeEnvelope {
            schema_version: SCHEMA_VERSION.to_string(),
            device_id: chunk.batch.device_id.clone(),
            payload: chunk.batch.clone(),
            sig,
        };
        let resp = client
            .post(&url)
            .timeout(HTTP_TIMEOUT)
            .json(&envelope)
            .send()
            .await
            .map_err(|e| KError::Transient {
                message: format!("post: {e}"),
                retry_after: None,
            })?;
        let status = resp.status();
        let retry_after = retry_after_from_headers(resp.headers());
        let body = resp.text().await.unwrap_or_default();

        if status == reqwest::StatusCode::NOT_FOUND {
            // Older cloud without the endpoint — quiet skip, try again later.
            tracing::debug!("knowledge sync: cloud has no /api/v1/knowledge yet (404)");
            return Ok(Outcome::Skipped("endpoint_missing"));
        }
        if status.as_u16() == 429 || status.is_server_error() {
            return Err(KError::Transient {
                message: format!("cloud answered {status}"),
                retry_after,
            });
        }
        if !status.is_success() {
            let parsed = parse_knowledge_response(&body);
            return Err(match parsed.error.as_str() {
                "unknown_device" => KError::ReLinkRequired,
                "bad_signature" => KError::SignatureRejected,
                "knowledge_disabled" => KError::Disabled,
                "content_not_allowed" => KError::ContentNotAllowed,
                "unsupported_schema_version" => KError::SchemaSkew(body),
                _ => KError::Fatal(format!("cloud answered {status}: {body}")),
            });
        }

        // 2xx — epoch handling first, then advance cursors.
        let parsed = parse_knowledge_response(&body);
        if let Some(epoch) = parsed.sync.data_epoch.as_deref() {
            if crate::sync::note_data_epoch(state, epoch)
                .await
                .map_err(KError::Fatal)?
            {
                // The cloud was reset: every cursor (attestation + knowledge)
                // was just blanked. Abort this flush — the next one re-sends
                // from scratch.
                return Ok(Outcome::Synced);
            }
        }
        if let Some(id) = &chunk.cursor_guard_id {
            meta_put(state, META_KNOWLEDGE_GUARD_CURSOR, id).await?;
        }
        if chunk.is_last {
            if let Some(seq) = chunk.decision_seq {
                meta_put(state, META_KNOWLEDGE_DECISION_CURSOR, &seq.to_string()).await?;
            }
            if let Some(seq) = chunk.spec_seq {
                meta_put(state, META_KNOWLEDGE_SPEC_CURSOR, &seq.to_string()).await?;
            }
            if let Some(rowid) = chunk.trailer_rowid {
                meta_put(state, META_KNOWLEDGE_TRAILER_CURSOR, &rowid.to_string()).await?;
            }
        }
    }
    Ok(Outcome::Synced)
}

async fn meta_i64(state: &AppState, key: &str) -> Result<i64, KError> {
    Ok(state
        .store
        .meta_get(key)
        .await
        .map_err(|e| KError::Fatal(format!("read {key}: {e}")))?
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0))
}

async fn meta_put(state: &AppState, key: &str, value: &str) -> Result<(), KError> {
    state
        .store
        .meta_set(key, value)
        .await
        .map_err(|e| KError::Fatal(format!("store {key}: {e}")))
}

/// Per-repo coverage/capture snapshot for every zavet-active repo the daemon
/// has a working directory for, throttled to once per [`STATS_TTL`] per repo.
/// Best-effort: a repo that fails to compute is simply omitted this round.
/// The paths that recorded knowledge claims to cover: active decisions' guard
/// globs ∪ EVERY spec's paths.
///
/// Specs are deliberately not filtered on `verified`. That flag records a
/// human review of whether a spec matches the code; it is not a statement
/// about whether the code is documented, and in practice nobody flips it —
/// across two repos using this daily, every spec sits at `verified: false`,
/// so gating on it made specs contribute exactly nothing and the metric read
/// as "guards only" while being labelled coverage.
///
/// It was also inconsistent: decisions are counted whenever they are `active`,
/// regardless of `origin`/`verified`, so an unverified reverse-engineered
/// decision already counts. Holding specs to a stricter standard than
/// decisions was an accident of implementation, not a rule anyone stated.
///
/// Superseded decisions drop out because a superseded guard enforces nothing —
/// that IS a statement about the code's current surface, unlike `verified`.
fn coverage_globs(decisions: &[ZavetDecisionRow], specs: &[ZavetSpecRow]) -> Vec<String> {
    let mut globs: Vec<String> = Vec::new();
    for d in decisions {
        if d.status.as_deref().unwrap_or("active") == "active" {
            globs.extend(d.guards.iter().cloned());
        }
    }
    for s in specs {
        globs.extend(s.paths.iter().cloned());
    }
    globs
}

async fn compute_repo_stats(state: &AppState) -> Vec<KnowledgeRepoStats> {
    let dirs: Vec<(String, String)> = match state.repo_dirs.lock() {
        Ok(map) => map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        Err(_) => return Vec::new(),
    };
    let now = time::OffsetDateTime::now_utc();
    let now_str = crate::heartbeat::fmt_rfc3339(now);
    let mut out = Vec::new();
    for (repo, dir) in dirs {
        let root = std::path::PathBuf::from(&dir);
        if !root.join(".zavet").is_dir() {
            continue;
        }
        // Throttle: skip when the last snapshot is younger than the TTL.
        let key = format!("{META_KNOWLEDGE_STATS_PREFIX}{repo}");
        if let Ok(Some(prev)) = state.store.meta_get(&key).await {
            if let Ok(prev) =
                time::OffsetDateTime::parse(&prev, &time::format_description::well_known::Rfc3339)
            {
                if now - prev < STATS_TTL {
                    continue;
                }
            }
        }
        // Coverage surface: active guard globs ∪ every spec's paths.
        let decisions = state
            .store
            .zavet_decisions_list(&repo)
            .await
            .unwrap_or_default();
        let specs = state
            .store
            .zavet_specs_list(&repo)
            .await
            .unwrap_or_default();
        let globs = coverage_globs(&decisions, &specs);
        let trailer_shas: HashSet<String> = state
            .store
            .zavet_trailer_shas(&repo)
            .await
            .unwrap_or_default()
            .into_iter()
            .collect();

        let root2 = root.clone();
        let globs2 = globs.clone();
        // One blocking git pass, now three commands: date the repo's adoption of
        // `.zavet/` first, so both counts are gathered over the SAME clamped
        // window rather than over 90 days of history that predates the practice
        // being measured (issue #67).
        let Ok((zavet_since, window, activity, covered)) = tokio::task::spawn_blocking(move || {
            let zavet_since = project::first_commit_date(&root2, ".zavet");
            let window = effective_window_days(zavet_since.as_deref(), now);
            let activity = project::knowledge_activity(&root2, window);
            let covered = project::paths_touched_since_days(&root2, window, &globs2);
            (zavet_since, window, activity, covered)
        })
        .await
        else {
            continue;
        };
        if activity.paths.is_empty() && activity.nontrivial_commits.is_empty() {
            // Nothing moved in the window — still stamp the throttle so quiet
            // repos don't re-walk 90 days of history every flush.
            let _ = state.store.meta_set(&key, &now_str).await;
            continue;
        }
        let trailer_commits = activity
            .nontrivial_commits
            .iter()
            .filter(|sha| trailer_shas.contains(*sha))
            .count() as u64;
        out.push(KnowledgeRepoStats {
            repo_canonical: repo.clone(),
            window_days: window,
            active_paths: activity.paths.len() as u64,
            covered_paths: covered.len() as u64,
            nontrivial_commits: activity.nontrivial_commits.len() as u64,
            trailer_commits,
            computed_at: now_str.clone(),
            zavet_since,
        });
        let _ = state.store.meta_set(&key, &now_str).await;
    }
    out
}

/// The window the stats actually cover: `STATS_WINDOW_DAYS`, clamped down to the
/// days since the repo adopted `.zavet/`.
///
/// Without the clamp the denominator answers a question nobody asked. On a repo
/// where `.zavet/` was initialised 10 days ago, a fixed 90-day window counts ~80
/// days of pre-zavet commits as "missed capture" and their paths as uncovered —
/// so capture ratio and knowledge coverage read far too low, which is exactly the
/// production report in issue #67. The arithmetic was never wrong; the window was.
///
/// Floored at 1 so a repo that adopted zavet today still divides by something, and
/// falls back to the full ceiling when the date is missing or unparseable — an
/// unknown adoption date must not silently shrink the window to nothing.
fn effective_window_days(zavet_since: Option<&str>, now: time::OffsetDateTime) -> u32 {
    let Some(since) = zavet_since
        .and_then(|s| {
            time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
        })
        .filter(|since| *since <= now)
    else {
        return STATS_WINDOW_DAYS;
    };
    // One clamp, both bounds: floor at 1 so today's adoption still divides by
    // something, ceiling at the window so the `u32` conversion cannot fail.
    let days = (now - since)
        .whole_days()
        .clamp(1, i64::from(STATS_WINDOW_DAYS));
    days as u32
}

/// The knowledge channel's [`HealthChannel`] keys: it reuses the attestation
/// channel's [`dira_core::sync::SyncHealth`] shape (`cursor` carries the
/// decision watermark; `cloud_watermark` is unused here).
const KNOWLEDGE_HEALTH_CHANNEL: HealthChannel = HealthChannel {
    log: "knowledge sync",
    health_key: META_KNOWLEDGE_HEALTH,
    cursor_key: META_KNOWLEDGE_DECISION_CURSOR,
    watermark_key: None,
};

/// Persist the knowledge channel's health snapshot to
/// [`META_KNOWLEDGE_HEALTH`] (see [`record_channel_health`]).
async fn record_health(
    state: &AppState,
    error_kind: Option<&str>,
    consecutive_failures: u32,
    backoff_secs: u64,
) {
    record_channel_health(
        state,
        &KNOWLEDGE_HEALTH_CHANNEL,
        error_kind,
        consecutive_failures,
        backoff_secs,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MockCloud, MockResp};
    use dira_core::store::{ZavetDecisionCapture, ZavetTrailer};

    fn at(rfc3339: &str) -> time::OffsetDateTime {
        time::OffsetDateTime::parse(rfc3339, &time::format_description::well_known::Rfc3339)
            .unwrap()
    }

    /// Issue #67: the ratios were reported as far too low on repos with long
    /// history. The arithmetic was right; the window was answering a question
    /// nobody asked — on a repo that adopted zavet 10 days ago, ~80 days of
    /// pre-zavet commits counted as missed capture.
    #[test]
    fn the_window_is_clamped_to_the_days_since_zavet_was_adopted() {
        let now = at("2026-07-30T12:00:00Z");
        assert_eq!(
            effective_window_days(Some("2026-07-20T12:00:00Z"), now),
            10,
            "a young zavet repo is measured over its own lifetime, not 90 days"
        );
        assert_eq!(
            effective_window_days(Some("2026-01-01T00:00:00Z"), now),
            STATS_WINDOW_DAYS,
            "a long-adopted repo still caps at the 90-day ceiling"
        );
    }

    /// An unknown or unusable adoption date must fall back to the full ceiling.
    /// Shrinking the window on missing data would corrupt the ratios in the other
    /// direction, which is worse than the bug being fixed.
    #[test]
    fn an_undatable_adoption_falls_back_to_the_ceiling() {
        let now = at("2026-07-30T12:00:00Z");
        assert_eq!(effective_window_days(None, now), STATS_WINDOW_DAYS);
        assert_eq!(
            effective_window_days(Some("not-a-date"), now),
            STATS_WINDOW_DAYS
        );
        // A future date (clock skew between the committer and this machine) is
        // not evidence of a zero-day window.
        assert_eq!(
            effective_window_days(Some("2027-01-01T00:00:00Z"), now),
            STATS_WINDOW_DAYS
        );
    }

    /// Adopted today: the window floors at 1 so the denominator is never zero.
    #[test]
    fn a_repo_that_adopted_zavet_today_still_has_a_one_day_window() {
        let now = at("2026-07-30T12:00:00Z");
        assert_eq!(effective_window_days(Some("2026-07-30T09:00:00Z"), now), 1);
    }

    fn decision_row(id: &str, status: &str, guards: &[&str]) -> ZavetDecisionRow {
        ZavetDecisionRow {
            id: id.into(),
            status: Some(status.into()),
            guards: guards.iter().map(|g| g.to_string()).collect(),
            ..Default::default()
        }
    }

    fn spec_row(slug: &str, verified: Option<bool>, paths: &[&str]) -> ZavetSpecRow {
        ZavetSpecRow {
            slug: slug.into(),
            verified,
            paths: paths.iter().map(|p| p.to_string()).collect(),
            ..Default::default()
        }
    }

    /// A spec covers code whether or not a human has reviewed it. `verified`
    /// records that review, not whether the code is documented — and since
    /// nobody flips it (every spec in the repos using this sits at `false`),
    /// gating on it made specs contribute nothing at all while the number was
    /// still labelled coverage.
    #[test]
    fn every_spec_counts_toward_coverage_verified_or_not() {
        let specs = vec![
            spec_row("reviewed", Some(true), &["src/a/**"]),
            spec_row("unreviewed", Some(false), &["src/b/**"]),
            spec_row("unstated", None, &["src/c/**"]),
        ];
        let globs = coverage_globs(&[], &specs);
        assert_eq!(globs, vec!["src/a/**", "src/b/**", "src/c/**"]);
    }

    /// The asymmetry that made the old rule wrong: an unverified decision has
    /// always counted, so holding specs to a stricter bar was an accident.
    #[test]
    fn decisions_and_specs_are_held_to_the_same_bar() {
        let decisions = vec![decision_row("D-0001", "active", &["src/d/**"])];
        let specs = vec![spec_row("s", Some(false), &["src/s/**"])];
        assert_eq!(
            coverage_globs(&decisions, &[]).len(),
            coverage_globs(&[], &specs).len(),
            "an unverified spec must contribute exactly what an unverified \
             decision does"
        );
    }

    /// Supersession IS a statement about the current surface, so it still
    /// filters — a superseded guard enforces nothing.
    #[test]
    fn superseded_decisions_drop_out_of_the_coverage_surface() {
        let decisions = vec![
            decision_row("D-0001", "active", &["src/live/**"]),
            decision_row("D-0002", "superseded", &["src/dead/**"]),
        ];
        assert_eq!(coverage_globs(&decisions, &[]), vec!["src/live/**"]);
    }

    fn capture(id: &str) -> ZavetDecisionCapture {
        ZavetDecisionCapture {
            id: id.to_string(),
            title: Some("Wire is metadata only".into()),
            path: format!(".zavet/decisions/{id}.md"),
            body_md: Some("SECRET-PROSE".into()),
            guards: vec!["src/**".into()],
            content_hash: Some("blob1".into()),
            ..Default::default()
        }
    }

    async fn linked_state(cloud: &MockCloud, knob: KnowledgeSyncMode) -> AppState {
        let store = dira_core::Store::open_in_memory().await.unwrap();
        dira_core::identity::set_device_id(&store, "01TESTDEVICE")
            .await
            .unwrap();
        store
            .zavet_upsert_decision(
                "github.com/acme/api",
                &capture("D-0001"),
                "sha1",
                None,
                None,
            )
            .await
            .unwrap();
        store
            .zavet_record_trailers(
                Some("github.com/acme/api"),
                "sha1",
                &[ZavetTrailer {
                    key: "why".into(),
                    value: "SECRET-TRAILER".into(),
                    decision_id: Some("D-0001".into()),
                }],
            )
            .await
            .unwrap();
        let config = dira_core::Config {
            cloud_url: Some(cloud.base_url().to_string()),
            sync: dira_core::config::SyncKnobs { knowledge: knob },
            ..Default::default()
        };
        let (state, _rx, _sync_rx, _knowledge_rx) =
            crate::build_state(store, config).await.unwrap();
        state
    }

    #[tokio::test]
    async fn knob_off_skips_without_touching_the_network() {
        let cloud = MockCloud::start(&["/api/v1/knowledge"]).await;
        let state = linked_state(&cloud, KnowledgeSyncMode::Off).await;
        let key = DeviceKey::generate();
        let out = flush_knowledge(&state, &key, &state.http).await.unwrap();
        assert!(matches!(out, Outcome::Skipped("off")));
        assert!(cloud.requests("/api/v1/knowledge").is_empty());
    }

    #[tokio::test]
    async fn cursor_advances_only_on_2xx_and_batch_id_is_stable_across_retries() {
        let cloud = MockCloud::start(&["/api/v1/knowledge"]).await;
        let state = linked_state(&cloud, KnowledgeSyncMode::Metadata).await;
        let key = DeviceKey::generate();

        cloud.push("/api/v1/knowledge", MockResp::status(500, "boom"));
        let err = flush_knowledge(&state, &key, &state.http).await;
        assert!(matches!(err, Err(KError::Transient { .. })));
        assert_eq!(
            state
                .store
                .meta_get(META_KNOWLEDGE_DECISION_CURSOR)
                .await
                .unwrap(),
            None,
            "cursor must not advance on a failed POST"
        );

        cloud.push(
            "/api/v1/knowledge",
            MockResp::ok(r#"{"status":"accepted","accepted":2}"#),
        );
        let out = flush_knowledge(&state, &key, &state.http).await.unwrap();
        assert!(matches!(out, Outcome::Synced));
        let cursor = state
            .store
            .meta_get(META_KNOWLEDGE_DECISION_CURSOR)
            .await
            .unwrap()
            .unwrap();
        assert!(cursor.parse::<i64>().unwrap() >= 1);

        // The retry re-sent byte-identical content: same deterministic batchId.
        let reqs = cloud.requests("/api/v1/knowledge");
        assert_eq!(reqs.len(), 2);
        let id_of = |body: &str| {
            let v: serde_json::Value = serde_json::from_str(body).unwrap();
            v["payload"]["batchId"].as_str().unwrap().to_string()
        };
        assert_eq!(id_of(&reqs[0]), id_of(&reqs[1]));

        // And a follow-up flush with nothing new does nothing.
        let out = flush_knowledge(&state, &key, &state.http).await.unwrap();
        assert!(matches!(out, Outcome::Nothing));
    }

    #[tokio::test]
    async fn metadata_tier_puts_no_content_on_the_wire() {
        let cloud = MockCloud::start(&["/api/v1/knowledge"]).await;
        let state = linked_state(&cloud, KnowledgeSyncMode::Metadata).await;
        let key = DeviceKey::generate();
        cloud.push("/api/v1/knowledge", MockResp::ok("{}"));
        flush_knowledge(&state, &key, &state.http).await.unwrap();
        let body = &cloud.requests("/api/v1/knowledge")[0];
        assert!(!body.contains("SECRET"), "no content at the metadata tier");
        assert!(!body.contains("bodyMd"));
        assert!(body.contains("\"tier\":\"metadata\""));
        assert!(body.contains("\"title\""), "titles are metadata");
        assert!(body.contains("\"recordSha\""));
    }

    #[tokio::test]
    async fn content_not_allowed_downgrades_the_window_once() {
        let cloud = MockCloud::start(&["/api/v1/knowledge"]).await;
        let state = linked_state(&cloud, KnowledgeSyncMode::Full).await;
        let key = DeviceKey::generate();
        cloud.push(
            "/api/v1/knowledge",
            MockResp::status(400, r#"{"error":"content_not_allowed"}"#),
        );
        cloud.push("/api/v1/knowledge", MockResp::ok("{}"));
        let out = flush_knowledge(&state, &key, &state.http).await.unwrap();
        assert!(matches!(out, Outcome::Synced));
        let reqs = cloud.requests("/api/v1/knowledge");
        assert_eq!(reqs.len(), 2);
        assert!(
            reqs[0].contains("SECRET-PROSE"),
            "first attempt was full-tier"
        );
        assert!(
            !reqs[1].contains("SECRET"),
            "retry was stripped to metadata"
        );
        assert!(reqs[1].contains("\"tier\":\"metadata\""));
    }

    #[tokio::test]
    async fn workspace_disabled_and_missing_endpoint_are_distinct_quiet_outcomes() {
        let cloud = MockCloud::start(&["/api/v1/knowledge"]).await;
        let state = linked_state(&cloud, KnowledgeSyncMode::Metadata).await;
        let key = DeviceKey::generate();
        cloud.push(
            "/api/v1/knowledge",
            MockResp::status(403, r#"{"error":"knowledge_disabled"}"#),
        );
        assert!(matches!(
            flush_knowledge(&state, &key, &state.http).await,
            Err(KError::Disabled)
        ));
        cloud.push("/api/v1/knowledge", MockResp::status(404, "not found"));
        assert!(matches!(
            flush_knowledge(&state, &key, &state.http).await.unwrap(),
            Outcome::Skipped("endpoint_missing")
        ));
    }

    #[tokio::test]
    async fn epoch_change_blanks_every_cursor_for_wipe_and_resync() {
        let cloud = MockCloud::start(&["/api/v1/knowledge"]).await;
        let state = linked_state(&cloud, KnowledgeSyncMode::Metadata).await;
        let key = DeviceKey::generate();
        state
            .store
            .meta_set(dira_core::sync::META_LAST_EPOCH, "ep-1")
            .await
            .unwrap();
        cloud.push(
            "/api/v1/knowledge",
            MockResp::ok(r#"{"status":"accepted","sync":{"dataEpoch":"ep-2"}}"#),
        );
        let out = flush_knowledge(&state, &key, &state.http).await.unwrap();
        assert!(matches!(out, Outcome::Synced));
        for cursor in [
            META_KNOWLEDGE_DECISION_CURSOR,
            META_KNOWLEDGE_SPEC_CURSOR,
            META_KNOWLEDGE_TRAILER_CURSOR,
            META_KNOWLEDGE_GUARD_CURSOR,
            dira_core::sync::META_SYNC_CURSOR,
            dira_core::sync::META_ARTIFACTS_CURSOR,
        ] {
            let v = state.store.meta_get(cursor).await.unwrap();
            assert!(
                v.as_deref().map(str::is_empty).unwrap_or(true),
                "{cursor} must be blank after an epoch change (got {v:?})"
            );
        }
        assert_eq!(
            state
                .store
                .meta_get(dira_core::sync::META_LAST_EPOCH)
                .await
                .unwrap()
                .as_deref(),
            Some("ep-2")
        );
        // The NEXT flush re-sends the whole backlog from scratch.
        cloud.push("/api/v1/knowledge", MockResp::ok("{}"));
        let out = flush_knowledge(&state, &key, &state.http).await.unwrap();
        assert!(matches!(out, Outcome::Synced));
        assert!(cloud.requests("/api/v1/knowledge")[1].contains("D-0001"));
    }
}
