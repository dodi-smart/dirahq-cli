//! Zavet knowledge module — the optional layer that records what the tracked
//! time *produced*: guard events from the zavet plugin, plus decision records
//! and commit trailers captured during the ordinary git poll.
//!
//! Everything here is off the accounting hot path: activation is decided per
//! repo, and an inactive repo costs one `meta` lookup plus one directory stat
//! inside the already-budgeted capture walk.

use crate::control::lock_recover;
use crate::state::AppState;
use dira_core::config::ZavetMode;
use dira_core::protocol::{
    Response, ZavetCheckView, ZavetDecisionView, ZavetDecisionsView, ZavetGuardStatView,
    ZavetPresence, ZavetSpecView, ZavetSpecWhyView, ZavetStatusView, ZavetSyncView,
    ZavetUncapturedView, ZavetWhyView,
};
use dira_core::store::{ZavetDecisionRow, ZavetSpecRow};
pub use dira_core::zavet::{
    canonical_decision_id, parse_config, parse_guard_event, GuardEventV1, ZavetConfig, CONFIG_PATH,
    ZAVET_DIR,
};
use std::path::{Path, PathBuf};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Whether zavet is active for a repo.
///
/// Precedence: the per-repo override (`dira zavet enable|disable`, stored in
/// `meta`) beats the global `modules.zavet` knob; in `auto` the knob defers to
/// whether the repo carries a `.zavet/` directory at its toplevel.
pub fn effective_mode(knob: ZavetMode, override_: Option<bool>, zavet_dir_exists: bool) -> bool {
    match (override_, knob) {
        (Some(forced), _) => forced,
        (None, ZavetMode::On) => true,
        (None, ZavetMode::Off) => false,
        (None, ZavetMode::Auto) => zavet_dir_exists,
    }
}

/// The `auto` probe: does the repo toplevel carry `.zavet/`?
pub fn zavet_dir_exists(repo_root: &Path) -> bool {
    repo_root.join(ZAVET_DIR).is_dir()
}

/// The repo's decision-id conventions, or the pre-prefix defaults (`D`, width
/// 4) when there is no readable config — which is every repo scaffolded before
/// prefixes existed, and is why they need no migration.
pub fn read_config(repo_root: &Path) -> ZavetConfig {
    std::fs::read_to_string(repo_root.join(CONFIG_PATH))
        .map(|t| parse_config(&t))
        .unwrap_or_default()
}

/// What one git probe of the working tree tells us about a repo's records.
///
/// Produced by [`working_tree_probe`] — one `spawn_blocking`, one resolved
/// toplevel, so every field describes the SAME tree. Splitting these across
/// tasks let a checkout land between them and produce a view that mixed two
/// branches, which is precisely what DIRASH-0026 forbids.
#[derive(Default)]
pub struct TreeProbe {
    /// The checked-out branch; `None` on a detached HEAD or with no workdir.
    pub branch: Option<String>,
    /// Record paths present in `HEAD`'s tree. `None` means *unknown* — no
    /// workdir, no HEAD, or git failed — and callers must render nothing
    /// rather than guessing.
    pub in_head: Option<std::collections::HashSet<String>>,
    /// Records on disk the store has never captured.
    pub uncaptured: Vec<ZavetUncapturedView>,
}

/// Which record kinds a probe should look for on disk.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kinds {
    Decisions,
    Both,
}

