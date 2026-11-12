//! The §6.5 loop review checks that v0 can decide from the recipe alone.
//!
//! A check whose inputs are not in the recipe skips; it never fails
//! (DIRASH-0022). Each check states a fact about the recipe and the practice
//! it departs from. None of them reads content or ranks people.
//!
//! Ids, titles, default severities and improvements come from the shared check
//! catalogue in `/contract` (DIRASH-0069). This module owns only the rules.

use super::schema::{
    By, Enforcer, Reach, Recipe, Rung, Severity, StageKind, SwitchScope, TierAction,
};
use super::{Finding, FindingLevel, Overridden};
use dira_contract::catalogue::{self, CatalogueEntry, CheckScope, CATALOGUE_VERSION};
use std::collections::BTreeSet;

/// How a loop check is answered here.
enum Rule {
    /// From the recipe alone.
    Offline(fn(&Recipe) -> Outcome),
    /// Needs observed data, so it always skips offline; the cloud's loop
    /// assay answers it from proba's hallmark history. The text names the
    /// missing input.
    Observed(&'static str),
}

/// One rule per catalogue loop check. `rules_match_the_catalogue` pins the
/// two lists to each other in both directions.
const RULES: &[(&str, Rule)] = &[
    ("L1", Rule::Offline(l1)),
    ("L2", Rule::Offline(l2)),
    ("L3", Rule::Offline(l3)),
    ("L4", Rule::Offline(l4)),
    ("L5", Rule::Offline(l5)),
    ("L6", Rule::Offline(l6)),
    ("L7", Rule::Offline(l7)),
    ("L8", Rule::Offline(l8)),
    ("L9", Rule::Offline(l9)),
    ("L10", Rule::Offline(l10)),
    ("L11", Rule::Observed("spec units against changed paths")),
    (
        "L12",
        Rule::Observed("model changes and the merges around them"),
    ),
    (
        "L13",
        Rule::Observed("skill and tool use from session metadata"),
    ),
    ("L14", Rule::Observed("how long items wait on people")),
    (
        "L15",
        Rule::Observed("required check runs, merges and ruleset bypasses"),
    ),
];

/// What a check concluded, before overrides.
enum Outcome {
    Pass(String),
    Skip(String),
    /// A finding at the catalogue's default severity.
    Found(String),
    /// A finding no stricter than this severity.
    Capped(Severity, String),
}

/// The catalogue's loop checks, in order.
pub(super) fn loop_entries() -> impl Iterator<Item = &'static CatalogueEntry> {
    catalogue::CATALOGUE
        .iter()
        .filter(|c| c.scope == CheckScope::Loop)
}

pub(super) fn run(r: &Recipe) -> Vec<Finding> {
    loop_entries()
        .map(|entry| {
            let outcome = match RULES.iter().find(|(id, _)| *id == entry.id) {
                Some((_, Rule::Offline(check))) => check(r),
                Some((_, Rule::Observed(what))) => {
                    Outcome::Skip(format!("needs observed data: {what}"))
                }
                // A catalogue newer than this module's rules: say so, never fail.
                None => Outcome::Skip("this dira has no rule for it yet".into()),
            };
            finish(r, entry, outcome)
        })
        .collect()
}

/// Apply the recipe's override, then map severity to a level: `required`
/// fails, `advisory` warns, `off` is recorded and passes.
fn finish(r: &Recipe, entry: &'static CatalogueEntry, outcome: Outcome) -> Finding {
    let (severity, summary) = match outcome {
        Outcome::Pass(s) => return Finding::new(entry.id, FindingLevel::Ok, s),
        Outcome::Skip(s) => return Finding::new(entry.id, FindingLevel::Skip, s),
        Outcome::Found(s) => (entry.default_severity, s),
        Outcome::Capped(cap, s) => (entry.default_severity.min(cap), s),
    };
    let ov = r
        .review
        .as_ref()
        .and_then(|rv| rv.overrides.iter().find(|o| o.check == entry.id));
    let effective = ov.map_or(severity, |o| o.severity);
    let level = match effective {
        Severity::Required => FindingLevel::Fail,
        Severity::Advisory => FindingLevel::Warn,
        Severity::Off => FindingLevel::Ok,
    };
    let mut f = Finding::new(entry.id, level, format!("{}: {summary}", entry.title));
    f.severity = Some(effective);
    f.remedy = entry.improvement.map(str::to_string);
    f.overridden = ov.map(|o| Overridden {
        from: severity,
        to: effective,
        reason: o.reason.trim().to_string(),
        record: o.record.clone(),
    });
    f
}

