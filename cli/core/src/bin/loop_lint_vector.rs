//! Emit the loop-lint conformance fixture, `contract/testdata/loop-lint-vector.json`.
//!
//! A set of `loop.yml` inputs and, for each, the exact status of the loop
//! checks that need only the recipe file (`L1`..`L10`), computed by the real
//! `dira loop lint` code. The cloud's TypeScript port of those checks tests
//! against this file, so the two implementations cannot drift apart silently
//! (DIRASH-0070). `L11`..`L15` need observed data and are left out.
//!
//! Regenerate with `just loop-vector` (part of `just contract`); CI diffs the
//! output against the committed file. `cli/core/tests/loop_lint_vector.rs`
//! loads this file as a module and checks the same bytes in-process.
//!
//! Each case states its expected `results` in catalogue order, exactly as
//! they ride the knowledge wire. A recipe that does not parse, names an
//! unsupported version or has a broken reference stops before the review
//! checks, so its `results` are empty. Every recipe here is synthetic.

use dira_contract::catalogue::{CheckScope, CATALOGUE, CATALOGUE_VERSION};
use dira_core::loop_recipe::{lint, wire};
use serde_json::{json, Value};

/// The loop checks a recipe alone can decide.
pub const OFFLINE_CHECKS: &[&str] = &["L1", "L2", "L3", "L4", "L5", "L6", "L7", "L8", "L9", "L10"];

/// A minimal recipe that passes every offline check, for cases to break.
const BASE: &str = r#"version: 0
recipe: acme/loop@1
tracker: { adapter: github }
state: { stage: "field:Triage state" }
lanes:
  dev:   { runtime: claude-code-action, pool: metered, family: anthropic, reach: [repo], pricing: table }
  judge: { runtime: codex, pool: metered, family: openai }
gates:
  planned: { rule: 'stage == "Plan ready"', enforced_by: actions/agent-gate }
stages:
  - { id: plan, kind: plan, by: person }
  - { id: build, kind: implement, by: "lane:dev", needs: [plan], gates: [planned] }
  - { id: fix, kind: fix, by: "lane:dev", rounds: { max: 3 } }
  - id: review
    kind: review
    by: "lane:judge"
    verdict: { status: "review / verdict", required: true }
  - id: merge
    kind: merge
    by: host
    tiers:
      - { name: auto, when: "risk == Low", action: merge }
      - { name: draft, action: draft }
holdout: { paths: [.holdout], frozen: true, enforced_by: actions/push-round }
autonomy: { rung: A3 }
kill_switches:
  - { name: AUTOMERGE, kind: variable, scope: org, stops: [merge] }
"#;

