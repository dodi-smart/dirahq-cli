//! The telemetry event model — the closed interface other crates code against.
//!
//! `dira`/`dirad` construct a [`TelemetryEvent`], never a [`super::wire::TelemetryEventWire`]
//! directly, so the set of things that can ever be reported is fixed here and
//! reviewable in one place. There is deliberately no `DeviceLinkAlias` event:
//! the cloud performs that alias at device-claim time, from data it already has.

use super::repo_facts::RepoFacts;
use super::wire::TelemetryEventWire;

/// The closed set of events dira may report.
#[derive(Debug, Clone)]
pub enum TelemetryEvent {
    /// A CLI (or daemon-served) command finished running.
    CommandExecuted {
        /// The command name, e.g. `"status"`, `"config"`. Always the
        /// top-level command name — never a sub-action (`config set` reports
        /// as `"config"`) — and never raw argv: no path, no flag value, no
        /// user text.
        command: &'static str,
        duration_ms: u64,
        success: bool,
        error_kind: Option<ErrorKind>,
        repo: Option<RepoFacts>,
    },
    /// `dirad` finished starting up.
    DaemonStarted,
    /// `dirad` is shutting down after `uptime_secs` of wall-clock life.
    DaemonStopped { uptime_secs: u64 },
    /// The telemetry consent knob changed (including its initial default).
    ConsentRecorded {
        enabled: bool,
        source: ConsentSource,
    },
}

/// A coarse failure classification for [`TelemetryEvent::CommandExecuted`].
/// Never the error's message or any value it carried — just which of a fixed
/// set of failure shapes occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    DaemonUnreachable,
    DaemonError,
    InvalidInput,
    IoError,
    Timeout,
    Internal,
}

impl ErrorKind {
    /// The lowercase snake_case wire spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorKind::DaemonUnreachable => "daemon_unreachable",
            ErrorKind::DaemonError => "daemon_error",
            ErrorKind::InvalidInput => "invalid_input",
            ErrorKind::IoError => "io_error",
            ErrorKind::Timeout => "timeout",
            ErrorKind::Internal => "internal",
        }
    }
}

/// How a [`TelemetryEvent::ConsentRecorded`] came about.
///
/// Deliberately closed to the ways the knob can actually change: there is no
/// `Env` variant because the environment kill switches (`DO_NOT_TRACK`,
/// `DIRA_TELEMETRY_ENABLED=0`) trip `TelemetryGate::hard_disabled` before
/// `record_consent` ever runs, so a production caller can never construct
/// this from an env override — and no `Default` variant, because the knob's
/// initial default is never itself an event (see `docs/TELEMETRY.md`:
/// accepting the default writes nothing and reports nothing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentSource {
    /// The interactive onboarding prompt.
    Prompt,
    /// A non-interactive `--yes`-style flag.
    YesFlag,
    /// `dira config set telemetry.enabled ...`.
    ConfigSet,
}

impl ConsentSource {
    /// The lowercase snake_case wire spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            ConsentSource::Prompt => "prompt",
            ConsentSource::YesFlag => "yes_flag",
            ConsentSource::ConfigSet => "config_set",
        }
    }
}

