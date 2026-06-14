//! Type-model acquisition glue (E006 US2 — Phase 4b; FR-014, FR-024).
//!
//! This module is the seam that turns a resolved [`TypeBinding`] into a
//! [`BoundType`] the off-frame validator can run against. It bridges the *pure*
//! binding-config core ([`crate::binding`]) to E004's native `ron-types`
//! acquisition pipeline:
//!
//! 1. pick a [`TypeSource`](ron_types::TypeSource) adapter from the binding's
//!    [`TypeSourceLocator`] (`RustSource` → [`SynSource`](ron_types::SynSource),
//!    `SchemaFile` → [`JsonSchemaSource`](ron_types::JsonSchemaSource)),
//! 2. [`acquire`](ron_types::TypeSource::acquire) + [`normalize`](ron_types::normalize)
//!    into one [`TypeModel`](ron_types::TypeModel),
//! 3. serialize with [`ron_types::to_json`] into the frozen JSON-Schema-2020-12 +
//!    `x-ron-*` interchange `serde_json::Value`,
//! 4. wrap it with the binding's `type_name` as a [`BoundType`].
//!
//! # Fail-soft contract (FR-014, FR-024, ADR-0004 Progressive Intelligence)
//!
//! Acquisition is **best-effort and never destructive**:
//!
//! - The type source path is read **as data only** — `SynSource` parses Rust with
//!   `syn` (static AST, never executed) and `JsonSchemaSource` parses JSON; neither
//!   runs the user's code (FR-024, project-instructions §VI).
//! - ANY failure or unresolvable/malformed source yields [`None`], so the caller
//!   degrades to structural-only validation rather than crashing (FR-014). A
//!   missing file, unreadable path, garbage Rust, or invalid JSON Schema all
//!   degrade to [`None`] via `ron-types`' never-fail acquire contract — there is
//!   nothing to catch because `ron-types` records failures as diagnostics and
//!   returns a (possibly empty) model rather than panicking.
//! - A [`BindingState::NoBinding`] binding yields [`None`] directly.
//!
//! # No-false-positive policy for a missing def
//!
//! After acquisition we require the binding's `type_name` to actually exist in the
//! acquired model's `$defs`. If the source resolved but does **not** define that
//! type (e.g. a typo in the rule, or the type was removed from the source), we
//! return [`None`] rather than a model the validator would have to handle as a
//! missing reference. Returning [`None`] degrades the document to structural-only,
//! which is the safe, no-false-positive outcome: the author sees "no type bound"
//! instead of a spurious cascade of errors against an absent schema (FR-016,
//! ADR-0004). The validator's own unknown/unconstrained handling still covers
//! `unknown` *nodes* inside a model that DOES contain the requested type.

use std::path::Path;
use std::sync::Arc;

use ron_types::{JsonSchemaSource, SynSource, TypeModel, TypeSource};

use crate::binding::{
    contain_type_source, resolve, BindingConfig, BindingState, DocumentOverride, TypeBinding,
    TypeSourceLocator,
};
use crate::reparse::BoundType;

/// Acquire a [`BoundType`] for a resolved [`TypeBinding`], or [`None`] when the
/// binding is [`BindingState::NoBinding`] or acquisition cannot produce the bound
/// type (FR-014, FR-024, FR-025).
///
/// For a [`BindingState::Bound`] binding this picks the source adapter by the
/// binding's [`TypeSourceLocator`], acquires + normalizes a [`TypeModel`],
/// serializes it via [`ron_types::to_json`], and wraps it with the binding's
/// `type_name`.
///
/// # Project-root containment (FR-025)
///
/// `project_root` is the trusted boundary for an *adversarial* config: the
/// binding's `type_source` path is resolved relative to it and rejected when it
/// escapes the root (a `..` traversal, or an absolute path outside the project)
/// via [`contain_type_source`]. A rejected source degrades the binding to
/// structural-only ([`None`]) — the out-of-project file is **never read**, so a
/// hostile config cannot widen RONin's read surface beyond the opened project.
///
/// Returns [`None`] (caller degrades to structural-only, never a panic) when:
/// - the binding is `NoBinding`,
/// - the `type_source` escapes `project_root` (path-traversal / out-of-project),
/// - the acquired model is empty (the source resolved to nothing), or
/// - the acquired model does not define the bound `type_name` (see the
///   module-level no-false-positive note).
///
/// The source path is consumed as **data only** and never executed (FR-024).
#[must_use]
pub fn acquire_bound_type(binding: &TypeBinding, project_root: &Path) -> Option<BoundType> {
    let BindingState::Bound {
        type_name,
        type_source,
        ..
    } = &binding.state
    else {
        // NoBinding ⇒ no model to acquire (FR-015).
        return None;
    };

    // FR-025: containment guard. Reject any type_source that escapes the project
    // root (path-traversal / out-of-project / unsafe symlink) *before* reading it,
    // degrading that binding to structural-only. The contained, normalized path is
    // what we hand to E004 acquisition — never the raw, possibly-escaping path.
    let contained = contain_type_source(project_root, type_source.path())?;
    let safe_source = match type_source {
        TypeSourceLocator::RustSource(_) => TypeSourceLocator::RustSource(contained),
        TypeSourceLocator::SchemaFile(_) => TypeSourceLocator::SchemaFile(contained),
    };

    // Acquire + normalize from the located source (never executes the source).
    let model = acquire_model(&safe_source);

    // An empty model means the source resolved to nothing usable — degrade to
    // structural-only rather than binding to a model with no defs.
    if model.is_empty() {
        return None;
    }

    // No-false-positive policy: only bind when the requested type actually exists
    // in the acquired model's `$defs`. A model that lacks it would make every
    // value a spurious mismatch against an absent schema, so degrade instead.
    if !model.contains(type_name) {
        return None;
    }

    // Serialize to the frozen JSON-Schema-2020-12 + `x-ron-*` interchange the
    // WASM-clean validator consumes.
    let interchange = ron_types::to_json(&model);

    Some(BoundType {
        model: Arc::new(interchange),
        type_name: type_name.clone(),
    })
}

