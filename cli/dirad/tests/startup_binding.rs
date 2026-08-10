//! Startup binding: the control socket must never be stolen from a live
//! daemon, and an unavailable HTTP port must not cost us the control socket.
//!
//! Both are regressions from a real incident: a second `dirad` could not bind
//! port 8722, exited before ever binding the control socket, and launchd's
//! `KeepAlive` respawned it every 10s. Every client saw "daemon down" even
//! though a healthy daemon was running the whole time.

use dira_core::protocol::{Request, Response};
use dira_core::{Config, Store};
use dirad::state::AppState;
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
use ulid::Ulid;

async fn test_state() -> AppState {
    let store = Store::open_in_memory().await.expect("in-memory store");
    let (state, _rx, _sync_rx, _knowledge_rx) = dirad::build_state(store, Config::default())
        .await
        .expect("build_state");
    state
}

/// Poll `state.http_ingress_error` until it matches `want_some`, or give up.
async fn wait_for_degraded(state: &AppState, want_some: bool) -> Option<String> {
    for _ in 0..100 {
        let cur = state.http_ingress_error.lock().unwrap().clone();
        if cur.is_some() == want_some {
            return cur;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    state.http_ingress_error.lock().unwrap().clone()
}

/// A short, unique control endpoint.
///
/// unix: a socket path under `$TMPDIR` — short because the whole path has to
/// stay under the ~104-byte `sun_path` limit. windows: a uniquely-named pipe
/// (nothing on disk; the filesystem failure modes the unix tests below cover
/// don't exist there).
///
/// Slices the ULID's *random* half (chars 16..26). The leading 10 characters
/// are its millisecond timestamp, so two of these built in the same tick would
/// collide — which is exactly what happened when these tests first ran in
/// parallel.
fn tmp_sock() -> PathBuf {
    let uniq = &Ulid::generate().to_string()[16..26];
    #[cfg(unix)]
    {
        let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(tmp).join(format!("d{uniq}.sock"))
    }
    #[cfg(windows)]
    {
        PathBuf::from(format!(r"\\.\pipe\dirad-sb-{uniq}"))
    }
}

/// The single-instance guard. Before this existed, `run()` unconditionally
/// unlinked the socket path and rebound it — so a second daemon would take the
/// path away from a healthy first one, leaving that first daemon alive but
/// unreachable by any client.
#[cfg(unix)]
#[tokio::test]
async fn refuses_to_steal_a_live_daemons_socket() {
    let sock = tmp_sock();
    let _live = UnixListener::bind(&sock).expect("first daemon binds");

    let err = dirad::bind_control_socket(&sock)
        .await
        .expect_err("must refuse while another daemon holds the socket");

    assert!(
        err.to_string().contains("already running"),
        "error must say another daemon is running, got: {err}"
    );
    assert!(
        sock.exists(),
        "the live daemon's socket file must survive the refusal"
    );
    assert!(
        UnixStream::connect(&sock).await.is_ok(),
        "the first daemon must still be reachable"
    );

    let _ = std::fs::remove_file(&sock);
}

/// The flip side: a socket file left behind by a daemon that died (dropping a
/// `UnixListener` does not unlink its path) must be reclaimed, not treated as
/// a live peer — otherwise a crashed daemon would wedge every later start.
#[cfg(unix)]
#[tokio::test]
async fn reclaims_a_stale_socket_file() {
    let sock = tmp_sock();
    {
        let dead = UnixListener::bind(&sock).expect("bind");
        drop(dead);
    }
    assert!(
        sock.exists(),
        "precondition: a stale socket file is present"
    );

    let _uds = dirad::bind_control_socket(&sock)
        .await
        .expect("a stale socket must be reclaimed");
    assert!(
        UnixStream::connect(&sock).await.is_ok(),
        "the reclaimed socket must accept connections"
    );

    let _ = std::fs::remove_file(&sock);
}

/// Two daemons racing to reclaim the same stale socket file: at most one may
/// win, and the winner must stay reachable. The probe→unlink→bind sequence is
/// not atomic on its own — both racers can observe "nothing answers" before
/// either unlinks, and then the loser's unlink takes the path away from the
/// winner's freshly bound listener. That is the same orphaned-listener failure
/// D-0009 exists to prevent, just triggered by a stale-file reclaim instead of
/// a port conflict.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_binders_never_both_win_a_stale_socket() {
    for round in 0..1000 {
        let sock = tmp_sock();
        {
            let dead = UnixListener::bind(&sock).expect("bind");
            drop(dead);
        }

        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(4));
        let racers: Vec<_> = (0..4)
            .map(|_| {
                let sock = sock.clone();
                let barrier = barrier.clone();
                tokio::spawn(async move {
                    barrier.wait().await;
                    dirad::bind_control_socket(&sock).await
                })
            })
            .collect();

        let mut results = Vec::new();
        for racer in racers {
            results.push(racer.await.expect("racer must not panic"));
        }

        let wins = results.iter().filter(|r| r.is_ok()).count();
        assert!(
            wins <= 1,
            "round {round}: both daemons claimed {}",
            sock.display()
        );
        if wins == 1 {
            assert!(
                UnixStream::connect(&sock).await.is_ok(),
                "round {round}: the winner must still be reachable at {}",
                sock.display()
            );
        }

        drop(results);
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(sock.with_extension("lock"));
    }
}

