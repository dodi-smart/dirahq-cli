//! Shared HTTP client construction for every device→cloud call.
//!
//! TLS trust is a property of the artifact: the binary ships its own Mozilla
//! root set (D-0011) and never reads the OS trust store. Cloud agent runtimes
//! (Claude Code on the web, Cursor cloud agents) break that assumption — all
//! egress is re-terminated by a security proxy whose CA is known only inside
//! the VM — so [`ENV_EXTRA_CA_CERTS`] lets an operator *add* trust anchors for
//! one process, without ever swapping the bundled store out. The env var names
//! a PEM bundle file; every certificate in it is appended to the default root
//! set. Deliberately not `SSL_CERT_FILE`: an inherited variable from an
//! unrelated toolchain must not silently widen what this binary trusts.
//!
//! Never-brick posture (same as `identity::env_key`): a missing or malformed
//! bundle logs a warning and yields the default client — a typo can't take
//! sync offline harder than the proxy already does.

/// Env var naming a PEM bundle whose certificates are appended to the bundled
/// roots. Additive only; unset or blank means bundled roots alone.
pub const ENV_EXTRA_CA_CERTS: &str = "DIRA_EXTRA_CA_CERTS";

/// Start a [`reqwest::ClientBuilder`] with the extra roots (if any) applied.
/// Callers chain their own pool/timeout settings and `build()`.
pub fn builder() -> reqwest::ClientBuilder {
    with_extra_roots(reqwest::Client::builder())
}

/// Append every certificate from the [`ENV_EXTRA_CA_CERTS`] bundle to
/// `builder`'s root set. Any failure to read the file leaves the builder
/// exactly as it came in; a bundle with SOME corrupt blocks still contributes
/// whichever certificates parsed (see [`parse_pem_certificates`]).
fn with_extra_roots(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    let Some(path) = env_bundle_path() else {
        return builder;
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("{ENV_EXTRA_CA_CERTS} names an unreadable file ({path}); ignoring: {e}");
            return builder;
        }
    };
    let (certs, failed) = parse_pem_certificates(&bytes);
    if certs.is_empty() {
        if failed == 0 {
            tracing::warn!("{ENV_EXTRA_CA_CERTS} bundle ({path}) holds no certificates; ignoring");
        } else {
            tracing::warn!(
                "{ENV_EXTRA_CA_CERTS} bundle ({path}) holds {failed} certificate block(s), \
                 none of which parsed; ignoring"
            );
        }
        return builder;
    }
    let n = certs.len();
    let builder = certs
        .into_iter()
        .fold(builder, |b, c| b.add_root_certificate(c));
    if failed > 0 {
        tracing::warn!(
            "added {n} extra CA root(s) from {ENV_EXTRA_CA_CERTS} ({path}); {failed} \
             certificate block(s) in the bundle did not parse and were skipped"
        );
    } else {
        tracing::debug!("added {n} extra CA root(s) from {ENV_EXTRA_CA_CERTS} ({path})");
    }
    builder
}

/// Split a PEM bundle into its individual `CERTIFICATE` blocks (as raw text
/// slices, unparsed) — the shared first step for both the fast and slow
/// paths in [`parse_pem_certificates`]. Splits on the PEM
/// `-----BEGIN/END CERTIFICATE-----` markers rather than handing the whole
/// file to `reqwest::Certificate::from_pem_bundle`, which fails the ENTIRE
/// bundle on a single corrupt block.
fn pem_certificate_blocks(bytes: &[u8]) -> Vec<&str> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";

    // `from_utf8` (not `_lossy`): a lossy replacement would shift every byte
    // offset found below, silently corrupting the slice bounds. Bytes this
    // module cares about (the PEM markers, base64) are always valid UTF-8;
    // any real-world file that CONTAINS invalid UTF-8 has no PEM structure a
    // human wrote on purpose, so treating it as "zero blocks found" (same
    // outcome `from_utf8_lossy` would eventually reach anyway, once every
    // corrupted marker fails to match) is the correct, simpler answer.
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Vec::new();
    };
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(BEGIN) {
        let after_begin = &rest[start..];
        // Markers are ASCII, so these byte offsets always land on char
        // boundaries — safe to slice `rest` with them directly.
        let Some(end_rel) = after_begin.find(END) else {
            break; // an unterminated block — nothing further to scan.
        };
        let block_end = start + end_rel + END.len();
        blocks.push(&rest[start..block_end]);
        rest = &rest[block_end..];
    }
    blocks
}

