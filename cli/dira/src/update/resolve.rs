//! Version + asset resolution against the GitHub Releases API.
//!
//! Mirrors `install.sh`'s `_resolve_unauthenticated` / `_resolve_authenticated`
//! functions (see the repo root `install.sh` and `docs/install.md`) closely
//! enough that the two stay easy to cross-check by eye: same env vars, same
//! two-path split (public asset URLs vs. bearer-authenticated asset ids for
//! the private-repo window), same asset naming (`dira-<version>-<target>.tar.gz`
//! / `.sha256` for the unix targets, `dira-<version>-<target>.zip` / `.sha256`
//! for the `windows`-containing ones (D-0010) — never `.tar.gz.sha256` /
//! `.zip.sha256`, see `taiki-e/upload-rust-binary-action`'s `checksum: sha256`
//! behavior).
//!
//! No CLI flag carries the repo/API base URL/download URL/token — those are
//! maintainer/CI/air-gapped knobs, not something an end user tunes per
//! invocation — so they're read straight from the environment here, exactly
//! like `install.sh`: `DIRA_REPO`, `DIRA_API_URL`, `DIRA_DOWNLOAD_URL`,
//! `GH_TOKEN`/`GITHUB_TOKEN` (`GH_TOKEN` wins if both are set, matching `gh`'s
//! own precedence). This also happens to be exactly what a test needs to
//! redirect resolution at a local mock server without touching the network.

use super::retry;
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

/// How `candidate` orders against `current` under SemVer 2.0 §11 — `Greater`
/// means `candidate` is genuinely newer. `None` when either side doesn't parse,
/// which callers must read as "make no claim", never as "different, therefore
/// newer".
///
/// This exists because that exact conflation was a bug: [`pick_latest`] above
/// has always ordered *releases against each other* correctly, but the two
/// places that compared a resolved release against the **running** version
/// (`update --check` and the passive notice) used `==` and treated any
/// inequality as an upgrade. A `0.1.1-develop.1` build therefore announced
/// stable `0.1.0` as "available" — a downgrade — because the strings differ.
/// One comparator, three callers, so they cannot drift again.
///
/// Both arguments accept a bare version or a `v`-prefixed tag.
pub fn compare_versions(candidate: &str, current: &str) -> Option<std::cmp::Ordering> {
    Some(tag_semver(candidate)?.cmp(&tag_semver(current)?))
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

/// GitHub answered 401 — the credential we sent was rejected.
///
/// A marker type rather than a formatted string so [`resolve`] can recognise
/// this one failure and recover from it without pattern-matching an error
/// message. Carried through `anyhow`'s context chain; use [`is_unauthorized`]
/// to test for it.
#[derive(Debug)]
pub struct Unauthorized;

impl std::fmt::Display for Unauthorized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GitHub rejected the token (401 Unauthorized)")
    }
}

impl std::error::Error for Unauthorized {}

/// True if `err` (or anything it wraps) is an [`Unauthorized`]. Walks the whole
/// chain, so a `.context(...)` added on the way up doesn't hide it.
pub fn is_unauthorized(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| cause.is::<Unauthorized>())
}

/// `GET` a GitHub Releases API path and return its body, retrying transport
/// failures, 5xx and 429 on the bounded ladder in [`retry::Policy::api`].
///
/// This hop used to be bounded by a timeout but never retried, so a single
/// transient abort here failed the whole update — the same stranded state the
/// download's retry was added to fix, one step earlier. It is also the hop
/// most exposed to a 429: anonymous API calls are capped at 60/hr per IP, and
/// on shared corporate egress that budget can be spent by other people.
///
/// A 4xx is never retried ([`retry::classify_status`]), so the
/// authenticated→anonymous fallback in [`resolve`] is unaffected: a 401 is
/// `Fatal`, surfaces immediately, and is caught there exactly as before.
async fn gh_get(http: &reqwest::Client, ctx: &GhContext, path: &str) -> Result<String> {
    let url = format!("{}{path}", ctx.api_url.trim_end_matches('/'));
    let policy = retry::Policy::api();
    retry::with_retry(policy, "release lookup", &url, || {
        gh_get_once(http, ctx, &url, policy.timeout)
    })
    .await
}