/// Lanes that hold a stage of the given kinds, in stage order.
fn lanes_of(r: &Recipe, pred: impl Fn(StageKind) -> bool) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for l in r
        .stages
        .iter()
        .filter(|s| pred(s.kind))
        .filter_map(|s| s.lane())
    {
        if !out.contains(&l) {
            out.push(l);
        }
    }
    out
}

/// Every lane that holds a stage.
fn agent_lanes(r: &Recipe) -> Vec<&str> {
    lanes_of(r, |_| true)
}

/// " (N undeclared)" when some lanes left a field out.
fn undeclared(n: usize) -> String {
    if n == 0 {
        String::new()
    } else {
        format!(" ({n} undeclared)")
    }
}

fn family<'a>(r: &'a Recipe, lane: &str) -> Option<&'a str> {
    r.lanes.get(lane)?.family.as_deref()
}

fn l1(r: &Recipe) -> Outcome {
    let authors = lanes_of(r, StageKind::authors);
    if authors.is_empty() {
        return Outcome::Skip("no agent lane authors code".into());
    }
    let mut author_families = BTreeSet::new();
    for l in &authors {
        match family(r, l) {
            Some(f) => {
                author_families.insert(f);
            }
            None => return Outcome::Skip(format!("authoring lane `{l}` declares no family")),
        }
    }
    let required: Vec<_> = r
        .stages
        .iter()
        .filter(|s| s.verdict.as_ref().is_some_and(|v| v.required))
        .collect();
    if required.is_empty() {
        return Outcome::Skip("no stage publishes a required verdict".into());
    }
    let mut same = Vec::new();
    for s in &required {
        let status = &s.verdict.as_ref().expect("filtered").status;
        match s.holder() {
            Some(By::Lane(l)) => match family(r, l) {
                None => return Outcome::Skip(format!("verdict lane `{l}` declares no family")),
                Some(f) if !author_families.contains(f) => {
                    return Outcome::Pass(format!(
                        "required verdict `{status}` comes from `{l}` ({f}), another family than the authors"
                    ))
                }
                Some(_) => same.push(format!("`{status}` from `{l}`")),
            },
            // A person or a deterministic check is not the authoring model.
            _ => {
                return Outcome::Pass(format!(
                    "required verdict `{status}` comes from `{}`, not a model",
                    s.by
                ))
            }
        }
    }
    let fams: Vec<&str> = author_families.into_iter().collect();
    Outcome::Found(format!(
        "every required verdict comes from the authors' family ({}): {}",
        fams.join(", "),
        same.join(", ")
    ))
}

fn l2(r: &Recipe) -> Outcome {
    if lanes_of(r, StageKind::authors).is_empty() {
        return Outcome::Skip("no agent lane authors code".into());
    }
    match &r.holdout {
        None => Outcome::Found("agent lanes author code and the recipe has no holdout".into()),
        Some(h) if !h.frozen => {
            Outcome::Found("the holdout is not frozen against the authoring agent".into())
        }
        Some(h) => match &h.enforced_by {
            None => Outcome::Found("the holdout freeze names nothing that enforces it".into()),
            Some(by) => Outcome::Pass(format!(
                "holdout frozen ({} path{}), enforced by {by}{}",
                h.paths.len(),
                plural(h.paths.len()),
                if h.hidden { ", hidden" } else { "" }
            )),
        },
    }
}

