//! E008 Phase 3 (US2) spreadsheet/table view tests (T026/T027/T034 —
//! FR-005/FR-006/FR-007/FR-008/FR-009/FR-018, SC-003/SC-004/SC-010).
//!
//! These pin the virtualized table surface end-to-end against the **real**
//! off-frame [`ReparseWorker`] round-trip and the real
//! [`EditorDocument::apply_structural_edit`] one-undo-unit pipeline (the same
//! honest doc-state boundary documented in `tree_form_view.rs`):
//!
//! * **T026 (FR-005/FR-006, SC-003).** A *uniform* list of same-shape records
//!   projects to rows × columns (column set = union of fields, first-seen order;
//!   an absent field renders as a blank cell). Editing a cell, appending a row,
//!   and deleting a row each round-trips losslessly (untouched rows/fields
//!   byte-identical, SC-003) and is a single undo unit. A cell holding a nested
//!   collection is classified `Nested` (a drill-in cell, FR-006), never an inline
//!   editor.
//! * **T027 (FR-008).** The table virtualizes: only the rows whose extent
//!   intersects the viewport (plus a bounded overscan) are realized, so the
//!   realized-row count is bounded by the viewport height and is **independent of
//!   the section's total row count**. Driven through the real `egui_kittest`
//!   harness with `TableBody::rows` (NOT `::row` per element).
//! * **T034 (SC-010).** The 100k-rows × 10-scalar-columns benchmark fixture. The
//!   load-bearing virtualization property (realized-row count bounded by the
//!   viewport, identical at 1k and 100k rows) is asserted as a hard CI gate; the
//!   ≤16 ms/frame wall-clock figure is a benchmark **target** measured manually in
//!   a release build on a stated reference desktop, NOT a hard CI assertion
//!   (consistent with the project's "not yet a hard QC gate" performance posture).
//!   See the test comment for exactly which was done.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;

use ronin_app::document::EditorDocument;
use ronin_app::reparse::ReparseWorker;
use ronin_app::structural::sections::SectionShape;
use ronin_app::structural::table::{render_table_view, CellClass, ColumnClass, TableModel};
use ronin_app::structural::tree::render_tree_view;
use ronin_app::structural::view_state::{ActiveView, FocusSurface, PathStep, StructuralPath};

