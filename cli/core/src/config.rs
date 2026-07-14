//! Layered configuration: built-in defaults → `config.toml` under XDG → `DIRA_*`
//! environment overrides. Resolved once at daemon start.

use directories::ProjectDirs;
use figment::{
    providers::{Env, Format, Serialized, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The hosted dira cloud — the out-of-the-box sync target. Local dev and
/// self-hosters override it per the layering above (`config.toml`'s
/// `cloud_url`, or the `DIRA_CLOUD_URL` env var, which wins).
pub const DEFAULT_CLOUD_URL: &str = "https://app.dirahq.sh";

/// Daemon + CLI configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Unix domain socket path for the CLI control channel.
    pub socket_path: PathBuf,
    /// Loopback port for the HTTP hook ingress.
    pub http_port: u16,
    /// SQLite database file (append-only event log + derived tables).
    pub db_path: PathBuf,
    /// Idle threshold in seconds; gaps wider than this are not counted as human time.
    pub idle_seconds: u64,
    /// Capture-time coalescing window in seconds. A high-volume tool-activity
    /// event (PreTool/PostTool) is dropped at capture if the session's last
    /// *stored* activity event is younger than this. Human signals, lifecycle,
    /// and CwdChanged events are never coalesced. MUST be `< idle_seconds` so
    /// `accounting::active_seconds` gap-counting is preserved (every surviving
    /// gap stays under the idle threshold); [`Config::coalesce`] clamps it.
    pub coalesce_seconds: u64,
    /// Retention window in days. Raw events older than this AND already synced
    /// (id ≤ sync cursor) are rolled up into `session_rollup_daily` and pruned
    /// by the daemon's maintenance task. Un-synced or recent events are kept.
    pub retention_days: u64,
    /// Cloud ingest base URL. Defaults to the hosted cloud
    /// ([`DEFAULT_CLOUD_URL`]); point it elsewhere for local dev or self-host
    /// via `config.toml` or `DIRA_CLOUD_URL` (e.g. `http://localhost:3000`).
    /// A URL alone never causes network traffic — every cloud task no-ops
    /// until the device is linked (`dira device link`). Sync is off if unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_url: Option<String>,
    /// How often the daemon POSTs a live-presence heartbeat, in seconds.
    ///
    /// Retained as the *baseline / fallback* cadence. Phase 6 replaces the fixed
    /// tick with adaptive timing bounded by `heartbeat_active_secs` (fast, when a
    /// live session is engaged) and `heartbeat_idle_secs` (slow, when all idle);
    /// this value is only used if those are unset/degenerate.
    pub heartbeat_interval_secs: u64,
    /// Fast heartbeat cadence (seconds) used while any live session is *active*
    /// (recent activity within the idle window). Keeps "Right now" fresh.
    pub heartbeat_active_secs: u64,
    /// Slow heartbeat cadence (seconds) used while all live sessions are idle (or
    /// there are none). MUST stay `< presence_ttl_secs` so the keepalive beat
    /// always renews presence before the cloud expires the device;
    /// [`Config::heartbeat_idle`] clamps it.
    pub heartbeat_idle_secs: u64,
    /// How long the cloud should treat a heartbeat as fresh before a device is
    /// considered offline, in seconds. Sent to / consumed by the cloud; the daemon
    /// only carries it (it should comfortably exceed `heartbeat_interval_secs`).
    pub presence_ttl_secs: u64,
    /// Emit a *partial* `SessionRollup` (ended_at=None) for a not-yet-ended session
    /// once it is older than this many seconds and has new activity since its last
    /// partial. Lets a multi-day session contribute settled-ish wall time before
    /// `SessionEnd`. The cloud MUST UPSERT session rows by `session_id` (latest
    /// wins) for this to be safe — see `sync::build_batch_with_partials`.
    pub partial_rollup_after_secs: u64,
    /// Compute report day boundaries (`Today` / `Week`) in the *system local*
    /// timezone instead of UTC. Default `false` preserves the historical
    /// UTC-midnight behavior (and keeps existing report tests deterministic). When
    /// `true`, the daemon resolves the local UTC offset at request time; if that
    /// resolution fails (it can in multithreaded contexts), it falls back to UTC.
    #[serde(default)]
    pub report_local_day: bool,
    /// Seconds of zero-active-sessions quiet (no session ended/ticked and no
    /// process activity) before the heartbeat considers the device "deep idle"
    /// and slows to `presence_ttl_deep_idle_secs`'s cadence instead of the
    /// ordinary idle band. An instant-wake `Notify` (fired at the same sites the
    /// sync trigger fires) still lands a beat immediately the moment activity
    /// resumes, so this only trades presence freshness for POST volume while the
    /// device is truly quiet.
    pub deep_idle_after_secs: u64,
    /// The presence TTL the daemon advertises to the cloud while deep idle. `600`
    /// is deliberately the cloud's clamp ceiling for a per-ping `presence_ttl_secs`
    /// (see the cloud's presence-ack clamp), so no cloud-side change is needed to
    /// honor it.
    pub presence_ttl_deep_idle_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        let dirs = project_dirs();
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::temp_dir().into())
            .unwrap_or_else(std::env::temp_dir);
        Self {
            socket_path: runtime.join("dira.sock"),
            http_port: 8722,
            db_path: dirs
                .map(|d| d.data_dir().join("dira.db"))
                .unwrap_or_else(|| std::env::temp_dir().join("dira.db")),
            idle_seconds: 300,
            coalesce_seconds: 45,
            retention_days: 14,
            cloud_url: Some(DEFAULT_CLOUD_URL.to_string()),
            heartbeat_interval_secs: 25,
            heartbeat_active_secs: 10,
            heartbeat_idle_secs: 90,
            presence_ttl_secs: 75,
            partial_rollup_after_secs: 3600,
            report_local_day: false,
            deep_idle_after_secs: 900,
            presence_ttl_deep_idle_secs: 600,
        }
    }
}

