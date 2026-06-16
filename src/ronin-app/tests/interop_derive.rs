//! Derive-from-type parse-conformance tests (E010 US3 — T025, FR-010,
//! [COMPLETES SC-005]).
//!
//! Two layers, mirroring the rest of the interop suite:
//!
//! * the **pure converter** [`derive_scaffold`] over hand-built `TypeModel`s — the
//!   scaffold parses cleanly + conforms to the type's field shape, partially-`unknown`
//!   types get the parseable sentinel + a "placeholder — fill in" diagnostic, and the
//!   output is deterministic (same `(type, model)` ⇒ identical scaffold) plus an
//!   `insta` snapshot of the canonical scaffold (SC-005);
//! * the **wired App command** [`App::derive_ron_from_type`] — a registered type
//!   opens a parseable scaffold in a NEW tab with the fill-in diagnostics published
//!   inline, while an unknown / unregistered type surfaces a clear "no type model
//!   available" message and creates **no** document (US3 AS1/AS2).

use std::sync::Arc;

use ronin_app::app::{App, NoticeKind};
use ronin_app::interop::{derive_scaffold, DeriveScaffold, UNKNOWN_SENTINEL};
use ronin_app::settings::AppSettings;

use ron_types::extension::RonKind;
use ron_types::model::{
    Discriminator, Field, NodeKind, Primitive, TypeModel, TypeNode, TypeRef, Variant, VariantShape,
};

// ===========================================================================
// Helpers
// ===========================================================================

/// A field with an inline value type and no serde flags.
fn field(key: &str, value: TypeNode) -> Field {
    Field {
        serialized_key: key.to_string(),
        value: TypeRef::inline(value),
        optional: false,
        flatten: false,
    }
}

/// A struct node from a list of fields.
fn struct_node(fields: Vec<Field>) -> TypeNode {
    TypeNode::new(NodeKind::Object {
        fields,
        deny_unknown_fields: false,
    })
}

/// A model with a single registered named type.
fn model_with(name: &str, node: TypeNode) -> TypeModel {
    let mut model = TypeModel::new();
    model.insert_named(name, node);
    model
}

/// A representative, mixed-kind config struct used across the parse-conformance and
/// snapshot tests (scalars, list, tuple, char, Option, unit, nested enum).
fn rich_config_model() -> TypeModel {
    let mut model = TypeModel::new();
    model.insert_named(
        "Mode",
        TypeNode::new(NodeKind::Enum {
            variants: vec![
                Variant {
                    serialized_name: "Fast".into(),
                    shape: VariantShape::Unit,
                },
                Variant {
                    serialized_name: "Slow".into(),
                    shape: VariantShape::Newtype(TypeRef::inline(TypeNode::primitive(
                        Primitive::Integer,
                    ))),
                },
            ],
            discriminator: Discriminator::External,
        }),
    );
    model.insert_named(
        "Config",
        struct_node(vec![
            field("count", TypeNode::primitive(Primitive::Integer)),
            field("ratio", TypeNode::primitive(Primitive::Number)),
            field("name", TypeNode::primitive(Primitive::String)),
            field("enabled", TypeNode::primitive(Primitive::Boolean)),
            field(
                "tags",
                TypeNode::new(NodeKind::Sequence {
                    element: TypeRef::inline(TypeNode::primitive(Primitive::String)),
                }),
            ),
            field(
                "pos",
                TypeNode::tuple(vec![
                    TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
                    TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
                ]),
            ),
            field("initial", TypeNode::char_()),
            field(
                "limit",
                TypeNode::option(TypeRef::inline(TypeNode::primitive(Primitive::Integer))),
            ),
            field("nothing", TypeNode::unit()),
            Field {
                serialized_key: "mode".into(),
                value: TypeRef::named("Mode"),
                optional: false,
                flatten: false,
            },
        ]),
    );
    model
}

/// Assert a scaffold parses with zero `ron_core` diagnostics (SC-005).
fn assert_parses(scaffold: &DeriveScaffold) {
    assert!(
        scaffold.document.diagnostics().is_empty(),
        "scaffold must parse cleanly, got diagnostics for:\n{}",
        scaffold.text
    );
}

/// Build an App with a registered type bound to its active document, so the derive
/// command can read it. The bound type is the **serialized** E004 interchange of
/// `model` (exactly the form a real binding carries), with `root` as the bound name.
fn app_with_bound_type(model: &TypeModel, root: &str) -> App {
    let mut app = App::new(AppSettings::default(), None);
    // Open an active (untitled) buffer to host the binding, then attach the serialized
    // type model + the bound root name — the type-pick surface the command consults.
    app.new_untitled();
    let serialized = ron_types::to_json(model);
    if let Some(doc) = app.active_document_mut() {
        doc.bound_type = Some(ronin_app::reparse::BoundType {
            model: Arc::new(serialized),
            type_name: root.to_string(),
        });
    }
    app
}