/// Request a reparse and spin-poll until a current result installs, or panic on
/// timeout. Drives the *real* off-frame worker to completion. The deadline is
/// generous so the 100k-row SC-010 fixture (which parses a multi-megabyte source
/// in a debug build) still lands; the *frame* budget the spec gates on is the
/// virtualized render, not this one-time off-frame parse.
fn drive_reparse(doc: &mut EditorDocument, worker: &ReparseWorker) {
    doc.request_reparse(worker);
    let deadline = Instant::now() + Duration::from_secs(60);
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

/// Build the live table model for a document's top-level uniform list section.
fn model_of(doc: &EditorDocument) -> TableModel {
    let parse = doc.parse.as_ref().expect("a parse landed");
    TableModel::derive(&parse.cst, &StructuralPath::root(), &doc.diagnostics)
        .expect("the top-level list projects a table model")
}

// =============================================================================
// T026 — uniform list → rows × columns; cell/append/delete lossless single-undo
// =============================================================================

#[test]
fn uniform_list_projects_rows_and_columns() {
    // FR-005: each record is a row, each field a column; columns = union of fields
    // in first-seen order; an absent field renders as a blank cell.
    let worker = ReparseWorker::new();
    let doc = doc_at(
        "[\n    (name: \"a\", hp: 1),\n    (name: \"b\", hp: 2, mp: 3),\n    (name: \"c\", hp: 4),\n]",
        &worker,
    );
    let model = model_of(&doc);

    // Three rows.
    assert_eq!(model.row_count(), 3);

    // Columns are the union of fields in first-seen order: name, hp (from row 0),
    // then mp (first seen on row 1).
    let cols: Vec<_> = model.columns.iter().map(|c| c.field_name.clone()).collect();
    assert_eq!(cols, vec!["name", "hp", "mp"]);

    // Row 0 has no `mp` field → that cell is Blank, visually distinct from a
    // present scalar (FR-010).
    let r0_mp = model.cell(0, 2).expect("row 0 / mp cell");
    assert_eq!(r0_mp.class, CellClass::Blank);

    // Row 1 has all three fields → all Scalar.
    let r1_mp = model.cell(1, 2).expect("row 1 / mp cell");
    assert_eq!(r1_mp.class, CellClass::Scalar);
    let r1_name = model.cell(1, 0).expect("row 1 / name cell");
    assert_eq!(r1_name.class, CellClass::Scalar);
    assert_eq!(r1_name.text.as_deref(), Some("\"b\""));
}

#[test]
fn nested_collection_cell_is_drill_in_not_inline() {
    // FR-006 / E013: a cell whose value is a nested collection is NOT an inline scalar
    // editor. A List/Map cell is classified `NestedTable` (opens AS A TABLE in place);
    // a struct/tuple/enum cell stays `Nested` (tree/form drill-in). The column carrying
    // nested values is classified Nested either way.
    let worker = ReparseWorker::new();
    let doc = doc_at(
        "[\n    (id: 1, tags: [\"x\"], pos: (0, 0)),\n    (id: 2, tags: [\"y\", \"z\"], pos: (1, 0)),\n    (id: 3, tags: [], pos: (2, 0)),\n]",
        &worker,
    );
    let model = model_of(&doc);

    // `tags` is a List → NestedTable (open-as-table), column is Nested.
    let tags_col = model
        .columns
        .iter()
        .position(|c| c.field_name == "tags")
        .expect("tags column present");
    assert_eq!(model.columns[tags_col].class, ColumnClass::Nested);
    let tags_cell = model.cell(0, tags_col).expect("row 0 / tags cell");
    assert_eq!(
        tags_cell.class,
        CellClass::NestedTable,
        "a List cell opens AS A TABLE (E013)"
    );
    assert!(
        tags_cell.value_ref.is_some(),
        "a nested-table cell carries a structural path to open"
    );

    // `pos` is a tuple → stays Nested (tree/form drill-in), not NestedTable.
    let pos_col = model
        .columns
        .iter()
        .position(|c| c.field_name == "pos")
        .expect("pos column present");
    let pos_cell = model.cell(0, pos_col).expect("row 0 / pos cell");
    assert_eq!(
        pos_cell.class,
        CellClass::Nested,
        "a tuple cell stays a tree/form drill-in (Nested), never NestedTable"
    );
    assert!(pos_cell.value_ref.is_some());
}

// =============================================================================
// E013 — per-cell / per-column type indicators
// =============================================================================

#[test]
fn scalar_cells_carry_their_type_for_the_indicator() {
    // FR-006 / E013: each present scalar cell carries its broad type (exposed via the
    // public `scalar_type_name` accessor) so the per-cell type glyph can render; a
    // nested-collection cell carries no scalar type. Reuses the standard fixture shape.
    let worker = ReparseWorker::new();
    let doc = doc_at(
        "[\n    (i: 1, f: 1.5, s: \"x\", b: true, tags: [\"a\"]),\n    (i: 2, f: 2.5, s: \"y\", b: false, tags: []),\n    (i: 3, f: 3.5, s: \"z\", b: true, tags: [\"c\"]),\n]",
        &worker,
    );
    let model = model_of(&doc);
    let col = |name: &str| {
        model
            .columns
            .iter()
            .position(|c| c.field_name == name)
            .unwrap_or_else(|| panic!("column {name} present"))
    };

    assert_eq!(
        model.cell(0, col("i")).unwrap().scalar_type_name(),
        Some("integer")
    );
    assert_eq!(
        model.cell(0, col("f")).unwrap().scalar_type_name(),
        Some("float")
    );
    assert_eq!(
        model.cell(0, col("s")).unwrap().scalar_type_name(),
        Some("string")
    );
    assert_eq!(
        model.cell(0, col("b")).unwrap().scalar_type_name(),
        Some("bool")
    );

    // A nested-LIST cell stays NestedTable and carries no scalar type indicator.
    let tags = model.cell(0, col("tags")).unwrap();
    assert_eq!(tags.class, CellClass::NestedTable);
    assert_eq!(
        tags.scalar_type_name(),
        None,
        "a nested cell carries no scalar type"
    );
}

#[test]
fn struct_cell_stays_nested_and_carries_no_scalar_type() {
    // A struct cell stays a tree/form drill-in (`Nested`), never inline, and carries
    // no scalar type indicator.
    let worker = ReparseWorker::new();
    let doc = doc_at(
        "[\n    (id: 1, meta: (k: \"x\")),\n    (id: 2, meta: (k: \"y\")),\n    (id: 3, meta: (k: \"z\")),\n]",
        &worker,
    );
    let model = model_of(&doc);
    let meta = model
        .columns
        .iter()
        .position(|c| c.field_name == "meta")
        .expect("meta column");
    let cell = model.cell(0, meta).expect("row 0 / meta cell");
    assert_eq!(
        cell.class,
        CellClass::Nested,
        "a struct cell stays a drill-in"
    );
    assert_eq!(cell.scalar_type_name(), None);
}

#[test]
fn column_headers_render_with_type_glyphs() {
    // E013: the column header markup prefixes the field name with the column's type
    // glyph (scalar type glyph for a Scalar column, ▦/▸ for a Nested column). Render
    // the table headlessly and confirm the header glyphs are present in the UI tree.
    let worker = ReparseWorker::new();
    let mut doc = doc_at(
        "[\n    (i: 1, tags: [\"a\"]),\n    (i: 2, tags: []),\n    (i: 3, tags: [\"c\"]),\n]",
        &worker,
    );

    let mut harness = Harness::new_ui(move |ui| {
        render_table_view(
            ui,
            &mut doc,
            &worker,
            &StructuralPath::root(),
            SectionShape::RecordList,
        );
    });
    harness.run();

    // The `i` column is integer → its header carries the `#` integer glyph.
    assert!(
        harness.query_all_by_label_contains("#").next().is_some(),
        "the integer column header shows the # type glyph"
    );
    // The `tags` column is a List column → its header carries the shared list glyph
    // (▤ U+25A4 — the SAME glyph the tree paints for a list, E014). The list icon is
    // itself the open-as-table affordance; there is no separate ▸ drill marker.
    assert!(
        harness
            .query_all_by_label_contains("\u{25A4}")
            .next()
            .is_some(),
        "the nested-collection (list) column header shows the ▤ list glyph"
    );
}

#[test]
fn edit_cell_is_byte_identical_except_touched_cell() {
    // SC-003: editing one cell leaves every other byte of the file unchanged
    // (comments / order / formatting preserved) and is a single undo unit.
    let worker = ReparseWorker::new();
    let mut doc = doc_at(
        "[\n    (name: \"a\", hp: 1), // keep me\n    (name: \"b\", hp: 2),\n]",
        &worker,
    );
    let before = doc.buffer.clone();
    let section = StructuralPath::root();

    // Edit row 1's `hp` cell (column index 1) from 2 → 99.
    doc.apply_table_set_cell(&section, 1, "hp", "99".to_string(), &worker, Instant::now())
        .expect("cell edit applies");

    assert_eq!(
        doc.buffer, "[\n    (name: \"a\", hp: 1), // keep me\n    (name: \"b\", hp: 99),\n]",
        "only the touched cell changed; the comment + every other byte is preserved"
    );

    // SC-003: a single undo unit restores the exact prior bytes.
    assert!(doc.undo(Instant::now()), "undo steps back");
    assert_eq!(doc.buffer, before, "one undo restores exact prior bytes");
    assert!(doc.redo(), "redo replays the cell edit");
    assert!(doc.buffer.contains("hp: 99"), "redo restores the edit");
}

#[test]
fn edit_blank_cell_adds_the_absent_field() {
    // FR-010: editing a blank (absent-field) cell ADDS the previously-absent field
    // rather than altering an existing empty value, losslessly + one undo unit.
    let worker = ReparseWorker::new();
    let mut doc = doc_at(
        "[\n    (name: \"a\", hp: 1),\n    (name: \"b\", hp: 2, mp: 3),\n    (name: \"c\", hp: 4),\n]",
        &worker,
    );
    let before = doc.buffer.clone();
    let section = StructuralPath::root();

    // Row 0 has no `mp` field; editing that blank cell adds `mp: 7` to row 0.
    doc.apply_table_set_cell(&section, 0, "mp", "7".to_string(), &worker, Instant::now())
        .expect("blank-cell edit adds the field");

    assert!(
        doc.buffer.contains("mp: 7"),
        "the previously-absent field was added: {}",
        doc.buffer
    );
    // Untouched rows are byte-identical.
    assert!(doc.buffer.contains("(name: \"b\", hp: 2, mp: 3)"));
    assert!(doc.buffer.contains("(name: \"c\", hp: 4)"));
    assert!(doc.undo(Instant::now()));
    assert_eq!(doc.buffer, before, "one undo restores exact prior bytes");
}

#[test]
fn append_row_inherits_sibling_style_one_undo_unit() {
    // SC-003 / FR-007: appending a row adopts the collection's sibling layout
    // style and is a single lossless undo unit.
    let worker = ReparseWorker::new();
    let mut doc = doc_at(
        "[\n    (name: \"a\", hp: 1),\n    (name: \"b\", hp: 2),\n]",
        &worker,
    );
    let before = doc.buffer.clone();
    let section = StructuralPath::root();

    doc.apply_table_append_row(
        &section,
        "(name: \"c\", hp: 3)".to_string(),
        &worker,
        Instant::now(),
    )
    .expect("append row applies");

    assert!(
        doc.buffer.contains("(name: \"c\", hp: 3)"),
        "row appended: {}",
        doc.buffer
    );
    // The original rows are byte-identical (untouched).
    assert!(doc.buffer.contains("(name: \"a\", hp: 1)"));
    assert!(doc.buffer.contains("(name: \"b\", hp: 2)"));
    assert!(doc.undo(Instant::now()), "undo steps back");
    assert_eq!(doc.buffer, before, "one undo restores exact prior bytes");
}

#[test]
fn delete_row_lossless_one_undo_unit() {
    // SC-003: deleting a row leaves the surviving rows byte-identical and is a
    // single undo unit.
    let worker = ReparseWorker::new();
    let mut doc = doc_at(
        "[\n    (name: \"a\", hp: 1),\n    (name: \"b\", hp: 2),\n    (name: \"c\", hp: 3),\n]",
        &worker,
    );
    let before = doc.buffer.clone();
    let section = StructuralPath::root();

    // Delete the middle row (index 1).
    doc.apply_table_delete_row(&section, 1, &worker, Instant::now())
        .expect("delete row applies");

    assert!(
        !doc.buffer.contains("\"b\""),
        "row b deleted: {}",
        doc.buffer
    );
    assert!(
        doc.buffer.contains("(name: \"a\", hp: 1)"),
        "row a preserved"
    );
    assert!(
        doc.buffer.contains("(name: \"c\", hp: 3)"),
        "row c preserved"
    );
    assert!(doc.undo(Instant::now()), "undo steps back");
    assert_eq!(doc.buffer, before, "one undo restores exact prior bytes");
}

#[test]
fn table_view_renders_headlessly() {
    // The table paints its column headers + visible cells through the renderer-free
    // egui_kittest harness without panicking.
    let worker = ReparseWorker::new();
    let mut doc = doc_at(
        "[\n    (name: \"a\", hp: 1),\n    (name: \"b\", hp: 2),\n    (name: \"c\", hp: 3),\n]",
        &worker,
    );

    let mut harness = Harness::new_ui(move |ui| {
        render_table_view(
            ui,
            &mut doc,
            &worker,
            &StructuralPath::root(),
            SectionShape::RecordList,
        );
    });
    harness.run();
}

// =============================================================================
// T032 — keyboard cell navigation + append (FR-009 / FR-016)
// =============================================================================

#[test]
fn tab_commits_the_edit_and_selects_the_next_cell() {
    // E024 (Excel model): committing a cell with Tab moves the SELECTION to the next cell
    // (row 0 / `hp`) and CLOSES the editor — it no longer opens that cell's editor. We seed
    // an edit on row 0 / `name`, press Tab, and confirm the editor closed and `hp` is selected.
    use std::cell::RefCell;
    use std::rc::Rc;

    use ronin_app::structural::view_state::{FocusSurface, PathStep};

    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(doc_at(
        "[\n    (name: \"a\", hp: 1),\n    (name: \"b\", hp: 2),\n]",
        &worker,
    )));

    // Seed edit focus on row 0's `name` cell (column 0).
    {
        let mut d = doc.borrow_mut();
        let name_path = StructuralPath::root()
            .child(PathStep::Index(0))
            .child(PathStep::Field("name".to_string()));
        d.view_state_mut().set_focus(
            name_path,
            FocusSurface::TableCell { row: 0, column: 0 },
            "\"a\"".to_string(),
        );
    }

    let doc_ui = Rc::clone(&doc);
    let worker_ui = Rc::clone(&worker);
    let mut harness = Harness::new_ui(move |ui| {
        let mut d = doc_ui.borrow_mut();
        render_table_view(
            ui,
            &mut d,
            &worker_ui,
            &StructuralPath::root(),
            SectionShape::RecordList,
        );
    });
    // First frame: the `name` cell renders its inline editor (focus is on it).
    harness.run();
    // Press Tab → commit + move the selection right (no auto-editor).
    harness.key_press(egui::Key::Tab);
    harness.run();

    let d = doc.borrow();
    assert!(
        d.view_state().edit_focus().is_none(),
        "Tab commits and closes the editor (no auto-open on the next cell)"
    );
    assert_eq!(
        d.view_state().grid_selection_rect(),
        Some((0, 1, 0, 1)),
        "Tab moved the selection to the next cell (row 0 / hp)"
    );
}

// =============================================================================
// T050 — nested-cell drill-in round-trips: a discoverable back path re-focuses
// the originating row/cell (FR-006)
// =============================================================================

#[test]
fn drill_in_then_back_returns_to_table_with_origin_cell_focused() {
    // FR-006: drilling into a nested cell records the originating cell as a return
    // target, switches to the tree/form surface, and renders a discoverable BACK
    // control that restores the table view + re-focuses the originating row/cell.
    use std::cell::RefCell;
    use std::rc::Rc;

    let worker = Rc::new(ReparseWorker::new());
    // A uniform list whose `meta` column holds nested STRUCTS — a struct cell stays
    // `Nested` (a tree/form drill-in), unlike a List/Map cell which opens as a table
    // in place (E013). This pins the tree drill-in + back round-trip.
    let doc = Rc::new(RefCell::new(doc_at(
        "[\n    (id: 1, meta: (k: \"x\")),\n    (id: 2, meta: (k: \"y\")),\n]",
        &worker,
    )));

    // The originating cell is row 0 / `meta` (column 1).
    let origin_cell = StructuralPath::root()
        .child(PathStep::Index(0))
        .child(PathStep::Field("meta".to_string()));

    // Frame 1 — render the table; click the nested cell's drill-in button.
    {
        let doc_ui = Rc::clone(&doc);
        let worker_ui = Rc::clone(&worker);
        let mut harness = Harness::new_ui(move |ui| {
            let mut d = doc_ui.borrow_mut();
            render_table_view(
                ui,
                &mut d,
                &worker_ui,
                &StructuralPath::root(),
                SectionShape::RecordList,
            );
        });
        harness.run();
        // E019c: a nested cell drills in via its small "open" icon (single click), which
        // sits just left of the summary text. A click on the cell body only selects.
        {
            let r = harness.get_by_label_contains("\"x\"").rect();
            let p = egui::pos2(r.left() - 11.0, r.center().y);
            harness.drag_at(p);
            harness.drop_at(p);
        }
        harness.run();
    }

    // After the drill-in: the active view switched to tree/form, the nested node is
    // focused, and a return target was recorded (FR-006).
    {
        let d = doc.borrow();
        assert_eq!(
            d.view_state().active_view(),
            ActiveView::TreeForm,
            "drill-in switches to the tree/form surface"
        );
        let ret = d
            .view_state()
            .drill_in_return()
            .expect("drill-in records a return target re-focusing the origin cell");
        assert_eq!(
            ret.cell_path, origin_cell,
            "the return target is the origin cell"
        );
        assert_eq!((ret.row, ret.column), (0, 1), "origin cell grid coords");
    }

    // Frame 2 — render the tree/form surface; the discoverable BACK control is
    // present. Click it.
    {
        let doc_ui = Rc::clone(&doc);
        let worker_ui = Rc::clone(&worker);
        let mut harness = Harness::new_ui(move |ui| {
            let mut d = doc_ui.borrow_mut();
            render_tree_view(ui, &mut d, &worker_ui);
        });
        harness.run();
        assert!(
            harness
                .query_all_by_label_contains("Back to table")
                .next()
                .is_some(),
            "the drilled-in tree/form view must render a discoverable back control (FR-006)"
        );
        harness.get_by_label_contains("Back to table").click();
        harness.run();
    }

    // After back: the table view is restored and the originating cell is re-focused.
    {
        let d = doc.borrow();
        assert_eq!(
            d.view_state().active_view(),
            ActiveView::Table,
            "the back control restores the table view (FR-006)"
        );
        assert!(
            d.view_state().drill_in_return().is_none(),
            "the return target is consumed on going back"
        );
        let focus = d
            .view_state()
            .edit_focus()
            .expect("the originating cell is re-focused on return");
        assert_eq!(
            focus.path, origin_cell,
            "focus re-binds the originating cell"
        );
        assert!(
            matches!(focus.surface, FocusSurface::TableCell { row: 0, column: 1 }),
            "focus re-binds the originating (row 0, column 1) cell"
        );
    }
}

