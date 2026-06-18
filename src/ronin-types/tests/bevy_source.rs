//! E009 T010 — the native Bevy registry [`BevySource`] adapter.
//!
//! Loads a realistic BRP `bevy/registry/schema`-shaped fixture and asserts:
//!
//! - each reflect `kind` maps to the matching [`NodeKind`] (Struct → object,
//!   TupleStruct → tuple, Enum → variants, List → sequence, Map → map, Value →
//!   primitive);
//! - the adapter is **version-tolerant** — unknown/extra schema fields are
//!   ignored, never fatal (FR-003);
//! - an **unrecognized reflect kind** degrades to a first-class `unknown` node
//!   plus a non-fatal diagnostic, never a false error (FR-006/008);
//! - **malformed / partial / empty** input never fails — it yields an
//!   empty-or-partial model + diagnostics, never a panic (FR-008).
//!
//! No `bevy` crate is involved: the fully-qualified type paths are JSON string
//! keys, consumed strictly as data (FR-003, ADR-0002).

use ronin_types::model::{NodeKind, Primitive, VariantShape};
use ronin_types::source::{BevyRegistry, ReflectKind};
use ronin_types::{BevySource, TypeSource};

/// Path to the shared registry-schema fixture.
const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/bevy_registry_schema.json"
);

/// Load the fixture's text.
fn fixture_json() -> String {
    std::fs::read_to_string(FIXTURE).expect("fixture readable")
}

#[test]
fn precedence_and_source_id() {
    let src = BevySource::from_schema_json("{}");
    assert_eq!(
        src.precedence(),
        ronin_types::source::SourcePrecedence::Bevy,
        "Bevy is the highest precedence rank"
    );
    assert_eq!(src.source_id(), "bevy-registry");
}

#[test]
fn struct_kind_maps_to_object() {
    let acq = BevySource::from_schema_json(fixture_json()).acquire();
    let transform = acq
        .model
        .lookup("bevy_transform::components::transform::Transform")
        .expect("Transform registered");
    let NodeKind::Object {
        fields,
        deny_unknown_fields,
    } = &transform.kind
    else {
        panic!("Struct → object, got {:?}", transform.kind);
    };
    assert!(
        deny_unknown_fields,
        "reflect structs set additionalProperties:false → deny_unknown_fields"
    );
    assert_eq!(fields.len(), 3);
    // Fields reference the named glam types by their full type path.
    let translation = fields
        .iter()
        .find(|f| f.serialized_key == "translation")
        .unwrap();
    assert_eq!(translation.value.as_named(), Some("glam::Vec3"));
    assert!(!translation.optional, "translation is reflect-required");
    assert!(acq.model.contains("glam::Vec3"), "referenced type present");
}

#[test]
fn tuple_struct_kind_maps_to_tuple() {
    let acq = BevySource::from_schema_json(fixture_json()).acquire();
    let quat = acq.model.lookup("glam::Quat").expect("Quat registered");
    let NodeKind::Tuple { elements } = &quat.kind else {
        panic!("TupleStruct → tuple, got {:?}", quat.kind);
    };
    assert_eq!(elements.len(), 4, "Quat is a 4-tuple");
    // A tuple node auto-attaches the RonKind::Tuple extension with the arity.
    assert_eq!(
        quat.ron_extension.as_ref().and_then(|e| e.tuple_arity),
        Some(4)
    );
}

#[test]
fn enum_kind_maps_to_unit_variants() {
    let acq = BevySource::from_schema_json(fixture_json()).acquire();
    let vis = acq
        .model
        .lookup("bevy_pbr::light::Visibility")
        .expect("Visibility registered");
    let NodeKind::Enum { variants, .. } = &vis.kind else {
        panic!("Enum → enum, got {:?}", vis.kind);
    };
    assert_eq!(variants.len(), 3);
    let names: Vec<&str> = variants
        .iter()
        .map(|v| v.serialized_name.as_str())
        .collect();
    assert_eq!(names, ["Inherited", "Hidden", "Visible"]);
    assert!(variants
        .iter()
        .all(|v| matches!(v.shape, VariantShape::Unit)));
}

#[test]
fn list_kind_maps_to_sequence() {
    let acq = BevySource::from_schema_json(fixture_json()).acquire();
    let list = acq
        .model
        .lookup("bevy_core::name::TagList")
        .expect("TagList registered");
    let NodeKind::Sequence { element } = &list.kind else {
        panic!("List → sequence, got {:?}", list.kind);
    };
    let elem = acq.model.resolve(element).expect("element resolves");
    assert!(matches!(
        elem.kind,
        NodeKind::Primitive {
            primitive: Primitive::String
        }
    ));
}

#[test]
fn map_kind_maps_to_map() {
    let acq = BevySource::from_schema_json(fixture_json()).acquire();
    let map = acq
        .model
        .lookup("bevy_utils::label::LabelMap")
        .expect("LabelMap registered");
    let NodeKind::Map { key, value } = &map.kind else {
        panic!("Map → map, got {:?}", map.kind);
    };
    // String key → ordinary string-keyed map.
    let key_node = acq.model.resolve(key).expect("key resolves");
    assert!(matches!(
        key_node.kind,
        NodeKind::Primitive {
            primitive: Primitive::String
        }
    ));
    // Value references the named Vec3 type.
    assert_eq!(value.as_named(), Some("glam::Vec3"));
}