/// Compute the start-of-day instant for `now` given a UTC `offset`: the most
/// recent local midnight, expressed back as a UTC `OffsetDateTime`.
///
/// Pure + offset-injectable so the boundary math is unit-testable without
/// depending on the ambient machine timezone. `start_of_day(now, UtcOffset::UTC)`
/// reproduces the historical UTC-midnight behavior exactly.
pub fn start_of_day(now: time::OffsetDateTime, offset: time::UtcOffset) -> time::OffsetDateTime {
    now.to_offset(offset)
        .replace_time(time::Time::MIDNIGHT)
        .to_offset(time::UtcOffset::UTC)
}

impl Config {
    /// Load defaults, then `config.toml` (if present), then `DIRA_*` env vars.
    pub fn load() -> Result<Self, crate::Error> {
        let mut fig = Figment::from(Serialized::defaults(Config::default()));
        if let Some(dirs) = project_dirs() {
            fig = fig.merge(Toml::file(dirs.config_dir().join("config.toml")));
        }
        fig.merge(
            Env::prefixed("DIRA_").map(|k| k.as_str().to_lowercase().replace("__", ".").into()),
        )
        .extract()
        .map_err(|e| crate::Error::Config(e.to_string()))
    }

    pub fn idle(&self) -> time::Duration {
        time::Duration::seconds(self.idle_seconds as i64)
    }

    /// The effective coalescing window, clamped to strictly less than the idle
    /// threshold. This is a hard invariant: if `coalesce_seconds` were ever set
    /// `>= idle_seconds`, coalescing could open a counted gap wider than `idle`
    /// and silently shrink `active_seconds`. We clamp (rather than reject) so a
    /// misconfiguration degrades gracefully — at worst, less aggressive
    /// coalescing. A `coalesce_seconds` of 0 disables coalescing.
    pub fn coalesce(&self) -> time::Duration {
        let max = self.idle_seconds.saturating_sub(1);
        time::Duration::seconds(self.coalesce_seconds.min(max) as i64)
    }

    pub fn retention(&self) -> time::Duration {
        time::Duration::days(self.retention_days as i64)
    }

    /// Fast (active) heartbeat cadence as a `Duration`, floored at 1s so a
    /// misconfigured `0` never busy-loops the beat task.
    pub fn heartbeat_active(&self) -> time::Duration {
        time::Duration::seconds(self.heartbeat_active_secs.max(1) as i64)
    }

