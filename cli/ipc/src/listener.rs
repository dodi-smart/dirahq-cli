//! [`Listener`] accepts inbound IPC connections: a Unix domain socket listener on
//! unix, a named-pipe accept loop on windows. [`connect`] is the client-side dual.

use crate::Stream;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(windows)]
use std::time::Duration;
#[cfg(windows)]
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
#[cfg(windows)]
use tokio::time::Instant;
#[cfg(windows)]
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY};

/// A bound IPC endpoint, accepting one connection at a time.
#[derive(Debug)]
pub enum Listener {
    #[cfg(unix)]
    Unix {
        listener: tokio::net::UnixListener,
        path: PathBuf,
    },
    /// `next` is the not-yet-connected pipe instance [`Listener::accept`] waits on. It
    /// is replaced with a freshly created instance the moment a client connects to it
    /// (before the connected one is handed back to the caller), so there is never a
    /// window with zero instances of the pipe for a subsequent client to race against.
    #[cfg(windows)]
    Pipe {
        name: String,
        next: NamedPipeServer,
        endpoint: PathBuf,
        /// The descriptor every instance of this pipe is created with — built
        /// once at bind, because each Claude event spawns a fresh `dira.exe` and
        /// a token query + SDDL parse per accepted connection would be pure
        /// waste. `None` on the `Default` rung.
        security: Option<crate::security::SecurityDescriptor>,
        /// Which rung of the ladder we actually got, and why, for `DaemonInfo`.
        level: crate::security::SecurityLevel,
        level_detail: Option<String>,
    },
}

/// Does this `create` error mean another daemon already owns the pipe name?
///
/// `first_pipe_instance(true)` reports a live owner as ACCESS_DENIED, which is
/// otherwise indistinguishable from a descriptor problem — and must NOT degrade
/// down the ladder, because a second daemon on one database is exactly what the
/// single-instance guard exists to prevent (D-0009).
#[cfg(windows)]
fn is_name_taken(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(x) if x == ERROR_ACCESS_DENIED as i32 || x == ERROR_PIPE_BUSY as i32
    )
}

/// Create one instance of `name`, applying `sec` when present.
///
/// Used by BOTH [`Listener::bind`] and [`Listener::accept`]. Forgetting the
/// accept-loop call site is the easiest mistake in this module and the failure is
/// invisible: connection #1 works, and every connection after it silently gets
/// the process token's default DACL.
#[cfg(windows)]
fn create_pipe_instance(
    name: &str,
    first_instance: bool,
    sec: Option<&crate::security::SecurityDescriptor>,
) -> io::Result<NamedPipeServer> {
    let mut opts = ServerOptions::new();
    if first_instance {
        opts.first_pipe_instance(true);
    }
    match sec {
        None => opts.create(name),
        Some(s) => {
            let mut attrs = s.attributes();
            // SAFETY: `attrs` points at a descriptor owned by `s`, which the
            // caller keeps alive across this call; tokio passes the pointer
            // straight through to `CreateNamedPipeW`'s `lpSecurityAttributes`.
            unsafe {
                opts.create_with_security_attributes_raw(
                    name,
                    (&mut attrs as *mut windows_sys::Win32::Security::SECURITY_ATTRIBUTES).cast(),
                )
            }
        }
    }
}

