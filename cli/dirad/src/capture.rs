//! Commit capture — kept strictly off the writer's hot path.
//!
//! `capture_commits` shells out to `git` (`rev-parse`, `log`, `diff-tree |
//! patch-id`), which is blocking IO with no inherent timeout. If git stalls
//! (index.lock contention, a slow filesystem, a hung credential helper) a naive
//! inline call would freeze the single writer task — and with it every session's
//! `active_seconds` accrual and the `ManualTick` queue.
//!
//! So the blocking git walk runs in [`tokio::task::spawn_blocking`] wrapped in a
//! [`tokio::time::timeout`]: a capture that overruns the budget is logged and
//! dropped (the next commit-bearing event retries it). The whole capture is
//! `spawn`ed as a detached task by the writer, which returns to draining
//! immediately — git can never block timer accrual again.

use crate::state::AppState;
use dira_core::model::EventKind;
use dira_core::project::{self, CapturedCommit};
use dira_core::store::SourcesWritten;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Minimum spacing between git polls for one repo. Keeps a tool-call burst from
/// shelling out to git on every event; the idle ticker covers the steady state.
const COMMIT_POLL_THROTTLE: Duration = Duration::from_secs(5);
/// Backfill depth the first time a repo is seen; afterwards only `<head>..HEAD`.
const COMMIT_BACKFILL_LIMIT: usize = 15;
/// Cap on commits captured in a single `<head>..HEAD` walk (a runaway guard).
const COMMIT_CAPTURE_LIMIT: usize = 200;
/// Wall-clock budget for one git capture's blocking work. A capture that exceeds
/// it is abandoned (logged + dropped); the next commit-bearing event retries.
/// Comfortably above a healthy `git log`, low enough that a wedged git is shed
/// long before it could matter to a watching human.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);
/// Budget for the source-spec resolve that follows a capture. Separate from
/// [`CAPTURE_TIMEOUT`] on purpose: a slow plugin must cost the source specs of
/// this sweep, never the commits the walk already recorded. The plugin runner
/// bounds each of its two invocations to 5 s on its own; this caps the pair.
const SOURCES_TIMEOUT: Duration = Duration::from_secs(12);

/// Per-repo poll throttle. Lives in the single-threaded writer, so no lock.
#[derive(Default)]
pub struct Throttle {
    last: HashMap<String, Instant>,
}

impl Throttle {
    /// True if `repo` hasn't been polled within [`COMMIT_POLL_THROTTLE`]; records
    /// the poll time when it returns true.
    pub fn ready(&mut self, repo: &str) -> bool {
        let now = Instant::now();
        match self.last.get(repo) {
            Some(t) if now.duration_since(*t) < COMMIT_POLL_THROTTLE => false,
            _ => {
                self.last.insert(repo.to_string(), now);
                true
            }
        }
    }
}

/// Events after which a commit may have just landed — a tool call returned, an
/// agent turn ended, or a session/manual session closed.
pub fn captures_commits(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::PostTool
            | EventKind::Stop
            | EventKind::SessionEnd
            | EventKind::ManualStart
            | EventKind::ManualStop
            | EventKind::ManualTick
    )
}

/// Spawn a detached commit capture for `(cwd, canonical)`. Returns immediately —
/// the caller (the writer) never awaits git. The blocking git walk inside is
/// timeout-bounded so a wedged git is shed instead of leaking a task forever.
pub fn spawn_capture(state: &AppState, cwd: &str, canonical: &str) {
    let state = state.clone();
    let cwd = cwd.to_string();
    let canonical = canonical.to_string();
    tokio::spawn(async move {
        capture_commits(&state, &cwd, &canonical).await;
    });
}

