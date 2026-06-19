//! Two-tier round-trip + JSONC snapshot tests for RON→JSON (E010 US1 — T007,
//! FR-011, SC-001).
//!
//! The **FR-011 round-trip oracle** is semantic value-tree equality (ignoring pure
//! formatting) plus comment text + anchor. The inverse direction (JSON→RON) is the
//! US2 cluster; until it lands, this file:
//!
//! * **snapshots the deterministic JSONC output** (`insta`) for a representative
//!   base-tier + expanded-tier corpus, so a change in the emitted bytes is caught;
//! * **asserts the loss report** (kinds + counts) — the half of the round-trip
//!   oracle observable from RON→JSON alone;
//! * **asserts the value mapping is faithful** (the JSON value matches the expected
//!   shape), so the RON→JSON half of the round trip is verified now;
//! * leaves the **full RON→JSON→RON value-equality** assertions as `#[ignore]`-with-
//!   reason, to be unignored in US2 once `json_to_ron` exists.
//!
//! Snapshots key on the JSONC text + the loss kinds, NOT on detail wording (plan
//! "Snapshot vs assertion scope").

use ronin_app::interop::{
    json_to_ron, render_json, ron_to_json, CommentMode, JsonToRonBinding, JsoncStyle, LossKind,
    RonToJson,
};
use ronin_core::parse;
use ronin_types::model::{
    Discriminator, Field, NodeKind, Primitive, TypeModel, TypeNode, TypeRef, Variant, VariantShape,
};

/// Convert `src` RON→JSON (unbound) and render the JSONC output text.
fn jsonc(src: &str) -> String {
    let doc = parse(src);
    let r = ron_to_json(&doc, None, CommentMode::JsoncInline);
    render_json(&r.value, &r.comments, 2, JsoncStyle::Jsonc)
}

/// Convert `src` and return the full result (value + loss report + carrier).
fn convert(src: &str) -> RonToJson {
    let doc = parse(src);
    ron_to_json(&doc, None, CommentMode::JsoncInline)
}

// ===========================================================================
// Base-tier (type-agnostic, always round-trip-safe) — no losses, JSONC snapshot.
// ===========================================================================

#[test]
fn base_tier_scalars_and_collections_have_no_losses() {
    // Scalars, a string-keyed map, a list, nested structs: the base tier (FR-011).
    let src = "(name: \"hero\", level: 3, alive: true, scores: [10, 20], meta: {\"k\": \"v\"})";
    let r = convert(src);
    assert!(
        r.loss_report.is_empty(),
        "base-tier document must report no losses: {:?}",
        r.loss_report
            .constructs()
            .iter()
            .map(|c| c.kind())
            .collect::<Vec<_>>()
    );
}

#[test]
fn snapshot_base_tier_jsonc_with_comments() {
    // A base-tier doc with comments → deterministic JSONC the snapshot pins.
    let src = "// player config\n(\n  name: \"hero\",\n  // the starting level\n  level: 3,\n  scores: [10, 20],\n)";
    insta::assert_snapshot!("base_tier_jsonc", jsonc(src));
}

#[test]
fn snapshot_base_tier_string_keyed_map() {
    let src = "(settings: {\"volume\": 80, \"mute\": false})";
    insta::assert_snapshot!("base_tier_string_map", jsonc(src));
}

// ===========================================================================
// Expanded-tier (type-bound) constructs — reported as losses; JSONC snapshot of
// the emit conventions (external tag, canonical key literal, tuple→array).
// ===========================================================================

#[test]
fn snapshot_expanded_tier_emit_conventions() {
    // A tuple, a char, a named enum variant (external-tag default), and a
    // non-string key (canonical literal): the expanded-tier emit conventions.
    let src = "(pos: (1, 2), initial: 'A', state: Running(5), keyed: {7: \"x\"})";
    insta::assert_snapshot!("expanded_tier_jsonc", jsonc(src));
}

#[test]
fn expanded_tier_reports_each_construct_as_a_loss() {
    let r = convert("(pos: (1, 2), initial: 'A', state: Running(5), keyed: {7: \"x\"})");
    assert_eq!(r.loss_report.count_of(LossKind::TupleVsList), 1);
    assert_eq!(r.loss_report.count_of(LossKind::Char), 1);
    assert_eq!(r.loss_report.count_of(LossKind::EnumTagging), 1);
    assert_eq!(r.loss_report.count_of(LossKind::NonStringKey), 1);
    assert!(
        r.loss_report.requires_confirmation(),
        "an expanded-tier conversion is lossy → requires confirm"
    );
}

// ===========================================================================
// RON→JSON value-tree fidelity (the RON→JSON half of the round-trip oracle).
// ===========================================================================

#[test]
fn value_mapping_is_faithful_for_the_base_tier() {
    let r = convert("(name: \"hero\", level: 3, scores: [10, 20])");
    assert_eq!(
        r.value,
        serde_json::json!({ "name": "hero", "level": 3, "scores": [10, 20] }),
        "base-tier value maps faithfully"
    );
}

#[test]
fn value_mapping_applies_emit_conventions_for_the_expanded_tier() {
    let r = convert("(pos: (1, 2), state: Running(5))");
    assert_eq!(r.value.get("pos"), Some(&serde_json::json!([1, 2])));
    assert_eq!(
        r.value.get("state"),
        Some(&serde_json::json!({ "Running": 5 })),
        "external-tag is the unbound default (FR-015)"
    );
}

