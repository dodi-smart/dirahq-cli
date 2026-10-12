//! `dira start --spec <slug>`: the slug rides the manual-start event, is stamped
//! on every later event of the session by the writer, and only a slug (never
//! prose) is accepted.

use dira_core::protocol::{Request, Response};
use dira_core::{Config, Store};
use dirad::state::EventMsg;

async fn start(spec: Option<&str>) -> (Response, Option<dira_core::model::RawEvent>) {
    let store = Store::open_in_memory().await.expect("in-memory store");
    let (state, mut rx, _sync_rx, _knowledge_rx) = dirad::build_state(store, Config::default())
        .await
        .expect("build_state");
    let resp = dirad::control::dispatch(
        &state,
        Request::Start {
            project: Some("github.com/acme/api".into()),
            label: None,
            activity: None,
            note: None,
            cwd: None,
            spec: spec.map(str::to_string),
        },
    )
    .await;
    let ev = match rx.try_recv() {
        Ok(EventMsg::Raw(ev)) => Some(*ev),
        _ => None,
    };
    (resp, ev)
}

#[tokio::test]
async fn start_threads_the_spec_slug_onto_the_manual_start_event() {
    let (resp, ev) = start(Some("openspec:add-csv-import")).await;
    assert!(matches!(resp, Response::Started { .. }), "{resp:?}");
    assert_eq!(
        ev.expect("a start event was queued").spec.as_deref(),
        Some("openspec:add-csv-import")
    );
    let (_, ev) = start(None).await;
    assert_eq!(ev.unwrap().spec, None);
}

#[tokio::test]
async fn start_refuses_anything_that_is_not_a_slug() {
    for bad in [
        "two words",
        "has\nnewline",
        "",
        "semi;colon",
        &"x".repeat(201),
    ] {
        let (resp, ev) = start(Some(bad)).await;
        assert!(
            matches!(resp, Response::Error { .. }),
            "{bad:?} -> {resp:?}"
        );
        assert!(ev.is_none(), "{bad:?} must not queue an event");
    }
}
