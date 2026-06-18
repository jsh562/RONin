//! Snapshot of the serialized interchange shape {TR-001} [COMPLETES TR-001].
//!
//! `insta` snapshot of a populated [`TypeModel`] serialized to the
//! JSON-Schema-2020-12-shaped interchange. The committed `.snap` file is the
//! stable wire-contract reference: any change to the interchange shape (key
//! ordering, `$defs` layout, `x-ron-*` keywords, `$schema` dialect) shows up as
//! a snapshot diff, so the serialized form a future validator (E006) / WASM core
//! consumes cannot drift silently.

use ronin_types::model::{
    Discriminator, Field, NodeKind, Primitive, TypeModel, TypeNode, TypeRef, Variant, VariantShape,
};
use ronin_types::serialize::to_json_string;

/// Build a model that exercises the full interchange surface: a struct (with a
/// required + optional field, `deny_unknown_fields`, a char field), a tuple, an
/// enum with all four variant shapes + an adjacent discriminator, a sequence, a
/// non-string-key map, an `Option`, and an explicit `unknown` node, plus
/// model-level active extensions.
fn populated_model() -> TypeModel {
    let mut model = TypeModel::new();
    model.add_active_extension("implicit_some");
    model.add_active_extension("unwrap_newtypes");

    // A struct referencing other named types.
    model.insert_named(
        "Entity",
        TypeNode::new(NodeKind::Object {
            fields: vec![
                Field {
                    serialized_key: "id".into(),
                    value: TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
                    optional: false,
                    flatten: false,
                },
                Field {
                    serialized_key: "name".into(),
                    value: TypeRef::inline(TypeNode::primitive(Primitive::String)),
                    optional: true,
                    flatten: false,
                },
                Field {
                    serialized_key: "kind".into(),
                    value: TypeRef::named("Kind"),
                    optional: false,
                    flatten: false,
                },
                Field {
                    serialized_key: "position".into(),
                    value: TypeRef::named("Coord"),
                    optional: false,
                    flatten: false,
                },
                Field {
                    serialized_key: "initial".into(),
                    value: TypeRef::inline(TypeNode::char_()),
                    optional: false,
                    flatten: false,
                },
            ],
            deny_unknown_fields: true,
        }),
    );

    // A fixed-arity tuple (x-ron tuple + prefixItems).
    model.insert_named(
        "Coord",
        TypeNode::tuple(vec![
            TypeRef::inline(TypeNode::primitive(Primitive::Number)),
            TypeRef::inline(TypeNode::primitive(Primitive::Number)),
        ]),
    );

    // An enum exercising all variant shapes + adjacent tagging.
    model.insert_named(
        "Kind",
        TypeNode::new(NodeKind::Enum {
            variants: vec![
                Variant {
                    serialized_name: "None".into(),
                    shape: VariantShape::Unit,
                },
                Variant {
                    serialized_name: "Id".into(),
                    shape: VariantShape::Newtype(TypeRef::inline(TypeNode::primitive(
                        Primitive::Integer,
                    ))),
                },
                Variant {
                    serialized_name: "Pair".into(),
                    shape: VariantShape::Tuple(vec![
                        TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
                        TypeRef::inline(TypeNode::char_()),
                    ]),
                },
                Variant {
                    serialized_name: "Named".into(),
                    shape: VariantShape::Struct(vec![Field {
                        serialized_key: "label".into(),
                        value: TypeRef::inline(TypeNode::primitive(Primitive::String)),
                        optional: false,
                        flatten: false,
                    }]),
                },
            ],
            discriminator: Discriminator::Adjacent {
                tag: "type".into(),
                content: "data".into(),
            },
        }),
    );

    // A sequence, an option, a non-string-key map, and an unknown node.
    model.insert_named(
        "Tags",
        TypeNode::new(NodeKind::Sequence {
            element: TypeRef::inline(TypeNode::primitive(Primitive::String)),
        }),
    );
    model.insert_named(
        "MaybeId",
        TypeNode::option(TypeRef::inline(TypeNode::primitive(Primitive::Integer))),
    );
    model.insert_named(
        "Lookup",
        TypeNode::non_string_key_map(
            TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
            TypeRef::named("Entity"),
        ),
    );
    model.insert_named("Foreign", TypeNode::unknown());

    model
}

/// The serialized interchange of a populated model is byte-stable (snapshot).
#[test]
fn interchange_shape_is_stable() {
    let model = populated_model();
    let json = to_json_string(&model);
    insta::assert_snapshot!("populated_type_model", json);
}
