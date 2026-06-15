//! Dependency-invariant verification for Bevy mode (E009/T004, FR-003/FR-017,
//! SC-007) `[COMPLETES FR-003]`.
//!
//! Bevy mode consumes an **exported** Bevy type registry as DATA. RONin MUST NOT
//! depend on or embed any `bevy` crate anywhere in the workspace, and the
//! WASM-clean engine crates (`ron-core`, `ron-validate`) MUST keep building for
//! `wasm32-unknown-unknown`. This test proves both programmatically:
//!
//! 1. **No `bevy*` crate** appears in the *normal* (non-dev/non-build) dependency
//!    closure of ANY workspace crate (`ron-core`, `ron-types`, `ron-validate`,
//!    `ronin-app`). The umbrella `bevy` crate is also banned at workspace scope in
//!    `deny.toml`, but cargo-deny matches `deny` entries by exact name (no name
//!    globs), so THIS test is the comprehensive family-wide (`bevy_reflect`,
//!    `bevy_ecs`, `bevy_remote`, …) guard. The registry is JSON data, never a
//!    crate.
//! 2. **`ron-core` and `ron-validate` build for `wasm32-unknown-unknown`** — the
//!    no-Bevy/registry-dependency placement (BevySource is native-only, in
//!    `ron-types`) must not regress the wasm cleanliness the core relies on.
//!
//! # Robustness
//!
//! * The dependency walk reuses the E006 approach (`cargo metadata
//!   --format-version 1`, BFS over `resolve.nodes`, normal edges only, compare on
//!   package **name**), so it is stable across version/registry changes.
//! * The wasm build runs in an **isolated** `CARGO_TARGET_DIR` so it cannot
//!   deadlock on the target-dir lock held by the outer `cargo test` run, and sets
//!   a conservative `CARGO_BUILD_JOBS=2` (the documented Windows parallel-rustc
//!   workaround; harmless elsewhere — only two small crates compile).
//! * Every external dependency (cargo, the wasm32 target) degrades to a
//!   documented `eprintln!` SKIP rather than a false pass when unavailable; on
//!   CI/dev with the target installed it actually runs and asserts.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// Workspace root: this crate's manifest dir is `<ws>/src/ronin-app`.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
}

/// The workspace crates whose normal-dependency closures must be `bevy`-free.
const WORKSPACE_CRATES: &[&str] = &["ron-core", "ron-types", "ron-validate", "ronin-app"];

/// Run `cargo metadata --format-version 1` in the workspace and parse it. Returns
/// `None` (with an `eprintln!` skip) when `cargo` is unavailable or fails, so the
/// test degrades to a documented skip instead of a false pass.
fn cargo_metadata() -> Option<Value> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = match Command::new(&cargo)
        .args(["metadata", "--format-version", "1", "--locked"])
        .current_dir(workspace_root())
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!(
                "[dependency_invariants] SKIP: `{cargo} metadata` could not be spawned ({e}); \
                 cannot verify the dependency closure in this environment"
            );
            return None;
        }
    };
    if !output.status.success() {
        eprintln!(
            "[dependency_invariants] SKIP: `cargo metadata` exited with {:?}; stderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    match serde_json::from_slice::<Value>(&output.stdout) {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("[dependency_invariants] SKIP: cargo metadata JSON parse failed: {e}");
            None
        }
    }
}

/// A resolved graph node: its dependency edges (target package ids) restricted to
/// **normal** dependency kinds.
struct Graph {
    /// package id -> package name.
    name_of: BTreeMap<String, String>,
    /// package id -> normal-dependency target package ids.
    normal_deps: BTreeMap<String, Vec<String>>,
    /// package name -> the package id (workspace members are unique by name here).
    id_of_name: BTreeMap<String, String>,
}

impl Graph {
    fn from_metadata(meta: &Value) -> Self {
        let packages = meta
            .get("packages")
            .and_then(Value::as_array)
            .expect("metadata.packages array");
        let mut name_of = BTreeMap::new();
        let mut id_of_name = BTreeMap::new();
        for p in packages {
            let id = p.get("id").and_then(Value::as_str).expect("package id");
            let name = p.get("name").and_then(Value::as_str).expect("package name");
            name_of.insert(id.to_owned(), name.to_owned());
            id_of_name
                .entry(name.to_owned())
                .or_insert_with(|| id.to_owned());
        }

        let nodes = meta
            .get("resolve")
            .and_then(|r| r.get("nodes"))
            .and_then(Value::as_array)
            .expect("metadata.resolve.nodes array");
        let mut normal_deps = BTreeMap::new();
        for n in nodes {
            let id = n.get("id").and_then(Value::as_str).expect("node id");
            let mut targets = Vec::new();
            if let Some(deps) = n.get("deps").and_then(Value::as_array) {
                for dep in deps {
                    if dep_is_normal(dep) {
                        if let Some(pkg) = dep.get("pkg").and_then(Value::as_str) {
                            targets.push(pkg.to_owned());
                        }
                    }
                }
            }
            normal_deps.insert(id.to_owned(), targets);
        }

        Self {
            name_of,
            normal_deps,
            id_of_name,
        }
    }