/// Record files in the working tree that the store has no row for.
///
/// Files are matched against the store by id/slug, not by path, so a record
/// renamed since capture is not reported as a new one. A file whose path is
/// already known is skipped before it is read — in the steady state that is
/// every record, which keeps this to a directory listing rather than N parses.
///
/// Filenames are classified with the same [`dira_core::zavet::is_decision_path`]
/// / [`is_spec_path`] predicates the capture path uses, so "what counts as a
/// record" cannot drift between capture and this report. A file that matches
/// but does not parse still comes back, with `None` for the id — "there is a
/// file here dira cannot read" is exactly the state worth surfacing.
///
/// Read-only: DIRASH-0024 gates repo-scope *writes* on a cwd check.
fn scan_uncaptured(
    root: &Path,
    cfg: &ZavetConfig,
    kinds: Kinds,
    captured_keys: &std::collections::HashSet<String>,
    captured_paths: &std::collections::HashSet<String>,
    in_head: &std::collections::HashSet<String>,
) -> Vec<ZavetUncapturedView> {
    let mut out: Vec<ZavetUncapturedView> = Vec::new();
    let dirs: &[&str] = match kinds {
        Kinds::Decisions => &[dira_core::zavet::DECISIONS_DIR],
        Kinds::Both => &[dira_core::zavet::DECISIONS_DIR, dira_core::zavet::SPECS_DIR],
    };
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let rel = format!("{dir}{}", entry.file_name().to_string_lossy());
            let is_spec = dira_core::zavet::is_spec_path(&rel);
            if !is_spec && !dira_core::zavet::is_decision_path(&rel) {
                continue;
            }
            // Already captured under this exact path: nothing to read.
            if captured_paths.contains(&rel) {
                continue;
            }
            let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
            let (id, title) = if is_spec {
                match dira_core::zavet::parse_spec(&text, &rel, cfg) {
                    Some(s) => (Some(s.slug), s.title),
                    None => (None, None),
                }
            } else {
                match dira_core::zavet::parse_decision(&text, &rel, cfg) {
                    Some(d) => (Some(d.id), d.title),
                    None => (None, None),
                }
            };
            if id.as_deref().is_some_and(|i| captured_keys.contains(i)) {
                continue;
            }
            out.push(ZavetUncapturedView {
                kind: if is_spec { "spec" } else { "decision" }.to_string(),
                // Committed but absent from the store means the daemon has not
                // walked that commit yet; absent from HEAD means it was never
                // committed. Different remedies, so they stay distinct.
                reason: if in_head.contains(&rel) {
                    "awaiting sweep"
                } else {
                    "uncommitted"
                }
                .to_string(),
                id,
                title,
                path: rel,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Everything the list views need to know about the working tree, in one
/// blocking task: the branch, which record paths `HEAD` carries, and which
/// records on disk were never captured.
///
/// `root` arrives already resolved by [`repo_root`]: `toplevel()` shells out to
/// git, and the probes used to pay for it once each.
async fn working_tree_probe(
    root: Option<PathBuf>,
    cfg: ZavetConfig,
    kinds: Kinds,
    captured: Vec<(String, String)>,
) -> TreeProbe {
    let Some(root) = root else {
        return TreeProbe::default();
    };
    tokio::task::spawn_blocking(move || {
        let branch = dira_core::project::current_branch(&root);
        // `None` = no HEAD (an unborn branch) or not a repo. That makes
        // *presence* unknown — never guessed — but it does NOT make the
        // on-disk scan unknown: the files are still right there. Bailing out
        // here would report zero uncaptured records in a freshly-`git init`ed
        // repo, which is the very case this feature exists to catch (the first
        // decision of a project is written before the first commit).
        //
        // With no HEAD, nothing is committed, so an empty set is the truthful
        // input and every record correctly reports `uncommitted`.
        let in_head = dira_core::project::ls_tree_paths(
            &root,
            &[dira_core::zavet::DECISIONS_DIR, dira_core::zavet::SPECS_DIR],
        );
        let keys = captured.iter().map(|(_, k)| k.clone()).collect();
        let paths = captured.iter().map(|(p, _)| p.clone()).collect();
        let uncaptured = scan_uncaptured(
            &root,
            &cfg,
            kinds,
            &keys,
            &paths,
            in_head
                .as_ref()
                .unwrap_or(&std::collections::HashSet::new()),
        );
        TreeProbe {
            branch,
            in_head,
            uncaptured,
        }
    })
    .await
    .unwrap_or_default()
}

/// Resolve the repo toplevel once, off the async path.
///
/// Every git probe in a query needs it, and each used to shell out for its own
/// copy. Resolving here also guarantees they all describe the same tree.
async fn repo_root(workdir: Option<PathBuf>) -> Option<PathBuf> {
    let dir = workdir?;
    tokio::task::spawn_blocking(move || dira_core::project::toplevel(&dir).unwrap_or(dir))
        .await
        .ok()
}

/// Presence for one record path, or `None` when the probe could not look.
fn presence_of(
    in_head: Option<&std::collections::HashSet<String>>,
    path: &str,
) -> Option<ZavetPresence> {
    in_head.map(|h| {
        if h.contains(path) {
            ZavetPresence::OnBranch
        } else {
            ZavetPresence::OffBranch
        }
    })
}

/// Attach each decision's guard-event tallies from one batched query.
async fn attach_guard_stats(state: &AppState, repo: &str, decisions: &mut [ZavetDecisionView]) {
    let mut stats = state
        .store
        .zavet_guard_event_stats_by_decision(repo)
        .await
        .unwrap_or_default();
    for d in decisions {
        if let Some(s) = stats.remove(&d.id) {
            d.guard_stats = guard_stat_views(s);
        }
    }
}

/// Resolve a payload cwd to `(canonical repo, .zavet/ exists, id config)` off
/// Resolve a payload cwd to `(canonical repo, .zavet/ exists, id config)` off
/// the async path — `project::resolve` shells out to git.
async fn resolve_repo(cwd: String) -> (Option<String>, bool, ZavetConfig) {
    tokio::task::spawn_blocking(move || {
        let dir = PathBuf::from(cwd);
        let top = dira_core::project::toplevel(&dir);
        let repo = dira_core::project::resolve(&dir).project;
        let dir_exists = top.as_deref().map(zavet_dir_exists).unwrap_or(false);
        let cfg = top.as_deref().map(read_config).unwrap_or_default();
        (repo, dir_exists, cfg)
    })
    .await
    .unwrap_or((None, false, ZavetConfig::default()))
}

/// Whether zavet is active for `repo`, per the standard precedence.
pub async fn active_for(state: &AppState, repo: &str, dir_exists: bool) -> bool {
    let override_ = state.store.zavet_override_get(repo).await.unwrap_or(None);
    effective_mode(state.config.modules.zavet, override_, dir_exists)
}

/// `Request::IngestZavet` — store a guard event, attributed to the unique
/// active session for its repo (or NULL). Deliberately writes the store
/// directly: guard events are low-volume control traffic and must never touch
/// the writer channel / accounting hot path.
pub async fn ingest(state: &AppState, payload: serde_json::Value) -> Response {
    let Some(mut ev) = parse_guard_event(&payload) else {
        tracing::debug!("zavet: dropped malformed guard event");
        return Response::Ok; // the shim is fire-and-forget; nothing to say
    };
    let (repo, dir_exists, cfg) = resolve_repo(ev.cwd.clone()).await;
    // The id arrives with its prefix normalized but its digits as written —
    // parse_guard_event runs before `cwd` is resolved, so this is the first
    // point that knows the repo's padding width. Without it a hand-authored
    // `id: D-7` would key differently from the `D-0007` its record captures as.
    if let Some(id) = canonical_decision_id(&ev.decision_id, cfg.id_width) {
        ev.decision_id = id;
    }
    let Some(repo) = repo else {
        tracing::debug!(cwd = %ev.cwd, "zavet: guard event outside a resolvable repo");
        return Response::Ok;
    };
    if !active_for(state, &repo, dir_exists).await {
        tracing::debug!(repo, "zavet: guard event for inactive repo dropped");
        return Response::Ok;
    }
    let session = lock_recover(&state.sessions).session_for_repo(&repo);
    let at = ev.ts.clone().unwrap_or_else(|| {
        OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_default()
    });
    match state
        .store
        .zavet_record_guard_event(
            &at,
            Some(&repo),
            &ev.decision_id,
            &ev.kind,
            ev.file_path.as_deref(),
            session.as_deref(),
        )
        .await
    {
        Ok(_) => {
            // Fresh telemetry for the knowledge channel — lossy nudge, the
            // backstop covers a miss.
            let _ = state.knowledge_sync.trigger.try_send(());
            Response::Ok
        }
        Err(e) => Response::Error {
            message: format!("zavet ingest failed: {e}"),
        },
    }
}

/// Repo resolution ladder for the query commands: explicit repo wins, else
/// resolve from `cwd` (or the daemon's own cwd). Also reports whether the
/// resolved toplevel carries `.zavet/` when a directory was available.
async fn query_repo(
    state: &AppState,
    repo: Option<String>,
    cwd: Option<String>,
) -> Result<(String, Option<bool>, ZavetConfig), Response> {
    // An explicit --project names a repo THIS PROCESS is not standing in — but
    // the daemon may well remember a directory for it, and that repo's id
    // conventions (`id-width`, the prefix set) still govern how a query id
    // canonicalizes. Falling straight back to the defaults silently re-pads
    // `CLOUD-42` to a width-4 `CLOUD-0042` for a repo that stores
    // `CLOUD-00042`, so the lookup misses a record that is right there.
    //
    // `dir_exists` stays `None`: it answers the `auto` probe for the tree this
    // process is standing in, and a named project is not that tree.
    if let Some(r) = repo {
        let cfg = match repo_root(known_dir(state, &r)).await {
            Some(root) => tokio::task::spawn_blocking(move || read_config(&root))
                .await
                .unwrap_or_default(),
            None => ZavetConfig::default(),
        };
        return Ok((r, None, cfg));
    }
    let dir = cwd.unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_string())
    });
    let (repo, dir_exists, cfg) = resolve_repo(dir).await;
    match repo {
        Some(r) => Ok((r, Some(dir_exists), cfg)),
        None => Err(Response::Error {
            message: "not inside a repo with a recognizable remote; pass --project".into(),
        }),
    }
}