// ===========================================================================
// Full RON→JSON→RON value-equality (the FR-011 oracle) — now that the US2
// inverse (json_to_ron) exists. The oracle is semantic value-tree equality
// ignoring pure formatting: we compare the re-parsed `ron::Value` of the original
// RON against the re-parsed `ron::Value` of the round-tripped RON (the serde `ron`
// crate is the grammar verifier / cross-check, ADR-0008). For the expanded tier we
// additionally assert the bound path recovered the RON-specific SHAPE in the text.
// ===========================================================================

/// The re-parsed `ron::Value` of `text` — the semantic value tree the FR-011 oracle
/// compares (ignores pure formatting; the serde `ron` crate is the grammar verifier).
fn ron_value(text: &str) -> ron::Value {
    ron::from_str(text).unwrap_or_else(|e| panic!("RON must parse for the oracle: {e}\n{text}"))
}

#[test]
fn base_tier_round_trips_value_stable() {
    // Base tier (type-agnostic, always safe): scalars, a list, a string-keyed map,
    // nested structs, and a comment. RON→JSON (unbound) → JSON→RON (unbound) must be
    // value-stable per the FR-011 oracle (struct/map and tuple/list collapse in the
    // value tree; comments preserved via JSONC).
    let original =
        "(name: \"hero\", level: 3, alive: true, scores: [10, 20], meta: {\"k\": \"v\"})";
    let doc = parse(original);

    // RON→JSON (unbound) — base tier is loss-free.
    let forward = ron_to_json(&doc, None, CommentMode::JsoncInline);
    assert!(
        forward.loss_report.is_empty(),
        "base tier is loss-free: {:?}",
        forward
            .loss_report
            .constructs()
            .iter()
            .map(|c| c.kind())
            .collect::<Vec<_>>()
    );

    // JSON→RON (unbound) — reconstruct from the projected value + the carrier.
    let back = json_to_ron(&forward.value, None, Some(&forward.comments));

    // The FR-011 oracle: the re-parsed value trees are semantically equal.
    assert_eq!(
        ron_value(original),
        ron_value(&back.text),
        "base-tier RON→JSON→RON is value-stable; round-tripped:\n{}",
        back.text
    );
}

#[test]
fn expanded_tier_round_trips_value_stable_when_bound() {
    // Expanded tier (safe only when a TypeModel is bound): a tuple, a char, a named
    // enum variant, an Option, and a non-string-key map. RON→JSON (bound) → JSON→RON
    // (bound) must recover each RON-specific shape and be value-stable.
    let original =
        "(pos: (1, 2), initial: 'A', state: Running(5), maybe: Some(9), keyed: {7: \"x\"})";
    let doc = parse(original);

    // Build the bound TypeModel mirroring the document's shapes.
    let mut model = TypeModel::new();
    model.insert_named(
        "Pos2",
        TypeNode::tuple(vec![
            TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
            TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
        ]),
    );
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
    model.insert_named(
        "Keyed",
        TypeNode::non_string_key_map(
            TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
            TypeRef::inline(TypeNode::primitive(Primitive::String)),
        ),
    );
    model.insert_named(
        "Root",
        TypeNode::new(NodeKind::Object {
            fields: vec![
                Field {
                    serialized_key: "pos".into(),
                    value: TypeRef::named("Pos2"),
                    optional: false,
                    flatten: false,
                },
                Field {
                    serialized_key: "initial".into(),
                    value: TypeRef::inline(TypeNode::char_()),
                    optional: false,
                    flatten: false,
                },
                Field {
                    serialized_key: "state".into(),
                    value: TypeRef::named("State"),
                    optional: false,
                    flatten: false,
                },
                Field {
                    serialized_key: "maybe".into(),
                    value: TypeRef::inline(TypeNode::option(TypeRef::inline(TypeNode::primitive(
                        Primitive::Integer,
                    )))),
                    optional: true,
                    flatten: false,
                },
                Field {
                    serialized_key: "keyed".into(),
                    value: TypeRef::named("Keyed"),
                    optional: false,
                    flatten: false,
                },
            ],
            deny_unknown_fields: false,
        }),
    );

    // RON→JSON (bound): enum tagging is driven by the bound type.
    let forward = ron_to_json(
        &doc,
        Some(ronin_app::interop::RonToJsonBinding::new(&model, "Root")),
        CommentMode::JsoncInline,
    );

    // JSON→RON (bound): recover each RON-specific shape from the bound type.
    let back = json_to_ron(
        &forward.value,
        Some(JsonToRonBinding::new(&model, "Root")),
        Some(&forward.comments),
    );

    // The bound path recovered each RON-specific SHAPE in the reconstructed text.
    assert!(
        back.text.contains("pos: (1, 2)"),
        "tuple recovered: {}",
        back.text
    );
    assert!(
        back.text.contains("initial: 'A'"),
        "char recovered: {}",
        back.text
    );
    assert!(
        back.text.contains("state: Running(5)"),
        "enum variant recovered: {}",
        back.text
    );
    assert!(
        back.text.contains("maybe: Some(9)"),
        "Option recovered: {}",
        back.text
    );
    assert!(
        back.text.contains("7: \"x\""),
        "typed int key recovered: {}",
        back.text
    );

    // The FR-011 oracle: the re-parsed value trees are semantically equal (char and
    // enum-variant identity ARE distinguished by ron::Value; tuple/list and
    // struct/map collapse, which the oracle permits as non-semantic).
    assert_eq!(
        ron_value(original),
        ron_value(&back.text),
        "expanded-tier bound RON→JSON→RON is value-stable; round-tripped:\n{}",
        back.text
    );
}
