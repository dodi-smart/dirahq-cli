//! The end-to-end capture probe: `dira doctor --probe`.
//!
//! Every other check reads a config file or asks the daemon how it feels. This
//! one injects a synthetic hook and checks that a row actually lands — the only
//! check that would have caught the incident behind #76, where a daemon started
//! from an elevated shell refused every ordinary-token hook while reporting
//! itself perfectly healthy.
//!
//! Two design choices carry the whole thing:
//!
//! - **Drive the command string configured in the harness's settings**, not
//!   `current_exe()`. What broke was precisely whether the command a hook config
//!   invokes reaches the daemon. Testing the configured string is what turns
//!   "hooks are wired" from a file-shape assertion into proof.
//! - **`dira doctor` spawns the child, never the daemon.** The daemon may be
//!   the elevated process; a child it forked would inherit that token and open
//!   the elevated channel happily, so the probe would pass on exactly the
//!   machine the bug is on.

use super::{Check, Level};
use dira_core::protocol::{CaptureProbeView, ProbePhase, Request, Response};
use dira_core::Config;
use serde_json::json;
use std::process::Stdio;
use std::time::Duration;

pub(crate) const ID: &str = "capture.e2e";

/// How long to wait for the spawned hook child.
///
/// `HOOK_TOTAL_BUDGET` (2s) plus process start, plus — on windows — the
/// busy-channel retry inside `HOOK_CONNECT_BUDGET` (1s).
const CHILD_DEADLINE: Duration = Duration::from_secs(5);

/// How long the daemon waits for the row after the child reports success.
/// Comfortably inside the daemon's own 30s arm TTL.
const VERIFY_WAIT_MS: u64 = 3000;

/// The furthest stage the probe reached. Ordered by how far the event got.
#[derive(Debug, Clone)]
pub(crate) enum Stage {
    /// No `dira hook claude` command is configured anywhere we look.
    NotConfigured,
    /// A command is configured but we refuse to execute it.
    Unparseable { command: String, path: String },
    /// The running daemon predates the probe request.
    DaemonTooOld { reason: String },
    /// The daemon declined to arm.
    ArmRefused { reason: String },
    /// The configured command could not be started at all.
    SpawnFailed {
        argv0: String,
        error: String,
        not_found: bool,
        path: String,
    },
    /// It started, but reported a transport failure (or was killed on timeout).
    ChildFailed {
        command: String,
        code: Option<i32>,
        stderr: String,
    },
    /// It reported success, but no row ever reached the store.
    NoRowLanded { waited_ms: u64 },
    /// Round trip complete: the row landed and was deleted.
    Landed { waited_ms: u64, deleted: u64 },
}

/// Context the verdict needs beyond the stage itself.
#[derive(Debug, Clone, Default)]
pub(crate) struct Ctx {
    pub daemon_elevated: bool,
    pub doctor_elevated: bool,
    pub control_channel_warning: Option<String>,
}

/// Split a configured hook command into argv.
///
/// Handles the shapes `dira init` writes (see `quote_if_needed`) plus hand
/// edits of the same shape: a double-quoted path with whitespace, a
/// single-quoted one, or plain whitespace separation.
///
/// Deliberately **not** a shell. `None` for anything containing a shell
/// metacharacter, which the caller reports as `Unparseable` rather than
/// guessing. Two reasons: on windows there is no portable `sh` and `cmd /C`
/// quoting differs from how the harness itself launches hooks, so a shell
/// round-trip would test *our* shell choice rather than the user's actual
/// invocation — the one thing this check exists to test. And on unix, running
/// a hand-edited command with a redirect or a pipe while feeding it JSON on
/// stdin could do something nobody intended.
pub(crate) fn split_command(cmd: &str) -> Option<Vec<String>> {
    const SHELLISH: &[char] = &['|', '&', ';', '<', '>', '$', '`', '(', ')', '\n', '*', '?'];
    // `$HOME`/`~` first: a config written as `$HOME/.local/bin/dira hook claude`
    // is a perfectly ordinary working config, and refusing to drive it would
    // make the probe useless on the machines that have one. Expanding one known
    // variable is not shell semantics — see `checks::expand_home`.
    let expanded = super::checks::expand_home(cmd);
    let cmd = expanded.trim();
    if cmd.is_empty() || cmd.contains(SHELLISH) {
        return None;
    }
    let mut argv = Vec::new();
    let mut rest = cmd;
    while !rest.is_empty() {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let (token, tail) = match rest.chars().next() {
            Some(q @ ('"' | '\'')) => {
                let body = &rest[1..];
                let end = body.find(q)?;
                (&body[..end], &body[end + 1..])
            }
            _ => match rest.find(char::is_whitespace) {
                Some(i) => (&rest[..i], &rest[i..]),
                None => (rest, ""),
            },
        };
        if !token.is_empty() {
            argv.push(token.to_string());
        }
        rest = tail;
    }
    (!argv.is_empty()).then_some(argv)
}