    /// Slow (idle) heartbeat cadence as a `Duration`, clamped to strictly less
    /// than the presence TTL. This is a hard invariant: the idle keepalive beat
    /// must renew presence *before* the cloud expires the device, otherwise a
    /// quiet-but-online device would flap offline every TTL. We clamp (rather than
    /// reject) so a misconfiguration degrades gracefully. Also floored at 1s and
    /// never below the active cadence.
    pub fn heartbeat_idle(&self) -> time::Duration {
        // Leave headroom under the TTL so a beat lands before expiry even with
        // tick jitter / a slow POST: cap at ttl - 5s (or ttl/2 for tiny TTLs).
        let ttl = self.presence_ttl_secs.max(1);
        let ceiling = ttl.saturating_sub(5).max(ttl / 2).max(1);
        let idle = self.heartbeat_idle_secs.min(ceiling).max(1);
        time::Duration::seconds(idle.max(self.heartbeat_active_secs.max(1)) as i64)
    }

    /// The age past which a not-yet-ended session is eligible for a partial
    /// rollup, as a `Duration`. `0` disables partial rollups.
    pub fn partial_rollup_after(&self) -> time::Duration {
        time::Duration::seconds(self.partial_rollup_after_secs as i64)
    }

    /// The quiet window (as a `Duration`) past which a device with zero active
    /// sessions is considered "deep idle" by the heartbeat (WP-A3). Floored at
    /// 60s: a smaller quiet window would let a device flap into the deep-idle
    /// TTL band (up to 600s) almost immediately after going idle, which is
    /// squarely the kind of misconfiguration this clamp exists to prevent —
    /// same pattern as [`Config::coalesce`] / [`Config::heartbeat_idle`].
    pub fn deep_idle_after(&self) -> time::Duration {
        time::Duration::seconds(self.deep_idle_after_secs.max(60) as i64)
    }

    /// The presence TTL advertised while deep idle, clamped to
    /// `[presence_ttl_secs, 600]`. `600` is the cloud's clamp ceiling for a
    /// per-ping `presence_ttl_secs` (see `presence_ttl_deep_idle_secs`'s field
    /// doc); the floor at `presence_ttl_secs` prevents a misconfigured deep-idle
    /// TTL from ever being SHORTER than the ordinary band's TTL, which would
    /// make the (already tight) normal→deep-idle transition racier than it
    /// needs to be. Clamped (not rejected) so a bad config degrades gracefully,
    /// matching the other knobs on this type.
    pub fn presence_ttl_deep_idle(&self) -> u64 {
        let floor = self.presence_ttl_secs.clamp(1, 600);
        self.presence_ttl_deep_idle_secs.clamp(floor, 600)
    }
}

