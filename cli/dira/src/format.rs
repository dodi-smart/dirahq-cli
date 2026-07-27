//! Pure formatting helpers shared by the plain-text renderer (`render.rs`) and
//! the live TUI dashboard (`tui`). Keeping them here means both surfaces format
//! durations, projects and bars identically — the TUI is just another view over
//! the same numbers.

use crate::theme::Role;
use dira_core::protocol::BillingView;

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

/// Truncate `s` to at most `max` display columns, appending `…` when clipped.
/// `max == 0` yields an empty string; `max == 1` yields just the ellipsis when
/// the input is longer. Operates on `char`s (good enough for ASCII-ish labels).
pub fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let head: String = s.chars().take(keep).collect();
    format!("{head}…")
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
        (" → ".to_string(), Role::Muted),
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
    format!("{}{}", "█".repeat(filled), "░".repeat(empty))
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
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello", 4), "hel…");
        assert_eq!(truncate("hello", 1), "…");
        assert_eq!(truncate("hello", 0), "");
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
}
