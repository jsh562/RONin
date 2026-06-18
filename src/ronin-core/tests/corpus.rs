//! Corpus round-trip harness (TR-003 / TR-015 / SC-001, COMPLETES TR-003,TR-015).
//!
//! Loads **every** fixture under `tests/corpus/` and asserts that
//! `parse → print` reproduces the original bytes **byte-for-byte** — for valid
//! files *and* for malformed files (whose error-recovered trees, INV-3, must
//! still cover all input and re-print exactly). A malformed fixture counts
//! toward the 100% round-trip denominator (SC-001).
//!
//! Reading fixture files via `std::fs` here is fine: this is a **test**, not the
//! WASM-clean `ronin-core` core (the core itself touches no filesystem, TR-007).
//!
//! # Corpus → property-strategy feedback loop (TR-015)
//!
//! The grammar-completeness loop is closed as follows:
//!
//! 1. The corpus exercises real-shaped RON across every TR-004 construct group
//!    (see `corpus/README.md`).
//! 2. This harness asserts byte-for-byte round-trip on each fixture. A fixture
//!    that fails to round-trip reveals a construct the lexer/parser does not yet
//!    cover losslessly — i.e. a grammar gap.
//! 3. **When a gap is found**, the policy (TR-015) is: add the newly discovered
//!    construct to the property-test strategies in `tests/roundtrip.rs`
//!    (`ron_strategy`) so the gap is exercised by *generated* inputs from then
//!    on — not just by the single corpus file. The corpus catches the gap; the
//!    property strategy prevents its regression at scale.
//! 4. The currently-covered construct set is enumerated in `corpus/README.md`
//!    and mirrored by `ron_strategy`; the two are kept in sync by this loop.
//!
//! This harness is the *detector* in that loop; the *feedback* is the manual
//! (but mandated) step of widening the strategies when a new construct appears.

use std::fs;
use std::path::{Path, PathBuf};

use ronin_core::{parse_bytes, print};

/// Recursively collect every `.ron` / `.scn.ron` fixture under `dir`.
fn collect_fixtures(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("failed to read corpus dir {}: {e}", dir.display()));
    for entry in entries {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.is_dir() {
            collect_fixtures(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("ron") {
            // Catches both `*.ron` and `*.scn.ron`; skips README.md etc.
            out.push(path);
        }
    }
}

/// The corpus root directory (`tests/corpus/` relative to this crate).
fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("corpus")
}

fn all_fixtures() -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_fixtures(&corpus_dir(), &mut out);
    out.sort();
    out
}

/// SC-001 core: every accepted (UTF-8) corpus file — valid OR malformed —
/// re-prints to its original bytes, byte-for-byte.
#[test]
fn corpus_round_trips_byte_for_byte() {
    let fixtures = all_fixtures();
    assert!(
        !fixtures.is_empty(),
        "corpus must not be empty (looked in {})",
        corpus_dir().display()
    );

    let mut checked = 0usize;
    for path in &fixtures {
        let bytes = fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        // Every fixture is valid UTF-8 (the round-trip domain); parse_bytes
        // accepts it (a leading BOM is preserved as trivia, not stripped).
        let doc = parse_bytes(&bytes).unwrap_or_else(|e| {
            panic!(
                "fixture {} is not valid UTF-8 (outside round-trip domain): {e}",
                path.display()
            )
        });
        let printed = print(&doc);
        assert_eq!(
            printed.as_bytes(),
            bytes.as_slice(),
            "round-trip mismatch for {}",
            path.display()
        );
        checked += 1;
    }
    // 100% of fixtures round-tripped.
    assert_eq!(checked, fixtures.len());
}

/// SC-001 floor: the corpus meets the documented composition requirements —
/// ≥ 30 files, ≥ 3 malformed, ≥ 1 file ≥ 1 MB.
#[test]
fn corpus_meets_composition_floor() {
    let fixtures = all_fixtures();
    assert!(
        fixtures.len() >= 30,
        "corpus must have ≥ 30 fixtures, found {}",
        fixtures.len()
    );

    let malformed = fixtures
        .iter()
        .filter(|p| p.components().any(|c| c.as_os_str() == "malformed"))
        .count();
    assert!(
        malformed >= 3,
        "corpus must have ≥ 3 malformed fixtures, found {malformed}"
    );

    let large = fixtures
        .iter()
        .filter_map(|p| fs::metadata(p).ok())
        .any(|m| m.len() >= 1_000_000);
    assert!(large, "corpus must contain ≥ 1 file ≥ 1 MB");
}

/// Malformed fixtures specifically: they still produce diagnostics (recovery
/// happened) AND still re-print byte-for-byte (INV-3 / SC-004).
#[test]
fn malformed_fixtures_recover_and_round_trip() {
    let dir = corpus_dir().join("malformed");
    let mut fixtures = Vec::new();
    collect_fixtures(&dir, &mut fixtures);
    assert!(fixtures.len() >= 3, "need ≥ 3 malformed fixtures");

    for path in fixtures {
        let bytes = fs::read(&path).unwrap();
        let doc = parse_bytes(&bytes).expect("malformed-but-UTF-8 still parses to a tree");
        // INV-3: the error-recovered tree re-prints to the original bytes.
        assert_eq!(
            print(&doc).as_bytes(),
            bytes.as_slice(),
            "malformed round-trip mismatch for {}",
            path.display()
        );
        // Recovery actually fired (≥ 1 diagnostic) — these files are broken.
        assert!(
            !doc.diagnostics().is_empty(),
            "expected diagnostics for malformed fixture {}",
            path.display()
        );
        // Every diagnostic range lies within the source.
        for d in doc.diagnostics() {
            assert!(d.range().end() <= doc.source_len());
        }
    }
}

/// The large (≥ 1 MB) fixture round-trips and parsing terminates — exercising
/// the *size* axis (SC-001) on a single big file. Correctness-only: no timing
/// assertion (TR-016 owns benchmarks; this is purely a round-trip check).
#[test]
fn large_fixture_round_trips() {
    let large = all_fixtures()
        .into_iter()
        .find(|p| {
            fs::metadata(p)
                .map(|m| m.len() >= 1_000_000)
                .unwrap_or(false)
        })
        .expect("a ≥ 1 MB fixture exists");
    let bytes = fs::read(&large).unwrap();
    assert!(bytes.len() >= 1_000_000);
    let doc = parse_bytes(&bytes).unwrap();
    assert_eq!(
        print(&doc).as_bytes(),
        bytes.as_slice(),
        "large fixture must round-trip"
    );
}
