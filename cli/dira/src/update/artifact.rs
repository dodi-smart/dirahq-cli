//! Target detection, artifact download, sha256 verification, and extraction.
//!
//! Mirrors `install.sh`'s equivalent functions (`detect_target`,
//! `_sha256_hex`/`_extract_expected_digest`/`verify_checksum`, and the `tar
//! -xzf` + presence/non-empty asserts) so the two implementations are easy to
//! cross-check by eye. See that file for the target-triple table and the
//! reasoning behind each check.

use super::resolve::AssetRef;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Detect this host's release target triple.
///
/// `DIRA_TARGET` overrides detection entirely — the same env var
/// `install.sh` honors via `--target`/`DIRA_TARGET`, useful for testing and
/// for the rare host `uname` misdetects.
///
/// Darwin always resolves to `universal-apple-darwin` regardless of arch:
/// the release ships a `lipo` fat binary covering Apple Silicon and Intel
/// from one artifact, so there is deliberately no Rosetta check and no
/// Intel-Mac branch here (see the production-distribution plan's §A1).
pub fn detect_target() -> Result<String> {
    if let Ok(t) = std::env::var("DIRA_TARGET") {
        if !t.is_empty() {
            return Ok(t);
        }
    }
    detect_target_for(std::env::consts::OS, std::env::consts::ARCH)
}

fn detect_target_for(os: &str, arch: &str) -> Result<String> {
    match os {
        "macos" => Ok("universal-apple-darwin".to_string()),
        "linux" => {
            let a = match arch {
                "x86_64" => "x86_64",
                "aarch64" => "aarch64",
                other => anyhow::bail!(
                    "unsupported Linux architecture: {other} (supported: x86_64, aarch64)"
                ),
            };
            Ok(format!("{a}-unknown-linux-musl"))
        }
        other => anyhow::bail!(
            "unsupported OS: {other} (supported targets: x86_64-unknown-linux-musl, \
             aarch64-unknown-linux-musl, universal-apple-darwin)"
        ),
    }
}

/// `User-Agent` header value. api.github.com rejects requests with none.
fn user_agent() -> String {
    format!("dira/{}", env!("CARGO_PKG_VERSION"))
}

/// Download `asset` to `dest`. Redirect-following is `reqwest`'s default and
/// matters here: GitHub 302s an unauthenticated asset URL to
/// `objects.githubusercontent.com`.
pub async fn download(http: &reqwest::Client, asset: &AssetRef, dest: &Path) -> Result<()> {
    let url = asset.url();
    let mut req = http
        .get(url)
        .header(reqwest::header::USER_AGENT, user_agent());
    if asset.is_authenticated() {
        req = req
            .header(reqwest::header::ACCEPT, "application/octet-stream")
            .header("X-GitHub-Api-Version", "2022-11-28");
    }
    let resp = req.send().await.with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("download failed: {url} returned 404 (asset not found on that release)");
    }
    if !status.is_success() {
        anyhow::bail!("download failed: {url} returned {status}");
    }
    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("read response body for {url}"))?;
    std::fs::write(dest, &bytes).with_context(|| format!("write {}", dest.display()))?;
    Ok(())
}

/// Parse a `sha256sum`-style checksum file and return the hex digest for
/// `want_name`.
///
/// The `.sha256` asset holds one line per asset built in that release job
/// (raw `sha256sum` output), so this must select the line whose filename
/// field matches — never just the first line. Tolerates both `sha256sum`'s
/// `<hash>  <name>` (two spaces, or a single space/tab) and `shasum -a
/// 256`'s `<hash> *<name>` (a leading `*` marking binary mode).
pub fn parse_sha256_file(contents: &str, want_name: &str) -> Result<String> {
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let hash = parts.next().unwrap_or("");
        let name = parts
            .next()
            .unwrap_or("")
            .trim_start()
            .trim_start_matches('*');
        if name.eq_ignore_ascii_case(want_name) {
            return Ok(hash.to_ascii_lowercase());
        }
    }
    anyhow::bail!("checksum file has no entry for {want_name}")
}

