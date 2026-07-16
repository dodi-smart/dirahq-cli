//! Human-friendly rendering of daemon responses.
//!
//! Column widths and bar lengths scale to the terminal width (detected via
//! [`crossterm::terminal::size`]). At the historical 80-column width the layout
//! is byte-for-byte what it was before width-scaling landed, so scripts that
//! parse the plain output (and pipes, which fall back to 80) stay stable.

use crate::format::{
    bar as bar_cells, billing_line, hms, project_label, repo_short, tokens_compact, truncate,
    usd_approx,
};
use crate::theme::{self, Role};
use dira_core::protocol::{
    any_engaged, LiveState, Response, SessionView, StatusView, ZavetDecisionView,
    ZavetGuardStatView, ZavetStatusView, ZavetWhyView,
};
use dira_core::report::Report;
use time::OffsetDateTime;

/// The width the layout was originally hand-tuned for. Used as the fallback when
/// stdout isn't a TTY (pipes/redirects) or the size probe fails, which keeps
/// piped output identical to the legacy fixed layout.
const BASELINE_COLS: u16 = 80;

/// Resolved, clamped column widths for one render pass. Derived from the terminal
/// width so narrow terminals don't wrap and wide ones don't sprawl; at 80 columns
/// every field equals the previous hardcoded constant.
struct Layout {
    /// PROJECT column in the session table (was 28).
    session_project: usize,
    /// Label column in the PARALLEL lanes (was 28).
    parallel_label: usize,
    /// Bar cells in the PARALLEL lanes (was 18).
    bar_cells: usize,
    /// PROJECT column in the report table (was 32).
    report_project: usize,
}

impl Layout {
    /// Build a layout for the given total terminal width. The `+ (cols - 80)`
    /// deltas are the slack beyond the baseline, distributed so that at exactly
    /// 80 columns each width collapses to its original constant.
    fn for_width(cols: u16) -> Self {
        let extra = (cols as i32 - BASELINE_COLS as i32).max(0);
        // Spread the surplus: projects get the bulk, the bar a little.
        let clamp = |base: i32, share: i32, max: i32| (base + share).clamp(base, max) as usize;
        Layout {
            session_project: clamp(28, extra, 60),
            parallel_label: clamp(28, extra, 60),
            bar_cells: clamp(18, extra / 4, 32),
            report_project: clamp(32, extra, 64),
        }
    }
}

/// Detect the usable terminal width, falling back to the baseline 80 columns when
/// stdout is not a TTY or the probe errors. Keeping the fallback at 80 means
/// piped/redirected output is identical to the pre-scaling layout.
fn terminal_cols() -> u16 {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        return BASELINE_COLS;
    }
    match crossterm::terminal::size() {
        Ok((cols, _)) if cols > 0 => cols,
        _ => BASELINE_COLS,
    }
}

/// A proportional bar `width` cells wide; `frac` is clamped to [0, 1]. Thin
/// wrapper over [`crate::format::bar`] that takes a fraction instead of
/// value/max, so the local call sites read unchanged.
fn bar(frac: f64, width: usize) -> String {
    let scaled = (frac.clamp(0.0, 1.0) * 1000.0).round() as i64;
    bar_cells(scaled, 1000, width)
}

/// Print a response, returning a non-zero-worthy bool on error responses.
pub fn print(resp: &Response) -> bool {
    match resp {
        Response::Ok => {
            println!("ok");
            true
        }
        Response::Pong => {
            println!("daemon is up");
            true
        }
        Response::Error { message } => {
            eprintln!("error: {message}");
            false
        }
        Response::Started { handle, project } => {
            println!("started [{handle}] on {}", project_label(project));
            true
        }
        Response::Stopped { count } => {
            println!("stopped {count} session(s)");
            true
        }
        Response::Logged { handle } => {
            println!("logged [{handle}]");
            true
        }
        Response::Status(s) => {
            print_status(s, false);
            true
        }
        Response::Sessions { sessions } => {
            // Drop degenerate sessions — no engaged time AND no agent activity,
            // just empty noise (mirrors the cloud dropping 0-engaged sessions
            // that also have no agent or compute).
            let shown: Vec<SessionView> = sessions
                .iter()
                .filter(|s| s.human_seconds > 0 || s.agent_seconds > 0)
                .cloned()
                .collect();
            print_sessions(&shown, &Layout::for_width(terminal_cols()));
            true
        }
        Response::Report(r) => {
            print_report(r, &Layout::for_width(terminal_cols()));
            true
        }
        Response::Nuked { events, tokens } => {
            println!(
                "Nuked {events} event(s) and {tokens} token row(s). \
                 Fresh slate — start a new session."
            );
            true
        }
        // `dira version` formats this itself (it also prints CLI-side info and
        // skew); it never routes through the generic renderer.
        Response::DaemonInfo { version, .. } => {
            println!("dirad {version}");
            true
        }
        // `dira device resync` prints its own summary; this is a generic fallback.
        Response::ResyncQueued { pending, from } => {
            match from {
                Some(id) => println!("resync queued from {id} — {pending} event(s) will re-sync"),
                None => {
                    println!("resync queued from the beginning — {pending} event(s) will re-sync")
                }
            }
            true
        }
        Response::ZavetStatus(v) => {
            print_zavet_status(v);
            true
        }
        Response::ZavetWhy(v) => {
            print_zavet_why(v);
            true
        }
        Response::ZavetDecisions { decisions } => {
            print_zavet_decisions(decisions);
            true
        }
        Response::ZavetSearch {
            query,
            hits,
            specs,
            trailers,
        } => {
            print_zavet_search(query, hits, specs, trailers);
            true
        }
        Response::ZavetWiki(v) => {
            print_zavet_wiki(v);
            true
        }
        Response::ZavetSpec(v) => {
            print_zavet_spec_why(v);
            true
        }
        Response::ZavetModeSet { repo, mode } => {
            match mode.as_str() {
                "clear" => println!("zavet override cleared for {repo} (follows modules.zavet)"),
                m => println!("zavet forced {m} for {repo}"),
            }
            true
        }
    }
}