/// Parse every certificate block in a PEM bundle, returning the certificates
/// that are actually usable as TLS trust anchors plus a count of the blocks
/// that were not (malformed base64, truncated DER, an unsupported PEM
/// label, ...).
///
/// Two-path by design, cheapest case first:
///
/// - **Fast path**: `reqwest::Certificate::from_pem` is cheap under the
///   rustls backend this crate builds with — it never validates anything, it
///   just stores the PEM bytes verbatim (see [`is_valid_root`]'s doc comment)
///   — so turning every block into a candidate `Certificate` costs nothing.
///   The overwhelmingly common case is then a FULLY valid bundle (the
///   documented `/etc/ssl/certs/ca-certificates.crt` last resort alone holds
///   roughly 140 certs), so [`whole_bundle_builds`] tries building ONE
///   throwaway client with every candidate at once — one `build()` call,
///   not one per certificate — and if that succeeds, every candidate is
///   returned as-is.
/// - **Slow path**: only reached when the whole-bundle build fails, i.e.
///   SOME block really is corrupt. [`salvage_valid_roots`] then falls back
///   to validating each candidate independently (one `build()` per
///   candidate) so a single bad block doesn't discard every good one
///   alongside it — the partial-bundle bug this module exists to fix. This
///   is the only place that per-certificate cost is ever paid.
fn parse_pem_certificates(bytes: &[u8]) -> (Vec<reqwest::Certificate>, usize) {
    let blocks = pem_certificate_blocks(bytes);
    let mut failed = 0;
    let candidates: Vec<reqwest::Certificate> = blocks
        .iter()
        .filter_map(|b| match reqwest::Certificate::from_pem(b.as_bytes()) {
            Ok(cert) => Some(cert),
            Err(_) => {
                failed += 1;
                None
            }
        })
        .collect();
    if candidates.is_empty() {
        return (Vec::new(), failed);
    }

    if whole_bundle_builds(&candidates) {
        return (candidates, failed);
    }

    let (certs, salvage_failed) = salvage_valid_roots(candidates);
    (certs, failed + salvage_failed)
}

/// Fast-path check: would a client with EVERY one of `certs` added as a root
/// build successfully? One throwaway `build()` for the whole set, so the
/// common fully-valid bundle pays this once instead of once per certificate
/// (see [`parse_pem_certificates`]'s doc comment). Clones each cert — cheap
/// (`Certificate` just wraps its PEM/DER bytes) — so the caller keeps its
/// owned candidates regardless of the outcome.
fn whole_bundle_builds(certs: &[reqwest::Certificate]) -> bool {
    certs
        .iter()
        .fold(
            // The question is only "do `certs` themselves parse as valid
            // trust anchors" — the bundled Mozilla/webpki roots this crate
            // ships are irrelevant to it. Turning them off means `build()`
            // only ever has to load `certs`, not the ~140-cert built-in store
            // on top, for every throwaway client this parses.
            reqwest::Client::builder().tls_built_in_root_certs(false),
            |b, c| b.add_root_certificate(c.clone()),
        )
        .build()
        .is_ok()
}

/// Slow path: [`whole_bundle_builds`] already proved SOME candidate in this
/// set doesn't build, so validate each one independently via
/// [`is_valid_root`] and keep only the ones that do — this is the only
/// function in this module that pays one `build()` per certificate, and it
/// is reachable ONLY from that failure branch of [`parse_pem_certificates`].
fn salvage_valid_roots(
    candidates: Vec<reqwest::Certificate>,
) -> (Vec<reqwest::Certificate>, usize) {
    let mut certs = Vec::with_capacity(candidates.len());
    let mut failed = 0;
    for cert in candidates {
        if is_valid_root(&cert) {
            certs.push(cert);
        } else {
            failed += 1;
        }
    }
    (certs, failed)
}