impl TelemetryEvent {
    /// The wire event name, e.g. `"cli_command_executed"`.
    pub fn name(&self) -> &'static str {
        match self {
            TelemetryEvent::CommandExecuted { .. } => "cli_command_executed",
            TelemetryEvent::DaemonStarted => "cli_daemon_started",
            TelemetryEvent::DaemonStopped { .. } => "cli_daemon_stopped",
            TelemetryEvent::ConsentRecorded { .. } => "cli_consent_recorded",
        }
    }

    /// Flatten into the wire shape, stamping `timestamp_rfc3339` and the
    /// running `dira_version` that every event carries regardless of variant.
    pub fn into_wire(self, timestamp_rfc3339: String, dira_version: &str) -> TelemetryEventWire {
        let mut wire = TelemetryEventWire::base(self.name(), timestamp_rfc3339, dira_version);
        match self {
            TelemetryEvent::CommandExecuted {
                command,
                duration_ms,
                success,
                error_kind,
                repo,
            } => {
                wire.command = Some(command.to_string());
                wire.duration_ms = Some(duration_ms);
                wire.success = Some(success);
                wire.error_kind = error_kind.map(|k| k.as_str().to_string());
                if let Some(facts) = repo {
                    wire.repo_host_class = Some(facts.host_class.as_str().to_string());
                    wire.repo_visibility = Some(facts.visibility.as_str().to_string());
                    wire.repo_hash = Some(facts.repo_hash);
                }
            }
            TelemetryEvent::DaemonStarted => {}
            TelemetryEvent::DaemonStopped { uptime_secs } => {
                wire.duration_ms = Some(uptime_secs.saturating_mul(1000));
            }
            TelemetryEvent::ConsentRecorded { enabled, source } => {
                wire.telemetry_enabled = Some(enabled);
                wire.consent_source = Some(source.as_str().to_string());
            }
        }
        wire
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::repo_facts::{RepoHostClass, RepoVisibility};
    use std::collections::BTreeSet;

    const TS: &str = "2026-01-01T00:00:00Z";
    const VERSION: &str = "0.5.1";

    fn keys(wire: &TelemetryEventWire) -> BTreeSet<String> {
        let json = serde_json::to_value(wire).unwrap();
        json.as_object().unwrap().keys().cloned().collect()
    }

    const BASE_KEYS: &[&str] = &["event", "timestamp", "diraVersion", "os", "arch"];

    fn expected(extra: &[&str]) -> BTreeSet<String> {
        BASE_KEYS
            .iter()
            .chain(extra)
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn command_executed_without_repo_carries_no_repo_keys() {
        let ev = TelemetryEvent::CommandExecuted {
            command: "status",
            duration_ms: 42,
            success: true,
            error_kind: None,
            repo: None,
        };
        let wire = ev.into_wire(TS.into(), VERSION);
        assert_eq!(keys(&wire), expected(&["command", "durationMs", "success"]));
    }

    #[test]
    fn command_executed_with_error_and_repo_carries_exactly_those_keys() {
        let ev = TelemetryEvent::CommandExecuted {
            command: "sync",
            duration_ms: 7,
            success: false,
            error_kind: Some(ErrorKind::DaemonUnreachable),
            repo: Some(RepoFacts {
                host_class: RepoHostClass::GitHub,
                visibility: RepoVisibility::Unknown,
                repo_hash: "deadbeef".into(),
            }),
        };
        let wire = ev.into_wire(TS.into(), VERSION);
        assert_eq!(
            keys(&wire),
            expected(&[
                "command",
                "durationMs",
                "success",
                "errorKind",
                "repoHostClass",
                "repoVisibility",
                "repoHash",
            ])
        );
        assert_eq!(wire.error_kind.as_deref(), Some("daemon_unreachable"));
        assert_eq!(wire.repo_host_class.as_deref(), Some("github"));
        assert_eq!(wire.repo_visibility.as_deref(), Some("unknown"));
        assert_eq!(wire.repo_hash.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn daemon_started_carries_only_base_keys() {
        let wire = TelemetryEvent::DaemonStarted.into_wire(TS.into(), VERSION);
        assert_eq!(keys(&wire), expected(&[]));
        assert_eq!(wire.event, "cli_daemon_started");
    }

    #[test]
    fn daemon_stopped_carries_duration_ms_as_uptime() {
        let wire = TelemetryEvent::DaemonStopped { uptime_secs: 90 }.into_wire(TS.into(), VERSION);
        assert_eq!(keys(&wire), expected(&["durationMs"]));
        assert_eq!(wire.duration_ms, Some(90_000));
    }

    #[test]
    fn consent_recorded_carries_enabled_and_source() {
        let wire = TelemetryEvent::ConsentRecorded {
            enabled: false,
            source: ConsentSource::YesFlag,
        }
        .into_wire(TS.into(), VERSION);
        assert_eq!(
            keys(&wire),
            expected(&["telemetryEnabled", "consentSource"])
        );
        assert_eq!(wire.telemetry_enabled, Some(false));
        assert_eq!(wire.consent_source.as_deref(), Some("yes_flag"));
    }

    #[test]
    fn event_names_match_the_wire_taxonomy() {
        assert_eq!(
            TelemetryEvent::CommandExecuted {
                command: "x",
                duration_ms: 0,
                success: true,
                error_kind: None,
                repo: None,
            }
            .name(),
            "cli_command_executed"
        );
        assert_eq!(TelemetryEvent::DaemonStarted.name(), "cli_daemon_started");
        assert_eq!(
            TelemetryEvent::DaemonStopped { uptime_secs: 0 }.name(),
            "cli_daemon_stopped"
        );
        assert_eq!(
            TelemetryEvent::ConsentRecorded {
                enabled: true,
                source: ConsentSource::Prompt
            }
            .name(),
            "cli_consent_recorded"
        );
    }

    #[test]
    fn error_kind_as_str_is_lowercase_snake() {
        let cases = [
            (ErrorKind::DaemonUnreachable, "daemon_unreachable"),
            (ErrorKind::DaemonError, "daemon_error"),
            (ErrorKind::InvalidInput, "invalid_input"),
            (ErrorKind::IoError, "io_error"),
            (ErrorKind::Timeout, "timeout"),
            (ErrorKind::Internal, "internal"),
        ];
        for (kind, want) in cases {
            assert_eq!(kind.as_str(), want);
        }
    }

    #[test]
    fn consent_source_as_str_is_lowercase_snake() {
        let cases = [
            (ConsentSource::Prompt, "prompt"),
            (ConsentSource::YesFlag, "yes_flag"),
            (ConsentSource::ConfigSet, "config_set"),
        ];
        for (source, want) in cases {
            assert_eq!(source.as_str(), want);
        }
    }
}