/// Run the probe. Never returns `Err`: every failure is a stage.
pub(crate) async fn run(config: &Config, facts: &super::Facts) -> Check {
    let mut ctx = Ctx {
        doctor_elevated: dira_ipc::elevation::is_elevated(),
        ..Default::default()
    };
    let stage = stage_of(config, facts, &mut ctx).await;
    to_check(&stage, &ctx)
}

/// How far the probe got. Split from [`run`] so there is exactly one place that
/// turns a stage into a verdict, and adding a stage is a one-line change.
async fn stage_of(config: &Config, facts: &super::Facts, ctx: &mut Ctx) -> Stage {
    // The configs `gather` already read: no second parse, and no second opinion
    // about which command counts as wired. `harness_config_paths` orders
    // project scope before global — Claude Code's own precedence — so the first
    // match is the command that would actually run.
    let Some((path, command)) = facts
        .hooks
        .iter()
        .filter(|w| w.harness == "claude")
        .flat_map(|w| w.commands.iter().map(move |c| (w.path.clone(), c)))
        .find(|(_, c)| crate::init::command_invokes_hook(c, "claude"))
    else {
        return Stage::NotConfigured;
    };

    let Some(argv) = split_command(command) else {
        return Stage::Unparseable {
            command: command.clone(),
            path,
        };
    };

    // Arm first: the daemon mints the id and registers the landing watch before
    // the child can possibly run.
    let armed = match client::send(
        &config.socket_path,
        &Request::CaptureProbe {
            phase: ProbePhase::Arm,
        },
    )
    .await
    {
        Ok(Response::CaptureProbe(v)) => *v,
        Ok(Response::Error { message }) => {
            // An older daemon fails to deserialize the unknown tag entirely.
            return if crate::client::is_daemon_too_old(&message) {
                Stage::DaemonTooOld { reason: message }
            } else {
                Stage::ArmRefused { reason: message }
            };
        }
        Ok(other) => {
            return Stage::ArmRefused {
                reason: format!("unexpected daemon response: {other:?}"),
            }
        }
        Err(e) => {
            return Stage::ArmRefused {
                reason: e.to_string(),
            }
        }
    };
    ctx.daemon_elevated = armed.daemon_elevated;
    ctx.control_channel_warning = armed.control_channel_warning.clone();
    let Some(session_id) = armed.session_id.clone() else {
        return Stage::ArmRefused {
            reason: "the daemon armed a probe without returning a session id".into(),
        };
    };

    let payload = dira_core::model::probe_hook_payload(
        &session_id,
        &std::env::temp_dir().display().to_string(),
    );
    let child = run_hook_child(&argv, &payload, CHILD_DEADLINE).await;

    // Whatever happened to the child, ALWAYS verify — that reaps the row even
    // when the child failed, so a probe can never leave one behind.
    let verified = verify(config, &session_id).await;

    match child {
        Err(e) => Stage::SpawnFailed {
            argv0: argv[0].clone(),
            not_found: e.kind() == std::io::ErrorKind::NotFound,
            error: e.to_string(),
            path,
        },
        Ok(outcome) if !outcome.delivered => Stage::ChildFailed {
            command: command.clone(),
            code: outcome.code,
            stderr: outcome.stderr,
        },
        Ok(_) => match verified {
            Some(v) if v.landed == Some(true) => Stage::Landed {
                waited_ms: v.waited_ms.unwrap_or(0),
                deleted: v.deleted,
            },
            Some(v) => Stage::NoRowLanded {
                waited_ms: v.waited_ms.unwrap_or(0),
            },
            None => Stage::NoRowLanded { waited_ms: 0 },
        },
    }
}

use crate::client;