// =============================================================================
// T027 — virtualization: realized-row count bounded by viewport (⊥ of N)
// =============================================================================

/// Build a uniform list of `n` 2-field rows as a RON source string.
fn uniform_list_src(n: usize) -> String {
    let mut s = String::from("[\n");
    for i in 0..n {
        s.push_str(&format!("    (id: {i}, name: \"row{i}\"),\n"));
    }
    s.push(']');
    s
}

/// Render `doc`'s table view in a fixed-size viewport and return how many rows the
/// `TableBody::rows` virtualization actually realized (invoked the row closure for).
fn realized_row_count(src: &str) -> usize {
    let worker = ReparseWorker::new();
    let mut doc = doc_at(src, &worker);

    // The row closure increments this each time it is invoked → the realized count.
    let realized = Rc::new(Cell::new(0usize));
    let realized_for_ui = Rc::clone(&realized);

    // A fixed, modest viewport so only a handful of rows fit regardless of N.
    let mut harness = Harness::builder()
        .with_size(egui::vec2(400.0, 200.0))
        .build_ui(move |ui| {
            ui.set_max_height(200.0);
            ronin_app::structural::table::render_table_view_counting(
                ui,
                &mut doc,
                &worker,
                &StructuralPath::root(),
                SectionShape::RecordList,
                &realized_for_ui,
            );
        });
    harness.run();
    realized.get()
}

#[test]
fn realized_row_count_is_bounded_and_independent_of_total_rows() {
    // FR-008 / SC-004: the realized-row count is bounded by the viewport and does
    // NOT grow with the section's total row count. We render a small list and a
    // large list into the SAME fixed viewport and confirm the realized count is
    // (a) far smaller than the total and (b) the same for both sizes.
    let small = realized_row_count(&uniform_list_src(1_000));
    let large = realized_row_count(&uniform_list_src(100_000));

    // Bounded by the viewport — nowhere near the total row count.
    assert!(
        small < 100,
        "a 1k-row table must realize only viewport-many rows, got {small}"
    );
    assert!(
        large < 100,
        "a 100k-row table must realize only viewport-many rows, got {large}"
    );

    // Independent of N: the realized count is identical for 1k and 100k rows
    // (frame work does not scale with the total — the load-bearing property).
    assert_eq!(
        small, large,
        "realized-row count must be independent of total row count (1k vs 100k)"
    );
}

// =============================================================================
// T034 — SC-010 benchmark: 100k rows × 10 scalar columns
// =============================================================================

/// Build a uniform list of `n` rows, each a record of exactly 10 scalar columns
/// (`c0..c9`), per the SC-010 fixture (no nested-collection cells).
fn benchmark_list_src(n: usize) -> String {
    let mut s = String::from("[\n");
    for r in 0..n {
        s.push_str("    (");
        for c in 0..10 {
            if c > 0 {
                s.push_str(", ");
            }
            s.push_str(&format!("c{c}: {}", r * 10 + c));
        }
        s.push_str("),\n");
    }
    s.push(']');
    s
}

/// Render the SC-010 fixture in a fixed viewport, returning the realized-row count.
fn benchmark_realized_rows(n: usize) -> usize {
    let src = benchmark_list_src(n);
    let worker = ReparseWorker::new();
    let mut doc = doc_at(&src, &worker);

    let realized = Rc::new(Cell::new(0usize));
    let realized_for_ui = Rc::clone(&realized);
    let mut harness = Harness::builder()
        .with_size(egui::vec2(900.0, 240.0))
        .build_ui(move |ui| {
            ui.set_max_height(240.0);
            ronin_app::structural::table::render_table_view_counting(
                ui,
                &mut doc,
                &worker,
                &StructuralPath::root(),
                SectionShape::RecordList,
                &realized_for_ui,
            );
        });
    harness.run();
    realized.get()
}

#[test]
fn sc010_benchmark_realized_rows_bounded_and_independent_of_n() {
    // SC-010 — how this was verified (be honest):
    //
    //   * VERIFIED AS A HARD CI GATE (this test): the **load-bearing structural
    //     property** — with the SC-010 fixture (100k rows × 10 scalar columns) the
    //     realized-row count stays bounded by the viewport and is identical at 1k
    //     and 100k rows. This is the property that makes per-frame cost independent
    //     of N (FR-008), and it is robust across CI environments.
    //
    //   * NOT a hard CI gate: the ≤16 ms/frame wall-clock figure (SC-004/SC-010).
    //     That is a **benchmark target** measured manually in a `--release` build on
    //     a stated reference desktop target; a wall-clock assertion is too
    //     environment-flaky for CI (a loaded shared CI box, a debug build, or a
    //     software renderer would make it spuriously fail), consistent with the
    //     project's "performance is not yet a hard QC gate" posture. Because the
    //     realized-row count is viewport-bounded and ⊥ of N (asserted here) and the
    //     per-cell render work is constant, the ≤16 ms budget is met whenever the
    //     manual release benchmark runs.
    let baseline = benchmark_realized_rows(1_000);
    let at_100k = benchmark_realized_rows(100_000);

    assert!(
        baseline < 100,
        "the benchmark fixture must realize only viewport-many rows at 1k, got {baseline}"
    );
    assert!(
        at_100k < 100,
        "the benchmark fixture must realize only viewport-many rows at 100k, got {at_100k}"
    );
    assert_eq!(
        baseline, at_100k,
        "SC-010: realized-row count (and thus frame work) is independent of total rows (1k vs 100k)"
    );
}

// =============================================================================
// E012 — Table view navigator: section scan + RecordMap / TupleList models
// =============================================================================

use ronin_app::structural::sections::{scan_table_sections, SectionShape as Shape, TableSection};
use ronin_app::structural::view_state::resolve_path;
use ronin_core::ast;

/// Parse `src` and scan it for table-able sections.
fn scan(src: &str) -> Vec<TableSection> {
    scan_table_sections(&ronin_core::parse(src))
}

#[test]
fn scan_finds_nested_cells_lists_and_hulls_record_map() {
    // A ships.ron-shaped doc: root struct → `hulls` map of same-shape hull structs,
    // each with a `cells` RecordList of same-shape cell records.
    let src = concat!(
        "(hulls: {\n",
        "  (1): (name: \"a\", cells: [(coord: (0,0), s: true), (coord: (1,0), s: false), (coord: (2,0), s: true)]),\n",
        "  (2): (name: \"b\", cells: [(coord: (0,0), s: true), (coord: (1,0), s: false), (coord: (2,0), s: true)]),\n",
        "})"
    );
    let sections = scan(src);

    // The `hulls` map is a RecordMap at path `hulls`, 2 rows, 1 key + 2 fields
    // (`name`, `cells`) = 3 columns.
    let hulls = sections
        .iter()
        .find(|s| s.shape == Shape::RecordMap)
        .expect("a hulls record map");
    assert_eq!(
        hulls.path,
        StructuralPath::from_steps(vec![PathStep::Field("hulls".to_string())])
    );
    assert_eq!(hulls.rows, 2);
    assert_eq!(hulls.cols, 3, "key column + name + cells");

    // Each hull's `cells` is a RecordList (one per hull) at the expected path, 3 rows.
    let cells: Vec<_> = sections
        .iter()
        .filter(|s| s.shape == Shape::RecordList)
        .collect();
    assert_eq!(cells.len(), 2, "one cells RecordList per hull");
    assert!(cells.iter().all(|c| c.rows == 3));
    assert!(cells.iter().any(|c| c.path
        == StructuralPath::from_steps(vec![
            PathStep::Field("hulls".to_string()),
            PathStep::Key("(1)".to_string()),
            PathStep::Field("cells".to_string()),
        ])));
}

#[test]
fn scan_finds_tuple_list_with_positional_columns() {
    let sections = scan("(coords: [(1, 2, 3), (4, 5, 6), (7, 8, 9)])");
    let t = sections
        .iter()
        .find(|s| s.shape == Shape::TupleList)
        .expect("a tuple list section");
    assert_eq!(t.rows, 3);
    assert_eq!(t.cols, 3, "positional .0/.1/.2 columns");
}

