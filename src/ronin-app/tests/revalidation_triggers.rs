//! US3 re-validation trigger coverage (T037, FR-014/FR-021) — [COMPLETES FR-021].
//!
//! Proves the system invalidates the prior type-diagnostic set AND recomputes it
//! IMMEDIATELY (not deferred to the next edit) on EACH of FR-021's exhaustive
//! triggers, driving the App's *real* off-frame [`ReparseWorker`] to completion:
//!
//! * (a) **document edit** — editing the buffer re-validates against the bound type
//!   (the bound model travels with each reparse request).
//! * (b) **type-info change** — the explicit re-acquire entry point
//!   ([`App::reacquire_active_binding`]) re-runs acquisition + re-validates; a source
//!   that changed on disk is picked up on demand.
//! * (c) **binding change** — making a doc active / opening it resolves + acquires +
//!   re-validates.
//! * (d) **`BindingConfig` change** — adding / editing / removing a rule re-resolves
//!   every open doc immediately.
//! * (e) **per-document override change** — set / clear re-resolves the active doc
//!   immediately.
//!
//! Plus a **stale-finding** check: after fixing the model/binding, the old type
//! finding is gone on the next landed result (the worker's full-set replace).
//!
//! All assertions go through the App's real worker + the document's resolved
//! binding / diagnostics state — the same honest doc-state boundary documented in
//! `binding_ui.rs` / `type_diagnostics.rs`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ronin_app::app::App;
use ronin_app::binding::{BindingOrigin, BindingRule, TypeSourceLocator};
use ronin_app::settings::AppSettings;

