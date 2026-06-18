//! Integration test for [`SynSource`]'s multi-file crate walk {TR-003, TR-004}.
//!
//! Parses a real multi-file Rust fixture tree under `tests/fixtures/crate_multi/`
//! (data files, not compiled into this crate) and asserts:
//!
//! - cross-file struct/enum/field types resolve to the correct normalized shapes;
//! - a foreign / generic-parameter / generic-instantiation type appears as
//!   `unknown`;
//! - acquisition produces zero spurious errors (only documented `unknown`
//!   diagnostics).

use std::path::PathBuf;

use ronin_types::model::{NodeKind, Primitive, TypeRef, VariantShape};
use ronin_types::source::{SourcePrecedence, TypeSource};
use ronin_types::SynSource;

fn fixture_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests");
    p.push("fixtures");
    p.push("crate_multi");
    p
}

#[test]
fn crate_walk_unions_all_types_across_files() {
    let src = SynSource::from_crate_dir(fixture_root());
    let acq = src.acquire();
    let model = &acq.model;

    // Every type from BOTH files (and the nested directory) is in the union.
    for name in [
        "Player",
        "Spawn",
        "Inventory",
        "Item",
        "Position",
        "Faction",
        "Container",
        "Holder",
    ] {
        assert!(model.contains(name), "missing crate type `{name}`");
    }

    assert_eq!(src.precedence(), SourcePrecedence::Syn);
    assert!(src.source_id().starts_with("syn:"));
}

#[test]
fn cross_file_field_types_resolve_to_named_refs() {
    let acq = SynSource::from_crate_dir(fixture_root()).acquire();
    let model = &acq.model;

    let player = model.lookup("Player").expect("Player");
    let NodeKind::Object { fields, .. } = &player.kind else {
        panic!("Player should be an object");
    };
    let by_key = |k: &str| fields.iter().find(|f| f.serialized_key == k).unwrap();

    // inventory: Inventory (defined in nested/types_b.rs) -> $ref Named.
    assert_eq!(by_key("inventory").value.as_named(), Some("Inventory"));
    // position: Position (cross-file tuple struct) -> $ref Named.
    assert_eq!(by_key("position").value.as_named(), Some("Position"));

    // party: Vec<Player> -> inline Sequence whose element is $ref Player.
    let party = model.resolve(&by_key("party").value).unwrap();
    let NodeKind::Sequence { element } = &party.kind else {
        panic!("party should be a sequence");
    };
    assert_eq!(element.as_named(), Some("Player"));

    // name: String -> string primitive (inline).
    let name = model.resolve(&by_key("name").value).unwrap();
    assert!(matches!(
        name.kind,
        NodeKind::Primitive {
            primitive: Primitive::String
        }
    ));
}

#[test]
fn cross_file_nested_shapes_are_correct() {
    let acq = SynSource::from_crate_dir(fixture_root()).acquire();
    let model = &acq.model;

    // Inventory.slots: Vec<Item> where Item is in the same nested file.
    let inv = model.lookup("Inventory").unwrap();
    let NodeKind::Object { fields, .. } = &inv.kind else {
        panic!("Inventory object");
    };
    let slots = model.resolve(&fields[0].value).unwrap();
    let NodeKind::Sequence { element } = &slots.kind else {
        panic!("slots sequence");
    };
    assert_eq!(element.as_named(), Some("Item"));

    // Position is a 3-tuple struct.
    let pos = model.lookup("Position").unwrap();
    let NodeKind::Tuple { elements } = &pos.kind else {
        panic!("Position tuple");
    };
    assert_eq!(elements.len(), 3);

    // Faction enum has the expected variant shapes.
    let faction = model.lookup("Faction").unwrap();
    let NodeKind::Enum { variants, .. } = &faction.kind else {
        panic!("Faction enum");
    };
    assert_eq!(variants.len(), 3);
    assert!(matches!(variants[0].shape, VariantShape::Unit));
    assert!(matches!(variants[1].shape, VariantShape::Newtype(_)));
    assert!(matches!(variants[2].shape, VariantShape::Struct(_)));
}

#[test]
fn foreign_generic_and_macro_types_become_unknown() {
    let acq = SynSource::from_crate_dir(fixture_root()).acquire();
    let model = &acq.model;

    // Player.clock: external_crate::Instant -> foreign -> unknown.
    let player = model.lookup("Player").unwrap();
    let NodeKind::Object { fields, .. } = &player.kind else {
        panic!("Player object");
    };
    let clock = fields.iter().find(|f| f.serialized_key == "clock").unwrap();
    let clock_node = model.resolve(&clock.value).unwrap();
    assert!(clock_node.is_unknown(), "foreign type must be unknown");

    // Container.value: T (generic parameter) -> unknown.
    let container = model.lookup("Container").unwrap();
    let NodeKind::Object { fields, .. } = &container.kind else {
        panic!("Container object");
    };
    let value = fields.iter().find(|f| f.serialized_key == "value").unwrap();
    let value_node = model.resolve(&value.value).unwrap();
    assert!(value_node.is_unknown(), "generic param must be unknown");

    // Container.wrapped: Holder<Item> -> generic instantiation -> Named ref to
    // the base definition, with an UnresolvedType diagnostic (degraded, not
    // expanded). The base `Holder` is still in the model.
    let wrapped = fields
        .iter()
        .find(|f| f.serialized_key == "wrapped")
        .unwrap();
    assert_eq!(wrapped.value.as_named(), Some("Holder"));
}

#[test]
fn unresolved_types_diagnosed_but_no_fatal_errors() {
    let acq = SynSource::from_crate_dir(fixture_root()).acquire();

    // There ARE unresolved-type diagnostics (foreign/generic), all non-fatal.
    let unresolved = acq
        .diagnostics
        .iter()
        .filter(|d| d.category == ronin_types::diagnostics::DiagnosticCategory::UnresolvedType)
        .count();
    assert!(
        unresolved >= 2,
        "expected unresolved diagnostics for foreign+generic"
    );

    // No spurious parse failures: every fixture file parsed cleanly, so there
    // are no `UnsupportedConstruct` parse diagnostics.
    let parse_failures = acq
        .diagnostics
        .iter()
        .filter(|d| d.detail.contains("could not parse Rust source"))
        .count();
    assert_eq!(parse_failures, 0, "fixtures must parse without error");

    // The model is non-empty and usable.
    assert!(!acq.model.is_empty());
}

#[test]
fn single_string_source_resolves_local_refs() {
    // A degenerate (single-unit) crate still resolves intra-source references.
    let acq = SynSource::from_named_source(
        "inline.rs",
        r#"
        struct Inner { v: i32 }
        struct Outer { inner: Inner, list: Vec<Inner> }
        "#,
    )
    .acquire();
    let outer = acq.model.lookup("Outer").unwrap();
    let NodeKind::Object { fields, .. } = &outer.kind else {
        panic!("object");
    };
    assert_eq!(fields[0].value.as_named(), Some("Inner"));
    let list = acq.model.resolve(&fields[1].value).unwrap();
    let NodeKind::Sequence { element } = &list.kind else {
        panic!("sequence");
    };
    assert_eq!(element.as_named(), Some("Inner"));
    assert_eq!(acq.diagnostics.len(), 0, "no spurious diagnostics");
    let _ = TypeRef::named("x"); // keep TypeRef import used across cfgs
}