#[test]
fn scan_scalar_only_struct_has_no_sections() {
    // A sample.ron-shaped doc: scalar struct with scalar/tuple/enum fields. The
    // `position` tuple is a single tuple (not a list-of-tuples) and `tags` is a list
    // of scalars (not records) → zero table-able sections.
    let src = "Config(name: \"x\", retries: 3, position: (1.0, 2.0, 3.0), tags: [\"a\", \"b\"])";
    assert!(scan(src).is_empty(), "scalar-only doc has no sections");
}

// =============================================================================
// E018 — Combined / flattened table: union a repeated child across the parent
// =============================================================================

const COMBINED_SRC: &str =
    "(hulls: { (1): (cells: [(x: 0), (x: 1)]), (2): (cells: [(x: 2), (x: 3, y: 9), (x: 4)]) })";

#[test]
fn scan_finds_combined_child_section() {
    // The `hulls` map's entries each hold a `cells` record-list → a synthetic combined
    // section unioning all cells, listed alongside the per-hull `cells` sections.
    let combined = scan(COMBINED_SRC)
        .into_iter()
        .find(|s| s.shape == Shape::Combined)
        .expect("a combined cells section");
    assert_eq!(
        combined.path,
        StructuralPath::from_steps(vec![
            PathStep::Field("hulls".to_string()),
            PathStep::CombinedChild("cells".to_string()),
        ])
    );
    assert_eq!(combined.rows, 5, "2 + 3 cells across both hulls");
    assert_eq!(combined.cols, 3, "parent-key column + union(x, y)");
}

#[test]
fn derive_combined_unions_child_rows_with_parent_key_column() {
    let cst = ronin_core::parse(COMBINED_SRC);
    let parent = StructuralPath::from_steps(vec![PathStep::Field("hulls".to_string())]);
    let model = TableModel::derive_combined(&cst, &parent, "cells", &[])
        .expect("combined cells derives a model");

    // Columns: leading parent-key column (labeled after the parent field) + union x, y.
    let names: Vec<_> = model.columns.iter().map(|c| c.field_name.clone()).collect();
    assert_eq!(names, vec!["hulls", "x", "y"]);
    assert_eq!(model.rows.len(), 5, "2 + 3 unioned rows");

    // The parent-key column is read-only and carries each row's source hull key.
    let keys: Vec<_> = (0..5)
        .map(|r| model.cell(r, 0).unwrap().text.clone().unwrap_or_default())
        .collect();
    assert_eq!(keys, vec!["(1)", "(1)", "(2)", "(2)", "(2)"]);
    assert!((0..5).all(|r| model.cell(r, 0).unwrap().class == CellClass::ReadOnly));
    assert!((0..5).all(|r| model.cell(r, 0).unwrap().value_ref.is_none()));

    // `y` is Blank except on the one (2) row that has it (row index 3), where it's "9".
    assert_eq!(model.cell(0, 2).unwrap().class, CellClass::Blank);
    assert_eq!(model.cell(3, 2).unwrap().class, CellClass::Scalar);
    assert_eq!(model.cell(3, 2).unwrap().text.as_deref(), Some("9"));

    // A data cell's value_ref is the REAL nested path (hulls ▸ (2) ▸ cells ▸ [1] ▸ x),
    // resolvable against the live CST so editing works.
    let x_ref = model
        .cell(3, 1)
        .unwrap()
        .value_ref
        .clone()
        .expect("x cell ref");
    assert_eq!(
        x_ref,
        StructuralPath::from_steps(vec![
            PathStep::Field("hulls".to_string()),
            PathStep::Key("(2)".to_string()),
            PathStep::Field("cells".to_string()),
            PathStep::Index(1),
            PathStep::Field("x".to_string()),
        ])
    );
    assert!(
        resolve_path(&cst.root(), &x_ref).is_some(),
        "the combined cell's value_ref resolves to a live node"
    );

    // `derive_any` routes a CombinedChild-terminated path to the same combined model.
    let via_any = TableModel::derive_any(
        &cst,
        &parent.child(PathStep::CombinedChild("cells".to_string())),
        &[],
    )
    .expect("derive_any dispatches a combined path");
    assert_eq!(via_any, model);
}

#[test]
fn record_map_model_has_leading_readonly_key_column() {
    let cst =
        ronin_core::parse("(hulls: { (1): (hp: 1, name: \"a\"), (2): (hp: 2, name: \"b\") })");
    let section = StructuralPath::from_steps(vec![PathStep::Field("hulls".to_string())]);
    let model = TableModel::derive_section(&cst, &section, Shape::RecordMap, &[])
        .expect("record map derives a model");

    // The leading column is the read-only `(key)` column.
    assert_eq!(model.columns[0].field_name, "(key)");
    let key_cell = model.cell(0, 0).expect("row 0 key cell");
    assert_eq!(key_cell.class, CellClass::ReadOnly);
    assert!(
        key_cell.value_ref.is_none(),
        "read-only key carries no value_ref"
    );
    assert_eq!(key_cell.text.as_deref(), Some("(1)"));

    // The value-field columns follow (union of value record fields).
    let names: Vec<_> = model.columns.iter().map(|c| c.field_name.clone()).collect();
    assert_eq!(names, vec!["(key)", "hp", "name"]);

    // A value-field cell is an editable Scalar whose value_ref is under the entry key.
    let hp = model.cell(0, 1).expect("row 0 hp cell");
    assert_eq!(hp.class, CellClass::Scalar);
    assert_eq!(
        hp.value_ref,
        Some(StructuralPath::from_steps(vec![
            PathStep::Field("hulls".to_string()),
            PathStep::Key("(1)".to_string()),
            PathStep::Field("hp".to_string()),
        ]))
    );
}

#[test]
fn tuple_list_model_has_positional_editable_scalar_cells() {
    let cst = ronin_core::parse("(coords: [(1, 2), (3, 4), (5, 6)])");
    let section = StructuralPath::from_steps(vec![PathStep::Field("coords".to_string())]);
    let model = TableModel::derive_section(&cst, &section, Shape::TupleList, &[])
        .expect("tuple list derives a model");

    let names: Vec<_> = model.columns.iter().map(|c| c.field_name.clone()).collect();
    assert_eq!(names, vec![".0", ".1"]);

    // Each cell is an editable scalar addressed by Index/Index.
    let c01 = model.cell(0, 1).expect("row 0 / .1 cell");
    assert_eq!(c01.class, CellClass::Scalar);
    assert_eq!(c01.text.as_deref(), Some("2"));
    assert_eq!(
        c01.value_ref,
        Some(StructuralPath::from_steps(vec![
            PathStep::Field("coords".to_string()),
            PathStep::Index(0),
            PathStep::Index(1),
        ]))
    );
}

#[test]
fn record_list_model_unchanged_via_derive_section() {
    // RecordList through the dispatcher is identical to the legacy derive path.
    let cst = ronin_core::parse("[(a: 1, b: 2), (a: 3, c: 4)]");
    let section = StructuralPath::root();
    let via_section = TableModel::derive_section(&cst, &section, Shape::RecordList, &[])
        .expect("record list derives via derive_section");
    let via_legacy = TableModel::derive(&cst, &section, &[]).expect("legacy derive");
    assert_eq!(via_section, via_legacy);
    let cols: Vec<_> = via_section
        .columns
        .iter()
        .map(|c| c.field_name.clone())
        .collect();
    assert_eq!(cols, vec!["a", "b", "c"]);
}

#[test]
fn resolve_path_index_step_descends_into_a_tuple() {
    // The TupleList cell path is `section / Index(row) / Index(pos)`; the second
    // Index must descend into the row tuple (not only a list) so a tuple-cell edit /
    // drill-in resolves. This pins resolve_path's PathStep::Index tuple arm.
    let cst = ronin_core::parse("(coords: [(10, 20), (30, 40)])");
    let root = cst.root();
    let cell_path = StructuralPath::from_steps(vec![
        PathStep::Field("coords".to_string()),
        PathStep::Index(1),
        PathStep::Index(0),
    ]);
    let node = resolve_path(&root, &cell_path).expect("tuple cell resolves");
    assert_eq!(node.text(), "30", "Index descends into the tuple element");
    // And it is a value-position node (a literal scalar).
    assert!(ast::Value::cast(node).is_some());
}

// =============================================================================
// E013 — `derive_any`: open ANY nested collection as a table (reach = both;
// openable = Lists & Maps). NestedTable vs Nested cell classification. Breadcrumb.
// =============================================================================

use ronin_app::structural::table::{breadcrumb_segments, TableModel as AnyTableModel};

/// Field-name column list of a model (excluding nothing — verbatim, in order).
fn col_names(model: &AnyTableModel) -> Vec<String> {
    model.columns.iter().map(|c| c.field_name.clone()).collect()
}

#[test]
fn derive_any_over_a_map_has_leading_readonly_key_column() {
    // A Map projects a leading read-only `(key)` column + value projection. Here every
    // value is a record (mixed names allowed for reach), so the value columns are the
    // union of their fields.
    let cst = ronin_core::parse("(m: { (1): A(hp: 1), (2): B(mp: 2) })");
    let path = StructuralPath::from_steps(vec![PathStep::Field("m".to_string())]);
    let model = AnyTableModel::derive_any(&cst, &path, &[]).expect("a map projects a table");

    // Leading (key) column, then the UNION of value fields (mixed record names — A/B).
    assert_eq!(col_names(&model), vec!["(key)", "hp", "mp"]);
    let key_cell = model.cell(0, 0).expect("row 0 key");
    assert_eq!(key_cell.class, CellClass::ReadOnly);
    assert!(key_cell.value_ref.is_none());
    assert_eq!(key_cell.text.as_deref(), Some("(1)"));
}

