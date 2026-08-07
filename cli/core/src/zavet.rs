//! Pure parsing for the zavet knowledge layer: decision-record and spec
//! frontmatter plus lore-protocol commit trailers. No IO — the daemon's
//! capture path feeds this from `git show`/`git log` output, and a future CI
//! mode reuses it verbatim.

use crate::store::{ZavetCheck, ZavetDecisionCapture, ZavetSpecCapture, ZavetTrailer};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Name of the repo-layer directory the zavet plugin scaffolds.
pub const ZAVET_DIR: &str = ".zavet";

/// Where decision records live inside the repo (trailing slash included, for
/// prefix-stripping repo-relative paths).
pub const DECISIONS_DIR: &str = ".zavet/decisions/";

/// Where living feature specs live inside the repo (trailing slash included,
/// for prefix-stripping repo-relative paths).
pub const SPECS_DIR: &str = ".zavet/specs/";

/// Where the per-repo id conventions live, relative to the repo toplevel.
pub const CONFIG_PATH: &str = ".zavet/config";

/// The historical prefix, and the one a repo with no config still mints.
pub const DEFAULT_PREFIX: &str = "D";
/// The historical padding width, likewise.
pub const DEFAULT_ID_WIDTH: usize = 4;
/// Longest accepted prefix — an id has to stay quotable in a commit trailer.
const MAX_PREFIX: usize = 6;

/// Per-repo decision-id conventions, read from `.zavet/config`.
///
/// [`Default`] is the pre-prefix behaviour (`D`, width 4), which is what a
/// repo with no config gets — every call path below is then byte-identical to
/// what it did before prefixes existed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZavetConfig {
    /// Every prefix an id in this repo may carry: the one currently minting,
    /// followed by any retired by `zavet prefix`. Never empty. Retired
    /// prefixes stay resolvable because records are append-only — an id keeps
    /// the prefix it was minted under, forever.
    pub prefixes: Vec<String>,
    /// Zero-padding width for canonical ids. Fixed per repo: an id minted at
    /// one width would never join a shorthand ref resolved at another.
    pub id_width: usize,
}

impl Default for ZavetConfig {
    fn default() -> Self {
        Self {
            prefixes: vec![DEFAULT_PREFIX.to_string()],
            id_width: DEFAULT_ID_WIDTH,
        }
    }
}

/// A well-formed prefix as it appears in a FILENAME: uppercase only.
///
/// Uppercase is load-bearing here rather than cosmetic. `.zavet/decisions/` is
/// a closed directory, but a generic prefix would otherwise make
/// `notes-2024.md` read as decision `NOTES-2024`; requiring the case that
/// `next-id` actually mints keeps stray files out.
fn is_prefix_filename(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= MAX_PREFIX
        && p.starts_with(|c: char| c.is_ascii_uppercase())
        && p.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// A well-formed prefix in an ID STRING. Case-insensitive — `d-42` has always
/// canonicalized to `D-0042`, and hand-authored frontmatter still may.
fn is_prefix_id(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= MAX_PREFIX
        && p.starts_with(|c: char| c.is_ascii_alphabetic())
        && p.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Split `<PREFIX>-<digits>` out of a decision filename stem, or `None`.
fn decision_filename_parts(file: &str) -> Option<(&str, &str)> {
    let stem = file.strip_suffix(".md")?;
    let (prefix, rest) = stem.split_once('-')?;
    if !is_prefix_filename(prefix) {
        return None;
    }
    let num = rest.split('-').next()?;
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((prefix, num))
}

/// Whether a repo-relative path is a real decision record
/// (`<PREFIX>-<digits>[-slug].md`, flat): scaffolding like `.template.md`
/// lives alongside the records and must never be captured as a decision.
///
/// Shape alone, no config needed — the prefix set only matters when scanning
/// FREE TEXT, where a generic prefix would swallow `UTF-8` and `SHA-256`.
pub fn is_decision_path(path: &str) -> bool {
    path.strip_prefix(DECISIONS_DIR)
        .is_some_and(|file| !file.contains('/') && decision_filename_parts(file).is_some())
}

/// Parse `.zavet/config` — plain `key: value` with `#` comments, the same item
/// grammar as frontmatter bodies minus the fences.
///
/// Anything missing or malformed falls back to the default rather than
/// failing: a typo in config must not cost the whole capture.
pub fn parse_config(text: &str) -> ZavetConfig {
    let mut cfg = ZavetConfig::default();
    let mut minting: Option<String> = None;
    let mut retired: Vec<String> = Vec::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = unquote(decomment(value.trim())).trim();
        match key.trim() {
            "prefix" => {
                if is_prefix_filename(value) {
                    minting = Some(value.to_string());
                }
            }
            "prefix-aliases" => {
                for p in value.split([',', ' ']).filter(|p| !p.is_empty()) {
                    if is_prefix_filename(p) && !retired.iter().any(|r| r == p) {
                        retired.push(p.to_string());
                    }
                }
            }
            "id-width" => {
                if let Ok(w) = value.parse::<usize>() {
                    if (1..=9).contains(&w) {
                        cfg.id_width = w;
                    }
                }
            }
            _ => {}
        }
    }
    let minting = minting.unwrap_or_else(|| DEFAULT_PREFIX.to_string());
    retired.retain(|p| *p != minting);
    cfg.prefixes = std::iter::once(minting).chain(retired).collect();
    cfg
}

/// Whether a repo-relative path is a living spec (flat `<slug>.md`):
/// dot-prefixed files (`.spec-template.md`) and subdirectories are never
/// captured as specs.
pub fn is_spec_path(path: &str) -> bool {
    path.strip_prefix(SPECS_DIR)
        .is_some_and(|file| !file.contains('/') && !file.starts_with('.') && file.ends_with(".md"))
}

/// The trailer keys zavet records (lowercase). Everything else in a commit
/// footer (`Signed-off-by:`, `Co-authored-by:`, …) is ignored.
pub const TRAILER_KEYS: &[&str] = &[
    "why",
    "rejected",
    "constraint",
    "refs",
    "supersedes",
    "spec",
];

/// Split an id string into its prefix and digits, case-insensitively.
fn split_id(s: &str) -> Option<(&str, &str)> {
    let q = s.trim();
    let (prefix, digits) = q.split_once('-')?;
    if !is_prefix_id(prefix) {
        return None;
    }
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((prefix, digits))
}

/// Uppercase the prefix of something that looks like a decision id, leaving
/// its digits exactly as written. `None` when it isn't one.
///
/// The no-padding counterpart of [`canonical_decision_id`], for the one call
/// site that has no repo config to hand: a guard event is parsed before its
/// `cwd` has been resolved to a repo, and re-padding it there at the WRONG
/// width would be worse than not padding at all. The daemon canonicalizes it
/// properly once the repo is known.
pub fn normalize_decision_id(s: &str) -> Option<String> {
    let (prefix, digits) = split_id(s)?;
    Some(format!("{}-{}", prefix.to_ascii_uppercase(), digits))
}

/// Normalize something that looks like a decision id (`d-42`, `CLOUD-0042`) to
/// the zero-padded canonical form, or `None` when it isn't one. Every
/// ingestion point runs ids through this, so `D-7` in a trailer and `D-0007`
/// in a record frontmatter land in the store as the same key. Numbers wider
/// than the field keep their natural width (`{:0w$}` only pads), so no width
/// imposes a ceiling.
///
/// Deliberately permissive about WHICH prefix: a record declares its own, and
/// validating a whole string cannot produce a false positive. Only free-text
/// scanning restricts to the repo's prefix set.
pub fn canonical_decision_id(s: &str, width: usize) -> Option<String> {
    let (prefix, digits) = split_id(s)?;
    let n: u64 = digits.parse().ok()?;
    Some(format!(
        "{}-{:0width$}",
        prefix.to_ascii_uppercase(),
        n,
        width = width
    ))
}

/// The first decision reference in `s`, canonicalized, if any.
pub fn scan_decision_ref(s: &str, cfg: &ZavetConfig) -> Option<String> {
    scan_all_decision_refs(s, cfg).into_iter().next()
}

/// EVERY decision reference in `s`, canonicalized, deduplicated, in order of
/// first appearance. Spec bodies auto-link the decisions they mention through
/// this. Hand-rolled scan — not worth a regex dependency.
///
/// Restricted to `cfg.prefixes`, and that restriction is the whole point: this
/// walks free prose, where accepting any `[A-Z]+-<digits>` would read `UTF-8`,
/// `SHA-256`, `RFC-2119` and `CVE-2024` as decision references.
pub fn scan_all_decision_refs(s: &str, cfg: &ZavetConfig) -> Vec<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // A word boundary on the left so e.g. `CMD-1` doesn't count. A
        // continuation byte of a multi-byte char is never ascii-alphanumeric,
        // and no prefix byte is either, so `i` can only advance to a char
        // boundary before any slicing happens.
        if i > 0 && bytes[i - 1].is_ascii_alphanumeric() {
            i += 1;
            continue;
        }
        let mut matched = false;
        for p in &cfg.prefixes {
            let pb = p.as_bytes();
            // prefix + '-' + at least one digit
            if i + pb.len() + 1 >= bytes.len() {
                continue;
            }
            if !bytes[i..i + pb.len()].eq_ignore_ascii_case(pb) {
                continue;
            }
            if bytes[i + pb.len()] != b'-' {
                continue;
            }
            let start = i + pb.len() + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j == start {
                continue;
            }
            if let Some(id) = canonical_decision_id(&s[i..j], cfg.id_width) {
                if !out.contains(&id) {
                    out.push(id);
                }
            }
            i = j;
            matched = true;
            break;
        }
        if !matched {
            i += 1;
        }
    }
    out
}

