//! Version + asset resolution against the GitHub Releases API.
//!
//! Mirrors `install.sh`'s `_resolve_unauthenticated` / `_resolve_authenticated`
//! functions (see the repo root `install.sh` and `docs/install.md`) closely
//! enough that the two stay easy to cross-check by eye: same env vars, same
//! two-path split (public asset URLs vs. bearer-authenticated asset ids for
//! the private-repo window), same asset naming (`dira-<version>-<target>.tar.gz`
//! / `.sha256`, NOT `.tar.gz.sha256` — see `taiki-e/upload-rust-binary-action`'s
//! `checksum: sha256` behavior).
//!
//! No CLI flag carries the repo/API base URL/download URL/token — those are
//! maintainer/CI/air-gapped knobs, not something an end user tunes per
//! invocation — so they're read straight from the environment here, exactly
//! like `install.sh`: `DIRA_REPO`, `DIRA_API_URL`, `DIRA_DOWNLOAD_URL`,
//! `GH_TOKEN`/`GITHUB_TOKEN` (`GH_TOKEN` wins if both are set, matching `gh`'s
//! own precedence). This also happens to be exactly what a test needs to
//! redirect resolution at a local mock server without touching the network.

use super::Channel;
use anyhow::{Context, Result};
use serde::Deserialize;

const DEFAULT_API_URL: &str = "https://api.github.com";
const DEFAULT_REPO: &str = "dodi-smart/dirahq-cli";

/// One asset attached to a GitHub release.
#[derive(Debug, Clone, Deserialize)]
pub struct RawAsset {
    pub id: u64,
    pub name: String,
    #[allow(dead_code)]
    // carried for parity with the API shape; unused on the unauthenticated path
    pub browser_download_url: String,
}

/// One GitHub release, as returned by `/releases`, `/releases/latest`, or
/// `/releases/tags/<tag>`.
#[derive(Debug, Clone, Deserialize)]
pub struct RawRelease {
    pub tag_name: String,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub assets: Vec<RawAsset>,
}

/// Parse a GitHub `/releases?per_page=…` array response (used for the
/// prerelease channel's tag listing).
pub fn parse_releases(json: &str) -> Result<Vec<RawRelease>> {
    serde_json::from_str(json).context("parse GitHub releases list JSON")
}

/// Parse a single-release GitHub response (`/releases/latest`,
/// `/releases/tags/<tag>`).
pub fn parse_release(json: &str) -> Result<RawRelease> {
    serde_json::from_str(json).context("parse GitHub release JSON")
}

/// The default channel when `--channel` is not given.
pub fn default_channel() -> Channel {
    Channel::Stable
}

/// A tag's bare SemVer, or `None` if it doesn't parse (a release we can't
/// order is a release we skip, not a crash).
fn tag_semver(tag: &str) -> Option<semver::Version> {
    semver::Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()
}

/// Pick the newest non-draft release for `channel`.
///
/// Stable only considers non-prerelease releases (mirrors GitHub's own
/// `/releases/latest`, which never returns a prerelease). Prerelease
/// considers every non-draft release — stable and prerelease alike — and
/// lets SemVer 2.0.0 ordering pick the winner, so a stable release still
/// outranks a same-core-version prerelease (`0.2.0` > `0.2.0-develop.10`) and
/// prerelease identifiers compare numerically, not lexically:
/// `0.2.0-develop.9 < 0.2.0-develop.10 < 0.2.0`.
pub fn pick_latest(releases: &[RawRelease], channel: Channel) -> Option<&RawRelease> {
    releases
        .iter()
        .filter(|r| !r.draft)
        .filter(|r| channel == Channel::Prerelease || !r.prerelease)
        .filter_map(|r| tag_semver(&r.tag_name).map(|v| (v, r)))
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, r)| r)
}

/// GitHub API context, read from the environment (see the module docs for
/// why these aren't `UpdateArgs` fields).
#[derive(Debug, Clone)]
pub struct GhContext {
    pub api_url: String,
    pub repo: String,
    pub download_base: Option<String>,
    pub token: Option<String>,
}

impl GhContext {
    pub fn from_env() -> Self {
        let non_empty = |v: Result<String, std::env::VarError>| v.ok().filter(|s| !s.is_empty());
        Self {
            api_url: non_empty(std::env::var("DIRA_API_URL"))
                .unwrap_or_else(|| DEFAULT_API_URL.to_string()),
            repo: non_empty(std::env::var("DIRA_REPO")).unwrap_or_else(|| DEFAULT_REPO.to_string()),
            download_base: non_empty(std::env::var("DIRA_DOWNLOAD_URL")),
            token: non_empty(std::env::var("GH_TOKEN"))
                .or_else(|| non_empty(std::env::var("GITHUB_TOKEN"))),
        }
    }
}