fn spec_view(s: &ZavetSpecRow, stale_commits: Option<u64>) -> ZavetSpecView {
    ZavetSpecView {
        slug: s.slug.clone(),
        title: s.title.clone(),
        version: s.version,
        origin: s.origin.clone(),
        verified: s.verified,
        confidence: s.confidence.clone(),
        date: s.date.clone(),
        path: s.path.clone(),
        paths: s.paths.clone(),
        decisions: s.decisions.clone(),
        checks: s.checks.iter().map(check_view).collect(),
        first_commit: s.first_commit.clone(),
        last_commit: s.last_commit.clone(),
        created_at: s.created_at.clone(),
        source_session: s.source_session.clone(),
        stale_commits,
        presence: None,
    }
}

/// A working directory to ask git about `repo` in: the dir the daemon last
/// observed for it, else the caller's `cwd`. `None` means staleness stays
/// unknown — never guessed.
fn repo_workdir(state: &AppState, repo: &str, cwd: Option<&str>) -> Option<PathBuf> {
    known_dir(state, repo).or_else(|| cwd.map(PathBuf::from))
}

/// The directory the daemon last observed for `repo`, if any.
///
/// Note what it holds: whatever the last writer put there — a cwd an event
/// happened to carry (often a subdirectory), or the toplevel `zavet sync`
/// resolved. Readers that need the root resolve it (see [`repo_root`]).
fn known_dir(state: &AppState, repo: &str) -> Option<PathBuf> {
    crate::control::lock_recover_map(&state.repo_dirs)
        .get(repo)
        .map(PathBuf::from)
}

/// A spec's `(last_commit, paths)` — what [`spec_staleness`] needs to know.
fn staleness_input(s: &ZavetSpecRow) -> (Option<String>, Vec<String>) {
    (s.last_commit.clone(), s.paths.clone())
}

