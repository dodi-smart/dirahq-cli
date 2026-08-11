//! Rendering for `dira doctor` — one human report, one JSON object.
//!
//! Two constraints inherited from the rest of the CLI's output layer:
//! padding happens *before* painting (SGR bytes have zero display width but
//! count toward `{:<n}`), and `theme::stdout_color()` is false whenever stdout
//! is not a TTY — so a piped report is byte-identical plain text, which is what
//! makes it safe to paste into a bug report.

use super::{Check, Level};
use crate::theme::{self, Role};
use serde::Serialize;

/// Output-format version for this command's stdout.
///
/// Deliberately its own integer and NOT `dira_contract::SCHEMA_VERSION`:
/// doctor is a local CLI surface, not the drift-gated attestation wire.
/// Adding a check id, a `detail` key, or a top-level field is additive and
/// does not bump this. Removing or renaming a field, or changing what a level
/// means for an existing id, does.
const REPORT_SCHEMA: u32 = 1;

#[derive(Serialize)]
struct Report<'a> {
    schema: u32,
    dira: &'static str,
    generated_at: String,
    summary: Summary,
    checks: &'a [Check],
}

#[derive(Serialize)]
struct Summary {
    ok: usize,
    warn: usize,
    fail: usize,
    skip: usize,
    verdict: Level,
    exit_code: i32,
}

fn summarize(checks: &[Check], exit_code: i32) -> Summary {
    let count = |l: Level| checks.iter().filter(|c| c.level == l).count();
    Summary {
        ok: count(Level::Ok),
        warn: count(Level::Warn),
        fail: count(Level::Fail),
        skip: count(Level::Skip),
        verdict: checks.iter().map(|c| c.level).max().unwrap_or(Level::Ok),
        exit_code,
    }
}

/// Exactly one JSON object on stdout, and nothing else.
pub(crate) fn print_json(checks: &[Check], exit_code: i32) {
    let report = Report {
        schema: REPORT_SCHEMA,
        dira: env!("CARGO_PKG_VERSION"),
        generated_at: time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
        summary: summarize(checks, exit_code),
        checks,
    };
    // A single terminal `println!`: stdout is a `LineWriter`, so this is
    // flushed before the caller's `process::exit`. Do not buffer this into a
    // `BufWriter` — the exit would drop it.
    match serde_json::to_string_pretty(&report) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("dira doctor: could not serialize the report: {e}"),
    }
}

/// The glyph and colour for a level.
///
/// Four distinct glyphs, not four colours: piped output has no colour at all,
/// and a report you cannot read once redirected is a report nobody attaches to
/// an issue.
fn mark(level: Level) -> (&'static str, Role) {
    let g = theme::glyphs();
    match level {
        Level::Ok => (g.bullet, Role::Engaged),
        Level::Warn => (g.triangle, Role::Compute),
        Level::Fail => (g.cross, Role::Negative),
        Level::Skip => (g.dot, Role::Faint),
    }
}

pub(crate) fn print_human(checks: &[Check], verbose: bool, exit_code: i32) {
    let shown: Vec<&Check> = checks
        .iter()
        .filter(|c| verbose || c.level != Level::Skip)
        .collect();

    // Width from the ids actually printed, so a `--check` run stays tight.
    let id_w = shown
        .iter()
        .map(|c| c.id.chars().count())
        .max()
        .unwrap_or(0)
        .max(1);

    let gutter = " ".repeat(id_w + 4);
    println!("\n{}", theme::paint("CHECKS", Role::Muted));
    println!();
    for c in &shown {
        let (glyph, role) = mark(c.level);
        // Pad before painting: SGR bytes have zero width.
        let label = format!("{glyph} {:<id_w$}", c.id);
        let summary = if c.level == Level::Skip {
            format!("skipped — {}", c.summary)
        } else {
            c.summary.clone()
        };
        println!(
            "{}  {}",
            theme::paint(&label, role),
            theme::paint(&summary, Role::Ink)
        );
        if let Some(remedy) = &c.remedy {
            // Gutter-align every line of a multi-line remedy: the elevation
            // advice is three steps long and must not lose its shape.
            for line in remedy.lines() {
                println!(
                    "{gutter}{}",
                    theme::paint(&format!("{} {line}", theme::glyphs().arrow), Role::Muted)
                );
            }
        }
        if verbose && !c.detail.is_null() {
            if let Ok(pretty) = serde_json::to_string_pretty(&c.detail) {
                for line in pretty.lines() {
                    println!("{gutter}{}", theme::paint(line, Role::Faint));
                }
            }
        }
    }

    let s = summarize(checks, exit_code);
    println!();
    println!("{}", theme::paint(&counts_line(&s), Role::Muted));
    if !verbose && s.skip > 0 {
        println!(
            "{}",
            theme::paint(
                &format!(
                    "{} check(s) skipped — re-run with --verbose to see them",
                    s.skip
                ),
                Role::Faint
            )
        );
    }
    let (verdict, role) = verdict_line(s.verdict);
    println!("{}", theme::paint(verdict, role));
}