/// The result of the blocking git portion of a capture, computed entirely inside
/// `spawn_blocking` so no `git` call ever runs on a runtime worker inline.
struct GitWalk {
    head: String,
    commits: Vec<CapturedCommit>,
    git_ref: Option<String>,
    /// Squash-resilient session anchoring signals over the cumulative diff
    /// (`merge-base(upstream, HEAD)..HEAD`), computed once per walk inside the
    /// same `spawn_blocking` so this extra git work never touches the hot path.
    signals: project::SessionSignals,
    /// Zavet knowledge captured from the walked commits (empty unless the repo
    /// is zavet-active): per-sha trailer sets, decision records parsed from
    /// `.zavet/decisions/*.md` blobs, and living specs parsed from
    /// `.zavet/specs/*.md` blobs touched by each commit.
    trailers: Vec<(String, Vec<dira_core::store::ZavetTrailer>)>,
    decisions: Vec<ZavetDecisionAt>,
    specs: Vec<ZavetSpecAt>,
    /// Whether the zavet sweep ran for this walk (the repo is zavet-active).
    zavet_active: bool,
    /// A walked commit touched a declared spec-source root or `.zavet/config`,
    /// so the source units are worth re-resolving after the walk.
    source_trigger: bool,
    /// A walked commit touched a framework rule file (a default path, a stored
    /// one, `AGENTS.md` or `.zavet/config`), so the rule sources are worth
    /// re-resolving — see [`refresh_sources`].
    rule_trigger: bool,
    /// Repo toplevel and id/source config the sweep resolved, handed on to the
    /// post-walk source refresh so neither is read twice.
    top: PathBuf,
    cfg: ZavetConfig,
}

use crate::zavet::ZavetConfig;

/// The trailer half of a sweep, on its own.
///
/// One batched `git log --no-walk` for the whole commit set, and nothing else.
/// [`zavet_sweep`]'s other half costs a `diff-tree` per commit plus a `show`
/// and a `rev-parse` per touched record — worth it when decisions and specs are
/// wanted, pure waste when only trailers are. Trailers ride arbitrary commits,
/// so their window is the widest one anybody walks; running the record parser
/// across it would spawn a subprocess per commit to discard every result.
///
/// This is not a second parser: decisions and specs are still parsed in exactly
/// one place (see [`zavet_sweep`]). This is that function's first stage, reused.
///
/// Takes the id config rather than reading it: every caller has already
/// resolved one for this tree, and `.zavet/config` is a file read per call.
pub fn zavet_trailers(
    root: &Path,
    commits: &[project::CommitRef],
    cfg: &crate::zavet::ZavetConfig,
) -> ZavetTrailers {
    let shas: Vec<String> = commits.iter().map(|c| c.sha.clone()).collect();
    project::commit_trailers(root, &shas)
        .into_iter()
        .map(|(sha, raw)| (sha, dira_core::zavet::normalize_trailers(&raw, cfg)))
        .filter(|(_, ts)| !ts.is_empty())
        .collect()
}

/// Per-sha trailer sets, as both the sweep and the reindex carry them.
pub type ZavetTrailers = Vec<(String, Vec<dira_core::store::ZavetTrailer>)>;

/// A decision record as of one commit, ready to upsert.
pub struct ZavetDecisionAt {
    pub sha: String,
    pub authored_at: Option<String>,
    pub cap: dira_core::store::ZavetDecisionCapture,
}

/// A living spec as of one commit, ready to upsert.
pub struct ZavetSpecAt {
    pub sha: String,
    pub authored_at: Option<String>,
    pub cap: dira_core::store::ZavetSpecCapture,
}

