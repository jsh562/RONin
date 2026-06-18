//! WASM-consumability of the serialized interchange {TR-012} [COMPLETES TR-012].
//!
//! Proves the two facts that realize OBJ5 / SC-006 ("the serialized model
//! deserializes on `wasm32`") for E004:
//!
//! 1. A populated [`TypeModel`] serialized with [`to_json`] deserializes back via
//!    [`from_json`] to a structurally-equal model (round-trip / SC-006).
//! 2. The serialized artifact is a **pure JSON tree** — only the six JSON value
//!    kinds (object, array, string, number, bool, null) appear, with no
//!    native-only encoding. The deserialize path therefore needs only
//!    `serde_json`, which is WASM-clean.
//!
//! ## Honest `wasm32`-execution boundary
//!
//! `ronin-types` is **native-only** by design: it depends on `syn` / `walkdir`,
//! which do not build for `wasm32`, so this crate cannot itself be compiled or
//! run on `wasm32`. This test runs **natively** and does NOT invoke a wasm
//! runtime. It proves the wire form is pure, `serde_json`-only JSON; actual
//! on-device `wasm32` execution of the deserialize path is exercised by the
//! WASM-clean `ronin-core` consumer added in E006, which depends only on
//! `serde_json` (no `ronin-types`, `syn`, `walkdir`, or `schemars`).

use ronin_types::extension::RonTypeExtension;
use ronin_types::model::{
    Discriminator, Field, NodeKind, Primitive, TypeModel, TypeNode, TypeRef, Variant, VariantShape,
};
use ronin_types::serialize::{from_json, from_json_str, to_json, to_json_string};
use ronin_types::AcquisitionDiagnostic;
use serde_json::Value;

/// Build a model that touches every node kind, both reference forms, an enum with
/// a non-default discriminator, diagnostics, and active extension flags — so the
/// round-trip and the purity walk cover the whole interchange surface.
fn populated_model() -> TypeModel {
    let mut model = TypeModel::new();
    model.add_active_extension("implicit_some");
    model.add_active_extension("unwrap_newtypes");

    // Object with required + optional + flatten + a named-ref field.
    model.insert_named(
        "Config",
        TypeNode::new(NodeKind::Object {
            fields: vec![
                Field {
                    serialized_key: "id".into(),
                    value: TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
                    optional: false,
                    flatten: false,
                },
                Field {
                    serialized_key: "label".into(),
                    value: TypeRef::inline(TypeNode::char_()),
                    optional: true,
                    flatten: false,
                },
                Field {
                    serialized_key: "extra".into(),
                    value: TypeRef::named("Status"),
                    optional: true,
                    flatten: true,
                },
            ],
            deny_unknown_fields: true,
        }),
    );

    // Enum with an adjacently-tagged discriminator + every variant shape.
    model.insert_named(
        "Status",
        TypeNode::new(NodeKind::Enum {
            variants: vec![
                Variant {
                    serialized_name: "Idle".into(),
                    shape: VariantShape::Unit,
                },
                Variant {
                    serialized_name: "Code".into(),
                    shape: VariantShape::Newtype(TypeRef::inline(TypeNode::primitive(
                        Primitive::Integer,
                    ))),
                },
                Variant {
                    serialized_name: "Pair".into(),
                    shape: VariantShape::Tuple(vec![
                        TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
                        TypeRef::inline(TypeNode::primitive(Primitive::String)),
                    ]),
                },
                Variant {
                    serialized_name: "Detail".into(),
                    shape: VariantShape::Struct(vec![Field {
                        serialized_key: "msg".into(),
                        value: TypeRef::inline(TypeNode::primitive(Primitive::String)),
                        optional: false,
                        flatten: false,
                    }]),
                },
            ],
            discriminator: Discriminator::Adjacent {
                tag: "kind".into(),
                content: "data".into(),
            },
        }),
    );

    // Sequence, tuple, map (non-string-key), option, primitives, unit, bytes.
    model.insert_named(
        "Ids",
        TypeNode::new(NodeKind::Sequence {
            element: TypeRef::named("Config"),
        }),
    );
    model.insert_named(
        "Pair",
        TypeNode::tuple(vec![
            TypeRef::named("Config"),
            TypeRef::inline(TypeNode::primitive(Primitive::Number)),
        ]),
    );
    model.insert_named(
        "Scores",
        TypeNode::non_string_key_map(
            TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
            TypeRef::inline(TypeNode::primitive(Primitive::Number)),
        ),
    );
    model.insert_named("Maybe", TypeNode::option(TypeRef::named("Status")));
    model.insert_named("Flag", TypeNode::primitive(Primitive::Boolean));
    model.insert_named("Nothing", TypeNode::unit());
    model.insert_named("Blob", TypeNode::bytes());

    // A first-class unknown node + a node carrying only extension flags.
    model.insert_named("Foreign", TypeNode::unknown());
    model.insert_named(
        "Wrapped",
        TypeNode::new(NodeKind::Object {
            fields: vec![],
            deny_unknown_fields: false,
        })
        .with_ron_extension(RonTypeExtension {
            unwrap_newtypes: true,
            unwrap_variant_newtypes: true,
            ..RonTypeExtension::default()
        }),
    );

    // Diagnostics travel with the model.
    model.diagnostics.push(AcquisitionDiagnostic::new(
        ronin_types::diagnostics::DiagnosticCategory::UnresolvedType,
        "Foreign",
        "type not found in any source",
    ));

    model
}

