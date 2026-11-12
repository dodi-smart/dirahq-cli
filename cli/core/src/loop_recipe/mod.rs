//! The loop recipe v0 (`.dira/loop.yml`) and its offline lint.
//!
//! A recipe writes a team's delivery loop down as data: stages, lanes, gates,
//! merge tiers, holdouts, round caps and kill switches (Letopis vision §6).
//! [`lint`] parses it, checks its references, and runs the §6.5 review checks
//! that the recipe alone can decide. It reads; it never edits.
//!
//! Local configuration only: the schema lives here, not in `/contract`, and
//! nothing sends a recipe off the machine (DIRASH-0068). Spec:
//! `.zavet/specs/loop-recipe.md`.

pub mod checks;
pub mod policy;
mod refs;
pub mod schema;

pub use schema::{Recipe, Severity};
use serde::Serialize;

/// The recipe file `dira loop lint` reads when given no path.
pub const DEFAULT_PATH: &str = ".dira/loop.yml";

/// The only recipe schema version v0 accepts.
pub const VERSION: u32 = 0;

/// The structural results [`lint`] emits before the catalogue's loop checks.
pub const RECIPE_CHECK_IDS: &[&str] = &[
    "recipe.parse",
    "recipe.version",
    "recipe.refs",
    "recipe.unused",
];

/// A result's level. Ordered like `dira doctor`'s: a skip never outranks a
/// warning, so it can never move the exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FindingLevel {
    Ok,
    Skip,
    Warn,
    Fail,
}

/// One lint result.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub id: &'static str,
    pub level: FindingLevel,
    pub summary: String,
    /// The catalogue's typical improvement, for a finding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
    /// Severity after any override, for a finding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<Severity>,
    /// The override the recipe applied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overridden: Option<Overridden>,
}

/// A recipe's recorded change to a finding's severity (vision §6.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Overridden {
    pub from: Severity,
    pub to: Severity,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<String>,
}

impl std::fmt::Display for Overridden {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} -> {} by the recipe: {}",
            self.from.as_str(),
            self.to.as_str(),
            self.reason
        )?;
        match &self.record {
            Some(r) => write!(f, " ({r})"),
            None => Ok(()),
        }
    }
}

impl Finding {
    fn new(id: &'static str, level: FindingLevel, summary: impl Into<String>) -> Self {
        Self {
            id,
            level,
            summary: summary.into(),
            remedy: None,
            severity: None,
            overridden: None,
        }
    }
}

