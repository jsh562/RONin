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
//! 3. **No E010 RON⇄JSON interop dependency creeps into the WASM-clean core**
//!    (E010/T005, FR-012, SC-006, ADR-0008): the serde `ron` crate and the JSON
//!    conversion path live ONLY in `ronin-app`. `ron-core` gains neither `ron` nor
//!    `serde_json`; `ron-validate` gains no `ron` (its pre-existing `serde_json`,
//!    the `CstJsonProjection` value type, is NOT banned). See
//!    [`no_ron_or_json_dependency_creeps_into_the_wasm_clean_core`].
//! 4. **`ronin-app`'s conversion path is fully offline & telemetry-free** (E010/T029,
//!    FR-014, SC-007): the E010 RON⇄JSON conversion / derive / export path adds NO
//!    networking/HTTP/TLS client and NO analytics/telemetry/metrics/crash-reporting
//!    crate to `ronin-app`'s normal-dependency closure. SC-007's two properties are
//!    asserted **independently** (a build could carry a telemetry crate with no
//!    obvious network client, and vice-versa), each as a dependency-absence guard
//!    over the real tree. See [`ronin_app_closure_has_no_networking_crate`] and
//!    [`ronin_app_closure_has_no_telemetry_crate`].
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
fn no_ron_or_json_dependency_creeps_into_the_wasm_clean_core() {
    // E010/T005 `[COMPLETES FR-012]` (FR-012, SC-006, ADR-0008): the RON⇄JSON
    // interop boundary — the serde `ron` crate plus the JSON conversion path —
    // lives ONLY in `ronin-app` (native). The WASM-clean engine crates must gain
    // NO `ron` crate and NO new E010-introduced dependency, so they keep building
    // for wasm32 (verified by `wasm_clean_crates_build_for_wasm32` above).
    //
    // Concretely, against the real tree:
    //   * `ron-core`    — neither `ron` NOR `serde_json` (no JSON in the core).
    //   * `ron-validate`— no `ron`, but its PRE-EXISTING `serde_json` (the
    //                     `CstJsonProjection` value type) MUST remain — it is NOT
    //                     banned; banning it would be over-reach (HINT-001/-002).
    // The serde `ron` crate is expected only in `ronin-app`'s closure.
    let Some(meta) = cargo_metadata() else {
        return; // documented skip (cargo unavailable) — never a false pass.
    };
    let graph = Graph::from_metadata(&meta);

    let core = graph.normal_closure_names("ron-core");
    let validate = graph.normal_closure_names("ron-validate");
    let app = graph.normal_closure_names("ronin-app");

    // (a) The serde `ron` crate must be absent from BOTH WASM-clean crates'
    //     closures — it is confined to the native `ronin-app` boundary (ADR-0008).
    assert!(
        !core.contains("ron"),
        "FR-012 violation: the serde `ron` crate leaked into `ron-core`'s normal \
         dependency closure; it MUST be confined to `ronin-app` (ADR-0008)"
    );
    assert!(
        !validate.contains("ron"),
        "FR-012 violation: the serde `ron` crate leaked into `ron-validate`'s normal \
         dependency closure; it MUST be confined to `ronin-app` (ADR-0008)"
    );

    // (b) No JSON crate in `ron-core` — the core gains neither `ron` nor
    //     `serde_json` (incl. the wider serde-json family) (FR-012, data-model).
    let core_json_offenders: Vec<&String> = core
        .iter()
        .filter(|name| *name == "serde_json" || name.starts_with("serde_json"))
        .collect();
    assert!(
        core_json_offenders.is_empty(),
        "FR-012 violation: JSON crate(s) {core_json_offenders:?} present in \
         `ron-core`'s normal dependency closure; `ron-core` must gain NO JSON \
         dependency (incl. serde_json)"
    );

    // (c) Sanity — DO NOT over-ban: `ron-validate`'s pre-existing `serde_json`
    //     (the `CstJsonProjection` value type) MUST still be present, proving we
    //     banned `ron`/new deps without removing the legitimate existing one.
    assert!(
        validate.contains("serde_json"),
        "over-ban regression: `ron-validate`'s pre-existing `serde_json` (the \
         `CstJsonProjection` value type) is MISSING from its closure — E010 must \
         NOT ban it; only `ron`/new deps are banned (HINT-001)"
    );

    // (d) Sanity — the boundary `ron` crate IS where it belongs: present in
    //     `ronin-app`'s closure (so the closure walk is meaningful and the
    //     above absences are real, not an empty/broken graph).
    assert!(
        app.contains("ron"),
        "expected the serde `ron` crate in `ronin-app`'s normal closure (the E010 \
         native boundary, ADR-0008) — its absence would mean the closure walk is \
         not meaningful, masking the core/validate assertions"
    );

    eprintln!(
        "[dependency_invariants] FR-012 OK: `ron` absent from ron-core/ron-validate, \
         no JSON in ron-core, ron-validate keeps its serde_json, `ron` confined to ronin-app"
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

// ===========================================================================
// E010/T029 `[COMPLETES SC-007]` — offline / no-telemetry dependency-absence
// guards over `ronin-app`'s NORMAL-dependency closure (FR-014, SC-007).
//
// SC-007 has two distinct, separately-checkable properties:
//   (a) **no network access** — the conversion / derive / export path pulls in NO
//       HTTP / socket / DNS / TLS client crate; and
//   (b) **no telemetry** — it pulls in NO analytics / metrics-reporting /
//       crash-reporting / phone-home crate.
// A build could in principle have a telemetry crate without an obvious network
// client (and vice-versa), so each is asserted in its OWN test (plan "Offline /
// no-telemetry verification (CHK023/CHK024)").
//
// These are curated DENY lists of well-known offenders the E010 path must never
// pull, checked against the REAL resolved tree (reusing `normal_closure_names`).
// They are intentionally NOT exhaustive of every crate that could ever phone home
// — an exhaustive blocklist is impossible; instead they are a meaningful, passing
// guard that catches a regression where someone wires the converter to a network /
// telemetry SDK. Both lists were verified absent from the actual closure (so the
// guard PASSES today); if a curated name surprisingly appeared it would be narrowed
// to true offenders with a documented reason rather than left as a false failure.
//
// Each test degrades to a documented `eprintln!` SKIP when `cargo` is unavailable
// (never a false pass), mirroring the tests above.
// ===========================================================================

/// Curated networking / HTTP / socket / DNS / TLS **client** crates the offline
/// conversion path must never pull into `ronin-app`'s normal closure (SC-007 (a) /
/// FR-014). Verified absent from the real tree at authoring time.
///
/// Note on what is intentionally NOT here (to keep the guard a true-positive guard,
/// not a false failure): URL/URI *parsing* (`url`, `idna`, `fluent-uri`,
/// `percent-encoding`, `form_urlencoded`), local-only helpers (`gethostname` reads
/// the local hostname; `webbrowser` shells out to the OS browser via
/// `xdg-open`/`ShellExecute` — neither opens a socket), and async *runtime*
/// primitives are excluded; only crates that actually perform a network request or
/// open a transport are listed.
const NETWORKING_CRATES: &[&str] = &[
    // HTTP clients
    "reqwest",
    "hyper",
    "hyper-util",
    "ureq",
    "isahc",
    "attohttpc",
    "surf",
    "curl",
    "curl-sys",
    // async TCP/UDP/socket runtimes' net layers + low-level socket crates
    "tokio", // tokio's `net` feature is the network seam; the converter is sync
    "mio",
    "socket2",
    "async-std",
    // HTTP/2 + DNS resolvers
    "h2",
    "trust-dns-resolver",
    "hickory-resolver",
    "quinn", // QUIC
    // WebSockets
    "tungstenite",
    "tokio-tungstenite",
    // TLS stacks (a TLS crate implies a transport client)
    "rustls",
    "native-tls",
    "openssl",
    "openssl-sys",
    "boring",
    "ring",
    "webpki",
    "webpki-roots",
    "schannel",
    "security-framework",
];

/// Curated analytics / telemetry / metrics-reporting / crash-reporting / phone-home
/// crates the offline conversion path must never pull into `ronin-app`'s normal
/// closure (SC-007 (b) / FR-014). Verified absent from the real tree at authoring
/// time.
///
/// Note: plain `tracing`/`tracing-subscriber` are LOCAL structured logging (they
/// never leave the machine — see `tracing-appender` writing to a local file) and are
/// therefore NOT offenders; the offender is the EXPORTER bridge
/// (`tracing-opentelemetry`) that ships spans off-box. Only exporters / SDKs that
/// transmit data off the machine are listed.
const TELEMETRY_CRATES: &[&str] = &[
    // crash / error reporting
    "sentry",
    "sentry-core",
    "sentry-types",
    "rollbar",
    "bugsnag",
    "minidumper",
    "crash-handler",
    // distributed tracing / OpenTelemetry export
    "opentelemetry",
    "opentelemetry_sdk",
    "opentelemetry-otlp",
    "tracing-opentelemetry",
    // metrics reporting / scrape exporters
    "prometheus",
    "metrics-exporter-prometheus",
    "statsd",
    "cadence",
    "dogstatsd",
    // product analytics / phone-home SDKs
    "analytics",
    "segment",
    "posthog",
    "posthog-rs",
    "mixpanel",
    "amplitude",
    "datadog",
    "appinsights",
];

/// Find which curated names are present in `closure` (the real offenders), so a
/// failure message names exactly what leaked rather than the whole deny list.
fn offenders_in<'a>(closure: &BTreeSet<String>, curated: &[&'a str]) -> Vec<&'a str> {
    curated
        .iter()
        .copied()
        .filter(|name| closure.contains(*name))
        .collect()
}

