//! Project & identity resolution. Maps a working directory to a canonical repo
//! ref and the git identity working in it, by shelling out to `git`.
//!
//! Canonicalization makes `git@github.com:Org/Repo.git` and
//! `https://github.com/Org/Repo` resolve to the same `github.com/org/repo`, so a
//! repo is one identity regardless of clone URL.

use std::path::Path;
use std::process::Command;

/// What we resolved about a working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// Canonical repo ref like `github.com/acme/api`, or `None` if not a git repo
    /// with a recognizable remote (work is still captured, just low-assurance).
    pub project: Option<String>,
    pub identity_email: Option<String>,
    pub identity_name: Option<String>,
}

/// Resolve a directory to its project + identity. Never errors — an unresolvable
/// directory simply yields `None` fields.
pub fn resolve(cwd: &Path) -> Resolved {
    let toplevel = git(cwd, &["rev-parse", "--show-toplevel"]);
    let root = toplevel.as_deref().map(Path::new).unwrap_or(cwd);

    let project = git(root, &["remote", "get-url", "origin"])
        .as_deref()
        .and_then(canonicalize_remote);

    Resolved {
        project,
        identity_email: git(root, &["config", "user.email"]),
        identity_name: git(root, &["config", "user.name"]),
    }
}

/// The repo toplevel for `cwd`, if inside a git work tree.
pub fn toplevel(cwd: &Path) -> Option<std::path::PathBuf> {
    git(cwd, &["rev-parse", "--show-toplevel"]).map(std::path::PathBuf::from)
}

/// Raw trailer pairs for each of `shas`, in one batched `git log --no-walk`
/// call: `%x1e`-separated records of `SHA%x1f<trailers, unfolded>`. Commits
/// with no trailers return an empty list. Best-effort — a git failure yields
/// an empty map.
pub fn commit_trailers(root: &Path, shas: &[String]) -> Vec<(String, Vec<(String, String)>)> {
    if shas.is_empty() {
        return Vec::new();
    }
    let mut args: Vec<&str> = vec![
        "log",
        "--no-walk=unsorted",
        "--no-color",
        "--pretty=format:%H%x1f%(trailers:only,unfold)%x1e",
    ];
    args.extend(shas.iter().map(String::as_str));
    let Some(out) = git(root, &args) else {
        return Vec::new();
    };
    out.split('\u{1e}')
        .filter_map(|record| {
            let record = record.trim_start_matches(['\n', '\r']);
            let (sha, block) = record.split_once('\u{1f}')?;
            let sha = sha.trim();
            if sha.is_empty() {
                return None;
            }
            Some((sha.to_string(), crate::zavet::parse_trailer_block(block)))
        })
        .collect()
}

/// Repo-relative paths added/modified/renamed-to by `sha` (deleted files
/// excluded — the zavet layers never parse a deletion). `--root` covers the
/// initial commit. One call serves every zavet path filter (see
/// `crate::zavet::{is_decision_path, is_spec_path}`) — don't diff-tree twice.
pub fn changed_paths(root: &Path, sha: &str) -> Vec<String> {
    let Some(out) = git(
        root,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-status",
            "-r",
            "--root",
            sha,
        ],
    ) else {
        return Vec::new();
    };
    out.lines()
        .filter_map(|line| {
            let (status, path) = line.split_once('\t')?;
            // A(dded)/M(odified)/R(enamed; path is "old\tnew" — take the new).
            let path = match status.chars().next()? {
                'A' | 'M' => path,
                'R' | 'C' => path.rsplit('\t').next()?,
                _ => return None,
            };
            Some(path.to_string())
        })
        .collect()
}

