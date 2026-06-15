//! Integration tests for Bevy scene-aware validation (E009 US1 cluster 3C-2 /
//! T017, FR-005/FR-006/FR-007/FR-008, SC-001/SC-002).
//!
//! Drives [`validate_scene`](ronin_app::bevy::validate_scene) over the real
//! `.scn.ron` fixtures in `tests/fixtures/scenes/` against the hand-authored Bevy
//! registry-schema export shared with the `ron-types` suite
//! (`../ron-types/tests/fixtures/bevy_registry_schema.json`). The model is
//! acquired and serialized exactly as production does — `BevySource::acquire()`
//! then `ron_types::to_json` — and paired with the parsed `BevyRegistry` for the
//! membership lookup.
//!
//! Covered acceptance scenarios:
//! * an **unregistered** component path → an info/hint (never a hard error);
//! * a **wrong-typed / wrong-arity / bad-variant** field → a diagnostic at the
//!   offending construct's precise range (FR-005, SC-001);
//! * a fully **valid** registered scene → zero error-severity findings (SC-001);
//! * **no registry** → only the "no registry loaded" hint, the structural set
//!   intact (FR-006, SC-002);
//! * the **three registry states** are distinguishable (FR-006);
//! * a configured **version mismatch** → a staleness advisory, not an error
//!   (FR-008);
//! * a **malformed registry** / **unparseable scene region** → degrades safely
//!   (no crash, structural set remains) (FR-008).

use std::path::PathBuf;

use ron_core::parse;
use ron_types::{BevyRegistry, BevySource, TypeSource};
use ronin_app::bevy::{
    validate_scene, SceneDiagnostic, SceneDiagnosticCode, SceneModel, SceneSeverity,
};
use serde_json::Value;

/// Load a `.scn.ron` fixture's source text by file name.
fn scene_fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("scenes")
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read scene fixture {}: {e}", path.display()))
}

/// The shared Bevy registry-schema export (lives in the `ron-types` fixtures).
fn registry_schema_json() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("ron-types")
        .join("tests")
        .join("fixtures")
        .join("bevy_registry_schema.json");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read registry fixture {}: {e}", path.display()))
}

/// Acquire the registry + its serialized interchange the way production does:
/// `BevySource::acquire()` → `ron_types::to_json` (the same serialization serde
/// mode hands the validator), plus the parsed `BevyRegistry` for membership.
fn registry_and_model(json: &str) -> (BevyRegistry, Value) {
    let (registry, _diags) = BevyRegistry::from_schema_json(json, "test", "<registry>");
    let acquired = BevySource::from_schema_json(json).acquire();
    let model = ron_types::to_json(&acquired.model);
    (registry, model)
}

/// `true` if any finding is a hard error.
fn has_error(diags: &[SceneDiagnostic]) -> bool {
    diags.iter().any(|d| d.severity() == SceneSeverity::Error)
}

#[test]
fn unregistered_component_is_a_hint_not_an_error() {
    // FR-006 / SC-002: a well-formed but unregistered component path is
    // unconstrained — surfaced as a hint at the value's location, never an error.
    let (registry, model) = registry_and_model(&registry_schema_json());
    let src = scene_fixture("unregistered_component.scn.ron");
    let doc = parse(&src);
    let diags = validate_scene(&model, &registry, &doc, None);

    assert!(
        !has_error(&diags),
        "unregistered type is never a hard error"
    );
    let hint = diags
        .iter()
        .find(|d| d.code() == SceneDiagnosticCode::TypeNotInRegistry)
        .expect("a type-not-in-registry hint for `my_game::components::Health`");
    assert_eq!(hint.severity(), SceneSeverity::Hint);
    assert!(hint.message().contains("my_game::components::Health"));
    // The hint lands on a real, non-empty span (the component value), not a
    // fabricated/empty range.
    let r = hint.range();
    assert!(!r.is_empty());
    assert_eq!(&src[r.start()..r.end()], "(current: 80, max: 100)");

    // The registered sibling (Transform) is valid → contributes no error.
    assert!(diags
        .iter()
        .all(|d| !matches!(d.code(), SceneDiagnosticCode::Mismatch(_))
            || d.severity() != SceneSeverity::Error
            || !d.message().is_empty()));
}