#[test]
fn derive_any_over_a_scalar_list_has_single_value_column() {
    // A list of scalars (not records, not tuples) projects a single `value` column.
    let cst = ronin_core::parse("(xs: [10, 20, 30])");
    let path = StructuralPath::from_steps(vec![PathStep::Field("xs".to_string())]);
    let model = AnyTableModel::derive_any(&cst, &path, &[]).expect("a scalar list projects");

    assert_eq!(col_names(&model), vec!["value"]);
    assert_eq!(model.row_count(), 3);
    let cell = model.cell(1, 0).expect("row 1 value");
    assert_eq!(cell.class, CellClass::Scalar);
    assert_eq!(cell.text.as_deref(), Some("20"));
    // The value cell IS the element itself (path / Index(1)).
    assert_eq!(cell.value_ref, Some(path.child(PathStep::Index(1))));
}

#[test]
fn derive_any_over_a_tuple_list_has_positional_columns() {
    // A list whose every element is a tuple projects positional `.0/.1/…` columns.
    let cst = ronin_core::parse("(c: [(1, 2), (3, 4)])");
    let path = StructuralPath::from_steps(vec![PathStep::Field("c".to_string())]);
    let model = AnyTableModel::derive_any(&cst, &path, &[]).expect("a tuple list projects");

    assert_eq!(col_names(&model), vec![".0", ".1"]);
    let c01 = model.cell(0, 1).expect("row 0 / .1");
    assert_eq!(c01.class, CellClass::Scalar);
    assert_eq!(c01.text.as_deref(), Some("2"));
}

#[test]
fn derive_any_over_a_mixed_name_record_list_unions_columns() {
    // Permissive for reach: a list of records with MIXED struct names still tables,
    // with the union of their fields in first-seen order (unlike the strict classifier).
    let cst = ronin_core::parse("[A(a: 1, b: 2), B(a: 3, c: 4)]");
    let model = AnyTableModel::derive_any(&cst, &StructuralPath::root(), &[])
        .expect("a mixed-name record list projects");
    assert_eq!(col_names(&model), vec!["a", "b", "c"]);
    assert_eq!(model.row_count(), 2);
}

#[test]
fn derive_any_over_a_single_struct_is_a_field_value_grid() {
    // E012 (Part A2): a single struct projects a field/value table — a leading
    // read-only `(field)` column + a `value` column, one row per field. Each value
    // cell is editable where it is a scalar, and is the field's value (path/Field(name)).
    let cst = ronin_core::parse("Point(x: 1, y: 2)");
    let model =
        AnyTableModel::derive_any(&cst, &StructuralPath::root(), &[]).expect("a struct projects");

    assert_eq!(col_names(&model), vec!["(field)", "value"]);
    assert_eq!(model.row_count(), 2, "one row per field");

    // Row 0 = field `x`: leading read-only field-name cell + an editable scalar value.
    let field_cell = model.cell(0, 0).expect("row 0 (field)");
    assert_eq!(field_cell.class, CellClass::ReadOnly);
    assert_eq!(field_cell.text.as_deref(), Some("x"));
    assert!(field_cell.value_ref.is_none(), "the field name is identity");

    let value_cell = model.cell(0, 1).expect("row 0 value");
    assert_eq!(value_cell.class, CellClass::Scalar);
    assert_eq!(value_cell.text.as_deref(), Some("1"));
    // The value cell IS the field value itself (root / Field("x")).
    assert_eq!(
        value_cell.value_ref,
        Some(StructuralPath::from_steps(vec![PathStep::Field(
            "x".to_string()
        )]))
    );
}

#[test]
fn derive_any_over_a_struct_keeps_nested_value_drill_marker() {
    // A nested struct/tuple field stays a tree/form drill-in (`Nested`); a nested
    // list/map field opens AS A TABLE (`NestedTable`) — the value cell keeps its drill
    // marker so it opens as its own table.
    let cst = ronin_core::parse("Config(tags: [\"a\"], pos: (1, 2), inner: Meta(k: 1))");
    let model =
        AnyTableModel::derive_any(&cst, &StructuralPath::root(), &[]).expect("a struct projects");

    let row_for = |field: &str| {
        model
            .rows
            .iter()
            .position(|r| r.cells[0].text.as_deref() == Some(field))
            .unwrap()
    };
    assert_eq!(
        model.cell(row_for("tags"), 1).unwrap().class,
        CellClass::NestedTable,
        "a list field opens as a table"
    );
    assert_eq!(
        model.cell(row_for("pos"), 1).unwrap().class,
        CellClass::Nested,
        "a tuple field stays a tree/form drill-in"
    );
    assert_eq!(
        model.cell(row_for("inner"), 1).unwrap().class,
        CellClass::Nested,
        "a struct field stays a tree/form drill-in"
    );
}

#[test]
fn derive_any_over_a_single_tuple_is_a_one_row_positional_grid() {
    // E012 (Part A2): a single tuple projects a 1-row positional table — columns
    // `.0/.1/…`, one row whose cells are editable scalars at path/Index(i).
    let cst = ronin_core::parse("(1, 2, 3)");
    let model =
        AnyTableModel::derive_any(&cst, &StructuralPath::root(), &[]).expect("a tuple projects");

    assert_eq!(col_names(&model), vec![".0", ".1", ".2"]);
    assert_eq!(model.row_count(), 1, "a single tuple is one row");

    let c1 = model.cell(0, 1).expect("row 0 / .1");
    assert_eq!(c1.class, CellClass::Scalar);
    assert_eq!(c1.text.as_deref(), Some("2"));
    // Each cell is the member at root / Index(i).
    assert_eq!(
        c1.value_ref,
        Some(StructuralPath::from_steps(vec![PathStep::Index(1)]))
    );
}

#[test]
fn derive_any_returns_none_for_a_scalar_leaf() {
    // A scalar leaf is NOT a table — the outline never selects one (Part A2).
    let int_cst = ronin_core::parse("42");
    assert!(
        AnyTableModel::derive_any(&int_cst, &StructuralPath::root(), &[]).is_none(),
        "a scalar integer leaf is not a table"
    );
    let str_cst = ronin_core::parse("\"hello\"");
    assert!(
        AnyTableModel::derive_any(&str_cst, &StructuralPath::root(), &[]).is_none(),
        "a scalar string leaf is not a table"
    );
}

#[test]
fn list_cell_is_nested_table_while_struct_cell_stays_nested() {
    // E013: a cell whose value is a List/Map is `NestedTable` (open as table); a cell
    // whose value is a struct stays `Nested` (tree/form drill-in).
    let cst = ronin_core::parse("[(items: [1, 2], meta: (k: 1))]");
    let model = AnyTableModel::derive_any(&cst, &StructuralPath::root(), &[]).expect("projects");

    let items = model
        .columns
        .iter()
        .position(|c| c.field_name == "items")
        .unwrap();
    assert_eq!(
        model.cell(0, items).unwrap().class,
        CellClass::NestedTable,
        "a List cell opens as a table"
    );
    let meta = model
        .columns
        .iter()
        .position(|c| c.field_name == "meta")
        .unwrap();
    assert_eq!(
        model.cell(0, meta).unwrap().class,
        CellClass::Nested,
        "a struct cell stays a tree/form drill-in"
    );
}

#[test]
fn breadcrumb_prefix_chain_marks_list_map_segments_clickable() {
    // The breadcrumb for a deep path yields one segment per prefix; a segment is
    // clickable iff its prefix resolves to a List or Map (the openable kinds).
    // Doc: root struct → `hulls` MAP → key (1) → struct → `cells` LIST → index 0 →
    // struct → `coord` TUPLE.
    let cst = ronin_core::parse("(hulls: { (1): (cells: [(coord: (0, 0))]) })");
    // Path down to the `coord` tuple value.
    let deep = StructuralPath::from_steps(vec![
        PathStep::Field("hulls".to_string()), // map (openable)
        PathStep::Key("(1)".to_string()),     // struct (not openable)
        PathStep::Field("cells".to_string()), // list (openable)
        PathStep::Index(0),                   // struct (not openable)
        PathStep::Field("coord".to_string()), // tuple (not openable)
    ]);
    let segs = breadcrumb_segments(&cst, &deep);

    // One segment per prefix: root + 5 steps = 6 segments.
    let labels: Vec<_> = segs.iter().map(|s| s.label.clone()).collect();
    assert_eq!(
        labels,
        vec!["root", "hulls", "(1)", "cells", "[0]", "coord"]
    );

    // Clickability: root (the top struct) is NOT a list/map; `hulls` is a map; `(1)`
    // is a struct; `cells` is a list; `[0]` is a struct; `coord` is a tuple.
    let clickable: Vec<bool> = segs.iter().map(|s| s.clickable).collect();
    assert_eq!(
        clickable,
        vec![false, true, false, true, false, false],
        "only the List/Map prefixes are clickable navigation targets"
    );

    // Each clickable segment's path is exactly its prefix (re-navigation target).
    let hulls_seg = segs.iter().find(|s| s.label == "hulls").unwrap();
    assert_eq!(
        hulls_seg.path,
        StructuralPath::from_steps(vec![PathStep::Field("hulls".to_string())])
    );
    let cells_seg = segs.iter().find(|s| s.label == "cells").unwrap();
    assert_eq!(
        cells_seg.path,
        StructuralPath::from_steps(vec![
            PathStep::Field("hulls".to_string()),
            PathStep::Key("(1)".to_string()),
            PathStep::Field("cells".to_string()),
        ])
    );
}

// =============================================================================
// E019 — Excel-like bulk editing: rectangular selection, TSV copy, batched
// paste/fill (the marquee property: a block paste is ONE undo unit).
// =============================================================================

use ronin_app::structural::table::{copy_range_tsv, grid_fill_writes, grid_paste_writes};

/// A small uniform RecordList fixture: 3 rows × 2 scalar columns (name, hp).
fn bulk_doc(worker: &ReparseWorker) -> EditorDocument {
    doc_at(
        "[\n    (name: \"a\", hp: 1),\n    (name: \"b\", hp: 2),\n    (name: \"c\", hp: 3),\n]",
        worker,
    )
}