/// Shas of commits after `since_sha` (exclusive) that touch any of
/// `pathspecs` — zavet's spec-staleness primitive, computed at query time
/// because no table stores per-commit paths. Globs go through git's own
/// `:(glob)` magic so `**` behaves like the spec authored it. Best-effort:
/// a git failure (unknown sha, unborn branch) yields an empty list.
pub fn commits_touching_since(root: &Path, since_sha: &str, pathspecs: &[String]) -> Vec<String> {
    if pathspecs.is_empty() {
        return Vec::new();
    }
    let range = format!("{since_sha}..HEAD");
    let mut args: Vec<String> = vec!["log".into(), "--format=%H".into(), range, "--".into()];
    args.extend(pathspecs.iter().map(|p| format!(":(glob){p}")));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    git(root, &args)
        .map(|out| out.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Recent-window activity for the knowledge repo-stats snapshot: distinct
/// non-`.zavet/` paths touched inside the rolling window, plus the shas of the
/// non-merge commits that touched at least one such path ("non-trivial").
/// One `git log --name-only` pass; `.zavet/` files are knowledge, not code,
/// so they count toward neither the coverage denominator nor triviality.
#[derive(Debug, Clone, Default)]
pub struct KnowledgeActivity {
    pub paths: Vec<String>,
    pub nontrivial_commits: Vec<String>,
}

pub fn knowledge_activity(root: &Path, window_days: u32) -> KnowledgeActivity {
    let since = format!("--since={window_days}.days");
    let out = match git(
        root,
        &[
            "log",
            "--no-merges",
            &since,
            "--format=%x01%H",
            "--name-only",
        ],
    ) {
        Some(out) => out,
        None => return KnowledgeActivity::default(),
    };
    let mut paths: Vec<String> = Vec::new();
    let mut seen_paths = std::collections::HashSet::new();
    let mut commits: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    let mut current_counts = false;
    for line in out.lines() {
        if let Some(sha) = line.strip_prefix('\u{1}') {
            if let (Some(sha), true) = (current.take(), current_counts) {
                commits.push(sha);
            }
            current = Some(sha.to_string());
            current_counts = false;
            continue;
        }
        let line = line.trim();
        if line.is_empty() || line.starts_with(".zavet/") {
            continue;
        }
        current_counts = true;
        if seen_paths.insert(line.to_string()) {
            paths.push(line.to_string());
        }
    }
    if let (Some(sha), true) = (current, current_counts) {
        commits.push(sha);
    }
    KnowledgeActivity {
        paths,
        nontrivial_commits: commits,
    }
}

/// Author date (RFC 3339) of the commit that first ADDED anything under
/// `pathspec`, or `None` when the path has no such commit (or this isn't a repo).
///
/// Exists to date a repo's adoption of a practice — specifically, when `.zavet/`
/// appeared — so a rolling statistics window can be clamped to the period the
/// practice was actually in force instead of spanning history that predates it
/// (issue #67).
///
/// `git log` lists newest-first, so the ADDING commit is the LAST line. Taking it
/// that way rather than with `--reverse --max-count=1` is deliberate: git applies
/// `--max-count` before reversing, so that pairing returns the most recent add,
/// which is the opposite of what's wanted whenever a path was deleted and
/// re-added.
pub fn first_commit_date(root: &Path, pathspec: &str) -> Option<String> {
    let out = git(
        root,
        &["log", "--diff-filter=A", "--format=%aI", "--", pathspec],
    )?;
    // `git` already trims its output and maps an empty result to `None`, so the
    // last line of what reaches here is always a real date.
    out.lines().last().map(|s| s.trim().to_string())
}

/// Distinct non-`.zavet/` paths touched inside the rolling window AND matching
/// one of `pathspecs` (git `:(glob)` semantics — the same dialect the
/// staleness query uses). The knowledge coverage numerator.
pub fn paths_touched_since_days(
    root: &Path,
    window_days: u32,
    pathspecs: &[String],
) -> Vec<String> {
    if pathspecs.is_empty() {
        return Vec::new();
    }
    let since = format!("--since={window_days}.days");
    let mut args: Vec<String> = vec![
        "log".into(),
        "--no-merges".into(),
        since,
        "--format=".into(),
        "--name-only".into(),
        "--".into(),
    ];
    args.extend(pathspecs.iter().map(|p| format!(":(glob){p}")));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut seen = std::collections::HashSet::new();
    let mut out_paths = Vec::new();
    if let Some(out) = git(root, &args) {
        for line in out.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(".zavet/") {
                continue;
            }
            if seen.insert(line.to_string()) {
                out_paths.push(line.to_string());
            }
        }
    }
    out_paths
}

/// The content of `path` as of `sha` (`git show sha:path`).
pub fn show_blob(root: &Path, sha: &str, path: &str) -> Option<String> {
    git(root, &["show", &format!("{sha}:{path}")])
}

/// The blob object id of `path` as of `sha` — zavet's `content_hash`.
pub fn blob_oid(root: &Path, sha: &str, path: &str) -> Option<String> {
    git(root, &["rev-parse", &format!("{sha}:{path}")])
}

/// A commit captured from `git log`, ready to record locally and ship as an
/// [`dira_contract::ArtifactRef`]. Metadata only — no diff or file contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedCommit {
    pub sha: String,
    /// RFC 3339 author date.
    pub authored_at: Option<String>,
    /// Commit author email (`%ae`). Used by the cloud to anchor to an identity.
    pub author_email: Option<String>,
    /// Commit author name (`%an`). Kept local-only — never shipped on the wire.
    pub author_name: Option<String>,
    /// Commit subject (first line).
    pub message: String,
    pub additions: u64,
    pub deletions: u64,
    /// `git patch-id --stable` — a stable hash of the change that survives
    /// rebase/amend/cherry-pick. Shipped so the cloud can re-anchor rewritten
    /// commits. `None` when git can't produce one (empty diff, merge).
    pub patch_id: Option<String>,
}

/// One touched file's post-image blob: the repo-relative path and the git object
/// id git already stores for it at the session tip. Metadata only — the blob SHA
/// names the content git holds, it is never the content itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TouchedBlob {
    pub path: String,
    pub blob: String,
}

