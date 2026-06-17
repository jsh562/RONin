//! E008 Phase 2 (US1) tree/form view tests (T016/T017 — FR-001/FR-002/FR-003/
//! FR-018/FR-019, SC-001/SC-002/SC-008).
//!
//! These pin the tree/form structural surface end-to-end against the **real**
//! off-frame [`ReparseWorker`] round-trip and the real
//! [`EditorDocument::apply_structural_edit`] one-undo-unit pipeline (the same
//! honest doc-state boundary documented in `structural_views.rs` /
//! `type_diagnostics.rs`):
//!
//! * **T016 (FR-001/FR-019).** The tree reflects the CST over struct / map /
//!   list / tuple / enum-variant, and an unparseable region renders as a
//!   read-only error node — well-formed nodes stay editable, nothing crashes.
//! * **T017 (FR-001/FR-002/FR-003/FR-018, SC-001/SC-002/SC-008).** Editing a
//!   value, adding / removing / reordering a field or element, renaming, and
//!   swapping an enum variant each round-trips byte-identically except the
//!   touched node (SC-001), is a single undo unit whose undo restores the exact
//!   prior bytes (SC-002), and a field carrying a diagnostic exposes an inline
//!   indicator with the same severity + code as the text view (SC-008). A rename
//!   collision is blocked inline with no undo entry (FR-003). An egui_kittest
//!   render confirms the tree paints its nodes headlessly.

use std::sync::Arc;
use std::time::{Duration, Instant};

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;

use ronin_app::document::EditorDocument;
use ronin_app::reparse::{BoundType, ReparseWorker};
use ronin_app::structural::tree::{
    collapse_id, is_collapsible_collection, render_tree_view, set_subtree_open, LeafWidget,
    OptionShape, TreeEditable, TreeFormModel, TreeNode, TreeNodeKind,
};
use ronin_app::structural::view_state::{PathStep, StructuralPath};

/// Request a reparse and spin-poll until a current result installs, or panic on
/// timeout. Drives the *real* off-frame worker to completion.
fn drive_reparse(doc: &mut EditorDocument, worker: &ReparseWorker) {
    doc.request_reparse(worker);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if doc.poll_parse(worker) {
            return;
        }
        if Instant::now() >= deadline {
            panic!("reparse did not land within timeout");
        }
        std::thread::yield_now();
    }
}

/// Build a document at `src`, drive a reparse so a projection lands, and return it.
fn doc_at(src: &str, worker: &ReparseWorker) -> EditorDocument {
    let mut doc = EditorDocument::new_untitled(1);
    doc.buffer = src.to_string();
    doc.on_edit();
    drive_reparse(&mut doc, worker);
    doc
}

/// Build the live tree model for a document's current projection + CST.
fn model_of(doc: &EditorDocument) -> TreeFormModel {
    let parse = doc.parse.as_ref().expect("a parse landed");
    TreeFormModel::derive(&parse.cst, &doc.diagnostics)
}

// =============================================================================
// T016 — the tree reflects the CST; unparseable regions read-only (FR-001/019)
// =============================================================================

#[test]
fn tree_reflects_struct_map_list_tuple_enum() {
    let worker = ReparseWorker::new();

    // Struct: one node per field, in source order, with the field name as label.
    let doc = doc_at("Point(x: 1, y: 2)", &worker);
    let model = model_of(&doc);
    let root = &model.roots[0];
    assert_eq!(root.kind, TreeNodeKind::Struct);
    let labels: Vec<_> = root.children.iter().map(|c| c.label.clone()).collect();
    assert_eq!(labels, vec!["x", "y"]);
    assert_eq!(root.children[0].kind, TreeNodeKind::Leaf);

    // Map: one node per entry keyed by the key's verbatim text (non-string keys).
    let doc = doc_at("{ 1: \"one\", 2: \"two\" }", &worker);
    let model = model_of(&doc);
    assert_eq!(model.roots[0].kind, TreeNodeKind::Map);
    let labels: Vec<_> = model.roots[0]
        .children
        .iter()
        .map(|c| c.label.clone())
        .collect();
    assert_eq!(labels, vec!["1", "2"]);

    // List: one node per element keyed by index.
    let doc = doc_at("[10, 20, 30]", &worker);
    let model = model_of(&doc);
    assert_eq!(model.roots[0].kind, TreeNodeKind::List);
    assert_eq!(model.roots[0].children.len(), 3);
    assert_eq!(model.roots[0].children[2].label, "2");

    // Tuple: positional elements keyed by index.
    let doc = doc_at("(1, \"two\", 'c')", &worker);
    let model = model_of(&doc);
    assert_eq!(model.roots[0].kind, TreeNodeKind::Tuple);
    assert_eq!(model.roots[0].children.len(), 3);

    // Enum variant with a struct-like payload uses `{ }` (a named struct uses
    // `( )`): a variant node over its fields.
    let doc = doc_at("Variant { a: 1, b: 2 }", &worker);
    let model = model_of(&doc);
    assert_eq!(model.roots[0].kind, TreeNodeKind::EnumVariant);
    let labels: Vec<_> = model.roots[0]
        .children
        .iter()
        .map(|c| c.label.clone())
        .collect();
    assert_eq!(labels, vec!["a", "b"]);

    // Nested: the child kinds are classified, and the deep node's path is correct.
    let doc = doc_at("Outer(items: [A(v: 1)], name: \"n\")", &worker);
    let model = model_of(&doc);
    let items = model.roots[0]
        .children
        .iter()
        .find(|c| c.label == "items")
        .expect("items present");
    assert_eq!(items.kind, TreeNodeKind::List);
    assert_eq!(
        items.node_ref,
        StructuralPath::from_steps(vec![PathStep::Field("items".to_string())])
    );
}

