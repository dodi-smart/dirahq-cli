//! Pure formatting helpers shared by the plain-text renderer (`render.rs`) and
//! the live TUI dashboard (`tui`). Keeping them here means both surfaces format
//! durations, projects and bars identically — the TUI is just another view over
//! the same numbers.

use crate::theme::Role;
use dira_core::protocol::BillingView;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Format seconds as `1h 30m` / `12m 05s` / `45s`. Negatives clamp to zero.
pub fn hms(seconds: i64) -> String {
    let s = seconds.max(0);
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {sec:02}s")
    } else {
        format!("{sec}s")
    }
}

/// Human label for an optional project, used everywhere a project may be
/// unresolved (no cwd → project mapping yet).
pub fn project_label(p: &Option<String>) -> String {
    p.clone().unwrap_or_else(|| "(unresolved)".to_string())
}

/// What orchestration time is called wherever a repo name would go.
pub const ORCHESTRATION_LABEL: &str = "orchestration";

/// What an orchestrator that spawned nothing and touched no repo is called
/// (#181): an hourly scheduled run in a scratch workspace, say. Display only —
/// the session keeps its role, and its repo-less time stays in the
/// orchestration report bucket (DIRASH-0041).
pub const SCRATCH_LABEL: &str = "scratch session";

/// The KIND cell: `orch` for an orchestrator, except one shown as a
/// [`SCRATCH_LABEL`] — it coordinated nothing, so it reads as the session it is.
fn kind_cell(s: &dira_core::protocol::SessionView, scratch: bool) -> &'static str {
    if s.orchestrator && !scratch {
        "orch"
    } else {
        kind_label(s.kind)
    }
}

/// Keep the first `n` top-level entries of `sessions` (a run counts as one,
/// grouped by [`dira_core::runs::group`]), in list order, and say how many
/// entries were left out. The one `--limit` rule for the table and `--json`.
pub fn limit_entries(
    sessions: &[dira_core::protocol::SessionView],
    n: usize,
) -> (Vec<dira_core::protocol::SessionView>, usize) {
    use dira_core::runs::{group, Entry};
    let entries = group(sessions);
    let hidden = entries.len().saturating_sub(n);
    let keep: std::collections::HashSet<&str> = entries
        .iter()
        .take(n)
        .flat_map(|e| match e {
            Entry::Session(s) => vec![s.session_id.as_str()],
            Entry::Run(r) => std::iter::once(r.root)
                .chain(r.members.iter().copied())
                .map(|s| s.session_id.as_str())
                .collect(),
        })
        .collect();
    let kept = sessions
        .iter()
        .filter(|s| keep.contains(s.session_id.as_str()))
        .cloned()
        .collect();
    (kept, hidden)
}

/// The sessions a status tree lays out: the live ones, then the ended
/// children of listed runs (#182), so a run keeps its members after they end.
/// Counts and lanes stay on `active` alone.
pub fn status_tree_sessions(
    s: &dira_core::protocol::StatusView,
) -> Vec<dira_core::protocol::SessionView> {
    s.active.iter().chain(&s.run_children).cloned().collect()
}

/// The PROJECT cell for a report line: the orchestration bucket is named as
/// such, never folded into `(unresolved)`.
pub fn report_line_label(p: &dira_core::report::ProjectReport) -> String {
    if p.orchestration {
        ORCHESTRATION_LABEL.to_string()
    } else {
        project_label(&p.project)
    }
}

/// The PROJECT cell for a session. An orchestrator names no single repo, so
/// it reads as `orchestration` (with how many repos it reached) rather than
/// `(unresolved)`, which means "we could not tell".
pub fn session_project_label(s: &dira_core::protocol::SessionView) -> String {
    if !s.orchestrator {
        return project_label(&s.project);
    }
    match s.repos.len() {
        0 => ORCHESTRATION_LABEL.to_string(),
        1 => format!("{ORCHESTRATION_LABEL} · {}", s.repos[0]),
        n => format!("{ORCHESTRATION_LABEL} · {n} repos"),
    }
}

