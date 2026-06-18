//! Sub-tree validation tests (E009/T012 — FR-005, FR-006/FR-016, FR-017).
//!
//! Exercises the generic, **Bevy-agnostic** [`validate_subtree_against_type`]
//! entry: validating ONE CST value sub-node against ONE named model type. These
//! tests are the engine-level oracle for the scene interpreter (which lives in
//! `ronin-app`); nothing here references a Bevy/scene/registry symbol, mirroring
//! the crate's WASM-clean, generic contract.
//!
//! Each fixture embeds the component value **deep inside a larger document**
//! (after multibyte text), then navigates to that sub-node and validates it
//! against a `TypeModel`-shaped `$defs`. Because rowan nodes carry absolute byte
//! offsets, the diagnostics must land at the offending construct's precise
//! **full-document** byte range — verified here against an exact source oracle.

use ronin_core::{parse, DiagnosticCode, SyntaxKind, SyntaxNode};
use ronin_validate::validate_subtree_against_type;
use serde_json::{json, Value};

/// Byte range `(start, end)` of the first occurrence of `needle` in `src`.
fn byte_span(src: &str, needle: &str) -> (usize, usize) {
    let s = src
        .find(needle)
        .unwrap_or_else(|| panic!("`{needle}` not in source"));
    (s, s + needle.len())
}

/// Find the first value-position sub-node (struct/tuple/enum-variant/...) whose
/// leading `Ident` token is `name`, anywhere under `node` — the test analogue of
/// "reach the component value for type path `name`". Walks the CST read-only.
fn find_named_value(node: &SyntaxNode, name: &str) -> Option<SyntaxNode> {
    if matches!(
        node.kind(),
        SyntaxKind::Struct | SyntaxKind::Tuple | SyntaxKind::EnumVariant
    ) && node
        .first_token_of(SyntaxKind::Ident)
        .is_some_and(|t| t.text() == name)
    {
        return Some(node.clone());
    }
    for child in node.children() {
        if let Some(found) = find_named_value(&child, name) {
            return Some(found);
        }
    }
    None
}

/// Parse `src`, locate the value sub-node named `name`, and validate it against
/// `type_name` in `model`.
fn validate_component(
    src: &str,
    name: &str,
    model: &Value,
    type_name: &str,
) -> Vec<ronin_core::Diagnostic> {
    let doc = parse(src);
    let subtree = find_named_value(&doc.root(), name)
        .unwrap_or_else(|| panic!("sub-node `{name}` not found in source"));
    validate_subtree_against_type(model, type_name, &subtree)
}

/// The example scene-ish document: a wrapper struct carrying a multibyte field
/// then a `Health` component value, so the component's byte offset differs from
/// its char offset and full-document coordinates are unambiguous.
fn health_model() -> Value {
    json!({
        "$defs": {
            "Health": {
                "type": "object",
                "properties": {
                    "hp": { "type": "integer" },
                    "max": { "type": "integer" }
                },
                "required": ["hp", "max"],
                "additionalProperties": false
            }
        }
    })
}