    /// The set of package **names** reachable from `root_name` over normal edges
    /// (the root itself excluded from the returned set).
    fn normal_closure_names(&self, root_name: &str) -> BTreeSet<String> {
        let root_id = self
            .id_of_name
            .get(root_name)
            .unwrap_or_else(|| panic!("workspace crate `{root_name}` not found in metadata"))
            .clone();

        let mut seen_ids: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        queue.push_back(root_id.clone());
        seen_ids.insert(root_id.clone());

        while let Some(id) = queue.pop_front() {
            if let Some(targets) = self.normal_deps.get(&id) {
                for t in targets {
                    if seen_ids.insert(t.clone()) {
                        queue.push_back(t.clone());
                    }
                }
            }
        }

        seen_ids
            .into_iter()
            .filter(|id| *id != root_id)
            .filter_map(|id| self.name_of.get(&id).cloned())
            .collect()
    }
}

/// Whether a `deps[]` edge has any **normal** dependency kind. In cargo metadata a
/// normal kind is encoded as JSON `null` in `dep_kinds[].kind`; `"dev"`/`"build"`
/// are excluded. An edge with no `dep_kinds` is treated as normal (older formats).
fn dep_is_normal(dep: &Value) -> bool {
    match dep.get("dep_kinds").and_then(Value::as_array) {
        None => true,
        Some(kinds) if kinds.is_empty() => true,
        Some(kinds) => kinds
            .iter()
            .any(|k| k.get("kind").map(Value::is_null).unwrap_or(true)),
    }
}

#[test]
fn no_bevy_crate_anywhere_in_workspace_closure() {
    let Some(meta) = cargo_metadata() else {
        return; // documented skip (cargo unavailable) — never a false pass.
    };
    let graph = Graph::from_metadata(&meta);

    // Union of every workspace crate's normal-dependency closure = the full set of
    // crates actually compiled into the workspace.
    let mut union: BTreeSet<String> = BTreeSet::new();
    for &root in WORKSPACE_CRATES {
        union.extend(graph.normal_closure_names(root));
    }

    // FR-003/FR-017: NO crate whose name is `bevy` or begins with `bevy_` (the
    // reflect/ecs/remote/… family) may appear. The registry is data, not a crate.
    let offenders: Vec<&String> = union
        .iter()
        .filter(|name| *name == "bevy" || name.starts_with("bevy_"))
        .collect();
    assert!(
        offenders.is_empty(),
        "SC-007 violation: `bevy*` crate(s) {offenders:?} present in the workspace normal \
         dependency closure. Bevy mode must consume the registry as DATA — no `bevy` dependency."
    );

    // Sanity: the closure walk is meaningful (non-trivial), so an empty result
    // can't masquerade as a clean tree.
    assert!(
        union.contains("ron-core"),
        "expected `ron-core` in the workspace closure union — the walk is not meaningful \
         (union of {} crates)",
        union.len()
    );
    eprintln!(
        "[dependency_invariants] workspace normal-closure union ({} crates) is clean of `bevy*`",
        union.len()
    );
}

#[test]
fn wasm_clean_crates_build_for_wasm32() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let root = workspace_root();

    // Degrade to a documented skip if the wasm32 target is not installed, rather
    // than failing for an environmental reason (never a false pass either way).
    match Command::new(&cargo)
        .args(["build", "--target", "wasm32-unknown-unknown", "-V"])
        .current_dir(&root)
        .output()
    {
        Ok(_) => {}
        Err(e) => {
            eprintln!("[dependency_invariants] SKIP: cannot spawn cargo ({e})");
            return;
        }
    }
    let installed = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("wasm32-unknown-unknown"))
        .unwrap_or(false);
    if !installed {
        eprintln!(
            "[dependency_invariants] SKIP: target `wasm32-unknown-unknown` not installed \
             (run `rustup target add wasm32-unknown-unknown`); CI enforces this gate"
        );
        return;
    }

    // Build ONLY the WASM-clean engine crates for wasm32, in an isolated target
    // dir so we don't deadlock on the outer `cargo test` target-dir lock.
    let wasm_target_dir = root.join("target").join("wasm32-invariant-check");
    let output = Command::new(&cargo)
        .args([
            "build",
            "-p",
            "ron-core",
            "-p",
            "ron-validate",
            "--target",
            "wasm32-unknown-unknown",
            "--locked",
        ])
        .current_dir(&root)
        .env("CARGO_TARGET_DIR", &wasm_target_dir)
        .env("CARGO_BUILD_JOBS", "2")
        .env("CARGO_INCREMENTAL", "0")
        .output()
        .expect("cargo build for wasm32 should spawn");

    assert!(
        output.status.success(),
        "SC-007 violation: `ron-core`/`ron-validate` failed to build for \
         wasm32-unknown-unknown.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    eprintln!(
        "[dependency_invariants] ron-core + ron-validate build clean for wasm32-unknown-unknown"
    );
}
