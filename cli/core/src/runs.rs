//! Runs — an orchestrator and every session it spawned, shown as one unit.
//!
//! A run is a *lens* over the session list, never a new accounting unit: each
//! session appears exactly once (in its run, or on its own), and its numbers
//! are untouched. The cloud implements the same rules in `assembleRuns`, so a
//! change here is a change there.
//!
//! Rules:
//! 1. **Flatten.** A run is rooted at the topmost orchestrator present in the
//!    list; nested orchestrators and their children all join that root's run.
//! 2. **Repos.** A run's repos are the union of its members' repos — a
//!    session's `project` plus its `repos` (an orchestrator's, or a spawner's
//!    that keeps its own repo but reached into others). A repo that only an
//!    orchestrator touched, and no other member worked in, is **direct**.
//! 3. **Fold.** A run touching at most one repo is *folded*: renderers show it
//!    as that repo's work rather than as a new group.
//! 4. **Orphans.** A child whose parent is not in the list stands alone; it
//!    joins its run once the parent shows up.

use crate::protocol::SessionView;
use std::collections::{BTreeSet, HashMap};

/// Ancestry deeper than this is treated as a cycle and cut. Real fan-outs are
/// one or two levels deep. Public so the daemon's run-children walk makes the
/// same cut.
pub const MAX_DEPTH: usize = 8;

/// One orchestration run.
#[derive(Debug)]
pub struct Run<'a> {
    pub root: &'a SessionView,
    /// Every other member, flattened, in list order.
    pub members: Vec<&'a SessionView>,
    /// Union of all members' repos, sorted.
    pub repos: Vec<String>,
    /// Repos only an orchestrator in the run touched directly, sorted.
    pub direct: Vec<String>,
}

impl Run<'_> {
    /// Rule 3: a run over at most one repo is shown as that repo's work.
    pub fn folded(&self) -> bool {
        self.repos.len() <= 1
    }

    /// An orchestrator with no members and no repos: it coordinated nothing
    /// that is listed (#181), so renderers name it a scratch session. Display
    /// only; its role and its report bucket are unchanged.
    pub fn is_scratch(&self) -> bool {
        self.root.orchestrator && self.members.is_empty() && self.repos.is_empty()
    }
}

/// One line of a grouped session list.
#[derive(Debug)]
pub enum Entry<'a> {
    Session(&'a SessionView),
    Run(Run<'a>),
}

/// Group a session list into runs, keeping the list's order: each run sits
/// where its earliest member appeared.
pub fn group(sessions: &[SessionView]) -> Vec<Entry<'_>> {
    let by_id: HashMap<&str, &SessionView> = sessions
        .iter()
        .map(|s| (s.session_id.as_str(), s))
        .collect();

    /// The topmost present ancestor, or `None` for a session in no run. A
    /// cyclic chain (corrupt links) puts nobody in a run rather than guess.
    fn root_of<'a>(s: &'a SessionView, by_id: &HashMap<&str, &'a SessionView>) -> Option<&'a str> {
        let mut seen = vec![s.session_id.as_str()];
        let mut cur = s;
        let mut root = s.orchestrator.then_some(s.session_id.as_str());
        for _ in 0..MAX_DEPTH {
            let Some(parent) = cur.parent_session.as_deref().and_then(|p| by_id.get(p)) else {
                return root;
            };
            if seen.contains(&parent.session_id.as_str()) {
                return None;
            }
            seen.push(parent.session_id.as_str());
            root = Some(parent.session_id.as_str());
            cur = parent;
        }
        root
    }

    let mut runs: HashMap<&str, Vec<&SessionView>> = HashMap::new();
    // A parent can head a run without carrying the orchestrator flag (its
    // spawn record fell outside the listed window). Being some member's root
    // is the claim, so it joins its own run instead of also standing alone.
    let roots: Vec<Option<&str>> = sessions.iter().map(|s| root_of(s, &by_id)).collect();
    let heads: BTreeSet<&str> = roots.iter().flatten().copied().collect();
    let roots: Vec<Option<&str>> = sessions
        .iter()
        .zip(roots)
        .map(|(s, root)| {
            root.or_else(|| {
                heads
                    .contains(s.session_id.as_str())
                    .then_some(s.session_id.as_str())
            })
        })
        .collect();
    for (s, root) in sessions.iter().zip(&roots) {
        if let Some(root) = *root {
            let members = runs.entry(root).or_default();
            if s.session_id != root {
                members.push(s);
            }
        }
    }

    let mut out = Vec::new();
    let mut emitted: BTreeSet<&str> = BTreeSet::new();
    for (s, root) in sessions.iter().zip(roots) {
        match root {
            None => out.push(Entry::Session(s)),
            Some(root) if emitted.insert(root) => {
                let members = runs.remove(root).unwrap_or_default();
                out.push(Entry::Run(build_run(by_id[root], members)));
            }
            Some(_) => {}
        }
    }
    out
}

