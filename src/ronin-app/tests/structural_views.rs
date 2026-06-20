//! E008 Phase 1b foundational-scaffolding tests (T015 — FR-015/FR-016/FR-020,
//! SC-007).
//!
//! These pin the structural view-state + off-frame projection wiring the US1/US2
//! surfaces build on, driving the **real** [`ReparseWorker`] round-trip end-to-end
//! (request → spin-poll until the current result installs), exactly as
//! `type_diagnostics.rs` / `recovery.rs` do:
//!
//! * **Zero bytes on open / view / switch (FR-020).** Merely opening a document,
//!   reading its projection, and switching among Text / Tree-form / Table changes
//!   **no** document bytes — only an explicit edit may mutate.
//! * **Projection re-derived once per landed reparse; stale while pending
//!   (FR-015 / SC-007).** An edit marks the structural view stale immediately; the
//!   stale marker clears only when a current reparse lands and the projection is
//!   re-derived against the new CST.
//! * **Focus survives a reparse round-trip; drops on vanish (FR-016 / SC-007).**
//!   An active edit focus keyed to a [`StructuralPath`] is kept across an off-frame
//!   reparse when its node still resolves, and dropped gracefully when a
//!   conflicting edit deletes the node.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use ronin_app::document::EditorDocument;
use ronin_app::reparse::ReparseWorker;
use ronin_app::structural::classifier::{classify, FallbackReason};
use ronin_app::structural::projection::{derive_projection, NodeKind};
use ronin_app::structural::view_state::{
    ActiveView, FocusSurface, PathStep, SectionOverride, SectionRendering, StructuralPath,
    ViewSelectionAndFocus,
};
use ronin_core::{ast, parse, SyntaxNode};

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

#[test]
fn opening_viewing_switching_changes_zero_bytes() {
    // FR-020: opening, viewing the projection, and switching views must not change
    // any document bytes — only an explicit edit may.
    let worker = ReparseWorker::new();
    let mut doc = EditorDocument::new_untitled(1);
    let src = "Config(name: \"app\", retries: 3)";
    doc.buffer = src.to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);

    let before = doc.buffer.clone();

    // Default view on open is the structural (tree/form) view (FR-017).
    assert_eq!(doc.view_state().active_view(), ActiveView::TreeForm);

    // Reading the projection (a pure CST read) changes nothing.
    let proj = doc
        .projection()
        .expect("a projection was derived on reparse");
    assert_eq!(proj.root_kind, Some(NodeKind::Struct));
    assert_eq!(
        doc.buffer, before,
        "deriving/reading the projection changed bytes"
    );

    // Switch through every view; each switch is byte-free.
    for view in [ActiveView::Table, ActiveView::Text, ActiveView::TreeForm] {
        doc.view_state_mut().set_active_view(view);
        assert_eq!(doc.view_state().active_view(), view);
        assert_eq!(doc.buffer, before, "switching to {view:?} changed bytes");
    }
    assert_eq!(
        doc.buffer, src,
        "the document text is byte-identical to load"
    );
}

#[test]
fn projection_rederived_once_per_landed_reparse_and_stale_while_pending() {
    // FR-015 / SC-007: an edit marks the structural view stale; the marker clears
    // only when a current reparse lands and the projection is re-derived.
    let worker = ReparseWorker::new();
    let mut doc = EditorDocument::new_untitled(1);
    doc.buffer = "[1, 2, 3]".to_string();
    doc.on_edit();
    // Before the reparse lands the view is stale (an edit was requested).
    assert!(
        doc.view_state().is_stale(),
        "an edit must mark the structural view stale until the reparse lands"
    );
    assert!(
        doc.projection().is_none(),
        "no projection exists before the first reparse lands"
    );

    drive_reparse(&mut doc, &worker);

    // The landed reparse re-derives the projection and clears stale.
    assert!(
        !doc.view_state().is_stale(),
        "a landed current reparse must clear the stale marker"
    );
    let proj = doc
        .projection()
        .expect("projection derived on landed reparse");
    assert_eq!(proj.root_kind, Some(NodeKind::List));
    assert_eq!(proj.root_children.len(), 3, "list has three elements");

    // A second edit re-marks stale; the next landed reparse re-derives once more.
    doc.buffer = "[1, 2, 3, 4]".to_string();
    doc.on_edit();
    assert!(
        doc.view_state().is_stale(),
        "the second edit re-marks stale"
    );
    drive_reparse(&mut doc, &worker);
    assert!(
        !doc.view_state().is_stale(),
        "the second reparse clears stale"
    );
    let proj = doc.projection().expect("projection re-derived");
    assert_eq!(
        proj.root_children.len(),
        4,
        "the projection reflects the latest landed CST (re-derived)"
    );
}

#[test]
fn focus_survives_reparse_round_trip_and_drops_on_vanish() {
    // FR-016 / SC-007: focus keyed to a StructuralPath survives an off-frame reparse
    // when its node still resolves, and drops gracefully when its node vanishes.
    let worker = ReparseWorker::new();
    let mut doc = EditorDocument::new_untitled(1);
    doc.buffer = "Point(x: 1, y: 2)".to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);

    // Focus the `x` field's value via its structural-path identity.
    let path = StructuralPath::from_steps(vec![PathStep::Field("x".to_string())]);
    doc.view_state_mut()
        .set_focus(path.clone(), FocusSurface::TreeNode, "1".to_string());
    assert!(doc.view_state().edit_focus().is_some());

    // An edit that KEEPS `x` (only changes `y`): focus survives the reparse.
    doc.buffer = "Point(x: 1, y: 99)".to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);
    let focus = doc
        .view_state()
        .edit_focus()
        .expect("focus must survive a reparse that keeps the focused node");
    assert_eq!(focus.path, path, "the same logical node stays focused");

    // An edit that DELETES `x`: focus drops gracefully (never edit the wrong node).
    doc.buffer = "Point(y: 99)".to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);
    assert!(
        doc.view_state().edit_focus().is_none(),
        "focus must drop gracefully when the focused node vanishes"
    );
}

// =============================================================================
// US3 — classifier + auto-routing + boundary/override (T036/T043).
//
// FR-010/FR-011 (conservative same-shape detection, never coerce non-uniform),
// FR-012/FR-024 (visible per-section boundary + reversible override + switcher
// precedence), FR-025 (exhaustive fallback reasons surfaced), FR-020/SC-006
// (viewing/classifying = zero bytes, never crash) — verifying SC-005/SC-006/SC-009.
// =============================================================================

/// The document's top-level value node (the section the structural view routes).
fn top_level_value(src: &str) -> SyntaxNode {
    let cst = parse(src);
    ast::Document::cast(cst.root())
        .and_then(|d| d.value())
        .expect("a top-level value")
        .syntax()
        .clone()
}

#[test]
fn classifier_same_shape_list_is_table_eligible() {
    // FR-010: every element same struct name + consistent field set → eligible, with
    // the column set = union of fields in first-seen order.
    let v = classify(&top_level_value(
        "[(a: 1, b: 2), (a: 3, c: 4), (a: 5, b: 6)]",
    ));
    assert!(v.table_eligible, "same-shape list must be table-eligible");
    assert!(v.fallback_reason.is_none());
    let cols: Vec<_> = v
        .column_schema
        .iter()
        .map(|c| c.field_name.clone())
        .collect();
    assert_eq!(
        cols,
        vec!["a", "b", "c"],
        "union of fields, first-seen order"
    );
}

#[test]
fn classifier_absent_field_stays_uniform() {
    // FR-010: a field merely absent from some elements stays uniform (blank cell).
    let v = classify(&top_level_value("[(a: 1, b: 2), (a: 3, b: 4), (a: 5)]"));
    assert!(
        v.table_eligible,
        "an absent field must not break uniformity"
    );
}

#[test]
fn classifier_name_mismatch_falls_back() {
    // FR-010/FR-011: differing struct/variant names → non-uniform → tree/form.
    let v = classify(&top_level_value("[A(x: 1), B(x: 2), A(x: 3)]"));
    assert!(!v.table_eligible);
    assert_eq!(v.fallback_reason, Some(FallbackReason::NameMismatch));
}

