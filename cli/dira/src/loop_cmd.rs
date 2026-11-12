//! `dira loop lint [path]`: check a loop recipe (`.dira/loop.yml`).
//!
//! The work is `dira_core::loop_recipe::lint_file`; this module picks the file and
//! prints the results through `dira doctor`'s renderer, so the report shape,
//! the `--json` object and the exit codes are doctor's contract (DIRASH-0022):
//! 0 all clear, 1 any warning, 2 any failure. It reads; it never edits.

use crate::doctor::{self, Check, Level};
use dira_core::loop_recipe::{self, Finding, FindingLevel};
use std::path::{Path, PathBuf};

const LINT: doctor::render::Kind = doctor::render::Kind {
    command: "dira loop lint",
    fail: "the recipe has failures — fix them above",
    warn: "the recipe lints, with warnings",
    ok: "all clear — the recipe lints clean",
};

/// Lint and print. Returns the process exit code.
pub(crate) fn run(path: Option<&Path>, json: bool, verbose: bool) -> i32 {
    let path = resolve(path);
    let checks = check_file(&path);
    let code = doctor::exit_code(&checks);
    if json {
        doctor::render::print_json(&checks, code, &LINT);
    } else {
        println!("\nRecipe {}", path.display());
        doctor::render::print_human(&checks, verbose, code, &LINT);
    }
    code
}

/// No path is `.dira/loop.yml` here; a directory is `<dir>/.dira/loop.yml`.
fn resolve(path: Option<&Path>) -> PathBuf {
    match path {
        None => PathBuf::from(loop_recipe::DEFAULT_PATH),
        Some(p) if p.is_dir() => p.join(loop_recipe::DEFAULT_PATH),
        Some(p) => p.to_path_buf(),
    }
}

fn check_file(path: &Path) -> Vec<Check> {
    loop_recipe::lint_file(path)
        .into_iter()
        .map(to_check)
        .collect()
}

fn to_check(f: Finding) -> Check {
    let summary = match &f.overridden {
        // An acknowledged finding stays on the page with its reason.
        Some(o) => format!("{} [{o}]", f.summary),
        None => f.summary,
    };
    let level = match f.level {
        FindingLevel::Ok => Level::Ok,
        FindingLevel::Skip => Level::Skip,
        FindingLevel::Warn => Level::Warn,
        FindingLevel::Fail => Level::Fail,
    };
    let mut c = Check::new(f.id, level, summary);
    c.remedy = f.remedy;
    if let Some(s) = f.severity {
        c.detail = serde_json::json!({ "severity": s, "overridden": f.overridden });
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, body: &str) -> PathBuf {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
        p
    }

    const DODI: &str = include_str!("../../../examples/loops/dodi-factory.yml");

    #[test]
    fn exit_codes_follow_doctor() {
        let tmp = tempfile::tempdir().unwrap();
        let code = |body: &str| doctor::exit_code(&check_file(&write(tmp.path(), "r.yml", body)));
        assert_eq!(code(DODI), 0);
        // A gate with no enforcer is a medium finding: a warning.
        assert_eq!(
            code(&DODI.replacen("    enforced_by: actions/factory\n", "", 1)),
            1
        );
        // A same-family verdict without the override is a high finding.
        let without_override = DODI.split("\nreview:\n").next().unwrap().to_string() + "\n";
        assert_eq!(code(&without_override), 2);
        assert_eq!(code("not: [a, recipe"), 2);
    }

    #[test]
    fn a_directory_resolves_the_default_file() {
        let tmp = tempfile::tempdir().unwrap();
        let want = write(tmp.path(), ".dira/loop.yml", DODI);
        assert_eq!(resolve(Some(tmp.path())), want);
        assert_eq!(
            doctor::exit_code(&check_file(&resolve(Some(tmp.path())))),
            0
        );
        assert_eq!(resolve(None), PathBuf::from(".dira/loop.yml"));
    }

    #[test]
    fn a_missing_file_is_a_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope.yml");
        let checks = check_file(&missing);
        assert_eq!(doctor::exit_code(&checks), 2);
        assert!(
            checks[0].summary.contains("nope.yml"),
            "{:?}",
            checks[0].summary
        );
    }
}
