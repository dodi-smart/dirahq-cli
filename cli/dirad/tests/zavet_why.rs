//! The M1 exit criterion, end to end: `dira zavet why D-0001` answers both
//! "what is this decision?" (record, guards, commits) and "what did it cost?"
//! (engaged/agent time + tokens of its evidencing sessions), with the cost
//! matching the same accounting the reports use.

use dira_contract::Harness;
use dira_core::model::{EventKind, RawEvent};
use dira_core::protocol::{Request, Response};
use dira_core::store::{ZavetDecisionCapture, ZavetTrailer};
use dira_core::{accounting, Config, Store};
use dirad::state::AppState;
use time::{Duration, OffsetDateTime};
use ulid::Ulid;

const REPO: &str = "github.com/acme/api";

async fn test_state() -> AppState {
    let store = Store::open_in_memory().await.expect("in-memory store");
    let config = Config::default();
    let (state, _rx, _sync_rx, _knowledge_rx) = dirad::build_state(store, config)
        .await
        .expect("build_state");
    state
}

fn ev(session: &str, kind: EventKind, at: OffsetDateTime) -> RawEvent {
    RawEvent {
        id: Ulid::new().to_string(),
        at,
        session_id: session.to_string(),
        harness: Harness::ClaudeCode,
        kind,
        cwd: None,
        project: Some(REPO.to_string()),
        identity_email: None,
        branch: None,
        tool: None,
        label: None,
        activity: None,
        note: None,
    }
}

#[tokio::test]
async fn zavet_why_answers_knowledge_and_cost() {
    let state = test_state().await;
    let t0 = OffsetDateTime::now_utc() - Duration::hours(2);

    // Session s1: a burst of human-signalled work (prompts 60s apart) with
    // agent activity in between — the shape accounting counts deterministically.
    for i in 0..5 {
        let at = t0 + Duration::seconds(i * 60);
        state
            .store
            .append(&ev("s1", EventKind::UserPrompt, at))
            .await
            .unwrap();
        state
            .store
            .append(&ev("s1", EventKind::PostTool, at + Duration::seconds(30)))
            .await
            .unwrap();
    }
    // Tokens for s1.
    let turn = dira_core::tokens::TokenTurn {
        id: "u1".into(),
        at: (t0 + Duration::seconds(90))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap(),
        model: "some-model".into(),
        input: 1000,
        output: 2000,
        cache_read: 0,
        cache_create: 0,
    };
    state
        .store
        .upsert_token_usage(&turn, "s1", Some(REPO))
        .await
        .unwrap();

    // The decision was introduced by commit sha1, attributed to s1, and a
    // trailer from an UNATTRIBUTED commit sha2 also references it.
    let cap = ZavetDecisionCapture {
        id: "D-0001".into(),
        title: Some("Poll git".into()),
        status: Some("active".into()),
        path: ".zavet/decisions/D-0001-poll.md".into(),
        body_md: Some("## Decision\npoll".into()),
        guards: vec!["src/**".into()],
        ..Default::default()
    };
    state
        .store
        .zavet_upsert_decision(REPO, &cap, "sha1", None, Some("s1"))
        .await
        .unwrap();
    let c1 = dira_core::project::CapturedCommit {
        sha: "sha1".into(),
        authored_at: None,
        author_email: None,
        author_name: None,
        message: "docs: decide".into(),
        additions: 1,
        deletions: 0,
        patch_id: None,
    };
    state
        .store
        .record_commit(&c1, Some(REPO), None, Some("s1"), None)
        .await
        .unwrap();
    let c2 = dira_core::project::CapturedCommit {
        sha: "sha2".into(),
        authored_at: None,
        author_email: None,
        author_name: None,
        message: "feat: comply".into(),
        additions: 1,
        deletions: 0,
        patch_id: None,
    };
    state
        .store
        .record_commit(&c2, Some(REPO), None, None, None)
        .await
        .unwrap();
    state
        .store
        .zavet_record_trailers(
            Some(REPO),
            "sha2",
            &[ZavetTrailer {
                key: "refs".into(),
                value: "D-0001".into(),
                decision_id: Some("D-0001".into()),
            }],
        )
        .await
        .unwrap();

    let resp = dirad::control::dispatch(
        &state,
        Request::ZavetWhy {
            query: "D-0001".into(),
            cwd: None,
            repo: Some(REPO.into()),
        },
    )
    .await;
    let v = match resp {
        Response::ZavetWhy(v) => v,
        other => panic!("expected ZavetWhy, got {other:?}"),
    };
    assert_eq!(v.matched_query, None, "an id lookup is not a search");

    // Knowledge: the record itself.
    assert_eq!(v.decision.id, "D-0001");
    assert_eq!(v.decision.title.as_deref(), Some("Poll git"));
    assert_eq!(v.decision.guards, vec!["src/**".to_string()]);
    assert!(v.body_md.as_deref().unwrap().contains("poll"));
    assert_eq!(v.commits.len(), 2);

    // Cost: exactly s1, priced by the same accounting the reports use.
    assert_eq!(v.sessions.len(), 1);
    let line = &v.sessions[0];
    assert_eq!(line.session_id, "s1");
    let events = state.store.events_since(None).await.unwrap();
    let human_signals: Vec<(OffsetDateTime, String)> = events
        .iter()
        .filter(|e| e.kind.is_human_signal())
        .map(|e| (e.at, e.session_id.clone()))
        .collect();
    let expected_human = accounting::per_key_seconds(&human_signals, state.config.idle())
        .get("s1")
        .copied()
        .unwrap();
    assert!(expected_human > 0, "the seeded burst must count human time");
    assert_eq!(line.human_seconds, expected_human);
    assert!(line.agent_seconds > 0);
    assert_eq!((line.input_tokens, line.output_tokens), (1000, 2000));
    assert_eq!(v.total_human_seconds, expected_human);

    // Honesty: sha2 (no source session) is reported as unattributed evidence.
    assert_eq!(v.unattributed_commits, 1);
}