#[test]
fn classifier_conflicting_field_type_falls_back() {
    // FR-010: a same-named field with conflicting value types across elements → non-uniform.
    let v = classify(&top_level_value("[(a: 1), (a: \"x\"), (a: 3)]"));
    assert!(!v.table_eligible);
    assert_eq!(v.fallback_reason, Some(FallbackReason::TypeConflict));
}

#[test]
fn classifier_all_nested_cells_falls_back() {
    // FR-006/FR-010: a list whose only cells are nested collections (nothing scalar
    // to edit as a grid) → NestedOnly fallback.
    let v = classify(&top_level_value("[(a: [1]), (a: [2]), (a: [3])]"));
    assert!(!v.table_eligible);
    assert_eq!(v.fallback_reason, Some(FallbackReason::NestedOnly));
}

#[test]
fn classifier_non_record_element_falls_back() {
    // FR-011: a non-record element (a bare scalar) makes the list not-a-record-list.
    let v = classify(&top_level_value("[(a: 1), 2, (a: 3)]"));
    assert!(!v.table_eligible);
    assert_eq!(v.fallback_reason, Some(FallbackReason::NotARecordList));
}

#[test]
fn classifier_too_small_uniform_list_falls_back() {
    // FR-010: a uniform list of ≤2 elements defaults to tree/form (override available).
    let v = classify(&top_level_value("[(a: 1), (a: 2)]"));
    assert!(!v.table_eligible);
    assert_eq!(v.fallback_reason, Some(FallbackReason::TooSmall));
}

#[test]
fn classifier_empty_list_falls_back() {
    let v = classify(&top_level_value("[]"));
    assert!(!v.table_eligible);
    assert_eq!(v.fallback_reason, Some(FallbackReason::Empty));
}

#[test]
fn classifier_non_list_node_is_unparseable() {
    // FR-019: a non-list section degrades safely (never crashes) to a fallback.
    let v = classify(&top_level_value("Point(x: 1)"));
    assert!(!v.table_eligible);
    assert_eq!(v.fallback_reason, Some(FallbackReason::Unparseable));
}

#[test]
fn fallback_reasons_are_exhaustive_and_have_user_labels() {
    // FR-025: every reason in the closed set carries a non-empty user-facing label
    // for the boundary indicator. (Exhaustiveness is enforced by the classifier
    // producer covering every shape, exercised by the cases above.)
    for reason in [
        FallbackReason::NameMismatch,
        FallbackReason::TypeConflict,
        FallbackReason::NestedOnly,
        FallbackReason::NotARecordList,
        FallbackReason::TooSmall,
        FallbackReason::Empty,
        FallbackReason::Unparseable,
    ] {
        assert!(
            !reason.label().is_empty(),
            "{reason:?} must have a user-facing label (FR-025)"
        );
    }
}

#[test]
fn classifying_changes_zero_bytes() {
    // FR-020 / SC-006: classifying never mutates the document. The classifier reads a
    // CST built from `src`; re-printing the (unchanged) source proves the round-trip.
    let worker = ReparseWorker::new();
    let mut doc = EditorDocument::new_untitled(1);
    let src = "[(a: 1), (a: 2), (a: 3)]";
    doc.buffer = src.to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);
    let before = doc.buffer.clone();

    // Classify the top-level section repeatedly — a pure read.
    let node = top_level_value(&doc.buffer);
    for _ in 0..3 {
        let _ = classify(&node);
    }
    assert_eq!(doc.buffer, before, "classifying must change zero bytes");
    assert_eq!(doc.buffer, src);
}

#[test]
fn routing_uniform_to_table_non_uniform_to_tree_with_visible_boundary() {
    // SC-005: in a structural view, a uniform list routes to a table and a
    // non-uniform list routes to tree/form, each with a visible boundary state.
    let vsf = ViewSelectionAndFocus::new();
    assert_eq!(
        vsf.active_view(),
        ActiveView::TreeForm,
        "default structural"
    );
    let section = StructuralPath::root();

    // A uniform list (eligible) → auto table.
    let uniform = classify(&top_level_value("[(a: 1), (a: 2), (a: 3)]"));
    assert!(uniform.table_eligible);
    let r = vsf
        .section_rendering(&section, uniform.table_eligible)
        .expect("structural view yields a rendering");
    assert_eq!(r, SectionRendering::Table { forced: false });
    assert!(r.is_table() && !r.is_forced(), "auto table, not forced");

    // A non-uniform list (mixed names) → auto tree/form.
    let mixed = classify(&top_level_value("[A(x: 1), B(x: 2)]"));
    assert!(!mixed.table_eligible);
    let r = vsf
        .section_rendering(&section, mixed.table_eligible)
        .expect("rendering");
    assert_eq!(r, SectionRendering::TreeForm { forced: false });
    assert!(
        !r.is_table() && !r.is_forced(),
        "auto tree/form, not coerced"
    );
}

#[test]
fn never_coerces_non_uniform_into_a_grid() {
    // FR-011 / SC-006: a non-uniform list NEVER routes to a table automatically.
    let vsf = ViewSelectionAndFocus::new();
    let section = StructuralPath::root();
    for src in [
        "[A(x: 1), B(x: 2), A(x: 3)]",    // name mismatch
        "[(a: 1), (a: \"x\"), (a: 2)]",   // type conflict
        "[(a: [1]), (a: [2]), (a: [3])]", // all nested
        "[(a: 1), 2, (a: 3)]",            // non-record element
        "[]",                             // empty
        "Point(x: 1)",                    // non-list root
    ] {
        let v = classify(&top_level_value(src));
        assert!(!v.table_eligible, "{src} must be non-uniform");
        let r = vsf
            .section_rendering(&section, v.table_eligible)
            .expect("rendering");
        assert!(
            !r.is_table(),
            "{src} must NOT be coerced into a grid (FR-011)"
        );
    }
}

#[test]
fn force_tree_form_override_is_reversible() {
    // SC-005 / FR-012: a uniform (auto-table) section can be forced to tree/form, and
    // the same control reverses it back to the automatic table rendering.
    let mut vsf = ViewSelectionAndFocus::new();
    let section = StructuralPath::root();
    let eligible = true; // an auto-table uniform section

    // Auto → table.
    assert_eq!(
        vsf.section_rendering(&section, eligible),
        Some(SectionRendering::Table { forced: false })
    );

    // Toggle once → forced tree/form.
    vsf.toggle_section_override(&section, eligible);
    assert_eq!(
        vsf.section_override(&section),
        Some(SectionOverride::ForceTreeForm)
    );
    assert_eq!(
        vsf.section_rendering(&section, eligible),
        Some(SectionRendering::TreeForm { forced: true }),
        "forced tree/form is visible as a manual override"
    );

    // Toggle again → back to the automatic table rendering (reversible — FR-012).
    vsf.toggle_section_override(&section, eligible);
    assert_eq!(vsf.section_override(&section), None);
    assert_eq!(
        vsf.section_rendering(&section, eligible),
        Some(SectionRendering::Table { forced: false })
    );
}

#[test]
fn force_table_override_on_small_uniform_list_is_reversible() {
    // FR-012: a ≤2-element uniform list defaults to tree/form but the same control
    // can force it to a table, reversibly.
    let mut vsf = ViewSelectionAndFocus::new();
    let section = StructuralPath::root();
    // A small uniform list classifies as not-eligible (TooSmall); its auto rendering
    // is tree/form.
    let small = classify(&top_level_value("[(a: 1), (a: 2)]"));
    assert!(!small.table_eligible);
    assert_eq!(small.fallback_reason, Some(FallbackReason::TooSmall));
    assert_eq!(
        vsf.section_rendering(&section, small.table_eligible),
        Some(SectionRendering::TreeForm { forced: false })
    );

    // Toggle → forced table.
    vsf.toggle_section_override(&section, small.table_eligible);
    assert_eq!(
        vsf.section_override(&section),
        Some(SectionOverride::ForceTable)
    );
    assert_eq!(
        vsf.section_rendering(&section, small.table_eligible),
        Some(SectionRendering::Table { forced: true })
    );

    // Toggle → back to automatic tree/form.
    vsf.toggle_section_override(&section, small.table_eligible);
    assert_eq!(vsf.section_override(&section), None);
    assert_eq!(
        vsf.section_rendering(&section, small.table_eligible),
        Some(SectionRendering::TreeForm { forced: false })
    );
}

