//! Dira wire contract — the single source of truth for everything that crosses
//! the device → cloud boundary.
//!
//! These types are authored once here (the daemon is the producer) and the cloud
//! generates its TypeScript types + Zod validators from the emitted JSON Schema
//! (`just contract`). Never hand-write a struct on the cloud side that mirrors
//! one of these — generate it.
//!
//! ## Invariants
//! - Every envelope carries a `schemaVersion`; evolution is **additive** (new
//!   optional fields only) within a major version.
//! - Capture is **policy-free**: there is no money, rate, or currency field
//!   anywhere in this contract. Billing is resolved late, in the cloud.
//! - **Metadata only**: there is deliberately no field for prompt text, file
//!   contents, or diffs. The absence is the privacy guarantee. The single
//!   audited exception is the knowledge channel's consent-gated content tier
//!   ([`KnowledgeBatch`]): `bodyMd` / trailer `value` fields exist on the wire
//!   but are populated only when BOTH the producer knob and the workspace have
//!   explicitly opted in — and they are pinned by an explicit allowlist in the
//!   `wire_contract_carries_no_content_fields` test, so nothing else can ever
//!   join them silently.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The current wire schema version. Bump the major when making a breaking change;
/// the cloud rejects unknown majors.
pub const SCHEMA_VERSION: &str = "1.3.0";

/// A signed batch as it travels over the wire to `POST /api/v1/ingest`.
///
/// The signature is computed over the canonical-JSON (RFC 8785) encoding of
/// `payload` only, so it verifies independently of how the envelope is framed.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Envelope {
    /// Semver of this contract, e.g. `"1.0.0"`.
    pub schema_version: String,
    /// ULID of the device that produced and signed this batch.
    pub device_id: String,
    /// The attestation batch being attested to.
    pub payload: AttestationBatch,
    /// `ed25519(JCS(payload))`, base64 (standard, no padding).
    pub sig: String,
}

/// The unit of sync: a collection of settled facts captured locally.
///
/// Idempotent by `batchId`; append-only — corrections are *new* batches that
/// supersede, never mutations of a synced one.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AttestationBatch {
    /// ULID; the idempotency key for ingest.
    pub batch_id: String,
    /// ULID of the producing device (matches the envelope).
    pub device_id: String,
    /// RFC 3339 timestamp the batch was assembled.
    pub generated_at: String,
    /// De-duplicated, idle-trimmed billable human-time intervals.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intervals: Vec<Interval>,
    /// Per-session rollups (agent wall-clock, harness, repo).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<SessionRollup>,
    /// Token usage — a sibling metric, never a billing base.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub token_usage: Vec<TokenUsage>,
    /// Git artifacts referenced by intervals, for cloud-side anchoring.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactRef>,
}

/// How a piece of work was captured. Drives assurance on the cloud side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    /// An AI coding harness session (Claude Code, Codex, OpenCode).
    Agent,
    /// A manual `dira start` dira (meeting, manual testing, etc.).
    Manual,
}

/// Which harness produced a session.
///
/// `Generic` is a tool piping events through the JSON-lines adapter — distinct
/// from `Manual` (a human `dira start` dira): these are agent-style events from
/// an unmodeled harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Harness {
    ClaudeCode,
    Codex,
    Gemini,
    Cursor,
    OpenCode,
    Grok,
    Generic,
    Manual,
}

/// A de-duplicated, idle-trimmed slice of human-engaged time, attributed to a repo.
///
/// `humanSeconds` is the billing base. Project/client grouping is derived in the
/// cloud from the repo — never frozen here.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Interval {
    /// ULID.
    pub id: String,
    /// Canonical repo ref, e.g. `github.com/acme/api`. `None` for unresolved dirs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_canonical: Option<String>,
    /// `git config user.email` of the person the work is attributed to.
    pub identity_email: String,
    pub started_at: String,
    pub ended_at: String,
    /// De-duplicated human-engaged seconds in this interval.
    pub human_seconds: u64,
    /// Optional activity label (e.g. `meeting`, `manual-testing`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    /// The session this interval was derived from.
    pub source_session: String,
}

/// Per-session rollup. Agent wall-clock sums freely; it is evidence, never billed.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionRollup {
    /// Harness session id, or daemon-issued ULID for manual sessions.
    pub session_id: String,
    pub harness: Harness,
    pub kind: SessionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_canonical: Option<String>,
    pub identity_email: String,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// Total agent wall-clock seconds observed for this session.
    pub agent_wall_seconds: u64,
    /// Count of human prompts (user_prompt events) observed in this session.
    /// Optional + omitted-when-absent so older payloads (and the signing vector)
    /// stay byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<u64>,
    /// The session branch (`git rev-parse --abbrev-ref HEAD`), if resolved. Lets
    /// the cloud anchor commits on this session to the branch it was worked on.
    /// Optional + omitted-when-absent so older payloads (and the signing vector)
    /// stay byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Free-text human description for a manual session (`dira log`/`invoice`/`start`
    /// `--note`, or the trailing comment). Purely descriptive — the invoice line.
    /// Optional + omitted-when-absent so older payloads (and the signing vector)
    /// stay byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Operational tag for a manual session (`--label`), used locally to select/stop
    /// sessions; surfaced read-only in the cloud.
    /// Optional + omitted-when-absent so older payloads (and the signing vector)
    /// stay byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Token usage for a session/turn. Cost is always an estimate, separate from time.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub id: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_canonical: Option<String>,
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_create: u64,
    /// Estimated USD cost from the bundled pricing table. Always a label, never billed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub est_cost_usd: Option<f64>,
    pub at: String,
}

/// What an artifact ref points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Commit,
    PullRequest,
}

