//! E009 US2 cluster 4B-1 (T021/T022) — per-document mode + type-source selection
//! wired into the **live** document/validation path (FR-012/FR-013).
//!
//! These integration tests drive the *real* off-frame [`ReparseWorker`] round-trip
//! (request → spin-poll until the current result installs) and assert that the
//! document selects serde-vs-Bevy validation per its own [`ModeState`]:
//!
//! * a `.scn.ron` document in Bevy mode with a loaded registry validates through
//!   the **scene path** (`BVY-S####` / `ronin-bevy` scene findings) (FR-013);
//! * a `.ron` document in serde mode validates through the **serde path**
//!   (`RON-V####` / `ronin-types` findings) in the same session (regression) (FR-013);
//! * switching a document's mode re-routes which validator runs (zero-byte switch,
//!   FR-011/FR-013);
//! * two documents in different modes coexist with no cross-talk (FR-012);
//! * a serde-only document's diagnostics are unchanged by the Bevy wiring
//!   (regression gate).
//!
//! The mode/registry state lives **1:1 per document** (no global state), which is
//! exactly what guarantees per-document coexistence (FR-012). Bevy mode REPLACES
//! the active source with the bound registry (AD-003): the scene validator runs
//! instead of `validate_against`, and the serde `bound_type` is ignored.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ronin_app::bevy::mode::{Mode, ModeState, RegistryBindingConfig, RegistryBindingRule};
use ronin_app::document::EditorDocument;
use ronin_app::reparse::{BoundType, ReparseWorker};

/// A tiny valid registry export: a `game::Vec3` struct (x/y/z numbers) plus an
/// apparent Bevy version, written for the load tests.
const REGISTRY: &str = r##"{
    "bevyVersion": "0.16.0",
    "$defs": {
        "game::Vec3": {
            "kind": "Struct",
            "additionalProperties": false,
            "properties": {
                "x": { "type": "number" },
                "y": { "type": "number" },
                "z": { "type": "number" }
            },
            "required": ["x", "y", "z"],
            "reflectTypes": ["Default"]
        }
    }
}"##;

/// Request a reparse and spin-poll until a current result installs, or panic on
/// timeout. Drives the real off-frame worker (parse + mode-specific validation).
fn drive_reparse(doc: &mut EditorDocument, worker: &ReparseWorker) {
    doc.request_reparse(worker);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if doc.poll_parse(worker) {
            return;
        }
        if Instant::now() >= deadline {
            panic!("reparse did not land within timeout");
        }
        std::thread::yield_now();
    }
}

/// A fresh temp directory for a test, named by `tag`.
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ronin_bevy_validation_{tag}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write `REGISTRY` into `root/registry.json` and return the root.
fn registry_root(tag: &str) -> PathBuf {
    let root = temp_dir(tag);
    std::fs::write(root.join("registry.json"), REGISTRY).unwrap();
    root
}

/// A `RegistryBindingConfig` that binds every `.scn.ron` to `registry.json`.
fn scene_config() -> RegistryBindingConfig {
    RegistryBindingConfig {
        rules: vec![RegistryBindingRule {
            pattern: "**/*.scn.ron".to_string(),
            exclude: None,
            registry_export_path: PathBuf::from("registry.json"),
            mode: None,
            expected_bevy_version: None,
        }],
        ..Default::default()
    }
}

/// Resolve + load a Bevy `ModeState` for `doc_path` against `config`, rooted at
/// `root`. Asserts the registry actually loaded (so the scene path engages).
fn loaded_bevy_mode(config: &RegistryBindingConfig, doc_path: &Path, root: &Path) -> ModeState {
    let mut state = ModeState::resolve(config, Some(doc_path), None, None);
    assert_eq!(
        state.active_mode(),
        Mode::Bevy,
        "a .scn.ron auto-detects Bevy"
    );
    assert!(state.load_registry(root), "the bound registry must load");
    assert!(state.has_registry());
    state
}

/// A minimal serde `Entity { id: integer }` interchange model.
fn entity_model() -> serde_json::Value {
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": {
            "Entity": {
                "type": "object",
                "properties": { "id": { "type": "integer" } },
                "required": ["id"],
                "additionalProperties": true
            }
        }
    })
}

fn bind_serde_entity(doc: &mut EditorDocument) {
    doc.bound_type = Some(BoundType {
        model: Arc::new(entity_model()),
        type_name: "Entity".to_string(),
    });
}

