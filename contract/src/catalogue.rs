//! The check catalogue: every check id a Dira product reports, in one format.
//!
//! Two scopes share it (DIRASH-0069):
//!
//! - `loop` checks judge a delivery loop's design offline, from its recipe
//!   (`dira loop lint`, the Letopis vision §6.5 catalogue, ids `L1`..`L15`).
//! - `change` checks judge one merged change from evidence and are sealed into
//!   a hallmark by proba. Their evidence logic lives in the cloud's registry;
//!   this list holds only their identity and defaults, so the open CLI can
//!   validate a `proba:<id>` reference without the network.
//!
//! `default_severity` is the same three-level vocabulary for both scopes, and
//! it means what proba's D-0041 says: what a failure costs. `off` runs and
//! records, `advisory` warns and never blocks, `required` fails.
//!
//! Ids are permanent. Never renumber, rename or reuse one; a retired check
//! stays listed. Every change to the list bumps [`CATALOGUE_VERSION`]. The
//! cloud vendors `contract/check-catalogue.json` and pins its registry to it.
//! This is not a wire type and carries no content: ids, enums, a title and a
//! citation (D-0001).

use serde::Serialize;

/// Bumped on every change to [`CATALOGUE`], additive or not.
pub const CATALOGUE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckScope {
    /// Judges the loop's design, from its recipe, offline.
    Loop,
    /// Judges one merged change, from evidence, sealed into a hallmark.
    Change,
}

/// What a failure costs (proba D-0041), shared by both scopes. A loop recipe's
/// severity overrides and gates speak the same three words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckSeverity {
    Off,
    Advisory,
    Required,
}

/// The §6.5 impact grade of a loop check, which sets its default severity.
/// Change checks have none: their weight is their rung.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Impact {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Serialize)]
pub struct CatalogueEntry {
    pub id: &'static str,
    pub scope: CheckScope,
    pub default_severity: CheckSeverity,
    /// Loop checks only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub impact: Option<Impact>,
    /// Change checks only: the hallmark element it rolls up into.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub element: Option<&'static str>,
    /// Change checks only: the fineness rung it belongs to (585, 750, 999).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rung: Option<u16>,
    pub title: &'static str,
    /// Where the check comes from: a vision section or a decision id.
    pub basis: &'static str,
    /// Loop checks only: the typical improvement, shown as a finding's remedy
    /// by `dira loop lint` and by proba's loop assay alike.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub improvement: Option<&'static str>,
}

/// The shipped file, `contract/check-catalogue.json`.
#[derive(Debug, Clone, Serialize)]
pub struct Catalogue {
    pub catalogue_version: u32,
    pub checks: &'static [CatalogueEntry],
}

pub fn catalogue() -> Catalogue {
    Catalogue {
        catalogue_version: CATALOGUE_VERSION,
        checks: CATALOGUE,
    }
}

pub fn lookup(id: &str) -> Option<&'static CatalogueEntry> {
    CATALOGUE.iter().find(|c| c.id == id)
}

/// A check of the given scope, or `None` when the id is unknown or is the
/// other scope's.
pub fn lookup_in(scope: CheckScope, id: &str) -> Option<&'static CatalogueEntry> {
    lookup(id).filter(|c| c.scope == scope)
}

impl CheckSeverity {
    /// The serialized spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Advisory => "advisory",
            Self::Required => "required",
        }
    }
}

const fn l(
    id: &'static str,
    impact: Impact,
    title: &'static str,
    basis: &'static str,
    improvement: &'static str,
) -> CatalogueEntry {
    CatalogueEntry {
        id,
        scope: CheckScope::Loop,
        // §6.5: critical and high fail, medium and low warn.
        default_severity: match impact {
            Impact::Critical | Impact::High => CheckSeverity::Required,
            Impact::Medium | Impact::Low => CheckSeverity::Advisory,
        },
        impact: Some(impact),
        element: None,
        rung: None,
        title,
        basis,
        improvement: Some(improvement),
    }
}

