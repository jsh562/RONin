//! E009 US2 (T024) — per-document mode + active-mode/registry indicator + explicit
//! toggle, exercised end-to-end through the [`App`] shell (egui_kittest)
//! [COMPLETES FR-009].
//!
//! These integration tests drive the *real* US2 mode pipeline through the running
//! shell:
//!
//! * Opening a `.scn.ron` document **auto-selects Bevy mode** (extension only,
//!   FR-009) and the always-visible indicator shows the active mode + the bound
//!   registry (FR-009/FR-011, SC-003) — asserted by querying the rendered AccessKit
//!   labels through the renderer-free `egui_kittest` harness (no pixel-scraping).
//! * The explicit per-document **toggle** flips serde ⇄ Bevy and changes **zero file
//!   bytes** (the document's buffer is byte-identical before and after — FR-011,
//!   SC-003).
//! * A **Bevy document and a serde document open side-by-side** are each validated /
//!   shown per their own mode in the same session (FR-012, SC-004): the Bevy doc runs
//!   the scene validator (`ronin-bevy` findings) and the serde doc runs the serde
//!   validator (`ron-types` findings), with no cross-talk.
//! * A **per-pattern registry binding** resolves for a matching scene path (the
//!   registry loads and scene-aware validation engages), and a **corrupt / absent**
//!   registry config degrades to defaults — no crash, no registry, only the visible
//!   "no registry loaded" hint (FR-010, SC-002).
//!
//! The UI-wiring assertions go through the shell's renderer-free `render_shell` path
//! (the same honest harness boundary as `binding_ui.rs`); the validation-behavior
//! assertions go through the App's real off-frame [`ReparseWorker`] driven to
//! completion via `App::poll_documents`, and the document's resolved mode state +
//! diagnostics.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;

use ronin_app::app::App;
use ronin_app::bevy::mode::Mode;
use ronin_app::settings::AppSettings;

/// A tiny valid registry export: a `game::Vec3` struct (x/y/z numbers) plus an
/// apparent Bevy version.
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

/// Create a unique temp project directory for a test (fresh each run).
fn temp_project(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ronin_bevy_mode_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp project dir");
    dir
}

/// Write `REGISTRY` into `project/registry.json` and return its path.
fn write_registry(project: &Path) -> PathBuf {
    let path = project.join("registry.json");
    std::fs::write(&path, REGISTRY).expect("write registry export");
    path
}

/// Write a `.ronin/bevy-registries.json` into `project` mapping a `.scn.ron` glob →
/// `registry.json`.
fn write_registry_config(project: &Path) {
    let ronin = project.join(".ronin");
    std::fs::create_dir_all(&ronin).expect("create .ronin dir");
    let json = r#"{
        "rules": [
            {
                "pattern": "**/*.scn.ron",
                "registry_export_path": "registry.json"
            }
        ],
        "version": 1
    }"#;
    std::fs::write(ronin.join("bevy-registries.json"), json.as_bytes())
        .expect("write bevy-registries.json");
}

/// Drive the App's real off-frame worker to completion for every open document:
/// spin-poll until at least one result installs, or panic on timeout.
fn drive_app_reparse(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if app.poll_documents() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("App reparse did not land within timeout");
        }
        std::thread::yield_now();
    }
}

/// Spin-poll the App's worker until `cond` holds for the active document, or panic
/// on timeout. Robust across re-validation (where the active document already has a
/// stale `parse` but its diagnostics have not yet refreshed for the new mode).
fn drive_until(app: &mut App, what: &str, mut cond: impl FnMut(&App) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        app.poll_documents();
        if cond(app) {
            return;
        }
        if Instant::now() >= deadline {
            panic!("condition '{what}' not reached within timeout");
        }
        std::thread::yield_now();
    }
}

/// Count diagnostics on the active document whose rendered source is `ronin-bevy`
/// (scene findings).
fn scene_diag_count(app: &App) -> usize {
    app.active_document()
        .map(|d| {
            d.diagnostics
                .iter()
                .filter(|v| v.source() == "ronin-bevy")
                .count()
        })
        .unwrap_or(0)
}

/// Count genuine serde (`ron-types`) type findings on the active document — a serde
/// finding has no `scene_code` AND a `ron-types` code source (a scene hint may carry
/// a `ron-types` raw source but is distinguished by `Some(scene_code)`).
fn serde_diag_count(app: &App) -> usize {
    app.active_document()
        .map(|d| {
            d.diagnostics
                .iter()
                .filter(|v| v.scene_code.is_none() && v.code.source() == "ron-types")
                .count()
        })
        .unwrap_or(0)
}