#[tokio::test]
async fn short_trailer_refs_join_zero_padded_decisions() {
    let state = test_state().await;
    let cap = ZavetDecisionCapture {
        id: "D-0007".into(),
        title: Some("Poll git".into()),
        status: Some("active".into()),
        path: ".zavet/decisions/D-0007-poll.md".into(),
        ..Default::default()
    };
    state
        .store
        .zavet_upsert_decision(REPO, &cap, "sha1", None, None)
        .await
        .unwrap();

    // The capture path canonicalizes refs at ingestion, so a shorthand
    // `Refs: D-7` in a commit footer lands on the padded record id.
    let trailers = dira_core::zavet::normalize_trailers(&[(
        "Refs".to_string(),
        "see D-7 for the rationale".to_string(),
    )]);
    assert_eq!(trailers.len(), 1);
    assert_eq!(trailers[0].decision_id.as_deref(), Some("D-0007"));
    state
        .store
        .zavet_record_trailers(Some(REPO), "sha2", &trailers)
        .await
        .unwrap();

    let resp = dirad::control::dispatch(
        &state,
        Request::ZavetWhy {
            query: "D-7".into(),
            cwd: None,
            repo: Some(REPO.into()),
        },
    )
    .await;
    let v = match resp {
        Response::ZavetWhy(v) => v,
        other => panic!("expected ZavetWhy, got {other:?}"),
    };
    assert_eq!(v.decision.id, "D-0007");
    assert!(
        v.commits.iter().any(|c| c.sha == "sha2"),
        "the `Refs: D-7` commit must evidence D-0007, got {:?}",
        v.commits
    );
}

