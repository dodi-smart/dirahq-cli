//! Structural validation the types cannot express: references between
//! stages, lanes, gates and kill switches, and the recipe id's shape.

use super::schema::{By, Recipe, StageKind};
use dira_contract::catalogue::{self, CheckScope};
use std::collections::BTreeSet;

/// Every broken reference, as one line each. Empty means the recipe holds
/// together.
pub(super) fn problems(r: &Recipe) -> Vec<String> {
    let mut out = Vec::new();

    if !valid_recipe_id(&r.recipe) {
        out.push(format!(
            "recipe `{}` is not `owner/name@N` (e.g. `dodi-smart/factory@1`)",
            r.recipe
        ));
    }

    for (key, sources) in &r.state {
        for s in sources.as_slice() {
            let ok = ["field:", "label:", "column:", "status:"].iter().any(|p| {
                s.strip_prefix(p)
                    .is_some_and(|rest| !rest.trim().is_empty())
            });
            if !ok {
                out.push(format!(
                    "state.{key}: `{s}` is not `field:`, `label:`, `column:` or `status:` followed by a name"
                ));
            }
        }
    }

    if r.stages.is_empty() {
        out.push("stages: a loop has at least one stage".into());
    }
    let mut seen: Vec<&str> = Vec::new();
    for st in &r.stages {
        if st.id.trim().is_empty() {
            out.push("stages: a stage has an empty id".into());
        } else if seen.contains(&st.id.as_str()) {
            out.push(format!("stages: `{}` is defined twice", st.id));
        }
        match st.holder() {
            None => out.push(format!(
                "stage `{}`: by `{}` is not `lane:<name>`, `person`, `host` or `script`",
                st.id, st.by
            )),
            Some(By::Lane(lane)) if !r.lanes.contains_key(lane) => {
                out.push(format!("stage `{}`: lane `{lane}` is not defined", st.id))
            }
            Some(_) => {}
        }
        for n in &st.needs {
            if !seen.contains(&n.as_str()) {
                out.push(format!(
                    "stage `{}`: needs `{n}`, which is not an earlier stage",
                    st.id
                ));
            }
        }
        for g in &st.gates {
            if !r.gates.contains_key(g) {
                out.push(format!("stage `{}`: gate `{g}` is not defined", st.id));
            }
        }
        if let Some(tiers) = &st.tiers {
            if st.kind != StageKind::Merge {
                out.push(format!(
                    "stage `{}`: tiers belong on a merge stage, not `{:?}`",
                    st.id, st.kind
                ));
            }
            let defaults = tiers.iter().filter(|t| t.when.is_none()).count();
            if defaults != 1 || tiers.last().is_some_and(|t| t.when.is_some()) {
                out.push(format!(
                    "stage `{}`: tiers need exactly one default (a tier with no `when`), and it comes last",
                    st.id
                ));
            }
            let mut names = BTreeSet::new();
            for t in tiers {
                if !names.insert(t.name.as_str()) {
                    out.push(format!(
                        "stage `{}`: tier `{}` is defined twice",
                        st.id, t.name
                    ));
                }
            }
        }
        seen.push(st.id.as_str());
    }

    let mut switches = BTreeSet::new();
    for ks in &r.kill_switches {
        if !switches.insert(ks.name.as_str()) {
            out.push(format!("kill switch `{}` is defined twice", ks.name));
        }
        if ks.stops.is_empty() {
            out.push(format!("kill switch `{}`: stops nothing", ks.name));
        }
        for s in &ks.stops {
            if s != "all" && !seen.contains(&s.as_str()) {
                out.push(format!(
                    "kill switch `{}`: stops `{s}`, which is not a stage (or `all`)",
                    ks.name
                ));
            }
        }
    }

    if let Some(h) = &r.holdout {
        if h.paths.is_empty() {
            out.push("holdout: lists no paths".into());
        }
    }

    if let Some(review) = &r.review {
        let mut named = BTreeSet::new();
        for o in &review.overrides {
            if catalogue::lookup_in(CheckScope::Loop, &o.check).is_none() {
                out.push(format!(
                    "review.overrides: `{}` is not a loop check",
                    o.check
                ));
            }
            if !named.insert(o.check.as_str()) {
                out.push(format!(
                    "review.overrides: `{}` is overridden twice",
                    o.check
                ));
            }
            if let Some(record) = &o.record {
                if crate::zavet::normalize_decision_id(record.trim()).is_none() {
                    out.push(format!(
                        "review.overrides: `{}` cites `{record}`, which is not a decision id (like `D-0042`)",
                        o.check
                    ));
                }
            }
            if o.reason.trim().is_empty() {
                out.push(format!(
                    "review.overrides: `{}` has no reason; an override is a record",
                    o.check
                ));
            }
        }
    }

    out
}

/// Lanes and gates nothing references.
pub(super) fn unused(r: &Recipe) -> Vec<String> {
    let lanes: BTreeSet<&str> = r.stages.iter().filter_map(|s| s.lane()).collect();
    let gates: BTreeSet<&str> = r
        .stages
        .iter()
        .flat_map(|s| s.gates.iter().map(String::as_str))
        .collect();
    let mut out: Vec<String> = r
        .lanes
        .keys()
        .filter(|k| !lanes.contains(k.as_str()))
        .map(|k| format!("lane `{k}`"))
        .collect();
    out.extend(
        r.gates
            .keys()
            .filter(|k| !gates.contains(k.as_str()))
            .map(|k| format!("gate `{k}`")),
    );
    out
}

/// `owner/name@N`: one slash, a non-empty owner and name without spaces, and
/// a whole-number version.
fn valid_recipe_id(id: &str) -> bool {
    let Some((path, ver)) = id.rsplit_once('@') else {
        return false;
    };
    let Some((owner, name)) = path.split_once('/') else {
        return false;
    };
    let part = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    part(owner) && part(name) && !ver.is_empty() && ver.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::valid_recipe_id;

    #[test]
    fn recipe_ids_are_owner_name_at_version() {
        assert!(valid_recipe_id("dodi-smart/factory@1"));
        assert!(valid_recipe_id("rado/loop@12"));
        assert!(!valid_recipe_id("factory@1"));
        assert!(!valid_recipe_id("dodi-smart/factory"));
        assert!(!valid_recipe_id("dodi-smart/factory@v1"));
        assert!(!valid_recipe_id("a/b/c@1"));
        assert!(!valid_recipe_id("dodi smart/factory@1"));
    }
}
