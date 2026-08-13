//! Human-friendly rendering of daemon responses.
//!
//! Column widths and bar lengths scale to the terminal width (detected via
//! [`crossterm::terminal::size`]). At the historical 80-column width the layout
//! is byte-for-byte what it was before width-scaling landed, so scripts that
//! parse the plain output (and pipes, which fall back to 80) stay stable.

use crate::format::{
    bar as bar_cells, billing_line, display_width, hms, kind_label, pad_cols, pad_left_cols,
    project_label, repo_short, tokens_compact, truncate_cols, usd_approx,
};
use crate::theme::{self, Role};
use dira_core::protocol::{
    any_engaged, LiveState, Response, SessionView, StatusView, ZavetCheckView, ZavetDecisionView,
    ZavetDecisionsView, ZavetGuardStatView, ZavetPresence, ZavetReindexView, ZavetSpecView,
    ZavetStatusView, ZavetSyncView, ZavetUncapturedView, ZavetWhyView,
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

/// Does this session have any counted time to show?
///
/// A **presentation** rule, deliberately NOT the daemon's liveness rule. The
/// registry's `had_signal` (issue #74) asks "did this session ever do anything" —
/// a single tool call qualifies. This asks the narrower question a time table
/// cares about: "is there a number to put in a row". A session with one `PreTool`
/// and no measurable gap answers yes to the first and no to the second, and both
/// answers are correct for their own question.
///
/// Named and shared so the two tables that ask it cannot drift apart, and so it
/// reads as its own rule rather than as a third copy of the liveness one.
fn has_time(s: &SessionView) -> bool {
    s.human_seconds > 0 || s.agent_seconds > 0
}

/// A proportional bar `width` cells wide; `frac` is clamped to [0, 1]. Thin
/// wrapper over [`crate::format::bar`] that takes a fraction instead of
/// value/max, so the local call sites read unchanged.
fn bar(frac: f64, width: usize) -> String {
    let scaled = (frac.clamp(0.0, 1.0) * 1000.0).round() as i64;
    bar_cells(scaled, 1000, width)
}

/// Print a response, returning a non-zero-worthy bool on error responses.
/// The generic `ResyncQueued` summary (`dira device resync` prints its own,
/// richer one). Pure, so the disclosure wording is pinned by tests rather than
/// only reachable through stdout: a `--from` rewind moves ONLY the event cursor,
/// and a user who is not told that will assume the token backlog moved too.
fn resync_fallback_lines(pending: u64, pending_tokens: u64, from: Option<&str>) -> Vec<String> {
    let mut lines = Vec::new();
    match from {
        Some(id) => {
            lines.push(format!(
                "resync queued from {id} — {pending} event(s) will re-sync"
            ));
            lines.push(
                "only the event cursor moved — artifacts and token usage are \
                 untouched; run `dira device resync` (no --from) to re-send those too"
                    .to_string(),
            );
        }
        None => lines.push(format!(
            "resync queued from the beginning — {pending} event(s) will re-sync"
        )),
    }
    if pending_tokens > 0 {
        lines.push(format!(
            "{pending_tokens} token usage row(s) will re-sync too"
        ));
    }
    lines
}

pub fn print(resp: &Response) -> bool {
    print_with(resp, RowOpts::default())
}

/// Print one response as a single JSON object — the `--json` modes.
///
/// Lives here, beside the human renderers, because "how is this response
/// presented" is one question with two answers. The boxed views are unwrapped
/// so a script sees the view itself rather than a one-key envelope; anything
/// without a natural view serializes whole. `Response` is `Serialize`, so the
/// fallback means adding `--json` to another subcommand is a flag change, not a
/// new match arm that silently falls through to human output when forgotten.
///
/// An error response still renders as an error, so a failure is never a
/// silently empty document.
pub fn print_json(resp: &Response) -> bool {
    fn emit<T: serde::Serialize>(v: &T) -> bool {
        match serde_json::to_string_pretty(v) {
            Ok(s) => {
                println!("{s}");
                true
            }
            Err(e) => {
                eprintln!("could not serialize response: {e}");
                false
            }
        }
    }
    match resp {
        Response::Error { .. } => print(resp),
        Response::ZavetDecisions(v) => emit(v),
        Response::ZavetSync(v) => emit(v),
        Response::ZavetWiki(v) => emit(v),
        Response::ZavetStatus(v) => emit(v),
        Response::ZavetWhy(v) => emit(v),
        Response::ZavetSpec(v) => emit(v),
        other => emit(other),
    }
}

/// [`print`], with per-row options the zavet list views honour. Every other
/// response ignores them.
pub fn print_with(resp: &Response, opts: RowOpts) -> bool {
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
            let shown: Vec<SessionView> =
                sessions.iter().filter(|s| has_time(s)).cloned().collect();
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
        Response::ResyncQueued {
            pending,
            pending_tokens,
            from,
        } => {
            for line in resync_fallback_lines(*pending, *pending_tokens, from.as_deref()) {
                println!("{line}");
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
        Response::ZavetSync(v) => {
            for line in zavet_sync_lines(v, terminal_cols() as usize) {
                println!("{line}");
            }
            true
        }
        Response::ZavetDecisions(v) => {
            print_zavet_decisions(v, opts);
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
        Response::ZavetReindex(v) => {
            print_zavet_reindex(v);
            true
        }
        // The capture probe never routes through the generic printer — it is
        // driven directly by `doctor::capture`, which renders it as a check.
        // Reaching here means a request was built somewhere else by mistake.
        Response::CaptureProbe(_) => {
            eprintln!("unexpected capture-probe response outside `dira doctor --probe`");
            false
        }
    }
}

// ---------------------------------------------------------------------------
// zavet — knowledge-module renderers. Same design language as `dira status`:
// glyph metric rows, `·` separators, Muted CAPS section headers, Faint empty
// states, and pad-then-paint so SGR bytes never break column alignment.
// ---------------------------------------------------------------------------

/// The separator glyph between dotted metadata parts.
fn dot_glyph() -> &'static str {
    theme::glyphs().dot
}

/// The ` · ` separator as a measurable segment. One definition — it was being
/// rebuilt inline at ten call sites.
fn dot_sep() -> Seg {
    Seg::new(&format!(" {} ", dot_glyph()), Role::Muted)
}

/// `1 commit` / `4 commits` — English pluralization for the counts these views
/// print, in one place rather than inlined per call site.
fn plural(n: u64, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

/// A `key · key · key` line painted Muted.
fn dots(parts: &[String]) -> String {
    theme::paint(&parts.join(&dot_sep().plain), Role::Muted)
}

/// A rendered fragment that remembers its own uncoloured width.
///
/// `theme::paint` wraps text in SGR bytes that occupy zero display columns but
/// that `display_width` counts anyway. Anything that MEASURES a fragment before
/// deciding whether it fits therefore has to keep the plain text around — and
/// the failure mode is nasty, because `paint` is a no-op when colour is off, so
/// every NO_COLOR test passes while the real terminal collapses its layout.
#[derive(Debug, Clone)]
struct Seg {
    plain: String,
    painted: String,
}

impl Seg {
    fn new(text: &str, role: Role) -> Self {
        Seg {
            plain: text.to_string(),
            painted: theme::paint(text, role),
        }
    }

    /// Join the non-empty segments with `sep`, keeping both halves in step.
    fn joined(parts: &[Seg], sep: &Seg) -> Seg {
        let kept: Vec<&Seg> = parts.iter().filter(|p| !p.plain.is_empty()).collect();
        Seg {
            plain: kept
                .iter()
                .map(|p| p.plain.as_str())
                .collect::<Vec<_>>()
                .join(&sep.plain),
            painted: kept
                .iter()
                .map(|p| p.painted.as_str())
                .collect::<Vec<_>>()
                .join(&sep.painted),
        }
    }
}

/// Word-wrap `text` to `width` display columns.
///
/// Used for the prose lines — empty-state hints and section explanations —
/// which are fixed sentences that would otherwise be the only thing in these
/// views still hard-wrapping at whatever width the terminal happens to be.
/// A single word longer than `width` is emitted whole rather than split.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    wrap_join(text.split_whitespace(), " ", width)
}

/// Greedy-wrap `items` joined by `sep` into lines at most `width` columns wide.
///
/// The one wrapper: prose (`sep = " "`) and dotted glob lists (`sep = " · "`)
/// are the same accumulate-until-overflow loop, and having two copies meant
/// any off-by-one in the budget had to be fixed twice. An item wider than
/// `width` is emitted whole rather than split.
fn wrap_join<'a>(items: impl Iterator<Item = &'a str>, sep: &str, width: usize) -> Vec<String> {
    let width = width.max(20);
    let mut lines = Vec::new();
    let mut cur = String::new();
    for item in items {
        if !cur.is_empty() && display_width(&cur) + display_width(sep) + display_width(item) > width
        {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push_str(sep);
        }
        cur.push_str(item);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// The `repo · branch` header, wrapped rather than truncated when it does not
/// fit.
///
/// Both halves are identifiers the reader may need to copy — a canonical repo
/// and a branch name — so clipping either one is worse than spending a second
/// line. A branch that still overflows on its own is truncated, since at that
/// point nothing fits.
fn zavet_header(repo: &str, branch: Option<&str>, cols: usize, lead: Option<Seg>) -> Vec<String> {
    let dot = Seg::new(&format!(" {} ", theme::glyphs().dot), Role::Muted);
    // [`Seg`] carries the plain text alongside the painted one so the fit test
    // below measures display columns rather than SGR bytes.
    let mut first: Vec<Seg> = lead.into_iter().collect();
    first.push(Seg::new(repo, Role::Ink));
    let Some(b) = branch else {
        return vec![Seg::joined(&first, &dot).painted];
    };
    let head = Seg::joined(&first, &dot);
    let branch_seg = Seg::new(b, Role::Accent);
    let one = Seg::joined(&[head.clone(), branch_seg.clone()], &dot);
    if display_width(&one.plain) <= cols {
        return vec![one.painted];
    }
    vec![
        head.painted,
        theme::paint(&truncate_cols(b, cols), Role::Accent),
    ]
}

/// The indent every zavet row and section body carries.
const ZAVET_INDENT: usize = 2;

/// Resolved column widths for one pass over a decision list.
///
/// The old layout had this backwards: titles were clipped at a hardcoded 46
/// columns while the guard globs beneath them ran to whatever length the repo's
/// paths happened to be. On a wide terminal the most informative field was the
/// only one rationed; on a deep-path Windows checkout the least informative one
/// wrapped three times and the list stopped being a list.
///
/// So: the title takes every column the fixed fields do not, and the fixed
/// fields drop out one at a time as the terminal narrows. A width of `0` means
/// "this column is not drawn at this size". Order of sacrifice is by
/// information density — activity first (it is the rarest to be non-empty),
/// then the guard count, then age.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ZavetLayout {
    id: usize,
    title: usize,
    guards: usize,
    activity: usize,
    age: usize,
}

impl ZavetLayout {
    /// Widths for a terminal `cols` wide, given the longest id in the list.
    ///
    /// `id` is measured rather than fixed because prefixes are per-repo
    /// (`D-0001` is 6 columns, `DIRASH-0024` is 11) and padding every repo to
    /// the longest possible prefix would waste columns on the common case.
    fn for_width(cols: usize, id_width: usize) -> Self {
        // The wiki's uncaptured section mixes decision ids with spec slugs,
        // which run longer; 20 fits every real one. Rows truncate to this as a
        // hard backstop so the "never wider than the terminal" invariant holds
        // no matter what a repo names things.
        let id = id_width.clamp(6, 20);
        // The title floor. Below this, drop a fixed column instead of squeezing
        // the one field that carries the actual knowledge — a 33-column title
        // is the same unreadable list in a different shape.
        let bare = ZAVET_INDENT + id + 2 + 36;
        // Sacrifice order, least informative first: activity is the rarest to
        // be non-empty, then the guard count, then age. Zeroing by index keeps
        // the priority in one place instead of spread across match arms.
        let mut opt = [18usize, 9, 4];
        let fixed = |o: &[usize; 3]| o.iter().filter(|w| **w > 0).map(|w| w + 2).sum::<usize>();
        for i in 0..opt.len() {
            if cols >= bare + fixed(&opt) {
                break;
            }
            opt[i] = 0;
        }
        let [activity, guards, age] = opt;
        ZavetLayout {
            id,
            title: cols
                .saturating_sub(ZAVET_INDENT + id + 2 + fixed(&opt))
                .clamp(20, 90),
            guards,
            activity,
            age,
        }
    }
}

/// The widest id the list will print, captured and uncaptured alike.
///
/// Ids are per-repo (`D-0001` is 6 columns, `DIRASH-0024` is 11), so the column
/// is measured rather than fixed. Uncaptured rows count: they sit in the same
/// table, and sizing the column without them makes every long id push its row
/// out of alignment with the ones above it.
fn id_width<'a>(
    decisions: impl Iterator<Item = &'a ZavetDecisionView>,
    uncaptured: &[ZavetUncapturedView],
) -> usize {
    decisions
        .map(|d| display_width(&d.id))
        .chain(
            uncaptured
                .iter()
                .filter_map(|u| u.id.as_deref())
                .map(display_width),
        )
        .max()
        .unwrap_or(6)
}

/// What a caller wants on each row beyond the defaults.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RowOpts {
    /// Spell out every guard glob beneath the row (`--guards`).
    pub guards: bool,
    /// Hide records whose file is not on the checked-out branch (`--branch`).
    pub branch_only: bool,
}