/// `(name, recipe)`: each case changes one thing, named for what it exercises.
fn cases() -> Vec<(&'static str, String)> {
    let b = |pairs: &[(&str, &str)]| {
        let mut s = BASE.to_string();
        for (from, to) in pairs {
            assert!(s.contains(from), "BASE lost `{from}`");
            s = s.replacen(from, to, 1);
        }
        s
    };
    let with_dispatch = |limits: &str| {
        b(&[
            (
                "  - { id: plan, kind: plan, by: person }\n",
                &format!(
                    "  - {{ id: plan, kind: plan, by: person }}\n  - {{ id: next, kind: dispatch, by: script{limits} }}\n"
                ),
            ),
            ("stops: [merge]", "stops: [merge, next]"),
        ])
    };
    // L1 fails when the judge shares the author's family; overrides build on it.
    let same_family = b(&[("family: openai", "family: anthropic")]);
    vec![
        ("base-clean", BASE.to_string()),
        (
            "dodi-factory-example",
            include_str!("../../../../examples/loops/dodi-factory.yml").to_string(),
        ),
        // L1: who decides the required verdict.
        ("l1-same-family-verdict-fails", same_family.clone()),
        (
            "l1-override-off-passes",
            same_family.clone()
                + "review:\n  overrides:\n    - { check: L1, severity: off, reason: judge is advisory for now }\n",
        ),
        (
            "l1-override-advisory-warns",
            same_family
                + "review:\n  overrides:\n    - { check: L1, severity: advisory, reason: one family for now }\n",
        ),
        ("l1-person-verdict-passes", b(&[("by: \"lane:judge\"", "by: person")])),
        ("l1-verdict-lane-without-family-skips", b(&[("family: openai", "pricing: table")])),
        (
            "l1-author-without-family-skips",
            b(&[("family: anthropic, ", "")]),
        ),
        (
            "l1-no-required-verdict-skips",
            b(&[("required: true", "required: false")]),
        ),
        // L2: the holdout.
        (
            "l2-no-holdout-fails",
            b(&[("holdout: { paths: [.holdout], frozen: true, enforced_by: actions/push-round }\n", "")]),
        ),
        ("l2-holdout-not-frozen", b(&[("frozen: true", "frozen: false")])),
        (
            "l2-blank-enforcer",
            b(&[("enforced_by: actions/push-round", "enforced_by: \"  \"")]),
        ),
        // L3: reach.
        ("l3-production-write-fails", b(&[("reach: [repo]", "reach: [repo, production-write]")])),
        ("l3-deploy-fails", b(&[("reach: [repo]", "reach: [deploy]")])),
        ("l3-production-read-passes", b(&[("reach: [repo]", "reach: [repo, production-read]")])),
        ("l3-org-reach-warns", b(&[("reach: [repo]", "reach: [org]")])),
        ("l3-undeclared-reach-skips", b(&[("reach: [repo], ", "")])),
        // L4: gate enforcers.
        ("l4-gate-without-enforcer", b(&[(", enforced_by: actions/agent-gate", "")])),
        (
            "l4-proba-change-check-passes",
            b(&[("enforced_by: actions/agent-gate", "enforced_by: \"proba:host.review\"")]),
        ),
        (
            "l4-unknown-proba-id",
            b(&[("enforced_by: actions/agent-gate", "enforced_by: \"proba:host.nope\"")]),
        ),
        (
            "l4-proba-loop-check-id",
            b(&[("enforced_by: actions/agent-gate", "enforced_by: \"proba:L1\"")]),
        ),
        (
            "l4-no-gates-skips",
            b(&[
                (
                    "gates:\n  planned: { rule: 'stage == \"Plan ready\"', enforced_by: actions/agent-gate }\n",
                    "",
                ),
                (", gates: [planned]", ""),
            ]),
        ),
        // L5: plan before implement.
        ("l5-implement-without-plan", b(&[(", needs: [plan]", "")])),
        // L6: dispatch limits.
        ("l6-dispatch-without-limits-fails", with_dispatch("")),
        ("l6-dispatch-without-ceiling-fails", with_dispatch(", limits: { wip: 2 }")),
        (
            "l6-dispatch-with-limits-passes",
            with_dispatch(", limits: { wip: 2, review_wait_ceiling_hours: 24 }"),
        ),
        // L7: the autonomy rung.
        ("l7-merge-tier-below-a3-fails", b(&[("rung: A3", "rung: A2")])),
        ("l7-no-rung-skips", b(&[("autonomy: { rung: A3 }\n", "")])),
        // L8: kill switches and round caps.
        ("l8-item-scope-switch-fails", b(&[("scope: org", "scope: item")])),
        ("l8-uncapped-rounds-fail", b(&[("rounds: { max: 3 }", "rounds: {}")])),
        (
            "l8-no-kill-switch-fails",
            b(&[(
                "kill_switches:\n  - { name: AUTOMERGE, kind: variable, scope: org, stops: [merge] }\n",
                "",
            )]),
        ),
        // L9: pricing.
        ("l9-unpriced-lane-warns", b(&[("pricing: table", "pricing: none")])),
        ("l9-undeclared-pricing-skips", b(&[(", pricing: table", "")])),
        // L10: one source per fact.
        (
            "l10-two-sources-warn",
            b(&[(
                "state: { stage: \"field:Triage state\" }",
                "state: { stage: [\"field:Triage state\", \"label:triage:\"] }",
            )]),
        ),
        // Recipes the lint stops on before the review checks.
        ("stops-unknown-key", b(&[("recipe: ", "stage_kind: x\nrecipe: ")])),
        ("stops-unsupported-version", b(&[("version: 0", "version: 1")])),
        ("stops-dangling-lane", b(&[("by: \"lane:judge\"", "by: \"lane:nope\"")])),
        ("stops-empty-file", String::new()),
    ]
}

/// Build the fixture `main` prints. Split out so the test builds the same value.
pub fn build_vector() -> Value {
    let cases: Vec<Value> = cases()
        .into_iter()
        .map(|(name, recipe)| {
            let results: Vec<Value> = wire::results(&lint(&recipe))
                .into_iter()
                .filter(|r| OFFLINE_CHECKS.contains(&r.id.as_str()))
                // `{id, status}` only: severity and overrides are the wire's
                // business, not the port's conformance.
                .map(|r| json!({ "id": r.id, "status": r.status }))
                .collect();
            json!({
                "name": name,
                "recipe": recipe,
                "recipeBlobOid": wire::git_blob_oid(recipe.as_bytes()),
                "results": results,
            })
        })
        .collect();
    // The fixture is only as good as the catalogue it was cut from.
    assert!(OFFLINE_CHECKS.iter().all(|id| CATALOGUE
        .iter()
        .any(|c| c.id == *id && c.scope == CheckScope::Loop)));
    json!({
        "about": "dira loop lint conformance fixture (DIRASH-0070). Generated by `just loop-vector`; never hand-edit. For each recipe, the exact status of each loop check decidable from the recipe alone, in catalogue order. Empty results: the lint stopped before the review checks (parse, version or reference failure). recipeBlobOid is the git blob oid of the recipe bytes.",
        "catalogueVersion": CATALOGUE_VERSION,
        "checks": OFFLINE_CHECKS,
        "cases": cases,
    })
}

#[allow(dead_code)]
fn main() {
    let v = build_vector();
    println!(
        "{}",
        serde_json::to_string_pretty(&v).expect("vector serializes")
    );
}