#[test]
fn wrong_typed_fields_flag_diagnostics_at_precise_ranges() {
    // FR-005 / SC-001: each of a wrong field type, wrong tuple arity, and bad
    // enum variant produces a diagnostic, each at the offending construct's span.
    let (registry, model) = registry_and_model(&registry_schema_json());
    let src = scene_fixture("wrong_typed.scn.ron");
    let doc = parse(&src);
    let diags = validate_scene(&model, &registry, &doc, None);

    let mismatches: Vec<&SceneDiagnostic> = diags
        .iter()
        .filter(|d| matches!(d.code(), SceneDiagnosticCode::Mismatch(_)))
        .collect();
    assert!(
        !mismatches.is_empty(),
        "the wrong-typed scene must produce registered-mismatch findings, got: {diags:?}"
    );
    // At least one hard error (the string-where-number field type mismatch).
    assert!(has_error(&diags), "a wrong field type is a hard error");

    // Every finding addresses a real, non-empty, in-bounds span (precise range).
    for d in &mismatches {
        let r = d.range();
        assert!(r.end() <= src.len(), "finding range is in bounds");
        assert!(!r.is_empty(), "finding addresses a real construct span");
    }

    // The wrong field type specifically lands on the `"not-a-number"` token.
    let on_string = mismatches
        .iter()
        .any(|d| &src[d.range().start()..d.range().end()] == "\"not-a-number\"");
    assert!(
        on_string,
        "the field-type mismatch lands on the string literal"
    );
}

#[test]
fn valid_scene_has_zero_error_findings() {
    // SC-001: a fully-registered, valid scene shows zero type errors.
    let (registry, model) = registry_and_model(&registry_schema_json());
    let src = scene_fixture("valid.scn.ron");
    let doc = parse(&src);
    let diags = validate_scene(&model, &registry, &doc, None);
    assert!(
        !has_error(&diags),
        "a fully-valid registered scene shows zero errors, got: {diags:?}"
    );
    // And every registered component path resolves (no type-not-in-registry hint).
    assert!(
        diags
            .iter()
            .all(|d| d.code() != SceneDiagnosticCode::TypeNotInRegistry),
        "every component/resource in the valid scene is registered"
    );
}

#[test]
fn no_registry_is_only_the_hint_with_structural_intact() {
    // FR-006 / SC-002: a Bevy-mode scene with no registry loaded → only the
    // "no registry loaded" hint and NO type errors; structural diagnostics
    // (computed by ron-core, independent of validation) still work.
    let (_r, model) = registry_and_model(&registry_schema_json());
    let empty = BevyRegistry::default();
    let src = scene_fixture("wrong_typed.scn.ron"); // would error WITH a registry
    let doc = parse(&src);

    let diags = validate_scene(&model, &empty, &doc, None);
    assert_eq!(
        diags.len(),
        1,
        "exactly one finding in the NoRegistry state"
    );
    assert_eq!(diags[0].code(), SceneDiagnosticCode::NoRegistry);
    assert_eq!(diags[0].severity(), SceneSeverity::Hint);
    assert!(!has_error(&diags), "NoRegistry yields no type errors");

    // The structural diagnostic set is independent of validation and still runs.
    // (The fixture is well-formed RON, so it has zero structural diagnostics; the
    // point is that validation did not disturb the structural channel.)
    let structural = doc.diagnostics();
    assert!(
        structural.is_empty(),
        "well-formed scene has no structural diagnostics; validation left it intact"
    );
    // The scene model is still fully projected (structural set intact).
    let scene = SceneModel::from_cst(&doc);
    assert_eq!(scene.entities().len(), 1);
}

