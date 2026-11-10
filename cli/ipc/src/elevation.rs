//! Whether this process runs with an elevated token, and what to tell the user
//! when the control channel refuses a connection because of it.
//!
//! This lives in `dira-ipc` rather than `dira-core` because elevation is not a
//! general OS concern here — it is a property of *who can open the control
//! channel*, which is precisely this crate's subject. It is also the crate that
//! already owns `windows-sys`.
//!
//! **The testability rule:** [`is_elevated`] itself cannot be meaningfully tested
//! (it is a no-op off windows, and CI runners give us exactly one token, probably
//! the elevated one). So every *decision* derived from it is a pure
//! `fn(bool) -> String` that compiles and is unit-tested on macOS and Linux too.
//! The same discipline `dira_core::config::sanitize_ident` uses for its
//! windows-only callers.

/// Is this process running with an elevated ("Administrator") token?
///
/// windows: `GetTokenInformation(TokenElevation)`.
///
/// unix: always `false`, deliberately **not** `geteuid() == 0`. A root-owned
/// `0600` socket is a different failure with different advice, and conflating the
/// two would make `dira`'s guidance wrong on Linux.
///
/// Fails *open* (returns `false`) on any error: a false positive would print
/// alarming, wrong advice on every command.
pub fn is_elevated() -> bool {
    #[cfg(windows)]
    {
        windows_impl::is_elevated().unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// What `dirad` should warn about at startup, or `None` when nothing is wrong.
///
/// Deliberately a warning and never a refusal. With the control channel's
/// security descriptor applied, an elevated daemon *works* — refusing to start
/// would break a configuration this crate's own fix makes correct, strand users
/// whose only workable setup is an always-elevated terminal, and recreate the
/// respawn-loop shape D-0009 exists to prevent.
pub fn daemon_elevation_warning(elevated: bool) -> Option<String> {
    if !elevated {
        return None;
    }
    Some(
        "dirad is running elevated (Administrator). This works, but every `dira` \
         command and every harness hook must then come from a process that can open \
         an admin-created control channel. Prefer a non-elevated daemon: run \
         `dira daemon stop`, start it again from a normal terminal, then \
         `dira daemon install` to register a logon task that runs at your normal \
         privilege level."
            .to_string(),
    )
}

/// What could be established about a control endpoint that refused us.
///
/// The two platforms refuse for unrelated reasons, and the advice must not
/// cross over. On windows the pipe's DACL and integrity label turn on token
/// elevation (D-0016), so "started from an Administrator terminal" is the
/// diagnosis. On unix the socket is a `0600` file (D-0008): permission denied
/// means it belongs to another user or has the wrong mode, and talk of
/// Administrator terminals there is simply wrong.
///
/// Gathered once by [`EndpointAccess::probe`], the only platform-dependent
/// part; [`EndpointAccess::denied_advice`] is a pure function of the variant,
/// so both arms are unit-tested on every host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointAccess {
    /// A windows named pipe. Only this process's elevation matters.
    WindowsPipe { client_elevated: bool },
    /// A unix domain socket file, with what `stat` could tell us. Every field
    /// is optional: a socket we cannot even `stat` (a parent directory is not
    /// searchable) is still a socket that refused us.
    UnixSocket {
        path: std::path::PathBuf,
        /// The socket file's owning uid.
        owner_uid: Option<u32>,
        /// This process's effective uid.
        current_uid: Option<u32>,
        /// The permission bits (`mode & 0o7777`).
        mode: Option<u32>,
    },
}

impl EndpointAccess {
    /// Gather the facts for this platform's endpoint at `path`. Never fails:
    /// anything unreadable is left `None`.
    pub fn probe(path: &std::path::Path) -> Self {
        #[cfg(windows)]
        {
            let _ = path;
            EndpointAccess::WindowsPipe {
                client_elevated: is_elevated(),
            }
        }
        // The crate builds only for unix and windows (see `lib.rs`), so the
        // two arms are exhaustive.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::metadata(path).ok();
            // SAFETY: `geteuid` takes no arguments, cannot fail, and touches
            // no memory we own.
            let euid = unsafe { libc::geteuid() };
            EndpointAccess::UnixSocket {
                path: path.to_path_buf(),
                owner_uid: meta.as_ref().map(|m| m.uid()),
                current_uid: Some(euid),
                mode: meta.as_ref().map(|m| m.mode() & 0o7777),
            }
        }
    }

    /// Whether this process is elevated, as far as the endpoint is concerned.
    /// Always `false` for a unix socket (see [`is_elevated`] for why root is
    /// not "elevated" here).
    pub fn client_elevated(&self) -> bool {
        matches!(
            self,
            EndpointAccess::WindowsPipe {
                client_elevated: true
            }
        )
    }

    /// What `dira` should say when this endpoint answered access-denied.
    ///
    /// The elevation story is produced only for [`EndpointAccess::WindowsPipe`].
    pub fn denied_advice(&self) -> String {
        match self {
            EndpointAccess::WindowsPipe { client_elevated } => {
                access_denied_advice(*client_elevated)
            }
            EndpointAccess::UnixSocket {
                path,
                owner_uid,
                current_uid,
                mode,
            } => unix_socket_denied_advice(path, *owner_uid, *current_uid, *mode),
        }
    }
}