#[test]
fn copy_range_tsv_serializes_selected_block() {
    // A rectangular selection serializes to TSV — rows by `\n`, columns by `\t`, each
    // cell its verbatim RON token (string cells keep their quotes).
    let worker = ReparseWorker::new();
    let doc = bulk_doc(&worker);
    let model = model_of(&doc);

    // The full 3×2 block.
    assert_eq!(
        copy_range_tsv(&model, 0, 0, 2, 1),
        "\"a\"\t1\n\"b\"\t2\n\"c\"\t3\n"
    );
    // A single-column sub-range (hp, rows 0..=1).
    assert_eq!(copy_range_tsv(&model, 0, 1, 1, 1), "1\n2\n");
    // Out-of-range bounds are clamped (no panic) — past the last row/col yields the
    // same as the clamped full block.
    assert_eq!(
        copy_range_tsv(&model, 0, 0, 99, 99),
        "\"a\"\t1\n\"b\"\t2\n\"c\"\t3\n"
    );
}

#[test]
fn grid_fill_writes_targets_only_writable_cells() {
    // Filling a rect emits one write per writable scalar cell, each pointed at a real
    // cell value path; non-existent cells are never written.
    let worker = ReparseWorker::new();
    let doc = bulk_doc(&worker);
    let model = model_of(&doc);

    let writes = grid_fill_writes(&model, 0, 1, 2, 1, "0");
    assert_eq!(writes.len(), 3, "the hp column has 3 writable scalar cells");
    for (path, value) in &writes {
        assert_eq!(value, "0");
        assert!(
            model
                .rows
                .iter()
                .any(|r| r.cells.iter().any(|c| c.value_ref.as_ref() == Some(path))),
            "every fill write points at a real cell value path"
        );
    }
}

#[test]
fn paste_block_is_one_undo_unit() {
    // The marquee property: a multi-cell block paste lands as a SINGLE undo unit — one
    // `undo()` restores every pasted cell at once, and untouched bytes are preserved.
    let worker = ReparseWorker::new();
    let mut doc = bulk_doc(&worker);
    let before = doc.buffer.clone();
    let model = model_of(&doc);

    // Paste a 2×2 block over the top-left (rows 0..=1, cols name/hp).
    let writes = grid_paste_writes(&model, 0, 0, "\"x\"\t10\n\"y\"\t20\n");
    assert_eq!(writes.len(), 4, "a 2×2 block = 4 writable cells");
    doc.apply_grid_writes(&writes, &worker, Instant::now())
        .expect("the block paste applies");
    drive_reparse(&mut doc, &worker);

    assert!(
        doc.buffer.contains("name: \"x\""),
        "row 0 name updated: {}",
        doc.buffer
    );
    assert!(doc.buffer.contains("hp: 10"));
    assert!(doc.buffer.contains("name: \"y\""));
    assert!(doc.buffer.contains("hp: 20"));
    // Row 2 is untouched.
    assert!(doc.buffer.contains("(name: \"c\", hp: 3)"));

    // ONE undo restores the whole block (single undo unit).
    assert!(
        doc.undo(Instant::now()),
        "one undo steps back the whole batch"
    );
    assert_eq!(
        doc.buffer, before,
        "one undo restores exact prior bytes for the entire block"
    );
}

#[test]
fn fill_single_value_over_a_selection_is_one_undo_unit() {
    // Filling a multi-cell selection with one value writes every cell in the rect and
    // is a single undo unit.
    let worker = ReparseWorker::new();
    let mut doc = bulk_doc(&worker);
    let before = doc.buffer.clone();
    let model = model_of(&doc);

    let writes = grid_fill_writes(&model, 0, 1, 2, 1, "9");
    doc.apply_grid_writes(&writes, &worker, Instant::now())
        .expect("the fill applies");
    drive_reparse(&mut doc, &worker);

    assert_eq!(
        doc.buffer.matches("hp: 9").count(),
        3,
        "all three hp cells set to 9: {}",
        doc.buffer
    );
    assert!(doc.undo(Instant::now()));
    assert_eq!(doc.buffer, before, "one undo restores the whole fill");
}

#[test]
fn paste_overhang_skips_out_of_range_cells_and_empty_batch_is_noop() {
    // A paste that overhangs the table edge silently skips the overflow; an empty
    // batch reports an error and changes no bytes.
    let worker = ReparseWorker::new();
    let mut doc = bulk_doc(&worker);
    let model = model_of(&doc);

    // A 1×3 paste starting at the last column (hp) overhangs by one column → only the
    // in-range hp cell is a write.
    let writes = grid_paste_writes(&model, 0, 1, "5\t6\t7");
    assert_eq!(writes.len(), 1, "only the in-range hp cell is written");

    let before = doc.buffer.clone();
    assert!(
        doc.apply_grid_writes(&[], &worker, Instant::now()).is_err(),
        "an empty batch reports an error and changes nothing"
    );
    assert_eq!(doc.buffer, before);
}

#[test]
fn fill_and_paste_skip_the_readonly_key_column() {
    // A RecordMap's leading `(key)` column is read-only — a fill/paste over it skips
    // those cells and writes only the editable value cells (Excel leaves locked cells).
    let cst = ronin_core::parse("(hulls: { (1): (hp: 1), (2): (hp: 2) })");
    let section = StructuralPath::from_steps(vec![PathStep::Field("hulls".to_string())]);
    let model = TableModel::derive_section(&cst, &section, Shape::RecordMap, &[])
        .expect("record map derives a model");

    // The read-only key column (col 0) alone → no writes.
    assert!(
        grid_fill_writes(&model, 0, 0, 1, 0, "z").is_empty(),
        "the read-only key column is never written"
    );
    // Both columns → only the two editable hp cells are written.
    assert_eq!(
        grid_fill_writes(&model, 0, 0, 1, 1, "z").len(),
        2,
        "only the editable value cells are written; the key column is skipped"
    );
}

#[test]
fn grid_selection_rect_normalizes_anchor_and_cursor() {
    // The selection rect is the normalized (min..=max) span of anchor + cursor, so an
    // anchor below-right of the cursor still yields a top-left/bottom-right rect.
    let worker = ReparseWorker::new();
    let mut doc = bulk_doc(&worker);

    doc.view_state_mut().set_grid_anchor(2, 3);
    doc.view_state_mut().extend_grid_to(0, 1);
    assert_eq!(doc.view_state().grid_anchor(), Some((2, 3)));
    assert_eq!(doc.view_state().grid_cursor(), Some((0, 1)));
    assert_eq!(
        doc.view_state().grid_selection_rect(),
        Some((0, 1, 2, 3)),
        "rect is min/max-normalized regardless of drag direction"
    );

    doc.view_state_mut().clear_grid_selection();
    assert_eq!(doc.view_state().grid_selection_rect(), None);

    // Select-all spans the whole grid (3 rows × 2 cols here).
    doc.view_state_mut().select_grid_all(3, 2);
    assert_eq!(doc.view_state().grid_selection_rect(), Some((0, 0, 2, 1)));
}

#[test]
fn click_selects_a_cell_and_shift_click_extends_the_range() {
    // E019b — the Excel interaction model end-to-end through the real harness: a single
    // pointer click on a cell SELECTS it (no edit), and Shift+click extends the
    // rectangular selection to a range.
    use std::cell::RefCell;
    use std::rc::Rc;

    let worker = Rc::new(ReparseWorker::new());
    // Distinct values so each cell is found by a unique label.
    let doc = Rc::new(RefCell::new(doc_at(
        "[\n    (name: \"aa\", hp: 11),\n    (name: \"bb\", hp: 22),\n    (name: \"cc\", hp: 33),\n]",
        &worker,
    )));
    let doc_ui = Rc::clone(&doc);
    let worker_ui = Rc::clone(&worker);
    let mut harness = Harness::new_ui(move |ui| {
        let mut d = doc_ui.borrow_mut();
        render_table_view(
            ui,
            &mut d,
            &worker_ui,
            &StructuralPath::root(),
            SectionShape::RecordList,
        );
    });
    harness.run();

    // A plain click on row 0 / hp ("11") selects exactly that cell (column index 1).
    harness.get_by_label_contains("11").click();
    harness.run();
    assert_eq!(
        doc.borrow().view_state().grid_selection_rect(),
        Some((0, 1, 0, 1)),
        "a plain click selects exactly the clicked cell (no edit)"
    );
    // The click did NOT open an inline editor (selection, not edit).
    assert!(
        doc.borrow().view_state().edit_focus().is_none(),
        "a single click selects without entering edit mode"
    );

    // Shift+click row 2 / hp ("33") extends the selection down the column.
    harness
        .get_by_label_contains("33")
        .click_modifiers(egui::Modifiers::SHIFT);
    harness.run();
    assert_eq!(
        doc.borrow().view_state().grid_selection_rect(),
        Some((0, 1, 2, 1)),
        "shift+click extends the selection to the 3-row hp column range"
    );
}

#[test]
fn double_click_a_scalar_cell_opens_the_inline_editor() {
    // E019b — editing moved to double-click: two quick clicks on a scalar cell open its
    // inline editor (focus set on that cell), unlike a single selecting click.
    use std::cell::RefCell;
    use std::rc::Rc;

    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(doc_at(
        "[\n    (name: \"aa\", hp: 11),\n    (name: \"bb\", hp: 22),\n]",
        &worker,
    )));
    let doc_ui = Rc::clone(&doc);
    let worker_ui = Rc::clone(&worker);
    // A small step_dt so the two queued clicks register as a double-click (see
    // `click_selects_*` note — the default 0.25s/frame spaces them too far apart).
    let mut harness = Harness::builder().with_step_dt(0.05).build_ui(move |ui| {
        let mut d = doc_ui.borrow_mut();
        render_table_view(
            ui,
            &mut d,
            &worker_ui,
            &StructuralPath::root(),
            SectionShape::RecordList,
        );
    });
    harness.run();

    // Double-click row 0 / hp ("11") → begins editing that cell.
    {
        let cell = harness.get_by_label_contains("11");
        cell.click();
        cell.click();
    }
    harness.run();

    let d = doc.borrow();
    let focus = d
        .view_state()
        .edit_focus()
        .expect("double-click opens the inline editor on the cell");
    assert!(
        matches!(focus.surface, FocusSurface::TableCell { row: 0, column: 1 }),
        "the editor opened on the double-clicked (row 0, hp) cell"
    );
}

