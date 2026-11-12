//! The loop recipe v0 schema: `.dira/loop.yml`.
//!
//! These structs ARE the schema (DIRASH-0068). Every one rejects unknown keys,
//! so a typo fails with its line instead of being ignored. The recipe is local
//! configuration: it is read from disk and never crosses the attestation wire.
//!
//! Rule and condition strings (`gate.rule`, `tier.when`, `done_when`) are the
//! loop's own words. v0 does not evaluate them.

use serde::Deserialize;
use std::collections::BTreeMap;

/// One value or a list of them. `state` accepts a list only so L10 can report
/// a fact held in more than one place.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    pub fn as_slice(&self) -> &[T] {
        match self {
            Self::One(v) => std::slice::from_ref(v),
            Self::Many(v) => v,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub version: u32,
    /// `owner/name@N`.
    pub recipe: String,
    pub tracker: Tracker,
    /// State key (stage, risk, effort, mode, priority, …) to its source.
    pub state: BTreeMap<String, OneOrMany<String>>,
    pub lanes: BTreeMap<String, Lane>,
    #[serde(default)]
    pub gates: BTreeMap<String, Gate>,
    pub stages: Vec<Stage>,
    #[serde(default)]
    pub holdout: Option<Holdout>,
    #[serde(default)]
    pub budgets: Option<Budgets>,
    #[serde(default)]
    pub autonomy: Option<Autonomy>,
    #[serde(default)]
    pub kill_switches: Vec<KillSwitch>,
    #[serde(default)]
    pub review: Option<Review>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tracker {
    pub adapter: Adapter,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Adapter {
    Github,
    Gitlab,
    Jira,
    Linear,
    AzureBoards,
}

/// A runtime + model policy + pool + concurrency limit (§6.1).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lane {
    pub runtime: String,
    pub pool: Pool,
    /// Model family: `anthropic`, `openai`, `xai`, `google`, …
    #[serde(default)]
    pub family: Option<String>,
    #[serde(default)]
    pub model: Option<ModelPolicy>,
    #[serde(default)]
    pub max_parallel: Option<u32>,
    /// What the lane's credentials can reach. Absent means not declared.
    #[serde(default)]
    pub reach: Option<Vec<Reach>>,
    #[serde(default)]
    pub pricing: Option<Pricing>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Pool {
    Seat,
    Coworker,
    Metered,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPolicy {
    pub default: String,
    #[serde(default)]
    pub rules: Vec<ModelRule>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRule {
    pub when: String,
    #[serde(rename = "use")]
    pub use_: String,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Reach {
    Repo,
    Org,
    ProductionRead,
    ProductionWrite,
    Deploy,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Pricing {
    Table,
    Reported,
    None,
}

/// A deterministic predicate before a stage or a merge.
///
/// `enforced_by` names what enforces it, one entry or a list: `proba:<check
/// id>` for a check proba observes on every merge, or a workflow, action or
/// required check name, which the recipe declares and nothing observes. A
/// gate can name both: the code that blocks and the proba check that watches.
/// Without any, the rule lives only in a prompt (L4). `required: false` makes
/// the gate advisory; a required gate backed by proba sets that check
/// `required` for the repo.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gate {
    pub rule: String,
    #[serde(default)]
    pub enforced_by: Option<OneOrMany<String>>,
    #[serde(default = "yes")]
    pub required: bool,
}

fn yes() -> bool {
    true
}

/// One entry of `Gate::enforced_by`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Enforcer<'a> {
    /// `proba:<id>`: a change check proba observes and seals per merge.
    Proba(&'a str),
    /// Anything else: code the recipe names but nothing observes.
    Declared,
}

impl Gate {
    /// Every named enforcer; empty when the rule is only a sentence.
    pub fn enforcers(&self) -> Vec<Enforcer<'_>> {
        self.enforced_by
            .as_ref()
            .map(|e| e.as_slice())
            .unwrap_or_default()
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| match s.strip_prefix("proba:") {
                Some(id) => Enforcer::Proba(id.trim()),
                None => Enforcer::Declared,
            })
            .collect()
    }

    /// The proba check ids this gate names.
    pub fn proba_checks(&self) -> Vec<&str> {
        self.enforcers()
            .into_iter()
            .filter_map(|e| match e {
                Enforcer::Proba(id) => Some(id),
                Enforcer::Declared => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stage {
    pub id: String,
    pub kind: StageKind,
    /// `lane:<name>`, `person`, `host` or `script`.
    pub by: String,
    #[serde(default)]
    pub request: Option<String>,
    #[serde(default)]
    pub needs: Vec<String>,
    #[serde(default)]
    pub gates: Vec<String>,
    #[serde(default)]
    pub done_when: Option<String>,
    #[serde(default)]
    pub rounds: Option<Rounds>,
    #[serde(default)]
    pub verdict: Option<Verdict>,
    #[serde(default)]
    pub limits: Option<Limits>,
    #[serde(default)]
    pub tiers: Option<Vec<Tier>>,
}

/// The canonical stage vocabulary of §6.1. A recipe renames a stage with
/// `id` and keeps its `kind`; a stage the vocabulary lacks is `other`.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum StageKind {
    Intake,
    Triage,
    Plan,
    Implement,
    Fix,
    Verify,
    Review,
    Judge,
    Merge,
    Release,
    Dispatch,
    Other,
}

impl StageKind {
    /// Stages whose holder writes the code under review.
    pub fn authors(self) -> bool {
        matches!(self, Self::Implement | Self::Fix)
    }
}

/// Who holds a stage, parsed from `Stage::by`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum By<'a> {
    Lane(&'a str),
    Person,
    Host,
    Script,
}

impl Stage {
    /// `None` when `by` is not one of the four forms.
    pub fn holder(&self) -> Option<By<'_>> {
        match self.by.as_str() {
            "person" => Some(By::Person),
            "host" => Some(By::Host),
            "script" => Some(By::Script),
            s => s
                .strip_prefix("lane:")
                .filter(|n| !n.is_empty())
                .map(By::Lane),
        }
    }

    /// The lane name when an agent lane holds this stage.
    pub fn lane(&self) -> Option<&str> {
        match self.holder() {
            Some(By::Lane(n)) => Some(n),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rounds {
    #[serde(default)]
    pub max: Option<u32>,
    #[serde(default)]
    pub on_limit: Option<String>,
}

/// A machine-readable review result on a head commit.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verdict {
    pub status: String,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default)]
    pub wip: Option<u32>,
    #[serde(default)]
    pub review_wait_ceiling_hours: Option<u32>,
}

/// A merge tier. The first whose `when` matches wins; the one without `when`
/// is the default and comes last.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tier {
    pub name: String,
    #[serde(default)]
    pub when: Option<String>,
    pub action: TierAction,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TierAction {
    Merge,
    Ready,
    Draft,
}

/// Acceptance tests or scenarios the coding agent cannot edit (`frozen`) or,
/// at A4, cannot see (`hidden`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Holdout {
    pub paths: Vec<String>,
    #[serde(default)]
    pub frozen: bool,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub enforced_by: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budgets {
    #[serde(default)]
    pub per_item_usd: Option<f64>,
    #[serde(default)]
    pub per_month_usd: Option<f64>,
    #[serde(default)]
    pub on_limit: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Autonomy {
    pub rung: Rung,
    #[serde(default)]
    pub scope: Option<String>,
    /// proba's enforcement dial for this scope. Absent means derived from the
    /// rung (see [`Autonomy::dial`]).
    #[serde(default)]
    pub dial: Option<Dial>,
}

/// proba's enforcement dial (D-0041): what a failed `required` check does.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Dial {
    Observe,
    Signal,
    Enforce,
}

impl Autonomy {
    /// The stated dial, or the rung's default: A0 and A1 observe, A2 signals,
    /// A3 and up enforce. A loop that merges without a person needs its
    /// required checks to refuse, not to report.
    pub fn dial(&self) -> Dial {
        self.dial.unwrap_or(match self.rung {
            Rung::A0 | Rung::A1 => Dial::Observe,
            Rung::A2 => Dial::Signal,
            Rung::A3 | Rung::A4 => Dial::Enforce,
        })
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rung {
    A0,
    A1,
    A2,
    A3,
    A4,
}

/// One variable or label that stops a class of automation.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KillSwitch {
    pub name: String,
    pub kind: SwitchKind,
    pub scope: SwitchScope,
    /// Stage ids, or `all`.
    pub stops: Vec<String>,
    #[serde(default)]
    pub off_when: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SwitchKind {
    Variable,
    Label,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SwitchScope {
    Org,
    Repo,
    Item,
}

impl KillSwitch {
    pub fn stops_stage(&self, id: &str) -> bool {
        self.stops.iter().any(|s| s == "all" || s == id)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Review {
    #[serde(default)]
    pub overrides: Vec<Override>,
}

/// A recorded change to a check's catalogue severity (§6.5).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Override {
    pub check: String,
    pub severity: Severity,
    pub reason: String,
    #[serde(default)]
    pub record: Option<String>,
}

/// `off`, `advisory` or `required`: the catalogue's vocabulary, shared with
/// proba (DIRASH-0069).
pub use dira_contract::catalogue::CheckSeverity as Severity;
