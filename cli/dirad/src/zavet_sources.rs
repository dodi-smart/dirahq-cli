//! Daemon glue for source-backed specs (OpenSpec, Spec Kit, BMAD, superpowers).
//!
//! The adapters live once, in the zavet plugin behind `zavet specs --json`;
//! [`dira_core::zavet_specs`] runs it (plugin-installed binary only, 5 s
//! timeout, fail-open). This module turns what it returns into the SAME
//! `zavet_specs` rows native specs use and keeps them current:
//!
//! - **Upsert** each unit as a spec — slug `<kind>:<id>`, origin `external`,
//!   `verified` permanently false, `path` = the unit's primary file.
//! - **Removed, never deleted.** A source slug seen before and absent from a
//!   fresh, successful resolve becomes state `removed`. A *failed* resolve
//!   changes nothing: absence of evidence is not a removal.
//! - **No churn.** A unit whose files, path, coverage and metadata match the
//!   stored row writes nothing, so the knowledge cursor (`touched_seq`) does not
//!   move and nothing is re-pushed (DIRASH-0028's skip, applied here).
//! - **Session attribution.** [`attribute_session`] binds a hook session to a
//!   spec from facts only: an exact `Spec:` trailer, else an exact branch
//!   match, else nothing.

use dira_contract::SpecSource;
use dira_core::store::{SourcesWritten, Store, ZavetSpecCapture, ZavetSpecRow, ZavetSpecWrite};
use dira_core::zavet_specs::ResolvedUnit;
use std::collections::BTreeSet;
use std::path::Path;

/// Cap on a source unit's composed `body_md` (characters), applied when the
/// full-tier text is filled in. Native spec bodies have no local cap; this
/// one keeps a verbose framework (a long proposal + design + tasks) from
/// making a single knowledge item unbounded.
pub const SOURCE_BODY_CAP_CHARS: usize = 64 * 1024;

/// Resolve the repo's source units via the installed plugin, failing open:
/// any error is logged at debug and yields `None` ("skip source specs this
/// sweep"). Blocking — call from `spawn_blocking`.
pub fn resolve_blocking(root: &Path, with_text: bool) -> Option<Vec<ResolvedUnit>> {
    match dira_core::zavet_specs::resolve_units_system(root, with_text) {
        Ok(units) => Some(units),
        Err(e) => {
            tracing::debug!(repo = %root.display(), "source specs skipped this sweep: {e}");
            None
        }
    }
}

/// Resolve the repo's framework rule sources via the installed plugin, failing
/// open exactly like [`resolve_blocking`]: `None` means "skip rule sources this
/// sweep" (no plugin, a plugin without `spec_sources_json`, a timeout, a
/// non-zero exit). Blocking — call from `spawn_blocking`.
pub fn resolve_rules_blocking(root: &Path) -> Option<Vec<dira_core::zavet_specs::RuleSource>> {
    match dira_core::zavet_specs::resolve_rule_sources_system(root) {
        Ok(rules) => Some(rules),
        Err(e) => {
            tracing::debug!(repo = %root.display(), "rule sources skipped this sweep: {e}");
            None
        }
    }
}

/// The spec-row input for a resolved unit.
pub fn spec_capture(unit: &ResolvedUnit) -> ZavetSpecCapture {
    ZavetSpecCapture {
        slug: unit.slug.clone(),
        title: unit.title.clone(),
        version: 1,
        origin: "external".into(),
        // Permanently false: the framework owns the claim, a human confirms it
        // in the framework, not here (design §8).
        verified: Some(false),
        confidence: "low".into(),
        date: None,
        paths: unit.paths.clone(),
        decisions: Vec::new(),
        checks: Vec::new(),
        path: unit.primary.clone(),
        body_md: None,
        content_hash: Some(unit.record_sha.clone()),
        source: Some(unit.source.clone()),
    }
}

/// Rebuild a capture from a stored source row, with its state forced to
/// `removed`. Everything else is kept as last observed.
fn removed_capture(row: &ZavetSpecRow) -> Option<ZavetSpecCapture> {
    let mut source = row.source.clone()?;
    source.state = "removed".into();
    Some(ZavetSpecCapture {
        slug: row.slug.clone(),
        title: row.title.clone(),
        version: row.version,
        origin: row.origin.clone().unwrap_or_else(|| "external".into()),
        verified: Some(false),
        confidence: row.confidence.clone().unwrap_or_else(|| "low".into()),
        date: row.date.clone(),
        paths: row.paths.clone(),
        decisions: row.decisions.clone(),
        checks: Vec::new(),
        path: row.path.clone(),
        body_md: None,
        content_hash: row.content_hash.clone(),
        source: Some(source),
    })
}

