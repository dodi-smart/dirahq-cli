//! Thin transport to the daemon over the platform IPC channel — a Unix domain
//! socket on unix, a named pipe on windows (see `dira_ipc`). Length-prefixed
//! JSON, one request → one response, identically on both platforms.

use anyhow::{anyhow, Result};
use dira_core::protocol::{Request, Response};
use std::path::Path;
use std::time::Duration;

/// How the control endpoint answered a connect attempt.
///
/// A boolean "is it up" cannot express the case that actually broke a user's
/// machine: a daemon that is running and refusing us. Collapsing that to "down"
/// is what produced the advice (`dira daemon start`) that made the situation
/// worse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// A daemon answered.
    Up,
    /// Nothing is listening — the daemon is genuinely not running.
    Down,
    /// A daemon IS listening and refused us: a privilege or ownership mismatch.
    Denied,
    /// Listening but every instance was momentarily taken. Only ever produced on
    /// windows (a named pipe's `ERROR_PIPE_BUSY`); a unix `connect` to a live
    /// listener has no equivalent state. The variant stays unconditional so
    /// callers match the same shape on both platforms.
    #[cfg_attr(not(windows), allow(dead_code))]
    Busy,
    /// Anything else.
    Other,
}

/// Classify a connect failure.
///
/// Pure, and unit-tested on every platform via synthesised `io::Error`s — the
/// `Denied` arm is unreachable from CI, and it is the one arm we cannot afford to
/// have silently stop matching.
pub fn classify(e: &std::io::Error) -> Reach {
    use std::io::ErrorKind;
    // std maps ERROR_ACCESS_DENIED (5) to PermissionDenied, but match the raw
    // code too: this arm is the whole point of the enum, and it is not exercised
    // by any automated test on any platform we build on.
    if e.kind() == ErrorKind::PermissionDenied || e.raw_os_error() == Some(5) {
        return Reach::Denied;
    }
    #[cfg(windows)]
    if e.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_PIPE_BUSY as i32) {
        return Reach::Busy;
    }
    match e.kind() {
        ErrorKind::NotFound | ErrorKind::ConnectionRefused => Reach::Down,
        _ => Reach::Other,
    }
}

/// The user-facing message for a failed connect.
pub fn connect_message(e: &std::io::Error) -> String {
    match classify(e) {
        Reach::Denied => {
            dira_ipc::elevation::access_denied_advice(dira_ipc::elevation::is_elevated())
        }
        Reach::Down => "daemon not running — start it with `dira daemon start`".to_string(),
        Reach::Busy => format!(
            "the daemon is busy and did not accept a connection in time ({e}) — \
             retry, or check `dira daemon status`"
        ),
        // Deliberately no `dira daemon start` here: we do not know that starting
        // one would help, and for a whole class of causes it actively hurts.
        Reach::Other | Reach::Up => {
            format!("could not reach dirad: {e} — check `dira daemon status`")
        }
    }
}

/// Send one request to the daemon and await its response.
pub async fn send(socket: &Path, req: &Request) -> Result<Response> {
    send_with_budget(socket, req, dira_ipc::DEFAULT_BUSY_BUDGET).await
}

/// [`send`] with an explicit connect budget.
///
/// The hook shim needs this: it wraps its send in a short outer timeout, and a
/// connect budget longer than that timeout makes the transport's busy-retry loop
/// unreachable — on windows every busy pipe was dropped rather than retried.
pub async fn send_with_budget(
    socket: &Path,
    req: &Request,
    connect_budget: Duration,
) -> Result<Response> {
    let mut stream = dira_ipc::connect_with_budget(socket, connect_budget)
        .await
        .map_err(|e| anyhow!("{}", connect_message(&e)))?;

    let bytes = serde_json::to_vec(req)?;
    dira_ipc::write_frame(&mut stream, &bytes).await?;
    let buf = dira_ipc::read_frame(&mut stream).await?;

    match serde_json::from_slice(&buf) {
        Ok(resp) => Ok(resp),
        // A response this build cannot read is almost always version skew, not
        // corruption: `dira update` replaces both binaries, but the running
        // `dirad` keeps serving its OLD code until it is restarted. Reporting
        // the raw serde error ("missing field `repo`") plus a backtrace sends
        // the reader hunting a parsing bug instead of restarting the daemon.
        Err(e) => Err(anyhow!("{}", skew_message(socket, &e).await)),
    }
}

