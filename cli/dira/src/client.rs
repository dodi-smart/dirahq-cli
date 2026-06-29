//! Thin transport to the daemon over the Unix domain socket. Length-prefixed
//! JSON, one request → one response.

use anyhow::{anyhow, Result};
use dira_core::protocol::{Request, Response};
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Send one request to the daemon and await its response.
pub async fn send(socket: &Path, req: &Request) -> Result<Response> {
    let mut stream = UnixStream::connect(socket).await.map_err(daemon_down)?;

    let bytes = serde_json::to_vec(req)?;
    stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;

    Ok(serde_json::from_slice(&buf)?)
}

/// Map a socket-connect failure to a friendly, actionable message instead of a
/// raw OS error. The common cases — no socket file (daemon never started) and
/// connection refused (stale socket, daemon dead) — both mean "start the daemon".
fn daemon_down(e: std::io::Error) -> anyhow::Error {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::NotFound | ErrorKind::ConnectionRefused => {
            anyhow!("daemon not running — start it with `dira daemon start`")
        }
        _ => anyhow!("could not reach dirad: {e} — try `dira daemon start`"),
    }
}
