//! Write a merged JSON settings document back **without** re-serialising the
//! parts that did not change.
//!
//! The hook writers ([`crate::init::inject_nested_hooks`] and friends) work on
//! a `serde_json::Value`, whose maps are sorted (the workspace cannot turn on
//! `preserve_order`: feature unification would reorder every generated
//! contract schema). Pretty-printing that value back over a user's
//! `.claude/settings.json` re-sorted every key and expanded every short array,
//! so each `dira cloud init` diff carried noise and a repo formatted with
//! biome failed CI on its own untouched `"deny": ["…"]` line.
//!
//! [`merge`] instead splices: it locates each changed value in the original
//! text and replaces only that span, recursing into objects so an unchanged
//! sibling (a hook event dira did not touch, a `permissions` block) keeps its
//! bytes. Keys new to an object are appended after its last entry. Anything
//! that is re-rendered keeps the original's key order where the original had
//! that key, and the original's indent unit.

use serde_json::{Map, Value};

/// Render `new` as the replacement for `original` (whose parse is `old`).
/// Unchanged spans keep their bytes; the result always parses to `new`.
pub(crate) fn merge(original: &str, old: &Value, new: &Value) -> String {
    splice(original, old, new).unwrap_or_else(|| {
        // Not spliceable (minified, duplicate keys, a non-object root, a key
        // removed): re-render the whole document, still in the original's key
        // order where it has one.
        let root = parse(original);
        let unit = root
            .as_ref()
            .map_or_else(|| "  ".to_string(), |r| indent_unit(original, r));
        render(new, root.as_ref(), &unit, "") + "\n"
    })
}

fn splice(text: &str, old: &Value, new: &Value) -> Option<String> {
    let root = parse(text)?;
    let (Kind::Obj(entries), Value::Object(old), Value::Object(new)) = (&root.kind, old, new)
    else {
        return None;
    };
    // A minified document has no line structure to splice into.
    let first = entries.first()?;
    if !starts_line(text, first.key_start) {
        return None;
    }
    let unit = indent_unit(text, &root);
    let mut edits = Vec::new();
    if !splice_obj(text, &root, old, new, &unit, &mut edits) {
        return None;
    }
    // Apply back to front; an insertion at a span's end sorts after the
    // replacement of that span, and touches nothing it replaced.
    edits.sort_by_key(|e| std::cmp::Reverse((e.start, e.end)));
    let mut out = text.to_string();
    for e in edits {
        out.replace_range(e.start..e.end, &e.text);
    }
    Some(out)
}

struct Edit {
    start: usize,
    end: usize,
    text: String,
}

/// Queue the edits that turn the object at `node` (parsing to `old`) into
/// `new`. `false` = the object must be re-rendered whole.
fn splice_obj(
    text: &str,
    node: &Node,
    old: &Map<String, Value>,
    new: &Map<String, Value>,
    unit: &str,
    edits: &mut Vec<Edit>,
) -> bool {
    let Kind::Obj(entries) = &node.kind else {
        return false;
    };
    // Duplicate keys (serde keeps the last) and removed keys have no
    // well-defined in-place edit.
    let mut seen = std::collections::HashSet::new();
    if entries.is_empty()
        || !entries.iter().all(|e| seen.insert(e.key.as_str()))
        || old.keys().any(|k| !new.contains_key(k))
    {
        return false;
    }

    for e in entries {
        let (Some(ov), Some(nv)) = (old.get(&e.key), new.get(&e.key)) else {
            return false;
        };
        if ov == nv {
            continue;
        }
        if let (Value::Object(om), Value::Object(nm)) = (ov, nv) {
            if splice_obj(text, &e.value, om, nm, unit, edits) {
                continue;
            }
        }
        edits.push(Edit {
            start: e.value.start,
            end: e.value.end,
            text: render(nv, Some(&e.value), unit, line_prefix(text, e.key_start)),
        });
    }

    let last = entries.last().expect("checked non-empty");
    let prefix = line_prefix(text, last.key_start);
    let mut appended = String::new();
    for (k, v) in new.iter().filter(|(k, _)| !old.contains_key(*k)) {
        appended.push_str(&format!(
            ",\n{prefix}{}: {}",
            quote(k),
            render(v, None, unit, prefix)
        ));
    }
    if !appended.is_empty() {
        edits.push(Edit {
            start: last.value.end,
            end: last.value.end,
            text: appended,
        });
    }
    true
}

