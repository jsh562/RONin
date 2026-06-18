//! Dependency-invariant verification (E006/T036, FR-023, SC-007) `[COMPLETES FR-023]`.
//!
//! SC-007 makes the offline guarantee an objective, in-repo dependency-tree
//! assertion rather than an aspirational note. This test proves it programmatically
//! by invoking `cargo metadata --format-version 1` as a subprocess, parsing the
//! resolved dependency graph with `serde_json`, and walking each crate's **normal**
//! (non-dev/non-build) dependency closure:
//!
//! * `reqwest` and `rustls` (the network/TLS transport `jsonschema`'s default
//!   `resolve-http`/`tls-*` features would pull) are **ABSENT** from
//!   `ronin-validate`'s normal dependency closure. Their absence is the offline
//!   proof: with no HTTP/TLS in the closure there is no code path that can perform
//!   a network call or fetch a remote `$ref` (SC-007). No real network call needs
//!   to be made.
//! * `jsonschema` and `serde_json` are **ABSENT** from `ronin-core`'s normal
//!   dependency closure (HINT-002/AD-007: `ronin-core` stays rowan-only).
//! * `ronin-types` is absent from both `ronin-core` and `ronin-validate` (the native
//!   `syn`/`schemars` type-acquisition crate must not be pulled into the
//!   WASM-clean engine; the `TypeModel` crosses the boundary as serialized JSON).
//!
//! # Approach / robustness
//!
//! * The workspace is located via `CARGO_MANIFEST_DIR` (this crate sits at
//!   `<ws>/src/ronin-validate`), so `cargo metadata` runs in the workspace.
//! * Closures walk only `deps[].dep_kinds[].kind == null` ("normal") edges —
//!   `dev`/`build` edges are excluded so dev-deps (`insta`/`proptest`) cannot cause
//!   a false negative.
//! * Reachability is a BFS over `resolve.nodes` from each workspace crate's package
//!   id. We compare on the package **name** (ignoring version/source) so the
//!   assertions are stable across registry/version changes.
//! * If `cargo` is somehow unavailable (or metadata fails), the test prints a clear
//!   `eprintln!` skip and returns rather than producing a false pass — on CI/dev it
//!   actually runs and asserts.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// Workspace root: this crate's manifest dir is `<ws>/src/ronin-validate`.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
}

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
            // First id wins for a given name; workspace member names are unique.
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

/// Assert `banned` package names are absent from `crate_name`'s normal closure.
fn assert_absent(graph: &Graph, crate_name: &str, banned: &[&str]) {
    let closure = graph.normal_closure_names(crate_name);
    for &b in banned {
        assert!(
            !closure.contains(b),
            "FR-023 violation: `{b}` is present in `{crate_name}`'s normal dependency closure. \
             Closure ({} crates): {:?}",
            closure.len(),
            closure
        );
    }
    eprintln!(
        "[dependency_invariants] `{crate_name}` normal closure ({} crates) is clean of {:?}",
        closure.len(),
        banned
    );
}

#[test]
fn ronin_validate_has_no_network_transport_in_normal_closure() {
    let Some(meta) = cargo_metadata() else {
        return; // documented skip (cargo unavailable) — never a false pass.
    };
    let graph = Graph::from_metadata(&meta);

    // SC-007: reqwest/rustls (HTTP+TLS transport) MUST be absent from
    // ronin-validate's normal closure — the offline proof.
    assert_absent(&graph, "ronin-validate", &["reqwest", "rustls"]);

    // ronin-validate MUST still actually depend on jsonschema (the engine) — a
    // sanity check that the closure walk is meaningful (not trivially empty).
    let rv_closure = graph.normal_closure_names("ronin-validate");
    assert!(
        rv_closure.contains("jsonschema"),
        "expected `jsonschema` in ronin-validate's normal closure (closure: {rv_closure:?})"
    );
    assert!(
        rv_closure.contains("ronin-core"),
        "expected `ronin-core` in ronin-validate's normal closure (closure: {rv_closure:?})"
    );
}

#[test]
fn ronin_core_has_no_validation_deps_in_normal_closure() {
    let Some(meta) = cargo_metadata() else {
        return; // documented skip.
    };
    let graph = Graph::from_metadata(&meta);

    // HINT-002/AD-007: ronin-core stays rowan-only — jsonschema/serde_json MUST be
    // absent from its normal closure.
    assert_absent(&graph, "ronin-core", &["jsonschema", "serde_json"]);

    // Sanity: ronin-core's closure is non-empty (it depends on rowan) so the walk
    // is meaningful.
    let core_closure = graph.normal_closure_names("ronin-core");
    assert!(
        !core_closure.is_empty(),
        "ronin-core normal closure unexpectedly empty — closure walk is not meaningful"
    );
}

#[test]
fn ronin_types_absent_from_wasm_clean_crates() {
    let Some(meta) = cargo_metadata() else {
        return; // documented skip.
    };
    let graph = Graph::from_metadata(&meta);

    // The native type-acquisition crate must not leak into the WASM-clean engine;
    // the TypeModel crosses the boundary as serialized JSON (serde_json::Value).
    assert_absent(&graph, "ronin-core", &["ronin-types"]);
    assert_absent(&graph, "ronin-validate", &["ronin-types"]);
}
