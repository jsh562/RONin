//! US2 binding-status + override smoke test (T027, FR-011/FR-013) —
//! [COMPLETES FR-013].
//!
//! Exercises the real US2 binding pipeline end-to-end through the [`App`] shell:
//!
//! * A project `.ronin/bindings.json` mapping a glob (`*.ron`) to a type whose
//!   `type_source` is a small JSON-Schema `SchemaFile` is loaded; opening a matching
//!   document resolves to a `Bound` binding (Entity, config) AND, driving the App's
//!   real off-frame [`ReparseWorker`] to completion, produces a `RON-V####` type
//!   diagnostic (source `ron-types`) at the wrong value (FR-011/FR-013).
//! * A document whose path matches no rule (and has no override) shows
//!   `no type bound` and produces zero `ron-types` diagnostics (structural-only).
//! * A per-document override flips the binding to `(override)` and validation uses
//!   the override's type (origin Override > config).
//! * A corrupt `.ronin/bindings.json` loads as an empty config → `NoBinding`, no
//!   panic, structural-only.
//!
//! UI-wiring proof: the binding indicator and the override control are rendered
//! through the renderer-free `egui_kittest` harness and queried by label, so the
//! shown state is asserted without scraping pixels. The validation-behavior
//! assertions go through the App's real worker + the document's resolved binding /
//! diagnostics state (the same honest doc-state boundary documented in
//! `type_diagnostics.rs` / `app_shell.rs`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;

use ronin_app::app::App;
use ronin_app::binding::{BindingOrigin, TypeSourceLocator};
use ronin_app::settings::AppSettings;

/// Create a unique temp project directory for a test (fresh each run).
fn temp_project(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ronin_binding_ui_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp project dir");
    dir
}

/// Write a JSON-Schema 2020-12 file defining `Entity { id: integer }` (id required)
/// into `dir` and return its path.
fn write_entity_schema(dir: &Path, file: &str) -> PathBuf {
    let path = dir.join(file);
    let schema = br#"{
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": {
            "Entity": {
                "type": "object",
                "properties": { "id": { "type": "integer" } },
                "required": ["id"],
                "additionalProperties": true
            }
        }
    }"#;
    std::fs::write(&path, schema).expect("write entity schema");
    path
}

/// Write a JSON-Schema 2020-12 file defining `Widget { id: string }` (id required)
/// into `dir` and return its path. Used to prove an override binds a *different*
/// type than the config rule.
fn write_widget_schema(dir: &Path, file: &str) -> PathBuf {
    let path = dir.join(file);
    let schema = br#"{
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": {
            "Widget": {
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
                "additionalProperties": true
            }
        }
    }"#;
    std::fs::write(&path, schema).expect("write widget schema");
    path
}

/// Write a `.ronin/bindings.json` into `project` mapping a `.ron` glob → `Entity`
/// from `schema_path`.
///
/// The pattern is `**/*.ron`: documents are matched against their *absolute* path
/// (what the shell stores), and the resolver matches with `literal_separator(true)`
/// so a single-segment `*.ron` would not cross directories — `**/` makes the glob
/// match the file regardless of its directory depth.
fn write_bindings(project: &Path, schema_path: &Path) {
    let ronin = project.join(".ronin");
    std::fs::create_dir_all(&ronin).expect("create .ronin dir");
    // Path is embedded as JSON; escape backslashes (Windows paths) so it parses.
    let escaped = schema_path.display().to_string().replace('\\', "\\\\");
    let json = format!(
        r#"{{
            "rules": [
                {{
                    "pattern": "**/*.ron",
                    "type_name": "Entity",
                    "type_source": {{ "SchemaFile": "{escaped}" }}
                }}
            ],
            "version": 1
        }}"#
    );
    std::fs::write(ronin.join("bindings.json"), json.as_bytes()).expect("write bindings.json");
}

/// Write a matching `.ron` document with a wrong-type `id` value into `project` and
/// return its path. `id` is a string where the schema requires an integer, so a
/// `RON-V####` type-mismatch must surface once bound.
fn write_wrong_type_doc(project: &Path, file: &str) -> PathBuf {
    let path = project.join(file);
    std::fs::write(&path, b"(id: \"oops\")\n").expect("write wrong-type doc");
    path
}