/// Resolve a document's binding from `config` (+ optional session override) and
/// acquire its [`BoundType`] in one step — the entry point the UI / re-validation
/// triggers (T037) call (FR-014, FR-025).
///
/// Resolution and acquisition stay separately callable ([`resolve`] /
/// [`acquire_bound_type`]); this is the convenience that pairs them. The returned
/// [`TypeBinding`] is always meaningful (it powers the active-binding UI), while
/// the [`Option<BoundType>`] is [`None`] whenever the binding is unbound, the
/// model could not be acquired, or the `type_source` escaped `project_root`
/// (degrade to structural-only).
///
/// `project_root` is the trusted containment boundary threaded into
/// [`acquire_bound_type`]: an out-of-project / path-traversal `type_source`
/// degrades that binding to structural-only and the offending file is never read
/// (FR-025). The display [`TypeBinding`] still reflects the *intended* binding so
/// the active-binding indicator shows what was configured even when acquisition
/// safely degrades.
#[must_use]
pub fn resolve_and_acquire(
    config: &BindingConfig,
    doc_path: Option<&Path>,
    override_: Option<&DocumentOverride>,
    project_root: &Path,
) -> (TypeBinding, Option<BoundType>) {
    let binding = resolve(config, doc_path, override_);
    let bound = acquire_bound_type(&binding, project_root);
    (binding, bound)
}

/// Acquire + normalize a [`TypeModel`] from a single [`TypeSourceLocator`].
///
/// Picks the adapter by source kind, runs `ron-types`' never-fail acquire through
/// [`normalize`](ron_types::normalize) (so the result carries provenance and
/// merge-time diagnostics consistently with multi-source acquisition), and returns
/// the (possibly empty) model. Never panics; a malformed/unreadable source yields
/// an empty model via the adapter's diagnostics path (FR-024).
fn acquire_model(source: &TypeSourceLocator) -> TypeModel {
    let adapter: Box<dyn TypeSource> = match source {
        // Rust source: a single `.rs` file or a crate/dir tree. `SynSource`
        // detects the input by reading it as data (static `syn` AST — never run).
        TypeSourceLocator::RustSource(path) => Box::new(syn_source_for(path)),
        // A user-authored JSON Schema 2020-12 file, read as data.
        TypeSourceLocator::SchemaFile(path) => Box::new(JsonSchemaSource::from_path(path)),
    };
    ron_types::normalize(&[adapter])
}