/// Run the blocking git walk for a repo: resolve HEAD, and (unless HEAD is
/// unchanged vs `baseline`) list the new commits + current branch. `None` when
/// the dir isn't a git repo, HEAD is unchanged, or git is unavailable.
///
/// `zavet` gates the knowledge sweep. The `.zavet/` dir probe for `auto` mode
/// happens here (inside the same blocking budget) — the caller passes the knob
/// + per-repo override pre-resolved, since the store is async.
fn git_walk(
    cwd: &str,
    baseline: Option<&str>,
    zavet: ZavetGate,
    rule_paths: &[String],
) -> Option<GitWalk> {
    let root = Path::new(cwd);
    let head = project::head_sha(root)?;
    if baseline == Some(head.as_str()) {
        return None; // HEAD unchanged since last poll — nothing to do
    }
    let range = baseline.map(|b| format!("{b}..HEAD"));
    let limit = if baseline.is_some() {
        COMMIT_CAPTURE_LIMIT
    } else {
        COMMIT_BACKFILL_LIMIT
    };
    let commits = project::log_commits(root, range.as_deref(), limit);
    let git_ref = project::current_branch(root);
    // Cumulative session signals — best-effort, all-None on merge/detached HEAD,
    // missing upstream, or git failure. Runs here so it shares the blocking budget.
    let signals = project::session_signals(root);

    let zavet_active = crate::zavet::effective_mode(
        zavet.knob,
        zavet.override_,
        crate::zavet::zavet_dir_exists(project::toplevel(root).as_deref().unwrap_or(root)),
    );
    let top = project::toplevel(root).unwrap_or_else(|| root.to_path_buf());
    let sweep = if zavet_active && !commits.is_empty() {
        let refs: Vec<project::CommitRef> = commits
            .iter()
            .map(|c| project::CommitRef {
                sha: c.sha.clone(),
                authored_at: c.authored_at.clone(),
            })
            .collect();
        zavet_sweep(root, &refs, rule_paths)
    } else {
        ZavetSweep::default()
    };

    Some(GitWalk {
        head,
        commits,
        git_ref,
        signals,
        trailers: sweep.trailers,
        decisions: sweep.decisions,
        specs: sweep.specs,
        zavet_active,
        source_trigger: sweep.source_trigger,
        rule_trigger: sweep.rule_trigger,
        top,
        cfg: sweep.cfg,
    })
}

/// The pre-resolved activation inputs for a walk (see [`git_walk`]).
#[derive(Clone, Copy)]
struct ZavetGate {
    knob: dira_core::config::ZavetMode,
    override_: Option<bool>,
}

impl ZavetGate {
    /// Whether the walk could find zavet active — false only when the knob or
    /// the per-repo override rules it out before the `.zavet/` probe.
    fn may_be_active(self) -> bool {
        crate::zavet::effective_mode(self.knob, self.override_, true)
    }
}

/// What one zavet sweep of a commit range yields.
#[derive(Default)]
pub struct ZavetSweep {
    pub trailers: Vec<(String, Vec<dira_core::store::ZavetTrailer>)>,
    pub decisions: Vec<ZavetDecisionAt>,
    pub specs: Vec<ZavetSpecAt>,
    /// A commit in the walked range touched a declared spec-source root or
    /// `.zavet/config` — see [`dira_core::zavet_specs::path_triggers_resolve`].
    /// The sweep only DETECTS this; resolving shells out to the plugin and is
    /// done by [`refresh_sources`] under its own budget.
    pub source_trigger: bool,
    /// A commit in the walked range touched a framework rule file — one of
    /// [`dira_core::zavet_specs::rule_trigger_paths`]. Detected here, resolved
    /// by [`refresh_sources`].
    pub rule_trigger: bool,
    /// The config this sweep resolved (id conventions + declared sources).
    pub cfg: ZavetConfig,
}

