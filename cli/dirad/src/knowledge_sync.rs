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
//!   not opted into — rebuild the same window at the metadata tier and retry
//!   once. Sync never wedges; content never lands without both consents.
//!
//! A 404 means the cloud predates the endpoint — treated as quietly skipped
//! (health kind `endpoint_missing`), not an error worth logging loudly.

use crate::state::AppState;
use crate::sync::retry_after_from_headers;
use dira_contract::{KnowledgeEnvelope, KnowledgeRepoStats, SCHEMA_VERSION};
use dira_core::identity;
use dira_core::signing::DeviceKey;
use dira_core::sync::knowledge::{
    build_knowledge_batches, parse_knowledge_response, KnowledgeChunk, KnowledgeTier,
    KNOWLEDGE_CHUNK_ITEMS, META_KNOWLEDGE_DECISION_CURSOR, META_KNOWLEDGE_GUARD_CURSOR,
    META_KNOWLEDGE_HEALTH, META_KNOWLEDGE_SPEC_CURSOR, META_KNOWLEDGE_STATS_PREFIX,
    META_KNOWLEDGE_TRAILER_CURSOR,
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
/// Cap for exponential backoff after a failure.
const MAX_BACKOFF: StdDuration = StdDuration::from_secs(300);
/// HTTP timeout for a single knowledge chunk POST.
const HTTP_TIMEOUT: StdDuration = StdDuration::from_secs(30);
/// Rolling window for the per-repo coverage/capture snapshot.
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
            Err(KError::Transient {
                message,
                retry_after,
            }) => {
                backoff = retry_after
                    .unwrap_or_else(|| next_backoff(backoff))
                    .min(MAX_BACKOFF);
                consecutive_failures += 1;
                tracing::warn!(
                    "knowledge sync: transient failure, backing off {backoff:?}: {message}"
                );
                record_health(
                    &state,
                    Some("transient"),
                    consecutive_failures,
                    backoff.as_secs(),
                )
                .await;
                sleep(backoff).await;
            }
            Err(KError::ReLinkRequired) => {
                backoff = StdDuration::ZERO;
                consecutive_failures += 1;
                tracing::error!(
                    "knowledge sync: cloud rejected device (unknown_device) — re-link required"
                );
                record_health(
                    &state,
                    Some("unknown_device"),
                    consecutive_failures,
                    backoff.as_secs(),
                )
                .await;
            }
            Err(KError::SignatureRejected) => {
                // The attestation task owns the pending-key self-heal (WP-B1b);
                // a rotation it promotes invalidates the shared cached key, so
                // this task recovers on its next tick without duplicating that
                // machinery here. Until then, ceiling backoff.
                backoff = MAX_BACKOFF;
                consecutive_failures += 1;
                tracing::error!(
                    "knowledge sync: signature rejected — waiting for the attestation task's \
                     key recovery (or `dira device link`)"
                );
                record_health(
                    &state,
                    Some("signature_rejected"),
                    consecutive_failures,
                    backoff.as_secs(),
                )
                .await;
                sleep(backoff).await;
            }
            Err(KError::Disabled) => {
                // Workspace-level opt-out — not a local failure; long quiet backoff.
                backoff = MAX_BACKOFF;
                consecutive_failures = 0;
                tracing::info!(
                    "knowledge sync: workspace has knowledge sync disabled (knowledge_disabled) \
                     — enable it in the dashboard's Connections screen to start receiving"
                );
                record_health(&state, Some("knowledge_disabled"), 0, backoff.as_secs()).await;
                sleep(backoff).await;
            }
            Err(KError::SchemaSkew(body)) => {
                backoff = MAX_BACKOFF;
                consecutive_failures += 1;
                tracing::error!(
                    "knowledge sync: cloud rejected our contract version — upgrade daemon or \
                     cloud: {body}"
                );
                record_health(
                    &state,
                    Some("schema_skew"),
                    consecutive_failures,
                    backoff.as_secs(),
                )
                .await;
                sleep(backoff).await;
            }
            Err(KError::Fatal(e)) => {
                backoff = next_backoff(backoff);
                consecutive_failures += 1;
                tracing::warn!("knowledge sync: error, backing off {backoff:?}: {e}");
                record_health(
                    &state,
                    Some("fatal"),
                    consecutive_failures,
                    backoff.as_secs(),
                )
                .await;
                sleep(backoff).await;
            }
        }
    }
}

