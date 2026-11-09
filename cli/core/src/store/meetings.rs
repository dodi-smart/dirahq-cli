//! Local meeting spans (migration 0011, DIRASH-0060).
//!
//! The daemon's detector writes what the tracker emits; the person confirms,
//! dismisses or assigns through `dira meetings`. Every write bumps `version` and
//! the store-wide `updated_seq`, which is what sync reads to re-send changes.

use super::{Error, Store};
use crate::stored_at::StoredAt;
use sqlx::Row;
use time::OffsetDateTime;

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

/// One stored meeting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMeeting {
    pub id: String,
    pub version: u32,
    pub updated_seq: i64,
    pub started_at: OffsetDateTime,
    pub ended_at: OffsetDateTime,
    pub state: MeetingState,
    /// `device`, `calendar`, `presence` or `user`.
    pub source: String,
    /// `high` or `low`.
    pub confidence: String,
    /// The closed wire provider class (`zoom`, `teams`, …).
    pub provider_class: String,
    pub evidence: u32,
    pub project: Option<String>,
    pub engaged_during_meeting_seconds: u64,
}

/// What the detector hands the store for a new span.
#[derive(Debug, Clone)]
pub struct NewMeeting {
    pub started_at: OffsetDateTime,
    pub ended_at: OffsetDateTime,
    pub state: MeetingState,
    pub source: &'static str,
    pub confidence: &'static str,
    pub provider_class: &'static str,
    pub evidence: u32,
}

impl Store {
    /// Insert a detected meeting; returns its new id.
    pub async fn insert_meeting(&self, m: &NewMeeting) -> Result<String, Error> {
        let id = ulid::Ulid::generate().to_string();
        let mut tx = self.pool.begin().await?;
        let seq: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(updated_seq), 0) + 1 FROM meetings")
            .fetch_one(&mut *tx)
            .await?;
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
        .bind(m.source)
        .bind(m.confidence)
        .bind(m.provider_class)
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
        let seq: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(updated_seq), 0) + 1 FROM meetings")
            .fetch_one(&mut *tx)
            .await?;
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
        let seq: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(updated_seq), 0) + 1 FROM meetings")
            .fetch_one(&mut *tx)
            .await?;
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

    /// Meetings overlapping `[from, to)`, oldest first.
    pub async fn meetings_between(
        &self,
        from: OffsetDateTime,
        to: OffsetDateTime,
    ) -> Result<Vec<StoredMeeting>, Error> {
        let rows = sqlx::query(
            "SELECT * FROM meetings WHERE started_at < ? AND ended_at > ? ORDER BY started_at",
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

    /// Pending suggestions, newest first, for `dira meetings`.
    pub async fn meeting_suggestions(&self) -> Result<Vec<StoredMeeting>, Error> {
        let rows = sqlx::query(
            "SELECT * FROM meetings WHERE state = 'suggested' ORDER BY started_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_meeting).collect()
    }

    /// Drop suggestions that started before `before` (they expire unconfirmed).
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
    Ok(StoredMeeting {
        id: row.try_get("id")?,
        version: row.try_get::<i64, _>("version")? as u32,
        updated_seq: row.try_get("updated_seq")?,
        started_at: row.try_get::<StoredAt, _>("started_at")?.0,
        ended_at: row.try_get::<StoredAt, _>("ended_at")?.0,
        state: MeetingState::parse(&state).unwrap_or(MeetingState::Dismissed),
        source: row.try_get("source")?,
        confidence: row.try_get("confidence")?,
        provider_class: row.try_get("provider_class")?,
        evidence: row.try_get::<i64, _>("evidence")? as u32,
        project: row.try_get("project")?,
        engaged_during_meeting_seconds: row.try_get::<i64, _>("engaged_during_meeting_seconds")?
            as u64,
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
            source: "device",
            confidence: "high",
            provider_class: "zoom",
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