/// Whether `cert` is actually usable as a TLS trust anchor.
///
/// Under the rustls backend this crate builds with, `Certificate::from_pem`
/// alone never validates anything — it just stores the PEM bytes verbatim,
/// deferring the real base64/DER decode to `ClientBuilder::build()`. Handed a
/// bundle with one corrupt block among good ones, that means the FIRST
/// `build()` call fails outright with no indication of which certificate (or
/// how many others) were fine — exactly the all-or-nothing failure this
/// module exists to avoid. Building a disposable single-cert client is the
/// cheapest way to ask "would this cert alone build" without hand-parsing
/// DER ourselves (which would mean a new dependency — `rustls-pki-types` is
/// only pulled in transitively via `reqwest`/`rustls`, not a direct
/// dependency of this crate). Only called from [`salvage_valid_roots`], i.e.
/// only once the cheaper [`whole_bundle_builds`] fast path has already
/// failed for the set this certificate belongs to.
fn is_valid_root(cert: &reqwest::Certificate) -> bool {
    // Same reasoning as `whole_bundle_builds`: only `cert` itself is under
    // test, so the built-in root store has no reason to be loaded too.
    reqwest::Client::builder()
        .tls_built_in_root_certs(false)
        .add_root_certificate(cert.clone())
        .build()
        .is_ok()
}