/// First run on a fresh machine: the data directory holding the socket may not
/// exist yet.
#[cfg(unix)]
#[tokio::test]
async fn creates_the_socket_parent_directory() {
    let dir = tmp_sock().with_extension("d");
    let sock = dir.join("dira.sock");
    assert!(!dir.exists(), "precondition: parent dir absent");

    let _uds = dirad::bind_control_socket(&sock)
        .await
        .expect("must create the parent directory");
    assert!(UnixStream::connect(&sock).await.is_ok());

    let _ = std::fs::remove_dir_all(&dir);
}

// -- hook ingress: a taken port must not cost us the daemon -----------------

/// The incident itself: the HTTP port was already held, `run()` returned `Err`
/// before the control socket was ever bound, and launchd respawned the daemon
/// every 10s forever. Binding the port must instead be survivable.
#[tokio::test]
async fn a_taken_port_leaves_the_daemon_up_but_degraded() {
    let state = test_state().await;
    let squatter = TcpListener::bind("127.0.0.1:0").await.expect("squatter");
    let addr = squatter.local_addr().unwrap().to_string();

    dirad::serve_http_ingress(state.clone(), addr.clone(), Duration::from_millis(20)).await;

    let reason = wait_for_degraded(&state, true)
        .await
        .expect("a port conflict must be recorded, not swallowed");
    assert!(
        reason.contains(&squatter.local_addr().unwrap().port().to_string()),
        "the reason must name the port so it is actionable, got: {reason}"
    );
}

/// A daemon that gave up on the port forever would need a manual restart to
/// come back — the retry is what removes the launchd respawn loop entirely.
#[tokio::test]
async fn the_ingress_recovers_once_the_port_frees_up() {
    let state = test_state().await;
    let squatter = TcpListener::bind("127.0.0.1:0").await.expect("squatter");
    let addr = squatter.local_addr().unwrap().to_string();

    dirad::serve_http_ingress(state.clone(), addr.clone(), Duration::from_millis(20)).await;
    assert!(
        wait_for_degraded(&state, true).await.is_some(),
        "precondition: degraded while the port is held"
    );

    drop(squatter);

    assert!(
        wait_for_degraded(&state, false).await.is_none(),
        "the retry must reclaim the port and clear the degradation"
    );
}

/// The healthy path must leave no degradation behind.
#[tokio::test]
async fn a_free_port_is_not_degraded() {
    let state = test_state().await;
    let free = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = free.local_addr().unwrap().to_string();
    drop(free);

    dirad::serve_http_ingress(state.clone(), addr, Duration::from_millis(20)).await;

    assert!(
        state.http_ingress_error.lock().unwrap().is_none(),
        "a clean bind must not report a degradation"
    );
}

// -- the degradation has to be visible to clients ---------------------------

async fn rpc(sock: &std::path::Path, req: &Request) -> Response {
    let mut stream = dira_ipc::connect(sock).await.expect("connect");
    let bytes = serde_json::to_vec(req).unwrap();
    dira_ipc::write_frame(&mut stream, &bytes).await.unwrap();
    let resp = dira_ipc::read_frame(&mut stream).await.unwrap();
    serde_json::from_slice(&resp).unwrap()
}

/// Problem #1 from the incident: a daemon that cannot capture must not look
/// identical to a healthy one. `DaemonInfo` carries the reason so
/// `dira daemon status` can say "degraded" instead of a bare "up".
#[tokio::test]
async fn daemon_info_reports_the_ingress_degradation() {
    let state = test_state().await;
    let sock = tmp_sock();
    let (uds, _lock) = dirad::bind_control_socket(&sock).await.expect("bind");
    dirad::serve_control(state.clone(), uds);

    // Healthy first: nothing to report.
    match rpc(&sock, &Request::DaemonInfo).await {
        Response::DaemonInfo {
            http_ingress_error, ..
        } => assert!(
            http_ingress_error.is_none(),
            "a healthy daemon must report no degradation"
        ),
        other => panic!("expected DaemonInfo, got {other:?}"),
    }

    let squatter = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = squatter.local_addr().unwrap().to_string();
    dirad::serve_http_ingress(state.clone(), addr, Duration::from_secs(3600)).await;

    match rpc(&sock, &Request::DaemonInfo).await {
        Response::DaemonInfo {
            http_ingress_error, ..
        } => {
            let reason = http_ingress_error.expect("degradation must reach the client");
            assert!(
                reason.contains("hook ingress"),
                "reason must explain what is broken, got: {reason}"
            );
        }
        other => panic!("expected DaemonInfo, got {other:?}"),
    }

    let _ = std::fs::remove_file(&sock);
}