#[test]
fn matching_subtree_reports_no_findings() {
    let src = "Entity(label: \"café-η\", comp: Health(hp: 10, max: 20))";
    let diags = validate_component(src, "Health", &health_model(), "Health");
    assert!(
        diags.is_empty(),
        "a matching sub-tree must report nothing, got: {:?}",
        diags
            .iter()
            .map(|d| (d.code().code(), d.message().to_owned()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn wrong_field_type_flagged_at_value_span_in_full_document_coords() {
    // `hp` expects integer; a string is given inside a nested component.
    let src = "Entity(label: \"café-η\", comp: Health(hp: \"oops\", max: 20))";
    let diags = validate_component(src, "Health", &health_model(), "Health");
    let mut hits: Vec<_> = diags
        .iter()
        .filter(|d| d.code() == DiagnosticCode::TypeMismatch)
        .collect();
    assert_eq!(hits.len(), 1, "expected one TypeMismatch, got: {diags:?}");
    let d = hits.remove(0);
    let span = byte_span(src, "\"oops\"");
    assert_eq!(
        (d.range().start(), d.range().end()),
        span,
        "type mismatch must point at the offending value in full-document coords"
    );
    // The leading multibyte text guarantees the byte offset is shifted from any
    // sub-tree-local offset, proving absolute (full-document) coordinates.
    assert!(
        span.0 > 30,
        "offending value sits well past the document start"
    );
}

#[test]
fn unknown_field_name_flagged_at_key_span() {
    // `additionalProperties: false` makes `mana` an unknown field of `Health`.
    let src = "Entity(label: \"αβγ\", comp: Health(hp: 1, max: 2, mana: 3))";
    let diags = validate_component(src, "Health", &health_model(), "Health");
    let mut hits: Vec<_> = diags
        .iter()
        .filter(|d| d.code() == DiagnosticCode::UnknownField)
        .collect();
    assert_eq!(hits.len(), 1, "expected one UnknownField, got: {diags:?}");
    let d = hits.remove(0);
    let span = byte_span(src, "mana");
    assert_eq!(
        (d.range().start(), d.range().end()),
        span,
        "unknown-field finding must point at the field-name key span"
    );
}

#[test]
fn wrong_enum_variant_flagged_at_value_span() {
    let model = json!({
        "$defs": {
            "Mode": {
                "oneOf": [
                    { "x-ron-variant": "On", "x-ron-variant-shape": "unit" },
                    { "x-ron-variant": "Off", "x-ron-variant-shape": "unit" }
                ]
            }
        }
    });
    // The component value is itself the enum (a bare variant ident).
    let src = "Entity(label: \"日本語\", comp: Whoops)";
    // `Whoops` is an EnumVariant value node; locate it by ident.
    let diags = validate_component(src, "Whoops", &model, "Mode");
    let mut hits: Vec<_> = diags
        .iter()
        .filter(|d| d.code() == DiagnosticCode::InvalidEnumVariant)
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "expected one InvalidEnumVariant, got: {diags:?}"
    );
    let d = hits.remove(0);
    let span = byte_span(src, "Whoops");
    assert_eq!(
        (d.range().start(), d.range().end()),
        span,
        "invalid-variant finding must point at the variant value span"
    );
}

#[test]
fn wrong_tuple_arity_flagged_at_tuple_span() {
    let model = json!({
        "$defs": {
            "Transform": {
                "type": "object",
                "properties": { "pos": { "$ref": "#/$defs/Vec2" } },
                "required": ["pos"]
            },
            "Vec2": {
                "type": "array",
                "prefixItems": [ { "type": "number" }, { "type": "number" } ],
                "items": false,
                "x-ron-kind": "tuple",
                "x-ron-tuple-arity": 2
            }
        }
    });
    // `pos` is a 2-tuple type but three elements are supplied.
    let src = "Entity(label: \"€uro\", comp: Transform(pos: (1.0, 2.0, 3.0)))";
    let diags = validate_component(src, "Transform", &model, "Transform");
    let mut hits: Vec<_> = diags
        .iter()
        .filter(|d| d.code() == DiagnosticCode::WrongTupleArity)
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "expected one WrongTupleArity, got: {diags:?}"
    );
    let d = hits.remove(0);
    let span = byte_span(src, "(1.0, 2.0, 3.0)");
    assert_eq!(
        (d.range().start(), d.range().end()),
        span,
        "tuple-arity finding must point at the offending tuple span"
    );
}

#[test]
fn unknown_type_is_unconstrained_no_findings() {
    // FR-006/FR-016: a type path not in the registry -> unconstrained, even where
    // the sub-tree would clearly violate some other type.
    let src = "Entity(label: \"x\", comp: Health(hp: \"oops\", max: 20))";
    let diags = validate_component(src, "Health", &health_model(), "NotRegistered");
    assert!(
        diags.is_empty(),
        "an absent/unknown type must be unconstrained, got: {diags:?}"
    );
}

#[test]
fn empty_or_null_model_yields_no_findings() {
    let src = "Entity(comp: Health(hp: \"oops\", max: 20))";
    let doc = parse(src);
    let subtree = find_named_value(&doc.root(), "Health").expect("Health sub-node");

    // `null` model and `{}` model are both structural-only.
    assert!(validate_subtree_against_type(&Value::Null, "Health", &subtree).is_empty());
    assert!(validate_subtree_against_type(&json!({}), "Health", &subtree).is_empty());
    // A model with no `$defs` is also unconstrained (no panic).
    assert!(
        validate_subtree_against_type(&json!({ "type": "object" }), "Health", &subtree).is_empty()
    );
}

#[test]
fn non_value_subtree_does_not_panic_and_reports_nothing() {
    // Passing a non-value node (the Root) must fail soft (FR-016): no panic, no
    // findings — the projection yields an empty instance.
    let doc = parse("Health(hp: \"oops\", max: 20)");
    let root = doc.root();
    let diags = validate_subtree_against_type(&health_model(), "Health", &root);
    assert!(
        diags.is_empty(),
        "a non-value sub-node must be unconstrained, got: {diags:?}"
    );
}
