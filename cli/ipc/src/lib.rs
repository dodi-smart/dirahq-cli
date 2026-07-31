//! Cross-platform IPC transport between `dira` (the CLI) and `dirad` (the resident
//! daemon).
//!
//! tokio has no Windows support for `AF_UNIX` in its async I/O layer, so the
//! transport is split by platform:
//! - **unix**: a Unix domain socket ([`tokio::net::UnixStream`] /
//!   [`tokio::net::UnixListener`]) — what `dira`/`dirad` have always used.
//! - **windows**: a named pipe ([`tokio::net::windows::named_pipe`]), the closest
//!   platform-native analogue: full-duplex, byte-stream, namespaced (pipe-namespaced,
//!   `\\.\pipe\...`, rather than filesystem-namespaced), and single-daemon-enforceable
//!   via `first_pipe_instance` the same way a live UDS bind already refuses a second
//!   `dirad`.
//!
//! Both transports carry the identical wire shape regardless of platform: a 4-byte
//! big-endian `u32` length prefix followed by that many bytes of JSON payload (see
//! [`read_frame`] / [`write_frame`]). `dira` (`client::send`) and `dirad`
//! (`serve_control` / `control::handle_conn`) both go through this crate — there is
//! no other framing implementation in the workspace.
//!
//! Entry points: [`Listener::bind`] + [`Listener::accept`] (server side), [`connect`]
//! (client side), and the platform-erased [`Stream`] both sides read/write through.

pub mod elevation;
mod framing;
mod listener;
pub mod security;
mod stream;

pub use framing::{read_frame, write_frame};
pub use listener::{connect, connect_with_budget, Listener, DEFAULT_BUSY_BUDGET};
pub use security::SecurityLevel;
pub use stream::Stream;

/// The CLI binary's file name, platform-appropriate for locating it on `PATH` or
/// alongside `dirad` (e.g. self-update, daemon supervision).
#[cfg(unix)]
pub const DIRA_BIN: &str = "dira";
/// The CLI binary's file name, platform-appropriate for locating it on `PATH` or
/// alongside `dirad` (e.g. self-update, daemon supervision).
#[cfg(windows)]
pub const DIRA_BIN: &str = "dira.exe";

/// The daemon binary's file name, platform-appropriate — see [`DIRA_BIN`].
#[cfg(unix)]
pub const DIRAD_BIN: &str = "dirad";
/// The daemon binary's file name, platform-appropriate — see [`DIRA_BIN`].
#[cfg(windows)]
pub const DIRAD_BIN: &str = "dirad.exe";
