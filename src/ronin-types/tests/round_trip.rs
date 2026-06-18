//! Round-trip property tests over every RON value kind {TR-002} [COMPLETES TR-002].
//!
//! For each RON value kind — tuple, char, unit, option (incl. implicit-some),
//! non-string-key map, bytes — and for the three RON extension flags
//! (`implicit_some`, `unwrap_newtypes`, `unwrap_variant_newtypes`), a model node
//! is built, serialized to the JSON-Schema-2020-12-shaped interchange, then
//! deserialized; the result MUST equal the original (SC-001, OBJ1-VC1).
//!
//! The `proptest` generator below produces arbitrary RON-kind nodes (including
//! nested tuples/options/sequences/maps and arbitrary flag combinations) so the
//! round-trip is exercised across a wide space, not just hand-picked cases.

use proptest::prelude::*;
use ronin_types::extension::RonTypeExtension;
use ronin_types::model::{NodeKind, Primitive, TypeModel, TypeNode, TypeRef};
use ronin_types::serialize::{from_json, from_json_str, to_json, to_json_string};

/// A leaf primitive node (the recursion base).
fn primitive_strategy() -> impl Strategy<Value = TypeNode> {
    prop_oneof![
        Just(TypeNode::primitive(Primitive::Boolean)),
        Just(TypeNode::primitive(Primitive::Integer)),
        Just(TypeNode::primitive(Primitive::Number)),
        Just(TypeNode::primitive(Primitive::String)),
        Just(TypeNode::primitive(Primitive::Null)),
        // RON scalar kinds: char/unit/bytes.
        Just(TypeNode::char_()),
        Just(TypeNode::unit()),
        Just(TypeNode::bytes()),
    ]
}

/// A recursive strategy that builds RON-kind nodes (tuple/option/sequence/
/// non-string-key map) over primitive leaves, plus arbitrary extension flags.
fn node_strategy() -> impl Strategy<Value = TypeNode> {
    primitive_strategy().prop_recursive(
        4,  // up to 4 levels deep
        32, // up to 32 total nodes
        4,  // up to 4 children per collection
        |inner| {
            prop_oneof![
                // Fixed-arity tuple (1..=4 elements).
                prop::collection::vec(inner.clone(), 1..=4).prop_map(|nodes| TypeNode::tuple(
                    nodes.into_iter().map(TypeRef::inline).collect()
                )),
                // Option<T>.
                inner
                    .clone()
                    .prop_map(|n| TypeNode::option(TypeRef::inline(n))),
                // Sequence<T> (plain, no RON extension).
                inner
                    .clone()
                    .prop_map(|n| TypeNode::new(NodeKind::Sequence {
                        element: TypeRef::inline(n),
                    })),
                // Non-string-key map<K, V>.
                (inner.clone(), inner.clone()).prop_map(|(k, v)| TypeNode::non_string_key_map(
                    TypeRef::inline(k),
                    TypeRef::inline(v),
                )),
            ]
        },
    )
}

/// Apply an arbitrary combination of the three RON extension flags on top of a
/// node's existing extension (preserving any `ron_kind`/`tuple_arity`).
fn with_flags(node: TypeNode, implicit_some: bool, unwrap_nt: bool, unwrap_vnt: bool) -> TypeNode {
    let base = node.ron_extension.clone().unwrap_or_default();
    let ext = RonTypeExtension {
        implicit_some,
        unwrap_newtypes: unwrap_nt,
        unwrap_variant_newtypes: unwrap_vnt,
        ..base
    };
    if ext.is_empty() {
        node
    } else {
        node.with_ron_extension(ext)
    }
}

