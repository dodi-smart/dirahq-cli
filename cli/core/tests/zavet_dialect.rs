//! Golden walker over the shared frontmatter-dialect fixtures.
//!
//! `cli/core/testdata/zavet-dialect/` is the CANONICAL copy of the corpus the
//! zavet plugin vendors (its `test/run.sh` executes the same inputs through
//! the awk parsers and diffs the same goldens; its CI compares the two
//! MANIFESTs). This test projects `parse_decision` / `parse_spec` output to
//! the exact TSV the plugin's `zavet list|guards|specs|spec-paths` emit — the
//! executable definition of "same dialect as dira-core::zavet". See the
//! corpus README for the projection rules and the documented out-of-scope
//! divergences.

use dira_core::zavet::{parse_decision, parse_spec};
use std::path::{Path, PathBuf};

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/zavet-dialect")
}

/// All `.md` files in a corpus subdirectory, filename-sorted (the sh glob and
/// this walker must agree on row order). Dot-prefixed files are INCLUDED here
/// — the parsers themselves must reject template files by filename.
fn md_files(sub: &str) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(corpus().join(sub))
        .expect("corpus subdir exists")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .collect();
    files.sort();
    files
}

fn golden(name: &str) -> String {
    std::fs::read_to_string(corpus().join("expected").join(name)).expect("golden exists")
}

fn assert_matches_golden(name: &str, actual: &str) {
    let expected = golden(name);
    assert_eq!(
        expected, actual,
        "\n--- {name}: Rust projection diverges from the shared golden ---\n\
         expected:\n{expected}\nactual:\n{actual}"
    );
}

#[test]
fn decisions_meta_matches_golden() {
    let mut out = String::new();
    for f in md_files("decisions") {
        let text = std::fs::read_to_string(&f).unwrap();
        let rel = format!(
            ".zavet/decisions/{}",
            f.file_name().unwrap().to_str().unwrap()
        );
        let Some(cap) = parse_decision(&text, &rel) else {
            continue; // rejected documents yield no rows, matching the awk
        };
        out.push_str(&format!(
            "{}\t{}\t{}\n",
            cap.id,
            cap.status.as_deref().unwrap_or("active"),
            cap.title.as_deref().unwrap_or("")
        ));
    }
    assert_matches_golden("decisions-meta.tsv", &out);
}

#[test]
fn decisions_guards_matches_golden() {
    let mut out = String::new();
    for f in md_files("decisions") {
        let text = std::fs::read_to_string(&f).unwrap();
        let rel = format!(
            ".zavet/decisions/{}",
            f.file_name().unwrap().to_str().unwrap()
        );
        let Some(cap) = parse_decision(&text, &rel) else {
            continue;
        };
        // Guard rows emit for ACTIVE decisions only (the enforcement surface).
        if cap.status.as_deref().unwrap_or("active") != "active" {
            continue;
        }
        for glob in &cap.guards {
            out.push_str(&format!("{}\t{}\n", cap.id, glob));
        }
    }
    assert_matches_golden("decisions-guards.tsv", &out);
}

#[test]
fn specs_meta_matches_golden() {
    let mut out = String::new();
    for f in md_files("specs") {
        let text = std::fs::read_to_string(&f).unwrap();
        let rel = format!(".zavet/specs/{}", f.file_name().unwrap().to_str().unwrap());
        // Dot-prefixed templates and unclosed fences both reject → no rows.
        let Some(cap) = parse_spec(&text, &rel) else {
            continue;
        };
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            cap.slug,
            cap.origin,
            cap.confidence,
            cap.date.as_deref().unwrap_or(""),
            cap.title.as_deref().unwrap_or("")
        ));
    }
    assert_matches_golden("specs-meta.tsv", &out);
}

#[test]
fn decision_checks_matches_golden() {
    let mut out = String::new();
    for f in md_files("decisions") {
        let text = std::fs::read_to_string(&f).unwrap();
        let rel = format!(
            ".zavet/decisions/{}",
            f.file_name().unwrap().to_str().unwrap()
        );
        let Some(cap) = parse_decision(&text, &rel) else {
            continue;
        };
        // Unlike guards, checks emit for superseded records too: a check is a
        // claim about how the record was verified, not an enforcement surface.
        for c in &cap.checks {
            out.push_str(&format!("{}\t{}\t{}\n", cap.id, c.label, c.command));
        }
    }
    assert_matches_golden("decision-checks.tsv", &out);
}

#[test]
fn spec_checks_matches_golden() {
    let mut out = String::new();
    for f in md_files("specs") {
        let text = std::fs::read_to_string(&f).unwrap();
        let rel = format!(".zavet/specs/{}", f.file_name().unwrap().to_str().unwrap());
        let Some(cap) = parse_spec(&text, &rel) else {
            continue;
        };
        for c in &cap.checks {
            out.push_str(&format!("{}\t{}\t{}\n", cap.slug, c.label, c.command));
        }
    }
    assert_matches_golden("spec-checks.tsv", &out);
}

#[test]
fn spec_paths_matches_golden() {
    let mut out = String::new();
    for f in md_files("specs") {
        let text = std::fs::read_to_string(&f).unwrap();
        let rel = format!(".zavet/specs/{}", f.file_name().unwrap().to_str().unwrap());
        let Some(cap) = parse_spec(&text, &rel) else {
            continue;
        };
        for glob in &cap.paths {
            out.push_str(&format!("{}\t{}\n", cap.slug, glob));
        }
    }
    assert_matches_golden("spec-paths.tsv", &out);
}