impl Listener {
    /// Bind a new listener at `endpoint`.
    ///
    /// unix: `endpoint` is a socket path. A stale file left at that path (e.g. by a
    /// daemon that didn't shut down cleanly) is removed first, and the parent
    /// directory is created if it doesn't exist yet — the same clear-then-bind
    /// sequence `dirad::run` already performs for its own socket.
    ///
    /// windows: `endpoint` is a pipe name (`\\.\pipe\...`).
    /// `ServerOptions::first_pipe_instance(true)` makes `create` fail outright if a
    /// pipe of that name already has a live instance, which doubles as the
    /// single-daemon guard a live UDS bind already gives unix for free — a second
    /// `dirad` can't silently share (or squat) the pipe.
    pub async fn bind(endpoint: &Path) -> io::Result<Listener> {
        #[cfg(unix)]
        {
            let _ = std::fs::remove_file(endpoint);
            if let Some(parent) = endpoint.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let listener = tokio::net::UnixListener::bind(endpoint)?;
            // Best-effort restrict the control socket to the owner (0600). The socket
            // carries CLI control requests (start/stop/nuke/...) with no auth of its
            // own beyond filesystem permissions, and the process umask can otherwise
            // leave it group- or world-readable, letting another local user talk to
            // the daemon. Mirrors `dirad::set_socket_perms`'s rationale (that code is
            // untouched; this is the same behavior ported into the new crate).
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(endpoint, std::fs::Permissions::from_mode(0o600));
            Ok(Listener::Unix {
                listener,
                path: endpoint.to_path_buf(),
            })
        }
        #[cfg(windows)]
        {
            use crate::security::{SecurityDescriptor, SecurityLevel};
            let name = pipe_name(endpoint)?;

            // Walk down the ladder: labeled → DACL-only → default. A descriptor
            // failure must never turn a working daemon into one that will not
            // start (D-0009), but it must never be silent either — the rung we
            // land on is reported on `DaemonInfo`.
            //
            // ERROR_ACCESS_DENIED / ERROR_PIPE_BUSY from `create` mean another
            // daemon already holds this name (`first_pipe_instance`), which is a
            // real bind failure and must propagate rather than degrade.
            let mut detail: Option<String> = None;
            for (level, with_label) in [
                (SecurityLevel::UserOnlyLabeled, true),
                (SecurityLevel::UserOnly, false),
            ] {
                let sec = match SecurityDescriptor::for_current_user(with_label) {
                    Ok(s) => s,
                    Err(e) => {
                        detail = Some(e.to_string());
                        continue;
                    }
                };
                match create_pipe_instance(&name, true, Some(&sec)) {
                    Ok(next) => {
                        return Ok(Listener::Pipe {
                            name,
                            next,
                            endpoint: endpoint.to_path_buf(),
                            security: Some(sec),
                            level,
                            level_detail: None,
                        })
                    }
                    Err(e) if is_name_taken(&e) => return Err(e),
                    Err(e) => detail = Some(e.to_string()),
                }
            }

            let next = create_pipe_instance(&name, true, None)?;
            Ok(Listener::Pipe {
                name,
                next,
                endpoint: endpoint.to_path_buf(),
                security: None,
                level: SecurityLevel::Default,
                level_detail: detail,
            })
        }
    }

    /// Why the control channel is not in its intended state, or `None` when it
    /// is. Always `None` on unix, where the `0600` chmod is unconditional.
    pub fn security_degradation(&self) -> Option<String> {
        match self {
            #[cfg(unix)]
            Listener::Unix { .. } => None,
            #[cfg(windows)]
            Listener::Pipe {
                level,
                level_detail,
                ..
            } => level.degradation(level_detail.as_deref()),
        }
    }

    /// Accept one inbound connection.
    pub async fn accept(&mut self) -> io::Result<Stream> {
        match self {
            #[cfg(unix)]
            Listener::Unix { listener, .. } => {
                let (stream, _addr) = listener.accept().await?;
                Ok(Stream::Unix(stream))
            }
            #[cfg(windows)]
            Listener::Pipe {
                name,
                next,
                security,
                ..
            } => {
                next.connect().await?;
                // Create the next waiting instance BEFORE handing back the connected
                // one: this ordering is what guarantees a second client dialing in
                // immediately after always finds an instance to connect to.
                //
                // The SAME descriptor goes on every instance. Omitting it here
                // would leave connection #1 protected and everything after it on
                // the token's default DACL — a bug that reproduces only on the
                // second command a user runs.
                let fresh = create_pipe_instance(name.as_str(), false, security.as_ref())?;
                let connected = std::mem::replace(next, fresh);
                Ok(Stream::PipeServer(connected))
            }
        }
    }

    /// The endpoint this listener is bound to (socket path, or pipe name as a `Path`).
    pub fn endpoint(&self) -> &Path {
        match self {
            #[cfg(unix)]
            Listener::Unix { path, .. } => path,
            #[cfg(windows)]
            Listener::Pipe { endpoint, .. } => endpoint,
        }
    }
}

/// How long [`connect`] retries a busy pipe before giving up.
#[cfg(windows)]
pub const DEFAULT_BUSY_BUDGET: Duration = Duration::from_secs(2);
/// Unix has no busy state to retry; the constant exists so callers compile
/// unchanged on both platforms.
#[cfg(not(windows))]
pub const DEFAULT_BUSY_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// Connect to a listener at `endpoint` as a client.
pub async fn connect(endpoint: &Path) -> io::Result<Stream> {
    connect_with_budget(endpoint, DEFAULT_BUSY_BUDGET).await
}

