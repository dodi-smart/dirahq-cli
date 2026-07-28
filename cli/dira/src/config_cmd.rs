//! `dira config` — inspect the effective configuration and persist overrides.
//!
//! - `dira config get [key]` prints the *resolved* config (defaults → the XDG
//!   `config.toml` → `DIRA_*` env), or a single key.
//! - `dira config set <key> <value>` writes the key to the XDG `config.toml`
//!   with [`toml_edit`], creating the file (and its parent dir) if absent and
//!   preserving any existing keys, ordering, and comments.
//! - `dira config path` prints where that file lives.
//!
//! Only daemon-side knobs the user is meant to tune are settable here. Pure
//! transport/identity values (socket path, db path, http port) are intentionally
//! omitted — they're derived from XDG and changing them by hand is a foot-gun.
//!
//! NOTE: the daemon resolves config once at startup, so `set` prints a reminder
//! that daemon-side knobs only take effect after `dira daemon restart`.

use anyhow::{anyhow, bail, Context, Result};
use dira_core::{config::project_dirs, Config};
use std::path::PathBuf;
use toml_edit::{value, DocumentMut};

/// A settable knob: its key, the kind of value it accepts, and a one-line help.
struct Knob {
    key: &'static str,
    kind: Kind,
    help: &'static str,
}

#[derive(Clone, Copy)]
enum Kind {
    /// A free-form string (e.g. `cloud_url`).
    Str,
    /// A non-negative integer of seconds/days.
    U64,
    /// One of a fixed set of lowercase values.
    Enum(&'static [&'static str]),
}

/// The keys `dira config set` understands. Keeping this as an explicit table (vs.
/// reflecting over `Config`) lets us refuse transport/identity fields and attach
/// per-key validation + help.
const KNOBS: &[Knob] = &[
    Knob {
        key: "cloud_url",
        kind: Kind::Str,
        help: "cloud ingest base URL (enables sync); unset to disable",
    },
    Knob {
        key: "idle_seconds",
        kind: Kind::U64,
        help: "idle threshold; gaps wider than this aren't counted as human time",
    },
    Knob {
        key: "heartbeat_active_secs",
        kind: Kind::U64,
        help: "fast heartbeat cadence while a live session is active",
    },
    Knob {
        key: "heartbeat_idle_secs",
        kind: Kind::U64,
        help: "slow heartbeat cadence while all sessions are idle",
    },
    Knob {
        key: "coalesce_seconds",
        kind: Kind::U64,
        help: "capture-time coalescing window; MUST be < idle_seconds",
    },
    Knob {
        key: "retention_days",
        kind: Kind::U64,
        help: "raw-event retention before rollup + prune",
    },
    Knob {
        key: "partial_rollup_after_secs",
        kind: Kind::U64,
        help: "age at which a still-open session emits a partial rollup (0 = off)",
    },
    Knob {
        key: "report_local_day",
        kind: Kind::U64, // accepts 0/1 or true/false; parsed in set()
        help: "compute report day boundaries in local time (0/1); default 0 (UTC)",
    },
    Knob {
        key: "deep_idle_after_secs",
        kind: Kind::U64,
        help: "quiet time before the heartbeat goes deep idle; clamped to a 60s floor",
    },
    Knob {
        key: "presence_ttl_deep_idle_secs",
        kind: Kind::U64,
        help: "presence TTL advertised while deep idle; clamped to [presence_ttl_secs, 600]",
    },
    Knob {
        key: "modules.zavet",
        kind: Kind::Enum(&["auto", "on", "off"]),
        help: "zavet knowledge module: auto (active when a repo has .zavet/), on, off",
    },
    // `Config.update.check` is a plain `bool` (see `dira_core::config::UpdateKnobs`),
    // not an enum type — unlike `modules.zavet`'s `ZavetMode`, there's no
    // `Deserialize`/`Serialize` impl mapping "on"/"off" to it for free. So this
    // knob gets its own `parse_and_validate` arm (bool <- "on"/"off") and its own
    // `get()` rendering (bool -> "on"/"off") below, instead of relying on the
    // generic `Kind::Enum` path the way `modules.zavet` does.
    Knob {
        key: "update.check",
        kind: Kind::Enum(&["on", "off"]),
        help: "passive update-available notice after status/version/daemon status: on, off",
    },
];

fn knob(key: &str) -> Option<&'static Knob> {
    KNOBS.iter().find(|k| k.key == key)
}

/// The "Settable keys" help block, rendered from [`KNOBS`] so `dira config set
/// --help` can never drift from what [`set`] actually validates: adding a knob
/// to the table updates the help (and the error message) in one place.
pub(crate) fn knobs_after_help() -> String {
    let mut s = String::from("Settable keys:\n");
    for k in KNOBS {
        s.push_str(&format!("  {:<26} {}\n", k.key, k.help));
    }
    s.push_str(
        "\nDaemon-side changes take effect after a daemon restart\n\
         (`dira daemon stop` then `dira daemon start`).",
    );
    s
}