#[test]
fn unparseable_region_renders_read_only() {
    // FR-019: a stray top-level token recovers into an error node; the tree shows
    // it as a read-only error leaf and never panics.
    let worker = ReparseWorker::new();
    let doc = doc_at("@", &worker);
    let model = model_of(&doc);
    assert_eq!(model.roots[0].kind, TreeNodeKind::Error);
    assert_eq!(model.roots[0].editable, TreeEditable::ReadOnly);
    assert!(model.roots[0].children.is_empty());
}

#[test]
fn tree_view_renders_headlessly() {
    // The tree paints its nodes through the renderer-free egui_kittest harness.
    let worker = ReparseWorker::new();
    let mut doc = doc_at("Config(name: \"app\", retries: 3)", &worker);

    let mut harness = Harness::new_ui(move |ui| {
        render_tree_view(ui, &mut doc, &worker);
    });
    harness.run();
    // The struct's field labels are painted as tree rows.
    assert!(
        harness.query_all_by_label_contains("name").next().is_some(),
        "the tree view must paint the `name` field row"
    );
    assert!(
        harness
            .query_all_by_label_contains("retries")
            .next()
            .is_some(),
        "the tree view must paint the `retries` field row"
    );
}

// =============================================================================
// T017 — edit lossless (SC-001), one undo unit (SC-002), diagnostics (SC-008)
// =============================================================================

#[test]
fn edit_value_is_byte_identical_except_touched_node() {
    // SC-001: editing a scalar value leaves every other byte unchanged.
    let worker = ReparseWorker::new();
    let mut doc = doc_at("Point(x: 1, y: 2) // keep\n", &worker);

    // Edit the `x` field's value to 99 via its structural path.
    let path = StructuralPath::from_steps(vec![PathStep::Field("x".to_string())]);
    doc.apply_tree_set_value(&path, "99".to_string(), &worker, Instant::now())
        .expect("set value applies");

    assert_eq!(doc.buffer, "Point(x: 99, y: 2) // keep\n");
}

#[test]
fn add_remove_reorder_rename_each_one_undo_unit() {
    // SC-001 + SC-002: each structural op round-trips losslessly except the touched
    // node and is a single undo unit whose undo restores the exact prior bytes.
    let worker = ReparseWorker::new();

    // --- add a field ---
    {
        let mut doc = doc_at("Foo(x: 1, y: 2)", &worker);
        let before = doc.buffer.clone();
        let parent = StructuralPath::root();
        doc.apply_tree_insert_field(
            &parent,
            2,
            "z".to_string(),
            "3".to_string(),
            &worker,
            Instant::now(),
        )
        .expect("insert applies");
        assert!(doc.buffer.contains("z: 3"), "field added: {}", doc.buffer);
        // SC-002: the structural op is a SINGLE undo unit — one undo restores the
        // exact prior bytes, and the result then differs from the post-edit state.
        assert!(doc.undo(Instant::now()), "undo steps back");
        assert_eq!(doc.buffer, before, "one undo restores exact prior bytes");
        assert!(doc.redo(), "redo replays the op");
        assert!(doc.buffer.contains("z: 3"), "redo restores the added field");
    }

    // --- remove a field ---
    {
        let mut doc = doc_at("Foo(x: 1, y: 2)", &worker);
        let before = doc.buffer.clone();
        let path = StructuralPath::from_steps(vec![PathStep::Field("y".to_string())]);
        doc.apply_tree_remove(&path, &worker, Instant::now())
            .expect("remove applies");
        assert!(!doc.buffer.contains('y'), "field removed: {}", doc.buffer);
        assert!(doc.undo(Instant::now()), "undo steps back");
        assert_eq!(doc.buffer, before, "undo restores exact prior bytes");
    }

    // --- reorder a field ---
    {
        let mut doc = doc_at("Foo(x: 1, y: 2)", &worker);
        let before = doc.buffer.clone();
        let parent = StructuralPath::root();
        doc.apply_tree_reorder(&parent, 1, 0, &worker, Instant::now())
            .expect("reorder applies");
        let y_pos = doc.buffer.find('y').unwrap();
        let x_pos = doc.buffer.find('x').unwrap();
        assert!(y_pos < x_pos, "y moved before x: {}", doc.buffer);
        assert!(doc.undo(Instant::now()), "undo steps back");
        assert_eq!(doc.buffer, before, "undo restores exact prior bytes");
    }

    // --- rename a field ---
    {
        let mut doc = doc_at("Foo(x: 1, y: 2)", &worker);
        let before = doc.buffer.clone();
        let path = StructuralPath::from_steps(vec![PathStep::Field("x".to_string())]);
        doc.apply_tree_rename(&path, "renamed".to_string(), &worker, Instant::now())
            .expect("rename applies");
        assert!(doc.buffer.contains("renamed: 1"), "renamed: {}", doc.buffer);
        assert!(doc.undo(Instant::now()), "undo steps back");
        assert_eq!(doc.buffer, before, "undo restores exact prior bytes");
    }
}