/// Compute `stale_commits` for each `(last_commit, paths)` input: commits
/// touching the paths after `last_commit`, via git in `workdir` — one
/// `spawn_blocking` for the whole batch. Specs without a `last_commit` or
/// paths stay `None`/`Some(0)` as appropriate; no workdir means every spec
/// reports `None` (unknown).
async fn spec_staleness(
    root: Option<PathBuf>,
    inputs: Vec<(Option<String>, Vec<String>)>,
) -> Vec<Option<u64>> {
    let n = inputs.len();
    let Some(root) = root else {
        return vec![None; n];
    };
    tokio::task::spawn_blocking(move || {
        inputs
            .iter()
            .map(|(last, paths)| {
                let last = last.as_deref()?;
                if paths.is_empty() {
                    return Some(0);
                }
                Some(dira_core::project::commits_touching_since(&root, last, paths).len() as u64)
            })
            .collect::<Vec<Option<u64>>>()
    })
    .await
    .unwrap_or_else(|_| vec![None; n])
}

fn check_view(c: &dira_core::store::ZavetCheck) -> ZavetCheckView {
    ZavetCheckView {
        label: c.label.clone(),
        command: c.command.clone(),
    }
}

fn decision_view(d: &ZavetDecisionRow) -> ZavetDecisionView {
    ZavetDecisionView {
        id: d.id.clone(),
        title: d.title.clone(),
        status: d.status.clone(),
        path: d.path.clone(),
        guards: d.guards.clone(),
        supersedes: d.supersedes.clone(),
        first_commit: d.first_commit.clone(),
        created_at: d.created_at.clone(),
        source_session: d.source_session.clone(),
        origin: d.origin.clone(),
        verified: d.verified,
        corrected_by: d.corrected_by.clone(),
        checks: d.checks.iter().map(check_view).collect(),
        // Both are batch-computed by the callers that have a repo to ask —
        // `why` reports them its own way and leaves these empty.
        presence: None,
        guard_stats: Vec::new(),
    }
}

/// Ranked results for a free-text query: decisions (their attached trailers
/// boost them), living specs (ranked with the same weights — their linked
/// decision ids and path globs are searchable), plus orphan commit trailers —
/// micro-decisions with no record, which can be the entire answer for a
/// question like "why is the timeout 10s".
struct ZavetSearchResults {
    decisions: Vec<(ZavetDecisionRow, u32)>,
    specs: Vec<(ZavetSpecRow, u32)>,
    trailers: Vec<dira_core::protocol::ZavetTrailerHit>,
}

impl ZavetSearchResults {
    fn is_empty(&self) -> bool {
        self.decisions.is_empty() && self.specs.is_empty() && self.trailers.is_empty()
    }
}

async fn search(state: &AppState, repo: &str, query: &str) -> ZavetSearchResults {
    let terms = dira_core::zavet::tokenize_query(query);
    if terms.is_empty() {
        return ZavetSearchResults {
            decisions: Vec::new(),
            specs: Vec::new(),
            trailers: Vec::new(),
        };
    }
    let decisions = state
        .store
        .zavet_decisions_list(repo)
        .await
        .unwrap_or_default();
    let specs = state.store.zavet_specs_list(repo).await.unwrap_or_default();
    let all_trailers = state
        .store
        .zavet_all_trailers(repo)
        .await
        .unwrap_or_default();
    let mut hits: Vec<(ZavetDecisionRow, u32)> = decisions
        .into_iter()
        .filter_map(|d| {
            let doc = dira_core::zavet::SearchDoc {
                id: &d.id,
                title: d.title.as_deref(),
                slug: d.slug.as_deref(),
                body: d.body_md.as_deref(),
                guards: &d.guards,
                trailers: all_trailers
                    .iter()
                    .filter(|(_, _, _, id)| id.as_deref() == Some(d.id.as_str()))
                    .map(|(_, _, v, _)| v.as_str())
                    .collect(),
            };
            let score = dira_core::zavet::score(&doc, &terms);
            (score > 0).then_some((d, score))
        })
        .collect();
    hits.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.id.cmp(&b.0.id)));

    // Specs rank with the same weights: the slug fills the slug slot, path
    // globs fill the guards slot, and the linked decision ids join the
    // searchable trailer text (so "D-0001" finds the specs covering it).
    let mut spec_hits: Vec<(ZavetSpecRow, u32)> = specs
        .into_iter()
        .filter_map(|s| {
            let linked = s.decisions.join(" ");
            let doc = dira_core::zavet::SearchDoc {
                id: &s.slug,
                title: s.title.as_deref(),
                slug: Some(&s.slug),
                body: s.body_md.as_deref(),
                guards: &s.paths,
                trailers: vec![linked.as_str()],
            };
            let score = dira_core::zavet::score(&doc, &terms);
            (score > 0).then_some((s, score))
        })
        .collect();
    spec_hits.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.slug.cmp(&b.0.slug)));

    // Orphan trailers (no decision ref) rank as hits of their own — a match
    // on an attached trailer already surfaces via its decision above.
    let mut trailers: Vec<dira_core::protocol::ZavetTrailerHit> = all_trailers
        .into_iter()
        .filter(|(_, _, _, id)| id.is_none())
        .filter_map(|(sha, key, value, _)| {
            let score = dira_core::zavet::score_trailer(&key, &value, &terms);
            (score > 0).then_some(dira_core::protocol::ZavetTrailerHit {
                sha,
                key,
                value,
                score,
            })
        })
        .collect();
    trailers.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.sha.cmp(&b.sha)));
    trailers.truncate(5);

    ZavetSearchResults {
        decisions: hits,
        specs: spec_hits,
        trailers,
    }
}

