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
    /// Zavet knowledge — decisions, guards, recorded rationale. Rose `#e87ca0`:
    /// the third point of the brand triad (teal ≈166° human time, purple ≈249°
    /// agent time, rose ≈332° what that time produced), so a `zavet why` cost
    /// panel fuses all three. Deliberately NOT amber — amber means compute and
    /// sits next to zavet content in every cost line.
    Knowledge,
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
            Role::Knowledge => (0xe8, 0x7c, 0xa0),
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
            // Bright magenta: closest ANSI-16 to rose, still distinct from the
            // agent/accent purple's plain magenta.
            Role::Knowledge => Color::LightMagenta,
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
            Role::Knowledge => 95,            // bright magenta (rose fallback)
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
    #[cfg(test)]
    if let Some(forced) = FORCE_COLOR.with(std::cell::Cell::get) {
        return forced;
    }
    static C: OnceLock<bool> = OnceLock::new();
    *C.get_or_init(|| {
        use std::io::IsTerminal;
        std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
    })
}

// Test-only colour override, so a renderer can be exercised WITH SGR bytes.
//
// Under `cargo test` stdout is never a TTY, so `paint` is a no-op and every
// width assertion runs against zero escape bytes — which means a renderer that
// measures painted text passes the whole suite and then collapses in a real
// terminal. That happened once. Forcing colour on is the only way to make those
// assertions able to fail for the reason they exist.
//
// Thread-local, not global: libtest runs each test on its own thread, so a test
// that forces colour cannot disturb one asserting on plain output.
#[cfg(test)]
thread_local! {
    static FORCE_COLOR: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Force [`stdout_color`] for the current test thread; `None` restores the probe.
#[cfg(test)]
pub fn force_color(v: Option<bool>) {
    FORCE_COLOR.with(|c| c.set(v));
}

/// The non-ASCII glyphs the renderers use, and their ASCII stand-ins.
///
/// Separate from [`stdout_color`] on purpose: a terminal that cannot draw `⚠`
/// is not the same terminal as one the user piped to a file, and conflating
/// them would strip colour from a capable console or leave mojibake on an
/// incapable one. The two probes are independent.
/// Every stand-in that lands in a padded column is exactly one display cell
/// wide, because the layouts pad around them. `check` and `ellipsis` are the
/// two exceptions — both only ever appear inside width-measured segments.
pub struct Glyphs {
    /// Separator between dotted metadata parts, and `doctor`'s skip mark.
    pub dot: &'static str,
    /// Verified / current.
    pub check: &'static str,
    /// Unverified — a hypothesis nobody confirmed.
    pub open: &'static str,
    /// Needs attention.
    pub warn: &'static str,
    /// Truncation ellipsis.
    pub ellipsis: &'static str,
    /// Human engaged time, and `doctor`'s pass mark.
    pub bullet: &'static str,
    /// Agent time / decisions.
    pub diamond: &'static str,
    /// Compute — hollow on purpose: an estimate, not measured time.
    pub diamond_hollow: &'static str,
    /// Commit trailers.
    pub square: &'static str,
    /// `doctor`'s warn mark. Distinct from [`Glyphs::warn`]: `doctor` uses four
    /// distinct SHAPES so a piped report stays readable without colour.
    pub triangle: &'static str,
    /// `doctor`'s fail mark.
    pub cross: &'static str,
    /// Leads a remedy line; width-1 so the gutter stays aligned.
    pub arrow: &'static str,
    /// Pending sync, in the live dashboard.
    pub up: &'static str,
    /// Filled cell of a proportional bar.
    pub bar_fill: &'static str,
    /// Empty cell of a proportional bar.
    pub bar_empty: &'static str,
    /// The parallelism multiplier sign.
    pub times: &'static str,
    /// Em dash used as a table decoration (NOT prose punctuation).
    pub dash: &'static str,
}

const UNICODE_GLYPHS: Glyphs = Glyphs {
    dot: "·",
    check: "✓",
    open: "○",
    warn: "⚠",
    ellipsis: "…",
    bullet: "●",
    diamond: "◆",
    diamond_hollow: "◇",
    square: "▪",
    triangle: "▲",
    cross: "✕",
    arrow: "→",
    up: "⇡",
    bar_fill: "█",
    bar_empty: "░",
    times: "×",
    dash: "—",
};

const ASCII_GLYPHS: Glyphs = Glyphs {
    dot: "-",
    check: "ok",
    open: "?",
    warn: "!",
    ellipsis: "...",
    // Distinct shapes, not just distinct colours: the whole point of the
    // fallback is a console that may also be rendering without colour.
    bullet: "*",
    diamond: "+",
    diamond_hollow: "~",
    square: ":",
    triangle: "!",
    cross: "x",
    arrow: ">",
    up: "^",
    bar_fill: "#",
    bar_empty: ".",
    times: "x",
    dash: "-",
};

/// The glyph set for this terminal. ASCII when `DIRA_ASCII` is set to anything
/// other than `0`, or when a Windows console is on a non-UTF-8 code page —
/// legacy `conhost` on a CP-1251/CP-437 machine renders `·` as garbage, which
/// is exactly the setup that produced the unreadable field screenshots.
pub fn glyphs() -> &'static Glyphs {
    static G: OnceLock<bool> = OnceLock::new();
    if *G.get_or_init(ascii_glyphs) {
        &ASCII_GLYPHS
    } else {
        &UNICODE_GLYPHS
    }
}