fn next_backoff(prev: StdDuration) -> StdDuration {
    if prev.is_zero() {
        StdDuration::from_secs(5)
    } else {
        (prev * 2).min(MAX_BACKOFF)
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
        Err(KError::Transient {
            message,
            retry_after,
        }) if message == CONTENT_NOT_ALLOWED_MARKER => {
            let _ = retry_after;
            // The workspace accepts metadata only — downgrade THIS window and
            // retry once. Re-sent metadata-only chunks that overlap already-
            // accepted ones dedup by batch id / natural keys.
            if tier == KnowledgeTier::Full {
                tracing::warn!(
                    "knowledge sync: workspace has not opted into content — re-sending this \
                     window at the metadata tier (set [sync] knowledge = \"metadata\" locally, \
                     or opt the workspace in, to silence this)"
                );
                let stripped = build_knowledge_batches(
                    &device_id,
                    &now,
                    KnowledgeTier::Metadata,
                    &decisions,
                    &specs,
                    &trailers,
                    &guard_events,
                    Vec::new(),
                );
                post_chunks(state, device_key, client, &cloud_url, &stripped).await
            } else {
                Err(KError::Fatal(
                    "cloud answered content_not_allowed to a metadata-tier batch".into(),
                ))
            }
        }
        other => other,
    }
}

/// Internal marker distinguishing `content_not_allowed` inside the transient
/// plumbing without another enum variant leaking into the run loop.
const CONTENT_NOT_ALLOWED_MARKER: &str = "\u{1}content_not_allowed";

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
                "content_not_allowed" => KError::Transient {
                    message: CONTENT_NOT_ALLOWED_MARKER.to_string(),
                    retry_after: None,
                },
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
        // Coverage surface: active guard globs ∪ verified specs' paths.
        let mut globs: Vec<String> = Vec::new();
        if let Ok(decisions) = state.store.zavet_decisions_list(&repo).await {
            for d in decisions {
                if d.status.as_deref().unwrap_or("active") == "active" {
                    globs.extend(d.guards);
                }
            }
        }
        if let Ok(specs) = state.store.zavet_specs_list(&repo).await {
            for s in specs {
                if s.verified == Some(true) {
                    globs.extend(s.paths);
                }
            }
        }
        let trailer_shas: HashSet<String> = state
            .store
            .zavet_trailer_shas(&repo)
            .await
            .unwrap_or_default()
            .into_iter()
            .collect();

        let root2 = root.clone();
        let globs2 = globs.clone();
        let Ok((activity, covered)) = tokio::task::spawn_blocking(move || {
            let activity = project::knowledge_activity(&root2, STATS_WINDOW_DAYS);
            let covered = project::paths_touched_since_days(&root2, STATS_WINDOW_DAYS, &globs2);
            (activity, covered)
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
            window_days: STATS_WINDOW_DAYS,
            active_paths: activity.paths.len() as u64,
            covered_paths: covered.len() as u64,
            nontrivial_commits: activity.nontrivial_commits.len() as u64,
            trailer_commits,
            computed_at: now_str.clone(),
        });
        let _ = state.store.meta_set(&key, &now_str).await;
    }
    out
}

/// Persist the knowledge channel's health snapshot to
/// [`META_KNOWLEDGE_HEALTH`], reusing the attestation channel's
/// [`dira_core::sync::SyncHealth`] shape (`cursor` carries the decision
/// watermark; `cloud_watermark` is unused here).
async fn record_health(
    state: &AppState,
    error_kind: Option<&str>,
    consecutive_failures: u32,
    backoff_secs: u64,
) {
    use dira_core::sync::parse_sync_health;
    let now = crate::heartbeat::fmt_rfc3339(time::OffsetDateTime::now_utc());
    let mut health = state
        .store
        .meta_get(META_KNOWLEDGE_HEALTH)
        .await
        .ok()
        .flatten()
        .and_then(|j| parse_sync_health(&j))
        .unwrap_or_default();
    health.last_attempt_at = Some(now.clone());
    health.consecutive_failures = consecutive_failures;
    health.backoff_secs = backoff_secs;
    health.cursor = state
        .store
        .meta_get(META_KNOWLEDGE_DECISION_CURSOR)
        .await
        .ok()
        .flatten()
        .filter(|s| !s.is_empty());
    match error_kind {
        None => {
            health.last_success_at = Some(now);
            health.last_error_kind = None;
        }
        Some(kind) => health.last_error_kind = Some(kind.to_string()),
    }
    if let Ok(json) = serde_json::to_string(&health) {
        let _ = state.store.meta_set(META_KNOWLEDGE_HEALTH, &json).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MockCloud, MockResp};
    use dira_core::store::{ZavetDecisionCapture, ZavetTrailer};

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