#[test]
fn opening_scn_ron_auto_selects_bevy_and_indicator_shows_mode_and_registry() {
    // FR-009/FR-011, SC-003: opening a `.scn.ron` auto-selects Bevy mode and the
    // always-visible indicator shows the active mode + the bound registry.
    let project = temp_project("auto_bevy");
    write_registry(&project);
    write_registry_config(&project);
    let scene = project.join("world.scn.ron");
    // A registered component with valid fields — so the registry actually engages.
    std::fs::write(
        &scene,
        br#"(entities: {0: (components: {"game::Vec3": (x: 1.0, y: 2.0, z: 3.0)})})"#,
    )
    .expect("write scene");

    let mut app = App::new(AppSettings::default(), Some(scene.clone()));
    // The registry config (one rule) must have loaded from the opened doc's root.
    assert_eq!(
        app.registry_binding_config().rules.len(),
        1,
        "the project bevy-registries.json (one rule) must load on open"
    );

    // Extension-only auto-detect selected Bevy mode (FR-009).
    assert_eq!(
        app.active_mode(),
        Some(Mode::Bevy),
        "a .scn.ron must auto-select Bevy mode"
    );
    // The bound registry resolved AND loaded (so scene validation engages).
    {
        let doc = app.active_document().expect("active doc");
        assert!(
            doc.mode_state().has_registry(),
            "the per-pattern registry must resolve and load for a matching scene"
        );
    }

    // The indicator labels reflect Bevy mode + the loaded registry.
    let mode_label = app.mode_indicator_label();
    let registry_label = app.registry_indicator_label();
    assert_eq!(mode_label, "Mode: bevy (auto)");
    assert_eq!(
        registry_label, "Registry: registry.json (loaded)",
        "the indicator must name the bound, loaded registry"
    );

    // UI wiring: the shell renders the active-mode + registry indicator (queried by
    // AccessKit label, not pixels).
    let mut harness = Harness::new_ui(move |ui| {
        app.render_shell(ui);
    });
    harness.run();
    assert!(
        harness
            .query_all_by_label_contains(&mode_label)
            .next()
            .is_some(),
        "the shell must render the active-mode indicator '{mode_label}'"
    );
    assert!(
        harness
            .query_all_by_label_contains("Registry: registry.json")
            .next()
            .is_some(),
        "the shell must render the bound-registry indicator"
    );

    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn explicit_toggle_switches_modes_and_changes_zero_bytes() {
    // FR-011, SC-003: the explicit per-document toggle switches serde ⇄ Bevy and
    // changes ZERO file bytes (the buffer is byte-identical before and after).
    let project = temp_project("toggle_zero_bytes");
    write_registry(&project);
    write_registry_config(&project);
    let scene = project.join("toggle.scn.ron");
    let original = br#"(entities: {0: (components: {"game::Vec3": (x: 1.0, y: 2.0, z: 3.0)})})"#;
    std::fs::write(&scene, original).expect("write scene");

    let mut app = App::new(AppSettings::default(), Some(scene.clone()));
    drive_app_reparse(&mut app);

    // Auto-selected Bevy on open.
    assert_eq!(app.active_mode(), Some(Mode::Bevy));
    let bytes_before = app.active_document().expect("active doc").buffer.clone();

    // Toggle to serde — zero bytes change.
    app.toggle_active_mode();
    assert_eq!(
        app.active_mode(),
        Some(Mode::Serde),
        "toggle flips to serde"
    );
    assert_eq!(
        app.active_document().expect("active doc").buffer,
        bytes_before,
        "toggling mode must change zero file bytes (SC-003)"
    );
    // The indicator now shows serde mode + a serde-mode registry note.
    assert_eq!(app.mode_indicator_label(), "Mode: serde (override)");
    assert_eq!(app.registry_indicator_label(), "Registry: n/a (serde mode)");

    // Toggle back to Bevy — still zero bytes change, registry re-engages.
    app.toggle_active_mode();
    assert_eq!(
        app.active_mode(),
        Some(Mode::Bevy),
        "toggle flips back to Bevy"
    );
    assert_eq!(
        app.active_document().expect("active doc").buffer,
        bytes_before,
        "toggling back must still change zero file bytes (SC-003)"
    );
    assert!(
        app.active_document()
            .expect("active doc")
            .mode_state()
            .has_registry(),
        "toggling back to Bevy re-resolves + reloads the bound registry"
    );

    // The on-disk file was never rewritten by the toggles.
    assert_eq!(
        std::fs::read(&scene).expect("read scene"),
        original,
        "the on-disk scene file must be byte-identical (no save happened)"
    );

    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn bevy_and_serde_documents_side_by_side_each_validated_per_mode() {
    // FR-012, SC-004: a Bevy document and a serde document open side-by-side are each
    // validated and treated per their own mode in the same session — proven at the
    // App level by (a) the active Bevy scene running the scene validator, and (b) the
    // two open documents holding independent per-document mode/registry state (no
    // global state, no cross-talk). Two-documents-simultaneously-validated through
    // separate workers is covered deterministically in `bevy_validation_path.rs`; at
    // the App level the shared off-frame worker validates the active document, while
    // each tab's mode/registry state coexists synchronously (the FR-012 guarantee).
    let project = temp_project("side_by_side");
    write_registry(&project);
    write_registry_config(&project);

    // (1) The Bevy scene — an UNregistered component path → a scene-level hint
    // (ronin-bevy), proving the scene validator runs.
    let scene = project.join("level.scn.ron");
    std::fs::write(
        &scene,
        br#"(entities: {0: (components: {"game::Unknown": (a: 1)})})"#,
    )
    .expect("write scene");
    // (2) A plain serde .ron document in the same project.
    let serde_doc = project.join("config.ron");
    std::fs::write(&serde_doc, br#"(id: "value")"#).expect("write serde doc");

    // Open the scene first and drive its (active, only-doc) validation to completion:
    // it runs the scene validator and produces a ronin-bevy finding.
    let mut app = App::new(AppSettings::default(), Some(scene.clone()));
    drive_until(&mut app, "bevy scene findings", |a| {
        scene_diag_count(a) >= 1
    });
    assert_eq!(
        app.active_mode(),
        Some(Mode::Bevy),
        "the .scn.ron document is in Bevy mode"
    );
    assert_eq!(
        serde_diag_count(&app),
        0,
        "the Bevy document must NOT run the serde validator (no cross-talk)"
    );

    // Open the serde document as a second tab — both are now open simultaneously.
    app.open_file(&serde_doc);
    assert_eq!(app.document_count(), 2, "two documents open simultaneously");

    // FR-012/SC-004 — per-document mode/registry coexistence (synchronous, no global
    // state): the scene tab stays Bevy with its loaded registry; the serde tab is
    // serde with no registry. Setting one never touched the other.
    let scene_doc = app.document_at(0).expect("scene tab");
    let cfg_doc = app.document_at(1).expect("serde tab");
    assert_eq!(
        scene_doc.mode_state().active_mode(),
        Mode::Bevy,
        "the scene tab is treated as Bevy"
    );
    assert!(
        scene_doc.mode_state().has_registry(),
        "the scene tab keeps its own loaded registry (no cross-talk)"
    );
    assert_eq!(
        cfg_doc.mode_state().active_mode(),
        Mode::Serde,
        "the serde tab is treated as serde"
    );
    assert!(
        !cfg_doc.mode_state().has_registry(),
        "the serde tab binds no registry"
    );

    // The active tab (serde) reports serde mode + the serde-mode indicator.
    assert_eq!(app.active_mode(), Some(Mode::Serde));
    assert_eq!(app.mode_indicator_label(), "Mode: serde (auto)");
    assert_eq!(app.registry_indicator_label(), "Registry: n/a (serde mode)");

    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn per_pattern_registry_resolves_for_matching_scene() {
    // FR-010, SC-002: a per-pattern registry binding resolves + loads for a matching
    // scene path so scene-aware validation engages.
    let project = temp_project("per_pattern");
    write_registry(&project);
    write_registry_config(&project);
    let scene = project.join("boss.scn.ron");
    std::fs::write(
        &scene,
        br#"(entities: {0: (components: {"game::Vec3": (x: "no", y: 0.0, z: 0.0)})})"#,
    )
    .expect("write scene");

    let mut app = App::new(AppSettings::default(), Some(scene.clone()));
    drive_app_reparse(&mut app);

    assert_eq!(app.active_mode(), Some(Mode::Bevy));
    // The per-pattern `**/*.scn.ron` rule matched the scene's absolute path and the
    // bound registry resolved + loaded.
    assert!(
        app.active_document()
            .expect("active doc")
            .mode_state()
            .has_registry(),
        "the per-pattern rule must resolve + load the registry for a matching scene"
    );
    // A registered type with a wrong-typed field renders as a real RON-V error
    // through the shared E006 surface — proving the registry actually validates.
    assert!(
        app.active_document()
            .expect("active doc")
            .diagnostics
            .iter()
            .any(|v| {
                v.code_str().starts_with("RON-V") && v.severity == ron_core::Severity::Error
            }),
        "a registered mismatch must surface a RON-V error, got {:?}",
        app.active_document().map(|d| d.diagnostics.clone())
    );

    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn corrupt_registry_config_degrades_to_defaults_no_crash() {
    // FR-010, SC-002: a corrupt .ronin/bevy-registries.json loads as an empty config
    // → no rules → no registry bound; opening a scene still auto-selects Bevy (by
    // extension) but shows the "no registry loaded" hint, with no crash and no errors.
    let project = temp_project("corrupt_config");
    let ronin = project.join(".ronin");
    std::fs::create_dir_all(&ronin).expect("create .ronin");
    std::fs::write(
        ronin.join("bevy-registries.json"),
        b"\x00\x01 not json at all }{][",
    )
    .expect("write corrupt config");
    let scene = project.join("world.scn.ron");
    std::fs::write(
        &scene,
        br#"(entities: {0: (components: {"game::Vec3": (x: 1.0, y: 2.0, z: 3.0)})})"#,
    )
    .expect("write scene");

    // Construction + open must not panic despite the corrupt config.
    let mut app = App::new(AppSettings::default(), Some(scene.clone()));
    assert!(
        app.registry_binding_config().rules.is_empty(),
        "a corrupt config must load as an empty config (zero rules)"
    );
    drive_app_reparse(&mut app);

    // Extension auto-detect still selects Bevy, but no registry is bound/loaded.
    assert_eq!(app.active_mode(), Some(Mode::Bevy));
    assert!(
        !app.active_document()
            .expect("active doc")
            .mode_state()
            .has_registry(),
        "an empty (corrupt-degraded) config binds no registry"
    );
    assert_eq!(
        app.registry_indicator_label(),
        "Registry: none",
        "the indicator must show 'none' when no registry is bound"
    );
    // No scene errors are produced (no-registry → hint only, structural intact).
    assert_eq!(
        scene_diag_count(&app),
        0,
        "a no-registry Bevy scene produces no scene findings (structural-only)"
    );

    // UI wiring: the shell renders the 'Registry: none' indicator without panic.
    let mut harness = Harness::new_ui(move |ui| {
        app.render_shell(ui);
    });
    harness.run();
    assert!(
        harness
            .query_all_by_label_contains("Registry: none")
            .next()
            .is_some(),
        "the shell must render the 'Registry: none' indicator for a no-registry scene"
    );

    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn registries_window_renders_toggle_and_rules() {
    // FR-009/FR-010/FR-011: the Bevy Registries window renders the per-document mode
    // toggle, the active-mode/registry indicator, and the loaded registry rule.
    let project = temp_project("registries_window");
    write_registry(&project);
    write_registry_config(&project);
    let scene = project.join("world.scn.ron");
    std::fs::write(
        &scene,
        br#"(entities: {0: (components: {"game::Vec3": (x: 1.0, y: 2.0, z: 3.0)})})"#,
    )
    .expect("write scene");

    let mut harness = Harness::new_ui(move |ui| {
        let mut app = App::new(AppSettings::default(), Some(scene.clone()));
        app.set_registries_open(true);
        app.render_shell(ui);
    });
    harness.run();

    // The toggle button renders (the active doc is Bevy → caption is "Switch to serde").
    assert!(
        harness
            .query_all_by_label_contains("Switch to serde")
            .next()
            .is_some(),
        "the registries window must render the mode toggle control"
    );
    // The loaded registry rule's pattern is listed.
    assert!(
        harness
            .query_all_by_label_contains("**/*.scn.ron")
            .next()
            .is_some(),
        "the rule editor must list the loaded registry rule pattern"
    );

    let _ = std::fs::remove_dir_all(&project);
}