/// `4d` / `3mo` since `created_at`, or `new` inside the first two days.
///
/// `created_at` is the *commit's* author date, not the ingest time, so this
/// answers "how long has this decision been in the repo" — which is the
/// question a reader scanning for recent work is actually asking.
fn age_label(created_at: Option<&str>, now: OffsetDateTime) -> Option<(String, Role)> {
    let t =
        OffsetDateTime::parse(created_at?, &time::format_description::well_known::Rfc3339).ok()?;
    let days = (now - t).whole_days();
    if days < 0 {
        // A clock skew or a rebased author date from the future. Say nothing
        // rather than print a negative age.
        return None;
    }
    Some(match days {
        0..=1 => ("new".to_string(), Role::Engaged),
        d if d < 30 => (format!("{d}d"), Role::Faint),
        d if d < 365 => (format!("{}mo", d / 30), Role::Faint),
        d => (format!("{}y", d / 365), Role::Faint),
    })
}

/// Guard activity folded into the two numbers a reader can act on.
///
/// `blocked` and `complied` both mean the guard stopped a change that would
/// have contradicted the record — a regression the decision prevented — so they
/// sum into one `kept` count. `overridden` is the human going ahead anyway,
/// which is the number worth seeing next to it: a decision overridden more than
/// it is kept is a decision that needs revisiting, not a guard that needs
/// tightening. `shown` is deliberately excluded; being displayed is not
/// evidence of anything.
///
/// Empty when nothing fired — an absent number, never a zero, because "no guard
/// event was ever recorded" and "the guard fired zero times" are the same
/// display but a zero reads as a measurement.
fn activity_label(stats: &[ZavetGuardStatView]) -> Option<String> {
    let tally = |k: &str| stats.iter().find(|s| s.kind == k).map_or(0, |s| s.total);
    let kept = tally("guard_blocked") + tally("guard_complied");
    let over = tally("guard_overridden");
    match (kept, over) {
        (0, 0) => None,
        (k, 0) => Some(format!("{k} kept")),
        (0, o) => Some(format!("{o} over")),
        (k, o) => Some(format!("{k} kept {} {o} over", theme::glyphs().dot)),
    }
}

/// One decision as a single line: `id  title …  guards  activity  age`.
///
/// The one place a decision row is formatted — `decisions` and `wiki` both call
/// it, so the two views cannot drift apart the way their two hand-rolled copies
/// did. Fixed fields are right-aligned into a cluster the eye can scan or
/// ignore as a column; the title absorbs the slack, so the cluster's left edge
/// stays put down the whole list.
fn decision_row(d: &ZavetDecisionView, l: &ZavetLayout, now: OffsetDateTime) -> String {
    let mut out = format!(
        "{}{}  {}",
        " ".repeat(ZAVET_INDENT),
        theme::paint(
            &pad_cols(&truncate_cols(&d.id, l.id), l.id),
            Role::Knowledge
        ),
        theme::paint(
            &pad_cols(
                &truncate_cols(d.title.as_deref().unwrap_or("(untitled)"), l.title),
                l.title
            ),
            Role::Ink
        ),
    );
    if l.guards > 0 {
        let n = d.guards.len();
        let text = if n == 0 {
            String::new()
        } else {
            plural(n as u64, "guard")
        };
        out.push_str(&format!(
            "  {}",
            theme::paint(&pad_cols(&text, l.guards), Role::Faint)
        ));
    }
    if l.activity > 0 {
        let text = activity_label(&d.guard_stats).unwrap_or_default();
        out.push_str(&format!(
            "  {}",
            theme::paint(&pad_cols(&text, l.activity), Role::Engaged)
        ));
    }
    if l.age > 0 {
        let (text, role) =
            age_label(d.created_at.as_deref(), now).unwrap_or_else(|| (String::new(), Role::Faint));
        out.push_str(&format!(
            "  {}",
            theme::paint(&pad_left_cols(&text, l.age), role)
        ));
    }
    // Trailing padding is invisible but shows up in width assertions and in a
    // terminal that highlights selections.
    out.trim_end().to_string()
}

/// The guard globs for one row, wrapped to the title column instead of running
/// off the edge. Only reached under `--guards`.
fn guard_lines(d: &ZavetDecisionView, l: &ZavetLayout, cols: usize) -> Vec<String> {
    let indent = ZAVET_INDENT + l.id + 2;
    wrap_join(
        d.guards.iter().map(String::as_str),
        &dot_sep().plain,
        cols.saturating_sub(indent),
    )
    .into_iter()
    .map(|line| format!("{}{}", " ".repeat(indent), theme::paint(&line, Role::Faint)))
    .collect()
}

/// A `TITLE · N` section head, with an optional dim explanation.
///
/// The note sits beside the head when it fits and wraps underneath when it does
/// not — these explanations are the whole reason a reader knows what `OFF
/// BRANCH` means, so they are never clipped.
fn section_head(title: &str, count: usize, note: Option<&str>, cols: usize) -> Vec<String> {
    let plain = format!("{title} {} {count}", theme::glyphs().dot);
    let head = theme::paint(&plain, Role::Muted);
    let Some(n) = note else {
        return vec![head];
    };
    if display_width(&plain) + 3 + display_width(n) <= cols {
        return vec![format!("{head}   {}", theme::paint(n, Role::Faint))];
    }
    let mut out = vec![head];
    out.extend(
        wrap_words(n, cols.saturating_sub(ZAVET_INDENT))
            .into_iter()
            .map(|l| {
                format!(
                    "{}{}",
                    " ".repeat(ZAVET_INDENT),
                    theme::paint(&l, Role::Faint)
                )
            }),
    );
    out
}

/// The section hint for a set of uncaptured rows.
///
/// The rows carry two different reasons and only one of them is fixed by
/// committing: an `awaiting sweep` record IS committed, and telling its author
/// to commit it sends them at a fix that cannot work. Pure so the pairing is
/// pinned by a test.
///
/// `sync` is named first because it is the common case — a record committed
/// moments ago, ahead of the baseline, which one sweep picks up. It cannot help
/// a record BEHIND the baseline (a fresh clone's older history), and from here
/// the two are indistinguishable: both are "in HEAD, not captured". So the
/// hint names `reindex` as the fallback rather than leaving a user to re-run a
/// sync that will never do anything (DIRASH-0028).
fn uncaptured_hint(rows: &[ZavetUncapturedView]) -> &'static str {
    let uncommitted = rows.iter().any(|u| u.reason == "uncommitted");
    let awaiting = rows.iter().any(|u| u.reason == "awaiting sweep");
    match (uncommitted, awaiting) {
        (true, true) => {
            "on disk, not captured — commit them, then `dira zavet sync` (or `reindex`)"
        }
        (false, true) => {
            "committed, not yet swept — run `dira zavet sync`, or `reindex` if it stays"
        }
        // Includes the empty case, which never reaches a caller.
        _ => "on disk, not captured — dira reads git, so commit them",
    }
}