/// The message for a response this build could not decode.
///
/// Asks the daemon its version first — [`Request::DaemonInfo`] is deliberately
/// tolerant of skew in both directions — so the report can name both builds
/// instead of guessing. If even that fails, the advice still holds.
async fn skew_message(socket: &Path, err: &serde_json::Error) -> String {
    skew_text(
        env!("CARGO_PKG_VERSION"),
        daemon_version(socket).await.as_deref(),
        &err.to_string(),
    )
}

/// The wording, pure so it is pinned by tests rather than only reachable by
/// running two mismatched builds against each other.
///
/// Equal version strings still mean skew here: the decode already failed, so
/// the daemon is serving different code under the same number — which is
/// exactly what a dev build or an un-restarted daemon after `dira update`
/// looks like, and the most confusing case to leave unexplained.
fn skew_text(cli: &str, daemon: Option<&str>, err: &str) -> String {
    let versions = match daemon {
        Some(d) if d != cli => format!("dira is {cli}, the running dirad is {d}"),
        Some(d) => {
            format!("both report {d}, so the daemon is running an older build than it claims")
        }
        None => format!("dira is {cli}; the daemon did not report its version"),
    };
    format!(
        "the daemon sent a response this dira cannot read — {versions}.\n\
         Restart it to pick up the matching build: dira daemon restart\n\
         (decode error: {err})"
    )
}

/// The running daemon's reported version, or `None` if it cannot be asked.
async fn daemon_version(socket: &Path) -> Option<String> {
    let mut stream = dira_ipc::connect_with_budget(socket, dira_ipc::DEFAULT_BUSY_BUDGET)
        .await
        .ok()?;
    let bytes = serde_json::to_vec(&Request::DaemonInfo).ok()?;
    dira_ipc::write_frame(&mut stream, &bytes).await.ok()?;
    let buf = dira_ipc::read_frame(&mut stream).await.ok()?;
    match serde_json::from_slice(&buf).ok()? {
        Response::DaemonInfo { version, .. } => Some(version),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    /// Every skew message names a remedy, and never leaves the reader with a
    /// bare serde error to interpret.
    #[test]
    fn skew_text_always_names_the_remedy() {
        for daemon in [Some("0.3.0"), Some("0.4.0"), None] {
            let m = skew_text("0.4.0", daemon, "missing field `repo`");
            assert!(m.contains("dira daemon restart"), "{m}");
            assert!(m.contains("missing field `repo`"), "{m}");
        }
        assert!(skew_text("0.4.0", Some("0.3.0"), "e").contains("the running dirad is 0.3.0"));
        // The confusing case: same number, different build.
        assert!(skew_text("0.4.0", Some("0.4.0"), "e").contains("older build than it claims"));
        assert!(skew_text("0.4.0", None, "e").contains("did not report its version"));
    }

    /// The regression this enum exists for. `ERROR_ACCESS_DENIED` means a daemon
    /// is running and refusing us — the *opposite* of "not running".
    #[test]
    fn access_denied_is_never_classified_as_down() {
        assert_eq!(classify(&Error::from_raw_os_error(5)), Reach::Denied);
        assert_eq!(
            classify(&Error::new(ErrorKind::PermissionDenied, "denied")),
            Reach::Denied
        );
    }

    #[test]
    fn a_missing_endpoint_is_down() {
        assert_eq!(
            classify(&Error::new(ErrorKind::NotFound, "nope")),
            Reach::Down
        );
        assert_eq!(
            classify(&Error::new(ErrorKind::ConnectionRefused, "nope")),
            Reach::Down
        );
    }

    #[test]
    fn anything_else_is_other() {
        assert_eq!(
            classify(&Error::new(ErrorKind::BrokenPipe, "boom")),
            Reach::Other
        );
    }

    /// Following the old advice spawned a second daemon that died on
    /// `first_pipe_instance` *after* the CLI had overwritten the live pidfile.
    /// No denied-path message may ever suggest it again.
    #[test]
    fn the_denied_message_never_advises_a_bare_daemon_start() {
        let msg = connect_message(&Error::from_raw_os_error(5));
        assert!(!msg.contains("start it with `dira daemon start`"));
        assert!(msg.contains("access denied"));
    }

    #[test]
    fn the_down_message_still_says_how_to_start() {
        let msg = connect_message(&Error::new(ErrorKind::NotFound, "nope"));
        assert!(msg.contains("dira daemon start"));
    }

    /// A genuinely unknown error must not claim the daemon is absent, nor send
    /// the user to an action we cannot justify.
    #[test]
    fn an_unknown_error_points_at_status_not_at_start() {
        let msg = connect_message(&Error::new(ErrorKind::BrokenPipe, "boom"));
        assert!(msg.contains("dira daemon status"));
        assert!(!msg.contains("dira daemon start"));
    }
}
