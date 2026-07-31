//! The windows control pipe's security descriptor.
//!
//! The unix arm of [`crate::Listener::bind`] has always chmod'd the control
//! socket to `0600`, because `dira_core::protocol` documents that the channel
//! carries `Nuke`/`Shutdown`/`IngestHook` with no auth of its own and is
//! "permission-gated by the socket itself". The windows arm never had an
//! equivalent: `CreateNamedPipeW` was called with a NULL security descriptor, so
//! the pipe inherited whatever the creating token's default DACL happened to be.
//!
//! For an *elevated* token that default grants `BUILTIN\Administrators` rather
//! than the interactive user, and the object additionally carries a High
//! mandatory integrity label. A medium-integrity `dira hook claude` is then
//! refused twice over — once by the DACL, once by the no-write-up policy — and
//! since the hook shim discards its transport result, capture dies silently.
//!
//! This module builds the descriptor the code always assumed it had.

/// Which protection the pipe actually ended up with.
///
/// The bind walks down this ladder rather than failing. The control channel is
/// the daemon's first and most load-bearing bind (D-0009: "the control socket
/// must stay the first thing up and the last thing lost"), and the elevated path
/// cannot be exercised on CI — so this code must be structurally incapable of
/// turning a working windows startup into one that will not start. A lower rung
/// is a *surfaced* degradation, never a silent one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityLevel {
    /// User-only DACL plus the medium integrity label. The intended state.
    UserOnlyLabeled,
    /// User-only DACL, but the label was rejected. Fixes the DACL half: a
    /// non-elevated client can still be denied by integrity if the daemon is
    /// elevated.
    UserOnly,
    /// No explicit descriptor — exactly the pre-fix behaviour.
    Default,
}

impl SecurityLevel {
    /// Why the channel is not in its intended state, for `DaemonInfo`.
    pub fn degradation(self, detail: Option<&str>) -> Option<String> {
        let detail = detail.unwrap_or("unknown error");
        match self {
            SecurityLevel::UserOnlyLabeled => None,
            SecurityLevel::UserOnly => Some(format!(
                "the control pipe was created without its integrity label ({detail}); \
                 it is restricted to your user account, but a daemon and a client at \
                 different elevation levels may still not reach each other"
            )),
            SecurityLevel::Default => Some(format!(
                "the control pipe could not be protected ({detail}); it fell back to \
                 the process token's default permissions, which for an elevated \
                 daemon are NOT reachable by ordinary `dira` commands or harness hooks"
            )),
        }
    }
}

/// Build the SDDL string for a pipe owned by `user_sid`.
///
/// Not `cfg`-gated: this is a pure `String` function, unit-tested on every
/// platform, following the precedent `dira_core::config::sanitize_ident` sets for
/// windows-only logic. It is the single highest-value locally-runnable test of
/// this whole module — a typo here is otherwise only observable on a real
/// windows machine.
///
/// `D:P(A;;GA;;;SY)(A;;GA;;;<user>)S:(ML;;NW;;;ME)`
///
/// - `D:P` — a *protected* DACL, so it replaces the token's default rather than
///   merging with it. Replacing that default is the entire fix.
/// - `(A;;GA;;;SY)` — LocalSystem. Costs nothing (SYSTEM can take ownership
///   regardless) and keeps service-hosting options open.
/// - `(A;;GA;;;<user>)` — **the fix.** A UAC-split account's filtered (Medium)
///   and elevated (High) tokens share one user SID, so a single ACE spans the
///   elevation boundary while still excluding every other local user. This is
///   the windows equivalent of the unix `0600`, where all of a user's processes
///   can reach the socket regardless of privilege — not a widening of it.
/// - `GA` (GENERIC_ALL) is required, not lazy. The generic mapping folds
///   `FILE_CREATE_PIPE_INSTANCE` into `FILE_GENERIC_WRITE`, and the daemon's own
///   accept loop needs that right against the first instance's DACL in order to
///   create instance N+1. A hand-tuned mask breaks the second connection.
/// - `S:(ML;;NW;;;ME)` — a **Medium** mandatory label with no-write-up. An
///   elevated daemon otherwise stamps the object High, which blocks a Medium
///   client's open (a pipe open requests write access) even with the DACL fixed.
///   Medium admits Medium and High (writing down is always allowed) while
///   keeping Low and AppContainer out — deliberately *not* `LW`, which would
///   hand this unauthenticated `Nuke`/`Shutdown` channel to sandboxed browser
///   renderers.
///
/// Applied unconditionally rather than only when elevated: on a medium daemon the
/// label is a no-op, which removes an entire "did we detect elevation correctly?"
/// branch from the security-relevant path.
pub fn sddl_for(user_sid: &str, with_label: bool) -> String {
    let dacl = format!("D:P(A;;GA;;;SY)(A;;GA;;;{user_sid})");
    if with_label {
        format!("{dacl}S:(ML;;NW;;;ME)")
    } else {
        dacl
    }
}

