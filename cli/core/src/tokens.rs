//! Token-usage capture from harness transcripts + a bundled cost estimator.
//!
//! Claude Code writes a JSONL transcript per session; each assistant turn carries
//! a `message.usage` block with input/output and cache token counts. We parse
//! those into [`TokenTurn`]s, keyed by the turn's transcript `uuid` so capture is
//! idempotent (re-reading the transcript never double-counts).
//!
//! Cost is **always an estimate** from the bundled per-model pricing table — a
//! label, never a billing base (the contract keeps compute out of money).

use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// One assistant turn's token usage. `id` is the transcript message uuid, used as
/// the idempotency key when persisting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenTurn {
    pub id: String,
    pub at: String,
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_create: u64,
}

impl TokenTurn {
    /// Estimated USD cost for this turn under the bundled pricing table.
    pub fn est_cost_usd(&self) -> f64 {
        let p = pricing_for(&self.model);
        (self.input as f64 * p.input
            + self.output as f64 * p.output
            + self.cache_read as f64 * p.cache_read
            + self.cache_create as f64 * p.cache_write)
            / 1_000_000.0
    }
}

/// Per-million-token USD prices for a model family.
#[derive(Debug, Clone, Copy)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

/// The fallback when the bundled table has no entry — mid-market sonnet shape,
/// so an unknown model still renders a plausible label rather than nothing.
/// Always paired with [`warn_if_unpriced`] so the guess is visible.
const FALLBACK: ModelPricing = ModelPricing {
    input: 3.0,
    output: 15.0,
    cache_read: 0.3,
    cache_write: 3.75,
};

/// Which bundled table key a model id resolves to, or `None` when nothing
/// matched and [`FALLBACK`] is being used.
///
/// Exposed so an unpriced model can be *noticed*: a cost label must always
/// render, but silently pricing an unknown model at fallback rates is
/// indistinguishable from pricing a known one correctly.
pub fn pricing_family(model: &str) -> Option<&'static str> {
    crate::pricing::resolve(model).map(|(k, _)| k)
}

/// Log once per process for each model with no bundled price, so a silent
/// fallback-rate estimate leaves a trace. Advisory only — never affects the
/// value returned by [`pricing_for`].
pub fn warn_if_unpriced(model: &str) {
    if pricing_family(model).is_some() {
        return;
    }
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    let seen = SEEN.get_or_init(Default::default);
    let mut guard = match seen.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.insert(model.to_string()) {
        tracing::info!(
            model = %model,
            "tokens: no bundled pricing for this model — cost estimated at fallback rates"
        );
    }
}

/// Bundled, approximate pricing by model (USD per million tokens). These are
/// estimates for the compute *label* only — see the module docs and
/// [`crate::pricing`] for where the table comes from and why it may drift.
pub fn pricing_for(model: &str) -> ModelPricing {
    crate::pricing::resolve(model)
        .map(|(_, p)| ModelPricing {
            input: p.input,
            output: p.output,
            cache_read: p.cache_read,
            cache_write: p.cache_write,
        })
        .unwrap_or(FALLBACK)
}

/// Parse a Claude Code transcript (JSONL) into per-turn token usage, one record
/// per assistant message that carries a `usage` block. Lines that don't parse,
/// aren't assistant turns, or lack usage are skipped. De-duplicated by uuid.
pub fn parse_transcript_usage(jsonl: &str) -> Vec<TokenTurn> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        let msg = match v.get("message") {
            Some(m) => m,
            None => continue,
        };
        let usage = match msg.get("usage") {
            Some(u) => u,
            None => continue,
        };
        let id = match v.get("uuid").and_then(|u| u.as_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        if !seen.insert(id.clone()) {
            continue;
        }
        let at = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let model = msg
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown")
            .to_string();
        let u = |k: &str| usage.get(k).and_then(|n| n.as_u64()).unwrap_or(0);
        out.push(TokenTurn {
            id,
            at,
            model,
            input: u("input_tokens"),
            output: u("output_tokens"),
            cache_read: u("cache_read_input_tokens"),
            cache_create: u("cache_creation_input_tokens"),
        });
    }
    out
}