/// [`repo_short`] for a session, orchestrator-aware like
/// [`session_project_label`].
pub fn session_repo_short(s: &dira_core::protocol::SessionView) -> String {
    if s.orchestrator {
        ORCHESTRATION_LABEL.to_string()
    } else {
        repo_short(&s.project)
    }
}

/// One line of a session list laid out as orchestration run trees: an
/// ordinary session, a run's root, one of its members, or a repo only the
/// orchestrator touched directly (`session: None`).
#[derive(Debug, Clone)]
pub struct TreeRow<'a> {
    /// Two-column gutter: `▾ ` on a run root with rows under it, `├ `/`└ ` on
    /// those rows, blank otherwise.
    pub glyph: &'static str,
    pub session: Option<&'a dira_core::protocol::SessionView>,
    pub handle: String,
    pub project: String,
    /// A short project label for narrow places (the dashboard's lanes).
    pub short: String,
    /// The KIND cell ([`kind_cell`]; `direct` on a direct-repo row).
    pub kind: &'static str,
}

/// Lay a session list out as run trees with [`dira_core::runs::group`] — the
/// one grouping every view shares (`dira sessions`, `status`, `watch`). A run
/// over at most one repo is folded and reads as that repo's work; a wider one
/// is named by its local title (never synced) or `orchestration`.
pub fn tree_rows<'a>(
    sessions: &'a [dira_core::protocol::SessionView],
    titles: &std::collections::HashMap<String, String>,
) -> Vec<TreeRow<'a>> {
    use dira_core::runs::{group, Entry};
    let mut out = Vec::new();
    for entry in group(sessions) {
        match entry {
            Entry::Session(s) => out.push(session_row("  ", s)),
            Entry::Run(run) => {
                let root = run.root;
                let scratch = run.is_scratch();
                let (project, short) = if scratch {
                    (SCRATCH_LABEL.to_string(), "scratch".to_string())
                } else if run.folded() {
                    let repo = run.repos.first().cloned();
                    (
                        repo.clone()
                            .unwrap_or_else(|| ORCHESTRATION_LABEL.to_string()),
                        repo_short(&repo.or_else(|| Some(ORCHESTRATION_LABEL.to_string()))),
                    )
                } else {
                    let name = titles
                        .get(&root.session_id)
                        .cloned()
                        .unwrap_or_else(|| ORCHESTRATION_LABEL.to_string());
                    (format!("{name} · {} repos", run.repos.len()), name)
                };
                let direct: &[String] = if run.folded() { &[] } else { &run.direct };
                let total = run.members.len() + direct.len();
                out.push(TreeRow {
                    glyph: if total == 0 { "  " } else { "▾ " },
                    session: Some(root),
                    handle: root.handle.clone(),
                    project,
                    short,
                    kind: kind_cell(root, scratch),
                });
                let branch = |i: usize| if i + 1 == total { "└ " } else { "├ " };
                for (i, s) in run.members.iter().enumerate() {
                    out.push(session_row(branch(i), s));
                }
                for (k, repo) in direct.iter().enumerate() {
                    out.push(TreeRow {
                        glyph: branch(run.members.len() + k),
                        session: None,
                        handle: "·".to_string(),
                        project: repo.clone(),
                        short: repo_short(&Some(repo.clone())),
                        kind: "direct",
                    });
                }
            }
        }
    }
    out
}

/// A row for a session that is not a run root.
fn session_row<'a>(glyph: &'static str, s: &'a dira_core::protocol::SessionView) -> TreeRow<'a> {
    TreeRow {
        glyph,
        session: Some(s),
        handle: s.handle.clone(),
        project: session_project_label(s),
        short: session_repo_short(s),
        kind: kind_cell(s, false),
    }
}

/// `sessions` one row each, in list order (`dira sessions --flat`), labelled
/// the way [`tree_rows`] labels them — a run root that reads as a
/// [`SCRATCH_LABEL`] there reads as one here.
pub fn flat_rows(sessions: &[dira_core::protocol::SessionView]) -> Vec<TreeRow<'_>> {
    use dira_core::runs::{group, Entry};
    let scratch: std::collections::HashSet<&str> = group(sessions)
        .iter()
        .filter_map(|e| match e {
            Entry::Run(r) if r.is_scratch() => Some(r.root.session_id.as_str()),
            _ => None,
        })
        .collect();
    sessions
        .iter()
        .map(|s| {
            let mut row = session_row("  ", s);
            if scratch.contains(s.session_id.as_str()) {
                row.project = SCRATCH_LABEL.to_string();
                row.short = "scratch".to_string();
                row.kind = kind_cell(s, true);
            }
            row
        })
        .collect()
}