#[cfg(windows)]
pub use windows_impl::SecurityDescriptor;

#[cfg(windows)]
mod windows_impl {
    use super::sddl_for;
    use crate::elevation::windows_impl::OwnedToken;
    use std::io;
    use windows_sys::Win32::Foundation::{LocalFree, HLOCAL};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
        TOKEN_USER,
    };

    /// An owned, `LocalAlloc`'d SECURITY_DESCRIPTOR built from an SDDL string.
    ///
    /// `pub` because `Listener::Pipe` is a public enum variant, so its fields are
    /// reachable at `pub` visibility and a `pub(crate)` type there trips
    /// rustc's `private_interfaces` lint (a hard error under `-D warnings`).
    /// Opaque in practice: every constructor and accessor below is `pub(crate)`,
    /// so nothing outside this crate can build or inspect one.
    pub struct SecurityDescriptor {
        psd: PSECURITY_DESCRIPTOR,
    }

    // SAFETY: the descriptor is immutable after construction and only ever *read*
    // by `CreateNamedPipeW`; `LocalFree` is callable from any thread. Needed
    // because the `Listener` holding one is moved into `tokio::spawn` by
    // `dirad::serve_control`.
    unsafe impl Send for SecurityDescriptor {}
    unsafe impl Sync for SecurityDescriptor {}

    impl std::fmt::Debug for SecurityDescriptor {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("SecurityDescriptor(..)")
        }
    }

    impl SecurityDescriptor {
        /// Build the pipe descriptor for the *current process's* user SID.
        /// `with_label = false` drops the `S:` clause (the fallback rung).
        pub(crate) fn for_current_user(with_label: bool) -> io::Result<Self> {
            let sid = current_user_sid()?;
            let sddl = sddl_for(&sid, with_label);
            let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();

            let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            // SAFETY: `wide` is NUL-terminated and outlives the call; `psd` is a
            // valid out-pointer. On success the buffer is LocalAlloc'd and owned
            // by the returned value, freed exactly once in `Drop`.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide.as_ptr(),
                    SDDL_REVISION_1,
                    &mut psd,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(SecurityDescriptor { psd })
        }

        /// A `SECURITY_ATTRIBUTES` pointing at this descriptor.
        ///
        /// The caller MUST keep `self` alive across the `CreateNamedPipeW` call —
        /// `let sa = SecurityDescriptor::for_current_user(true)?.attributes();`
        /// compiles and is a use-after-free. Bind it to a named local first.
        pub(crate) fn attributes(&self) -> SECURITY_ATTRIBUTES {
            SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.psd,
                bInheritHandle: 0,
            }
        }
    }

    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            if !self.psd.is_null() {
                // SAFETY: allocated by
                // `ConvertStringSecurityDescriptorToSecurityDescriptorW`, which
                // documents `LocalFree` as the matching deallocator.
                unsafe { LocalFree(self.psd as HLOCAL) };
            }
        }
    }

    /// The current process token's user SID, as an SDDL string (`S-1-5-21-…`).
    fn current_user_sid() -> io::Result<String> {
        let token = OwnedToken::for_current_process(TOKEN_QUERY)?;

        // First call sizes the buffer; it is expected to fail with
        // ERROR_INSUFFICIENT_BUFFER.
        let mut needed = 0u32;
        // SAFETY: a null buffer with zero length is the documented way to query
        // the required size.
        unsafe {
            GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        }
        if needed == 0 {
            return Err(io::Error::last_os_error());
        }

        // `TOKEN_USER` contains a pointer, so the backing buffer must be aligned
        // for it — a `Vec<u8>` is only byte-aligned. `Vec<u64>` over-aligns.
        let words = (needed as usize).div_ceil(8);
        let mut buf = vec![0u64; words];
        // SAFETY: `buf` is at least `needed` bytes and suitably aligned for
        // `TOKEN_USER`.
        let ok = unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buf.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: on success the buffer holds a `TOKEN_USER` whose `User.Sid`
        // points inside that same buffer, which is still alive here.
        let sid_ptr = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        let mut wide: *mut u16 = std::ptr::null_mut();
        // SAFETY: `sid_ptr` is a valid SID for the lifetime of `buf`.
        let ok = unsafe { ConvertSidToStringSidW(sid_ptr, &mut wide) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        // This buffer is separately LocalAlloc'd — an easy second leak to miss.
        let sid = unsafe {
            let mut len = 0usize;
            while *wide.add(len) != 0 {
                len += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(wide, len));
            LocalFree(wide as HLOCAL);
            s
        };
        Ok(sid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact descriptor, asserted byte for byte. On a real machine a typo
    /// here degrades silently to "nobody can reach the daemon"; this is the only
    /// place it can be caught without windows.
    #[test]
    fn sddl_for_a_known_sid_is_exact() {
        let sid = "S-1-5-21-1111111111-2222222222-3333333333-1001";
        assert_eq!(
            sddl_for(sid, true),
            "D:P(A;;GA;;;SY)(A;;GA;;;S-1-5-21-1111111111-2222222222-3333333333-1001)S:(ML;;NW;;;ME)"
        );
    }

    #[test]
    fn dropping_the_label_leaves_the_dacl_intact() {
        let sid = "S-1-5-21-1-2-3-1001";
        let unlabeled = sddl_for(sid, false);
        assert_eq!(unlabeled, "D:P(A;;GA;;;SY)(A;;GA;;;S-1-5-21-1-2-3-1001)");
        assert!(!unlabeled.contains("S:"));
        // The labeled form is the unlabeled one plus the SACL, nothing else.
        assert_eq!(sddl_for(sid, true), format!("{unlabeled}S:(ML;;NW;;;ME)"));
    }

    /// The gate must stay scoped to one user. `BA` (Administrators), `WD`
    /// (Everyone) or `AU` (Authenticated Users) would each be a real regression
    /// versus even the accidental status quo.
    #[test]
    fn the_descriptor_never_grants_a_wider_audience() {
        let sddl = sddl_for("S-1-5-21-1-2-3-1001", true);
        for wider in [";;BA)", ";;WD)", ";;AU)", ";;IU)"] {
            assert!(!sddl.contains(wider), "SDDL must not grant {wider}: {sddl}");
        }
        // And the label must be Medium, never Low.
        assert!(sddl.contains("(ML;;NW;;;ME)"));
        assert!(!sddl.contains(";;LW)"));
    }

    #[test]
    fn only_the_intended_level_is_undegraded() {
        assert!(SecurityLevel::UserOnlyLabeled.degradation(None).is_none());
        let d = SecurityLevel::UserOnly.degradation(Some("boom")).unwrap();
        assert!(d.contains("boom"));
        let d = SecurityLevel::Default.degradation(Some("boom")).unwrap();
        assert!(d.contains("NOT reachable"));
    }
}