/// Drive the App's real off-frame worker to completion for the active document:
/// re-apply the binding (which requests an off-frame reparse) and spin-poll until a
/// result installs, or panic on timeout.
fn drive_app_reparse(app: &mut App) {
    app.apply_binding_to_active();
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

/// Count `ron-types` diagnostics on the active document.
fn type_diag_count(app: &App) -> usize {
    app.active_document()
        .map(|d| {
            d.diagnostics
                .iter()
                .filter(|v| v.code.source() == "ron-types")
                .count()
        })
        .unwrap_or(0)
}

#[test]
fn matching_file_is_bound_shown_and_validated() {
    // FR-011/FR-013: a project mapping (glob → type + schema source) binds a
    // matching opened file; the binding shows Bound (Entity, config) and a RON-V
    // type diagnostic surfaces at the wrong value.
    let project = temp_project("match");
    let schema = write_entity_schema(&project, "entity.schema.json");
    write_bindings(&project, &schema);
    let doc_path = write_wrong_type_doc(&project, "thing.ron");

    let mut app = App::new(AppSettings::default(), Some(doc_path.clone()));
    // The config must have loaded from the opened doc's project root.
    assert_eq!(
        app.binding_config().rules.len(),
        1,
        "the project bindings.json (one rule) must load on open"
    );

    // Open already applied the binding; drive the real worker to land diagnostics.
    drive_app_reparse(&mut app);

    let doc = app.active_document().expect("active document present");
    assert!(
        doc.binding.is_bound(),
        "a matching file must resolve to a Bound binding, got {:?}",
        doc.binding
    );
    assert_eq!(
        doc.binding.origin(),
        Some(BindingOrigin::Config),
        "the binding origin must be Config (from the project rule)"
    );
    assert_eq!(
        doc.binding.type_name(),
        Some("Entity"),
        "the bound type must be Entity"
    );
    assert_eq!(
        doc.binding_label(),
        "Type: Entity (config)",
        "the indicator label must show the bound type + (config) origin"
    );

    // A RON-V type diagnostic (source ron-types) must be present at the wrong value.
    let type_views: Vec<_> = doc
        .diagnostics
        .iter()
        .filter(|v| v.code.source() == "ron-types")
        .cloned()
        .collect();
    assert!(
        !type_views.is_empty(),
        "a bound document with a wrong-type value must produce a type diagnostic, \
         got {:?}",
        doc.diagnostics
    );
    assert!(
        type_views
            .iter()
            .all(|v| v.code.code().starts_with("RON-V")),
        "every type diagnostic must carry a RON-V#### code, got {type_views:?}"
    );

    // UI wiring: render the shell and confirm the active-binding indicator shows the
    // bound type label (queried by AccessKit label, not pixels).
    let label = doc.binding_label();
    let mut harness = Harness::new_ui(move |ui| {
        app.render_shell(ui);
    });
    harness.run();
    assert!(
        harness.query_all_by_label_contains(&label).next().is_some(),
        "the active-binding indicator must render the bound type label '{label}'"
    );

    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn non_matching_file_shows_no_type_bound_and_structural_only() {
    // FR-011/FR-015: a document whose path matches no rule (and no override) shows
    // "no type bound" and produces zero ron-types diagnostics.
    let project = temp_project("nomatch");
    let schema = write_entity_schema(&project, "entity.schema.json");
    write_bindings(&project, &schema);
    // The rule matches `*.ron`; this file is `.txt` so it never matches.
    let doc_path = project.join("notes.txt");
    std::fs::write(&doc_path, b"(id: \"oops\")\n").expect("write non-matching doc");

    let mut app = App::new(AppSettings::default(), Some(doc_path.clone()));
    drive_app_reparse(&mut app);

    let doc = app.active_document().expect("active document present");
    assert!(
        !doc.binding.is_bound(),
        "a non-matching file must resolve to NoBinding, got {:?}",
        doc.binding
    );
    assert_eq!(
        doc.binding_label(),
        "no type bound",
        "the indicator must explicitly show 'no type bound'"
    );
    assert_eq!(
        type_diag_count(&app),
        0,
        "a non-matching file must produce zero ron-types diagnostics (structural-only)"
    );

    // UI wiring: the "no type bound" indicator must render in the shell.
    let mut harness = Harness::new_ui(move |ui| {
        app.render_shell(ui);
    });
    harness.run();
    assert!(
        harness
            .query_all_by_label_contains("no type bound")
            .next()
            .is_some(),
        "the shell must render the 'no type bound' indicator for an unbound doc"
    );

    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn override_takes_effect_over_config() {
    // FR-009: a per-document override binds the active document to a DIFFERENT type
    // (Widget) than the config rule (Entity); the binding flips to (override) and
    // validation uses the override's type (origin Override > config).
    let project = temp_project("override");
    let entity = write_entity_schema(&project, "entity.schema.json");
    let widget = write_widget_schema(&project, "widget.schema.json");
    write_bindings(&project, &entity);
    // `(id: 7)` — an integer. Entity (id: integer) accepts it; Widget (id: string)
    // rejects it. So a type diagnostic appears ONLY when the Widget override wins.
    let doc_path = project.join("thing.ron");
    std::fs::write(&doc_path, b"(id: 7)\n").expect("write doc");

    let mut app = App::new(AppSettings::default(), Some(doc_path.clone()));
    drive_app_reparse(&mut app);

    // Config-bound to Entity: integer `id` is valid → zero type diagnostics.
    {
        let doc = app.active_document().expect("active doc");
        assert_eq!(doc.binding.origin(), Some(BindingOrigin::Config));
        assert_eq!(doc.binding.type_name(), Some("Entity"));
    }
    assert_eq!(
        type_diag_count(&app),
        0,
        "Entity (id: integer) must accept the integer id (no type diagnostics)"
    );

    // Apply a per-document override to Widget (id: string); the integer now violates.
    app.set_active_override(
        "Widget".to_string(),
        TypeSourceLocator::SchemaFile(widget.clone()),
    );
    drive_app_reparse(&mut app);

    let doc = app.active_document().expect("active doc");
    assert!(doc.binding.is_bound(), "override must be a Bound binding");
    assert_eq!(
        doc.binding.origin(),
        Some(BindingOrigin::Override),
        "the override must take precedence over config (origin Override)"
    );
    assert_eq!(
        doc.binding.type_name(),
        Some("Widget"),
        "the override must bind the override's type, not the config's"
    );
    assert_eq!(
        doc.binding_label(),
        "Type: Widget (override)",
        "the indicator must show (override) once the override is set"
    );
    assert!(
        type_diag_count(&app) >= 1,
        "Widget (id: string) must reject the integer id (a type diagnostic appears)"
    );

    // Clearing the override falls back to the config (Entity) → diagnostics clear.
    app.clear_active_override();
    drive_app_reparse(&mut app);
    let doc = app.active_document().expect("active doc");
    assert_eq!(
        doc.binding.origin(),
        Some(BindingOrigin::Config),
        "clearing the override must fall back to the config rule"
    );
    assert_eq!(
        type_diag_count(&app),
        0,
        "back on Entity, the integer id is valid again (no type diagnostics)"
    );

    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn corrupt_config_degrades_to_no_binding_no_panic() {
    // FR-013: a corrupt .ronin/bindings.json loads as an empty config → NoBinding,
    // no panic, structural-only.
    let project = temp_project("corrupt");
    let ronin = project.join(".ronin");
    std::fs::create_dir_all(&ronin).expect("create .ronin");
    std::fs::write(
        ronin.join("bindings.json"),
        b"\x00\x01 not json at all }{][",
    )
    .expect("write corrupt bindings");
    let doc_path = project.join("thing.ron");
    std::fs::write(&doc_path, b"(id: \"oops\")\n").expect("write doc");

    // Construction + open must not panic despite the corrupt config.
    let mut app = App::new(AppSettings::default(), Some(doc_path.clone()));
    assert!(
        app.binding_config().rules.is_empty(),
        "a corrupt config must load as an empty config (zero rules)"
    );
    drive_app_reparse(&mut app);

    let doc = app.active_document().expect("active doc");
    assert!(
        !doc.binding.is_bound(),
        "a corrupt config must resolve every doc to NoBinding, got {:?}",
        doc.binding
    );
    assert_eq!(doc.binding_label(), "no type bound");
    assert_eq!(
        type_diag_count(&app),
        0,
        "with an empty (corrupt-degraded) config, only structural diagnostics run"
    );

    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn override_control_renders_in_bindings_window() {
    // FR-009/FR-011: the Type Bindings window renders the per-document override
    // control and the active-binding state, proving the override UI is wired (the
    // behavior of setting it is covered by `override_takes_effect_over_config`).
    let project = temp_project("override_ui");
    let schema = write_entity_schema(&project, "entity.schema.json");
    write_bindings(&project, &schema);
    let doc_path = write_wrong_type_doc(&project, "thing.ron");

    let mut harness = Harness::new_ui(move |ui| {
        let mut app = App::new(AppSettings::default(), Some(doc_path.clone()));
        app.set_bindings_open(true);
        app.render_shell(ui);
    });
    harness.run();

    // The override control's "Set Override" / "Clear Override" buttons render.
    assert!(
        harness
            .query_all_by_label_contains("Set Override")
            .next()
            .is_some(),
        "the Type Bindings window must render the override Set control"
    );
    // The project rule editor shows the loaded rule's pattern + type.
    assert!(
        harness
            .query_all_by_label_contains("Entity")
            .next()
            .is_some(),
        "the rule editor must list the loaded Entity rule"
    );

    let _ = std::fs::remove_dir_all(&project);
}