// ---------------------------------------------------------------------------
// zavet — knowledge-module renderers. Same design language as `dira status`:
// glyph metric rows, `·` separators, Muted CAPS section headers, Faint empty
// states, and pad-then-paint so SGR bytes never break column alignment.
// ---------------------------------------------------------------------------

/// A `key · key · key` line painted Muted.
fn dots(parts: &[String]) -> String {
    theme::paint(&parts.join(" · "), Role::Muted)
}

/// The `[status]`/verification badges for a decision, pre-painted.
fn zavet_badges(status: Option<&str>, origin: Option<&str>, verified: Option<bool>) -> String {
    let status = status.unwrap_or("active");
    let mut parts = vec![theme::paint(
        status,
        if status == "active" {
            Role::Engaged
        } else {
            Role::Faint
        },
    )];
    if dira_core::zavet::is_unverified(origin, verified) {
        // Amber, deliberately loud: this is a hypothesis, not recorded fact.
        parts.push(theme::paint("unverified — hypothesis", Role::Compute));
    }
    parts.join(&theme::paint(" · ", Role::Muted))
}

/// Guard-event tallies as one dotted line (`3 shown · 1 override`), the
/// `guard_` kind prefix stripped.
fn guard_stats_line(stats: &[ZavetGuardStatView]) -> String {
    stats
        .iter()
        .map(|s| format!("{} {}", s.total, s.kind.trim_start_matches("guard_")))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The first 9 characters of a sha (shorter values pass through whole).
fn short_sha(s: &str) -> &str {
    &s[..s.len().min(9)]
}

/// The `origin · confidence · verification · staleness` badge line for a
/// spec, pre-painted. `verified: true` is the only verified state — the
/// origin badge tells the reader HOW the spec was produced. Staleness
/// (commits touching the spec's paths after its last capture) renders nothing
/// when unknown (`None` — no working dir to ask git in); search hits pass
/// `None`.
fn spec_badges(
    origin: Option<&str>,
    confidence: Option<&str>,
    verified: Option<bool>,
    stale_commits: Option<u64>,
) -> String {
    let mut parts = Vec::new();
    if let Some(o) = origin {
        parts.push(theme::paint(o, Role::Faint));
    }
    if let Some(c) = confidence {
        parts.push(theme::paint(&format!("confidence {c}"), Role::Faint));
    }
    if verified == Some(true) {
        parts.push(theme::paint("✓ verified", Role::Engaged));
    } else {
        // Amber, deliberately loud: no human confirmed spec-matches-code yet.
        parts.push(theme::paint("○ unverified", Role::Compute));
    }
    match stale_commits {
        Some(0) => parts.push(theme::paint("✓ current", Role::Engaged)),
        Some(n) => parts.push(theme::paint(
            &format!("⚠ stale · {n} commit{}", if n == 1 { "" } else { "s" }),
            Role::Compute,
        )),
        None => {}
    }
    parts.join(&theme::paint(" · ", Role::Muted))
}

/// A record body: `## ` headings in the knowledge rose, everything else
/// indented plain. Shared by the decision and spec why views.
fn print_zavet_body(body: &str) {
    println!();
    for line in body.lines() {
        if let Some(h) = line.strip_prefix("## ") {
            println!("  {}", theme::paint(h, Role::Knowledge));
        } else {
            println!("  {line}");
        }
    }
}

/// The COMMITS panel: short sha, truncated subject, day, session badge.
/// Shared by the decision and spec why views.
fn print_zavet_commits(commits: &[dira_core::protocol::ZavetCommitView]) {
    if commits.is_empty() {
        return;
    }
    println!("\n{}", theme::paint("COMMITS", Role::Muted));
    for c in commits {
        let sha = short_sha(&c.sha);
        let day = c.authored_at.as_deref().map(|t| &t[..t.len().min(10)]);
        let sess = match &c.session_id {
            Some(s) => theme::paint(&format!("● {}", &s[..s.len().min(8)]), Role::Engaged),
            None => theme::paint("unattributed", Role::Faint),
        };
        println!(
            "  {}  {}  {} {}",
            theme::paint(sha, Role::Faint),
            theme::paint(
                &truncate(c.message.as_deref().unwrap_or("(not captured)"), 42),
                Role::Ink
            ),
            theme::paint(day.unwrap_or(""), Role::Muted),
            sess,
        );
    }
}

/// Render `dira zavet status`: verdict line, reason, metric rows.
fn print_zavet_status(v: &ZavetStatusView) {
    let verdict = if v.active {
        theme::paint("active", Role::Engaged)
    } else {
        theme::paint("inactive", Role::Faint)
    };
    println!(
        "{} {} {} {}",
        theme::paint("zavet", Role::Knowledge),
        verdict,
        theme::paint("·", Role::Muted),
        theme::paint(&v.repo, Role::Ink),
    );
    let mut why = vec![format!("mode {}", v.knob)];
    if let Some(o) = &v.override_mode {
        why.push(format!("override {o}"));
    }
    if let Some(dir) = v.zavet_dir {
        why.push(if dir { ".zavet/ present" } else { "no .zavet/" }.to_string());
    }
    println!("{}", dots(&why));

    println!();
    let shown = guard_stats_line(&v.guard_stats);
    let rows: [(char, &str, String, String, Role); 3] = [
        (
            '◆',
            "decisions",
            v.decisions_total.to_string(),
            format!("{} active", v.decisions_active),
            Role::Knowledge,
        ),
        (
            '▪',
            "trailers",
            v.trailers.to_string(),
            "micro-decisions in commit footers".to_string(),
            Role::Ink,
        ),
        (
            '●',
            "guards",
            v.guard_events.to_string(),
            if shown.is_empty() {
                "no events yet".to_string()
            } else {
                shown
            },
            Role::Engaged,
        ),
    ];
    let value_w = rows.iter().map(|r| r.2.chars().count()).max().unwrap_or(0);
    for (glyph, label, value, note, role) in rows {
        println!(
            "{} {}   {}",
            theme::paint(&format!("{glyph} {label:<10}"), role),
            theme::paint(&format!("{value:>value_w$}"), Role::Ink),
            theme::paint(&note, Role::Muted),
        );
    }
}

/// Render `dira zavet decisions`: an aligned table, guards underneath.
fn print_zavet_decisions(decisions: &[ZavetDecisionView]) {
    if decisions.is_empty() {
        println!(
            "{}",
            theme::paint(
                "no captured decisions yet — record one with /zavet:decide, or run /zavet:backfill for an existing codebase",
                Role::Faint
            )
        );
        return;
    }
    println!(
        "{}",
        theme::paint(&format!("  {:<9}{:<12}TITLE", "ID", "STATUS"), Role::Muted)
    );
    for d in decisions {
        let status = d.status.as_deref().unwrap_or("active");
        println!(
            "  {}{}{}",
            theme::paint(&format!("{:<9}", d.id), Role::Knowledge),
            theme::paint(
                &format!("{status:<12}"),
                if status == "active" {
                    Role::Engaged
                } else {
                    Role::Faint
                }
            ),
            theme::paint(
                &truncate(d.title.as_deref().unwrap_or("(untitled)"), 46),
                Role::Ink
            ),
        );
        if !d.guards.is_empty() {
            println!(
                "  {}",
                theme::paint(
                    &format!("{:<9}guards {}", "", d.guards.join(" · ")),
                    Role::Faint
                )
            );
        }
    }
}

/// Render `dira zavet why`: the knowledge first, then evidence, then cost.
fn print_zavet_why(v: &ZavetWhyView) {
    let d = &v.decision;
    println!(
        "{} {} {}",
        theme::paint(&d.id, Role::Knowledge),
        theme::paint("·", Role::Muted),
        theme::paint(d.title.as_deref().unwrap_or("(untitled)"), Role::Ink),
    );
    if let Some(q) = &v.matched_query {
        println!("{}", theme::paint(&format!("matched \"{q}\""), Role::Faint));
    }
    println!(
        "{} {} {}",
        zavet_badges(d.status.as_deref(), d.origin.as_deref(), d.verified),
        theme::paint("·", Role::Muted),
        theme::paint(&d.path, Role::Faint),
    );
    if let Some(s) = &d.supersedes {
        println!("{}", theme::paint(&format!("supersedes {s}"), Role::Muted));
    }
    if let Some(s) = &v.superseded_by {
        println!(
            "{}",
            theme::paint(
                &format!("superseded by {s} — read that instead"),
                Role::Negative
            )
        );
    }
    if !d.guards.is_empty() {
        println!(
            "{} {}",
            theme::paint("guards", Role::Muted),
            theme::paint(&d.guards.join(" · "), Role::Ink),
        );
    }
    if !v.specs.is_empty() {
        let refs = v
            .specs
            .iter()
            .map(|s| s.slug.clone())
            .collect::<Vec<_>>()
            .join(" · ");
        println!(
            "{} {}",
            theme::paint("specs", Role::Muted),
            theme::paint(&refs, Role::Knowledge),
        );
    }

    if let Some(body) = &v.body_md {
        print_zavet_body(body);
    }

    print_zavet_commits(&v.commits);

    if !v.guard_stats.is_empty() {
        println!("\n{}", theme::paint("GUARDS", Role::Muted));
        println!(
            "  {}",
            theme::paint(&guard_stats_line(&v.guard_stats), Role::Ink)
        );
    }

    let unattributed = (v.unattributed_commits > 0 || v.unattributed_guard_events > 0).then(|| {
        format!(
            "honest lower bound — unattributed: {} commit(s), {} guard event(s)",
            v.unattributed_commits, v.unattributed_guard_events
        )
    });
    print_zavet_cost(
        &v.sessions,
        v.total_human_seconds,
        v.total_agent_seconds,
        v.total_input_tokens + v.total_output_tokens,
        unattributed.as_deref(),
    );
}

/// The COST panel shared by decision and spec why views: per-session lines
/// when more than one, totals, and the honest-lower-bound note.
fn print_zavet_cost(
    sessions: &[dira_core::protocol::ZavetSessionCostView],
    total_human_seconds: i64,
    total_agent_seconds: i64,
    total_tokens: u64,
    unattributed_note: Option<&str>,
) {
    println!("\n{}", theme::paint("COST", Role::Muted));
    if sessions.is_empty() {
        println!(
            "  {}",
            theme::paint("no attributed sessions yet — cost unknown", Role::Faint)
        );
    } else {
        if sessions.len() > 1 {
            for s in sessions {
                println!(
                    "  {}  {}  {}  {}",
                    theme::paint(
                        &format!("{:<10}", &s.session_id[..s.session_id.len().min(8)]),
                        Role::Muted
                    ),
                    theme::paint(&format!("● {}", hms(s.human_seconds)), Role::Engaged),
                    theme::paint(&format!("◆ {}", hms(s.agent_seconds)), Role::Agent),
                    theme::paint(
                        &format!("◇ {} tok", tokens_compact(s.input_tokens + s.output_tokens)),
                        Role::Compute
                    ),
                );
            }
        }
        println!(
            "  {}   {}   {}",
            theme::paint(
                &format!("● engaged {}", hms(total_human_seconds)),
                Role::Engaged
            ),
            theme::paint(
                &format!("◆ agent {}", hms(total_agent_seconds)),
                Role::Agent
            ),
            theme::paint(
                &format!("◇ compute {} tok", tokens_compact(total_tokens)),
                Role::Compute
            ),
        );
        let n = sessions.len();
        let plural = if n == 1 { "" } else { "s" };
        println!(
            "  {}",
            theme::paint(&format!("across {n} session{plural}"), Role::Muted)
        );
    }
    if let Some(note) = unattributed_note {
        println!("  {}", theme::paint(note, Role::Faint));
    }
}

/// Render `dira zavet why <spec>`: the living document first, then its
/// evidence, then cost — the spec twin of [`print_zavet_why`].
fn print_zavet_spec_why(v: &dira_core::protocol::ZavetSpecWhyView) {
    let s = &v.spec;
    println!(
        "{} {} {}",
        theme::paint(&s.slug, Role::Knowledge),
        theme::paint("·", Role::Muted),
        theme::paint(s.title.as_deref().unwrap_or("(untitled)"), Role::Ink),
    );
    if let Some(q) = &v.matched_query {
        println!("{}", theme::paint(&format!("matched \"{q}\""), Role::Faint));
    }
    println!(
        "{} {} {}",
        spec_badges(
            s.origin.as_deref(),
            s.confidence.as_deref(),
            s.verified,
            s.stale_commits
        ),
        theme::paint("·", Role::Muted),
        theme::paint(&s.path, Role::Faint),
    );
    if !s.paths.is_empty() {
        println!(
            "{} {}",
            theme::paint("paths", Role::Muted),
            theme::paint(&s.paths.join(" · "), Role::Ink),
        );
    }
    if !s.decisions.is_empty() {
        println!(
            "{} {}",
            theme::paint("decisions", Role::Muted),
            theme::paint(&s.decisions.join(" · "), Role::Knowledge),
        );
    }

    if let Some(body) = &v.body_md {
        print_zavet_body(body);
    }

    print_zavet_commits(&v.commits);

    let unattributed = (v.unattributed_commits > 0).then(|| {
        format!(
            "honest lower bound — unattributed: {} commit(s)",
            v.unattributed_commits
        )
    });
    print_zavet_cost(
        &v.sessions,
        v.total_human_seconds,
        v.total_agent_seconds,
        v.total_input_tokens + v.total_output_tokens,
        unattributed.as_deref(),
    );
}

/// Render ranked matches for a free-text `why`/`wiki <topic>` query: decision
/// records first, then living specs, then matching micro-decisions (orphan
/// commit trailers).
fn print_zavet_search(
    query: &str,
    hits: &[dira_core::protocol::ZavetSearchHit],
    specs: &[dira_core::protocol::ZavetSpecHit],
    trailers: &[dira_core::protocol::ZavetTrailerHit],
) {
    if hits.is_empty() && specs.is_empty() && trailers.is_empty() {
        println!(
            "{}",
            theme::paint(
                &format!("nothing recorded matches \"{query}\""),
                Role::Faint
            )
        );
        return;
    }
    let n = hits.len() + specs.len() + trailers.len();
    let plural = if n == 1 { "" } else { "es" };
    println!(
        "{} {}",
        theme::paint(&format!("{n} match{plural}"), Role::Ink),
        theme::paint(&format!("for \"{query}\""), Role::Muted),
    );
    if !hits.is_empty() {
        println!();
        for h in hits {
            println!(
                "  {}  {} {}",
                theme::paint(&format!("{:<8}", h.id), Role::Knowledge),
                theme::paint(
                    &truncate(h.title.as_deref().unwrap_or("(untitled)"), 52),
                    Role::Ink
                ),
                zavet_badges(h.status.as_deref(), None, h.verified),
            );
            if let Some(e) = &h.excerpt {
                println!(
                    "  {}",
                    theme::paint(&format!("{:<8}  {}", "", truncate(e, 60)), Role::Faint)
                );
            }
        }
    }
    if !specs.is_empty() {
        println!(
            "\n{}",
            theme::paint("SPECS (living documents)", Role::Muted)
        );
        for s in specs {
            println!(
                "  {}  {} {}",
                theme::paint(&format!("{:<18}", truncate(&s.slug, 18)), Role::Knowledge),
                theme::paint(
                    &truncate(s.title.as_deref().unwrap_or("(untitled)"), 42),
                    Role::Ink
                ),
                spec_badges(
                    s.origin.as_deref(),
                    s.confidence.as_deref(),
                    s.verified,
                    None
                ),
            );
            if let Some(e) = &s.excerpt {
                println!(
                    "  {}",
                    theme::paint(&format!("{:<18}  {}", "", truncate(e, 50)), Role::Faint)
                );
            }
        }
    }
    if !trailers.is_empty() {
        println!(
            "\n{}",
            theme::paint("MICRO-DECISIONS (commit trailers)", Role::Muted)
        );
        for t in trailers {
            println!(
                "  {}  {} {}",
                theme::paint(short_sha(&t.sha), Role::Faint),
                theme::paint(&format!("{}:", t.key), Role::Knowledge),
                theme::paint(&truncate(&t.value, 56), Role::Ink),
            );
        }
        println!(
            "  {}",
            theme::paint("full context: git show <sha>", Role::Faint)
        );
    }
    let follow_up = hits
        .first()
        .map(|h| h.id.as_str())
        .or_else(|| specs.first().map(|s| s.slug.as_str()));
    if let Some(target) = follow_up {
        println!(
            "\n{}",
            theme::paint(
                &format!("full record + cost: dira zavet why {target}"),
                Role::Faint
            )
        );
    }
}

/// Render `dira zavet wiki`: the knowledge-base overview.
fn print_zavet_wiki(v: &dira_core::protocol::ZavetWikiView) {
    println!(
        "{} {} {}",
        theme::paint("ZAVET", Role::Knowledge),
        theme::paint("·", Role::Muted),
        theme::paint(&v.repo, Role::Ink),
    );
    let plural = |n: u64, s: &str| format!("{n} {s}{}", if n == 1 { "" } else { "s" });
    let mut summary = vec![
        plural(v.decisions_total, "decision"),
        plural(v.trailers, "trailer"),
        plural(v.guard_events, "guard event"),
    ];
    if v.specs_total > 0 {
        summary.push(plural(v.specs_total, "spec"));
    }
    println!("{}", dots(&summary));

    let section = |title: &str, list: &[ZavetDecisionView]| {
        if list.is_empty() {
            return;
        }
        println!("\n{}", theme::paint(title, Role::Muted));
        for d in list {
            let verified = if dira_core::zavet::is_unverified(d.origin.as_deref(), d.verified) {
                theme::paint("○ unverified", Role::Compute)
            } else {
                theme::paint("✓", Role::Engaged)
            };
            println!(
                "  {}  {} {}",
                theme::paint(&format!("{:<8}", d.id), Role::Knowledge),
                theme::paint(
                    &truncate(d.title.as_deref().unwrap_or("(untitled)"), 52),
                    Role::Ink
                ),
                verified,
            );
            if !d.guards.is_empty() {
                println!(
                    "  {}",
                    theme::paint(
                        &format!("{:<8}  guards {}", "", d.guards.join(" · ")),
                        Role::Faint
                    )
                );
            }
        }
    };
    section("ACTIVE DECISIONS", &v.active);
    section("SUPERSEDED", &v.superseded);

    if !v.specs.is_empty() {
        println!("\n{}", theme::paint("SPECS", Role::Muted));
        for s in &v.specs {
            println!(
                "  {}  {} {}",
                theme::paint(&format!("{:<18}", truncate(&s.slug, 18)), Role::Knowledge),
                theme::paint(
                    &truncate(s.title.as_deref().unwrap_or("(untitled)"), 38),
                    Role::Ink
                ),
                spec_badges(
                    s.origin.as_deref(),
                    s.confidence.as_deref(),
                    s.verified,
                    s.stale_commits
                ),
            );
            let mut under: Vec<String> = Vec::new();
            if !s.paths.is_empty() {
                under.push(format!("paths {}", s.paths.join(" · ")));
            }
            if !s.decisions.is_empty() {
                under.push(format!("decisions {}", s.decisions.join(" · ")));
            }
            if !under.is_empty() {
                println!(
                    "  {}",
                    theme::paint(&format!("{:<18}  {}", "", under.join("  ·  ")), Role::Faint)
                );
            }
        }
    }

    if !v.recent.is_empty() {
        println!("\n{}", theme::paint("RECENT KNOWLEDGE", Role::Muted));
        for (sha, key, value) in &v.recent {
            println!(
                "  {}  {}  {}",
                theme::paint(short_sha(sha), Role::Faint),
                theme::paint(&format!("{key:<11}"), Role::Knowledge),
                theme::paint(&truncate(value, 52), Role::Ink),
            );
        }
    }
    if v.decisions_total == 0 {
        println!(
            "\n{}",
            theme::paint(
                "empty knowledge base — /zavet:decide records a decision, /zavet:backfill reverse-engineers an existing codebase",
                Role::Faint
            )
        );
    }
}

/// Render `dira status`: the summary block always; the detail sections
/// (ACTIVE SESSIONS / PARALLEL / TODAY) only under `--detailed`.
pub fn print_status(s: &StatusView, detailed: bool) {
    // Hide degenerate sessions — a bare SessionStart with no engaged time and no
    // agent activity is noise (e.g. a project you opened but didn't work in).
    let active: Vec<SessionView> = s
        .active
        .iter()
        .filter(|v| v.human_seconds > 0 || v.agent_seconds > 0)
        .cloned()
        .collect();

    for line in summary_lines(s, &active, OffsetDateTime::now_utc()) {
        println!("{line}");
    }

    if detailed {
        let layout = Layout::for_width(terminal_cols());
        println!();
        if !active.is_empty() {
            println!("{}", theme::paint("ACTIVE SESSIONS", Role::Muted));
            print_sessions(&active, &layout);
            println!();
            print_parallel(&active, &s.today, &layout);
        }
        println!("{}", theme::paint("TODAY", Role::Muted));
        print_report(&s.today, &layout);
    }

    if s.sync_pending > 0 {
        println!(
            "\n{}",
            theme::paint(
                &format!("{} event(s) pending sync", s.sync_pending),
                Role::Compute,
            )
        );
    }

    // Writer health (WP-B7): only surfaced when there's something to say — a
    // healthy writer (no panics, no stalls, not wedged) prints nothing extra.
    // Omitted entirely for an older daemon (`writer_health: None`, skew-safe).
    if let Some(h) = s.writer_health {
        if h.panics > 0 || h.stalls > 0 || h.wedged {
            let mut line = format!("writer: {} panic(s) caught", h.panics);
            if h.stalls > 0 {
                line.push_str(&format!(", {} stall(s) flagged", h.stalls));
            }
            if h.wedged {
                line.push_str(", currently WEDGED — restart the daemon");
            }
            println!("\n{}", theme::paint(&line, Role::Negative));
        }
    }

    // Sync health (WP-B9): only surfaced when there's something to say — a
    // healthy sync (no consecutive failures, no recorded error) prints
    // nothing extra. Omitted entirely for an older daemon (`sync_health:
    // None`, skew-safe).
    if let Some(h) = &s.sync_health {
        if let Some(line) = sync_health_line(h) {
            println!("\n{}", theme::paint(&line, Role::Negative));
        }
    }
}

/// Build the `sync: …` status line for a [`SyncHealthView`], or `None` when
/// there's nothing worth surfacing. Pure (no printing) so the gate is
/// unit-testable.
///
/// `"skipped"` (the device isn't configured/linked — see
/// `dirad::sync::record_health`) is a NEUTRAL kind, not a failure: it never
/// increments `consecutive_failures`, so a never-linked daemon would
/// otherwise print a permanent, red "0 consecutive failure(s) (skipped)"
/// line on every `dira status`. Suppress it in exactly that
/// skipped-with-zero-failures case; any OTHER combination (a real failure
/// kind, or `consecutive_failures > 0`) still renders as before.
fn sync_health_line(h: &dira_core::protocol::SyncHealthView) -> Option<String> {
    if h.consecutive_failures == 0 && h.last_error_kind.as_deref() == Some("skipped") {
        return None;
    }
    if h.consecutive_failures == 0 && h.last_error_kind.is_none() {
        return None;
    }
    let mut line = format!("sync: {} consecutive failure(s)", h.consecutive_failures);
    if let Some(kind) = &h.last_error_kind {
        line.push_str(&format!(" ({kind})"));
    }
    if h.backoff_secs > 0 {
        line.push_str(&format!(", backing off {}s", h.backoff_secs));
    }
    Some(line)
}

/// A cloud-fetched billing value older than this reads as stale and gets an
/// "(as of …)" suffix. Matches the daemon's refresh cadence.
const BILLING_STALE_AFTER_SECS: i64 = 15 * 60;

/// The status summary block, mirroring the concept layout:
///
/// ```text
/// 3 active sessions · 1 you · 2.5× parallel
///
///   ● engaged   11h 54m    billable base
///   ◆ agent     26h 41m    wall-clock
///   ◇ compute   2.06M tok  ~$15 est
///
/// 10.4h billable → €1,064 unbilled, this week
/// ```
///
/// Pure (no printing, `now` injected) so tests can assert exact bytes — under
/// the test harness stdout isn't a TTY, `paint` passes through, and the
/// asserted strings double as the piped-output spec. Padding happens *before*
/// painting (the theme contract), so columns stay aligned in color too.
///
/// Omission rules: the compute row disappears when there are no tokens today
/// (or an old daemon didn't send them); the billable footer disappears when
/// there is no cloud summary (unlinked / never fetched). The compute estimate
/// renders in `~$` (a local USD estimate from the bundled pricing table); the
/// billable footer carries the workspace policy currency from the cloud —
/// different quantities from different authorities, deliberately not unified.
fn summary_lines(s: &StatusView, active: &[SessionView], now: OffsetDateTime) -> Vec<String> {
    let mut lines = Vec::new();

    // --- header: `3 active sessions · 1 you · 2.5× parallel` -----------------
    if active.is_empty() {
        lines.push(theme::paint("no active sessions", Role::Faint));
    } else {
        let n = active.len();
        let plural = if n == 1 { "" } else { "s" };
        let mut head = theme::paint(&format!("{n} active session{plural}"), Role::Ink);
        let you = active
            .iter()
            .filter(|v| v.live_state() == LiveState::Engaged)
            .count();
        if you > 0 {
            head.push_str(&theme::paint(" · ", Role::Muted));
            head.push_str(&theme::paint(&format!("{you} you"), Role::Engaged));
        }
        if s.today.total_human_seconds > 0 {
            let mult = s.today.total_agent_seconds as f64 / s.today.total_human_seconds as f64;
            head.push_str(&theme::paint(" · ", Role::Muted));
            head.push_str(&theme::paint(&format!("{mult:.1}× parallel"), Role::Accent));
        }
        lines.push(head);
    }

    // --- metric rows ----------------------------------------------------------
    let tokens = s.tokens.filter(|t| t.total_tokens > 0);
    let mut rows: Vec<(char, &str, String, String, Role)> = Vec::new();
    if s.today.total_human_seconds > 0 || s.today.total_agent_seconds > 0 || tokens.is_some() {
        rows.push((
            '●',
            "engaged",
            hms(s.today.total_human_seconds),
            "billable base".to_string(),
            Role::Engaged,
        ));
        rows.push((
            '◆',
            "agent",
            hms(s.today.total_agent_seconds),
            "wall-clock".to_string(),
            Role::Agent,
        ));
    }
    if let Some(t) = tokens {
        // `◇` is hollow on purpose: compute is an estimate, not measured time.
        rows.push((
            '◇',
            "compute",
            format!("{} tok", tokens_compact(t.total_tokens)),
            format!("{} est", usd_approx(t.est_cost_usd)),
            Role::Compute,
        ));
    }
    if !rows.is_empty() {
        let value_w = rows.iter().map(|r| r.2.chars().count()).max().unwrap_or(0);
        lines.push(String::new());
        for (glyph, label, value, note, role) in rows {
            // Pad the raw text, then paint each already-sized column.
            let head = theme::paint(&format!("{glyph} {label:<8}"), role);
            let val = theme::paint(&format!("{value:<value_w$}"), Role::Ink);
            let note = theme::paint(&note, Role::Muted);
            lines.push(format!("  {head} {val}  {note}"));
        }
    }

    // --- billable footer -------------------------------------------------------
    if let Some(b) = &s.billing {
        let footer: String = billing_line(b)
            .iter()
            .map(|(text, role)| theme::paint(text, *role))
            .collect();
        let stale = billing_age_suffix(&b.fetched_at, now)
            .map(|a| format!(" {}", theme::paint(&a, Role::Faint)))
            .unwrap_or_default();
        lines.push(String::new());
        lines.push(format!("{footer}{stale}"));
    }

    lines
}

/// `Some("(as of 32m ago)")` when `fetched_at` parses and is older than the
/// staleness threshold; `None` when fresh, absent, or unparseable.
fn billing_age_suffix(fetched_at: &str, now: OffsetDateTime) -> Option<String> {
    let t = crate::format::parse_ts(Some(fetched_at))?;
    let age = (now - t).whole_seconds();
    if age <= BILLING_STALE_AFTER_SECS {
        return None;
    }
    let label = if age < 3600 {
        format!("{}m", age / 60)
    } else if age < 86_400 {
        format!("{}h", age / 3600)
    } else {
        format!("{}d", age / 86_400)
    };
    Some(format!("(as of {label} ago)"))
}

fn print_sessions(sessions: &[SessionView], layout: &Layout) {
    if sessions.is_empty() {
        println!("  (none)");
        return;
    }
    let pw = layout.session_project;
    let header = format!(
        "  {:<8} {:<10} {:<7} {:<pw$} {:>8} {:>8}  STATE",
        "HANDLE", "HARNESS", "KIND", "PROJECT", "HUMAN", "AGENT"
    );
    println!("{}", theme::paint(&header, Role::Muted));
    for s in sessions {
        // Paint only the trailing STATE word so the fixed-width columns stay
        // aligned (the colour escapes have zero display width). "engaged" (you're
        // driving it) is teal like the human lane; "active" (its agent is working
        // on its own) is purple like the agent lane; "idle" is faint.
        let (label, role) = match s.live_state() {
            LiveState::Engaged => ("engaged", Role::Engaged),
            LiveState::Active => ("active", Role::Agent),
            LiveState::Idle => ("idle", Role::Faint),
        };
        let state = theme::paint(label, role);
        println!(
            "  {:<8} {:<10} {:<7} {:<pw$} {:>8} {:>8}  {}",
            truncate(&s.handle, 8),
            truncate(&s.harness, 10),
            truncate(&s.kind, 7),
            truncate(&project_label(&s.project), pw),
            hms(s.human_seconds),
            hms(s.agent_seconds),
            state,
        );
        print_session_meta(s);
    }
}

/// An indented sub-line with a manual session's metadata: the `activity`
/// classification, the `#label` tag, and the free-text note in quotes — only the
/// parts that are set. Nothing prints for a plain agent session.
fn print_session_meta(s: &SessionView) {
    let mut bits: Vec<String> = Vec::new();
    if let Some(a) = &s.activity {
        bits.push(a.clone());
    }
    if let Some(l) = &s.label {
        bits.push(format!("#{l}"));
    }
    if let Some(n) = &s.note {
        bits.push(format!("\u{201c}{}\u{201d}", truncate(n, 56)));
    }
    if !bits.is_empty() {
        println!(
            "{}",
            theme::paint(&format!("           {}", bits.join("  ")), Role::Muted)
        );
    }
}

/// A compact "parallel sessions" timeline — one agent lane per active session,
/// bars scaled to the longest, with the deduped human (engaged) lane underneath
/// so "one person supervising several agents" is visible at a glance, like the
/// web's Right Now view.
fn print_parallel(active: &[SessionView], today: &Report, layout: &Layout) {
    if active.is_empty() {
        return;
    }
    let lw = layout.parallel_label;
    let bw = layout.bar_cells;
    let eng = today.total_human_seconds;
    let max = active
        .iter()
        .map(|s| s.agent_seconds)
        .chain(std::iter::once(eng))
        .max()
        .unwrap_or(1)
        .max(1);
    // With no engaged human time the multiplier (agent ÷ engaged) is undefined and
    // "0.0× today" reads as meaningless — show just the agent count instead.
    let head = theme::paint("PARALLEL", Role::Muted);
    // Mark the operator in when they're actively supervising (see `any_engaged`).
    let you = if any_engaged(active) {
        theme::paint(" and you", Role::Engaged)
    } else {
        String::new()
    };
    if eng > 0 {
        let parallel = today.total_agent_seconds as f64 / eng as f64;
        let mult = theme::paint(&format!("{parallel:.1}× today"), Role::Accent);
        println!("{head}  ·  {} agent(s){you} · {mult}", active.len());
    } else {
        println!("{head}  ·  {} agent(s){you}", active.len());
    }
    println!();
    // `◆` marks an agent lane (purple), `●` the deduped human baseline (teal) —
    // the same shape/colour language as the cloud's "Right Now" view. Painting
    // only the glyph keeps the padded label column aligned.
    let agent_mark = theme::paint("◆", Role::Agent);
    for s in active {
        let label = truncate(&format!("{} · {}", s.harness, repo_short(&s.project)), lw);
        println!(
            "  {agent_mark} {label:<lw$}   {}   {:>8}",
            bar(s.agent_seconds as f64 / max as f64, bw),
            hms(s.agent_seconds),
        );
    }
    println!(
        "  {} {:<lw$}   {}   {:>8}",
        theme::paint("●", Role::Engaged),
        "you (engaged)",
        bar(eng as f64 / max as f64, bw),
        hms(eng),
    );
    println!();
}

fn print_report(r: &Report, layout: &Layout) {
    if r.projects.is_empty() {
        println!("  {}", theme::paint("no time tracked", Role::Faint));
        return;
    }
    let pw = layout.report_project;
    let header = format!("  {:<pw$} {:>10} {:>10}", "PROJECT", "HUMAN", "AGENT");
    println!("{}", theme::paint(&header, Role::Muted));
    for p in &r.projects {
        println!(
            "  {:<pw$} {:>10} {:>10}",
            truncate(&project_label(&p.project), pw),
            hms(p.human_seconds),
            hms(p.agent_wall_seconds),
        );
    }
    let total = format!(
        "  {:<pw$} {:>10} {:>10}",
        "— total —",
        hms(r.total_human_seconds),
        hms(r.total_agent_seconds),
    );
    println!("{}", theme::paint(&total, Role::Ink));
    println!("  ({} session(s))", r.session_count);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_width_preserves_legacy_constants() {
        // At 80 columns (the historical hand-tuned width and the non-TTY/pipe
        // fallback) every column equals its previous hardcoded value, so scripted
        // output is byte-for-byte stable.
        let l = Layout::for_width(BASELINE_COLS);
        assert_eq!(l.session_project, 28);
        assert_eq!(l.parallel_label, 28);
        assert_eq!(l.bar_cells, 18);
        assert_eq!(l.report_project, 32);
    }

    #[test]
    fn narrow_width_never_shrinks_below_baseline() {
        // Below 80 columns we don't go *under* the minimums (truncate keeps the
        // text from wrapping); we just keep the legacy widths.
        let l = Layout::for_width(40);
        assert_eq!(l.session_project, 28);
        assert_eq!(l.report_project, 32);
        assert_eq!(l.bar_cells, 18);
    }

    #[test]
    fn wide_width_grows_within_caps() {
        // A wide terminal grows the columns, but each is capped so nothing sprawls.
        let l = Layout::for_width(200);
        assert_eq!(l.session_project, 60);
        assert_eq!(l.parallel_label, 60);
        assert_eq!(l.report_project, 64);
        assert_eq!(l.bar_cells, 32);
        // Moderate widening lands between the floor and the cap.
        let m = Layout::for_width(100);
        assert_eq!(m.report_project, 52); // 32 + 20
        assert_eq!(m.bar_cells, 23); // 18 + 20/4
    }

    #[test]
    fn bar_fraction_wrapper_matches_format_bar() {
        // The fraction wrapper agrees with the shared value/max bar at the ends.
        assert_eq!(bar(0.0, 10), "░░░░░░░░░░");
        assert_eq!(bar(1.0, 10), "██████████");
        assert_eq!(bar(0.5, 10), "█████░░░░░");
    }

    use dira_core::protocol::{BillingView, ComputeView};
    use dira_core::report::Report;
    use time::macros::datetime;

    fn view(idle: bool, agent_active: bool) -> SessionView {
        SessionView {
            handle: "h".into(),
            session_id: "s".into(),
            harness: "claude".into(),
            kind: "agent".into(),
            project: Some("github.com/acme/api".into()),
            label: None,
            activity: None,
            note: None,
            started_at: "now".into(),
            human_seconds: 60,
            agent_seconds: 120,
            idle,
            agent_active,
            last_activity_at: None,
            last_human_at: None,
        }
    }

    fn status(human: i64, agent: i64) -> StatusView {
        StatusView {
            active: vec![],
            today: Report {
                projects: vec![],
                total_human_seconds: human,
                total_agent_seconds: agent,
                session_count: 1,
            },
            sync_pending: 0,
            hydrating: false,
            tokens: None,
            billing: None,
            writer_health: None,
            sync_health: None,
        }
    }

    const NOW: time::OffsetDateTime = datetime!(2026-07-02 10:00:00 UTC);

    /// The full mockup case. These byte-exact assertions ARE the piped-output
    /// spec: tests run without a TTY, so `paint` is a pass-through and the
    /// strings below are exactly what a script sees from `dira status | cat`.
    #[test]
    fn summary_full_mockup_case() {
        let mut s = status(42_840, 96_060); // 11h54m engaged, 26h41m agent
        s.tokens = Some(ComputeView {
            total_tokens: 2_060_000,
            est_cost_usd: 15.2,
        });
        s.billing = Some(BillingView {
            billable_hours: 10.4,
            unbilled_amount: 1064.0,
            currency: "€".into(),
            period: "week".into(),
            fetched_at: "2026-07-02T09:55:00Z".into(), // 5m ago — fresh
        });
        let active = vec![view(false, true), view(true, true), view(true, true)];
        let lines = summary_lines(&s, &active, NOW);
        assert_eq!(
            lines,
            vec![
                "3 active sessions · 1 you · 2.2× parallel".to_string(),
                String::new(),
                "  ● engaged  11h 54m    billable base".to_string(),
                "  ◆ agent    26h 41m    wall-clock".to_string(),
                "  ◇ compute  2.06M tok  ~$15 est".to_string(),
                String::new(),
                "10.4h billable → €1,064 unbilled, this week".to_string(),
            ]
        );
    }

    #[test]
    fn summary_omits_compute_row_without_tokens() {
        let s = status(3600, 7200);
        let active = vec![view(false, false)];
        let lines = summary_lines(&s, &active, NOW);
        assert_eq!(
            lines,
            vec![
                "1 active session · 1 you · 2.0× parallel".to_string(),
                String::new(),
                "  ● engaged  1h 00m  billable base".to_string(),
                "  ◆ agent    2h 00m  wall-clock".to_string(),
            ]
        );
        // Zero tokens is the same as absent tokens.
        let mut s = status(3600, 7200);
        s.tokens = Some(ComputeView {
            total_tokens: 0,
            est_cost_usd: 0.0,
        });
        assert_eq!(summary_lines(&s, &active, NOW).len(), 4);
    }

    #[test]
    fn summary_omits_billing_footer_and_multiplier_when_absent() {
        // No billing → no footer; zero human time → no `×` (and no `you`).
        let mut s = status(0, 7200);
        s.tokens = Some(ComputeView {
            total_tokens: 45_200,
            est_cost_usd: 0.42,
        });
        let active = vec![view(true, true)];
        let lines = summary_lines(&s, &active, NOW);
        assert_eq!(
            lines,
            vec![
                "1 active session".to_string(),
                String::new(),
                "  ● engaged  0s         billable base".to_string(),
                "  ◆ agent    2h 00m     wall-clock".to_string(),
                "  ◇ compute  45.2K tok  ~$0.42 est".to_string(),
            ]
        );
    }

    #[test]
    fn summary_zero_day_shows_only_the_empty_header() {
        let s = status(0, 0);
        let lines = summary_lines(&s, &[], NOW);
        assert_eq!(lines, vec!["no active sessions".to_string()]);
    }

    #[test]
    fn summary_flags_a_stale_billing_fetch() {
        let mut s = status(3600, 3600);
        s.billing = Some(BillingView {
            billable_hours: 8.0,
            unbilled_amount: 980.0,
            currency: "$".into(),
            period: "week".into(),
            fetched_at: "2026-07-02T09:28:00Z".into(), // 32m ago — stale
        });
        let lines = summary_lines(&s, &[view(false, false)], NOW);
        assert_eq!(
            lines.last().unwrap(),
            "8h billable → $980 unbilled, this week (as of 32m ago)"
        );
    }

    use dira_core::protocol::SyncHealthView;

    fn sync_health(kind: Option<&str>, consecutive_failures: u32) -> SyncHealthView {
        SyncHealthView {
            last_attempt_at: Some("2026-07-09T10:00:00Z".into()),
            last_error_kind: kind.map(str::to_string),
            consecutive_failures,
            ..Default::default()
        }
    }

    #[test]
    fn sync_health_line_is_quiet_for_a_never_linked_daemon() {
        // The bug this guards: a never-linked daemon skips every flush tick
        // (kind "skipped", never a failure), which must NOT render as a red
        // "0 consecutive failure(s) (skipped)" line on every `dira status`.
        assert_eq!(sync_health_line(&sync_health(Some("skipped"), 0)), None);
    }

    #[test]
    fn sync_health_line_is_quiet_when_fully_healthy() {
        assert_eq!(sync_health_line(&sync_health(None, 0)), None);
    }

    #[test]
    fn sync_health_line_still_reports_real_failures() {
        assert_eq!(
            sync_health_line(&sync_health(Some("transient"), 3)),
            Some("sync: 3 consecutive failure(s) (transient)".to_string())
        );
    }

    #[test]
    fn sync_health_line_reports_skipped_if_failures_are_somehow_nonzero() {
        // Defensive: `record_health`'s "skipped" path never increments
        // `consecutive_failures`, but the render gate itself only special-cases
        // the exact skipped-and-zero combination — anything else still shows.
        assert_eq!(
            sync_health_line(&sync_health(Some("skipped"), 2)),
            Some("sync: 2 consecutive failure(s) (skipped)".to_string())
        );
    }
}