#[test]
fn rename_collision_blocks_inline_with_no_undo_entry() {
    // FR-003: a rename colliding with an existing key in the same struct is blocked
    // with no byte change and no undo entry.
    let worker = ReparseWorker::new();
    let mut doc = doc_at("Foo(x: 1, y: 2)", &worker);
    let before = doc.buffer.clone();
    let depth_before = doc.undo_depth();

    let path = StructuralPath::from_steps(vec![PathStep::Field("x".to_string())]);
    let err = doc
        .apply_tree_rename(&path, "y".to_string(), &worker, Instant::now())
        .expect_err("renaming x to y must collide");
    assert_eq!(err, ron_core::BlockedReason::RenameCollision);
    assert_eq!(doc.buffer, before, "a blocked rename changes no bytes");
    assert_eq!(
        doc.undo_depth(),
        depth_before,
        "a blocked rename records no undo entry"
    );
}

#[test]
fn variant_swap_keeps_surrounding_document() {
    // FR-003: swapping an enum variant swaps its field set in place; a shared field
    // keeps its bytes, an old-only field is dropped, a new-only field is added.
    let worker = ReparseWorker::new();
    let mut doc = doc_at("Outer(v: Variant { a: 1, b: 2 })", &worker);

    let path = StructuralPath::from_steps(vec![PathStep::Field("v".to_string())]);
    doc.apply_tree_swap_variant(
        &path,
        "Other".to_string(),
        vec!["a".to_string(), "c".to_string()],
        "0".to_string(),
        &worker,
        Instant::now(),
    )
    .expect("variant swap applies");

    assert!(
        doc.buffer.contains("Other"),
        "variant renamed: {}",
        doc.buffer
    );
    assert!(doc.buffer.contains("a: 1"), "shared field a kept its value");
    assert!(!doc.buffer.contains("b: 2"), "old-only field b removed");
    assert!(
        doc.buffer.contains("c: 0"),
        "new-only field c added with placeholder"
    );
    // The surrounding document (the `Outer(...)` wrapper) is preserved.
    assert!(doc.buffer.starts_with("Outer(v: Other"));
}

#[test]
fn list_element_add_remove_reorder_lossless() {
    // SC-001/SC-002 over list elements.
    let worker = ReparseWorker::new();

    // append
    {
        let mut doc = doc_at("[10, 20, 30]", &worker);
        let before = doc.buffer.clone();
        let parent = StructuralPath::root();
        doc.apply_tree_insert_element(&parent, 3, "40".to_string(), &worker, Instant::now())
            .expect("append applies");
        assert!(
            doc.buffer.contains("40"),
            "element appended: {}",
            doc.buffer
        );
        assert!(doc.undo(Instant::now()));
        assert_eq!(doc.buffer, before);
    }

    // remove middle
    {
        let mut doc = doc_at("[10, 20, 30]", &worker);
        let before = doc.buffer.clone();
        let path = StructuralPath::from_steps(vec![PathStep::Index(1)]);
        doc.apply_tree_remove(&path, &worker, Instant::now())
            .expect("remove applies");
        assert_eq!(doc.buffer, "[10, 30]");
        assert!(doc.undo(Instant::now()));
        assert_eq!(doc.buffer, before);
    }
}