proptest! {
    /// Every generated RON-kind node round-trips through the JSON interchange
    /// (Value form) unchanged.
    #[test]
    fn ron_kind_node_round_trips_value(
        node in node_strategy(),
        implicit_some in any::<bool>(),
        unwrap_nt in any::<bool>(),
        unwrap_vnt in any::<bool>(),
    ) {
        let node = with_flags(node, implicit_some, unwrap_nt, unwrap_vnt);
        let mut model = TypeModel::new();
        model.insert_named("Root", node);

        let json = to_json(&model);
        let back = from_json(&json).expect("interchange Value must round-trip");
        prop_assert_eq!(model, back);
    }

    /// Same, through the string form (catches any text-level instability).
    #[test]
    fn ron_kind_node_round_trips_string(
        node in node_strategy(),
        implicit_some in any::<bool>(),
        unwrap_nt in any::<bool>(),
        unwrap_vnt in any::<bool>(),
    ) {
        let node = with_flags(node, implicit_some, unwrap_nt, unwrap_vnt);
        let mut model = TypeModel::new();
        model.insert_named("Root", node);

        let s = to_json_string(&model);
        let back = from_json_str(&s).expect("interchange string must round-trip");
        prop_assert_eq!(model, back);
    }
}

/// Hand-picked exhaustive coverage of each RON value kind in one model, asserting
/// the explicit `x-ron-kind` annotations survive the round-trip (belt-and-braces
/// alongside the property tests).
#[test]
fn every_ron_value_kind_round_trips() {
    let mut model = TypeModel::new();
    model.add_active_extension("implicit_some");
    model.add_active_extension("unwrap_newtypes");
    model.add_active_extension("unwrap_variant_newtypes");

    model.insert_named(
        "TupleKind",
        TypeNode::tuple(vec![
            TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
            TypeRef::inline(TypeNode::char_()),
        ]),
    );
    model.insert_named("CharKind", TypeNode::char_());
    model.insert_named("UnitKind", TypeNode::unit());
    model.insert_named("BytesKind", TypeNode::bytes());
    model.insert_named(
        "OptionKind",
        TypeNode::option(TypeRef::inline(TypeNode::primitive(Primitive::String))),
    );
    model.insert_named(
        "NonStringKeyMapKind",
        TypeNode::non_string_key_map(
            TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
            TypeRef::inline(TypeNode::primitive(Primitive::String)),
        ),
    );
    // A node carrying ONLY extension flags (no ron_kind).
    model.insert_named(
        "FlaggedStruct",
        TypeNode::new(NodeKind::Object {
            fields: vec![],
            deny_unknown_fields: false,
        })
        .with_ron_extension(RonTypeExtension {
            implicit_some: true,
            unwrap_newtypes: true,
            unwrap_variant_newtypes: true,
            ..RonTypeExtension::default()
        }),
    );

    let json = to_json(&model);
    let back = from_json(&json).expect("round-trips");
    assert_eq!(model, back);

    // Spot-check the wire annotations are present.
    let defs = &json["$defs"];
    assert_eq!(defs["TupleKind"]["x-ron-kind"], serde_json::json!("tuple"));
    assert_eq!(defs["TupleKind"]["x-ron-tuple-arity"], serde_json::json!(2));
    assert_eq!(defs["CharKind"]["x-ron-kind"], serde_json::json!("char"));
    assert_eq!(defs["UnitKind"]["x-ron-kind"], serde_json::json!("unit"));
    assert_eq!(defs["BytesKind"]["x-ron-kind"], serde_json::json!("bytes"));
    assert_eq!(
        defs["OptionKind"]["x-ron-kind"],
        serde_json::json!("option")
    );
    assert_eq!(
        defs["NonStringKeyMapKind"]["x-ron-kind"],
        serde_json::json!("non-string-key-map")
    );
    assert_eq!(
        defs["FlaggedStruct"]["x-ron-implicit-some"],
        serde_json::json!(true)
    );
    assert_eq!(
        defs["FlaggedStruct"]["x-ron-unwrap-newtypes"],
        serde_json::json!(true)
    );
    assert_eq!(
        defs["FlaggedStruct"]["x-ron-unwrap-variant-newtypes"],
        serde_json::json!(true)
    );
}
