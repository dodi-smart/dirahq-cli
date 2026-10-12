//! Golden walker over the shared frontmatter-dialect fixtures.
//!
//! `cli/core/testdata/zavet-dialect/` is the CANONICAL copy of the corpus the
//! zavet plugin vendors (its `test/run.sh` executes the same inputs through
//! the awk parsers and diffs the same goldens; its CI compares the two
//! MANIFESTs). This test projects `parse_decision` / `parse_spec` output to
//! the exact TSV the plugin's `zavet list|guards|specs|spec-paths` emit — the
//! executable definition of "same dialect as dira-core::zavet". See the
//! corpus README for the projection rules and the documented out-of-scope
//! divergences.

use dira_core::zavet::{parse_config, parse_decision, parse_spec, ZavetConfig};
use std::path::{Path, PathBuf};

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/zavet-dialect")
}

/// All `.md` files in a corpus subdirectory, filename-sorted (the sh glob and
/// this walker must agree on row order). Dot-prefixed files are INCLUDED here
/// — the parsers themselves must reject template files by filename.
fn md_files(sub: &str) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(corpus().join(sub))
        .expect("corpus subdir exists")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .collect();
    files.sort();
    files
}

/// The corpus's own `.zavet/config`. Both sides read it: the plugin's
/// `test/run.sh` copies it into the fixture repo, this walker parses it here.
/// It is what makes `ZAVET-0018` and the older `D-*` records resolve side by
/// side — the retired prefix stays in the set.
fn corpus_config() -> ZavetConfig {
    std::fs::read_to_string(corpus().join("config"))
        .map(|t| parse_config(&t))
        .unwrap_or_default()
}

fn golden(name: &str) -> String {
    std::fs::read_to_string(corpus().join("expected").join(name)).expect("golden exists")
}

fn assert_matches_golden(name: &str, actual: &str) {
    let expected = golden(name);
    assert_eq!(
        expected, actual,
        "\n--- {name}: Rust projection diverges from the shared golden ---\n\
         expected:\n{expected}\nactual:\n{actual}"
    );
}

#[test]
fn decisions_meta_matches_golden() {
    let mut out = String::new();
    for f in md_files("decisions") {
        let text = std::fs::read_to_string(&f).unwrap();
        let rel = format!(
            ".zavet/decisions/{}",
            f.file_name().unwrap().to_str().unwrap()
        );
        let Some(cap) = parse_decision(&text, &rel, &corpus_config()) else {
            continue; // rejected documents yield no rows, matching the awk
        };
        out.push_str(&format!(
            "{}\t{}\t{}\n",
            cap.id,
            cap.status.as_deref().unwrap_or("active"),
            cap.title.as_deref().unwrap_or("")
        ));
    }
    assert_matches_golden("decisions-meta.tsv", &out);
}

#[test]
fn decisions_guards_matches_golden() {
    let mut out = String::new();
    for f in md_files("decisions") {
        let text = std::fs::read_to_string(&f).unwrap();
        let rel = format!(
            ".zavet/decisions/{}",
            f.file_name().unwrap().to_str().unwrap()
        );
        let Some(cap) = parse_decision(&text, &rel, &corpus_config()) else {
            continue;
        };
        // Guard rows emit for ACTIVE decisions only (the enforcement surface).
        if cap.status.as_deref().unwrap_or("active") != "active" {
            continue;
        }
        for glob in &cap.guards {
            out.push_str(&format!("{}\t{}\n", cap.id, glob));
        }
    }
    assert_matches_golden("decisions-guards.tsv", &out);
}

