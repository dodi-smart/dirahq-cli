//! Source-backed spec units, consumed ONLY through the installed zavet plugin.
//!
//! Spec-driven-development frameworks (OpenSpec, Spec Kit, BMAD, superpowers)
//! resolve into zavet's existing living-spec object — slug `<kind>:<id>`,
//! `origin: external`, never verified. The adapters that read those
//! frameworks' files live exactly once, in zavet's `bin/zavet`, behind
//! `zavet specs --json` (JSON Lines, schema v1). This module is the one place
//! dira runs that command and the one place it parses the rows.
//!
//! Two rules are non-negotiable (DIRASH-0039):
//!
//! 1. **Never execute repository content.** The binary run is the
//!    *plugin-installed* `<plugin_root>/bin/zavet`, resolved offline from
//!    Claude Code's own registry — never the repo's vendored
//!    `.zavet/bin/zavet`, which is checked-in code a stranger's commit can
//!    change. [`plugin_root_offline`] (and [`plugin_root_cached`], which only
//!    memoises it) is the only resolver.
//! 2. **Fail open.** A missing plugin, an old plugin without the `specs_json`
//!    capability, a timeout, a malformed line: every one yields "no source
//!    specs this sweep" (or one skipped row), never an error that reaches the
//!    capture path. The caller logs at debug and moves on.
//!
//! Text never leaves this module unless the caller asks for it
//! (`with_text`), and it is never persisted locally — the store strips it
//! (`store::strip_source_text`) and the knowledge sync re-reads it from the
//! repo at the consent-gated full tier (D-0001, DIRASH-0030).

use dira_contract::{
    BodyRef, RequirementRef, SpecCounts, SpecDrift, SpecGroup, SpecSource, TaskRef, UnitFile,
};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime};

/// `<plugin>@<marketplace>` id of the zavet plugin in Claude Code's registry.
pub const PLUGIN_ID: &str = "zavet@dirahq";
/// Marketplace the zavet plugin ships from.
pub const MARKETPLACE_NAME: &str = "dirahq";
/// The `specs --json` row schema this build understands (`"v"` on each row,
/// `"specs_json"` in `zavet version --json`).
pub const SPECS_JSON_VERSION: u64 = 1;
/// Wall-clock budget for one plugin invocation. A wedged `bin/zavet` is killed
/// and the sweep goes on without source specs.
pub const RUN_TIMEOUT: Duration = Duration::from_secs(5);
/// Cap on captured stdout (a runaway guard — a real repo emits far less).
pub const MAX_OUTPUT_BYTES: u64 = 16 * 1024 * 1024;
/// Env override of the plugin root. Lets a user with a non-standard install
/// point dira at it, and lets tests inject a fake plugin. Never a repo path:
/// it is read from the daemon's own environment, not from repository content.
pub const PLUGIN_ROOT_ENV: &str = "DIRA_ZAVET_PLUGIN_ROOT";

// ---------------------------------------------------------------------------
// Plugin root — offline resolution
// ---------------------------------------------------------------------------

/// One plugin record from `installed_plugins.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledRecord {
    pub version: String,
    pub scope: String,
    pub install_path: String,
}

#[derive(Debug, Deserialize)]
struct InstalledPluginsFile {
    version: u64,
    plugins: HashMap<String, Vec<InstalledPluginRecord>>,
}

#[derive(Debug, Deserialize)]
struct InstalledPluginRecord {
    scope: String,
    #[serde(rename = "installPath")]
    install_path: String,
    version: String,
}

/// The `installed_plugins.json` schema this reader knows. A future bump makes
/// an old binary correctly refuse a file it no longer understands.
pub const KNOWN_INSTALLED_PLUGINS_SCHEMA: u64 = 2;

/// `~/.claude/plugins/installed_plugins.json`.
fn installed_plugins_path(home: &Path) -> PathBuf {
    home.join(".claude")
        .join("plugins")
        .join("installed_plugins.json")
}

/// `~/.claude/plugins/known_marketplaces.json`.
fn known_marketplaces_path(home: &Path) -> PathBuf {
    home.join(".claude")
        .join("plugins")
        .join("known_marketplaces.json")
}

/// Read `~/.claude/plugins/installed_plugins.json` without spawning anything.
///
/// `None` = absent / unreadable / unknown schema; `Some(None)` = readable and
/// the zavet plugin is not in it; `Some(Some(rec))` = installed.
pub fn installed_plugin_record(home: &Path) -> Option<Option<InstalledRecord>> {
    let text = std::fs::read_to_string(installed_plugins_path(home)).ok()?;
    let file: InstalledPluginsFile = serde_json::from_str(&text).ok()?;
    if file.version != KNOWN_INSTALLED_PLUGINS_SCHEMA {
        return None;
    }
    Some(
        file.plugins
            .get(PLUGIN_ID)
            .and_then(|v| v.first())
            .map(|rec| InstalledRecord {
                version: rec.version.clone(),
                scope: rec.scope.clone(),
                install_path: rec.install_path.clone(),
            }),
    )
}

#[derive(Debug, Deserialize)]
struct MarketplaceEntry {
    source: MarketplaceSource,
    #[serde(rename = "installLocation")]
    install_location: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MarketplaceSource {
    source: String,
    path: Option<String>,
}

/// The directory that actually holds the plugin's files: the reported cache
/// path when it exists, else the `dirahq` marketplace's own on-disk location
/// when that is a `directory` source (how anyone developing zavet has it
/// registered — it is loaded in place and never populates the cache).
pub fn resolve_plugin_root(home: &Path, reported: &str) -> String {
    if Path::new(reported).is_dir() {
        return reported.to_string();
    }
    marketplace_directory_location(home).unwrap_or_else(|| reported.to_string())
}

fn marketplace_directory_location(home: &Path) -> Option<String> {
    let text = std::fs::read_to_string(known_marketplaces_path(home)).ok()?;
    let all: HashMap<String, MarketplaceEntry> = serde_json::from_str(&text).ok()?;
    let entry = all.get(MARKETPLACE_NAME)?;
    if entry.source.source != "directory" {
        return None;
    }
    entry
        .source
        .path
        .clone()
        .or_else(|| entry.install_location.clone())
        .filter(|p| Path::new(p).is_dir())
}

/// The installed plugin's root, resolved from the registry file alone — it
/// never invokes `claude` (which is not free of side effects, and the daemon
/// and onboarding's read-only detection both need an inert probe).
///
/// [`PLUGIN_ROOT_ENV`] is authoritative when set (non-empty): it names the
/// plugin, or — if it names nothing — there is no plugin. It never falls back
/// to the registry, so a pinned root cannot silently turn into a different one.
pub fn plugin_root_offline() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(PLUGIN_ROOT_ENV).filter(|v| !v.is_empty()) {
        let p = PathBuf::from(p);
        return p.is_dir().then_some(p);
    }
    let home = crate::config::home_dir().ok()?;
    let rec = installed_plugin_record(&home)??;
    let root = PathBuf::from(resolve_plugin_root(&home, &rec.install_path));
    root.is_dir().then_some(root)
}

/// `<plugin_root>/bin/zavet`.
pub fn plugin_bin(plugin_root: &Path) -> PathBuf {
    plugin_root.join("bin").join("zavet")
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn lock_cache<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the cached plugin root was resolved from: the two registry files that
/// decide it. A re-install, an upgrade or a marketplace change rewrites one of
/// them, which changes its mtime and invalidates the entry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RootStamp {
    installed: Option<SystemTime>,
    marketplaces: Option<SystemTime>,
}

/// [`plugin_root_offline`] for the daemon's repeated resolves: the answer is
/// reused until a registry file changes, so a burst of sweeps does not re-read
/// and re-parse the registry each time. Only a *found* root is cached (a
/// missing plugin is already a failed `read` away), and the cached directory
/// is re-checked on every hit. A pinned [`PLUGIN_ROOT_ENV`] is a single `stat`
/// and bypasses the cache.
pub fn plugin_root_cached() -> Option<PathBuf> {
    static CACHE: OnceLock<Mutex<Option<(RootStamp, PathBuf)>>> = OnceLock::new();
    if std::env::var_os(PLUGIN_ROOT_ENV).is_some_and(|v| !v.is_empty()) {
        return plugin_root_offline();
    }
    let home = crate::config::home_dir().ok()?;
    let stamp = RootStamp {
        installed: file_mtime(&installed_plugins_path(&home)),
        marketplaces: file_mtime(&known_marketplaces_path(&home)),
    };
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if let Some((s, root)) = lock_cache(cache).as_ref() {
        if *s == stamp && root.is_dir() {
            return Some(root.clone());
        }
    }
    let root = plugin_root_offline()?;
    *lock_cache(cache) = Some((stamp, root.clone()));
    Some(root)
}

// ---------------------------------------------------------------------------
// Runner — spawn with a hard timeout
// ---------------------------------------------------------------------------

/// Why a plugin invocation produced no usable output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// The binary could not be spawned.
    Spawn(String),
    /// Exceeded the budget and was killed (also: the output reader did not
    /// finish in time, so the stdout we hold may be incomplete).
    Timeout,
    /// Wrote more than [`MAX_OUTPUT_BYTES`]. Distinct from a timeout so the
    /// truncation is never mistaken for a complete (shorter) answer.
    OutputCap,
    /// Exited non-zero.
    Failed(i32),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Spawn(e) => write!(f, "could not run zavet: {e}"),
            RunError::Timeout => write!(f, "zavet timed out"),
            RunError::OutputCap => {
                write!(f, "zavet output exceeded the {MAX_OUTPUT_BYTES} byte cap")
            }
            RunError::Failed(c) => write!(f, "zavet exited with status {c}"),
        }
    }
}

/// Runs `bin args…` in `cwd` and returns stdout. Abstracted so the resolver is
/// unit-testable without a real plugin, and so a test can simulate a hang.
pub trait SpecRunner {
    fn run(
        &self,
        bin: &Path,
        cwd: &Path,
        args: &[&str],
        timeout: Duration,
    ) -> Result<String, RunError>;

    /// Modification time of `path`, or `None` when it cannot be read. Part of
    /// the runner seam so a test can move a file's "mtime" without touching
    /// the disk; the capability cache is keyed on it.
    fn stamp(&self, path: &Path) -> Option<SystemTime> {
        file_mtime(path)
    }
}

/// The real runner: spawn, drain stdout on a thread, poll for exit, kill on
/// timeout. stdin is closed; stderr is discarded.
pub struct SystemSpecRunner;

