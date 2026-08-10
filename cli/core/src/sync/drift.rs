//! Reporting for cloud response bodies the current contract can't read.
//!
//! Every `parse_*_response` in this module is deliberately tolerant: a body we
//! can't read degrades to defaults rather than failing the flush, because the
//! daemon must keep working against an older or slightly-off cloud. That
//! tolerance is only safe if it is *audible*. A silently zeroed response is
//! indistinguishable from a cloud that genuinely acked nothing — which is how
//! `unwrap_or_default()` turned contract drift into a mystery (#104).
//!
//! So the parsers return the `serde_json::Error` and each caller decides. On a
//! 2xx body, "unreadable" always means drift and gets [`warn_unreadable_body`].
//! On an *error* body it usually doesn't — a proxy's HTML 502 is a perfectly
//! ordinary thing to fail to parse — so those sites fall back quietly and let
//! the surrounding error carry the raw body.
//!
//! What is logged here is a *cloud response* (D-0001 metadata: statuses, ids,
//! counters, epochs), never anything content-bearing, and it never leaves the
//! device — this is the daemon's own log, not the wire.

/// Cap a response body for logging, never splitting a multi-byte character.
/// Bodies here are metadata-only and tiny; one big enough to need this is
/// already off-contract, so a head is all we want.
pub fn body_head(body: &str) -> String {
    const MAX: usize = 200;
    let trimmed = body.trim();
    let mut chars = trimmed.chars();
    let head: String = chars.by_ref().take(MAX).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Warn that a **success** body didn't parse and the caller is falling back to
/// defaults. `what` names the response so the line is greppable and every call
/// site reads identically (`presence_ack`, `ingest_response`, …).
pub fn warn_unreadable_body(what: &str, err: &serde_json::Error, body: &str) {
    tracing::warn!(
        response = what,
        error = %err,
        body = %body_head(body),
        "cloud response did not parse — falling back to defaults (contract drift?)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cap is a char count, not a byte count — a body of multi-byte
    /// characters must not panic or produce a partial code point.
    #[test]
    fn body_head_truncates_on_a_char_boundary() {
        assert_eq!(body_head("  {\"a\":1}  "), "{\"a\":1}");
        let long = "é".repeat(500);
        let head = body_head(&long);
        assert!(head.ends_with('…'));
        assert_eq!(head.chars().count(), 201);
    }

    /// A body that fits is returned trimmed and whole, with no ellipsis.
    #[test]
    fn body_head_leaves_a_short_body_alone() {
        let head = body_head("<html>502 Bad Gateway</html>");
        assert_eq!(head, "<html>502 Bad Gateway</html>");
        assert!(!head.ends_with('…'));
    }
}
