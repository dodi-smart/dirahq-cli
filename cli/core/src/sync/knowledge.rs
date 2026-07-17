//! Knowledge sync (M2): turn "changed since cursor" zavet rows into
//! signed-ready [`dira_contract::KnowledgeBatch`] chunks. Pure, testable
//! derivation — the daemon (`dirad::knowledge_sync`) owns scheduling,
//! signing, and transport.
//!
//! Cursors are per-table because the tables move differently: decisions and
//! specs are upserted (their `touched_seq` watermark, migration 0004),
//! trailers are insert-only (rowid), guard events are ULID-keyed (id). All
//! four blank together on a `dataEpoch` change, on `nuke`, and on
//! `dira device resync` — wipe-and-resync reproduces cloud state because the
//! cloud is idempotent by natural keys.

use crate::store::{ZavetDecisionRow, ZavetGuardEventSyncRow, ZavetSpecRow, ZavetTrailerSyncRow};
use dira_contract::{
    KnowledgeBatch, KnowledgeDecision, KnowledgeGuardEvent, KnowledgeRepoStats, KnowledgeSpec,
    KnowledgeTrailerRef,
};
use serde::Deserialize;

/// `meta` key: highest `zavet_decisions.touched_seq` confirmed-synced.
pub const META_KNOWLEDGE_DECISION_CURSOR: &str = "knowledge_cursor_decision_seq";
/// `meta` key: highest `zavet_specs.touched_seq` confirmed-synced.
pub const META_KNOWLEDGE_SPEC_CURSOR: &str = "knowledge_cursor_spec_seq";
/// `meta` key: highest `zavet_trailers.rowid` confirmed-synced.
pub const META_KNOWLEDGE_TRAILER_CURSOR: &str = "knowledge_cursor_trailer_rowid";
/// `meta` key: highest `zavet_guard_events.id` (ULID) confirmed-synced.
pub const META_KNOWLEDGE_GUARD_CURSOR: &str = "knowledge_cursor_guard_event_id";
/// `meta` key: the knowledge channel's health snapshot (same shape discipline
/// as [`super::META_SYNC_HEALTH`]).
pub const META_KNOWLEDGE_HEALTH: &str = "knowledge_sync_health";
/// `meta` key prefix: RFC 3339 instant the per-repo stats snapshot was last
/// computed (`knowledge_stats_at:<canonical repo>`), throttling the git pass.
pub const META_KNOWLEDGE_STATS_PREFIX: &str = "knowledge_stats_at:";

/// Guard events per chunk — the only high-volume stream. Decisions, specs,
/// trailers, and repo stats ride the final chunk (mirroring how artifacts
/// ride the last attestation chunk).
pub const KNOWLEDGE_CHUNK_ITEMS: usize = 500;

/// The tier a batch is built at. `Off` never reaches here — the daemon gates
/// before building.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnowledgeTier {
    Metadata,
    Full,
}

impl KnowledgeTier {
    pub fn label(&self) -> &'static str {
        match self {
            KnowledgeTier::Metadata => "metadata",
            KnowledgeTier::Full => "full",
        }
    }
}

/// One POST-able chunk plus the cursor bookkeeping to apply on its 2xx.
#[derive(Debug, Clone)]
pub struct KnowledgeChunk {
    pub batch: KnowledgeBatch,
    /// High-water guard-event id in THIS chunk (advance per chunk).
    pub cursor_guard_id: Option<String>,
    /// Window high-waters for the upserted/insert-only tables — applied only
    /// on the LAST chunk's ack, like the artifacts cursor.
    pub decision_seq: Option<i64>,
    pub spec_seq: Option<i64>,
    pub trailer_rowid: Option<i64>,
    pub is_last: bool,
}

fn decision_to_wire(row: &ZavetDecisionRow, tier: KnowledgeTier) -> KnowledgeDecision {
    KnowledgeDecision {
        repo_canonical: row.repo.clone(),
        id: row.id.clone(),
        slug: row.slug.clone(),
        status: row.status.clone(),
        supersedes: row.supersedes.clone(),
        path: row.path.clone(),
        title: row.title.clone(),
        record_sha: row.content_hash.clone(),
        first_commit: row.first_commit.clone(),
        last_commit: row.last_commit.clone(),
        created_at: row.created_at.clone(),
        updated_at: row.updated_at.clone(),
        source_session: row.source_session.clone(),
        origin: row.origin.clone(),
        verified: row.verified,
        guards: row.guards.clone(),
        // Content flows only at the full tier — the metadata arm never even
        // reads the column's value into the wire struct.
        body_md: match tier {
            KnowledgeTier::Full => row.body_md.clone(),
            KnowledgeTier::Metadata => None,
        },
    }
}

