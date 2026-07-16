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
    Response, ZavetDecisionView, ZavetGuardStatView, ZavetStatusView, ZavetWhyView,
};
use dira_core::store::ZavetDecisionRow;
pub use dira_core::zavet::{parse_guard_event, GuardEventV1, ZAVET_DIR};
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

/// Resolve a payload cwd to `(canonical repo, .zavet/ exists)` off the async
/// path — `project::resolve` shells out to git.
async fn resolve_repo(cwd: String) -> (Option<String>, bool) {
    tokio::task::spawn_blocking(move || {
        let dir = PathBuf::from(cwd);
        let top = dira_core::project::toplevel(&dir);
        let repo = dira_core::project::resolve(&dir).project;
        let dir_exists = top.as_deref().map(zavet_dir_exists).unwrap_or(false);
        (repo, dir_exists)
    })
    .await
    .unwrap_or((None, false))
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
    let Some(ev) = parse_guard_event(&payload) else {
        tracing::debug!("zavet: dropped malformed guard event");
        return Response::Ok; // the shim is fire-and-forget; nothing to say
    };
    let (repo, dir_exists) = resolve_repo(ev.cwd.clone()).await;
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
        Ok(_) => Response::Ok,
        Err(e) => Response::Error {
            message: format!("zavet ingest failed: {e}"),
        },
    }
}

/// Repo resolution ladder for the query commands: explicit repo wins, else
/// resolve from `cwd` (or the daemon's own cwd). Also reports whether the
/// resolved toplevel carries `.zavet/` when a directory was available.
async fn query_repo(
    repo: Option<String>,
    cwd: Option<String>,
) -> Result<(String, Option<bool>), Response> {
    if let Some(r) = repo {
        return Ok((r, None));
    }
    let dir = cwd.unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_string())
    });
    let (repo, dir_exists) = resolve_repo(dir).await;
    match repo {
        Some(r) => Ok((r, Some(dir_exists))),
        None => Err(Response::Error {
            message: "not inside a repo with a recognizable remote; pass --project".into(),
        }),
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
    }
}

/// Ranked results for a free-text query: decisions (their attached trailers
/// boost them) plus orphan commit trailers — micro-decisions with no record,
/// which can be the entire answer for a question like "why is the timeout 10s".
struct ZavetSearchResults {
    decisions: Vec<(ZavetDecisionRow, u32)>,
    trailers: Vec<dira_core::protocol::ZavetTrailerHit>,
}

impl ZavetSearchResults {
    fn is_empty(&self) -> bool {
        self.decisions.is_empty() && self.trailers.is_empty()
    }
}

async fn search(state: &AppState, repo: &str, query: &str) -> ZavetSearchResults {
    let terms = dira_core::zavet::tokenize_query(query);
    if terms.is_empty() {
        return ZavetSearchResults {
            decisions: Vec::new(),
            trailers: Vec::new(),
        };
    }
    let decisions = state
        .store
        .zavet_decisions_list(repo)
        .await
        .unwrap_or_default();
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
    let (repo, dir_exists) = match query_repo(repo, cwd).await {
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
    let (repo, _) = match query_repo(repo, cwd).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    match state.store.zavet_decisions_list(&repo).await {
        Ok(rows) => Response::ZavetDecisions {
            decisions: rows.iter().map(decision_view).collect(),
        },
        Err(e) => Response::Error {
            message: format!("zavet decisions failed: {e}"),
        },
    }
}

/// `Request::ZavetWiki` — the browsable knowledge base: an overview without a
/// topic, ranked matches with one.
pub async fn wiki(
    state: &AppState,
    topic: Option<String>,
    cwd: Option<String>,
    repo: Option<String>,
) -> Response {
    let (repo, _) = match query_repo(repo, cwd).await {
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
            trailers: results.trailers,
        };
    }
    let decisions = state
        .store
        .zavet_decisions_list(&repo)
        .await
        .unwrap_or_default();
    let counts = state.store.zavet_counts(&repo).await.unwrap_or_default();
    let recent = state
        .store
        .zavet_recent_trailers(&repo, 5)
        .await
        .unwrap_or_default();
    let (active, superseded): (Vec<_>, Vec<_>) = decisions
        .iter()
        .map(decision_view)
        .partition(|d| d.status.as_deref().unwrap_or("active") == "active");
    Response::ZavetWiki(Box::new(dira_core::protocol::ZavetWikiView {
        repo,
        decisions_total: counts.decisions_total,
        trailers: counts.trailers,
        guard_events: counts.guard_events,
        active,
        superseded,
        recent,
    }))
}