impl SpecRunner for SystemSpecRunner {
    fn run(
        &self,
        bin: &Path,
        cwd: &Path,
        args: &[&str],
        timeout: Duration,
    ) -> Result<String, RunError> {
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // Its own process group, so a timeout can kill the script AND the
        // `git`/`sleep` grandchildren it spawned, not just the shell.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().map_err(|e| RunError::Spawn(e.to_string()))?;
        let stdout = child.stdout.take();
        // The reader hands its bytes over a channel instead of being joined: a
        // grandchild (`git`, `sleep`) that outlives the killed script keeps the
        // pipe open, and joining would wait for it. A detached reader simply
        // ends when the pipe finally closes. It reads one byte past the cap so
        // an over-cap answer is detectable, then drops the pipe (the child gets
        // EPIPE instead of blocking forever on a full pipe).
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(out) = stdout {
                let _ = out.take(MAX_OUTPUT_BYTES + 1).read_to_end(&mut buf);
            }
            let over = buf.len() as u64 > MAX_OUTPUT_BYTES;
            let _ = tx.send((buf, over));
        });
        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) if Instant::now() >= deadline => {
                    kill_group(&mut child);
                    let _ = child.wait();
                    return Err(RunError::Timeout);
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => return Err(RunError::Spawn(e.to_string())),
            }
        };
        // A reader that cannot finish means the answer may be incomplete — an
        // empty or short string here would read as "no units" and could mark
        // every unit removed. That is a timeout, never an empty success.
        let Ok((buf, over)) = rx.recv_timeout(Duration::from_secs(2)) else {
            kill_group(&mut child);
            return Err(RunError::Timeout);
        };
        if over {
            return Err(RunError::OutputCap);
        }
        if !status.success() {
            return Err(RunError::Failed(status.code().unwrap_or(-1)));
        }
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }
}

/// Kill the child's whole process group (unix) so the script's own children go
/// with it, then the child itself. Elsewhere only the child is killed.
fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // `process_group(0)` made the child a group leader: pgid == pid.
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", child.id())])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
}

// ---------------------------------------------------------------------------
// Rows (plan C2) and their mapping to the contract
// ---------------------------------------------------------------------------

/// `null` -> the type's default. The plugin emits JSON `null` for absent
/// scalars (`"oid":null`); serde's `#[serde(default)]` only covers a MISSING
/// key, so a plain `String` would reject the whole row on an explicit null.
fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RowGroup {
    #[serde(deserialize_with = "null_default")]
    kind: String,
    #[serde(deserialize_with = "null_default")]
    id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RowFile {
    #[serde(deserialize_with = "null_default")]
    path: String,
    #[serde(deserialize_with = "null_default")]
    oid: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RowRequirement {
    #[serde(deserialize_with = "null_default")]
    id: String,
    #[serde(deserialize_with = "null_default")]
    state: String,
    #[serde(deserialize_with = "null_default")]
    scenarios: u32,
    capability: Option<String>,
    text: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RowTask {
    #[serde(deserialize_with = "null_default")]
    id: String,
    #[serde(deserialize_with = "null_default")]
    done: bool,
    story: Option<String>,
    #[serde(deserialize_with = "null_default")]
    parallel: bool,
    status: Option<String>,
    text: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct RowDrift {
    #[serde(deserialize_with = "null_default")]
    planned_untouched: u32,
    #[serde(deserialize_with = "null_default")]
    touched_unplanned: u32,
    #[serde(deserialize_with = "null_default")]
    edited_with_implementation: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RowDoc {
    #[serde(deserialize_with = "null_default")]
    path: String,
    #[serde(deserialize_with = "null_default")]
    md: String,
}

/// One `zavet specs --json` line. Unknown fields are ignored (the plugin may
/// grow); every field is optional so a partial row degrades, never panics.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Row {
    #[serde(deserialize_with = "null_default")]
    v: u64,
    #[serde(deserialize_with = "null_default")]
    slug: String,
    #[serde(deserialize_with = "null_default")]
    kind: String,
    #[serde(deserialize_with = "null_default")]
    id: String,
    #[serde(deserialize_with = "null_default")]
    root: String,
    framework_version: Option<String>,
    title: Option<String>,
    #[serde(deserialize_with = "null_default")]
    state: String,
    native_state: Option<String>,
    group: Option<RowGroup>,
    #[serde(deserialize_with = "null_default")]
    capabilities: Vec<String>,
    risk: Option<String>,
    #[serde(deserialize_with = "null_default")]
    primary: String,
    #[serde(deserialize_with = "null_default")]
    files: Vec<RowFile>,
    first_commit: Option<String>,
    first_at: Option<String>,
    last_commit: Option<String>,
    last_at: Option<String>,
    closed_on: Option<String>,
    #[serde(deserialize_with = "null_default")]
    paths: Vec<String>,
    #[serde(deserialize_with = "null_default")]
    planned_paths: Vec<String>,
    #[serde(deserialize_with = "null_default")]
    requirements: Vec<RowRequirement>,
    #[serde(deserialize_with = "null_default")]
    tasks: Vec<RowTask>,
    drift: Option<RowDrift>,
    #[serde(deserialize_with = "null_default")]
    tracker_refs: Vec<String>,
    unresolved: Option<String>,
    summary: Option<String>,
    #[serde(deserialize_with = "null_default")]
    docs: Vec<RowDoc>,
}

/// A resolved source unit: the contract [`SpecSource`] plus the capture-side
/// facts that live on the spec row itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedUnit {
    /// `<kind>:<id>` — the spec slug.
    pub slug: String,
    pub title: Option<String>,
    /// The unit's primary file (becomes the spec's `path`).
    pub primary: String,
    pub first_commit: Option<String>,
    pub first_at: Option<String>,
    pub last_commit: Option<String>,
    pub last_at: Option<String>,
    /// Accreted `Spec:`-trailer coverage plus declared globs. Informational
    /// here: coverage the store already tracks is not overwritten from it.
    pub paths: Vec<String>,
    /// Stable hash of the sorted unit-file blob oids (`record_sha`).
    pub record_sha: String,
    pub source: SpecSource,
    /// Composed primary documents — populated only with `with_text`.
    pub docs: Vec<(String, String)>,
}

/// Why a whole resolve produced nothing. Informational: every variant means
/// "skip source specs this sweep".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    NoPlugin,
    /// The plugin predates the command asked for (`zavet specs --json`, or
    /// `zavet spec-sources --json` for rule sources).
    Unsupported,
    Run(RunError),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::NoPlugin => write!(f, "zavet plugin is not installed"),
            ResolveError::Unsupported => {
                write!(
                    f,
                    "installed zavet plugin lacks the needed `--json` capability"
                )
            }
            ResolveError::Run(e) => write!(f, "{e}"),
        }
    }
}

/// Stable identity of a unit's files: sorted `path\0oid` pairs hashed with two
/// seeded FNV-1a passes (128 bits, hex). Order-independent; changes iff a unit
/// file is added, removed, renamed or edited.
pub fn record_sha(files: &[UnitFile]) -> String {
    let mut parts: Vec<String> = files
        .iter()
        .map(|f| format!("{}\u{0}{}", f.path, f.oid))
        .collect();
    parts.sort_unstable();
    let joined = parts.join("\u{1}");
    let lo = crate::sync::batch::fnv1a_bytes(joined.as_bytes(), 0xcbf29ce484222325);
    let hi = crate::sync::batch::fnv1a_bytes(joined.as_bytes(), 0x84222325cbf29ce4);
    format!("{hi:016x}{lo:016x}")
}

/// The lifecycle words a source unit's normalised `state` may take (the
/// contract's closed list).
pub const KNOWN_STATES: &[&str] = &[
    "proposed",
    "active",
    "in-progress",
    "review",
    "done",
    "archived",
    "removed",
    "unresolved",
];

/// The story-status vocabulary `SpecCounts.story_status` is keyed by.
pub const KNOWN_STORY_STATUSES: &[&str] = &["backlog", "ready", "in-progress", "review", "done"];

/// Bucket every unknown word under this key.
pub const OTHER_BUCKET: &str = "other";

/// A vocabulary word as the closed lists spell it: trimmed, lower-cased, with
/// spaces and underscores folded to `-` (`In Progress`, `file_missing`).
fn fold_word(raw: &str) -> String {
    raw.trim().to_ascii_lowercase().replace([' ', '_'], "-")
}

fn bucket(raw: &str, vocab: &'static [&'static str]) -> &'static str {
    let k = fold_word(raw);
    vocab
        .iter()
        .find(|v| **v == k)
        .copied()
        .unwrap_or(OTHER_BUCKET)
}

/// A task status as a `story_status` map key: one of
/// [`KNOWN_STORY_STATUSES`], else `"other"`. The key rides the signed
/// attestation as a JSON object key; a verbatim framework string such as
/// `__proto__` is stripped by the cloud's zod, the body no longer matches the
/// signature, and the whole batch is rejected as `bad_signature`.
pub fn normalize_story_status(raw: &str) -> &'static str {
    bucket(raw, KNOWN_STORY_STATUSES)
}

/// A source state as a census (`by_state`) key: one of [`KNOWN_STATES`], else
/// `"other"`. Same signing hazard as [`normalize_story_status`].
pub fn normalize_state_bucket(raw: &str) -> &'static str {
    bucket(raw, KNOWN_STATES)
}

/// Longest `native_state` kept, in characters.
pub const MAX_NATIVE_STATE_CHARS: usize = 64;

/// A framework's own state word, verbatim except that control characters are
/// dropped and the length is capped. `None` when nothing printable remains.
pub fn sanitize_native_state(raw: &str) -> Option<String> {
    sanitize_scalar(Some(raw), MAX_NATIVE_STATE_CHARS)
}