async fn verify(config: &Config, session_id: &str) -> Option<CaptureProbeView> {
    match client::send(
        &config.socket_path,
        &Request::CaptureProbe {
            phase: ProbePhase::Verify {
                session_id: session_id.to_string(),
                wait_ms: VERIFY_WAIT_MS,
            },
        },
    )
    .await
    {
        Ok(Response::CaptureProbe(v)) => Some(*v),
        _ => None,
    }
}

#[derive(Debug)]
struct ChildOutcome {
    /// `false` when the child reported a transport failure (probe-mode exit 3)
    /// or was killed at the deadline.
    delivered: bool,
    code: Option<i32>,
    stderr: String,
}

/// Spawn the configured command, feed it the payload, and wait.
///
/// `deadline` is a parameter rather than a constant so the timeout path is
/// testable in milliseconds instead of seconds.
async fn run_hook_child(
    argv: &[String],
    payload: &serde_json::Value,
    deadline: Duration,
) -> std::io::Result<ChildOutcome> {
    use tokio::io::AsyncWriteExt;

    let mut child = tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        // Resolve to NO repo: `project::resolve` on the temp dir yields no
        // project, so even in the moments the row exists it carries none.
        .current_dir(std::env::temp_dir())
        // Makes a transport failure exit non-zero and suppresses hook-health
        // bookkeeping — see `hook_probe_mode` in main.rs.
        .env("DIRA_HOOK_PROBE", "1")
        // The shim's own diagnosis, verbatim, including the elevation advice.
        .env("DIRA_HOOK_DEBUG", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        let bytes = serde_json::to_vec(payload).unwrap_or_default();
        let _ = stdin.write_all(&bytes).await;
        let _ = stdin.shutdown().await;
    }

    match tokio::time::timeout(deadline, child.wait_with_output()).await {
        Ok(Ok(out)) => Ok(ChildOutcome {
            delivered: out.status.success(),
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        }),
        Ok(Err(e)) => Err(e),
        Err(_) => Ok(ChildOutcome {
            delivered: false,
            code: None,
            stderr: format!("the hook command did not finish within {:?}", deadline),
        }),
    }
}

