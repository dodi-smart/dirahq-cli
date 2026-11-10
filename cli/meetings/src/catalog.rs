//! The meeting-app catalogue (`data/apps.toml`): process identity -> [`Holder`].
//!
//! This is the one place a raw process identity (bundle id, executable name,
//! Flatpak id) is looked at. [`Catalog::classify`] returns a [`Holder`] that keeps
//! only the catalogue's own class; the identity string is not stored anywhere.

use std::fmt;

/// The embedded catalogue. Changing it is a data change (see the file header).
pub const APPS_TOML: &str = include_str!("../data/apps.toml");

/// How much corroboration a holder of this kind needs (see `apps.toml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AppKind {
    Platform,
    Messaging,
    Browser,
    OffByDefault,
}

/// What a holder is evidence of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HolderRole {
    /// A call app's ordinary process.
    Call,
    /// A process that exists only while a meeting is live (Zoom's `CptHost`).
    CallOnly,
    /// Never evidence of a call (dictation, recorders, note-takers).
    Deny,
    /// Sits between the mic and the call app; never starts a meeting alone.
    Relay,
}

/// The closed wire class (contract `providerClass`). Coarser than the catalogue,
/// so a new app never forces a contract or cloud change.
pub use dira_contract::ProviderClass;

fn parse_wire(s: &str) -> Option<ProviderClass> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
}

/// A process as the fusion sees it: a catalogue class, never an identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// Catalogue class (`"zoom"`), or `"deny"` / `"relay"` for those lists.
    pub class: &'static str,
    pub kind: AppKind,
    pub role: HolderRole,
    pub wire: ProviderClass,
}

#[derive(Debug, Clone)]
struct Entry {
    prefix: String,
    holder: Holder,
}

/// The loaded catalogue.
#[derive(Debug, Clone)]
pub struct Catalog {
    entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogError(String);

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "meeting catalogue: {}", self.0)
    }
}

impl std::error::Error for CatalogError {}

impl Catalog {
    /// The catalogue shipped in the binary, parsed once per process.
    pub fn embedded() -> &'static Self {
        static EMBEDDED: std::sync::OnceLock<Catalog> = std::sync::OnceLock::new();
        EMBEDDED.get_or_init(|| {
            Self::parse(APPS_TOML).expect("embedded apps.toml is valid (checked by tests)")
        })
    }

    pub fn parse(src: &str) -> Result<Self, CatalogError> {
        let doc: toml_edit::DocumentMut = src
            .parse()
            .map_err(|e: toml_edit::TomlError| CatalogError(e.to_string()))?;
        let mut entries = Vec::new();

        let apps = doc
            .get("app")
            .and_then(|a| a.as_array_of_tables())
            .ok_or_else(|| CatalogError("missing [[app]] entries".into()))?;
        for app in apps.iter() {
            let class = leak(str_field(app, "class")?);
            let kind = match str_field(app, "kind")?.as_str() {
                "platform" => AppKind::Platform,
                "messaging" => AppKind::Messaging,
                "browser" => AppKind::Browser,
                "off_by_default" => AppKind::OffByDefault,
                other => return Err(CatalogError(format!("{class}: unknown kind {other:?}"))),
            };
            let wire_s = str_field(app, "wire")?;
            let wire = parse_wire(&wire_s)
                .ok_or_else(|| CatalogError(format!("{class}: unknown wire class {wire_s:?}")))?;
            for (key, role) in [
                ("ids", HolderRole::Call),
                ("call_only", HolderRole::CallOnly),
            ] {
                for prefix in str_list(app.get(key))? {
                    entries.push(Entry {
                        prefix,
                        holder: Holder {
                            class,
                            kind,
                            role,
                            wire,
                        },
                    });
                }
            }
        }
        for (table, role) in [("deny", HolderRole::Deny), ("relay", HolderRole::Relay)] {
            let ids = doc.get(table).and_then(|t| t.get("ids"));
            for prefix in str_list(ids)? {
                entries.push(Entry {
                    prefix,
                    holder: Holder {
                        class: table_class(table),
                        kind: AppKind::OffByDefault,
                        role,
                        wire: ProviderClass::Other,
                    },
                });
            }
        }
        Ok(Self { entries })
    }

    /// Classify a process from every identity the adapter knows for it (bundle
    /// id, executable file name, Flatpak id). The longest case-insensitive prefix
    /// match wins. `None` means "not a call app": the process is ignored.
    pub fn classify(&self, identities: &[&str]) -> Option<Holder> {
        let mut best: Option<(&Entry, usize)> = None;
        for id in identities {
            let id = id.to_ascii_lowercase();
            for e in &self.entries {
                if id.starts_with(&e.prefix) && best.is_none_or(|(_, len)| e.prefix.len() > len) {
                    best = Some((e, e.prefix.len()));
                }
            }
        }
        best.map(|(e, _)| e.holder.clone())
    }

    /// Distinct catalogue classes, for `dira doctor` ("ignored: mumble, …").
    pub fn classes(&self) -> Vec<&'static str> {
        let mut v: Vec<&'static str> = self.entries.iter().map(|e| e.holder.class).collect();
        v.sort_unstable();
        v.dedup();
        v
    }
}

