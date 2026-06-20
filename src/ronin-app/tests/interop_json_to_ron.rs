//! JSON→RON reconstruction tests — schema-aware (bound) vs deterministic
//! best-effort (unbound) over paired fixtures (E010 US2 — T018, FR-009/015, SC-004).
//!
//! Each fixture feeds the SAME JSON to the converter twice — once with a bound
//! `TypeModel` (the schema-aware path) and once unbound (the best-effort path) — and
//! asserts the two diverge exactly per FR-015 / SC-004: the bound path recovers the
//! RON-specific shape (tuple / named variant / char / Option / typed key), the
//! unbound path applies the documented default (list / external-tag / string key)
//! and surfaces the residual ambiguity. Assertions key on the reconstructed RON
//! TEXT shape (and the serde `ron` grammar cross-check), never on detail wording.

use ronin_app::interop::{json_to_ron, JsonToRonBinding};
use ronin_types::extension::{RonKind, RonTypeExtension};
use ronin_types::model::{
    Discriminator, Field, NodeKind, Primitive, TypeModel, TypeNode, TypeRef, Variant, VariantShape,
};

/// Reconstruct `json` unbound (best-effort) and return the RON text + notes.
fn unbound(json: serde_json::Value) -> (String, Vec<String>) {
    let r = json_to_ron(&json, None, None);
    assert!(
        ronin_app::interop::json_to_ron::grammar_verify(&r.text),
        "unbound output must be grammar-valid RON: {}",
        r.text
    );
    (r.text, r.notes)
}

/// Reconstruct `json` bound to `(model, root)` and return the RON text + notes.
fn bound(json: serde_json::Value, model: &TypeModel, root: &str) -> (String, Vec<String>) {
    let r = json_to_ron(&json, Some(JsonToRonBinding::new(model, root)), None);
    assert!(
        ronin_app::interop::json_to_ron::grammar_verify(&r.text),
        "bound output must be grammar-valid RON: {}",
        r.text
    );
    (r.text, r.notes)
}

/// A root struct with one field `pos` of the given type.
fn root_with_field(field: &str, value: TypeRef) -> TypeNode {
    TypeNode::new(NodeKind::Object {
        fields: vec![Field {
            serialized_key: field.into(),
            value,
            optional: false,
            flatten: false,
        }],
        deny_unknown_fields: false,
    })
}

// ===========================================================================
// 2-tuple: bound → tuple by arity; unbound → list + ambiguity note (SC-004).
// ===========================================================================

#[test]
fn two_tuple_bound_vs_unbound() {
    let json = serde_json::json!({ "pos": [1, 2] });

    // Unbound: an array reconstructs as a RON list, with the ambiguity noted.
    let (text, notes) = unbound(json.clone());
    assert!(text.contains("pos: [1, 2]"), "unbound → list: {text}");
    assert!(
        notes.iter().any(|n| n.contains("could be a tuple")),
        "unbound tuple-vs-list ambiguity is surfaced: {notes:?}"
    );

    // Bound to a 2-tuple type: the array reconstructs as a RON tuple by arity.
    let mut model = TypeModel::new();
    model.insert_named(
        "Pos2",
        TypeNode::tuple(vec![
            TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
            TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
        ]),
    );
    model.insert_named("Root", root_with_field("pos", TypeRef::named("Pos2")));
    let (text, _) = bound(json, &model, "Root");
    assert!(
        text.contains("pos: (1, 2)"),
        "bound → tuple by arity: {text}"
    );
}

// ===========================================================================
// Enum tagging: external / internal / adjacent / untagged (SC-004, FR-015).
// ===========================================================================

#[test]
fn external_tagged_enum_bound_vs_unbound() {
    // Unbound: a single-key `{"Running": 5}` object is best-effort external-tag.
    let (text, notes) = unbound(serde_json::json!({ "Running": 5 }));
    assert_eq!(text.trim(), "Running(5)", "unbound external-tag default");
    assert!(
        notes.iter().any(|n| n.contains("externally-tagged")),
        "unbound external-tag assumption is noted: {notes:?}"
    );

    // Bound: external tagging recovers the named newtype variant payload.
    let mut model = TypeModel::new();
    model.insert_named(
        "State",
        TypeNode::new(NodeKind::Enum {
            variants: vec![Variant {
                serialized_name: "Running".into(),
                shape: VariantShape::Newtype(TypeRef::inline(TypeNode::primitive(
                    Primitive::Integer,
                ))),
            }],
            discriminator: Discriminator::External,
        }),
    );
    let (text, _) = bound(serde_json::json!({ "Running": 5 }), &model, "State");
    assert_eq!(text.trim(), "Running(5)", "bound external variant");
}