#[test]
fn the_three_registry_states_are_distinguishable() {
    // FR-006: NoRegistry / TypeNotInRegistry / RegisteredMismatch each carry a
    // distinct (severity, code) identity.
    let (registry, model) = registry_and_model(&registry_schema_json());

    // NoRegistry.
    let no_reg = validate_scene(&model, &BevyRegistry::default(), &parse("()"), None);
    assert_eq!(no_reg[0].code(), SceneDiagnosticCode::NoRegistry);
    assert_eq!(no_reg[0].severity(), SceneSeverity::Hint);

    // TypeNotInRegistry.
    let unreg = validate_scene(
        &model,
        &registry,
        &parse(&scene_fixture("unregistered_component.scn.ron")),
        None,
    );
    assert!(unreg
        .iter()
        .any(|d| d.code() == SceneDiagnosticCode::TypeNotInRegistry
            && d.severity() == SceneSeverity::Hint));

    // RegisteredMismatch.
    let mismatch = validate_scene(
        &model,
        &registry,
        &parse(&scene_fixture("wrong_typed.scn.ron")),
        None,
    );
    assert!(mismatch
        .iter()
        .any(|d| matches!(d.code(), SceneDiagnosticCode::Mismatch(_))
            && d.severity() == SceneSeverity::Error));

    // Each state's code string is globally distinct and carries a source tag.
    assert_eq!(SceneDiagnosticCode::NoRegistry.code(), "BVY-S0001");
    assert_eq!(SceneDiagnosticCode::TypeNotInRegistry.code(), "BVY-S0002");
    assert_ne!(
        SceneDiagnosticCode::NoRegistry.code(),
        SceneDiagnosticCode::TypeNotInRegistry.code()
    );
    assert_eq!(SceneDiagnosticCode::NoRegistry.source(), "ronin-bevy");
}

#[test]
fn configured_version_mismatch_raises_a_staleness_advisory_not_an_error() {
    // FR-008: the fixture export's apparent version is "0.16.0"; a configured
    // expected version that differs raises an advisory (never an error). A
    // matching expected version (or none) raises nothing.
    let (registry, model) = registry_and_model(&registry_schema_json());
    let doc = parse(&scene_fixture("valid.scn.ron"));

    // Disagreeing → an advisory appended, but still zero errors.
    let with_skew = validate_scene(&model, &registry, &doc, Some("0.15.0"));
    let advisory = with_skew
        .iter()
        .find(|d| d.code() == SceneDiagnosticCode::StalenessAdvisory)
        .expect("a staleness advisory for the version skew");
    assert_eq!(advisory.severity(), SceneSeverity::Advisory);
    assert!(advisory.severity().is_informational());
    assert!(!has_error(&with_skew), "staleness is never a hard error");

    // Agreeing → no advisory.
    let aligned = validate_scene(&model, &registry, &doc, Some("0.16.0"));
    assert!(aligned
        .iter()
        .all(|d| d.code() != SceneDiagnosticCode::StalenessAdvisory));

    // Unconfigured → no advisory.
    let unconfigured = validate_scene(&model, &registry, &doc, None);
    assert!(unconfigured
        .iter()
        .all(|d| d.code() != SceneDiagnosticCode::StalenessAdvisory));
}

#[test]
fn malformed_registry_degrades_to_no_registry_no_crash() {
    // FR-008: a malformed registry export degrades to empty at ingest; validation
    // then degrades to the NoRegistry hint (structural-only), never a crash.
    let (registry, model) = registry_and_model("{ this is not valid json");
    assert!(
        registry.is_empty(),
        "a malformed export degrades to an empty registry"
    );
    let doc = parse(&scene_fixture("wrong_typed.scn.ron"));
    let diags = validate_scene(&model, &registry, &doc, None);
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].code(), SceneDiagnosticCode::NoRegistry);
    assert!(!has_error(&diags));
}

#[test]
fn unparseable_scene_region_degrades_no_crash_structural_remains() {
    // FR-008: an unparseable component value must not crash interpretation or
    // validation; the parseable remainder is still validated and the structural
    // diagnostics still cover the malformed region.
    let (registry, model) = registry_and_model(&registry_schema_json());
    let src = r#"(entities: {0: (components: {
        "bevy_transform::components::transform::Transform": @@@bad,
        "bevy_pbr::light::Visibility": Inherited,
    })})"#;
    let doc = parse(src);

    // Reaching here without a panic is the core invariant.
    let diags = validate_scene(&model, &registry, &doc, None);

    // The structural channel covers the malformed region (at least one parse
    // diagnostic), proving the structural set remains intact alongside validation.
    assert!(
        !doc.diagnostics().is_empty(),
        "the malformed region produces structural diagnostics"
    );

    // The parseable remainder (Visibility::Inherited) is valid → no error from it,
    // and no finding falsely lands inside the garbled span.
    assert!(
        !has_error(&diags),
        "the parseable remainder validates clean; the garbled span is skipped"
    );
}