const fn c(
    id: &'static str,
    element: &'static str,
    rung: u16,
    default_severity: CheckSeverity,
    title: &'static str,
    basis: &'static str,
) -> CatalogueEntry {
    CatalogueEntry {
        id,
        scope: CheckScope::Change,
        default_severity,
        impact: None,
        element: Some(element),
        rung: Some(rung),
        title,
        basis,
        improvement: None,
    }
}

use CheckSeverity::{Advisory, Off, Required};

/// Loop checks first (§6.5 order), then change checks in proba's registry
/// order (rung, then element).
pub const CATALOGUE: &[CatalogueEntry] = &[
    l(
        "L1",
        Impact::High,
        "The author grades its own work",
        "vision §6.5, §13.1 #2",
        "an independent judge of another family, or a non-model validator, as a required verdict",
    ),
    l(
        "L2",
        Impact::High,
        "Agents can weaken the tests that judge them",
        "vision §6.5, §13.1 #4",
        "freeze holdout paths and refuse rounds that touch them with a push guard",
    ),
    l(
        "L3",
        Impact::Critical,
        "Agent lanes can write to production",
        "vision §6.5, §13.1 #6",
        "scope agent credentials to the repo; move production needs into a read-only observe lane",
    ),
    l(
        "L4",
        Impact::Medium,
        "Rules live only in a prompt",
        "vision §6.5, §13.1 #3",
        "back each gate with a proba check (`proba:<id>`), or name the hook, check or branch rule that enforces it",
    ),
    l(
        "L5",
        Impact::Medium,
        "Implementation starts without a plan",
        "vision §6.5, §13.1 #1",
        "make the implement stage need a plan stage",
    ),
    l(
        "L6",
        Impact::High,
        "Throughput outruns review",
        "vision §6.5, §13.1 #5",
        "give the dispatcher a WIP cap and a review-wait ceiling",
    ),
    l(
        "L7",
        Impact::High,
        "Autonomy above its evidence",
        "vision §6.5, §5",
        "lower the tier, or collect the outcome evidence the next rung needs",
    ),
    l(
        "L8",
        Impact::High,
        "No way to stop",
        "vision §6.5, principle 9",
        "a kill switch per automation class, and a round cap on every fix loop",
    ),
    l(
        "L9",
        Impact::Medium,
        "Spend nobody can explain",
        "vision §6.5, §8",
        "price every agent run",
    ),
    l(
        "L10",
        Impact::Low,
        "State smeared across labels and fields",
        "vision §6.5, DODI-00001",
        "one source per fact: fields as state, labels as requests",
    ),
    l(
        "L11",
        Impact::Medium,
        "Specs drift from code",
        "vision §6.5, zavet",
        "a spec staleness check",
    ),
    l(
        "L12",
        Impact::Medium,
        "Model changes go unmeasured",
        "vision §6.5, §13.1 #7",
        "mark model changes and compare the next 20 merges",
    ),
    l(
        "L13",
        Impact::Low,
        "Tool and instruction sprawl",
        "vision §6.5, §13.1 #7",
        "prune to what sessions use",
    ),
    l(
        "L14",
        Impact::Medium,
        "Questions rot",
        "vision §6.5, §12.4",
        "route questions; a waiting-on-people view",
    ),
    l(
        "L15",
        Impact::High,
        "Stuck required checks get resolved silently",
        "vision §6.5, DODI-00062",
        "a recorded sign-off path; no bypass actor on the ruleset",
    ),
    // 585: what the code host can see by itself.
    c(
        "host.ci",
        "ci",
        585,
        Required,
        "CI green at merge",
        "proba D-0041",
    ),
    c(
        "host.review",
        "review",
        585,
        Required,
        "Review by a non-author human",
        "proba D-0041",
    ),
    c(
        "host.review_current",
        "review",
        585,
        Required,
        "The approval covers what merged",
        "proba D-0046",
    ),
    c(
        "host.machine_review",
        "review",
        585,
        Advisory,
        "Reviewed by an app",
        "proba D-0041",
    ),
    c(
        "host.branch_rules",
        "review",
        585,
        Off,
        "The branch demanded a review or a check",
        "proba D-0041",
    ),
    c(
        "host.agent_trailer",
        "authorship",
        585,
        Advisory,
        "Agent authorship declared",
        "proba D-0041",
    ),
    c(
        "host.self_merge",
        "review",
        585,
        Off,
        "Not merged by its own author",
        "proba D-0041",
    ),
    c(
        "host.signed_commits",
        "authorship",
        585,
        Off,
        "Merge commit signed",
        "proba D-0041",
    ),
    // 750: dira capturing on the workstation.
    c(
        "dira.authorship",
        "authorship",
        750,
        Required,
        "Authorship from a signed session",
        "proba D-0041",
    ),
    c(
        "dira.supervision",
        "supervision",
        750,
        Required,
        "Supervision minutes recorded",
        "proba D-0041",
    ),
    c(
        "dira.sessions",
        "authorship",
        750,
        Advisory,
        "Every commit accounted for",
        "proba D-0041",
    ),
    c(
        "dira.supervision_ratio",
        "supervision",
        750,
        Advisory,
        "Supervision meets your floor",
        "proba D-0045",
    ),
    c(
        "dira.supervisor_not_reviewer",
        "supervision",
        750,
        Advisory,
        "Supervised and reviewed by different people",
        "proba D-0041",
    ),
    // 999: zavet in the repository.
    c(
        "zavet.trailers",
        "intent",
        999,
        Required,
        "Guarded changes carry a trailer",
        "proba D-0044",
    ),
    c(
        "zavet.decisions",
        "intent",
        999,
        Required,
        "Decisions referenced",
        "proba D-0044",
    ),
    c(
        "zavet.guards",
        "intent",
        999,
        Required,
        "Guards respected",
        "proba D-0044",
    ),
    c(
        "zavet.specs",
        "intent",
        999,
        Advisory,
        "Covering spec named",
        "proba D-0044",
    ),
    c(
        "zavet.spec_current",
        "intent",
        999,
        Advisory,
        "Covering spec current at merge",
        "proba D-0044",
    ),
    c(
        "zavet.audit",
        "intent",
        999,
        Advisory,
        "Knowledge audit clean",
        "proba D-0044",
    ),
    c(
        "zavet.verify",
        "intent",
        999,
        Advisory,
        "Bound checks pass",
        "proba D-0044",
    ),
    c(
        "zavet.trailer_matches",
        "intent",
        999,
        Advisory,
        "The cited decision governs these paths",
        "proba D-0044",
    ),
    c(
        "zavet.decision_current",
        "intent",
        999,
        Advisory,
        "The cited decision was still current",
        "proba D-0044",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn ids_are_unique() {
        let mut seen = BTreeSet::new();
        for c in CATALOGUE {
            assert!(seen.insert(c.id), "{} is listed twice", c.id);
        }
    }

    #[test]
    fn each_scope_carries_only_its_own_fields() {
        for c in CATALOGUE {
            match c.scope {
                CheckScope::Loop => {
                    assert!(
                        c.impact.is_some()
                            && c.improvement.is_some()
                            && c.element.is_none()
                            && c.rung.is_none(),
                        "{}",
                        c.id
                    );
                    assert!(c.id.starts_with('L'), "{}", c.id);
                }
                CheckScope::Change => {
                    assert!(c.impact.is_none() && c.improvement.is_none(), "{}", c.id);
                    assert!(matches!(c.rung, Some(585 | 750 | 999)), "{}", c.id);
                    assert!(
                        matches!(
                            c.element,
                            Some("ci" | "review" | "authorship" | "supervision" | "intent")
                        ),
                        "{}",
                        c.id
                    );
                    // `namespace.name`, like proba's registry.
                    assert!(
                        c.id.split_once('.')
                            .is_some_and(|(a, b)| !a.is_empty() && !b.is_empty()),
                        "{}",
                        c.id
                    );
                }
            }
        }
    }

    #[test]
    fn every_loop_check_from_l1_to_l15_is_listed() {
        for n in 1..=15 {
            let id = format!("L{n}");
            assert_eq!(lookup(&id).map(|c| c.scope), Some(CheckScope::Loop), "{id}");
        }
    }
}
