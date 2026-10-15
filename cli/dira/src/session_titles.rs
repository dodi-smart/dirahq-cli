//! Local session titles for display — never synced.
//!
//! Claude Code records each live session under `~/.claude/sessions/<pid>.json`
//! with its `sessionId` and a `name` (user-set or generated). A title can
//! paraphrase a prompt, so it is read here, at render time, for the terminal
//! only: it never enters the event log or the wire (D-0001, DIRASH-0041).

use std::collections::HashMap;

/// `sessionId → name` for every Claude Code session file that parses. Any
/// I/O or parse failure just means fewer titles.
pub fn claude_titles() -> HashMap<String, String> {
    let Ok(home) = dira_core::config::home_dir() else {
        return HashMap::new();
    };
    from_dir(&home.join(".claude").join("sessions"))
}

/// [`claude_titles`] memoized for 30 s, for views that redraw continuously
/// (`dira watch`); a title renamed mid-session shows up on the next refresh.
pub fn cached_claude_titles() -> HashMap<String, String> {
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    type Cached = Option<(Instant, HashMap<String, String>)>;
    static CACHE: OnceLock<Mutex<Cached>> = OnceLock::new();
    let cell = CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    match &*guard {
        Some((at, titles)) if at.elapsed() < Duration::from_secs(30) => titles.clone(),
        _ => {
            let titles = claude_titles();
            *guard = Some((Instant::now(), titles.clone()));
            titles
        }
    }
}

fn from_dir(dir: &std::path::Path) -> HashMap<String, String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return HashMap::new();
    };
    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read(e.path()).ok())
        // Only two fields are read, so only two are deserialized.
        .filter_map(|bytes| serde_json::from_slice::<TitleFile>(&bytes).ok())
        .filter_map(|f| {
            let name = f.name?.trim().to_string();
            (!name.is_empty()).then_some((f.session_id?, name))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_names_and_skips_what_does_not_parse() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("1.json"),
            r#"{"pid":1,"sessionId":"abc","name":"Roll out jactionlint"}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("2.json"), r#"{"sessionId":"def"}"#).unwrap();
        std::fs::write(dir.path().join("3.json"), "not json").unwrap();
        std::fs::write(dir.path().join("1.key"), "secret").unwrap();
        let t = from_dir(dir.path());
        assert_eq!(t.len(), 1);
        assert_eq!(t["abc"], "Roll out jactionlint");
        assert!(from_dir(&dir.path().join("missing")).is_empty());
    }
}

#[derive(serde::Deserialize)]
struct TitleFile {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    name: Option<String>,
}
