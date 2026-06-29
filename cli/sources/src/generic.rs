//! Generic JSON adapter — a harness-neutral ingress for any tool.
//!
//! Accepts `{ kind, session_id, cwd?, tool_name?, transcript_path? }` using the
//! same `kind` vocabulary as the OpenCode plugin, so any tool that can POST JSON
//! (or pipe it through `dira hook generic`) can report time without a bespoke
//! source. Produces [`Harness::Generic`] so the cloud can tell these apart from a
//! modeled harness or a human `dira start` (`Manual`).

use crate::opencode::map_kind;
use crate::{HarnessSource, Normalized};
use dira_contract::Harness;
use serde::Deserialize;

/// The generic ingress shape. Identical to the OpenCode plugin shape; unknown
/// fields are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct GenericHook {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
}

/// Map a generic hook to a normalized event. Unknown `kind`s are ignored.
pub fn normalize(hook: &GenericHook) -> Option<Normalized> {
    let kind = map_kind(hook.kind.as_deref()?)?;
    Some(Normalized {
        session_id: hook
            .session_id
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        kind,
        cwd: hook.cwd.clone(),
        tool: hook.tool_name.clone(),
        transcript_path: hook.transcript_path.clone(),
    })
}

/// The generic source.
pub struct GenericSource;

impl HarnessSource for GenericSource {
    fn harness(&self) -> Harness {
        Harness::Generic
    }
    fn id(&self) -> &'static str {
        "generic"
    }
    fn normalize(&self, payload: serde_json::Value) -> Option<Normalized> {
        let hook: GenericHook = serde_json::from_value(payload).ok()?;
        normalize(&hook)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dira_core::model::EventKind;

    #[test]
    fn maps_user_prompt() {
        let hook: GenericHook =
            serde_json::from_str(r#"{"kind":"UserPrompt","session_id":"sess","cwd":"/repo"}"#)
                .unwrap();
        let n = normalize(&hook).unwrap();
        assert_eq!(n.kind, EventKind::UserPrompt);
        assert_eq!(n.session_id, "sess");
        assert_eq!(n.cwd.as_deref(), Some("/repo"));
    }

    #[test]
    fn maps_tool_with_name() {
        let hook: GenericHook =
            serde_json::from_str(r#"{"kind":"PostTool","session_id":"s","tool_name":"make"}"#)
                .unwrap();
        let n = normalize(&hook).unwrap();
        assert_eq!(n.kind, EventKind::PostTool);
        assert_eq!(n.tool.as_deref(), Some("make"));
    }

    #[test]
    fn missing_session_falls_back_to_unknown() {
        let hook: GenericHook = serde_json::from_str(r#"{"kind":"Stop"}"#).unwrap();
        let n = normalize(&hook).unwrap();
        assert_eq!(n.session_id, "unknown");
    }

    #[test]
    fn unknown_kind_is_ignored() {
        let hook: GenericHook = serde_json::from_str(r#"{"kind":"Frobnicate"}"#).unwrap();
        assert!(normalize(&hook).is_none());
    }

    #[test]
    fn source_reports_generic_harness() {
        assert_eq!(GenericSource.harness(), Harness::Generic);
        assert_eq!(GenericSource.id(), "generic");
    }
}
