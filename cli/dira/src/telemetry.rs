//! CLI-side telemetry instrumentation: the gate, the fire-and-forget senders,
//! and the first-run notice.
//!
//! This module owns exactly one thing the daemon's `telemetry_sync` does not:
//! the decision about whether an event may be emitted **from this process**
//! at all. Everything past that gate is a best-effort, budget-capped send to
//! the daemon over the local control socket — see [`send_fire_and_forget`].
//! The daemon re-checks its own `[telemetry] enabled` knob independently
//! (`dirad`'s `telemetry_sync::ingest`), so a version skew or a stale gate
//! here can never let an event through that the daemon itself would refuse.
//!
//! See `docs/TELEMETRY.md` for the user-facing contract this implements.

use crate::client;
use dira_core::protocol::Request;
use dira_core::telemetry::event::{ConsentSource, ErrorKind, TelemetryEvent};
use dira_core::telemetry::NOTICE_MARKER_FILE;
use dira_core::Config;
use std::env;
use std::io::IsTerminal;
use std::time::Duration;

/// Total wall-clock a telemetry send may spend end to end. Short and
/// unconditional: nothing about `dira`'s own responsiveness may ever depend
/// on the daemon or the network being fast, or even reachable.
const TOTAL_BUDGET: Duration = Duration::from_millis(150);
/// How long the connect itself may retry a busy endpoint — strictly inside
/// [`TOTAL_BUDGET`], mirroring the hook shim's `HOOK_CONNECT_BUDGET`/
/// `HOOK_TOTAL_BUDGET` split in `main.rs`.
const CONNECT_BUDGET: Duration = Duration::from_millis(100);

/// Whether telemetry may be emitted **from this process**, folding together
/// every kill switch `docs/TELEMETRY.md` documents plus the two the docs
/// promise but WP1/WP2 could not yet make true: dev builds and CI.
///
/// Deliberately carries no TTY check — emission is not a display concern
/// (contrast [`maybe_show_first_run_notice`], which is).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TelemetryGate {
    /// `CI` is set to anything.
    ci: bool,
    /// `DO_NOT_TRACK` is set and truthy — the cross-tool convention.
    do_not_track: bool,
    /// `DIRA_TELEMETRY_ENABLED` is set and falsy (`"0"`).
    env_off: bool,
    /// The `telemetry.enabled` config knob is off.
    knob_off: bool,
    /// The running executable looks like a `target/{release,debug}` dev
    /// build (same predicate the update notice uses).
    dev_build: bool,
}

impl TelemetryGate {
    pub(crate) fn from_process(config: &Config) -> Self {
        TelemetryGate {
            ci: env::var_os("CI").is_some(),
            do_not_track: env::var_os("DO_NOT_TRACK")
                .is_some_and(|v| crate::update::notice::is_truthy_env_value(&v)),
            env_off: env::var_os("DIRA_TELEMETRY_ENABLED")
                .is_some_and(|v| !crate::update::notice::is_truthy_env_value(&v)),
            knob_off: !config.telemetry.enabled,
            dev_build: crate::update::notice::is_dev_build(),
        }
    }

    /// Whether an ordinary event — `CommandExecuted`, or a `ConsentRecorded`
    /// reporting the knob being turned ON — may be emitted.
    pub(crate) fn allows_emission(&self) -> bool {
        !self.hard_disabled() && !self.knob_off
    }

    /// The subset of the gate that overrides even the one event allowed
    /// through a disabled knob (see [`record_consent`]): the environment and
    /// build-shape kill switches, never the knob itself.
    fn hard_disabled(&self) -> bool {
        self.ci || self.do_not_track || self.env_off || self.dev_build
    }
}