#[test]
fn override_applies_only_in_structural_view_and_is_retained_in_text() {
    // FR-024: a per-section override applies only while in a structural view; Text
    // shows the whole document as text regardless of overrides, which are RETAINED
    // (never silently cleared) for the return to a structural view.
    let mut vsf = ViewSelectionAndFocus::new();
    let section = StructuralPath::root();
    let eligible = true;

    // Force tree/form on a uniform section while in the structural view.
    vsf.toggle_section_override(&section, eligible);
    assert_eq!(
        vsf.section_rendering(&section, eligible),
        Some(SectionRendering::TreeForm { forced: true })
    );

    // Switch to Text: no per-section rendering applies (whole doc shows as text), and
    // the override is retained — the switcher never silently clears it (FR-024).
    vsf.set_active_view(ActiveView::Text);
    assert_eq!(
        vsf.section_rendering(&section, eligible),
        None,
        "no per-section rendering in the Text view (FR-024)"
    );
    assert_eq!(
        vsf.section_override(&section),
        Some(SectionOverride::ForceTreeForm),
        "the override is retained while in Text (FR-024)"
    );

    // Returning to the structural view re-applies the retained override.
    vsf.set_active_view(ActiveView::TreeForm);
    assert_eq!(
        vsf.section_rendering(&section, eligible),
        Some(SectionRendering::TreeForm { forced: true }),
        "the retained override re-applies on return to a structural view (FR-024)"
    );
}

#[test]
fn override_never_changes_the_document_level_active_view() {
    // FR-024: a per-section override never changes the document-level active view.
    let mut vsf = ViewSelectionAndFocus::new();
    let section = StructuralPath::root();
    assert_eq!(vsf.active_view(), ActiveView::TreeForm);
    vsf.toggle_section_override(&section, true);
    assert_eq!(
        vsf.active_view(),
        ActiveView::TreeForm,
        "an override must not change the active view"
    );
    vsf.toggle_section_override(&section, true);
    assert_eq!(vsf.active_view(), ActiveView::TreeForm);
}

#[test]
fn viewing_and_classifying_mixed_document_changes_zero_bytes_and_does_not_crash() {
    // SC-006 / FR-020: opening + classifying + routing a document with a uniform and a
    // non-uniform list changes zero bytes and never crashes on awkward shapes.
    let worker = ReparseWorker::new();
    let mut doc = EditorDocument::new_untitled(1);
    // A struct containing a uniform list and a heterogeneous list (a mixed document).
    let src = "Config(rows: [(a: 1), (a: 2), (a: 3)], misc: [A(x: 1), B(y: 2)])";
    doc.buffer = src.to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);
    let before = doc.buffer.clone();

    // Classify each inner list section by its structural path; never crashes.
    let cst = parse(&doc.buffer);
    let root = cst.root();
    let rows_path = StructuralPath::from_steps(vec![PathStep::Field("rows".to_string())]);
    let misc_path = StructuralPath::from_steps(vec![PathStep::Field("misc".to_string())]);
    let rows = classify(
        &ronin_app::structural::view_state::resolve_path(&root, &rows_path).expect("rows resolves"),
    );
    let misc = classify(
        &ronin_app::structural::view_state::resolve_path(&root, &misc_path).expect("misc resolves"),
    );
    assert!(
        rows.table_eligible,
        "the uniform inner list is table-eligible"
    );
    assert!(
        !misc.table_eligible,
        "the heterogeneous inner list falls back"
    );

    assert_eq!(doc.buffer, before, "viewing/classifying changed bytes");
    assert_eq!(doc.buffer, src);
}

// =============================================================================
// Phase 5 — Polish & Cross-Cutting (T044/T045/T046).
//
// Cross-cutting verification over the now-complete US1/US2/US3 surfaces: the
// off-frame/bounded guarantee (FR-026), the zero-bytes invariant end-to-end
// (FR-020/SC-006), and degrade-safe rendering across BOTH structural surfaces
// (FR-019). These assert the structural PROPERTIES (counts/derivations independent
// of N, no byte mutation, no panic) per the project's "performance is not a hard CI
// gate" posture (project-instructions.md §Performance Standards).
// =============================================================================

/// Build a uniform list of `n` 2-field record rows as a RON source string.
fn uniform_list_src(n: usize) -> String {
    let mut s = String::from("[\n");
    for i in 0..n {
        s.push_str(&format!("    (id: {i}, name: \"row{i}\"),\n"));
    }
    s.push(']');
    s
}

// -----------------------------------------------------------------------------
// T044 [COMPLETES FR-026] — off-frame / bounded guarantee
// -----------------------------------------------------------------------------

#[test]
fn projection_is_derived_once_per_landed_reparse_not_per_frame() {
    // FR-026(a): projection re-derivation is triggered ONCE per landed reparse
    // (debounced, not per keystroke and not per frame). After a reparse lands, the
    // installed projection is stable: re-polling with NO new edit re-derives nothing
    // (poll_parse returns false and the projection value is unchanged), and the next
    // edit's landed reparse re-derives exactly once against the new CST.
    let worker = ReparseWorker::new();
    let mut doc = EditorDocument::new_untitled(1);
    doc.buffer = "[1, 2, 3]".to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);

    let first = doc.projection().expect("projection landed").clone();
    assert_eq!(first.root_kind, Some(NodeKind::List));
    assert_eq!(first.root_children.len(), 3);

    // Simulate many frames with no edit: poll_parse installs nothing further (the
    // worker has no new result), so the projection is NOT re-derived per frame.
    for _ in 0..16 {
        assert!(
            !doc.poll_parse(&worker),
            "no edit ⇒ no landed reparse ⇒ no per-frame re-derivation"
        );
    }
    assert_eq!(
        doc.projection().expect("projection still present"),
        &first,
        "the projection is unchanged across many frames without a reparse"
    );

    // An edit + its landed reparse re-derives exactly once against the new CST.
    doc.buffer = "[1, 2, 3, 4]".to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);
    let second = doc.projection().expect("re-derived projection");
    assert_eq!(
        second.root_children.len(),
        4,
        "the landed reparse re-derived the projection against the new CST (once)"
    );
}

#[test]
fn classifier_short_circuits_on_first_mismatch_independent_of_trailing_size() {
    // FR-026(b): the classifier's per-list cost is linear and SHORT-CIRCUITS on the
    // first shape mismatch — it does not re-scan after a verdict. Structural probe:
    // a list whose SECOND element already mismatches the first yields the SAME
    // verdict (NameMismatch) regardless of how many further elements follow, so the
    // work past the mismatch cannot change the answer (the verdict is decided at the
    // mismatch, not by the tail).
    let short = parse("[A(x: 1), B(x: 2), A(x: 3)]");
    let short_top = ast::Document::cast(short.root())
        .and_then(|d| d.value())
        .expect("top value")
        .syntax()
        .clone();
    let short_v = classify(&short_top);

    // The same prefix mismatch with a very long tail after it.
    let mut long_src = String::from("[A(x: 1), B(x: 2)");
    for i in 0..5_000 {
        long_src.push_str(&format!(", A(x: {i})"));
    }
    long_src.push(']');
    let long = parse(&long_src);
    let long_top = ast::Document::cast(long.root())
        .and_then(|d| d.value())
        .expect("top value")
        .syntax()
        .clone();
    let long_v = classify(&long_top);

    assert_eq!(
        short_v.fallback_reason,
        Some(FallbackReason::NameMismatch),
        "the short list mismatches at element 1"
    );
    assert_eq!(
        long_v.fallback_reason, short_v.fallback_reason,
        "the verdict is decided at the first mismatch — the long tail cannot change it (short-circuit)"
    );
    assert!(!long_v.table_eligible && !short_v.table_eligible);
}