/// Whether the stored row already says everything the fresh unit says.
fn unchanged(row: &ZavetSpecRow, cap: &ZavetSpecCapture) -> bool {
    let Some(stored) = row.source.as_ref() else {
        return false; // a native spec squatting the slug: let the upsert rewrite it
    };
    let Some(fresh) = cap.source.as_ref() else {
        return false;
    };
    // `state_changed_at` is stamped by the store on a transition, never sent
    // by the resolver, so it must not take part in the comparison.
    let mut fresh = fresh.clone();
    fresh.state_changed_at = stored.state_changed_at.clone();
    let mut a = row.paths.clone();
    let mut b = cap.paths.clone();
    a.sort();
    b.sort();
    row.content_hash == cap.content_hash
        && row.path == cap.path
        && row.title == cap.title
        && a == b
        && *stored == fresh
}

/// Write a fresh, successful resolve into the store and, when `allow_removals`,
/// mark vanished units `removed`. `session` is the unambiguous live session, if
/// any (provenance of the sighting, like any other capture).
///
/// `allow_removals` is false for a resolve that cannot speak for the repo's
/// source units: a checkout that is not on the repo's default branch (the store
/// is keyed by canonical repo but a resolve sees ONE checkout, so a feature
/// branch that predates a unit would flap it removed/active), or an empty
/// answer whose declared roots still exist on disk (DIRASH-0039). Seen units
/// are still upserted either way.
pub async fn persist_units(
    store: &Store,
    repo: &str,
    units: &[ResolvedUnit],
    session: Option<&str>,
    allow_removals: bool,
) -> Result<SourcesWritten, dira_core::Error> {
    let stored = store.zavet_specs_list(repo).await?;
    let by_slug: std::collections::HashMap<&str, &ZavetSpecRow> = stored
        .iter()
        .filter(|r| r.source.is_some())
        .map(|r| (r.slug.as_str(), r))
        .collect();
    let mut out = SourcesWritten::default();

    // Collected first, written in ONE transaction below: a sweep costs one
    // commit instead of one per unit, and lands whole or not at all.
    let mut pending: Vec<(ZavetSpecCapture, &str, Option<&str>)> = Vec::new();
    for unit in units {
        let cap = spec_capture(unit);
        let prev = by_slug.get(unit.slug.as_str()).copied();
        if prev.is_some_and(|r| unchanged(r, &cap)) {
            continue;
        }
        // First sight lands at the unit's introducing commit, then the latest
        // sighting overwrites the living fields — the store preserves
        // first_commit/created_at on conflict (the same oldest-then-newest
        // order `dira zavet reindex` uses). A unit with no commit at all passes
        // "" which the store records as NULL (never as an empty-string origin)
        // and upgrades when a real commit is first seen.
        let last = unit
            .last_commit
            .as_deref()
            .or(unit.first_commit.as_deref())
            .unwrap_or("");
        // Also when the stored row has no origin yet (a unit first seen without
        // commits): the store fills the first-sight triple from this one.
        if prev.is_none_or(|r| r.first_commit.as_deref().is_none_or(str::is_empty)) {
            if let Some(first) = unit
                .first_commit
                .as_deref()
                .filter(|f| !f.is_empty() && *f != last)
            {
                pending.push((cap.clone(), first, unit.first_at.as_deref()));
            }
        }
        pending.push((cap, last, unit.last_at.as_deref()));
        out.upserted += 1;
    }

    let live: BTreeSet<&str> = units.iter().map(|u| u.slug.as_str()).collect();
    let removed: Vec<(ZavetSpecCapture, &ZavetSpecRow)> = if allow_removals {
        stored
            .iter()
            .filter(|r| {
                r.source
                    .as_ref()
                    .is_some_and(|s| s.state != "removed" && !live.contains(r.slug.as_str()))
            })
            .filter_map(|row| removed_capture(row).map(|cap| (cap, row)))
            .collect()
    } else {
        Vec::new()
    };
    out.removed = removed.len();
    let writes: Vec<ZavetSpecWrite<'_>> = pending
        .iter()
        .map(|(cap, commit_sha, authored_at)| ZavetSpecWrite {
            cap,
            commit_sha,
            authored_at: *authored_at,
        })
        .chain(removed.iter().map(|(cap, row)| ZavetSpecWrite {
            cap,
            commit_sha: row.last_commit.as_deref().unwrap_or(""),
            authored_at: row.updated_at.as_deref(),
        }))
        .collect();
    store.zavet_upsert_specs(repo, session, &writes).await?;
    Ok(out)
}

