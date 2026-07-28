//! Integration test for T7 / plan section A6: `dirad` must treat SIGTERM the
//! same as Ctrl-C (SIGINT).
//!
//! `kill <pid>` (what `dira daemon stop` sends), `launchctl kickstart -k`, and
//! `systemctl restart` all deliver **SIGTERM**, never SIGINT. Before this fix,
//! `cli/dirad/src/lib.rs` awaited `ctrl_c()` only, so every one of those paths —
//! including every restart `dira update` performs — killed the daemon via the
//! default signal disposition: no "shutting down" log line, no offline beat, no
//! orderly flush.
//!
//! This spawns the real `dirad` binary (the lib+thin-bin split exists precisely
//! so the daemon can be stood up for integration tests — see `lib.rs`'s module
//! doc), waits for its control socket to come up, sends a **real** SIGTERM via
//! `kill -TERM <pid>`, and asserts a clean, bounded-time exit whose captured
//! stdout carries the same "shutting down" log line the Ctrl-C path emits.
//!
//! Unix-only by nature: SIGTERM and `kill(1)` don't exist on windows. The
//! windows orderly-shutdown path is `Request::Shutdown` over the control
//! channel, covered cross-platform in `daemon_stability.rs`.
#![cfg(unix)]

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use ulid::Ulid;

/// Bounded wait for the control socket file to appear, so the test never hangs
/// if `dirad` fails to start.
fn wait_for_socket(sock: &std::path::Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !sock.exists() {
        assert!(
            Instant::now() < deadline,
            "dirad did not create its control socket ({}) within {:?}",
            sock.display(),
            timeout
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn sigterm_triggers_the_same_orderly_shutdown_as_ctrl_c() {
    let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    let unique = &Ulid::new().to_string()[..12];
    let sock = std::path::Path::new(&tmp).join(format!("dsig{unique}.sock"));
    let db = std::path::Path::new(&tmp).join(format!("dsig{unique}.db"));
    let _ = std::fs::remove_file(&sock);

    let mut child = Command::new(env!("CARGO_BIN_EXE_dirad"))
        .env("DIRA_SOCKET_PATH", &sock)
        .env("DIRA_DB_PATH", &db)
        // Ephemeral port so this never collides with a real dirad or another
        // concurrently-running test on the fixed default (8722).
        .env("DIRA_HTTP_PORT", "0")
        .env("RUST_LOG", "dirad=info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn dirad binary");

    wait_for_socket(&sock, Duration::from_secs(5));

    // The signal `dira daemon stop`, `launchctl kickstart -k`, and
    // `systemctl restart` all actually send.
    let pid = child.id();
    let kill_status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("invoke kill(1)");
    assert!(kill_status.success(), "kill -TERM {pid} must succeed");

    // Bounded wait for the process to exit. Before this fix, the process was
    // killed by the default SIGTERM disposition too — which also exits
    // promptly — so the exit-status/log-line assertions below are what
    // actually distinguish "handled" from "defaulted".
    let exit_deadline = Instant::now() + Duration::from_secs(5);
    let exit_status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            Instant::now() < exit_deadline,
            "dirad did not exit within 5s of SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(20));
    };

    assert!(
        exit_status.success(),
        "orderly SIGTERM shutdown must exit 0, got {exit_status:?}"
    );

    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("child stdout")
        .read_to_string(&mut stdout)
        .expect("read child stdout");
    assert!(
        stdout.contains("shutting down"),
        "expected the same \"shutting down\" log line Ctrl-C triggers; stdout was:\n{stdout}"
    );

    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(&db);
}