#[test]
fn field_with_diagnostic_shows_inline_indicator() {
    // SC-008 / FR-018: a field carrying a type diagnostic exposes an inline
    // indicator on the tree node with the same severity + code as the text view.
    let worker = ReparseWorker::new();
    let mut doc = EditorDocument::new_untitled(1);
    doc.bound_type = Some(BoundType {
        model: Arc::new(entity_model()),
        type_name: "Entity".to_string(),
    });
    doc.buffer = "(id: \"oops\")".to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);

    // The text view sees a type diagnostic (RON-V####, source ron-types).
    let type_diag = doc
        .diagnostics
        .iter()
        .find(|v| v.code.source() == "ron-types")
        .cloned()
        .expect("a type diagnostic is present");

    let model = model_of(&doc);
    // The `id` field node carries the same diagnostic (by CST range overlap), with
    // identical severity + code to the text view.
    let id_node = model.roots[0]
        .children
        .iter()
        .find(|c| c.label == "id")
        .expect("id field present");
    assert!(
        !id_node.diagnostics.is_empty(),
        "the id field node must carry an inline diagnostic indicator"
    );
    let shown = &id_node.diagnostics[0];
    assert_eq!(
        shown.severity, type_diag.severity,
        "same severity as text view"
    );
    assert_eq!(shown.code, type_diag.code, "same code as text view");
}

/// A minimal `TypeModel`: `Entity { id: integer }` with `id` required.
fn entity_model() -> serde_json::Value {
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": {
            "Entity": {
                "type": "object",
                "properties": { "id": { "type": "integer" } },
                "required": ["id"],
                "additionalProperties": true
            }
        }
    })
}

// =============================================================================
// T048 — inline enum-variant selector + Option Some/None editor (FR-002)
// =============================================================================

#[test]
fn variant_value_classifies_with_a_selector_widget() {
    // FR-002: the type→widget mapping is total — a bare enum variant edits inline as
    // a variant selector (LeafWidget::Variant) carrying the candidate variant names.
    let worker = ReparseWorker::new();

    // A bare variant in a list of peer variants: the candidate set is the union of
    // sibling variant names, so the selector can swap to a peer (FR-002).
    let doc = doc_at("[A(x: 1), B(x: 2), A(x: 3)]", &worker);
    let model = model_of(&doc);
    let first = &model.roots[0].children[0];
    assert_eq!(
        first.kind,
        TreeNodeKind::Struct,
        "A(x: 1) is a named struct"
    );

    // A bare-variant list element (no payload) classifies as a Variant leaf widget
    // with a candidate set derived from its peers.
    let doc = doc_at("[Red, Green, Blue]", &worker);
    let model = model_of(&doc);
    let elem = &model.roots[0].children[0];
    assert_eq!(elem.kind, TreeNodeKind::EnumVariant);
    assert_eq!(elem.editable, TreeEditable::ScalarLeaf);
    assert_eq!(elem.leaf_widget, Some(LeafWidget::Variant));
    assert_eq!(elem.variant_name.as_deref(), Some("Red"));
    // The candidate set is the value's own variant plus its sibling/peer variants.
    assert!(elem.variant_candidates.contains(&"Red".to_string()));
    assert!(elem.variant_candidates.contains(&"Green".to_string()));
    assert!(elem.variant_candidates.contains(&"Blue".to_string()));
}

#[test]
fn variant_selector_renders_and_is_discoverable() {
    // FR-002/FR-022: a struct-like enum variant exposes a discoverable inline variant
    // selector (a non-blocking combobox) in the rendered tree.
    let worker = ReparseWorker::new();
    let render_doc = std::cell::RefCell::new(doc_at("Outer(v: Variant { a: 1, b: 2 })", &worker));
    let mut harness = Harness::new_ui(move |ui| {
        render_tree_view(ui, &mut render_doc.borrow_mut(), &worker);
    });
    harness.run();
    // The combobox's selected text is exposed as the node's accessibility *value*.
    assert!(
        harness
            .query_all_by_value("variant: Variant")
            .next()
            .is_some(),
        "the tree view must paint the inline variant selector for an enum variant"
    );
}

#[test]
fn changing_a_variant_swaps_it_losslessly_one_undo_unit() {
    // FR-002/FR-003: changing a variant via the inline selector swaps the variant in
    // place (shared fields keep their bytes), is lossless except the touched node, and
    // is a single undo unit. Driven via the document op the selector emits.
    let worker = ReparseWorker::new();
    let mut doc = doc_at("Outer(v: Variant { a: 1, b: 2 }) // keep\n", &worker);
    let before = doc.buffer.clone();

    // The selector commits a SwapEnumVariant keeping the current payload fields.
    let path = StructuralPath::from_steps(vec![PathStep::Field("v".to_string())]);
    doc.apply_tree_swap_variant(
        &path,
        "Other".to_string(),
        vec!["a".to_string(), "b".to_string()],
        "0".to_string(),
        &worker,
        Instant::now(),
    )
    .expect("variant swap applies");

    assert!(
        doc.buffer.contains("Other"),
        "variant renamed: {}",
        doc.buffer
    );
    assert!(doc.buffer.contains("a: 1"), "shared field a kept its value");
    assert!(doc.buffer.contains("b: 2"), "shared field b kept its value");
    assert!(
        doc.buffer.ends_with("// keep\n"),
        "surrounding bytes preserved"
    );

    // SC-002: a single undo unit restores the exact prior bytes.
    assert!(doc.undo(Instant::now()), "undo steps back");
    assert_eq!(doc.buffer, before, "one undo restores exact prior bytes");
    assert!(doc.redo(), "redo replays the swap");
    assert!(doc.buffer.contains("Other"), "redo restores the swap");
}