/// The agent lanes' rows (`status --detailed` PARALLEL and the `watch`
/// chart): run trees grouped over `tree` — so a root whose children ended
/// (#182) keeps its run label — keeping only sessions in `live` that `keep`
/// accepts, with branches re-closed. Yields each session, its gutter glyph
/// (`""` off-tree) and its short label.
pub fn lane_rows<'a>(
    tree: &'a [dira_core::protocol::SessionView],
    live: &[dira_core::protocol::SessionView],
    titles: &std::collections::HashMap<String, String>,
    keep: impl Fn(&dira_core::protocol::SessionView) -> bool,
) -> Vec<(&'a dira_core::protocol::SessionView, &'static str, String)> {
    let live: std::collections::HashSet<&str> =
        live.iter().map(|s| s.session_id.as_str()).collect();
    let kept: Vec<TreeRow<'a>> = tree_rows(tree, titles)
        .into_iter()
        .filter(|r| {
            r.session
                .is_some_and(|s| live.contains(s.session_id.as_str()) && keep(s))
        })
        .collect();
    let mut glyphs: Vec<&'static str> = kept.iter().map(|r| r.glyph).collect();
    retree(&mut glyphs);
    kept.into_iter()
        .zip(glyphs)
        .map(|(r, g)| {
            let s = r.session.expect("filtered to sessions");
            (s, if g == "  " { "" } else { g }, r.short)
        })
        .collect()
}

/// Re-close tree branches after rows were filtered out: a member glyph
/// (`├ `/`└ `) becomes `└ ` when no member of the same run follows it, and
/// `├ ` otherwise. A run root whose rows all dropped loses its `▾ `.
pub fn retree(glyphs: &mut [&'static str]) {
    let is_member = |g: &str| g == "├ " || g == "└ ";
    for i in 0..glyphs.len() {
        let next_is_member = glyphs.get(i + 1).is_some_and(|g| is_member(g));
        if is_member(glyphs[i]) {
            glyphs[i] = if next_is_member { "├ " } else { "└ " };
        } else if glyphs[i] == "▾ " && !next_is_member {
            glyphs[i] = "  ";
        }
    }
}

/// A short, last-segment label for a project path/slug — e.g. `acme/api` →
/// `api`, `/Users/me/work/foo` → `foo`. Leaves bare names untouched. Used by the
/// TUI where horizontal space is tight; the plain renderer keeps full labels.
pub fn repo_short(project: &Option<String>) -> String {
    match project {
        None => "(unresolved)".to_string(),
        Some(p) => {
            let trimmed = p.trim_end_matches('/');
            trimmed
                .rsplit(['/', '\\'])
                .find(|seg| !seg.is_empty())
                .unwrap_or(trimmed)
                .to_string()
        }
    }
}

/// The KIND column label for a session.
///
/// Explicit rather than the enum's `Debug`, which is what previously leaked
/// `"Manual"` onto the wire and broke every consumer comparing against
/// `"manual"`. Matches the serde wire spelling.
pub fn kind_label(k: dira_contract::SessionKind) -> &'static str {
    match k {
        dira_contract::SessionKind::Agent => "agent",
        dira_contract::SessionKind::Manual => "manual",
    }
}