/// `User-Agent` header value. api.github.com rejects requests with none.
fn user_agent() -> String {
    format!("dira/{}", env!("CARGO_PKG_VERSION"))
}

async fn gh_get(http: &reqwest::Client, ctx: &GhContext, path: &str) -> Result<String> {
    let url = format!("{}{path}", ctx.api_url.trim_end_matches('/'));
    let mut req = http
        .get(&url)
        .header(reqwest::header::USER_AGENT, user_agent())
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(token) = &ctx.token {
        req = req.bearer_auth(token);
    }
    let resp = req.send().await.with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "GitHub API request failed ({status}) for {url}: {}",
            text.chars().take(300).collect::<String>()
        );
    }
    Ok(text)
}

/// Where to `GET` an asset from, and which headers that `GET` needs.
#[derive(Debug, Clone)]
pub enum AssetRef {
    /// A public, unauthenticated URL — `browser_download_url`-shaped, or
    /// `DIRA_DOWNLOAD_URL`-overridden.
    Url(String),
    /// `{api_url}/repos/{repo}/releases/assets/{id}`, which requires
    /// `Accept: application/octet-stream` and a bearer token — the path
    /// that works for a private repo (`browser_download_url` does not).
    ApiAsset { url: String },
}

impl AssetRef {
    pub fn url(&self) -> &str {
        match self {
            AssetRef::Url(u) => u,
            AssetRef::ApiAsset { url } => url,
        }
    }

    pub fn is_authenticated(&self) -> bool {
        matches!(self, AssetRef::ApiAsset { .. })
    }
}

/// A resolved release + the two assets `dira update` needs.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub version: String,
    pub tag: String,
    pub tarball_name: String,
    pub sha_name: String,
    pub tarball: AssetRef,
    pub sha: AssetRef,
}

fn asset_names(version: &str, target: &str) -> (String, String) {
    (
        format!("dira-{version}-{target}.tar.gz"),
        format!("dira-{version}-{target}.sha256"),
    )
}

fn find_asset_id(release: &RawRelease, name: &str) -> Result<u64> {
    release
        .assets
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case(name))
        .map(|a| a.id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "release {} has no asset named {name} — was it built for this target?",
                release.tag_name
            )
        })
}

/// Resolve the release + assets to install: an exact `version_pin`, or the
/// newest release on `channel`.
pub async fn resolve(
    http: &reqwest::Client,
    target: &str,
    version_pin: Option<&str>,
    channel: Channel,
) -> Result<Resolved> {
    let ctx = GhContext::from_env();

    if ctx.token.is_some() {
        resolve_authenticated(http, &ctx, target, version_pin, channel).await
    } else {
        resolve_unauthenticated(http, &ctx, target, version_pin, channel).await
    }
}

/// The path every real end user takes: no token, so asset URLs are
/// constructed directly (never id-looked-up) and, for a version pin, no API
/// call happens at all.
async fn resolve_unauthenticated(
    http: &reqwest::Client,
    ctx: &GhContext,
    target: &str,
    version_pin: Option<&str>,
    channel: Channel,
) -> Result<Resolved> {
    let (version, tag) = match version_pin {
        Some(v) => {
            let version = v.strip_prefix('v').unwrap_or(v).to_string();
            (version.clone(), format!("v{version}"))
        }
        None if channel == Channel::Prerelease => {
            let body = gh_get(
                http,
                ctx,
                &format!("/repos/{}/releases?per_page=30", ctx.repo),
            )
            .await
            .with_context(|| {
                format!(
                    "failed to list releases for {} (network error, or the repo is private \
                         — set GITHUB_TOKEN/GH_TOKEN)",
                    ctx.repo
                )
            })?;
            let releases = parse_releases(&body)?;
            let picked = pick_latest(&releases, Channel::Prerelease)
                .ok_or_else(|| anyhow::anyhow!("no releases found for {}", ctx.repo))?;
            let version = picked
                .tag_name
                .strip_prefix('v')
                .unwrap_or(&picked.tag_name)
                .to_string();
            (version, picked.tag_name.clone())
        }
        None => {
            let body = gh_get(http, ctx, &format!("/repos/{}/releases/latest", ctx.repo))
                .await
                .with_context(|| {
                    format!(
                        "failed to resolve the latest stable release for {} (no stable release \
                         yet? try --channel prerelease, or the repo is private — set \
                         GITHUB_TOKEN/GH_TOKEN)",
                        ctx.repo
                    )
                })?;
            let release = parse_release(&body)?;
            let version = release
                .tag_name
                .strip_prefix('v')
                .unwrap_or(&release.tag_name)
                .to_string();
            (version, release.tag_name.clone())
        }
    };

    let (tarball_name, sha_name) = asset_names(&version, target);
    let base = ctx
        .download_base
        .clone()
        .unwrap_or_else(|| format!("https://github.com/{}/releases/download/{tag}", ctx.repo));
    let base = base.trim_end_matches('/');

    Ok(Resolved {
        version,
        tag,
        tarball: AssetRef::Url(format!("{base}/{tarball_name}")),
        sha: AssetRef::Url(format!("{base}/{sha_name}")),
        tarball_name,
        sha_name,
    })
}