/// [`connect`] with an explicit busy-retry budget.
///
/// Exists because the hook shim wraps its send in a short outer timeout, and a
/// retry budget longer than that timeout is dead code — the outer deadline always
/// fires first, so on windows every `ERROR_PIPE_BUSY` was silently dropped rather
/// than retried. Callers with their own deadline pass a budget strictly inside it.
///
/// On unix the budget is unused: a UDS `connect` to a live listener has no
/// equivalent busy state, and a stall would be on read, which the caller's own
/// timeout already covers.
pub async fn connect_with_budget(
    endpoint: &Path,
    #[cfg_attr(unix, allow(unused_variables))] busy_budget: std::time::Duration,
) -> io::Result<Stream> {
    #[cfg(unix)]
    {
        let stream = tokio::net::UnixStream::connect(endpoint).await?;
        Ok(Stream::Unix(stream))
    }
    #[cfg(windows)]
    {
        let name = pipe_name(endpoint)?;
        // ERROR_PIPE_BUSY means every existing instance is momentarily taken (e.g.
        // the daemon is mid `accept`-swap, or briefly overloaded) — not that the pipe
        // doesn't exist. Retry with a short sleep rather than failing the CLI command
        // outright, bounded to ~2s total so a genuinely dead/wedged daemon still fails
        // fast instead of hanging the caller.
        const RETRY_INTERVAL: Duration = Duration::from_millis(50);
        let deadline = Instant::now() + busy_budget;
        loop {
            match ClientOptions::new().open(&name) {
                Ok(client) => return Ok(Stream::PipeClient(client)),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                    if Instant::now() >= deadline {
                        return Err(e);
                    }
                    tokio::time::sleep(RETRY_INTERVAL).await;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// Validate + stringify a pipe endpoint. The named-pipe APIs take a name, not a
/// `Path`; UTF-8 is required. In practice this only fails for a user-supplied
/// override with non-UTF-8 bytes: the config default's username segment is already
/// sanitized to `[A-Za-z0-9_-]` (see `dira_core::config`), so the built-in default is
/// always valid.
#[cfg(windows)]
fn pipe_name(endpoint: &Path) -> io::Result<String> {
    endpoint.to_str().map(str::to_string).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "pipe endpoint path must be valid UTF-8",
        )
    })
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn bind_accept_connect_round_trip_with_owner_only_perms() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("dira-ipc-test.sock");

        let mut listener = Listener::bind(&sock).await.unwrap();
        assert_eq!(listener.endpoint(), sock.as_path());

        let perms = std::fs::metadata(&sock).unwrap().permissions();
        assert_eq!(
            perms.mode() & 0o777,
            0o600,
            "control socket must be owner-only (0600)"
        );

        let accept = tokio::spawn(async move { listener.accept().await });
        let mut client = connect(&sock).await.unwrap();

        crate::write_frame(&mut client, b"ping").await.unwrap();
        let mut server_stream = accept.await.unwrap().unwrap();
        let got = crate::read_frame(&mut server_stream).await.unwrap();
        assert_eq!(got, b"ping");
    }

    #[tokio::test]
    async fn bind_removes_a_stale_socket_file_at_the_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("stale.sock");
        std::fs::write(&sock, b"not a socket").unwrap();

        let bound = Listener::bind(&sock).await;
        assert!(
            bound.is_ok(),
            "bind must clear a stale non-socket file left at the path: {:?}",
            bound.err()
        );
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    /// A per-test pipe name so parallel tests never collide on the same pipe.
    fn unique_pipe_name(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        PathBuf::from(format!(
            r"\\.\pipe\dira-ipc-test-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn bind_accept_connect_round_trip() {
        let name = unique_pipe_name("roundtrip");
        let mut listener = Listener::bind(&name).await.unwrap();
        assert_eq!(listener.endpoint(), name.as_path());

        let accept = tokio::spawn(async move { listener.accept().await });
        let mut client = connect(&name).await.unwrap();

        crate::write_frame(&mut client, b"ping").await.unwrap();
        let mut server_stream = accept.await.unwrap().unwrap();
        let got = crate::read_frame(&mut server_stream).await.unwrap();
        assert_eq!(got, b"ping");
    }

    #[tokio::test]
    async fn a_second_bind_to_the_same_name_fails_while_the_first_is_alive() {
        let name = unique_pipe_name("single-instance");
        let _first = Listener::bind(&name).await.unwrap();

        let second = Listener::bind(&name).await;
        assert!(
            second.is_err(),
            "a second bind to a live pipe name must fail, like a live UDS bind would"
        );
    }
}
