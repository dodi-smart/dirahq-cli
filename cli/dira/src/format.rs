//! Pure formatting helpers shared by the plain-text renderer (`render.rs`) and
//! the live TUI dashboard (`tui`). Keeping them here means both surfaces format
//! durations, projects and bars identically — the TUI is just another view over
//! the same numbers.

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
}