/// Build a [`SynSource`] for a Rust source path, walking a directory or reading a
/// single file. A non-existent path is handled by `SynSource`'s never-fail read
/// (it records a diagnostic and yields an empty model), so this never errors.
fn syn_source_for(path: &Path) -> SynSource {
    if path.is_dir() {
        SynSource::from_crate_dir(path)
    } else {
        // Covers a real `.rs` file and (defensively) a missing path: the latter
        // becomes an unparseable unit + diagnostic, not a panic (FR-024).
        SynSource::from_path(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binding::{BindingOrigin, BindingRule};
    use std::path::PathBuf;

    /// A unique temp directory for this test process, created fresh.
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ronin_type_acquire_{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Build a `Bound` binding pointing at a schema file.
    fn schema_binding(type_name: &str, schema_path: &Path) -> TypeBinding {
        TypeBinding::bound(
            type_name.to_string(),
            TypeSourceLocator::SchemaFile(schema_path.to_path_buf()),
            BindingOrigin::Config,
        )
    }

    /// Build a `Bound` binding pointing at a Rust source file/dir.
    fn rust_binding(type_name: &str, rust_path: &Path) -> TypeBinding {
        TypeBinding::bound(
            type_name.to_string(),
            TypeSourceLocator::RustSource(rust_path.to_path_buf()),
            BindingOrigin::Config,
        )
    }

    #[test]
    fn acquire_from_json_schema_file_yields_bound_type() {
        let dir = temp_dir("schema_ok");
        let path = dir.join("config.schema.json");
        // A 2020-12 schema whose `$defs` defines the bound type `AppConfig`.
        let schema = br#"{
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$defs": {
                "AppConfig": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" },
                        "port": { "type": "integer" }
                    },
                    "required": ["host", "port"]
                }
            }
        }"#;
        std::fs::write(&path, schema).unwrap();

        let binding = schema_binding("AppConfig", &path);
        let bound =
            acquire_bound_type(&binding, &dir).expect("schema with the def yields a BoundType");

        assert_eq!(bound.type_name, "AppConfig");
        // The serialized interchange carries the requested def under `$defs`.
        let defs = bound
            .model
            .get("$defs")
            .and_then(|d| d.as_object())
            .expect("interchange has a $defs object");
        assert!(
            defs.contains_key("AppConfig"),
            "the acquired model's $defs contains the bound type"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn no_binding_yields_none() {
        assert!(acquire_bound_type(&TypeBinding::none(), Path::new(".")).is_none());
    }

    #[test]
    fn nonexistent_schema_path_yields_none_no_panic() {
        let dir = temp_dir("schema_missing");
        let path = dir.join("does-not-exist.schema.json");
        // The file is never created; acquire must degrade to None, not panic.
        let binding = schema_binding("Whatever", &path);
        assert!(acquire_bound_type(&binding, &dir).is_none());
    }

    #[test]
    fn malformed_schema_yields_none_no_panic() {
        let dir = temp_dir("schema_bad");
        let path = dir.join("garbage.schema.json");
        std::fs::write(&path, b"this is not json at all {{{").unwrap();
        let binding = schema_binding("Whatever", &path);
        assert!(acquire_bound_type(&binding, &dir).is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn schema_missing_requested_type_yields_none() {
        // The schema resolves fine but does NOT define the bound type name; the
        // no-false-positive policy degrades to None (structural-only).
        let dir = temp_dir("schema_wrong_name");
        let path = dir.join("other.schema.json");
        let schema = br#"{
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$defs": { "SomethingElse": { "type": "object" } }
        }"#;
        std::fs::write(&path, schema).unwrap();
        let binding = schema_binding("AppConfig", &path);
        assert!(acquire_bound_type(&binding, &dir).is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn acquire_from_rust_source_file_yields_bound_type() {
        // A tiny `.rs` with a two-field struct; SynSource lowers it to a named def.
        let dir = temp_dir("rust_ok");
        let path = dir.join("types.rs");
        std::fs::write(&path, b"pub struct Point { pub x: i32, pub y: f64 }\n").unwrap();

        let binding = rust_binding("Point", &path);
        let bound = acquire_bound_type(&binding, &dir).expect("rust struct yields a BoundType");

        assert_eq!(bound.type_name, "Point");
        let defs = bound
            .model
            .get("$defs")
            .and_then(|d| d.as_object())
            .expect("interchange has a $defs object");
        assert!(defs.contains_key("Point"), "$defs contains the Rust struct");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn nonexistent_rust_path_yields_none_no_panic() {
        let dir = temp_dir("rust_missing");
        let path = dir.join("nope.rs");
        let binding = rust_binding("Anything", &path);
        assert!(acquire_bound_type(&binding, &dir).is_none());
    }

    #[test]
    fn resolve_and_acquire_pairs_resolution_with_acquisition() {
        // A config rule whose schema file defines the bound type: resolve to a
        // `Bound` binding (Config origin) and acquire its model in one step.
        let dir = temp_dir("resolve_acquire");
        let schema_path = dir.join("doc.schema.json");
        let schema = br#"{
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$defs": { "Doc": { "type": "object" } }
        }"#;
        std::fs::write(&schema_path, schema).unwrap();

        let config = BindingConfig {
            rules: vec![BindingRule {
                pattern: "**/*.ron".to_string(),
                exclude: None,
                type_name: "Doc".to_string(),
                type_source: TypeSourceLocator::SchemaFile(schema_path.clone()),
            }],
            version: crate::binding::BINDING_CONFIG_VERSION,
        };

        let doc = Path::new("data/sample.ron");
        // The schema lives under `dir`, so the project root is `dir` (containment
        // accepts the absolute schema path that resolves inside the root).
        let (binding, bound) = resolve_and_acquire(&config, Some(doc), None, &dir);
        assert!(binding.is_bound(), "the rule matches the document");
        assert_eq!(binding.origin(), Some(BindingOrigin::Config));
        let bound = bound.expect("the resolved schema acquires a BoundType");
        assert_eq!(bound.type_name, "Doc");

        let _ = std::fs::remove_file(&schema_path);
    }

    #[test]
    fn resolve_and_acquire_no_match_yields_no_binding_and_none() {
        let config = BindingConfig::default();
        let doc = Path::new("data/sample.ron");
        let (binding, bound) = resolve_and_acquire(&config, Some(doc), None, Path::new("."));
        assert!(!binding.is_bound(), "empty config ⇒ NoBinding");
        assert!(bound.is_none(), "NoBinding ⇒ no BoundType");
    }
}