fn search_hit(d: &ZavetDecisionRow, score: u32) -> dira_core::protocol::ZavetSearchHit {
    dira_core::protocol::ZavetSearchHit {
        id: d.id.clone(),
        title: d.title.clone(),
        status: d.status.clone(),
        verified: d.verified,
        excerpt: d.body_md.as_deref().and_then(dira_core::zavet::excerpt),
        score,
    }
}

fn spec_search_hit(s: &ZavetSpecRow, score: u32) -> dira_core::protocol::ZavetSpecHit {
    dira_core::protocol::ZavetSpecHit {
        slug: s.slug.clone(),
        title: s.title.clone(),
        origin: s.origin.clone(),
        confidence: s.confidence.clone(),
        verified: s.verified,
        excerpt: s.body_md.as_deref().and_then(dira_core::zavet::excerpt),
        score,
    }
}

fn guard_stat_views(stats: Vec<dira_core::store::ZavetGuardStat>) -> Vec<ZavetGuardStatView> {
    stats
        .into_iter()
        .map(|s| ZavetGuardStatView {
            kind: s.kind,
            total: s.total,
            unattributed: s.unattributed,
        })
        .collect()
}

/// `Request::ZavetStatus`.
pub async fn status(state: &AppState, cwd: Option<String>, repo: Option<String>) -> Response {
    let (repo, dir_exists, _) = match query_repo(state, repo, cwd).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let override_ = state.store.zavet_override_get(&repo).await.unwrap_or(None);
    let active = effective_mode(
        state.config.modules.zavet,
        override_,
        dir_exists.unwrap_or(false),
    );
    let counts = state.store.zavet_counts(&repo).await.unwrap_or_default();
    let stats = state
        .store
        .zavet_guard_event_stats(&repo, None)
        .await
        .unwrap_or_default();
    Response::ZavetStatus(Box::new(ZavetStatusView {
        repo,
        active,
        knob: state.config.modules.zavet.as_str().to_string(),
        override_mode: override_.map(|v| if v { "on" } else { "off" }.to_string()),
        zavet_dir: dir_exists,
        decisions_total: counts.decisions_total,
        decisions_active: counts.decisions_active,
        trailers: counts.trailers,
        guard_events: counts.guard_events,
        guard_stats: guard_stat_views(stats),
    }))
}

/// `Request::ZavetDecisions`.
pub async fn decisions(state: &AppState, cwd: Option<String>, repo: Option<String>) -> Response {
    let (repo, _, cfg) = match query_repo(state, repo, cwd.clone()).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let rows = match state.store.zavet_decisions_list(&repo).await {
        Ok(rows) => rows,
        Err(e) => {
            return Response::Error {
                message: format!("zavet decisions failed: {e}"),
            }
        }
    };
    let mut decisions: Vec<ZavetDecisionView> = rows.iter().map(decision_view).collect();
    attach_guard_stats(state, &repo, &mut decisions).await;
    let probe = working_tree_probe(
        repo_root(repo_workdir(state, &repo, cwd.as_deref())).await,
        cfg,
        // A spec on disk is not a decision — this view reports only its kind,
        // and scanning `.zavet/specs/` here would parse every spec to discard it.
        Kinds::Decisions,
        decisions
            .iter()
            .map(|d| (d.path.clone(), d.id.clone()))
            .collect(),
    )
    .await;
    for d in &mut decisions {
        d.presence = presence_of(probe.in_head.as_ref(), &d.path);
    }
    Response::ZavetDecisions(Box::new(ZavetDecisionsView {
        repo,
        branch: probe.branch,
        decisions,
        uncaptured: probe.uncaptured,
    }))
}

/// `Request::ZavetSync` — sweep this repo's knowledge now instead of waiting
/// for the idle ticker, and make sure the ticker keeps sweeping it.
///
/// Two holes, one command. The ticker only visits repos in `state.repo_dirs`
/// (`lib.rs`'s `idle_ticker`), and that map is otherwise populated only by
/// session registration and agent events — so it is EMPTY on a freshly
/// restarted daemon, and a repo nobody has opened a session in is never swept
/// at all. Registering here is therefore the fix, not a side effect. The sweep
/// itself closes the ordinary 30 s / ~5 min latency.
///
/// Deliberately no `force`: this calls the same `capture_commits` the ticker
/// does, so an unchanged HEAD stays a complete no-op. Forcing a re-read could
/// only re-ingest blobs already stored, and could NOT pick up an uncommitted
/// record — capture reads git objects, not the worktree (DIRASH-0026). Those
/// stay reported through `uncaptured`.
pub async fn sync(state: &AppState, cwd: Option<String>, repo: Option<String>) -> Response {
    let (repo, dir_exists, cfg) = match query_repo(state, repo, cwd.clone()).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    // A sweep needs a real directory, not just a canonical name: `--project`
    // alone can only work if the daemon already remembers a dir for it.
    let Some(root) = repo_root(repo_workdir(state, &repo, cwd.as_deref())).await else {
        return Response::Error {
            message: format!(
                "no working directory known for {repo}; run `dira zavet sync` from inside a checkout"
            ),
        };
    };
    let dir = root.display().to_string();

    let registered = crate::control::register_repo_dir(state, &repo, &dir);

    let before = state.store.zavet_counts(&repo).await.unwrap_or_default();
    crate::capture::capture_commits(state, &dir, &repo).await;
    let after = state.store.zavet_counts(&repo).await.unwrap_or_default();

    let active = active_for(state, &repo, dir_exists.unwrap_or(false)).await;
    // Probed AFTER the sweep, so the two halves of the output agree: a record
    // this sweep just captured must not still read as uncaptured.
    let captured = state
        .store
        .zavet_decisions_list(&repo)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|d| (d.path, d.id))
        .collect();
    let probe = working_tree_probe(Some(root), cfg, Kinds::Decisions, captured).await;

    Response::ZavetSync(Box::new(ZavetSyncView {
        repo,
        active,
        decisions_captured: after.decisions_total.saturating_sub(before.decisions_total),
        trailers_captured: after.trailers.saturating_sub(before.trailers),
        decisions_total: after.decisions_total,
        registered,
        uncaptured: probe.uncaptured,
    }))
}