/// Parse a grok-build session transcript (`~/.grok/sessions/<encoded-cwd>/<id>/updates.jsonl`,
/// JSONL) into per-turn token usage.
///
/// Each line is an envelope `{"timestamp": <unix_secs>, "method": "...", "params": {...}}`.
/// Only lines where `method == "_x.ai/session/update"` and
/// `params.update.sessionUpdate == "turn_completed"` carry usage; per-turn usage is
/// otherwise absent from the file entirely. `params.update.usage` is itself optional
/// (omitted on some error/cancel paths) — such lines are skipped rather than treated
/// as zero usage. Lines that don't parse, aren't turn-completion records, or lack
/// usage are skipped.
///
/// Idempotency key: `params._meta.eventId` when present, else
/// `grok:{prompt_id}:{envelope timestamp}` built from `update.prompt_id` and the
/// envelope's `timestamp`. A line with neither is skipped — there's no stable key to
/// dedup or upsert by. De-duplicated by that id within a single parse call.
pub fn parse_grok_updates_usage(jsonl: &str) -> Vec<TokenTurn> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("method").and_then(|m| m.as_str()) != Some("_x.ai/session/update") {
            continue;
        }
        let Some(params) = v.get("params") else {
            continue;
        };
        let Some(update) = params.get("update") else {
            continue;
        };
        if update.get("sessionUpdate").and_then(|s| s.as_str()) != Some("turn_completed") {
            continue;
        }
        let Some(usage) = update.get("usage").filter(|u| u.is_object()) else {
            continue; // usage is optional (error/cancel paths) — skip, never zero
        };

        let envelope_ts = v.get("timestamp").and_then(|t| t.as_i64());
        // `_meta` rides inside `params` (the ACP notification), not the envelope.
        let meta = params.get("_meta");
        let event_id = meta
            .and_then(|m| m.get("eventId"))
            .and_then(|e| e.as_str())
            .map(|s| s.to_string());
        let prompt_id = update.get("prompt_id").and_then(|p| p.as_str());
        let id = match (&event_id, prompt_id, envelope_ts) {
            (Some(id), _, _) => id.clone(),
            (None, Some(pid), Some(ts)) => format!("grok:{pid}:{ts}"),
            _ => continue, // no stable key to dedup or upsert by
        };
        if !seen.insert(id.clone()) {
            continue;
        }

        let at = meta
            .and_then(|m| m.get("agentTimestampMs"))
            .and_then(|ms| ms.as_i64())
            .and_then(|ms| OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000).ok())
            .or_else(|| envelope_ts.and_then(|ts| OffsetDateTime::from_unix_timestamp(ts).ok()))
            .and_then(|t| t.format(&Rfc3339).ok())
            .unwrap_or_default();

        let u64_of =
            |obj: &serde_json::Value, k: &str| obj.get(k).and_then(|n| n.as_u64()).unwrap_or(0);
        // TokenTurn.input follows Claude semantics of non-cached input tokens; grok's
        // inputTokens includes cache reads, so we subtract them out here.
        // reasoningTokens are informational only and not separately priced.
        let model_usage = usage
            .get("modelUsage")
            .and_then(|m| m.as_object())
            .filter(|m| !m.is_empty());
        if let Some(model_usage) = model_usage {
            for (model, m) in model_usage {
                let input_tokens = u64_of(m, "inputTokens");
                let cache_read = u64_of(m, "cachedReadTokens");
                out.push(TokenTurn {
                    id: format!("{id}:{model}"),
                    at: at.clone(),
                    model: model.clone(),
                    input: input_tokens.saturating_sub(cache_read),
                    output: u64_of(m, "outputTokens"),
                    cache_read,
                    cache_create: 0,
                });
            }
        } else {
            let input_tokens = u64_of(usage, "inputTokens");
            let cache_read = u64_of(usage, "cachedReadTokens");
            out.push(TokenTurn {
                id,
                at,
                model: "grok".to_string(),
                input: input_tokens.saturating_sub(cache_read),
                output: u64_of(usage, "outputTokens"),
                cache_read,
                cache_create: 0,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
{"type":"user","uuid":"u1","message":{"role":"user"}}
{"type":"assistant","uuid":"a1","timestamp":"2026-06-27T15:18:07.732Z","message":{"model":"claude-opus-4-8","usage":{"input_tokens":2,"output_tokens":1120,"cache_read_input_tokens":344876,"cache_creation_input_tokens":3751}}}
not-json
{"type":"assistant","uuid":"a1","timestamp":"2026-06-27T15:18:09Z","message":{"model":"claude-opus-4-8","usage":{"input_tokens":5,"output_tokens":5}}}
{"type":"assistant","uuid":"a2","timestamp":"2026-06-27T15:20:00Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":1000,"output_tokens":2000}}}
"#;

    #[test]
    fn parses_assistant_usage_dedup_by_uuid() {
        let turns = parse_transcript_usage(SAMPLE);
        assert_eq!(turns.len(), 2, "user/malformed skipped, a1 deduped");
        assert_eq!(turns[0].id, "a1");
        assert_eq!(turns[0].input, 2);
        assert_eq!(turns[0].output, 1120);
        assert_eq!(turns[0].cache_read, 344876);
        assert_eq!(turns[0].cache_create, 3751);
        assert_eq!(turns[1].id, "a2");
        assert_eq!(turns[1].model, "claude-sonnet-4-6");
    }

    #[test]
    fn opus_cost_uses_opus_pricing() {
        let t = TokenTurn {
            id: "x".into(),
            at: "t".into(),
            model: "claude-opus-4-8".into(),
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 0,
            cache_create: 0,
        };
        // 1M input @ $5 + 1M output @ $25 = $30.
        assert!((t.est_cost_usd() - 30.0).abs() < 1e-9);
    }

    /// Generation, not just family, decides the price — and getting this wrong is
    /// not a rounding error. A single "contains opus" rule priced `opus-4-8` at
    /// `opus-4-1`'s rate, i.e. 3× too high, across the bulk of a typical store.
    #[test]
    fn pricing_splits_on_generation_not_just_family() {
        // opus: 4-1 is the expensive legacy generation; 4-5 onward is not.
        assert_eq!(pricing_for("claude-opus-4-1").input, 15.0);
        assert_eq!(pricing_for("claude-opus-4-1-20250805").input, 15.0);
        for m in [
            "claude-opus-4-5",
            "claude-opus-4-6",
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-opus-5",
        ] {
            assert_eq!(pricing_for(m).input, 5.0, "{m}");
            assert_eq!(pricing_for(m).output, 25.0, "{m}");
            assert_eq!(pricing_for(m).cache_read, 0.5, "{m}");
        }

        // haiku: 3-5 is cheaper than 4-5.
        assert_eq!(pricing_for("claude-3-5-haiku").input, 0.8);
        assert_eq!(pricing_for("claude-haiku-4-5-20251001").input, 1.0);

        // sonnet: 5 is cheaper than the 4-x line.
        assert_eq!(pricing_for("claude-sonnet-4-6").input, 3.0);
        assert_eq!(pricing_for("claude-sonnet-5").input, 2.0);
    }

    /// `claude-fable-5` used to match nothing and fall through to the sonnet-shaped
    /// fallback, which under-estimated it by ~3×. It is a real family with its own
    /// price, and it must not be reported as unpriced.
    #[test]
    fn fable_is_priced_and_recognised() {
        let p = pricing_for("claude-fable-5");
        assert_eq!(
            (p.input, p.output, p.cache_read, p.cache_write),
            (10.0, 50.0, 1.0, 12.5)
        );
        assert_eq!(pricing_family("claude-fable-5"), Some("claude-fable-5"));
    }

    #[test]
    fn an_unknown_model_falls_back_and_is_reported_as_unpriced() {
        assert_eq!(pricing_family("some-new-llm-9"), None);
        assert_eq!(pricing_for("some-new-llm-9").input, FALLBACK.input);
        // Everything the bundled table (or an override) covers resolves, so
        // nothing known is ever warned about.
        for m in [
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-haiku-4-5",
            "claude-fable-5",
            "grok",
        ] {
            assert!(pricing_family(m).is_some(), "{m}");
        }
    }

    #[test]
    fn unknown_model_falls_back_to_sonnet_pricing() {
        let p = pricing_for("some-future-model");
        assert_eq!(p.input, 3.0);
        assert_eq!(p.output, 15.0);
    }

    #[test]
    fn parsing_an_appended_tail_yields_only_the_new_turns() {
        // Simulates the daemon's offset-watermark capture: parse the whole file
        // once, then parse only the bytes appended afterward. The tail must yield
        // exactly the new turn, and re-parsing from a byte offset that lands on a
        // line boundary must not lose or duplicate it.
        let head = "{\"type\":\"assistant\",\"uuid\":\"a1\",\"timestamp\":\"2026-06-27T15:18:07Z\",\"message\":{\"model\":\"claude-opus-4-8\",\"usage\":{\"input_tokens\":2,\"output_tokens\":10}}}\n";
        let appended = "{\"type\":\"assistant\",\"uuid\":\"a2\",\"timestamp\":\"2026-06-27T15:20:00Z\",\"message\":{\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":5,\"output_tokens\":20}}}\n";

        let first = parse_transcript_usage(head);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].id, "a1");

        // The watermark advances to the end of `head` (a line boundary); the next
        // capture parses only the appended slice.
        let offset = head.len();
        let full = format!("{head}{appended}");
        let tail = &full[offset..];
        let second = parse_transcript_usage(tail);
        assert_eq!(second.len(), 1, "only the appended turn is parsed");
        assert_eq!(second[0].id, "a2");
        assert_eq!(second[0].input, 5);
        assert_eq!(second[0].output, 20);
    }

    #[test]
    fn cache_tokens_are_priced() {
        let t = TokenTurn {
            id: "x".into(),
            at: "t".into(),
            model: "claude-opus-4-8".into(),
            input: 0,
            output: 0,
            cache_read: 1_000_000,
            cache_create: 1_000_000,
        };
        // 1M cache_read @ $0.5 + 1M cache_write @ $6.25 = $6.75.
        // cache_read dominates real usage (1.24B of 1.32B tokens in one observed
        // store), so this rate is the single biggest lever on the estimate — and
        // it was 3× too high while opus matched a family-wide rule.
        assert!((t.est_cost_usd() - 6.75).abs() < 1e-9);
    }

    const GROK_SAMPLE: &str = r#"
{"timestamp":1753420000,"method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk"}}}
not-json
{"timestamp":1753420100,"method":"_x.ai/session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"turn_completed","prompt_id":"p-1"}}}
{"timestamp":1753420412,"method":"_x.ai/session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"turn_completed","prompt_id":"p-17","agent_result":"Done.","usage":{"inputTokens":15234,"outputTokens":842,"cachedReadTokens":12000,"modelUsage":{"grok-4-fast":{"inputTokens":15234,"outputTokens":842,"cachedReadTokens":12000}}}},"_meta":{"eventId":"ev_9f2a","agentTimestampMs":1753420412873}}}
{"timestamp":1753420500,"method":"_x.ai/session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"turn_completed","prompt_id":"p-18","usage":{"inputTokens":5000,"outputTokens":300,"cachedReadTokens":1000}},"_meta":{"eventId":"ev_abcd"}}}
"#;

    #[test]
    fn parses_grok_turn_completed_usage() {
        let turns = parse_grok_updates_usage(GROK_SAMPLE);
        assert_eq!(
            turns.len(),
            2,
            "plain session/update, malformed line, and no-usage turn are all skipped"
        );

        assert_eq!(turns[0].id, "ev_9f2a:grok-4-fast");
        assert_eq!(turns[0].model, "grok-4-fast");
        assert_eq!(turns[0].input, 15234 - 12000);
        assert_eq!(turns[0].output, 842);
        assert_eq!(turns[0].cache_read, 12000);
        assert_eq!(turns[0].cache_create, 0);

        assert_eq!(turns[1].id, "ev_abcd");
        assert_eq!(
            turns[1].model, "grok",
            "no modelUsage falls back to \"grok\""
        );
        assert_eq!(turns[1].input, 5000 - 1000);
        assert_eq!(turns[1].output, 300);
        assert_eq!(turns[1].cache_read, 1000);
    }

    #[test]
    fn grok_duplicate_event_id_is_deduped() {
        let sample = r#"
{"timestamp":1753420412,"method":"_x.ai/session/update","params":{"update":{"sessionUpdate":"turn_completed","prompt_id":"p-1","usage":{"inputTokens":10,"outputTokens":5}},"_meta":{"eventId":"ev_dup"}}}
{"timestamp":1753420500,"method":"_x.ai/session/update","params":{"update":{"sessionUpdate":"turn_completed","prompt_id":"p-1","usage":{"inputTokens":99,"outputTokens":99}},"_meta":{"eventId":"ev_dup"}}}
"#;
        let turns = parse_grok_updates_usage(sample);
        assert_eq!(turns.len(), 1, "same eventId is deduped within the parse");
        assert_eq!(turns[0].input, 10);
    }

    #[test]
    fn grok_missing_meta_falls_back_to_prompt_id_and_timestamp() {
        let sample = r#"
{"timestamp":1700000000,"method":"_x.ai/session/update","params":{"update":{"sessionUpdate":"turn_completed","prompt_id":"p-99","usage":{"inputTokens":100,"outputTokens":50}}}}
"#;
        let turns = parse_grok_updates_usage(sample);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].id, "grok:p-99:1700000000");
        let want_at = OffsetDateTime::from_unix_timestamp(1_700_000_000)
            .unwrap()
            .format(&Rfc3339)
            .unwrap();
        assert_eq!(turns[0].at, want_at);
    }

    /// `parse_grok_updates_usage` synthesizes the bare label `grok` when a turn
    /// carries no per-model breakdown. That string is dira's own invention and
    /// models.dev has never heard of it, so it lives in `overrides.json` — the
    /// case the override layer exists for.
    #[test]
    fn the_synthesized_grok_label_is_priced_by_the_override() {
        let p = pricing_for("grok");
        assert_eq!(p.input, 3.0);
        assert_eq!(p.output, 15.0);
        assert_eq!(p.cache_read, 0.75);
        assert_eq!(
            pricing_family("grok"),
            Some("grok"),
            "the override must resolve, not fall through to the generic fallback"
        );

        let t = TokenTurn {
            id: "x".into(),
            at: "t".into(),
            model: "grok".into(),
            input: 0,
            output: 0,
            cache_read: 1_000_000,
            cache_create: 0,
        };
        // 1M cache_read @ $0.75 = $0.75.
        assert!((t.est_cost_usd() - 0.75).abs() < 1e-9);
    }
}