fn l3(r: &Recipe) -> Outcome {
    let lanes = agent_lanes(r);
    let declared: Vec<(&str, &Vec<Reach>)> = lanes
        .iter()
        .filter_map(|l| r.lanes.get(*l)?.reach.as_ref().map(|x| (*l, x)))
        .collect();
    if declared.is_empty() {
        return Outcome::Skip("no agent lane declares its reach".into());
    }
    let prod: Vec<&str> = declared
        .iter()
        .filter(|(_, x)| {
            x.iter()
                .any(|c| matches!(c, Reach::ProductionWrite | Reach::Deploy))
        })
        .map(|(l, _)| *l)
        .collect();
    if !prod.is_empty() {
        return Outcome::Found(format!(
            "lane{} {} can write or deploy to production",
            plural(prod.len()),
            ticks(&prod)
        ));
    }
    let org: Vec<&str> = declared
        .iter()
        .filter(|(_, x)| x.contains(&Reach::Org))
        .map(|(l, _)| *l)
        .collect();
    if !org.is_empty() {
        // Wider than it needs, but not production: a warning, not a failure.
        return Outcome::Capped(
            Severity::Advisory,
            format!(
                "lane{} {} reach the whole org, beyond the repo",
                plural(org.len()),
                ticks(&org)
            ),
        );
    }
    Outcome::Pass(format!(
        "no agent lane reaches production{}",
        undeclared(lanes.len() - declared.len())
    ))
}

fn l4(r: &Recipe) -> Outcome {
    if r.gates.is_empty() {
        return Outcome::Skip("the recipe declares no gates".into());
    }
    let (mut bare, mut unknown, mut not_change) = (Vec::new(), Vec::new(), Vec::new());
    let (mut observed, mut declared) = (0usize, 0usize);
    for (name, g) in &r.gates {
        let enforcers = g.enforcers();
        if enforcers.is_empty() {
            bare.push(name.as_str());
            continue;
        }
        let mut watched = false;
        for e in enforcers {
            let Enforcer::Proba(id) = e else { continue };
            if catalogue::lookup_in(CheckScope::Change, id).is_some() {
                watched = true;
            } else if catalogue::lookup_in(CheckScope::Loop, id).is_some() {
                not_change.push(format!("`{name}` names `proba:{id}`, a loop check"));
            } else {
                unknown.push(format!("`{name}` names `proba:{id}`"));
            }
        }
        if watched {
            observed += 1;
        } else {
            declared += 1;
        }
    }
    let mut gaps = Vec::new();
    if !bare.is_empty() {
        gaps.push(format!("no enforcer named for {}", ticks(&bare)));
    }
    if !unknown.is_empty() {
        gaps.push(format!(
            "{}, not in this dira's check catalogue (v{CATALOGUE_VERSION}); fix the id or update dira",
            unknown.join(", ")
        ));
    }
    if !not_change.is_empty() {
        gaps.push(format!(
            "{}; proba observes change checks only",
            not_change.join(", ")
        ));
    }
    if !gaps.is_empty() {
        return Outcome::Found(gaps.join("; "));
    }
    // Both pass. Only a proba-backed gate is observed on every merge; a
    // declared one is the recipe's word for it.
    Outcome::Pass(format!(
        "every gate names its enforcer: {observed} observed by proba, {declared} declared"
    ))
}

fn l5(r: &Recipe) -> Outcome {
    let implement: Vec<_> = r
        .stages
        .iter()
        .filter(|s| s.kind == StageKind::Implement && s.lane().is_some())
        .collect();
    if implement.is_empty() {
        return Outcome::Skip("no agent implement stage".into());
    }
    let plan_ids: BTreeSet<&str> = r
        .stages
        .iter()
        .filter(|s| matches!(s.kind, StageKind::Plan | StageKind::Triage))
        .map(|s| s.id.as_str())
        .collect();
    let unplanned: Vec<&str> = implement
        .iter()
        .filter(|s| !s.needs.iter().any(|n| plan_ids.contains(n.as_str())))
        .map(|s| s.id.as_str())
        .collect();
    if unplanned.is_empty() {
        Outcome::Pass("every agent implement stage needs a plan stage".into())
    } else {
        Outcome::Found(format!(
            "{} start{} without needing a plan stage",
            ticks(&unplanned),
            if unplanned.len() == 1 { "s" } else { "" }
        ))
    }
}