/// Classify an `anyhow::Result<()>` into the closed [`ErrorKind`] set, or
/// `None` on success. Never inspects the message for anything beyond
/// classification, and never returns or logs it — the caller must not ship
/// message text.
pub(crate) fn classify_error(result: &anyhow::Result<()>) -> Option<ErrorKind> {
    let err = result.as_ref().err()?;

    // A transport-level `std::io::Error` anywhere in the chain: a genuine I/O
    // failure (permissions, disk, a broken pipe), not an application-level
    // rejection.
    if err
        .chain()
        .any(|c| c.downcast_ref::<std::io::Error>().is_some())
    {
        return Some(ErrorKind::IoError);
    }

    let lower = err.to_string().to_ascii_lowercase();

    if lower.contains("timed out") || lower.contains("timeout") {
        return Some(ErrorKind::Timeout);
    }

    // `client::connect_message`'s own wording for every flavor of "the
    // daemon isn't answering" — recognizing the shapes that module already
    // produces, never re-deriving them.
    const UNREACHABLE_MARKERS: &[&str] = &[
        "daemon not running",
        "could not reach dirad",
        "the daemon is busy",
        "access denied",
    ];
    if UNREACHABLE_MARKERS.iter().any(|m| lower.contains(m)) {
        return Some(ErrorKind::DaemonUnreachable);
    }

    // A `Response::Error { message }` a caller wrapped and propagated —
    // every such site's wording pairs a verb with "failed" (`link failed`,
    // `resync failed`, `key rotation failed`, ...).
    if lower.contains("failed") {
        return Some(ErrorKind::DaemonError);
    }

    // clap/validation-shaped `bail!`s: they name what the user typed, not
    // anything the daemon or transport did.
    const INVALID_INPUT_MARKERS: &[&str] = &[
        "must be",
        "unknown ",
        "is not settable",
        "required",
        "invalid",
    ];
    if INVALID_INPUT_MARKERS.iter().any(|m| lower.contains(m)) {
        return Some(ErrorKind::InvalidInput);
    }

    Some(ErrorKind::Internal)
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn clamp_millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Fire one request at the daemon and forget it: any failure — connect,
/// timeout, a `Response::Error` — is silently discarded. Mirrors the budget
/// discipline `forward_stdin`/`client::send_with_budget` use for hook shims
/// (`main.rs`), applied to telemetry instead of hook payloads.
async fn send_fire_and_forget(config: &Config, req: Request) {
    let _ = tokio::time::timeout(
        TOTAL_BUDGET,
        client::send_with_budget(&config.socket_path, &req, CONNECT_BUDGET),
    )
    .await;
}

/// Record one `cli_command_executed` event, if the gate allows it.
///
/// `result` is the outcome of the command's own `run()`, inspected only to
/// classify success/failure and, on failure, a coarse [`ErrorKind`] — never
/// to extract or forward its message. When the current directory resolves to
/// a git repo, its canonical remote crosses the local control socket as
/// `repo_canonical` so the daemon can salt-hash it; see
/// [`dira_core::protocol::Request::IngestTelemetry`]'s doc for why that never
/// happens CLI-side.
pub(crate) async fn record_command(
    config: &Config,
    name: &'static str,
    elapsed: Duration,
    result: &anyhow::Result<()>,
) {
    let gate = TelemetryGate::from_process(config);
    if !gate.allows_emission() {
        return;
    }

    let error_kind = classify_error(result);
    let repo_canonical = env::current_dir()
        .ok()
        .and_then(|cwd| dira_core::project::explain_project(&cwd).ok());

    let event = TelemetryEvent::CommandExecuted {
        command: name,
        duration_ms: clamp_millis(elapsed),
        success: result.is_ok(),
        error_kind,
        // Repo facts are filled in daemon-side from `repo_canonical` — see
        // `telemetry_sync::ingest`. The CLI never computes (or has the salt
        // to compute) the hash itself.
        repo: None,
    };
    let wire = event.into_wire(now_rfc3339(), env!("CARGO_PKG_VERSION"));
    send_fire_and_forget(
        config,
        Request::IngestTelemetry {
            event: wire,
            repo_canonical,
        },
    )
    .await;
}

/// Record one `cli_consent_recorded` event — the telemetry toggle changing,
/// including its initial default.
///
/// Deliberately NOT gated on `knob_off`: the caller passes `enabled` as the
/// value *just* decided (onboarding's prompt, `--telemetry`, or `dira config
/// set telemetry.enabled`), and by the time this runs that decision — on or
/// off — is the one worth reporting once, including the disable transition
/// itself. Every other kill switch (CI, `DO_NOT_TRACK`,
/// `DIRA_TELEMETRY_ENABLED`, a dev build) still applies — see
/// [`TelemetryGate::hard_disabled`].
pub(crate) async fn record_consent(config: &Config, enabled: bool, source: ConsentSource) {
    let gate = TelemetryGate::from_process(config);
    if gate.hard_disabled() {
        return;
    }
    let event = TelemetryEvent::ConsentRecorded { enabled, source };
    let wire = event.into_wire(now_rfc3339(), env!("CARGO_PKG_VERSION"));
    send_fire_and_forget(
        config,
        Request::IngestTelemetry {
            event: wire,
            repo_canonical: None,
        },
    )
    .await;
}

/// The XDG path of the first-run notice marker, or `None` if it cannot be
/// resolved (no XDG config dir — same "nothing to show" fallback the update
/// notice uses for its own cache path).
fn marker_path() -> Option<std::path::PathBuf> {
    dira_core::config::project_dirs().map(|d| d.config_dir().join(NOTICE_MARKER_FILE))
}

/// Pure decision for [`maybe_show_first_run_notice`]: show once a marker is
/// absent, the session looks interactive, and the gate allows emission at
/// all. Split out so the matrix is unit-testable without a real terminal or
/// filesystem.
fn should_show(marker_exists: bool, is_interactive_session: bool, gate: &TelemetryGate) -> bool {
    !marker_exists && is_interactive_session && gate.allows_emission()
}

/// Print the one-time "telemetry is on by default" disclosure to stderr, if
/// warranted, then best-effort mark it shown. Never shows in CI, a dev build,
/// a disabled knob, or a non-interactive session (piped stderr, or stdin/
/// stdout not a TTY) — and never twice on the same machine.
pub(crate) fn maybe_show_first_run_notice(config: &Config) {
    let gate = TelemetryGate::from_process(config);
    let Some(path) = marker_path() else {
        return;
    };
    let is_interactive_session =
        std::io::stderr().is_terminal() && crate::onboard::prompt::is_interactive();
    if !should_show(path.exists(), is_interactive_session, &gate) {
        return;
    }

    eprintln!("dira telemetry (first run):");
    eprintln!("{}", crate::onboard::steps::TELEMETRY_DISCLOSURE);

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, "");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow_gate() -> TelemetryGate {
        TelemetryGate::default()
    }

    // --- TelemetryGate::allows_emission --------------------------------

    #[test]
    fn allows_emission_when_nothing_disables_it() {
        assert!(allow_gate().allows_emission());
    }

    #[test]
    fn ci_disables_emission() {
        let gate = TelemetryGate {
            ci: true,
            ..allow_gate()
        };
        assert!(!gate.allows_emission());
    }

    #[test]
    fn do_not_track_disables_emission() {
        let gate = TelemetryGate {
            do_not_track: true,
            ..allow_gate()
        };
        assert!(!gate.allows_emission());
    }

    #[test]
    fn env_off_disables_emission() {
        let gate = TelemetryGate {
            env_off: true,
            ..allow_gate()
        };
        assert!(!gate.allows_emission());
    }

    #[test]
    fn knob_off_disables_emission() {
        let gate = TelemetryGate {
            knob_off: true,
            ..allow_gate()
        };
        assert!(!gate.allows_emission());
    }

    #[test]
    fn dev_build_disables_emission() {
        let gate = TelemetryGate {
            dev_build: true,
            ..allow_gate()
        };
        assert!(!gate.allows_emission());
    }

    /// The one asymmetry: `knob_off` alone does not count as "hard disabled"
    /// — it is what lets `record_consent` report a disable transition even
    /// though ordinary emission (`allows_emission`) is still off.
    #[test]
    fn knob_off_alone_is_not_hard_disabled() {
        let gate = TelemetryGate {
            knob_off: true,
            ..allow_gate()
        };
        assert!(!gate.hard_disabled());
        assert!(!gate.allows_emission());
    }

    #[test]
    fn every_hard_switch_also_disables_emission() {
        for gate in [
            TelemetryGate {
                ci: true,
                ..allow_gate()
            },
            TelemetryGate {
                do_not_track: true,
                ..allow_gate()
            },
            TelemetryGate {
                env_off: true,
                ..allow_gate()
            },
            TelemetryGate {
                dev_build: true,
                ..allow_gate()
            },
        ] {
            assert!(gate.hard_disabled());
            assert!(!gate.allows_emission());
        }
    }

    #[test]
    fn from_process_maps_the_config_knob() {
        let off = Config {
            telemetry: dira_core::config::TelemetryKnobs { enabled: false },
            ..Config::default()
        };
        assert!(TelemetryGate::from_process(&off).knob_off);
        let on = Config {
            telemetry: dira_core::config::TelemetryKnobs { enabled: true },
            ..Config::default()
        };
        assert!(!TelemetryGate::from_process(&on).knob_off);
    }

    // --- classify_error ---------------------------------------------------

    #[test]
    fn classify_error_table() {
        let ok: anyhow::Result<()> = Ok(());
        assert_eq!(classify_error(&ok), None);

        let io: anyhow::Result<()> = Err(anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "denied",
        )));
        assert_eq!(classify_error(&io), Some(ErrorKind::IoError));

        let unreachable: anyhow::Result<()> = Err(anyhow::anyhow!(
            "daemon not running — start it with `dira daemon start`"
        ));
        assert_eq!(
            classify_error(&unreachable),
            Some(ErrorKind::DaemonUnreachable)
        );

        let unreachable2: anyhow::Result<()> =
            Err(anyhow::anyhow!("could not reach dirad: connection refused"));
        assert_eq!(
            classify_error(&unreachable2),
            Some(ErrorKind::DaemonUnreachable)
        );

        let timeout: anyhow::Result<()> = Err(anyhow::anyhow!("timed out reaching dirad"));
        assert_eq!(classify_error(&timeout), Some(ErrorKind::Timeout));

        let daemon_err: anyhow::Result<()> =
            Err(anyhow::anyhow!("resync failed: some cloud message"));
        assert_eq!(classify_error(&daemon_err), Some(ErrorKind::DaemonError));

        let invalid: anyhow::Result<()> = Err(anyhow::anyhow!(
            "telemetry.enabled must be one of: on, off (got `x`)"
        ));
        assert_eq!(classify_error(&invalid), Some(ErrorKind::InvalidInput));

        let internal: anyhow::Result<()> = Err(anyhow::anyhow!("something unexpected happened"));
        assert_eq!(classify_error(&internal), Some(ErrorKind::Internal));
    }

    #[test]
    fn classify_error_never_needs_the_message_to_build_the_kind() {
        // A message containing a secret-looking token must still classify
        // fine — proving nothing about the message itself is required beyond
        // matching a marker; the caller (record_command) never stores it.
        let err: anyhow::Result<()> = Err(anyhow::anyhow!("daemon not running: token=SECRET123"));
        assert_eq!(classify_error(&err), Some(ErrorKind::DaemonUnreachable));
    }

    // --- first-run notice: should_show -------------------------------------

    #[test]
    fn should_show_only_when_unshown_interactive_and_allowed() {
        let allow = allow_gate();
        assert!(should_show(false, true, &allow));
        assert!(!should_show(true, true, &allow), "already shown");
        assert!(!should_show(false, false, &allow), "not interactive");

        let denied = TelemetryGate {
            ci: true,
            ..allow_gate()
        };
        assert!(!should_show(false, true, &denied), "gate denies emission");
    }

    #[test]
    fn should_show_never_fires_twice() {
        let allow = allow_gate();
        assert!(should_show(false, true, &allow));
        // Once the marker exists, the same inputs stop showing it.
        assert!(!should_show(true, true, &allow));
    }
}
