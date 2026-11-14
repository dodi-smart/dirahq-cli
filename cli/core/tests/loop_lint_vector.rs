//! Guards on `contract/testdata/loop-lint-vector.json`, the loop-lint
//! conformance fixture the cloud's TypeScript port of L1..L10 tests against
//! (see `cli/core/src/bin/loop_lint_vector.rs`, DIRASH-0070).

#[path = "../src/bin/loop_lint_vector.rs"]
mod loop_lint_vector;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

fn vector_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contract/testdata/loop-lint-vector.json")
}

/// Mirrors the CI drift gate, runnable without `just loop-vector`.
#[test]
fn the_committed_fixture_is_current() {
    let on_disk = std::fs::read_to_string(vector_path()).expect("read loop-lint-vector.json");
    let fresh = serde_json::to_string_pretty(&loop_lint_vector::build_vector()).unwrap() + "\n";
    assert!(
        on_disk == fresh,
        "contract/testdata/loop-lint-vector.json is stale: run `just loop-vector`"
    );
}

/// A fixture that never makes a check fail proves nothing about it. Every
/// offline check must be seen passing and seen not passing.
#[test]
fn every_offline_check_is_exercised_both_ways() {
    let v = loop_lint_vector::build_vector();
    let mut seen: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut stopped = 0;
    for case in v["cases"].as_array().unwrap() {
        let results = case["results"].as_array().unwrap();
        if results.is_empty() {
            stopped += 1;
            continue;
        }
        let ids: Vec<&str> = results.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(ids, loop_lint_vector::OFFLINE_CHECKS, "{}", case["name"]);
        for r in results {
            seen.entry(r["id"].as_str().unwrap().into())
                .or_default()
                .insert(r["status"].as_str().unwrap().into());
        }
    }
    assert!(stopped >= 3, "the stop cases must stay in the fixture");
    for id in loop_lint_vector::OFFLINE_CHECKS {
        let s = &seen[*id];
        assert!(s.contains("pass"), "{id} never passes: {s:?}");
        assert!(
            s.contains("warn") || s.contains("fail"),
            "{id} never fires: {s:?}"
        );
    }
}
