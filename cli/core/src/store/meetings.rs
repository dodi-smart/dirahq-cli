//! Local meeting spans (migration 0011, DIRASH-0060).
//!
//! The daemon's detector writes what the tracker emits; the person confirms,
//! dismisses or assigns through `dira meetings`. Every write bumps `version`
//! and takes the next store-wide `updated_seq`, which sync reads to re-send
//! changes. The sequence's high-water mark is kept in `meta` and never goes
//! backwards: deleting rows (an expired suggestion, `nuke`) must never let a
//! later change reuse a number the sync cursor has already passed.

use super::{Error, Store};
use crate::stored_at::StoredAt;
use dira_contract::{MeetingSource, ProviderClass};
use sqlx::Row;
use time::OffsetDateTime;

const META_SEQ: &str = "meeting_seq";

/// A meeting's lifecycle in the local store. Only `Recorded` and `Confirmed`
/// count as meeting time; `Suggested` waits for the person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeetingState {
    Suggested,
    Recorded,
    Confirmed,
    Dismissed,
    Retracted,
}

impl MeetingState {
    pub fn as_str(self) -> &'static str {
        match self {
            MeetingState::Suggested => "suggested",
            MeetingState::Recorded => "recorded",
            MeetingState::Confirmed => "confirmed",
            MeetingState::Dismissed => "dismissed",
            MeetingState::Retracted => "retracted",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "suggested" => MeetingState::Suggested,
            "recorded" => MeetingState::Recorded,
            "confirmed" => MeetingState::Confirmed,
            "dismissed" => MeetingState::Dismissed,
            "retracted" => MeetingState::Retracted,
            _ => return None,
        })
    }

    /// Whether this state counts as meeting time.
    pub fn counts(self) -> bool {
        matches!(self, MeetingState::Recorded | MeetingState::Confirmed)
    }
}

/// How sure the detector was. The wire knows only `high` and `confirmed`
/// (unconfirmed low runs never leave the device), so this is local.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectionConfidence {
    High,
    Low,
}

impl DetectionConfidence {
    pub fn as_str(self) -> &'static str {
        match self {
            DetectionConfidence::High => "high",
            DetectionConfidence::Low => "low",
        }
    }
}

/// A contract enum's wire spelling (they are all `snake_case` unit variants).
pub fn wire_str<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|j| j.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn from_wire<T: serde::de::DeserializeOwned>(s: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
}

/// One stored meeting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMeeting {
    pub id: String,
    pub version: u32,
    pub updated_seq: i64,
    pub started_at: OffsetDateTime,
    pub ended_at: OffsetDateTime,
    pub state: MeetingState,
    pub source: MeetingSource,
    pub confidence: DetectionConfidence,
    pub provider_class: ProviderClass,
    pub evidence: u32,
    pub project: Option<String>,
}

/// What the detector hands the store for a new span.
#[derive(Debug, Clone)]
pub struct NewMeeting {
    pub started_at: OffsetDateTime,
    pub ended_at: OffsetDateTime,
    pub state: MeetingState,
    pub source: MeetingSource,
    pub confidence: DetectionConfidence,
    pub provider_class: ProviderClass,
    pub evidence: u32,
}

/// The next change number: past every row AND the recorded high-water mark,
/// which is then raised. Run inside the write's transaction.
async fn next_seq(tx: &mut sqlx::SqliteConnection) -> Result<i64, Error> {
    let rows: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(updated_seq), 0) FROM meetings")
        .fetch_one(&mut *tx)
        .await?;
    let hw: i64 = sqlx::query_scalar::<_, String>("SELECT value FROM meta WHERE key = ?")
        .bind(META_SEQ)
        .fetch_optional(&mut *tx)
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let seq = rows.max(hw) + 1;
    sqlx::query("INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
        .bind(META_SEQ)
        .bind(seq.to_string())
        .execute(&mut *tx)
        .await?;
    Ok(seq)
}