/// `Request::ZavetSetMode` (`dira zavet enable|disable`).
pub async fn set_mode(
    state: &AppState,
    cwd: Option<String>,
    repo: Option<String>,
    mode: String,
) -> Response {
    let (repo, _) = match query_repo(repo, cwd).await {
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

/// `Request::ZavetWhy` — the recorded knowledge plus its evidence. Session
/// pricing (`sessions`, `total_*`) is filled by [`price_why`].
pub async fn why(
    state: &AppState,
    query: String,
    cwd: Option<String>,
    repo: Option<String>,
) -> Response {
    let (repo, _) = match query_repo(repo, cwd).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    // A decision id answers directly; free text searches. One confidence rule:
    // the top decision answers in full iff its score at least doubles the
    // runner-up — the second decision or the best orphan trailer, whichever is
    // stronger (a sole hit trivially qualifies). Anything less confident
    // returns the ranked matches rather than guessing; when only trailers
    // matched, the trailers ARE the answer. Zero is a clean pointer to the wiki.
    let mut matched_query = None;
    let decision_id = match dira_core::zavet::canonical_decision_id(&query) {
        Some(id) => id,
        None => {
            let results = search(state, &repo, &query).await;
            if results.is_empty() {
                return Response::Error {
                    message: format!(
                        "nothing recorded matches `{query}` for {repo} — browse with `dira zavet wiki`, or record it via /zavet:decide"
                    ),
                };
            }
            let second = results.decisions.get(1).map(|(_, s)| *s).unwrap_or(0);
            let best_trailer = results.trailers.first().map(|t| t.score).unwrap_or(0);
            let runner_up = second.max(best_trailer);
            match results.decisions.first() {
                Some((top, score)) if *score >= runner_up * 2 => {
                    matched_query = Some(query.clone());
                    top.id.clone()
                }
                _ => {
                    return Response::ZavetSearch {
                        query,
                        hits: results
                            .decisions
                            .iter()
                            .take(5)
                            .map(|(d, s)| search_hit(d, *s))
                            .collect(),
                        trailers: results.trailers,
                    }
                }
            }
        }
    };
    let decision = match state.store.zavet_decision_get(&repo, &decision_id).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            return Response::Error {
                message: format!("no captured decision `{decision_id}` for {repo} (is it committed, and is the daemon watching this repo?)"),
            }
        }
        Err(e) => {
            return Response::Error {
                message: format!("zavet why failed: {e}"),
            }
        }
    };
    // Reverse supersedes link: the decision that replaced this one, if captured.
    let superseded_by = state
        .store
        .zavet_superseded_by(&repo, &decision_id)
        .await
        .unwrap_or(None);
    let commits = state
        .store
        .zavet_commits_for_decision(&repo, &decision_id)
        .await
        .unwrap_or_default();
    let stats = state
        .store
        .zavet_guard_event_stats(&repo, Some(&decision_id))
        .await
        .unwrap_or_default();
    let sessions = state
        .store
        .zavet_sessions_for_decision(&repo, &decision_id)
        .await
        .unwrap_or_default();

    let unattributed_commits = commits
        .iter()
        .filter(|c| c.source_session.is_none())
        .count() as u64;
    let unattributed_guard_events = stats.iter().map(|s| s.unattributed).sum();

    let mut view = ZavetWhyView {
        repo: repo.clone(),
        matched_query,
        body_md: decision.body_md.clone(),
        decision: decision_view(&decision),
        superseded_by,
        commits: commits
            .into_iter()
            .map(|c| dira_core::protocol::ZavetCommitView {
                sha: c.sha,
                message: c.message,
                authored_at: c.authored_at,
                session_id: c.source_session,
            })
            .collect(),
        guard_stats: guard_stat_views(stats),
        sessions: Vec::new(),
        total_human_seconds: 0,
        total_agent_seconds: 0,
        total_input_tokens: 0,
        total_output_tokens: 0,
        unattributed_commits,
        unattributed_guard_events,
    };
    price_why(state, &mut view, &sessions).await;
    Response::ZavetWhy(Box::new(view))
}

/// Fill a why-view's time panel: for each evidencing session, de-duplicated
/// human seconds (global opening-signal attribution over the raw event log —
/// the same math `dira report` uses), idle-trimmed agent seconds, and token
/// sums; compacted history joins from the daily rollup. Totals are the sums.
async fn price_why(state: &AppState, view: &mut ZavetWhyView, session_ids: &[String]) {
    if session_ids.is_empty() {
        return;
    }
    let events = state.store.events_since(None).await.unwrap_or_default();
    let idle = state.config.idle();
    let human_signals: Vec<(OffsetDateTime, String)> = events
        .iter()
        .filter(|e| e.kind.is_human_signal())
        .map(|e| (e.at, e.session_id.clone()))
        .collect();
    let human_by_session = dira_core::accounting::per_key_seconds(&human_signals, idle);

    for sid in session_ids {
        let raw_human = human_by_session.get(sid).copied().unwrap_or(0);
        let raw_agent = crate::control::session_agent_seconds(&events, sid, idle);
        let hist = state
            .store
            .zavet_session_totals(sid)
            .await
            .unwrap_or_default();
        let line = dira_core::protocol::ZavetSessionCostView {
            session_id: sid.clone(),
            human_seconds: raw_human + hist.rollup_human_seconds,
            agent_seconds: raw_agent + hist.rollup_agent_seconds,
            input_tokens: hist.input_tokens,
            output_tokens: hist.output_tokens,
        };
        view.total_human_seconds += line.human_seconds;
        view.total_agent_seconds += line.agent_seconds;
        view.total_input_tokens += line.input_tokens;
        view.total_output_tokens += line.output_tokens;
        view.sessions.push(line);
    }
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