/// The XDG path of the writable `config.toml`.
fn config_path() -> Result<PathBuf> {
    let dirs =
        project_dirs().ok_or_else(|| anyhow!("could not resolve an XDG config directory"))?;
    Ok(dirs.config_dir().join("config.toml"))
}

/// `dira config path`.
pub fn path() -> Result<()> {
    println!("{}", config_path()?.display());
    Ok(())
}

/// `dira config get [key]` — print the effective, resolved configuration.
pub fn get(config: &Config, key: Option<&str>) -> Result<()> {
    // Round-trip through serde so we print exactly the resolved values (after
    // defaults + file + env), in a stable `key = value` form.
    let value = serde_json::to_value(config).context("serialize resolved config")?;
    let map = value
        .as_object()
        .ok_or_else(|| anyhow!("config did not serialize to an object"))?;

    if let Some(k) = key {
        // Dotted keys (`modules.zavet`) traverse into nested tables.
        let mut cur = Some(&value);
        for seg in k.split('.') {
            cur = cur.and_then(|v| v.get(seg));
        }
        match cur {
            Some(v) => {
                println!("{}", render_scalar(k, v));
                Ok(())
            }
            None => bail!("unknown config key `{k}` (try `dira config get` to list all)"),
        }
    } else {
        // Print every resolved key, sorted for stable output.
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        for k in keys {
            println!("{k} = {}", render_json_scalar(&map[k]));
        }
        Ok(())
    }
}