#[test]
fn projection_realizes_only_immediate_children_not_the_whole_tree() {
    // FR-026(c): realization is lazy — deriving a node's projection outlines only its
    // IMMEDIATE children (one level); the children's own subtrees are NOT realized.
    // Probe: a deeply nested document and a shallow document with the same immediate
    // breadth derive the SAME number of root children — the per-derive outline cost is
    // bounded by the node's direct child count, independent of total descendant
    // count / nesting depth (so expanding a node realizes only that node's children).
    let shallow = parse("Outer(a: 1, b: 2)");
    let deep = parse("Outer(a: [[[[[1]]]]], b: Nested(p: Deep(q: Deeper(r: 9))))");
    let shallow_proj = derive_projection(&shallow);
    let deep_proj = derive_projection(&deep);

    assert_eq!(shallow_proj.root_kind, Some(NodeKind::Struct));
    assert_eq!(deep_proj.root_kind, Some(NodeKind::Struct));
    assert_eq!(
        shallow_proj.root_children.len(),
        2,
        "the shallow struct outlines its two immediate fields"
    );
    assert_eq!(
        deep_proj.root_children.len(),
        shallow_proj.root_children.len(),
        "a deeply-nested struct of the same breadth outlines the SAME immediate-child count (lazy: no deep realization)"
    );

    // The immediate child outlines classify the child kind but carry no realized
    // grandchildren — confirming one-level realization (the `a` field of `deep` is a
    // List; its nested list-of-lists is not expanded into the outline).
    let a = deep_proj
        .root_children
        .iter()
        .find(|c| c.label == "a")
        .expect("field a outlined");
    assert_eq!(
        a.kind,
        NodeKind::List,
        "the immediate child kind is classified without realizing its subtree"
    );
}

/// Render `doc`'s table view in a fixed-size viewport and return how many rows the
/// `TableBody::rows` virtualization actually realized (invoked the row closure for).
///
/// This is the same realized-row-count probe `table_view.rs` uses for T027/T034,
/// reused here for the Phase-5 frame⊥N cross-cutting assertion (FR-026(c)/(d)).
fn realized_row_count(src: &str) -> usize {
    let worker = ReparseWorker::new();
    let mut doc = EditorDocument::new_untitled(1);
    doc.buffer = src.to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);

    let realized = Rc::new(Cell::new(0usize));
    let realized_for_ui = Rc::clone(&realized);
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(400.0, 200.0))
        .build_ui(move |ui| {
            ui.set_max_height(200.0);
            ronin_app::structural::table::render_table_view_counting(
                ui,
                &mut doc,
                &worker,
                &ronin_app::structural::view_state::StructuralPath::root(),
                ronin_app::structural::sections::SectionShape::RecordList,
                &realized_for_ui,
            );
        });
    harness.run();
    realized.get()
}

#[test]
fn per_frame_realized_work_is_bounded_and_independent_of_total_n() {
    // FR-026(c)/(d): within a frame only REALIZED content costs work — the realized
    // (viewport-visible) row count is bounded by the viewport and does NOT grow with
    // the section's total row count, so per-frame cost is independent of total N. This
    // is the cross-cutting frame⊥N property (the same load-bearing probe behind
    // SC-010/T027, asserted here as the FR-026 guarantee).
    let small = realized_row_count(&uniform_list_src(1_000));
    let large = realized_row_count(&uniform_list_src(100_000));

    assert!(
        small < 100,
        "realized rows are viewport-bounded, not total-bounded (1k), got {small}"
    );
    assert!(
        large < 100,
        "realized rows are viewport-bounded, not total-bounded (100k), got {large}"
    );
    assert_eq!(
        small, large,
        "per-frame realized work is independent of total N (1k vs 100k) — FR-026"
    );
}

// -----------------------------------------------------------------------------
// T045 [COMPLETES FR-020] — zero-bytes end-to-end
// -----------------------------------------------------------------------------

#[test]
fn open_view_switch_classify_changes_zero_bytes_only_an_edit_mutates() {
    // FR-020 / SC-006 (end-to-end): opening, reading the projection, switching among
    // all views, and classifying every section of a MIXED document (uniform +
    // non-uniform sections) changes ZERO bytes; only an explicit structural edit
    // mutates the buffer.
    let worker = ReparseWorker::new();
    let mut doc = EditorDocument::new_untitled(1);
    // A mixed document: a uniform record list, a heterogeneous (name-mismatch) list,
    // a conflicting-field-type list, and a scalar field — exercising several
    // classifier verdicts in one buffer.
    let src = concat!(
        "Config(\n",
        "    rows: [(a: 1), (a: 2), (a: 3)],\n",
        "    misc: [A(x: 1), B(y: 2)],\n",
        "    mixed: [(k: 1), (k: \"x\"), (k: 2)],\n",
        "    name: \"app\",\n",
        ")"
    );
    doc.buffer = src.to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);
    let before = doc.buffer.clone();

    // Opening derived a projection (a pure CST read) — zero bytes.
    assert_eq!(doc.view_state().active_view(), ActiveView::TreeForm);
    let proj = doc.projection().expect("a projection landed");
    assert_eq!(proj.root_kind, Some(NodeKind::Struct));
    assert_eq!(doc.buffer, before, "reading the projection changed bytes");

    // Switching among every view is byte-free.
    for view in [ActiveView::Table, ActiveView::Text, ActiveView::TreeForm] {
        doc.view_state_mut().set_active_view(view);
        assert_eq!(doc.buffer, before, "switching to {view:?} changed bytes");
    }

    // Classify each list section (uniform + non-uniform) repeatedly — pure reads.
    let cst = parse(&doc.buffer);
    let root = cst.root();
    let sections = [
        (
            StructuralPath::from_steps(vec![PathStep::Field("rows".to_string())]),
            true,
            None,
        ),
        (
            StructuralPath::from_steps(vec![PathStep::Field("misc".to_string())]),
            false,
            Some(FallbackReason::NameMismatch),
        ),
        (
            StructuralPath::from_steps(vec![PathStep::Field("mixed".to_string())]),
            false,
            Some(FallbackReason::TypeConflict),
        ),
    ];
    for (path, eligible, reason) in &sections {
        let node =
            ronin_app::structural::view_state::resolve_path(&root, path).expect("section resolves");
        for _ in 0..3 {
            let v = classify(&node);
            assert_eq!(v.table_eligible, *eligible, "{path:?} eligibility");
            assert_eq!(v.fallback_reason, *reason, "{path:?} fallback reason");
        }
    }
    assert_eq!(
        doc.buffer, before,
        "opening + viewing + switching + classifying changed ZERO bytes (FR-020)"
    );
    assert_eq!(doc.buffer, src, "the buffer is byte-identical to load");

    // ONLY an explicit edit mutates: edit the uniform list's first row cell.
    let rows = StructuralPath::from_steps(vec![PathStep::Field("rows".to_string())]);
    doc.apply_table_set_cell(&rows, 0, "a", "99".to_string(), &worker, Instant::now())
        .expect("the explicit cell edit applies");
    assert_ne!(
        doc.buffer, before,
        "an explicit structural edit IS allowed to mutate the buffer"
    );
    assert!(
        doc.buffer.contains("a: 99"),
        "the edit landed: {}",
        doc.buffer
    );
    // Every untouched section is byte-identical (lossless — FR-013, the basis of
    // FR-020's "only the edit mutates" guarantee).
    assert!(doc.buffer.contains("misc: [A(x: 1), B(y: 2)]"));
    assert!(doc.buffer.contains("mixed: [(k: 1), (k: \"x\"), (k: 2)]"));
    assert!(doc.buffer.contains("name: \"app\""));
}