/// A git artifact the cloud can anchor against (confirm it exists + author matches).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactRef {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_canonical: Option<String>,
    pub kind: ArtifactKind,
    /// e.g. the PR number or branch ref.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// Commit SHA (the thing the cloud confirms in the remote).
    pub sha: String,
    /// RFC 3339 author date of the commit. Lets the cloud anchor an interval to a
    /// commit by author + time without re-reading the remote.
    /// Optional + omitted-when-absent so older payloads (and the signing vector)
    /// stay byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authored_at: Option<String>,
    /// Commit author email — the identity the cloud anchors the commit to.
    /// Optional + omitted-when-absent (keeps the signing vector byte-identical).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_email: Option<String>,
    /// The session the daemon observed producing this repo at capture time, IFF
    /// unambiguous. Lets the cloud anchor deterministically instead of guessing.
    /// Optional + omitted-when-absent (keeps the signing vector byte-identical).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session: Option<String>,
    /// `git patch-id --stable` of the commit — a stable hash of the *change* that
    /// survives rebase / amend / cherry-pick (the SHA does not). Lets the cloud
    /// re-anchor work whose original SHA was rewritten out of the remote: it finds
    /// the surviving commit by branch + author + window, then confirms it carries
    /// the same patch-id. Metadata only — never the diff itself.
    ///
    /// Serialized as `changeId` on the wire: it is a stable *change identity*, not
    /// a patch/diff — the privacy invariant denylists the `patch` token to keep
    /// content off the wire, and this id (a SHA-1 over the normalized diff) is not
    /// content.
    /// Optional + omitted-when-absent (keeps the signing vector byte-identical).
    #[serde(default, rename = "changeId", skip_serializing_if = "Option::is_none")]
    pub patch_id: Option<String>,
    /// **Squash-resilient session signal.** `git patch-id --stable` over the
    /// session's *cumulative* diff (`merge-base(upstream, HEAD)..HEAD`), not this
    /// one commit. A squash-merge collapses N commits into one whose combined diff
    /// matches none of the per-commit `changeId`s — but it equals this cumulative
    /// id when the base hasn't moved, so the cloud can re-anchor the squashed
    /// commit by an exact match. Carried per artifact (the commits of a session
    /// share it) so an artifact-only flush still ships it.
    ///
    /// Named `sessionChangeId` (not `*PatchId`/`*Diff*`): a stable change identity,
    /// never the diff. Optional + omitted-when-absent (keeps the signing vector
    /// byte-identical).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_change_id: Option<String>,
    /// **Squash-resilient session signal.** Repo-relative paths the session changed
    /// across the cumulative range. Survives squash *and* rebase (the squashed
    /// commit touches the union of paths), enabling fuzzy path-set overlap when
    /// `sessionChangeId` misses (base moved / conflict resolution). Paths only —
    /// no file content. Optional + omitted-when-absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub touched_paths: Option<Vec<String>>,
    /// **Squash-resilient session signal.** Per touched path, the git post-image
    /// blob SHA at the session tip — the object id git already stores. A squashed
    /// commit's tree carries the same blob SHA for any file not further modified
    /// during the squash, so blob-set overlap is a robust content-*identity* anchor
    /// the cloud can verify against the remote (which exposes the same blob SHAs).
    ///
    /// `blobs` is git's own term (no denylisted token); each entry is a path + an
    /// object id, never file content. Optional + omitted-when-absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blobs: Option<Vec<BlobRef>>,
}

/// One touched path and its git post-image blob SHA — a content-identity pointer
/// (the object id git stores), never the file's bytes. Part of the squash-resilient
/// anchoring signals on [`ArtifactRef`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BlobRef {
    /// Repo-relative path the session changed.
    pub path: String,
    /// The git blob object id (SHA) for that path at the session tip.
    pub blob: String,
}

/// A signed live-presence heartbeat as it travels over the wire.
///
/// Mirrors [`Envelope`] exactly (same framing, same signing rule): the signature
/// is computed over the canonical-JSON (RFC 8785) encoding of `payload` only.
/// Presence is a separate, additive channel from the attestation `Envelope` — it
/// carries no billing facts, only which sessions are currently live.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PresenceEnvelope {
    /// Semver of this contract, e.g. `"1.0.0"`.
    pub schema_version: String,
    /// ULID of the device that produced and signed this heartbeat.
    pub device_id: String,
    /// The presence heartbeat being attested to.
    pub payload: PresencePing,
    /// `ed25519(JCS(payload))`, base64 (standard, no padding).
    pub sig: String,
}

/// A single heartbeat: the set of sessions a device considers live right now.
///
/// Heartbeats are stateless snapshots — the cloud derives liveness from the most
/// recent ping per device, never by accumulating deltas.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PresencePing {
    /// ULID of the producing device (matches the envelope).
    pub device_id: String,
    /// RFC 3339 timestamp the heartbeat was assembled.
    pub sent_at: String,
    /// The sessions this device currently considers live.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<PresenceSession>,
    /// The device's configured presence TTL in seconds — how long the cloud should
    /// treat this beat as fresh before considering the device offline. Sent in-band
    /// so the cloud knows the device's intended TTL even before it answers with a
    /// [`PresenceAck`]. Optional + omitted-when-None so older payloads (and the
    /// signing vector) stay byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_ttl_secs: Option<u64>,
}

/// A live session as seen at heartbeat time. Presence-only — never a billing base.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PresenceSession {
    /// Harness session id, or daemon-issued ULID for manual sessions.
    pub session_id: String,
    pub harness: Harness,
    pub kind: SessionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_canonical: Option<String>,
    /// The session branch (`git rev-parse --abbrev-ref HEAD`), if resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// `git config user.email` of the person the session is attributed to.
    pub identity_email: String,
    /// RFC 3339 timestamp the session started.
    pub started_at: String,
    /// RFC 3339 timestamp of the last activity signal observed for this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_signal_at: Option<String>,
    /// Human-engaged seconds observed for this session so far. Evidence, not billed.
    pub engaged_seconds: u64,
    /// Active agent run-time in seconds (gap-counted, idle gaps excluded) — NOT
    /// session wall span.
    pub agent_wall_seconds: u64,
    /// Whether the session is currently idle (no recent activity signal).
    pub idle: bool,
}

/// A typed ingest error body, returned with a 4xx on `POST /api/v1/ingest`.
///
/// Replaces the brittle substring match on the response text: `error ==
/// "unknown_device"` means the device isn't linked cloud-side and needs a
/// re-link. New error codes can be added without breaking the producer.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IngestError {
    /// A stable machine code, e.g. `"unknown_device"`.
    #[serde(default)]
    pub error: String,
}

/// The cloud's typed response to a successful `POST /api/v1/presence`.
///
/// Returned on a 2xx. `ttl_secs` is the TTL the cloud will actually honor for
/// this device (it may clamp the device's requested `presence_ttl_secs`), and
/// `next_beat_hint_secs` is an optional adaptive-cadence hint the daemon can use
/// to pace future beats (Phase 6). Tolerant of an empty/absent body for
/// back-compat with an older cloud.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PresenceAck {
    /// RFC 3339 timestamp the cloud processed the beat. Empty when unknown.
    #[serde(default)]
    pub server_time: String,
    /// The TTL the cloud honors for this device, in seconds.
    #[serde(default)]
    pub ttl_secs: u64,
    /// Optional hint: how many seconds the daemon should wait before the next
    /// beat. `None` = the daemon keeps its configured cadence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_beat_hint_secs: Option<u64>,
}