/// Filter raw `key: value` trailer pairs down to the zavet allowlist,
/// normalizing keys to lowercase and extracting the first decision reference.
pub fn normalize_trailers(raw: &[(String, String)], cfg: &ZavetConfig) -> Vec<ZavetTrailer> {
    raw.iter()
        .filter_map(|(k, v)| {
            let key = k.trim().to_ascii_lowercase();
            if !TRAILER_KEYS.contains(&key.as_str()) {
                return None;
            }
            let value = v.trim().to_string();
            if value.is_empty() {
                return None;
            }
            let decision_id = scan_decision_ref(&value, cfg);
            Some(ZavetTrailer {
                key,
                value,
                decision_id,
            })
        })
        .collect()
}

/// Parse one raw git trailer block (as `%(trailers:only,unfold)` emits it —
/// one `Key: value` per line) into raw pairs, tolerating junk lines.
pub fn parse_trailer_block(block: &str) -> Vec<(String, String)> {
    block
        .lines()
        .filter_map(|line| {
            let (k, v) = line.split_once(':')?;
            let k = k.trim();
            // Trailer keys are single tokens (git enforces `token: value`);
            // a colon in prose (e.g. "note: see above" inside a body) would
            // have been excluded by `trailers:only` already.
            if k.is_empty() || k.contains(char::is_whitespace) {
                return None;
            }
            Some((k.to_string(), v.trim().to_string()))
        })
        .collect()
}

/// One parsed frontmatter entry: a scalar `key: value` or a string list
/// (inline `[a, b]` or block `- a`). Values are raw (trimmed, still quoted /
/// possibly comment-suffixed) — consumers clean per key, because `title` is
/// free text while structured keys allow inline `# comments`.
enum FmValue {
    Scalar(String),
    List(Vec<String>),
}

struct RawFrontmatter {
    entries: Vec<(String, FmValue)>,
    body_start: Option<usize>,
}

/// Walk the `---`-fenced frontmatter shared by decision records and specs.
/// The dialect is a deliberate YAML subset — plain scalars plus inline
/// (`[a, b]`) or block (`- a`) string lists — matching what the zavet
/// plugin's templates emit. A document without a closed fence yields `None`
/// (capture is best-effort); unknown keys pass through for consumers to
/// ignore.
fn parse_frontmatter(text: &str) -> Option<RawFrontmatter> {
    if text.lines().next()?.trim_end() != "---" {
        return None;
    }

    let mut entries: Vec<(String, FmValue)> = Vec::new();
    // Whether the LAST entry is a block list still accepting `- item` lines.
    let mut in_list = false;
    let mut body_start: Option<usize> = None;

    // Byte offset scanning for the body: walk with offsets instead of a line
    // iterator so we can slice the remainder after the closing `---`.
    let mut offset = text.find('\n').map(|i| i + 1).unwrap_or(text.len());
    let mut closed = false;
    while offset <= text.len() {
        let rest = &text[offset..];
        let line_end = rest.find('\n').map(|i| offset + i).unwrap_or(text.len());
        let line = &text[offset..line_end];
        let next_offset = line_end.saturating_add(1).min(text.len() + 1);

        if line.trim_end() == "---" {
            closed = true;
            body_start = Some(line_end + 1);
            break;
        }

        if in_list {
            let t = line.trim_start();
            if let Some(item) = t.strip_prefix("- ").or_else(|| t.strip_prefix("-\t")) {
                if let Some((_, FmValue::List(items))) = entries.last_mut() {
                    items.push(item.trim().to_string());
                }
                offset = next_offset;
                continue;
            } else if line.starts_with(char::is_whitespace) && !t.is_empty() {
                // Indented non-list content under a list key: tolerate, skip.
                offset = next_offset;
                continue;
            }
            in_list = false;
        }

        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            let value = value.trim();
            let structured = decomment(value);
            if let Some(inner) = structured
                .strip_prefix('[')
                .and_then(|v| v.strip_suffix(']'))
            {
                entries.push((
                    key.to_string(),
                    FmValue::List(inner.split(',').map(|i| i.trim().to_string()).collect()),
                ));
            } else if structured.is_empty() {
                // `key:` (bare or comment-only) opens a block list; with no
                // `- item` lines following it stays an empty list.
                entries.push((key.to_string(), FmValue::List(Vec::new())));
                in_list = true;
            } else {
                entries.push((key.to_string(), FmValue::Scalar(value.to_string())));
            }
        }
        if next_offset > text.len() {
            break;
        }
        offset = next_offset;
    }

    if !closed {
        return None;
    }
    Some(RawFrontmatter {
        entries,
        body_start,
    })
}