/// Control characters dropped, trimmed, capped at `max` characters; `None`
/// when nothing printable remains.
fn sanitize_scalar(raw: Option<&str>, max: usize) -> Option<String> {
    let s: String = raw?.chars().filter(|c| !c.is_control()).take(max).collect();
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn map_row(mut row: Row, with_text: bool) -> Option<ResolvedUnit> {
    if row.kind == "zavet" {
        return None; // native specs keep their own file parse (DIRASH-0028)
    }
    if row.v != SPECS_JSON_VERSION {
        return None; // a schema this build does not know
    }
    // An unresolved unit is exactly the one whose primary file is missing, so
    // it still needs a stable `path`: its first file, else its source root.
    if row.primary.is_empty() && row.unresolved.is_some() {
        row.primary = row
            .files
            .iter()
            .map(|f| f.path.clone())
            .find(|p| !p.is_empty())
            .unwrap_or_else(|| row.root.clone());
    }
    if row.kind.is_empty() || row.id.is_empty() || row.primary.is_empty() {
        return None;
    }
    let slug = format!("{}:{}", row.kind, row.id);
    if !row.slug.is_empty() && row.slug != slug {
        return None; // the row disagrees with itself — trust nothing from it
    }
    let unit_files: Vec<UnitFile> = row
        .files
        .iter()
        .filter(|f| !f.path.is_empty())
        .map(|f| UnitFile {
            path: f.path.clone(),
            oid: f.oid.clone(),
        })
        .collect();
    let sha = record_sha(&unit_files);
    let mut source = SpecSource {
        kind: row.kind.clone(),
        id: row.id.clone(),
        root: row.root.clone(),
        framework_version: row.framework_version.clone(),
        state: row.state.trim().to_ascii_lowercase(),
        native_state: row.native_state.as_deref().and_then(sanitize_native_state),
        unit_files,
        ..SpecSource::default()
    };
    if source.state.is_empty() {
        source.state = "unresolved".into();
    }

    let mut docs = Vec::new();
    // Unresolved rows carry no other claims: identity and files only.
    if row.unresolved.is_some() {
        source.state = "unresolved".into();
    } else {
        source.closed_on = row.closed_on.clone();
        source.group = row
            .group
            .as_ref()
            .filter(|g| !g.kind.is_empty() && !g.id.is_empty())
            .map(|g| SpecGroup {
                kind: g.kind.clone(),
                id: g.id.clone(),
            });
        source.capabilities = row.capabilities.clone();
        source.risk = row.risk.clone();
        source.planned_paths = row.planned_paths.clone();
        source.drift = row.drift.as_ref().map(|d| SpecDrift {
            planned_untouched: d.planned_untouched,
            touched_unplanned: d.touched_unplanned,
            edited_with_implementation: d.edited_with_implementation,
        });
        source.requirement_refs = row
            .requirements
            .iter()
            .map(|r| RequirementRef {
                id: r.id.clone(),
                state: r.state.clone(),
                scenarios: r.scenarios,
                capability: r.capability.clone(),
            })
            .collect();
        source.task_refs = row
            .tasks
            .iter()
            .map(|t| TaskRef {
                id: t.id.clone(),
                done: t.done,
                story: t.story.clone(),
                parallel: t.parallel,
                status: t.status.clone(),
            })
            .collect();
        source.counts = counts_of(&row);
        if with_text {
            source.requirement_bodies = row
                .requirements
                .iter()
                .filter_map(|r| {
                    r.text.as_ref().map(|t| BodyRef {
                        id: r.id.clone(),
                        md: t.clone(),
                    })
                })
                .collect();
            source.task_bodies = row
                .tasks
                .iter()
                .filter_map(|t| {
                    t.text.as_ref().map(|x| BodyRef {
                        id: t.id.clone(),
                        md: x.clone(),
                    })
                })
                .collect();
            source.summary_md = row.summary.clone();
            source.tracker_refs = row.tracker_refs.clone();
            docs = row
                .docs
                .iter()
                .filter(|d| !d.md.is_empty())
                .map(|d| (d.path.clone(), d.md.clone()))
                .collect();
        }
    }
    Some(ResolvedUnit {
        slug,
        title: row.title,
        primary: row.primary,
        first_commit: row.first_commit,
        first_at: row.first_at,
        last_commit: row.last_commit,
        last_at: row.last_at,
        paths: row.paths,
        record_sha: sha,
        source,
        docs,
    })
}

fn counts_of(row: &Row) -> SpecCounts {
    let mut c = SpecCounts::default();
    for r in &row.requirements {
        c.req_total += 1;
        match r.state.as_str() {
            "added" => c.req_added += 1,
            "modified" => c.req_modified += 1,
            "removed" => c.req_removed += 1,
            "renamed" => c.req_renamed += 1,
            "open" => c.req_open += 1,
            _ => {}
        }
        c.scenarios += r.scenarios;
    }
    let mut stories: BTreeSet<&str> = BTreeSet::new();
    let mut story_status: BTreeMap<String, u32> = BTreeMap::new();
    for t in &row.tasks {
        c.tasks_total += 1;
        if t.done {
            c.tasks_done += 1;
        }
        if t.parallel {
            c.tasks_parallel += 1;
        }
        if let Some(s) = t.story.as_deref().filter(|s| !s.is_empty()) {
            stories.insert(s);
        }
        if let Some(st) = t.status.as_deref().filter(|s| !s.trim().is_empty()) {
            *story_status
                .entry(normalize_story_status(st).to_string())
                .or_insert(0) += 1;
        }
    }
    c.stories = stories.len() as u32;
    c.story_status = story_status;
    c
}

/// Parse JSON Lines into units. A blank or malformed line is skipped (one bad
/// row must not cost the rest); duplicates by slug keep the first.
pub fn parse_rows(stdout: &str, with_text: bool) -> Vec<ResolvedUnit> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Row>(line) else {
            tracing::debug!("zavet specs: skipped a malformed row");
            continue;
        };
        if let Some(unit) = map_row(row, with_text) {
            if seen.insert(unit.slug.clone()) {
                out.push(unit);
            }
        }
    }
    out
}

#[derive(Debug, Default, Deserialize)]
struct VersionJson {
    #[serde(default)]
    specs_json: u64,
    #[serde(default)]
    spec_sources_json: u64,
}

/// What the installed plugin says it can do (`zavet version --json`). One
/// probe answers both flags; a field an older plugin does not emit is `false`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Caps {
    /// `zavet specs --json` at [`SPECS_JSON_VERSION`].
    specs_json: bool,
    /// `zavet spec-sources --json` at [`SPEC_SOURCES_JSON_VERSION`].
    spec_sources_json: bool,
}

/// The capabilities the plugin at `bin` advertises. Unparseable output is "no
/// capabilities", not an error: the plugin answered, it just knows nothing.
fn probe_caps(runner: &dyn SpecRunner, bin: &Path, repo: &Path) -> Result<Caps, RunError> {
    let out = runner.run(bin, repo, &["version", "--json"], RUN_TIMEOUT)?;
    Ok(serde_json::from_str::<VersionJson>(out.trim())
        .map(|v| Caps {
            specs_json: v.specs_json >= SPECS_JSON_VERSION,
            spec_sources_json: v.spec_sources_json >= SPEC_SOURCES_JSON_VERSION,
        })
        .unwrap_or_default())
}

/// What the cached capability answer was read under.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CapStamp {
    bin: SystemTime,
    registry: Option<SystemTime>,
}

/// [`probe_caps`], remembered per binary. The capabilities are a property of
/// the installed plugin, not of the repo, so one `version --json` per (binary
/// path, binary mtime, `installed_plugins.json` mtime) answers every resolve —
/// units and rule sources alike — until the plugin is upgraded or reinstalled.
/// A failed probe is never cached (a timeout says nothing about the plugin),
/// and neither is a binary whose mtime cannot be read.
fn plugin_caps(runner: &dyn SpecRunner, bin: &Path, repo: &Path) -> Result<Caps, RunError> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (CapStamp, Caps)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let stamp = runner.stamp(bin).map(|bin_mtime| CapStamp {
        bin: bin_mtime,
        registry: crate::config::home_dir()
            .ok()
            .and_then(|h| runner.stamp(&installed_plugins_path(&h))),
    });
    if let Some(stamp) = &stamp {
        if let Some((seen, caps)) = lock_cache(cache).get(bin) {
            if seen == stamp {
                return Ok(*caps);
            }
        }
    }
    let caps = probe_caps(runner, bin, repo)?;
    if let Some(stamp) = stamp {
        lock_cache(cache).insert(bin.to_path_buf(), (stamp, caps));
    }
    Ok(caps)
}

/// Resolve the repo's source-backed units through the plugin at `plugin_root`.
///
/// `with_text` adds `--with-text`: requirement/task text, summaries and
/// documents come back and are mapped into the full-tier fields. Callers use
/// it only for the consent-gated full-tier sync.
pub fn resolve_units(
    runner: &dyn SpecRunner,
    plugin_root: &Path,
    repo: &Path,
    with_text: bool,
) -> Result<Vec<ResolvedUnit>, ResolveError> {
    let bin = plugin_bin(plugin_root);
    match plugin_caps(runner, &bin, repo) {
        Ok(c) if c.specs_json => {}
        Ok(_) => return Err(ResolveError::Unsupported),
        Err(e) => return Err(ResolveError::Run(e)),
    }
    let args: Vec<&str> = if with_text {
        vec!["specs", "--json", "--with-text"]
    } else {
        vec!["specs", "--json"]
    };
    let out = runner
        .run(&bin, repo, &args, RUN_TIMEOUT)
        .map_err(ResolveError::Run)?;
    Ok(parse_rows(&out, with_text))
}

/// [`resolve_units`] through the real plugin and the real runner, with the
/// plugin located by [`plugin_root_cached`].
pub fn resolve_units_system(
    repo: &Path,
    with_text: bool,
) -> Result<Vec<ResolvedUnit>, ResolveError> {
    let root = plugin_root_cached().ok_or(ResolveError::NoPlugin)?;
    resolve_units(&SystemSpecRunner, &root, repo, with_text)
}

// ---------------------------------------------------------------------------
// Rule sources — a linked framework's standing-rule files (metadata only)
// ---------------------------------------------------------------------------
//
// A framework linked by a `spec-source:` line also brings its standing rules:
// the Spec Kit constitution, OpenSpec's `config.yaml` context/rules (and the
// legacy `project.md`), BMAD's `AGENTS.md` block (and the legacy
// `project-context.md`). The plugin derives them from the declarations and
// reports them as `"type":"rule-source"` rows of `zavet spec-sources --json`.
// dira keeps their METADATA locally — kind, path, blob oid, version, dates,
// rule count, state — and never their text: the rules reach an agent through
// the plugin's own session context, and nothing here crosses any wire until
// the contract carries it (DIRASH-0040, under DIRASH-0039).

/// The `spec-sources --json` row schema this build understands (`"v"` on each
/// row, `"spec_sources_json"` in `zavet version --json`).
pub const SPEC_SOURCES_JSON_VERSION: u64 = 1;
/// Longest `version`, `ratified` or `lastAmended` kept, in characters.
pub const MAX_RULE_META_CHARS: usize = 64;
/// Longest `reason` kept, in characters. Longer than the other scalars because
/// it is the one human sentence (`deprecated by OpenSpec: move it into …`) and
/// it is shown, never keyed on.
pub const MAX_RULE_REASON_CHARS: usize = 200;
/// Longest rule-source `path` accepted; a longer one drops the row.
pub const MAX_RULE_PATH_CHARS: usize = 512;

/// Which framework file a rule source is. Closed: anything the plugin grows
/// later lands in [`RuleSourceKind::Other`] until dira learns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RuleSourceKind {
    /// `.specify/memory/constitution.md`.
    SpeckitConstitution,
    /// `openspec/config.yaml` `context:` / `rules:`.
    OpenspecConfig,
    /// `openspec/project.md` (legacy).
    OpenspecProjectMd,
    /// The `<!-- bmad:context -->` block of `AGENTS.md`.
    BmadAgentsBlock,
    /// `_bmad-output/project-context.md` (legacy).
    BmadProjectContext,
    Other,
}

