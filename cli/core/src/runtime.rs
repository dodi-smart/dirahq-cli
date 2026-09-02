//! Cloud agent runtime detection.
//!
//! A cloud runtime is an ephemeral VM a harness vendor runs the agent in —
//! Claude Code on the web, Cursor cloud agents — as opposed to a developer's
//! own machine. Detection is metadata for labeling and diagnostics only
//! (device labels, `dira doctor`): capture and accounting behave identically
//! everywhere, per the policy-free posture.
//!
//! Detection is deliberately conservative: only markers the vendor documents
//! as set inside their VM, plus an explicit [`ENV_RUNTIME`] override for
//! runtimes without a confirmed marker (a Cursor cloud environment sets
//! `DIRA_RUNTIME=cursor-cloud` in its committed environment config). An
//! unmarked environment detects as `None` — a false "cloud" label on a
//! laptop is worse than a missing one in a VM.

/// Explicit override naming the runtime, e.g. `claude-web` / `cursor-cloud`.
/// Any non-blank value is honored verbatim (trimmed): the set of runtimes is
/// open, and an unknown-but-stated runtime is still better metadata than
/// guessing. Blank reads as unset.
pub const ENV_RUNTIME: &str = "DIRA_RUNTIME";
/// Claude Code sets this to `true` in its cloud sessions (and never locally).
pub const ENV_CLAUDE_CODE_REMOTE: &str = "CLAUDE_CODE_REMOTE";
/// Claude Code's cloud session id (`cse_…`), usable as a transcript back-link.
pub const ENV_CLAUDE_CODE_REMOTE_SESSION_ID: &str = "CLAUDE_CODE_REMOTE_SESSION_ID";

/// The runtime id detection reports for Claude Code on the web.
pub const RUNTIME_CLAUDE_WEB: &str = "claude-web";

/// A detected cloud runtime: a short stable id plus, when the vendor exposes
/// one, an opaque session reference (e.g. Claude's `cse_…` id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudRuntime {
    pub id: String,
    pub session_ref: Option<String>,
}

/// Detect the cloud runtime from the process environment, or `None` on an
/// ordinary machine.
pub fn detect() -> Option<CloudRuntime> {
    detect_from(crate::env::non_blank)
}

/// `id`/`session_ref` are metadata for device labels (`cloud:<id>:<ref>`,
/// see `dira/src/device.rs::runner_label`) and diagnostics only — never a
/// trust or routing decision — but they're still vendor-supplied strings, and
/// an unbounded one (a misbehaving harness, or a future vendor marker) would
/// otherwise flow straight into a label sent to the cloud. Chars, not bytes,
/// so a clamp never lands mid-codepoint.
const MAX_FIELD_CHARS: usize = 64;

/// Truncate `s` to [`MAX_FIELD_CHARS`] characters, verbatim below that.
fn clamp_field(s: String) -> String {
    if s.chars().count() <= MAX_FIELD_CHARS {
        return s;
    }
    s.chars().take(MAX_FIELD_CHARS).collect()
}

