//! Orchestration sessions — the coordinating sessions Claude Desktop runs from
//! a scratch workspace, which reach repos indirectly rather than by `cwd`.
//!
//! Everything here is a pure function over metadata the daemon already holds
//! for one hook: the tool name, the tool's input/response JSON, and the cwd.
//! The input/response are read **only** to derive a canonical repo ref, a
//! workspace digest, or a child session id; none of them is ever stored or
//! sent (D-0001). See DIRASH-0041.
//!
//! Detection is conservative in the same way [`crate::project`] is: an input
//! that names several distinct repos yields no evidence at all rather than a
//! guess, and a path containing a shell variable is skipped because the agent's
//! environment is not the daemon's.

use std::path::{Component, Path, PathBuf};

/// The Claude Desktop tool an orchestrator uses to spawn a child session.
pub const START_SESSION_TOOL: &str = "mcp__ccd_session__start_session";

/// Env var Claude Desktop sets in every session's process tree to that
/// session's host id (`local_<uuid>`). The hook shim forwards it.
pub const ENV_HOST_SESSION_ID: &str = "CLAUDE_CODE_HOST_SESSION_ID";

/// The payload key the hook shim nests forwarded env hints under. Reserved:
/// no harness emits it.
pub const PAYLOAD_ENV_KEY: &str = "dira_env";

/// The directory Claude Desktop roots scratch workspaces in, under its
/// per-OS application data directory.
const SCRATCH_ROOT: &str = "scratch-workspaces";

/// Most repos one session's rollup lists. Generous for real orchestration
/// (a fan-out across a dozen repos) while keeping the wire row bounded.
pub const MAX_REPOS: usize = 32;

/// Where a tool call says it worked: a directory to resolve through
/// [`crate::project::resolve`], or a repo named outright (`gh -R`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evidence {
    Path(PathBuf),
    Repo(String),
}

