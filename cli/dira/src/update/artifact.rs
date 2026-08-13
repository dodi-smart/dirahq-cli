//! Target detection, artifact download, sha256 verification, and extraction.
//!
//! Mirrors `install.sh`'s equivalent functions (`detect_target`,
//! `_sha256_hex`/`_extract_expected_digest`/`verify_checksum`, and the `tar
//! -xzf` + presence/non-empty asserts) so the two implementations are easy to
//! cross-check by eye. See that file for the target-triple table and the
//! reasoning behind each check.

use super::resolve::AssetRef;
use super::retry;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;
use std::time::Duration;

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
        "windows" => {
            let a = match arch {
                "x86_64" => "x86_64",
                "aarch64" => "aarch64",
                other => anyhow::bail!(
                    "unsupported Windows architecture: {other} (supported: x86_64, aarch64)"
                ),
            };
            Ok(format!("{a}-pc-windows-msvc"))
        }
        other => anyhow::bail!(
            "unsupported OS: {other} (supported targets: x86_64-unknown-linux-musl, \
             aarch64-unknown-linux-musl, universal-apple-darwin, x86_64-pc-windows-msvc, \
             aarch64-pc-windows-msvc)"
        ),
    }
}

/// `User-Agent` header value. api.github.com rejects requests with none.
fn user_agent() -> String {
    format!("dira/{}", env!("CARGO_PKG_VERSION"))
}

/// A failed attempt, tagged with whether another one could plausibly succeed.
enum Attempt {
    /// Deterministic — surface it immediately, unchanged.
    Fatal(anyhow::Error),
    /// Transient. `retry_after` carries a server-supplied delay when there was
    /// a response to read one from (a 429); a dead connection has none.
    Transient {
        err: anyhow::Error,
        retry_after: Option<Duration>,
    },
}

/// Download `asset` to `dest`. Redirect-following is `reqwest`'s default and
/// matters here: GitHub 302s an unauthenticated asset URL to
/// `objects.githubusercontent.com`.
///
/// Retries transport failures, timeouts, 5xx and 429 on a bounded ladder — see
/// [`retry`] for the policy and for why a 4xx (notably the 404 below) is never
/// retried. A single mid-stream abort on a lossy link used to fail the whole
/// update; the installers have always retried (`install.ps1`, `install.sh`'s
/// `curl --retry 3`) and this closes the gap.
pub async fn download(http: &reqwest::Client, asset: &AssetRef, dest: &Path) -> Result<()> {
    download_with(http, asset, dest, retry::Policy::download()).await
}

async fn download_with(
    http: &reqwest::Client,
    asset: &AssetRef,
    dest: &Path,
    policy: retry::Policy,
) -> Result<()> {
    let url = asset.url();
    let mut backoff = Duration::ZERO;

    for attempt in 1..=policy.attempts {
        match download_once(http, asset, dest, policy.timeout).await {
            Ok(()) => return Ok(()),
            Err(Attempt::Fatal(err)) => return Err(err),
            Err(Attempt::Transient { err, retry_after }) => {
                if attempt == policy.attempts {
                    return Err(err.context(format!(
                        "download failed after {} attempts: {url}",
                        policy.attempts
                    )));
                }
                backoff = policy.transient_wait(retry_after, backoff);
                // To stderr, not stdout: `dira update`'s stdout is its progress
                // narrative, and this is a hiccup being handled, not progress.
                // Saying it out loud beats a long unexplained pause.
                eprintln!(
                    "dira update: download attempt {attempt}/{} failed ({err}) — retrying in {:.1}s",
                    policy.attempts,
                    backoff.as_secs_f32()
                );
                tokio::time::sleep(backoff).await;
            }
        }
    }

    // `policy.attempts` is always >= 1, so the loop either returned or
    // exhausted its budget through the `attempt == attempts` arm above.
    unreachable!("download loop exhausted without returning")
}

