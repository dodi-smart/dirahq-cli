//! Regenerate the bundled model-price table from the models.dev catalog.
//!
//! Reads the raw catalog on **stdin** and writes the normalized table to
//! **stdout**, so the fetch stays in the caller (`just pricing-sync`) and this
//! binary needs no HTTP client — the same shape as `sign_vector`.
//!
//!     curl -fsSL https://models.dev/api.json -o catalog.json
//!     cargo run -q -p dira-core --bin pricing_sync -- cli/core/pricing/models.json \
//!       < catalog.json > models.json.new
//!
//! Fetch to a file, never straight down a pipe into `cargo run`: cargo spends
//! minutes compiling before it reads a byte, the pipe buffer fills, and the
//! server drops the connection it has been holding open for nothing. That is
//! the 2026-09-02 outage, and `just pricing-sync` is the invocation to copy.
//!
//! Only the providers behind the harnesses dira tracks are kept: the full
//! catalog carries the same model under dozens of resellers at *different*
//! prices. Keys are canonicalized with the very same
//! [`dira_core::pricing::normalize_model`] the runtime resolver applies to
//! observed model strings, so wrapper forms collapse onto one bare id instead
//! of shipping as near-duplicates — sharing that function is the point, since a
//! second copy of the normalization would drift from the lookups silently.
//!
//! Exits non-zero if the payload doesn't look like the catalog, so a truncated
//! or error response can never overwrite a good table with a bad one.
//!
//! Takes an optional positional argument: the path to the table this refresh
//! is replacing. With it, the refresh is append-only — a key upstream still
//! publishes gets the fresh price, a key upstream has quietly dropped keeps
//! its last-known price instead of vanishing. That matters because the
//! cloud's counterpart re-prices historical `token_usage` rows against this
//! table, and a dropped key leaves that resolve cascade with no family key to
//! fall back to: the rows become permanently unpriceable. Deliberately
//! removing a bad entry stays an explicit `null` in `overrides.json`, not a
//! side effect of upstream silently unpublishing it. Without the argument the
//! binary does a clean regenerate exactly as before, so it stays usable from
//! scratch (first run, tests) without carrying merge logic along for the ride.
//!
//! Deliberately mirrors the cloud's `scripts/pricing-sync.ts`. The two tables
//! are allowed to drift between refreshes: the cloud is authoritative and
//! re-prices historical rows, while this copy only labels local views.

use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::io::Read;

/// models.dev provider ids to vendor, in precedence order — when two publish the
/// same canonical id, the earlier wins. One entry per supported harness family:
/// anthropic → Claude Code, openai → Codex, google → Gemini CLI, xai → grok,
/// opencode → OpenCode's zen gateway.
const PROVIDERS: &[&str] = &["anthropic", "openai", "google", "xai", "opencode"];