/// Create a unique temp project directory for a test (fresh each run).
fn temp_project(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ronin_reval_{tag}_{}_{}",
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

/// Write a JSON-Schema 2020-12 file defining `Widget { id: string }` (id required).
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
fn write_entity_bindings(project: &Path, schema_path: &Path) {
    let ronin = project.join(".ronin");
    std::fs::create_dir_all(&ronin).expect("create .ronin dir");
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

/// Drive the App's real off-frame worker to completion for the active document:
/// request a fresh reparse for all docs and spin-poll until a result installs.
fn drive_until_landed(app: &mut App) {
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

/// Count `ron-types` (type) diagnostics on the active document.
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

/// Replace the active document's buffer and drive its reparse to completion.
/// Models a user edit: bump the edit generation + request an off-frame reparse,
/// then spin-poll until the result lands.
fn edit_active_and_land(app: &mut App, new_buffer: &str) {
    app.replace_active_buffer_for_test(new_buffer);
    drive_until_landed(app);
}

#[test]
fn trigger_a_document_edit_revalidates_against_bound_type() {
    // (a) A bound doc that is initially valid produces zero type diagnostics; an
    // EDIT that introduces a wrong-type value must re-validate and surface one.
    let project = temp_project("edit");
    let schema = write_entity_schema(&project, "entity.schema.json");
    write_entity_bindings(&project, &schema);
    // Start VALID: id is an integer (Entity accepts it).
    let doc_path = project.join("thing.ron");
    std::fs::write(&doc_path, b"(id: 7)\n").expect("write valid doc");

    let mut app = App::new(AppSettings::default(), Some(doc_path));
    drive_until_landed(&mut app);
    assert_eq!(
        type_diag_count(&app),
        0,
        "a valid bound doc starts with zero type diagnostics"
    );

    // Edit → introduce a wrong-type id (string where integer required).
    edit_active_and_land(&mut app, "(id: \"oops\")\n");
    assert!(
        type_diag_count(&app) >= 1,
        "editing in a wrong-type value must re-validate and surface a type finding"
    );

    // Stale-finding check: editing BACK to a valid value clears the old finding on
    // the next landed result (the worker's full-set replace — no stale survivor).
    edit_active_and_land(&mut app, "(id: 7)\n");
    assert_eq!(
        type_diag_count(&app),
        0,
        "fixing the value must clear the prior type finding (no stale finding survives)"
    );

    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn trigger_b_type_info_change_reacquires_and_revalidates() {
    // (b) The explicit re-acquire entry point re-reads the type_source from disk and
    // re-validates: a source EDITED on disk (Entity.id integer → string) flips a
    // previously-valid integer id to a violation after re-acquire.
    let project = temp_project("typeinfo");
    let schema = write_entity_schema(&project, "entity.schema.json");
    write_entity_bindings(&project, &schema);
    // id is an integer; Entity (id: integer) accepts it → zero type diagnostics.
    let doc_path = project.join("thing.ron");
    std::fs::write(&doc_path, b"(id: 7)\n").expect("write doc");

    let mut app = App::new(AppSettings::default(), Some(doc_path));
    drive_until_landed(&mut app);
    assert_eq!(
        type_diag_count(&app),
        0,
        "Entity (id: integer) accepts the integer id initially"
    );

    // Externally edit the SOURCE schema so Entity.id now requires a STRING. The
    // integer id then violates — but only once we re-acquire (no auto-watch).
    let changed = br#"{
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": {
            "Entity": {
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
                "additionalProperties": true
            }
        }
    }"#;
    std::fs::write(&schema, changed).expect("rewrite entity schema");

    // Explicit re-acquire (the FR-021 (b) trigger): re-reads the changed source and
    // re-validates immediately.
    app.reacquire_active_binding();
    drive_until_landed(&mut app);
    assert!(
        type_diag_count(&app) >= 1,
        "re-acquiring the changed source must re-validate the integer id as a violation"
    );

    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn trigger_c_binding_change_on_open_revalidates() {
    // (c) Opening / making a doc active resolves + acquires + re-validates: a freshly
    // opened bound doc with a wrong-type value surfaces a type finding without any
    // further edit.
    let project = temp_project("binding");
    let schema = write_entity_schema(&project, "entity.schema.json");
    write_entity_bindings(&project, &schema);
    let doc_path = project.join("thing.ron");
    std::fs::write(&doc_path, b"(id: \"oops\")\n").expect("write wrong-type doc");

    let mut app = App::new(AppSettings::default(), Some(doc_path));
    // Open already applied the binding (the binding-change trigger); drive to land.
    drive_until_landed(&mut app);
    assert_eq!(
        app.active_document().and_then(|d| d.binding.type_name()),
        Some("Entity"),
        "opening the doc resolves the Entity binding"
    );
    assert!(
        type_diag_count(&app) >= 1,
        "a freshly opened bound doc with a wrong value validates on open (no edit needed)"
    );

    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn trigger_d_binding_config_change_revalidates_immediately() {
    // (d) Editing the BindingConfig (add a rule) must re-resolve + re-validate every
    // open doc immediately — without any document edit.
    let project = temp_project("config");
    let schema = write_entity_schema(&project, "entity.schema.json");
    // Start with NO bindings file → the doc is unbound (structural-only).
    let doc_path = project.join("thing.ron");
    std::fs::write(&doc_path, b"(id: \"oops\")\n").expect("write doc");

    let mut app = App::new(AppSettings::default(), Some(doc_path));
    drive_until_landed(&mut app);
    assert!(
        !app.active_document().unwrap().binding.is_bound(),
        "with no config the doc is unbound"
    );
    assert_eq!(
        type_diag_count(&app),
        0,
        "an unbound doc has zero type diagnostics"
    );

    // Config-change trigger: add a rule mapping the doc to Entity. This must
    // re-resolve + re-validate all open docs immediately (no edit).
    app.add_binding_rule(BindingRule {
        pattern: "**/*.ron".to_string(),
        exclude: None,
        type_name: "Entity".to_string(),
        type_source: TypeSourceLocator::SchemaFile(schema.clone()),
    });
    drive_until_landed(&mut app);
    assert_eq!(
        app.active_document().and_then(|d| d.binding.type_name()),
        Some("Entity"),
        "adding the rule re-resolves the doc to Entity"
    );
    assert!(
        type_diag_count(&app) >= 1,
        "the config change must re-validate immediately and surface the wrong-type finding"
    );

    // Removing the rule again must re-resolve back to unbound and clear the finding.
    app.remove_binding_rule(0);
    drive_until_landed(&mut app);
    assert!(
        !app.active_document().unwrap().binding.is_bound(),
        "removing the rule falls back to unbound"
    );
    assert_eq!(
        type_diag_count(&app),
        0,
        "removing the rule clears the type finding (no stale finding survives)"
    );

    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn trigger_e_override_change_revalidates_immediately() {
    // (e) Setting / clearing a per-document override re-resolves the active doc
    // immediately and validation switches to the new type.
    let project = temp_project("override");
    let entity = write_entity_schema(&project, "entity.schema.json");
    let widget = write_widget_schema(&project, "widget.schema.json");
    write_entity_bindings(&project, &entity);
    // `(id: 7)` — Entity (id: integer) accepts; Widget (id: string) rejects.
    let doc_path = project.join("thing.ron");
    std::fs::write(&doc_path, b"(id: 7)\n").expect("write doc");

    let mut app = App::new(AppSettings::default(), Some(doc_path));
    drive_until_landed(&mut app);
    assert_eq!(
        type_diag_count(&app),
        0,
        "config-bound to Entity, the integer id is valid"
    );

    // Set override → Widget (id: string): the integer id now violates. Must
    // re-resolve + re-validate immediately (no edit).
    app.set_active_override(
        "Widget".to_string(),
        TypeSourceLocator::SchemaFile(widget.clone()),
    );
    drive_until_landed(&mut app);
    assert_eq!(
        app.active_document().and_then(|d| d.binding.origin()),
        Some(BindingOrigin::Override),
        "the override takes precedence (origin Override)"
    );
    assert!(
        type_diag_count(&app) >= 1,
        "setting the Widget override must re-validate the integer id as a violation"
    );

    // Clear override → fall back to Entity (config): the finding clears (no stale).
    app.clear_active_override();
    drive_until_landed(&mut app);
    assert_eq!(
        app.active_document().and_then(|d| d.binding.origin()),
        Some(BindingOrigin::Config),
        "clearing the override falls back to the config rule"
    );
    assert_eq!(
        type_diag_count(&app),
        0,
        "back on Entity the integer id is valid (no stale finding survives the override clear)"
    );

    let _ = std::fs::remove_dir_all(&project);
}
