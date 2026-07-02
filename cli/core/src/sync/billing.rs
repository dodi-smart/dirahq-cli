//! Cloud billing-summary fetch: response types + cache for the `dira status`
//! billable footer.
//!
//! Response-only and **unsigned** — like [`super::handshake`], this is NOT part
//! of the signed request contract (`dira_contract` stays policy-free: no money,
//! rate, or currency on the wire *request*). The daemon POSTs a signed
//! [`dira_contract::BillingSummaryEnvelope`] to `/api/v1/billing/summary` and
//! parses this tolerant shape out of the 2xx body. Every field defaults, so an
//! older/newer cloud degrades to "no summary" instead of erroring.

use serde::{Deserialize, Serialize};

/// The cloud's billable rollup for one period, as computed by the cloud's
/// billing policy (late-bound: rates, rounding, and assurance live there).
/// Numbers are raw — the CLI owns formatting.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BillingSummary {
    /// Engaged hours of billable intervals in the period (raw, not rounded).
    #[serde(default)]
    pub billable_hours: f64,
    /// Policy-priced value of those intervals (rounding/minimums applied).
    #[serde(default)]
    pub unbilled_amount: f64,
    /// Currency symbol from the workspace policy, e.g. `"€"`.
    #[serde(default)]
    pub currency: String,
    /// The period this covers, e.g. `"week"` (rolling 7 days).
    #[serde(default)]
    pub period: String,
    /// RFC 3339 start/end of the period window.
    #[serde(default)]
    pub period_start: String,
    #[serde(default)]
    pub period_end: String,
    /// RFC 3339 timestamp the cloud computed this at.
    #[serde(default)]
    pub as_of: String,
}

/// The full 2xx body: `{ "status": "ok", "summary": { … } }`.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BillingSummaryResponse {
    #[serde(default)]
    status: String,
    #[serde(default)]
    summary: Option<BillingSummary>,
}

/// Parse a 2xx billing-summary response body. Returns `Some` only for a
/// well-formed `status: "ok"` body carrying a summary; anything else (empty
/// body, old cloud, unexpected shape) is `None` — the caller keeps its cache.
pub fn parse_billing_summary_response(body: &str) -> Option<BillingSummary> {
    let resp: BillingSummaryResponse = serde_json::from_str(body).ok()?;
    if resp.status == "ok" {
        resp.summary
    } else {
        None
    }
}

/// A fetched summary plus when *this device* fetched it, persisted under
/// [`META_BILLING_SUMMARY`] so a restarted (or offline) daemon still serves the
/// last-known value to `dira status`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CachedBillingSummary {
    pub summary: BillingSummary,
    /// RFC 3339 timestamp of the successful fetch (device clock).
    pub fetched_at: String,
}

/// `meta` key: the JSON-serialized [`CachedBillingSummary`] from the last
/// successful fetch. Never cleared on fetch failure — stale beats absent.
pub const META_BILLING_SUMMARY: &str = "billing_summary_cache";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ok_body_with_summary() {
        let s = parse_billing_summary_response(
            r#"{"status":"ok","summary":{"billableHours":10.4,"unbilledAmount":1064,
                "currency":"€","period":"week","periodStart":"a","periodEnd":"b","asOf":"c"}}"#,
        )
        .expect("summary parsed");
        assert_eq!(s.billable_hours, 10.4);
        assert_eq!(s.unbilled_amount, 1064.0);
        assert_eq!(s.currency, "€");
        assert_eq!(s.period, "week");
    }

    #[test]
    fn tolerates_missing_fields_in_summary() {
        // A newer/older cloud may omit fields — they default instead of erroring.
        let s = parse_billing_summary_response(r#"{"status":"ok","summary":{}}"#)
            .expect("empty summary still parses");
        assert_eq!(s.billable_hours, 0.0);
        assert!(s.currency.is_empty());
    }

    #[test]
    fn rejects_non_ok_and_malformed_bodies() {
        assert!(parse_billing_summary_response("").is_none());
        assert!(parse_billing_summary_response("not json").is_none());
        assert!(parse_billing_summary_response(r#"{"status":"error"}"#).is_none());
        // ok but no summary block — nothing to cache.
        assert!(parse_billing_summary_response(r#"{"status":"ok"}"#).is_none());
    }

    #[test]
    fn cached_summary_roundtrips_json() {
        let cached = CachedBillingSummary {
            summary: BillingSummary {
                billable_hours: 1.5,
                unbilled_amount: 150.0,
                currency: "€".into(),
                period: "week".into(),
                ..Default::default()
            },
            fetched_at: "2026-07-02T09:00:00Z".into(),
        };
        let json = serde_json::to_string(&cached).unwrap();
        let back: CachedBillingSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.summary.unbilled_amount, 150.0);
        assert_eq!(back.fetched_at, "2026-07-02T09:00:00Z");
    }
}