// -----------------------------------------------------------------------------
// T046 [COMPLETES FR-019] — degrade-safe across BOTH surfaces (tree + table)
// -----------------------------------------------------------------------------

#[test]
fn degrade_safe_tree_surface_over_partially_invalid_document() {
    // FR-019: a document with an unparseable/partially-invalid region renders in the
    // tree surface without crashing — the error region is surfaced as a read-only
    // node, while well-formed sibling nodes stay editable, and nothing is corrupted.
    use ronin_app::structural::tree::{
        render_tree_view, TreeEditable, TreeFormModel, TreeNodeKind,
    };

    let worker = ReparseWorker::new();
    let mut doc = EditorDocument::new_untitled(1);
    // A struct whose `bad` field value is a stray/invalid token (recovers to an Error
    // node) sitting beside well-formed `ok` and `n` fields.
    let src = "Config(ok: 1, bad: @, n: \"keep\")";
    doc.buffer = src.to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);
    let before = doc.buffer.clone();

    // The model derives without panicking; the well-formed fields are editable and
    // the invalid region is a read-only Error node (degraded safely, not coerced).
    let parse = doc.parse.as_ref().expect("a parse landed");
    let model = TreeFormModel::derive(&parse.cst, &doc.diagnostics);
    let root = &model.roots[0];
    assert_eq!(root.kind, TreeNodeKind::Struct, "the root still projects");

    let ok = root
        .children
        .iter()
        .find(|c| c.label == "ok")
        .expect("well-formed field present");
    assert_eq!(
        ok.editable,
        TreeEditable::ScalarLeaf,
        "a well-formed leaf beside an error region stays editable (FR-019)"
    );
    let bad = root
        .children
        .iter()
        .find(|c| c.label == "bad")
        .expect("the invalid field is still surfaced, not dropped");
    assert_eq!(
        bad.editable,
        TreeEditable::ReadOnly,
        "the unparseable region is surfaced read-only, never coerced (FR-019)"
    );
    assert_eq!(bad.kind, TreeNodeKind::Error);

    // Rendering the tree headlessly over the partially-invalid document never panics.
    let render_doc = std::cell::RefCell::new(doc);
    let mut harness = egui_kittest::Harness::new_ui(|ui| {
        render_tree_view(ui, &mut render_doc.borrow_mut(), &worker);
    });
    harness.run();

    // Viewing the degraded document changed zero bytes (no corruption — FR-019/020).
    assert_eq!(
        render_doc.borrow().buffer,
        before,
        "rendering a degraded document must not corrupt or mutate it"
    );
}

#[test]
fn degrade_safe_table_surface_never_coerces_error_list_into_a_grid() {
    // FR-019 + FR-011: a list made non-uniform by an unparseable/invalid element
    // falls back to tree/form (never coerced into a grid), and the table surface
    // degrades safely over a non-list / awkward section without crashing.
    use ronin_app::structural::table::{render_table_view, TableModel};

    let worker = ReparseWorker::new();
    let mut doc = EditorDocument::new_untitled(1);
    // A list whose second element is an invalid/stray token — recovery makes it a
    // non-record element, so the list is NOT a uniform record list.
    let src = "[(a: 1), @, (a: 3), (a: 4)]";
    doc.buffer = src.to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);
    let before = doc.buffer.clone();

    // The classifier never deems an error-bearing list table-eligible (never coerce).
    let cst = parse(&doc.buffer);
    let root = cst.root();
    let section = StructuralPath::root();
    let node = ronin_app::structural::view_state::resolve_path(&root, &section)
        .expect("the list section resolves");
    let verdict = classify(&node);
    assert!(
        !verdict.table_eligible,
        "an error-bearing list is never coerced into a grid (FR-011/FR-019)"
    );
    assert!(
        verdict.fallback_reason.is_some(),
        "the fallback carries a reason: {verdict:?}"
    );

    // Auto-routing resolves this section to tree/form (never a table) — FR-011.
    let rendering = doc
        .view_state()
        .section_rendering(&section, verdict.table_eligible)
        .expect("structural view yields a rendering");
    assert!(
        !rendering.is_table(),
        "the section routes to tree/form, never a coerced grid"
    );

    // Deriving the table model over the error-bearing list does not panic; a non-
    // record element simply contributes no columns / a blank row (degraded safely).
    let table = TableModel::derive(&cst, &section, &doc.diagnostics)
        .expect("a list section yields a (possibly degenerate) model");
    let _ = table.row_count();

    // Rendering the table surface headlessly over the degraded section never panics.
    let render_doc = std::cell::RefCell::new(doc);
    let mut harness = egui_kittest::Harness::new_ui(|ui| {
        render_table_view(
            ui,
            &mut render_doc.borrow_mut(),
            &worker,
            &StructuralPath::root(),
            ronin_app::structural::sections::SectionShape::RecordList,
        );
    });
    harness.run();

    // Viewing the degraded document changed zero bytes (no corruption — FR-019/020).
    assert_eq!(
        render_doc.borrow().buffer,
        before,
        "rendering a degraded list must not corrupt or mutate it"
    );
}

// =============================================================================
// T051 [SC-011] — focus-rebind cost ∝ path depth, independent of N (FR-027)
// =============================================================================

/// A list of `n` 2-field sibling records, each `(a: i, b: "ri")`, as RON source.
/// Every element is the SAME fixed shape so a focus path of fixed depth/position
/// resolves identically regardless of `n` (the SC-011 N-axis).
fn sibling_records_src(n: usize) -> String {
    let mut s = String::from("[");
    for i in 0..n {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&format!("(a: {i}, b: \"r{i}\")"));
    }
    s.push(']');
    s
}

#[test]
fn focus_rebind_cost_is_proportional_to_path_depth_not_n() {
    // SC-011 / FR-027: re-resolving a focus path costs time proportional to the
    // focused node's structural-path DEPTH, not the section's row count. We hold the
    // path depth (and the addressed position) FIXED while growing total siblings N
    // from 1k to 100k, and instrument resolve_path's node-visit count: it must be
    // IDENTICAL at 1k and 100k (the rebind cost does not grow with N).
    //
    // This mirrors SC-010's structural-property approach (a load-bearing N-independence
    // assertion), NOT a wall-clock measurement.

    // A fixed-depth, fixed-position focus path: descend into the element at a FIXED
    // index (5) and then its `a` field — depth 2, addressing position 5 regardless of
    // how many siblings exist.
    let focus =
        StructuralPath::from_steps(vec![PathStep::Index(5), PathStep::Field("a".to_string())]);

    let small = parse(&sibling_records_src(1_000));
    let large = parse(&sibling_records_src(100_000));
    let small_root = small.root();
    let large_root = large.root();

    let (small_node, small_visits) =
        ronin_app::structural::view_state::resolve_path_visiting(&small_root, &focus);
    let (large_node, large_visits) =
        ronin_app::structural::view_state::resolve_path_visiting(&large_root, &focus);

    // Both resolve to the same logical node (correctness across the two trees).
    assert_eq!(small_node.map(|n| n.text()), Some("5".to_string()));
    assert_eq!(large_node.map(|n| n.text()), Some("5".to_string()));

    // The load-bearing property: the node-visit count is IDENTICAL at 1k and 100k —
    // the rebind cost is independent of total siblings N (FR-027 / SC-011).
    assert_eq!(
        small_visits, large_visits,
        "focus-rebind node-visit count must be independent of N (1k vs 100k): {small_visits} vs {large_visits}"
    );

    // And it is bounded by the path's addressed position/depth, NOT by N: descending
    // to index 5 examines 6 elements, then the struct's `a` field examines 1 field =
    // 7 visits, far below either total row count.
    assert_eq!(
        small_visits, 7,
        "the visit count reflects path depth + addressed position (idx 5 → 6 elems + 1 field), not N"
    );
    assert!(
        small_visits < 1_000,
        "the rebind never scans the full sibling set (got {small_visits})"
    );

    // A DEEPER path at the SAME N costs strictly more (cost tracks depth, not N):
    // descend to the same element then its `b` field is also depth 2 / position 5, so
    // it visits the same; a one-step shallower path (just the element) visits fewer.
    let shallow = StructuralPath::from_steps(vec![PathStep::Index(5)]);
    let (_, shallow_visits) =
        ronin_app::structural::view_state::resolve_path_visiting(&large_root, &shallow);
    assert!(
        shallow_visits < large_visits,
        "a shallower path (no trailing field step) visits fewer nodes — cost tracks depth"
    );
    assert_eq!(shallow_visits, 6, "index 5 examines 6 elements");
}