#[test]
fn specs_meta_matches_golden() {
    let mut out = String::new();
    for f in md_files("specs") {
        let text = std::fs::read_to_string(&f).unwrap();
        let rel = format!(".zavet/specs/{}", f.file_name().unwrap().to_str().unwrap());
        // Dot-prefixed templates and unclosed fences both reject → no rows.
        let Some(cap) = parse_spec(&text, &rel, &corpus_config()) else {
            continue;
        };
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            cap.slug,
            cap.origin,
            cap.confidence,
            cap.date.as_deref().unwrap_or(""),
            cap.title.as_deref().unwrap_or("")
        ));
    }
    assert_matches_golden("specs-meta.tsv", &out);
}

#[test]
fn decision_checks_matches_golden() {
    let mut out = String::new();
    for f in md_files("decisions") {
        let text = std::fs::read_to_string(&f).unwrap();
        let rel = format!(
            ".zavet/decisions/{}",
            f.file_name().unwrap().to_str().unwrap()
        );
        let Some(cap) = parse_decision(&text, &rel, &corpus_config()) else {
            continue;
        };
        // Unlike guards, checks emit for superseded records too: a check is a
        // claim about how the record was verified, not an enforcement surface.
        for c in &cap.checks {
            out.push_str(&format!("{}\t{}\t{}\n", cap.id, c.label, c.command));
        }
    }
    assert_matches_golden("decision-checks.tsv", &out);
}

#[test]
fn spec_checks_matches_golden() {
    let mut out = String::new();
    for f in md_files("specs") {
        let text = std::fs::read_to_string(&f).unwrap();
        let rel = format!(".zavet/specs/{}", f.file_name().unwrap().to_str().unwrap());
        let Some(cap) = parse_spec(&text, &rel, &corpus_config()) else {
            continue;
        };
        for c in &cap.checks {
            out.push_str(&format!("{}\t{}\t{}\n", cap.slug, c.label, c.command));
        }
    }
    assert_matches_golden("spec-checks.tsv", &out);
}

#[test]
fn spec_paths_matches_golden() {
    let mut out = String::new();
    for f in md_files("specs") {
        let text = std::fs::read_to_string(&f).unwrap();
        let rel = format!(".zavet/specs/{}", f.file_name().unwrap().to_str().unwrap());
        let Some(cap) = parse_spec(&text, &rel, &corpus_config()) else {
            continue;
        };
        for glob in &cap.paths {
            out.push_str(&format!("{}\t{}\n", cap.slug, glob));
        }
    }
    assert_matches_golden("spec-paths.tsv", &out);
}

#[test]
fn spec_sources_matches_golden() {
    // `kind \t root \t globs` (space-joined), file order. The corpus config
    // carries a `madeup` kind that must be ignored, so it never shows here.
    let mut out = String::new();
    for d in &corpus_config().spec_sources {
        out.push_str(&format!("{}\t{}\t{}\n", d.kind, d.root, d.globs.join(" ")));
    }
    assert_matches_golden("spec-sources.tsv", &out);
}

#[test]
fn corpus_manifest_matches_the_files_on_disk() {
    // Mirrors the plugin's `test/sync-check.sh`: every vendored file is pinned
    // by its `git hash-object` blob oid. A hand edit that skips the MANIFEST
    // regeneration fails here, not only in the plugin's CI.
    let manifest = std::fs::read_to_string(corpus().join("MANIFEST")).unwrap();
    let mut listed = 0;
    for line in manifest.lines().filter(|l| !l.trim().is_empty()) {
        let (oid, path) = line.split_once("  ").expect("`<oid>  <path>` rows");
        let bytes = std::fs::read(corpus().join(path)).unwrap();
        let mut blob = format!("blob {}\0", bytes.len()).into_bytes();
        blob.extend_from_slice(&bytes);
        assert_eq!(
            sha1_hex(&blob),
            oid,
            "{path} drifted from MANIFEST — regenerate it (see the README)"
        );
        listed += 1;
    }
    assert!(listed > 20, "MANIFEST looks truncated ({listed} rows)");
}

/// Minimal SHA-1 (git blob oids). Test-only; avoids a dependency for one hash.
fn sha1_hex(data: &[u8]) -> String {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for (i, b) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}