/// `Request::ZavetWiki` — the browsable knowledge base: an overview without a
/// topic, ranked matches with one.
pub async fn wiki(
    state: &AppState,
    topic: Option<String>,
    cwd: Option<String>,
    repo: Option<String>,
) -> Response {
    let (repo, _, cfg) = match query_repo(state, repo, cwd.clone()).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    if let Some(topic) = topic.filter(|t| !t.trim().is_empty()) {
        let results = search(state, &repo, &topic).await;
        return Response::ZavetSearch {
            query: topic,
            hits: results
                .decisions
                .iter()
                .take(10)
                .map(|(d, s)| search_hit(d, *s))
                .collect(),
            specs: results
                .specs
                .iter()
                .take(5)
                .map(|(sp, s)| spec_search_hit(sp, *s))
                .collect(),
            trailers: results.trailers,
        };
    }
    let decisions = state
        .store
        .zavet_decisions_list(&repo)
        .await
        .unwrap_or_default();
    let spec_rows = state
        .store
        .zavet_specs_list(&repo)
        .await
        .unwrap_or_default();
    let counts = state.store.zavet_counts(&repo).await.unwrap_or_default();
    let recent = state
        .store
        .zavet_recent_trailers(&repo, 5)
        .await
        .unwrap_or_default();
    // One working directory for every git probe, so staleness, branch and
    // presence can never describe two different trees.
    let root = repo_root(repo_workdir(state, &repo, cwd.as_deref())).await;
    let staleness = spec_staleness(
        root.clone(),
        spec_rows.iter().map(staleness_input).collect(),
    )
    .await;
    let mut decisions: Vec<ZavetDecisionView> = decisions.iter().map(decision_view).collect();
    let mut specs: Vec<ZavetSpecView> = spec_rows
        .iter()
        .zip(staleness)
        .map(|(s, stale)| spec_view(s, stale))
        .collect();
    attach_guard_stats(state, &repo, &mut decisions).await;
    let probe = working_tree_probe(
        root,
        cfg,
        Kinds::Both,
        decisions
            .iter()
            .map(|d| (d.path.clone(), d.id.clone()))
            .chain(specs.iter().map(|s| (s.path.clone(), s.slug.clone())))
            .collect(),
    )
    .await;
    // Looked up by path, so neither list's order is part of the contract.
    for d in &mut decisions {
        d.presence = presence_of(probe.in_head.as_ref(), &d.path);
    }
    for s in &mut specs {
        s.presence = presence_of(probe.in_head.as_ref(), &s.path);
    }
    let (active, superseded): (Vec<_>, Vec<_>) = decisions
        .into_iter()
        .partition(|d| d.status.as_deref().unwrap_or("active") == "active");
    Response::ZavetWiki(Box::new(dira_core::protocol::ZavetWikiView {
        repo,
        branch: probe.branch,
        decisions_total: counts.decisions_total,
        trailers: counts.trailers,
        guard_events: counts.guard_events,
        specs_total: counts.specs_total,
        active,
        superseded,
        specs,
        recent,
        uncaptured: probe.uncaptured,
    }))
}

/// `Request::ZavetSetMode` (`dira zavet enable|disable`).
pub async fn set_mode(
    state: &AppState,
    cwd: Option<String>,
    repo: Option<String>,
    mode: String,
) -> Response {
    let (repo, _, _) = match query_repo(state, repo, cwd).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let value = match mode.as_str() {
        "on" => Some(true),
        "off" => Some(false),
        "clear" => None,
        other => {
            return Response::Error {
                message: format!("mode must be on, off, or clear (got `{other}`)"),
            }
        }
    };
    match state.store.zavet_override_set(&repo, value).await {
        Ok(()) => Response::ZavetModeSet { repo, mode },
        Err(e) => Response::Error {
            message: format!("zavet set mode failed: {e}"),
        },
    }
}