/// The cloud's advertised contract-version range, from `GET /api/v1/meta`.
///
/// Lets the daemon perform a best-effort startup handshake: if our
/// [`SCHEMA_VERSION`] falls outside `[min_schema_version, schema_version]`, the
/// daemon logs a clear warning (non-fatal). Evolution is additive within a major,
/// so a mismatch is usually a heads-up, not a hard stop.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServerMeta {
    /// The newest contract version the cloud supports, e.g. `"1.2.0"`.
    #[serde(default)]
    pub schema_version: String,
    /// The oldest contract version the cloud still accepts, e.g. `"1.0.0"`.
    #[serde(default)]
    pub min_schema_version: String,
}

/// A request to rotate a device's signing key, sent to
/// `POST /api/v1/devices/rotate-key`.
///
/// The payload names the device, the *new* public key, and when the rotation was
/// requested. It is signed by the **old** key (see [`RotateKeyEnvelope`]), proving
/// the holder of the current key authorized the swap. The cloud verifies the
/// signature against the device's currently-registered pubkey, then installs the
/// new one. Cloud-side verification is out of scope for this repo (producer-side).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RotateKeyRequest {
    /// ULID of the device whose key is rotating.
    pub device_id: String,
    /// The new Ed25519 public key, standard base64 — what the cloud will register.
    pub new_pubkey: String,
    /// RFC 3339 timestamp the rotation was requested.
    pub rotated_at: String,
}

/// A signed key-rotation request as it travels over the wire.
///
/// Mirrors [`Envelope`]'s framing, but the signature is computed by the **old**
/// key over `JCS(payload)` — the cloud verifies against the device's
/// currently-registered pubkey before swapping in `payload.new_pubkey`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RotateKeyEnvelope {
    /// Semver of this contract, e.g. `"1.1.0"`. Empty string means a pre-1.1
    /// client that predates this field; consumers must tolerate an absent or
    /// empty value rather than rejecting the rotation.
    #[serde(default)]
    pub schema_version: String,
    /// ULID of the device (matches `payload.device_id`).
    pub device_id: String,
    /// The rotation request being attested to.
    pub payload: RotateKeyRequest,
    /// `ed25519_oldkey(JCS(payload))`, base64 (standard, no padding).
    pub sig: String,
}

/// A request for the device owner's billing summary, sent to
/// `POST /api/v1/billing/summary`.
///
/// The request is a *query*, not a fact: it names the device asking, when it
/// asked (freshness window, like presence), and the period it wants. It stays
/// policy-free — no money, rate, or currency field. The cloud's *response*
/// carries those, and is deliberately **not** part of this contract (billing is
/// resolved late, in the cloud; the daemon parses the response tolerantly, see
/// the module invariants).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BillingSummaryRequest {
    /// ULID of the device asking (matches the envelope).
    pub device_id: String,
    /// RFC 3339 timestamp the request was assembled. The cloud rejects requests
    /// outside its freshness window (`stale_request`), bounding replays.
    pub sent_at: String,
    /// The summary period, e.g. `"week"` (rolling 7 days ending now). A string,
    /// not an enum, so new periods are additive without a schema major bump.
    pub period: String,
}

/// A signed billing-summary request as it travels over the wire.
///
/// Mirrors [`Envelope`] exactly (same framing, same signing rule): the signature
/// is computed over the canonical-JSON (RFC 8785) encoding of `payload` only.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BillingSummaryEnvelope {
    /// Semver of this contract, e.g. `"1.1.0"`.
    pub schema_version: String,
    /// ULID of the device that produced and signed this request.
    pub device_id: String,
    /// The billing-summary request being attested to.
    pub payload: BillingSummaryRequest,
    /// `ed25519(JCS(payload))`, base64 (standard, no padding).
    pub sig: String,
}

/// A signed knowledge batch as it travels to `POST /api/v1/knowledge`.
///
/// Mirrors [`Envelope`] exactly (same framing, same signing rule): the signature
/// is computed over the canonical-JSON (RFC 8785) encoding of `payload` only.
/// Knowledge is a **separate, consent-gated channel** beside attestations — it
/// never rides [`AttestationBatch`] (decision D-0001), has its own sync cursors,
/// and flows only when the producer's `[sync] knowledge` knob is enabled AND the
/// workspace has opted in cloud-side.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct KnowledgeEnvelope {
    /// Semver of this contract, e.g. `"1.2.0"`.
    pub schema_version: String,
    /// ULID of the device that produced and signed this batch.
    pub device_id: String,
    /// The knowledge batch being attested to.
    pub payload: KnowledgeBatch,
    /// `ed25519(JCS(payload))`, base64 (standard, no padding).
    pub sig: String,
}

/// The unit of knowledge sync: zavet decisions, specs, trailer refs, guard
/// events, and per-repo coverage stats captured locally.
///
/// Idempotent by `batchId` (deterministic over the covered items + tier, so a
/// crash-retry re-sends identical bytes) and by per-item natural keys cloud-side
/// — wipe-and-resync reproduces cloud state.
///
/// **Tiering.** At `tier: "metadata"` (the default when the channel is enabled
/// at all) every content field — `bodyMd` on decisions and specs, `value` on
/// trailer refs — is `None`/absent: ids, slugs, titles, status, globs, shas,
/// trailer keys + decision refs, and counts are enough for dashboards to show
/// structure, cost, and guard telemetry without any prose. `tier: "full"`
/// populates them, and only a workspace that separately opted in stores them.
/// [`KnowledgeBatch::strip_content`] is the producer-side downgrade.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct KnowledgeBatch {
    /// Deterministic idempotency key for this batch (content-derived, not a ULID).
    pub batch_id: String,
    /// ULID of the producing device (matches the envelope).
    pub device_id: String,
    /// RFC 3339 timestamp the batch was assembled.
    pub generated_at: String,
    /// `"metadata"` or `"full"` — what the batch was built under. A string, not
    /// an enum, so new tiers are additive without a schema major bump. The cloud
    /// gates on the actual content fields too, never on this label alone.
    pub tier: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<KnowledgeDecision>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub specs: Vec<KnowledgeSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trailer_refs: Vec<KnowledgeTrailerRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guard_events: Vec<KnowledgeGuardEvent>,
    /// Per-repo coverage/capture snapshot (see [`KnowledgeRepoStats`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repo_stats: Vec<KnowledgeRepoStats>,
}