#[test]
fn internal_tagged_enum_recovers_variant() {
    // {"type": "Circle", "r": 2} bound to an internally-tagged struct variant →
    // Circle(r: 2) (the tag field is consumed, FR-015).
    let mut model = TypeModel::new();
    model.insert_named(
        "Shape",
        TypeNode::new(NodeKind::Enum {
            variants: vec![Variant {
                serialized_name: "Circle".into(),
                shape: VariantShape::Struct(vec![Field {
                    serialized_key: "r".into(),
                    value: TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
                    optional: false,
                    flatten: false,
                }]),
            }],
            discriminator: Discriminator::Internal { tag: "type".into() },
        }),
    );
    let json = serde_json::json!({ "type": "Circle", "r": 2 });
    let (text, _) = bound(json, &model, "Shape");
    assert!(
        text.contains("Circle("),
        "internal-tag → named variant: {text}"
    );
    assert!(text.contains("r: 2"), "tag stripped, payload kept: {text}");
    assert!(!text.contains("type:"), "the tag field is consumed: {text}");
}

#[test]
fn adjacent_tagged_enum_recovers_variant() {
    // {"t": "Ping", "c": 7} bound to an adjacently-tagged newtype variant → Ping(7).
    let mut model = TypeModel::new();
    model.insert_named(
        "Msg",
        TypeNode::new(NodeKind::Enum {
            variants: vec![Variant {
                serialized_name: "Ping".into(),
                shape: VariantShape::Newtype(TypeRef::inline(TypeNode::primitive(
                    Primitive::Integer,
                ))),
            }],
            discriminator: Discriminator::Adjacent {
                tag: "t".into(),
                content: "c".into(),
            },
        }),
    );
    let json = serde_json::json!({ "t": "Ping", "c": 7 });
    let (text, _) = bound(json, &model, "Msg");
    assert_eq!(text.trim(), "Ping(7)", "adjacent-tag → Ping(7)");
}

#[test]
fn untagged_enum_is_best_effort_with_note() {
    // An untagged enum has no tag to recover the variant from → best-effort + note.
    let mut model = TypeModel::new();
    model.insert_named(
        "U",
        TypeNode::new(NodeKind::Enum {
            variants: vec![Variant {
                serialized_name: "N".into(),
                shape: VariantShape::Newtype(TypeRef::inline(TypeNode::primitive(
                    Primitive::Integer,
                ))),
            }],
            discriminator: Discriminator::Untagged,
        }),
    );
    let r = json_to_ron(
        &serde_json::json!(5),
        Some(JsonToRonBinding::new(&model, "U")),
        None,
    );
    assert!(
        r.notes.iter().any(|n| n.contains("untagged")),
        "untagged reconstruction notes the ambiguity: {:?}",
        r.notes
    );
    assert!(
        ronin_app::interop::json_to_ron::grammar_verify(&r.text),
        "untagged best-effort output still parses: {}",
        r.text
    );
}

// ===========================================================================
// char: bound → 'x'; unbound → "x" (string) (SC-004).
// ===========================================================================

#[test]
fn char_bound_vs_unbound() {
    let json = serde_json::json!({ "c": "x" });

    // Unbound: a one-character string stays a RON string.
    let (text, _) = unbound(json.clone());
    assert!(text.contains("c: \"x\""), "unbound → string: {text}");

    // Bound to a char field: the one-character string re-types to a RON char.
    let mut model = TypeModel::new();
    model.insert_named(
        "Root",
        root_with_field("c", TypeRef::inline(TypeNode::char_())),
    );
    let (text, _) = bound(json, &model, "Root");
    assert!(text.contains("c: 'x'"), "bound → char: {text}");
}

