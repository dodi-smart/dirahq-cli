//! The one form a timestamp takes in a TEXT time column of the local store.
//!
//! `events.at`, `token_usage.at` and `artifacts.authored_at` are TEXT, and
//! SQLite compares and orders them as strings. `time`'s `Rfc3339` formatter
//! trims trailing fractional zeros, so `…:05.12Z` sorted AFTER `…:05.123Z` and
//! events from the same second replayed out of order. Every value is therefore
//! stored as fixed-width UTC (`YYYY-MM-DDTHH:MM:SS.fffffffffZ`, nine fractional
//! digits), where text order is time order and the `at` indexes stay usable.
//!
//! Every bind against one of those columns goes through [`StoredAt`] (a parsed
//! instant) or [`RawAt`] (text a harness or git wrote), never a hand-formatted
//! string. See DIRASH-0043.

use sqlx::encode::IsNull;
use sqlx::error::BoxDynError;
use sqlx::sqlite::{Sqlite, SqliteArgumentsBuffer, SqliteTypeInfo, SqliteValueRef};
use sqlx::{Decode, Encode, Type};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

const FORMAT: &[time::format_description::BorrowedFormatItem<'static>] = time::macros::format_description!(
    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:9]Z"
);

/// An instant bound to, or read from, a store time column in the canonical
/// fixed-width UTC form. Decoding accepts any RFC 3339 text, so rows written
/// before the canonical form still read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredAt(pub OffsetDateTime);

impl StoredAt {
    /// The canonical stored text for this instant.
    pub fn text(self) -> Result<String, time::error::Format> {
        self.0.to_offset(UtcOffset::UTC).format(FORMAT)
    }

    /// The canonical stored text for `raw`, or `None` when `raw` is not RFC 3339.
    pub fn canonical(raw: &str) -> Option<String> {
        let t = OffsetDateTime::parse(raw, &Rfc3339).ok()?;
        StoredAt(t).text().ok()
    }
}

impl Type<Sqlite> for StoredAt {
    fn type_info() -> SqliteTypeInfo {
        <String as Type<Sqlite>>::type_info()
    }
}

impl Encode<'_, Sqlite> for StoredAt {
    fn encode_by_ref(&self, args: &mut SqliteArgumentsBuffer) -> Result<IsNull, BoxDynError> {
        <String as Encode<Sqlite>>::encode(self.text()?, args)
    }
}

impl<'r> Decode<'r, Sqlite> for StoredAt {
    fn decode(value: SqliteValueRef<'r>) -> Result<Self, BoxDynError> {
        let s = <&str as Decode<Sqlite>>::decode(value)?;
        Ok(StoredAt(OffsetDateTime::parse(s, &Rfc3339)?))
    }
}

/// Timestamp text from outside dira (a transcript line, `git log %aI`) bound to
/// a store time column. RFC 3339 text is stored canonical. Anything else (a
/// transcript line with no timestamp yields `""`) is stored verbatim, because
/// the row itself must not be lost over its date (D-0018).
#[derive(Debug, Clone, Copy)]
pub struct RawAt<'a>(pub &'a str);

impl Type<Sqlite> for RawAt<'_> {
    fn type_info() -> SqliteTypeInfo {
        <String as Type<Sqlite>>::type_info()
    }
}

impl Encode<'_, Sqlite> for RawAt<'_> {
    fn encode_by_ref(&self, args: &mut SqliteArgumentsBuffer) -> Result<IsNull, BoxDynError> {
        let s = StoredAt::canonical(self.0).unwrap_or_else(|| self.0.to_string());
        <String as Encode<Sqlite>>::encode(s, args)
    }
}

/// Stored time-column text as the wire and the UI show it: plain RFC 3339
/// (`time`'s trimmed form, UTC). Non-RFC 3339 text passes through verbatim.
pub fn display_at(stored: String) -> String {
    match OffsetDateTime::parse(&stored, &Rfc3339) {
        Ok(t) => t.format(&Rfc3339).unwrap_or(stored),
        Err(_) => stored,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_is_fixed_width_utc_and_text_order_is_time_order() {
        let cases = [
            ("2026-10-16T12:00:05Z", "2026-10-16T12:00:05.000000000Z"),
            ("2026-10-16T12:00:05.12Z", "2026-10-16T12:00:05.120000000Z"),
            ("2026-10-16T12:00:05.123Z", "2026-10-16T12:00:05.123000000Z"),
            (
                "2026-10-16T12:00:05.123456789Z",
                "2026-10-16T12:00:05.123456789Z",
            ),
            (
                "2026-10-16T14:00:59.9999999+02:00",
                "2026-10-16T12:00:59.999999900Z",
            ),
            (
                "2026-10-16T09:30:00-02:30",
                "2026-10-16T12:00:00.000000000Z",
            ),
        ];
        for (raw, want) in cases {
            assert_eq!(StoredAt::canonical(raw).as_deref(), Some(want), "{raw}");
        }
        assert_eq!(StoredAt::canonical(""), None);
        assert_eq!(StoredAt::canonical("not a time"), None);
        let mut sorted: Vec<&str> = cases.iter().map(|c| c.1).collect();
        sorted.sort();
        assert_eq!(
            sorted,
            [
                "2026-10-16T12:00:00.000000000Z",
                "2026-10-16T12:00:05.000000000Z",
                "2026-10-16T12:00:05.120000000Z",
                "2026-10-16T12:00:05.123000000Z",
                "2026-10-16T12:00:05.123456789Z",
                "2026-10-16T12:00:59.999999900Z",
            ]
        );
    }

    #[test]
    fn display_at_is_trimmed_rfc3339_and_passes_garbage_through() {
        assert_eq!(
            display_at("2026-10-16T12:00:05.120000000Z".into()),
            "2026-10-16T12:00:05.12Z"
        );
        assert_eq!(display_at(String::new()), "");
    }
}