/// The unix arm of [`EndpointAccess::denied_advice`]: an ownership or mode
/// problem on a `0600` socket file, never an elevation one.
///
/// Like the windows advice it never says "try `dira daemon start`" on its own:
/// a socket file that refuses us is held by someone, and a daemon started with
/// `sudo` to "get past" the error leaves a root-owned socket behind, which is
/// the same failure for the next ordinary command.
fn unix_socket_denied_advice(
    path: &std::path::Path,
    owner_uid: Option<u32>,
    current_uid: Option<u32>,
    mode: Option<u32>,
) -> String {
    let path = path.display();
    let mode_str = mode.map(|m| format!("{m:04o}"));
    let other_user = "Then either run dira as the user that owns that daemon, or stop it as \
                      that user (`sudo dira daemon stop` if root started it) and start it \
                      again as yourself with `dira daemon start`.";
    let (diagnosis, remedy) = match (owner_uid, current_uid) {
        (Some(owner), Some(me)) if owner != me => (
            format!(
                "the socket is owned by uid {owner}, but you are uid {me}{} — most likely \
                 a dirad running as another user (or started with sudo)",
                mode_str
                    .as_deref()
                    .map(|m| format!(" (mode {m})"))
                    .unwrap_or_default()
            ),
            other_user.to_string(),
        ),
        (Some(owner), Some(_)) => (
            format!(
                "the socket is yours (uid {owner}) but its mode is {} — dirad creates it \
                 0600, so something changed it, or a parent directory is not searchable",
                mode_str.as_deref().unwrap_or("unknown")
            ),
            format!(
                "Then restore the mode (`chmod 600 {path}`), or restart the daemon so it \
                 recreates the socket: dira daemon restart"
            ),
        ),
        _ => (
            "its owner or mode could not be read — a parent directory may not be \
             searchable by you, or the socket belongs to another user"
                .to_string(),
            other_user.to_string(),
        ),
    };
    format!(
        "the daemon's control socket refused this connection (permission denied on \
         {path}): {diagnosis}.\n\
         \n\
         Check who owns it:\n\
         \x20 ls -l {path}\n\
         \n\
         {remedy}\n\
         \n\
         Do NOT run `sudo dira daemon start` — a root-owned socket recreates the problem."
    )
}