/// One attempt: request, status check, body read. Every failure is classified
/// here so [`gh_get`] only has to decide whether to wait.
async fn gh_get_once(
    http: &reqwest::Client,
    ctx: &GhContext,
    url: &str,
    timeout: std::time::Duration,
) -> std::result::Result<String, retry::Attempt> {
    // Per-request budget rather than a client-wide one: this is a small JSON
    // response, unlike the artifact download sharing the same client. `--check`
    // also runs speculatively from a detached background refresh, where hanging
    // is worse than missing a check. Rebuilt per attempt — a `RequestBuilder`
    // is consumed by `send`.
    let mut req = http
        .get(url)
        .timeout(timeout)
        .header(reqwest::header::USER_AGENT, user_agent())
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(token) = &ctx.token {
        req = req.bearer_auth(token);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| retry::Attempt::from_transport(e, format!("GET {url}")))?;

    let status = resp.status();
    if !status.is_success() {
        let retry_after = retry::parse_retry_after(resp.headers());
        // Best-effort: the body is only wanted for the message here, so a
        // failed read costs a less specific error, not a retry.
        let text = resp.text().await.unwrap_or_default();
        let err = if status == reqwest::StatusCode::UNAUTHORIZED && ctx.token.is_some() {
            // Typed, so `resolve` can drop the token and retry anonymously.
            // Only meaningful when we actually sent one: a 401 with no
            // credential is a genuine server-side problem, so that stays a
            // plain error.
            anyhow::Error::new(Unauthorized)
                .context(format!("GitHub API request failed (401) for {url}"))
        } else {
            anyhow::anyhow!(
                "GitHub API request failed ({status}) for {url}: {}",
                text.chars().take(300).collect::<String>()
            )
        };
        return Err(retry::Attempt::new(
            retry::classify_status(status),
            err,
            retry_after,
        ));
    }

    // The body read is inside the retried unit on purpose. #113's reported
    // failure — `connection closed before message completed` — surfaces here,
    // after `send()` has already returned `Ok`. This used to be
    // `unwrap_or_default()`, which turned a truncated response into an empty
    // body and then into a confusing downstream JSON parse error; a truncated
    // success is exactly what deserves another attempt.
    resp.text()
        .await
        .map_err(|e| retry::Attempt::from_transport(e, format!("read response body for {url}")))
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
    pub archive_name: String,
    pub sha_name: String,
    pub archive: AssetRef,
    pub sha: AssetRef,
}

/// The release archive extension for `target` — `.zip` for the `windows`
/// targets (D-0010: windows release assets are packaged as `.zip`, not
/// `.tar.gz`, since neither `tar`/`gzip` nor `Expand-Archive` can be assumed
/// present/scriptable the way `tar` is on every macOS/Linux target), `.tar.gz`
/// for everything else. String-driven off `target` (not `cfg!`) so a
/// `DIRA_TARGET` override still resolves the matching archive kind rather
/// than the host's own platform.
fn archive_extension(target: &str) -> &'static str {
    if target.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    }
}