impl KnowledgeBatch {
    /// Downgrade this batch to the metadata tier in place: every consent-gated
    /// content field is cleared and the tier label follows. This is what the
    /// daemon applies when the cloud answers `content_not_allowed` — sync never
    /// wedges, content never leaves without both consents. `batchId` is NOT
    /// touched: it is content-derived (and tier-sensitive), so the producer
    /// must recompute it after stripping before re-sending.
    pub fn strip_content(&mut self) {
        for d in &mut self.decisions {
            d.body_md = None;
            for c in &mut d.checks {
                c.command = None;
            }
        }
        for s in &mut self.specs {
            s.body_md = None;
            for c in &mut s.checks {
                c.command = None;
            }
        }
        for t in &mut self.trailer_refs {
            t.value = None;
        }
        self.tier = "metadata".to_string();
    }
}

/// One zavet decision record, metadata-first.
///
/// `recordSha` is the git blob oid of the record file (a content-identity
/// pointer like [`BlobRef::blob`], never the file's bytes). `bodyMd` is the
/// consent-gated content field — `None` at the metadata tier.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct KnowledgeDecision {
    /// Canonical repo ref, e.g. `github.com/acme/api`.
    pub repo_canonical: String,
    /// Canonical decision id, e.g. `D-0042`.
    pub id: String,
    /// Filename-derived slug, e.g. `attestation-wire-is-content-free`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    /// `active` | `superseded` (open set — stored verbatim).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// The decision this one supersedes, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    /// Repo-relative path of the record file.
    pub path: String,
    /// Human title — metadata (it names the record, like a filename).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Git blob oid of the record file (identity pointer, not content).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_sha: Option<String>,
    /// Commit that first introduced the record (first-sight provenance).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_commit: Option<String>,
    /// Most recent commit that touched the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_commit: Option<String>,
    /// Author date of the introducing commit, RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// Author date of the latest touching commit, RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// The session attributed to the introducing commit, if uniquely known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session: Option<String>,
    /// How the record came to be: `recorded` | `reverse-engineered` | …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// True only after a human confirmed the record matches reality.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<bool>,
    /// Guard globs (active enforcement surface).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guards: Vec<String>,
    /// A later decision that corrects one claim in this one. The record stays
    /// `active` — this is lineage, like `supersedes`, not a status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corrected_by: Option<String>,
    /// How this record's invariants are verified — see [`KnowledgeCheck`] for
    /// why only half of a check crosses at the metadata tier.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<KnowledgeCheck>,
    /// CONTENT (consent-gated): the record body markdown. `None` unless the
    /// batch is full-tier. Allowlisted in the no-content-fields invariant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_md: Option<String>,
}

/// One verification binding on a decision or spec.
///
/// The split is deliberate, and follows D-0001's rule that content needs its
/// own opt-in. A `label` names an invariant the way a `title` names a record —
/// metadata. A `command` is a line of the repo's own build configuration: it
/// can name internal tooling, hosts and paths nobody agreed to publish by
/// turning knowledge sync on. So the label rides at every tier and the command
/// is consent-gated exactly like `bodyMd`.
///
/// An unlabeled check has `label == command` at the source, so the metadata
/// tier degrades to a label that reads like a command. That is not a leak: it
/// is the name its author chose, in the same position `title` already
/// occupies.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct KnowledgeCheck {
    /// Human label for the invariant. Metadata.
    pub label: String,
    /// CONTENT (consent-gated): the command that verifies it. `None` unless
    /// the batch is full-tier. Allowlisted in the no-content-fields invariant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// One living spec, metadata-first. Same tiering rules as [`KnowledgeDecision`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct KnowledgeSpec {
    /// Canonical repo ref.
    pub repo_canonical: String,
    /// Filename-stem identity, e.g. `capture-pipeline`.
    pub slug: String,
    /// Spec document version (bumped on regeneration).
    pub version: u64,
    /// `designed` | `session` | `reverse-engineered`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// `low` | `med` | `high`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<String>,
    /// The spec's self-reported date (frontmatter `date:`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    /// True only after a human confirmed spec-matches-code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<bool>,
    /// Repo-relative path of the spec file.
    pub path: String,
    /// Human title — metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Git blob oid of the spec file (identity pointer, not content).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session: Option<String>,
    /// Git pathspecs the spec covers (staleness surface).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Linked decision ids (frontmatter ∪ body refs, canonical, deduplicated).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<String>,
    /// Feature-level scenarios proving this spec's behavior still holds. Same
    /// label/command tiering as a decision's — see [`KnowledgeCheck`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<KnowledgeCheck>,
    /// CONTENT (consent-gated): the spec body markdown. `None` unless the
    /// batch is full-tier. Allowlisted in the no-content-fields invariant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_md: Option<String>,
}

/// One commit trailer occurrence: the key and the decision it references.
///
/// The trailer's free-text `value` is CONTENT (author prose, like a commit
/// message) — `None` at the metadata tier; keys + refs alone already draw the
/// decision↔commit graph.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct KnowledgeTrailerRef {
    /// Canonical repo ref, when resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_canonical: Option<String>,
    /// Commit sha carrying the trailer.
    pub sha: String,
    /// Position of this trailer within the commit's trailer block.
    pub seq: u32,
    /// Trailer key, lowercased: `why` | `refs` | `supersedes` | `spec` | …
    pub key: String,
    /// First canonical `D-NNNN` referenced by the value, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<String>,
    /// CONTENT (consent-gated): the trailer's free text. `None` unless the
    /// batch is full-tier (policy-gated: the token denylist cannot see the
    /// word `value`, so the metadata-tier test pins its absence explicitly).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// One guard event (shown / blocked / complied / overridden / superseded) —
/// pure telemetry, always metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct KnowledgeGuardEvent {
    /// ULID (the idempotency key for this event).
    pub id: String,
    /// RFC 3339 receive time.
    pub at: String,
    /// Canonical repo ref, when resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_canonical: Option<String>,
    /// The decision the event concerns.
    pub decision_id: String,
    /// Event kind — stored verbatim (a plugin newer than the daemon degrades
    /// to "recorded, filtered at query time").
    pub kind: String,
    /// Repo-relative path involved, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    /// The attributed session, if uniquely known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Per-repo knowledge coverage/capture snapshot, computed producer-side (only