/// Squash-resilient anchoring signals computed over the *cumulative* session diff
/// `merge-base(@{upstream}, HEAD)..HEAD` (not per individual commit).
///
/// A squash merge collapses N commits into one whose combined diff matches none of
/// the per-commit patch-ids — but it equals the cumulative diff, and its tree keeps
/// the same post-image blob SHAs. Shipping these cumulative signals lets the cloud
/// re-anchor a squashed/rewritten commit: exact `session_change_id` first, then
/// blob-set / path-set overlap as graceful degradation. All fields are best-effort
/// and metadata only — never a diff or file contents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSignals {
    /// `git patch-id --stable` over the cumulative diff. Equals a squash-merge
    /// commit's patch-id when the base hasn't moved. `None` on a merge commit,
    /// detached HEAD, a missing upstream base, an empty diff, or git failure.
    pub change_id: Option<String>,
    /// Repo-relative paths the session changed (the union over the cumulative
    /// range). `None` when the range can't be resolved.
    pub touched_paths: Option<Vec<String>>,
    /// Per touched path, its post-image blob SHA at the session tip. Deleted paths
    /// (no post-image) are omitted. `None` when the range can't be resolved.
    pub blobs: Option<Vec<TouchedBlob>>,
}

/// The current HEAD sha of the repo at `root`, or `None` if not resolvable.
pub fn head_sha(root: &Path) -> Option<String> {
    git(root, &["rev-parse", "HEAD"])
}

/// Compute the squash-resilient [`SessionSignals`] for the repo at `root` over the
/// cumulative range `merge-base(@{upstream}, HEAD)..HEAD`.
///
/// Best-effort: a detached HEAD, no configured upstream, a HEAD that is a merge
/// commit, an empty cumulative diff, or any git failure yields the corresponding
/// `None` fields (or an all-`None` [`SessionSignals`]) rather than an error. Uses
/// the same synchronous git-subprocess pattern as [`patch_id`]; the diff is piped
/// between git processes and never retained.
pub fn session_signals(root: &Path) -> SessionSignals {
    // A merge commit (≥2 parents) has no single cumulative author-diff to anchor —
    // mirror the per-commit `patch_id` "None on merge" rule for the whole session.
    if is_merge_head(root) {
        return SessionSignals::default();
    }
    // The session base is the fork point with the tracked upstream. No upstream
    // (or a detached HEAD where `@{upstream}` won't resolve) ⇒ no cumulative range.
    let Some(base) = merge_base_upstream(root) else {
        return SessionSignals::default();
    };
    let head = match head_sha(root) {
        Some(h) => h,
        None => return SessionSignals::default(),
    };
    // No commits since the base (HEAD == base) ⇒ nothing this session changed.
    if base == head {
        return SessionSignals::default();
    }

    let range = format!("{base}..{head}");
    let touched = touched_paths(root, &base, &head);
    let blobs = touched
        .as_ref()
        .map(|paths| collect_blobs(root, &head, paths));
    SessionSignals {
        change_id: cumulative_change_id(root, &range),
        touched_paths: touched,
        blobs,
    }
}

/// True when HEAD is a merge commit (two or more parents). A merge has no single
/// cumulative diff to anchor, so the session signals are dropped.
fn is_merge_head(root: &Path) -> bool {
    // `rev-list --parents -n1 HEAD` prints "<sha> <parent1> <parent2?>..."; >2
    // tokens means ≥2 parents.
    match git(root, &["rev-list", "--parents", "-n", "1", "HEAD"]) {
        Some(line) => line.split_whitespace().count() > 2,
        None => false,
    }
}

/// The merge-base of the tracked upstream and HEAD — the session's fork point.
/// `None` when there is no configured upstream or git can't resolve it (detached
/// HEAD, a branch with no tracking ref).
fn merge_base_upstream(root: &Path) -> Option<String> {
    git(root, &["merge-base", "@{upstream}", "HEAD"])
}