/// One attempt: request, status check, body read, write. Every failure is
/// classified here so [`download_with`] only has to decide whether to wait.
async fn download_once(
    http: &reqwest::Client,
    asset: &AssetRef,
    dest: &Path,
    timeout: Duration,
) -> std::result::Result<(), Attempt> {
    let url = asset.url();
    let mut req = http
        .get(url)
        .timeout(timeout)
        .header(reqwest::header::USER_AGENT, user_agent());
    if asset.is_authenticated() {
        req = req
            .header(reqwest::header::ACCEPT, "application/octet-stream")
            .header("X-GitHub-Api-Version", "2022-11-28");
    }

    let resp = match req.send().await {
        Ok(resp) => resp,
        Err(e) => {
            let disposition = retry::classify_transport(&e);
            let err = anyhow::Error::new(e).context(format!("GET {url}"));
            return Err(match disposition {
                retry::Disposition::Fatal => Attempt::Fatal(err),
                retry::Disposition::Retry => Attempt::Transient {
                    err,
                    retry_after: None,
                },
            });
        }
    };

    let status = resp.status();
    if !status.is_success() {
        // 404 keeps its bespoke message: it is by far the most likely
        // deterministic failure here and "asset not found on that release"
        // says more than the status alone.
        let err = if status == reqwest::StatusCode::NOT_FOUND {
            anyhow::anyhow!("download failed: {url} returned 404 (asset not found on that release)")
        } else {
            anyhow::anyhow!("download failed: {url} returned {status}")
        };
        return Err(match retry::classify_status(status) {
            retry::Disposition::Fatal => Attempt::Fatal(err),
            retry::Disposition::Retry => Attempt::Transient {
                retry_after: retry::parse_retry_after(resp.headers()),
                err,
            },
        });
    }

    let bytes = match resp.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            // The observed failure: the body starts arriving and the stream
            // dies part-way through.
            let disposition = retry::classify_transport(&e);
            let err = anyhow::Error::new(e).context(format!("read response body for {url}"));
            return Err(match disposition {
                retry::Disposition::Fatal => Attempt::Fatal(err),
                retry::Disposition::Retry => Attempt::Transient {
                    err,
                    retry_after: None,
                },
            });
        }
    };

    // A local write failure is never a network problem — don't spend the retry
    // budget on a full disk or a read-only directory.
    std::fs::write(dest, &bytes)
        .with_context(|| format!("write {}", dest.display()))
        .map_err(Attempt::Fatal)
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
    // sha2 0.11 (digest 0.11) dropped the `io::Write` impl on hashers, so the
    // release archive is streamed through a fixed buffer rather than
    // `io::copy`-ed into the hasher — still constant-memory over the file.
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buf)
            .with_context(|| format!("hash {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
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

/// Extract `archive` (flat root — `dira`/`dirad` (or `dira.exe`/`dirad.exe`
/// on windows) with no leading directory, per
/// `taiki-e/upload-rust-binary-action`'s `leading-dir: false` default) into
/// `dest_dir`, then assert both binaries exist and are non-empty — catches a
/// packaging-action layout change with a clear message instead of a
/// confusing later failure. The actual unpacking is platform-specific (see
/// [`extract_impl`]); this wrapper owns the dest-dir setup and the
/// post-extract assertion shared by both.
pub fn extract(archive: &Path, dest_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dest_dir)
        .with_context(|| format!("create extraction dir {}", dest_dir.display()))?;

    extract_impl(archive, dest_dir)?;

    for name in [dira_ipc::DIRA_BIN, dira_ipc::DIRAD_BIN] {
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

/// Unix: shells out to `tar -xzf` rather than depending on the `tar`/`flate2`
/// crates: neither is in `Cargo.lock` today (unlike `sha2`/`hex`/`semver`),
/// so pulling them in would be genuinely new dependency weight for one
/// extraction step, and `tar` is already a hard requirement of `install.sh`
/// — present on every macOS (bsdtar) and Linux (GNU/busybox) target we ship.
#[cfg(unix)]
fn extract_impl(archive: &Path, dest_dir: &Path) -> Result<()> {
    let status = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(dest_dir)
        .status()
        .context(
            "failed to spawn `tar` — it is required to extract release archives and should be \
             on PATH on every macOS or Linux system",
        )?;
    if !status.success() {
        anyhow::bail!("tar -xzf {} failed ({status})", archive.display());
    }
    Ok(())
}

/// Windows: release assets are `.zip` (D-0010 — no guaranteed `tar`/`gzip` on
/// Windows, and `Expand-Archive` is PowerShell, not something `dira update`
/// can shell out to the way `install.sh` shells out to `tar`). Extracted via
/// the `zip` crate (already a `cfg(windows)` dependency staged for this —
/// deflate-only, no encryption/zstd support needed since the updater only
/// ever reads archives our own release workflow produced).
///
/// Deliberately restrictive about what it writes: only entries whose name is
/// *exactly* [`dira_ipc::DIRA_BIN`] or [`dira_ipc::DIRAD_BIN`] are extracted,
/// and any entry name containing a path separator is skipped outright — a
/// zip-slip guard. `/` is the zip spec's own separator and `\` is what
/// Windows itself treats as one, so both are checked regardless of which
/// tool built the archive; this guarantees extraction can never escape
/// `dest_dir` even against a maliciously crafted archive, even though in
/// practice we only ever expect two flat-root entries here.
#[cfg(windows)]
fn extract_impl(archive: &Path, dest_dir: &Path) -> Result<()> {
    let file = std::fs::File::open(archive)
        .with_context(|| format!("open {} for extraction", archive.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("read {} as a zip archive", archive.display()))?;

    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .with_context(|| format!("read entry {i} of {}", archive.display()))?;
        if !entry.is_file() {
            continue;
        }
        let name = entry.name().to_string();
        // zip-slip guard: skip anything that isn't a bare file name.
        if name.contains('/') || name.contains('\\') {
            continue;
        }
        if name != dira_ipc::DIRA_BIN && name != dira_ipc::DIRAD_BIN {
            continue;
        }
        let dest = dest_dir.join(&name);
        let mut out =
            std::fs::File::create(&dest).with_context(|| format!("create {}", dest.display()))?;
        std::io::copy(&mut entry, &mut out)
            .with_context(|| format!("extract {name} to {}", dest.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // --- download retry -----------------------------------------------------

    /// One scripted response for [`scripted_server`].
    #[derive(Clone)]
    enum Reply {
        /// A complete, well-formed 200.
        Body(&'static str),
        /// A status line with no body (plus optional extra headers).
        Status(u16, &'static str),
        /// Announce a `Content-Length` far larger than what is actually sent,
        /// then drop the connection — the exact shape of the reported failure
        /// ("connection closed before message completed"): the body starts
        /// arriving and the stream dies part-way through.
        Truncated,
    }

    /// A raw HTTP server on an OS-assigned loopback port that serves `script`
    /// in order (the last entry repeats once exhausted), counting connections
    /// so a test can assert exactly how many attempts were made.
    ///
    /// Deliberately not [`crate::test_support::MockCloud`]: axum cannot express
    /// a half-written body, which is the whole point of [`Reply::Truncated`].
    async fn scripted_server(script: Vec<Reply>) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind scripted server");
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();

        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let n = counter.fetch_add(1, Ordering::SeqCst);

                // Drain the request head so the client sees a well-formed
                // exchange rather than a connection reset on write.
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;

                let reply = script
                    .get(n)
                    .or_else(|| script.last())
                    .cloned()
                    .unwrap_or(Reply::Status(500, ""));
                match reply {
                    Reply::Body(body) => {
                        let head =
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
                        let _ = sock.write_all(head.as_bytes()).await;
                        let _ = sock.write_all(body.as_bytes()).await;
                    }
                    Reply::Status(code, extra) => {
                        let head = format!("HTTP/1.1 {code} X\r\n{extra}Content-Length: 0\r\n\r\n");
                        let _ = sock.write_all(head.as_bytes()).await;
                    }
                    Reply::Truncated => {
                        let _ = sock
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\n\r\nhalf")
                            .await;
                        // Dropping `sock` here closes mid-body.
                    }
                }
                let _ = sock.flush().await;
            }
        });

        (format!("http://{addr}/artifact.zip"), hits)
    }

    /// The production loop on a millisecond ladder, so these tests assert the
    /// real control flow without adding seconds of sleeping to the suite.
    fn fast_policy(attempts: u32) -> retry::Policy {
        retry::Policy {
            attempts,
            seed: Duration::from_millis(1),
            max_backoff: Duration::from_millis(4),
            timeout: Duration::from_secs(5),
        }
    }

    async fn run_download(script: Vec<Reply>, attempts: u32) -> (Result<()>, usize, PathBuf) {
        let (url, hits) = scripted_server(script).await;
        let dir = std::env::temp_dir().join(format!("dira-dl-test-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("artifact.zip");
        let http = reqwest::Client::builder().build().unwrap();
        let out = download_with(&http, &AssetRef::Url(url), &dest, fast_policy(attempts)).await;
        (out, hits.load(Ordering::SeqCst), dest)
    }

    /// The reported incident, deterministically: one mid-stream abort followed
    /// by a good response. Before the retry loop this failed the whole update.
    #[tokio::test]
    async fn a_truncated_body_is_retried_and_the_next_attempt_succeeds() {
        let (out, hits, dest) =
            run_download(vec![Reply::Truncated, Reply::Body("payload-bytes")], 4).await;
        out.expect("transient abort should be retried");
        assert_eq!(hits, 2, "should have taken exactly one retry");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "payload-bytes");
    }

    /// A missing asset is deterministic: fail immediately with the bespoke
    /// message, and above all do not spend the retry budget on it.
    #[tokio::test]
    async fn a_404_fails_on_the_first_attempt() {
        let (out, hits, _) = run_download(vec![Reply::Status(404, "")], 4).await;
        let err = out.expect_err("404 must fail");
        assert!(
            format!("{err:#}").contains("asset not found on that release"),
            "404 should keep its bespoke message, got: {err:#}"
        );
        assert_eq!(hits, 1, "a 404 must not be retried");
    }

    #[tokio::test]
    async fn a_non_404_client_error_also_fails_on_the_first_attempt() {
        let (out, hits, _) = run_download(vec![Reply::Status(403, "")], 4).await;
        out.expect_err("403 must fail");
        assert_eq!(hits, 1, "a 4xx must not be retried");
    }

    #[tokio::test]
    async fn server_errors_are_retried_until_one_succeeds() {
        let (out, hits, dest) = run_download(
            vec![
                Reply::Status(500, ""),
                Reply::Status(503, ""),
                Reply::Body("ok"),
            ],
            4,
        )
        .await;
        out.expect("5xx should be retried");
        assert_eq!(hits, 3);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "ok");
    }

    /// The budget is bounded: a server that is simply down fails, and does so
    /// after exactly `attempts` tries rather than looping forever.
    #[tokio::test]
    async fn retries_are_bounded_and_the_final_error_says_so() {
        let (out, hits, _) = run_download(vec![Reply::Status(503, "")], 3).await;
        let err = out.expect_err("a permanently failing server must still fail");
        assert_eq!(hits, 3, "should stop at the attempt budget");
        assert!(
            format!("{err:#}").contains("after 3 attempts"),
            "the final error should report the budget, got: {err:#}"
        );
    }

    /// A 429's `Retry-After` is honoured in place of the ladder — and capped,
    /// so a huge value can't wedge an interactive command (the cap itself is
    /// unit-tested in `retry`; here we only prove the header reaches it).
    #[tokio::test]
    async fn a_429_is_retried_and_honours_retry_after() {
        let started = std::time::Instant::now();
        let (out, hits, _) = run_download(
            vec![
                Reply::Status(429, "Retry-After: 9999\r\n"),
                Reply::Body("x"),
            ],
            4,
        )
        .await;
        out.expect("429 should be retried");
        assert_eq!(hits, 2);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a huge Retry-After must be capped, not slept through"
        );
    }

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
    fn detect_target_windows_maps_known_arches() {
        assert_eq!(
            detect_target_for("windows", "x86_64").unwrap(),
            "x86_64-pc-windows-msvc"
        );
        assert_eq!(
            detect_target_for("windows", "aarch64").unwrap(),
            "aarch64-pc-windows-msvc"
        );
    }

    #[test]
    fn detect_target_rejects_unsupported_windows_arch() {
        assert!(detect_target_for("windows", "riscv64").is_err());
    }

    #[test]
    fn detect_target_rejects_unsupported_os() {
        assert!(detect_target_for("plan9", "x86_64").is_err());
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

    /// Release archives are megabytes, so hashing spans many reads of the
    /// 64 KiB buffer — the loop that replaced `io::copy` when sha2 0.11 dropped
    /// the hasher's `io::Write` impl. A payload that is neither buffer-aligned
    /// nor single-read catches an off-by-one in the `&buf[..read]` slicing that
    /// the 11-byte cases above cannot. Digest computed out-of-band (`shasum`).
    #[test]
    fn verify_sha256_hashes_a_payload_spanning_many_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload.bin");
        let data: Vec<u8> = (0..200_000u32)
            .map(|i| ((i * 7 + 11) % 251) as u8)
            .collect();
        std::fs::write(&path, &data).unwrap();
        verify_sha256(
            &path,
            "d3d6f9698b1b5f12224df555b0704c17f0c4feb6485b49105d4dd99df343660c",
        )
        .unwrap();
    }

    #[test]
    fn verify_sha256_rejects_a_mismatched_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload.bin");
        std::fs::write(&path, b"hello world").unwrap();
        let wrong = "0000000000000000000000000000000000000000000000000000000000000";
        assert!(verify_sha256(&path, wrong).is_err());
    }

    // --- extract (unix: real .tar.gz fixtures via `tar`) ------------------

    #[cfg(unix)]
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

    #[cfg(unix)]
    #[test]
    fn extract_flat_archive_with_both_binaries_succeeds() {
        // Spawns `tar`, resolved via PATH — see `test_env_lock`.
        let _guard = super::super::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let tarball = build_tarball(
            dir.path(),
            &[
                (dira_ipc::DIRA_BIN, b"fake-dira"),
                (dira_ipc::DIRAD_BIN, b"fake-dirad"),
            ],
        );
        let dest = dir.path().join("out");
        extract(&tarball, &dest).unwrap();
        assert_eq!(
            std::fs::read(dest.join(dira_ipc::DIRA_BIN)).unwrap(),
            b"fake-dira"
        );
        assert_eq!(
            std::fs::read(dest.join(dira_ipc::DIRAD_BIN)).unwrap(),
            b"fake-dirad"
        );
    }

    #[cfg(unix)]
    #[test]
    fn extract_missing_dirad_gives_a_clear_error() {
        // Spawns `tar`, resolved via PATH — see `test_env_lock`.
        let _guard = super::super::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let tarball = build_tarball(dir.path(), &[(dira_ipc::DIRA_BIN, b"fake-dira")]);
        let dest = dir.path().join("out");
        let err = extract(&tarball, &dest).unwrap_err();
        assert!(err.to_string().contains("dirad"), "error was: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn extract_empty_binary_is_rejected() {
        // Spawns `tar`, resolved via PATH — see `test_env_lock`.
        let _guard = super::super::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let tarball = build_tarball(
            dir.path(),
            &[
                (dira_ipc::DIRA_BIN, b""),
                (dira_ipc::DIRAD_BIN, b"fake-dirad"),
            ],
        );
        let dest = dir.path().join("out");
        let err = extract(&tarball, &dest).unwrap_err();
        assert!(err.to_string().contains("empty"), "error was: {err}");
    }

    // --- extract (windows: real .zip fixtures via the `zip` crate) --------

    /// Builds a real zip in memory with the `zip` crate — the same
    /// `cfg(windows)` dependency `extract_impl` uses, so this exercises the
    /// real read path rather than a hand-rolled byte layout. Confirms both
    /// expected binaries land, and that a zip-slip-shaped nested entry is
    /// silently skipped rather than escaping `dest_dir`.
    #[cfg(windows)]
    #[test]
    fn extract_windows_zip_extracts_only_the_named_binaries_and_skips_nested_paths() {
        use std::io::{Cursor, Write as _};

        let dir = tempfile::tempdir().unwrap();
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file(dira_ipc::DIRA_BIN, options).unwrap();
        zip.write_all(b"fake-dira").unwrap();
        zip.start_file(dira_ipc::DIRAD_BIN, options).unwrap();
        zip.write_all(b"fake-dirad").unwrap();
        // A zip-slip attempt: must be skipped, never extracted anywhere.
        zip.start_file("nested/evil.txt", options).unwrap();
        zip.write_all(b"should never be written").unwrap();
        let cursor = zip.finish().unwrap();

        let archive_path = dir.path().join("archive.zip");
        std::fs::write(&archive_path, cursor.into_inner()).unwrap();

        let dest = dir.path().join("out");
        extract(&archive_path, &dest).unwrap();
        assert_eq!(
            std::fs::read(dest.join(dira_ipc::DIRA_BIN)).unwrap(),
            b"fake-dira"
        );
        assert_eq!(
            std::fs::read(dest.join(dira_ipc::DIRAD_BIN)).unwrap(),
            b"fake-dirad"
        );
        assert!(!dest.join("nested").exists());
        assert!(!dest.join("evil.txt").exists());
    }

    #[cfg(windows)]
    #[test]
    fn extract_windows_zip_missing_dirad_gives_a_clear_error() {
        use std::io::{Cursor, Write as _};

        let dir = tempfile::tempdir().unwrap();
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file(dira_ipc::DIRA_BIN, options).unwrap();
        zip.write_all(b"fake-dira").unwrap();
        let cursor = zip.finish().unwrap();

        let archive_path = dir.path().join("archive.zip");
        std::fs::write(&archive_path, cursor.into_inner()).unwrap();

        let dest = dir.path().join("out");
        let err = extract(&archive_path, &dest).unwrap_err();
        assert!(err.to_string().contains("dirad"), "error was: {err}");
    }
}