/// What `dira` should say when the windows control pipe answers with
/// access-denied. Callers on any platform go through
/// [`EndpointAccess::denied_advice`]; this stays public and platform-free so
/// its wording is testable everywhere.
///
/// `client_elevated` is *this* process's elevation, which flips the diagnosis:
/// an unelevated client being refused almost always means the daemon is elevated,
/// while an elevated client being refused points at a different user account
/// entirely.
///
/// Neither branch says "try `dira daemon start`" on its own. That was the old
/// catch-all advice, and following it makes the situation strictly worse: the
/// spawned daemon fails `first_pipe_instance` against the live one and exits,
/// after the CLI has already overwritten the real daemon's pidfile.
pub fn access_denied_advice(client_elevated: bool) -> String {
    if client_elevated {
        "the daemon refused this connection (access denied) while you are running \
         elevated — the control channel is most likely owned by a different user \
         account. Check which user the daemon runs as, and `dira config path` for \
         which endpoint this CLI is dialling."
            .to_string()
    } else {
        "the daemon refused this connection (access denied). A dirad IS running — it \
         is just not reachable from a normal process, which almost always means it \
         was started elevated (from an Administrator terminal, or by an installer \
         run as Administrator).\n\
         \n\
         Fix it once:\n\
         \x20 1. from an ADMIN terminal:   dira daemon stop\n\
         \x20 2. from a NORMAL terminal:  dira daemon start\n\
         \x20 3. from a NORMAL terminal:  dira daemon install\n\
         \n\
         Do NOT run `dira daemon start` as Administrator — that recreates the problem."
            .to_string()
    }
}

#[cfg(windows)]
pub(crate) mod windows_impl {
    use std::io;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::TOKEN_QUERY;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// A process token handle that closes itself. Shared with `security.rs`,
    /// which needs the same open-query-close dance for the user SID.
    pub(crate) struct OwnedToken(pub(crate) HANDLE);