/// The private-repo (or maintainer/CI) path: resolve asset **ids** and
/// download them via `Accept: application/octet-stream`, since
/// `browser_download_url` is not bearer-fetchable on a private repo.
async fn resolve_authenticated(
    http: &reqwest::Client,
    ctx: &GhContext,
    target: &str,
    version_pin: Option<&str>,
    channel: Channel,
) -> Result<Resolved> {
    let release = match version_pin {
        Some(v) => {
            let version = v.strip_prefix('v').unwrap_or(v).to_string();
            let tag = format!("v{version}");
            let body = gh_get(
                http,
                ctx,
                &format!("/repos/{}/releases/tags/{tag}", ctx.repo),
            )
            .await
            .with_context(|| {
                format!(
                    "failed to resolve release {tag} for {} (authenticated) — does that tag \
                         have a published release?",
                    ctx.repo
                )
            })?;
            parse_release(&body)?
        }
        None if channel == Channel::Prerelease => {
            let body = gh_get(
                http,
                ctx,
                &format!("/repos/{}/releases?per_page=30", ctx.repo),
            )
            .await
            .with_context(|| format!("failed to list releases for {} (authenticated)", ctx.repo))?;
            let releases = parse_releases(&body)?;
            pick_latest(&releases, Channel::Prerelease)
                .ok_or_else(|| anyhow::anyhow!("no releases found for {}", ctx.repo))?
                .clone()
        }
        None => {
            let body = gh_get(http, ctx, &format!("/repos/{}/releases/latest", ctx.repo))
                .await
                .with_context(|| {
                    format!(
                        "failed to resolve the latest stable release for {} (authenticated) — \
                         does a stable release exist yet? try --channel prerelease",
                        ctx.repo
                    )
                })?;
            parse_release(&body)?
        }
    };

    let version = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name)
        .to_string();
    let (tarball_name, sha_name) = asset_names(&version, target);
    let tarball_id = find_asset_id(&release, &tarball_name)?;
    let sha_id = find_asset_id(&release, &sha_name)?;

    let asset_url = |id: u64| {
        format!(
            "{}/repos/{}/releases/assets/{id}",
            ctx.api_url.trim_end_matches('/'),
            ctx.repo
        )
    };

    Ok(Resolved {
        version,
        tag: release.tag_name.clone(),
        tarball: AssetRef::ApiAsset {
            url: asset_url(tarball_id),
        },
        sha: AssetRef::ApiAsset {
            url: asset_url(sha_id),
        },
        tarball_name,
        sha_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, prerelease: bool, draft: bool) -> RawRelease {
        RawRelease {
            tag_name: tag.to_string(),
            prerelease,
            draft,
            assets: vec![],
        }
    }

    // --- parse_releases / parse_release --------------------------------

    fn fixture() -> String {
        std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/release.json"
        ))
        .expect("read tests/fixtures/release.json")
    }

    #[test]
    fn parse_releases_reads_the_captured_fixture() {
        let releases = parse_releases(&fixture()).expect("parse fixture");
        assert!(!releases.is_empty());
        assert!(releases.iter().any(|r| r.tag_name == "v0.1.0-develop.10"));
        assert!(releases.iter().any(|r| r.tag_name == "v0.1.0-develop.9"));
        let stable = releases
            .iter()
            .find(|r| r.tag_name == "v0.1.0")
            .expect("fixture has a stable v0.1.0 release");
        assert!(!stable.prerelease);
        assert!(stable.assets.iter().any(|a| a.name.contains("tar.gz")));
    }

    #[test]
    fn parse_release_reads_a_single_object() {
        // `parse_release` is the `/releases/latest` / `/releases/tags/<tag>`
        // shape: a single JSON object, not the array `parse_releases` reads.
        // Exercise it against the first element of the real fixture (not a
        // hand-rolled literal) by re-serializing just that element.
        let arr: serde_json::Value = serde_json::from_str(&fixture()).unwrap();
        let first = serde_json::to_string(&arr[0]).unwrap();
        let parsed = parse_release(&first).expect("parse single release");
        assert_eq!(parsed.tag_name, "v0.1.0-develop.10");
        assert!(!parsed.assets.is_empty());
    }

    // --- pick_latest ordering -------------------------------------------

    #[test]
    fn pick_latest_orders_develop_prereleases_numerically_not_lexically() {
        let releases = vec![
            release("v0.2.0-develop.2", true, false),
            release("v0.2.0-develop.10", true, false),
            release("v0.2.0-develop.9", true, false),
        ];
        let picked = pick_latest(&releases, Channel::Prerelease).unwrap();
        assert_eq!(
            picked.tag_name, "v0.2.0-develop.10",
            "0.2.0-develop.9 < 0.2.0-develop.10 must hold under numeric SemVer prerelease \
             comparison, not string comparison (which would put .9 last)"
        );
    }

    #[test]
    fn pick_latest_prefers_a_stable_release_over_a_same_core_prerelease() {
        let releases = vec![
            release("v0.2.0-develop.10", true, false),
            release("v0.2.0", false, false),
        ];
        let picked = pick_latest(&releases, Channel::Prerelease).unwrap();
        assert_eq!(picked.tag_name, "v0.2.0");
    }

    #[test]
    fn pick_latest_stable_channel_ignores_prereleases_entirely() {
        let releases = vec![
            release("v0.2.0-develop.99", true, false),
            release("v0.1.0", false, false),
        ];
        let picked = pick_latest(&releases, Channel::Stable).unwrap();
        assert_eq!(picked.tag_name, "v0.1.0");
    }

    #[test]
    fn pick_latest_skips_drafts() {
        let releases = vec![
            release("v0.3.0", false, true),
            release("v0.2.0", false, false),
        ];
        let picked = pick_latest(&releases, Channel::Stable).unwrap();
        assert_eq!(picked.tag_name, "v0.2.0");
    }

    #[test]
    fn pick_latest_empty_input_is_none() {
        assert!(pick_latest(&[], Channel::Stable).is_none());
    }

    #[test]
    fn pick_latest_skips_unparseable_tags_rather_than_panicking() {
        let releases = vec![
            release("not-a-version", false, false),
            release("v0.1.0", false, false),
        ];
        let picked = pick_latest(&releases, Channel::Stable).unwrap();
        assert_eq!(picked.tag_name, "v0.1.0");
    }

    // --- default_channel --------------------------------------------------

    #[test]
    fn default_channel_is_stable() {
        assert_eq!(default_channel(), Channel::Stable);
    }

    // --- GhContext::from_env -----------------------------------------------

    #[test]
    fn gh_context_defaults_match_install_sh() {
        // Run in isolation from other env-mutating tests in this binary.
        let _guard = super::super::test_env_lock();
        for var in [
            "DIRA_API_URL",
            "DIRA_REPO",
            "DIRA_DOWNLOAD_URL",
            "GH_TOKEN",
            "GITHUB_TOKEN",
        ] {
            std::env::remove_var(var);
        }
        let ctx = GhContext::from_env();
        assert_eq!(ctx.api_url, DEFAULT_API_URL);
        assert_eq!(ctx.repo, DEFAULT_REPO);
        assert_eq!(ctx.download_base, None);
        assert_eq!(ctx.token, None);
    }

    #[test]
    fn gh_context_gh_token_wins_over_github_token() {
        let _guard = super::super::test_env_lock();
        std::env::set_var("GH_TOKEN", "gh-wins");
        std::env::set_var("GITHUB_TOKEN", "should-not-win");
        let ctx = GhContext::from_env();
        assert_eq!(ctx.token.as_deref(), Some("gh-wins"));
        std::env::remove_var("GH_TOKEN");
        std::env::remove_var("GITHUB_TOKEN");
    }
}