/// The zavet portion of a walk: batched trailer parse over the walked shas,
/// plus decision-record and spec parsing for every `.zavet/decisions/*.md` /
/// `.zavet/specs/*.md` blob touched by each commit. All plain
/// `git log`/`diff-tree`/`show` subprocess calls — shares the walk's blocking
/// budget.
///
/// Takes bare [`CommitRef`](project::CommitRef)s rather than full
/// [`CapturedCommit`]s so `dira zavet reindex` can drive the same parser over a
/// pathspec-scoped history walk. There must only ever be one implementation of
/// this parsing — two would drift, and a reindex that disagrees with the ambient
/// poll is worse than the under-indexing it exists to fix.
///
/// `rule_paths` are the rule-source paths already on record for the repo (an
/// override path is only known once resolved); with the declared defaults they
/// decide [`ZavetSweep::rule_trigger`].
pub fn zavet_sweep(
    root: &Path,
    commits: &[project::CommitRef],
    rule_paths: &[String],
) -> ZavetSweep {
    // One config read per sweep. Read from the WORKING TREE, not from each
    // historical blob: retired prefixes stay in `prefix-aliases`, so the
    // current config is what resolves an id minted under an older one.
    let cfg = crate::zavet::read_config(root);
    let trailers = zavet_trailers(root, commits, &cfg);
    let rule_triggers = dira_core::zavet_specs::rule_trigger_paths(&cfg, rule_paths);

    let mut decisions = Vec::new();
    let mut specs = Vec::new();
    let mut source_trigger = false;
    let mut rule_trigger = false;
    // Oldest first, so within one walk the INTRODUCING commit lands first and
    // the store's first-sight preservation keeps pointing at it. One
    // diff-tree per commit serves both the decision and the spec filter.
    for c in commits.iter().rev() {
        // Deletions count for the source trigger (a commit that removes a whole
        // source root adds and modifies nothing) but never for blob capture.
        let (changed, touched) = project::diff_paths(root, &c.sha);
        if !source_trigger
            && touched
                .iter()
                .any(|p| dira_core::zavet_specs::path_triggers_resolve(&cfg, p))
        {
            source_trigger = true;
        }
        if !rule_trigger && touched.iter().any(|p| rule_triggers.contains(p)) {
            rule_trigger = true;
        }
        for path in changed {
            let is_decision = dira_core::zavet::is_decision_path(&path);
            if !is_decision && !dira_core::zavet::is_spec_path(&path) {
                continue;
            }
            let Some(text) = project::show_blob(root, &c.sha, &path) else {
                continue;
            };
            if is_decision {
                let Some(mut cap) = dira_core::zavet::parse_decision(&text, &path, &cfg) else {
                    tracing::debug!(sha = %c.sha, path, "zavet: unparseable decision record skipped");
                    continue;
                };
                cap.content_hash = project::blob_oid(root, &c.sha, &path);
                decisions.push(ZavetDecisionAt {
                    sha: c.sha.clone(),
                    authored_at: c.authored_at.clone(),
                    cap,
                });
            } else {
                let Some(mut cap) = dira_core::zavet::parse_spec(&text, &path, &cfg) else {
                    tracing::debug!(sha = %c.sha, path, "zavet: unparseable spec skipped");
                    continue;
                };
                cap.content_hash = project::blob_oid(root, &c.sha, &path);
                specs.push(ZavetSpecAt {
                    sha: c.sha.clone(),
                    authored_at: c.authored_at.clone(),
                    cap,
                });
            }
        }
    }
    ZavetSweep {
        trailers,
        decisions,
        specs,
        source_trigger,
        rule_trigger,
        cfg,
    }
}

/// Which halves of [`refresh_sources`] a sweep asks for.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SourceRefresh {
    /// Source-backed spec units (`zavet specs --json`).
    pub units: bool,
    /// Framework rule sources (`zavet spec-sources --json`, DIRASH-0040).
    pub rules: bool,
}

