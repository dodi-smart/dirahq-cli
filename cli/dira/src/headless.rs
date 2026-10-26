//! Headless runs: when no person is present (CI, scheduled agents), `dira hook`
//! withholds every hook that normalizes to a human signal and forwards the
//! rest, so the run records agent time and no human time. The shim decides
//! because only it sees the agent's own environment. See DIRASH-0047.

/// Explicit switch. `0`/`false`/`no`/`off` force interactive; any other
/// non-blank value forces headless. Blank reads as unset.
pub(crate) const ENV_HEADLESS: &str = "DIRA_HEADLESS";
/// GitHub sets this to `true` in every Actions job and never elsewhere. No
/// person can type into a hosted job's agent, so it implies headless unless
/// [`ENV_HEADLESS`] says otherwise.
pub(crate) const ENV_GITHUB_ACTIONS: &str = "GITHUB_ACTIONS";

/// Is this `dira hook` invocation part of a headless run?
///
/// Env read at the call site, the same shape as `hook_yield::via_portable`.
pub(crate) fn is_headless() -> bool {
    decide(dira_core::env::non_blank)
}

/// [`is_headless`] against an injected env lookup, so every branch is
/// unit-testable without mutating process-global state.
fn decide(var: impl Fn(&str) -> Option<String>) -> bool {
    if let Some(v) = var(ENV_HEADLESS) {
        return dira_core::env::parse_bool(&v) != Some(false);
    }
    var(ENV_GITHUB_ACTIONS).is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

/// Would `payload` reach the daemon as a human signal for `harness`?
///
/// Asks the same normalizer the daemon runs, so the shim can never disagree
/// with the daemon about which hooks are human. Only the payload's scalar
/// top-level fields are copied: the event name is always one, while
/// `tool_input`/`tool_response` can be megabytes and never decide the kind.
pub(crate) fn is_human_signal(harness: &str, payload: &serde_json::Value) -> bool {
    let Some(obj) = payload.as_object() else {
        return false;
    };
    let scalars: serde_json::Map<_, _> = obj
        .iter()
        .filter(|(_, v)| !v.is_object() && !v.is_array())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    dira_sources::normalize_for(harness, scalars.into())
        .is_some_and(|(norm, _)| norm.kind.is_human_signal())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn an_ordinary_machine_is_interactive() {
        assert!(!decide(env(&[])));
    }

    #[test]
    fn github_actions_is_headless() {
        assert!(decide(env(&[(ENV_GITHUB_ACTIONS, "true")])));
        assert!(!decide(env(&[(ENV_GITHUB_ACTIONS, "false")])));
    }

    #[test]
    fn the_explicit_switch_wins_both_ways() {
        assert!(decide(env(&[(ENV_HEADLESS, "1")])));
        assert!(decide(env(&[(ENV_HEADLESS, "yes")])));
        for off in ["0", "false", "FALSE", "no", "off"] {
            assert!(
                !decide(env(&[(ENV_HEADLESS, off), (ENV_GITHUB_ACTIONS, "true")])),
                "{off} must force interactive even on Actions"
            );
        }
    }

    #[test]
    fn human_hooks_are_recognised_and_agent_hooks_are_not() {
        let cases = [
            ("UserPromptSubmit", true),
            ("Notification", true),
            ("PermissionRequest", true),
            ("SessionStart", false),
            ("PreToolUse", false),
            ("PostToolUse", false),
            ("Stop", false),
            ("SessionEnd", false),
        ];
        for (event, human) in cases {
            let p =
                json!({ "hook_event_name": event, "session_id": "s", "tool_input": { "x": 1 } });
            assert_eq!(is_human_signal("claude", &p), human, "{event}");
        }
        let cursor = json!({ "hook_event_name": "beforeSubmitPrompt", "conversation_id": "c" });
        assert!(is_human_signal("cursor", &cursor));
        assert!(!is_human_signal("nonsense", &json!({})));
    }
}