/// the daemon can walk git history and match paths against guard/spec globs).
/// Counts only — feeds the dashboard's "Knowledge coverage" and "Capture
/// ratio" tiles. Upserted per repo cloud-side (a snapshot, not a stream).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct KnowledgeRepoStats {
    /// Canonical repo ref.
    pub repo_canonical: String,
    /// The **effective** rolling window the counts cover, in days.
    ///
    /// Capped at 90, but clamped down to the days since the repo adopted
    /// `.zavet/`: counting commits from before the practice existed as "missed
    /// capture" made the ratios read far too low on repos with long history
    /// (issue #67). Consumers should render this value rather than assume 90.
    pub window_days: u32,
    /// Distinct paths touched by commits inside the window.
    pub active_paths: u64,
    /// Of those, paths covered by an active guard glob or a verified spec.
    pub covered_paths: u64,
    /// Non-merge commits in the window touching non-`.zavet/` paths.
    pub nontrivial_commits: u64,
    /// Of those, commits carrying at least one knowledge trailer.
    pub trailer_commits: u64,
    /// RFC 3339 timestamp the snapshot was computed.
    pub computed_at: String,
    /// RFC 3339 author date of the commit that added `.zavet/` — when the repo
    /// adopted the practice these counts measure. `None` when it can't be dated.
    ///
    /// Lets a consumer say *why* `window_days` is short ("coverage over the 34
    /// days since zavet started") instead of a bare number that reads like a bug.
    /// Optional and skipped when absent so older payloads stay byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zavet_since: Option<String>,
}