fn counts_line(s: &Summary) -> String {
    format!("{} ok, {} warning(s), {} failure(s)", s.ok, s.warn, s.fail)
}

/// The bottom line, which is the only line most people read.
fn verdict_line(verdict: Level) -> (&'static str, Role) {
    match verdict {
        Level::Fail => (
            "capture is not working — fix the failures above",
            Role::Negative,
        ),
        Level::Warn => ("capture is working, with warnings", Role::Compute),
        _ => ("all clear — capture is working", Role::Engaged),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checks() -> Vec<Check> {
        vec![
            Check::ok("daemon.reachable", "daemon answering"),
            Check::warn("hooks.config", "6/8 events wired").remedy("dira init"),
            Check::fail("daemon.ingress", "port busy").remedy("free the port"),
            Check::skip("store.divergence", "the daemon is not reachable"),
        ]
    }

    #[test]
    fn the_summary_counts_every_level_and_the_verdict_is_the_worst() {
        let s = summarize(&checks(), 2);
        assert_eq!((s.ok, s.warn, s.fail, s.skip), (1, 1, 1, 1));
        assert_eq!(s.verdict, Level::Fail);
        assert_eq!(s.exit_code, 2);
    }

    #[test]
    fn the_json_report_is_one_object_with_a_pinned_schema() {
        let cs = checks();
        let report = Report {
            schema: REPORT_SCHEMA,
            dira: env!("CARGO_PKG_VERSION"),
            generated_at: "2026-08-09T00:00:00Z".into(),
            summary: summarize(&cs, 2),
            checks: &cs,
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&report).expect("serialize"))
                .expect("parse");
        assert_eq!(v["schema"], 1);
        assert_eq!(v["summary"]["exit_code"], 2);
        assert_eq!(v["summary"]["verdict"], "fail");
        let checks = v["checks"].as_array().expect("checks array");
        assert_eq!(checks.len(), 4);
        for c in checks {
            assert!(c["id"].is_string());
            assert!(c["level"].is_string());
            assert!(c["summary"].is_string());
        }
        // Absent optional fields are omitted rather than serialized as null,
        // so a consumer can test presence.
        assert!(checks[0].get("remedy").is_none());
        assert!(checks[0].get("detail").is_none());
        assert_eq!(checks[1]["remedy"], "dira init");
    }

    #[test]
    fn every_level_gets_a_distinct_glyph_so_piped_output_stays_readable() {
        let glyphs: Vec<&str> = [Level::Ok, Level::Warn, Level::Fail, Level::Skip]
            .iter()
            .map(|l| mark(*l).0)
            .collect();
        let mut sorted = glyphs.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), glyphs.len(), "glyphs must be distinct");
    }

    #[test]
    fn the_verdict_line_names_the_worst_level() {
        assert!(verdict_line(Level::Ok).0.contains("all clear"));
        assert!(verdict_line(Level::Skip).0.contains("all clear"));
        assert!(verdict_line(Level::Warn).0.contains("with warnings"));
        assert!(verdict_line(Level::Fail).0.contains("not working"));
    }

    #[test]
    fn counts_line_reads_naturally() {
        let s = summarize(&checks(), 2);
        assert_eq!(counts_line(&s), "1 ok, 1 warning(s), 1 failure(s)");
    }

    /// Every id must fit the 80-column fallback with room for a summary, or
    /// piped reports wrap and stop being pasteable.
    #[test]
    fn check_ids_leave_room_for_a_summary_at_eighty_columns() {
        let widest = super::super::CHECK_IDS
            .iter()
            .map(|s| s.len())
            .max()
            .unwrap_or(0);
        assert!(widest + 4 < 40, "widest id {widest} leaves too little room");
    }
}