fn body_of(text: &str, body_start: Option<usize>) -> Option<String> {
    body_start
        .and_then(|s| text.get(s..))
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty())
}

/// Parse a decision record file into a [`ZavetDecisionCapture`].
///
/// Malformed documents yield `None` (capture is best-effort); unknown keys
/// are ignored. `content_hash` is left empty for the caller (it comes from
/// git, not the text).
pub fn parse_decision(text: &str, path: &str, cfg: &ZavetConfig) -> Option<ZavetDecisionCapture> {
    let fm = parse_frontmatter(text)?;
    let mut cap = ZavetDecisionCapture {
        path: path.to_string(),
        ..Default::default()
    };
    for (key, value) in &fm.entries {
        match (key.as_str(), value) {
            ("id", FmValue::Scalar(v)) => cap.id = unquote(decomment(v)).to_string(),
            ("title", FmValue::Scalar(v)) => cap.title = non_empty(unquote(v)),
            ("status", FmValue::Scalar(v)) => cap.status = non_empty(unquote(decomment(v))),
            ("supersedes", FmValue::Scalar(v)) => cap.supersedes = non_empty(unquote(decomment(v))),
            ("origin", FmValue::Scalar(v)) => cap.origin = non_empty(unquote(decomment(v))),
            ("verified", FmValue::Scalar(v)) => cap.verified = parse_bool(v),
            ("guards", FmValue::List(items)) => cap.guards.extend(clean_list(items)),
            ("checks", FmValue::List(items)) => cap.checks.extend(clean_checks(items)),
            // The errata pointer. A record stays `active` and keeps its body
            // (append-only); this is how ONE claim inside it is marked wrong
            // without superseding the whole record. Uncanonicalizable values
            // drop rather than reject the document — a typo'd pointer must not
            // cost the record itself.
            ("corrected-by", FmValue::Scalar(v)) => {
                cap.corrected_by = canonical_decision_id(unquote(decomment(v)), cfg.id_width)
            }
            _ => {} // unknown keys (tags, …) are fine
        }
    }
    // Ids are stored canonical (`D-1` → `D-0001`); a frontmatter id that
    // doesn't canonicalize (`D-x7`, missing) rejects the whole document.
    cap.id = canonical_decision_id(&cap.id, cfg.id_width)?;
    cap.body_md = body_of(text, fm.body_start);
    if cap.status.is_none() {
        cap.status = Some("active".to_string());
    }
    cap.slug = slug_of(path, &cap.id);
    Some(cap)
}

/// Parse a living spec file into a [`ZavetSpecCapture`].
///
/// The slug is the filename stem (the identity — dira captures the directory
/// by name); dot-prefixed files (templates) reject. `decisions` is the
/// frontmatter list ∪ every `D-NNNN` reference in the body, canonicalized and
/// deduplicated — links live on the spec side only, decisions stay
/// append-only. Missing `origin`/`confidence` default to the most skeptical
/// values (`reverse-engineered`, `low`): an unlabeled spec never renders more
/// trustworthy than a labeled one.
pub fn parse_spec(text: &str, path: &str, cfg: &ZavetConfig) -> Option<ZavetSpecCapture> {
    let slug = spec_slug_of(path)?;
    let fm = parse_frontmatter(text)?;
    let mut cap = ZavetSpecCapture {
        slug,
        path: path.to_string(),
        version: 1,
        ..Default::default()
    };
    let mut decisions: Vec<String> = Vec::new();
    for (key, value) in &fm.entries {
        match (key.as_str(), value) {
            ("title", FmValue::Scalar(v)) => cap.title = non_empty(unquote(v)),
            ("version", FmValue::Scalar(v)) => {
                if let Ok(n) = unquote(decomment(v)).parse::<i64>() {
                    cap.version = n;
                }
            }
            ("origin", FmValue::Scalar(v)) => {
                if let Some(o) = non_empty(unquote(decomment(v))) {
                    cap.origin = o;
                }
            }
            ("verified", FmValue::Scalar(v)) => cap.verified = parse_bool(v),
            ("confidence", FmValue::Scalar(v)) => {
                if let Some(c) = non_empty(unquote(decomment(v))) {
                    cap.confidence = c;
                }
            }
            ("date", FmValue::Scalar(v)) => cap.date = non_empty(unquote(decomment(v))),
            ("paths", FmValue::List(items)) => cap.paths.extend(clean_list(items)),
            ("checks", FmValue::List(items)) => cap.checks.extend(clean_checks(items)),
            ("decisions", FmValue::List(items)) => {
                for id in clean_list(items).filter_map(|d| canonical_decision_id(&d, cfg.id_width))
                {
                    if !decisions.contains(&id) {
                        decisions.push(id);
                    }
                }
            }
            _ => {}
        }
    }
    if cap.origin.is_empty() {
        cap.origin = "reverse-engineered".to_string();
    }
    if cap.confidence.is_empty() {
        cap.confidence = "low".to_string();
    }
    cap.body_md = body_of(text, fm.body_start);
    if let Some(body) = &cap.body_md {
        for id in scan_all_decision_refs(body, cfg) {
            if !decisions.contains(&id) {
                decisions.push(id);
            }
        }
    }
    cap.decisions = decisions;
    Some(cap)
}

/// Clean raw list items: strip inline comments and quotes, drop empties.
fn clean_list<'a>(items: &'a [String]) -> impl Iterator<Item = String> + 'a {
    items
        .iter()
        .map(|i| unquote(decomment(i)).to_string())
        .filter(|i| !i.is_empty())
}

/// The separator between a check's human label and the command that verifies
/// it. A convention, not YAML: the dialect's list grammar only carries
/// scalars, and a nested mapping would be a real grammar change the plugin's
/// awk parser could not follow.
const CHECK_SEP: &str = "::";

/// Split a `label :: command` item. With no separator the whole item IS the
/// command and doubles as its own label, so the cheapest possible check —
/// a bare command — stays legal. Only the FIRST separator splits, so a
/// command may contain `::` (Rust paths, `--grep 'A::b'`) unquoted.
fn split_check(item: &str) -> Option<ZavetCheck> {
    let (label, command) = match item.split_once(CHECK_SEP) {
        Some((l, c)) => (l.trim(), c.trim()),
        None => (item.trim(), item.trim()),
    };
    // A label with no command verifies nothing; a command is what makes it a
    // check, so an item that is all label drops.
    (!command.is_empty()).then(|| ZavetCheck {
        label: if label.is_empty() { command } else { label }.to_string(),
        command: command.to_string(),
    })
}

/// Clean raw `checks:` items into label/command pairs.
///
/// Items go through the same comment/quote stripping as every other
/// structured list, which means an UNQUOTED command containing ` #` is
/// truncated there — quote the item to keep it (`- "lint :: sh -c 'x # y'"`).
/// The command is otherwise opaque: no framework is detected, inferred or
/// special-cased anywhere in this crate.
fn clean_checks<'a>(items: &'a [String]) -> impl Iterator<Item = ZavetCheck> + 'a {
    items
        .iter()
        .map(|i| unquote(decomment(i)).to_string())
        .filter_map(|i| split_check(&i))
}