// ===========================================================================
// Option: bound → None / Some(v); unbound null → None (SC-004).
// ===========================================================================

#[test]
fn option_bound_recovers_none_and_some() {
    let mut model = TypeModel::new();
    model.insert_named(
        "Root",
        TypeNode::new(NodeKind::Object {
            fields: vec![
                Field {
                    serialized_key: "a".into(),
                    value: TypeRef::inline(TypeNode::option(TypeRef::inline(TypeNode::primitive(
                        Primitive::Integer,
                    )))),
                    optional: true,
                    flatten: false,
                },
                Field {
                    serialized_key: "b".into(),
                    value: TypeRef::inline(TypeNode::option(TypeRef::inline(TypeNode::primitive(
                        Primitive::Integer,
                    )))),
                    optional: true,
                    flatten: false,
                },
            ],
            deny_unknown_fields: false,
        }),
    );
    let json = serde_json::json!({ "a": null, "b": 5 });
    let (text, _) = bound(json, &model, "Root");
    assert!(text.contains("a: None"), "null → None: {text}");
    assert!(text.contains("b: Some(5)"), "value → Some(v): {text}");
}

// ===========================================================================
// Non-string-key map: bound → typed key re-parsed; unbound → string key + note.
// ===========================================================================

#[test]
fn non_string_key_map_bound_vs_unbound() {
    let json = serde_json::json!({ "7": "a", "9": "b" });

    // Unbound: the keys "7"/"9" are not valid RON idents, so the object
    // reconstructs as a string-keyed RON map (the base-tier round-trip-safe form);
    // the keys stay quoted strings (typed-key recovery only happens when bound).
    let (text, notes) = unbound(json.clone());
    assert!(
        text.contains("\"7\": \"a\""),
        "unbound → string-keyed map: {text}"
    );
    assert!(
        text.trim_start().starts_with('{'),
        "unbound → RON map: {text}"
    );
    assert!(
        notes.iter().any(|n| n.contains("non-identifier keys")),
        "unbound surfaces the struct-vs-map / typed-key ambiguity: {notes:?}"
    );

    // Bound to a non-string-key map (int keys): the canonical RON-literal keys
    // re-parse to typed integer keys (FR-015).
    let mut model = TypeModel::new();
    model.insert_named(
        "Keyed",
        TypeNode::non_string_key_map(
            TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
            TypeRef::inline(TypeNode::primitive(Primitive::String)),
        ),
    );
    let (text, _) = bound(json, &model, "Keyed");
    assert!(text.contains("7: \"a\""), "bound → typed int key 7: {text}");
    assert!(text.contains("9: \"b\""), "bound → typed int key 9: {text}");
    assert!(
        !text.contains("\"7\":"),
        "the key is not a quoted string: {text}"
    );
}

#[test]
fn tuple_key_map_reparses_canonical_literal() {
    // A canonical RON-literal tuple key "(1, 2)" bound to a non-string-key map
    // re-parses back to the typed tuple key (FR-015).
    let mut model = TypeModel::new();
    let key_node = TypeNode::tuple(vec![
        TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
        TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
    ]);
    model.insert_named(
        "Keyed",
        TypeNode::non_string_key_map(
            TypeRef::inline(key_node),
            TypeRef::inline(TypeNode::primitive(Primitive::String)),
        )
        .with_ron_extension(RonTypeExtension::kind(RonKind::NonStringKeyMap)),
    );
    let json = serde_json::json!({ "(1, 2)": "x" });
    let (text, _) = bound(json, &model, "Keyed");
    assert!(
        text.contains("(1, 2): \"x\""),
        "tuple key re-parses from the canonical literal: {text}"
    );
}

// ===========================================================================
// Unbound external-tag + string-key defaults are deterministic (SC-004).
// ===========================================================================

#[test]
fn unbound_defaults_are_deterministic() {
    // Same input twice → identical output (deterministic best-effort, FR-009).
    let json = serde_json::json!({ "Variant": [1, 2], "kv": { "k": "v" } });
    let a = json_to_ron(&json, None, None);
    let b = json_to_ron(&json, None, None);
    assert_eq!(a.text, b.text, "unbound reconstruction is deterministic");
    assert_eq!(a.notes, b.notes, "the notes are deterministic too");
}