#[tokio::test]
async fn zavet_why_resolves_specs_and_links_both_ways() {
    let state = test_state().await;
    // A decision plus the living spec that links it.
    let decision = ZavetDecisionCapture {
        id: "D-0001".into(),
        title: Some("Poll git instead of watching the filesystem".into()),
        status: Some("active".into()),
        path: ".zavet/decisions/D-0001-poll.md".into(),
        body_md: Some("## Decision\nPoll on events.".into()),
        ..Default::default()
    };
    state
        .store
        .zavet_upsert_decision(REPO, &decision, "sha1", None, None)
        .await
        .unwrap();
    let spec = dira_core::store::ZavetSpecCapture {
        slug: "capture-pipeline".into(),
        title: Some("Commit capture pipeline".into()),
        version: 1,
        origin: "session".into(),
        verified: Some(false),
        confidence: "high".into(),
        date: Some("2026-07-16".into()),
        paths: vec!["src/capture/**".into()],
        decisions: vec!["D-0001".into()],
        path: ".zavet/specs/capture-pipeline.md".into(),
        body_md: Some("## Overview\nThe sweep batches trailer parsing.".into()),
        content_hash: None,
    };
    state
        .store
        .zavet_upsert_spec(REPO, &spec, "shaS", None, Some("s1"))
        .await
        .unwrap();
    // A commit carrying `Spec: capture-pipeline` evidences the spec.
    let c = dira_core::project::CapturedCommit {
        sha: "sha3".into(),
        authored_at: None,
        author_email: None,
        author_name: None,
        message: "feat: extend sweep".into(),
        additions: 1,
        deletions: 0,
        patch_id: None,
    };
    state
        .store
        .record_commit(&c, Some(REPO), None, Some("s1"), None)
        .await
        .unwrap();
    state
        .store
        .zavet_record_trailers(
            Some(REPO),
            "sha3",
            &[ZavetTrailer {
                key: "spec".into(),
                value: "capture-pipeline".into(),
                decision_id: None,
            }],
        )
        .await
        .unwrap();

    // An exact slug answers directly with the spec detail.
    let resp = dirad::control::dispatch(
        &state,
        Request::ZavetWhy {
            query: "capture-pipeline".into(),
            cwd: None,
            repo: Some(REPO.into()),
        },
    )
    .await;
    let v = match resp {
        Response::ZavetSpec(v) => v,
        other => panic!("expected ZavetSpec, got {other:?}"),
    };
    assert_eq!(v.matched_query, None, "a slug lookup is not a search");
    assert_eq!(v.spec.slug, "capture-pipeline");
    assert_eq!(v.spec.decisions, vec!["D-0001"]);
    assert!(v.body_md.as_deref().unwrap().contains("batches"));
    assert!(
        v.commits.iter().any(|c| c.sha == "sha3"),
        "the Spec: trailer commit must evidence the spec, got {:?}",
        v.commits
    );
    assert_eq!(v.sessions.len(), 1, "s1 evidences the spec via sha3");

    // Free text whose vocabulary only the spec carries resolves to it.
    let resp = dirad::control::dispatch(
        &state,
        Request::ZavetWhy {
            query: "how does the capture pipeline sweep batch trailers".into(),
            cwd: None,
            repo: Some(REPO.into()),
        },
    )
    .await;
    match resp {
        Response::ZavetSpec(v) => {
            assert_eq!(v.spec.slug, "capture-pipeline");
            assert!(v.matched_query.is_some());
        }
        other => panic!("expected ZavetSpec, got {other:?}"),
    }

    // The decision detail carries the reverse link to its covering spec.
    let resp = dirad::control::dispatch(
        &state,
        Request::ZavetWhy {
            query: "D-0001".into(),
            cwd: None,
            repo: Some(REPO.into()),
        },
    )
    .await;
    match resp {
        Response::ZavetWhy(v) => {
            assert_eq!(v.specs.len(), 1);
            assert_eq!(v.specs[0].slug, "capture-pipeline");
        }
        other => panic!("expected ZavetWhy, got {other:?}"),
    }

    // A query balanced between the decision and the spec (one strong title
    // term each, no 2x winner) returns ranked matches from BOTH pools
    // instead of guessing.
    let resp = dirad::control::dispatch(
        &state,
        Request::ZavetWhy {
            query: "pipeline filesystem".into(),
            cwd: None,
            repo: Some(REPO.into()),
        },
    )
    .await;
    match resp {
        Response::ZavetSearch { hits, specs, .. } => {
            assert!(!hits.is_empty(), "the decision should surface");
            assert!(!specs.is_empty(), "the spec should surface");
        }
        other => panic!("expected mixed ZavetSearch, got {other:?}"),
    }
}