/// What a why query confidently resolved to.
/// `Request::ZavetWhy` — resolve a query to the entity that answers it, then
/// delegate to [`decision_why`]/[`spec_why`] for the detail + cost.
pub async fn why(
    state: &AppState,
    query: String,
    cwd: Option<String>,
    repo: Option<String>,
) -> Response {
    let (repo, _, cfg) = match query_repo(state, repo, cwd.clone()).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    // Direct addressing first: a decision id, or an exact spec slug (slugs
    // are the spec's identity, like ids — a whitespace-free query is checked
    // against them before searching).
    //
    // Two candidate spellings, because the width this repo pads to is not
    // always knowable here: `--project` names a repo we are not standing in,
    // so cfg is the default. Canonical-at-cfg-width first (which resolves the
    // shorthand a human actually types, `CLOUD-42`), then the query as
    // written (which resolves a full id pasted from a repo of another width).
    let mut candidates: Vec<String> = Vec::new();
    if let Some(id) = dira_core::zavet::canonical_decision_id(&query, cfg.id_width) {
        candidates.push(id);
    }
    if let Some(id) = dira_core::zavet::normalize_decision_id(&query) {
        if !candidates.contains(&id) {
            candidates.push(id);
        }
    }
    if let Some(first) = candidates.first().cloned() {
        for id in &candidates {
            match state.store.zavet_decision_get(&repo, id).await {
                Ok(Some(d)) => return decision_why(state, &repo, d, None).await,
                Ok(None) => continue,
                Err(e) => {
                    return Response::Error {
                        message: format!("zavet why failed: {e}"),
                    }
                }
            }
        }
        return Response::Error {
            message: format!("no captured decision `{first}` for {repo} (is it committed, and is the daemon watching this repo?)"),
        };
    }
    let slug = query.trim();
    if !slug.is_empty() && !slug.contains(char::is_whitespace) {
        if let Some(spec) = state
            .store
            .zavet_spec_get(&repo, slug)
            .await
            .unwrap_or(None)
        {
            return spec_why(state, &repo, spec, None, cwd.as_deref()).await;
        }
    }
    // Free text ranks decisions AND specs with ONE confidence rule: the top
    // entity answers in full iff its score at least doubles the runner-up —
    // its own kind's second, the other kind's best, or the best orphan
    // trailer, whichever is stronger (a sole hit trivially qualifies). Ties
    // prefer decisions (records outrank documents). Anything less confident
    // returns the ranked matches rather than guessing; when only trailers
    // matched, the trailers ARE the answer. Zero is a clean pointer to the wiki.
    let mut results = search(state, &repo, &query).await;
    if results.is_empty() {
        return Response::Error {
            message: format!(
                "nothing recorded matches `{query}` for {repo} — browse with `dira zavet wiki`, or record it via /zavet:decide"
            ),
        };
    }
    let d1 = results.decisions.first().map(|(_, s)| *s).unwrap_or(0);
    let s1 = results.specs.first().map(|(_, s)| *s).unwrap_or(0);
    let d2 = results.decisions.get(1).map(|(_, s)| *s).unwrap_or(0);
    let s2 = results.specs.get(1).map(|(_, s)| *s).unwrap_or(0);
    let best_trailer = results.trailers.first().map(|t| t.score).unwrap_or(0);
    let (top, own_second, other_top, spec_wins) = if d1 >= s1 {
        (d1, d2, s1, false)
    } else {
        (s1, s2, d1, true)
    };
    // `top > 0` guarantees the winning list is non-empty.
    if top > 0 && top >= own_second.max(other_top).max(best_trailer) * 2 {
        return if spec_wins {
            let (spec, _) = results.specs.remove(0);
            spec_why(state, &repo, spec, Some(query), cwd.as_deref()).await
        } else {
            let (decision, _) = results.decisions.remove(0);
            decision_why(state, &repo, decision, Some(query)).await
        };
    }
    Response::ZavetSearch {
        query,
        hits: results
            .decisions
            .iter()
            .take(5)
            .map(|(d, s)| search_hit(d, *s))
            .collect(),
        specs: results
            .specs
            .iter()
            .take(5)
            .map(|(sp, s)| spec_search_hit(sp, *s))
            .collect(),
        trailers: results.trailers,
    }
}

/// Summed cost over priced session lines: `(human, agent, input, output)`.
fn cost_totals(sessions: &[dira_core::protocol::ZavetSessionCostView]) -> (i64, i64, u64, u64) {
    sessions.iter().fold((0, 0, 0, 0), |(h, a, i, o), s| {
        (
            h + s.human_seconds,
            a + s.agent_seconds,
            i + s.input_tokens,
            o + s.output_tokens,
        )
    })
}

/// The decision detail `zavet why` answers with: the record, its evidence
/// (commits, guard history, covering specs), and the cost panel.
async fn decision_why(
    state: &AppState,
    repo: &str,
    decision: ZavetDecisionRow,
    matched_query: Option<String>,
) -> Response {
    let decision_id = decision.id.clone();
    // Reverse supersedes link: the decision that replaced this one, if captured.
    let superseded_by = state
        .store
        .zavet_superseded_by(repo, &decision_id)
        .await
        .unwrap_or(None);
    // Reverse errata link: the records THIS one corrects. The forward
    // direction rides on the record itself (`corrected_by`).
    let corrects = state
        .store
        .zavet_corrects(repo, &decision_id)
        .await
        .unwrap_or_default();
    let commits = state
        .store
        .zavet_commits_for_decision(repo, &decision_id)
        .await
        .unwrap_or_default();
    let stats = state
        .store
        .zavet_guard_event_stats(repo, Some(&decision_id))
        .await
        .unwrap_or_default();
    let sessions = state
        .store
        .zavet_sessions_for_decision(repo, &decision_id)
        .await
        .unwrap_or_default();
    let specs = state
        .store
        .zavet_specs_for_decision(repo, &decision_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(slug, title)| dira_core::protocol::ZavetSpecRef { slug, title })
        .collect();

    let unattributed_commits = commits
        .iter()
        .filter(|c| c.source_session.is_none())
        .count() as u64;
    let unattributed_guard_events = stats.iter().map(|s| s.unattributed).sum();

    let priced = price_sessions(state, &sessions).await;
    let (total_human_seconds, total_agent_seconds, total_input_tokens, total_output_tokens) =
        cost_totals(&priced);
    let view = ZavetWhyView {
        repo: repo.to_string(),
        matched_query,
        body_md: decision.body_md.clone(),
        decision: decision_view(&decision),
        superseded_by,
        corrects,
        commits: commit_views(commits),
        guard_stats: guard_stat_views(stats),
        total_human_seconds,
        total_agent_seconds,
        total_input_tokens,
        total_output_tokens,
        sessions: priced,
        unattributed_commits,
        unattributed_guard_events,
        specs,
    };
    Response::ZavetWhy(Box::new(view))
}