/// How many terminal columns `s` occupies.
///
/// Character counts are wrong for a right-aligned column: one CJK ideograph is
/// two columns, a combining mark is zero, and either one skews an alignment
/// computed from `chars().count()`.
pub fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Truncate `s` to at most `max` display columns, appending the ellipsis glyph
/// when clipped.
///
/// It replaced a char-counting variant that also hardcoded `…`: two functions
/// with near-identical names and divergent semantics meant a call site could
/// silently pick the wrong one, and the hardcoded ellipsis leaked a non-ASCII
/// glyph into `DIRA_ASCII` output. [`truncate_middle_cols`] is the one other
/// cut, sharing its width walk ([`take_cols`]).
///
/// The result never exceeds `max` columns, including the ellipsis, and never
/// splits a wide character in half.
pub fn truncate_cols(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if display_width(s) <= max {
        return s.to_string();
    }
    let ell = crate::theme::glyphs().ellipsis;
    let budget = max.saturating_sub(display_width(ell));
    let mut out = take_cols(s.chars(), budget);
    out.push_str(ell);
    out
}

/// The leading chars of `chars` that fit in `budget` display columns, never
/// splitting a wide character.
fn take_cols(chars: impl Iterator<Item = char>, budget: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for c in chars {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > budget {
            break;
        }
        out.push(c);
        w += cw;
    }
    out
}

/// Truncate `s` to at most `max` display columns by cutting the middle, so
/// both ends survive: `acme/platform-api` and `acme/platform-web` stay
/// distinguishable where [`truncate_cols`] would clip both to `acme/platf…`.
/// Same guarantees as [`truncate_cols`]: never wider than `max`, never splits
/// a wide character.
pub fn truncate_middle_cols(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    let ell = crate::theme::glyphs().ellipsis;
    let budget = max.saturating_sub(display_width(ell));
    if budget == 0 {
        return truncate_cols(s, max);
    }
    let tail_budget = budget / 2;
    let head = take_cols(s.chars(), budget - tail_budget);
    let tail: String = take_cols(s.chars().rev(), tail_budget)
        .chars()
        .rev()
        .collect();
    format!("{head}{ell}{tail}")
}

/// A repo label for the report table: the forge host is dropped
/// (`github.com/acme/api` → `acme/api`) — it is the same on nearly every row
/// and only pushed the part that tells repos apart past the column edge — and
/// what is still too wide is cut in the middle ([`truncate_middle_cols`]).
pub fn report_repo_cell(label: &str, max: usize) -> String {
    let short = match label.split_once('/') {
        Some((host, rest)) if host.contains('.') && rest.contains('/') => rest,
        _ => label,
    };
    truncate_middle_cols(short, max)
}

/// Compact age since `started_at` for the sessions table: `45s`, `12m`,
/// `3h`, `2d`. Empty when the timestamp is missing or in the future.
pub fn age_compact(started_at: &str, now: time::OffsetDateTime) -> String {
    let Some(t) = parse_ts(Some(started_at)) else {
        return String::new();
    };
    let secs = (now - t).whole_seconds();
    match secs {
        s if s < 0 => String::new(),
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// Pad `s` on the right to `width` display columns. Use before painting — SGR
/// bytes have zero width, so padding a coloured string miscounts.
pub fn pad_cols(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        return s.to_string();
    }
    format!("{s}{}", " ".repeat(width - w))
}

/// Pad `s` on the left to `width` display columns (right-alignment).
pub fn pad_left_cols(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        return s.to_string();
    }
    format!("{}{s}", " ".repeat(width - w))
}

/// Compact token count with 3 significant digits: `982`, `45.2K`, `2.06M`,
/// `1.10B`. The renderer appends ` tok`; keeping the unit out of the helper
/// lets the TUI reuse it in tighter spans. Rounding carries across units
/// (`999_950` → `1.00M`, never `1000K`).
pub fn tokens_compact(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    let units = ["K", "M", "B", "T"];
    let mut v = n as f64;
    let mut idx = 0usize;
    v /= 1_000.0;
    while v >= 1_000.0 && idx + 1 < units.len() {
        v /= 1_000.0;
        idx += 1;
    }
    // 3 significant digits, with a carry check: 999.95 formats as "1000" at 0
    // decimals, so it must be promoted to the next unit first.
    if format_sig3(v) == "1000" && idx + 1 < units.len() {
        v /= 1_000.0;
        idx += 1;
    }
    format!("{}{}", format_sig3(v), units[idx])
}

/// `v` (in `[1, 1000)`) with 3 significant digits: `1.10`, `45.2`, `982`.
fn format_sig3(v: f64) -> String {
    if v >= 100.0 {
        format!("{v:.0}")
    } else if v >= 10.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.2}")
    }
}