// ===========================================================================
// Parse-conformance + field-shape conformance (SC-005)
// ===========================================================================

#[test]
fn scaffold_parses_cleanly_and_conforms_to_field_shape() {
    // SC-005 / US3 AS1: the derived scaffold parses without error and contains every
    // field of the type's shape, with deterministic typed placeholders per kind.
    let model = rich_config_model();
    let s = derive_scaffold(&model, "Config");
    assert_parses(&s);

    // Every declared field is present with its typed placeholder.
    for needle in [
        "count: 0",
        "ratio: 0.0",
        "name: \"\"",
        "enabled: false",
        "tags: []",
        "pos: (0, 0)",
        "initial: '\\0'",
        "limit: None",
        "nothing: ()",
        // The enum field uses the FIRST-declared variant (Fast, a unit variant).
        "mode: Fast",
    ] {
        assert!(
            s.text.contains(needle),
            "missing `{needle}` in:\n{}",
            s.text
        );
    }

    // A fully-resolved type has no fill-in placeholders.
    assert!(
        s.fill_in_diagnostics.is_empty(),
        "a fully-resolved type needs no fill-in placeholders"
    );
}

#[test]
fn partially_unknown_type_uses_sentinel_plus_fill_in_diagnostic() {
    // FR-010 / SC-005: a field whose type resolves to `unknown` gets the deterministic
    // parseable sentinel + an inline "placeholder — fill in" diagnostic, and the
    // scaffold STILL parses + conforms.
    let model = model_with(
        "Partial",
        struct_node(vec![
            field("known", TypeNode::primitive(Primitive::Integer)),
            field("mystery", TypeNode::unknown()),
            field("another", TypeNode::unknown()),
        ]),
    );
    let s = derive_scaffold(&model, "Partial");
    assert_parses(&s);
    assert!(s.text.contains("known: 0"));
    assert!(
        s.text.contains(&format!("mystery: {UNKNOWN_SENTINEL}")),
        "unknown field → sentinel:\n{}",
        s.text
    );

    // Two unknown fields → two "fill in" diagnostics, each coded RON-I0010 and
    // anchored exactly on a sentinel token.
    assert_eq!(
        s.fill_in_diagnostics.len(),
        2,
        "one fill-in per unknown field"
    );
    for c in s.fill_in_diagnostics.constructs() {
        assert_eq!(
            c.code(),
            "RON-I0010",
            "fill-in carries the stable placeholder code"
        );
        let span = c.source_range();
        assert_eq!(
            &s.text[span.start()..span.end()],
            UNKNOWN_SENTINEL,
            "the diagnostic anchors exactly on the sentinel token"
        );
    }
}

#[test]
fn derive_is_deterministic_for_the_same_type_and_model() {
    // FR-010: same (type, model) ⇒ byte-identical scaffold + identical fill-in set.
    let model = rich_config_model();
    let a = derive_scaffold(&model, "Config");
    let b = derive_scaffold(&model, "Config");
    assert_eq!(a.text, b.text, "identical scaffold text");
    assert_eq!(
        a.fill_in_diagnostics.len(),
        b.fill_in_diagnostics.len(),
        "identical fill-in count"
    );
}

#[test]
fn enum_root_uses_first_declared_variant() {
    // The first-declared variant is chosen deterministically even at the root.
    let model = model_with(
        "Signal",
        TypeNode::new(NodeKind::Enum {
            variants: vec![
                Variant {
                    serialized_name: "Red".into(),
                    shape: VariantShape::Unit,
                },
                Variant {
                    serialized_name: "Green".into(),
                    shape: VariantShape::Unit,
                },
            ],
            discriminator: Discriminator::External,
        }),
    );
    let s = derive_scaffold(&model, "Signal");
    assert_parses(&s);
    assert_eq!(s.text.trim(), "Red", "first-declared variant:\n{}", s.text);
}

#[test]
fn non_string_key_map_scaffold_is_empty_map_and_parses() {
    // A non-string-keyed map still scaffolds to an empty `{}` (always parses + conforms);
    // exercise the RonKind::NonStringKeyMap path explicitly.
    let model = model_with(
        "Keyed",
        TypeNode::non_string_key_map(
            TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
            TypeRef::inline(TypeNode::primitive(Primitive::String)),
        ),
    );
    let s = derive_scaffold(&model, "Keyed");
    assert_parses(&s);
    assert_eq!(s.text.trim(), "{}");
    // Sanity: the RonKind we exercised is the non-string-key map kind.
    assert_eq!(RonKind::NonStringKeyMap.as_keyword(), "non-string-key-map");
}