/// The spec detail `zavet why` answers with when the winner is a living spec:
/// the document, its badges and links, staleness, `Spec:`-trailer commits,
/// and the same honestly-lower-bound cost panel decisions get.
async fn spec_why(
    state: &AppState,
    repo: &str,
    spec: ZavetSpecRow,
    matched_query: Option<String>,
    cwd: Option<&str>,
) -> Response {
    let commits = state
        .store
        .zavet_commits_for_spec(repo, &spec.slug)
        .await
        .unwrap_or_default();
    let sessions = state
        .store
        .zavet_sessions_for_spec(repo, &spec.slug)
        .await
        .unwrap_or_default();
    let stale = spec_staleness(repo_workdir(state, repo, cwd), vec![staleness_input(&spec)])
        .await
        .pop()
        .flatten();
    let unattributed_commits = commits
        .iter()
        .filter(|c| c.source_session.is_none())
        .count() as u64;
    let priced = price_sessions(state, &sessions).await;
    let (total_human_seconds, total_agent_seconds, total_input_tokens, total_output_tokens) =
        cost_totals(&priced);
    let view = ZavetSpecWhyView {
        repo: repo.to_string(),
        matched_query,
        body_md: spec.body_md.clone(),
        spec: spec_view(&spec, stale),
        commits: commit_views(commits),
        total_human_seconds,
        total_agent_seconds,
        total_input_tokens,
        total_output_tokens,
        sessions: priced,
        unattributed_commits,
    };
    Response::ZavetSpec(Box::new(view))
}

fn commit_views(
    commits: Vec<dira_core::store::ZavetCommitRef>,
) -> Vec<dira_core::protocol::ZavetCommitView> {
    commits
        .into_iter()
        .map(|c| dira_core::protocol::ZavetCommitView {
            sha: c.sha,
            message: c.message,
            authored_at: c.authored_at,
            session_id: c.source_session,
        })
        .collect()
}

/// Price an evidencing session set: for each session, de-duplicated human
/// seconds (global opening-signal attribution over the raw event log — the
/// same math `dira report` uses), idle-trimmed agent seconds, and token sums;
/// compacted history joins from the daily rollup. Shared by decision and spec
/// why views.
async fn price_sessions(
    state: &AppState,
    session_ids: &[String],
) -> Vec<dira_core::protocol::ZavetSessionCostView> {
    if session_ids.is_empty() {
        return Vec::new();
    }
    let events = state.store.events_since(None).await.unwrap_or_default();
    let idle = state.config.idle();
    let human_signals: Vec<(OffsetDateTime, String)> = events
        .iter()
        .filter(|e| e.kind.is_human_signal())
        .map(|e| (e.at, e.session_id.clone()))
        .collect();
    let human_by_session = dira_core::accounting::per_key_seconds(&human_signals, idle);

    let mut lines = Vec::with_capacity(session_ids.len());
    for sid in session_ids {
        let raw_human = human_by_session.get(sid).copied().unwrap_or(0);
        let raw_agent =
            crate::control::session_agent_evidence(&events, sid, state.config.agent_policy()).1;
        let hist = state
            .store
            .zavet_session_totals(sid)
            .await
            .unwrap_or_default();
        lines.push(dira_core::protocol::ZavetSessionCostView {
            session_id: sid.clone(),
            human_seconds: raw_human + hist.rollup_human_seconds,
            agent_seconds: raw_agent + hist.rollup_agent_seconds,
            input_tokens: hist.input_tokens,
            output_tokens: hist.output_tokens,
        });
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence_table_is_exact() {
        use ZavetMode::*;
        // (knob, override, dir_exists) -> active
        let cases = [
            (Auto, None, false, false),
            (Auto, None, true, true),
            (Auto, Some(true), false, true),
            (Auto, Some(false), true, false),
            (On, None, false, true),
            (On, None, true, true),
            (On, Some(false), true, false),
            (Off, None, true, false),
            (Off, None, false, false),
            (Off, Some(true), false, true),
        ];
        for (knob, ov, dir, want) in cases {
            assert_eq!(
                effective_mode(knob, ov, dir),
                want,
                "knob={knob:?} override={ov:?} dir={dir}",
            );
        }
    }

    #[test]
    fn dir_probe_checks_the_toplevel_only() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!zavet_dir_exists(tmp.path()));
        std::fs::create_dir(tmp.path().join(ZAVET_DIR)).unwrap();
        assert!(zavet_dir_exists(tmp.path()));
    }
}
