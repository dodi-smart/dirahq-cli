//! Step past a chunk the cloud keeps rejecting (DIRASH-0049).
//!
//! A `413` or a non-auth `4xx` on an ingest chunk used to back off and retry
//! the identical chunk forever. Re-sending the same bytes cannot succeed, so
//! one malformed row stopped the event cursor for good and every later event
//! queued up behind it, which users saw as a large backlog that never cleared.
//!
//! Now the same chunk failing [`THRESHOLD`] times in a row with such a
//! rejection is quarantined: the cursors advance to the chunk's high-water
//! marks and the chunk's bounds and reason are kept in `meta` for `dira doctor`
//! and `dira device resync`. Nothing is deleted locally, and anything that is
//! not a content rejection (auth, schema skew, 429, 5xx, network) never counts.

use dira_core::Store;
use serde::{Deserialize, Serialize};

/// Consecutive identical rejections before a chunk is stepped past.
pub const THRESHOLD: u32 = 5;
/// The newest entries kept in the quarantine list.
const KEEP: usize = 100;

const META_POISON: &str = "sync_poison";
pub use dira_core::sync::META_SYNC_QUARANTINE as META_QUARANTINE;

/// The cursors a chunk would have advanced to on success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkBounds {
    pub event_cursor: Option<String>,
    pub artifact_rowid: Option<i64>,
    pub token_rowid: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Poison {
    bounds: ChunkBounds,
    count: u32,
}

/// One stepped-past chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub at: String,
    pub reason: String,
    pub bounds: ChunkBounds,
}

/// Record a content rejection of `bounds`. Returns `Some(bounds)` when this was
/// the [`THRESHOLD`]th in a row for the same chunk and the caller must advance
/// the cursors past it. The quarantine entry is already written by then.
pub async fn note_rejection(
    store: &Store,
    bounds: &ChunkBounds,
    reason: &str,
) -> Option<ChunkBounds> {
    note_rejection_on(store, META_POISON, bounds, reason).await
}

/// [`note_rejection`] on its own streak key, so a producer with its own cursor
/// (meeting restatements) is neither reset by the flush's successes nor resets
/// them.
pub async fn note_rejection_on(
    store: &Store,
    streak_key: &str,
    bounds: &ChunkBounds,
    reason: &str,
) -> Option<ChunkBounds> {
    let prev: Option<Poison> = store
        .meta_get(streak_key)
        .await
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok());
    let count = match prev {
        Some(p) if &p.bounds == bounds => p.count + 1,
        _ => 1,
    };
    if count < THRESHOLD {
        let p = Poison {
            bounds: bounds.clone(),
            count,
        };
        if let Ok(s) = serde_json::to_string(&p) {
            let _ = store.meta_set(streak_key, &s).await;
        }
        return None;
    }
    let mut list = entries(store).await;
    list.push(Entry {
        at: time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
        reason: reason.chars().take(500).collect(),
        bounds: bounds.clone(),
    });
    if list.len() > KEEP {
        let drop = list.len() - KEEP;
        list.drain(..drop);
    }
    if let Ok(s) = serde_json::to_string(&list) {
        let _ = store.meta_set(META_QUARANTINE, &s).await;
    }
    let _ = store.meta_set(streak_key, "").await;
    Some(bounds.clone())
}

/// Forget a pending rejection streak (a chunk went through).
pub async fn clear_streak(store: &Store) {
    let _ = store.meta_set(META_POISON, "").await;
}

/// Every quarantined chunk, oldest first.
pub async fn entries(store: &Store) -> Vec<Entry> {
    store
        .meta_get(META_QUARANTINE)
        .await
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(ev: &str) -> ChunkBounds {
        ChunkBounds {
            event_cursor: Some(ev.into()),
            artifact_rowid: Some(7),
            token_rowid: None,
        }
    }

    #[tokio::test]
    async fn steps_past_only_after_the_threshold_for_the_same_chunk() {
        let store = Store::open_in_memory().await.unwrap();
        for _ in 1..THRESHOLD {
            assert!(note_rejection(&store, &bounds("A"), "400 bad row")
                .await
                .is_none());
        }
        assert_eq!(
            note_rejection(&store, &bounds("A"), "400 bad row").await,
            Some(bounds("A"))
        );
        let list = entries(&store).await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].bounds, bounds("A"));
    }

    #[tokio::test]
    async fn a_different_chunk_or_a_success_resets_the_streak() {
        let store = Store::open_in_memory().await.unwrap();
        for _ in 1..THRESHOLD {
            note_rejection(&store, &bounds("A"), "x").await;
        }
        assert!(note_rejection(&store, &bounds("B"), "x").await.is_none());
        clear_streak(&store).await;
        for _ in 1..THRESHOLD {
            assert!(note_rejection(&store, &bounds("B"), "x").await.is_none());
        }
        assert!(entries(&store).await.is_empty());
    }
}