/// The one-time probe behind [`glyphs`].
///
/// Both platform arms are tail expressions rather than early returns — an
/// arm that ends in `return` is `needless_return` on the platform where it is
/// the last statement, and that only fails on the platform CI compiles it for.
fn ascii_glyphs() -> bool {
    if std::env::var("DIRA_ASCII").is_ok_and(|v| v != "0") {
        return true;
    }
    #[cfg(windows)]
    {
        // 65001 is CP_UTF8; anything else cannot render the glyph set.
        // SAFETY: a pure getter over console state, no arguments.
        unsafe { windows_sys::Win32::System::Console::GetConsoleOutputCP() != 65001 }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Ask a Windows console to interpret ANSI escapes.
///
/// Windows Terminal does this already; legacy `conhost` does not, and without
/// it every `paint` leaks raw `\x1b[38;2;…m` into the output. Best-effort — a
/// console that refuses the mode just keeps its old behaviour, and the failure
/// is indistinguishable from the pre-existing one. No-op off Windows.
pub fn enable_ansi() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
            STD_OUTPUT_HANDLE,
        };
        // SAFETY: standard handle round-trip; every pointer is a local, and a
        // failed call is reported by the return value rather than by writing.
        unsafe {
            let h = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut mode = 0u32;
            if GetConsoleMode(h, &mut mode) != 0 {
                SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
            }
        }
    }
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
    /// Every ASCII stand-in must actually be ASCII — the whole point is a
    /// console that cannot encode anything else. A new glyph added to the
    /// Unicode set and copied verbatim into the ASCII one would otherwise ship
    /// mojibake to exactly the users the fallback exists for.
    #[test]
    fn ascii_glyphs_are_ascii() {
        let g = &super::ASCII_GLYPHS;
        for (name, v) in [
            ("dot", g.dot),
            ("check", g.check),
            ("open", g.open),
            ("warn", g.warn),
            ("ellipsis", g.ellipsis),
            ("bullet", g.bullet),
            ("diamond", g.diamond),
            ("diamond_hollow", g.diamond_hollow),
            ("square", g.square),
            ("triangle", g.triangle),
            ("cross", g.cross),
            ("arrow", g.arrow),
            ("up", g.up),
            ("bar_fill", g.bar_fill),
            ("bar_empty", g.bar_empty),
            ("times", g.times),
            ("dash", g.dash),
        ] {
            assert!(v.is_ascii(), "{name} = {v:?} is not ASCII");
            assert!(!v.is_empty(), "{name} is empty");
        }
    }

    /// Glyphs that land in a padded column must be one cell wide, or every row
    /// carrying one shifts relative to the rows that do not. `check` and
    /// `ellipsis` are exempt: both only appear inside width-measured segments.
    #[test]
    fn column_glyphs_are_one_cell_wide() {
        for set in [&super::UNICODE_GLYPHS, &super::ASCII_GLYPHS] {
            for (name, v) in [
                ("dot", set.dot),
                ("bullet", set.bullet),
                ("diamond", set.diamond),
                ("diamond_hollow", set.diamond_hollow),
                ("square", set.square),
                ("triangle", set.triangle),
                ("cross", set.cross),
                ("arrow", set.arrow),
                ("bar_fill", set.bar_fill),
                ("bar_empty", set.bar_empty),
            ] {
                assert_eq!(
                    crate::format::display_width(v),
                    1,
                    "{name} = {v:?} is not one cell wide"
                );
            }
        }
    }

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
