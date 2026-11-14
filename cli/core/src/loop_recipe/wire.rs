//! What of a loop lint may leave the machine (DIRASH-0070).
//!
//! The knowledge channel carries one [`KnowledgeLoopLint`] per repo that holds
//! a recipe: the recipe's path and git blob oid, the catalogue version, and
//! each loop check's id and status, a finding's severity after overrides, and
//! the decision id an override cites. Summaries, remedies, override reasons and
//! the recipe's text stay local (D-0001). The structural `recipe.*` results
//! stay local too: when one stops the lint, `results` is empty.

use super::{lint, Finding, FindingLevel, Severity};
use dira_contract::catalogue::{self, CheckScope, CATALOGUE_VERSION};
use dira_contract::{KnowledgeLoopLint, LoopLintResult, LoopLintSeverity, LoopLintStatus};
use sha1::{Digest, Sha1};

/// `git hash-object` for a file's bytes: SHA-1 of `"blob <len>\0"` + bytes,
/// lowercase hex. The same id GitHub's contents API reports as `sha`.
pub fn git_blob_oid(bytes: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(format!("blob {}\0", bytes.len()).as_bytes());
    h.update(bytes);
    hex::encode(h.finalize())
}

/// A lint level as the wire names it.
fn status(level: FindingLevel) -> LoopLintStatus {
    match level {
        FindingLevel::Ok => LoopLintStatus::Pass,
        FindingLevel::Skip => LoopLintStatus::Skip,
        FindingLevel::Warn => LoopLintStatus::Warn,
        FindingLevel::Fail => LoopLintStatus::Fail,
    }
}

/// The loop checks' results only, in lint (catalogue) order: ids, statuses,
/// a finding's severity, and the decision id behind an override that changed it.
pub fn results(findings: &[Finding]) -> Vec<LoopLintResult> {
    findings
        .iter()
        .filter(|f| catalogue::lookup_in(CheckScope::Loop, f.id).is_some())
        .map(|f| LoopLintResult {
            id: f.id.to_string(),
            status: status(f.level),
            severity: match (f.level, f.severity) {
                (FindingLevel::Warn | FindingLevel::Fail, Some(Severity::Required)) => {
                    Some(LoopLintSeverity::Required)
                }
                (FindingLevel::Warn | FindingLevel::Fail, Some(Severity::Advisory)) => {
                    Some(LoopLintSeverity::Advisory)
                }
                _ => None,
            },
            // Normalized as the refs check validated it, so the cloud can
            // match it to the decision.
            overridden_by: f
                .overridden
                .as_ref()
                .filter(|o| o.from != o.to)
                .and_then(|o| o.record.as_deref())
                .and_then(|r| crate::zavet::normalize_decision_id(r.trim())),
        })
        .collect()
}

/// Lint a recipe's bytes and reduce the result to its wire record. Bytes that
/// are not UTF-8 do not parse, so they yield no results.
pub fn knowledge_loop_lint(
    repo_canonical: &str,
    recipe_path: &str,
    bytes: &[u8],
) -> KnowledgeLoopLint {
    let results = match std::str::from_utf8(bytes) {
        Ok(src) => results(&lint(src)),
        Err(_) => Vec::new(),
    };
    KnowledgeLoopLint {
        repo_canonical: repo_canonical.to_string(),
        recipe_path: recipe_path.to_string(),
        recipe_blob_oid: git_blob_oid(bytes),
        catalogue_version: CATALOGUE_VERSION,
        results,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DODI: &str = include_str!("../../../../examples/loops/dodi-factory.yml");

    #[test]
    fn the_blob_oid_matches_git() {
        // `git hash-object /dev/null` and `printf 'hello\n' | git hash-object --stdin`.
        assert_eq!(
            git_blob_oid(b""),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        assert_eq!(
            git_blob_oid(b"hello\n"),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn only_loop_check_ids_and_statuses_cross() {
        let w = knowledge_loop_lint("github.com/acme/api", ".dira/loop.yml", DODI.as_bytes());
        let ids: Vec<&str> = w.results.iter().map(|r| r.id.as_str()).collect();
        let want: Vec<&str> = catalogue::CATALOGUE
            .iter()
            .filter(|c| c.scope == CheckScope::Loop)
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, want, "L1..L15, in order, no recipe.* ids");
        assert_eq!(w.catalogue_version, CATALOGUE_VERSION);
        assert_eq!(w.recipe_blob_oid, git_blob_oid(DODI.as_bytes()));
        // Nothing the recipe says rides along: no word of it is on the wire.
        let json = serde_json::to_string(&w).unwrap();
        for word in [
            "dodi-smart/factory",
            "anthropic",
            "merge-gate",
            "Plan ready",
        ] {
            assert!(!json.contains(word), "{word} leaked: {json}");
        }
    }

    #[test]
    fn a_finding_carries_its_severity_and_the_override_its_decision_id() {
        // The example turns L1 off with `record: DODI-00065`: it reads pass,
        // names the decision, and carries no severity. The reason stays home.
        let w = knowledge_loop_lint("r", ".dira/loop.yml", DODI.as_bytes());
        let l1 = &w.results[0];
        assert_eq!(l1.status, LoopLintStatus::Pass);
        assert_eq!(l1.overridden_by.as_deref(), Some("DODI-00065"));
        assert_eq!(l1.severity, None);
        assert!(w.results[1..].iter().all(|r| r.overridden_by.is_none()));
        let json = serde_json::to_string(&w).unwrap();
        assert!(!json.contains("Known gap"), "{json}");
        // The id crosses in its normalized form, as the refs check read it.
        let src = DODI.replacen("record: DODI-00065", "record: \" dodi-00065\"", 1);
        assert_ne!(src, DODI, "fixture lost the override record");
        let w = knowledge_loop_lint("r", ".dira/loop.yml", src.as_bytes());
        assert_eq!(w.results[0].overridden_by.as_deref(), Some("DODI-00065"));

        // An unoverridden finding carries the catalogue severity.
        let src = DODI.replacen("pricing: table", "pricing: none", 1);
        let w = knowledge_loop_lint("r", ".dira/loop.yml", src.as_bytes());
        let l9 = w.results.iter().find(|r| r.id == "L9").unwrap();
        assert_eq!(l9.status, LoopLintStatus::Warn);
        assert_eq!(l9.severity, Some(LoopLintSeverity::Advisory));
        // A pass or a skip carries neither field.
        assert!(w
            .results
            .iter()
            .filter(|r| matches!(r.status, LoopLintStatus::Pass | LoopLintStatus::Skip))
            .all(|r| r.severity.is_none()));
    }

    #[test]
    fn a_recipe_that_does_not_hold_together_sends_no_results() {
        for src in [&b"stage_kind: x\n"[..], b"version: 7\n", &[0xff, 0xfe]] {
            let w = knowledge_loop_lint("r", ".dira/loop.yml", src);
            assert!(w.results.is_empty(), "{src:?}: {w:?}");
            assert_eq!(w.recipe_blob_oid, git_blob_oid(src));
        }
    }
}