// ===========================================================================
// Deterministic scaffold snapshot (SC-005)
// ===========================================================================

#[test]
fn scaffold_snapshot_is_stable() {
    // The canonical derived scaffold for the rich config type — an `insta` snapshot so
    // a deterministic-output regression is caught (SC-005, plan "Snapshot vs assertion
    // scope"). The snapshot is the scaffold TEXT (the byte-stable derive output).
    let model = rich_config_model();
    let s = derive_scaffold(&model, "Config");
    assert_parses(&s);
    insta::assert_snapshot!("rich_config_scaffold", s.text);
}

// ===========================================================================
// Wired App command (US3 AS1/AS2)
// ===========================================================================

#[test]
fn derive_command_opens_a_parseable_scaffold_in_a_new_tab() {
    // US3 AS1: deriving from a registered type opens a parseable scaffold in a NEW tab
    // (the active document is untouched) with the typed placeholders.
    let model = rich_config_model();
    let mut app = app_with_bound_type(&model, "Config");
    let tabs_before = app.document_count();
    let active_before = app.active_index();

    app.derive_ron_from_type();

    assert_eq!(
        app.document_count(),
        tabs_before + 1,
        "derive opened exactly one new tab"
    );
    assert_ne!(
        app.active_index(),
        active_before,
        "the new scaffold tab is active"
    );

    let scaffold = app
        .active_document()
        .expect("active scaffold tab")
        .buffer
        .clone();
    assert!(
        scaffold.contains("count: 0"),
        "scaffold has the typed fields:\n{scaffold}"
    );
    assert!(scaffold.contains("name: \"\""));
    assert!(scaffold.contains("mode: Fast"));
    // The scaffold parses cleanly (SC-005).
    let doc = ron_core::parse(&scaffold);
    assert!(
        doc.diagnostics().is_empty(),
        "scaffold parses cleanly:\n{scaffold}"
    );
    // A success notice was surfaced.
    assert!(
        app.notices()
            .iter()
            .any(|n| n.kind == NoticeKind::Info && n.message.contains("Derived a RON scaffold")),
        "a success notice is surfaced: {:?}",
        app.notices()
    );
}

#[test]
fn derive_command_publishes_fill_in_diagnostics_inline_for_unknown_fields() {
    // FR-006/010: a partially-unknown type's fill-in placeholders are published inline
    // on the new scaffold tab through the same E006 surface (RON-I0010).
    let model = model_with(
        "Partial",
        struct_node(vec![
            field("known", TypeNode::primitive(Primitive::Integer)),
            field("mystery", TypeNode::unknown()),
        ]),
    );
    let mut app = app_with_bound_type(&model, "Partial");
    app.derive_ron_from_type();

    let doc = app.active_document().expect("scaffold tab");
    assert!(
        doc.diagnostics.iter().any(|d| d.code_str() == "RON-I0010"),
        "the 'fill in' placeholder is inline on the scaffold, got {:?}",
        doc.diagnostics
            .iter()
            .map(|d| d.code_str())
            .collect::<Vec<_>>()
    );
}

#[test]
fn derive_with_no_bound_type_surfaces_a_clear_message_and_creates_no_document() {
    // US3 AS2: deriving with no type bound (no document, or an unbound document)
    // surfaces a clear "no type model available" message and creates NO document.
    let mut app = App::new(AppSettings::default(), None);
    let tabs_before = app.document_count();

    app.derive_ron_from_type();

    assert_eq!(
        app.document_count(),
        tabs_before,
        "no document is created when no type is bound"
    );
    assert!(
        app.notices().iter().any(|n| {
            n.kind == NoticeKind::Error
                && n.message.contains("Cannot derive")
                && n.message.contains("no type model")
        }),
        "a clear 'no type model available' message is surfaced: {:?}",
        app.notices()
    );
}

#[test]
fn derive_with_unregistered_root_type_surfaces_a_clear_message_and_creates_no_document() {
    // US3 AS2: a bound type whose root name is NOT in the model degrades to the same
    // clear message and creates no document (no partial/corrupt scaffold).
    let model = model_with(
        "Real",
        struct_node(vec![field("x", TypeNode::primitive(Primitive::Integer))]),
    );
    // Bind a root name that is absent from the serialized model.
    let mut app = app_with_bound_type(&model, "DoesNotExist");
    let tabs_before = app.document_count();

    app.derive_ron_from_type();

    assert_eq!(
        app.document_count(),
        tabs_before,
        "an unregistered root type creates no document"
    );
    assert!(
        app.notices()
            .iter()
            .any(|n| n.kind == NoticeKind::Error && n.message.contains("Cannot derive")),
        "a clear error is surfaced for an unregistered root: {:?}",
        app.notices()
    );
}