/// Pretty-print `v` whose first line continues a line indented by `prefix`.
/// Object keys follow `guide`'s order where it has them, then `v`'s own.
fn render(v: &Value, guide: Option<&Node>, unit: &str, prefix: &str) -> String {
    let inner = format!("{prefix}{unit}");
    match v {
        Value::Object(m) if !m.is_empty() => {
            let guided: Vec<(&str, &Node)> = match guide.map(|g| &g.kind) {
                Some(Kind::Obj(entries)) => entries
                    .iter()
                    .filter(|e| m.contains_key(&e.key))
                    .map(|e| (e.key.as_str(), &e.value))
                    .collect(),
                _ => Vec::new(),
            };
            let mut keys: Vec<&str> = Vec::new();
            for (k, _) in &guided {
                if !keys.contains(k) {
                    keys.push(k);
                }
            }
            for k in m.keys() {
                if !keys.contains(&k.as_str()) {
                    keys.push(k);
                }
            }
            let body: Vec<String> = keys
                .iter()
                .map(|k| {
                    let g = guided.iter().rev().find(|(gk, _)| gk == k).map(|(_, n)| *n);
                    format!("{inner}{}: {}", quote(k), render(&m[*k], g, unit, &inner))
                })
                .collect();
            format!("{{\n{}\n{prefix}}}", body.join(",\n"))
        }
        Value::Array(a) if !a.is_empty() => {
            let body: Vec<String> = a
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    let g = guide.and_then(|g| element_guide(g, i, item));
                    format!("{inner}{}", render(item, g, unit, &inner))
                })
                .collect();
            format!("[\n{}\n{prefix}]", body.join(",\n"))
        }
        other => serde_json::to_string(other).expect("a Value always serialises"),
    }
}

/// The original array element whose key order should guide `item`: the one at
/// the same index if it has the same keys, else the first that does.
fn element_guide<'a>(guide: &'a Node, i: usize, item: &Value) -> Option<&'a Node> {
    let (Kind::Arr(items), Value::Object(m)) = (&guide.kind, item) else {
        return None;
    };
    let same_keys = |n: &&Node| match &n.kind {
        Kind::Obj(entries) => {
            entries.len() == m.len() && entries.iter().all(|e| m.contains_key(&e.key))
        }
        _ => false,
    };
    items
        .get(i)
        .filter(same_keys)
        .or_else(|| items.iter().find(same_keys))
}

fn quote(k: &str) -> String {
    serde_json::to_string(k).expect("a string always serialises")
}