fn asset_names(version: &str, target: &str) -> (String, String) {
    (
        format!("dira-{version}-{target}.{}", archive_extension(target)),
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

    if ctx.token.is_none() {
        return resolve_unauthenticated(http, &ctx, target, version_pin, channel).await;
    }

    match resolve_authenticated(http, &ctx, target, version_pin, channel).await {
        Err(e) if is_unauthorized(&e) => {
            // A token is an optimization on a public repo, never a requirement:
            // it only lifts GitHub's 60 req/hr anonymous per-IP limit. So a
            // token GitHub rejects must not be terminal — drop it and resolve
            // anonymously, the path every normal user takes.
            //
            // This mirrors install.sh / install.ps1, which hit the same wall:
            // an expired or wrong-account GITHUB_TOKEN/GH_TOKEN exported in the
            // user's shell (common, and nothing to do with dira) turned a
            // routine `dira update` into a hard 401 against a repo that needs
            // no credentials.
            //
            // Anonymous resolution also yields plain public asset URLs rather
            // than `AssetRef::ApiAsset`, so the download that follows stops
            // carrying the rejected bearer too.
            eprintln!(
                "warning: GITHUB_TOKEN/GH_TOKEN was rejected by GitHub (401) -- ignoring it \
                 and continuing anonymously. Unset or replace that token to silence this."
            );
            let anonymous = GhContext {
                token: None,
                ..ctx.clone()
            };
            resolve_unauthenticated(http, &anonymous, target, version_pin, channel).await
        }
        other => other,
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

    let (archive_name, sha_name) = asset_names(&version, target);
    let base = ctx
        .download_base
        .clone()
        .unwrap_or_else(|| format!("https://github.com/{}/releases/download/{tag}", ctx.repo));
    let base = base.trim_end_matches('/');

    Ok(Resolved {
        version,
        tag,
        archive: AssetRef::Url(format!("{base}/{archive_name}")),
        sha: AssetRef::Url(format!("{base}/{sha_name}")),
        archive_name,
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
    let (archive_name, sha_name) = asset_names(&version, target);
    let archive_id = find_asset_id(&release, &archive_name)?;
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
        archive: AssetRef::ApiAsset {
            url: asset_url(archive_id),
        },
        sha: AssetRef::ApiAsset {
            url: asset_url(sha_id),
        },
        archive_name,
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

    // --- compare_versions (#63) --------------------------------------------

    #[test]
    fn compare_versions_orders_a_prerelease_below_its_own_release() {
        use std::cmp::Ordering;
        // SemVer §11, and the exact case that produced the bug: stable 0.1.0 is
        // *older* than the 0.1.1-develop.1 prerelease, even though the strings
        // merely differ.
        assert_eq!(
            compare_versions("0.1.0", "0.1.1-develop.1"),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_versions("0.1.1-develop.1", "0.1.0"),
            Some(Ordering::Greater)
        );
        // A finished release outranks its own prerelease, and prerelease
        // identifiers compare numerically rather than lexically.
        assert_eq!(
            compare_versions("0.2.0", "0.2.0-develop.10"),
            Some(Ordering::Greater)
        );
        assert_eq!(
            compare_versions("0.2.0-develop.10", "0.2.0-develop.9"),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn compare_versions_is_equal_for_the_same_version() {
        assert_eq!(
            compare_versions("1.2.3", "1.2.3"),
            Some(std::cmp::Ordering::Equal)
        );
    }

    #[test]
    fn compare_versions_accepts_a_v_prefixed_tag_on_either_side() {
        assert_eq!(
            compare_versions("v1.2.3", "1.2.3"),
            Some(std::cmp::Ordering::Equal)
        );
        assert_eq!(
            compare_versions("1.2.3", "v1.2.3"),
            Some(std::cmp::Ordering::Equal)
        );
    }

    #[test]
    fn compare_versions_makes_no_claim_when_either_side_is_unparseable() {
        // `None` must mean "no claim". Callers treating it as "different,
        // therefore newer" is the bug this comparator replaced.
        assert!(compare_versions("not-a-version", "1.2.3").is_none());
        assert!(compare_versions("1.2.3", "not-a-version").is_none());
    }

    // --- asset_names / archive_extension (D-0010) --------------------------

    #[test]
    fn asset_names_uses_zip_for_windows_targets() {
        let (archive, sha) = asset_names("0.3.0", "x86_64-pc-windows-msvc");
        assert_eq!(archive, "dira-0.3.0-x86_64-pc-windows-msvc.zip");
        assert_eq!(sha, "dira-0.3.0-x86_64-pc-windows-msvc.sha256");

        let (archive, sha) = asset_names("0.3.0", "aarch64-pc-windows-msvc");
        assert_eq!(archive, "dira-0.3.0-aarch64-pc-windows-msvc.zip");
        assert_eq!(sha, "dira-0.3.0-aarch64-pc-windows-msvc.sha256");
    }

    #[test]
    fn asset_names_uses_tar_gz_for_unix_targets() {
        let (archive, sha) = asset_names("0.3.0", "x86_64-unknown-linux-musl");
        assert_eq!(archive, "dira-0.3.0-x86_64-unknown-linux-musl.tar.gz");
        assert_eq!(sha, "dira-0.3.0-x86_64-unknown-linux-musl.sha256");

        let (archive, sha) = asset_names("0.3.0", "universal-apple-darwin");
        assert_eq!(archive, "dira-0.3.0-universal-apple-darwin.tar.gz");
        assert_eq!(sha, "dira-0.3.0-universal-apple-darwin.sha256");
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

    // --- gh_get retry (#115) -------------------------------------------
    //
    // The API hop was bounded by a timeout but never retried, so one transient
    // abort here failed the whole update — the stranded state the download's
    // retry was added to fix, one step earlier. These drive the real `gh_get`
    // against the same raw-TCP server the download tests use, because axum
    // cannot express a half-written body.

    use crate::test_support::{scripted_server, Reply};
    use std::sync::atomic::Ordering;

    /// A minimal well-formed releases array, enough for `gh_get` to return it
    /// verbatim (parsing is asserted separately, against the real fixture).
    const ONE_RELEASE: &str =
        r#"[{"tag_name":"v1.2.3","prerelease":false,"draft":false,"assets":[]}]"#;

    async fn get_against(script: Vec<Reply>, token: Option<&str>) -> (Result<String>, usize) {
        let (base, hits) = scripted_server(script).await;
        let ctx = GhContext {
            api_url: base,
            repo: DEFAULT_REPO.to_string(),
            download_base: None,
            token: token.map(str::to_string),
        };
        let http = reqwest::Client::builder().build().unwrap();
        let out = gh_get(&http, &ctx, "/releases").await;
        (out, hits.load(Ordering::SeqCst))
    }

    /// The #113 mechanism, at the hop #115 is about: the stream dies part-way
    /// through a body that had already started arriving, *after* `send()`
    /// returned `Ok`. A driver wrapped around `send()` alone would not catch
    /// this, which is why the body read is inside the retried unit.
    #[tokio::test]
    async fn a_truncated_api_body_is_retried_and_the_next_attempt_succeeds() {
        let (out, hits) = get_against(vec![Reply::Truncated, Reply::Body(ONE_RELEASE)], None).await;
        assert_eq!(
            out.expect("a transient abort on the API hop should be retried"),
            ONE_RELEASE
        );
        assert_eq!(hits, 2, "should have taken exactly one retry");
    }

    /// The failure this hop is most exposed to: anonymous API calls are capped
    /// at 60/hr per IP, so on shared egress the budget can be spent by other
    /// people entirely.
    #[tokio::test]
    async fn a_429_is_retried() {
        let (out, hits) = get_against(
            vec![
                Reply::Status(429, "Retry-After: 0\r\n"),
                Reply::Body(ONE_RELEASE),
            ],
            None,
        )
        .await;
        assert_eq!(out.expect("429 should be retried"), ONE_RELEASE);
        assert_eq!(hits, 2);
    }

    #[tokio::test]
    async fn a_5xx_is_retried_until_the_budget_is_spent() {
        let (out, hits) = get_against(vec![Reply::Status(503, "")], None).await;
        let err = out.expect_err("an unrelenting 503 must eventually fail");
        assert!(
            format!("{err:#}").contains("release lookup failed after 3 attempts"),
            "error should name the exhausted budget, got: {err:#}"
        );
        assert_eq!(hits, retry::Policy::api().attempts as usize);
    }

    /// A 4xx is deterministic. Retrying it only delays a clear message — and
    /// for a 401 it would also delay the anonymous fallback below.
    #[tokio::test]
    async fn client_errors_fail_on_the_first_attempt() {
        for status in [404, 403, 400] {
            let (out, hits) = get_against(vec![Reply::Status(status, "")], None).await;
            out.expect_err("a 4xx must fail");
            assert_eq!(hits, 1, "{status} must not be retried");
        }
    }

    /// The token fallback is unaffected by the new ladder: a 401 classifies as
    /// `Fatal`, so it surfaces immediately and still carries the typed
    /// `Unauthorized` marker `resolve` downcasts to drop the token.
    #[tokio::test]
    async fn a_rejected_token_is_fatal_and_still_typed_for_the_fallback() {
        let (out, hits) = get_against(vec![Reply::Status(401, "")], Some("bad-token")).await;
        let err = out.expect_err("401 must fail");
        assert!(
            is_unauthorized(&err),
            "the typed marker must survive so resolve can retry anonymously: {err:#}"
        );
        assert_eq!(hits, 1, "a retried 401 would only delay the fallback");
    }

    /// Without a token, a 401 is a genuine server-side problem rather than a
    /// credential to drop — it must not masquerade as the fallback signal.
    #[tokio::test]
    async fn an_anonymous_401_is_not_the_typed_fallback_marker() {
        let (out, _) = get_against(vec![Reply::Status(401, "")], None).await;
        let err = out.expect_err("401 must fail");
        assert!(!is_unauthorized(&err), "got: {err:#}");
    }
}