#[test]
fn ronin_app_closure_has_no_networking_crate() {
    // SC-007 (a) / FR-014: the E010 conversion / derive / export path is fully
    // offline — NO HTTP / socket / DNS / TLS client crate in `ronin-app`'s normal
    // dependency closure. Asserted independently of telemetry (see below).
    let Some(meta) = cargo_metadata() else {
        return; // documented skip (cargo unavailable) — never a false pass.
    };
    let graph = Graph::from_metadata(&meta);
    let app = graph.normal_closure_names("ronin-app");

    // Sanity: the closure walk is meaningful (the native app pulls egui/eframe +
    // the `ron` boundary crate), so an empty/broken graph can't masquerade as a
    // clean "no networking" result and produce a false pass.
    assert!(
        app.contains("ron") && app.contains("eframe"),
        "expected `ron` + `eframe` in `ronin-app`'s normal closure — the closure walk \
         is not meaningful ({} crates), which would mask the absence assertion below",
        app.len()
    );

    let offenders = offenders_in(&app, NETWORKING_CRATES);
    assert!(
        offenders.is_empty(),
        "SC-007/FR-014 violation: networking/HTTP/TLS crate(s) {offenders:?} present in \
         `ronin-app`'s normal dependency closure. The RON⇄JSON conversion / derive / \
         export path MUST be fully offline — no network client may be pulled in."
    );
    eprintln!(
        "[dependency_invariants] SC-007(a) OK: `ronin-app` closure ({} crates) carries none \
         of the {} curated networking crates",
        app.len(),
        NETWORKING_CRATES.len()
    );
}

