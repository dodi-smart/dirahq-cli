//! Human-friendly rendering of daemon responses.
//!
//! Column widths and bar lengths scale to the terminal width (detected via
//! [`crossterm::terminal::size`]). At the historical 80-column width the layout
//! is byte-for-byte what it was before width-scaling landed, so scripts that
//! parse the plain output (and pipes, which fall back to 80) stay stable.

use crate::format::{bar as bar_cells, hms, project_label, repo_short, truncate};
use crate::theme::{self, Role};
use dira_core::protocol::{Response, SessionView, StatusView};
use dira_core::report::Report;

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
            print_status(s, &Layout::for_width(terminal_cols()));
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
    }
}

fn print_status(s: &StatusView, layout: &Layout) {
    // Hide degenerate sessions — a bare SessionStart with no engaged time and no
    // agent activity is noise (e.g. a project you opened but didn't work in).
    let active: Vec<SessionView> = s
        .active
        .iter()
        .filter(|v| v.human_seconds > 0 || v.agent_seconds > 0)
        .cloned()
        .collect();
    if active.is_empty() {
        println!("{}", theme::paint("no active sessions", Role::Faint));
    } else {
        println!("{}", theme::paint("ACTIVE SESSIONS", Role::Muted));
        print_sessions(&active, layout);
        println!();
        print_parallel(&active, &s.today, layout);
    }
    println!("{}", theme::paint("TODAY", Role::Muted));
    print_report(&s.today, layout);
    if s.sync_pending > 0 {
        println!(
            "\n{}",
            theme::paint(
                &format!("{} event(s) pending sync", s.sync_pending),
                Role::Compute,
            )
        );
    }
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
        // aligned (the colour escapes have zero display width).
        let state = if s.idle {
            theme::paint("idle", Role::Faint)
        } else {
            theme::paint("engaged", Role::Engaged)
        };
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
        println!("{}", theme::paint(&format!("           {}", bits.join("  ")), Role::Muted));
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
    if eng > 0 {
        let parallel = today.total_agent_seconds as f64 / eng as f64;
        let mult = theme::paint(&format!("{parallel:.1}× today"), Role::Accent);
        println!("{head}  ·  {} agent(s) · {mult}", active.len());
    } else {
        println!("{head}  ·  {} agent(s)", active.len());
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
}