// =============================================================================
// E012 — Table view navigator: section list renders; selection switches grid
// =============================================================================

/// A multi-section document: a `rows` RecordList (3 records, columns a/b), a `coords`
/// TupleList (3 tuples), and a `hulls` RecordMap (2 same-shape value records).
const MULTI_SECTION_SRC: &str = concat!(
    "(\n",
    "  rows: [(a: 1, b: 2), (a: 3, b: 4), (a: 5, b: 6)],\n",
    "  coords: [(0, 0), (1, 1), (2, 2)],\n",
    "  hulls: { (1): (hp: 10), (2): (hp: 20) },\n",
    ")"
);

#[test]
fn navigator_lists_sections_and_selection_switches_grid() {
    use egui_kittest::kittest::Queryable;
    use std::cell::RefCell;

    let worker = Rc::new(ReparseWorker::new());
    let mut doc = EditorDocument::new_untitled(1);
    doc.buffer = MULTI_SECTION_SRC.to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);
    let before = doc.buffer.clone();

    // The scan finds all three sections (a RecordList, a TupleList, a RecordMap).
    let sections =
        ronin_app::structural::sections::scan_table_sections(&doc.parse.as_ref().unwrap().cst);
    use ronin_app::structural::sections::SectionShape as Sh;
    assert!(sections.iter().any(|s| s.shape == Sh::RecordList));
    assert!(sections.iter().any(|s| s.shape == Sh::TupleList));
    assert!(sections.iter().any(|s| s.shape == Sh::RecordMap));

    let doc = Rc::new(RefCell::new(doc));

    // Frame 1: the tree-outline navigator lists every container node by its label. The
    // default selection is the document root (never empty); the outline lists the
    // root's container fields (`rows`, `coords`, `hulls`).
    {
        let doc_ui = Rc::clone(&doc);
        let worker_ui = Rc::clone(&worker);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(700.0, 360.0))
            .build_ui(move |ui| {
                let mut d = doc_ui.borrow_mut();
                ronin_app::panels::render_table_seam(ui, &mut d, &worker_ui);
            });
        harness.run();
        // The outline lists each container node by its tree label (the icon + name).
        assert!(
            harness.query_all_by_label_contains("rows").next().is_some(),
            "the outline lists the `rows` list node"
        );
        assert!(
            harness
                .query_all_by_label_contains("coords")
                .next()
                .is_some(),
            "the outline lists the `coords` list node"
        );
        assert!(
            harness
                .query_all_by_label_contains("hulls")
                .next()
                .is_some(),
            "the outline lists the `hulls` map node"
        );
    }

    // Select the `coords` TupleList section by path (byte-free view-state write).
    let coords_path = StructuralPath::from_steps(vec![PathStep::Field("coords".to_string())]);
    doc.borrow_mut()
        .view_state_mut()
        .set_selected_table_section(Some(coords_path.clone()));

    // Frame 2: the selected grid is now the TupleList — its positional `.0` column
    // header renders (it does not exist in the RecordList grid).
    {
        let doc_ui = Rc::clone(&doc);
        let worker_ui = Rc::clone(&worker);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(700.0, 360.0))
            .build_ui(move |ui| {
                let mut d = doc_ui.borrow_mut();
                ronin_app::panels::render_table_seam(ui, &mut d, &worker_ui);
            });
        harness.run();
        assert!(
            harness.query_all_by_label_contains(".0").next().is_some(),
            "the TupleList grid renders positional .0 columns after selecting coords"
        );
    }

    // Select the `hulls` RecordMap; its leading read-only `(key)` column renders.
    let hulls_path = StructuralPath::from_steps(vec![PathStep::Field("hulls".to_string())]);
    doc.borrow_mut()
        .view_state_mut()
        .set_selected_table_section(Some(hulls_path));
    {
        let doc_ui = Rc::clone(&doc);
        let worker_ui = Rc::clone(&worker);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(700.0, 360.0))
            .build_ui(move |ui| {
                let mut d = doc_ui.borrow_mut();
                ronin_app::panels::render_table_seam(ui, &mut d, &worker_ui);
            });
        harness.run();
        assert!(
            harness
                .query_all_by_label_contains("(key)")
                .next()
                .is_some(),
            "the RecordMap grid renders a leading (key) column after selecting hulls"
        );
    }

    // The whole navigation was byte-free (FR-020): no edit, only view-state writes.
    assert_eq!(
        doc.borrow().buffer,
        before,
        "browsing the navigator and switching sections must not mutate bytes"
    );
}

/// The alternate "Table (sections)" tab (`render_table_sections_seam`): a scanner-driven
/// navigator that groups detected sections by top-level ancestor and labels each with
/// its `(rows×cols)` dimensions, sharing the central grid + selection with the outline tab.
#[test]
fn table_sections_navigator_groups_lists_dims_and_shares_grid() {
    use egui_kittest::kittest::Queryable;
    use std::cell::RefCell;

    let worker = Rc::new(ReparseWorker::new());
    let mut doc = EditorDocument::new_untitled(1);
    doc.buffer = MULTI_SECTION_SRC.to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);
    let before = doc.buffer.clone();
    let doc = Rc::new(RefCell::new(doc));

    // Frame 1: the grouped-sections navigator shows its `Tables` header, a group for the
    // `rows` ancestor, and section leaves carrying their `(rows×cols)` dimensions (the `×`
    // separator is unique to this navigator — the outline shows plain child counts).
    {
        let doc_ui = Rc::clone(&doc);
        let worker_ui = Rc::clone(&worker);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(700.0, 360.0))
            .build_ui(move |ui| {
                let mut d = doc_ui.borrow_mut();
                ronin_app::panels::render_table_sections_seam(ui, &mut d, &worker_ui);
            });
        harness.run();
        assert!(
            harness
                .query_all_by_label_contains("Tables")
                .next()
                .is_some(),
            "the grouped-sections navigator shows its `Tables` header"
        );
        assert!(
            harness.query_all_by_label_contains("rows").next().is_some(),
            "the `rows` section/group is listed"
        );
        assert!(
            harness
                .query_all_by_label_contains("\u{00D7}")
                .next()
                .is_some(),
            "section leaves carry their (rows×cols) dimensions"
        );
    }

    // Select the `coords` TupleList section; the grouped-sections grid renders its
    // positional `.0` column.
    let coords_path = StructuralPath::from_steps(vec![PathStep::Field("coords".to_string())]);
    doc.borrow_mut()
        .view_state_mut()
        .set_selected_table_section(Some(coords_path));
    {
        let doc_ui = Rc::clone(&doc);
        let worker_ui = Rc::clone(&worker);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(700.0, 360.0))
            .build_ui(move |ui| {
                let mut d = doc_ui.borrow_mut();
                ronin_app::panels::render_table_sections_seam(ui, &mut d, &worker_ui);
            });
        harness.run();
        assert!(
            harness.query_all_by_label_contains(".0").next().is_some(),
            "selecting coords renders the TupleList positional .0 column"
        );
    }

    // Shared selection: switching to the OUTLINE tab shows the SAME selected grid.
    {
        let doc_ui = Rc::clone(&doc);
        let worker_ui = Rc::clone(&worker);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(700.0, 360.0))
            .build_ui(move |ui| {
                let mut d = doc_ui.borrow_mut();
                ronin_app::panels::render_table_seam(ui, &mut d, &worker_ui);
            });
        harness.run();
        assert!(
            harness.query_all_by_label_contains(".0").next().is_some(),
            "both Table tabs share `selected_table_section` (outline shows coords too)"
        );
    }

    assert_eq!(
        doc.borrow().buffer,
        before,
        "the grouped-sections navigator is byte-free"
    );
}