fn num(v: Option<&Value>) -> Option<f64> {
    match v.and_then(Value::as_f64) {
        Some(n) if n.is_finite() && n >= 0.0 => Some(n),
        _ => None,
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The previous table, when given, is what makes this an append-only
    // refresh instead of a clean regenerate — see the module doc. Missing or
    // unreadable is a hard error, never a silent fall-through to a clean
    // regenerate: a typo'd path would otherwise drop every retained key
    // without so much as a warning, exactly the bug this binary exists to fix.
    let existing_path = std::env::args().nth(1);
    let existing_models: Map<String, Value> = match &existing_path {
        None => Map::new(),
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| format!("reading existing table at {path}: {e}"))?;
            let parsed: Value = serde_json::from_str(&raw)
                .map_err(|e| format!("parsing existing table at {path}: {e}"))?;
            parsed
                .get("models")
                .and_then(Value::as_object)
                .cloned()
                .ok_or_else(|| format!("existing table at {path} has no \"models\" object"))?
        }
    };

    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    let catalog: Map<String, Value> = serde_json::from_str(&raw)?;

    // A catalog this small means a truncated or error payload, not a real
    // shrink — bail rather than publish it. This gate matters more now than
    // it used to: with retention, a partial catalog can no longer be caught
    // downstream by "the table shrank" — missing keys just quietly keep the
    // previous refresh's price instead. Without this check that failure mode is silent
    // instead of loud, which is the one thing this binary must never be.
    if catalog.len() < 10 {
        return Err(format!("catalog looks truncated: {} providers", catalog.len()).into());
    }
    let missing: Vec<&str> = PROVIDERS
        .iter()
        .copied()
        .filter(|p| !catalog.contains_key(*p))
        .collect();
    if !missing.is_empty() {
        return Err(format!("providers missing from catalog: {}", missing.join(", ")).into());
    }

    let mut models: BTreeMap<String, Value> = BTreeMap::new();
    let mut claimed: BTreeMap<String, &str> = BTreeMap::new();
    let (mut considered, mut collisions) = (0usize, 0usize);

    for pid in PROVIDERS {
        let Some(list) = catalog
            .get(*pid)
            .and_then(|p| p.get("models"))
            .and_then(Value::as_object)
        else {
            continue;
        };
        for (mid, m) in list {
            let cost = m.get("cost");
            let (Some(input), Some(output)) = (
                num(cost.and_then(|c| c.get("input"))),
                num(cost.and_then(|c| c.get("output"))),
            ) else {
                continue;
            };
            // Subscription-plan providers report 0/0 because inference is
            // bundled in the plan. That is "no price data", not "free" — letting
            // it claim an id would zero out every estimate for that model.
            if input == 0.0 && output == 0.0 {
                continue;
            }
            // Keep only models a coding harness can actually drive. Tool calling
            // IS the interaction model for every harness dira supports, so a
            // model without it can never produce a turn we capture: embeddings,
            // image/TTS/live models, and pre-tool chat models like
            // gpt-3.5-turbo. Derived from the catalog rather than a hand list,
            // so it stays right as models come and go.
            if m.get("tool_call").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            considered += 1;

            let key = dira_core::pricing::normalize_model(mid);
            let mut entry = Map::new();
            entry.insert("input".into(), input.into());
            entry.insert("output".into(), output.into());
            if let Some(r) = num(cost.and_then(|c| c.get("cache_read"))) {
                entry.insert("cacheRead".into(), r.into());
            }
            if let Some(w) = num(cost.and_then(|c| c.get("cache_write"))) {
                entry.insert("cacheWrite".into(), w.into());
            }

            match models.get(&key) {
                None => {
                    models.insert(key.clone(), Value::Object(entry));
                    claimed.insert(key, pid);
                }
                // Only report collisions that would have changed the price —
                // those are the ones worth eyeballing in the refresh PR.
                Some(existing) => {
                    if existing.get("input") != entry.get("input")
                        || existing.get("output") != entry.get("output")
                    {
                        collisions += 1;
                        eprintln!(
                            "collision: {key} kept {} , dropped {pid}",
                            claimed.get(&key).copied().unwrap_or("?")
                        );
                    }
                }
            }
        }
    }

    // Append-only merge: every key still standing in `models` at this point
    // came from the fresh sync above and already has this run's price, so it
    // must win outright. A key only the previous table knows about is one
    // upstream has quietly stopped publishing — fill the gap with its
    // last-known entry instead of letting it disappear. This has to run after
    // the provider loop (so freshness always wins) and before the dated-alias
    // prune below (so a retained key is pruned by the same rule a fresh one
    // would be, not exempted from it).
    let mut retained: Vec<String> = Vec::new();
    for (key, entry) in &existing_models {
        if !models.contains_key(key) {
            models.insert(key.clone(), entry.clone());
            retained.push(key.clone());
        }
    }

    // Drop dated aliases whose undated form is already present at the same
    // price: the resolver's cascade strips `-20251001` before looking up, so the
    // entry can never be reached and only bloats the bundle. Kept when the
    // prices differ — then the pin is load-bearing and the strip would be wrong.
    let redundant: Vec<String> = models
        .keys()
        .filter(|k| {
            let base = dira_core::pricing::strip_release(k);
            base != **k && models.get(&base).is_some_and(|b| b == &models[*k])
        })
        .cloned()
        .collect();
    for k in &redundant {
        models.remove(k);
    }
    // A retained key can itself get pruned here (its undated form showed up
    // fresh at the same price this run) — drop it from the report too, since
    // it no longer needs a reviewer's eyes as a surviving gap-fill.
    retained.retain(|k| models.contains_key(k));
    retained.sort();

    eprintln!(
        "pricing sync: {} models from {considered} tool-calling, cost-bearing entries across {}, \
         {collisions} price collisions resolved by provider precedence, \
         {} redundant dated aliases dropped, \
         {} keys retained because upstream no longer publishes them",
        models.len(),
        PROVIDERS.join(", "),
        redundant.len(),
        retained.len()
    );
    if !retained.is_empty() {
        // One per line rather than joined into the summary above — a wide
        // comma list is easy to skim past, and this is exactly the list a
        // refresh PR reviewer needs to actually look at.
        eprintln!("retained (upstream dropped these ids):");
        for key in &retained {
            eprintln!("  {key}");
        }
    }

    let mut out = Map::new();
    out.insert(
        "$comment".into(),
        "Generated by `just pricing-sync` from https://models.dev/api.json — do not hand-edit; \
         corrections go in overrides.json. Prices are USD per 1M tokens. Scope: tool-calling, \
         cost-bearing models from the providers behind the supported harnesses, minus dated \
         aliases the resolver already reaches by stripping the pin. Append-only: a refresh \
         updates the price of any key upstream still publishes but never drops a key upstream \
         stops publishing, so the cloud's re-pricing cascade always has a key to resolve \
         historical usage against. To actually remove an entry, add an explicit null override \
         in overrides.json — that is the only supported way a key leaves this table. The cloud \
         keeps its own copy and is authoritative; this one only labels local views and may \
         drift between weekly refreshes."
            .into(),
    );
    out.insert("providers".into(), PROVIDERS.into());
    out.insert("models".into(), Value::Object(models.into_iter().collect()));
    // No `generatedAt`: it would churn the file on every run and open an empty
    // PR each month even when no price moved.
    println!("{}", serde_json::to_string_pretty(&Value::Object(out))?);
    Ok(())
}