fn l6(r: &Recipe) -> Outcome {
    let dispatch: Vec<_> = r
        .stages
        .iter()
        .filter(|s| s.kind == StageKind::Dispatch)
        .collect();
    if dispatch.is_empty() {
        return Outcome::Skip("no dispatch stage: work starts on a request".into());
    }
    let mut gaps = Vec::new();
    for s in &dispatch {
        let l = s.limits.as_ref();
        if l.and_then(|l| l.wip).is_none() {
            gaps.push(format!("`{}` has no WIP cap", s.id));
        }
        if l.and_then(|l| l.review_wait_ceiling_hours).is_none() {
            gaps.push(format!("`{}` has no review-wait ceiling", s.id));
        }
    }
    if gaps.is_empty() {
        Outcome::Pass("every dispatch stage has a WIP cap and a review-wait ceiling".into())
    } else {
        Outcome::Found(gaps.join("; "))
    }
}

fn merge_tier_stages(r: &Recipe) -> Vec<&str> {
    r.stages
        .iter()
        .filter(|s| {
            s.tiers
                .as_ref()
                .is_some_and(|t| t.iter().any(|t| t.action == TierAction::Merge))
        })
        .map(|s| s.id.as_str())
        .collect()
}

fn l7(r: &Recipe) -> Outcome {
    let Some(a) = &r.autonomy else {
        return Outcome::Skip("the recipe declares no autonomy rung".into());
    };
    let merges = !merge_tier_stages(r).is_empty();
    let dispatch = r.stages.iter().any(|s| s.kind == StageKind::Dispatch);
    let mut over = Vec::new();
    if a.rung < Rung::A3 {
        if merges {
            over.push("an unattended merge tier");
        }
        if dispatch {
            over.push("a dispatch stage");
        }
    }
    if over.is_empty() {
        Outcome::Pass(format!(
            "rung {:?} allows what the recipe runs; outcome evidence is not checked offline",
            a.rung
        ))
    } else {
        Outcome::Found(format!(
            "rung {:?} is below A3 but the recipe has {}",
            a.rung,
            over.join(" and ")
        ))
    }
}

fn l8(r: &Recipe) -> Outcome {
    if r.kill_switches.is_empty() {
        return Outcome::Found("the recipe has no kill switch".into());
    }
    let class = |id: &str| {
        r.kill_switches
            .iter()
            .any(|k| matches!(k.scope, SwitchScope::Org | SwitchScope::Repo) && k.stops_stage(id))
    };
    let mut gaps = Vec::new();
    for id in merge_tier_stages(r) {
        if !class(id) {
            gaps.push(format!(
                "the merge tier of `{id}` has no org or repo kill switch"
            ));
        }
    }
    for s in r.stages.iter().filter(|s| s.kind == StageKind::Dispatch) {
        if !class(&s.id) {
            gaps.push(format!(
                "dispatch stage `{}` has no org or repo kill switch",
                s.id
            ));
        }
    }
    for s in &r.stages {
        let capped = s.rounds.as_ref().and_then(|x| x.max).is_some();
        if s.rounds.is_some() && !capped {
            gaps.push(format!("`{}` has rounds with no max", s.id));
        } else if s.kind == StageKind::Fix && s.lane().is_some() && !capped {
            gaps.push(format!("fix loop `{}` has no round cap", s.id));
        }
    }
    if gaps.is_empty() {
        Outcome::Pass(format!(
            "{} kill switch{}; merge and dispatch can be stopped; fix loops are capped",
            r.kill_switches.len(),
            if r.kill_switches.len() == 1 { "" } else { "es" }
        ))
    } else {
        Outcome::Found(gaps.join("; "))
    }
}

