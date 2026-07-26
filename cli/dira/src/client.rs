//! Thin transport to the daemon over the platform IPC channel — a Unix domain
//! socket on unix, a named pipe on windows (see `dira_ipc`). Length-prefixed
//! JSON, one request → one response, identically on both platforms.

use anyhow::{anyhow, Result};
use dira_core::protocol::{Request, Response};
use std::path::Path;

/// Send one request to the daemon and await its response.
pub async fn send(socket: &Path, req: &Request) -> Result<Response> {
    let mut stream = dira_ipc::connect(socket).await.map_err(daemon_down)?;

    let bytes = serde_json::to_vec(req)?;
    dira_ipc::write_frame(&mut stream, &bytes).await?;
    let buf = dira_ipc::read_frame(&mut stream).await?;

    Ok(serde_json::from_slice(&buf)?)
}

/// Map a connect failure to a friendly, actionable message instead of a raw OS
/// error. The common cases — no socket/pipe to connect to (daemon never
/// started) and connection refused (stale socket, daemon dead) — both mean
/// "start the daemon". On windows a dead/never-started daemon surfaces the
/// same way: `ClientOptions::open` on a pipe name with no live server
/// instance returns `NotFound` (there's nothing at that pipe-namespace path
/// to open, the same shape as a missing UDS file), so this one match arm
/// already covers both platforms without a `#[cfg]` split.
fn daemon_down(e: std::io::Error) -> anyhow::Error {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::NotFound | ErrorKind::ConnectionRefused => {
            anyhow!("daemon not running — start it with `dira daemon start`")
        }
        _ => anyhow!("could not reach dirad: {e} — try `dira daemon start`"),
    }
}