fn spec_to_wire(row: &ZavetSpecRow, tier: KnowledgeTier) -> KnowledgeSpec {
    KnowledgeSpec {
        repo_canonical: row.repo.clone(),
        slug: row.slug.clone(),
        version: row.version.max(0) as u64,
        origin: row.origin.clone(),
        confidence: row.confidence.clone(),
        date: row.date.clone(),
        verified: row.verified,
        path: row.path.clone(),
        title: row.title.clone(),
        record_sha: row.content_hash.clone(),
        first_commit: row.first_commit.clone(),
        last_commit: row.last_commit.clone(),
        created_at: row.created_at.clone(),
        updated_at: row.updated_at.clone(),
        source_session: row.source_session.clone(),
        paths: row.paths.clone(),
        decisions: row.decisions.clone(),
        body_md: match tier {
            KnowledgeTier::Full => row.body_md.clone(),
            KnowledgeTier::Metadata => None,
        },
    }
}

fn trailer_to_wire(row: &ZavetTrailerSyncRow, tier: KnowledgeTier) -> KnowledgeTrailerRef {
    KnowledgeTrailerRef {
        repo_canonical: row.repo.clone(),
        sha: row.sha.clone(),
        seq: row.seq.max(0) as u32,
        key: row.key.clone(),
        decision_id: row.decision_id.clone(),
        value: match tier {
            KnowledgeTier::Full => row.value.clone(),
            KnowledgeTier::Metadata => None,
        },
    }
}

fn event_to_wire(row: &ZavetGuardEventSyncRow) -> KnowledgeGuardEvent {
    KnowledgeGuardEvent {
        id: row.id.clone(),
        at: row.at.clone(),
        repo_canonical: row.repo.clone(),
        decision_id: row.decision_id.clone(),
        kind: row.kind.clone(),
        file_path: row.file_path.clone(),
        session_id: row.session_id.clone(),
    }
}

/// Deterministic batch id over the covered item identities + tier: a
/// crash-retry of the same window re-sends identical bytes and the cloud
/// no-ops on the duplicate `batchId`; a knob upgrade to `full` re-ships the
/// same items under a NEW id so the cloud re-unpacks with content.
/// `repo_stats` deliberately does not participate — it is a snapshot, not a
/// cursor stream, and must not change the identity of the covered window.
///
/// Public so the daemon can recompute the id after downgrading a batch in
/// place with [`KnowledgeBatch::strip_content`] (the `content_not_allowed`
/// retry): the tier participates in the id, so the stripped re-send must not
/// reuse the full-tier id.
pub fn knowledge_batch_id(batch: &KnowledgeBatch) -> String {
    let mut ids: Vec<String> = Vec::new();
    for d in &batch.decisions {
        ids.push(format!(
            "d:{}:{}:{}",
            d.repo_canonical,
            d.id,
            d.record_sha.as_deref().unwrap_or("")
        ));
    }
    for s in &batch.specs {
        ids.push(format!(
            "s:{}:{}:{}",
            s.repo_canonical,
            s.slug,
            s.record_sha.as_deref().unwrap_or("")
        ));
    }
    for t in &batch.trailer_refs {
        ids.push(format!("t:{}:{}", t.sha, t.seq));
    }
    for g in &batch.guard_events {
        ids.push(format!("g:{}", g.id));
    }
    ids.push(format!("tier:{}", batch.tier));
    ids.sort_unstable();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let lo = super::batch::fnv1a(&refs, 0xcbf29ce484222325);
    let hi = super::batch::fnv1a(&refs, 0x100000001b3);
    ulid::Ulid::from(((hi as u128) << 64) | lo as u128).to_string()
}

