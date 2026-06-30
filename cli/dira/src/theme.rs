//! Centralised colour palette for the CLI, mirroring the Dira cloud + landing
//! design system so the terminal reads as the same product. Colours are authored
//! once here as semantic [`Role`]s and consumed by both surfaces:
//!
//! - the live `dira watch` dashboard (ratatui [`Color`]/[`Style`]), and
//! - the plain `dira status` renderer (raw SGR escapes via [`paint`]).
//!
//! Each role maps to the exact brand hex on truecolor terminals and degrades to
//! the closest ANSI-16 colour elsewhere — so we get the real `#9079ff` purple
//! where the terminal can show it, and the previous cyan/magenta/green look as a
//! faithful fallback where it can't.

use ratatui::style::{Color, Style};
use std::sync::OnceLock;

/// A semantic colour role. The name says what the colour *means*, not what it
/// looks like, so call sites stay legible and the whole palette can be retuned
/// in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Human, supervised time — the billable base. Teal `#1fd6ae`.
    Engaged,
    /// Agent wall-clock time, runs in parallel. Brand purple `#9079ff`.
    Agent,
    /// Brand accent for titles & emphasis. Purple `#9079ff`.
    Accent,
    /// Token compute / estimated cost. Amber `#e5a53b`.
    Compute,
    /// Device-signed attribution. Blue `#7fa6e0`. Reserved for the assurance
    /// tiers (anchored/attributed/unverified) once the CLI surfaces them, kept
    /// here so the palette stays the single source of truth alongside the cloud.
    #[allow(dead_code)]
    Attributed,
    /// Primary text. `#eceaf2`.
    Ink,
    /// Secondary / label text — column heads, section eyebrows. `#8b8799`.
    Muted,
    /// Idle rows, dividers, help text, empty states. `#5c586b`.
    Faint,
    /// Errors / daemon down. `#c76b6b`.
    Negative,
}

impl Role {
    /// Exact brand RGB, used on truecolor terminals.
    const fn rgb(self) -> (u8, u8, u8) {
        match self {
            Role::Engaged => (0x1f, 0xd6, 0xae),
            Role::Agent | Role::Accent => (0x90, 0x79, 0xff),
            Role::Compute => (0xe5, 0xa5, 0x3b),
            Role::Attributed => (0x7f, 0xa6, 0xe0),
            Role::Ink => (0xec, 0xea, 0xf2),
            Role::Muted => (0x8b, 0x87, 0x99),
            Role::Faint => (0x5c, 0x58, 0x6b),
            Role::Negative => (0xc7, 0x6b, 0x6b),
        }
    }

    /// Closest ANSI-16 fallback for non-truecolor terminals. This deliberately
    /// preserves the pre-rebrand look where it was already close (human≈cyan,
    /// agent≈magenta) so degraded terminals don't regress.
    const fn ansi16(self) -> Color {
        match self {
            Role::Engaged => Color::Cyan,
            Role::Agent | Role::Accent => Color::Magenta,
            Role::Compute => Color::Yellow,
            Role::Attributed => Color::Blue,
            Role::Ink => Color::White,
            Role::Muted => Color::Gray,
            Role::Faint => Color::DarkGray,
            Role::Negative => Color::Red,
        }
    }

    /// SGR foreground code for the ANSI-16 fallback in the plain renderer.
    const fn sgr16(self) -> u8 {
        match self {
            Role::Engaged => 36,              // cyan
            Role::Agent | Role::Accent => 35, // magenta
            Role::Compute => 33,              // yellow
            Role::Attributed => 34,           // blue
            Role::Ink => 97,                  // bright white
            Role::Muted => 37,                // white
            Role::Faint => 90,                // bright black / gray
            Role::Negative => 31,             // red
        }
    }
}

/// Whether the terminal advertises 24-bit colour (`COLORTERM=truecolor|24bit`).
/// Probed once; the environment can't change underneath a single process.
fn truecolor() -> bool {
    static TC: OnceLock<bool> = OnceLock::new();
    *TC.get_or_init(|| {
        std::env::var("COLORTERM")
            .map(|v| v.contains("truecolor") || v.contains("24bit"))
            .unwrap_or(false)
    })
}