#[test]
fn option_value_classifies_with_some_none_control() {
    // FR-002: an Option value renders a Some/None control. `None` is a bare variant,
    // `Some(x)` a one-element `Some(..)` tuple; both classify as the Option widget.
    let worker = ReparseWorker::new();

    let doc = doc_at("Config(opt: None)", &worker);
    let model = model_of(&doc);
    let opt = model.roots[0]
        .children
        .iter()
        .find(|c| c.label == "opt")
        .expect("opt field present");
    assert_eq!(opt.editable, TreeEditable::ScalarLeaf);
    assert_eq!(opt.leaf_widget, Some(LeafWidget::Option));
    assert_eq!(opt.option_shape, Some(OptionShape::None));

    let doc = doc_at("Config(opt: Some(5))", &worker);
    let model = model_of(&doc);
    let opt = model.roots[0]
        .children
        .iter()
        .find(|c| c.label == "opt")
        .expect("opt field present");
    assert_eq!(opt.leaf_widget, Some(LeafWidget::Option));
    assert_eq!(opt.option_shape, Some(OptionShape::Some("5".to_string())));
}

#[test]
fn option_some_none_control_renders_and_toggles_losslessly() {
    // FR-002: the Option editor renders a Some/None control; toggling None → Some and
    // editing the inner value commit losslessly as single undo units.
    let worker = ReparseWorker::new();

    // The rendered control shows the current arm ("None") as a discoverable selector.
    let render_doc = std::cell::RefCell::new(doc_at("Config(opt: None)", &worker));
    let worker_render = ReparseWorker::new();
    let mut harness = Harness::new_ui(move |ui| {
        render_tree_view(ui, &mut render_doc.borrow_mut(), &worker_render);
    });
    harness.run();
    // The Some/None combobox's current arm is exposed as the node's value.
    assert!(
        harness.query_all_by_value("None").next().is_some(),
        "the Option editor must paint a Some/None control showing the current arm"
    );

    // Toggling None → Some and editing the inner value commit losslessly (the editor
    // emits SetValue replacing the whole Option value).
    let mut doc = doc_at("Config(opt: None) // tail\n", &worker);
    let before = doc.buffer.clone();
    let path = StructuralPath::from_steps(vec![PathStep::Field("opt".to_string())]);
    doc.apply_tree_set_value(&path, "Some(7)".to_string(), &worker, Instant::now())
        .expect("Some toggle applies");
    assert!(
        doc.buffer.contains("Some(7)"),
        "toggled to Some: {}",
        doc.buffer
    );
    assert!(
        doc.buffer.ends_with("// tail\n"),
        "surrounding bytes preserved"
    );
    assert!(doc.undo(Instant::now()), "undo steps back");
    assert_eq!(doc.buffer, before, "one undo restores exact prior bytes");
}

// =============================================================================
// T049 — discoverable rename + change-variant node-op affordances (FR-003/FR-022)
// =============================================================================

#[test]
fn rename_control_is_discoverable_and_applies_one_undo_unit() {
    // FR-003/FR-022: a focused struct field exposes a discoverable rename affordance in
    // the rendered tree; invoking it applies the rename as one undo unit, and a
    // collision is surfaced inline with no byte change and no undo entry.
    let worker = ReparseWorker::new();

    // Discoverability: the rendered tree paints a "rename" control on a renameable
    // field, and NOT on a list index (FR-022 never offers rename on an index).
    let render_doc = std::cell::RefCell::new(doc_at("Foo(x: 1, y: 2)", &worker));
    let worker_render = ReparseWorker::new();
    let mut harness = Harness::new_ui(move |ui| {
        render_tree_view(ui, &mut render_doc.borrow_mut(), &worker_render);
    });
    harness.run();
    assert!(
        harness
            .query_all_by_label_contains("rename")
            .next()
            .is_some(),
        "the tree must paint a discoverable rename control on a renameable field"
    );

    let list_doc = std::cell::RefCell::new(doc_at("[1, 2, 3]", &worker));
    let worker_list = ReparseWorker::new();
    let mut list_harness = Harness::new_ui(move |ui| {
        render_tree_view(ui, &mut list_doc.borrow_mut(), &worker_list);
    });
    list_harness.run();
    assert!(
        list_harness
            .query_all_by_label_contains("rename")
            .next()
            .is_none(),
        "rename must NOT be offered on a list index (FR-022)"
    );

    // Invoking the rename applies it as one undo unit (the control emits the op the
    // document API performs).
    let mut doc = doc_at("Foo(x: 1, y: 2)", &worker);
    let before = doc.buffer.clone();
    let path = StructuralPath::from_steps(vec![PathStep::Field("x".to_string())]);
    doc.apply_tree_rename(&path, "renamed".to_string(), &worker, Instant::now())
        .expect("rename applies");
    assert!(doc.buffer.contains("renamed: 1"), "renamed: {}", doc.buffer);
    assert!(doc.undo(Instant::now()), "undo steps back");
    assert_eq!(doc.buffer, before, "one undo restores exact prior bytes");

    // A collision is blocked inline with no byte change and no undo entry (FR-003).
    let mut doc = doc_at("Foo(x: 1, y: 2)", &worker);
    let before = doc.buffer.clone();
    let depth_before = doc.undo_depth();
    let err = doc
        .apply_tree_rename(&path, "y".to_string(), &worker, Instant::now())
        .expect_err("a colliding rename is blocked");
    assert_eq!(err, ron_core::BlockedReason::RenameCollision);
    assert_eq!(doc.buffer, before, "a blocked rename changes no bytes");
    assert_eq!(
        doc.undo_depth(),
        depth_before,
        "a blocked rename records no undo entry"
    );
}