/// Build the chunked batches for one flush window. Guard events split into
/// [`KNOWLEDGE_CHUNK_ITEMS`]-sized chunks; every other stream (and the stats
/// snapshot) rides the last chunk. An entirely empty window yields no chunks.
#[allow(clippy::too_many_arguments)]
pub fn build_knowledge_batches(
    device_id: &str,
    generated_at: &str,
    tier: KnowledgeTier,
    decisions: &[(i64, ZavetDecisionRow)],
    specs: &[(i64, ZavetSpecRow)],
    trailers: &[ZavetTrailerSyncRow],
    guard_events: &[ZavetGuardEventSyncRow],
    repo_stats: Vec<KnowledgeRepoStats>,
) -> Vec<KnowledgeChunk> {
    let has_tail = !decisions.is_empty()
        || !specs.is_empty()
        || !trailers.is_empty()
        || !repo_stats.is_empty();
    if guard_events.is_empty() && !has_tail {
        return Vec::new();
    }

    let decision_seq = decisions.iter().map(|(s, _)| *s).max();
    let spec_seq = specs.iter().map(|(s, _)| *s).max();
    let trailer_rowid = trailers.iter().map(|t| t.rowid).max();

    let mut chunks: Vec<KnowledgeChunk> = Vec::new();
    let mut event_slices: Vec<&[ZavetGuardEventSyncRow]> =
        guard_events.chunks(KNOWLEDGE_CHUNK_ITEMS).collect();
    if event_slices.is_empty() {
        event_slices.push(&[]);
    }
    let n = event_slices.len();
    for (i, slice) in event_slices.into_iter().enumerate() {
        let is_last = i + 1 == n;
        let mut batch = KnowledgeBatch {
            batch_id: String::new(),
            device_id: device_id.to_string(),
            generated_at: generated_at.to_string(),
            tier: tier.label().to_string(),
            decisions: Vec::new(),
            specs: Vec::new(),
            trailer_refs: Vec::new(),
            guard_events: slice.iter().map(event_to_wire).collect(),
            repo_stats: Vec::new(),
        };
        if is_last {
            batch.decisions = decisions
                .iter()
                .map(|(_, d)| decision_to_wire(d, tier))
                .collect();
            batch.specs = specs.iter().map(|(_, s)| spec_to_wire(s, tier)).collect();
            batch.trailer_refs = trailers.iter().map(|t| trailer_to_wire(t, tier)).collect();
            batch.repo_stats = repo_stats.clone();
        }
        batch.batch_id = knowledge_batch_id(&batch);
        chunks.push(KnowledgeChunk {
            cursor_guard_id: slice.iter().map(|e| e.id.clone()).max(),
            decision_seq: if is_last { decision_seq } else { None },
            spec_seq: if is_last { spec_seq } else { None },
            trailer_rowid: if is_last { trailer_rowid } else { None },
            is_last,
            batch,
        });
    }
    chunks
}

/// The knowledge endpoint's response `sync` block (unsigned, response-only,
/// tolerant — mirrors [`super::handshake::SyncBlock`] with the channel's own
/// watermark name).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnowledgeSyncBlock {
    /// Cloud's persisted knowledge watermark for this device. Advisory.
    #[serde(default)]
    pub knowledge_synced_id: Option<String>,
    /// Opaque token that changes when the cloud's data was reset.
    #[serde(default)]
    pub data_epoch: Option<String>,
}

/// Tolerantly-parsed `POST /api/v1/knowledge` response body (2xx or 4xx).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnowledgeResponse {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub batch_id: String,
    /// Stable machine code on errors, e.g. `knowledge_disabled`,
    /// `content_not_allowed`, `unknown_device`, `bad_signature`.
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub accepted: u64,
    #[serde(default)]
    pub duplicates: u64,
    #[serde(default)]
    pub sync: KnowledgeSyncBlock,
}

