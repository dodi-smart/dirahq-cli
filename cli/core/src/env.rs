//! Process-environment helpers shared across the workspace.

/// The value of `key`, trimmed, or `None` when unset or blank.
///
/// Blank means "not configured" everywhere dira reads an optional env knob —
/// a whitespace-only `DIRA_EXTRA_CA_CERTS` or `DIRA_IDENTITY_EMAIL` must read
/// as absent, not as an empty path or address. One helper so no reader can
/// disagree on that.
pub fn non_blank(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_blank_and_padded_values_normalize() {
        // Process-global env: one test covers all cases to avoid races.
        let key = "DIRA_TEST_NON_BLANK";
        std::env::remove_var(key);
        assert_eq!(non_blank(key), None);
        std::env::set_var(key, "   ");
        assert_eq!(non_blank(key), None, "blank reads as unset");
        std::env::set_var(key, "  value  ");
        assert_eq!(non_blank(key).as_deref(), Some("value"), "trimmed");
        std::env::remove_var(key);
    }
}