/// Repo evidence from one tool call. Empty when the call names no location, or
/// names more than one distinct location (the caller still has to collapse
/// path evidence that resolves to different repos — see [`single_repo`]).
pub fn repo_evidence(tool: &str, input: &serde_json::Value, cwd: Option<&Path>) -> Vec<Evidence> {
    let found = match tool {
        "Edit" | "Write" | "MultiEdit" | "Read" => file_evidence(input, "file_path", cwd),
        "NotebookEdit" => file_evidence(input, "notebook_path", cwd),
        "Bash" => input
            .get("command")
            .and_then(|c| c.as_str())
            .map(|c| bash_evidence(c, cwd))
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    dedup_or_nothing(found)
}

/// The one canonical repo several resolved refs agree on, or `None` when they
/// disagree or there are none. Ambiguity is never resolved by picking.
pub fn single_repo<I: IntoIterator<Item = String>>(refs: I) -> Option<String> {
    let mut it = refs.into_iter();
    let first = it.next()?;
    it.all(|r| r == first).then_some(first)
}

/// The child host id (`local_<uuid>`) a `start_session` response announces,
/// if it started one. A proposal card returns a task id instead, and yields
/// `None` — nothing ran, so there is no child to link.
pub fn spawned_child(response: &serde_json::Value) -> Option<String> {
    let text = match response {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    find_host_id(&text)
}

/// Normalize a forwarded host session id: must look like `local_<uuid>`.
/// Anything else (a blank, a stray value, an oversized string) reads as unset.
pub fn host_session_id(raw: &str) -> Option<String> {
    find_host_id(raw.trim()).filter(|id| id.len() == raw.trim().len())
}

/// Nest the env hints the hook shim can see under [`PAYLOAD_ENV_KEY`]: today
/// only the host session id. `var` is the env lookup (injected for tests).
/// Leaves the payload untouched when there is nothing valid to add, or when
/// it is not a JSON object.
pub fn inject_env_hints(payload: &mut serde_json::Value, var: impl Fn(&str) -> Option<String>) {
    let Some(host) = var(ENV_HOST_SESSION_ID).and_then(|v| host_session_id(&v)) else {
        return;
    };
    if let Some(obj) = payload.as_object_mut() {
        obj.insert(
            PAYLOAD_ENV_KEY.to_string(),
            serde_json::json!({ "host_session_id": host }),
        );
    }
}

/// A session's orchestration facts, folded from its events in time order.
/// The single implementation behind both the synced rollup
/// (`sync::batch::build_sessions`) and the daemon's live registry, so the two
/// can never disagree about what a session is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionLineage {
    pub orchestrator: bool,
    pub parent_session: Option<String>,
    pub workspace: Option<String>,
    /// Every canonical repo an event of this session resolved to.
    pub repos: std::collections::BTreeSet<String>,
}

impl SessionLineage {
    pub fn observe(&mut self, ev: &crate::model::RawEvent) {
        let l = &ev.lineage;
        self.orchestrator |= l.marks_orchestrator();
        if self.parent_session.is_none() {
            self.parent_session.clone_from(&l.parent_session);
        }
        if self.workspace.is_none() {
            self.workspace.clone_from(&l.workspace);
        }
        if let Some(p) = &ev.project {
            // Clone only for a repo not yet seen: this runs on every event.
            if self.repos.len() < MAX_REPOS && !self.repos.contains(p) {
                self.repos.insert(p.clone());
            }
        }
    }

    /// The session's single repo for the rollup: never one for an
    /// orchestrator, whose work is described by [`Self::repos_touched`].
    pub fn repo_canonical(&self, first_seen: Option<String>) -> Option<String> {
        if self.orchestrator {
            None
        } else {
            first_seen
        }
    }

    pub fn role(&self) -> Option<dira_contract::SessionRole> {
        self.orchestrator
            .then_some(dira_contract::SessionRole::Orchestrator)
    }

    /// Sorted repos, only for an orchestrator — an ordinary session's rollup
    /// stays byte-identical to a pre-1.6 one.
    pub fn repos_touched(&self) -> Option<Vec<String>> {
        (self.orchestrator && !self.repos.is_empty()).then(|| self.repos.iter().cloned().collect())
    }

    pub fn workspace_ref(&self) -> Option<String> {
        self.orchestrator.then(|| self.workspace.clone()).flatten()
    }
}

/// Opaque digest of the Claude Desktop scratch workspace `cwd` sits in, or
/// `None` when it is not inside one. The digest covers the path components
/// *below* the scratch root (account, org, scratch dir), so it is stable
/// across OSes and never carries the user's home path.
pub fn scratch_workspace(cwd: &Path) -> Option<String> {
    let parts: Vec<String> = cwd
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    let at = parts
        .windows(2)
        .position(|w| w[0] == "Claude" && w[1] == SCRATCH_ROOT)?;
    let below = parts.get(at + 2..at + 5)?;
    if !below[2].starts_with("scratch-") {
        return None;
    }
    // 64-bit FNV-1a as 16 lowercase hex chars: a grouping key, not a security
    // boundary, stable across releases and platforms.
    let h = crate::sync::fnv1a_bytes(below.join("/").as_bytes(), FNV_OFFSET_BASIS);
    Some(format!("{h:016x}"))
}

/// The standard 64-bit FNV-1a offset basis.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;

fn find_host_id(text: &str) -> Option<String> {
    const PREFIX: &str = "local_";
    const UUID_LEN: usize = 36;
    let mut from = 0;
    while let Some(i) = text[from..].find(PREFIX) {
        let start = from + i + PREFIX.len();
        let tail = text.get(start..start + UUID_LEN);
        if let Some(uuid) = tail.filter(|t| is_uuid(t)) {
            return Some(format!("{PREFIX}{uuid}"));
        }
        from = start;
    }
    None
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

fn file_evidence(input: &serde_json::Value, key: &str, cwd: Option<&Path>) -> Vec<Evidence> {
    let Some(raw) = input.get(key).and_then(|v| v.as_str()) else {
        return Vec::new();
    };
    let Some(path) = local_path(raw, cwd) else {
        return Vec::new();
    };
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => vec![Evidence::Path(dir.to_path_buf())],
        _ => Vec::new(),
    }
}

/// Evidence from a shell command: the directory a `cd` chain ends in,
/// `git -C <dir>`, `gh … -R owner/name`, and `gh api repos/owner/name/…`.
/// Only token prefixes are read; the command is never evaluated.
fn bash_evidence(command: &str, cwd: Option<&Path>) -> Vec<Evidence> {
    let mut out = Vec::new();
    let mut here: Option<PathBuf> = cwd.map(Path::to_path_buf);
    let mut moved = false;
    for segment in split_segments(command) {
        let words = shell_words(&segment);
        let Some(head) = words.first().map(String::as_str) else {
            continue;
        };
        match head {
            "cd" | "pushd" => {
                let Some(target) = words.get(1) else { continue };
                match local_path(target, here.as_deref()) {
                    Some(p) => {
                        here = Some(p);
                        moved = true;
                    }
                    // `cd $TMPDIR`: somewhere we cannot know. Anything that
                    // follows is relative to an unknown dir, so stop moving.
                    None => {
                        here = None;
                        moved = false;
                    }
                }
            }
            "git" => {
                if let Some(dir) = flag_value(&words, &["-C"]) {
                    if let Some(p) = local_path(&dir, here.as_deref()) {
                        out.push(Evidence::Path(p));
                    }
                }
            }
            "gh" => {
                if let Some(r) = flag_value(&words, &["-R", "--repo"]).and_then(|r| gh_repo(&r)) {
                    out.push(Evidence::Repo(r));
                }
                if words.get(1).map(String::as_str) == Some("api") {
                    if let Some(r) = words.iter().skip(2).find_map(|w| gh_api_repo(w)) {
                        out.push(Evidence::Repo(r));
                    }
                }
            }
            _ => {}
        }
    }
    if moved {
        if let Some(dir) = here {
            out.push(Evidence::Path(dir));
        }
    }
    out
}

fn dedup_or_nothing(mut found: Vec<Evidence>) -> Vec<Evidence> {
    found.dedup();
    let repos: Vec<&String> = found
        .iter()
        .filter_map(|e| match e {
            Evidence::Repo(r) => Some(r),
            Evidence::Path(_) => None,
        })
        .collect();
    if repos.windows(2).any(|w| w[0] != w[1]) {
        return Vec::new();
    }
    found
}

/// `owner/name` or `host/owner/name` → canonical ref; anything else → `None`.
fn gh_repo(spec: &str) -> Option<String> {
    let parts: Vec<&str> = spec.trim_matches('/').split('/').collect();
    let (host, owner, name) = match parts.as_slice() {
        [o, n] => ("github.com", *o, *n),
        [h, o, n] if h.contains('.') => (*h, *o, *n),
        _ => return None,
    };
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (ok(owner) && ok(name))
        .then(|| crate::project::canonicalize_remote(&format!("https://{host}/{owner}/{name}")))
        .flatten()
}

/// `repos/owner/name[/…]` (optionally with a leading `/`) → canonical ref.
fn gh_api_repo(word: &str) -> Option<String> {
    let rest = word.trim_start_matches('/').strip_prefix("repos/")?;
    let mut it = rest.split(['/', '?']);
    let owner = it.next()?;
    let name = it.next()?;
    gh_repo(&format!("{owner}/{name}"))
}

/// The value of the first of `flags` in `words` (`-R x`, `--repo x`, `--repo=x`).
fn flag_value(words: &[String], flags: &[&str]) -> Option<String> {
    for (i, w) in words.iter().enumerate() {
        for f in flags {
            if w == f {
                return words.get(i + 1).cloned();
            }
            if let Some(v) = w.strip_prefix(&format!("{f}=")) {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// A path as the daemon can know it: absolute, `~`-relative, or relative to a
/// known `base`. A path that leans on a shell variable is unknowable here.
fn local_path(raw: &str, base: Option<&Path>) -> Option<PathBuf> {
    if raw.is_empty() || raw.contains('$') || raw.contains('`') || raw == "-" {
        return None;
    }
    let p = if let Some(rest) = raw.strip_prefix("~/") {
        crate::config::home_dir().ok()?.join(rest)
    } else if raw == "~" {
        crate::config::home_dir().ok()?
    } else {
        let p = PathBuf::from(raw);
        if p.is_absolute() {
            p
        } else {
            base?.join(p)
        }
    };
    Some(normalize(&p))
}

/// Lexically fold `.` and `..` so `repos/../repos/x` and `repos/x` agree.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Split a command on `&&`, `||`, `;`, `|` and newlines, outside quotes.
fn split_segments(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => {
                quote = None;
                cur.push(c);
            }
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                cur.push(c);
            }
            (None, ';' | '\n' | '|' | '&') => {
                if matches!(c, '|' | '&') && chars.peek() == Some(&c) {
                    chars.next();
                }
                out.push(std::mem::take(&mut cur));
            }
            (None, c) => cur.push(c),
        }
    }
    out.push(cur);
    out.retain(|s| !s.trim().is_empty());
    out
}

/// Minimal POSIX-ish word split: whitespace-separated, single/double quotes
/// grouped, backslash escapes outside single quotes. Enough for prefixes.
fn shell_words(segment: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = segment.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('\''), c) => cur.push(c),
            (_, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
                in_word = true;
            }
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bash(cmd: &str, cwd: &str) -> Vec<Evidence> {
        repo_evidence("Bash", &json!({ "command": cmd }), Some(Path::new(cwd)))
    }

    fn path(p: &str) -> Evidence {
        Evidence::Path(PathBuf::from(p))
    }

    fn repo(r: &str) -> Evidence {
        Evidence::Repo(r.into())
    }

    #[test]
    fn cd_chain_resolves_to_the_final_directory() {
        assert_eq!(
            bash("cd repos && cd kodni-mobile && git push", "/w"),
            vec![path("/w/repos/kodni-mobile")]
        );
        assert_eq!(bash("cd /abs/x; ls", "/w"), vec![path("/abs/x")]);
        assert_eq!(
            bash("cd repos/../repos/x && ls", "/w"),
            vec![path("/w/repos/x")]
        );
    }

    #[test]
    fn a_cd_into_a_shell_variable_is_unknowable() {
        assert!(bash("cd $TMPDIR && git status", "/w").is_empty());
        // …but a repo named outright after it still counts.
        assert_eq!(
            bash(
                "cd $TMPDIR; gh api repos/dodi-smart/kodni-mobile/commits/abc",
                "/w"
            ),
            vec![repo("github.com/dodi-smart/kodni-mobile")]
        );
    }

    #[test]
    fn git_dash_c_is_relative_to_the_current_directory() {
        assert_eq!(
            bash("git -C repos/a log -1", "/w"),
            vec![path("/w/repos/a")]
        );
    }

    #[test]
    fn gh_repo_flags_and_api_paths_name_the_repo() {
        let want = vec![repo("github.com/acme/api")];
        assert_eq!(bash("gh pr list -R acme/api --state all", "/w"), want);
        assert_eq!(bash("gh pr view 4 --repo=acme/api", "/w"), want);
        assert_eq!(bash("gh api /repos/acme/api/pulls?state=open", "/w"), want);
        assert_eq!(bash("gh issue list --repo \"acme/api\"", "/w"), want);
    }

    #[test]
    fn several_distinct_repos_in_one_command_are_ambiguous() {
        assert!(bash("gh pr list -R acme/api; gh pr list -R acme/web", "/w").is_empty());
        // The same repo twice is not ambiguous.
        assert_eq!(
            bash("gh pr list -R acme/api && gh api repos/acme/api", "/w"),
            vec![repo("github.com/acme/api")]
        );
    }

    #[test]
    fn quoted_separators_do_not_split_segments() {
        assert_eq!(
            bash("cd 'my repo' && git commit -m 'a; cd /etc'", "/w"),
            vec![path("/w/my repo")]
        );
    }

    #[test]
    fn file_tools_point_at_the_containing_directory() {
        // A platform-absolute path: `/w/…` has no drive letter on Windows, so it
        // is relative there and, with no cwd, rightly yields nothing.
        let root = std::env::temp_dir().join("w").join("repos").join("a");
        let file = root.join("src").join("lib.rs");
        let ev = repo_evidence(
            "Edit",
            &json!({ "file_path": file.to_string_lossy(), "old_string": "x" }),
            None,
        );
        assert_eq!(ev, vec![Evidence::Path(normalize(&root.join("src")))]);
        let ev = repo_evidence(
            "Write",
            &json!({ "file_path": "notes.md" }),
            Some(Path::new("/w")),
        );
        assert_eq!(ev, vec![path("/w")]);
        assert!(repo_evidence("Edit", &json!({}), None).is_empty());
        assert!(repo_evidence("WebFetch", &json!({ "url": "x" }), None).is_empty());
    }

    #[test]
    fn single_repo_refuses_to_pick() {
        assert_eq!(single_repo(vec!["a".into(), "a".into()]), Some("a".into()));
        assert_eq!(single_repo(vec!["a".into(), "b".into()]), None);
        assert_eq!(single_repo(Vec::<String>::new()), None);
    }

    #[test]
    fn spawned_child_reads_the_host_id_from_a_start_session_response() {
        let resp = json!([{ "type": "text", "text":
            "Session started: local_f42e16b4-7364-4ba7-966e-6d2f2eaa2147 (name: ci-fix)" }]);
        assert_eq!(
            spawned_child(&resp).as_deref(),
            Some("local_f42e16b4-7364-4ba7-966e-6d2f2eaa2147")
        );
        // A proposal card carries a task id, not a started session.
        assert_eq!(spawned_child(&json!({ "task_id": "task_ab12cd34" })), None);
        // A truncated id is not an id.
        assert_eq!(spawned_child(&json!("local_f42e16b4-7364")), None);
    }

    #[test]
    fn host_session_id_must_be_exactly_one_local_id() {
        let id = "local_684b6117-0abc-44f3-9ce0-44f092702849";
        assert_eq!(host_session_id(id).as_deref(), Some(id));
        assert_eq!(host_session_id(&format!("  {id}\n")).as_deref(), Some(id));
        assert_eq!(host_session_id(""), None);
        assert_eq!(host_session_id(&format!("{id}junk")), None);
    }

    #[test]
    fn env_hints_are_injected_only_when_valid() {
        let id = "local_684b6117-0abc-44f3-9ce0-44f092702849";
        let mut p = json!({ "hook_event_name": "Stop" });
        inject_env_hints(&mut p, |k| {
            (k == ENV_HOST_SESSION_ID).then(|| id.to_string())
        });
        assert_eq!(p[PAYLOAD_ENV_KEY]["host_session_id"], id);

        let mut p = json!({ "hook_event_name": "Stop" });
        inject_env_hints(&mut p, |_| Some("garbage".into()));
        assert!(p.get(PAYLOAD_ENV_KEY).is_none());
        inject_env_hints(&mut p, |_| None);
        assert!(p.get(PAYLOAD_ENV_KEY).is_none());
    }

    #[test]
    fn scratch_workspace_is_detected_on_every_os_layout() {
        let mac = Path::new(
            "/Users/u/Library/Application Support/Claude/scratch-workspaces/acct/org/scratch-2026-10-15-bcf1a3/repos/x",
        );
        let win = Path::new(
            "C:/Users/u/AppData/Roaming/Claude/scratch-workspaces/acct/org/scratch-2026-10-15-bcf1a3",
        );
        let a = scratch_workspace(mac).expect("mac layout");
        assert_eq!(a.len(), 16);
        // Same workspace below the root → same digest, wherever the root is.
        assert_eq!(scratch_workspace(win).as_deref(), Some(a.as_str()));
        // Another scratch dir → another digest.
        let other = Path::new(
            "/Users/u/Library/Application Support/Claude/scratch-workspaces/acct/org/scratch-2026-10-15-c1ab12",
        );
        assert_ne!(scratch_workspace(other).as_deref(), Some(a.as_str()));
        assert_eq!(
            scratch_workspace(Path::new("/Users/u/Workspace/repo")),
            None
        );
        assert_eq!(
            scratch_workspace(Path::new(
                "/x/Claude/scratch-workspaces/acct/org/not-scratch"
            )),
            None
        );
    }
}