/// The `UNCAPTURED` rows: records dira can see on disk but has never captured.
///
/// This section exists because the alternative is silence. Capture reads git
/// objects, so a record written a minute ago is absent from every query with no
/// hint that anything is missing — the failure a user reads as "dira lost my
/// decision". Naming the file and the remedy turns it into a two-second fix.
fn uncaptured_lines(rows: &[ZavetUncapturedView], l: &ZavetLayout, cols: usize) -> Vec<String> {
    if rows.is_empty() {
        return Vec::new();
    }
    let mut out = vec![String::new()];
    out.extend(section_head(
        "UNCAPTURED",
        rows.len(),
        Some(uncaptured_hint(rows)),
        cols,
    ));
    // These rows carry a trailing reason that the decision layout knows nothing
    // about, so they budget for it here rather than borrowing `l.title` — which
    // is what pushed them past the edge at narrow widths. Capped at `l.title`
    // so the two sections still share a left edge when there is room.
    let reason_w = rows
        .iter()
        .map(|u| display_width(&u.reason))
        .max()
        .unwrap_or(0);
    let width = cols
        .saturating_sub(ZAVET_INDENT + l.id + 4 + reason_w)
        .clamp(12, l.title.max(12));
    for u in rows {
        let label = u.id.clone().unwrap_or_else(|| "?".to_string());
        let title = u.title.clone().unwrap_or_else(|| format!("({})", u.path));
        out.push(format!(
            "{}{}  {}  {}",
            " ".repeat(ZAVET_INDENT),
            theme::paint(
                &pad_cols(&truncate_cols(&label, l.id), l.id),
                Role::Knowledge
            ),
            theme::paint(&pad_cols(&truncate_cols(&title, width), width), Role::Ink),
            theme::paint(&u.reason, Role::Compute),
        ));
    }
    out
}

/// `dira zavet sync` as lines — pure, like the other zavet views, so the
/// wording is pinned by tests rather than reachable only through stdout.
///
/// The uncaptured section rides the shared [`uncaptured_lines`] renderer, and
/// deliberately prints AFTER the counts: a sweep that captured nothing because
/// the records were never committed must not read as "already up to date".
fn zavet_sync_lines(v: &ZavetSyncView, cols: usize) -> Vec<String> {
    let mut out = vec![format!(
        "{} {} {}",
        theme::paint("zavet sync", Role::Knowledge),
        theme::paint(dot_glyph(), Role::Muted),
        theme::paint(&v.repo, Role::Ink),
    )];
    let mut why = Vec::new();
    if !v.active {
        why.push("zavet inactive here — nothing to capture".to_string());
    }
    if v.registered {
        why.push("repo registered — the daemon will keep sweeping it".to_string());
    }
    if !why.is_empty() {
        out.push(dots(&why));
    }
    out.push(String::new());
    if v.decisions_captured == 0 && v.trailers_captured == 0 {
        out.push(theme::paint(
            "already up to date · nothing new to capture",
            Role::Muted,
        ));
    } else {
        out.push(format!(
            "{}   {}",
            theme::paint(
                &format!(
                    "captured {} · {}",
                    plural(v.decisions_captured, "decision"),
                    plural(v.trailers_captured, "trailer"),
                ),
                Role::Engaged
            ),
            theme::paint(
                &format!("{} total", plural(v.decisions_total, "decision")),
                Role::Muted
            ),
        ));
    }
    let l = ZavetLayout::for_width(cols, id_width(std::iter::empty(), &v.uncaptured));
    out.extend(uncaptured_lines(&v.uncaptured, &l, cols));
    out
}

/// `dira zavet decisions` as lines — pure, so the layout is pinned by tests
/// rather than reachable only through stdout.
fn zavet_decisions_lines(
    v: &ZavetDecisionsView,
    cols: usize,
    opts: RowOpts,
    now: OffsetDateTime,
) -> Vec<String> {
    if v.decisions.is_empty() && v.uncaptured.is_empty() {
        return wrap_words(
            "no captured decisions yet — record one with /zavet:decide, or run /zavet:backfill for an existing codebase",
            cols,
        )
        .iter()
        .map(|l| theme::paint(l, Role::Faint))
        .collect();
    }
    let l = ZavetLayout::for_width(cols, id_width(v.decisions.iter(), &v.uncaptured));
    let mut out = Vec::new();

    out.extend(zavet_header(&v.repo, v.branch.as_deref(), cols, None));

    // Off-branch is its own group rather than a badge on a flat list: the
    // question "what governs the code in front of me" deserves an answer the
    // eye can take in without filtering, and the pooled set stays visible
    // directly beneath it.
    let (present, off): (Vec<&ZavetDecisionView>, Vec<&ZavetDecisionView>) = v
        .decisions
        .iter()
        .partition(|d| d.presence != Some(ZavetPresence::OffBranch));
    let (active, superseded): (Vec<&ZavetDecisionView>, Vec<&ZavetDecisionView>) = present
        .into_iter()
        .partition(|d| d.status.as_deref().unwrap_or("active") == "active");

    let mut section = |title: &str, note: Option<&str>, rows: Vec<&ZavetDecisionView>| {
        if rows.is_empty() {
            return;
        }
        out.push(String::new());
        out.extend(section_head(title, rows.len(), note, cols));
        for d in rows {
            out.push(decision_row(d, &l, now));
            if opts.guards {
                out.extend(guard_lines(d, &l, cols));
            }
        }
    };
    section("ACTIVE", None, active);
    section("SUPERSEDED", None, superseded);
    if !opts.branch_only {
        section(
            "OFF BRANCH",
            Some("recorded on another branch — not in this working tree"),
            off,
        );
    } else if !off.is_empty() {
        out.push(String::new());
        out.push(theme::paint(
            &format!(
                "{} recorded on other branches — dira zavet decisions (without --branch)",
                off.len()
            ),
            Role::Faint,
        ));
    }
    out.extend(uncaptured_lines(&v.uncaptured, &l, cols));
    out
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
    parts.join(&dot_sep().painted)
}

/// Guard-event tallies as one dotted line (`3 shown · 1 override`), the
/// `guard_` kind prefix stripped.
fn guard_stats_line(stats: &[ZavetGuardStatView]) -> String {
    stats
        .iter()
        .map(|s| format!("{} {}", s.total, s.kind.trim_start_matches("guard_")))
        .collect::<Vec<_>>()
        .join(&dot_sep().plain)
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
    let (provenance, trust) = spec_badge_segs(origin, confidence, verified, stale_commits);
    Seg::joined(&[provenance, trust], &dot_sep()).painted
}

/// The spec badge vocabulary, split into the two groups the list views drop
/// independently: `(provenance, trust)`.
///
/// One definition for both callers. They previously had a copy each and had
/// already drifted — the wiki rendered `⚠ stale 1` while `zavet why` rendered
/// `⚠ stale · 1 commit` for the same state.
fn spec_badge_segs(
    origin: Option<&str>,
    confidence: Option<&str>,
    verified: Option<bool>,
    stale_commits: Option<u64>,
) -> (Seg, Seg) {
    let dot = dot_sep();
    let mut prov = Vec::new();
    if let Some(o) = origin {
        prov.push(Seg::new(o, Role::Faint));
    }
    if let Some(c) = confidence {
        prov.push(Seg::new(&format!("confidence {c}"), Role::Faint));
    }
    let mut trust = vec![if verified == Some(true) {
        Seg::new(
            &format!("{} verified", theme::glyphs().check),
            Role::Engaged,
        )
    } else {
        // Amber, deliberately loud: no human confirmed spec-matches-code yet.
        Seg::new(
            &format!("{} unverified", theme::glyphs().open),
            Role::Compute,
        )
    }];
    match stale_commits {
        Some(0) => trust.push(Seg::new(
            &format!("{} current", theme::glyphs().check),
            Role::Engaged,
        )),
        Some(n) => trust.push(Seg::new(
            &format!(
                "{} stale {} {}",
                theme::glyphs().warn,
                dot_glyph(),
                plural(n, "commit")
            ),
            Role::Compute,
        )),
        None => {}
    }
    (Seg::joined(&prov, &dot), Seg::joined(&trust, &dot))
}