/// The whitespace a line opens with, for the line holding `pos`.
fn line_prefix(text: &str, pos: usize) -> &str {
    let start = text[..pos].rfind('\n').map_or(0, |i| i + 1);
    let line = &text[start..];
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

fn starts_line(text: &str, pos: usize) -> bool {
    let start = text[..pos].rfind('\n').map_or(0, |i| i + 1);
    text[start..pos].trim().is_empty()
}

/// The document's indent unit, read off the root's first key; two spaces when
/// there is nothing to read.
fn indent_unit(text: &str, root: &Node) -> String {
    match &root.kind {
        Kind::Obj(entries) => entries
            .first()
            .filter(|e| starts_line(text, e.key_start))
            .map(|e| line_prefix(text, e.key_start))
            .filter(|p| !p.is_empty())
            .unwrap_or("  ")
            .to_string(),
        _ => "  ".to_string(),
    }
}

// --- a span-recording JSON reader --------------------------------------------
//
// Only as much JSON as locating values needs: well-formedness is serde_json's
// job (the caller has already parsed the same text successfully).

struct Node {
    start: usize,
    end: usize,
    kind: Kind,
}

enum Kind {
    Obj(Vec<Entry>),
    Arr(Vec<Node>),
    Scalar,
}

struct Entry {
    key: String,
    key_start: usize,
    value: Node,
}

fn parse(text: &str) -> Option<Node> {
    let b = text.as_bytes();
    let mut i = 0;
    let node = value(text, b, &mut i)?;
    ws(b, &mut i);
    (i == b.len()).then_some(node)
}

fn ws(b: &[u8], i: &mut usize) {
    while *i < b.len() && matches!(b[*i], b' ' | b'\t' | b'\n' | b'\r') {
        *i += 1;
    }
}

fn value(text: &str, b: &[u8], i: &mut usize) -> Option<Node> {
    ws(b, i);
    let start = *i;
    let kind = match *b.get(*i)? {
        b'{' => {
            *i += 1;
            let mut entries = Vec::new();
            ws(b, i);
            if b.get(*i) == Some(&b'}') {
                *i += 1;
            } else {
                loop {
                    ws(b, i);
                    let key_start = *i;
                    string(b, i)?;
                    let key: String = serde_json::from_str(&text[key_start..*i]).ok()?;
                    ws(b, i);
                    (b.get(*i) == Some(&b':')).then_some(())?;
                    *i += 1;
                    let v = value(text, b, i)?;
                    entries.push(Entry {
                        key,
                        key_start,
                        value: v,
                    });
                    ws(b, i);
                    match b.get(*i)? {
                        b',' => *i += 1,
                        b'}' => {
                            *i += 1;
                            break;
                        }
                        _ => return None,
                    }
                }
            }
            Kind::Obj(entries)
        }
        b'[' => {
            *i += 1;
            let mut items = Vec::new();
            ws(b, i);
            if b.get(*i) == Some(&b']') {
                *i += 1;
            } else {
                loop {
                    items.push(value(text, b, i)?);
                    ws(b, i);
                    match b.get(*i)? {
                        b',' => *i += 1,
                        b']' => {
                            *i += 1;
                            break;
                        }
                        _ => return None,
                    }
                }
            }
            Kind::Arr(items)
        }
        b'"' => {
            string(b, i)?;
            Kind::Scalar
        }
        _ => {
            while *i < b.len()
                && !matches!(b[*i], b',' | b']' | b'}' | b' ' | b'\t' | b'\n' | b'\r')
            {
                *i += 1;
            }
            (*i > start).then_some(())?;
            Kind::Scalar
        }
    };
    Some(Node {
        start,
        end: *i,
        kind,
    })
}

/// Advance past a string literal starting at `b[*i] == '"'`.
fn string(b: &[u8], i: &mut usize) -> Option<()> {
    (b.get(*i) == Some(&b'"')).then_some(())?;
    *i += 1;
    while let Some(&c) = b.get(*i) {
        *i += 1;
        match c {
            b'\\' => *i += 1,
            b'"' => return Some(()),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn merged(original: &str, edit: impl FnOnce(&mut Value)) -> String {
        let old: Value = serde_json::from_str(original).unwrap();
        let mut new = old.clone();
        edit(&mut new);
        let out = merge(original, &old, &new);
        let reparsed: Value = serde_json::from_str(&out).expect("merge output is JSON");
        assert_eq!(reparsed, new, "merge output must parse to the new value");
        out
    }

    const BIOME_STYLE: &str = r#"{
  "permissions": {
    "deny": ["Bash(supabase config push:*)"]
  },
  "hooks": {
    "Stop": [{ "hooks": [{ "type": "command", "command": "user-stop" }] }]
  },
  "env": { "B": "1", "A": "2" }
}
"#;

    #[test]
    fn untouched_keys_keep_their_bytes_and_order() {
        let out = merged(BIOME_STYLE, |v| {
            v["hooks"]["SessionStart"] =
                json!([{ "hooks": [{ "type": "command", "command": "x" }] }]);
        });
        assert!(out.starts_with(
            "{\n  \"permissions\": {\n    \"deny\": [\"Bash(supabase config push:*)\"]\n  },\n  \"hooks\": {\n    \"Stop\": [{ \"hooks\": [{ \"type\": \"command\", \"command\": \"user-stop\" }] }],\n    \"SessionStart\": ["
        ), "{out}");
        assert!(
            out.ends_with("  },\n  \"env\": { \"B\": \"1\", \"A\": \"2\" }\n}\n"),
            "{out}"
        );
    }

    #[test]
    fn a_changed_array_rerenders_in_its_original_key_order() {
        let out = merged(BIOME_STYLE, |v| {
            v["hooks"]["Stop"]
                .as_array_mut()
                .unwrap()
                .push(json!({ "hooks": [{ "type": "command", "command": "dira" }] }));
        });
        // The user's entry keeps `type` before `command` even though the
        // Value it was rendered from is sorted.
        assert!(
            out.contains(
                "    \"Stop\": [\n      {\n        \"hooks\": [\n          {\n            \"type\": \"command\",\n            \"command\": \"user-stop\"\n"
            ),
            "{out}"
        );
        assert!(out.contains(
            "\"permissions\": {\n    \"deny\": [\"Bash(supabase config push:*)\"]\n  },"
        ));
    }

    #[test]
    fn new_top_level_keys_are_appended_last() {
        let out = merged("{\n\t\"z\": 1,\n\t\"a\": [1]\n}\n", |v| {
            v["hooks"] = json!({ "Stop": [] });
        });
        assert_eq!(
            out,
            "{\n\t\"z\": 1,\n\t\"a\": [1],\n\t\"hooks\": {\n\t\t\"Stop\": []\n\t}\n}\n"
        );
    }

    #[test]
    fn an_empty_object_is_rendered_whole() {
        let out = merged("{}\n", |v| v["version"] = json!(1));
        assert_eq!(out, "{\n  \"version\": 1\n}\n");
    }

    #[test]
    fn minified_and_duplicate_key_documents_fall_back_to_a_guided_render() {
        let out = merged(r#"{"z":1,"a":{"y":2,"b":3}}"#, |v| v["m"] = json!(true));
        assert_eq!(
            out,
            "{\n  \"z\": 1,\n  \"a\": {\n    \"y\": 2,\n    \"b\": 3\n  },\n  \"m\": true\n}\n"
        );
        merged("{\n  \"a\": 1,\n  \"a\": 2\n}\n", |v| v["b"] = json!(3));
    }

    #[test]
    fn escaped_keys_and_strings_are_located_correctly() {
        let original = "{\n  \"a\\\"}\": \"x\\\\\\\"]\",\n  \"hooks\": {}\n}\n";
        let out = merged(original, |v| v["hooks"]["Stop"] = json!([]));
        assert!(
            out.starts_with(
                "{\n  \"a\\\"}\": \"x\\\\\\\"]\",\n  \"hooks\": {\n    \"Stop\": []\n  }\n}"
            ),
            "{out}"
        );
    }
}