#[tokio::test]
async fn zavet_why_for_an_unknown_decision_is_a_clean_error() {
    let state = test_state().await;
    let resp = dirad::control::dispatch(
        &state,
        Request::ZavetWhy {
            query: "D-9999".into(),
            cwd: None,
            repo: Some(REPO.into()),
        },
    )
    .await;
    match resp {
        Response::Error { message } => assert!(message.contains("D-9999")),
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn zavet_why_answers_free_text_questions() {
    let state = test_state().await;
    // Two decisions with distinct vocabularies.
    let mut poll = ZavetDecisionCapture {
        id: "D-0001".into(),
        title: Some("Poll git instead of watching the filesystem".into()),
        status: Some("active".into()),
        path: ".zavet/decisions/D-0001-poll-not-watch.md".into(),
        body_md: Some("## Decision\nCommit capture polls on agent events.\n## Why\nWatchers add platform-specific failure modes.".into()),
        ..Default::default()
    };
    poll.slug = Some("poll-not-watch".into());
    let wire = ZavetDecisionCapture {
        id: "D-0002".into(),
        title: Some("The attestation wire is content-free".into()),
        status: Some("active".into()),
        path: ".zavet/decisions/D-0002-wire.md".into(),
        body_md: Some("## Decision\nNo content fields on the wire.".into()),
        ..Default::default()
    };
    state
        .store
        .zavet_upsert_decision(REPO, &poll, "sha1", None, None)
        .await
        .unwrap();
    state
        .store
        .zavet_upsert_decision(REPO, &wire, "sha2", None, None)
        .await
        .unwrap();

    // A plain question resolves to the confident hit and says what matched.
    let resp = dirad::control::dispatch(
        &state,
        Request::ZavetWhy {
            query: "why are we polling instead of a filesystem watcher".into(),
            cwd: None,
            repo: Some(REPO.into()),
        },
    )
    .await;
    match resp {
        Response::ZavetWhy(v) => {
            assert_eq!(v.decision.id, "D-0001");
            assert!(v.matched_query.is_some());
        }
        other => panic!("expected ZavetWhy, got {other:?}"),
    }

    // A lowercase short id still answers directly.
    let resp = dirad::control::dispatch(
        &state,
        Request::ZavetWhy {
            query: "d-2".into(),
            cwd: None,
            repo: Some(REPO.into()),
        },
    )
    .await;
    match resp {
        Response::ZavetWhy(v) => assert_eq!(v.decision.id, "D-0002"),
        other => panic!("expected ZavetWhy, got {other:?}"),
    }

    // A genuinely ambiguous query (one strong title term per record, so no
    // 2x-confident winner) returns ranked matches instead of guessing.
    let resp = dirad::control::dispatch(
        &state,
        Request::ZavetWhy {
            query: "wire watching".into(),
            cwd: None,
            repo: Some(REPO.into()),
        },
    )
    .await;
    match resp {
        Response::ZavetSearch { hits, .. } => {
            assert!(hits.len() >= 2, "both decisions should surface");
        }
        other => panic!("expected ZavetSearch, got {other:?}"),
    }

    // An orphan trailer — a micro-decision that never got a record — is
    // findable by text: the trailers ARE the answer when no record matches.
    state
        .store
        .zavet_record_trailers(
            Some(REPO),
            "sha9",
            &[ZavetTrailer {
                key: "why".into(),
                value: "widened the capture timeout for cold NFS volumes".into(),
                decision_id: None,
            }],
        )
        .await
        .unwrap();
    let resp = dirad::control::dispatch(
        &state,
        Request::ZavetWhy {
            query: "why the timeout on NFS volumes".into(),
            cwd: None,
            repo: Some(REPO.into()),
        },
    )
    .await;
    match resp {
        Response::ZavetSearch { hits, trailers, .. } => {
            assert!(hits.is_empty(), "no record should match");
            assert_eq!(trailers.len(), 1);
            assert_eq!(trailers[0].sha, "sha9");
            assert!(trailers[0].value.contains("cold NFS"));
        }
        other => panic!("expected trailer-only ZavetSearch, got {other:?}"),
    }

    // Gibberish is a clean, helpful error.
    let resp = dirad::control::dispatch(
        &state,
        Request::ZavetWhy {
            query: "quantum llama juggling".into(),
            cwd: None,
            repo: Some(REPO.into()),
        },
    )
    .await;
    assert!(matches!(resp, Response::Error { .. }));

    // The wiki overview groups by status and carries counts.
    let resp = dirad::control::dispatch(
        &state,
        Request::ZavetWiki {
            topic: None,
            cwd: None,
            repo: Some(REPO.into()),
        },
    )
    .await;
    match resp {
        Response::ZavetWiki(w) => {
            assert_eq!(w.decisions_total, 2);
            assert_eq!(w.active.len(), 2);
        }
        other => panic!("expected ZavetWiki, got {other:?}"),
    }

    // A wiki topic search returns hits with excerpts.
    let resp = dirad::control::dispatch(
        &state,
        Request::ZavetWiki {
            topic: Some("filesystem watcher".into()),
            cwd: None,
            repo: Some(REPO.into()),
        },
    )
    .await;
    match resp {
        Response::ZavetSearch { hits, .. } => {
            assert_eq!(hits[0].id, "D-0001");
            assert!(hits[0].excerpt.as_deref().unwrap().contains("polls"));
        }
        other => panic!("expected ZavetSearch, got {other:?}"),
    }
}
