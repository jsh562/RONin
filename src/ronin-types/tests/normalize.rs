//! TR-010 / TR-015 / SC-005 — end-to-end multi-source normalization.
//!
//! Exercises [`normalize`] through the real adapters:
//!
//! - **(a) Conflict resolution.** A `syn` source and a higher-precedence
//!   user-schema source define the *same* named type with *different* shapes.
//!   The user-schema shape wins and a single
//!   [`DiagnosticCategory::SourceConflict`] diagnostic names winner and loser.
//! - **(b) Empty fallback.** No sources (and a set of contribute-nothing
//!   sources) yield a valid empty / structural-only model — never an error
//!   (SC-005, ADR-0004 Progressive Intelligence).

use ronin_types::diagnostics::DiagnosticCategory;
use ronin_types::model::NodeKind;
use ronin_types::normalize;
use ronin_types::source::TypeSource;
use ronin_types::{JsonSchemaSource, SynSource};

/// A user schema for `Config` that disagrees with the syn definition below:
/// here `Config` is a string-keyed map, while syn sees it as a struct.
const USER_CONFIG_AS_MAP: &str = r#"{
  "title": "Config",
  "type": "object",
  "additionalProperties": { "type": "string" }
}"#;

#[test]
fn conflict_resolves_to_higher_precedence_with_diagnostic() {
    // syn: `struct Config { host: String }` (an object, precedence syn).
    let syn = SynSource::from_source("struct Config { host: String }");
    // user-schema: `Config` is a string→string map (precedence user-schema).
    let user = JsonSchemaSource::from_json_str(USER_CONFIG_AS_MAP);

    let sources: Vec<Box<dyn TypeSource>> = vec![Box::new(syn), Box::new(user)];
    let model = normalize(&sources);

    // The higher-precedence user schema wins: Config is a map, not an object.
    let config = model.lookup("Config").expect("Config registered");
    assert!(
        matches!(config.kind, NodeKind::Map { .. }),
        "user-schema (map) wins over syn (object), got {:?}",
        config.kind
    );

    // Exactly one source-conflict diagnostic for Config, naming both sources.
    let conflicts: Vec<_> = model
        .diagnostics
        .iter()
        .filter(|d| d.category == DiagnosticCategory::SourceConflict && d.subject == "Config")
        .collect();
    assert_eq!(conflicts.len(), 1, "one conflict diagnostic for Config");
    let conflict = conflicts[0];
    assert!(
        conflict.detail.contains("user-schema"),
        "names the winning source"
    );
    assert!(conflict.detail.contains("syn"), "names the losing source");
    // The diagnostic is attributed to the winner for auditing.
    assert_eq!(conflict.source_id.as_deref(), Some("user-schema"));
}

#[test]
fn no_sources_yield_empty_structural_only_model() {
    let sources: Vec<Box<dyn TypeSource>> = Vec::new();
    let model = normalize(&sources);
    assert!(model.is_empty(), "no named types");
    assert!(model.diagnostics.is_empty(), "no diagnostics, no error");
    // Still a valid model with the pinned dialect.
    assert!(!model.schema_dialect.is_empty());
}

#[test]
fn contribute_nothing_sources_yield_empty_model() {
    // A syn source over empty/garbage source text and an empty user schema both
    // contribute no named types; the merged model is still empty, not an error.
    let syn = SynSource::from_source("// just a comment, no types\n");
    let user = JsonSchemaSource::from_json_str("{}");
    let sources: Vec<Box<dyn TypeSource>> = vec![Box::new(syn), Box::new(user)];

    let model = normalize(&sources);
    assert!(
        model.is_empty(),
        "no named types from contribute-nothing sources, got {:?}",
        model.named_types.keys().collect::<Vec<_>>()
    );
}

#[test]
fn disjoint_real_sources_union_all_types() {
    let syn = SynSource::from_source("struct A { x: i32 }");
    let user = JsonSchemaSource::from_json_str(
        r#"{ "title": "B", "type": "object", "properties": { "y": { "type": "string" } }, "required": ["y"] }"#,
    );
    let sources: Vec<Box<dyn TypeSource>> = vec![Box::new(syn), Box::new(user)];
    let model = normalize(&sources);
    assert!(model.contains("A"));
    assert!(model.contains("B"));
}