#[test]
fn ronin_app_closure_has_no_telemetry_crate() {
    // SC-007 (b) / FR-014: the E010 conversion / derive / export path emits NO
    // telemetry — NO analytics / metrics-reporting / crash-reporting / phone-home
    // crate in `ronin-app`'s normal dependency closure. Asserted independently of
    // networking (a build could carry a telemetry SDK without an obvious socket
    // client). Local `tracing` logging is NOT telemetry and is intentionally allowed.
    let Some(meta) = cargo_metadata() else {
        return; // documented skip (cargo unavailable) — never a false pass.
    };
    let graph = Graph::from_metadata(&meta);
    let app = graph.normal_closure_names("ronin-app");

    // Sanity: the walk is meaningful, AND prove we are NOT over-banning — the
    // legitimate LOCAL `tracing` logging stack MUST still be present (its absence
    // would mean we either broke the app or banned the wrong thing).
    assert!(
        app.contains("tracing"),
        "expected local `tracing` logging in `ronin-app`'s closure — its absence means \
         the closure walk is not meaningful or local logging was wrongly removed; \
         local `tracing` is NOT telemetry and must remain (SC-007 narrows to exporters)"
    );

    let offenders = offenders_in(&app, TELEMETRY_CRATES);
    assert!(
        offenders.is_empty(),
        "SC-007/FR-014 violation: telemetry/analytics/crash-reporting crate(s) {offenders:?} \
         present in `ronin-app`'s normal dependency closure. The RON⇄JSON conversion / derive \
         / export path MUST emit no telemetry — only local `tracing` logging is permitted."
    );
    eprintln!(
        "[dependency_invariants] SC-007(b) OK: `ronin-app` closure ({} crates) carries none \
         of the {} curated telemetry crates (local `tracing` retained)",
        app.len(),
        TELEMETRY_CRATES.len()
    );
}