/// The CHECKS panel: how a record says its invariants are verified.
///
/// Reported, never run — dira only shows what the record claims about itself;
/// `zavet verify` is what executes a check, and only when a human asks. An
/// unlabeled check has label == command, so print the command once.
fn print_zavet_checks(checks: &[ZavetCheckView]) {
    if checks.is_empty() {
        return;
    }
    println!("\n{}", theme::paint("CHECKS", Role::Muted));
    for c in checks {
        if c.label == c.command {
            println!("  {}", theme::paint(&c.command, Role::Ink));
        } else {
            println!(
                "  {}\n    {}",
                theme::paint(&c.label, Role::Ink),
                theme::paint(&c.command, Role::Faint),
            );
        }
    }
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
            Some(s) => theme::paint(
                &format!("{} {}", theme::glyphs().bullet, &s[..s.len().min(8)]),
                Role::Engaged,
            ),
            None => theme::paint("unattributed", Role::Faint),
        };
        println!(
            "  {}  {}  {} {}",
            theme::paint(sha, Role::Faint),
            theme::paint(
                &truncate_cols(c.message.as_deref().unwrap_or("(not captured)"), 42),
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
        theme::paint(dot_glyph(), Role::Muted),
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
    let g = theme::glyphs();
    let rows: [(&str, &str, String, String, Role); 3] = [
        (
            g.diamond,
            "decisions",
            v.decisions_total.to_string(),
            format!("{} active", v.decisions_active),
            Role::Knowledge,
        ),
        (
            g.square,
            "trailers",
            v.trailers.to_string(),
            "micro-decisions in commit footers".to_string(),
            Role::Ink,
        ),
        (
            g.bullet,
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
    for line in metric_rows(&rows) {
        println!("{line}");
    }
}

/// The zavet metric block: `glyph label  value   note`, values right-aligned to
/// the widest one so the note column starts at a single x across every row.
///
/// Shared by every zavet screen that shows one — the alignment rules are layout,
/// not content, and two copies would drift the first time either is tweaked.
/// Glyphs are `&str` because the palette resolves them per terminal
/// (`theme::glyphs`), and the ASCII fallbacks are not all one char.
fn metric_rows(rows: &[(&str, &str, String, String, Role)]) -> Vec<String> {
    let value_w = rows.iter().map(|r| r.2.chars().count()).max().unwrap_or(0);
    rows.iter()
        .map(|(glyph, label, value, note, role)| {
            format!(
                "{} {}   {}",
                theme::paint(&format!("{glyph} {label:<10}"), *role),
                theme::paint(&format!("{value:>value_w$}"), Role::Ink),
                theme::paint(note, Role::Muted),
            )
        })
        .collect()
}

/// Render `dira zavet decisions`.
fn print_zavet_decisions(v: &ZavetDecisionsView, opts: RowOpts) {
    for line in zavet_decisions_lines(v, terminal_cols() as usize, opts, OffsetDateTime::now_utc())
    {
        println!("{line}");
    }
}

/// `dira zavet reindex` — what the walk saw and what it wrote.
fn print_zavet_reindex(v: &ZavetReindexView) {
    for line in zavet_reindex_lines(v) {
        println!("{line}");
    }
}

/// [`print_zavet_reindex`] as lines — pure, like every other zavet view, so
/// what the command claims about its own coverage is testable rather than
/// reachable only through stdout.
fn zavet_reindex_lines(v: &ZavetReindexView) -> Vec<String> {
    let mut out = vec![format!(
        "{} {} {}",
        theme::paint("zavet reindex", Role::Knowledge),
        theme::paint(dot_glyph(), Role::Muted),
        theme::paint(&v.repo, Role::Ink),
    )];
    if !v.active {
        out.push(dots(
            &["zavet inactive here — nothing to index".to_string()],
        ));
        return out;
    }
    out.push(String::new());
    let g = theme::glyphs();
    let rows: [(&str, &str, String, String, Role); 3] = [
        (
            g.diamond,
            "decisions",
            v.decisions_indexed.to_string(),
            format!("{} unchanged", v.decisions_skipped),
            Role::Knowledge,
        ),
        (
            g.diamond_hollow,
            "specs",
            v.specs_indexed.to_string(),
            format!("{} unchanged", v.specs_skipped),
            Role::Knowledge,
        ),
        (
            g.square,
            "trailers",
            v.trailer_commits_recorded.to_string(),
            format!(
                "commits carrying them, of {} scanned",
                v.trailer_commits_scanned
            ),
            Role::Ink,
        ),
    ];
    out.extend(metric_rows(&rows));
    out.push(String::new());
    let mut notes = vec![format!("{} commits touched .zavet/", v.commits_scanned)];
    if v.trailers_bounded {
        // Say what was NOT covered rather than letting a bounded scan read as
        // exhaustive — the whole bug being fixed here was a silent bound.
        notes.push("trailer scan bounded — `--all-trailers` for full history".to_string());
    }
    out.push(dots(&notes));
    out
}

/// Render `dira zavet why`: the knowledge first, then evidence, then cost.
fn print_zavet_why(v: &ZavetWhyView) {
    let d = &v.decision;
    println!(
        "{} {} {}",
        theme::paint(&d.id, Role::Knowledge),
        theme::paint(dot_glyph(), Role::Muted),
        theme::paint(d.title.as_deref().unwrap_or("(untitled)"), Role::Ink),
    );
    if let Some(q) = &v.matched_query {
        println!("{}", theme::paint(&format!("matched \"{q}\""), Role::Faint));
    }
    println!(
        "{} {} {}",
        zavet_badges(d.status.as_deref(), d.origin.as_deref(), d.verified),
        theme::paint(dot_glyph(), Role::Muted),
        theme::paint(&d.path, Role::Faint),
    );
    if let Some(s) = &d.supersedes {
        println!("{}", theme::paint(&format!("supersedes {s}"), Role::Muted));
    }
    if !v.corrects.is_empty() {
        println!(
            "{}",
            theme::paint(
                &format!("corrects {}", v.corrects.join(&dot_sep().plain)),
                Role::Muted
            )
        );
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
    // Amber and ABOVE the body: the record still stands (it was not
    // superseded), but one claim inside it is wrong, and a reader who stops
    // before the correction leaves with the wrong answer.
    if let Some(s) = &d.corrected_by {
        println!(
            "{}",
            theme::paint(
                &format!(
                    "{} corrected by {s} — one claim below is wrong; read that too",
                    theme::glyphs().warn
                ),
                Role::Compute
            )
        );
    }
    if !d.guards.is_empty() {
        println!(
            "{} {}",
            theme::paint("guards", Role::Muted),
            theme::paint(&d.guards.join(&dot_sep().plain), Role::Ink),
        );
    }
    if !v.specs.is_empty() {
        let refs = v
            .specs
            .iter()
            .map(|s| s.slug.clone())
            .collect::<Vec<_>>()
            .join(&dot_sep().plain);
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

    print_zavet_checks(&d.checks);

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
                    theme::paint(
                        &format!("{} {}", theme::glyphs().bullet, hms(s.human_seconds)),
                        Role::Engaged,
                    ),
                    theme::paint(
                        &format!("{} {}", theme::glyphs().diamond, hms(s.agent_seconds)),
                        Role::Agent,
                    ),
                    theme::paint(
                        &format!(
                            "{} {} tok",
                            theme::glyphs().diamond_hollow,
                            tokens_compact(s.input_tokens + s.output_tokens)
                        ),
                        Role::Compute
                    ),
                );
            }
        }
        println!(
            "  {}   {}   {}",
            theme::paint(
                &format!(
                    "{} engaged {}",
                    theme::glyphs().bullet,
                    hms(total_human_seconds)
                ),
                Role::Engaged
            ),
            theme::paint(
                &format!(
                    "{} agent {}",
                    theme::glyphs().diamond,
                    hms(total_agent_seconds)
                ),
                Role::Agent
            ),
            theme::paint(
                &format!(
                    "{} compute {} tok",
                    theme::glyphs().diamond_hollow,
                    tokens_compact(total_tokens)
                ),
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
        theme::paint(dot_glyph(), Role::Muted),
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
        theme::paint(dot_glyph(), Role::Muted),
        theme::paint(&s.path, Role::Faint),
    );
    if !s.paths.is_empty() {
        println!(
            "{} {}",
            theme::paint("paths", Role::Muted),
            theme::paint(&s.paths.join(&dot_sep().plain), Role::Ink),
        );
    }
    if !s.decisions.is_empty() {
        println!(
            "{} {}",
            theme::paint("decisions", Role::Muted),
            theme::paint(&s.decisions.join(&dot_sep().plain), Role::Knowledge),
        );
    }

    if let Some(body) = &v.body_md {
        print_zavet_body(body);
    }

    print_zavet_commits(&v.commits);

    print_zavet_checks(&s.checks);

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
                    &truncate_cols(h.title.as_deref().unwrap_or("(untitled)"), 52),
                    Role::Ink
                ),
                zavet_badges(h.status.as_deref(), None, h.verified),
            );
            if let Some(e) = &h.excerpt {
                println!(
                    "  {}",
                    theme::paint(&format!("{:<8}  {}", "", truncate_cols(e, 60)), Role::Faint)
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
                theme::paint(
                    &format!("{:<18}", truncate_cols(&s.slug, 18)),
                    Role::Knowledge
                ),
                theme::paint(
                    &truncate_cols(s.title.as_deref().unwrap_or("(untitled)"), 42),
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
                    theme::paint(
                        &format!("{:<18}  {}", "", truncate_cols(e, 50)),
                        Role::Faint
                    )
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
                theme::paint(&truncate_cols(&t.value, 56), Role::Ink),
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
    for line in zavet_wiki_lines(v, terminal_cols() as usize, OffsetDateTime::now_utc()) {
        println!("{line}");
    }
}

/// How many decisions the overview shows per section before deferring to the
/// full list. `wiki` is a landing page; a repo with eighty decisions should not
/// print eighty rows before the specs the reader came for.
const WIKI_SECTION_CAP: usize = 10;

/// One spec row: `slug  title  badges`, with paths and linked decisions folded
/// into counts.
///
/// The globs themselves were the other unbounded line in this view, and they
/// are the same kind of detail the guard list is — useful when you are looking
/// at one spec, noise when you are scanning ten. `dira zavet why <slug>` prints
/// them in full.
fn spec_tail(s: &ZavetSpecView, slug_w: usize, cols: usize) -> Seg {
    let mut c: Vec<String> = Vec::new();
    if !s.paths.is_empty() {
        c.push(plural(s.paths.len() as u64, "path"));
    }
    if !s.decisions.is_empty() {
        c.push(plural(s.decisions.len() as u64, "decision"));
    }
    let counts = Seg::new(&c.join(&dot_sep().plain), Role::Faint);
    let (provenance, trust) = spec_badge_segs(
        s.origin.as_deref(),
        s.confidence.as_deref(),
        s.verified,
        s.stale_commits,
    );
    // Priority order, most important first: trust says whether the document can
    // be believed right now; the rest is context `dira zavet why <slug>` gives
    // in full. The longest prefix that fits wins — clipping instead would cut a
    // badge mid-word and read as corruption.
    let parts = [trust, counts, provenance];
    let sep = Seg::new(&format!("   {} ", dot_glyph()), Role::Muted);
    let fixed = ZAVET_INDENT + slug_w + 2;
    (1..=parts.len())
        .rev()
        .map(|n| Seg::joined(&parts[..n], &sep))
        // 12 columns is the floor below which a title says nothing; anything
        // narrower and the tail loses rather than the title.
        .find(|t| fixed + 12 + 2 + display_width(&t.plain) <= cols)
        .unwrap_or_else(|| Seg::new("", Role::Faint))
}

/// One spec row, given the section's shared column widths.
fn spec_row(s: &ZavetSpecView, slug_w: usize, title_w: usize, tail: &Seg) -> String {
    format!(
        "{}{}  {}  {}",
        " ".repeat(ZAVET_INDENT),
        theme::paint(
            &pad_cols(&truncate_cols(&s.slug, slug_w), slug_w),
            Role::Knowledge
        ),
        theme::paint(
            &pad_cols(
                &truncate_cols(s.title.as_deref().unwrap_or("(untitled)"), title_w),
                title_w
            ),
            Role::Ink
        ),
        tail.painted,
    )
    .trim_end()
    .to_string()
}

/// The triage line: what in this knowledge base needs a human.
///
/// Sits directly under the totals because the totals answer "how much is here"
/// and this answers "what is wrong with it" — which is the question that
/// actually decides whether the reader does anything next. Empty (and omitted)
/// when nothing needs attention, so its presence is itself the signal.
fn wiki_attention_line(v: &dira_core::protocol::ZavetWikiView) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if !v.uncaptured.is_empty() {
        parts.push(format!("{} uncaptured", v.uncaptured.len()));
    }
    let off = v
        .active
        .iter()
        .chain(&v.superseded)
        .filter(|d| d.presence == Some(ZavetPresence::OffBranch))
        .count();
    if off > 0 {
        parts.push(format!("{off} off branch"));
    }
    let unverified = v
        .active
        .iter()
        .filter(|d| dira_core::zavet::is_unverified(d.origin.as_deref(), d.verified))
        .count();
    if unverified > 0 {
        parts.push(format!("{unverified} unverified"));
    }
    let stale = v
        .specs
        .iter()
        .filter(|s| s.stale_commits.is_some_and(|n| n > 0))
        .count();
    if stale > 0 {
        parts.push(format!(
            "{stale} stale spec{}",
            if stale == 1 { "" } else { "s" }
        ));
    }
    if parts.is_empty() {
        return None;
    }
    Some(theme::paint(
        &format!(
            "{} {}",
            theme::glyphs().warn,
            parts.join(&format!(" {} ", theme::glyphs().dot))
        ),
        Role::Compute,
    ))
}

/// `dira zavet wiki` as lines — pure, so the layout is testable.
fn zavet_wiki_lines(
    v: &dira_core::protocol::ZavetWikiView,
    cols: usize,
    now: OffsetDateTime,
) -> Vec<String> {
    let l = ZavetLayout::for_width(
        cols,
        id_width(v.active.iter().chain(&v.superseded), &v.uncaptured),
    );
    let mut out = Vec::new();

    out.extend(zavet_header(
        &v.repo,
        v.branch.as_deref(),
        cols,
        Some(Seg::new("ZAVET", Role::Knowledge)),
    ));

    let mut summary = vec![
        plural(v.decisions_total, "decision"),
        plural(v.trailers, "trailer"),
        plural(v.guard_events, "guard event"),
    ];
    if v.specs_total > 0 {
        summary.push(plural(v.specs_total, "spec"));
    }
    out.push(dots(&summary));
    out.extend(wiki_attention_line(v));

    let mut section = |title: &str, note: Option<&str>, rows: Vec<&ZavetDecisionView>| {
        if rows.is_empty() {
            return;
        }
        out.push(String::new());
        out.extend(section_head(title, rows.len(), note, cols));
        for d in rows.iter().take(WIKI_SECTION_CAP) {
            out.push(decision_row(d, &l, now));
        }
        if let Some(rest) = rows.len().checked_sub(WIKI_SECTION_CAP).filter(|n| *n > 0) {
            out.push(format!(
                "{}{}",
                " ".repeat(ZAVET_INDENT),
                theme::paint(
                    &format!(
                        "{} {rest} more — dira zavet decisions",
                        theme::glyphs().ellipsis
                    ),
                    Role::Faint
                )
            ));
        }
    };
    let on_branch = |d: &&ZavetDecisionView| d.presence != Some(ZavetPresence::OffBranch);
    section(
        "ACTIVE DECISIONS",
        None,
        v.active.iter().filter(on_branch).collect(),
    );
    section(
        "SUPERSEDED",
        None,
        v.superseded.iter().filter(on_branch).collect(),
    );
    section(
        "OFF BRANCH",
        Some("recorded on another branch — not in this working tree"),
        v.active
            .iter()
            .chain(&v.superseded)
            .filter(|d| d.presence == Some(ZavetPresence::OffBranch))
            .collect(),
    );
    out.extend(uncaptured_lines(&v.uncaptured, &l, cols));

    if !v.specs.is_empty() {
        let slug_w = v
            .specs
            .iter()
            .map(|s| s.slug.len())
            .max()
            .unwrap_or(18)
            .clamp(12, 28);
        out.push(String::new());
        out.extend(section_head("SPECS", v.specs.len(), None, cols));
        // One title width for the whole section, sized to the widest tail, so
        // the badge column has a single left edge instead of ragging per row.
        let tails: Vec<Seg> = v.specs.iter().map(|s| spec_tail(s, slug_w, cols)).collect();
        let tail_w = tails
            .iter()
            .map(|t| display_width(&t.plain))
            .max()
            .unwrap_or(0);
        let title_w = cols
            .saturating_sub(ZAVET_INDENT + slug_w + 4 + tail_w)
            .clamp(12, 60);
        for (s, tail) in v.specs.iter().zip(&tails) {
            out.push(spec_row(s, slug_w, title_w, tail));
        }
    }

    if !v.recent.is_empty() {
        out.push(String::new());
        out.extend(section_head("RECENT KNOWLEDGE", v.recent.len(), None, cols));
        let w = cols
            .saturating_sub(ZAVET_INDENT + 9 + 2 + 11 + 2)
            .clamp(20, 80);
        for (sha, key, value) in &v.recent {
            out.push(format!(
                "{}{}  {}  {}",
                " ".repeat(ZAVET_INDENT),
                theme::paint(short_sha(sha), Role::Faint),
                theme::paint(&pad_cols(key, 11), Role::Knowledge),
                theme::paint(&truncate_cols(value, w), Role::Ink),
            ));
        }
    }
    if v.decisions_total == 0 && v.uncaptured.is_empty() {
        out.push(String::new());
        out.extend(
            wrap_words(
                "empty knowledge base — /zavet:decide records a decision, /zavet:backfill reverse-engineers an existing codebase",
                cols,
            )
            .iter()
            .map(|l| theme::paint(l, Role::Faint)),
        );
    }
    out
}

/// Render `dira status`: the summary block always; the detail sections
/// (ACTIVE SESSIONS / PARALLEL / TODAY) only under `--detailed`.
pub fn print_status(s: &StatusView, detailed: bool) {
    // Hide degenerate sessions — a bare SessionStart with no engaged time and no
    // agent activity is noise (e.g. a project you opened but didn't work in).
    let active: Vec<SessionView> = s.active.iter().filter(|v| has_time(v)).cloned().collect();

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

    // Two independent backlogs on two independent cursors, so both are reported
    // and either can be non-zero alone. A compute backlog with zero pending events
    // is the exact shape of the bug that made this line necessary.
    if s.sync_pending > 0 || s.tokens_pending > 0 {
        let mut parts = Vec::new();
        if s.sync_pending > 0 {
            parts.push(format!("{} event(s)", s.sync_pending));
        }
        if s.tokens_pending > 0 {
            parts.push(format!("{} token row(s)", s.tokens_pending));
        }
        println!(
            "\n{}",
            theme::paint(&format!("{} pending sync", parts.join(", ")), Role::Compute,)
        );
    }

    // Writer health (WP-B7, extended by issue #93): only surfaced when there's
    // something to say — a fully healthy writer prints nothing extra. Omitted
    // entirely for an older daemon (`writer_health: None`, skew-safe).
    if let Some(h) = &s.writer_health {
        if let Some(line) = writer_health_line(h, detailed) {
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

/// Build the `writer: …` status line for a [`WriterHealthView`], or `None`
/// when there's nothing worth surfacing. Pure (no printing), mirroring
/// [`sync_health_line`], so the gate is unit-testable.
///
/// `unattributed_token_rows` (issue #93) is `detailed`-only. It is NOT a
/// fault: every turn a harness runs outside a repo lands here, so on a normal
/// machine it climbs into the hundreds within a day and would put a permanent
/// red line under an entirely healthy `dira status` — the same failure mode
/// `sync_health_line` avoids for a never-linked device. Under `--detailed` it
/// still renders on its own (zero panics/stalls, not wedged), because that is
/// where an operator goes to ask why the compute total looks low.
fn writer_health_line(h: &dira_core::protocol::WriterHealthView, detailed: bool) -> Option<String> {
    let show_unattributed = detailed && h.unattributed_token_rows > 0;
    if h.panics == 0 && h.stalls == 0 && !h.wedged && !show_unattributed {
        return None;
    }
    let mut line = format!("writer: {} panic(s) caught", h.panics);
    if h.stalls > 0 {
        line.push_str(&format!(", {} stall(s) flagged", h.stalls));
    }
    if show_unattributed {
        line.push_str(&format!(
            ", {} token turn(s) with no repo — that usage is not counted",
            h.unattributed_token_rows
        ));
    }
    if h.wedged {
        line.push_str(", currently WEDGED — restart the daemon");
    }
    Some(line)
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
    health_line(
        h.consecutive_failures,
        h.last_error_kind.as_deref(),
        h.backoff_secs,
    )
}

/// One human-readable line about sync health, or `None` when there is nothing
/// worth saying.
///
/// Takes plain fields rather than a snapshot type because its two callers hold
/// different ones: `dira status` gets a `SyncHealthView` over the control
/// socket, `dira device status` reads a `SyncHealth` straight off `dira.db`.
/// They must never disagree about whether a daemon is healthy, and one function
/// is how that is guaranteed.
///
/// Two silences, both deliberate:
/// - `"skipped"` with zero failures — the device isn't configured or linked. It
///   is a NEUTRAL outcome, never a failure, so printing it would give a
///   never-linked daemon a permanent red line saying "0 consecutive failure(s)".
/// - no error kind and zero failures — steady state. Silence is the good news.
pub(crate) fn health_line(
    consecutive_failures: u32,
    last_error_kind: Option<&str>,
    backoff_secs: u64,
) -> Option<String> {
    if consecutive_failures == 0 && matches!(last_error_kind, None | Some("skipped")) {
        return None;
    }
    let mut line = format!("sync: {consecutive_failures} consecutive failure(s)");
    if let Some(kind) = last_error_kind {
        line.push_str(&format!(" ({kind})"));
    }
    if backoff_secs > 0 {
        line.push_str(&format!(", backing off {backoff_secs}s"));
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
            head.push_str(&dot_sep().painted);
            head.push_str(&theme::paint(&format!("{you} you"), Role::Engaged));
        }
        if s.today.total_human_seconds > 0 {
            let mult = s.today.total_agent_seconds as f64 / s.today.total_human_seconds as f64;
            head.push_str(&dot_sep().painted);
            head.push_str(&theme::paint(
                &format!("{mult:.1}{} parallel", theme::glyphs().times),
                Role::Accent,
            ));
        }
        lines.push(head);
    }

    // --- metric rows ----------------------------------------------------------
    let tokens = s.tokens.filter(|t| t.total_tokens > 0);
    let g = theme::glyphs();
    let mut rows: Vec<(&str, &str, String, String, Role)> = Vec::new();
    if s.today.total_human_seconds > 0 || s.today.total_agent_seconds > 0 || tokens.is_some() {
        rows.push((
            g.bullet,
            "engaged",
            hms(s.today.total_human_seconds),
            "billable base".to_string(),
            Role::Engaged,
        ));
        rows.push((
            g.diamond,
            "agent",
            hms(s.today.total_agent_seconds),
            "wall-clock".to_string(),
            Role::Agent,
        ));
    }
    if let Some(t) = tokens {
        // `◇` is hollow on purpose: compute is an estimate, not measured time.
        rows.push((
            g.diamond_hollow,
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
            truncate_cols(&s.handle, 8),
            truncate_cols(dira_sources::harness_id(s.harness), 10),
            truncate_cols(kind_label(s.kind), 7),
            truncate_cols(&project_label(&s.project), pw),
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
        bits.push(format!("\u{201c}{}\u{201d}", truncate_cols(n, 56)));
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
    // Only sessions with real agent evidence get a lane. A manual `dira start`
    // stopwatch is not a parallel agent, and listing one both invented a lane and
    // skewed the ×-today multiplier next to it. This filter was missing entirely
    // here — the TUI had one, but mis-cased, so neither surface actually applied
    // it (see `SessionView::accrues_agent_time`).
    let agents: Vec<&SessionView> = active.iter().filter(|s| s.accrues_agent_time()).collect();
    if agents.is_empty() {
        return;
    }
    let lw = layout.parallel_label;
    let bw = layout.bar_cells;
    let eng = today.total_human_seconds;
    let max = agents
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
        let mult = theme::paint(
            &format!("{parallel:.1}{} today", theme::glyphs().times),
            Role::Accent,
        );
        println!(
            "{head}  {d}  {} agent(s){you} {d} {mult}",
            agents.len(),
            d = dot_glyph()
        );
    } else {
        println!(
            "{head}  {d}  {} agent(s){you}",
            agents.len(),
            d = dot_glyph()
        );
    }
    println!();
    // `◆` marks an agent lane (purple), `●` the deduped human baseline (teal) —
    // the same shape/colour language as the cloud's "Right Now" view. Painting
    // only the glyph keeps the padded label column aligned.
    let agent_mark = theme::paint(theme::glyphs().diamond, Role::Agent);
    for s in &agents {
        let label = truncate_cols(
            &format!(
                "{} {} {}",
                dira_sources::harness_id(s.harness),
                dot_glyph(),
                repo_short(&s.project)
            ),
            lw,
        );
        println!(
            "  {agent_mark} {label:<lw$}   {}   {:>8}",
            bar(s.agent_seconds as f64 / max as f64, bw),
            hms(s.agent_seconds),
        );
    }
    println!(
        "  {} {:<lw$}   {}   {:>8}",
        theme::paint(theme::glyphs().bullet, Role::Engaged),
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
            truncate_cols(&project_label(&p.project), pw),
            hms(p.human_seconds),
            hms(p.agent_wall_seconds),
        );
    }
    let dash = theme::glyphs().dash;
    let total = format!(
        "  {:<pw$} {:>10} {:>10}",
        format!("{dash} total {dash}"),
        hms(r.total_human_seconds),
        hms(r.total_agent_seconds),
    );
    println!("{}", theme::paint(&total, Role::Ink));
    println!("  ({} session(s))", r.session_count);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uncaptured_row(reason: &str) -> ZavetUncapturedView {
        ZavetUncapturedView {
            reason: reason.to_string(),
            ..Default::default()
        }
    }

    /// An `awaiting sweep` record is already committed, so the hint must not
    /// tell its author to commit it — that is a fix which cannot work. Now that
    /// `dira zavet sync` exists, there is a real remedy to name instead.
    #[test]
    fn the_uncaptured_hint_matches_the_reasons_present() {
        let sync_only = uncaptured_hint(&[uncaptured_row("awaiting sweep")]);
        assert!(sync_only.contains("dira zavet sync"));
        assert!(
            !sync_only.contains("commit them") && !sync_only.contains("commit it"),
            "these are already committed; it must not ask for that: {sync_only}"
        );

        let commit_only = uncaptured_hint(&[uncaptured_row("uncommitted")]);
        assert!(commit_only.contains("commit them"));
        assert!(!commit_only.contains("dira zavet sync"));

        let both = uncaptured_hint(&[
            uncaptured_row("uncommitted"),
            uncaptured_row("awaiting sweep"),
        ]);
        assert!(both.contains("commit them") && both.contains("dira zavet sync"));
    }

    /// DIRASH-0028's closing directive is "if a walk is bounded, say so in the
    /// output". The trailer pass is the bounded half, so the disclosure has to
    /// be present when it applies and absent when it doesn't — otherwise a full
    /// scan reads as partial, or worse, a partial one reads as exhaustive.
    #[test]
    fn a_bounded_trailer_scan_says_so_and_a_full_one_does_not() {
        let view = |bounded: bool| ZavetReindexView {
            repo: "github.com/acme/api".into(),
            active: true,
            trailers_bounded: bounded,
            ..Default::default()
        };
        let bounded = zavet_reindex_lines(&view(true)).join("\n");
        assert!(bounded.contains("--all-trailers"), "{bounded}");

        let full = zavet_reindex_lines(&view(false)).join("\n");
        assert!(!full.contains("bounded"), "{full}");
        assert!(!full.contains("--all-trailers"), "{full}");
    }

    /// An inactive repo must not render a metric block of zeroes — that reads
    /// as "indexed nothing because there is nothing", not "did not look".
    #[test]
    fn an_inactive_repo_reports_why_instead_of_zero_counts() {
        let lines = zavet_reindex_lines(&ZavetReindexView {
            repo: "github.com/acme/api".into(),
            active: false,
            ..Default::default()
        });
        let text = lines.join("\n");
        assert!(text.contains("inactive"), "{text}");
        assert!(!text.contains("decisions"), "no metric block: {text}");
    }

    /// `sync` honors the capture baseline, so it can never pick up a record
    /// from history BEHIND that baseline — a fresh clone's whole back catalogue.
    /// From the uncaptured probe those are indistinguishable from a
    /// just-committed record, so every hint that names `sync` must also name
    /// the command that works when it doesn't, or the user re-runs a no-op
    /// forever (DIRASH-0028).
    #[test]
    fn every_sync_hint_offers_reindex_as_the_fallback() {
        for rows in [
            vec![uncaptured_row("awaiting sweep")],
            vec![
                uncaptured_row("uncommitted"),
                uncaptured_row("awaiting sweep"),
            ],
        ] {
            let hint = uncaptured_hint(&rows);
            assert!(
                hint.contains("reindex"),
                "a hint naming sync must name its fallback: {hint}"
            );
        }
    }

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
            harness: dira_contract::Harness::ClaudeCode,
            kind: dira_contract::SessionKind::Agent,
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
            has_agent_activity: true,
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
            tokens_pending: 0,
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

    use dira_core::protocol::{SyncHealthView, WriterHealthView};

    /// Under `--detailed`, a nonzero `unattributed_token_rows` renders even
    /// with an otherwise spotless writer — issue #93's whole point is that a
    /// healthy-looking writer can still be silently losing compute.
    #[test]
    fn writer_health_line_surfaces_unattributed_token_rows_when_detailed() {
        let h = WriterHealthView {
            panics: 0,
            stalls: 0,
            idle_secs: Some(2),
            wedged: false,
            unattributed_token_rows: 142,
        };
        assert_eq!(
            writer_health_line(&h, true),
            Some(
                "writer: 0 panic(s) caught, 142 token turn(s) with no repo — that usage is not counted"
                    .to_string()
            )
        );
    }

    /// …and stays out of the default view entirely. Repo-less turns are
    /// routine, so surfacing them there marks a healthy writer as unhealthy on
    /// every single invocation.
    #[test]
    fn writer_health_line_hides_unattributed_token_rows_by_default() {
        let h = WriterHealthView {
            panics: 0,
            stalls: 0,
            idle_secs: Some(2),
            wedged: false,
            unattributed_token_rows: 671,
        };
        assert_eq!(writer_health_line(&h, false), None);
    }

    /// A real fault still prints in the default view — and without the
    /// repo-less clause riding along.
    #[test]
    fn writer_health_line_reports_a_real_fault_without_the_unattributed_clause() {
        let h = WriterHealthView {
            panics: 2,
            stalls: 0,
            idle_secs: Some(2),
            wedged: false,
            unattributed_token_rows: 671,
        };
        assert_eq!(
            writer_health_line(&h, false),
            Some("writer: 2 panic(s) caught".to_string())
        );
    }

    #[test]
    fn writer_health_line_is_silent_when_everything_is_clean() {
        let h = WriterHealthView {
            panics: 0,
            stalls: 0,
            idle_secs: Some(2),
            wedged: false,
            unattributed_token_rows: 0,
        };
        assert_eq!(writer_health_line(&h, false), None);
        assert_eq!(writer_health_line(&h, true), None);
    }

    /// Issue #94: the generic fallback must carry the SAME disclosure as
    /// `dira device resync`'s own summary. A user told only "cursor rewound"
    /// reasonably assumes every stream rewound — the one thing `--from` does
    /// not do.
    #[test]
    fn resync_fallback_discloses_that_only_the_event_cursor_moved() {
        let lines = resync_fallback_lines(3, 0, Some("01EVENTID"));
        let joined = lines.join("\n");
        assert!(joined.contains("01EVENTID"), "{joined}");
        assert!(
            joined.contains("only the event cursor moved"),
            "a --from rewind must say what it did NOT rewind: {joined}"
        );
        assert!(
            joined.contains("dira device resync"),
            "and name the command that does re-send everything: {joined}"
        );
    }

    #[test]
    fn resync_fallback_reports_the_token_backlog_only_when_nonzero() {
        let full = resync_fallback_lines(2, 48_601, None).join("\n");
        assert!(full.contains("48601 token usage row(s)"), "{full}");
        assert!(
            !full.contains("only the event cursor moved"),
            "a full rewind has nothing to disclaim: {full}"
        );

        let quiet = resync_fallback_lines(2, 0, None).join("\n");
        assert!(
            !quiet.contains("token usage row"),
            "no backlog means no token line: {quiet}"
        );
    }

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

    // -----------------------------------------------------------------
    // zavet list views
    // -----------------------------------------------------------------

    fn dec(id: &str, title: &str, guards: usize) -> ZavetDecisionView {
        ZavetDecisionView {
            id: id.to_string(),
            title: Some(title.to_string()),
            status: Some("active".into()),
            path: format!(".zavet/decisions/{id}.md"),
            guards: (0..guards)
                .map(|i| format!("src/lib/some/deeply/nested/module/path-{i}.ts"))
                .collect(),
            created_at: Some("2026-08-01T00:00:00Z".into()),
            ..Default::default()
        }
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::parse(
            "2026-08-10T00:00:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap()
    }

    fn decisions_view(decisions: Vec<ZavetDecisionView>) -> ZavetDecisionsView {
        ZavetDecisionsView {
            repo: "gitlab.com/teamschedule/time-schedule-application".into(),
            branch: Some("1881-time-related-configuration-overrides".into()),
            decisions,
            uncaptured: Vec::new(),
        }
    }

    /// The defect the whole layout change exists to fix: a decision with a
    /// fistful of long guard globs used to emit an unbounded second line that
    /// wrapped two or three times in a real terminal. One record is one row.
    #[test]
    fn a_decision_with_long_guards_is_still_one_row() {
        let v = decisions_view(vec![dec(
            "D-0001",
            "Deploy-skew defense is one coordinated system",
            8,
        )]);
        let lines = zavet_decisions_lines(&v, 100, RowOpts::default(), now());
        let rows: Vec<_> = lines.iter().filter(|l| l.contains("D-0001")).collect();
        assert_eq!(rows.len(), 1, "expected one row, got {rows:#?}");
        assert!(rows[0].contains("8 guards"));
        assert!(!rows[0].contains("src/lib/some"));
    }

    /// Every emitted line has to fit the terminal it was measured for —
    /// otherwise the renderer has merely moved the wrap, not removed it.
    #[test]
    fn no_line_exceeds_the_terminal_width() {
        let v = decisions_view(vec![
            dec(
                "DIRASH-0024",
                "Repo-scope zavet writes are gated on cwd, and dira never sets core.hooksPath",
                6,
            ),
            dec(
                "D-0002",
                "Награди names the points-spending surface; магазин is the store",
                3,
            ),
        ]);
        for cols in [60, 80, 100, 120, 200] {
            for line in zavet_decisions_lines(&v, cols, RowOpts::default(), now()) {
                assert!(
                    display_width(&line) <= cols,
                    "{cols}-col line overflows ({}): {line:?}",
                    display_width(&line)
                );
            }
        }
    }

    /// `--guards` restores the globs, wrapped under the title column rather
    /// than run out to whatever length the repo's paths happen to be.
    #[test]
    fn guards_flag_wraps_globs_within_the_width() {
        let v = decisions_view(vec![dec("D-0001", "A decision", 8)]);
        let opts = RowOpts {
            guards: true,
            ..Default::default()
        };
        let lines = zavet_decisions_lines(&v, 100, opts, now());
        assert!(lines.iter().any(|l| l.contains("src/lib/some")));
        for line in &lines {
            assert!(display_width(line) <= 100, "overflow: {line:?}");
        }
    }

    /// Off-branch records get their own group and are never dropped — id
    /// allocation is repo-wide, so the row has to stay reachable.
    #[test]
    fn off_branch_decisions_are_grouped_not_hidden() {
        let mut here = dec("D-0005", "On this branch", 1);
        here.presence = Some(ZavetPresence::OnBranch);
        let mut elsewhere = dec("D-0001", "From another branch", 1);
        elsewhere.presence = Some(ZavetPresence::OffBranch);
        let v = decisions_view(vec![elsewhere, here]);

        let lines = zavet_decisions_lines(&v, 120, RowOpts::default(), now());
        let text = lines.join("\n");
        assert!(text.contains("OFF BRANCH"));
        assert!(text.contains("D-0001"));
        let off_at = lines.iter().position(|l| l.contains("OFF BRANCH")).unwrap();
        let d1_at = lines.iter().position(|l| l.contains("D-0001")).unwrap();
        let d5_at = lines.iter().position(|l| l.contains("D-0005")).unwrap();
        assert!(
            d5_at < off_at && off_at < d1_at,
            "sections out of order:\n{text}"
        );
    }

    /// `--branch` narrows to the checked-out branch but still says how many it
    /// set aside — a record must never silently vanish from the list.
    #[test]
    fn branch_only_still_reports_what_it_excluded() {
        let mut elsewhere = dec("D-0001", "From another branch", 1);
        elsewhere.presence = Some(ZavetPresence::OffBranch);
        let v = decisions_view(vec![elsewhere]);
        let opts = RowOpts {
            branch_only: true,
            ..Default::default()
        };
        let text = zavet_decisions_lines(&v, 120, opts, now()).join("\n");
        assert!(!text.contains("OFF BRANCH"));
        assert!(text.contains("1 recorded on other branches"));
    }

    /// Uncaptured records name the file AND the remedy. The whole point is that
    /// "I can see it in my editor" and "dira does not list it" stop reading as
    /// a bug.
    #[test]
    fn uncaptured_records_report_their_reason() {
        let mut v = decisions_view(vec![dec("D-0005", "Captured", 1)]);
        v.uncaptured = vec![ZavetUncapturedView {
            id: Some("D-0008".into()),
            title: Some("Unassigned configuration resolution".into()),
            path: ".zavet/decisions/D-0008-unassigned.md".into(),
            reason: "uncommitted".into(),
            kind: "decision".into(),
        }];
        let text = zavet_decisions_lines(&v, 120, RowOpts::default(), now()).join("\n");
        assert!(text.contains("UNCAPTURED"));
        assert!(text.contains("D-0008"));
        assert!(text.contains("uncommitted"));
        assert!(text.contains("commit them"));
    }

    /// Presence is unknown without a working directory, and unknown renders as
    /// one plain list — never as "off branch", which would be a guess.
    #[test]
    fn unknown_presence_produces_no_branch_sections() {
        let v = ZavetDecisionsView {
            branch: None,
            ..decisions_view(vec![dec("D-0001", "Something", 2)])
        };
        let text = zavet_decisions_lines(&v, 120, RowOpts::default(), now()).join("\n");
        assert!(!text.contains("OFF BRANCH"));
        assert!(text.contains("ACTIVE"));
    }

    /// A long repo plus a long branch is the common case on a ticket-numbered
    /// branch; both are identifiers worth copying, so the header wraps rather
    /// than clipping either one.
    #[test]
    fn a_long_header_wraps_instead_of_overflowing() {
        let v = decisions_view(vec![dec("D-0001", "x", 0)]);
        let lines = zavet_decisions_lines(&v, 80, RowOpts::default(), now());
        assert!(display_width(&lines[0]) <= 80, "{:?}", lines[0]);
        assert!(lines[1].contains("1881-time-related-config"));
        // At a width that fits, it stays one line.
        let wide = zavet_decisions_lines(&v, 160, RowOpts::default(), now());
        assert!(wide[0].contains("1881-time-related-config"));
    }

    #[test]
    fn age_label_marks_recent_records_and_ignores_the_future() {
        let at = |s: &str| age_label(Some(s), now()).map(|(t, _)| t);
        assert_eq!(at("2026-08-09T01:00:00Z").as_deref(), Some("new"));
        assert_eq!(at("2026-08-08T00:00:00Z").as_deref(), Some("2d"));
        assert_eq!(at("2026-05-10T00:00:00Z").as_deref(), Some("3mo"));
        assert_eq!(at("2024-08-10T00:00:00Z").as_deref(), Some("2y"));
        // Clock skew or a rebased author date: say nothing, never a negative.
        assert_eq!(at("2026-09-01T00:00:00Z"), None);
        assert_eq!(age_label(None, now()), None);
    }

    /// `blocked` and `complied` both mean the guard prevented a change; `shown`
    /// is not evidence of anything and must not inflate the count.
    #[test]
    fn activity_folds_kept_and_separates_overrides() {
        let stat = |kind: &str, total: u64| ZavetGuardStatView {
            kind: kind.to_string(),
            total,
            unattributed: 0,
        };
        assert_eq!(activity_label(&[]), None);
        assert_eq!(activity_label(&[stat("guard_shown", 9)]), None);
        assert_eq!(
            activity_label(&[stat("guard_blocked", 7), stat("guard_complied", 5)]).as_deref(),
            Some("12 kept")
        );
        assert_eq!(
            activity_label(&[stat("guard_overridden", 2)]).as_deref(),
            Some("2 over")
        );
    }

    /// Right-aligned columns are measured in cells, not chars — one ideograph
    /// is two columns wide.
    #[test]
    fn truncate_cols_counts_display_cells() {
        assert_eq!(display_width("日本語"), 6);
        assert!(display_width(&crate::format::truncate_cols("日本語テスト", 6)) <= 6);
        assert_eq!(crate::format::truncate_cols("abc", 10), "abc");
    }

    /// The overview leads with what needs a human, and stays silent when
    /// nothing does — its presence is the signal.
    #[test]
    fn wiki_attention_line_summarizes_only_real_problems() {
        let mut v = dira_core::protocol::ZavetWikiView {
            repo: "gitlab.com/team/app".into(),
            active: vec![dec("D-0001", "Fine", 1)],
            ..Default::default()
        };
        assert_eq!(wiki_attention_line(&v), None);

        v.uncaptured = vec![ZavetUncapturedView {
            reason: "uncommitted".into(),
            kind: "decision".into(),
            ..Default::default()
        }];
        v.active[0].presence = Some(ZavetPresence::OffBranch);
        let line = wiki_attention_line(&v).unwrap();
        assert!(line.contains("1 uncaptured"), "{line}");
        assert!(line.contains("1 off branch"), "{line}");
    }

    /// The wiki's uncaptured section mixes decision ids with spec slugs, which
    /// are longer. A slug wider than the id column used to push its whole row
    /// past the terminal edge.
    #[test]
    fn a_long_uncaptured_slug_does_not_widen_its_row() {
        let mut v = decisions_view(vec![dec("D-0001", "Something", 1)]);
        v.uncaptured = vec![ZavetUncapturedView {
            id: Some("a-very-long-living-spec-slug-indeed".into()),
            title: Some("Some spec".into()),
            path: ".zavet/specs/a-very-long-living-spec-slug-indeed.md".into(),
            reason: "awaiting sweep".into(),
            kind: "spec".into(),
        }];
        for cols in [60, 80, 100, 200] {
            for line in zavet_decisions_lines(&v, cols, RowOpts::default(), now()) {
                assert!(
                    display_width(&line) <= cols,
                    "{cols}-col overflow ({}): {line:?}",
                    display_width(&line)
                );
            }
        }
    }

    /// Strip SGR sequences, so a painted line can be measured as the terminal
    /// would see it.
    fn strip_sgr(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    /// The regression `Seg` exists to prevent, exercised with colour ON.
    ///
    /// Every other width assertion in this module runs with `paint` as a no-op,
    /// so none of them can catch a renderer that measures painted text. This
    /// one forces colour, strips the escapes back off, and measures what the
    /// terminal would actually show. Before `Seg`, the spec tail and the header
    /// both failed here while the rest of the suite stayed green.
    #[test]
    fn every_view_fits_its_width_with_colour_on() {
        theme::force_color(Some(true));
        let decisions = decisions_view(vec![
            dec("DIRASH-0024", "Repo-scope zavet writes are gated on cwd", 6),
            dec("D-0002", "Награди names the points-spending surface", 3),
        ]);
        let wiki = dira_core::protocol::ZavetWikiView {
            repo: "gitlab.com/teamschedule/time-schedule-application".into(),
            branch: Some("1881-time-related-configuration-overrides".into()),
            decisions_total: 2,
            specs_total: 1,
            active: decisions.decisions.clone(),
            specs: vec![ZavetSpecView {
                slug: "distribution-and-update".into(),
                title: Some("Distribution and self-update".into()),
                origin: Some("reverse-engineered".into()),
                confidence: Some("low".into()),
                stale_commits: Some(4),
                paths: (0..6).map(|i| format!("p{i}")).collect(),
                decisions: (0..13).map(|i| format!("D-{i:04}")).collect(),
                ..Default::default()
            }],
            recent: vec![("f6ea343678fd".into(), "why".into(), "a trailer".into())],
            ..Default::default()
        };
        let opts = RowOpts {
            guards: true,
            branch_only: false,
        };
        for cols in [60, 80, 100, 140, 200] {
            let lines = zavet_decisions_lines(&decisions, cols, opts, now())
                .into_iter()
                .chain(zavet_wiki_lines(&wiki, cols, now()));
            for line in lines {
                let plain = strip_sgr(&line);
                assert!(
                    line.contains('\x1b') || plain.trim().is_empty(),
                    "colour was not applied — this test proves nothing: {line:?}"
                );
                assert!(
                    display_width(&plain) <= cols,
                    "{cols}-col overflow ({}): {plain:?}",
                    display_width(&plain)
                );
            }
        }
        // Overflow is only half the failure. Measuring painted text makes a
        // segment look ~10 columns wider per part, so the real symptom is the
        // opposite: badges silently DROP at a width where they fit. Assert the
        // full tail survives on a wide terminal, or this test passes against
        // the very bug it exists for.
        let wide = zavet_wiki_lines(&wiki, 200, now()).join("\n");
        let wide = strip_sgr(&wide);
        assert!(wide.contains("unverified"), "trust badge dropped:\n{wide}");
        assert!(wide.contains("stale"), "staleness badge dropped:\n{wide}");
        assert!(wide.contains("13 decisions"), "counts dropped:\n{wide}");
        assert!(
            wide.contains("reverse-engineered"),
            "provenance dropped:\n{wide}"
        );
        theme::force_color(None);
    }

    /// The invariant behind [`Seg`]: the measured half never carries SGR bytes.
    ///
    /// This cannot be caught by rendering assertions — `theme::paint` is a
    /// no-op whenever stdout is not a TTY, which is always true under `cargo
    /// test`, so a renderer that measures painted text passes every width test
    /// and then collapses in a real terminal. It happened once; the type is the
    /// fix and this pins it.
    #[test]
    fn seg_measures_plain_text_only() {
        let a = Seg::new("verified", Role::Engaged);
        let b = Seg::new("stale 4", Role::Compute);
        let sep = Seg::new(" · ", Role::Muted);
        let joined = Seg::joined(&[a, Seg::new("", Role::Faint), b], &sep);
        assert_eq!(joined.plain, "verified · stale 4");
        assert!(!joined.plain.contains('\x1b'));
        // Empty segments drop out rather than leaving a dangling separator.
        assert!(!joined.plain.contains("·  ·"));
    }

    /// Spec rows carry the widest badge set in either view; at 80 columns the
    /// tail has to shed segments rather than run off the edge. Trust
    /// (verified + staleness) is the last thing dropped — it is what says
    /// whether the document can be believed right now.
    #[test]
    fn spec_rows_shed_badges_instead_of_overflowing() {
        let spec = |slug: &str, stale: Option<u64>| ZavetSpecView {
            slug: slug.to_string(),
            title: Some("Distribution and self-update across every platform".into()),
            origin: Some("reverse-engineered".into()),
            confidence: Some("low".into()),
            verified: None,
            stale_commits: stale,
            paths: (0..6).map(|i| format!("p{i}")).collect(),
            decisions: (0..13).map(|i| format!("D-{i:04}")).collect(),
            ..Default::default()
        };
        let v = dira_core::protocol::ZavetWikiView {
            repo: "gitlab.com/team/app".into(),
            specs_total: 2,
            specs: vec![
                spec("distribution-and-update", Some(4)),
                spec("attestation-sync", Some(0)),
            ],
            ..Default::default()
        };
        for cols in [60, 80, 100, 140, 200] {
            for line in zavet_wiki_lines(&v, cols, now()) {
                assert!(
                    display_width(&line) <= cols,
                    "{cols}-col overflow ({}): {line:?}",
                    display_width(&line)
                );
            }
        }
        // Even squeezed to 80, the trust badges survive.
        let narrow = zavet_wiki_lines(&v, 80, now()).join("\n");
        assert!(narrow.contains("unverified"), "{narrow}");
        // One badge vocabulary across the views: the wiki says exactly what
        // `dira zavet why` says, which is the point of the shared builder.
        assert!(narrow.contains("stale · 4 commits"), "{narrow}");
    }

    /// The wiki is a landing page: it caps each section and points at the full
    /// list rather than printing eighty rows before the specs.
    #[test]
    fn wiki_caps_sections_and_says_what_it_held_back() {
        let v = dira_core::protocol::ZavetWikiView {
            repo: "gitlab.com/team/app".into(),
            decisions_total: 14,
            active: (1..=14)
                .map(|i| dec(&format!("D-{i:04}"), "A recorded decision", 2))
                .collect(),
            ..Default::default()
        };
        let lines = zavet_wiki_lines(&v, 120, now());
        let shown = lines
            .iter()
            .filter(|l| l.contains("A recorded decision"))
            .count();
        assert_eq!(shown, WIKI_SECTION_CAP);
        assert!(lines
            .iter()
            .any(|l| l.contains("4 more — dira zavet decisions")));
        for line in &lines {
            assert!(display_width(line) <= 120, "overflow: {line:?}");
        }
    }
}