/// Re-resolve the repo's source-backed spec units and/or framework rule
/// sources through the installed plugin and write them (plus `removed` marks)
/// into the store. The single owner of every plugin run a sweep makes
/// (DIRASH-0039, DIRASH-0040).
///
/// Runs OFF the writer task and OFF the commit walk's blocking budget, each
/// resolve under its own [`SOURCES_TIMEOUT`]: a wedged or missing plugin costs
/// this sweep its source specs or rule sources — never its commits. When both
/// are asked for, the two plugin runs overlap; the store writes that follow
/// stay sequential. Fail-open end to end: an unresolved run changes nothing,
/// and in particular marks nothing `removed`.
///
/// With no `spec-source:` lines declared the answer is known without asking the
/// plugin (there are no units, and rule sources follow the declarations), but
/// only when the config file really is there — a momentarily unreadable config
/// must not read as "everything was deleted".
///
/// Removals are the dangerous half of a resolve. They need BOTH of:
///  - the resolving checkout is on the repo's default branch (computed once,
///    for both halves): the store is keyed by canonical repo but a resolve
///    sees one checkout, so a feature branch or worktree that predates a unit
///    would flap it removed/active;
///  - an EMPTY answer for non-empty declarations is corroborated: every
///    declared root is absent (units), and none of the declared frameworks'
///    default rule files is on disk (rule sources). A plugin that emitted
///    nothing for something still there is a failed resolve, not a deletion.
///
/// Without them, seen rows are still upserted but nothing is removed. Rule
/// text is never read. Returns what changed, `(units, rules)` (all zero for a
/// half not asked for or not resolved).
pub async fn refresh_sources(
    state: &AppState,
    repo: &str,
    top: &Path,
    cfg: &ZavetConfig,
    session: Option<&str>,
    want: SourceRefresh,
) -> (SourcesWritten, SourcesWritten) {
    let (units, rules) = tokio::join!(
        async {
            if !want.units {
                return None;
            }
            resolve_declared(repo, top, cfg, "source spec", |top| {
                crate::zavet_sources::resolve_blocking(&top, false)
            })
            .await
        },
        async {
            if !want.rules {
                return None;
            }
            resolve_declared(repo, top, cfg, "rule source", |top| {
                crate::zavet_sources::resolve_rules_blocking(&top)
            })
            .await
        },
    );
    if units.is_none() && rules.is_none() {
        return Default::default();
    }
    let on_default_branch = {
        let top = top.to_path_buf();
        tokio::task::spawn_blocking(move || project::on_default_branch(&top))
            .await
            .unwrap_or(false)
    };
    let mut out = (SourcesWritten::default(), SourcesWritten::default());
    if let Some(units) = units {
        let roots: Vec<String> = cfg.spec_sources.iter().map(|d| d.root.clone()).collect();
        let allow_removals = removal_allowed(
            top,
            on_default_branch,
            units.is_empty(),
            &roots,
            Path::exists,
        );
        out.0 = persist_sources(state, repo, cfg, &units, session, allow_removals).await;
    }
    if let Some(rules) = rules {
        let defaults = dira_core::zavet_specs::rule_default_paths(cfg);
        let allow_removals = removal_allowed(
            top,
            on_default_branch,
            rules.is_empty(),
            &defaults,
            Path::is_file,
        );
        out.1 = persist_rule_sources(state, repo, &rules, allow_removals).await;
    }
    out
}

/// One declared-source resolve, off the runtime under [`SOURCES_TIMEOUT`] and
/// failing open (`None`: skip this sweep, logged at debug). With nothing
/// declared the answer is `Some(empty)` without running the plugin, but only
/// while `.zavet/config` is really there (see [`refresh_sources`]).
async fn resolve_declared<T: Send + 'static>(
    repo: &str,
    top: &Path,
    cfg: &ZavetConfig,
    what: &str,
    resolve: impl FnOnce(PathBuf) -> Option<Vec<T>> + Send + 'static,
) -> Option<Vec<T>> {
    if cfg.spec_sources.is_empty() {
        return top.join(crate::zavet::CONFIG_PATH).is_file().then(Vec::new);
    }
    let top = top.to_path_buf();
    match tokio::time::timeout(
        SOURCES_TIMEOUT,
        tokio::task::spawn_blocking(move || resolve(top)),
    )
    .await
    {
        Ok(Ok(found)) => found,
        Ok(Err(e)) => {
            tracing::debug!(repo, "{what} resolve task failed: {e}");
            None
        }
        Err(_) => {
            tracing::debug!(repo, "{what} resolve timed out — skipped this sweep");
            None
        }
    }
}

/// Whether a successful resolve may mark rows removed: the checkout is on the
/// default branch, and an `empty_answer` is corroborated by none of the
/// `corroborating` paths being `present` under `top` (see [`refresh_sources`]).
fn removal_allowed(
    top: &Path,
    on_default_branch: bool,
    empty_answer: bool,
    corroborating: &[String],
    present: fn(&Path) -> bool,
) -> bool {
    on_default_branch && !(empty_answer && corroborating.iter().any(|p| present(&top.join(p))))
}

/// Write a resolved unit set (the units half of [`refresh_sources`]).
async fn persist_sources(
    state: &AppState,
    repo: &str,
    cfg: &ZavetConfig,
    units: &[dira_core::zavet_specs::ResolvedUnit],
    session: Option<&str>,
    allow_removals: bool,
) -> SourcesWritten {
    if units.is_empty() && !cfg.spec_sources.is_empty() && !allow_removals {
        // Nothing to upsert and nothing we may remove: a no-op either way.
        tracing::debug!(
            repo,
            "empty source resolve not corroborated; changing nothing"
        );
        return SourcesWritten::default();
    }
    match crate::zavet_sources::persist_units(&state.store, repo, units, session, allow_removals)
        .await
    {
        Ok(p) => {
            if p.changed() {
                tracing::info!(
                    repo,
                    upserted = p.upserted,
                    removed = p.removed,
                    "captured source-backed specs"
                );
            }
            p
        }
        Err(e) => {
            tracing::warn!("source spec persist failed for {repo}: {e}");
            SourcesWritten::default()
        }
    }
}