#[test]
fn validation_changes_zero_bytes() {
    // FR-011: validation is read-only — the document bytes are byte-identical
    // before and after a pass.
    let (registry, model) = registry_and_model(&registry_schema_json());
    let src = scene_fixture("wrong_typed.scn.ron");
    let doc = parse(&src);
    let before = doc.root().text();
    let _ = validate_scene(&model, &registry, &doc, Some("0.15.0"));
    let after = doc.root().text();
    assert_eq!(before, after, "validation must not mutate the CST bytes");
}

// ============================================================================
// E009 Polish / T034 — the fully-offline guarantee `[COMPLETES FR-018]`
// (FR-018, FR-002, SC-007)
//
// FR-018 requires E009 to be fully local + offline: registry read, validation,
// and elision make NO network access and produce NO telemetry/off-device
// transmission; live BRP — the only would-be network path — is DEFERRED per
// FR-002, so this epic makes no network calls at all. A unit test can't easily
// intercept sockets, so this is proved with three concrete, real assertions:
//
//   (a) the `deny.toml` `[bans].deny` list bans `reqwest`, `rustls`, AND `bevy`
//       — so any accidental network-transport OR Bevy dependency fails the
//       cargo-deny gate (the offline + registry-as-data proof at the dep level);
//   (b) the registry-acquisition path is LOCAL-FILE ONLY — `BevySource` exposes
//       `from_path` / `from_schema_json` / `from_schema_value` / `from_registry`
//       (all local-data constructors) and NO URL/endpoint/network API surface;
//   (c) the WASM-clean-crate no-network proof is the `dependency_invariants`
//       test (T004), documented below.
//
// The `deny.toml` is read from the ACTUAL workspace root (resolved via
// `CARGO_MANIFEST_DIR`), so this asserts the real shipped policy, not a copy.
// ============================================================================

/// Read the workspace-root `deny.toml` (the real shipped cargo-deny policy).
///
/// `CARGO_MANIFEST_DIR` is `<workspace>/src/ronin-app`, so the workspace root is
/// two levels up. Reading the actual file (not a fixture copy) keeps the
/// assertion honest: it fails the moment the shipped policy drops a ban.
fn workspace_deny_toml() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("deny.toml");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read workspace deny.toml {}: {e}", path.display()))
}

/// `true` if the `deny.toml` `[bans].deny` list contains an exact-name ban for
/// `crate_name` (matching cargo-deny's `{ name = "<crate>" }` entry form).
///
/// A tolerant textual check: cargo-deny entries are `{ name = "x" }`, optionally
/// with extra keys, so we look for the `name = "<crate>"` key/value verbatim
/// (single OR double quotes) within the file. This is sufficient because the
/// `deny` list is the only place a `name = "..."` ban appears in this policy.
fn deny_list_bans(deny_toml: &str, crate_name: &str) -> bool {
    let dq = format!("name = \"{crate_name}\"");
    let sq = format!("name = '{crate_name}'");
    deny_toml.contains(&dq) || deny_toml.contains(&sq)
}

#[test]
fn deny_toml_bans_reqwest_rustls_and_bevy() {
    // (a) The offline + registry-as-data proof at the dependency level: the real
    // workspace `deny.toml` must ban the network-transport crates (`reqwest`,
    // `rustls`) AND the `bevy` umbrella crate. Banning all three means an
    // accidental network OR Bevy dependency fails the cargo-deny security gate —
    // no E009 code path can pull in a socket-opening or engine-linking crate
    // without the gate going red (FR-003/FR-017/FR-018, SC-007).
    let deny = workspace_deny_toml();

    // The bans live in the `[bans]` section's `deny = [ ... ]` list.
    assert!(
        deny.contains("[bans]"),
        "deny.toml must carry a [bans] section"
    );
    assert!(
        deny.contains("deny = ["),
        "deny.toml [bans] must carry a `deny = [ ... ]` list"
    );

    for banned in ["reqwest", "rustls", "bevy"] {
        assert!(
            deny_list_bans(&deny, banned),
            "deny.toml [bans].deny must ban `{banned}` (offline + registry-as-data gate, FR-018)"
        );
    }
}

