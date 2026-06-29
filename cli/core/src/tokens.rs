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

/// Bundled, approximate pricing by model family (USD per million tokens). These
/// are estimates for the compute *label* only — see the module docs.
pub fn pricing_for(model: &str) -> ModelPricing {
    let m = model.to_ascii_lowercase();
    if m.contains("opus") {
        ModelPricing {
            input: 15.0,
            output: 75.0,
            cache_read: 1.5,
            cache_write: 18.75,
        }
    } else if m.contains("haiku") {
        ModelPricing {
            input: 0.8,
            output: 4.0,
            cache_read: 0.08,
            cache_write: 1.0,
        }
    } else {
        // sonnet + unknown fallback
        ModelPricing {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
        }
    }
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
        // 1M input @ $15 + 1M output @ $75 = $90.
        assert!((t.est_cost_usd() - 90.0).abs() < 1e-9);
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
        // 1M cache_read @ $1.5 + 1M cache_write @ $18.75 = $20.25.
        assert!((t.est_cost_usd() - 20.25).abs() < 1e-9);
    }
}