/// Detection against an injected env lookup, so every branch is unit-testable
/// without mutating process-global state (the same pattern `config`'s
/// platform-path helpers use).
fn detect_from(var: impl Fn(&str) -> Option<String>) -> Option<CloudRuntime> {
    let non_blank = |k: &str| {
        var(k)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };

    // The explicit override names ONLY the runtime id — it's a statement
    // about which vendor's VM this is, not a session reference, so (unlike
    // the claude-web branch below) it never reads
    // `CLAUDE_CODE_REMOTE_SESSION_ID`: an override paired with a stale or
    // unrelated session id lying around in the environment must not leak
    // into a runtime this override is naming as something else entirely.
    if let Some(id) = non_blank(ENV_RUNTIME) {
        return Some(CloudRuntime {
            id: clamp_field(id),
            session_ref: None,
        });
    }
    if non_blank(ENV_CLAUDE_CODE_REMOTE).as_deref() == Some("true") {
        return Some(CloudRuntime {
            id: RUNTIME_CLAUDE_WEB.to_string(),
            session_ref: non_blank(ENV_CLAUDE_CODE_REMOTE_SESSION_ID).map(clamp_field),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn plain_machine_detects_nothing() {
        assert_eq!(detect_from(env(&[])), None);
        // A blank or non-"true" marker is not a cloud VM.
        assert_eq!(detect_from(env(&[(ENV_CLAUDE_CODE_REMOTE, "")])), None);
        assert_eq!(detect_from(env(&[(ENV_CLAUDE_CODE_REMOTE, "false")])), None);
    }

    #[test]
    fn claude_web_marker_detects_with_session_ref() {
        let got = detect_from(env(&[
            (ENV_CLAUDE_CODE_REMOTE, "true"),
            (ENV_CLAUDE_CODE_REMOTE_SESSION_ID, "cse_01ABC"),
        ]))
        .unwrap();
        assert_eq!(got.id, RUNTIME_CLAUDE_WEB);
        assert_eq!(got.session_ref.as_deref(), Some("cse_01ABC"));
    }

    #[test]
    fn explicit_override_wins_and_is_taken_verbatim() {
        let got = detect_from(env(&[
            (ENV_RUNTIME, " cursor-cloud "),
            (ENV_CLAUDE_CODE_REMOTE, "true"),
        ]))
        .unwrap();
        assert_eq!(got.id, "cursor-cloud", "override wins over vendor markers");
        assert_eq!(got.session_ref, None);
        // Blank override reads as unset: vendor detection still applies.
        let got = detect_from(env(&[
            (ENV_RUNTIME, "  "),
            (ENV_CLAUDE_CODE_REMOTE, "true"),
        ]));
        assert_eq!(got.unwrap().id, RUNTIME_CLAUDE_WEB);
    }

    /// The override names ONLY the runtime — it is not the claude-web branch,
    /// so a session id sitting in the environment alongside it must never
    /// leak into the reported `session_ref`.
    #[test]
    fn explicit_override_never_carries_a_session_ref() {
        let got = detect_from(env(&[
            (ENV_RUNTIME, "cursor-cloud"),
            (ENV_CLAUDE_CODE_REMOTE_SESSION_ID, "cse_01ABC"),
        ]))
        .unwrap();
        assert_eq!(got.id, "cursor-cloud");
        assert_eq!(
            got.session_ref, None,
            "the override branch must not read CLAUDE_CODE_REMOTE_SESSION_ID"
        );
    }

    /// `id` and `session_ref` feed straight into a device label sent to the
    /// cloud (`cloud:<id>:<ref>`); an unbounded vendor-supplied string must
    /// not flow through unclamped. Clamped by characters, not bytes, so a
    /// multi-byte string is never cut mid-codepoint.
    #[test]
    fn id_and_session_ref_are_clamped_to_64_chars() {
        let long_id = "x".repeat(200);
        let got = detect_from(env(&[(ENV_RUNTIME, long_id.as_str())])).unwrap();
        assert_eq!(got.id.chars().count(), MAX_FIELD_CHARS);
        assert_eq!(got.id, "x".repeat(MAX_FIELD_CHARS));

        let long_session = "y".repeat(200);
        let got = detect_from(env(&[
            (ENV_CLAUDE_CODE_REMOTE, "true"),
            (ENV_CLAUDE_CODE_REMOTE_SESSION_ID, long_session.as_str()),
        ]))
        .unwrap();
        let session_ref = got.session_ref.unwrap();
        assert_eq!(session_ref.chars().count(), MAX_FIELD_CHARS);
        assert_eq!(session_ref, "y".repeat(MAX_FIELD_CHARS));

        // A multi-byte string must clamp on a char boundary, not a byte one.
        let multibyte = "é".repeat(200);
        let got = detect_from(env(&[(ENV_RUNTIME, multibyte.as_str())])).unwrap();
        assert_eq!(got.id.chars().count(), MAX_FIELD_CHARS);
    }

    /// A short id/session_ref must pass through unchanged — the clamp is a
    /// ceiling, not a fixed width.
    #[test]
    fn short_id_and_session_ref_are_left_untouched() {
        let got = detect_from(env(&[
            (ENV_CLAUDE_CODE_REMOTE, "true"),
            (ENV_CLAUDE_CODE_REMOTE_SESSION_ID, "cse_01ABC"),
        ]))
        .unwrap();
        assert_eq!(got.session_ref.as_deref(), Some("cse_01ABC"));
    }

    /// Boundary test for `clamp_field` itself, directly: exactly at the
    /// ceiling must pass through untouched, one under must be untouched, and
    /// one over must be cut to exactly the ceiling — the off-by-one case a
    /// `<=` vs `<` slip would get wrong.
    #[test]
    fn clamp_field_boundary_at_63_64_and_65_chars() {
        let s63 = "a".repeat(63);
        let s64 = "a".repeat(64);
        let s65 = "a".repeat(65);

        assert_eq!(
            clamp_field(s63.clone()),
            s63,
            "one under the ceiling: untouched"
        );
        assert_eq!(
            clamp_field(s64.clone()),
            s64,
            "exactly at the ceiling: untouched"
        );
        assert_eq!(
            clamp_field(s65),
            s64,
            "one over the ceiling: cut down to exactly MAX_FIELD_CHARS"
        );
    }
}