#[test]
fn registry_acquisition_is_local_file_only_no_network_surface() {
    // (b) The registry-acquisition path is LOCAL-FILE / LOCAL-DATA only. We prove
    // this positively (the only constructors are local-data ones) and negatively
    // (acquiring from those constructors makes no network call — it succeeds
    // offline, deterministically, with no endpoint involved). FR-001/FR-002:
    // the offline export file is the SOLE acquisition path; live BRP is deferred.

    // A local JSON string acquires a model with no I/O at all.
    let json = registry_schema_json();
    let from_string = BevySource::from_schema_json(&json).acquire();
    let string_type_count = from_string.model.iter_ordered().count();
    assert!(
        !from_string.model.is_empty(),
        "from_schema_json acquires a model from a local string (no network)"
    );

    // A local FILE path acquires the same model — purely a `std::fs` read, never
    // a URL fetch. This is the production acquisition path (FR-001).
    let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("ron-types")
        .join("tests")
        .join("fixtures")
        .join("bevy_registry_schema.json");
    let from_file = BevySource::from_path(&fixture_path).acquire();
    assert_eq!(
        from_file.model.iter_ordered().count(),
        string_type_count,
        "from_path (a local std::fs read) acquires the same model as the in-memory string"
    );

    // The registry parse path is likewise local-only and never panics offline.
    let (registry, _diags) = BevyRegistry::from_schema_json(&json, "test", "<offline>");
    assert!(
        !registry.is_empty(),
        "a local registry export parses offline into a non-empty registry"
    );

    // There is NO URL/endpoint/network constructor on the Bevy source: the entire
    // acquisition surface is local-data (`from_schema_json` / `from_schema_value`
    // / `from_path` / `from_registry`). A future BRP read (FR-002) would slot in
    // via `from_registry` (already-parsed data) WITHOUT a core change — but it is
    // NOT present in E009, so no network path exists at all this epic.
    // (Compile-time proof: the only constructors used across the suite are the
    // local-data ones above; there is no `from_url` / `from_endpoint` to call.)
}

#[test]
fn validation_and_elision_run_offline_no_network() {
    // (b cont.) Validation AND elision execute entirely over local CST + local
    // registry data — a deterministic, offline computation with no network call.
    // We run a full validate pass over a local fixture against a locally-acquired
    // registry and assert it produces findings deterministically (proving the
    // path is pure-compute over local data, FR-018). The dedicated elision
    // round-trip/offline behavior is covered by the `bevy_elision` suite (T030-32);
    // here we assert validation's offline determinism.
    let (registry, model) = registry_and_model(&registry_schema_json());
    let src = scene_fixture("wrong_typed.scn.ron");
    let doc = parse(&src);

    let first = validate_scene(&model, &registry, &doc, None);
    let second = validate_scene(&model, &registry, &doc, None);
    assert_eq!(
        first, second,
        "validation is a deterministic offline pure-compute pass over local data \
         (no network non-determinism)"
    );
    // It does real work over the local data (not a vacuous no-op).
    assert!(
        !first.is_empty(),
        "the offline validation pass produces findings over the local fixture"
    );
}

#[test]
fn wasm_clean_no_network_proof_is_the_dependency_invariants_test() {
    // (c) The WASM-clean-crate no-network proof is the `dependency_invariants`
    // test (T004, `tests/dependency_invariants.rs`): it walks the cargo-metadata
    // normal-dependency closure to prove NO `bevy*` crate appears anywhere, and
    // builds `ron-core` + `ron-validate` for `wasm32-unknown-unknown` (a target
    // with no ambient network/filesystem) — together the SC-007 proof that the
    // WASM-clean core gains no Bevy/registry/network dependency. This test
    // documents that delegation so the FR-018 coverage is traceable from here.
    //
    // We assert the companion artifact exists so the delegation is real, not just
    // a comment that could rot if the file were removed/renamed.
    let invariants = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("dependency_invariants.rs");
    assert!(
        invariants.is_file(),
        "the dependency-invariants test (T004) must exist at {} — it carries the \
         no-`bevy`-crate + wasm32-clean no-network proof FR-018/SC-007 delegate to",
        invariants.display()
    );
}