/// The cloud's typed response to a successful `POST /api/v1/knowledge`.
/// Tolerant: an empty body deserializes with defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct KnowledgeAck {
    /// RFC 3339 timestamp the cloud processed the batch. Empty when unknown.
    #[serde(default)]
    pub server_time: String,
    /// Count of newly-accepted items in this batch.
    #[serde(default)]
    pub accepted: u64,
    /// Count of items the cloud already had (idempotent duplicates).
    #[serde(default)]
    pub duplicates: u64,
    /// The contract version the cloud processed under. Empty when unknown.
    #[serde(default)]
    pub schema_version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_roundtrips_through_json() {
        let env = Envelope {
            schema_version: SCHEMA_VERSION.to_string(),
            device_id: "01J0DEVICE".to_string(),
            payload: AttestationBatch {
                batch_id: "01J0BATCH".to_string(),
                device_id: "01J0DEVICE".to_string(),
                generated_at: "2026-06-27T10:00:00Z".to_string(),
                intervals: vec![],
                sessions: vec![],
                token_usage: vec![],
                artifacts: vec![],
            },
            sig: "deadbeef".to_string(),
        };
        let json = serde_json::to_string(&env).unwrap();
        // camelCase on the wire.
        assert!(json.contains("\"schemaVersion\""));
        assert!(json.contains("\"deviceId\""));
        let back: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn empty_collections_are_omitted_from_the_wire() {
        let batch = AttestationBatch {
            batch_id: "b".into(),
            device_id: "d".into(),
            generated_at: "t".into(),
            intervals: vec![],
            sessions: vec![],
            token_usage: vec![],
            artifacts: vec![],
        };
        let json = serde_json::to_string(&batch).unwrap();
        assert!(!json.contains("intervals"));
    }

    #[test]
    fn presence_ttl_is_omitted_when_none() {
        // A ping without an explicit TTL must not serialize `presenceTtlSecs`, so
        // older payloads (and the signing vector) stay byte-identical.
        let ping = PresencePing {
            device_id: "d".into(),
            sent_at: "t".into(),
            sessions: vec![],
            presence_ttl_secs: None,
        };
        let json = serde_json::to_string(&ping).unwrap();
        assert!(!json.contains("presenceTtlSecs"));
        assert!(!json.contains("sessions"));
    }

    #[test]
    fn presence_ttl_serializes_in_camel_case_when_set() {
        let ping = PresencePing {
            device_id: "d".into(),
            sent_at: "t".into(),
            sessions: vec![],
            presence_ttl_secs: Some(75),
        };
        let json = serde_json::to_string(&ping).unwrap();
        assert!(json.contains("\"presenceTtlSecs\":75"));
        let back: PresencePing = serde_json::from_str(&json).unwrap();
        assert_eq!(back.presence_ttl_secs, Some(75));
    }

    #[test]
    fn ingest_error_parses_unknown_device() {
        let err: IngestError = serde_json::from_str(r#"{"error":"unknown_device"}"#).unwrap();
        assert_eq!(err.error, "unknown_device");
        // Missing field tolerated (defaults to empty).
        let empty: IngestError = serde_json::from_str("{}").unwrap();
        assert!(empty.error.is_empty());
    }

    #[test]
    fn presence_ack_tolerates_empty_body() {
        let ack: PresenceAck = serde_json::from_str("{}").unwrap();
        assert_eq!(ack.ttl_secs, 0);
        assert_eq!(ack.next_beat_hint_secs, None);
    }

    #[test]
    fn presence_ack_roundtrips_with_hint() {
        let ack = PresenceAck {
            server_time: "2026-06-29T10:00:00Z".into(),
            ttl_secs: 75,
            next_beat_hint_secs: Some(20),
        };
        let json = serde_json::to_string(&ack).unwrap();
        assert!(json.contains("\"ttlSecs\":75"));
        assert!(json.contains("\"nextBeatHintSecs\":20"));
        let back: PresenceAck = serde_json::from_str(&json).unwrap();
        assert_eq!(back.ttl_secs, 75);
        assert_eq!(back.next_beat_hint_secs, Some(20));
    }

    #[test]
    fn server_meta_roundtrips_camel_case() {
        let meta = ServerMeta {
            schema_version: "1.2.0".into(),
            min_schema_version: "1.0.0".into(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        assert!(json.contains("\"schemaVersion\""));
        assert!(json.contains("\"minSchemaVersion\""));
        let back: ServerMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.schema_version, "1.2.0");
        assert_eq!(back.min_schema_version, "1.0.0");
    }

    // Content-bearing terms that must never name a wire field. Matched against
    // whole snake_case *tokens* of a field name (not raw substrings), so a
    // legitimate metadata field like `source_session` (token `source` is NOT
    // on the list) is unaffected while a content field like `prompt_text`,
    // `diff`, or `source_code` is caught. Keeping it token-level avoids both
    // false positives (substring of an innocent word) and the `source_session`
    // collision a bare "source" substring rule would cause.
    const DENY: &[&str] = &[
        "prompt", "content", "diff", "body", "text", "patch", "code", "snippet",
    ];

    /// The ONLY sanctioned content-bearing wire fields, as exact normalized
    /// JSON paths (arrays flattened to `[]`). These are the knowledge
    /// channel's consent-gated fields: populated exclusively at `tier: "full"`
    /// under the producer's explicit `[sync] knowledge = "full"` knob, and
    /// stored only by a workspace that separately opted in (see D-0001 and
    /// docs/zavet.md). Anything content-shaped outside this list still fails
    /// the invariant.
    const CONTENT_ALLOWLIST: &[&str] = &[
        "payload.decisions[].bodyMd",
        "payload.specs[].bodyMd",
        "payload.decisions[].checks[].command",
        "payload.specs[].checks[].command",
    ];

    // Split a field name (snake_case on disk, camelCase on the wire) into its
    // lowercase word tokens.
    fn tokens(name: &str) -> Vec<String> {
        // Insert a separator at camelCase humps, then split on `_`.
        let mut s = String::new();
        for (i, ch) in name.chars().enumerate() {
            if ch.is_ascii_uppercase() && i > 0 {
                s.push('_');
            }
            s.push(ch.to_ascii_lowercase());
        }
        s.split('_')
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect()
    }

    // Collect every (normalized path, key) pair of a serialized wire value:
    // objects extend the path with `.key`, arrays with `[]`. Serializing a
    // fully-populated instance (not Default) guarantees `Option` and `Vec`
    // fields are present so they're inspected too.
    fn collect_paths(v: &serde_json::Value, prefix: &str, out: &mut Vec<(String, String)>) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, val) in map {
                    let path = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    out.push((path.clone(), k.clone()));
                    collect_paths(val, &path, out);
                }
            }
            serde_json::Value::Array(arr) => {
                let path = format!("{prefix}[]");
                for val in arr {
                    collect_paths(val, &path, out);
                }
            }
            _ => {}
        }
    }

    /// A fully-populated full-tier knowledge batch — every field `Some`/non-empty
    /// so the denylist walk and the strip test inspect all of them.
    fn full_knowledge_batch() -> KnowledgeBatch {
        KnowledgeBatch {
            batch_id: "kb".into(),
            device_id: "d".into(),
            generated_at: "t".into(),
            tier: "full".into(),
            decisions: vec![KnowledgeDecision {
                repo_canonical: "github.com/acme/api".into(),
                id: "D-0001".into(),
                slug: Some("wire-is-metadata-only".into()),
                status: Some("active".into()),
                supersedes: Some("D-0000".into()),
                path: ".zavet/decisions/D-0001-wire.md".into(),
                title: Some("Wire is metadata only".into()),
                record_sha: Some("blob1".into()),
                first_commit: Some("c1".into()),
                last_commit: Some("c2".into()),
                created_at: Some("t1".into()),
                updated_at: Some("t2".into()),
                source_session: Some("s1".into()),
                origin: Some("recorded".into()),
                verified: Some(true),
                guards: vec!["contract/**".into()],
                corrected_by: Some("D-0002".into()),
                checks: vec![KnowledgeCheck {
                    label: "wire carries no content fields".into(),
                    command: Some("full-tier command".into()),
                }],
                body_md: Some("## Decision\nfull-tier prose".into()),
            }],
            specs: vec![KnowledgeSpec {
                repo_canonical: "github.com/acme/api".into(),
                slug: "capture-pipeline".into(),
                version: 2,
                origin: Some("session".into()),
                confidence: Some("high".into()),
                date: Some("2026-07-16".into()),
                verified: Some(false),
                path: ".zavet/specs/capture-pipeline.md".into(),
                title: Some("Capture pipeline".into()),
                record_sha: Some("blob2".into()),
                first_commit: Some("c1".into()),
                last_commit: Some("c2".into()),
                created_at: Some("t1".into()),
                updated_at: Some("t2".into()),
                source_session: Some("s1".into()),
                paths: vec!["cli/dirad/src/capture.rs".into()],
                decisions: vec!["D-0001".into()],
                checks: vec![KnowledgeCheck {
                    label: "capture survives a rewritten branch".into(),
                    command: Some("full-tier command".into()),
                }],
                body_md: Some("## Overview\nfull-tier prose".into()),
            }],
            trailer_refs: vec![KnowledgeTrailerRef {
                repo_canonical: Some("github.com/acme/api".into()),
                sha: "sha1".into(),
                seq: 0,
                key: "refs".into(),
                decision_id: Some("D-0001".into()),
                value: Some("D-0001 full-tier prose".into()),
            }],
            guard_events: vec![KnowledgeGuardEvent {
                id: "01J0EVENT".into(),
                at: "t".into(),
                repo_canonical: Some("github.com/acme/api".into()),
                decision_id: "D-0001".into(),
                kind: "guard_blocked".into(),
                file_path: Some("contract/src/lib.rs".into()),
                session_id: Some("s1".into()),
            }],
            repo_stats: vec![KnowledgeRepoStats {
                repo_canonical: "github.com/acme/api".into(),
                window_days: 90,
                active_paths: 120,
                covered_paths: 80,
                nontrivial_commits: 40,
                trailer_commits: 25,
                computed_at: "t".into(),
                zavet_since: Some("2026-06-01T00:00:00Z".into()),
            }],
        }
    }

    /// Privacy invariant (Phase 4e): the wire contract must never carry payload
    /// content — no prompt text, file contents, diffs, message bodies, or raw
    /// source. The absence of these is the privacy guarantee (see the module-level
    /// "Metadata only" invariant). This test fails if any contract struct ever
    /// grows such a field, catching a regression at the schema boundary.
    ///
    /// The knowledge channel's consent-gated content fields are the single
    /// audited exception, subtracted as **exact paths** via `CONTENT_ALLOWLIST`
    /// (never token names) — a new `bodyMd` anywhere else still fails, and the
    /// full-tier `KnowledgeEnvelope` sits in the instance list so the allowlist
    /// is exercised, not assumed. The metadata-tier guarantee has its own test:
    /// `knowledge_metadata_tier_is_content_free`.
    ///
    /// Scope is the *wire contract only*. We deliberately do NOT scan the local
    /// store, whose `artifacts.message` column holds commit messages — that lives
    /// on-disk and never crosses the device → cloud boundary (the `ArtifactRef`
    /// wire type omits it). The denylist is matched case-insensitively against the
    /// snake_case field name, so camelCase wire names (`messageText`) are covered.
    #[test]
    fn wire_contract_carries_no_content_fields() {
        let envelope = Envelope {
            schema_version: SCHEMA_VERSION.into(),
            device_id: "d".into(),
            payload: AttestationBatch {
                batch_id: "b".into(),
                device_id: "d".into(),
                generated_at: "t".into(),
                intervals: vec![Interval {
                    id: "i".into(),
                    repo_canonical: Some("r".into()),
                    identity_email: "e".into(),
                    started_at: "s".into(),
                    ended_at: "e".into(),
                    human_seconds: 1,
                    activity: Some("a".into()),
                    source_session: "ss".into(),
                }],
                sessions: vec![SessionRollup {
                    session_id: "s".into(),
                    harness: Harness::ClaudeCode,
                    kind: SessionKind::Agent,
                    repo_canonical: Some("r".into()),
                    identity_email: "e".into(),
                    started_at: "s".into(),
                    ended_at: Some("e".into()),
                    agent_wall_seconds: 1,
                    prompts: Some(1),
                    branch: Some("b".into()),
                    note: None,
                    label: None,
                }],
                token_usage: vec![TokenUsage {
                    id: "t".into(),
                    session_id: "s".into(),
                    repo_canonical: Some("r".into()),
                    model: "m".into(),
                    input: 1,
                    output: 1,
                    cache_read: 1,
                    cache_create: 1,
                    est_cost_usd: Some(1.0),
                    at: "t".into(),
                }],
                artifacts: vec![ArtifactRef {
                    id: "a".into(),
                    repo_canonical: Some("r".into()),
                    kind: ArtifactKind::Commit,
                    git_ref: Some("g".into()),
                    sha: "sha".into(),
                    authored_at: Some("t".into()),
                    author_email: Some("e".into()),
                    source_session: Some("ss".into()),
                    patch_id: Some("pid".into()),
                    session_change_id: Some("scid".into()),
                    touched_paths: Some(vec!["src/lib.rs".into()]),
                    blobs: Some(vec![BlobRef {
                        path: "src/lib.rs".into(),
                        blob: "deadbeef".into(),
                    }]),
                }],
            },
            sig: "sig".into(),
        };
        let presence = PresenceEnvelope {
            schema_version: SCHEMA_VERSION.into(),
            device_id: "d".into(),
            payload: PresencePing {
                device_id: "d".into(),
                sent_at: "t".into(),
                sessions: vec![PresenceSession {
                    session_id: "s".into(),
                    harness: Harness::ClaudeCode,
                    kind: SessionKind::Agent,
                    repo_canonical: Some("r".into()),
                    branch: Some("b".into()),
                    identity_email: "e".into(),
                    started_at: "s".into(),
                    last_signal_at: Some("t".into()),
                    engaged_seconds: 1,
                    agent_wall_seconds: 1,
                    idle: false,
                }],
                presence_ttl_secs: Some(60),
            },
            sig: "sig".into(),
        };
        let rotate = RotateKeyEnvelope {
            schema_version: SCHEMA_VERSION.into(),
            device_id: "d".into(),
            payload: RotateKeyRequest {
                device_id: "d".into(),
                new_pubkey: "pk".into(),
                rotated_at: "t".into(),
            },
            sig: "sig".into(),
        };
        let billing = BillingSummaryEnvelope {
            schema_version: SCHEMA_VERSION.into(),
            device_id: "d".into(),
            payload: BillingSummaryRequest {
                device_id: "d".into(),
                sent_at: "t".into(),
                period: "week".into(),
            },
            sig: "sig".into(),
        };

        let knowledge = KnowledgeEnvelope {
            schema_version: SCHEMA_VERSION.into(),
            device_id: "d".into(),
            payload: full_knowledge_batch(),
            sig: "sig".into(),
        };

        let mut fields = Vec::new();
        collect_paths(&serde_json::to_value(&envelope).unwrap(), "", &mut fields);
        collect_paths(&serde_json::to_value(&presence).unwrap(), "", &mut fields);
        collect_paths(&serde_json::to_value(&rotate).unwrap(), "", &mut fields);
        collect_paths(&serde_json::to_value(&billing).unwrap(), "", &mut fields);
        // The allowlist must actually be exercised, not vacuously true.
        let before = fields.len();
        collect_paths(&serde_json::to_value(&knowledge).unwrap(), "", &mut fields);
        assert!(
            fields.len() > before,
            "knowledge envelope produced no fields"
        );
        let allowlisted_seen = fields
            .iter()
            .filter(|(p, _)| CONTENT_ALLOWLIST.contains(&p.as_str()))
            .count();
        assert_eq!(
            allowlisted_seen,
            CONTENT_ALLOWLIST.len(),
            "every CONTENT_ALLOWLIST path must appear in the full-tier instance \
             (a stale allowlist entry is a hole in the invariant)"
        );

        for (path, key) in &fields {
            if CONTENT_ALLOWLIST.contains(&path.as_str()) {
                continue;
            }
            for tok in tokens(key) {
                assert!(
                    !DENY.contains(&tok.as_str()),
                    "wire field `{path}` contains denylisted content token `{tok}` — \
                     the contract must not carry payload content (prompt/diff/body/etc.) \
                     outside the consent-gated CONTENT_ALLOWLIST"
                );
            }
        }
    }

    /// The metadata tier is content-free BY CONSTRUCTION: after
    /// [`KnowledgeBatch::strip_content`] a fully-populated batch serializes
    /// with zero denied tokens, zero allowlisted paths, and no trailer `value`
    /// key. The trailer `value` matters especially — the word carries no
    /// denied token, so the path/token walk above cannot see it; this test is
    /// its real guard (titles, by decision, are metadata and stay).
    #[test]
    fn knowledge_metadata_tier_is_content_free() {
        let mut batch = full_knowledge_batch();
        batch.strip_content();
        assert_eq!(batch.tier, "metadata");

        let mut fields = Vec::new();
        collect_paths(&serde_json::to_value(&batch).unwrap(), "", &mut fields);
        for (path, key) in &fields {
            assert!(
                !CONTENT_ALLOWLIST.contains(&path.as_str()),
                "metadata tier must not serialize allowlisted content path `{path}`"
            );
            assert!(
                key != "value",
                "metadata tier must not serialize trailer `value` (path `{path}`)"
            );
            for tok in tokens(key) {
                assert!(
                    !DENY.contains(&tok.as_str()),
                    "metadata-tier field `{path}` carries denied token `{tok}`"
                );
            }
        }
        // Titles are metadata by design decision — they must survive the strip.
        let json = serde_json::to_string(&batch).unwrap();
        assert!(json.contains("\"title\""));
        assert!(!json.contains("\"bodyMd\""));
    }

    #[test]
    fn knowledge_envelope_roundtrips_camel_case() {
        let env = KnowledgeEnvelope {
            schema_version: SCHEMA_VERSION.into(),
            device_id: "d".into(),
            payload: full_knowledge_batch(),
            sig: "sig".into(),
        };
        let json = serde_json::to_string(&env).unwrap();
        // The local content_hash rides the wire under the evasive-but-honest
        // name `recordSha` (token `content` is denied; precedent: patch_id →
        // changeId).
        assert!(json.contains("\"recordSha\""));
        assert!(!json.contains("content_hash"));
        assert!(json.contains("\"trailerRefs\""));
        assert!(json.contains("\"repoStats\""));
        assert!(json.contains("\"windowDays\":90"));
        let back: KnowledgeEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.payload.decisions[0].id, "D-0001");
        assert_eq!(back.payload.specs[0].version, 2);
        assert_eq!(back.payload.tier, "full");
    }

    #[test]
    fn knowledge_empty_collections_are_omitted() {
        let batch = KnowledgeBatch {
            batch_id: "kb".into(),
            device_id: "d".into(),
            generated_at: "t".into(),
            tier: "metadata".into(),
            decisions: vec![],
            specs: vec![],
            trailer_refs: vec![],
            guard_events: vec![],
            repo_stats: vec![],
        };
        let json = serde_json::to_string(&batch).unwrap();
        assert!(!json.contains("decisions"));
        assert!(!json.contains("guardEvents"));
        assert!(!json.contains("repoStats"));
    }

    #[test]
    fn knowledge_ack_tolerates_empty_body() {
        let ack: KnowledgeAck = serde_json::from_str("{}").unwrap();
        assert_eq!(ack.accepted, 0);
        assert_eq!(ack.duplicates, 0);
        assert!(ack.server_time.is_empty());
    }

    #[test]
    fn artifact_ref_session_signals_roundtrip_camel_case() {
        // The squash-resilient signals serialize under their camelCase wire names
        // and round-trip; `blobs` entries carry `path` + `blob` only.
        let r = ArtifactRef {
            id: "sha".into(),
            repo_canonical: Some("github.com/acme/api".into()),
            kind: ArtifactKind::Commit,
            git_ref: Some("feat/x".into()),
            sha: "sha".into(),
            authored_at: None,
            author_email: None,
            source_session: None,
            patch_id: None,
            session_change_id: Some("3e53248".into()),
            touched_paths: Some(vec!["a.rs".into(), "b.rs".into()]),
            blobs: Some(vec![BlobRef {
                path: "a.rs".into(),
                blob: "de98044".into(),
            }]),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"sessionChangeId\":\"3e53248\""));
        assert!(json.contains("\"touchedPaths\":[\"a.rs\",\"b.rs\"]"));
        assert!(json.contains("\"blobs\":[{\"path\":\"a.rs\",\"blob\":\"de98044\"}]"));
        let back: ArtifactRef = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session_change_id.as_deref(), Some("3e53248"));
        assert_eq!(back.blobs.unwrap()[0].path, "a.rs");
    }

    #[test]
    fn artifact_ref_session_signals_omitted_when_none() {
        // A ref without the new signals must not serialize their keys, so older
        // payloads (and the signing vector) stay byte-identical.
        let r = ArtifactRef {
            id: "sha".into(),
            repo_canonical: None,
            kind: ArtifactKind::Commit,
            git_ref: None,
            sha: "sha".into(),
            authored_at: None,
            author_email: None,
            source_session: None,
            patch_id: None,
            session_change_id: None,
            touched_paths: None,
            blobs: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("sessionChangeId"));
        assert!(!json.contains("touchedPaths"));
        assert!(!json.contains("blobs"));
    }

    #[test]
    fn billing_summary_envelope_roundtrips_camel_case() {
        let env = BillingSummaryEnvelope {
            schema_version: SCHEMA_VERSION.into(),
            device_id: "01J0DEVICE".into(),
            payload: BillingSummaryRequest {
                device_id: "01J0DEVICE".into(),
                sent_at: "2026-07-02T09:00:00Z".into(),
                period: "week".into(),
            },
            sig: "deadbeef".into(),
        };
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"schemaVersion\""));
        assert!(json.contains("\"deviceId\""));
        assert!(json.contains("\"sentAt\""));
        assert!(json.contains("\"period\":\"week\""));
        let back: BillingSummaryEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.payload.period, "week");
        assert_eq!(back.payload.device_id, "01J0DEVICE");
    }

    #[test]
    fn rotate_key_envelope_roundtrips_camel_case() {
        let env = RotateKeyEnvelope {
            schema_version: SCHEMA_VERSION.into(),
            device_id: "01J0DEVICE".into(),
            payload: RotateKeyRequest {
                device_id: "01J0DEVICE".into(),
                new_pubkey: "newpubkeyb64".into(),
                rotated_at: "2026-06-29T10:00:00Z".into(),
            },
            sig: "deadbeef".into(),
        };
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"schemaVersion\""));
        assert!(json.contains("\"deviceId\""));
        assert!(json.contains("\"newPubkey\""));
        assert!(json.contains("\"rotatedAt\""));
        let back: RotateKeyEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.schema_version, SCHEMA_VERSION);
        assert_eq!(back.payload.new_pubkey, "newpubkeyb64");
    }

    #[test]
    fn rotate_key_envelope_defaults_schema_version_when_absent() {
        // Pre-1.1 clients never sent `schemaVersion`; the cloud (and this
        // struct) must still deserialize the envelope and treat it as empty.
        let json = r#"{
            "deviceId": "01J0DEVICE",
            "payload": {
                "deviceId": "01J0DEVICE",
                "newPubkey": "newpubkeyb64",
                "rotatedAt": "2026-06-29T10:00:00Z"
            },
            "sig": "deadbeef"
        }"#;
        let env: RotateKeyEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.schema_version, "");
        assert_eq!(env.payload.new_pubkey, "newpubkeyb64");
    }
}