fn build_run<'a>(root: &'a SessionView, members: Vec<&'a SessionView>) -> Run<'a> {
    let mut via_children = BTreeSet::new();
    let mut via_orchestrators = BTreeSet::new();
    for s in std::iter::once(root).chain(members.iter().copied()) {
        if s.orchestrator {
            via_orchestrators.extend(s.repos.iter().cloned());
        } else {
            via_children.extend(s.project.iter().chain(&s.repos).cloned());
        }
    }
    Run {
        root,
        members,
        repos: via_children.union(&via_orchestrators).cloned().collect(),
        direct: via_orchestrators
            .difference(&via_children)
            .cloned()
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dira_contract::{Harness, SessionKind};

    fn view(id: &str) -> SessionView {
        SessionView {
            handle: id.into(),
            session_id: id.into(),
            harness: Harness::ClaudeCode,
            kind: SessionKind::Agent,
            project: None,
            label: None,
            activity: None,
            note: None,
            started_at: String::new(),
            human_seconds: 0,
            agent_seconds: 0,
            idle: true,
            agent_active: false,
            last_activity_at: None,
            last_human_at: None,
            has_agent_activity: false,
            orchestrator: false,
            parent_session: None,
            repos: Vec::new(),
            branch: None,
            spec: None,
            tokens: Default::default(),
        }
    }

    fn orch(id: &str, repos: &[&str]) -> SessionView {
        SessionView {
            orchestrator: true,
            repos: repos.iter().map(|r| r.to_string()).collect(),
            ..view(id)
        }
    }

    fn child(id: &str, parent: &str, project: &str) -> SessionView {
        SessionView {
            parent_session: Some(parent.into()),
            project: Some(project.into()),
            ..view(id)
        }
    }

    fn ids(e: &Entry) -> Vec<String> {
        match e {
            Entry::Session(s) => vec![s.session_id.clone()],
            Entry::Run(r) => std::iter::once(r.root)
                .chain(r.members.iter().copied())
                .map(|s| s.session_id.clone())
                .collect(),
        }
    }

    #[test]
    fn children_join_their_orchestrator_and_appear_once() {
        let list = vec![
            child("k1", "o", "a"),
            view("plain"),
            orch("o", &["b"]),
            child("k2", "o", "c"),
        ];
        let g = group(&list);
        assert_eq!(g.len(), 2);
        // The run sits where its earliest member (k1) appeared.
        assert_eq!(ids(&g[0]), vec!["o", "k1", "k2"]);
        assert_eq!(ids(&g[1]), vec!["plain"]);
        let Entry::Run(r) = &g[0] else { panic!() };
        assert_eq!(r.repos, vec!["a", "b", "c"]);
        assert_eq!(r.direct, vec!["b"]);
        assert!(!r.folded());
        let total: usize = g.iter().map(|e| ids(e).len()).sum();
        assert_eq!(total, list.len(), "every session exactly once");
    }

    #[test]
    fn nested_orchestrators_flatten_to_the_root() {
        let list = vec![
            orch("root", &[]),
            SessionView {
                parent_session: Some("root".into()),
                ..orch("mid", &["x"])
            },
            child("leaf", "mid", "y"),
        ];
        let g = group(&list);
        assert_eq!(g.len(), 1);
        assert_eq!(ids(&g[0]), vec!["root", "mid", "leaf"]);
    }

    #[test]
    fn a_single_repo_run_folds() {
        let list = vec![orch("o", &["a"]), child("k", "o", "a")];
        let Entry::Run(r) = &group(&list)[0] else {
            panic!()
        };
        assert!(r.folded());
        assert!(r.direct.is_empty(), "the child covers the only repo");
    }

    #[test]
    fn an_orphan_stands_alone_until_its_parent_appears() {
        let list = vec![child("k", "gone", "a")];
        assert!(matches!(group(&list)[0], Entry::Session(_)));
    }

    #[test]
    fn a_parent_not_flagged_orchestrator_still_heads_its_run_once() {
        // The spawn record may sit outside the listed window, so the parent
        // can arrive without the orchestrator flag. It must still head the
        // run, and not also stand alone.
        let list = vec![view("p"), child("k", "p", "a")];
        let g = group(&list);
        assert_eq!(g.len(), 1);
        assert_eq!(ids(&g[0]), vec!["p", "k"]);
    }

    #[test]
    fn a_spawner_heads_its_run_with_its_own_repo_and_reach() {
        // A repo-rooted session that spawned a child keeps its repo, carries
        // the other repos it touched, and heads the run as the child's parent.
        let spawner = SessionView {
            project: Some("app".into()),
            repos: vec!["api".into(), "app".into()],
            ..view("p")
        };
        let list = vec![spawner, child("k", "p", "web")];
        let g = group(&list);
        assert_eq!(g.len(), 1);
        let Entry::Run(r) = &g[0] else { panic!() };
        assert_eq!(r.root.session_id, "p");
        assert_eq!(r.repos, vec!["api", "app", "web"]);
        assert!(r.direct.is_empty(), "only orchestrators have direct repos");
    }

    #[test]
    fn a_parent_cycle_cannot_hang_grouping() {
        let list = vec![child("a", "b", "x"), child("b", "a", "y")];
        let g = group(&list);
        let total: usize = g.iter().map(|e| ids(e).len()).sum();
        assert_eq!(total, 2);
    }
}