/// Parse a knowledge response body. Tolerant: missing/empty/non-JSON bodies
/// yield all-default.
pub fn parse_knowledge_response(body: &str) -> KnowledgeResponse {
    serde_json::from_str(body).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decision(id: &str, seq: i64) -> (i64, ZavetDecisionRow) {
        (
            seq,
            ZavetDecisionRow {
                repo: "github.com/acme/api".into(),
                id: id.into(),
                title: Some("T".into()),
                path: format!(".zavet/decisions/{id}.md"),
                body_md: Some("secret prose".into()),
                content_hash: Some("blob".into()),
                guards: vec!["src/**".into()],
                ..Default::default()
            },
        )
    }

    fn event(id: &str) -> ZavetGuardEventSyncRow {
        ZavetGuardEventSyncRow {
            id: id.into(),
            at: "t".into(),
            repo: Some("github.com/acme/api".into()),
            decision_id: "D-0001".into(),
            kind: "guard_blocked".into(),
            file_path: None,
            session_id: None,
        }
    }

    #[test]
    fn empty_window_builds_nothing() {
        let chunks = build_knowledge_batches(
            "d",
            "t",
            KnowledgeTier::Metadata,
            &[],
            &[],
            &[],
            &[],
            Vec::new(),
        );
        assert!(chunks.is_empty());
    }

    #[test]
    fn guard_events_chunk_and_tail_rides_last() {
        let events: Vec<_> = (0..KNOWLEDGE_CHUNK_ITEMS + 3)
            .map(|i| event(&format!("01J{i:06}")))
            .collect();
        let ds = [decision("D-0001", 7)];
        let chunks = build_knowledge_batches(
            "d",
            "t",
            KnowledgeTier::Metadata,
            &ds,
            &[],
            &[],
            &events,
            Vec::new(),
        );
        assert_eq!(chunks.len(), 2);
        assert!(!chunks[0].is_last);
        assert!(chunks[0].batch.decisions.is_empty());
        assert_eq!(chunks[0].batch.guard_events.len(), KNOWLEDGE_CHUNK_ITEMS);
        assert!(chunks[0].decision_seq.is_none());
        assert!(chunks[1].is_last);
        assert_eq!(chunks[1].batch.guard_events.len(), 3);
        assert_eq!(chunks[1].batch.decisions.len(), 1);
        assert_eq!(chunks[1].decision_seq, Some(7));
        // Per-chunk guard cursor is that chunk's max event id.
        assert_eq!(
            chunks[0].cursor_guard_id.as_deref(),
            Some(format!("01J{:06}", KNOWLEDGE_CHUNK_ITEMS - 1).as_str())
        );
    }

    #[test]
    fn batch_id_is_deterministic_and_tier_sensitive() {
        let ds = [decision("D-0001", 1)];
        let build = |tier| {
            build_knowledge_batches("d", "t", tier, &ds, &[], &[], &[], Vec::new())
                .pop()
                .unwrap()
                .batch
                .batch_id
        };
        let a = build(KnowledgeTier::Metadata);
        let b = build(KnowledgeTier::Metadata);
        let c = build(KnowledgeTier::Full);
        assert_eq!(a, b, "same window + tier must re-produce the same id");
        assert_ne!(a, c, "a tier upgrade must produce a new id");
    }

    #[test]
    fn metadata_tier_never_serializes_content() {
        let ds = [decision("D-0001", 1)];
        let trailers = [ZavetTrailerSyncRow {
            rowid: 1,
            sha: "sha".into(),
            repo: None,
            key: "why".into(),
            value: Some("secret trailer prose".into()),
            decision_id: None,
            seq: 0,
        }];
        let chunk = build_knowledge_batches(
            "d",
            "t",
            KnowledgeTier::Metadata,
            &ds,
            &[],
            &trailers,
            &[],
            Vec::new(),
        )
        .pop()
        .unwrap();
        let json = serde_json::to_string(&chunk.batch).unwrap();
        assert!(!json.contains("secret"));
        assert!(!json.contains("bodyMd"));
        assert!(!json.contains("\"value\""));
        // Titles are metadata by design and survive.
        assert!(json.contains("\"title\""));
    }

    #[test]
    fn full_tier_carries_content() {
        let ds = [decision("D-0001", 1)];
        let chunk = build_knowledge_batches(
            "d",
            "t",
            KnowledgeTier::Full,
            &ds,
            &[],
            &[],
            &[],
            Vec::new(),
        )
        .pop()
        .unwrap();
        let json = serde_json::to_string(&chunk.batch).unwrap();
        assert!(json.contains("secret prose"));
    }

    #[test]
    fn knowledge_response_parses_tolerantly() {
        let r = parse_knowledge_response(
            r#"{"status":"accepted","batchId":"01K","accepted":3,"duplicates":1,
                "sync":{"knowledgeSyncedId":"01X","dataEpoch":"ep-2"}}"#,
        );
        assert_eq!(r.status, "accepted");
        assert_eq!(r.accepted, 3);
        assert_eq!(r.sync.data_epoch.as_deref(), Some("ep-2"));
        let empty = parse_knowledge_response("");
        assert!(empty.error.is_empty());
        let err = parse_knowledge_response(r#"{"error":"content_not_allowed"}"#);
        assert_eq!(err.error, "content_not_allowed");
    }
}