/// Stage → verdict. Pure, so every arm is unit-testable on every platform —
/// including the windows access-denied case, which no CI runner can produce.
pub(crate) fn to_check(stage: &Stage, ctx: &Ctx) -> Check {
    let check = match stage {
        Stage::Landed { waited_ms, deleted } => {
            let mut c = Check::ok(
                ID,
                format!("hooks reach the daemon end to end (round trip {waited_ms} ms)"),
            );
            // Works today only because both sides happen to match. Worth saying.
            if ctx.daemon_elevated && !ctx.doctor_elevated {
                c = c.remedy(
                    "note: the daemon is running elevated. This probe passed, but a hook \
                     launched by an ordinary process may still be refused — restart the \
                     daemon from a non-elevated shell",
                );
            }
            c.detail(json!({ "waited_ms": waited_ms, "deleted": deleted }))
        }
        Stage::NotConfigured => Check::warn(
            ID,
            "no `dira hook claude` command is configured, so there is nothing to probe",
        )
        .remedy("dira init"),
        Stage::Unparseable { command, path } => Check::warn(
            ID,
            format!("the configured hook command can't be driven safely: `{command}`"),
        )
        .remedy(format!(
            "check the `command` value in {path} — it should be `<path-to-dira> hook claude`"
        )),
        Stage::DaemonTooOld { reason } => Check::skip(
            ID,
            "the running daemon is older than this CLI and can't run a capture probe",
        )
        .remedy("dira daemon restart")
        .detail(json!({ "reason": reason })),
        Stage::ArmRefused { reason } => Check::skip(
            ID,
            format!("the daemon would not start a capture probe: {reason}"),
        )
        .remedy("retry in a moment — another `dira doctor --probe` may be running"),
        Stage::SpawnFailed {
            argv0,
            error,
            not_found,
            path,
        } => Check::fail(
            ID,
            format!("the configured hook command could not be started: `{argv0}` ({error})"),
        )
        .remedy(if *not_found {
            format!("the path recorded in {path} no longer exists — re-run `dira init`")
        } else {
            format!("`{argv0}` exists but could not be executed — check its permissions")
        })
        .detail(json!({ "argv0": argv0, "error": error })),
        Stage::ChildFailed {
            command,
            code,
            stderr,
        } => {
            let first = stderr.lines().next().unwrap_or("no diagnosis reported");
            Check::fail(
                ID,
                format!("`{command}` ran but could not deliver the event: {first}"),
            )
            // The child's own stderr, passed through rather than re-derived.
            // It is the only process in the system with the right token to
            // diagnose an access-denied channel, and a second copy of that
            // reasoning here would be free to drift out of agreement with it.
            .remedy(if stderr.is_empty() {
                "check `dira daemon status`".to_string()
            } else {
                stderr.clone()
            })
            .detail(json!({ "exit_code": code, "stderr": stderr }))
        }
        Stage::NoRowLanded { waited_ms } => Check::fail(
            ID,
            format!(
                "the hook was delivered and acked, but the daemon never stored the event \
                 (waited {waited_ms} ms)"
            ),
        )
        .remedy(
            "the ingest queue may be saturated or the writer stalled — check \
             `dira daemon status` and the daemon log",
        )
        .detail(json!({ "waited_ms": waited_ms })),
    };

    // The descriptor-ladder / elevation warning is often the most diagnostic
    // line available on windows; attach it to anything that isn't clean.
    match &ctx.control_channel_warning {
        Some(w) if matches!(check.level, Level::Fail | Level::Warn) => check.note(w),
        _ => check,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write an executable shell stub. Three tests need one; the
    /// `PermissionsExt::from_mode` incantation is not worth repeating.
    #[cfg(unix)]
    fn stub(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write stub");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    fn ctx() -> Ctx {
        Ctx::default()
    }

    #[test]
    fn split_command_handles_every_shape_init_writes() {
        assert_eq!(
            split_command("\"/Users/John Doe/.local/bin/dira\" hook claude"),
            Some(vec![
                "/Users/John Doe/.local/bin/dira".to_string(),
                "hook".into(),
                "claude".into()
            ])
        );
        assert_eq!(
            split_command("/usr/bin/dira hook claude"),
            Some(vec![
                "/usr/bin/dira".to_string(),
                "hook".into(),
                "claude".into()
            ])
        );
        assert_eq!(
            split_command("\"C:/Program Files/dira/dira.exe\" hook claude"),
            Some(vec![
                "C:/Program Files/dira/dira.exe".to_string(),
                "hook".into(),
                "claude".into()
            ])
        );
        // Extra whitespace is not an error.
        assert_eq!(
            split_command("  /usr/bin/dira   hook  claude "),
            Some(vec![
                "/usr/bin/dira".to_string(),
                "hook".into(),
                "claude".into()
            ])
        );
    }

    /// We refuse to run what we would have to guess at — a wrong verdict is
    /// worse than an honest "can't check this".
    #[test]
    fn split_command_refuses_anything_shell_shaped() {
        for cmd in [
            "$XDG_BIN_HOME/dira hook claude",
            "/usr/bin/dira hook claude | tee /tmp/x",
            "/usr/bin/dira hook claude && rm -rf /",
            "/usr/bin/dira hook claude > /tmp/out",
            "/usr/bin/dira hook claude; echo hi",
            "`which dira` hook claude",
            "",
            "   ",
        ] {
            assert_eq!(split_command(cmd), None, "{cmd} should be unparseable");
        }
    }

    /// `$HOME` is expanded, not refused: it is what real configs contain.
    #[test]
    fn a_home_relative_command_is_driveable() {
        let home = dira_core::config::home_dir().expect("home");
        let argv = split_command("$HOME/.local/bin/dira hook claude").expect("driveable");
        assert_eq!(
            argv,
            vec![
                format!("{}/.local/bin/dira", home.display()),
                "hook".into(),
                "claude".into()
            ]
        );
        assert_eq!(
            split_command("~/.local/bin/dira hook claude"),
            Some(vec![
                format!("{}/.local/bin/dira", home.display()),
                "hook".into(),
                "claude".into()
            ])
        );
    }

    #[test]
    fn a_landed_probe_is_ok() {
        let c = to_check(
            &Stage::Landed {
                waited_ms: 12,
                deleted: 1,
            },
            &ctx(),
        );
        assert_eq!(c.level, Level::Ok);
        assert!(c.remedy.is_none());
    }

    /// A pass on a machine where the daemon is elevated and we are not is
    /// luck, not health — say so.
    #[test]
    fn a_pass_against_an_elevated_daemon_still_says_something() {
        let mut ctx = ctx();
        ctx.daemon_elevated = true;
        ctx.doctor_elevated = false;
        let c = to_check(
            &Stage::Landed {
                waited_ms: 5,
                deleted: 1,
            },
            &ctx,
        );
        assert_eq!(c.level, Level::Ok);
        assert!(c.remedy.expect("note").contains("elevated"));
    }

    /// **The regression guard for the incident behind #76.**
    ///
    /// The child is the only process with the right token to diagnose an
    /// access-denied channel, so its stderr is passed through verbatim. What
    /// must never happen is the probe answering a refusal with "the daemon
    /// isn't running" or a bare `dira daemon start`.
    #[test]
    fn a_refused_child_reports_its_own_diagnosis_and_never_says_not_running() {
        let advice = dira_ipc::elevation::access_denied_advice(false);
        let c = to_check(
            &Stage::ChildFailed {
                command: "/usr/bin/dira hook claude".into(),
                code: Some(3),
                stderr: format!("dira hook claude: {advice}"),
            },
            &ctx(),
        );
        assert_eq!(c.level, Level::Fail);
        let remedy = c.remedy.expect("the child's diagnosis");
        assert!(
            remedy.contains("Administrator") || remedy.contains("elevated"),
            "{remedy}"
        );
        assert_ne!(remedy.trim(), "dira daemon start");
        assert!(!c.summary.contains("not running"), "{}", c.summary);
    }

    /// The three failure modes must stay distinguishable — they have entirely
    /// different remedies, and telling them apart is the point of the stages.
    #[test]
    fn the_three_failure_stages_say_different_things() {
        let spawn = to_check(
            &Stage::SpawnFailed {
                argv0: "/old/bin/dira".into(),
                error: "No such file or directory".into(),
                not_found: true,
                path: "/home/me/.claude/settings.json".into(),
            },
            &ctx(),
        );
        assert_eq!(spawn.level, Level::Fail);
        assert!(spawn.remedy.expect("advice").contains("dira init"));

        let child = to_check(
            &Stage::ChildFailed {
                command: "/usr/bin/dira hook claude".into(),
                code: Some(3),
                stderr: "connection refused".into(),
            },
            &ctx(),
        );
        assert_eq!(child.level, Level::Fail);

        let dropped = to_check(&Stage::NoRowLanded { waited_ms: 3000 }, &ctx());
        assert_eq!(dropped.level, Level::Fail);
        // This one is specifically "the daemon accepted it and dropped it".
        assert!(dropped.summary.contains("acked"), "{}", dropped.summary);
        assert!(!dropped.summary.contains("not running"));

        // All three summaries are distinct.
        let mut all = vec![&spawn.summary, &child.summary, &dropped.summary];
        all.sort();
        all.dedup();
        assert_eq!(all.len(), 3, "the failure stages must read differently");
    }

    /// Skew and contention degrade to a skip. A doctor that reports "capture is
    /// broken" because the daemon is one version behind is a doctor nobody
    /// trusts.
    #[test]
    fn an_old_daemon_or_a_busy_one_skips_rather_than_failing() {
        assert_eq!(
            to_check(
                &Stage::DaemonTooOld {
                    reason: "bad request: unknown variant `capture_probe`".into()
                },
                &ctx()
            )
            .level,
            Level::Skip
        );
        assert_eq!(
            to_check(
                &Stage::ArmRefused {
                    reason: "a capture probe is already in flight".into()
                },
                &ctx()
            )
            .level,
            Level::Skip
        );
    }

    #[test]
    fn an_unconfigured_or_unparseable_hook_warns_rather_than_failing() {
        assert_eq!(to_check(&Stage::NotConfigured, &ctx()).level, Level::Warn);
        let c = to_check(
            &Stage::Unparseable {
                command: "$XDG_BIN_HOME/dira hook claude".into(),
                path: "/home/me/.claude/settings.json".into(),
            },
            &ctx(),
        );
        assert_eq!(c.level, Level::Warn);
        assert!(c.remedy.expect("advice").contains("settings.json"));
    }

    /// The spawn plumbing, against a shell stub rather than the real `dira`.
    ///
    /// Everything the probe promises about how it launches the configured
    /// command is asserted here: the payload reaches the child's stdin intact,
    /// it runs in the temp dir (so it resolves to no repo, and no probe row can
    /// ever carry a project), and probe mode is switched on.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_child_gets_the_payload_on_stdin_in_a_repo_less_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let record = dir.path().join("record");
        let stub = stub(
            dir.path(),
            "stub with space.sh",
            &format!(
                "#!/bin/sh\n{{ echo \"argv:$*\"; echo \"cwd:$PWD\"; \
                 echo \"probe:$DIRA_HOOK_PROBE\"; echo \"debug:$DIRA_HOOK_DEBUG\"; \
                 echo \"stdin:$(cat)\"; }} > '{}'\nexit 0\n",
                record.display()
            ),
        );

        // Quoted, because the path has a space — the shape `dira init` writes.
        let argv = split_command(&format!("\"{}\" hook claude", stub.display())).expect("argv");
        let payload = dira_core::model::probe_hook_payload("dira-probe-01TEST", "/tmp");
        let out = run_hook_child(&argv, &payload, Duration::from_secs(10))
            .await
            .expect("spawn");
        assert!(out.delivered, "stderr: {}", out.stderr);

        let recorded = std::fs::read_to_string(&record).expect("record");
        assert!(recorded.contains("argv:hook claude"), "{recorded}");
        assert!(recorded.contains("probe:1"), "{recorded}");
        assert!(recorded.contains("debug:1"), "{recorded}");
        assert!(recorded.contains("dira-probe-01TEST"), "{recorded}");
        assert!(recorded.contains("UserPromptSubmit"), "{recorded}");
        // `canonicalize` because macOS temp dirs are symlinked via /private.
        let temp = std::fs::canonicalize(std::env::temp_dir()).expect("temp");
        let cwd_line = recorded
            .lines()
            .find_map(|l| l.strip_prefix("cwd:"))
            .expect("cwd recorded");
        assert_eq!(
            std::fs::canonicalize(cwd_line).expect("cwd"),
            temp,
            "the child must run where it resolves to no repo"
        );
    }

    /// Probe mode's exit 3 is what separates "never reached the daemon" from
    /// "the daemon acked and dropped it" — the two have different remedies.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_probe_mode_transport_failure_is_visible_through_the_exit_code() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = dir.path().join("stub.sh");
        std::fs::write(
            &stub,
            "#!/bin/sh\ncat > /dev/null\necho refused >&2\nexit 3\n",
        )
        .expect("write");
        std::fs::set_permissions(
            &stub,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .expect("chmod");

        let argv = vec![stub.display().to_string(), "hook".into(), "claude".into()];
        let out = run_hook_child(&argv, &serde_json::json!({}), Duration::from_secs(10))
            .await
            .expect("spawn");
        assert!(!out.delivered);
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stderr, "refused");
    }

    /// A hook that never returns must not hang the doctor.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_hanging_child_is_killed_at_the_deadline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = stub(dir.path(), "hang.sh", "#!/bin/sh\nsleep 30\n");

        let argv = vec![stub.display().to_string()];
        let started = std::time::Instant::now();
        let out = run_hook_child(&argv, &serde_json::json!({}), Duration::from_millis(200))
            .await
            .expect("spawn");
        assert!(!out.delivered);
        assert!(out.code.is_none());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the deadline did not fire"
        );
    }

    #[tokio::test]
    async fn spawning_a_missing_binary_surfaces_not_found() {
        let argv = vec!["/nope/does/not/exist/dira".to_string(), "hook".into()];
        let err = run_hook_child(&argv, &serde_json::json!({}), Duration::from_secs(1))
            .await
            .expect_err("a missing binary must not look like a delivery");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn the_control_channel_warning_rides_along_on_a_bad_verdict() {
        let mut ctx = ctx();
        ctx.control_channel_warning = Some("pipe fell back to a DACL-only descriptor".into());
        let bad = to_check(&Stage::NoRowLanded { waited_ms: 10 }, &ctx);
        assert!(bad.summary.contains("DACL-only"), "{}", bad.summary);
        assert!(bad.detail.get("control_channel_warning").is_some());

        // ...but a clean pass is not cluttered with it.
        let good = to_check(
            &Stage::Landed {
                waited_ms: 1,
                deleted: 1,
            },
            &ctx,
        );
        assert!(!good.summary.contains("DACL-only"));
    }
}