/// The ratatui [`Color`] for `role` — truecolor brand hex when supported, else
/// the ANSI-16 fallback. Use this in the `dira watch` dashboard.
pub fn color(role: Role) -> Color {
    if truecolor() {
        let (r, g, b) = role.rgb();
        Color::Rgb(r, g, b)
    } else {
        role.ansi16()
    }
}

/// A ratatui foreground [`Style`] for `role`. Shorthand for the common case.
pub fn style(role: Role) -> Style {
    Style::default().fg(color(role))
}

/// Whether stdout should carry colour: a real TTY with `NO_COLOR` unset. Probed
/// once. When false, [`paint`] is a no-op, so piped/redirected `dira status`
/// output stays byte-for-byte identical to the uncoloured layout.
pub fn stdout_color() -> bool {
    static C: OnceLock<bool> = OnceLock::new();
    *C.get_or_init(|| {
        use std::io::IsTerminal;
        std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
    })
}

/// Wrap `text` in an SGR colour for `role`, for the plain `dira status`
/// renderer. A no-op (returns `text` unchanged) when stdout isn't colour-capable.
///
/// Apply any width/padding to `text` *before* painting: SGR bytes have zero
/// display width, so colouring a pre-padded string keeps column alignment intact.
pub fn paint(text: &str, role: Role) -> String {
    paint_with(text, role, stdout_color(), truecolor())
}

/// The pure core of [`paint`], with the two environment probes passed in so the
/// exact emitted bytes are unit-testable. `enabled == false` returns `text`
/// verbatim; otherwise wraps it in a truecolor or ANSI-16 SGR pair.
fn paint_with(text: &str, role: Role, enabled: bool, truecolor: bool) -> String {
    if !enabled {
        return text.to_string();
    }
    let prefix = if truecolor {
        let (r, g, b) = role.rgb();
        format!("\x1b[38;2;{r};{g};{b}m")
    } else {
        format!("\x1b[{}m", role.sgr16())
    };
    format!("{prefix}{text}\x1b[0m")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brand_roles_carry_the_exact_hex() {
        // The three headline metrics + brand accent are the load-bearing colours;
        // pin them so a careless edit can't silently drift the identity.
        assert_eq!(Role::Engaged.rgb(), (0x1f, 0xd6, 0xae));
        assert_eq!(Role::Agent.rgb(), (0x90, 0x79, 0xff));
        assert_eq!(Role::Accent.rgb(), (0x90, 0x79, 0xff));
        assert_eq!(Role::Compute.rgb(), (0xe5, 0xa5, 0x3b));
    }

    #[test]
    fn paint_is_a_no_op_without_colour() {
        // In the test harness stdout isn't a TTY, so `paint` must pass text
        // through untouched — this is what keeps piped output stable.
        assert!(!stdout_color());
        assert_eq!(paint("engaged", Role::Engaged), "engaged");
    }

    #[test]
    fn paint_emits_truecolor_brand_bytes() {
        // The agent glyph on a truecolor terminal must carry the exact #9079ff
        // (144,121,255) foreground and reset — this is the literal byte stream a
        // user's terminal renders.
        assert_eq!(
            paint_with("◆", Role::Agent, true, true),
            "\x1b[38;2;144;121;255m◆\x1b[0m"
        );
        // Engaged teal #1fd6ae = (31,214,174).
        assert_eq!(
            paint_with("●", Role::Engaged, true, true),
            "\x1b[38;2;31;214;174m●\x1b[0m"
        );
    }

    #[test]
    fn paint_falls_back_to_sgr16_without_truecolor() {
        // No truecolor → 16-colour SGR (magenta=35 for agent), preserving the
        // legacy look on basic terminals.
        assert_eq!(
            paint_with("◆", Role::Agent, true, false),
            "\x1b[35m◆\x1b[0m"
        );
    }

    #[test]
    fn fallback_preserves_legacy_ansi() {
        // Non-truecolor terminals keep the original cyan/magenta/red mapping.
        assert_eq!(Role::Engaged.ansi16(), Color::Cyan);
        assert_eq!(Role::Agent.ansi16(), Color::Magenta);
        assert_eq!(Role::Negative.ansi16(), Color::Red);
    }
}