/// Approximate local USD cost label for the compute row: `~$15` at a dollar or
/// more (rounded), `~$0.42` under a dollar, `~$0` for nothing. Always `~` and
/// always `$` — this is the CLI's own estimate from the bundled pricing table,
/// distinct from the cloud-priced billable amount (which carries the workspace
/// policy currency).
pub fn usd_approx(v: f64) -> String {
    if v <= 0.0 {
        "~$0".to_string()
    } else if v < 0.995 {
        format!("~${v:.2}")
    } else {
        format!("~${}", v.round() as i64)
    }
}

/// Hours label matching the cloud's `hoursLabel`: whole hours drop the decimal
/// (`14h`), otherwise one decimal (`10.4h`).
pub fn hours_compact(h: f64) -> String {
    let rounded = (h * 10.0).round() / 10.0;
    if rounded.fract() == 0.0 {
        format!("{}h", rounded as i64)
    } else {
        format!("{rounded:.1}h")
    }
}

/// Money label matching the cloud's `money()`: currency symbol + the rounded
/// amount with en-US thousands separators — `€1,064`, `$980`.
pub fn money(currency: &str, amount: f64) -> String {
    let n = amount.round() as i64;
    let sign = if n < 0 { "-" } else { "" };
    let digits = n.unsigned_abs().to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    let lead = digits.len() % 3;
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (i + 3 - lead).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    format!("{sign}{currency}{grouped}")
}

/// Human label for a billing period on the wire: `"week"` (and the tolerant
/// empty default) → `this week`; unknown periods degrade to `this <period>`.
fn period_label(period: &str) -> String {
    match period {
        "week" | "" => "this week".to_string(),
        other => format!("this {other}"),
    }
}

/// The billable footer, `10.4h billable → €1,064 unbilled, this week`, as
/// role-tagged segments (spacing included) so the plain renderer and the TUI
/// compose the identical sentence from one definition: `render.rs` maps each
/// segment through `theme::paint`, the dashboard through `Span::styled`.
pub fn billing_line(b: &BillingView) -> Vec<(String, Role)> {
    vec![
        (
            format!("{} billable", hours_compact(b.billable_hours)),
            Role::Engaged,
        ),
        (format!(" {} ", crate::theme::glyphs().arrow), Role::Muted),
        (money(&b.currency, b.unbilled_amount), Role::Ink),
        (
            format!(" unbilled, {}", period_label(&b.period)),
            Role::Muted,
        ),
    ]
}

/// Parse an RFC 3339 timestamp from the daemon; `None` if absent/unparseable.
pub fn parse_ts(s: Option<&str>) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::parse(s?, &time::format_description::well_known::Rfc3339).ok()
}

