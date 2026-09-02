//! Pure repo-fact classification for telemetry. No I/O and no store access —
//! callers supply the canonical remote, the install salt, and any visibility
//! they already resolved; this module only classifies and hashes.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Which forge a canonical remote belongs to. Never carries owner/repo — the
/// point is to say "this is a GitHub project" without saying which one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoHostClass {
    GitHub,
    GitLab,
    Bitbucket,
    SelfHosted,
}

impl RepoHostClass {
    /// The lowercase wire spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            RepoHostClass::GitHub => "github",
            RepoHostClass::GitLab => "gitlab",
            RepoHostClass::Bitbucket => "bitbucket",
            RepoHostClass::SelfHosted => "self_hosted",
        }
    }
}

/// Repo visibility, when the caller can determine it. `Unknown` (not a default
/// guess of `Private`) is deliberate: WP1 has no visibility source of its own,
/// so every caller until a later work package resolves this passes `Unknown`
/// rather than a fabricated answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoVisibility {
    Public,
    Private,
    Unknown,
}

impl RepoVisibility {
    /// The lowercase wire spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            RepoVisibility::Public => "public",
            RepoVisibility::Private => "private",
            RepoVisibility::Unknown => "unknown",
        }
    }
}

/// The repo facts attached to a `CommandExecuted` event: enough to segment
/// usage by forge and visibility, plus a salted hash that lets the SAME repo
/// be recognized across events from one install without identifying which
/// repo it is.
#[derive(Debug, Clone)]
pub struct RepoFacts {
    pub host_class: RepoHostClass,
    pub visibility: RepoVisibility,
    pub repo_hash: String,
}

/// Classify a canonical remote's host. `canonical` is
/// [`crate::project::canonicalize_remote`]'s output — lowercase `host/owner/repo`
/// — so the host is always its first `/`-separated segment.
pub fn classify_host(canonical: &str) -> RepoHostClass {
    match canonical.split('/').next().unwrap_or("") {
        "github.com" => RepoHostClass::GitHub,
        "gitlab.com" => RepoHostClass::GitLab,
        "bitbucket.org" => RepoHostClass::Bitbucket,
        _ => RepoHostClass::SelfHosted,
    }
}

/// Derive [`RepoFacts`] for a canonical remote: the host class, the
/// caller-supplied visibility, and `repo_hash = hex(HMAC-SHA256(salt, canonical))`.
///
/// Keying the hash on `salt` (not a bare digest of `canonical`) is the whole
/// point: two installs hashing the same repo produce unrelated hashes, and
/// the hash cannot be reversed to the plain remote without the salt, which
/// never leaves the machine (see [`super::identity`]).
pub fn compute(canonical: &str, salt: &[u8; 32], visibility: RepoVisibility) -> RepoFacts {
    // `new_from_slice` only fails for MACs with a fixed key length; HMAC accepts
    // any key length (short keys are zero-padded, long ones pre-hashed), so a
    // 32-byte salt can never trip this.
    let mut mac = HmacSha256::new_from_slice(salt).expect("HMAC-SHA256 accepts any key length");
    mac.update(canonical.as_bytes());
    let repo_hash = hex::encode(mac.finalize().into_bytes());
    RepoFacts {
        host_class: classify_host(canonical),
        visibility,
        repo_hash,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_forges() {
        let cases = [
            ("github.com/acme/api", RepoHostClass::GitHub),
            ("gitlab.com/acme/api", RepoHostClass::GitLab),
            ("bitbucket.org/acme/api", RepoHostClass::Bitbucket),
            ("git.acme.internal/acme/api", RepoHostClass::SelfHosted),
            ("", RepoHostClass::SelfHosted),
        ];
        for (canonical, want) in cases {
            assert_eq!(classify_host(canonical), want, "canonical={canonical}");
        }
    }

    #[test]
    fn as_str_is_lowercase_snake() {
        assert_eq!(RepoHostClass::GitHub.as_str(), "github");
        assert_eq!(RepoHostClass::GitLab.as_str(), "gitlab");
        assert_eq!(RepoHostClass::Bitbucket.as_str(), "bitbucket");
        assert_eq!(RepoHostClass::SelfHosted.as_str(), "self_hosted");
        assert_eq!(RepoVisibility::Public.as_str(), "public");
        assert_eq!(RepoVisibility::Private.as_str(), "private");
        assert_eq!(RepoVisibility::Unknown.as_str(), "unknown");
    }

    #[test]
    fn same_canonical_and_salt_is_deterministic() {
        let salt = [7u8; 32];
        let a = compute("github.com/acme/api", &salt, RepoVisibility::Unknown);
        let b = compute("github.com/acme/api", &salt, RepoVisibility::Unknown);
        assert_eq!(a.repo_hash, b.repo_hash);
    }

    #[test]
    fn different_salt_diverges() {
        let a = compute("github.com/acme/api", &[1u8; 32], RepoVisibility::Unknown);
        let b = compute("github.com/acme/api", &[2u8; 32], RepoVisibility::Unknown);
        assert_ne!(a.repo_hash, b.repo_hash);
    }

    #[test]
    fn different_repo_diverges_under_the_same_salt() {
        let salt = [9u8; 32];
        let a = compute("github.com/acme/api", &salt, RepoVisibility::Unknown);
        let b = compute("github.com/acme/other", &salt, RepoVisibility::Unknown);
        assert_ne!(a.repo_hash, b.repo_hash);
    }

    #[test]
    fn repo_hash_is_hex() {
        let facts = compute("github.com/acme/api", &[0u8; 32], RepoVisibility::Unknown);
        assert_eq!(facts.repo_hash.len(), 64); // SHA-256 -> 32 bytes -> 64 hex chars
        assert!(facts.repo_hash.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