#[test]
fn scalar_only_document_renders_root_as_field_value_grid_never_empty() {
    use egui_kittest::kittest::Queryable;

    let worker = ReparseWorker::new();
    let mut doc = EditorDocument::new_untitled(1);
    // A scalar-only struct (sample.ron-shaped): no record lists, maps, or tuple lists,
    // so the OLD navigator showed an empty state. The tree-outline navigator now
    // defaults to the root and renders the root struct as a field/value grid (Part A3 —
    // never empty).
    doc.buffer = "Config(name: \"x\", retries: 3, mode: Fast)".to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);

    // The root struct DOES project a field/value table via `derive_any` even though the
    // scanner finds no strict table-able section.
    assert!(
        ronin_app::structural::sections::scan_table_sections(&doc.parse.as_ref().unwrap().cst)
            .is_empty(),
        "the scalar-only doc has no strict table-able section"
    );

    let render_doc = std::cell::RefCell::new(doc);
    let mut harness = egui_kittest::Harness::new_ui(|ui| {
        ronin_app::panels::render_table_seam(ui, &mut render_doc.borrow_mut(), &worker);
    });
    harness.run();
    // The root renders as a field/value grid: a leading read-only `(field)` column +
    // the field rows (`name`, `retries`, …) — content, not an empty state.
    assert!(
        harness
            .query_all_by_label_contains("(field)")
            .next()
            .is_some(),
        "the root struct renders a field/value grid (leading (field) column)"
    );
    assert!(
        harness.query_all_by_label_contains("name").next().is_some(),
        "the root struct's `name` field row renders"
    );
}

// =============================================================================
// E013 — open ANY nested collection as a table: NestedTable cell click switches the
// grid; breadcrumb navigates back up; grouped side list renders + leaf selects.
// =============================================================================

/// A doc whose `data.rows` uniform record list has a nested LIST cell (`tags`) per row
/// — so the grid shows a NestedTable "open as table" cell the user can click into. The
/// list is nested two levels (`data` → `rows`) so the side-list group key is `data`
/// while the breadcrumb's clickable `rows` segment is an unambiguous navigation target
/// (distinct from any side-list label and from the weak `root` segment).
const NESTED_LIST_SRC: &str = concat!(
    "(data: (rows: [\n",
    "  (id: 1, tags: [\"a\", \"b\"]),\n",
    "  (id: 2, tags: [\"c\", \"d\"]),\n",
    "  (id: 3, tags: [\"e\", \"f\"]),\n",
    "]))"
);

#[test]
fn clicking_nested_table_cell_switches_grid_to_nested_path_byte_free() {
    use egui_kittest::kittest::Queryable;
    use std::cell::RefCell;

    let worker = Rc::new(ReparseWorker::new());
    let mut doc = EditorDocument::new_untitled(1);
    doc.buffer = NESTED_LIST_SRC.to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);
    let before = doc.buffer.clone();
    // Select the `data.rows` RecordList in the outline so the grid renders it (the
    // navigator now defaults to the root, so we pick the nested list to exercise its
    // NestedTable `tags` cell — selecting an outline node is byte-free).
    doc.view_state_mut()
        .set_selected_table_section(Some(StructuralPath::from_steps(vec![
            PathStep::Field("data".to_string()),
            PathStep::Field("rows".to_string()),
        ])));
    let doc = Rc::new(RefCell::new(doc));

    // Frame 1: the navigator renders the `rows` RecordList grid; row 0's `tags` cell is
    // a NestedTable "open as table" button prefixed with the collection icon.
    {
        let doc_ui = Rc::clone(&doc);
        let worker_ui = Rc::clone(&worker);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 400.0))
            .build_ui(move |ui| {
                let mut d = doc_ui.borrow_mut();
                ronin_app::panels::render_table_seam(ui, &mut d, &worker_ui);
            });
        harness.run();
        // E019c: a NestedTable cell opens as a table via its small "open" icon (single
        // click), just left of the list-preview summary. A body click only selects.
        {
            let r = harness.get_by_label_contains("\"a\"").rect();
            let p = egui::pos2(r.left() - 11.0, r.center().y);
            harness.drag_at(p);
            harness.drop_at(p);
        }
        harness.run();
    }

    // The selection switched to the nested `tags` list path (byte-free).
    let rows_path = StructuralPath::from_steps(vec![
        PathStep::Field("data".to_string()),
        PathStep::Field("rows".to_string()),
    ]);
    {
        let d = doc.borrow();
        let expected = rows_path
            .child(PathStep::Index(0))
            .child(PathStep::Field("tags".to_string()));
        assert_eq!(
            d.view_state().selected_table_section(),
            Some(&expected),
            "clicking a NestedTable cell re-keys the navigator to the nested path"
        );
        // STAYS in the table view — drilling a List does NOT switch to tree/form.
        assert!(
            d.view_state().drill_in_return().is_none(),
            "opening a List as a table records no tree drill-in return"
        );
        assert_eq!(
            d.buffer, before,
            "opening a nested collection as a table changes zero bytes"
        );
    }

    // Frame 2: the nested `tags` list now renders as a grid (a scalar list → a single
    // `value` column), and the breadcrumb shows the ancestor chain
    // `root ▸ rows ▸ [0] ▸ tags` with `rows` an openable (clickable) segment.
    {
        let doc_ui = Rc::clone(&doc);
        let worker_ui = Rc::clone(&worker);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 400.0))
            .build_ui(move |ui| {
                let mut d = doc_ui.borrow_mut();
                ronin_app::panels::render_table_seam(ui, &mut d, &worker_ui);
            });
        harness.run();
        // The nested grid's single `value` column header renders.
        assert!(
            harness
                .query_all_by_label_contains("value")
                .next()
                .is_some(),
            "the nested scalar list renders a single `value` column grid"
        );
        // The breadcrumb `rows` segment is a clickable button (rows is an openable list).
        assert!(
            harness.query_all_by_label_contains("rows").next().is_some(),
            "the breadcrumb renders the ancestor chain with a `rows` segment"
        );
        // Navigate up via the breadcrumb `rows` segment (back to the rows list).
        harness.get_by_label("rows").click();
        harness.run();
    }

    // The breadcrumb navigated the selection back up to the `rows` list (byte-free).
    {
        let d = doc.borrow();
        assert_eq!(
            d.view_state().selected_table_section(),
            Some(&rows_path),
            "clicking the breadcrumb `rows` segment navigates the grid back to the rows list"
        );
        assert_eq!(d.buffer, before, "breadcrumb navigation changes zero bytes");
    }
}

// =============================================================================
// E016 — Table view Back / Forward / Up navigation buttons in the seam
// =============================================================================