/// `git patch-id --stable` over the cumulative diff for `range` (`<base>..<head>`).
/// `git diff <range> | git patch-id --stable`, returning the leading patch-id
/// token. `None` on an empty diff or any git failure. Metadata only — the diff is
/// piped between git processes and never retained.
fn cumulative_change_id(root: &Path, range: &str) -> Option<String> {
    use std::io::Write;
    use std::process::Stdio;

    let diff = git_command()
        .arg("-C")
        .arg(root)
        .args(["diff", "--no-color", range])
        .output()
        .ok()?;
    if !diff.status.success() || diff.stdout.is_empty() {
        return None;
    }

    let mut child = git_command()
        .arg("-C")
        .arg(root)
        .args(["patch-id", "--stable"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    {
        let mut stdin = child.stdin.take()?;
        stdin.write_all(&diff.stdout).ok()?;
    }
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
}

/// Repo-relative paths changed across `<base>..<head>` (`git diff --name-only`).
/// `None` on git failure; an empty (but successful) result is `Some(vec![])`.
fn touched_paths(root: &Path, base: &str, head: &str) -> Option<Vec<String>> {
    let out = git_command()
        .arg("-C")
        .arg(root)
        .args(["diff", "--name-only", "--no-color", base, head])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// Resolve each touched path's post-image blob SHA at `head` via
/// `git rev-parse <head>:<path>`. Deleted paths (no object at `head`) are dropped,
/// so blob-set overlap stays an honest content-identity signal.
fn collect_blobs(root: &Path, head: &str, paths: &[String]) -> Vec<TouchedBlob> {
    paths
        .iter()
        .filter_map(|path| {
            let spec = format!("{head}:{path}");
            git(root, &["rev-parse", &spec]).map(|blob| TouchedBlob {
                path: path.clone(),
                blob,
            })
        })
        .collect()
}

/// The current branch name (`git rev-parse --abbrev-ref HEAD`), or `None`. A
/// detached HEAD yields `"HEAD"`, which we treat as no branch.
pub fn current_branch(root: &Path) -> Option<String> {
    match git(root, &["rev-parse", "--abbrev-ref", "HEAD"]) {
        Some(b) if b == "HEAD" => None,
        other => other,
    }
}

/// Walk `git log` at `root` and return the captured commits, newest first.
///
/// `range` is `None` for a bounded backfill (the most recent `limit` commits on
/// first sight of a repo) or `Some("<sha>..HEAD")` to capture only what landed
/// since the last watermark. `--shortstat` gives the additions/deletions; the
/// pretty format uses unit separators (`%x1f`) so a subject with tabs or pipes
/// never confuses parsing.
pub fn log_commits(root: &Path, range: Option<&str>, limit: usize) -> Vec<CapturedCommit> {
    let limit_arg = format!("-{limit}");
    let mut args: Vec<&str> = vec![
        "log",
        &limit_arg,
        "--no-color",
        "--pretty=format:%H%x1f%aI%x1f%ae%x1f%an%x1f%s",
        "--shortstat",
    ];
    if let Some(r) = range {
        args.push(r);
    }
    let mut commits = match git(root, &args) {
        Some(out) => parse_git_log(&out),
        None => Vec::new(),
    };
    // patch-id needs the diff, which `git log` can't emit inline — compute it per
    // commit. It's a stable id of the change (survives rebase), shipped so the
    // cloud can re-anchor a commit whose SHA was rewritten out of the remote.
    for c in commits.iter_mut() {
        c.patch_id = patch_id(root, &c.sha);
    }
    commits
}

/// `git patch-id --stable` for one commit: `git diff-tree -p <sha> | git patch-id
/// --stable`, returning the leading patch-id token. `None` when git produces no
/// diff (merge/empty) or either process fails. Metadata only — the diff is piped
/// between git processes and never retained.
fn patch_id(root: &Path, sha: &str) -> Option<String> {
    use std::io::Write;
    use std::process::Stdio;

    let diff = git_command()
        .arg("-C")
        .arg(root)
        .args(["diff-tree", "-p", "--no-color", "--root", sha])
        .output()
        .ok()?;
    if !diff.status.success() || diff.stdout.is_empty() {
        return None;
    }

    let mut child = git_command()
        .arg("-C")
        .arg(root)
        .args(["patch-id", "--stable"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Close stdin (drop) after writing so `patch-id` sees EOF, then collect output.
    {
        let mut stdin = child.stdin.take()?;
        stdin.write_all(&diff.stdout).ok()?;
    }
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    // Output is "<patch-id> <commit-id>\n" — the first token is the patch id.
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
}

/// Parse `git log --pretty=format:%H%x1f%aI%x1f%ae%x1f%an%x1f%s --shortstat`
/// output. Header lines carry `\x1f`-separated
/// `sha / author-date / author-email / author-name / subject`; subject is last so
/// an embedded `\x1f` in it can't shift the earlier fields. The optional shortstat
/// line that follows (`N files changed, A insertions(+), D deletions(-)`) attaches
/// its counts to the commit just opened.
fn parse_git_log(out: &str) -> Vec<CapturedCommit> {
    let mut commits: Vec<CapturedCommit> = Vec::new();
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.contains('\u{1f}') {
            // splitn(5) keeps the subject (last field) intact even if it embeds a
            // `\x1f`; short/malformed lines just leave later fields defaulted.
            let mut parts = line.splitn(5, '\u{1f}');
            let sha = parts.next().unwrap_or("");
            let authored_at = parts.next().unwrap_or("");
            let author_email = parts.next().unwrap_or("");
            let author_name = parts.next().unwrap_or("");
            let message = parts.next().unwrap_or("");
            // An empty string maps to `None` (same convention as authored_at).
            let opt = |s: &str| (!s.is_empty()).then(|| s.to_string());
            commits.push(CapturedCommit {
                sha: sha.to_string(),
                authored_at: opt(authored_at),
                author_email: opt(author_email),
                author_name: opt(author_name),
                message: message.to_string(),
                additions: 0,
                deletions: 0,
                patch_id: None,
            });
        } else if line.contains("changed") {
            if let Some(cur) = commits.last_mut() {
                cur.additions = extract_count(line, "insertion");
                cur.deletions = extract_count(line, "deletion");
            }
        }
    }
    commits
}

/// Pull the integer preceding `noun` (e.g. `"insertion"`) out of a shortstat line.
fn extract_count(line: &str, noun: &str) -> u64 {
    let idx = match line.find(noun) {
        Some(i) => i,
        None => return 0,
    };
    line[..idx]
        .split_whitespace()
        .next_back()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// Build a `git` [`Command`], platform-adjusted. Every git spawn in this module
/// must go through here: `dirad` runs console-less on windows (spawned with
/// `CREATE_NO_WINDOW`), and a console subprocess launched from a console-less
/// parent without that same flag makes Windows allocate — and briefly flash — a
/// brand-new console window. These spawns fire from the capture path on every
/// idle-ticker sweep, so an unflagged spawn is a visible window strobe in the
/// user's session.
fn git_command() -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new("git");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Run a git command in `dir`, returning trimmed stdout on success.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = git_command().arg("-C").arg(dir).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Normalize any git remote URL to `host/owner/repo` (lowercased), independent of
/// SSH vs HTTPS and `.git` suffix.
pub fn canonicalize_remote(url: &str) -> Option<String> {
    let url = url.trim();
    let stripped = url.strip_suffix(".git").unwrap_or(url);

    // scp-like: git@github.com:owner/repo
    let body = if let Some(rest) = stripped.strip_prefix("git@") {
        rest.replacen(':', "/", 1)
    } else if let Some(rest) = stripped.strip_prefix("ssh://") {
        rest.trim_start_matches("git@").to_string()
    } else if let Some(rest) = stripped.strip_prefix("https://") {
        rest.to_string()
    } else {
        stripped.strip_prefix("http://")?.to_string()
    };

    // Drop any userinfo and port, keep host/owner/repo, require at least 3 parts.
    let body = body.split('@').next_back().unwrap_or(&body);
    let host_and_path = body.splitn(2, '/').collect::<Vec<_>>();
    if host_and_path.len() != 2 {
        return None;
    }
    let host = host_and_path[0]
        .split(':')
        .next()
        .unwrap_or(host_and_path[0]);
    let path = host_and_path[1].trim_matches('/');
    if host.is_empty() || path.split('/').count() < 2 {
        return None;
    }
    Some(format!("{host}/{path}").to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::{
        canonicalize_remote, first_commit_date, knowledge_activity, paths_touched_since_days,
    };

    #[test]
    fn ssh_scp_form() {
        assert_eq!(
            canonicalize_remote("git@github.com:Acme/Api.git").as_deref(),
            Some("github.com/acme/api")
        );
    }

    #[test]
    fn https_form() {
        assert_eq!(
            canonicalize_remote("https://github.com/Acme/Api").as_deref(),
            Some("github.com/acme/api")
        );
    }

    #[test]
    fn ssh_protocol_form() {
        assert_eq!(
            canonicalize_remote("ssh://git@github.com/acme/api.git").as_deref(),
            Some("github.com/acme/api")
        );
    }

    #[test]
    fn https_and_ssh_canonicalize_equal() {
        assert_eq!(
            canonicalize_remote("git@github.com:acme/api.git"),
            canonicalize_remote("https://github.com/acme/api.git")
        );
    }

    #[test]
    fn non_url_is_none() {
        assert_eq!(canonicalize_remote("not a url"), None);
    }

    use super::parse_git_log;

    #[test]
    fn parses_log_with_shortstat() {
        // Two commits; the first has a full shortstat, the second only insertions.
        // Header shape: sha / author-date / author-email / author-name / subject.
        let out = "abc123\u{1f}2026-06-27T10:00:00+00:00\u{1f}dev@example.com\u{1f}Dev One\u{1f}feat: add thing\n \
                   2 files changed, 10 insertions(+), 3 deletions(-)\n\
                   def456\u{1f}2026-06-27T09:00:00+00:00\u{1f}two@example.com\u{1f}Dev Two\u{1f}fix: a tab\there\n \
                   1 file changed, 1 insertion(+)\n";
        let commits = parse_git_log(out);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].sha, "abc123");
        assert_eq!(commits[0].message, "feat: add thing");
        assert_eq!(commits[0].additions, 10);
        assert_eq!(commits[0].deletions, 3);
        assert_eq!(
            commits[0].authored_at.as_deref(),
            Some("2026-06-27T10:00:00+00:00")
        );
        assert_eq!(commits[0].author_email.as_deref(), Some("dev@example.com"));
        assert_eq!(commits[0].author_name.as_deref(), Some("Dev One"));
        // Subject with an embedded tab survives (unit-separator framing).
        assert_eq!(commits[1].message, "fix: a tab\there");
        assert_eq!(commits[1].author_email.as_deref(), Some("two@example.com"));
        assert_eq!(commits[1].author_name.as_deref(), Some("Dev Two"));
        assert_eq!(commits[1].additions, 1);
        assert_eq!(commits[1].deletions, 0);
    }

    #[test]
    fn parses_commit_without_shortstat() {
        // An empty commit (no diff) has no shortstat line.
        let out =
            "abc123\u{1f}2026-06-27T10:00:00+00:00\u{1f}dev@example.com\u{1f}Dev One\u{1f}chore: empty\n";
        let commits = parse_git_log(out);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].author_email.as_deref(), Some("dev@example.com"));
        assert_eq!(commits[0].author_name.as_deref(), Some("Dev One"));
        assert_eq!(commits[0].additions, 0);
        assert_eq!(commits[0].deletions, 0);
    }

    #[test]
    fn subject_with_embedded_unit_separator_stays_intact() {
        // A `\x1f` inside the subject must not shift earlier fields — subject is the
        // last `splitn(5)` field, so it absorbs the extra separator.
        let out =
            "abc123\u{1f}2026-06-27T10:00:00+00:00\u{1f}dev@example.com\u{1f}Dev One\u{1f}weird\u{1f}subject\n";
        let commits = parse_git_log(out);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].author_email.as_deref(), Some("dev@example.com"));
        assert_eq!(commits[0].author_name.as_deref(), Some("Dev One"));
        assert_eq!(commits[0].message, "weird\u{1f}subject");
    }

    #[test]
    fn short_header_line_defaults_missing_fields() {
        // A malformed/short header must never panic; missing fields default to None.
        let out = "abc123\u{1f}2026-06-27T10:00:00+00:00\n";
        let commits = parse_git_log(out);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].sha, "abc123");
        assert_eq!(commits[0].author_email, None);
        assert_eq!(commits[0].author_name, None);
        assert_eq!(commits[0].message, "");
    }

    // --- Squash-resilient session signals -----------------------------------

    use super::{git_command, session_signals, SessionSignals};
    use std::path::Path;

    /// Run a git command in `dir`, panicking on failure (test setup helper).
    fn run_git(dir: &Path, args: &[&str]) {
        run_git_env(dir, args, None);
    }

    /// Like [`run_git`] but with the commit dates pinned, so window/date tests
    /// don't depend on when they run.
    fn run_git_at(dir: &Path, args: &[&str], date: &str) {
        run_git_env(dir, args, Some(date));
    }

    /// Run git with the identity pinned, optionally pinning the commit dates too.
    fn run_git_env(dir: &Path, args: &[&str], date: Option<&str>) {
        let mut cmd = git_command();
        cmd.arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.com");
        if let Some(date) = date {
            cmd.env("GIT_AUTHOR_DATE", date)
                .env("GIT_COMMITTER_DATE", date);
        }
        let status = cmd.output().expect("git runs");
        assert!(
            status.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    /// A bare repo with one dated commit on `main`. No upstream — these tests
    /// only walk local history.
    fn init_plain_repo(tag: &str, date: &str) -> std::path::PathBuf {
        let root = temp_repo_dir(tag);
        run_git(&root, &["init", "-q", "-b", "main"]);
        run_git(&root, &["config", "user.email", "t@example.com"]);
        run_git(&root, &["config", "user.name", "T"]);
        write(&root, "src.rs", "fn main() {}\n");
        run_git(&root, &["add", "."]);
        run_git_at(&root, &["commit", "-q", "-m", "base"], date);
        root
    }

    fn commit_file(root: &Path, rel: &str, body: &str, msg: &str, date: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        run_git(root, &["add", "."]);
        run_git_at(root, &["commit", "-q", "-m", msg], date);
    }

    /// Issue #67 hinges on dating a repo's adoption of `.zavet/`. Nothing did
    /// that before, so this helper — and these three tests — are the first
    /// coverage of it.
    #[test]
    fn first_commit_date_reports_when_a_path_was_added() {
        let root = init_plain_repo("first-add", "2026-05-01T10:00:00+00:00");
        commit_file(
            &root,
            ".zavet/decisions/D-0001.md",
            "# D-0001\n",
            "chore(repo): adopt zavet",
            "2026-06-15T10:00:00+00:00",
        );
        // Later work under the same directory must NOT move the adoption date.
        commit_file(
            &root,
            ".zavet/decisions/D-0002.md",
            "# D-0002\n",
            "chore(repo): another decision",
            "2026-07-20T10:00:00+00:00",
        );

        let got = first_commit_date(&root, ".zavet").expect("dated");
        assert!(
            got.starts_with("2026-06-15"),
            "must report the ADDING commit, not the newest one: {got}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A repo that never adopted zavet has no date — the caller falls back to the
    /// full window rather than clamping to nothing.
    #[test]
    fn first_commit_date_is_none_for_a_path_that_never_existed() {
        let root = init_plain_repo("no-zavet", "2026-05-01T10:00:00+00:00");
        assert_eq!(first_commit_date(&root, ".zavet"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn first_commit_date_is_none_outside_a_repo() {
        let dir = temp_repo_dir("not-a-repo");
        assert_eq!(first_commit_date(&dir, ".zavet"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `knowledge_activity` feeds the capture-ratio denominator and had no test.
    /// The two things it must get right: `.zavet/` files are knowledge, not code,
    /// so they count toward neither paths nor triviality.
    #[test]
    fn knowledge_activity_counts_code_and_ignores_zavet_only_commits() {
        let root = init_plain_repo("activity", "2026-07-01T10:00:00+00:00");
        commit_file(
            &root,
            "src/lib.rs",
            "pub fn a() {}\n",
            "feat(cli): add a",
            "2026-07-02T10:00:00+00:00",
        );
        commit_file(
            &root,
            ".zavet/decisions/D-0001.md",
            "# D-0001\n",
            "chore(repo): decision only",
            "2026-07-03T10:00:00+00:00",
        );

        let a = knowledge_activity(&root, 365);
        assert!(
            a.paths.contains(&"src/lib.rs".to_string()),
            "code paths count: {:?}",
            a.paths
        );
        assert!(
            !a.paths.iter().any(|p| p.starts_with(".zavet/")),
            "zavet paths are knowledge, not the code being covered: {:?}",
            a.paths
        );
        assert_eq!(
            a.nontrivial_commits.len(),
            2,
            "base + the code commit; the zavet-only commit is trivial"
        );

        // The window is a real bound, not decoration.
        assert!(
            knowledge_activity(&root, 1).nontrivial_commits.is_empty(),
            "a 1-day window excludes commits dated weeks ago"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `paths_touched_since_days` is the coverage numerator and also had no test.
    #[test]
    fn paths_touched_since_days_matches_only_the_given_globs() {
        let root = init_plain_repo("covered", "2026-07-01T10:00:00+00:00");
        commit_file(
            &root,
            "src/lib.rs",
            "pub fn a() {}\n",
            "feat(cli): a",
            "2026-07-02T10:00:00+00:00",
        );
        commit_file(
            &root,
            "docs/readme.md",
            "hi\n",
            "docs(repo): readme",
            "2026-07-03T10:00:00+00:00",
        );

        let covered = paths_touched_since_days(&root, 365, &["src/**".to_string()]);
        assert_eq!(covered, vec!["src/lib.rs".to_string()]);

        assert!(
            paths_touched_since_days(&root, 365, &[]).is_empty(),
            "no guards means nothing is covered, not everything"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Capture trimmed stdout of a git command (test helper).
    fn out_git(dir: &Path, args: &[&str]) -> String {
        let out = git_command()
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git runs");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn write(dir: &Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).unwrap();
    }

    /// A unique temp dir for a test repo, removed by the OS eventually; we also
    /// best-effort clean it at the end of each test.
    fn temp_repo_dir(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "dira-sig-{tag}-{}-{}",
            std::process::id(),
            // a cheap monotonic-ish suffix so parallel tests never collide
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    /// Init a repo with a `base` commit on `main` and an `origin` remote tracking
    /// it, so `@{upstream}` resolves. Returns the repo root.
    fn init_repo_with_upstream(tag: &str) -> std::path::PathBuf {
        let root = temp_repo_dir(tag);
        run_git(&root, &["init", "-q", "-b", "main"]);
        run_git(&root, &["config", "user.email", "t@example.com"]);
        run_git(&root, &["config", "user.name", "T"]);
        write(&root, "f1.txt", "a\n");
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-q", "-m", "base"]);
        // Fake an upstream with a local "remote" tracking ref + branch tracking
        // config so `@{upstream}` resolves to origin/main with no network. Setting
        // the config directly (rather than `--set-upstream-to`) avoids git's
        // "starting point is not a branch" check on a bare ref.
        run_git(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        // A remote with a standard fetch refspec is what makes `@{upstream}`
        // resolve to the tracking ref (a bare `branch.*.merge` config alone is
        // rejected by git as "not stored as a remote-tracking branch").
        run_git(&root, &["remote", "add", "origin", "."]);
        run_git(
            &root,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
        );
        set_upstream(&root, "main");
        root
    }

    /// Point `branch`'s upstream at `origin/main` via tracking config (works for a
    /// local fake remote ref where `--set-upstream-to` refuses).
    fn set_upstream(root: &Path, branch: &str) {
        run_git(
            root,
            &["config", &format!("branch.{branch}.remote"), "origin"],
        );
        run_git(
            root,
            &[
                "config",
                &format!("branch.{branch}.merge"),
                "refs/heads/main",
            ],
        );
    }

    #[test]
    fn session_signals_match_squashed_commit_patch_id() {
        // Two commits on a feature branch; the cumulative change-id must equal the
        // patch-id of an equivalent squash-merge commit, and touched-paths/blobs
        // must reflect the whole cumulative change (both files, both commits).
        let root = init_repo_with_upstream("squash");
        let base = out_git(&root, &["rev-parse", "HEAD"]);

        run_git(&root, &["checkout", "-q", "-b", "feat"]);
        // Re-point the upstream of `feat` at origin/main (the unmoved base).
        set_upstream(&root, "feat");
        write(&root, "f1.txt", "a\nb\n");
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-q", "-m", "c1"]);
        write(&root, "f1.txt", "a\nb\nc\n");
        write(&root, "f2.txt", "x\n");
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-q", "-m", "c2"]);

        let sig = session_signals(&root);

        // Build an equivalent squash commit off the same base and read its patch-id.
        run_git(&root, &["checkout", "-q", "-b", "squash", &base]);
        run_git(&root, &["merge", "--squash", "-q", "feat"]);
        run_git(&root, &["commit", "-q", "-m", "squashed"]);
        let squash_pid = {
            let diff = git_command()
                .arg("-C")
                .arg(&root)
                .args(["diff-tree", "-p", "--no-color", "--root", "HEAD"])
                .output()
                .unwrap();
            let mut child = git_command()
                .arg("-C")
                .arg(&root)
                .args(["patch-id", "--stable"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            use std::io::Write;
            child.stdin.take().unwrap().write_all(&diff.stdout).unwrap();
            let o = child.wait_with_output().unwrap();
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .next()
                .unwrap()
                .to_string()
        };

        assert_eq!(
            sig.change_id.as_deref(),
            Some(squash_pid.as_str()),
            "cumulative change-id equals the squash commit's patch-id"
        );

        // Cumulative touched paths cover BOTH commits' files, not just the last.
        let mut paths = sig.touched_paths.clone().expect("touched paths resolved");
        paths.sort();
        assert_eq!(paths, vec!["f1.txt".to_string(), "f2.txt".to_string()]);

        // Blobs are the post-image SHAs at the feature tip for both files.
        let blobs = sig.blobs.clone().expect("blobs resolved");
        assert_eq!(blobs.len(), 2);
        for b in &blobs {
            let expect = out_git(&root, &["rev-parse", &format!("feat:{}", b.path)]);
            assert_eq!(
                b.blob, expect,
                "blob SHA matches the tree object for {}",
                b.path
            );
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn session_change_id_is_stable_across_a_rebase() {
        // The same logical change, rebased onto an unmoved base, must yield the
        // same cumulative change-id (the patch-id of the combined diff is stable
        // even though every commit SHA is rewritten by the rebase).
        let root = init_repo_with_upstream("rebase");

        run_git(&root, &["checkout", "-q", "-b", "feat"]);
        set_upstream(&root, "feat");
        write(&root, "f1.txt", "a\nb\n");
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-q", "-m", "c1"]);
        write(&root, "g.txt", "z\n");
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-q", "-m", "c2"]);

        let before = session_signals(&root).change_id;
        assert!(before.is_some(), "change-id computed before rebase");

        // Rewrite history without changing the net diff: an interactive-style
        // rebase that just re-applies the commits onto the (unmoved) base. We
        // reset to base and cherry-pick to simulate a SHA-rewriting rebase.
        // Pin a *distinct, fixed* committer date on the cherry-pick so the
        // rewritten SHAs reliably differ from the originals (without an explicit
        // date a same-second cherry-pick can reproduce the original SHA byte for
        // byte, since the commit hash folds in the committer timestamp).
        let tip = out_git(&root, &["rev-parse", "HEAD"]);
        let parent = out_git(&root, &["rev-parse", "HEAD~1"]);
        run_git(&root, &["reset", "--hard", "-q", "origin/main"]);
        let status = git_command()
            .arg("-C")
            .arg(&root)
            .args(["cherry-pick", &parent, &tip])
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            // A fixed timestamp far from "now" guarantees a different commit hash.
            .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00 +0000")
            .output()
            .expect("git runs");
        assert!(
            status.status.success(),
            "cherry-pick failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );

        // The SHAs changed…
        assert_ne!(
            tip,
            out_git(&root, &["rev-parse", "HEAD"]),
            "rebase rewrote the tip SHA"
        );
        // …but the cumulative change-id is unchanged.
        let after = session_signals(&root).change_id;
        assert_eq!(after, before, "cumulative change-id survives the rebase");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn session_signals_none_without_upstream() {
        // A repo with commits but no configured upstream ⇒ no cumulative range ⇒
        // all-None signals.
        let root = temp_repo_dir("noupstream");
        run_git(&root, &["init", "-q", "-b", "main"]);
        run_git(&root, &["config", "user.email", "t@example.com"]);
        run_git(&root, &["config", "user.name", "T"]);
        write(&root, "f1.txt", "a\n");
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-q", "-m", "base"]);

        assert_eq!(session_signals(&root), SessionSignals::default());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn session_signals_none_on_detached_head() {
        // Detached HEAD: `@{upstream}` can't resolve ⇒ all-None.
        let root = init_repo_with_upstream("detached");
        let sha = out_git(&root, &["rev-parse", "HEAD"]);
        run_git(&root, &["checkout", "-q", &sha]); // detach
        assert_eq!(session_signals(&root), SessionSignals::default());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn session_signals_none_on_merge_head() {
        // A merge commit at HEAD has no single cumulative diff to anchor ⇒ all-None.
        let root = init_repo_with_upstream("merge");
        run_git(&root, &["checkout", "-q", "-b", "feat"]);
        write(&root, "feat.txt", "f\n");
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-q", "-m", "feat work"]);
        run_git(&root, &["checkout", "-q", "main"]);
        write(&root, "main.txt", "m\n");
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-q", "-m", "main work"]);
        // Force a real merge commit (two parents).
        run_git(&root, &["merge", "--no-ff", "-q", "-m", "merge", "feat"]);

        assert!(session_signals(&root).change_id.is_none());
        assert_eq!(session_signals(&root), SessionSignals::default());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn session_signals_none_when_head_is_base() {
        // On `main` tracking origin/main with no commits ahead, the cumulative
        // range is empty ⇒ all-None (nothing this session changed).
        let root = init_repo_with_upstream("atbase");
        assert_eq!(session_signals(&root), SessionSignals::default());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn session_signals_none_outside_a_repo() {
        // A plain (non-git) directory must never panic — all-None.
        let dir = temp_repo_dir("nonrepo");
        assert_eq!(session_signals(&dir), SessionSignals::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