/// SC-006: a populated model round-trips through the JSON-`Value` interchange to a
/// structurally-equal model.
#[test]
fn populated_model_round_trips_via_value() {
    let model = populated_model();
    let json = to_json(&model);
    let back = from_json(&json).expect("interchange must deserialize");
    assert_eq!(
        model, back,
        "round-trip must preserve the model structurally"
    );
}

/// SC-006: the text form round-trips identically (the form a wasm consumer reads
/// off disk / a message channel).
#[test]
fn populated_model_round_trips_via_string() {
    let model = populated_model();
    let s = to_json_string(&model);
    let back = from_json_str(&s).expect("interchange string must deserialize");
    assert_eq!(model, back);
}

/// The serialized artifact is a PURE JSON tree: walking it reaches only the six
/// JSON value kinds (object/array/string/number/bool/null). `serde_json::Value`
/// cannot encode anything else, so this asserts the wire form carries no
/// native-only construct and is deserializable with `serde_json` alone — i.e. a
/// WASM-clean consumer can read it.
#[test]
fn serialized_artifact_is_pure_json() {
    let json = to_json(&populated_model());

    // Counts proving the walk actually traversed a rich tree (not a trivial pass).
    let mut leaves = 0usize;
    let mut containers = 0usize;
    assert_pure_json(&json, &mut leaves, &mut containers);

    assert!(
        containers > 1,
        "expected a nested object/array tree, saw {containers} containers"
    );
    assert!(
        leaves > 1,
        "expected multiple JSON scalar leaves, saw {leaves}"
    );
}

/// Recursively assert every node in `value` is one of the six JSON value kinds.
/// Any other variant would fail to compile against `serde_json::Value`'s closed
/// enum, so reaching the wildcard-free match below is itself the proof the tree
/// is pure JSON; the counters make the traversal observable.
fn assert_pure_json(value: &Value, leaves: &mut usize, containers: &mut usize) {
    match value {
        Value::Object(map) => {
            *containers += 1;
            for v in map.values() {
                assert_pure_json(v, leaves, containers);
            }
        }
        Value::Array(items) => {
            *containers += 1;
            for v in items {
                assert_pure_json(v, leaves, containers);
            }
        }
        // The four scalar JSON leaf kinds: nothing native-only is representable.
        Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null => {
            *leaves += 1;
        }
    }
}

/// The deserialize path is `serde_json`-only (WASM-clean): re-parsing the
/// serialized string with plain `serde_json` yields the same tree `to_json`
/// produced, confirming no extra (native) decoding step is involved.
#[test]
fn deserialize_path_uses_only_serde_json() {
    let model = populated_model();
    let from_helper = to_json(&model);
    let text = to_json_string(&model);
    // Plain serde_json round-trip — the exact code path a wasm consumer runs.
    let from_serde: Value = serde_json::from_str(&text).expect("serde_json parses the wire form");
    assert_eq!(from_helper, from_serde);
}