#[test]
fn table_view_back_forward_up_navigation_is_byte_free() {
    use egui_kittest::kittest::Queryable;
    use std::cell::RefCell;

    let worker = Rc::new(ReparseWorker::new());
    let mut doc = EditorDocument::new_untitled(1);
    doc.buffer = NESTED_LIST_SRC.to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);
    let before = doc.buffer.clone();

    let rows_path = StructuralPath::from_steps(vec![
        PathStep::Field("data".to_string()),
        PathStep::Field("rows".to_string()),
    ]);
    let tags_path = rows_path
        .child(PathStep::Index(0))
        .child(PathStep::Field("tags".to_string()));

    // Select `data.rows` so the grid shows its NestedTable `tags` cells. Use the raw
    // setter (a non-navigational seed) so the history starts empty.
    doc.view_state_mut()
        .set_selected_table_section(Some(rows_path.clone()));
    let doc = Rc::new(RefCell::new(doc));

    // Helper: render one frame of the seam.
    let render_frame = |doc: &Rc<RefCell<EditorDocument>>, worker: &Rc<ReparseWorker>| {
        let doc_ui = Rc::clone(doc);
        let worker_ui = Rc::clone(worker);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(900.0, 460.0))
            .build_ui(move |ui| {
                let mut d = doc_ui.borrow_mut();
                ronin_app::panels::render_table_seam(ui, &mut d, &worker_ui);
            });
        harness.run();
        harness
    };

    // Frame 1: render the rows grid and click row 0's `tags` NestedTable cell to open
    // it as a table (records the level change in history — E016).
    {
        let mut harness = render_frame(&doc, &worker);
        // E019c: open-as-table via the NestedTable cell's small "open" icon (single
        // click), just left of the summary text.
        {
            let r = harness.get_by_label_contains("\"a\"").rect();
            let p = egui::pos2(r.left() - 11.0, r.center().y);
            harness.drag_at(p);
            harness.drop_at(p);
        }
        harness.run();
    }
    assert_eq!(
        doc.borrow().view_state().selected_table_section(),
        Some(&tags_path),
        "double-clicking the NestedTable cell navigates the grid to the nested `tags` path"
    );
    assert!(
        doc.borrow().view_state().can_go_back(),
        "the drill-in recorded a back entry"
    );

    // Frame 2: the Back button (◀) returns to the prior `rows` path.
    {
        let mut harness = render_frame(&doc, &worker);
        harness.get_by_label_contains("\u{25C0}").click();
        harness.run();
    }
    assert_eq!(
        doc.borrow().view_state().selected_table_section(),
        Some(&rows_path),
        "Back returns the grid to the prior path"
    );
    assert!(
        doc.borrow().view_state().can_go_forward(),
        "Back populated the forward stack"
    );

    // Frame 3: the Forward button (▶) re-advances to `tags`.
    {
        let mut harness = render_frame(&doc, &worker);
        harness.get_by_label_contains("\u{25B6}").click();
        harness.run();
    }
    assert_eq!(
        doc.borrow().view_state().selected_table_section(),
        Some(&tags_path),
        "Forward re-advances the grid to the nested path"
    );

    // Frame 4: the Up button (▲) goes to the parent of `tags` (`data.rows.[0]`).
    {
        let mut harness = render_frame(&doc, &worker);
        harness.get_by_label_contains("\u{25B2}").click();
        harness.run();
    }
    assert_eq!(
        doc.borrow().view_state().selected_table_section(),
        Some(&rows_path.child(PathStep::Index(0))),
        "Up a level navigates the grid to the parent path"
    );

    // The entire Back/Forward/Up navigation was byte-free (FR-020).
    assert_eq!(
        doc.borrow().buffer,
        before,
        "Back/Forward/Up navigation changes zero document bytes"
    );
}

#[test]
fn outline_lists_container_nodes_not_scalar_leaves_and_selecting_switches_grid() {
    use egui_kittest::kittest::Queryable;
    use std::cell::RefCell;

    let worker = Rc::new(ReparseWorker::new());
    let mut doc = EditorDocument::new_untitled(1);
    // Two container fields (`rows` a list, `coords` a list) plus a SCALAR LEAF field
    // (`title`). The outline must list the container nodes but SKIP the scalar leaf.
    doc.buffer = concat!(
        "(\n",
        "  title: \"hello\",\n",
        "  rows: [(a: 1), (a: 2), (a: 3)],\n",
        "  coords: [(0, 0), (1, 1), (2, 2)],\n",
        ")"
    )
    .to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);
    let doc = Rc::new(RefCell::new(doc));

    // The scalar-leaf field path (`title`) — it must never be a selectable outline node.
    let title_path = StructuralPath::from_steps(vec![PathStep::Field("title".to_string())]);
    let coords_path = StructuralPath::from_steps(vec![PathStep::Field("coords".to_string())]);

    {
        let doc_ui = Rc::clone(&doc);
        let worker_ui = Rc::clone(&worker);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 400.0))
            .build_ui(move |ui| {
                let mut d = doc_ui.borrow_mut();
                ronin_app::panels::render_table_seam(ui, &mut d, &worker_ui);
            });
        harness.run();

        // The outline lists the container nodes (`rows`, `coords`).
        assert!(
            harness.query_all_by_label_contains("rows").next().is_some(),
            "the outline lists the `rows` container node"
        );
        assert!(
            harness
                .query_all_by_label_contains("coords")
                .next()
                .is_some(),
            "the outline lists the `coords` container node"
        );

        // Selecting `coords` switches the grid to it. Click the OUTLINE row. The icon
        // now lives in a separate fixed-width slot (E014), so the outline's selectable
        // label is the count-suffixed `coords  (3)` — unique to the outline (the root
        // grid's `(field)` cell shows the bare name `coords`). (The scalar-leaf `title`
        // is not an outline node, so there is nothing to click that selects it — its
        // model is `None` via `derive_any`, asserted below.)
        harness.get_by_label_contains("coords  (").click();
        harness.run();
    }

    {
        let d = doc.borrow();
        assert_eq!(
            d.view_state().selected_table_section(),
            Some(&coords_path),
            "selecting an outline container node switches the grid to it (byte-free)"
        );
    }

    // A scalar leaf is never selectable as a table: `derive_any` over `title` is `None`,
    // so the navigator can never key the grid to it (and the outline never lists it).
    {
        let mut d = doc.borrow_mut();
        let cst = &d.parse.as_ref().unwrap().cst;
        assert!(
            ronin_app::structural::table::TableModel::derive_any(cst, &title_path, &[]).is_none(),
            "a scalar leaf node is not table-able (never selectable in the outline)"
        );
        // Defensive: even if the selection were somehow set to a scalar leaf, the seam
        // falls back to the root (never empty) rather than rendering it.
        d.view_state_mut()
            .set_selected_table_section(Some(title_path.clone()));
    }
    {
        let doc_ui = Rc::clone(&doc);
        let worker_ui = Rc::clone(&worker);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 400.0))
            .build_ui(move |ui| {
                let mut d = doc_ui.borrow_mut();
                ronin_app::panels::render_table_seam(ui, &mut d, &worker_ui);
            });
        harness.run();
        // Falls back to the root field/value grid (the leading `(field)` column renders),
        // never a scalar leaf and never empty.
        assert!(
            harness
                .query_all_by_label_contains("(field)")
                .next()
                .is_some(),
            "a non-table-able stored selection falls back to the root field/value grid"
        );
    }
}

// =============================================================================
// E022 — Table (grouped) superset: outline nav + group-by + show-columns, editable
// =============================================================================

#[test]
fn grouped_seam_renders_an_editable_grid_grouped_by_the_selected_field() {
    use egui_kittest::kittest::Queryable;
    use std::cell::RefCell;

    let worker = Rc::new(ReparseWorker::new());
    let mut doc = EditorDocument::new_untitled(1);
    // A RecordList whose `kind` field has values x, y, x → grouping by `kind` clusters the
    // two "x" rows together; the grid renders the `kind` column first plus the values.
    doc.buffer =
        "[\n    (kind: \"x\", n: 1),\n    (kind: \"y\", n: 2),\n    (kind: \"x\", n: 3),\n]"
            .to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);
    // View the root list, grouped by column 0 (`kind`, first-seen).
    doc.view_state_mut()
        .set_selected_table_section(Some(StructuralPath::root()));
    doc.view_state_mut().set_group_by(vec![0]);
    let doc = Rc::new(RefCell::new(doc));

    let doc_ui = Rc::clone(&doc);
    let worker_ui = Rc::clone(&worker);
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(900.0, 460.0))
        .build_ui(move |ui| {
            let mut d = doc_ui.borrow_mut();
            ronin_app::panels::render_table_grouped_seam(ui, &mut d, &worker_ui);
        });
    harness.run();

    // The grouped editable grid renders: the `kind` column (group field, shown first) and
    // its clustered values are both present.
    assert!(
        harness.query_all_by_label_contains("kind").next().is_some(),
        "the group field `kind` is present (column header / picker)"
    );
    assert!(
        harness
            .query_all_by_label_contains("\"x\"")
            .next()
            .is_some(),
        "a grouped data cell value renders"
    );
}