#[test]
fn scn_ronin_validates_via_scene_path_and_ron_via_serde_in_same_session() {
    // T021/T022 — a .scn.ron validates via the scene path and a .ron via the serde
    // path in the *same* session (FR-013).
    let worker = ReparseWorker::new();
    let root = registry_root("same_session");
    let config = scene_config();

    // (1) A Bevy .scn.ron document: an unregistered component path → a scene-level
    // hint (BVY-S0002, ronin-bevy), NOT a RON-V type finding.
    let mut scene = EditorDocument::from_loaded(root.join("world.scn.ron"), b"").unwrap();
    scene.set_mode_state(loaded_bevy_mode(
        &config,
        &root.join("world.scn.ron"),
        &root,
    ));
    scene.buffer = r#"(entities: {0: (components: {"game::Unknown": (a: 1)})})"#.to_string();
    scene.on_edit();
    drive_reparse(&mut scene, &worker);
    assert!(
        scene.diagnostics.iter().any(|v| v.source() == "ronin-bevy"),
        "the Bevy scene document must produce a scene finding, got {:?}",
        scene.diagnostics
    );
    // No genuine serde finding: a serde type finding has scene_code == None AND a
    // ronin-types code source. (A scene-level hint carries a non-error RON-V0006
    // placeholder whose raw source is also ronin-types, so it is distinguished by its
    // Some(scene_code); the rendered `source()` is "ronin-bevy".)
    assert!(
        scene
            .diagnostics
            .iter()
            .all(|v| !(v.scene_code.is_none() && v.code.source() == "ronin-types")),
        "the Bevy document (unregistered type) must NOT run the serde validator, got {:?}",
        scene.diagnostics
    );

    // (2) A serde .ron document in the SAME session: a wrong-type value → a RON-V
    // type finding (ronin-types), proving the serde path still runs unchanged.
    let mut serde_doc = EditorDocument::from_loaded(root.join("config.ron"), b"").unwrap();
    bind_serde_entity(&mut serde_doc);
    // Default ModeState is Serde (no .scn.ron extension) — left as-is.
    assert!(!serde_doc.is_bevy_mode(), "a .ron stays serde");
    serde_doc.buffer = r#"(id: "oops")"#.to_string();
    serde_doc.on_edit();
    drive_reparse(&mut serde_doc, &worker);
    assert!(
        serde_doc
            .diagnostics
            .iter()
            .any(|v| v.code.source() == "ronin-types"),
        "the serde document must produce a RON-V type finding, got {:?}",
        serde_doc.diagnostics
    );
    assert!(
        serde_doc
            .diagnostics
            .iter()
            .all(|v| v.source() != "ronin-bevy"),
        "the serde document must NOT run the scene validator"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn registered_scene_mismatch_renders_as_a_ron_v_finding() {
    // A registered type with a wrong-typed field surfaces a registered-mismatch,
    // which renders byte-for-byte as a RON-V type finding (FR-005/FR-007) — so the
    // scene path reuses the exact E006 surface for real mismatches.
    let worker = ReparseWorker::new();
    let root = registry_root("registered_mismatch");
    let config = scene_config();

    let path = root.join("level.scn.ron");
    let mut scene = EditorDocument::from_loaded(&path, b"").unwrap();
    scene.set_mode_state(loaded_bevy_mode(&config, &path, &root));
    // `x` is a string where a number is required for the registered game::Vec3.
    scene.buffer =
        r#"(entities: {0: (components: {"game::Vec3": (x: "no", y: 0.0, z: 0.0)})})"#.to_string();
    scene.on_edit();
    drive_reparse(&mut scene, &worker);

    assert!(
        scene
            .diagnostics
            .iter()
            .any(|v| v.code_str().starts_with("RON-V") && v.severity == ronin_core::Severity::Error),
        "a registered mismatch must render as a RON-V error, got {:?}",
        scene.diagnostics
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn switching_mode_reroutes_validation_with_zero_byte_change() {
    // Switching a document's mode re-routes which validator runs and changes ZERO
    // bytes (FR-011/FR-013). A .scn.ron forced to Serde runs the serde validator;
    // toggled back to Bevy it runs the scene validator — same buffer throughout.
    let worker = ReparseWorker::new();
    let root = registry_root("switch_mode");
    let config = scene_config();
    let path = root.join("toggle.scn.ron");

    let mut doc = EditorDocument::from_loaded(&path, b"").unwrap();
    doc.set_mode_state(loaded_bevy_mode(&config, &path, &root));
    bind_serde_entity(&mut doc); // a serde binding is present but ignored in Bevy mode
    doc.buffer = r#"(entities: {0: (components: {"game::Unknown": (a: 1)})})"#.to_string();
    doc.on_edit();
    let bytes_before = doc.buffer.clone();
    drive_reparse(&mut doc, &worker);

    // Bevy mode: a scene finding, no serde finding.
    assert!(
        doc.diagnostics.iter().any(|v| v.source() == "ronin-bevy"),
        "Bevy mode runs the scene validator"
    );

    // Toggle to Serde — zero bytes change; re-validate.
    doc.mode_state_mut().set_mode_override(Mode::Serde);
    assert_eq!(
        doc.buffer, bytes_before,
        "switching mode changes zero bytes"
    );
    assert!(!doc.is_bevy_mode());
    doc.revalidate(&worker);
    drive_reparse(&mut doc, &worker);
    assert!(
        doc.diagnostics.iter().all(|v| v.source() != "ronin-bevy"),
        "after toggling to Serde the scene validator no longer runs, got {:?}",
        doc.diagnostics
    );
    assert_eq!(doc.buffer, bytes_before, "still zero bytes changed");

    // Toggle back to Bevy — the scene validator runs again, still zero bytes.
    doc.mode_state_mut().set_mode_override(Mode::Bevy);
    doc.revalidate(&worker);
    drive_reparse(&mut doc, &worker);
    assert!(
        doc.diagnostics.iter().any(|v| v.source() == "ronin-bevy"),
        "toggled back to Bevy, the scene validator runs again"
    );
    assert_eq!(doc.buffer, bytes_before, "still zero bytes changed");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn two_documents_in_different_modes_coexist_without_cross_talk() {
    // FR-012 — two open documents, one Bevy and one serde, are each validated per
    // their own mode in the same session with no cross-contamination. The mode /
    // registry / diagnostic state is per-document (no global state).
    let worker = ReparseWorker::new();
    let root = registry_root("coexist");
    let config = scene_config();

    let scene_path = root.join("a.scn.ron");
    let mut bevy_doc = EditorDocument::from_loaded(&scene_path, b"").unwrap();
    bevy_doc.set_mode_state(loaded_bevy_mode(&config, &scene_path, &root));
    bevy_doc.buffer = r#"(entities: {0: (components: {"game::Unknown": (a: 1)})})"#.to_string();
    bevy_doc.on_edit();

    let mut serde_doc = EditorDocument::from_loaded(root.join("b.ron"), b"").unwrap();
    bind_serde_entity(&mut serde_doc);
    serde_doc.buffer = r#"(id: "oops")"#.to_string();
    serde_doc.on_edit();

    // Drive both to completion (interleaved is fine — each holds its own state).
    drive_reparse(&mut bevy_doc, &worker);
    drive_reparse(&mut serde_doc, &worker);

    // The Bevy doc has scene findings and NO serde findings.
    assert!(bevy_doc.is_bevy_mode());
    assert!(
        bevy_doc
            .diagnostics
            .iter()
            .any(|v| v.source() == "ronin-bevy"),
        "the Bevy doc validates via the scene path"
    );
    assert!(
        bevy_doc
            .diagnostics
            .iter()
            .all(|v| !(v.scene_code.is_none() && v.code.source() == "ronin-types")),
        "the Bevy doc shows no serde findings (no cross-talk), got {:?}",
        bevy_doc.diagnostics
    );

    // The serde doc has RON-V findings and NO scene findings.
    assert!(!serde_doc.is_bevy_mode());
    assert!(
        serde_doc
            .diagnostics
            .iter()
            .any(|v| v.code.source() == "ronin-types"),
        "the serde doc validates via the serde path"
    );
    assert!(
        serde_doc
            .diagnostics
            .iter()
            .all(|v| v.source() != "ronin-bevy"),
        "the serde doc shows no scene findings (no cross-talk)"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn two_bevy_docs_bound_to_different_registries_do_not_cross_contaminate() {
    // FR-012 — two Bevy docs bound to DIFFERENT registries each validate against
    // their own. Registry `a` knows `game::Vec3`; registry `b` knows `other::Thing`.
    // A scene using `game::Vec3` is registered under `a` (no type-not-in-registry
    // hint) but unregistered under `b` (a hint), proving the bindings don't bleed.
    let worker = ReparseWorker::new();
    let root_a = temp_dir("two_reg_a");
    std::fs::write(root_a.join("a.json"), REGISTRY).unwrap();
    let registry_b = r##"{
        "bevyVersion": "0.16.0",
        "$defs": { "other::Thing": { "kind": "Struct", "properties": {} } }
    }"##;
    let root_b = temp_dir("two_reg_b");
    std::fs::write(root_b.join("b.json"), registry_b).unwrap();

    let config_a = RegistryBindingConfig {
        rules: vec![RegistryBindingRule {
            pattern: "**/*.scn.ron".to_string(),
            exclude: None,
            registry_export_path: PathBuf::from("a.json"),
            mode: None,
            expected_bevy_version: None,
        }],
        ..Default::default()
    };
    let config_b = RegistryBindingConfig {
        rules: vec![RegistryBindingRule {
            pattern: "**/*.scn.ron".to_string(),
            exclude: None,
            registry_export_path: PathBuf::from("b.json"),
            mode: None,
            expected_bevy_version: None,
        }],
        ..Default::default()
    };

    let scene_src = r#"(entities: {0: (components: {"game::Vec3": (x: 1.0, y: 2.0, z: 3.0)})})"#;

    let path_a = root_a.join("a.scn.ron");
    let mut doc_a = EditorDocument::from_loaded(&path_a, b"").unwrap();
    doc_a.set_mode_state(loaded_bevy_mode(&config_a, &path_a, &root_a));
    doc_a.buffer = scene_src.to_string();
    doc_a.on_edit();

    let path_b = root_b.join("b.scn.ron");
    let mut doc_b = EditorDocument::from_loaded(&path_b, b"").unwrap();
    doc_b.set_mode_state(loaded_bevy_mode(&config_b, &path_b, &root_b));
    doc_b.buffer = scene_src.to_string();
    doc_b.on_edit();

    drive_reparse(&mut doc_a, &worker);
    drive_reparse(&mut doc_b, &worker);

    // Under registry `a`, game::Vec3 is registered and valid → no scene hint.
    assert!(
        doc_a.diagnostics.iter().all(|v| v.scene_code.is_none()),
        "doc_a's registry knows game::Vec3 → no scene hint, got {:?}",
        doc_a.diagnostics
    );
    // Under registry `b`, game::Vec3 is NOT registered → a type-not-in-registry hint.
    assert!(
        doc_b
            .diagnostics
            .iter()
            .any(|v| v.code_str() == "BVY-S0002"),
        "doc_b's registry does NOT know game::Vec3 → a type-not-in-registry hint, got {:?}",
        doc_b.diagnostics
    );

    let _ = std::fs::remove_dir_all(&root_a);
    let _ = std::fs::remove_dir_all(&root_b);
}

#[test]
fn serde_only_document_diagnostics_unchanged_regression() {
    // Regression gate: a plain serde document (no Bevy involvement at all) produces
    // exactly the structural + type diagnostics it always did. The Bevy wiring is
    // inert for a default (Serde) ModeState — `bound_validation` falls straight
    // through to the serde branch, byte-for-byte the prior behavior.
    let worker = ReparseWorker::new();
    let mut doc = EditorDocument::new_untitled(1);
    bind_serde_entity(&mut doc);
    // A default document is in Serde mode with no registry.
    assert!(!doc.is_bevy_mode());

    doc.buffer = r#"(id: "oops")"#.to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);

    // Exactly the serde findings, no scene_code on any view.
    assert!(
        doc.diagnostics
            .iter()
            .any(|v| v.code.source() == "ronin-types"),
        "serde validation still produces a RON-V finding"
    );
    assert!(
        doc.diagnostics.iter().all(|v| v.scene_code.is_none()),
        "no scene_code is ever attached to a serde document's diagnostics"
    );

    // A valid value clears the type finding (live refresh, replace-not-merge).
    doc.buffer = r#"(id: 7)"#.to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);
    assert!(
        doc.diagnostics
            .iter()
            .all(|v| v.code.source() != "ronin-types"),
        "fixing the value clears the serde type finding, got {:?}",
        doc.diagnostics
    );
}