/// Render a resolved JSON scalar as a bare value (no surrounding quotes for
/// strings — this is human/script output, not TOML).
fn render_json_scalar(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "(unset)".to_string(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Like [`render_json_scalar`], but lets a dotted-key lookup render in the
/// vocabulary its `set` counterpart accepts. Only `update.check` needs this:
/// its backing field is a plain `bool` (see `UpdateKnobs`), but `set` speaks
/// "on"/"off" like the enum knobs do, so `get` echoes the same words rather
/// than leaking the wire representation.
fn render_scalar(key: &str, v: &serde_json::Value) -> String {
    if key == "update.check" {
        if let Some(b) = v.as_bool() {
            return if b { "on" } else { "off" }.to_string();
        }
    }
    render_json_scalar(v)
}

/// `dira config set <key> <value>` — validate then persist to `config.toml`.
pub fn set(config: &Config, key: &str, raw: &str) -> Result<()> {
    let Some(knob) = knob(key) else {
        let known = KNOBS
            .iter()
            .map(|k| format!("  {} — {}", k.key, k.help))
            .collect::<Vec<_>>()
            .join("\n");
        bail!("`{key}` is not settable. settable keys:\n{known}");
    };

    // Parse + validate the new value, computing the cross-field invariant against
    // the *currently resolved* config so e.g. coalesce stays under idle.
    let item = parse_and_validate(config, knob, raw)?;

    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create config dir {}", parent.display()))?;
    }

    // Load the existing document (or start a fresh one), edit surgically, write
    // back — comments, ordering, and untouched keys are preserved by toml_edit.
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: DocumentMut = existing
        .parse()
        .with_context(|| format!("parse existing {}", path.display()))?;
    assign(&mut doc, key, item);
    std::fs::write(&path, doc.to_string()).with_context(|| format!("write {}", path.display()))?;

    println!("set {key} = {raw}");
    println!("wrote {}", path.display());
    println!("note: restart the daemon for daemon-side changes to take effect (`dira daemon stop` then `dira daemon start`)");
    Ok(())
}

/// Write `item` at `key` in the document, where a dotted key (`modules.zavet`)
/// addresses a nested table — toml_edit creates intermediate tables implicitly.
fn assign(doc: &mut DocumentMut, key: &str, item: toml_edit::Item) {
    let mut segs: Vec<&str> = key.split('.').collect();
    let last = segs.pop().expect("knob keys are non-empty");
    let mut cur = doc.as_item_mut();
    for seg in segs {
        // Materialize intermediate segments as real `[table]`s (indexing alone
        // would create an inline table on assignment).
        if cur.get(seg).is_none_or(toml_edit::Item::is_none) {
            cur[seg] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        cur = &mut cur[seg];
    }
    cur[last] = item;
}

/// Parse `raw` per the knob's kind and enforce validation, returning the TOML item
/// to write. Mirrors the `Config` clamp invariants so a `set` that would violate
/// them is rejected up front with a clear message rather than silently clamped.
fn parse_and_validate(config: &Config, knob: &Knob, raw: &str) -> Result<toml_edit::Item> {
    match knob.key {
        "report_local_day" => {
            let b = match raw.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => true,
                "0" | "false" | "no" | "off" => false,
                _ => bail!("report_local_day must be a boolean (0/1, true/false)"),
            };
            Ok(value(b))
        }
        "cloud_url" => {
            let v = raw.trim();
            if v.is_empty() {
                bail!("cloud_url must not be empty (remove the key from config.toml to unset)");
            }
            if !(v.starts_with("http://") || v.starts_with("https://")) {
                bail!("cloud_url must start with http:// or https:// (got `{v}`)");
            }
            Ok(value(v))
        }
        // Bridges the "on"/"off" vocabulary (matching the other enum-shaped
        // knobs) onto the real `bool` `Config.update.check` field — a real
        // TOML boolean, not the string "on"/"off", since that's what
        // `UpdateKnobs` deserializes.
        "update.check" => match raw.trim().to_ascii_lowercase().as_str() {
            "on" => Ok(value(true)),
            "off" => Ok(value(false)),
            _ => bail!("update.check must be one of: on, off (got `{raw}`)"),
        },
        key => match knob.kind {
            Kind::U64 => {
                let n: u64 = raw
                    .trim()
                    .parse()
                    .map_err(|_| anyhow!("{key} must be a non-negative integer (got `{raw}`)"))?;
                validate_u64(config, key, n)?;
                Ok(value(n as i64))
            }
            Kind::Str => Ok(value(raw.trim())),
            Kind::Enum(allowed) => {
                let v = raw.trim().to_ascii_lowercase();
                if !allowed.contains(&v.as_str()) {
                    bail!("{key} must be one of: {} (got `{raw}`)", allowed.join(", "));
                }
                Ok(value(v))
            }
        },
    }
}

/// Cross-field numeric validation, mirroring the `Config` invariants.
fn validate_u64(config: &Config, key: &str, n: u64) -> Result<()> {
    match key {
        // The hard invariant from `Config::coalesce`: a coalescing window >= the
        // idle threshold could open a counted gap wider than idle and shrink
        // accounted human time. We reject here (the runtime still clamps as a
        // safety net) so the user gets an explicit error instead of silent clamp.
        "coalesce_seconds" => {
            if n >= config.idle_seconds {
                bail!(
                    "coalesce_seconds ({n}) must be < idle_seconds ({}) — \
                     a wider window could undercount human time",
                    config.idle_seconds
                );
            }
        }
        // If idle is lowered below the current coalesce window, the same invariant
        // would break. Flag it so the user lowers coalesce too.
        "idle_seconds" => {
            if n == 0 {
                bail!("idle_seconds must be > 0");
            }
            if config.coalesce_seconds >= n {
                bail!(
                    "idle_seconds ({n}) must be > the current coalesce_seconds ({}) — \
                     lower coalesce_seconds first",
                    config.coalesce_seconds
                );
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            idle_seconds: 300,
            coalesce_seconds: 45,
            ..Config::default()
        }
    }

    #[test]
    fn rejects_unknown_key() {
        let err = set(&cfg(), "socket_path", "/tmp/x.sock").unwrap_err();
        assert!(err.to_string().contains("not settable"));
    }

    #[test]
    fn coalesce_must_stay_below_idle() {
        let knob = knob("coalesce_seconds").unwrap();
        // 300 == idle is rejected.
        assert!(parse_and_validate(&cfg(), knob, "300").is_err());
        // 299 is fine.
        assert!(parse_and_validate(&cfg(), knob, "299").is_ok());
    }

    #[test]
    fn idle_cannot_drop_below_current_coalesce() {
        let knob = knob("idle_seconds").unwrap();
        // coalesce is 45; idle of 40 would invert the invariant.
        assert!(parse_and_validate(&cfg(), knob, "40").is_err());
        assert!(parse_and_validate(&cfg(), knob, "600").is_ok());
        // zero idle is nonsense.
        assert!(parse_and_validate(&cfg(), knob, "0").is_err());
    }

    #[test]
    fn non_numeric_is_rejected() {
        let knob = knob("retention_days").unwrap();
        assert!(parse_and_validate(&cfg(), knob, "soon").is_err());
        assert!(parse_and_validate(&cfg(), knob, "30").is_ok());
    }

    #[test]
    fn cloud_url_must_be_http() {
        let knob = knob("cloud_url").unwrap();
        assert!(parse_and_validate(&cfg(), knob, "app.dirahq.sh").is_err());
        assert!(parse_and_validate(&cfg(), knob, "https://app.dirahq.sh").is_ok());
        assert!(parse_and_validate(&cfg(), knob, "  ").is_err());
    }

    #[test]
    fn report_local_day_accepts_bool_forms() {
        let knob = knob("report_local_day").unwrap();
        assert!(parse_and_validate(&cfg(), knob, "true").is_ok());
        assert!(parse_and_validate(&cfg(), knob, "0").is_ok());
        assert!(parse_and_validate(&cfg(), knob, "maybe").is_err());
    }

    #[test]
    fn set_preserves_existing_keys_and_comments() {
        // Drive the surgical-edit core directly (set() writes to the real XDG path).
        let original = "# my config\nidle_seconds = 300\ncloud_url = \"https://old\"\n";
        let mut doc: DocumentMut = original.parse().unwrap();
        doc["retention_days"] = value(30i64);
        let out = doc.to_string();
        assert!(out.contains("# my config"));
        assert!(out.contains("idle_seconds = 300"));
        assert!(out.contains("cloud_url = \"https://old\""));
        assert!(out.contains("retention_days = 30"));
    }

    #[test]
    fn modules_zavet_accepts_only_the_three_modes() {
        let knob = knob("modules.zavet").unwrap();
        assert!(parse_and_validate(&cfg(), knob, "auto").is_ok());
        assert!(parse_and_validate(&cfg(), knob, "ON").is_ok()); // case-folded
        assert!(parse_and_validate(&cfg(), knob, "off").is_ok());
        assert!(parse_and_validate(&cfg(), knob, "maybe").is_err());
    }

    #[test]
    fn update_check_accepts_only_on_off() {
        let knob = knob("update.check").unwrap();
        assert!(parse_and_validate(&cfg(), knob, "on").is_ok());
        assert!(parse_and_validate(&cfg(), knob, "OFF").is_ok()); // case-folded
        assert!(parse_and_validate(&cfg(), knob, "true").is_err());
        assert!(parse_and_validate(&cfg(), knob, "maybe").is_err());
    }

    #[test]
    fn update_check_writes_a_real_toml_bool_not_a_string() {
        let knob = knob("update.check").unwrap();
        let item = parse_and_validate(&cfg(), knob, "off").unwrap();
        assert_eq!(item.as_bool(), Some(false));
        let item = parse_and_validate(&cfg(), knob, "on").unwrap();
        assert_eq!(item.as_bool(), Some(true));
    }

    #[test]
    fn update_check_round_trips_through_set_and_get() {
        // The same dotted-table assign `set()` uses, then the same traversal
        // `get()` uses, then rendered the way `get`'s CLI path renders it
        // (on/off, not true/false) — end-to-end proof of the T5 acceptance
        // criterion without touching the real XDG config path.
        let original = "idle_seconds = 300\n";
        let mut doc: DocumentMut = original.parse().unwrap();
        let item = parse_and_validate(&cfg(), knob("update.check").unwrap(), "off").unwrap();
        assign(&mut doc, "update.check", item);
        let out = doc.to_string();
        assert!(out.contains("[update]"));
        assert!(out.contains("check = false"));

        use figment::providers::Format as _;
        let resolved: Config =
            figment::Figment::from(figment::providers::Serialized::defaults(Config::default()))
                .merge(figment::providers::Toml::string(&out))
                .extract()
                .unwrap();
        assert!(!resolved.update.check);

        let v = serde_json::to_value(&resolved).unwrap();
        let got = v.get("update").and_then(|u| u.get("check")).unwrap();
        assert_eq!(render_scalar("update.check", got), "off");
    }

    #[test]
    fn dotted_key_writes_a_nested_table_and_preserves_the_rest() {
        let original = "# my config\nidle_seconds = 300\n";
        let mut doc: DocumentMut = original.parse().unwrap();
        assign(&mut doc, "modules.zavet", value("on"));
        let out = doc.to_string();
        assert!(out.contains("# my config"));
        assert!(out.contains("idle_seconds = 300"));
        assert!(out.contains("[modules]"));
        assert!(out.contains("zavet = \"on\""));
        // And the round-trip parses back into Config with the mode applied.
        use figment::providers::Format as _;
        let cfg: Config =
            figment::Figment::from(figment::providers::Serialized::defaults(Config::default()))
                .merge(figment::providers::Toml::string(&out))
                .extract()
                .unwrap();
        assert_eq!(cfg.modules.zavet, dira_core::config::ZavetMode::On);
    }

    #[test]
    fn get_resolves_dotted_keys() {
        // The same traversal `get` uses, against the serialized config.
        let v = serde_json::to_value(cfg()).unwrap();
        let got = v.get("modules").and_then(|m| m.get("zavet")).unwrap();
        assert_eq!(got, &serde_json::json!("auto"));
    }

    #[test]
    fn set_overwrites_only_the_target_key() {
        let original = "idle_seconds = 300\nretention_days = 14\n";
        let mut doc: DocumentMut = original.parse().unwrap();
        doc["retention_days"] = value(60i64);
        let out = doc.to_string();
        assert!(out.contains("idle_seconds = 300"));
        assert!(out.contains("retention_days = 60"));
        assert!(!out.contains("retention_days = 14"));
    }
}
