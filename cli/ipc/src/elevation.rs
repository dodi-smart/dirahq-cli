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

/// What `dira` should say when the control channel answers with access-denied.
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