fn parse_bool(v: &str) -> Option<bool> {
    match unquote(decomment(v)).to_ascii_lowercase().as_str() {
        "true" | "yes" => Some(true),
        "false" | "no" => Some(false),
        _ => None,
    }
}

/// A guard event as parsed (permissively) from the plugin's schema-v1 payload.
/// Unknown fields are ignored and unknown `kind`s stored verbatim, so a plugin
/// newer than the consumer degrades to "recorded, filtered at query time".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardEventV1 {
    pub kind: String,
    pub decision_id: String,
    pub cwd: String,
    pub file_path: Option<String>,
    /// RFC 3339, already validated; `None` means "stamp at receive time".
    pub ts: Option<String>,
}

/// Parse a guard event, returning `None` (never an error — the emitting shim
/// is fire-and-forget) when required fields are missing or malformed. The
/// decision id is canonicalized; one that doesn't canonicalize rejects the
/// event.
pub fn parse_guard_event(payload: &serde_json::Value) -> Option<GuardEventV1> {
    let kind = payload.get("kind")?.as_str()?.trim();
    let decision_id = payload.get("decision_id")?.as_str()?.trim();
    let cwd = payload.get("cwd")?.as_str()?.trim();
    if kind.is_empty() || cwd.is_empty() {
        return None;
    }
    let decision_id = normalize_decision_id(decision_id)?;
    let file_path = payload
        .get("file_path")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    // A ts that doesn't parse falls back to receive time rather than storing
    // junk in the `at` ordering column.
    let ts = payload
        .get("ts")
        .and_then(|v| v.as_str())
        .filter(|s| OffsetDateTime::parse(s, &Rfc3339).is_ok())
        .map(str::to_string);
    Some(GuardEventV1 {
        kind: kind.to_string(),
        decision_id,
        cwd: cwd.to_string(),
        file_path,
        ts,
    })
}

/// Whether a decision renders as "unverified — hypothesis": explicitly marked
/// `verified: false`, or reverse-engineered and never confirmed by a human.
/// A reverse-engineered record with `verified: true` counts as verified.
pub fn is_unverified(origin: Option<&str>, verified: Option<bool>) -> bool {
    verified == Some(false) || (origin == Some("reverse-engineered") && verified != Some(true))
}

/// Lowercased alphanumeric query terms, with filler words dropped so
/// "why are we polling instead of having a filesystem watcher" searches for
/// `polling filesystem watcher`.
pub fn tokenize_query(query: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "a", "an", "and", "are", "as", "at", "be", "by", "did", "do", "does", "for", "from",
        "have", "having", "how", "in", "instead", "is", "it", "no", "not", "of", "on", "or", "our",
        "should", "that", "the", "then", "there", "this", "to", "use", "using", "we", "what",
        "when", "where", "which", "why", "with", "you",
    ];
    query
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_ascii_lowercase)
        .filter(|t| t.len() >= 2 && !STOP.contains(&t.as_str()))
        .collect()
}

/// One decision's searchable text, assembled by the caller from the store.
pub struct SearchDoc<'a> {
    pub id: &'a str,
    pub title: Option<&'a str>,
    pub slug: Option<&'a str>,
    pub body: Option<&'a str>,
    pub guards: &'a [String],
    /// Values of trailers referencing this decision.
    pub trailers: Vec<&'a str>,
}

/// Relevance of `doc` for the tokenized `terms` — a weighted contains-count
/// (title 5, slug 4, trailers 2, guards 2, body 1). Zero means "no term
/// matched anywhere". Grep-grade on purpose: single-repo scale, no index.
pub fn score(doc: &SearchDoc<'_>, terms: &[String]) -> u32 {
    let title = doc.title.map(str::to_ascii_lowercase).unwrap_or_default();
    let slug = doc.slug.map(str::to_ascii_lowercase).unwrap_or_default();
    let body = doc.body.map(str::to_ascii_lowercase).unwrap_or_default();
    let guards = doc.guards.join(" ").to_ascii_lowercase();
    let trailers = doc.trailers.join(" ").to_ascii_lowercase();
    let mut total = 0;
    for term in terms {
        let t = term.as_str();
        if title.contains(t) {
            total += 5;
        }
        if slug.contains(t) {
            total += 4;
        }
        if trailers.contains(t) {
            total += 2;
        }
        if guards.contains(t) {
            total += 2;
        }
        if body.contains(t) {
            total += 1;
        }
    }
    total
}

/// Relevance of an orphan commit trailer (a micro-decision with no record)
/// for the tokenized `terms`: matched terms × 2 — the same weight [`score`]
/// gives trailers attached to a decision, kept here so the ranking weights
/// live in one file.
pub fn score_trailer(key: &str, value: &str, terms: &[String]) -> u32 {
    let text = format!("{key} {value}").to_ascii_lowercase();
    terms.iter().filter(|t| text.contains(t.as_str())).count() as u32 * 2
}

/// A short plain-prose excerpt of a record body: the first non-heading,
/// non-list, non-empty line (usually the sentence under `## Decision`).
pub fn excerpt(body: &str) -> Option<String> {
    body.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('-'))
        .map(str::to_string)
}

