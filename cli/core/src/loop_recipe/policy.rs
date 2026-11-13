//! The recipe's proba settings: which change checks it makes `required` or
//! `advisory` for the repo, and the enforcement dial.
//!
//! This is the projection the cloud applies as proba policy (cloud spec
//! `loop-recipe-and-proba`). It holds check ids, two enums and the rung, and
//! no rule text. Computing it is local; sending it is a separate decision
//! (DIRASH-0068).

use super::schema::{Dial, Recipe, Rung, Severity};
use dira_contract::catalogue::{self, CheckScope};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbaPolicy {
    /// The scope's rung, when the recipe states one.
    pub rung: Option<Rung>,
    /// The dial, stated or derived from the rung.
    pub dial: Option<Dial>,
    /// Change check id to the severity the recipe sets for this repo. A check
    /// no gate names is absent: the recipe leaves it to the cascade.
    pub checks: BTreeMap<&'static str, Severity>,
}

/// A required gate sets its proba checks `required`; an advisory one sets them
/// `advisory`. Two gates naming one check: the stricter wins. Ids missing from
/// the catalogue are left out (L4 reports them).
pub fn proba_policy(r: &Recipe) -> ProbaPolicy {
    let mut checks: BTreeMap<&'static str, Severity> = BTreeMap::new();
    for g in r.gates.values() {
        let sev = if g.required {
            Severity::Required
        } else {
            Severity::Advisory
        };
        for id in g.proba_checks() {
            let Some(c) = catalogue::lookup_in(CheckScope::Change, id) else {
                continue;
            };
            let slot = checks.entry(c.id).or_insert(sev);
            *slot = (*slot).max(sev);
        }
    }
    ProbaPolicy {
        rung: r.autonomy.as_ref().map(|a| a.rung),
        dial: r.autonomy.as_ref().map(|a| a.dial()),
        checks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loop_recipe::parse;

    fn recipe(gates: &str, autonomy: &str) -> Recipe {
        parse(&format!(
            "version: 0\nrecipe: a/b@1\ntracker: {{ adapter: github }}\nstate: {{}}\nlanes: {{}}\n\
             gates:\n{gates}stages: [{{ id: s, kind: other, by: person }}]\n{autonomy}"
        ))
        .unwrap()
    }

    #[test]
    fn a_required_gate_makes_its_check_required_and_the_stricter_gate_wins() {
        let gates = [
            "  a: { rule: x, enforced_by: \"proba:host.review\", required: false }\n",
            "  b: { rule: y, enforced_by: [ci, \"proba:host.review\", \"proba:host.ci\"] }\n",
            "  c: { rule: z, enforced_by: \"proba:zavet.specs\", required: false }\n",
            "  d: { rule: w, enforced_by: [\"proba:nope\", \"proba:L1\"] }\n",
        ]
        .concat();
        let r = recipe(&gates, "");
        let p = proba_policy(&r);
        assert_eq!(p.checks.get("host.review"), Some(&Severity::Required));
        assert_eq!(p.checks.get("host.ci"), Some(&Severity::Required));
        assert_eq!(p.checks.get("zavet.specs"), Some(&Severity::Advisory));
        assert_eq!(p.checks.len(), 3, "unknown and loop ids are left out");
        assert_eq!((p.rung, p.dial), (None, None));
    }

    #[test]
    fn the_dial_is_stated_or_derived_from_the_rung() {
        let dial = |a: &str| proba_policy(&recipe("  a: { rule: x }\n", a)).dial;
        assert_eq!(dial("autonomy: { rung: A1 }\n"), Some(Dial::Observe));
        assert_eq!(dial("autonomy: { rung: A2 }\n"), Some(Dial::Signal));
        assert_eq!(dial("autonomy: { rung: A3 }\n"), Some(Dial::Enforce));
        assert_eq!(
            dial("autonomy: { rung: A3, dial: signal }\n"),
            Some(Dial::Signal)
        );
    }
}