/// The bundle path from the env, or `None` when unset/blank (blank means "not
/// configured" — see [`crate::env::non_blank`]).
fn env_bundle_path() -> Option<String> {
    crate::env::non_blank(ENV_EXTRA_CA_CERTS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;

    /// Env vars are process-global; serialize the tests that touch this one.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Restore-on-drop guard so a panicking test can't leak the env var into
    /// its neighbors.
    struct ClearEnv;
    impl Drop for ClearEnv {
        fn drop(&mut self) {
            std::env::remove_var(ENV_EXTRA_CA_CERTS);
        }
    }

    /// A static self-signed EC cert: a syntactically valid PEM bundle for the
    /// happy path. The key it belongs to was discarded at generation time.
    const TEST_CA_PEM: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBjzCCATWgAwIBAgIUXpXnKLVhiwKnNTTzd0Lq8PEqc1swCgYIKoZIzj0EAwIw\n\
HTEbMBkGA1UEAwwSZGlyYS10ZXN0LWV4dHJhLWNhMB4XDTI2MDgyNDIxMzgwNFoX\n\
DTM2MDgyMTIxMzgwNFowHTEbMBkGA1UEAwwSZGlyYS10ZXN0LWV4dHJhLWNhMFkw\n\
EwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEv0BvAbB12CZdMapl2zG1Gd+7LMqtK5P6\n\
XJu9r/nnwsGUVYvwb4rFIIlT16d2Ot2+PXpbSvJ/XjOllJL4SgWUdqNTMFEwHQYD\n\
VR0OBBYEFBmXk5aaeMPHN6gYZrA75skD+jWAMB8GA1UdIwQYMBaAFBmXk5aaeMPH\n\
N6gYZrA75skD+jWAMA8GA1UdEwEB/wQFMAMBAf8wCgYIKoZIzj0EAwIDSAAwRQIh\n\
AIa/+iicnZ47YGmniv6mgKzdM7pQolt/98xNEY98lkQoAiBkcCiW3rF2f8Inx2Nk\n\
o4qY+uwjZ5ussV1DKK74M2cH+Q==\n\
-----END CERTIFICATE-----\n";

    /// A second, distinct self-signed EC cert (different key/subject from
    /// `TEST_CA_PEM`) — needed to build a bundle with more than one
    /// certificate for the fast-path tests below (`whole_bundle_builds`
    /// needs a REAL multi-cert set, not the same cert added twice, to prove
    /// anything about a many-certificate bundle).
    const TEST_CA_PEM_2: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBkjCCATmgAwIBAgIUUJyNOCSgyIOau2bpb3D3mQ4KtLgwCgYIKoZIzj0EAwIw\n\
HzEdMBsGA1UEAwwUZGlyYS10ZXN0LWV4dHJhLWNhLTIwHhcNMjYwOTAyMTQxMDU4\n\
WhcNMzYwODMwMTQxMDU4WjAfMR0wGwYDVQQDDBRkaXJhLXRlc3QtZXh0cmEtY2Et\n\
MjBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABNXKLUTtsxp7jc6sdYA79N27CWzv\n\
zGdbITXXAJ3sNf6rm6y5eHtSSa3ZELbfKKmLO3gvGtfFSifgmPNnMwXW57SjUzBR\n\
MB0GA1UdDgQWBBR6ZwPFhDj8uF0fG5gMR62xB8eOuTAfBgNVHSMEGDAWgBR6ZwPF\n\
hDj8uF0fG5gMR62xB8eOuTAPBgNVHRMBAf8EBTADAQH/MAoGCCqGSM49BAMCA0cA\n\
MEQCIB/DAliMSa4Ex7V/7X1SuzhsobzIYWOH60PMAOTgD53aAiAMW7Lq1xo5RwwA\n\
MND1KS6AlAZf4uB9Gu2l3c2EOrC68Q==\n\
-----END CERTIFICATE-----\n";

    #[test]
    fn unset_or_blank_env_yields_default_builder() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _clear = ClearEnv;
        std::env::remove_var(ENV_EXTRA_CA_CERTS);
        builder().build().expect("default client must build");
        std::env::set_var(ENV_EXTRA_CA_CERTS, "   ");
        builder().build().expect("blank value reads as unset");
    }

    #[test]
    fn unreadable_or_garbage_bundle_never_bricks_the_client() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _clear = ClearEnv;

        // Missing file: warn-and-continue.
        std::env::set_var(ENV_EXTRA_CA_CERTS, "/nonexistent/dira-extra-ca.pem");
        builder().build().expect("missing bundle must not brick");

        // Garbage contents: warn-and-continue.
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"not a pem bundle").unwrap();
        std::env::set_var(ENV_EXTRA_CA_CERTS, f.path());
        builder().build().expect("garbage bundle must not brick");
    }

    #[test]
    fn valid_bundle_builds_with_extra_roots() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _clear = ClearEnv;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(TEST_CA_PEM.as_bytes()).unwrap();
        std::env::set_var(ENV_EXTRA_CA_CERTS, f.path());
        builder()
            .build()
            .expect("bundle with one valid cert must build");
    }

    /// A bundle with a valid header/footer but a corrupt (non-decodable, or
    /// decodable-but-not-a-real-certificate) block in between must not sink
    /// an otherwise-good certificate sitting alongside it in the same file —
    /// `Certificate::from_pem_bundle` (and a naive per-block `from_pem`,
    /// which never validates anything under the rustls backend) both fail
    /// the whole parse on one bad block, which is exactly the partial-bundle
    /// bug this test pins closed. Named in DIRASH-0033's `checks:`.
    #[test]
    fn partial_bundle_loads_the_valid_cert_and_skips_the_corrupt_one() {
        let corrupt = "-----BEGIN CERTIFICATE-----\n\
not valid base64 !!! ###\n\
-----END CERTIFICATE-----\n";
        let (certs, failed) = parse_pem_certificates(format!("{TEST_CA_PEM}{corrupt}").as_bytes());
        assert_eq!(certs.len(), 1, "the valid cert must still parse");
        assert_eq!(
            failed, 1,
            "the corrupt block must be counted, not silently dropped"
        );

        let _guard = ENV_LOCK.lock().unwrap();
        let _clear = ClearEnv;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(format!("{TEST_CA_PEM}{corrupt}").as_bytes())
            .unwrap();
        std::env::set_var(ENV_EXTRA_CA_CERTS, f.path());
        builder()
            .build()
            .expect("a bundle with one good and one corrupt cert must still build");
    }

    /// Review fix round 1, finding 2: prove the fast path actually fires for
    /// the common case — a fully valid MULTI-certificate bundle — rather
    /// than paying a `build()` per certificate every time. `whole_bundle_builds`
    /// is the fast-path gate; `parse_pem_certificates` can only reach its
    /// `salvage_valid_roots` slow path when this returns `false` (see its
    /// source), so proving this returns `true` for an all-valid set, plus
    /// the next test proving the end-to-end result matches what only the
    /// fast-path return could have produced, together pin the fast path
    /// closed without needing a call-counting mock.
    #[test]
    fn whole_bundle_builds_is_true_for_an_all_valid_multi_cert_set() {
        let a = reqwest::Certificate::from_pem(TEST_CA_PEM.as_bytes()).unwrap();
        let b = reqwest::Certificate::from_pem(TEST_CA_PEM_2.as_bytes()).unwrap();
        assert!(
            whole_bundle_builds(&[a, b]),
            "two distinct valid certs must build together in one shot"
        );
    }

    /// The other half of the gate: a single corrupt candidate in the set
    /// must make the whole-bundle build fail, which is what routes
    /// `parse_pem_certificates` into the per-certificate salvage path at all.
    #[test]
    fn whole_bundle_builds_is_false_when_one_candidate_is_corrupt() {
        let good = reqwest::Certificate::from_pem(TEST_CA_PEM.as_bytes()).unwrap();
        let corrupt = reqwest::Certificate::from_pem(
            b"-----BEGIN CERTIFICATE-----
not valid base64 !!! ###
-----END CERTIFICATE-----
",
        )
        .unwrap(); // `from_pem` itself never fails under rustls — see `is_valid_root`'s doc comment.
        assert!(
            !whole_bundle_builds(&[good, corrupt]),
            "one corrupt candidate must sink the whole-bundle build"
        );
    }

    /// `salvage_valid_roots` (the slow path, reachable only once
    /// `whole_bundle_builds` has already returned `false` for a set) must
    /// still recover every good candidate and count every bad one — this is
    /// the exact per-certificate logic the OLD single-path implementation
    /// used unconditionally; now it only runs when the fast path can't.
    #[test]
    fn salvage_valid_roots_keeps_the_good_ones_and_counts_the_bad() {
        let good = reqwest::Certificate::from_pem(TEST_CA_PEM.as_bytes()).unwrap();
        let corrupt = reqwest::Certificate::from_pem(
            b"-----BEGIN CERTIFICATE-----
not valid base64 !!! ###
-----END CERTIFICATE-----
",
        )
        .unwrap();
        let (certs, failed) = salvage_valid_roots(vec![good, corrupt]);
        assert_eq!(certs.len(), 1, "the good candidate must survive salvage");
        assert_eq!(failed, 1, "the corrupt candidate must be counted");
    }

    /// End-to-end: a fully valid, MULTI-certificate bundle must come back
    /// with every certificate and zero failures. By `parse_pem_certificates`'s
    /// source, the ONLY branch that returns without ever calling
    /// `salvage_valid_roots` is the `whole_bundle_builds(&candidates)` `true`
    /// branch — combined with the direct `whole_bundle_builds` test above
    /// (which proves this exact two-cert shape passes that gate), this
    /// result is only reachable via the fast path.
    #[test]
    fn parse_pem_certificates_resolves_a_fully_valid_multi_cert_bundle_via_the_fast_path() {
        let (certs, failed) =
            parse_pem_certificates(format!("{TEST_CA_PEM}{TEST_CA_PEM_2}").as_bytes());
        assert_eq!(certs.len(), 2, "both certs in the bundle must be returned");
        assert_eq!(failed, 0);
    }

    /// A bundle built from `TEST_CA_PEM_2` alone, through the full `builder()`
    /// path, must still work — belt-and-braces alongside the single-cert
    /// `valid_bundle_builds_with_extra_roots` test above (which uses
    /// `TEST_CA_PEM`), so both fixture certs are proven independently usable,
    /// not just as a pair.
    #[test]
    fn second_fixture_cert_alone_also_builds() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _clear = ClearEnv;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(TEST_CA_PEM_2.as_bytes()).unwrap();
        std::env::set_var(ENV_EXTRA_CA_CERTS, f.path());
        builder()
            .build()
            .expect("the second fixture cert must build on its own too");
    }
}