    impl OwnedToken {
        pub(crate) fn for_current_process(access: u32) -> io::Result<Self> {
            let mut handle: HANDLE = std::ptr::null_mut();
            // SAFETY: `handle` is a valid out-pointer; the returned handle is
            // owned by `OwnedToken` and closed exactly once in `Drop`.
            let ok = unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut handle) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(OwnedToken(handle))
        }
    }

    impl Drop for OwnedToken {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: opened by `OpenProcessToken` above and not closed yet.
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    pub(super) fn is_elevated() -> io::Result<bool> {
        let token = OwnedToken::for_current_process(TOKEN_QUERY)?;
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut returned = 0u32;
        // SAFETY: `elevation` is a correctly-sized, correctly-typed buffer for
        // the `TokenElevation` class, and `returned` is a valid out-pointer.
        let ok = unsafe {
            GetTokenInformation(
                token.0,
                TokenElevation,
                (&mut elevation as *mut TOKEN_ELEVATION).cast(),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut returned,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(elevation.TokenIsElevated != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unelevated_daemon_warns_about_nothing() {
        assert!(daemon_elevation_warning(false).is_none());
    }

    #[test]
    fn an_elevated_daemon_warns_and_says_how_to_recover() {
        let w = daemon_elevation_warning(true).expect("elevated daemons must warn");
        assert!(w.contains("elevated"));
        assert!(w.contains("dira daemon install"));
    }

    /// The exact regression guard on the advice being removed: the old catch-all
    /// told an access-denied caller to run `dira daemon start`, which spawns a
    /// second daemon that dies on `first_pipe_instance` *after* clobbering the
    /// live pidfile.
    #[test]
    fn the_denied_advice_never_tells_you_to_just_start_the_daemon() {
        let advice = access_denied_advice(false);
        assert!(
            advice.contains("Do NOT run `dira daemon start` as Administrator"),
            "must warn against the action that makes it worse"
        );
        assert!(
            advice.contains("dira daemon stop"),
            "recovery starts by stopping the elevated daemon"
        );
        // It must not claim the daemon is absent — it is running and refusing.
        assert!(!advice.contains("not running"));
    }

    /// An elevated client being refused is a different diagnosis: not our own
    /// elevation, but a different account owning the endpoint.
    #[test]
    fn an_elevated_client_gets_a_different_diagnosis() {
        let advice = access_denied_advice(true);
        assert!(advice.contains("different user account"));
        assert!(!advice.contains("dira daemon stop"));
    }

    /// The bug this split exists for: a unix permission-denied socket was told
    /// to go find an Administrator terminal. The unix arm must talk about
    /// ownership and mode, name the path, and never mention elevation.
    #[test]
    fn the_unix_socket_advice_never_mentions_elevation() {
        let a = EndpointAccess::UnixSocket {
            path: "/run/user/501/dira/dira.sock".into(),
            owner_uid: Some(0),
            current_uid: Some(501),
            mode: Some(0o600),
        };
        let advice = a.denied_advice();
        assert!(!advice.contains("Administrator"), "{advice}");
        assert!(!advice.to_lowercase().contains("elevat"), "{advice}");
        assert!(
            advice.contains("ls -l /run/user/501/dira/dira.sock"),
            "{advice}"
        );
        assert!(advice.contains("uid 0"), "{advice}");
        assert!(advice.contains("uid 501"), "{advice}");
        assert!(advice.contains("0600"), "{advice}");
        assert!(advice.contains("Do NOT run `sudo dira daemon start`"));
        assert!(!a.client_elevated());
    }

    /// Same owner but refused: the mode (or a parent directory) is the
    /// problem, not another user.
    #[test]
    fn a_socket_we_own_points_at_the_mode() {
        let advice = unix_socket_denied_advice(
            std::path::Path::new("/tmp/x.sock"),
            Some(501),
            Some(501),
            Some(0o000),
        );
        assert!(advice.contains("is yours (uid 501)"), "{advice}");
        assert!(advice.contains("0000"), "{advice}");
        assert!(advice.contains("chmod 600 /tmp/x.sock"), "{advice}");
        assert!(advice.contains("dira daemon restart"), "{advice}");
        assert!(!advice.contains("another user (or"), "{advice}");
    }

    /// An unreadable owner still produces actionable advice with the path.
    #[test]
    fn an_unstattable_socket_still_names_the_path() {
        let advice =
            unix_socket_denied_advice(std::path::Path::new("/x/dira.sock"), None, Some(1), None);
        assert!(advice.contains("ls -l /x/dira.sock"), "{advice}");
        assert!(advice.contains("could not be read"), "{advice}");
    }

    /// The windows pipe keeps its elevation diagnosis, for both tokens.
    #[test]
    fn the_windows_pipe_keeps_the_elevation_advice() {
        for elevated in [false, true] {
            let a = EndpointAccess::WindowsPipe {
                client_elevated: elevated,
            };
            assert_eq!(a.denied_advice(), access_denied_advice(elevated));
            assert_eq!(a.client_elevated(), elevated);
        }
    }

    /// The runtime selection: only a windows build ever produces the pipe
    /// variant; a unix build reads the socket file and its own uid.
    #[test]
    fn probe_selects_this_platforms_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-socket");
        std::fs::write(&path, b"").unwrap();
        let a = EndpointAccess::probe(&path);
        #[cfg(windows)]
        assert!(matches!(a, EndpointAccess::WindowsPipe { .. }), "{a:?}");
        #[cfg(unix)]
        match a {
            EndpointAccess::UnixSocket {
                owner_uid,
                current_uid,
                mode,
                ..
            } => {
                // We just created it, so we own it.
                assert_eq!(owner_uid, current_uid);
                assert!(mode.is_some());
            }
            other => panic!("unix must probe the socket file: {other:?}"),
        }
    }

    /// It must not panic, and on non-windows it is definitionally false. No
    /// assertion on the value under windows: the CI runner is very likely
    /// elevated, so asserting either way would be a coin flip.
    #[test]
    fn is_elevated_is_callable() {
        let elevated = is_elevated();
        #[cfg(not(windows))]
        assert!(!elevated, "unix must never report elevation");
        #[cfg(windows)]
        eprintln!("is_elevated() = {elevated} on this runner");
    }
}