#[test]
fn change_variant_node_op_is_discoverable_for_an_enum_variant() {
    // FR-003/FR-022: the change-variant affordance is discoverable on an enum-variant
    // node (the inline variant selector doubles as the discoverable node-op control),
    // and is NOT offered on a non-variant node.
    let worker = ReparseWorker::new();

    // An enum-variant node: supports_variant_swap + the rendered selector are present.
    let doc = doc_at("Outer(v: Variant { a: 1 })", &worker);
    let model = model_of(&doc);
    let v = model.roots[0]
        .children
        .iter()
        .find(|c| c.label == "v")
        .expect("v field present");
    assert_eq!(v.kind, TreeNodeKind::EnumVariant);
    assert!(v.supports_variant_swap(), "an enum variant supports a swap");

    let render_doc = std::cell::RefCell::new(doc_at("Outer(v: Variant { a: 1 })", &worker));
    let worker_render = ReparseWorker::new();
    let mut harness = Harness::new_ui(move |ui| {
        render_tree_view(ui, &mut render_doc.borrow_mut(), &worker_render);
    });
    harness.run();
    assert!(
        harness
            .query_all_by_value("variant: Variant")
            .next()
            .is_some(),
        "the change-variant control is discoverable on an enum-variant node (FR-022)"
    );

    // A non-variant node (a plain struct) never offers a variant swap.
    let doc = doc_at("Point(x: 1)", &worker);
    let model = model_of(&doc);
    assert!(
        !model.roots[0].supports_variant_swap(),
        "variant-change must NOT be offered on a non-enum node (FR-022)"
    );
}

// =============================================================================
// Collapse-Id stability (UI Fix 3) — path-keyed, nesting-independent, persistent
// =============================================================================

/// Find the first node in `model` whose label is `label` AND that has a child
/// labelled `child_label` (so we can pick out the two distinct `cells:` owners).
fn find_node_with_child<'a>(
    node: &'a TreeNode,
    label: &str,
    child_label: &str,
) -> Option<&'a TreeNode> {
    if node.label == label && node.children.iter().any(|c| c.label == child_label) {
        return Some(node);
    }
    for child in &node.children {
        if let Some(found) = find_node_with_child(child, label, child_label) {
            return Some(found);
        }
    }
    None
}

#[test]
fn collapse_id_distinguishes_distinct_nodes_sharing_depth_and_label() {
    // Two sibling maps each containing a `cells:` list (mirrors
    // `hulls: { (1): {cells:[…]}, (2): {cells:[…]} }`). The two `cells` collection
    // nodes share depth + label but live at distinct structural paths, so their
    // collapse Ids MUST differ — the old depth+label key collided here, sharing
    // collapse state across the two unrelated subtrees.
    let worker = ReparseWorker::new();
    let doc = doc_at(
        "Hulls(hulls: { (1): (cells: [1, 2]), (2): (cells: [3, 4]) })",
        &worker,
    );
    let model = model_of(&doc);
    let root = &model.roots[0];

    // The two map-entry value structs each own a `cells` list child. Collect the
    // `cells` nodes (the collection nodes that share depth + label).
    let mut cells_nodes: Vec<&TreeNode> = Vec::new();
    fn collect_cells<'a>(node: &'a TreeNode, out: &mut Vec<&'a TreeNode>) {
        if node.label == "cells" {
            out.push(node);
        }
        for child in &node.children {
            collect_cells(child, out);
        }
    }
    collect_cells(root, &mut cells_nodes);
    assert_eq!(
        cells_nodes.len(),
        2,
        "fixture must yield exactly two `cells` collection nodes"
    );
    assert_ne!(
        cells_nodes[0].node_ref, cells_nodes[1].node_ref,
        "the two `cells` nodes must occupy distinct structural paths"
    );

    let doc_id = doc.id();
    let id_a = collapse_id(doc_id, cells_nodes[0]);
    let id_b = collapse_id(doc_id, cells_nodes[1]);
    assert_ne!(
        id_a, id_b,
        "two distinct nodes sharing depth + label must get DISTINCT collapse Ids"
    );

    // Sanity: the two map-entry value owners (labels "(1)" and "(2)" — the verbatim
    // tuple-key text the projection uses) are also distinct.
    let entry1 = find_node_with_child(root, "(1)", "cells").expect("entry (1) present");
    let entry2 = find_node_with_child(root, "(2)", "cells").expect("entry (2) present");
    assert_ne!(
        collapse_id(doc_id, entry1),
        collapse_id(doc_id, entry2),
        "distinct map-entry owners must also get distinct collapse Ids"
    );
}