impl RuleSourceKind {
    pub const ALL: &'static [RuleSourceKind] = &[
        RuleSourceKind::SpeckitConstitution,
        RuleSourceKind::OpenspecConfig,
        RuleSourceKind::OpenspecProjectMd,
        RuleSourceKind::BmadAgentsBlock,
        RuleSourceKind::BmadProjectContext,
        RuleSourceKind::Other,
    ];

    /// The plugin's word for the kind (what the store keeps).
    pub fn as_str(self) -> &'static str {
        match self {
            RuleSourceKind::SpeckitConstitution => "speckit-constitution",
            RuleSourceKind::OpenspecConfig => "openspec-config",
            RuleSourceKind::OpenspecProjectMd => "openspec-project-md",
            RuleSourceKind::BmadAgentsBlock => "bmad-agents-block",
            RuleSourceKind::BmadProjectContext => "bmad-project-context",
            RuleSourceKind::Other => OTHER_BUCKET,
        }
    }

    /// Parse the plugin's (or the store's) word, folded like every closed
    /// vocabulary here; unknown words are `Other`.
    pub fn parse(raw: &str) -> Self {
        parse_closed(raw, Self::ALL, Self::as_str, RuleSourceKind::Other)
    }

    /// Short human name: `Spec Kit constitution`, `OpenSpec config`, ….
    pub fn label(self) -> &'static str {
        match self {
            RuleSourceKind::SpeckitConstitution => "Spec Kit constitution",
            RuleSourceKind::OpenspecConfig => "OpenSpec config",
            RuleSourceKind::OpenspecProjectMd => "OpenSpec project.md",
            RuleSourceKind::BmadAgentsBlock => "BMAD project context",
            RuleSourceKind::BmadProjectContext => "BMAD project-context.md",
            RuleSourceKind::Other => "framework rules",
        }
    }
}

/// A rule source's state as the plugin reports it. Closed, like the kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RuleSourceState {
    /// Parsed and in agent context.
    Ok,
    /// Still carries template placeholders or an unresolved Sync Impact Report.
    Unratified,
    /// A file its framework has deprecated in favour of another.
    Legacy,
    /// Present but unparseable (`reason` says why).
    Unresolved,
    /// An override path that does not exist.
    FileMissing,
    /// Turned off in `.zavet/config`.
    Off,
    Other,
}

impl RuleSourceState {
    pub const ALL: &'static [RuleSourceState] = &[
        RuleSourceState::Ok,
        RuleSourceState::Unratified,
        RuleSourceState::Legacy,
        RuleSourceState::Unresolved,
        RuleSourceState::FileMissing,
        RuleSourceState::Off,
        RuleSourceState::Other,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            RuleSourceState::Ok => "ok",
            RuleSourceState::Unratified => "unratified",
            RuleSourceState::Legacy => "legacy",
            RuleSourceState::Unresolved => "unresolved",
            RuleSourceState::FileMissing => "file-missing",
            RuleSourceState::Off => "off",
            RuleSourceState::Other => OTHER_BUCKET,
        }
    }

    /// Parse the plugin's word (`file_missing`, ` OK ` fold in); unknown words
    /// are `Other`.
    pub fn parse(raw: &str) -> Self {
        parse_closed(raw, Self::ALL, Self::as_str, RuleSourceState::Other)
    }
}

/// The member of a closed vocabulary `raw` names after [`fold_word`], else
/// `other`. `other` itself is never matched by its word, so it cannot be
/// smuggled in as anything but the fallback it already is.
fn parse_closed<T: Copy + PartialEq>(
    raw: &str,
    all: &[T],
    word: fn(T) -> &'static str,
    other: T,
) -> T {
    let k = fold_word(raw);
    all.iter()
        .copied()
        .find(|v| *v != other && word(*v) == k)
        .unwrap_or(other)
}

/// One `"type":"rule-source"` line of `zavet spec-sources --json`. As with
/// [`Row`], every field is optional and unknown fields are ignored. There is
/// deliberately no text field: the plugin emits none, and one it grew later
/// would be dropped here.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct RuleRow {
    #[serde(deserialize_with = "null_default")]
    v: u64,
    #[serde(rename = "type", deserialize_with = "null_default")]
    ty: String,
    #[serde(deserialize_with = "null_default")]
    kind: String,
    #[serde(deserialize_with = "null_default")]
    path: String,
    #[serde(deserialize_with = "null_default")]
    source: String,
    oid: Option<String>,
    version: Option<String>,
    ratified: Option<String>,
    last_amended: Option<String>,
    rule_count: Option<u32>,
    #[serde(deserialize_with = "null_default")]
    state: String,
    reason: Option<String>,
}

/// A framework rule file, as the installed plugin reports it. Metadata only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleSource {
    pub kind: RuleSourceKind,
    /// Repo-relative, `/`-separated.
    pub path: String,
    /// Set by a `rule-source: <kind> <path>` override rather than the default.
    pub overridden: bool,
    /// Working-tree blob oid (`git hash-object`); `None` when absent.
    pub oid: Option<String>,
    pub version: Option<String>,
    pub ratified: Option<String>,
    pub last_amended: Option<String>,
    pub rule_count: Option<u32>,
    pub state: RuleSourceState,
    pub reason: Option<String>,
}

/// A rule-source path dira will store: repo-relative, `/`-separated, no `..`
/// or `.` segments, no control characters, not too long. Anything else drops
/// the row (the plugin already refuses such paths; this is the second lock).
fn usable_rule_path(p: &str) -> bool {
    !p.is_empty()
        && p.chars().count() <= MAX_RULE_PATH_CHARS
        && !p.starts_with('/')
        && !p.contains('\\')
        && !p.chars().any(char::is_control)
        && !p
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
}

fn map_rule_row(row: RuleRow) -> Option<RuleSource> {
    if row.ty != "rule-source" || row.v != SPEC_SOURCES_JSON_VERSION {
        return None;
    }
    if !usable_rule_path(&row.path) {
        return None;
    }
    Some(RuleSource {
        kind: RuleSourceKind::parse(&row.kind),
        path: row.path,
        overridden: row.source == "override",
        oid: sanitize_scalar(row.oid.as_deref(), MAX_RULE_META_CHARS),
        version: sanitize_scalar(row.version.as_deref(), MAX_RULE_META_CHARS),
        ratified: sanitize_scalar(row.ratified.as_deref(), MAX_RULE_META_CHARS),
        last_amended: sanitize_scalar(row.last_amended.as_deref(), MAX_RULE_META_CHARS),
        rule_count: row.rule_count,
        state: RuleSourceState::parse(&row.state),
        reason: sanitize_scalar(row.reason.as_deref(), MAX_RULE_REASON_CHARS),
    })
}

/// Parse `spec-sources --json` output into rule sources. Spec-source rows
/// (`"type":"spec-source"`) are ignored — dira still reads its declarations
/// from `.zavet/config` itself, and those rows could later replace that parse.
/// A blank or malformed line is skipped; duplicates by `(kind, path)` keep the
/// first. The result is sorted by `(kind, path)`.
pub fn parse_rule_rows(stdout: &str) -> Vec<RuleSource> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<RuleRow>(line) else {
            tracing::debug!("zavet spec-sources: skipped a malformed row");
            continue;
        };
        if let Some(r) = map_rule_row(row) {
            if seen.insert((r.kind, r.path.clone())) {
                out.push(r);
            }
        }
    }
    out.sort_by(|a, b| (a.kind, &a.path).cmp(&(b.kind, &b.path)));
    out
}

/// What standing rules linking a framework kind brings into agent context, in
/// words — `None` for a framework that has none (superpowers). Pure, so the
/// onboarding prompt and its `--print` plan can both say it without I/O.
pub fn rule_files_of(kind: &str) -> Option<&'static str> {
    match kind {
        "speckit" => Some("the Spec Kit constitution"),
        "openspec" => Some("OpenSpec's config.yaml context and rules"),
        "bmad" => Some("the BMAD project context"),
        _ => None,
    }
}

/// Resolve the repo's framework rule sources through the plugin at
/// `plugin_root`: `zavet spec-sources --json`, gated on `spec_sources_json`
/// (an older plugin is [`ResolveError::Unsupported`] and is never asked).
/// The same runner, timeout, output cap and capability cache as
/// [`resolve_units`].
pub fn resolve_rule_sources(
    runner: &dyn SpecRunner,
    plugin_root: &Path,
    repo: &Path,
) -> Result<Vec<RuleSource>, ResolveError> {
    let bin = plugin_bin(plugin_root);
    match plugin_caps(runner, &bin, repo) {
        Ok(c) if c.spec_sources_json => {}
        Ok(_) => return Err(ResolveError::Unsupported),
        Err(e) => return Err(ResolveError::Run(e)),
    }
    let out = runner
        .run(&bin, repo, &["spec-sources", "--json"], RUN_TIMEOUT)
        .map_err(ResolveError::Run)?;
    Ok(parse_rule_rows(&out))
}

/// [`resolve_rule_sources`] through the real plugin and the real runner, with
/// the plugin located by [`plugin_root_cached`].
pub fn resolve_rule_sources_system(repo: &Path) -> Result<Vec<RuleSource>, ResolveError> {
    let root = plugin_root_cached().ok_or(ResolveError::NoPlugin)?;
    resolve_rule_sources(&SystemSpecRunner, &root, repo)
}

/// Each declared framework's default rule-file paths (the plugin's map,
/// mirrored): Spec Kit's constitution, OpenSpec's `config.yaml` and legacy
/// `project.md` under its root, BMAD's legacy `project-context.md` under its
/// root. `AGENTS.md` (BMAD's live block) is not listed: it exists in most
/// repos whether or not it carries the block, so it says nothing on its own.
pub fn rule_default_paths(cfg: &crate::zavet::ZavetConfig) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |p: String| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    for d in &cfg.spec_sources {
        match d.kind.as_str() {
            "speckit" => push(".specify/memory/constitution.md".into()),
            "openspec" => {
                push(format!("{}/config.yaml", d.root));
                push(format!("{}/project.md", d.root));
            }
            "bmad" => push(format!("{}/project-context.md", d.root)),
            _ => {}
        }
    }
    out
}

/// The repo-relative paths whose change makes the rule sources worth
/// re-resolving: the config and `AGENTS.md` always, [`rule_default_paths`],
/// and the `stored` rule paths already on record. Override paths are not
/// derivable from the declarations (dira does not parse `rule-source:` lines)
/// — the config edit that sets one is itself a trigger, and from then on the
/// path is a stored one. Built once per sweep; a touched path triggers when it
/// is a member.
pub fn rule_trigger_paths(cfg: &crate::zavet::ZavetConfig, stored: &[String]) -> HashSet<String> {
    let mut out: HashSet<String> = [".zavet/config", "AGENTS.md"]
        .into_iter()
        .map(String::from)
        .collect();
    out.extend(rule_default_paths(cfg));
    out.extend(stored.iter().cloned());
    out
}

/// The kinds a repo's `.zavet/config` declares, deduplicated, in file order.
pub fn declared_kinds(cfg: &crate::zavet::ZavetConfig) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for d in &cfg.spec_sources {
        if !out.contains(&d.kind) {
            out.push(d.kind.clone());
        }
    }
    out
}