/// XDG project dirs for `sh.dirahq.dira`. `None` only on exotic platforms.
pub fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("sh", "dirahq", "dira")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_cloud_url_is_the_hosted_cloud() {
        assert_eq!(
            Config::default().cloud_url.as_deref(),
            Some(DEFAULT_CLOUD_URL),
            "a fresh install must point at the hosted cloud out of the box",
        );
    }

    #[test]
    fn cloud_url_default_is_overridable_by_a_config_overlay() {
        // Same layering `Config::load` uses (defaults → toml → env), with the
        // toml layer inlined so the test touches no real XDG paths or env vars.
        let c: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::string("cloud_url = \"http://localhost:3000\""))
            .extract()
            .unwrap();
        assert_eq!(c.cloud_url.as_deref(), Some("http://localhost:3000"));
    }

    #[test]
    fn defaults_keep_coalesce_below_idle() {
        let c = Config::default();
        assert!(c.coalesce() < c.idle(), "coalesce must stay under idle");
        assert_eq!(c.coalesce(), time::Duration::seconds(45));
        assert_eq!(c.retention(), time::Duration::days(14));
    }

    #[test]
    fn coalesce_is_clamped_below_idle() {
        // A misconfiguration with coalesce >= idle is clamped to idle - 1.
        let c = Config {
            coalesce_seconds: 600,
            idle_seconds: 300,
            ..Config::default()
        };
        assert_eq!(c.coalesce(), time::Duration::seconds(299));
        assert!(c.coalesce() < c.idle());
    }

    #[test]
    fn coalesce_zero_disables() {
        let c = Config {
            coalesce_seconds: 0,
            ..Config::default()
        };
        assert_eq!(c.coalesce(), time::Duration::ZERO);
    }

    #[test]
    fn heartbeat_idle_stays_below_presence_ttl() {
        // The default idle cadence (90) is deliberately *above* the default TTL
        // (75) to prove the clamp keeps the keepalive under the TTL.
        let c = Config::default();
        assert!(
            c.heartbeat_idle() < time::Duration::seconds(c.presence_ttl_secs as i64),
            "idle cadence must stay under the presence TTL so the keepalive renews in time"
        );
    }

    #[test]
    fn heartbeat_idle_clamped_under_ttl_with_headroom() {
        let c = Config {
            heartbeat_idle_secs: 600,
            presence_ttl_secs: 120,
            ..Config::default()
        };
        // Capped at ttl - 5 = 115s.
        assert_eq!(c.heartbeat_idle(), time::Duration::seconds(115));
    }

    #[test]
    fn heartbeat_active_floored_at_one_second() {
        let c = Config {
            heartbeat_active_secs: 0,
            ..Config::default()
        };
        assert_eq!(c.heartbeat_active(), time::Duration::seconds(1));
    }

    #[test]
    fn report_local_day_defaults_off() {
        // The default MUST stay UTC so existing report behavior/tests are stable.
        assert!(!Config::default().report_local_day);
    }

    #[test]
    fn start_of_day_in_utc_matches_utc_midnight() {
        // With a UTC offset the helper reproduces the historical behavior exactly:
        // truncate the time-of-day to midnight, same calendar date.
        let now = time::macros::datetime!(2026-06-29 14:30:15 UTC);
        let sod = start_of_day(now, time::UtcOffset::UTC);
        assert_eq!(sod, time::macros::datetime!(2026-06-29 00:00:00 UTC));
    }

    #[test]
    fn start_of_day_uses_local_offset_for_the_boundary() {
        // 01:30 UTC on the 29th is still the 28th at UTC-04 (e.g. US Eastern):
        // local midnight of the 28th == 04:00 UTC on the 28th.
        let now = time::macros::datetime!(2026-06-29 01:30:00 UTC);
        let east = time::UtcOffset::from_hms(-4, 0, 0).unwrap();
        let sod = start_of_day(now, east);
        assert_eq!(sod, time::macros::datetime!(2026-06-28 04:00:00 UTC));
    }

    #[test]
    fn start_of_day_positive_offset_can_advance_the_date() {
        // 23:30 UTC on the 29th is already 09:30 on the 30th at UTC+10:
        // local midnight of the 30th == 14:00 UTC on the 29th.
        let now = time::macros::datetime!(2026-06-29 23:30:00 UTC);
        let plus10 = time::UtcOffset::from_hms(10, 0, 0).unwrap();
        let sod = start_of_day(now, plus10);
        assert_eq!(sod, time::macros::datetime!(2026-06-29 14:00:00 UTC));
    }

    #[test]
    fn heartbeat_idle_never_below_active() {
        let c = Config {
            heartbeat_active_secs: 30,
            heartbeat_idle_secs: 10,
            presence_ttl_secs: 300,
            ..Config::default()
        };
        assert!(c.heartbeat_idle() >= c.heartbeat_active());
    }

    #[test]
    fn deep_idle_after_floored_at_60_seconds() {
        let c = Config {
            deep_idle_after_secs: 1,
            ..Config::default()
        };
        assert_eq!(c.deep_idle_after(), time::Duration::seconds(60));
    }

    #[test]
    fn deep_idle_after_passes_through_above_the_floor() {
        let c = Config::default();
        assert_eq!(c.deep_idle_after(), time::Duration::seconds(900));
    }

    #[test]
    fn presence_ttl_deep_idle_clamped_to_600_ceiling() {
        let c = Config {
            presence_ttl_deep_idle_secs: 10_000,
            ..Config::default()
        };
        assert_eq!(c.presence_ttl_deep_idle(), 600);
    }

    #[test]
    fn presence_ttl_deep_idle_never_below_the_normal_ttl() {
        // A misconfigured deep-idle TTL shorter than the normal band's TTL is
        // clamped up to the normal TTL, not left racier than the normal band.
        let c = Config {
            presence_ttl_secs: 300,
            presence_ttl_deep_idle_secs: 100,
            ..Config::default()
        };
        assert_eq!(c.presence_ttl_deep_idle(), 300);
    }

    #[test]
    fn presence_ttl_deep_idle_passes_through_in_range() {
        let c = Config::default();
        assert_eq!(c.presence_ttl_deep_idle(), 600);
    }
}