/// `D-0001-poll-not-watch.md` → `poll-not-watch`.
fn slug_of(path: &str, id: &str) -> Option<String> {
    let file = path.rsplit('/').next()?;
    let stem = file.strip_suffix(".md")?;
    stem.strip_prefix(id)
        .and_then(|rest| rest.strip_prefix('-'))
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// `.zavet/specs/capture-pipeline.md` → `capture-pipeline`. The filename IS
/// the spec's identity; dot-prefixed stems (templates) reject.
fn spec_slug_of(path: &str) -> Option<String> {
    let file = path.rsplit('/').next()?;
    let stem = file.strip_suffix(".md")?;
    (!stem.is_empty() && !stem.starts_with('.')).then(|| stem.to_string())
}

/// Strip an inline `# comment` (a `#` preceded by whitespace) from an
/// unquoted structured value — the same dialect the plugin's awk parser
/// speaks. Quoted values pass through untouched; `title` never goes through
/// this (free text: "Fix #123 handling" keeps its `#`).
fn decomment(s: &str) -> &str {
    let t = s.trim();
    if t.starts_with('"') || t.starts_with('\'') {
        return t;
    }
    if t.starts_with('#') {
        return "";
    }
    let bytes = t.as_bytes();
    for i in 1..bytes.len() {
        if bytes[i] == b'#' && (bytes[i - 1] == b' ' || bytes[i - 1] == b'\t') {
            return t[..i].trim_end();
        }
    }
    t
}

fn unquote(s: &str) -> &str {
    let s = s.trim();
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(s)
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "---\nid: D-0042\ntitle: \"Poll git, don't watch\"\nstatus: active\nguards:\n  - cli/dirad/src/capture.rs\n  - \"cli/core/src/project.rs\"\nsupersedes: D-0007\norigin: recorded\nverified: true\n---\n\n## Decision\nPoll on events.\n\n## Why\nWatchers add failure modes.\n";

    #[test]
    fn parses_a_full_record() {
        let cap = parse_decision(
            DOC,
            ".zavet/decisions/D-0042-poll.md",
            &ZavetConfig::default(),
        )
        .unwrap();
        assert_eq!(cap.id, "D-0042");
        assert_eq!(cap.slug.as_deref(), Some("poll"));
        assert_eq!(cap.title.as_deref(), Some("Poll git, don't watch"));
        assert_eq!(cap.status.as_deref(), Some("active"));
        assert_eq!(cap.supersedes.as_deref(), Some("D-0007"));
        assert_eq!(
            cap.guards,
            vec!["cli/dirad/src/capture.rs", "cli/core/src/project.rs"]
        );
        let body = cap.body_md.unwrap();
        assert!(body.starts_with("## Decision"));
        assert!(body.contains("Watchers add failure modes."));
    }

    #[test]
    fn parses_checks_splitting_label_from_command() {
        let doc = "---\nid: D-0001\nchecks:\n  - pg suite forbids mocks :: run-the-pg-suite\n  - run-the-lint-suite\n---\nbody";
        let cap =
            parse_decision(doc, ".zavet/decisions/D-0001.md", &ZavetConfig::default()).unwrap();
        assert_eq!(cap.checks.len(), 2);
        assert_eq!(cap.checks[0].label, "pg suite forbids mocks");
        assert_eq!(cap.checks[0].command, "run-the-pg-suite");
        // Unlabeled: the command IS the label, so the cheapest check is legal.
        assert_eq!(cap.checks[1].label, "run-the-lint-suite");
        assert_eq!(cap.checks[1].command, "run-the-lint-suite");
    }

    #[test]
    fn check_keeps_later_separators_and_drops_a_commandless_item() {
        let doc = "---\nid: D-0001\nchecks:\n  - keeps later :: runner --grep 'A::b'\n  - label-only ::\n---\nbody";
        let cap =
            parse_decision(doc, ".zavet/decisions/D-0001.md", &ZavetConfig::default()).unwrap();
        assert_eq!(
            cap.checks.len(),
            1,
            "a label with no command is not a check"
        );
        assert_eq!(cap.checks[0].command, "runner --grep 'A::b'");
    }

    #[test]
    fn quoting_protects_a_hash_inside_a_check_command() {
        // Unquoted, the shared decomment rule truncates at ` #` — same as
        // every other structured list item. Quoting is the escape hatch.
        let bare = "---\nid: D-0001\nchecks:\n  - t :: runner -c 'a # b'\n---\nbody";
        let cap =
            parse_decision(bare, ".zavet/decisions/D-0001.md", &ZavetConfig::default()).unwrap();
        assert_eq!(cap.checks[0].command, "runner -c 'a");

        let quoted = "---\nid: D-0001\nchecks:\n  - \"t :: runner -c 'a # b'\"\n---\nbody";
        let cap = parse_decision(
            quoted,
            ".zavet/decisions/D-0001.md",
            &ZavetConfig::default(),
        )
        .unwrap();
        assert_eq!(cap.checks[0].command, "runner -c 'a # b'");
    }

    #[test]
    fn corrected_by_canonicalizes_and_never_rejects_the_record() {
        let doc = "---\nid: D-0014\ncorrected-by: D-7\n---\nbody";
        let cap =
            parse_decision(doc, ".zavet/decisions/D-0014.md", &ZavetConfig::default()).unwrap();
        assert_eq!(cap.corrected_by.as_deref(), Some("D-0007"));
        // A record stays ACTIVE when corrected — that is the whole point:
        // supersession replaces, correction annotates.
        assert_eq!(cap.status.as_deref(), Some("active"));

        // A malformed pointer drops the pointer, never the record.
        let bad = "---\nid: D-0014\ncorrected-by: nonsense\n---\nbody";
        let cap =
            parse_decision(bad, ".zavet/decisions/D-0014.md", &ZavetConfig::default()).unwrap();
        assert_eq!(cap.corrected_by, None);
        assert_eq!(cap.id, "D-0014");
    }

    #[test]
    fn specs_parse_checks_with_the_same_rule_as_decisions() {
        let doc =
            "---\ntitle: T\nchecks:\n  - no overflow :: run-the-sweep\nchecks2: []\n---\nbody";
        let cap = parse_spec(doc, ".zavet/specs/mobile.md", &ZavetConfig::default()).unwrap();
        assert_eq!(cap.checks.len(), 1);
        assert_eq!(cap.checks[0].label, "no overflow");
        assert_eq!(cap.checks[0].command, "run-the-sweep");
    }

    #[test]
    fn parses_inline_guard_lists_and_defaults_status() {
        let doc = "---\nid: D-0001\nguards: [a/**, \"b.rs\"]\n---\nbody";
        let cap =
            parse_decision(doc, ".zavet/decisions/D-0001.md", &ZavetConfig::default()).unwrap();
        assert_eq!(cap.guards, vec!["a/**", "b.rs"]);
        assert_eq!(cap.status.as_deref(), Some("active"));
        assert_eq!(cap.slug, None); // no slug segment in the filename
    }

    #[test]
    fn malformed_documents_yield_none() {
        for doc in [
            "no frontmatter at all",
            "---\nid: D-0001\nnever closed",
            "---\ntitle: no id\n---\nbody",
            "---\nid: NOTDECISION\n---\nbody",
            "---\nid: D-x7\n---\nbody", // non-canonicalizable id
            "",
        ] {
            assert_eq!(
                parse_decision(doc, "x.md", &ZavetConfig::default()),
                None,
                "doc: {doc:?}"
            );
        }
    }

    const SPEC: &str = "---\ntitle: Zavet capture pipeline\nversion: 2\norigin: session          # designed | session | reverse-engineered\nverified: false          # true only after human review\nconfidence: high         # low | med | high\ndate: 2026-07-16\npaths:                   # git pathspecs\n  - cli/dirad/src/capture.rs\n  - \"cli/core/src/zavet.rs\"\ndecisions: [D-1, D-0042]      # optional\n---\n\n## Overview\nOldest-first sweep per D-7; see also D-0042 again.\n\n## Open Questions\n- none\n";

    #[test]
    fn parses_a_full_spec_with_inline_comments() {
        let cap = parse_spec(
            SPEC,
            ".zavet/specs/capture-pipeline.md",
            &ZavetConfig::default(),
        )
        .unwrap();
        assert_eq!(cap.slug, "capture-pipeline");
        assert_eq!(cap.title.as_deref(), Some("Zavet capture pipeline"));
        assert_eq!(cap.version, 2);
        assert_eq!(cap.origin, "session");
        assert_eq!(cap.verified, Some(false));
        assert_eq!(cap.confidence, "high");
        assert_eq!(cap.date.as_deref(), Some("2026-07-16"));
        assert_eq!(
            cap.paths,
            vec!["cli/dirad/src/capture.rs", "cli/core/src/zavet.rs"]
        );
        // frontmatter list ∪ body refs, canonicalized, deduped, in order.
        assert_eq!(cap.decisions, vec!["D-0001", "D-0042", "D-0007"]);
        assert!(cap.body_md.unwrap().starts_with("## Overview"));
    }

    #[test]
    fn spec_defaults_are_the_most_skeptical() {
        let cap = parse_spec(
            "---\ntitle: X\n---\nbody",
            ".zavet/specs/x.md",
            &ZavetConfig::default(),
        )
        .unwrap();
        assert_eq!(cap.origin, "reverse-engineered");
        assert_eq!(cap.confidence, "low");
        assert_eq!(cap.version, 1);
        assert_eq!(cap.verified, None);
        assert!(cap.paths.is_empty());
        assert!(cap.decisions.is_empty());
    }

    #[test]
    fn spec_slug_comes_from_the_filename_and_templates_reject() {
        assert!(parse_spec(
            "---\n---\nbody",
            ".zavet/specs/.spec-template.md",
            &ZavetConfig::default()
        )
        .is_none());
        assert!(parse_spec(
            "---\n---\nbody",
            ".zavet/specs/.md",
            &ZavetConfig::default()
        )
        .is_none());
        assert!(parse_spec(
            "no frontmatter",
            ".zavet/specs/x.md",
            &ZavetConfig::default()
        )
        .is_none());
        assert!(parse_spec(
            "---\nnever closed",
            ".zavet/specs/x.md",
            &ZavetConfig::default()
        )
        .is_none());
        let cap = parse_spec(
            "---\n---\nbody",
            ".zavet/specs/auth-flow.md",
            &ZavetConfig::default(),
        )
        .unwrap();
        assert_eq!(cap.slug, "auth-flow");
    }

    #[test]
    fn decision_titles_keep_hashes_but_structured_values_decomment() {
        let doc = "---\nid: D-1\ntitle: Fix #123 handling\nstatus: active # hmm\nguards: [a/**] # inline comment\n---\nbody";
        let cap =
            parse_decision(doc, ".zavet/decisions/D-0001.md", &ZavetConfig::default()).unwrap();
        assert_eq!(cap.title.as_deref(), Some("Fix #123 handling"));
        assert_eq!(cap.status.as_deref(), Some("active"));
        assert_eq!(cap.guards, vec!["a/**"]);
    }

    #[test]
    fn scan_all_refs_collects_deduped_in_order() {
        assert_eq!(
            scan_all_decision_refs(
                "D-7 then D-0042, D-7 again; CMD-9 no",
                &ZavetConfig::default()
            ),
            vec!["D-0007", "D-0042"]
        );
        assert!(scan_all_decision_refs("nothing here", &ZavetConfig::default()).is_empty());
    }

    /// Why free-text scanning is restricted to the repo's prefix set rather
    /// than accepting any `[A-Z]+-<digits>`: prose is full of things that
    /// would otherwise read as decision references.
    #[test]
    fn scan_ignores_lookalikes_outside_the_prefix_set() {
        let cfg = ZavetConfig {
            prefixes: vec!["CLOUD".to_string()],
            id_width: 5,
        };
        let prose = "UTF-8 and SHA-256 per RFC-2119; see CVE-2024 and AES-128. \
                     The decision is CLOUD-42.";
        assert_eq!(scan_all_decision_refs(prose, &cfg), vec!["CLOUD-00042"]);
        // And the historical prefix is NOT magic — a repo that mints CLOUD
        // does not silently pick up D-refs it never retired.
        assert!(scan_all_decision_refs("see D-7", &cfg).is_empty());
    }

    /// A retired prefix stays resolvable: records are append-only, so ids
    /// minted before a `zavet prefix` change keep their old prefix forever.
    #[test]
    fn scan_resolves_retired_prefixes() {
        let cfg = ZavetConfig {
            prefixes: vec!["CLOUD".to_string(), "D".to_string()],
            id_width: 5,
        };
        assert_eq!(
            scan_all_decision_refs("D-41 then CLOUD-42", &cfg),
            vec!["D-00041", "CLOUD-00042"]
        );
    }

    /// One prefix being a prefix of another must not shadow it — the `-`
    /// is what disambiguates.
    #[test]
    fn scan_disambiguates_overlapping_prefixes() {
        let cfg = ZavetConfig {
            prefixes: vec!["D".to_string(), "DB".to_string()],
            id_width: 4,
        };
        assert_eq!(
            scan_all_decision_refs("D-1 and DB-2", &cfg),
            vec!["D-0001", "DB-0002"]
        );
    }

    #[test]
    fn config_defaults_to_the_pre_prefix_behaviour() {
        // The guarantee that makes this migration-free.
        for text in ["", "# nothing\n", "garbage\nno colons\n"] {
            let cfg = parse_config(text);
            assert_eq!(cfg.prefixes, vec!["D".to_string()], "text: {text:?}");
            assert_eq!(cfg.id_width, DEFAULT_ID_WIDTH);
        }
    }

    #[test]
    fn config_parses_prefix_aliases_and_width() {
        let cfg = parse_config(
            "# comment\nprefix: CLOUD  # minting\nprefix-aliases: D, LEGACY\nid-width: 5\n",
        );
        assert_eq!(cfg.prefixes, vec!["CLOUD", "D", "LEGACY"]);
        assert_eq!(cfg.id_width, 5);
        // Malformed values fall back rather than failing the whole capture.
        let cfg = parse_config("prefix: not-valid\nid-width: 99\n");
        assert_eq!(cfg.prefixes, vec!["D".to_string()]);
        assert_eq!(cfg.id_width, DEFAULT_ID_WIDTH);
        // The minting prefix is never duplicated into the retired list.
        let cfg = parse_config("prefix: CLOUD\nprefix-aliases: CLOUD D\n");
        assert_eq!(cfg.prefixes, vec!["CLOUD", "D"]);
    }

    #[test]
    fn decision_paths_accept_any_prefix_but_reject_stray_files() {
        for ok in [
            ".zavet/decisions/D-0042-poll.md",
            ".zavet/decisions/CLOUD-00042-poll-not-watch.md",
            ".zavet/decisions/A1-7.md",
        ] {
            assert!(is_decision_path(ok), "should accept: {ok}");
        }
        for bad in [
            ".zavet/decisions/notes-2024.md", // lowercase prefix: not a record
            ".zavet/decisions/.template.md",
            ".zavet/decisions/README.md",
            ".zavet/decisions/TOOLONGPREFIX-1.md",
            ".zavet/decisions/D-x7.md",
            ".zavet/decisions/nested/D-1.md",
        ] {
            assert!(!is_decision_path(bad), "should reject: {bad}");
        }
    }

    #[test]
    fn canonical_id_pads_to_the_repo_width() {
        assert_eq!(
            canonical_decision_id("CLOUD-42", 5).as_deref(),
            Some("CLOUD-00042")
        );
        // Padding only widens — no width imposes a ceiling.
        assert_eq!(
            canonical_decision_id("CLOUD-123456", 5).as_deref(),
            Some("CLOUD-123456")
        );
        // normalize keeps the digits exactly as written.
        assert_eq!(
            normalize_decision_id("cloud-0042").as_deref(),
            Some("CLOUD-0042")
        );
        assert_eq!(
            normalize_decision_id("CLOUD-42").as_deref(),
            Some("CLOUD-42")
        );
        for bad in ["TOOLONGPREFIX-1", "-1", "1-2", "D-", "D-x"] {
            assert_eq!(normalize_decision_id(bad), None, "input: {bad:?}");
        }
    }

    #[test]
    fn frontmatter_ids_are_stored_canonical() {
        let cap = parse_decision(
            "---\nid: D-1\n---\nbody",
            ".zavet/decisions/D-0001.md",
            &ZavetConfig::default(),
        )
        .unwrap();
        assert_eq!(cap.id, "D-0001");
    }

    #[test]
    fn canonical_id_pads_and_rejects() {
        assert_eq!(
            canonical_decision_id("D-7", DEFAULT_ID_WIDTH).as_deref(),
            Some("D-0007")
        );
        assert_eq!(
            canonical_decision_id("d-42", DEFAULT_ID_WIDTH).as_deref(),
            Some("D-0042")
        );
        assert_eq!(
            canonical_decision_id(" D-0042 ", DEFAULT_ID_WIDTH).as_deref(),
            Some("D-0042")
        );
        assert_eq!(
            canonical_decision_id("D-12345", DEFAULT_ID_WIDTH).as_deref(),
            Some("D-12345")
        );
        for bad in ["D-", "D-x7", "D-7x", "poll", "", "D-7 D-8"] {
            assert_eq!(
                canonical_decision_id(bad, DEFAULT_ID_WIDTH),
                None,
                "input: {bad:?}"
            );
        }
    }

    #[test]
    fn trailer_normalization_filters_and_extracts_refs() {
        let raw = vec![
            ("Why".to_string(), "polling beats watching".to_string()),
            ("Refs".to_string(), "D-0042 and D-0007".to_string()),
            ("Signed-off-by".to_string(), "A <a@b.c>".to_string()),
            ("REJECTED".to_string(), "notify crate".to_string()),
            ("Constraint".to_string(), "".to_string()), // empty value dropped
        ];
        let ts = normalize_trailers(&raw, &ZavetConfig::default());
        assert_eq!(ts.len(), 3);
        assert_eq!(ts[0].key, "why");
        assert_eq!(ts[0].decision_id, None);
        assert_eq!(ts[1].key, "refs");
        assert_eq!(ts[1].decision_id.as_deref(), Some("D-0042")); // first ref wins
        assert_eq!(ts[2].key, "rejected");
    }

    /// A trailer's ref resolves through the repo's prefix set at the repo's
    /// width — including a retired prefix, which is what keeps commits made
    /// before a `zavet prefix` change attributable to their decision.
    #[test]
    fn trailer_refs_resolve_prefixed_and_retired_ids() {
        let cfg = ZavetConfig {
            prefixes: vec!["CLOUD".to_string(), "D".to_string()],
            id_width: 5,
        };
        let ts = normalize_trailers(
            &[
                ("Refs".to_string(), "CLOUD-42".to_string()),
                ("Supersedes".to_string(), "D-7".to_string()),
                // Prose in a trailer value must not become a ref.
                (
                    "Why".to_string(),
                    "UTF-8 everywhere per RFC-2119".to_string(),
                ),
            ],
            &cfg,
        );
        assert_eq!(ts[0].decision_id.as_deref(), Some("CLOUD-00042"));
        assert_eq!(ts[1].decision_id.as_deref(), Some("D-00007"));
        assert_eq!(ts[2].decision_id, None);
    }

    /// The slug is the filename minus the id — it has to survive a prefix
    /// that is not `D`, or every prefixed record would capture slugless.
    #[test]
    fn prefixed_records_keep_their_slug_and_pointers() {
        let cfg = ZavetConfig {
            prefixes: vec!["CLOUD".to_string()],
            id_width: 5,
        };
        let doc = "---\nid: CLOUD-42\ntitle: T\nstatus: active\nsupersedes: CLOUD-00007\n\
                   corrected-by: CLOUD-9\nguards:\n  - src/**\n---\n\nbody\n";
        let cap = parse_decision(doc, ".zavet/decisions/CLOUD-00042-poll-not-watch.md", &cfg)
            .expect("parses");
        assert_eq!(cap.id, "CLOUD-00042");
        assert_eq!(cap.slug.as_deref(), Some("poll-not-watch"));
        // `supersedes` is stored verbatim (it always was); `corrected-by`
        // canonicalizes, so the shorthand joins the padded record.
        assert_eq!(cap.supersedes.as_deref(), Some("CLOUD-00007"));
        assert_eq!(cap.corrected_by.as_deref(), Some("CLOUD-00009"));
        assert_eq!(cap.guards, vec!["src/**"]);
    }

    /// Spec auto-linking is the widest free-text surface in the codebase —
    /// the frontmatter list ∪ every body ref. Both go through the prefix set.
    #[test]
    fn prefixed_specs_autolink_only_real_refs() {
        let cfg = ZavetConfig {
            prefixes: vec!["CLOUD".to_string(), "D".to_string()],
            id_width: 5,
        };
        let doc = "---\ntitle: Sync\ndecisions: [CLOUD-1, D-7]\n---\n\n\
                   Per CLOUD-42 and D-7 again. Encoded as UTF-8, hashed with \
                   SHA-256, per RFC-2119 and CVE-2024.\n";
        let cap = parse_spec(doc, ".zavet/specs/sync.md", &cfg).expect("parses");
        assert_eq!(
            cap.decisions,
            vec!["CLOUD-00001", "D-00007", "CLOUD-00042"],
            "frontmatter list first, then body refs in order of appearance"
        );
    }

    /// A repo that has NOT adopted a prefix must behave exactly as it did
    /// before prefixes existed. This is the whole backward-compatibility
    /// claim, asserted end to end rather than per-function.
    #[test]
    fn default_config_reproduces_pre_prefix_behaviour() {
        let cfg = ZavetConfig::default();
        let doc = "---\nid: D-1\ntitle: T\nstatus: active\nguards:\n  - a/**\n---\n\nSee D-7.\n";
        let cap = parse_decision(doc, ".zavet/decisions/D-0001-poll.md", &cfg).unwrap();
        assert_eq!(cap.id, "D-0001");
        assert_eq!(cap.slug.as_deref(), Some("poll"));
        assert_eq!(
            scan_all_decision_refs("D-7 then D-0042, D-7 again; CMD-9 no", &cfg),
            vec!["D-0007", "D-0042"]
        );
        assert_eq!(
            canonical_decision_id("d-42", cfg.id_width).as_deref(),
            Some("D-0042")
        );
        // And a prefix it never adopted is invisible to free-text scanning.
        assert!(scan_all_decision_refs("CLOUD-42", &cfg).is_empty());
    }

    #[test]
    fn decision_ref_scan_requires_word_boundary_and_digits() {
        assert_eq!(
            scan_decision_ref("see D-0042 there", &ZavetConfig::default()),
            Some("D-0042".into())
        );
        // Short refs canonicalize, so `Refs: D-7` joins a record `D-0007`.
        assert_eq!(
            scan_decision_ref("D-7", &ZavetConfig::default()),
            Some("D-0007".into())
        );
        assert_eq!(scan_decision_ref("CMD-123", &ZavetConfig::default()), None);
        assert_eq!(scan_decision_ref("D-", &ZavetConfig::default()), None);
        assert_eq!(scan_decision_ref("nothing", &ZavetConfig::default()), None);
    }

    #[test]
    fn unverified_rule_is_exact() {
        // (origin, verified) -> unverified?
        let cases = [
            (None, None, false),
            (None, Some(true), false),
            (None, Some(false), true),
            (Some("recorded"), None, false),
            (Some("reverse-engineered"), None, true),
            (Some("reverse-engineered"), Some(false), true),
            // A human confirmed the hypothesis: verified wins.
            (Some("reverse-engineered"), Some(true), false),
        ];
        for (origin, verified, want) in cases {
            assert_eq!(
                is_unverified(origin, verified),
                want,
                "origin={origin:?} verified={verified:?}",
            );
        }
    }

    #[test]
    fn trailer_block_parsing_tolerates_junk() {
        let block = "Why: because\nRefs: D-1\nnot a trailer line\nBad Key: spaces\n";
        let raw = parse_trailer_block(block);
        assert_eq!(raw.len(), 2);
        assert_eq!(raw[0], ("Why".to_string(), "because".to_string()));
    }

    #[test]
    fn guard_event_accepts_v1_and_tolerates_unknowns() {
        let ev = parse_guard_event(&serde_json::json!({
            "v": 1,
            "kind": "guard_shown",
            "decision_id": "D-0042",
            "file_path": "src/auth.rs",
            "cwd": "/repo",
            "ts": "2026-07-15T12:00:00Z",
            "meta": {"future": true},
            "unknown_field": [1, 2, 3],
        }))
        .unwrap();
        assert_eq!(ev.kind, "guard_shown");
        assert_eq!(ev.decision_id, "D-0042");
        assert_eq!(ev.ts.as_deref(), Some("2026-07-15T12:00:00Z"));
        // Unknown kinds are stored verbatim, not rejected (plugin newer than
        // daemon).
        //
        // The id is NORMALIZED, not padded: this runs before `cwd` has been
        // resolved to a repo, so the padding width is not yet known, and
        // guessing it would key a width-5 repo's ids wrong. The daemon pads
        // in `zavet::ingest` once it has read the repo's config.
        let ev = parse_guard_event(&serde_json::json!({
            "v": 9, "kind": "guard_hyperdrive", "decision_id": "d-1", "cwd": "/r",
        }))
        .unwrap();
        assert_eq!(ev.kind, "guard_hyperdrive");
        assert_eq!(ev.decision_id, "D-1");
        assert_eq!(
            canonical_decision_id(&ev.decision_id, DEFAULT_ID_WIDTH).as_deref(),
            Some("D-0001")
        );
        assert_eq!(
            canonical_decision_id(&ev.decision_id, 5).as_deref(),
            Some("D-00001")
        );
    }

    #[test]
    fn guard_event_rejects_missing_or_malformed_requireds() {
        for payload in [
            serde_json::json!({}),
            serde_json::json!({"kind": "guard_shown", "cwd": "/r"}), // no decision_id
            serde_json::json!({"kind": "guard_shown", "decision_id": "nope", "cwd": "/r"}),
            serde_json::json!({"kind": "guard_shown", "decision_id": "D-", "cwd": "/r"}),
            serde_json::json!({"kind": "guard_shown", "decision_id": "D-x7", "cwd": "/r"}),
            serde_json::json!({"kind": "", "decision_id": "D-1", "cwd": "/r"}),
            serde_json::json!({"kind": "guard_shown", "decision_id": "D-1"}), // no cwd
            serde_json::json!({"kind": 7, "decision_id": "D-1", "cwd": "/r"}),
        ] {
            assert_eq!(parse_guard_event(&payload), None, "payload: {payload}");
        }
        // A junk ts degrades to receive-time, not rejection.
        let ev = parse_guard_event(&serde_json::json!({
            "kind": "guard_shown", "decision_id": "D-1", "cwd": "/r", "ts": "yesterday-ish",
        }))
        .unwrap();
        assert_eq!(ev.ts, None);
    }

    proptest::proptest! {
        /// The parsers must never panic on arbitrary input — malformed docs
        /// return `None`/empty, byte-offset math stays in bounds.
        #[test]
        fn parse_decision_never_panics(text in ".{0,400}", path in "[a-zA-Z0-9./-]{0,40}") {
            let _ = parse_decision(&text, &path, &ZavetConfig::default());
        }

        #[test]
        fn parse_spec_never_panics(text in ".{0,400}", path in "[a-zA-Z0-9./-]{0,40}") {
            let _ = parse_spec(&text, &path, &ZavetConfig::default());
            let _ = scan_all_decision_refs(&text, &ZavetConfig::default());
        }

        #[test]
        fn trailer_scan_never_panics(s in ".{0,200}") {
            let _ = scan_decision_ref(&s, &ZavetConfig::default());
            let _ = parse_trailer_block(&s);
            let _ = canonical_decision_id(&s, DEFAULT_ID_WIDTH);
            let _ = normalize_decision_id(&s);
        }

        /// The scanner slices `&s[i..j]` off byte offsets, so an arbitrary
        /// prefix set over arbitrary (possibly multi-byte) text must never
        /// land mid-char. Widths and prefixes both vary.
        #[test]
        fn scan_never_panics_on_any_prefix_set(
            s in ".{0,200}",
            prefixes in proptest::collection::vec("[A-Z][A-Z0-9]{0,5}", 1..4),
            width in 1usize..9,
        ) {
            let cfg = ZavetConfig { prefixes, id_width: width };
            let _ = scan_all_decision_refs(&s, &cfg);
        }

        #[test]
        fn parse_config_never_panics(text in ".{0,400}") {
            let cfg = parse_config(&text);
            // The invariant every caller leans on: never an empty prefix set.
            assert!(!cfg.prefixes.is_empty());
            assert!((1..=9).contains(&cfg.id_width));
        }

        /// The guard-event parser must never panic on arbitrary plugin input —
        /// worst case it returns `None` (the shim is fire-and-forget either way).
        #[test]
        fn guard_event_never_panics_on_arbitrary_json(
            kind in proptest::option::of(".*"),
            decision in proptest::option::of(".*"),
            cwd in proptest::option::of(".*"),
            ts in proptest::option::of(".*"),
            extra_key in "[a-z_]{0,12}",
            extra_num in proptest::num::i64::ANY,
        ) {
            let mut obj = serde_json::Map::new();
            if let Some(k) = kind { obj.insert("kind".into(), k.into()); }
            if let Some(d) = decision { obj.insert("decision_id".into(), d.into()); }
            if let Some(c) = cwd { obj.insert("cwd".into(), c.into()); }
            if let Some(t) = ts { obj.insert("ts".into(), t.into()); }
            if !extra_key.is_empty() { obj.insert(extra_key, extra_num.into()); }
            let _ = parse_guard_event(&serde_json::Value::Object(obj));
        }
    }
}