/// Whether `path` (repo-relative, `/`-separated) lies under a declared source
/// root, or is the repo's `.zavet/config`.
pub fn path_triggers_resolve(cfg: &crate::zavet::ZavetConfig, path: &str) -> bool {
    if path == ".zavet/config" {
        return true;
    }
    cfg.spec_sources.iter().any(|d| {
        path == d.root
            || path
                .strip_prefix(d.root.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// A framework found in a repo and where its units live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedFramework {
    pub kind: &'static str,
    /// Repo-relative, `/`-separated source root — what a `spec-source:` line
    /// for this framework would name.
    pub root: String,
}

/// Which spec-driven frameworks `root` carries, and the source root of each.
/// Mirrors the plugin's own `sdlc_detect`: `openspec/`, `.specify/` with
/// `specs/`, `_bmad/` (its root is the configured output folder, see
/// [`bmad_store`]), `docs/superpowers/`.
///
/// Read-only (DIRASH-0029): directory `stat`s, plus at most one small read of
/// `_bmad/custom/config.toml` when BMAD's default output folder is absent.
/// Nothing is written and nothing is spawned.
pub fn detect_frameworks(root: &Path) -> Vec<DetectedFramework> {
    let mut out = Vec::new();
    let mut found = |kind: &'static str, root: &str| {
        out.push(DetectedFramework {
            kind,
            root: root.to_string(),
        })
    };
    if root.join("openspec").is_dir() {
        found("openspec", "openspec");
    }
    if root.join(".specify").is_dir() && root.join("specs").is_dir() {
        found("speckit", "specs");
    }
    if root.join("_bmad").is_dir() {
        found("bmad", &bmad_store(root, BMAD_DEFAULT_OUTPUT));
    }
    if root.join("docs").join("superpowers").is_dir() {
        found("superpowers", "docs/superpowers");
    }
    out
}

/// BMAD's default `output_folder`.
const BMAD_DEFAULT_OUTPUT: &str = "_bmad-output";
/// Longest `_bmad/custom/config.toml` read; a real one is a few lines.
const BMAD_CONFIG_MAX_BYTES: u64 = 64 * 1024;

/// BMAD keeps its artifacts in `output_folder` (default `_bmad-output`), set in
/// `_bmad/custom/config.toml` as `"{project-root}/<dir>"`. The repo-relative
/// directory to read: `declared` when it exists, else the configured folder
/// when that exists, else `declared`. A line-for-line port of the plugin's
/// `bmad_store` (zavet `bin/zavet`), so dira and the plugin never disagree
/// about where a repo's BMAD units are.
pub fn bmad_store(root: &Path, declared: &str) -> String {
    if root.join(declared).is_dir() {
        return declared.to_string();
    }
    match bmad_output_folder(root) {
        Some(dir) if root.join(&dir).is_dir() => dir,
        _ => declared.to_string(),
    }
}

/// The first `output_folder = …` line of `_bmad/custom/config.toml`, cleaned
/// the way the plugin's awk cleans it, or `None` when the file is absent, is a
/// symlink (never followed) or names nothing usable (empty, absolute, `..`).
fn bmad_output_folder(root: &Path) -> Option<String> {
    use std::io::Read;
    let path = root.join("_bmad").join("custom").join("config.toml");
    if !std::fs::symlink_metadata(&path).ok()?.file_type().is_file() {
        return None;
    }
    let mut text = String::new();
    std::fs::File::open(&path)
        .ok()?
        .take(BMAD_CONFIG_MAX_BYTES)
        .read_to_string(&mut text)
        .ok()?;
    let line = text.lines().find(|l| {
        l.trim_start_matches([' ', '\t'])
            .strip_prefix("output_folder")
            .is_some_and(|rest| rest.trim_start_matches([' ', '\t']).starts_with('='))
    })?;
    let value = line.split_once('=')?.1.trim_start_matches([' ', '\t']);
    // A trailing ` # comment` (whitespace then `#`), before quotes are dropped.
    let comment = value
        .char_indices()
        .find(|&(i, c)| c == '#' && value[..i].ends_with([' ', '\t']))
        .map(|(i, _)| i);
    let value = match comment {
        Some(i) => value[..i].trim_end_matches([' ', '\t']),
        None => value,
    };
    let value: String = value.chars().filter(|c| !matches!(c, '"' | '\'')).collect();
    let value = value.trim_end_matches([' ', '\t', '\r']);
    let value = value
        .strip_prefix("{project-root}")
        .map_or(value, |v| v.strip_prefix('/').unwrap_or(v));
    let value = value.strip_prefix("./").unwrap_or(value);
    let value = value.trim_end_matches('/');
    (!value.is_empty() && !value.starts_with('/') && !value.contains(".."))
        .then(|| value.to_string())
}

/// Whether `s` is a plausible spec slug: 1..=200 chars of
/// `[A-Za-z0-9._:/-]`. Used to keep `dira start --spec` a slug and nothing else
/// — the value rides the attestation wire (`SessionRollup.spec`, D-0001), so it
/// must never carry prose.
pub fn is_valid_spec_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '-'))
}