impl Store {
    /// Insert a detected meeting; returns its new id.
    pub async fn insert_meeting(&self, m: &NewMeeting) -> Result<String, Error> {
        let id = ulid::Ulid::generate().to_string();
        let mut tx = self.pool.begin().await?;
        let seq = next_seq(&mut tx).await?;
        sqlx::query(
            "INSERT INTO meetings (id, version, updated_seq, started_at, ended_at, state, source, \
             confidence, provider_class, evidence, created_at) \
             VALUES (?, 1, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(seq)
        .bind(StoredAt(m.started_at))
        .bind(StoredAt(m.ended_at))
        .bind(m.state.as_str())
        .bind(wire_str(&m.source))
        .bind(m.confidence.as_str())
        .bind(wire_str(&m.provider_class))
        .bind(i64::from(m.evidence))
        .bind(StoredAt(OffsetDateTime::now_utc()))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Move a meeting to `state` (confirm, dismiss, retract), bumping its version
    /// and the change cursor. Returns false when no such meeting exists.
    pub async fn set_meeting_state(&self, id: &str, state: MeetingState) -> Result<bool, Error> {
        let mut tx = self.pool.begin().await?;
        let seq = next_seq(&mut tx).await?;
        let done = sqlx::query(
            "UPDATE meetings SET state = ?, version = version + 1, updated_seq = ? WHERE id = ?",
        )
        .bind(state.as_str())
        .bind(seq)
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        tx.commit().await?;
        Ok(done)
    }

    /// Assign a meeting to a project (`None` clears it).
    pub async fn assign_meeting(&self, id: &str, project: Option<&str>) -> Result<bool, Error> {
        let mut tx = self.pool.begin().await?;
        let seq = next_seq(&mut tx).await?;
        let done = sqlx::query(
            "UPDATE meetings SET project = ?, version = version + 1, updated_seq = ? WHERE id = ?",
        )
        .bind(project)
        .bind(seq)
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        tx.commit().await?;
        Ok(done)
    }

    /// Meetings overlapping `[from, to)`, by start then id (a total order, so
    /// overlap tie-breaks never depend on the read).
    pub async fn meetings_between(
        &self,
        from: OffsetDateTime,
        to: OffsetDateTime,
    ) -> Result<Vec<StoredMeeting>, Error> {
        let rows = sqlx::query(
            "SELECT * FROM meetings WHERE started_at < ? AND ended_at > ? ORDER BY started_at, id",
        )
        .bind(StoredAt(to))
        .bind(StoredAt(from))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_meeting).collect()
    }

    /// Meetings whose change cursor is past `after_seq`, in cursor order: what
    /// sync has not sent yet.
    pub async fn meetings_changed_after(
        &self,
        after_seq: i64,
    ) -> Result<Vec<StoredMeeting>, Error> {
        let rows = sqlx::query("SELECT * FROM meetings WHERE updated_seq > ? ORDER BY updated_seq")
            .bind(after_seq)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(row_to_meeting).collect()
    }

    /// How many meeting changes sync has not sent yet: the flush gate's cheap check.
    pub async fn count_meetings_changed_after(&self, after_seq: i64) -> Result<i64, Error> {
        Ok(
            sqlx::query_scalar("SELECT COUNT(*) FROM meetings WHERE updated_seq > ?")
                .bind(after_seq)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// Pending suggestions, newest first, for `dira meetings`.
    pub async fn meeting_suggestions(&self) -> Result<Vec<StoredMeeting>, Error> {
        let rows = sqlx::query(
            "SELECT * FROM meetings WHERE state = 'suggested' ORDER BY started_at DESC, id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_meeting).collect()
    }

    /// Drop suggestions that started before `before` (they expire unconfirmed).
    /// The change sequence keeps its high-water mark (see the module doc).
    pub async fn expire_meeting_suggestions(&self, before: OffsetDateTime) -> Result<u64, Error> {
        Ok(
            sqlx::query("DELETE FROM meetings WHERE state = 'suggested' AND started_at < ?")
                .bind(StoredAt(before))
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }
}

fn row_to_meeting(row: &sqlx::sqlite::SqliteRow) -> Result<StoredMeeting, Error> {
    let state: String = row.try_get("state")?;
    let source: String = row.try_get("source")?;
    let confidence: String = row.try_get("confidence")?;
    let provider: String = row.try_get("provider_class")?;
    Ok(StoredMeeting {
        id: row.try_get("id")?,
        version: row.try_get::<i64, _>("version")? as u32,
        updated_seq: row.try_get("updated_seq")?,
        started_at: row.try_get::<StoredAt, _>("started_at")?.0,
        ended_at: row.try_get::<StoredAt, _>("ended_at")?.0,
        state: MeetingState::parse(&state).unwrap_or(MeetingState::Dismissed),
        source: from_wire(&source).unwrap_or(MeetingSource::Device),
        confidence: if confidence == "high" {
            DetectionConfidence::High
        } else {
            DetectionConfidence::Low
        },
        provider_class: from_wire(&provider).unwrap_or(ProviderClass::Other),
        evidence: row.try_get::<i64, _>("evidence")? as u32,
        project: row.try_get("project")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn new(s: OffsetDateTime, e: OffsetDateTime, state: MeetingState) -> NewMeeting {
        NewMeeting {
            started_at: s,
            ended_at: e,
            state,
            source: MeetingSource::Device,
            confidence: DetectionConfidence::High,
            provider_class: ProviderClass::Zoom,
            evidence: 1,
        }
    }

    #[tokio::test]
    async fn every_change_bumps_version_and_cursor() {
        let store = Store::open_in_memory().await.unwrap();
        let a = store
            .insert_meeting(&new(
                datetime!(2026-11-06 10:00 UTC),
                datetime!(2026-11-06 11:00 UTC),
                MeetingState::Recorded,
            ))
            .await
            .unwrap();
        let b = store
            .insert_meeting(&new(
                datetime!(2026-11-06 12:00 UTC),
                datetime!(2026-11-06 12:30 UTC),
                MeetingState::Suggested,
            ))
            .await
            .unwrap();
        let all = store.meetings_changed_after(0).await.unwrap();
        assert_eq!(
            all.iter()
                .map(|m| (m.id.as_str(), m.updated_seq))
                .collect::<Vec<_>>(),
            vec![(a.as_str(), 1), (b.as_str(), 2)]
        );
        assert_eq!(
            (all[0].provider_class, all[0].source),
            (ProviderClass::Zoom, MeetingSource::Device)
        );

        assert!(store
            .set_meeting_state(&b, MeetingState::Confirmed)
            .await
            .unwrap());
        assert!(store
            .assign_meeting(&a, Some("github.com/acme/api"))
            .await
            .unwrap());
        let changed = store.meetings_changed_after(2).await.unwrap();
        assert_eq!(
            changed
                .iter()
                .map(|m| (m.id.as_str(), m.version, m.state))
                .collect::<Vec<_>>(),
            vec![
                (b.as_str(), 2, MeetingState::Confirmed),
                (a.as_str(), 2, MeetingState::Recorded)
            ]
        );
        assert_eq!(changed[1].project.as_deref(), Some("github.com/acme/api"));
        assert!(!store
            .set_meeting_state("nope", MeetingState::Dismissed)
            .await
            .unwrap());
    }

    /// Reviewer H2: deleting the newest row must not let the next change reuse
    /// its number, or a sync cursor already past it would skip that change.
    #[tokio::test]
    async fn a_deleted_newest_change_never_has_its_number_reused() {
        let store = Store::open_in_memory().await.unwrap();
        store
            .insert_meeting(&new(
                datetime!(2026-11-01 10:00 UTC),
                datetime!(2026-11-01 10:30 UTC),
                MeetingState::Suggested,
            ))
            .await
            .unwrap();
        assert_eq!(
            store
                .expire_meeting_suggestions(datetime!(2026-11-03 00:00 UTC))
                .await
                .unwrap(),
            1
        );
        store
            .insert_meeting(&new(
                datetime!(2026-11-06 10:00 UTC),
                datetime!(2026-11-06 10:30 UTC),
                MeetingState::Recorded,
            ))
            .await
            .unwrap();
        let after_cursor = store.meetings_changed_after(1).await.unwrap();
        assert_eq!(
            after_cursor.len(),
            1,
            "the new meeting must be past a cursor at the expired one"
        );
        assert_eq!(after_cursor[0].updated_seq, 2);
    }

    #[tokio::test]
    async fn suggestions_list_and_expire() {
        let store = Store::open_in_memory().await.unwrap();
        store
            .insert_meeting(&new(
                datetime!(2026-11-01 10:00 UTC),
                datetime!(2026-11-01 10:30 UTC),
                MeetingState::Suggested,
            ))
            .await
            .unwrap();
        store
            .insert_meeting(&new(
                datetime!(2026-11-06 10:00 UTC),
                datetime!(2026-11-06 10:30 UTC),
                MeetingState::Suggested,
            ))
            .await
            .unwrap();
        assert_eq!(store.meeting_suggestions().await.unwrap().len(), 2);
        assert_eq!(
            store
                .expire_meeting_suggestions(datetime!(2026-11-03 00:00 UTC))
                .await
                .unwrap(),
            1
        );
        let left = store.meeting_suggestions().await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].started_at, datetime!(2026-11-06 10:00 UTC));
        let window = store
            .meetings_between(
                datetime!(2026-11-06 10:15 UTC),
                datetime!(2026-11-06 11:00 UTC),
            )
            .await
            .unwrap();
        assert_eq!(window.len(), 1);
    }
}