/// A recipe that does not read or does not match the schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub line: Option<usize>,
    pub column: Option<usize>,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.line, self.column) {
            (Some(l), Some(c)) => write!(f, "line {l}, column {c}: {}", self.message),
            (Some(l), None) => write!(f, "line {l}: {}", self.message),
            _ => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse a recipe. Unknown keys, wrong types and bad values fail with their
/// line where the parser knows it.
pub fn parse(src: &str) -> Result<Recipe, ParseError> {
    serde_norway::from_str(src).map_err(|e| {
        let loc = e.location();
        // The location is folded into Display; keep the bare message.
        let full = e.to_string();
        let message = match full.rsplit_once(" at line ") {
            Some((head, _)) if loc.is_some() => head.to_string(),
            _ => full,
        };
        ParseError {
            message,
            line: loc.as_ref().map(|l| l.line()),
            column: loc.as_ref().map(|l| l.column()),
        }
    })
}

/// Parse, validate and review a recipe. A parse failure stops after
/// `recipe.parse`; a wrong version or a broken reference stops before the
/// review checks, which assume a recipe that holds together.
fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

pub fn lint(src: &str) -> Vec<Finding> {
    let recipe = match parse(src) {
        Ok(r) => r,
        Err(e) => {
            let mut f = Finding::new("recipe.parse", FindingLevel::Fail, e.to_string());
            f.remedy = Some("fix the recipe against .zavet/specs/loop-recipe.md".into());
            return vec![f];
        }
    };
    let mut out = vec![Finding::new(
        "recipe.parse",
        FindingLevel::Ok,
        format!(
            "{} parses: {} stage{}, {} lane{}",
            recipe.recipe,
            recipe.stages.len(),
            plural(recipe.stages.len()),
            recipe.lanes.len(),
            plural(recipe.lanes.len()),
        ),
    )];

    if recipe.version != VERSION {
        let mut f = Finding::new(
            "recipe.version",
            FindingLevel::Fail,
            format!(
                "version {} is not supported; this dira reads recipe version {VERSION}",
                recipe.version
            ),
        );
        f.remedy = Some("update dira, or write the recipe at version 0".into());
        out.push(f);
        return out;
    }
    out.push(Finding::new(
        "recipe.version",
        FindingLevel::Ok,
        format!("version {VERSION}"),
    ));

    let problems = refs::problems(&recipe);
    if !problems.is_empty() {
        out.push(Finding::new(
            "recipe.refs",
            FindingLevel::Fail,
            problems.join("; "),
        ));
        return out;
    }
    out.push(Finding::new(
        "recipe.refs",
        FindingLevel::Ok,
        "every stage, lane, gate and kill switch reference resolves",
    ));

    let unused = refs::unused(&recipe);
    out.push(if unused.is_empty() {
        Finding::new(
            "recipe.unused",
            FindingLevel::Ok,
            "every lane and gate is used",
        )
    } else {
        Finding::new(
            "recipe.unused",
            FindingLevel::Warn,
            format!("nothing references {}", unused.join(", ")),
        )
    });

    out.extend(checks::run(&recipe));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DODI: &str = include_str!("../../../../examples/loops/dodi-factory.yml");

    fn ids(fs: &[Finding]) -> Vec<&str> {
        fs.iter().map(|f| f.id).collect()
    }

    #[test]
    fn the_dodi_factory_example_lints_clean() {
        let fs = lint(DODI);
        let expected: Vec<&str> = RECIPE_CHECK_IDS
            .iter()
            .copied()
            .chain(checks::loop_entries().map(|c| c.id))
            .collect();
        assert_eq!(ids(&fs), expected, "every result, in order");
        for f in &fs {
            assert!(
                matches!(f.level, FindingLevel::Ok | FindingLevel::Skip),
                "{f:#?}"
            );
        }
        // The known L1 gap is acknowledged, not hidden.
        let l1 = fs.iter().find(|f| f.id == "L1").unwrap();
        assert!(l1.overridden.is_some(), "{l1:#?}");
        assert_eq!(l1.severity, Some(Severity::Off));
    }

    #[test]
    fn an_unknown_field_fails_with_a_line() {
        // `stage_kind` is not a recipe key; `stages` is.
        let src = DODI.replacen("recipe: ", "stage_kind: x\nrecipe: ", 1);
        let err = parse(&src).unwrap_err();
        assert!(err.message.contains("stage_kind"), "{err}");
        assert!(err.line.is_some(), "{err:?}");
        let fs = lint(&src);
        assert_eq!(ids(&fs), ["recipe.parse"]);
        assert_eq!(fs[0].level, FindingLevel::Fail);
    }

    #[test]
    fn an_empty_file_fails_parse() {
        for src in ["", "# nothing\n", "- a\n- b\n"] {
            let fs = lint(src);
            assert_eq!(ids(&fs), ["recipe.parse"], "{src:?}");
            assert_eq!(fs[0].level, FindingLevel::Fail, "{src:?}");
        }
    }

    #[test]
    fn a_version_other_than_0_is_refused() {
        let src = DODI.replacen("version: 0", "version: 1", 1);
        let fs = lint(&src);
        assert_eq!(ids(&fs), ["recipe.parse", "recipe.version"]);
        assert_eq!(fs[1].level, FindingLevel::Fail);
    }

    #[test]
    fn dangling_references_fail_refs() {
        let cases = [
            ("by: \"lane:implement\"", "by: \"lane:nope\"", "lane `nope`"),
            ("needs: [triage]", "needs: [review]", "not an earlier stage"),
            (
                "gates: [no-touch, planned",
                "gates: [no-touch, nope",
                "gate `nope`",
            ),
            ("stops: [dispatch]", "stops: [nowhere]", "not a stage"),
            ("check: L1", "check: L99", "not a loop check"),
            (
                "record: DODI-00065",
                "record: see slack",
                "not a decision id",
            ),
        ];
        for (from, to, want) in cases {
            assert!(DODI.contains(from), "fixture lost `{from}`");
            let fs = lint(&DODI.replacen(from, to, 1));
            let refs = fs.iter().find(|f| f.id == "recipe.refs").unwrap();
            assert_eq!(refs.level, FindingLevel::Fail, "{from} -> {to}");
            assert!(
                refs.summary.contains(want),
                "{from} -> {to}: {}",
                refs.summary
            );
        }
    }

    #[test]
    fn an_override_with_no_reason_fails_refs() {
        let src = "version: 0\nrecipe: a/b@1\ntracker: { adapter: github }\nstate: {}\nlanes: {}\n\
                   stages: [{ id: s, kind: other, by: person }]\n\
                   review: { overrides: [{ check: L1, severity: off, reason: \"  \" }] }\n";
        let f = lint(src)
            .into_iter()
            .find(|f| f.id == "recipe.refs")
            .unwrap();
        assert_eq!(f.level, FindingLevel::Fail);
        assert!(f.summary.contains("no reason"), "{f:#?}");
    }

    #[test]
    fn tiers_need_one_trailing_default() {
        let base = "version: 0\nrecipe: a/b@1\ntracker: { adapter: github }\nstate: {}\nlanes: {}\nstages:\n  - id: m\n    kind: merge\n    by: host\n    tiers:\n";
        let bad = [
            "      - { name: a, action: draft }\n      - { name: b, when: x, action: merge }\n",
            "      - { name: a, action: draft }\n      - { name: b, action: draft }\n",
            "      - { name: a, when: x, action: merge }\n",
        ];
        for t in bad {
            let f = lint(&format!("{base}{t}"))
                .into_iter()
                .find(|f| f.id == "recipe.refs")
                .unwrap();
            assert_eq!(f.level, FindingLevel::Fail, "{t}");
        }
        let ok =
            "      - { name: a, when: x, action: merge }\n      - { name: b, action: draft }\n";
        let f = lint(&format!("{base}{ok}"))
            .into_iter()
            .find(|f| f.id == "recipe.refs")
            .unwrap();
        assert_eq!(f.level, FindingLevel::Ok);
    }
}