/// The slugs a NEW session may be bound to: [`known_slugs`] minus source units
/// that are `removed` or `unresolved` (a closed or unreadable unit is not what
/// a session is working on).
pub async fn bindable_slugs(store: &Store, repo: &str) -> BTreeSet<String> {
    store
        .zavet_spec_slugs(repo)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, state)| !matches!(state.as_deref(), Some("removed" | "unresolved")))
        .map(|(slug, _)| slug)
        .collect()
}

/// The slugs of every captured spec for a repo (native and source) — the set a
/// `Spec:` trailer value must match EXACTLY to count as a reference.
pub async fn known_slugs(store: &Store, repo: &str) -> BTreeSet<String> {
    store
        .zavet_spec_slugs(repo)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(slug, _)| slug)
        .collect()
}

/// Resolve and store the spec a captured session works under, from facts only
/// (see [`dira_core::zavet_specs::resolve_session_spec`]): exact `Spec:`
/// trailer of the walked commits (newest first), else exact branch match, else
/// none. A session that already has a spec (explicit `dira start --spec`, or a
/// previous resolution) is never overridden. Returns the slug when one was
/// newly bound.
///
/// Best-effort: a session already ended in the registry is skipped (its rollup
/// has been emitted, so a late binding could not reach the cloud and would only
/// make the local row disagree with what was shipped).
pub async fn attribute_session(
    store: &Store,
    sessions: &std::sync::Mutex<crate::state::SessionRegistry>,
    session_id: &str,
    repo: &str,
    commit_spec_values: &[Vec<String>],
    branch: Option<&str>,
) -> Option<String> {
    // Nothing to match on: no `Spec:` trailer in the walked commits and no
    // branch. Settle that before touching the registry or the database.
    if commit_spec_values.iter().all(Vec::is_empty) && branch.is_none_or(|b| b.trim().is_empty()) {
        return None;
    }
    {
        let reg = crate::control::lock_recover(sessions);
        if reg.spec_of(session_id).is_some() || reg.is_ended(session_id) {
            return None;
        }
    }
    let known = bindable_slugs(store, repo).await;
    if known.is_empty() {
        return None;
    }
    let slug = dira_core::zavet_specs::resolve_session_spec(commit_spec_values, branch, &known)?;
    if let Err(e) = store.session_spec_set(session_id, &slug).await {
        tracing::warn!("session spec attribution failed for {session_id}: {e}");
        return None;
    }
    crate::control::lock_recover(sessions).set_spec_if_none(session_id, &slug);
    tracing::info!(session = %session_id, spec = %slug, "attributed session to spec");
    Some(slug)
}

/// Compose a source unit's primary documents into one markdown body, capped at
/// [`SOURCE_BODY_CAP_CHARS`] on a char boundary. Empty input yields `None`.
pub fn compose_body(docs: &[(String, String)]) -> Option<String> {
    if docs.is_empty() {
        return None;
    }
    let mut out = String::new();
    for (i, (path, md)) in docs.iter().enumerate() {
        if i > 0 {
            out.push_str("\n\n---\n\n");
        }
        out.push_str(&format!("<!-- {path} -->\n\n"));
        out.push_str(md.trim());
    }
    if out.chars().count() > SOURCE_BODY_CAP_CHARS {
        out = out.chars().take(SOURCE_BODY_CAP_CHARS).collect();
    }
    Some(out)
}

/// Copy the full-tier fields of `text` onto the wire `source` (metadata fields
/// stay as the store has them).
pub fn overlay_text(dst: &mut SpecSource, text: &SpecSource) {
    dst.requirement_bodies = text.requirement_bodies.clone();
    dst.task_bodies = text.task_bodies.clone();
    dst.summary_md = text.summary_md.clone();
    dst.tracker_refs = text.tracker_refs.clone();
}