fn l9(r: &Recipe) -> Outcome {
    let lanes = agent_lanes(r);
    let declared: Vec<(&str, _)> = lanes
        .iter()
        .filter_map(|l| r.lanes.get(*l)?.pricing.map(|p| (*l, p)))
        .collect();
    if declared.is_empty() {
        return Outcome::Skip("no agent lane declares how its runs are priced".into());
    }
    let unpriced: Vec<&str> = declared
        .iter()
        .filter(|(_, p)| *p == super::schema::Pricing::None)
        .map(|(l, _)| *l)
        .collect();
    if unpriced.is_empty() {
        Outcome::Pass(format!(
            "{} agent lane{} price their runs{}",
            declared.len(),
            plural(declared.len()),
            undeclared(lanes.len() - declared.len())
        ))
    } else {
        Outcome::Found(format!(
            "lane{} {} run unpriced",
            plural(unpriced.len()),
            ticks(&unpriced)
        ))
    }
}

fn l10(r: &Recipe) -> Outcome {
    let smeared: Vec<&str> = r
        .state
        .iter()
        .filter(|(_, v)| v.as_slice().len() > 1)
        .map(|(k, _)| k.as_str())
        .collect();
    if smeared.is_empty() {
        Outcome::Pass(format!(
            "each of {} state keys has one source",
            r.state.len()
        ))
    } else {
        Outcome::Found(format!("{} held in more than one place", ticks(&smeared)))
    }
}

use super::plural;