/// Human name of a framework kind.
pub fn kind_label(kind: &str) -> &str {
    match kind {
        "openspec" => "OpenSpec",
        "speckit" => "Spec Kit",
        "bmad" => "BMAD",
        "superpowers" => "superpowers",
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Session attribution (capture-time resolution)
// ---------------------------------------------------------------------------

/// Resolve the spec a captured session works under, from facts only — never a
/// guess. Order: (1) the first `Spec:` trailer slug of the captured commits
/// that exactly matches a known spec slug; (2) an exact branch match — the
/// branch is `<kind>/<id>` or its final segment equals a known *source* id,
/// and exactly one known source spec matches. Anything else is `None`.
pub fn resolve_session_spec(
    trailer_values: &[Vec<String>],
    branch: Option<&str>,
    known_slugs: &BTreeSet<String>,
) -> Option<String> {
    for slugs in trailer_values {
        for s in slugs {
            if known_slugs.contains(s) {
                return Some(s.clone());
            }
        }
    }
    let branch = branch?.trim();
    if branch.is_empty() {
        return None;
    }
    // `<kind>/<id>` as the whole branch name.
    if let Some((kind, id)) = branch.split_once('/') {
        let slug = format!("{kind}:{id}");
        if known_slugs.contains(&slug) {
            return Some(slug);
        }
    }
    // Final segment equals a known source id — unique match only.
    let last = branch.rsplit('/').next().unwrap_or(branch);
    let mut matches = known_slugs
        .iter()
        .filter(|s| s.split_once(':').is_some_and(|(_, id)| id == last));
    let first = matches.next()?;
    if matches.next().is_some() {
        return None; // ambiguous — never guess
    }
    Some(first.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Scripted runner: maps the joined argv to a canned result and records
    /// every call.
    struct Fake {
        replies: Vec<(&'static str, Result<String, RunError>)>,
        calls: RefCell<Vec<String>>,
        /// Fake mtimes (seconds); `None` = unreadable, i.e. not cacheable.
        bin_stamp: std::cell::Cell<Option<u64>>,
        registry_stamp: std::cell::Cell<Option<u64>>,
    }

    impl Fake {
        fn new(replies: Vec<(&'static str, Result<String, RunError>)>) -> Self {
            Self {
                replies,
                calls: RefCell::new(Vec::new()),
                bin_stamp: std::cell::Cell::new(None),
                registry_stamp: std::cell::Cell::new(Some(1)),
            }
        }
    }

    impl SpecRunner for Fake {
        fn stamp(&self, path: &Path) -> Option<SystemTime> {
            let secs = if path.ends_with("installed_plugins.json") {
                self.registry_stamp.get()
            } else {
                self.bin_stamp.get()
            }?;
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
        }

        fn run(
            &self,
            bin: &Path,
            _cwd: &Path,
            args: &[&str],
            _timeout: Duration,
        ) -> Result<String, RunError> {
            let key = args.join(" ");
            self.calls
                .borrow_mut()
                .push(format!("{} {key}", bin.display()));
            self.replies
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, r)| r.clone())
                .unwrap_or(Err(RunError::Spawn("unstubbed".into())))
        }
    }

    const GOOD: &str = r##"{"v":1,"slug":"openspec:add-csv-import","kind":"openspec","id":"add-csv-import","root":"openspec","frameworkVersion":"0.9.1","title":"Add CSV import","origin":"external","verified":false,"state":"active","nativeState":"in-progress","group":null,"capabilities":["csv-import"],"risk":null,"primary":"openspec/changes/add-csv-import/proposal.md","files":[{"path":"openspec/changes/add-csv-import/proposal.md","oid":"b1"},{"path":"openspec/changes/add-csv-import/tasks.md","oid":"a2"}],"firstCommit":"c1","firstAt":"2026-09-01T00:00:00Z","lastCommit":"c2","lastAt":"2026-09-02T00:00:00Z","closedOn":null,"paths":["src/import/**"],"plannedPaths":["src/import/csv.ts"],"requirements":[{"id":"R1","state":"added","scenarios":2,"capability":"csv-import","text":"REQ TEXT"},{"id":"R2","state":"open","scenarios":0}],"tasks":[{"id":"1.1","done":true,"story":"US1","parallel":false,"status":null,"text":"TASK TEXT"},{"id":"1.2","done":false,"parallel":true}],"drift":{"plannedUntouched":1,"touchedUnplanned":0,"editedWithImplementation":false},"trackerRefs":["GH#123"],"summary":"WHY TEXT","docs":[{"path":"openspec/changes/add-csv-import/proposal.md","md":"# Proposal"}],"unresolved":null,"extraFieldFromTheFuture":42}"##;
    const NATIVE: &str = r#"{"v":1,"slug":"capture-pipeline","kind":"zavet","id":"capture-pipeline","root":".zavet/specs","primary":".zavet/specs/capture-pipeline.md","state":"active"}"#;
    const UNRESOLVED: &str = r#"{"v":1,"slug":"bmad:epic-x","kind":"bmad","id":"epic-x","root":"_bmad-output","state":"active","primary":"_bmad-output/epic-x.md","files":[{"path":"_bmad-output/epic-x.md","oid":"z"}],"requirements":[{"id":"R1","state":"declared"}],"tasks":[{"id":"t","done":true}],"unresolved":"tickets.toml is malformed"}"#;

    fn root() -> PathBuf {
        PathBuf::from("/plugin")
    }

    fn version_ok() -> (&'static str, Result<String, RunError>) {
        (
            "version --json",
            Ok(r#"{"version":"1.8.0","specs_json":1}"#.into()),
        )
    }

    #[test]
    fn good_rows_map_to_metadata_only_by_default() {
        let out = format!("{GOOD}\n{NATIVE}\n");
        let units = parse_rows(&out, false);
        assert_eq!(units.len(), 1, "the native `zavet` row is skipped");
        let u = &units[0];
        assert_eq!(u.slug, "openspec:add-csv-import");
        assert_eq!(u.title.as_deref(), Some("Add CSV import"));
        assert_eq!(u.primary, "openspec/changes/add-csv-import/proposal.md");
        assert_eq!(u.source.state, "active");
        assert_eq!(u.source.native_state.as_deref(), Some("in-progress"));
        assert_eq!(u.source.framework_version.as_deref(), Some("0.9.1"));
        assert_eq!(u.source.capabilities, vec!["csv-import"]);
        assert_eq!(u.source.planned_paths, vec!["src/import/csv.ts"]);
        let c = &u.source.counts;
        assert_eq!((c.req_total, c.req_added, c.req_open), (2, 1, 1));
        assert_eq!((c.scenarios, c.tasks_total, c.tasks_done), (2, 2, 1));
        assert_eq!((c.tasks_parallel, c.stories), (1, 1));
        assert_eq!(u.source.requirement_refs[0].id, "R1");
        assert_eq!(u.source.task_refs[0].story.as_deref(), Some("US1"));
        assert_eq!(u.source.drift.as_ref().unwrap().planned_untouched, 1);
        // No text without the explicit request — even though the row carried it.
        assert!(u.source.requirement_bodies.is_empty());
        assert!(u.source.task_bodies.is_empty());
        assert!(u.source.summary_md.is_none());
        assert!(u.source.tracker_refs.is_empty());
        assert!(u.docs.is_empty());
    }

    #[test]
    fn with_text_fills_the_full_tier_fields() {
        let units = parse_rows(GOOD, true);
        let s = &units[0].source;
        assert_eq!(s.requirement_bodies.len(), 1);
        assert_eq!(s.requirement_bodies[0].md, "REQ TEXT");
        assert_eq!(s.task_bodies[0].md, "TASK TEXT");
        assert_eq!(s.summary_md.as_deref(), Some("WHY TEXT"));
        assert_eq!(s.tracker_refs, vec!["GH#123"]);
        assert_eq!(units[0].docs[0].1, "# Proposal");
    }

    #[test]
    fn an_unresolved_row_without_a_primary_is_kept_with_a_fallback_path() {
        let row = r#"{"v":1,"slug":"openspec:orphan","kind":"openspec","id":"orphan","root":"openspec","state":"unresolved","primary":null,"files":[],"unresolved":"proposal.md missing"}"#;
        let units = parse_rows(row, false);
        assert_eq!(units.len(), 1, "unresolved units must reach the store");
        assert_eq!(units[0].primary, "openspec");
        assert_eq!(units[0].source.state, "unresolved");
    }

    #[test]
    fn unresolved_rows_keep_identity_and_carry_no_claims() {
        let units = parse_rows(UNRESOLVED, false);
        assert_eq!(units.len(), 1);
        let s = &units[0].source;
        assert_eq!(s.state, "unresolved");
        assert_eq!(s.unit_files.len(), 1);
        assert!(s.requirement_refs.is_empty() && s.task_refs.is_empty());
        assert_eq!(s.counts, SpecCounts::default());
    }

    #[test]
    fn malformed_and_foreign_rows_are_skipped_without_losing_the_rest() {
        let out = format!(
            "not json at all\n\n{{\"v\":2,\"kind\":\"openspec\",\"id\":\"x\",\"primary\":\"p\"}}\n\
             {{\"v\":1,\"slug\":\"openspec:wrong\",\"kind\":\"openspec\",\"id\":\"x\",\"primary\":\"p\"}}\n\
             {{\"v\":1,\"kind\":\"openspec\",\"id\":\"\",\"primary\":\"p\"}}\n{GOOD}\n{GOOD}\n"
        );
        let units = parse_rows(&out, false);
        assert_eq!(units.len(), 1, "only the one good (de-duplicated) row");
    }

    #[test]
    fn record_sha_is_order_independent_and_tracks_content() {
        let a = vec![
            UnitFile {
                path: "a.md".into(),
                oid: "1".into(),
            },
            UnitFile {
                path: "b.md".into(),
                oid: "2".into(),
            },
        ];
        let mut b = a.clone();
        b.reverse();
        assert_eq!(record_sha(&a), record_sha(&b));
        let mut c = a.clone();
        c[1].oid = "3".into();
        assert_ne!(record_sha(&a), record_sha(&c));
        assert_eq!(record_sha(&a).len(), 32);
    }

    #[test]
    fn resolve_runs_version_then_specs_and_requests_text_only_when_asked() {
        let fake = Fake::new(vec![
            version_ok(),
            ("specs --json", Ok(GOOD.into())),
            ("specs --json --with-text", Ok(GOOD.into())),
        ]);
        let units = resolve_units(&fake, &root(), Path::new("/repo"), false).unwrap();
        assert_eq!(units.len(), 1);
        resolve_units(&fake, &root(), Path::new("/repo"), true).unwrap();
        let calls = fake.calls.borrow();
        // The binary is joined with the platform separator (`\` on Windows).
        let bin = root().join("bin").join("zavet").display().to_string();
        assert!(calls[0].starts_with(&format!("{bin} ")), "{calls:?}");
        assert!(
            calls
                .iter()
                .all(|c| !c.contains(".zavet/bin") && !c.contains(".zavet\\bin")),
            "{calls:?}"
        );
        assert_eq!(calls[1], format!("{bin} specs --json"));
        assert_eq!(calls[3], format!("{bin} specs --json --with-text"));
    }

    #[test]
    fn a_plugin_without_specs_json_is_unsupported_and_never_asked_for_specs() {
        for ver in [r#"{"version":"1.7.1"}"#, r#"{"specs_json":0}"#, "garbage"] {
            let fake = Fake::new(vec![("version --json", Ok(ver.into()))]);
            let err = resolve_units(&fake, &root(), Path::new("/repo"), false).unwrap_err();
            assert_eq!(err, ResolveError::Unsupported, "{ver}");
            assert_eq!(fake.calls.borrow().len(), 1, "specs must not be invoked");
        }
    }

    #[test]
    fn a_timeout_or_failure_fails_open() {
        let fake = Fake::new(vec![version_ok(), ("specs --json", Err(RunError::Timeout))]);
        assert_eq!(
            resolve_units(&fake, &root(), Path::new("/r"), false).unwrap_err(),
            ResolveError::Run(RunError::Timeout)
        );
        let fake = Fake::new(vec![("version --json", Err(RunError::Failed(127)))]);
        assert!(resolve_units(&fake, &root(), Path::new("/r"), false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn the_system_runner_kills_a_wedged_plugin_at_the_deadline() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("zavet");
        std::fs::write(&bin, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let t0 = Instant::now();
        let r = SystemSpecRunner.run(&bin, dir.path(), &["specs"], Duration::from_millis(200));
        assert_eq!(r, Err(RunError::Timeout));
        assert!(t0.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[cfg(unix)]
    #[test]
    fn a_reader_that_never_finishes_is_a_timeout_not_an_empty_success() {
        // The script exits at once but leaves a grandchild holding stdout open,
        // so the reader cannot reach EOF: the answer is incomplete, and an
        // empty `Ok("")` would read as "the repo has no units".
        let dir = tempfile::tempdir().unwrap();
        let bin = script(dir.path(), "slow", "sleep 30 &\necho partial");
        let t0 = Instant::now();
        let r = SystemSpecRunner.run(&bin, dir.path(), &[], Duration::from_secs(10));
        assert_eq!(r, Err(RunError::Timeout));
        assert!(t0.elapsed() < Duration::from_secs(8));
    }

    #[cfg(unix)]
    #[test]
    fn output_over_the_cap_is_a_distinct_error_never_a_silently_cut_answer() {
        let dir = tempfile::tempdir().unwrap();
        let n = MAX_OUTPUT_BYTES + 1024;
        let bin = script(
            dir.path(),
            "big",
            &format!("head -c {n} /dev/zero | tr '\\0' 'x'"),
        );
        let r = SystemSpecRunner.run(&bin, dir.path(), &[], Duration::from_secs(20));
        assert_eq!(r, Err(RunError::OutputCap));
        // Exactly at the cap is fine.
        let bin = script(
            dir.path(),
            "exact",
            &format!("head -c {MAX_OUTPUT_BYTES} /dev/zero | tr '\\0' 'x'"),
        );
        let out = SystemSpecRunner
            .run(&bin, dir.path(), &[], Duration::from_secs(20))
            .unwrap();
        assert_eq!(out.len() as u64, MAX_OUTPUT_BYTES);
    }

    #[cfg(unix)]
    #[test]
    fn a_timeout_kills_the_whole_process_group_not_just_the_shell() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("grandchild.pid");
        let bin = script(
            dir.path(),
            "tree",
            &format!("sleep 60 &\necho $! > {}\nwait", pidfile.display()),
        );
        let r = SystemSpecRunner.run(&bin, dir.path(), &[], Duration::from_millis(400));
        assert_eq!(r, Err(RunError::Timeout));
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let alive = || {
            Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        let t0 = Instant::now();
        while alive() && t0.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!alive(), "the grandchild must die with the script");
    }

    #[test]
    fn json_null_scalars_degrade_to_defaults_instead_of_dropping_the_row() {
        let row = r##"{"v":1,"slug":"openspec:n","kind":"openspec","id":"n","root":null,"state":null,"primary":"openspec/changes/n/proposal.md","files":[{"path":"openspec/changes/n/proposal.md","oid":null}],"capabilities":null,"requirements":[{"id":"R1","state":null,"scenarios":null}],"tasks":[{"id":null,"done":null,"parallel":null}],"drift":{"plannedUntouched":null},"docs":[{"path":null,"md":null}],"paths":null}"##;
        let units = parse_rows(row, true);
        assert_eq!(units.len(), 1, "a null scalar must not cost the row");
        let u = &units[0];
        assert_eq!(u.source.state, "unresolved", "empty state falls back");
        assert_eq!(u.source.unit_files[0].oid, "");
        assert_eq!(u.source.counts.req_total, 1);
        assert!(u.docs.is_empty(), "an empty doc body is dropped");
    }

    #[test]
    fn story_status_and_state_keys_are_normalised_to_a_closed_vocabulary() {
        for (raw, want) in [
            ("done", "done"),
            (" In Progress ", "in-progress"),
            ("in_progress", "in-progress"),
            ("REVIEW", "review"),
            ("__proto__", "other"),
            ("constructor", "other"),
            ("weird\u{0}key", "other"),
        ] {
            assert_eq!(normalize_story_status(raw), want, "{raw:?}");
        }
        assert_eq!(normalize_state_bucket("Archived"), "archived");
        assert_eq!(normalize_state_bucket("__proto__"), "other");
        assert_eq!(normalize_state_bucket("draft"), "other");

        let row = r##"{"v":1,"kind":"bmad","id":"e","primary":"p.md","tasks":[{"id":"1","status":"done"},{"id":"2","status":"__proto__"},{"id":"3","status":"Done"},{"id":"4","status":"hasOwnProperty"}]}"##;
        let units = parse_rows(row, false);
        let m = &units[0].source.counts.story_status;
        assert_eq!(m.get("done"), Some(&2));
        assert_eq!(m.get("other"), Some(&2));
        assert_eq!(m.len(), 2, "{m:?}");
    }

    #[test]
    fn native_state_is_capped_and_stripped_of_control_characters() {
        let long = "x".repeat(200);
        let got = sanitize_native_state(&long).unwrap();
        assert_eq!(got.chars().count(), MAX_NATIVE_STATE_CHARS);
        assert_eq!(
            sanitize_native_state("In\u{7} Pro\ngress\u{1b}[0m").as_deref(),
            Some("In Progress[0m")
        );
        assert_eq!(sanitize_native_state("\u{0}\n "), None);
        let row = format!(
            r##"{{"v":1,"kind":"bmad","id":"e","primary":"p.md","nativeState":"{long}"}}"##
        );
        let u = &parse_rows(&row, false)[0];
        assert_eq!(
            u.source.native_state.as_deref().map(|s| s.chars().count()),
            Some(MAX_NATIVE_STATE_CHARS)
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_system_runner_returns_stdout_and_maps_a_failing_exit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let ok = dir.path().join("ok");
        std::fs::write(&ok, "#!/bin/sh\necho hello\n").unwrap();
        let bad = dir.path().join("bad");
        std::fs::write(&bad, "#!/bin/sh\nexit 3\n").unwrap();
        for p in [&ok, &bad] {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let out = SystemSpecRunner
            .run(&ok, dir.path(), &[], Duration::from_secs(5))
            .unwrap();
        assert_eq!(out.trim(), "hello");
        assert_eq!(
            SystemSpecRunner.run(&bad, dir.path(), &[], Duration::from_secs(5)),
            Err(RunError::Failed(3))
        );
    }

    #[test]
    fn path_triggers_cover_declared_roots_and_the_config() {
        let cfg = crate::zavet::parse_config(
            "spec-source: openspec openspec\nspec-source: speckit specs\n",
        );
        assert!(path_triggers_resolve(&cfg, ".zavet/config"));
        assert!(path_triggers_resolve(
            &cfg,
            "openspec/changes/x/proposal.md"
        ));
        assert!(path_triggers_resolve(&cfg, "specs/001-a/spec.md"));
        assert!(!path_triggers_resolve(&cfg, "specs-extra/a.md"));
        assert!(!path_triggers_resolve(&cfg, "src/main.rs"));
        assert_eq!(declared_kinds(&cfg), vec!["openspec", "speckit"]);
    }

    fn known(slugs: &[&str]) -> BTreeSet<String> {
        slugs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn session_spec_prefers_a_trailer_then_an_exact_branch_and_never_guesses() {
        let k = known(&["capture-pipeline", "openspec:add-csv-import", "bmad:epic-x"]);
        // Trailer: first matching slug of the first commit that has one.
        assert_eq!(
            resolve_session_spec(
                &[vec!["nope".into(), "capture-pipeline".into()]],
                Some("openspec/add-csv-import"),
                &k
            )
            .as_deref(),
            Some("capture-pipeline")
        );
        // Branch `<kind>/<id>`.
        assert_eq!(
            resolve_session_spec(&[], Some("openspec/add-csv-import"), &k).as_deref(),
            Some("openspec:add-csv-import")
        );
        // Final segment equals a unique source id.
        assert_eq!(
            resolve_session_spec(&[], Some("feat/epic-x"), &k).as_deref(),
            Some("bmad:epic-x")
        );
        // Substring is not a match.
        assert_eq!(resolve_session_spec(&[], Some("feat/epic-xy"), &k), None);
        assert_eq!(resolve_session_spec(&[], Some("main"), &k), None);
        assert_eq!(resolve_session_spec(&[], None, &k), None);
        // Ambiguous source id across two kinds -> none.
        let amb = known(&["openspec:dup", "speckit:dup"]);
        assert_eq!(resolve_session_spec(&[], Some("feat/dup"), &amb), None);
        // A native slug is never inferred from a branch.
        assert_eq!(
            resolve_session_spec(&[], Some("feat/capture-pipeline"), &k),
            None
        );
    }

    #[test]
    fn a_spec_slug_is_a_slug_and_never_prose() {
        for ok in [
            "capture-pipeline",
            "openspec:add-csv-import",
            "bmad:epic-checkout.v2",
            "superpowers:2026-09-18-design",
        ] {
            assert!(is_valid_spec_slug(ok), "{ok}");
        }
        for bad in [
            "",
            "two words",
            "a\nb",
            "semi;colon",
            "quo\"te",
            &"x".repeat(201),
        ] {
            assert!(!is_valid_spec_slug(bad), "{bad:?}");
        }
    }

    fn version_calls(fake: &Fake) -> usize {
        fake.calls
            .borrow()
            .iter()
            .filter(|c| c.ends_with("version --json"))
            .count()
    }

    /// Each cache test uses its own plugin root: the cache is process-wide and
    /// the tests run in parallel.
    fn cached_fake(root: &str) -> (Fake, PathBuf) {
        let fake = Fake::new(vec![version_ok(), ("specs --json", Ok(String::new()))]);
        fake.bin_stamp.set(Some(100));
        (fake, PathBuf::from(root))
    }

    #[test]
    fn the_capability_is_probed_once_until_the_plugin_changes() {
        let (fake, root) = cached_fake("/cache-hit/plugin");
        for _ in 0..3 {
            resolve_units(&fake, &root, Path::new("/repo"), false).unwrap();
        }
        assert_eq!(version_calls(&fake), 1, "a burst probes the plugin once");
        assert_eq!(
            fake.calls.borrow().len(),
            4,
            "but every resolve lists specs"
        );
    }

    #[test]
    fn a_new_binary_or_registry_mtime_invalidates_the_capability() {
        let (fake, root) = cached_fake("/cache-invalidate/plugin");
        resolve_units(&fake, &root, Path::new("/repo"), false).unwrap();
        // Upgrade in place: same path, new binary mtime.
        fake.bin_stamp.set(Some(200));
        resolve_units(&fake, &root, Path::new("/repo"), false).unwrap();
        assert_eq!(version_calls(&fake), 2);
        // Reinstall: the registry file is rewritten.
        fake.registry_stamp.set(Some(2));
        resolve_units(&fake, &root, Path::new("/repo"), false).unwrap();
        assert_eq!(version_calls(&fake), 3);
        // Unchanged since: cached again.
        resolve_units(&fake, &root, Path::new("/repo"), false).unwrap();
        assert_eq!(version_calls(&fake), 3);
    }

    #[test]
    fn an_unsupported_answer_is_cached_but_a_failed_probe_never_is() {
        // Old plugin: the negative answer is as stable as a positive one.
        let old = Fake::new(vec![(
            "version --json",
            Ok(r#"{"version":"1.0.0"}"#.into()),
        )]);
        old.bin_stamp.set(Some(1));
        let root = PathBuf::from("/cache-unsupported/plugin");
        for _ in 0..2 {
            assert_eq!(
                resolve_units(&old, &root, Path::new("/r"), false).unwrap_err(),
                ResolveError::Unsupported
            );
        }
        assert_eq!(version_calls(&old), 1);
        // A timed-out probe says nothing about the plugin: probe again.
        let flaky = Fake::new(vec![("version --json", Err(RunError::Timeout))]);
        flaky.bin_stamp.set(Some(1));
        let root = PathBuf::from("/cache-failed/plugin");
        for _ in 0..2 {
            assert!(resolve_units(&flaky, &root, Path::new("/r"), false).is_err());
        }
        assert_eq!(version_calls(&flaky), 2);
    }

    #[test]
    fn a_binary_with_no_readable_mtime_is_never_cached() {
        let (fake, root) = cached_fake("/cache-unstamped/plugin");
        fake.bin_stamp.set(None);
        for _ in 0..2 {
            resolve_units(&fake, &root, Path::new("/repo"), false).unwrap();
        }
        assert_eq!(version_calls(&fake), 2);
    }

    #[test]
    fn detect_frameworks_is_a_stat_only_probe() {
        let kinds = |d: &Path| -> Vec<&'static str> {
            detect_frameworks(d).into_iter().map(|f| f.kind).collect()
        };
        let d = tempfile::tempdir().unwrap();
        assert!(kinds(d.path()).is_empty());
        std::fs::create_dir_all(d.path().join("openspec")).unwrap();
        std::fs::create_dir_all(d.path().join("specs")).unwrap();
        // `specs/` alone is not Spec Kit.
        assert_eq!(kinds(d.path()), vec!["openspec"]);
        std::fs::create_dir_all(d.path().join(".specify")).unwrap();
        std::fs::create_dir_all(d.path().join("_bmad")).unwrap();
        std::fs::create_dir_all(d.path().join("docs/superpowers")).unwrap();
        assert_eq!(
            kinds(d.path()),
            vec!["openspec", "speckit", "bmad", "superpowers"]
        );
        let roots: Vec<String> = detect_frameworks(d.path())
            .into_iter()
            .map(|f| f.root)
            .collect();
        assert_eq!(
            roots,
            ["openspec", "specs", "_bmad-output", "docs/superpowers"]
        );
    }

    /// A repo with `_bmad/custom/config.toml` holding `config` and the named
    /// directories created; returns the BMAD root `detect_frameworks` reports.
    fn bmad_root_with(config: Option<&str>, dirs: &[&str]) -> String {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("_bmad/custom")).unwrap();
        if let Some(c) = config {
            std::fs::write(d.path().join("_bmad/custom/config.toml"), c).unwrap();
        }
        for dir in dirs {
            std::fs::create_dir_all(d.path().join(dir)).unwrap();
        }
        detect_frameworks(d.path())
            .into_iter()
            .find(|f| f.kind == "bmad")
            .unwrap()
            .root
    }

    /// Pinned to the plugin's `bmad_store` (zavet `bin/zavet`, tests in
    /// `test/run.sh`): the declared default wins when it exists, else the
    /// configured `output_folder` when it exists, else the default.
    #[test]
    fn bmad_root_follows_the_plugins_output_folder_semantics() {
        let cfg = "[core]\noutput_folder = \"{project-root}/planning-out\"\n";
        // The plugin's own fixture: configured folder exists, default does not.
        assert_eq!(bmad_root_with(Some(cfg), &["planning-out"]), "planning-out");
        // The default directory wins whenever it exists.
        assert_eq!(
            bmad_root_with(Some(cfg), &["planning-out", "_bmad-output"]),
            "_bmad-output"
        );
        // A configured folder that is not on disk falls back to the default.
        assert_eq!(bmad_root_with(Some(cfg), &[]), "_bmad-output");
        // No config at all.
        assert_eq!(bmad_root_with(None, &[]), "_bmad-output");
        // Cleaning: single quotes, `./`, trailing slash, trailing comment.
        assert_eq!(
            bmad_root_with(Some("output_folder = './out/' # where\n"), &["out"]),
            "out"
        );
        // A bare path without the `{project-root}` token.
        assert_eq!(
            bmad_root_with(Some("output_folder=\"docs/bmad\""), &["docs/bmad"]),
            "docs/bmad"
        );
        // First `output_folder` line wins; a longer key does not match.
        assert_eq!(
            bmad_root_with(
                Some("output_folder_x = \"nope\"\noutput_folder = \"a\"\noutput_folder = \"b\"\n"),
                &["a", "b", "nope"]
            ),
            "a"
        );
    }

    #[test]
    fn bmad_output_folder_rejects_unusable_values_and_symlinks() {
        // Absolute and `..` values are refused (the plugin's `case` guard).
        assert_eq!(
            bmad_root_with(Some("output_folder = \"/etc\""), &["etc"]),
            "_bmad-output"
        );
        assert_eq!(
            bmad_root_with(Some("output_folder = \"{project-root}/../x\""), &["../x"]),
            "_bmad-output"
        );
        assert_eq!(
            bmad_root_with(Some("output_folder = \"\"\n"), &[]),
            "_bmad-output"
        );
        #[cfg(unix)]
        {
            // A symlinked config is never followed.
            let d = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(d.path().join("_bmad/custom")).unwrap();
            std::fs::create_dir_all(d.path().join("elsewhere")).unwrap();
            let real = d.path().join("real.toml");
            std::fs::write(&real, "output_folder = \"elsewhere\"\n").unwrap();
            std::os::unix::fs::symlink(&real, d.path().join("_bmad/custom/config.toml")).unwrap();
            let root = detect_frameworks(d.path()).remove(0).root;
            assert_eq!(root, "_bmad-output");
        }
    }

    // -- rule sources -------------------------------------------------------

    const RULE_OK: &str = r#"{"v":1,"type":"rule-source","kind":"speckit-constitution","path":".specify/memory/constitution.md","source":"default","enabled":true,"oid":"bda08d2a","version":"1.2.0","ratified":"2026-06-01","lastAmended":"2026-09-10","ruleCount":2,"origin":"external","verified":false,"state":"ok","reason":null,"text":"NEVER KEPT"}"#;
    const RULE_LEGACY: &str = r#"{"v":1,"type":"rule-source","kind":"openspec-project-md","path":"openspec/project.md","source":"default","enabled":true,"oid":"1d4e","version":null,"ratified":null,"lastAmended":null,"ruleCount":null,"state":"legacy","reason":"deprecated by OpenSpec: move it into openspec/config.yaml context"}"#;
    const SOURCE_ROW: &str = r#"{"v":1,"type":"spec-source","kind":"speckit","root":"specs","globs":[],"state":"ok","units":3,"unresolved":1}"#;

    fn version_rules_ok() -> (&'static str, Result<String, RunError>) {
        (
            "version --json",
            Ok(r#"{"version":"1.9.0","specs_json":1,"spec_sources_json":1}"#.into()),
        )
    }

    #[test]
    fn rule_rows_map_and_spec_source_rows_are_ignored() {
        let out = format!("{SOURCE_ROW}\n{RULE_LEGACY}\n{RULE_OK}\n");
        let rules = parse_rule_rows(&out);
        assert_eq!(rules.len(), 2, "the spec-source row is not a rule source");
        let c = &rules[0];
        assert_eq!(
            c.kind,
            RuleSourceKind::SpeckitConstitution,
            "sorted by kind"
        );
        assert_eq!(c.path, ".specify/memory/constitution.md");
        assert_eq!(c.state, RuleSourceState::Ok);
        assert!(!c.overridden);
        assert_eq!(c.version.as_deref(), Some("1.2.0"));
        assert_eq!(c.ratified.as_deref(), Some("2026-06-01"));
        assert_eq!(c.last_amended.as_deref(), Some("2026-09-10"));
        assert_eq!(c.rule_count, Some(2));
        assert_eq!(c.oid.as_deref(), Some("bda08d2a"));
        let l = &rules[1];
        assert_eq!(l.kind, RuleSourceKind::OpenspecProjectMd);
        assert_eq!(l.state, RuleSourceState::Legacy);
        assert_eq!(l.rule_count, None);
        assert!(l
            .reason
            .as_deref()
            .unwrap()
            .starts_with("deprecated by OpenSpec"));
    }

    #[test]
    fn unknown_rule_kinds_and_states_land_in_other() {
        let row = r#"{"v":1,"type":"rule-source","kind":"kiro-steering","path":".kiro/steering.md","state":"brand-new","enabled":true}"#;
        let r = &parse_rule_rows(row)[0];
        assert_eq!(r.kind, RuleSourceKind::Other);
        assert_eq!(r.kind.as_str(), "other");
        assert_eq!(r.state, RuleSourceState::Other);
        assert_eq!(
            RuleSourceState::parse("file_missing"),
            RuleSourceState::FileMissing
        );
        assert_eq!(RuleSourceState::parse(" OK "), RuleSourceState::Ok);
        assert_eq!(RuleSourceState::parse("__proto__"), RuleSourceState::Other);
        assert_eq!(RuleSourceKind::parse("other"), RuleSourceKind::Other);
        for k in RuleSourceKind::ALL {
            assert_eq!(RuleSourceKind::parse(k.as_str()), *k, "round-trips");
        }
        for s in RuleSourceState::ALL {
            assert_eq!(RuleSourceState::parse(s.as_str()), *s, "round-trips");
        }
    }

    #[test]
    fn foreign_versions_bad_paths_and_duplicates_are_dropped() {
        let out = format!(
            "garbage\n\
             {{\"v\":2,\"type\":\"rule-source\",\"kind\":\"openspec-config\",\"path\":\"openspec/config.yaml\",\"state\":\"ok\"}}\n\
             {{\"v\":1,\"type\":\"rule-source\",\"kind\":\"openspec-config\",\"path\":\"/etc/passwd\",\"state\":\"ok\"}}\n\
             {{\"v\":1,\"type\":\"rule-source\",\"kind\":\"openspec-config\",\"path\":\"../x.yaml\",\"state\":\"ok\"}}\n\
             {{\"v\":1,\"type\":\"rule-source\",\"kind\":\"openspec-config\",\"path\":\"\",\"state\":\"ok\"}}\n\
             {{\"v\":1,\"kind\":\"openspec-config\",\"path\":\"openspec/config.yaml\",\"state\":\"ok\"}}\n\
             {RULE_OK}\n{RULE_OK}\n"
        );
        let rules = parse_rule_rows(&out);
        assert_eq!(rules.len(), 1, "{rules:?}");
        assert_eq!(rules[0].kind, RuleSourceKind::SpeckitConstitution);
    }

    #[test]
    fn rule_scalars_are_capped_and_stripped_and_off_is_disabled() {
        let long = "v".repeat(300);
        let row = format!(
            r#"{{"v":1,"type":"rule-source","kind":"openspec-config","path":"openspec/config.yaml","state":"off","version":"{long}","reason":"{long}","lastAmended":"2026\u0007-01"}}"#
        );
        let r = &parse_rule_rows(&row)[0];
        assert_eq!(
            r.version.as_ref().unwrap().chars().count(),
            MAX_RULE_META_CHARS
        );
        assert_eq!(
            r.reason.as_ref().unwrap().chars().count(),
            MAX_RULE_REASON_CHARS
        );
        assert_eq!(r.last_amended.as_deref(), Some("2026-01"));
        assert_eq!(r.state, RuleSourceState::Off);
    }

    #[test]
    fn rule_resolve_runs_spec_sources_json_on_the_plugin_binary() {
        let fake = Fake::new(vec![
            version_rules_ok(),
            (
                "spec-sources --json",
                Ok(format!("{SOURCE_ROW}\n{RULE_OK}\n")),
            ),
        ]);
        let rules = resolve_rule_sources(&fake, &root(), Path::new("/repo")).unwrap();
        assert_eq!(rules.len(), 1);
        let bin = root().join("bin").join("zavet").display().to_string();
        let calls = fake.calls.borrow();
        assert_eq!(calls[1], format!("{bin} spec-sources --json"));
        assert!(calls.iter().all(|c| !c.contains(".zavet")), "{calls:?}");
    }

    #[test]
    fn a_plugin_without_spec_sources_json_is_never_asked_for_rule_sources() {
        for ver in [
            r#"{"version":"1.8.0","specs_json":1}"#,
            r#"{"spec_sources_json":0}"#,
            "garbage",
        ] {
            let fake = Fake::new(vec![("version --json", Ok(ver.into()))]);
            let err = resolve_rule_sources(&fake, &root(), Path::new("/repo")).unwrap_err();
            assert_eq!(err, ResolveError::Unsupported, "{ver}");
            assert_eq!(fake.calls.borrow().len(), 1, "spec-sources must not run");
        }
    }

    #[test]
    fn a_rule_resolve_timeout_cap_or_failure_fails_open() {
        for e in [RunError::Timeout, RunError::OutputCap, RunError::Failed(1)] {
            let fake = Fake::new(vec![
                version_rules_ok(),
                ("spec-sources --json", Err(e.clone())),
            ]);
            assert_eq!(
                resolve_rule_sources(&fake, &root(), Path::new("/r")).unwrap_err(),
                ResolveError::Run(e)
            );
        }
        let fake = Fake::new(vec![("version --json", Err(RunError::Timeout))]);
        assert_eq!(
            resolve_rule_sources(&fake, &root(), Path::new("/r")).unwrap_err(),
            ResolveError::Run(RunError::Timeout)
        );
    }

    #[test]
    fn one_cached_probe_serves_both_units_and_rule_sources() {
        let fake = Fake::new(vec![
            version_rules_ok(),
            ("specs --json", Ok(String::new())),
            ("spec-sources --json", Ok(String::new())),
        ]);
        fake.bin_stamp.set(Some(7));
        let root = PathBuf::from("/cache-shared/plugin");
        resolve_units(&fake, &root, Path::new("/repo"), false).unwrap();
        resolve_rule_sources(&fake, &root, Path::new("/repo")).unwrap();
        resolve_rule_sources(&fake, &root, Path::new("/repo")).unwrap();
        assert_eq!(version_calls(&fake), 1);
    }

    #[test]
    fn rule_triggers_cover_config_agents_md_defaults_and_stored_paths() {
        let cfg = crate::zavet::parse_config(
            "spec-source: openspec openspec\nspec-source: speckit specs\nspec-source: bmad _bmad-output\n",
        );
        let stored = vec!["docs/rules/constitution.md".to_string()];
        let triggers = rule_trigger_paths(&cfg, &stored);
        for p in [
            ".zavet/config",
            "AGENTS.md",
            ".specify/memory/constitution.md",
            "openspec/config.yaml",
            "openspec/project.md",
            "_bmad-output/project-context.md",
            "docs/rules/constitution.md",
        ] {
            assert!(triggers.contains(p), "{p}");
        }
        for p in [
            "src/main.rs",
            "openspec/changes/x/proposal.md",
            "sub/AGENTS.md",
        ] {
            assert!(!triggers.contains(p), "{p}");
        }
        // Undeclared frameworks contribute no default path.
        let none = crate::zavet::parse_config("");
        assert!(!rule_trigger_paths(&none, &[]).contains(".specify/memory/constitution.md"));
    }
}