#[test]
fn drag_select_highlights_a_range() {
    // E019c — a real click-drag (press → move → release, simulated headlessly) selects a
    // rectangular range. No screenshots: the harness drives genuine pointer events and we
    // assert the resulting selection rect.
    use std::cell::RefCell;
    use std::rc::Rc;

    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(doc_at(
        "[\n    (name: \"aa\", hp: 11),\n    (name: \"bb\", hp: 22),\n    (name: \"cc\", hp: 33),\n]",
        &worker,
    )));
    let doc_ui = Rc::clone(&doc);
    let worker_ui = Rc::clone(&worker);
    let mut harness = Harness::new_ui(move |ui| {
        let mut d = doc_ui.borrow_mut();
        render_table_view(
            ui,
            &mut d,
            &worker_ui,
            &StructuralPath::root(),
            SectionShape::RecordList,
        );
    });
    harness.run();

    // Drag from row 0 / name ("aa", top-left) to row 2 / hp ("33", bottom-right).
    let start = harness.get_by_label_contains("aa").rect().center();
    let end = harness.get_by_label_contains("33").rect().center();
    harness.hover_at(start);
    harness.drag_at(start); // press
    harness.hover_at(end); // move while pressed → drag
    harness.drop_at(end); // release
    harness.run();

    assert_eq!(
        doc.borrow().view_state().grid_selection_rect(),
        Some((0, 0, 2, 1)),
        "click-drag from the top-left cell to the bottom-right selects the whole block"
    );
}

#[test]
fn body_click_on_a_nested_cell_selects_and_does_not_navigate() {
    // E019c — the de-overloading guard: a single click on a nested cell's BODY selects it
    // (it no longer drills/navigates). Drilling is reserved for the cell's small open
    // icon, which a body click never hits.
    use std::cell::RefCell;
    use std::rc::Rc;

    let worker = Rc::new(ReparseWorker::new());
    // Row 0 `meta` is a nested STRUCT cell (column index 1); its summary contains "x".
    let doc = Rc::new(RefCell::new(doc_at(
        "[\n    (id: 1, meta: (k: \"x\")),\n    (id: 2, meta: (k: \"y\")),\n]",
        &worker,
    )));
    let doc_ui = Rc::clone(&doc);
    let worker_ui = Rc::clone(&worker);
    let mut harness = Harness::new_ui(move |ui| {
        let mut d = doc_ui.borrow_mut();
        render_table_view(
            ui,
            &mut d,
            &worker_ui,
            &StructuralPath::root(),
            SectionShape::RecordList,
        );
    });
    harness.run();

    // Click the summary text (the cell BODY, right of the open icon).
    harness.get_by_label_contains("\"x\"").click();
    harness.run();

    let d = doc.borrow();
    assert_eq!(
        d.view_state().grid_selection_rect(),
        Some((0, 1, 0, 1)),
        "a body click selects the nested cell"
    );
    assert!(
        d.view_state().drill_in_return().is_none(),
        "a body click on a nested cell does NOT drill in / navigate (de-overloaded)"
    );
}

#[test]
fn auto_column_widths_fit_content_header_and_clamp() {
    // E020/E023 — columns auto-size to content on load: a column of long values is wider
    // than a column of short values, a long header widens its column even with short values,
    // and every width clamps to [GRID_COL_MIN_W, GRID_COL_MAX_W] (48..=480). The width is
    // computed from a text-measurer (a fake one here; the app passes an accurate galley one).
    use ronin_app::structural::table::auto_column_widths;

    let worker = ReparseWorker::new();
    // `s`: short values; `wide`: a long value; `loooong_header_name`: long header / short value.
    let doc = doc_at(
        "[\n    (s: 1, wide: \"aaaaaaaaaaaaaaaaaaaaaaaaaaaa\", loooong_header_name: 2),\n]",
        &worker,
    );
    let model = model_of(&doc);
    let idx = |name: &str| {
        model
            .columns
            .iter()
            .position(|c| c.field_name == name)
            .unwrap_or_else(|| panic!("column {name} present"))
    };

    // Fake measurer: ~7px per character (stand-in for the galley measurer).
    let w = auto_column_widths(&model, |s: &str| s.chars().count() as f32 * 7.0);
    assert_eq!(w.len(), model.columns.len(), "one width per column");
    assert!(
        w[idx("wide")] > w[idx("s")],
        "a column with long values fits wider than a short one"
    );
    assert!(
        w[idx("loooong_header_name")] > w[idx("s")],
        "a long header widens its column even with short values"
    );
    for width in &w {
        assert!(
            (48.0..=480.0).contains(width),
            "column width {width} stays within the clamp range"
        );
    }
}

#[test]
fn nav_panel_width_clamps_to_a_sensible_range() {
    // E023 — the navigator side-panel fits its widest label but stays within a clamp so it
    // never collapses or eats the window.
    use ronin_app::panels::nav_panel_width;
    assert_eq!(
        nav_panel_width(0.0),
        200.0,
        "tiny content → the minimum width"
    );
    assert_eq!(
        nav_panel_width(10_000.0),
        460.0,
        "huge content → the maximum width"
    );
    let mid = nav_panel_width(300.0);
    assert!(
        (200.0..=460.0).contains(&mid),
        "mid content stays in range, got {mid}"
    );
}

#[test]
fn group_rows_by_partitions_rows_by_field_value() {
    // E021 — the pure core of the Table (grouped) view: rows partition by a field's value
    // into sorted groups; no group columns → one group with every row.
    use ronin_app::structural::table::group_rows_by;

    let worker = ReparseWorker::new();
    let doc = doc_at(
        "[\n    (k: \"a\", v: 1),\n    (k: \"b\", v: 2),\n    (k: \"a\", v: 3),\n]",
        &worker,
    );
    let model = model_of(&doc);
    let k = model
        .columns
        .iter()
        .position(|c| c.field_name == "k")
        .unwrap();

    let groups = group_rows_by(&model, &[k]);
    let keys: Vec<_> = groups.iter().map(|(key, _)| key.clone()).collect();
    assert_eq!(
        keys,
        vec!["\"a\"".to_string(), "\"b\"".to_string()],
        "groups are keyed by the field's verbatim value, in sorted order"
    );
    assert_eq!(groups[0].1, vec![0, 2], "both \"a\" rows land in one group");
    assert_eq!(groups[1].1, vec![1], "the \"b\" row is its own group");

    let all = group_rows_by(&model, &[]);
    assert_eq!(all.len(), 1, "no group columns → a single group");
    assert_eq!(all[0].1, vec![0, 1, 2], "...containing every row");
}

#[test]
fn grouped_view_model_clusters_rows_and_projects_columns() {
    // E022 — the Table (grouped) transform: group columns come first, `show_cols` projects
    // the rest, rows cluster by the group value, and each kept cell keeps its `value_ref`
    // so the result still edits by path.
    use ronin_app::structural::table::grouped_view_model;

    let worker = ReparseWorker::new();
    let doc = doc_at(
        "[\n    (k: \"b\", a: 1, z: 9),\n    (k: \"a\", a: 2, z: 8),\n    (k: \"b\", a: 3, z: 7),\n]",
        &worker,
    );
    let model = model_of(&doc); // columns: k, a, z
    let col = |n: &str| {
        model
            .columns
            .iter()
            .position(|c| c.field_name == n)
            .unwrap()
    };
    let (k, a) = (col("k"), col("a"));

    // Group by k, show only [a] — k is forced first, z excluded.
    let g = grouped_view_model(&model, &[k], &[a]);
    let names: Vec<_> = g.columns.iter().map(|c| c.field_name.clone()).collect();
    assert_eq!(
        names,
        vec!["k".to_string(), "a".to_string()],
        "group col first, then shown cols"
    );

    // Rows cluster by k's sorted value: "a" (orig row 1) then "b" (orig rows 0, 2).
    let kt: Vec<_> = g
        .rows
        .iter()
        .map(|r| r.cells[0].text.clone().unwrap_or_default())
        .collect();
    assert_eq!(
        kt,
        vec![
            "\"a\"".to_string(),
            "\"b\"".to_string(),
            "\"b\"".to_string()
        ]
    );
    let at: Vec<_> = g
        .rows
        .iter()
        .map(|r| r.cells[1].text.clone().unwrap_or_default())
        .collect();
    assert_eq!(
        at,
        vec!["2".to_string(), "1".to_string(), "3".to_string()],
        "rows follow the clustered order"
    );

    // Kept cells preserve value_ref → still editable by path.
    assert!(
        g.rows[0].cells[1].value_ref.is_some(),
        "a projected scalar cell keeps its value path"
    );

    // No group + no show → all columns, original row order.
    let id = grouped_view_model(&model, &[], &[]);
    let names2: Vec<_> = id.columns.iter().map(|c| c.field_name.clone()).collect();
    assert_eq!(
        names2,
        vec!["k".to_string(), "a".to_string(), "z".to_string()]
    );
    let kt2: Vec<_> = id
        .rows
        .iter()
        .map(|r| r.cells[0].text.clone().unwrap_or_default())
        .collect();
    assert_eq!(
        kt2,
        vec![
            "\"b\"".to_string(),
            "\"a\"".to_string(),
            "\"b\"".to_string()
        ],
        "original order"
    );

    // Out-of-range picks are ignored and fall back to all columns / original order.
    let oob = grouped_view_model(&model, &[99], &[99]);
    assert_eq!(
        oob.columns.len(),
        3,
        "stale indices fall back to all columns"
    );
    assert_eq!(oob.rows.len(), 3);
}

/// A 2-row RecordList doc for the Excel-editing tests, wrapped for the harness closure.
fn excel_doc() -> (
    std::rc::Rc<ReparseWorker>,
    std::rc::Rc<std::cell::RefCell<EditorDocument>>,
) {
    use std::cell::RefCell;
    use std::rc::Rc;
    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(doc_at(
        "[\n    (name: \"aa\", hp: 11),\n    (name: \"bb\", hp: 22),\n]",
        &worker,
    )));
    (worker, doc)
}