fn ticks(xs: &[&str]) -> String {
    xs.iter()
        .map(|x| format!("`{x}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::super::{lint, FindingLevel};

    /// A minimal recipe that passes every offline check, for tests to break.
    const BASE: &str = r#"
version: 0
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

    fn level(src: &str, id: &str) -> FindingLevel {
        lint(src)
            .into_iter()
            .find(|f| f.id == id)
            .unwrap_or_else(|| panic!("{id} missing: {:#?}", lint(src)))
            .level
    }

    #[test]
    fn rules_match_the_catalogue() {
        let rules: Vec<&str> = super::RULES.iter().map(|(id, _)| *id).collect();
        let catalogue: Vec<&str> = super::loop_entries().map(|c| c.id).collect();
        assert_eq!(
            rules, catalogue,
            "one rule per catalogue loop check, in order"
        );
    }

    #[test]
    fn the_base_recipe_is_clean() {
        for f in lint(BASE) {
            assert!(
                matches!(f.level, FindingLevel::Ok | FindingLevel::Skip),
                "{f:#?}"
            );
        }
    }

    #[test]
    fn a_same_family_required_verdict_fires_l1() {
        let src = BASE.replace("family: openai", "family: anthropic");
        assert_eq!(level(&src, "L1"), FindingLevel::Fail);
        // A person's required verdict is independent of the authoring model.
        let src = BASE.replace("by: \"lane:judge\"", "by: person");
        assert_eq!(level(&src, "L1"), FindingLevel::Ok);
    }

    #[test]
    fn an_override_keeps_the_finding_and_never_raises_the_exit() {
        let src = BASE.replace("family: openai", "family: anthropic")
            + "review:\n  overrides:\n    - { check: L1, severity: off, reason: judge is advisory for now, record: DODI-00065 }\n";
        let f = lint(&src).into_iter().find(|f| f.id == "L1").unwrap();
        assert_eq!(f.level, FindingLevel::Ok);
        let o = f
            .overridden
            .expect("the override is recorded on the finding");
        assert_eq!(
            (o.from, o.to),
            (super::Severity::Required, super::Severity::Off)
        );
        assert_eq!(o.record.as_deref(), Some("DODI-00065"));
        assert!(o.to_string().contains("required -> off"), "{o}");
        assert!(
            f.summary.contains("authors' family"),
            "the finding itself stays visible"
        );
    }

    #[test]
    fn missing_inputs_skip_never_fail() {
        let src = BASE
            .replace(", reach: [repo], pricing: table", "")
            .replace("autonomy: { rung: A3 }\n", "");
        assert_eq!(level(&src, "L3"), FindingLevel::Skip);
        assert_eq!(level(&src, "L7"), FindingLevel::Skip);
        assert_eq!(level(&src, "L9"), FindingLevel::Skip);
        for id in ["L11", "L12", "L13", "L14", "L15"] {
            assert_eq!(level(&src, id), FindingLevel::Skip, "{id}");
        }
    }

    #[test]
    fn production_write_reach_is_critical() {
        let src = BASE.replace("reach: [repo]", "reach: [repo, production-write]");
        let f = lint(&src).into_iter().find(|f| f.id == "L3").unwrap();
        assert_eq!(f.level, FindingLevel::Fail);
        assert_eq!(f.severity, Some(super::Severity::Required));
        // Read-only production reach is the observe lane, and encouraged.
        let src = BASE.replace("reach: [repo]", "reach: [repo, production-read]");
        assert_eq!(level(&src, "L3"), FindingLevel::Ok);
        let src = BASE.replace("reach: [repo]", "reach: [org]");
        assert_eq!(level(&src, "L3"), FindingLevel::Warn);
    }

    #[test]
    fn a_gate_without_enforcer_fires_l4() {
        let src = BASE.replace(", enforced_by: actions/agent-gate", "");
        assert_eq!(level(&src, "L4"), FindingLevel::Warn);
    }

    #[test]
    fn a_proba_backed_gate_is_observed_and_an_unknown_id_is_a_finding() {
        let src = BASE.replace(
            "enforced_by: actions/agent-gate",
            "enforced_by: \"proba:host.review\"",
        );
        let f = lint(&src).into_iter().find(|f| f.id == "L4").unwrap();
        assert_eq!(f.level, FindingLevel::Ok);
        assert!(
            f.summary.contains("1 observed by proba, 0 declared"),
            "{}",
            f.summary
        );

        let src = BASE.replace(
            "enforced_by: actions/agent-gate",
            "enforced_by: \"proba:host.nope\"",
        );
        let f = lint(&src).into_iter().find(|f| f.id == "L4").unwrap();
        assert_eq!(f.level, FindingLevel::Warn);
        assert!(f.summary.contains("proba:host.nope"), "{}", f.summary);
        assert!(f.summary.contains("catalogue (v"), "{}", f.summary);

        // A loop check is not something proba observes on a merge.
        let src = BASE.replace(
            "enforced_by: actions/agent-gate",
            "enforced_by: \"proba:L1\"",
        );
        assert_eq!(level(&src, "L4"), FindingLevel::Warn);
    }

    #[test]
    fn an_implement_stage_without_a_plan_fires_l5() {
        let src = BASE.replace(", needs: [plan]", "");
        assert_eq!(level(&src, "L5"), FindingLevel::Warn);
    }

    #[test]
    fn a_dispatch_stage_without_wip_fires_l6() {
        let with = |limits: &str| {
            BASE.replace(
                "  - { id: plan, kind: plan, by: person }\n",
                &format!(
                    "  - {{ id: plan, kind: plan, by: person }}\n  - {{ id: next, kind: dispatch, by: script{limits} }}\n"
                ),
            )
            .replace("stops: [merge]", "stops: [merge, next]")
        };
        assert_eq!(level(&with(""), "L6"), FindingLevel::Fail);
        assert_eq!(
            level(&with(", limits: { wip: 2 }"), "L6"),
            FindingLevel::Fail
        );
        assert_eq!(
            level(
                &with(", limits: { wip: 2, review_wait_ceiling_hours: 24 }"),
                "L6"
            ),
            FindingLevel::Ok
        );
    }

    #[test]
    fn a_merge_tier_without_a_class_kill_switch_fires_l8() {
        let src = BASE.replace("scope: org", "scope: item");
        assert_eq!(level(&src, "L8"), FindingLevel::Fail);
        let src = BASE.replace("rounds: { max: 3 }", "rounds: {}");
        assert_eq!(level(&src, "L8"), FindingLevel::Fail);
    }

    #[test]
    fn a_merge_tier_below_a3_fires_l7() {
        let src = BASE.replace("rung: A3", "rung: A2");
        assert_eq!(level(&src, "L7"), FindingLevel::Fail);
    }

    #[test]
    fn two_sources_for_one_fact_fire_l10() {
        let src = BASE.replace(
            "state: { stage: \"field:Triage state\" }",
            "state: { stage: [\"field:Triage state\", \"label:triage:\"] }",
        );
        assert_eq!(level(&src, "L10"), FindingLevel::Warn);
    }
}