#[test]
fn collapse_id_is_stable_across_a_reparse_of_the_same_source() {
    // The same source re-derives the same structural path, so a node's collapse Id
    // (and therefore its expand/collapse state) must survive an off-frame reparse
    // (FR-016 cross-reparse identity).
    let worker = ReparseWorker::new();
    let src = "Hulls(hulls: { (1): (cells: [1, 2]), (2): (cells: [3, 4]) })";
    let mut doc = doc_at(src, &worker);
    let doc_id = doc.id();

    let before = model_of(&doc);
    let before_cells = find_node_with_child(&before.roots[0], "(1)", "cells")
        .and_then(|e| e.children.iter().find(|c| c.label == "cells"))
        .expect("cells node under entry (1) before reparse");
    let id_before = collapse_id(doc_id, before_cells);
    let path_before = before_cells.node_ref.clone();

    // Force a fresh reparse of the identical buffer (a no-op edit re-deriving the CST).
    doc.on_edit();
    drive_reparse(&mut doc, &worker);
    assert_eq!(doc.id(), doc_id, "document identity is stable across reparse");

    let after = model_of(&doc);
    let after_cells = find_node_with_child(&after.roots[0], "(1)", "cells")
        .and_then(|e| e.children.iter().find(|c| c.label == "cells"))
        .expect("cells node under entry (1) after reparse");
    let id_after = collapse_id(doc_id, after_cells);

    assert_eq!(
        after_cells.node_ref, path_before,
        "the same source must re-resolve the node to the same structural path"
    );
    assert_eq!(
        id_before, id_after,
        "collapse Id for a given path must be identical before and after a reparse"
    );
}

/// Collect every collapsible-collection node in `node`'s subtree (depth-first), the
/// exact set the Expand/Collapse-All walk targets and that owns a `CollapsingState`.
fn collect_collapsible<'a>(node: &'a TreeNode, out: &mut Vec<&'a TreeNode>) {
    if is_collapsible_collection(node) {
        out.push(node);
    }
    for child in &node.children {
        collect_collapsible(child, out);
    }
}

/// Read back the stored open state of `node`'s `CollapsingState` from egui memory.
/// Uses a `default_open` of the OPPOSITE of what the walk just stored, so a `true`
/// result proves the stored value (not the default) was read.
fn stored_open(ctx: &egui::Context, doc_id: u64, node: &TreeNode, default_open: bool) -> bool {
    egui::collapsing_header::CollapsingState::load_with_default_open(
        ctx,
        collapse_id(doc_id, node),
        default_open,
    )
    .is_open()
}

#[test]
fn expand_all_then_collapse_all_toggle_every_collection_nodes_stored_state() {
    // Expand/Collapse-All (Fix 3 / FR-026): the header walk must set EVERY
    // collapsible-collection node's stored egui CollapsingState — and only set it via
    // `collapse_id` — so the next render frame reads the updated state. Drive the same
    // `set_subtree_open` walk the buttons run against a headless `egui::Context` and
    // assert the stored open flag for every collection node flips on Expand-all and
    // off on Collapse-all.
    let worker = ReparseWorker::new();
    let doc = doc_at(
        "Hulls(hulls: { (1): (cells: [1, 2]), (2): (cells: [3, 4, 5]) })",
        &worker,
    );
    let doc_id = doc.id();
    let model = model_of(&doc);

    let mut nodes: Vec<&TreeNode> = Vec::new();
    for root in &model.roots {
        collect_collapsible(root, &mut nodes);
    }
    assert!(
        nodes.len() >= 4,
        "fixture must contain several nested collapsible collections (got {})",
        nodes.len()
    );

    let ctx = egui::Context::default();

    // Expand-all: store `open = true` for every collection node, then read back with a
    // `false` default so a `true` result proves the stored value drives it.
    for root in &model.roots {
        set_subtree_open(&ctx, doc_id, root, true);
    }
    for node in &nodes {
        assert!(
            stored_open(&ctx, doc_id, node, false),
            "after Expand-all the node at {:?} must have its stored CollapsingState OPEN",
            node.node_ref
        );
    }

    // Collapse-all: store `open = false` for every collection node, then read back with
    // a `true` default so a `false` result proves the stored value drives it.
    for root in &model.roots {
        set_subtree_open(&ctx, doc_id, root, false);
    }
    for node in &nodes {
        assert!(
            !stored_open(&ctx, doc_id, node, true),
            "after Collapse-all the node at {:?} must have its stored CollapsingState CLOSED",
            node.node_ref
        );
    }
}