/// Hash `path` and compare against `expected_hex` (case-insensitive).
pub fn verify_sha256(path: &Path, expected_hex: &str) -> Result<()> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("open {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).with_context(|| format!("hash {}", path.display()))?;
    let actual = hex::encode(hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected_hex) {
        anyhow::bail!(
            "checksum mismatch for {}: expected {expected_hex}, got {actual} — download is \
             corrupt or tampered, aborting",
            path.display()
        );
    }
    Ok(())
}

/// Extract `tarball` (flat root — `dira`, `dirad` with no leading directory,
/// per `taiki-e/upload-rust-binary-action`'s `leading-dir: false` default)
/// into `dest_dir`, then assert both binaries exist and are non-empty —
/// catches a packaging-action layout change with a clear message instead of
/// a confusing later failure.
///
/// Shells out to `tar -xzf` rather than depending on the `tar`/`flate2`
/// crates: neither is in `Cargo.lock` today (unlike `sha2`/`hex`/`semver`),
/// so pulling them in would be genuinely new dependency weight for one
/// extraction step, and `tar` is already a hard requirement of `install.sh`
/// — present on every macOS (bsdtar) and Linux (GNU/busybox) target we ship.
pub fn extract(tarball: &Path, dest_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dest_dir)
        .with_context(|| format!("create extraction dir {}", dest_dir.display()))?;

    let status = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(tarball)
        .arg("-C")
        .arg(dest_dir)
        .status()
        .context(
            "failed to spawn `tar` — it is required to extract release archives and should be \
             on PATH on every macOS or Linux system",
        )?;
    if !status.success() {
        anyhow::bail!("tar -xzf {} failed ({status})", tarball.display());
    }

    for name in ["dira", "dirad"] {
        let p = dest_dir.join(name);
        let meta = std::fs::metadata(&p).with_context(|| {
            format!(
                "downloaded archive is missing '{name}' at its root — packaging layout may have \
                 changed"
            )
        })?;
        if meta.len() == 0 {
            anyhow::bail!("downloaded '{name}' binary is empty");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- detect_target ------------------------------------------------------

    #[test]
    fn detect_target_darwin_is_always_universal_regardless_of_arch() {
        assert_eq!(
            detect_target_for("macos", "x86_64").unwrap(),
            "universal-apple-darwin"
        );
        assert_eq!(
            detect_target_for("macos", "aarch64").unwrap(),
            "universal-apple-darwin"
        );
    }

    #[test]
    fn detect_target_linux_maps_known_arches() {
        assert_eq!(
            detect_target_for("linux", "x86_64").unwrap(),
            "x86_64-unknown-linux-musl"
        );
        assert_eq!(
            detect_target_for("linux", "aarch64").unwrap(),
            "aarch64-unknown-linux-musl"
        );
    }

    #[test]
    fn detect_target_rejects_unsupported_linux_arch() {
        assert!(detect_target_for("linux", "riscv64").is_err());
    }

    #[test]
    fn detect_target_rejects_unsupported_os() {
        assert!(detect_target_for("windows", "x86_64").is_err());
    }

    #[test]
    fn dira_target_env_overrides_detection() {
        let _guard = super::super::test_env_lock();
        std::env::set_var("DIRA_TARGET", "custom-target-triple");
        let t = detect_target().unwrap();
        std::env::remove_var("DIRA_TARGET");
        assert_eq!(t, "custom-target-triple");
    }

    // --- parse_sha256_file ---------------------------------------------------

    #[test]
    fn parse_sha256_file_selects_the_matching_filename_not_the_first_line() {
        let contents = "\
aaaa000000000000000000000000000000000000000000000000000000000  dira-0.2.0-aarch64-unknown-linux-musl.tar.gz
bbbb111111111111111111111111111111111111111111111111111111111  dira-0.2.0-x86_64-unknown-linux-musl.tar.gz
cccc222222222222222222222222222222222222222222222222222222222  dira-0.2.0-universal-apple-darwin.tar.gz
";
        let hash =
            parse_sha256_file(contents, "dira-0.2.0-x86_64-unknown-linux-musl.tar.gz").unwrap();
        assert_eq!(
            hash,
            "bbbb111111111111111111111111111111111111111111111111111111111"
        );
    }

    #[test]
    fn parse_sha256_file_tolerates_shasum_star_prefix() {
        let contents = "deadbeef00000000000000000000000000000000000000000000000000000 *dira-0.2.0-universal-apple-darwin.tar.gz\n";
        let hash = parse_sha256_file(contents, "dira-0.2.0-universal-apple-darwin.tar.gz").unwrap();
        assert_eq!(
            hash,
            "deadbeef00000000000000000000000000000000000000000000000000000"
        );
    }

    #[test]
    fn parse_sha256_file_is_case_insensitive_on_the_filename_and_lowercases_the_hash() {
        let contents = "ABCDEF00000000000000000000000000000000000000000000000000000A  DIRA-0.2.0-X86_64-UNKNOWN-LINUX-MUSL.TAR.GZ\n";
        let hash =
            parse_sha256_file(contents, "dira-0.2.0-x86_64-unknown-linux-musl.tar.gz").unwrap();
        assert_eq!(
            hash,
            "abcdef00000000000000000000000000000000000000000000000000000a"
        );
    }

    #[test]
    fn parse_sha256_file_missing_entry_errors() {
        let contents = "aaaa  dira-0.2.0-x86_64-unknown-linux-musl.tar.gz\n";
        assert!(
            parse_sha256_file(contents, "dira-0.2.0-aarch64-unknown-linux-musl.tar.gz").is_err()
        );
    }

    #[test]
    fn parse_sha256_file_skips_blank_lines() {
        let contents = "\n\naaaa  dira-x.tar.gz\n\n";
        assert_eq!(
            parse_sha256_file(contents, "dira-x.tar.gz").unwrap(),
            "aaaa"
        );
    }

    // --- verify_sha256 --------------------------------------------------------

    #[test]
    fn verify_sha256_accepts_the_correct_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload.bin");
        std::fs::write(&path, b"hello world").unwrap();
        // sha256("hello world")
        let expected = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        verify_sha256(&path, expected).unwrap();
        // Case-insensitive.
        verify_sha256(&path, &expected.to_ascii_uppercase()).unwrap();
    }

    #[test]
    fn verify_sha256_rejects_a_mismatched_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload.bin");
        std::fs::write(&path, b"hello world").unwrap();
        let wrong = "0000000000000000000000000000000000000000000000000000000000000";
        assert!(verify_sha256(&path, wrong).is_err());
    }

    // --- extract ---------------------------------------------------------------

    fn build_tarball(dir: &Path, entries: &[(&str, &[u8])]) -> std::path::PathBuf {
        let root = dir.join("root");
        std::fs::create_dir_all(&root).unwrap();
        for (name, content) in entries {
            std::fs::write(root.join(name), content).unwrap();
        }
        let tarball = dir.join("archive.tar.gz");
        let status = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&tarball)
            .arg("-C")
            .arg(&root)
            .args(entries.iter().map(|(n, _)| *n))
            .status()
            .expect("spawn tar to build the test fixture archive");
        assert!(
            status.success(),
            "building the test archive with tar failed"
        );
        tarball
    }

    #[test]
    fn extract_flat_archive_with_both_binaries_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let tarball = build_tarball(
            dir.path(),
            &[("dira", b"fake-dira"), ("dirad", b"fake-dirad")],
        );
        let dest = dir.path().join("out");
        extract(&tarball, &dest).unwrap();
        assert_eq!(std::fs::read(dest.join("dira")).unwrap(), b"fake-dira");
        assert_eq!(std::fs::read(dest.join("dirad")).unwrap(), b"fake-dirad");
    }

    #[test]
    fn extract_missing_dirad_gives_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let tarball = build_tarball(dir.path(), &[("dira", b"fake-dira")]);
        let dest = dir.path().join("out");
        let err = extract(&tarball, &dest).unwrap_err();
        assert!(err.to_string().contains("dirad"), "error was: {err}");
    }

    #[test]
    fn extract_empty_binary_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let tarball = build_tarball(dir.path(), &[("dira", b""), ("dirad", b"fake-dirad")]);
        let dest = dir.path().join("out");
        let err = extract(&tarball, &dest).unwrap_err();
        assert!(err.to_string().contains("empty"), "error was: {err}");
    }
}
