//! Native-gating invariant: `ronin-core` must NOT depend on `ronin-types` {TR-013}.
//!
//! `ronin-types` is native-only (it pulls in `syn` / `walkdir` / `schemars`, none
//! of which build for `wasm32`). The WASM-clean `ronin-core` (project-instructions
//! §II, ADR-0002) MUST therefore never depend on `ronin-types` or any of those
//! native crates — otherwise `ronin-core`'s `wasm32` build would break.
//!
//! This test is **hermetic and fast**: it reads `ronin-core`'s `Cargo.toml` from
//! disk (located relative to this crate via `CARGO_MANIFEST_DIR`) and asserts the
//! forbidden crates appear in none of its dependency tables. It does NOT shell
//! out to `cargo tree`. The complementary live proof is the
//! `wasm32-unknown-unknown` build of `ronin-core` (T036), which fails if any native
//! dependency leaks in.

use std::path::PathBuf;

/// Crates that must never appear in `ronin-core`'s dependency tables.
const FORBIDDEN: &[&str] = &["ronin-types", "syn", "walkdir", "schemars"];

/// The `[dependencies]`-family tables a dependency could legitimately hide in.
const DEPENDENCY_TABLE_HEADERS: &[&str] = &[
    "[dependencies]",
    "[dev-dependencies]",
    "[build-dependencies]",
];

fn ronin_core_manifest_path() -> PathBuf {
    // CARGO_MANIFEST_DIR = .../src/ronin-types ; ronin-core is its sibling.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .expect("ronin-types crate dir must have a parent (the workspace src/ dir)")
        .join("ronin-core")
        .join("Cargo.toml")
}

/// Collect the dependency-name keys declared under any `[dependencies]`-family
/// table (including `[target.*.dependencies]`). Inline-table deps
/// (`foo = { ... }`) and string-version deps (`foo = "1"`) both start a line with
/// `name =`, so we capture the left-hand key. Section headers reset the "in a
/// dependency table" state, so `[package]`/`[lib]` keys are excluded.
fn declared_dependency_names(manifest: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_dep_table = false;

    for raw_line in manifest.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // A new table header: decide whether it is a dependency table.
        if line.starts_with('[') {
            in_dep_table = is_dependency_table_header(line);
            continue;
        }

        if !in_dep_table {
            continue;
        }

        // Inside a dependency table: the dependency name is the key before '='.
        if let Some((key, _)) = line.split_once('=') {
            let name = key.trim().trim_matches('"').to_string();
            if !name.is_empty() {
                names.push(name);
            }
        }
    }

    names
}

/// `true` for `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`, and
/// platform-specific `[target.'cfg(..)'.dependencies]` variants.
fn is_dependency_table_header(header: &str) -> bool {
    if DEPENDENCY_TABLE_HEADERS.contains(&header) {
        return true;
    }
    // [target.<...>.dependencies] / .dev-dependencies / .build-dependencies
    let inner = header.trim_start_matches('[').trim_end_matches(']');
    inner.starts_with("target.")
        && (inner.ends_with(".dependencies")
            || inner.ends_with(".dev-dependencies")
            || inner.ends_with(".build-dependencies"))
}

#[test]
fn ronin_core_does_not_depend_on_ronin_types_or_native_crates() {
    let manifest_path = ronin_core_manifest_path();
    let manifest = std::fs::read_to_string(&manifest_path).unwrap_or_else(|e| {
        panic!(
            "could not read ronin-core manifest at {}: {e}",
            manifest_path.display()
        )
    });

    let deps = declared_dependency_names(&manifest);

    for forbidden in FORBIDDEN {
        assert!(
            !deps.iter().any(|d| d == forbidden),
            "ronin-core must stay WASM-clean (TR-013): it must NOT declare a \
             dependency on `{forbidden}`, but it appears in {}. Declared deps: {deps:?}",
            manifest_path.display()
        );
    }
}

/// Sanity check that the parser actually found `ronin-core`'s real dependency
/// (`rowan`) — guards against a silently-passing test if the manifest layout or
/// parser ever drifts (a false-clean would otherwise hide a regression).
#[test]
fn parser_sees_ronin_core_real_dependencies() {
    let manifest_path = ronin_core_manifest_path();
    let manifest = std::fs::read_to_string(&manifest_path).expect("ronin-core manifest readable");
    let deps = declared_dependency_names(&manifest);
    assert!(
        deps.iter().any(|d| d == "rowan"),
        "expected to parse ronin-core's `rowan` dependency; parsed: {deps:?}"
    );
}