// =============================================================================
// E013 — Part B: tree readability (per-kind icon + child count; large fixture)
// =============================================================================

#[test]
fn tree_headers_include_per_kind_icon_and_child_count() {
    use ronin_app::structural::TypeIndicator;

    let worker = ReparseWorker::new();
    // A root struct with two fields → its collapsible header shows the struct icon
    // and a `(2)` child count.
    let mut doc = doc_at("Config(name: \"app\", retries: 3)", &worker);

    let mut harness = Harness::new_ui(move |ui| {
        render_tree_view(ui, &mut doc, &worker);
    });
    harness.run();

    // The per-kind struct icon (the shared `TypeIndicator` glyph) is painted in the
    // root collapsible header.
    let struct_icon = TypeIndicator::Struct.glyph();
    assert!(
        harness
            .query_all_by_label_contains(struct_icon)
            .next()
            .is_some(),
        "the tree header must paint the per-kind struct icon `{struct_icon}`"
    );
    // The collapsed-collection child count `(2)` is painted (the struct has 2 fields).
    assert!(
        harness.query_all_by_label_contains("(2)").next().is_some(),
        "the collection header must paint a `(N)` child count"
    );
    // The kind word is still present (icon ADDS to, does not replace, the header).
    assert!(
        harness
            .query_all_by_label_contains("[struct]")
            .next()
            .is_some(),
        "the header still labels the kind word"
    );
}

#[test]
fn kind_icon_is_total_and_distinct_per_collection_kind() {
    use ronin_app::structural::tree::TreeNodeKind;
    use ronin_app::structural::TypeIndicator;
    // Every kind maps (via the shared `TypeIndicator`) to a non-empty icon; the
    // collection kinds use distinct icons so the user can tell a struct from a
    // list/map/tuple/enum at a glance.
    let kinds = [
        TreeNodeKind::Struct,
        TreeNodeKind::Map,
        TreeNodeKind::List,
        TreeNodeKind::Tuple,
        TreeNodeKind::EnumVariant,
        TreeNodeKind::Leaf,
        TreeNodeKind::Error,
    ];
    let mut icons = Vec::new();
    for k in kinds {
        let icon = ronin_app::structural::indicators::from_tree_kind(k).glyph();
        assert!(!icon.is_empty(), "{k:?} must have a non-empty icon");
        icons.push(icon);
    }
    // The five collection kinds (struct/map/list/tuple/enum) are mutually distinct.
    let distinct: std::collections::HashSet<_> = icons[..5].iter().collect();
    assert_eq!(distinct.len(), 5, "struct/map/list/tuple/enum icons are distinct");

    // The direct indicator variants and the kind→indicator conversion agree (the
    // SAME glyph regardless of entry point — cross-view consistency, E014).
    assert_eq!(
        ronin_app::structural::indicators::from_tree_kind(TreeNodeKind::List).glyph(),
        TypeIndicator::List.glyph()
    );
    assert_eq!(
        ronin_app::structural::indicators::from_tree_kind(TreeNodeKind::Tuple).glyph(),
        TypeIndicator::Tuple.glyph()
    );
}

#[test]
fn large_fixture_tree_renders_within_responsiveness_guard() {
    // Render the large_ships fixture through the tree view (with all the Part-B
    // readability work: per-kind icons, child counts, monospace previews, indent
    // guides, separators, scroll area) and confirm it renders without panicking,
    // well within a generous responsiveness budget — no regression of structural_perf.
    let worker = ReparseWorker::new();
    let path = format!(
        "{}/tests/fixtures/large_ships.ron",
        env!("CARGO_MANIFEST_DIR")
    );
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    assert!(src.len() > 35_000, "fixture too small to guard ({} bytes)", src.len());

    let mut doc = doc_at(&src, &worker);

    let t0 = Instant::now();
    let mut harness = Harness::builder()
        .with_size(egui::vec2(900.0, 600.0))
        .build_ui(move |ui| {
            render_tree_view(ui, &mut doc, &worker);
        });
    harness.run();
    let render_ms = t0.elapsed().as_secs_f64() * 1000.0;
    eprintln!("large fixture tree render = {render_ms:.3} ms");

    // A generous guard (the structural_perf suite owns the derive-cost gates; this is a
    // no-panic + no-gross-regression render guard with the readability work present).
    const BUDGET_MS: f64 = 2000.0;
    assert!(
        render_ms < BUDGET_MS,
        "large fixture tree render took {render_ms:.3} ms (budget {BUDGET_MS} ms)"
    );
}