#[test]
fn value_kind_maps_to_primitive() {
    let acq = BevySource::from_schema_json(fixture_json()).acquire();
    let f32_node = acq
        .model
        .lookup("core::primitive::f32")
        .expect("f32 registered");
    assert!(matches!(
        f32_node.kind,
        NodeKind::Primitive {
            primitive: Primitive::Number
        }
    ));
}

#[test]
fn unrecognized_kind_degrades_to_unknown_with_diagnostic() {
    let acq = BevySource::from_schema_json(fixture_json()).acquire();
    let fancy = acq
        .model
        .lookup("some_mod::experimental::FancyThing")
        .expect("FancyThing still registered (as unknown)");
    assert!(
        fancy.is_unknown(),
        "an unrecognized reflect kind becomes a first-class unknown node"
    );
    assert!(
        acq.diagnostics
            .iter()
            .any(|d| d.subject == "some_mod::experimental::FancyThing"
                && d.category == ronin_types::diagnostics::DiagnosticCategory::UnsupportedConstruct),
        "the degradation is recorded as a non-fatal diagnostic"
    );
    // Diagnostics carry the source provenance.
    assert!(acq
        .diagnostics
        .iter()
        .all(|d| d.source_id.as_deref() == Some("bevy-registry")));
}

#[test]
fn version_tolerance_unknown_fields_are_ignored() {
    // The fixture carries `registryFormat`, `x_future_field_for_tolerance`,
    // `unknown_enum_metadata`, etc. None of these are fatal; the known types are
    // still acquired and the apparent version is read.
    let (registry, diags) =
        BevyRegistry::from_schema_json(&fixture_json(), "bevy-registry", "<fixture>");
    assert_eq!(registry.apparent_version(), Some("0.16.0"));
    assert!(registry.contains("glam::Vec3"));
    assert_eq!(
        registry.reflect_kind("glam::Quat"),
        Some(&ReflectKind::TupleStruct)
    );
    // Only the unrecognized-kind type would normally emit a *mapping* diagnostic;
    // pure parsing of the registry produces none (unknown fields are ignored).
    assert!(
        diags.is_empty(),
        "version-tolerant parse emits no diagnostics: {diags:?}"
    );
}

#[test]
fn registry_reports_default_reflection_and_values() {
    let (registry, _) =
        BevyRegistry::from_schema_json(&fixture_json(), "bevy-registry", "<fixture>");
    // Transform reflects Default AND the fixture is a defaults-carrying export.
    assert!(registry.is_default_reflected("bevy_transform::components::transform::Transform"));
    assert!(registry
        .default_value("bevy_transform::components::transform::Transform")
        .is_some());
    // Vec3 reflects Default but carries no concrete default value here.
    assert!(registry.is_default_reflected("glam::Vec3"));
    assert!(registry.default_value("glam::Vec3").is_none());
    // The unrecognized-kind type does not reflect Default.
    assert!(!registry.is_default_reflected("some_mod::experimental::FancyThing"));
}

#[test]
fn malformed_json_never_fails() {
    let acq = BevySource::from_schema_json("{ this is not json ]").acquire();
    assert!(acq.model.is_empty(), "malformed input → empty model");
    assert!(
        !acq.diagnostics.is_empty(),
        "malformed input → at least one diagnostic, never a panic"
    );
}

#[test]
fn partial_json_yields_partial_model_no_panic() {
    // One good entry, one entry that is not a JSON object. The good one is
    // acquired; the bad one is skipped with a diagnostic — never a panic.
    let partial = r#"{
        "$defs": {
            "ok::Thing": { "kind": "Value", "type": "integer" },
            "bad::Thing": "this should be an object"
        }
    }"#;
    let acq = BevySource::from_schema_json(partial).acquire();
    assert!(acq.model.contains("ok::Thing"), "good entry acquired");
    assert!(!acq.model.contains("bad::Thing"), "bad entry skipped");
    assert!(acq.diagnostics.iter().any(|d| d.subject == "bad::Thing"));
}

#[test]
fn empty_input_is_structural_only() {
    let acq = BevySource::from_schema_json("{}").acquire();
    assert!(acq.model.is_empty(), "empty registry → empty model");
    assert!(
        acq.diagnostics.is_empty(),
        "a well-formed empty object is not an error"
    );
}

#[test]
fn non_object_root_is_diagnostic_not_panic() {
    let acq = BevySource::from_schema_json("\"not a registry\"").acquire();
    assert!(acq.model.is_empty());
    assert_eq!(acq.diagnostics.len(), 1);
}

#[test]
fn from_registry_round_trips_through_acquire() {
    // The constructor a future BRP read reuses: hand an already-parsed registry.
    let (registry, _) =
        BevyRegistry::from_schema_json(&fixture_json(), "bevy-registry", "<fixture>");
    let acq = BevySource::from_registry(registry).acquire();
    assert!(acq.model.contains("glam::Vec3"));
    assert!(acq.model.contains("bevy_pbr::light::Visibility"));
}