/// Write a resolved rule-source set (the rules half of [`refresh_sources`]).
/// Local metadata only: nothing here nudges the knowledge sync.
async fn persist_rule_sources(
    state: &AppState,
    repo: &str,
    rules: &[dira_core::zavet_specs::RuleSource],
    allow_removals: bool,
) -> SourcesWritten {
    match state
        .store
        .zavet_upsert_rule_sources(repo, rules, allow_removals)
        .await
    {
        Ok(w) => {
            if w.changed() {
                tracing::info!(
                    repo,
                    upserted = w.upserted,
                    removed = w.removed,
                    "captured framework rule sources"
                );
            }
            w
        }
        Err(e) => {
            tracing::warn!("rule source persist failed for {repo}: {e}");
            SourcesWritten::default()
        }
    }
}

/// Poll a repo for new commits and record them locally. Best-effort: a non-git
/// dir or a git failure simply captures nothing. On a repo's first sight it does a
/// bounded backfill of recent commits, then only commits past the recorded HEAD
/// watermark. Nudges sync when anything new was recorded.
///
/// The git portion runs in [`spawn_blocking`](tokio::task::spawn_blocking) under
/// a [`timeout`](tokio::time::timeout); the surrounding async DB work is cheap.
/// This whole future is itself spawned detached by the writer (see
/// [`spawn_capture`]), so neither the blocking git nor a hung capture can stall
/// the drain loop.
pub async fn capture_commits(state: &AppState, cwd: &str, canonical: &str) {
    let baseline = match state.store.repo_baseline_get(canonical).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("repo baseline read failed for {canonical}: {e}");
            return;
        }
    };

    // Zavet activation inputs, resolved before the blocking walk (the store is
    // async): the global knob plus the per-repo override. One meta read for
    // active repos; the `.zavet/` dir probe itself runs inside the walk.
    let zavet = ZavetGate {
        knob: state.config.modules.zavet,
        override_: state
            .store
            .zavet_override_get(canonical)
            .await
            .unwrap_or(None),
    };

    // Rule-source paths already on record, so the sweep also notices an edit
    // to an override path (the defaults it derives from the config itself).
    // Only read when the sweep can run at all: a repo zavet is forced or
    // knobbed off for never consults them.
    let rule_paths = if zavet.may_be_active() {
        state
            .store
            .zavet_rule_source_paths(canonical)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // The blocking git work, off the runtime worker and time-boxed.
    let walk = {
        let cwd = cwd.to_string();
        let baseline = baseline.clone();
        let join = tokio::task::spawn_blocking(move || {
            git_walk(&cwd, baseline.as_deref(), zavet, &rule_paths)
        });
        match tokio::time::timeout(CAPTURE_TIMEOUT, join).await {
            Ok(Ok(walk)) => walk,
            Ok(Err(e)) => {
                tracing::warn!("commit capture task failed for {canonical}: {e}");
                return;
            }
            Err(_) => {
                // git overran the budget — drop this capture; the next
                // commit-bearing event retries. Never blocks the writer.
                tracing::warn!(
                    repo = %canonical,
                    timeout_secs = CAPTURE_TIMEOUT.as_secs(),
                    "commit capture timed out — dropping (will retry on next event)"
                );
                return;
            }
        }
    };
    let Some(walk) = walk else {
        return; // not a git repo, or HEAD unchanged
    };

    // The session the daemon currently observes for this repo, IFF unambiguous
    // (exactly one active). Concurrent or absent sessions yield None and the
    // cloud anchors on author + time instead.
    let source_session = crate::control::lock_recover(&state.sessions).session_for_repo(canonical);

    let mut recorded = 0usize;
    for c in &walk.commits {
        match state
            .store
            .record_commit(
                c,
                Some(canonical),
                walk.git_ref.as_deref(),
                source_session.as_deref(),
                Some(&walk.signals),
            )
            .await
        {
            Ok(true) => recorded += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!("record commit {} failed: {e}", c.sha),
        }
    }
    // Persist the zavet sweep (no-ops when the repo isn't zavet-active). All
    // idempotent — a re-walk re-records nothing.
    for (sha, trailers) in &walk.trailers {
        if let Err(e) = state
            .store
            .zavet_record_trailers(Some(canonical), sha, trailers)
            .await
        {
            tracing::warn!("zavet trailers for {sha} failed: {e}");
        }
    }
    for d in &walk.decisions {
        if let Err(e) = state
            .store
            .zavet_upsert_decision(
                canonical,
                &d.cap,
                &d.sha,
                d.authored_at.as_deref(),
                source_session.as_deref(),
            )
            .await
        {
            tracing::warn!("zavet decision {} failed: {e}", d.cap.id);
        } else {
            tracing::info!(decision = %d.cap.id, repo = %canonical, "captured zavet decision");
        }
    }
    for s in &walk.specs {
        if let Err(e) = state
            .store
            .zavet_upsert_spec(
                canonical,
                &s.cap,
                &s.sha,
                s.authored_at.as_deref(),
                source_session.as_deref(),
            )
            .await
        {
            tracing::warn!("zavet spec {} failed: {e}", s.cap.slug);
        } else {
            tracing::info!(spec = %s.cap.slug, repo = %canonical, "captured zavet spec");
        }
    }

    // Source-backed spec units (OpenSpec / Spec Kit / BMAD / superpowers) —
    // only when a walked commit touched a declared root or `.zavet/config` —
    // and framework rule files (constitution, OpenSpec config, BMAD context) —
    // only when one, `AGENTS.md` or `.zavet/config` was touched. Rule sources
    // are local metadata: only a unit change nudges the knowledge sync.
    let want = SourceRefresh {
        units: walk.zavet_active && walk.source_trigger,
        rules: walk.zavet_active && walk.rule_trigger,
    };
    let mut sources_changed = false;
    if want.units || want.rules {
        let (units, _rules) = refresh_sources(
            state,
            canonical,
            &walk.top,
            &walk.cfg,
            source_session.as_deref(),
            want,
        )
        .await;
        sources_changed = units.changed();
    }
    // Bind the session to a spec from facts only (exact `Spec:` trailer, else
    // exact branch). After the source refresh so a unit introduced by these very
    // commits is already known. An unambiguous live session is required, and an
    // explicit `dira start --spec` is never overridden.
    if walk.zavet_active {
        if let Some(sid) = source_session.as_deref() {
            let by_sha: HashMap<&str, Vec<String>> = walk
                .trailers
                .iter()
                .map(|(sha, ts)| {
                    (
                        sha.as_str(),
                        ts.iter()
                            .filter(|t| t.key == "spec")
                            .flat_map(|t| dira_core::zavet::spec_trailer_slugs(&t.value))
                            .collect(),
                    )
                })
                .collect();
            // Newest commit first, as `git log` walked them.
            let values: Vec<Vec<String>> = walk
                .commits
                .iter()
                .filter_map(|c| by_sha.get(c.sha.as_str()).cloned())
                .collect();
            crate::zavet_sources::attribute_session(
                &state.store,
                &state.sessions,
                sid,
                canonical,
                &values,
                walk.git_ref.as_deref(),
            )
            .await;
        }
    }

    if let Err(e) = state.store.repo_baseline_set(canonical, &walk.head).await {
        tracing::warn!("repo baseline set failed for {canonical}: {e}");
    }
    if sources_changed
        || !walk.trailers.is_empty()
        || !walk.decisions.is_empty()
        || !walk.specs.is_empty()
    {
        // Fresh knowledge landed — nudge its channel (lossy; backstop covers).
        let _ = state.knowledge_sync.trigger.try_send(());
    }
    if recorded > 0 {
        tracing::info!(commits = recorded, repo = %canonical, "captured commits");
        let _ = state.sync.trigger.try_send(());
        // A commit landing is activity — wake the heartbeat too (WP-A3).
        state.presence_wake.notify_waiters();
    }
}