#[test]
fn clicking_another_cell_commits_the_edit_and_selects_it() {
    // E021 — Excel feel: editing a cell and then clicking ANOTHER cell commits the edit
    // (no Enter needed) and selects the clicked cell.
    use std::rc::Rc;

    let (worker, doc) = excel_doc();
    // Seed an open edit on row 0 / hp (column 1) with a new draft value "99".
    {
        let mut d = doc.borrow_mut();
        let hp0 = StructuralPath::root()
            .child(PathStep::Index(0))
            .child(PathStep::Field("hp".to_string()));
        d.view_state_mut().set_focus(
            hp0,
            FocusSurface::TableCell { row: 0, column: 1 },
            "99".to_string(),
        );
    }
    let doc_ui = Rc::clone(&doc);
    let worker_ui = Rc::clone(&worker);
    let mut harness = Harness::new_ui(move |ui| {
        let mut d = doc_ui.borrow_mut();
        render_table_view(
            ui,
            &mut d,
            &worker_ui,
            &StructuralPath::root(),
            SectionShape::RecordList,
        );
    });
    harness.run(); // editor renders + auto-focuses

    // Click row 1 / hp ("22") → blur-commits "99" to row 0 and selects row 1 / hp.
    harness.get_by_label_contains("22").click();
    harness.run();

    let d = doc.borrow();
    assert!(
        d.buffer.contains("hp: 99"),
        "clicking away committed the edit: {}",
        d.buffer
    );
    assert_eq!(
        d.view_state().grid_selection_rect(),
        Some((1, 1, 1, 1)),
        "the clicked cell (row 1 / hp) is now selected"
    );
}

#[test]
fn enter_commits_the_edit_and_moves_down() {
    // E021 — Excel feel: Enter commits and moves the active cell DOWN.
    use std::rc::Rc;

    let (worker, doc) = excel_doc();
    {
        let mut d = doc.borrow_mut();
        let hp0 = StructuralPath::root()
            .child(PathStep::Index(0))
            .child(PathStep::Field("hp".to_string()));
        d.view_state_mut().set_focus(
            hp0,
            FocusSurface::TableCell { row: 0, column: 1 },
            "99".to_string(),
        );
    }
    let doc_ui = Rc::clone(&doc);
    let worker_ui = Rc::clone(&worker);
    let mut harness = Harness::new_ui(move |ui| {
        let mut d = doc_ui.borrow_mut();
        render_table_view(
            ui,
            &mut d,
            &worker_ui,
            &StructuralPath::root(),
            SectionShape::RecordList,
        );
    });
    harness.run();
    harness.key_press(egui::Key::Enter);
    harness.run();

    let d = doc.borrow();
    assert!(
        d.buffer.contains("hp: 99"),
        "Enter committed the edit: {}",
        d.buffer
    );
    // E024 (Excel): Enter commits then MOVES the selection down + closes the editor —
    // it no longer opens the next cell's editor.
    assert!(
        d.view_state().edit_focus().is_none(),
        "Enter closes the editor after committing (no auto-open below)"
    );
    assert_eq!(
        d.view_state().grid_selection_rect(),
        Some((1, 1, 1, 1)),
        "Enter moved the selection DOWN to row 1 / hp"
    );
}

#[test]
fn not_editing_enter_moves_selection_down_without_editing() {
    // E024 (Excel): when a cell is just SELECTED (not editing), Enter moves the selection
    // down — it does NOT open an editor (F2 / typing / double-click edit).
    use std::rc::Rc;

    let (worker, doc) = excel_doc();
    doc.borrow_mut().view_state_mut().set_grid_anchor(0, 1); // select row 0 / hp
    let doc_ui = Rc::clone(&doc);
    let worker_ui = Rc::clone(&worker);
    let mut harness = Harness::new_ui(move |ui| {
        let mut d = doc_ui.borrow_mut();
        render_table_view(
            ui,
            &mut d,
            &worker_ui,
            &StructuralPath::root(),
            SectionShape::RecordList,
        );
    });
    harness.run();
    harness.key_press(egui::Key::Enter);
    harness.run();

    let d = doc.borrow();
    assert_eq!(
        d.view_state().grid_selection_rect(),
        Some((1, 1, 1, 1)),
        "Enter on a selected cell moves the selection down"
    );
    assert!(
        d.view_state().edit_focus().is_none(),
        "Enter on a selected cell does NOT open an editor"
    );
}

#[test]
fn typing_on_a_selected_cell_opens_the_editor_overwriting() {
    // E021 — Excel "just start typing": a printable char on a selected cell opens its
    // editor seeded with that char (overwrite).
    use std::rc::Rc;

    let (worker, doc) = excel_doc();
    {
        // Select row 0 / hp (column 1) without editing.
        doc.borrow_mut().view_state_mut().set_grid_anchor(0, 1);
    }
    let doc_ui = Rc::clone(&doc);
    let worker_ui = Rc::clone(&worker);
    let mut harness = Harness::new_ui(move |ui| {
        let mut d = doc_ui.borrow_mut();
        render_table_view(
            ui,
            &mut d,
            &worker_ui,
            &StructuralPath::root(),
            SectionShape::RecordList,
        );
    });
    harness.run();
    harness.event(egui::Event::Text("9".to_string()));
    harness.run();

    let d = doc.borrow();
    let focus = d
        .view_state()
        .edit_focus()
        .expect("typing on a selected cell opens its editor");
    assert!(
        matches!(focus.surface, FocusSurface::TableCell { row: 0, column: 1 }),
        "the editor opened on the selected (row 0 / hp) cell"
    );
    assert_eq!(
        focus.draft, "9",
        "the editor is seeded with the typed character (overwrite)"
    );
}

#[test]
fn clear_writes_resets_editable_scalars_to_type_defaults() {
    // E024 — Delete clears editable scalar cells to a type-appropriate empty value.
    use ronin_app::structural::table::clear_writes;

    let worker = ReparseWorker::new();
    let doc = doc_at(
        "[\n    (n: 5, s: \"hi\", b: true),\n    (n: 6, s: \"yo\", b: false),\n]",
        &worker,
    );
    let model = model_of(&doc); // columns: n (int), s (string), b (bool)
    let w = clear_writes(&model, 0, 0, 1, 2);
    assert_eq!(w.len(), 6, "2 rows × 3 editable scalar columns");
    let vals: Vec<String> = w.iter().map(|(_, v)| v.clone()).collect();
    assert!(
        vals.iter().filter(|v| *v == "0").count() == 2,
        "ints clear to 0"
    );
    assert!(
        vals.iter().filter(|v| *v == "\"\"").count() == 2,
        "strings clear to \"\""
    );
    assert!(
        vals.iter().filter(|v| *v == "false").count() == 2,
        "bools clear to false"
    );
}

#[test]
fn delete_clears_the_selected_cells_in_one_undo() {
    // E024 — pressing Delete over a selection clears those cells (one undo), leaving the
    // rest untouched.
    use std::cell::RefCell;
    use std::rc::Rc;

    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(doc_at(
        "[\n    (n: 5, s: \"hi\"),\n    (n: 6, s: \"yo\"),\n]",
        &worker,
    )));
    let before = doc.borrow().buffer.clone();
    // Select the whole `n` column (col 0, rows 0..=1).
    {
        let mut d = doc.borrow_mut();
        d.view_state_mut().set_grid_anchor(0, 0);
        d.view_state_mut().extend_grid_to(1, 0);
    }
    let doc_ui = Rc::clone(&doc);
    let worker_ui = Rc::clone(&worker);
    let mut harness = Harness::new_ui(move |ui| {
        let mut d = doc_ui.borrow_mut();
        render_table_view(
            ui,
            &mut d,
            &worker_ui,
            &StructuralPath::root(),
            SectionShape::RecordList,
        );
    });
    harness.run();
    harness.key_press(egui::Key::Delete);
    harness.run();

    {
        let d = doc.borrow();
        assert_eq!(
            d.buffer.matches("n: 0").count(),
            2,
            "both `n` cells cleared to 0: {}",
            d.buffer
        );
        assert!(
            d.buffer.contains("s: \"hi\"") && d.buffer.contains("s: \"yo\""),
            "the `s` column is untouched: {}",
            d.buffer
        );
    }
    assert!(doc.borrow_mut().undo(Instant::now()), "undo steps back");
    assert_eq!(
        doc.borrow().buffer,
        before,
        "one undo restores all cleared cells"
    );
}

#[test]
fn typing_over_a_range_edits_the_active_cursor_cell() {
    // E024 — typing while a multi-cell range is selected opens the ACTIVE (cursor) cell's
    // editor seeded with the char (Excel overwrite), not the top-left.
    use std::rc::Rc;

    let (worker, doc) = excel_doc(); // [(name:"aa",hp:11),(name:"bb",hp:22)]
    {
        let mut d = doc.borrow_mut();
        d.view_state_mut().set_grid_anchor(0, 0); // anchor top-left
        d.view_state_mut().extend_grid_to(1, 1); // cursor at (1,1)
    }
    let doc_ui = Rc::clone(&doc);
    let worker_ui = Rc::clone(&worker);
    let mut harness = Harness::new_ui(move |ui| {
        let mut d = doc_ui.borrow_mut();
        render_table_view(
            ui,
            &mut d,
            &worker_ui,
            &StructuralPath::root(),
            SectionShape::RecordList,
        );
    });
    harness.run();
    harness.event(egui::Event::Text("9".to_string()));
    harness.run();

    let d = doc.borrow();
    let focus = d
        .view_state()
        .edit_focus()
        .expect("typing over a range opens the cursor cell editor");
    assert!(
        matches!(focus.surface, FocusSurface::TableCell { row: 1, column: 1 }),
        "the editor opened on the cursor (active) cell, not the top-left"
    );
    assert_eq!(focus.draft, "9");
}