/// Render a proportional bar of `width` columns for `value` relative to `max`,
/// using filled (`█`) and empty (`░`) blocks — mirrors the daemon-status bars.
/// A zero or negative `max` (or `width`) yields an all-empty bar, never a panic.
pub fn bar(value: i64, max: i64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let filled = if max <= 0 || value <= 0 {
        0
    } else {
        let v = value.min(max) as f64;
        ((v / max as f64) * width as f64).round() as usize
    }
    .min(width);
    let empty = width - filled;
    let g = crate::theme::glyphs();
    format!("{}{}", g.bar_fill.repeat(filled), g.bar_empty.repeat(empty))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hms_buckets_by_magnitude() {
        assert_eq!(hms(0), "0s");
        assert_eq!(hms(45), "45s");
        assert_eq!(hms(60), "1m 00s");
        assert_eq!(hms(125), "2m 05s");
        assert_eq!(hms(3600), "1h 00m");
        assert_eq!(hms(5400), "1h 30m");
    }

    #[test]
    fn hms_clamps_negative() {
        assert_eq!(hms(-10), "0s");
    }

    #[test]
    fn retree_recloses_branches_after_filtering() {
        // A run whose last member (a direct-repo row) was filtered out.
        let mut g = vec!["▾ ", "├ ", "├ ", "  ", "▾ ", "  "];
        retree(&mut g);
        assert_eq!(g, vec!["▾ ", "├ ", "└ ", "  ", "  ", "  "]);
    }

    #[test]
    fn project_label_falls_back() {
        assert_eq!(project_label(&Some("acme".into())), "acme");
        assert_eq!(project_label(&None), "(unresolved)");
    }

    #[test]
    fn repo_short_takes_last_segment() {
        assert_eq!(repo_short(&Some("acme/api".into())), "api");
        assert_eq!(repo_short(&Some("/Users/me/work/foo".into())), "foo");
        assert_eq!(repo_short(&Some("foo/".into())), "foo");
        assert_eq!(repo_short(&Some("bare".into())), "bare");
        assert_eq!(repo_short(&None), "(unresolved)");
    }

    #[test]
    fn truncate_clips_with_ellipsis() {
        assert_eq!(truncate_cols("hello", 10), "hello");
        assert_eq!(truncate_cols("hello", 5), "hello");
        assert_eq!(truncate_cols("hello", 4), "hel…");
        assert_eq!(truncate_cols("hello", 1), "…");
        assert_eq!(truncate_cols("hello", 0), "");
    }

    #[test]
    fn bar_scales_to_width() {
        assert_eq!(bar(0, 100, 10), "░░░░░░░░░░");
        assert_eq!(bar(100, 100, 10), "██████████");
        assert_eq!(bar(50, 100, 10), "█████░░░░░");
        // value above max clamps to full, never overflows the width.
        assert_eq!(bar(500, 100, 4), "████");
    }

    #[test]
    fn bar_handles_zero_max_and_width() {
        assert_eq!(bar(10, 0, 5), "░░░░░");
        assert_eq!(bar(10, -1, 5), "░░░░░");
        assert_eq!(bar(10, 100, 0), "");
    }

    #[test]
    fn tokens_compact_keeps_three_significant_digits() {
        assert_eq!(tokens_compact(0), "0");
        assert_eq!(tokens_compact(982), "982");
        assert_eq!(tokens_compact(999), "999");
        assert_eq!(tokens_compact(1_000), "1.00K");
        assert_eq!(tokens_compact(45_200), "45.2K");
        assert_eq!(tokens_compact(999_400), "999K");
        assert_eq!(tokens_compact(2_060_000), "2.06M");
        assert_eq!(tokens_compact(1_100_000_000), "1.10B");
    }

    #[test]
    fn tokens_compact_carries_rounding_into_the_next_unit() {
        // 999.95K rounds to "1000" at 0 decimals — must promote, not print 1000K.
        assert_eq!(tokens_compact(999_950), "1.00M");
        assert_eq!(tokens_compact(999_950_000), "1.00B");
    }

    #[test]
    fn usd_approx_rounds_dollars_and_keeps_cents_below_one() {
        assert_eq!(usd_approx(0.0), "~$0");
        assert_eq!(usd_approx(-1.0), "~$0");
        assert_eq!(usd_approx(0.42), "~$0.42");
        assert_eq!(usd_approx(1.0), "~$1");
        assert_eq!(usd_approx(15.2), "~$15");
        assert_eq!(usd_approx(15.6), "~$16");
    }

    #[test]
    fn hours_compact_matches_cloud_hours_label() {
        assert_eq!(hours_compact(10.4), "10.4h");
        assert_eq!(hours_compact(14.0), "14h");
        assert_eq!(hours_compact(0.0), "0h");
        // One decimal, rounded — 10.44 → 10.4, 10.45+ → 10.5 territory.
        assert_eq!(hours_compact(10.44), "10.4h");
        assert_eq!(hours_compact(9.96), "10h");
    }

    #[test]
    fn money_groups_thousands_like_the_cloud() {
        assert_eq!(money("€", 1064.0), "€1,064");
        assert_eq!(money("$", 980.4), "$980");
        assert_eq!(money("€", 0.0), "€0");
        assert_eq!(money("$", 1_234_567.0), "$1,234,567");
        assert_eq!(money("£", 999.5), "£1,000");
    }

    fn sv(id: &str, extra: serde_json::Value) -> dira_core::protocol::SessionView {
        let mut v = serde_json::json!({
            "handle": id, "session_id": id, "harness": "claude_code", "kind": "agent",
            "started_at": "2026-10-27T10:00:00Z", "human_seconds": 0,
            "agent_seconds": 60, "idle": true,
        });
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(v).unwrap()
    }

    /// #181: an orchestrator that spawned nothing listed and touched no repo
    /// reads as a scratch session; one with a child or a repo stays a run.
    #[test]
    fn a_bare_orchestrator_reads_as_a_scratch_session() {
        let bare = sv("o", serde_json::json!({"orchestrator": true}));
        let list = vec![bare.clone()];
        let rows = tree_rows(&list, &Default::default());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].project, SCRATCH_LABEL);
        assert_eq!(rows[0].kind, "agent", "a scratch session is not `orch`");
        assert_eq!(flat_rows(&list)[0].project, SCRATCH_LABEL, "--flat agrees");

        let kid = sv(
            "k",
            serde_json::json!({"parent_session": "o", "project": "acme/api"}),
        );
        let list = vec![bare.clone(), kid];
        let rows = tree_rows(&list, &Default::default());
        assert_eq!(rows[0].kind, "orch");
        assert_ne!(rows[0].project, SCRATCH_LABEL);
        assert!(flat_rows(&list).iter().all(|r| r.project != SCRATCH_LABEL));

        let reached = sv(
            "r",
            serde_json::json!({"orchestrator": true, "repos": ["acme/api"]}),
        );
        let rows = tree_rows(std::slice::from_ref(&reached), &Default::default());
        assert_eq!(rows[0].kind, "orch");
        assert_eq!(rows[0].project, "acme/api");
    }

    /// `--limit` keeps whole runs: the root and every member stay or go
    /// together, in list order, and the count says how many entries went.
    #[test]
    fn limit_entries_counts_a_run_as_one() {
        let list = vec![
            sv("o", serde_json::json!({"orchestrator": true})),
            sv("k", serde_json::json!({"parent_session": "o"})),
            sv("p", serde_json::json!({})),
            sv("q", serde_json::json!({})),
        ];
        let (kept, hidden) = limit_entries(&list, 2);
        let ids: Vec<&str> = kept.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, ["o", "k", "p"]);
        assert_eq!(hidden, 1);
        assert_eq!(limit_entries(&list, 10).1, 0);
    }

    #[test]
    fn report_repo_cell_drops_the_host_and_keeps_both_ends() {
        assert_eq!(report_repo_cell("github.com/acme/api", 32), "acme/api");
        // A bare name or a two-part slug keeps its first segment.
        assert_eq!(report_repo_cell("acme/api", 32), "acme/api");
        assert_eq!(report_repo_cell("scratch", 32), "scratch");
        let a = report_repo_cell("github.com/dodi-smart/dirahq-platform-api", 20);
        let b = report_repo_cell("github.com/dodi-smart/dirahq-platform-web", 20);
        assert_ne!(a, b, "distinct repos stay distinguishable");
        assert!(display_width(&a) <= 20, "{a}");
        assert!(a.starts_with("dodi-smart") && a.ends_with("-api"), "{a}");
    }

    #[test]
    fn truncate_middle_never_exceeds_the_width() {
        for max in 0..12 {
            let t = truncate_middle_cols("abcdefghijklmnop", max);
            assert!(display_width(&t) <= max, "{max}: {t}");
        }
        assert_eq!(truncate_middle_cols("short", 10), "short");
    }

    #[test]
    fn age_compact_buckets_by_magnitude() {
        let now = time::macros::datetime!(2026-10-27 12:00 UTC);
        assert_eq!(age_compact("2026-10-27T11:59:15Z", now), "45s");
        assert_eq!(age_compact("2026-10-27T11:48:00Z", now), "12m");
        assert_eq!(age_compact("2026-10-27T09:00:00Z", now), "3h");
        assert_eq!(age_compact("2026-10-25T12:00:00Z", now), "2d");
        assert_eq!(age_compact("2026-10-27T13:00:00Z", now), "", "future");
        assert_eq!(age_compact("", now), "", "unparseable");
    }
}