fn table_class(table: &str) -> &'static str {
    if table == "deny" {
        "deny"
    } else {
        "relay"
    }
}

fn str_field(t: &toml_edit::Table, key: &str) -> Result<String, CatalogError> {
    t.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| CatalogError(format!("an [[app]] is missing {key:?}")))
}

fn str_list(item: Option<&toml_edit::Item>) -> Result<Vec<String>, CatalogError> {
    let Some(item) = item else {
        return Ok(Vec::new());
    };
    let arr = item
        .as_array()
        .ok_or_else(|| CatalogError("ids must be an array of strings".into()))?;
    arr.iter()
        .map(|v| {
            v.as_str()
                .map(|s| s.to_ascii_lowercase())
                .ok_or_else(|| CatalogError("ids must be strings".into()))
        })
        .collect()
}

/// Catalogue classes live for the whole process (the embedded catalogue is
/// parsed once); leaking the handful of class names keeps `Holder` `'static` and
/// cheap to clone.
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalogue_parses() {
        let c = Catalog::embedded();
        for class in [
            "zoom", "teams", "slack", "discord", "chrome", "deny", "relay",
        ] {
            assert!(c.classes().contains(&class), "{class} missing");
        }
    }

    #[test]
    fn helpers_match_by_prefix_and_longest_wins() {
        let c = Catalog::embedded();
        let discord = c.classify(&["com.hnc.Discord.helper.Renderer"]).unwrap();
        assert_eq!(
            (discord.class, discord.kind),
            ("discord", AppKind::Messaging)
        );

        // Zoom's meeting-only helper beats the generic zoom prefix.
        let cpt = c.classify(&["CptHost"]).unwrap();
        assert_eq!((cpt.class, cpt.role), ("zoom", HolderRole::CallOnly));
        let zoom = c.classify(&["us.zoom.xos"]).unwrap();
        assert_eq!(zoom.role, HolderRole::Call);
        assert_eq!(zoom.wire, ProviderClass::Zoom);
    }

    #[test]
    fn any_identity_can_match() {
        let c = Catalog::embedded();
        let teams = c.classify(&["/opt/whatever", "ms-teams.exe"]).unwrap();
        assert_eq!(teams.class, "teams");
        assert!(c.classify(&["com.apple.Terminal"]).is_none());
    }

    #[test]
    fn deny_and_relay_are_roles() {
        let c = Catalog::embedded();
        assert_eq!(
            c.classify(&["com.apple.VoiceMemos"]).unwrap().role,
            HolderRole::Deny
        );
        assert_eq!(
            c.classify(&["ai.krisp.krispMac"]).unwrap().role,
            HolderRole::Relay
        );
    }

    #[test]
    fn a_bad_catalogue_is_an_error() {
        assert!(Catalog::parse("version = 1").is_err());
        let bad_kind = "[[app]]\nclass='x'\nkind='nope'\nwire='other'\nids=['x']\n";
        assert!(Catalog::parse(bad_kind).is_err());
        let bad_wire = "[[app]]\nclass='x'\nkind='platform'\nwire='skype'\nids=['x']\n";
        assert!(Catalog::parse(bad_wire).is_err());
    }
}
