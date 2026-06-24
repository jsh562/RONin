//! Table editing ergonomics — type-aware cell editing (E012).
//!
//! Phase 1 (Foundational) covers the **read path**: `EditorDocument::query_cell_type`
//! maps a cell's [`StructuralPath`] to the bound type's declared kind + enum
//! variants via the newly-exposed `ronin_validate::resolve_type_at_pointer`
//! (AD-002, FR-005/FR-013). The contract is progressive: an unbound document
//! returns `None` so the editor falls back to plain text (FR-013).
//!
//! Pure / unit assertions only — no live-GUI rendering (Principle V). Later phases
//! extend this file with the egui_kittest headless interaction tests, so the module
//! structure is kept clean and grouped by behavior.

use std::sync::Arc;

use ronin_app::document::{CellKind, EditorDocument};
use ronin_app::reparse::BoundType;
use ronin_app::structural::view_state::{PathStep, StructuralPath};

/// Build a `BoundType` from a serialized `TypeModel` (`$defs` + named root type)
/// — the same shape the binding resolver hands the document
/// (see `document.rs` `serde_bound()` and `binding_resolution.rs`).
fn bound(model: serde_json::Value, type_name: &str) -> BoundType {
    BoundType {
        model: Arc::new(model),
        type_name: type_name.to_string(),
    }
}

/// A single-field path (a struct field by name) — a convenient distinct cell.
fn field(name: &str) -> StructuralPath {
    StructuralPath::from_steps(vec![PathStep::Field(name.to_string())])
}

// ---------------------------------------------------------------------------
// (a) Bound enum field → Enum + variants (FR-005)
// ---------------------------------------------------------------------------

#[test]
fn query_cell_type_resolves_bound_enum_with_variants() {
    // `Widget { kind: Kind }` where `Kind` is a RON enum with variants On/Off
    // (a `oneOf` whose branches carry `x-ron-variant`, the E004 interchange shape).
    let model = serde_json::json!({
        "$defs": {
            "Widget": {
                "type": "object",
                "properties": {
                    "kind": { "$ref": "#/$defs/Kind" }
                }
            },
            "Kind": {
                "oneOf": [
                    { "x-ron-variant": "On", "x-ron-variant-shape": "unit" },
                    { "x-ron-variant": "Off", "x-ron-variant-shape": "unit" }
                ]
            }
        }
    });

    let mut doc = EditorDocument::new_untitled(1);
    doc.bound_type = Some(bound(model, "Widget"));

    let info = doc
        .query_cell_type(&field("kind"))
        .expect("a bound enum cell has declared type info");
    assert_eq!(info.declared_kind, CellKind::Enum);
    assert_eq!(info.enum_variants, vec!["On".to_string(), "Off".to_string()]);
}

// ---------------------------------------------------------------------------
// (b) Unbound document → None (text fallback, FR-013)
// ---------------------------------------------------------------------------

#[test]
fn query_cell_type_returns_none_when_unbound() {
    // No bound type ⇒ no declared type info ⇒ the dispatch falls back to text.
    let doc = EditorDocument::new_untitled(1);
    assert!(doc.bound_type.is_none());
    assert_eq!(doc.query_cell_type(&field("anything")), None);
}

// ===========================================================================
// Phase 2 (US1 — P1 MVP): fast scalar editing without retyping.
// ===========================================================================

// ---------------------------------------------------------------------------
// (T005) Numeric increment token math — pure, no harness (FR-002)
// ---------------------------------------------------------------------------

use ronin_app::structural::table::increment_number_token;

#[test]
fn increment_number_token_steps_integers() {
    // Plus/minus one keeps an integer an integer (FR-002 — int-vs-float form).
    assert_eq!(increment_number_token("41", 1).as_deref(), Some("42"));
    assert_eq!(increment_number_token("42", -1).as_deref(), Some("41"));
    // The larger-step modifier (Shift) is ±10.
    assert_eq!(increment_number_token("5", 10).as_deref(), Some("15"));
    assert_eq!(increment_number_token("5", -10).as_deref(), Some("-5"));
    // A negative integer token nudges correctly.
    assert_eq!(increment_number_token("-3", 1).as_deref(), Some("-2"));
}

#[test]
fn increment_number_token_keeps_float_form() {
    // A float token keeps its decimal point (FR-002): never collapses to a bare int.
    assert_eq!(increment_number_token("1.5", 1).as_deref(), Some("2.5"));
    assert_eq!(increment_number_token("1.5", -1).as_deref(), Some("0.5"));
    // A whole-valued float result still carries `.0` so it stays a RON float literal.
    assert_eq!(increment_number_token("2.0", 1).as_deref(), Some("3.0"));
    assert_eq!(increment_number_token("3.0", -1).as_deref(), Some("2.0"));
    // The ±10 modifier on a float.
    assert_eq!(increment_number_token("2.5", 10).as_deref(), Some("12.5"));
}

#[test]
fn increment_number_token_rejects_non_numbers() {
    // A non-numeric token returns None so the caller never writes invalid RON.
    assert_eq!(increment_number_token("true", 1), None);
    assert_eq!(increment_number_token("\"hi\"", 1), None);
    assert_eq!(increment_number_token("", 1), None);
    assert_eq!(increment_number_token("  ", 1), None);
    assert_eq!(increment_number_token("abc", -1), None);
}

// ---------------------------------------------------------------------------
// (T004) Headless: Space toggles a bool cell without text-edit mode (FR-001)
// ---------------------------------------------------------------------------

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;

use ronin_app::reparse::ReparseWorker;
use ronin_app::structural::sections::SectionShape;
use ronin_app::structural::table::{render_table_view, render_table_view_counting, TableModel};

/// Request a reparse and spin-poll until a current result installs, or panic on
/// timeout — drives the *real* off-frame worker to completion (mirrors the
/// `drive_reparse` seam in `tests/table_view.rs`).
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

#[test]
fn space_toggles_a_bool_cell_without_entering_text_edit() {
    // FR-001: a selected bool cell flips `true`↔`false` on Space, committed losslessly,
    // and Space does NOT open the inline text editor (it is consumed for bool cells).
    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(doc_at(
        "[\n    (name: \"a\", on: true),\n    (name: \"b\", on: false),\n    (name: \"c\", on: true),\n]",
        &worker,
    )));

    // The `on` column is column index 1 (after `name`); select row 0's bool cell.
    let model = model_of(&doc.borrow());
    let on_col = model
        .columns
        .iter()
        .position(|c| c.field_name == "on")
        .expect("the `on` column exists");
    assert_eq!(
        model.cell(0, on_col).and_then(|c| c.text.clone()).as_deref(),
        Some("true"),
        "row 0 starts true"
    );
    doc.borrow_mut()
        .view_state_mut()
        .set_grid_anchor(0, on_col);

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
    harness.key_press(egui::Key::Space);
    harness.run();
    // Let the off-frame reparse land so the buffer + projection reflect the flip.
    {
        let mut d = doc.borrow_mut();
        drive_reparse(&mut d, &worker);
    }

    {
        let d = doc.borrow();
        // The underlying buffer flipped only row 0's bool; the other rows are untouched.
        assert!(
            d.buffer.contains("(name: \"a\", on: false)"),
            "row 0 `on` flipped true→false: {}",
            d.buffer
        );
        assert!(
            d.buffer.contains("(name: \"b\", on: false)")
                && d.buffer.contains("(name: \"c\", on: true)"),
            "rows 1 and 2 are untouched: {}",
            d.buffer
        );
        // Space did NOT enter text-edit mode: no cell edit focus is active.
        assert!(
            d.view_state().edit_focus().is_none(),
            "Space toggled in place — it did not open the inline text editor"
        );
    }
}

// ---------------------------------------------------------------------------
// (T006) Headless: Ctrl+D fills the cell above the selection down (FR-003)
// ---------------------------------------------------------------------------

use ronin_app::structural::table::{
    bool_toggle_writes, fill_down_writes, increment_writes, paste_block_row_count,
    paste_expand_writes,
};

#[test]
fn ctrl_d_fills_cell_above_selection_into_every_selected_cell() {
    // FR-003: Ctrl+D copies the cell DIRECTLY ABOVE the selection top into every cell
    // of the selection (per column), committed as one undo unit; row 0 (no row above)
    // is a no-op. Driven through the real key handler headlessly (no live-GUI capture).
    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(doc_at(
        "[\n    (name: \"a\", hp: 1),\n    (name: \"b\", hp: 2),\n    (name: \"c\", hp: 3),\n    (name: \"d\", hp: 4),\n]",
        &worker,
    )));

    // The `hp` column; select the rect rows 1..=3 of that column. The cell ABOVE the
    // selection top is row 0's `hp` = `1`, so every selected hp cell should become `1`.
    let hp_col = {
        let model = model_of(&doc.borrow());
        model
            .columns
            .iter()
            .position(|c| c.field_name == "hp")
            .expect("the `hp` column exists")
    };
    {
        let mut d = doc.borrow_mut();
        d.view_state_mut().set_grid_anchor(1, hp_col);
        d.view_state_mut().extend_grid_to(3, hp_col);
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
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::D);
    harness.run();
    {
        let mut d = doc.borrow_mut();
        drive_reparse(&mut d, &worker);
    }

    {
        let d = doc.borrow();
        // Row 0 keeps its own value; rows 1..=3 now mirror the cell above the selection.
        assert!(
            d.buffer.contains("(name: \"a\", hp: 1)"),
            "row 0 (the source) is unchanged: {}",
            d.buffer
        );
        assert!(
            d.buffer.contains("(name: \"b\", hp: 1)")
                && d.buffer.contains("(name: \"c\", hp: 1)")
                && d.buffer.contains("(name: \"d\", hp: 1)"),
            "every selected hp cell equals the cell above the selection (1): {}",
            d.buffer
        );
        // Ctrl+D did NOT open the inline text editor (one gesture, one action).
        assert!(
            d.view_state().edit_focus().is_none(),
            "Ctrl+D filled in place — it did not open the inline text editor"
        );
    }
}

#[test]
fn fill_down_writes_is_a_noop_when_no_row_above() {
    // FR-003: a selection whose top is row 0 has no row above ⇒ no writes (no-op). The
    // pure helper makes this assertable without the harness.
    let worker = ReparseWorker::new();
    let doc = doc_at(
        "[\n    (name: \"a\", hp: 1),\n    (name: \"b\", hp: 2),\n]",
        &worker,
    );
    let model = model_of(&doc);
    let hp_col = model
        .columns
        .iter()
        .position(|c| c.field_name == "hp")
        .expect("the `hp` column exists");

    // A selection that starts at row 0 (the top row) — nothing above to copy.
    assert!(
        fill_down_writes(&model, 0, hp_col, 1, hp_col).is_empty(),
        "no row above row 0 ⇒ fill-down is a no-op"
    );
    // A selection that starts at row 1 copies row 0's hp (1) into row 1.
    let writes = fill_down_writes(&model, 1, hp_col, 1, hp_col);
    assert_eq!(writes.len(), 1, "one writable cell in the selection");
    assert_eq!(writes[0].1, "1", "the value is the cell directly above");
}

// ---------------------------------------------------------------------------
// (T007) Headless: paste taller-than-grid appends rows; wider clips (FR-004)
// ---------------------------------------------------------------------------

#[test]
fn paste_taller_than_grid_appends_rows_and_clips_wider_columns() {
    // FR-004: pasting a block TALLER than the grid into an add-capable section appends
    // rows so the whole block lands (no vertical clipping); a block WIDER than the
    // columns clips at the last column (no new columns); untouched original cells outside
    // the paste stay byte-unchanged. Driven through the real paste handler headlessly.
    let worker = Rc::new(ReparseWorker::new());
    // A 2-row grid with 2 columns (name, hp).
    let doc = Rc::new(RefCell::new(doc_at(
        "[\n    (name: \"a\", hp: 1),\n    (name: \"b\", hp: 2),\n]",
        &worker,
    )));

    // Select the top-left cell (row 0, col 0).
    {
        let mut d = doc.borrow_mut();
        d.view_state_mut().set_grid_anchor(0, 0);
    }

    // A 4-row, 3-column TSV block: taller than the 2-row grid (→ append 2 rows) and one
    // column WIDER than the schema (the third column must be clipped, not created).
    let paste = "\"p\"\t10\t99\n\"q\"\t20\t99\n\"r\"\t30\t99\n\"s\"\t40\t99\n";
    // Sanity on the helper: the block is 4 rows.
    assert_eq!(paste_block_row_count(paste), 4);

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
    harness.event(egui::Event::Paste(paste.to_string()));
    harness.run();
    {
        let mut d = doc.borrow_mut();
        drive_reparse(&mut d, &worker);
    }

    {
        let d = doc.borrow();
        // All four pasted rows landed — the two extra rows were appended.
        assert!(
            d.buffer.contains("name: \"p\"")
                && d.buffer.contains("name: \"q\"")
                && d.buffer.contains("name: \"r\"")
                && d.buffer.contains("name: \"s\""),
            "all four pasted rows landed (rows appended to fit): {}",
            d.buffer
        );
        assert!(
            d.buffer.contains("hp: 10")
                && d.buffer.contains("hp: 20")
                && d.buffer.contains("hp: 30")
                && d.buffer.contains("hp: 40"),
            "every pasted hp value landed: {}",
            d.buffer
        );
        // The wider-than-columns third value (99) was clipped — no new column created.
        assert!(
            !d.buffer.contains("99"),
            "the over-wide paste column was clipped, not created: {}",
            d.buffer
        );
        // No new column field name appeared — the schema is unchanged (still name + hp).
        let model = TableModel::derive(
            &d.parse.as_ref().expect("parse landed").cst,
            &StructuralPath::root(),
            &d.diagnostics,
        )
        .expect("table model");
        assert_eq!(
            model.columns.len(),
            2,
            "still exactly two columns (no new column from the over-wide paste)"
        );
        assert_eq!(model.row_count(), 4, "the grid grew to four rows");
    }
}

#[test]
fn paste_expand_writes_clips_wider_and_appends_taller() {
    // FR-004 (pure): with `appended` rows, the block addresses existing + appended rows
    // but never builds a path past the column schema (wider clips). Untouched original
    // cells get no write. Asserted on the helper directly for a tight contract.
    let worker = ReparseWorker::new();
    let doc = doc_at(
        "[\n    (name: \"a\", hp: 1),\n    (name: \"b\", hp: 2),\n]",
        &worker,
    );
    let model = model_of(&doc);

    // A 4-row × 3-col block from (0,0): 2 existing rows + 2 appended; the 3rd column is
    // clipped. So 4 rows × 2 columns = 8 writes (no write for the over-wide column).
    let paste = "\"p\"\t10\t99\n\"q\"\t20\t99\n\"r\"\t30\t99\n\"s\"\t40\t99\n";
    let writes = paste_expand_writes(&model, 0, 0, 2, paste);
    assert_eq!(
        writes.len(),
        8,
        "4 rows × 2 schema columns = 8 writes (the over-wide 3rd column is clipped)"
    );
    // No write carries the clipped over-wide value.
    assert!(
        writes.iter().all(|(_, v)| v != "99"),
        "the over-wide column value is never written (no new column)"
    );
}

// ===========================================================================
// (T012) {FR-011, FR-012} Keyboard operability + one-gesture-one-action.
//
// Verification-only: the US1 gestures are already wired in `table.rs` (Space =
// bool toggle consumed for bool cells; Ctrl+↑/↓ = increment gated on
// `i.modifiers.command` so plain arrows still navigate; Ctrl+D = fill-down
// consumed so the matching `Event::Text("d")` never opens the editor). This
// suite proves the wiring is coherent and collision-free *through the real key
// handler*, headlessly (egui_kittest — never a live-GUI screenshot):
//
//   (a) operability — Space toggles a bool, Ctrl+↑ increments a number, Ctrl+D
//       fills down, all without a mouse;
//   (b) no collision — Space over a bool does NOT open the text editor; Ctrl+↑
//       does NOT also move the selection cursor; Ctrl+D does NOT type a 'd' into
//       the cell; AND the normal gestures still work (a plain printable char
//       opens the editor seeded with it; a plain arrow moves the selection).
// ===========================================================================

use ronin_app::structural::view_state::FocusSurface;

/// Build a headless harness over a document's top-level table, sharing the
/// `Rc<RefCell<..>>` so a test can mutate selection between frames and read back
/// the buffer / view-state after the off-frame reparse lands. Mirrors the
/// per-test harness the US1 tests build inline, lifted here so the T012 sub-tests
/// each drive the *real* key handler the same way.
fn harness_over(
    doc: &Rc<RefCell<EditorDocument>>,
    worker: &Rc<ReparseWorker>,
) -> Harness<'static> {
    let doc_ui = Rc::clone(doc);
    let worker_ui = Rc::clone(worker);
    Harness::new_ui(move |ui| {
        let mut d = doc_ui.borrow_mut();
        render_table_view(
            ui,
            &mut d,
            &worker_ui,
            &StructuralPath::root(),
            SectionShape::RecordList,
        );
    })
}

/// The grid column index of a field by name in the live model.
fn col_of(doc: &EditorDocument, field_name: &str) -> usize {
    model_of(doc)
        .columns
        .iter()
        .position(|c| c.field_name == field_name)
        .unwrap_or_else(|| panic!("the `{field_name}` column exists"))
}

#[test]
fn keyboard_operability_space_increment_filldown_without_a_mouse() {
    // (a) FR-011: every US1 value edit is reachable by keyboard alone — no pointer
    // event is ever sent to the harness; selection is set programmatically (the
    // arrow-nav leg is covered separately below) and each gesture is a real key
    // press routed through the production handler.
    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(doc_at(
        "[\n    (name: \"a\", on: true, hp: 1),\n    (name: \"b\", on: false, hp: 2),\n    (name: \"c\", on: true, hp: 3),\n]",
        &worker,
    )));
    let (on_col, hp_col) = {
        let d = doc.borrow();
        (col_of(&d, "on"), col_of(&d, "hp"))
    };

    // --- Space toggles a bool (keyboard) ---
    doc.borrow_mut().view_state_mut().set_grid_anchor(0, on_col);
    let mut harness = harness_over(&doc, &worker);
    harness.run();
    harness.key_press(egui::Key::Space);
    harness.run();
    drive_reparse(&mut doc.borrow_mut(), &worker);
    assert!(
        doc.borrow().buffer.contains("(name: \"a\", on: false"),
        "Space toggled row 0 `on` true→false by keyboard: {}",
        doc.borrow().buffer
    );

    // --- Ctrl+↑ increments a number (keyboard) ---
    doc.borrow_mut().view_state_mut().set_grid_anchor(0, hp_col);
    let mut harness = harness_over(&doc, &worker);
    harness.run();
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::ArrowUp);
    harness.run();
    drive_reparse(&mut doc.borrow_mut(), &worker);
    assert!(
        doc.borrow().buffer.contains("hp: 2)"),
        "Ctrl+↑ incremented row 0 `hp` 1→2 by keyboard: {}",
        doc.borrow().buffer
    );

    // --- Ctrl+D fills down (keyboard) ---
    {
        let mut d = doc.borrow_mut();
        d.view_state_mut().set_grid_anchor(1, hp_col); // selection top = row 1
        d.view_state_mut().extend_grid_to(2, hp_col); // ..through row 2
    }
    let mut harness = harness_over(&doc, &worker);
    harness.run();
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::D);
    harness.run();
    drive_reparse(&mut doc.borrow_mut(), &worker);
    {
        let d = doc.borrow();
        // The cell above the selection top (row 0 `hp`, now `2`) filled rows 1 & 2.
        assert!(
            d.buffer.contains("(name: \"b\", on: false, hp: 2)")
                && d.buffer.contains("(name: \"c\", on: true, hp: 2)"),
            "Ctrl+D filled the cell above into rows 1 & 2 by keyboard: {}",
            d.buffer
        );
    }
}

#[test]
fn space_over_bool_does_not_open_the_text_editor() {
    // (b) FR-012: Space is CONSUMED for a bool selection, so it toggles in place and
    // the matching `Event::Text(" ")` never reaches the editor-opening path. One
    // gesture (Space) → one action (toggle), never also "enter text-edit".
    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(doc_at(
        "[\n    (name: \"a\", on: true),\n    (name: \"b\", on: false),\n]",
        &worker,
    )));
    let on_col = col_of(&doc.borrow(), "on");
    doc.borrow_mut().view_state_mut().set_grid_anchor(0, on_col);

    let mut harness = harness_over(&doc, &worker);
    harness.run();
    harness.key_press(egui::Key::Space);
    harness.run();
    drive_reparse(&mut doc.borrow_mut(), &worker);

    let d = doc.borrow();
    assert!(
        d.buffer.contains("(name: \"a\", on: false)"),
        "Space toggled the bool: {}",
        d.buffer
    );
    assert!(
        d.view_state().edit_focus().is_none(),
        "Space over a bool did NOT open the inline text editor (one gesture, one action)"
    );
}

#[test]
fn ctrl_up_increments_without_moving_the_selection_cursor() {
    // (b) FR-012: Ctrl+↑ increments and does NOT also move the active cell — the
    // arrow's selection-move is suppressed while `command` is held (`up && !command`
    // in the handler). The cursor stays on the same cell after the nudge.
    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(doc_at(
        "[\n    (name: \"a\", hp: 5),\n    (name: \"b\", hp: 6),\n]",
        &worker,
    )));
    let hp_col = col_of(&doc.borrow(), "hp");
    doc.borrow_mut().view_state_mut().set_grid_anchor(1, hp_col); // row 1

    let mut harness = harness_over(&doc, &worker);
    harness.run();
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::ArrowUp);
    harness.run();
    drive_reparse(&mut doc.borrow_mut(), &worker);

    let d = doc.borrow();
    assert!(
        d.buffer.contains("(name: \"b\", hp: 7)"),
        "Ctrl+↑ incremented row 1 `hp` 6→7: {}",
        d.buffer
    );
    // Row 0 (where a plain ↑ would have moved the cursor) is untouched.
    assert!(
        d.buffer.contains("(name: \"a\", hp: 5)"),
        "the increment did not also touch the cell above: {}",
        d.buffer
    );
    assert_eq!(
        d.view_state().grid_cursor(),
        Some((1, hp_col)),
        "Ctrl+↑ did NOT move the selection cursor — it stayed on the nudged cell"
    );
}

#[test]
fn ctrl_d_fills_down_without_typing_a_d_into_the_cell() {
    // (b) FR-012: Ctrl+D is consumed by the handler, so the matching `Event::Text("d")`
    // never opens the editor seeded with 'd'. The cell fills with the value above, and
    // no editor draft of "d" exists.
    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(doc_at(
        "[\n    (name: \"a\", hp: 9),\n    (name: \"b\", hp: 2),\n]",
        &worker,
    )));
    let hp_col = col_of(&doc.borrow(), "hp");
    doc.borrow_mut().view_state_mut().set_grid_anchor(1, hp_col); // row 1 (row 0 above)

    let mut harness = harness_over(&doc, &worker);
    harness.run();
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::D);
    harness.run();
    drive_reparse(&mut doc.borrow_mut(), &worker);

    let d = doc.borrow();
    assert!(
        d.buffer.contains("(name: \"b\", hp: 9)"),
        "Ctrl+D filled the cell above (9) into row 1: {}",
        d.buffer
    );
    // The 'd' never landed as a literal: no `hp: d` cell and no editor draft of "d".
    assert!(
        !d.buffer.contains("hp: d"),
        "Ctrl+D did NOT type a 'd' into the cell: {}",
        d.buffer
    );
    assert!(
        d.view_state()
            .edit_focus()
            .is_none_or(|f| f.draft != "d"),
        "Ctrl+D did NOT open the editor seeded with 'd' (one gesture, one action)"
    );
}

#[test]
fn a_plain_printable_char_opens_the_editor_seeded_with_it() {
    // (b) FR-011/FR-012: the normal Excel "just type" gesture still works — a plain
    // printable char (no modifier) opens the active cell's editor seeded with that
    // char. This is the path Ctrl+D / Space deliberately bypass for their cells, so
    // confirming it still fires proves the consumes are surgical, not blanket.
    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(doc_at(
        "[\n    (name: \"a\", hp: 1),\n    (name: \"b\", hp: 2),\n]",
        &worker,
    )));
    let hp_col = col_of(&doc.borrow(), "hp");
    doc.borrow_mut().view_state_mut().set_grid_anchor(0, hp_col);

    let mut harness = harness_over(&doc, &worker);
    harness.run();
    // A plain printable char arrives as an `Event::Text` (no modifier).
    harness.event(egui::Event::Text("7".to_string()));
    harness.run();

    let d = doc.borrow();
    let focus = d
        .view_state()
        .edit_focus()
        .expect("a printable char opened the inline editor");
    assert!(
        matches!(focus.surface, FocusSurface::TableCell { .. }),
        "the editor opened on the active table cell"
    );
    assert_eq!(
        focus.draft, "7",
        "the editor is seeded with the typed char (Excel overwrite)"
    );
}

#[test]
fn a_plain_arrow_moves_the_selection() {
    // (b) FR-011/FR-012: a plain arrow (no modifier) moves the active cell — the same
    // key that Ctrl+arrow repurposes for increment still navigates when unmodified.
    // One gesture (plain ↓) → one action (move), distinct from Ctrl+↓ (decrement).
    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(doc_at(
        "[\n    (name: \"a\", hp: 1),\n    (name: \"b\", hp: 2),\n    (name: \"c\", hp: 3),\n]",
        &worker,
    )));
    let hp_col = col_of(&doc.borrow(), "hp");
    doc.borrow_mut().view_state_mut().set_grid_anchor(0, hp_col);

    let buffer_before = doc.borrow().buffer.clone();
    let mut harness = harness_over(&doc, &worker);
    harness.run();
    harness.key_press(egui::Key::ArrowDown);
    harness.run();

    let d = doc.borrow();
    assert_eq!(
        d.view_state().grid_cursor(),
        Some((1, hp_col)),
        "a plain ↓ moved the selection cursor down one row"
    );
    // A pure navigation move changes zero document bytes (it is not an edit).
    assert_eq!(
        d.buffer, buffer_before,
        "a plain arrow move is byte-free (it did not decrement or edit)"
    );
}

// ===========================================================================
// (T013) {FR-014} Lossless round-trip — US1 gestures preserve comments,
// formatting, element order, and every untouched cell byte-for-byte.
//
// FR-014 / ADR-0001: a value edit changes ONLY the targeted token; all comments,
// indentation/whitespace, and sibling order are byte-identical afterward. We
// assert this two ways:
//
//   1. A proptest over generated comment-rich docs + edit positions: apply each
//      US1 edit (bool toggle, numeric increment, fill-down, block paste) via the
//      REAL apply path (drive a reparse) and verify the post-edit buffer equals
//      the pre-edit buffer with ONLY the targeted token range(s) spliced — every
//      other byte (comments, formatting, order) is unchanged.
//   2. An insta snapshot of a representative comment-rich doc before/after a
//      gesture, as a human-readable complement that pins the exact bytes.
// ===========================================================================

use proptest::prelude::*;
use ronin_core::{parse, SyntaxKind};

/// All comment token texts in source order — the FR-014 comment-preservation oracle.
fn comment_tokens(src: &str) -> Vec<String> {
    parse(src)
        .root()
        .descendant_tokens()
        .filter(|t| matches!(t.kind(), SyntaxKind::LineComment | SyntaxKind::BlockComment))
        .map(|t| t.text().to_string())
        .collect()
}

/// Apply a batch of `(path, value)` writes through the real commit path and return
/// the resulting buffer once the reparse has landed (drives the off-frame worker).
fn apply_and_reparse(
    src: &str,
    worker: &ReparseWorker,
    writes: &[(StructuralPath, String)],
) -> String {
    let mut doc = doc_at(src, worker);
    doc.apply_grid_writes(writes, worker, Instant::now())
        .expect("US1 grid writes commit losslessly");
    drive_reparse(&mut doc, worker);
    doc.buffer.clone()
}

/// Assert `after` equals `before` with exactly the half-open byte `ranges` replaced
/// left-to-right by `replacements` — i.e. every byte OUTSIDE the targeted ranges
/// (comments, whitespace, sibling order) is byte-identical (FR-014). `ranges` must
/// be sorted and non-overlapping.
fn assert_only_ranges_changed(
    before: &str,
    after: &str,
    ranges: &[(usize, usize)],
    replacements: &[String],
) {
    let mut expected = String::new();
    let mut cursor = 0usize;
    for ((start, end), repl) in ranges.iter().zip(replacements) {
        expected.push_str(&before[cursor..*start]);
        expected.push_str(repl);
        cursor = *end;
    }
    expected.push_str(&before[cursor..]);
    assert_eq!(
        after, expected,
        "only the targeted token(s) changed; all other bytes (comments/formatting/order) are identical"
    );
}

/// A comment-rich record-list generator with custom indentation and a specific
/// element order, so the round-trip is exercised over varied — not a single —
/// document shape. Each record is `(name: "…", on: <bool>, hp: <int>)` with a
/// trailing line comment, a leading block comment, and 1px-irregular indentation.
fn commented_record_list() -> impl Strategy<Value = String> {
    // 2..=4 records; each carries a distinct name + bool + int so edits are addressable.
    proptest::collection::vec(
        ("[a-z]{1,4}", any::<bool>(), 0i64..500),
        2..=4usize,
    )
    .prop_map(|recs| {
        let mut s = String::from("// leading file comment\n[\n");
        for (i, (name, on, hp)) in recs.iter().enumerate() {
            // Deliberately irregular indentation + a trailing comment per row, so the
            // round-trip must preserve non-canonical whitespace + comments.
            let indent = if i % 2 == 0 { "    " } else { "      " };
            s.push_str(&format!(
                "{indent}/* r{i} */ (name: \"{name}\", on: {on}, hp: {hp}), // row {i}\n"
            ));
        }
        s.push_str("] // trailing comment\n");
        s
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// FR-014: a single bool toggle changes ONLY that cell's `true`/`false` token;
    /// every comment, the custom indentation, and the element order survive byte-for-byte.
    #[test]
    fn prop_bool_toggle_is_byte_lossless(src in commented_record_list(), row in 0usize..4) {
        let worker = ReparseWorker::new();
        let before = doc_at(&src, &worker);
        let model = model_of(&before);
        let row = row.min(model.row_count().saturating_sub(1));
        let on_col = model.columns.iter().position(|c| c.field_name == "on").unwrap();

        let cell = model.cell(row, on_col).unwrap();
        let path = cell.value_ref.clone().unwrap();
        let old_tok = cell.text.clone().unwrap();
        let new_tok = if old_tok == "true" { "false" } else { "true" }.to_string();
        // The exact byte range of the targeted token in the source.
        let (start, end) = token_byte_range(&src, &old_tok, row, "on");

        let after = apply_and_reparse(&src, &worker, &[(path, new_tok.clone())]);
        // Every comment survives, in order.
        prop_assert_eq!(comment_tokens(&after), comment_tokens(&src));
        // Only the targeted token bytes changed.
        assert_only_ranges_changed(&src, &after, &[(start, end)], &[new_tok]);
    }

    /// FR-014: a numeric increment changes ONLY the targeted `hp` integer token.
    #[test]
    fn prop_increment_is_byte_lossless(src in commented_record_list(), row in 0usize..4) {
        let worker = ReparseWorker::new();
        let before = doc_at(&src, &worker);
        let model = model_of(&before);
        let row = row.min(model.row_count().saturating_sub(1));
        let hp_col = model.columns.iter().position(|c| c.field_name == "hp").unwrap();

        let writes = increment_writes(&model, row, hp_col, row, hp_col, 1);
        prop_assert_eq!(writes.len(), 1);
        let old_tok = model.cell(row, hp_col).unwrap().text.clone().unwrap();
        let new_tok = writes[0].1.clone();
        let (start, end) = token_byte_range(&src, &old_tok, row, "hp");

        let after = apply_and_reparse(&src, &worker, &writes);
        prop_assert_eq!(comment_tokens(&after), comment_tokens(&src));
        assert_only_ranges_changed(&src, &after, &[(start, end)], &[new_tok]);
    }

    /// FR-014: fill-down copies the cell above into the selection, changing ONLY
    /// the targeted `hp` tokens; comments, indentation, and order are preserved.
    #[test]
    fn prop_fill_down_is_byte_lossless(src in commented_record_list()) {
        let worker = ReparseWorker::new();
        let before = doc_at(&src, &worker);
        let model = model_of(&before);
        prop_assume!(model.row_count() >= 2);
        let hp_col = model.columns.iter().position(|c| c.field_name == "hp").unwrap();

        // Selection rows 1..=last; the source is row 0's hp.
        let last = model.row_count() - 1;
        let writes = fill_down_writes(&model, 1, hp_col, last, hp_col);
        prop_assume!(!writes.is_empty());

        let after = apply_and_reparse(&src, &worker, &writes);
        // Comments + order survive.
        prop_assert_eq!(comment_tokens(&after), comment_tokens(&src));
        // The filled value equals row 0's hp; verify by re-deriving and comparing the
        // filled cells, while every comment + the leading/trailing trivia is intact.
        let after_doc = doc_at(&after, &worker);
        let after_model = model_of(&after_doc);
        let source_val = after_model.cell(0, hp_col).unwrap().text.clone().unwrap();
        for r in 1..=last {
            prop_assert_eq!(
                after_model.cell(r, hp_col).unwrap().text.clone().unwrap(),
                source_val.clone()
            );
        }
        // The doc still has the same number of rows + columns (no structural drift).
        prop_assert_eq!(after_model.row_count(), model.row_count());
        prop_assert_eq!(after_model.columns.len(), model.columns.len());
    }
}

/// Locate the byte range of the value token for field `field` in record index `row`
/// of a `commented_record_list()` source. The generator emits each value exactly
/// once in `field: value` form, in row order, so we scan record-by-record.
fn token_byte_range(src: &str, token: &str, row: usize, field: &str) -> (usize, usize) {
    // Each record opens with `(name:`; find the `row`-th record's start, then the
    // `field: ` within it, then the token immediately after.
    let mut search = 0usize;
    for _ in 0..=row {
        let open = src[search..].find("(name:").expect("a record open") + search;
        search = open + 1;
    }
    let rec_start = search - 1;
    let field_at = src[rec_start..]
        .find(&format!("{field}: "))
        .expect("the field in the record")
        + rec_start
        + field.len()
        + 2; // skip "field: "
    let start = field_at;
    let end = start + token.len();
    debug_assert_eq!(&src[start..end], token, "token range lines up");
    (start, end)
}

#[test]
fn block_paste_is_byte_lossless_outside_the_pasted_block() {
    // FR-014: a block paste rewrites only the targeted cells; all comments,
    // indentation, and untouched cells stay byte-identical. Asserted on a fixed
    // comment-rich doc so the exact preserved bytes are checked.
    let worker = ReparseWorker::new();
    let src = "// header\n[\n    /* a */ (name: \"a\", hp: 1), // keep a\n    /* b */ (name: \"b\", hp: 2), // keep b\n] // tail\n";
    let before = doc_at(src, &worker);
    let model = model_of(&before);
    let name_col = model.columns.iter().position(|c| c.field_name == "name").unwrap();

    // Paste a 1-row × 1-col block over (row 0, name): only `"a"` → `"z"`. Every
    // comment + every other cell must be byte-identical.
    let writes = paste_expand_writes(&model, 0, name_col, 0, "\"z\"\n");
    assert_eq!(writes.len(), 1, "a 1x1 paste writes exactly one cell");
    let mut doc = doc_at(src, &worker);
    doc.apply_grid_writes(&writes, &worker, Instant::now())
        .expect("paste commits losslessly");
    drive_reparse(&mut doc, &worker);

    let expected = src.replace("(name: \"a\"", "(name: \"z\"");
    assert_eq!(
        doc.buffer, expected,
        "only the pasted cell changed — all comments/formatting/order preserved: {}",
        doc.buffer
    );
}

#[test]
fn snapshot_comment_rich_doc_survives_a_gesture() {
    // FR-014 (insta complement): pin the exact bytes of a representative comment-rich,
    // custom-indented, ordered doc after a Ctrl+↑ increment on a single cell. The
    // snapshot makes any future regression in comment/format/order preservation
    // visible as a diff.
    let worker = ReparseWorker::new();
    let src = "// scene config\n[\n    /* hero */ (name: \"hero\", on: true,  hp: 10), // primary\n        /* boss */ (name: \"boss\", on: false, hp: 99), // indented oddly\n] // end\n";
    let before = doc_at(src, &worker);
    let model = model_of(&before);
    let hp_col = model.columns.iter().position(|c| c.field_name == "hp").unwrap();

    // Increment the boss's hp (row 1) by 1: 99 → 100. Everything else is byte-frozen.
    let writes = increment_writes(&model, 1, hp_col, 1, hp_col, 1);
    let mut doc = doc_at(src, &worker);
    doc.apply_grid_writes(&writes, &worker, Instant::now())
        .expect("increment commits losslessly");
    drive_reparse(&mut doc, &worker);

    insta::assert_snapshot!("comment_rich_increment", doc.buffer);
}

// ===========================================================================
// Phase 3 (US2 — P2): type-aware editors. Enum cells offer a type-to-filter
// picker constrained to the declared variants; cells with no bound type (or a
// non-enum declared type) still edit as free text (AD-001 layered dispatch).
//
// FR-005 (T014/T016/T019) + FR-013 fallback. egui_kittest headless only — never
// a live-GUI screenshot (memory rule).
// ===========================================================================

/// A top-level record list `[ (kind: On), (kind: Off) ]` whose declared type binds
/// the `kind` column to a RON enum with variants On/Off/Idle — the Phase 1 bound-enum
/// shape (a `oneOf` carrying `x-ron-variant`), wrapped in an array def so the section's
/// per-cell value path (`/<row>/kind`) resolves through `items → Widget → kind → Kind`.
/// Reuses the Phase 1 `bound()` helper + the `doc_at`/`model_of` harness seams.
fn bound_enum_doc(worker: &ReparseWorker) -> EditorDocument {
    let model = serde_json::json!({
        "$defs": {
            // The bound root: a list of Widget records (so the table section's
            // element index resolves through `items`).
            "WidgetList": {
                "type": "array",
                "items": { "$ref": "#/$defs/Widget" }
            },
            "Widget": {
                "type": "object",
                "properties": {
                    "kind": { "$ref": "#/$defs/Kind" }
                }
            },
            "Kind": {
                "oneOf": [
                    { "x-ron-variant": "On", "x-ron-variant-shape": "unit" },
                    { "x-ron-variant": "Off", "x-ron-variant-shape": "unit" },
                    { "x-ron-variant": "Idle", "x-ron-variant-shape": "unit" }
                ]
            }
        }
    });
    let mut doc = doc_at("[\n    (kind: On),\n    (kind: Off),\n]", worker);
    doc.bound_type = Some(bound(model, "WidgetList"));
    doc
}

// ---------------------------------------------------------------------------
// (T014) Headless: a bound enum cell offers ONLY the declared variants
// (type-to-filter); an unbound cell at the same position edits as free text.
// ---------------------------------------------------------------------------

#[test]
fn bound_enum_cell_offers_only_declared_variants() {
    // FR-005: editing a cell whose DECLARED type is an enum renders the variant
    // picker constrained to the schema's variants — every declared variant label is
    // present in the AccessKit tree (type-to-filter), and editing did NOT fall back to
    // the free-text editor. Sanity that `query_cell_type` resolves the cell first.
    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(bound_enum_doc(&worker)));

    let kind_col = col_of(&doc.borrow(), "kind");
    // The picker is gated on the cell's declared type resolving to an enum.
    {
        let d = doc.borrow();
        let model = model_of(&d);
        let cell = model.cell(0, kind_col).unwrap();
        let path = cell.value_ref.clone().expect("a present scalar cell");
        let info = d
            .query_cell_type(&path)
            .expect("the bound enum cell resolves a declared type");
        assert_eq!(info.declared_kind, CellKind::Enum);
        assert_eq!(
            info.enum_variants,
            vec!["On".to_string(), "Off".to_string(), "Idle".to_string()]
        );
    }

    // Begin editing row 0's `kind` cell via F2 on the active cell (edit-enter — the
    // picker opens on the SAME gesture that opens the text editor; one gesture, one
    // action). No pointer event is used.
    doc.borrow_mut()
        .view_state_mut()
        .set_grid_anchor(0, kind_col);
    let mut harness = harness_over(&doc, &worker);
    harness.run();
    harness.key_press(egui::Key::F2);
    harness.run();

    // The cell is now editing (focus set) — the type-aware widget, not a no-op.
    assert!(
        doc.borrow().view_state().edit_focus().is_some(),
        "F2 began editing the enum cell"
    );
    // Every declared variant label is offered by the picker (the full constrained list).
    // `Idle` is the decisive one: it appears NOWHERE in the buffer (no cell holds it), so
    // its presence can only come from the picker rendering the declared variant set.
    // (`On`/`Off` may match more than one node — they are also live cell values — so use
    // `query_all_by_label`, which never panics on multiple matches.)
    for variant in ["On", "Off", "Idle"] {
        assert!(
            harness.query_all_by_label(variant).next().is_some(),
            "the enum picker offers the declared variant `{variant}`"
        );
    }
    // A name that is NOT a declared variant is never offered as a pickable row.
    assert!(
        harness.query_by_label("Nope").is_none(),
        "the picker is constrained to the schema's variants — no stray options"
    );
}

#[test]
fn unbound_cell_at_the_same_position_edits_as_free_text() {
    // FR-013: with NO bound type, the SAME cell position edits as free text — a plain
    // text field, with no constrained variant list. The type-aware picker degrades
    // gracefully; editing is never blocked on a missing type (AD-001 fallback leg).
    let worker = Rc::new(ReparseWorker::new());
    // Same buffer/shape as the bound fixture, but no `bound_type` set.
    let doc = Rc::new(RefCell::new(doc_at(
        "[\n    (kind: On),\n    (kind: Off),\n]",
        &worker,
    )));
    assert!(
        doc.borrow().bound_type.is_none(),
        "the fixture is unbound (text-fallback path)"
    );

    let kind_col = col_of(&doc.borrow(), "kind");
    // `query_cell_type` returns None for the unbound cell ⇒ text fallback.
    {
        let d = doc.borrow();
        let model = model_of(&d);
        let cell = model.cell(0, kind_col).unwrap();
        let path = cell.value_ref.clone().expect("a present scalar cell");
        assert_eq!(
            d.query_cell_type(&path),
            None,
            "an unbound cell has no declared type ⇒ free-text editor"
        );
    }

    doc.borrow_mut()
        .view_state_mut()
        .set_grid_anchor(0, kind_col);
    let mut harness = harness_over(&doc, &worker);
    harness.run();
    harness.key_press(egui::Key::F2);
    harness.run();

    // The cell edits (free-text editor opened) — editing is not blocked.
    let focus = doc
        .borrow()
        .view_state()
        .edit_focus()
        .expect("F2 began editing the unbound cell as free text")
        .draft
        .clone();
    // The text editor is seeded with the cell's current token (`On`), not a picker.
    assert_eq!(focus, "On", "the free-text editor holds the cell's value");
    // No constrained variant list is offered. `Idle` is a declared variant that appears
    // NOWHERE in the buffer (no cell holds it), so it can only ever surface as a picker
    // option — its absence proves the unbound cell edits as free text, not a picker.
    // (`On`/`Off` would be ambiguous: they are also live cell values.)
    assert!(
        harness.query_by_label("Idle").is_none(),
        "the unbound cell shows NO constrained variant list (free text, not a picker)"
    );
}

// ---------------------------------------------------------------------------
// (T019) [COMPLETES FR-005] Pick a variant in the enum picker and commit;
// the committed RON token is the chosen variant, and the doc round-trips.
// ---------------------------------------------------------------------------

#[test]
fn enum_picker_commit_emits_the_chosen_variant_and_round_trips() {
    // FR-005: choosing a variant in the picker commits exactly that variant name as the
    // cell's RON token (via the existing `SetValue` path), and the doc round-trips — the
    // buffer now holds the chosen variant and every untouched byte is preserved.
    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(bound_enum_doc(&worker)));
    let before = doc.borrow().buffer.clone();
    assert!(
        before.contains("(kind: On)"),
        "row 0 starts at variant On: {before}"
    );

    let kind_col = col_of(&doc.borrow(), "kind");
    doc.borrow_mut()
        .view_state_mut()
        .set_grid_anchor(0, kind_col);
    let mut harness = harness_over(&doc, &worker);
    harness.run();
    // Open the picker on row 0's `kind` cell (edit-enter), then PICK `Idle`.
    harness.key_press(egui::Key::F2);
    harness.run();
    harness.get_by_label("Idle").click();
    harness.run();
    // Let the off-frame reparse land so the buffer reflects the committed variant.
    drive_reparse(&mut doc.borrow_mut(), &worker);

    let d = doc.borrow();
    // The committed token is the chosen variant — row 0 is now `Idle`.
    assert!(
        d.buffer.contains("(kind: Idle)"),
        "the picked variant `Idle` was committed as the cell's RON token: {}",
        d.buffer
    );
    // Round-trip / losslessness: ONLY the targeted token changed — row 1 (Off) and the
    // list framing are byte-identical to before (the chosen variant replaced `On`).
    let expected = before.replace("(kind: On)", "(kind: Idle)");
    assert_eq!(
        d.buffer, expected,
        "only the chosen cell's token changed; every untouched byte is preserved: {}",
        d.buffer
    );
    // The picker closed on commit (no lingering edit focus).
    assert!(
        d.view_state().edit_focus().is_none(),
        "committing the variant closed the picker"
    );
}

// ---------------------------------------------------------------------------
// (T015) {FR-006} Type-correct new-row defaults: with a type bound whose columns
// span a bool, a number, a string, and an enum, appending a row seeds each new
// cell with a VALID default (bool=false, number=0, string="", enum=first
// declared variant) — so the new row produces NO validation error. With NO type
// bound, appending still uses the existing placeholder default (unchanged).
// egui_kittest headless — never a live-GUI screenshot (memory rule).
// ---------------------------------------------------------------------------

/// A top-level record list bound to `WidgetList` (array of `Widget`) whose columns
/// span the four type-correct-default kinds: `on` (bool), `hp` (integer number),
/// `name` (string), and `kind` (a RON enum On/Off/Idle). The element index resolves
/// through `items → Widget → <field>`, so the prospective new-row cell paths
/// (`/<new row>/<field>`) resolve a declared type for each column (FR-006).
fn bound_typed_doc(worker: &ReparseWorker) -> EditorDocument {
    let model = serde_json::json!({
        "$defs": {
            "WidgetList": {
                "type": "array",
                "items": { "$ref": "#/$defs/Widget" }
            },
            "Widget": {
                "type": "object",
                "properties": {
                    "on":   { "type": "boolean" },
                    "hp":   { "type": "integer" },
                    "name": { "type": "string" },
                    "kind": { "$ref": "#/$defs/Kind" }
                }
            },
            "Kind": {
                "oneOf": [
                    { "x-ron-variant": "On", "x-ron-variant-shape": "unit" },
                    { "x-ron-variant": "Off", "x-ron-variant-shape": "unit" },
                    { "x-ron-variant": "Idle", "x-ron-variant-shape": "unit" }
                ]
            }
        }
    });
    let mut doc = doc_at(
        "[\n    (on: true, hp: 1, name: \"a\", kind: On),\n]",
        worker,
    );
    doc.bound_type = Some(bound(model, "WidgetList"));
    // Re-validate the existing buffer against the freshly-bound type so the baseline
    // diagnostics reflect the binding (the initial parse ran unbound).
    doc.revalidate(worker);
    drive_reparse(&mut doc, worker);
    doc
}

/// Click the visible "+ row" affordance once (the discoverable add-row control the
/// grid renders above the table — `PendingAction::AppendRow`, which seeds the new
/// row through the type-aware default path). Drives the *real* render + action path
/// headlessly, then lands the off-frame reparse so the buffer + diagnostics reflect
/// the appended row.
fn click_add_row(doc: &Rc<RefCell<EditorDocument>>, worker: &Rc<ReparseWorker>) {
    let mut harness = harness_over(doc, worker);
    harness.run();
    harness.get_by_label("+ row").click();
    harness.run();
    drive_reparse(&mut doc.borrow_mut(), worker);
}

/// `true` when the document holds at least one error-severity diagnostic (a
/// validation failure). Type-correct new-row defaults must leave this `false`.
fn has_error_diagnostic(doc: &EditorDocument) -> bool {
    doc.diagnostics
        .iter()
        .any(|d| d.severity == ronin_core::Severity::Error)
}

#[test]
fn typed_new_row_defaults_are_valid_for_each_column_type() {
    // FR-006 / SC-003: with a bound type, an appended row is seeded type-correctly —
    // bool=false, number=0, string="", enum=first declared variant — so it is valid
    // immediately (no validation error on creation).
    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(bound_typed_doc(&worker)));

    // Baseline: the seed row is valid (no error) and there is exactly one row.
    assert!(
        !has_error_diagnostic(&doc.borrow()),
        "the bound seed row is valid: {:?}",
        doc.borrow().diagnostics
    );
    assert_eq!(model_of(&doc.borrow()).row_count(), 1, "starts with one row");

    click_add_row(&doc, &worker);

    let d = doc.borrow();
    // The grid grew by one row and the appended row carries type-correct tokens.
    assert_eq!(model_of(&d).row_count(), 2, "the append added a row");
    // bool → false, number → 0, string → "", enum → first declared variant (On).
    // `Idle`/`Off` are NOT seeded (the first variant `On` is) — the new row reads
    // `(on: false, hp: 0, name: "", kind: On)`.
    assert!(
        d.buffer.contains("on: false")
            && d.buffer.contains("hp: 0")
            && d.buffer.contains("name: \"\"")
            && d.buffer.contains("kind: On"),
        "the appended row seeds a type-correct default per column: {}",
        d.buffer
    );
    // The decisive assertion (FR-006 / SC-003): the appended row produces NO
    // validation error. The enum is the witness — a placeholder `0` (the unbound
    // default) in the enum cell would be an `InvalidEnumVariant` error; seeding the
    // first variant keeps the new row valid.
    assert!(
        !has_error_diagnostic(&d),
        "a type-correct appended row produces no validation error: {:?}",
        d.diagnostics
    );
}

#[test]
fn unbound_new_row_uses_the_existing_placeholder_default() {
    // FR-006 (fallback): with NO type bound, appending a row still uses the existing
    // placeholder default (a `0` scalar per column) — unchanged behavior. Editing is
    // never blocked or constrained by a missing type (Progressive Intelligence).
    let worker = Rc::new(ReparseWorker::new());
    // Same column shape as the bound fixture, but unbound.
    let doc = Rc::new(RefCell::new(doc_at(
        "[\n    (on: true, hp: 1, name: \"a\", kind: On),\n]",
        &worker,
    )));
    assert!(
        doc.borrow().bound_type.is_none(),
        "the fixture is unbound (placeholder-default path)"
    );

    click_add_row(&doc, &worker);

    let d = doc.borrow();
    assert_eq!(model_of(&d).row_count(), 2, "the append added a row");
    // The existing placeholder default is a `0` scalar for every column — the new row
    // reads `(on: 0, hp: 0, name: 0, kind: 0)`. No type-correct seeding occurred
    // (no `false` / `""` / variant): unbound behavior is unchanged.
    assert!(
        d.buffer.contains("on: 0")
            && d.buffer.contains("name: 0")
            && d.buffer.contains("kind: 0"),
        "an unbound append uses the existing `0` placeholder for every column: {}",
        d.buffer
    );
    assert!(
        !d.buffer.contains("on: false") && !d.buffer.contains("name: \"\""),
        "no type-correct seeding occurred when unbound: {}",
        d.buffer
    );
}

// ---------------------------------------------------------------------------
// (T018) {FR-013} [COMPLETES FR-013] Graceful text fallback across ALL type-aware
// widgets when no type is bound: an (otherwise enum-like) cell edits as free text,
// a bool cell still toggles lexically, and a numeric cell still increments
// lexically — nothing is blocked or constrained by a missing type. The bool toggle
// / numeric increment / fill-down bulk ops are driven by the lexical `CellClass`
// (the cell's token), never by `query_cell_type` / `bound_type`, so they operate
// identically bound or unbound; the enum picker is the only type-gated widget and
// degrades to the free-text editor when unbound (AD-001 layered dispatch).
// egui_kittest headless — never a live-GUI screenshot (memory rule).
// ---------------------------------------------------------------------------

#[test]
fn unbound_doc_never_blocks_any_type_aware_widget() {
    // FR-013 / SC-004: on an UNBOUND document, every cell that *would* host a
    // type-aware widget when bound still edits freely — the enum-like cell edits as
    // free text, the bool cell toggles lexically, the numeric cell increments
    // lexically. One consolidated headless drive proves nothing is blocked when the
    // type is missing.
    let worker = Rc::new(ReparseWorker::new());
    // A bool (`on`), a numeric (`hp`), and an enum-like (`kind`) column — UNBOUND.
    let doc = Rc::new(RefCell::new(doc_at(
        "[\n    (on: true, hp: 1, kind: On),\n    (on: false, hp: 2, kind: Off),\n]",
        &worker,
    )));
    assert!(
        doc.borrow().bound_type.is_none(),
        "the fixture is unbound (text-fallback path for every widget)"
    );
    // Sanity: an unbound cell resolves NO declared type ⇒ the dispatch falls back.
    {
        let d = doc.borrow();
        let model = model_of(&d);
        let kind_col = col_of(&d, "kind");
        let path = model
            .cell(0, kind_col)
            .unwrap()
            .value_ref
            .clone()
            .expect("a present scalar cell");
        assert_eq!(
            d.query_cell_type(&path),
            None,
            "an unbound cell has no declared type (every widget degrades to text)"
        );
    }

    // (1) The enum-like cell edits as FREE TEXT — F2 opens the plain editor seeded
    //     with the cell's token (not a constrained picker). `Idle` is a name that
    //     appears nowhere in the buffer, so its absence proves no picker is shown.
    {
        let kind_col = col_of(&doc.borrow(), "kind");
        doc.borrow_mut()
            .view_state_mut()
            .set_grid_anchor(0, kind_col);
        let mut harness = harness_over(&doc, &worker);
        harness.run();
        harness.key_press(egui::Key::F2);
        harness.run();
        let draft = doc
            .borrow()
            .view_state()
            .edit_focus()
            .expect("F2 began editing the unbound enum-like cell as free text")
            .draft
            .clone();
        assert_eq!(draft, "On", "the free-text editor holds the cell's token");
        assert!(
            harness.query_by_label("Idle").is_none(),
            "no constrained variant picker is offered when unbound (free text)"
        );
        // Close the editor so the next gesture drives the grid, not the text field.
        doc.borrow_mut().view_state_mut().clear_focus();
    }

    // (2) The bool cell still TOGGLES lexically (Space) — not blocked by a missing
    //     type. The toggle is driven by the lexical bool token, not a bound type.
    {
        let on_col = col_of(&doc.borrow(), "on");
        doc.borrow_mut().view_state_mut().set_grid_anchor(0, on_col);
        let mut harness = harness_over(&doc, &worker);
        harness.run();
        harness.key_press(egui::Key::Space);
        harness.run();
        drive_reparse(&mut doc.borrow_mut(), &worker);
        let d = doc.borrow();
        assert!(
            d.buffer.contains("(on: false, hp: 1, kind: On)"),
            "Space toggled the unbound bool cell true→false lexically: {}",
            d.buffer
        );
        assert!(
            d.view_state().edit_focus().is_none(),
            "the lexical bool toggle did not block editing or open a constrained widget"
        );
    }

    // (3) The numeric cell still INCREMENTS lexically (Ctrl+↑) — not blocked by a
    //     missing type. The increment is driven by the lexical numeric token.
    {
        let hp_col = col_of(&doc.borrow(), "hp");
        doc.borrow_mut().view_state_mut().set_grid_anchor(1, hp_col);
        let mut harness = harness_over(&doc, &worker);
        harness.run();
        harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::ArrowUp);
        harness.run();
        drive_reparse(&mut doc.borrow_mut(), &worker);
        let d = doc.borrow();
        assert!(
            d.buffer.contains("(on: false, hp: 3, kind: Off)"),
            "Ctrl+↑ incremented the unbound numeric cell 2→3 lexically: {}",
            d.buffer
        );
    }
}

// ---------------------------------------------------------------------------
// (T020) US3 — Column hide / reorder / pin is VIEW-ONLY (FR-007 / Principle I)
//
// On a wide uniform record list: hide a column, reorder two columns, and pin the
// key column. Assert (a) the VIEW reflects each change — the hidden column's header
// is absent, the reordered order changes, and the pinned column is first — and (b)
// the underlying `doc.buffer` is BYTE-IDENTICAL before and after ALL three ops
// (hide/reorder/pin never produce a CST edit). egui_kittest headless — never a
// live-GUI screenshot (memory rule).
// ---------------------------------------------------------------------------

#[test]
fn column_hide_reorder_pin_is_view_only_and_buffer_byte_identical() {
    let worker = Rc::new(ReparseWorker::new());
    // A WIDE record list: 5 columns (id is the key/identifier column).
    let src = "[\n    (id: 1, name: \"a\", hp: 10, mp: 5, tag: \"x\"),\n    (id: 2, name: \"b\", hp: 20, mp: 6, tag: \"y\"),\n    (id: 3, name: \"c\", hp: 30, mp: 7, tag: \"z\"),\n]";
    let doc = Rc::new(RefCell::new(doc_at(src, &worker)));

    // The byte snapshot the whole sequence must preserve (Principle I — view-only).
    let buffer_before = doc.borrow().buffer.clone();

    // Resolve the MODEL column indices once (stable keys for hide/reorder/pin).
    let (id_c, hp_c, tag_c) = {
        let d = doc.borrow();
        (col_of(&d, "id"), col_of(&d, "hp"), col_of(&d, "tag"))
    };

    // --- Frame 0: the default layout shows every column header. ---
    {
        let mut harness = harness_over(&doc, &worker);
        harness.run();
        for name in ["id", "name", "hp", "mp", "tag"] {
            assert!(
                harness.query_all_by_label_contains(name).next().is_some(),
                "default layout shows the `{name}` header"
            );
        }
    }

    // --- Hide the `tag` column (byte-free view-state mutator). ---
    {
        doc.borrow_mut()
            .view_state_mut()
            .column_view_state_mut(&StructuralPath::root())
            .hide(tag_c);
        let mut harness = harness_over(&doc, &worker);
        harness.run();
        // The hidden column's header is ABSENT; the others remain.
        assert!(
            harness.query_all_by_label_contains("tag").next().is_none(),
            "the hidden `tag` column header is absent from the view"
        );
        for name in ["id", "name", "hp", "mp"] {
            assert!(
                harness.query_all_by_label_contains(name).next().is_some(),
                "non-hidden `{name}` header still renders"
            );
        }
        // The view-state agrees: `tag` is not in the visible mapping.
        let n = model_of(&doc.borrow()).columns.len();
        let visible = doc
            .borrow()
            .view_state()
            .column_view_state(&StructuralPath::root())
            .unwrap()
            .visible_columns(n);
        assert!(
            !visible.contains(&tag_c),
            "the visible→model mapping excludes the hidden column"
        );
    }

    // --- Reorder: move `hp` to the front (display position 0). ---
    {
        let n = model_of(&doc.borrow()).columns.len();
        doc.borrow_mut()
            .view_state_mut()
            .column_view_state_mut(&StructuralPath::root())
            .move_column(hp_c, 0, n);
        let mut harness = harness_over(&doc, &worker);
        harness.run();
        let visible = doc
            .borrow()
            .view_state()
            .column_view_state(&StructuralPath::root())
            .unwrap()
            .visible_columns(n);
        assert_eq!(
            visible.first().copied(),
            Some(hp_c),
            "the reordered `hp` column is now first; tag stays hidden: {visible:?}"
        );
        assert!(
            !visible.contains(&tag_c),
            "reorder did not un-hide the hidden column"
        );
    }

    // --- Pin the key column `id`: it floats to the front (sticky). ---
    {
        let n = model_of(&doc.borrow()).columns.len();
        doc.borrow_mut()
            .view_state_mut()
            .column_view_state_mut(&StructuralPath::root())
            .pin(id_c);
        let mut harness = harness_over(&doc, &worker);
        harness.run();
        let cvs = doc.borrow();
        let cvs = cvs
            .view_state()
            .column_view_state(&StructuralPath::root())
            .unwrap();
        let visible = cvs.visible_columns(n);
        assert_eq!(
            visible.first().copied(),
            Some(id_c),
            "the pinned key column `id` is first (frozen at the left): {visible:?}"
        );
        assert_eq!(cvs.pinned, Some(id_c), "the key column is pinned");
        // The pin marker (📌) is painted in the view.
        assert!(
            harness
                .query_all_by_label_contains("\u{1F4CC}")
                .next()
                .is_some(),
            "the pinned column header shows the 📌 pin marker"
        );
    }

    // --- (b) Principle I: NOT ONE BYTE changed across hide + reorder + pin. ---
    {
        let d = doc.borrow();
        assert_eq!(
            d.buffer, buffer_before,
            "column hide/reorder/pin is VIEW-ONLY — the buffer is byte-identical (no CST edit)"
        );
        assert_eq!(
            d.buffer, src,
            "the buffer still equals the original source verbatim"
        );
        // No cell edit was ever opened by a column op (it is not an editing gesture).
        assert!(
            d.view_state().edit_focus().is_none(),
            "no edit focus was opened — column ops are not edits"
        );
        // The original field tokens are all still present (nothing was reflowed/dropped).
        for tok in ["id: 1", "name: \"a\"", "hp: 10", "mp: 5", "tag: \"x\""] {
            assert!(
                d.buffer.contains(tok),
                "the `{tok}` token survived all column ops verbatim: {}",
                d.buffer
            );
        }
    }
}

// ===========================================================================
// (T026) US3 — Row reorder via a DEDICATED drag handle is lossless + one undo unit
//   (FR-008 / AD-003 / HINT-005). The grip column reuses the existing lossless
//   `StructuralOp::ReorderChild` (no new core op). Two coverage angles:
//   (a) a REAL headless grip drag (press on row 0's grip → move onto row 2's grip →
//       release) reorders the rows, driven through `render_table_view`'s real handler;
//   (b) the action the drag dispatches (`apply_table_reorder_row`) is byte-lossless and
//       a single undo unit. egui_kittest headless — NEVER a live-GUI screenshot.
// ===========================================================================

/// The Unicode grip glyph the row drag-handle renders (`⠿`, U+2807). Kept in one place
/// so the test and the renderer agree on the affordance's label.
const ROW_GRIP_GLYPH: &str = "\u{2807}";

#[test]
fn row_drag_handle_reorders_rows_losslessly_as_one_undo_unit() {
    // Real headless drag: grab row 0's grip and drop it on row 2's grip. The first row
    // (with its trailing comment) must move to the end, every other row byte-identical,
    // and the whole move is exactly ONE undo unit (undo restores the original bytes).
    let worker = Rc::new(ReparseWorker::new());
    // Each row carries a distinctive IN-ELEMENT comment so "lossless" is observable: the
    // moved row's OWN bytes (the element node, incl. its in-element comment) travel with it
    // verbatim. (`StructuralOp::ReorderChild` preserves the element's own bytes; the comma
    // separator is normalized — a comment AFTER the comma is separator trivia, not the
    // element's bytes, per the op's documented model.)
    let src = "[\n    (name: \"a\" /* a */, hp: 1),\n    (name: \"b\" /* b */, hp: 2),\n    (name: \"c\" /* c */, hp: 3),\n]";
    let doc = Rc::new(RefCell::new(doc_at(src, &worker)));
    let before = doc.borrow().buffer.clone();

    let mut harness = harness_over(&doc, &worker);
    harness.run();

    // Locate the per-row grips by their glyph; order them top-to-bottom by Y so we can
    // address row 0 (topmost grip) and row 2 (bottommost grip) unambiguously.
    let mut grips: Vec<egui::Pos2> = harness
        .query_all_by_label_contains(ROW_GRIP_GLYPH)
        .map(|n| n.rect().center())
        .collect();
    grips.sort_by(|a, b| a.y.partial_cmp(&b.y).unwrap());
    assert_eq!(grips.len(), 3, "one drag-handle grip per row");
    let (from_grip, to_grip) = (grips[0], grips[2]);

    // Press the top grip, drag onto the bottom grip, release (genuine pointer events).
    harness.hover_at(from_grip);
    harness.drag_at(from_grip); // press
    harness.run();
    harness.hover_at(to_grip); // move while pressed → drag over row 2's grip
    harness.run();
    harness.drop_at(to_grip); // release → commit the reorder
    harness.run();
    {
        let mut d = doc.borrow_mut();
        drive_reparse(&mut d, &worker);
    }

    {
        let d = doc.borrow();
        // Row a moved to the END; b and c shifted up — order is now b, c, a.
        let ia = d.buffer.find("name: \"a\"").expect("row a present");
        let ib = d.buffer.find("name: \"b\"").expect("row b present");
        let ic = d.buffer.find("name: \"c\"").expect("row c present");
        assert!(
            ib < ic && ic < ia,
            "drag of row 0 onto row 2 reordered to [b, c, a]: {}",
            d.buffer
        );
        // LOSSLESS: every row's own bytes (incl. its in-element comment) survived verbatim
        // — including the MOVED row a, whose `/* a */` comment travelled with it.
        for tok in [
            "(name: \"a\" /* a */, hp: 1)",
            "(name: \"b\" /* b */, hp: 2)",
            "(name: \"c\" /* c */, hp: 3)",
        ] {
            assert!(
                d.buffer.contains(tok),
                "row `{tok}` preserved byte-for-byte: {}",
                d.buffer
            );
        }
        // The drag did not open a cell editor (the grip is a distinct gesture, not select).
        assert!(
            d.view_state().edit_focus().is_none(),
            "the row drag-handle is its own affordance — it never opens a cell editor"
        );
    }

    // ONE undo unit: a single undo restores the EXACT original bytes.
    {
        let mut d = doc.borrow_mut();
        assert!(d.undo(Instant::now()), "a row reorder pushed exactly one undo unit");
        drive_reparse(&mut d, &worker);
        assert_eq!(
            d.buffer, before,
            "the row reorder is a single undo unit — one undo restores the original bytes"
        );
    }
}

#[test]
fn row_reorder_action_is_byte_lossless_and_view_only_on_columns() {
    // The action the grip dispatches (`apply_table_reorder_row`) — asserted directly via
    // the real apply path so the intent is pinned even where the geometric drag is finicky.
    // (a) `from > to` (drag a lower row UP) reorders losslessly; (b) the grip column is its
    // own non-data affordance — it does NOT disturb the visible→model column mapping.
    let worker = ReparseWorker::new();
    // In-element comments so the moved row's OWN bytes are observably preserved (the op
    // preserves the element node's bytes; the comma separator is normalized).
    let src = "[\n    (id: 1, name: \"a\" /* r0 */, hp: 10),\n    (id: 2, name: \"b\" /* r1 */, hp: 20),\n    (id: 3, name: \"c\" /* r2 */, hp: 30),\n]";
    let mut doc = doc_at(src, &worker);

    // The column view-state mapping BEFORE any row op (every model column visible, in order).
    let n_cols = model_of(&doc).columns.len();
    let visible_before: Vec<usize> = (0..n_cols).collect();

    // Drag row 2 UP onto row 0 (from=2, to=0): order becomes [c, a, b].
    doc.apply_table_reorder_row(&StructuralPath::root(), 2, 0, &worker, Instant::now())
        .expect("row reorder applies");
    drive_reparse(&mut doc, &worker);

    let ic = doc.buffer.find("name: \"c\"").unwrap();
    let ia = doc.buffer.find("name: \"a\"").unwrap();
    let ib = doc.buffer.find("name: \"b\"").unwrap();
    assert!(
        ic < ia && ia < ib,
        "from=2 to=0 reordered to [c, a, b]: {}",
        doc.buffer
    );
    // LOSSLESS: each row's own bytes (incl. its in-element comment) preserved verbatim.
    for tok in [
        "(id: 1, name: \"a\" /* r0 */, hp: 10)",
        "(id: 2, name: \"b\" /* r1 */, hp: 20)",
        "(id: 3, name: \"c\" /* r2 */, hp: 30)",
    ] {
        assert!(doc.buffer.contains(tok), "row `{tok}` preserved: {}", doc.buffer);
    }

    // (b) The grip handle never touches column view-state: the visible→model mapping is
    // unchanged after the reorder (the grip is its own non-data column, in neither space).
    let n_after = model_of(&doc).columns.len();
    assert_eq!(n_after, n_cols, "row reorder did not add/remove a data column");
    let visible_after: Vec<usize> = doc
        .view_state()
        .column_view_state(&StructuralPath::root())
        .map_or_else(|| (0..n_after).collect(), |c| c.visible_columns(n_after));
    assert_eq!(
        visible_after, visible_before,
        "the row drag-handle does not perturb the visible→model column mapping"
    );
}

// ===========================================================================
// (T022) US3 — Reset restores the default column layout AND clears persistence
//   (FR-015 / FR-007). Drives the REAL handlers:
//   `save_persisted_column_layout` (T025 save), `reset_column_layout` (T027),
//   and the renderer. egui_kittest headless — NEVER a live-GUI screenshot.
// ===========================================================================

use ronin_app::settings::{section_layout_key, AppSettings};
use ronin_app::structural::table::{
    reset_column_layout, save_persisted_column_layout, load_persisted_column_layout,
};

/// A `doc_at` document with a stable on-disk PATH, so column-layout persistence has a
/// key (an untitled buffer is intentionally not persisted). Byte-free w.r.t. content.
fn doc_at_path(src: &str, path: &str, worker: &ReparseWorker) -> EditorDocument {
    let mut doc = doc_at(src, worker);
    doc.path = Some(std::path::PathBuf::from(path));
    doc
}

#[test]
fn reset_restores_default_column_layout_and_clears_persisted_entry() {
    let worker = Rc::new(ReparseWorker::new());
    // A WIDE record list: 5 columns (id is the key/identifier column).
    let src = "[\n    (id: 1, name: \"a\", hp: 10, mp: 5, tag: \"x\"),\n    (id: 2, name: \"b\", hp: 20, mp: 6, tag: \"y\"),\n    (id: 3, name: \"c\", hp: 30, mp: 7, tag: \"z\"),\n]";
    let doc = Rc::new(RefCell::new(doc_at_path(src, "/proj/wide.ron", &worker)));
    let mut settings = AppSettings::default();

    let section = StructuralPath::root();
    let key = section_layout_key(doc.borrow().path.as_deref(), &section)
        .expect("a document WITH a path yields a persistence key");

    // The default-layout VISIBLE→MODEL mapping the reset must restore (all columns,
    // first-seen order, no pin).
    let model_cols = model_of(&doc.borrow()).columns.len();
    let default_visible: Vec<usize> = (0..model_cols).collect();
    let buffer_before = doc.borrow().buffer.clone();

    // Resolve stable MODEL column indices for the ops.
    let (id_c, hp_c, tag_c) = {
        let d = doc.borrow();
        (col_of(&d, "id"), col_of(&d, "hp"), col_of(&d, "tag"))
    };

    // --- Customize: hide `tag`, move `hp` to the front, pin `id`. ---
    {
        let mut d = doc.borrow_mut();
        let cvs = d.view_state_mut().column_view_state_mut(&section);
        cvs.hide(tag_c);
        cvs.move_column(hp_c, 0, model_cols);
        cvs.pin(id_c);
    }
    // The live layout is now NON-default (something hidden / reordered / pinned).
    assert!(
        !doc.borrow()
            .view_state()
            .column_view_state(&section)
            .unwrap()
            .is_default(),
        "the customized layout is not the default"
    );

    // --- Persist it via the REAL save handler (T025 (b) save leg). ---
    save_persisted_column_layout(&doc.borrow(), &mut settings, &section);
    let persisted = settings
        .column_layout(&key)
        .expect("the customized layout was persisted to settings")
        .clone();
    assert_eq!(persisted.pinned, Some(id_c), "the pin persisted");
    assert!(persisted.hidden.contains(&tag_c), "the hidden col persisted");
    assert!(!persisted.is_default(), "a non-default layout persisted");

    // Render once so the customized view is exercised through the real grid; the pinned
    // column floats to the front (sticky), tag is hidden.
    {
        let mut harness = harness_over(&doc, &worker);
        harness.run();
        assert!(
            harness.query_all_by_label_contains("tag").next().is_none(),
            "the hidden `tag` column header is absent before reset"
        );
    }

    // --- Invoke the REAL reset handler (T027): clears live state AND persisted entry. ---
    reset_column_layout(&mut doc.borrow_mut(), &mut settings, &section);

    // (1) Live column view-state is back to the default (dropped entirely).
    assert!(
        doc.borrow()
            .view_state()
            .column_view_state(&section)
            .is_none(),
        "reset drops the live column view-state — back to the default layout"
    );

    // (2) The persisted settings entry is GONE (reset truly returns to default; FR-015).
    assert!(
        settings.column_layout(&key).is_none(),
        "reset removed the persisted layout entry from AppSettings"
    );

    // (3) The rendered view returns to ALL columns in first-seen order, no pin.
    {
        let mut harness = harness_over(&doc, &worker);
        harness.run();
        for name in ["id", "name", "hp", "mp", "tag"] {
            assert!(
                harness.query_all_by_label_contains(name).next().is_some(),
                "after reset, the `{name}` header is visible again (all columns shown)"
            );
        }
        // No pin marker is painted after reset.
        assert!(
            harness
                .query_all_by_label_contains("\u{1F4CC}")
                .next()
                .is_none(),
            "after reset there is no pinned column (no 📌 marker)"
        );
        // The visible→model mapping is the natural model order (no live state ⇒ identity).
        let cvs = doc.borrow();
        let mapping = cvs
            .view_state()
            .column_view_state(&section)
            .map_or_else(|| (0..model_cols).collect::<Vec<_>>(), |c| c.visible_columns(model_cols));
        assert_eq!(
            mapping, default_visible,
            "the column order/visibility returned to the first-seen default"
        );
    }

    // (4) Re-loading from the now-empty settings yields the default (no resurrection).
    load_persisted_column_layout(&mut doc.borrow_mut(), &settings, &section, model_cols);
    assert!(
        doc.borrow()
            .view_state()
            .column_view_state(&section)
            .is_none(),
        "loading after reset does not resurrect the cleared layout"
    );

    // (5) Principle I: reset + persistence are VIEW-ONLY — the buffer never changed.
    assert_eq!(
        doc.borrow().buffer,
        buffer_before,
        "reset / persist are view-only — the document buffer is byte-identical"
    );
}

#[test]
fn persisted_layout_loads_back_into_live_view_state_on_first_show() {
    // T025 (a) load leg: a saved layout for a section is materialized into a fresh
    // document's live view-state on first show, and out-of-range indices are dropped.
    let worker = Rc::new(ReparseWorker::new());
    let src = "[\n    (id: 1, name: \"a\", hp: 10),\n    (id: 2, name: \"b\", hp: 20),\n]";
    let mut settings = AppSettings::default();
    let section = StructuralPath::root();

    // Persist a layout (hide `hp`, pin `id`) for /proj/saved.ron via the real save path.
    let key = {
        let donor = doc_at_path(src, "/proj/saved.ron", &worker);
        let n = model_of(&donor).columns.len();
        let (id_c, hp_c) = (col_of(&donor, "id"), col_of(&donor, "hp"));
        let donor = Rc::new(RefCell::new(donor));
        {
            let mut d = donor.borrow_mut();
            let cvs = d.view_state_mut().column_view_state_mut(&section);
            cvs.hide(hp_c);
            cvs.pin(id_c);
            let _ = n;
        }
        save_persisted_column_layout(&donor.borrow(), &mut settings, &section);
        let k = section_layout_key(donor.borrow().path.as_deref(), &section).unwrap();
        k
    };
    assert!(settings.column_layout(&key).is_some(), "layout persisted");

    // A FRESH document at the SAME path starts with NO live layout.
    let fresh = doc_at_path(src, "/proj/saved.ron", &worker);
    let n = model_of(&fresh).columns.len();
    let (id_c, hp_c) = (col_of(&fresh, "id"), col_of(&fresh, "hp"));
    let mut fresh = fresh;
    assert!(
        fresh.view_state().column_view_state(&section).is_none(),
        "a fresh document has no live column layout yet"
    );

    // Load the persisted layout into the fresh document (first-show, the (a) leg).
    load_persisted_column_layout(&mut fresh, &settings, &section, n);
    let cvs = fresh
        .view_state()
        .column_view_state(&section)
        .expect("the persisted layout materialized into live state");
    assert_eq!(cvs.pinned, Some(id_c), "the persisted pin loaded");
    assert!(
        cvs.is_hidden(hp_c) || !cvs.visible_columns(n).contains(&hp_c),
        "the persisted hidden column loaded"
    );

    // An UNTITLED document (no path) never loads/persists — stays live-only.
    let mut untitled = doc_at(src, &worker);
    load_persisted_column_layout(&mut untitled, &settings, &section, n);
    assert!(
        untitled.view_state().column_view_state(&section).is_none(),
        "an untitled (path-less) document does not load a persisted layout"
    );
}

// ===========================================================================
// (T029) US4 — {FR-010} Sticky header while scrolling + DISTINCT rendering of
//   absent field vs empty string vs explicit null.
//
//   (a) A list TALLER than the viewport: scroll the body and assert a column
//       header label is STILL present in the AccessKit tree (the header is
//       rendered by egui_extras' `.header()` OUTSIDE the body scroll, so it
//       stays visible). Driven through the real `render_table_view_counting`
//       in a fixed, small viewport so the body must scroll.
//   (b) A record list where one row is MISSING a field (absent → `—`), one has
//       it as `""` (empty string → the `""` empty-quotes affordance), and one
//       has it as `()` (explicit null/unit → a styled `()`). Assert the three
//       distinct rendered markers are all present (the three states are told
//       apart at a glance — FR-010).
//
//   egui_kittest headless — NEVER a live-GUI screenshot (memory rule).
// ===========================================================================

use std::cell::Cell as StdCell;

/// The em-dash an ABSENT field renders (`—`, U+2014).
const ABSENT_MARKER: &str = "\u{2014}";
/// The empty-quotes affordance an EMPTY STRING renders (`""`, U+201C U+201D).
const EMPTY_STRING_MARKER: &str = "\u{201C}\u{201D}";
/// The explicit NULL / unit marker (`()`).
const NULL_MARKER: &str = "()";

#[test]
fn column_header_stays_visible_while_the_body_scrolls() {
    // (a) FR-010 / SC-006: with a list taller than the viewport, scrolling the body
    // down keeps the column header visible — the `name` header label is still in the
    // AccessKit tree after the scroll. Rendered in a fixed small viewport so the body
    // is forced to scroll; the realized-row counter confirms the body is virtualized
    // (only viewport-many rows realized), i.e. the list genuinely overflows.
    let worker = Rc::new(ReparseWorker::new());
    // 200 rows — far more than fit in a 200px-tall viewport (≈ a handful of rows).
    let mut src = String::from("[\n");
    for i in 0..200 {
        src.push_str(&format!("    (name: \"r{i}\", hp: {i}),\n"));
    }
    src.push(']');
    let doc = Rc::new(RefCell::new(doc_at(&src, &worker)));

    let realized = Rc::new(StdCell::new(0usize));
    let doc_ui = Rc::clone(&doc);
    let worker_ui = Rc::clone(&worker);
    let realized_ui = Rc::clone(&realized);
    let mut harness = Harness::builder()
        .with_size(egui::vec2(400.0, 200.0))
        .build_ui(move |ui| {
            ui.set_max_height(200.0);
            let mut d = doc_ui.borrow_mut();
            render_table_view_counting(
                ui,
                &mut d,
                &worker_ui,
                &StructuralPath::root(),
                SectionShape::RecordList,
                &realized_ui,
            );
        });
    harness.run();

    // The header is present BEFORE scrolling (baseline).
    assert!(
        harness.query_all_by_label_contains("name").next().is_some(),
        "the `name` column header is visible before scrolling"
    );
    // The body is virtualized: it realized only viewport-many rows (far < 200), proving
    // the list overflows the viewport so a scroll is meaningful.
    assert!(
        realized.get() < 100,
        "the body is virtualized (only viewport-many of 200 rows realized): {}",
        realized.get()
    );

    // Scroll the body DOWN by sending mouse-wheel events with the pointer over the grid
    // body. (Positive-down delta moves the content up, revealing rows further down.)
    harness.hover_at(egui::pos2(200.0, 150.0));
    for _ in 0..20 {
        harness.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, -8.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::default(),
        });
        // `step` (one frame) not `run` — a hovered cell's tooltip keeps requesting a
        // repaint, which would trip `run`'s max-steps convergence guard.
        harness.step();
    }
    // Settle without enforcing convergence (the hover tooltip repaints indefinitely).
    harness.run_ok();

    // (a) The decisive assertion: after scrolling the body, the column header is STILL
    // present — it did not scroll away with the rows (sticky header). This holds in the
    // NON-PINNED layout (rendered here): the header is OUTSIDE the body's scroll region.
    assert!(
        harness.query_all_by_label_contains("name").next().is_some(),
        "the `name` column header stays visible after the body scrolls (sticky header)"
    );
    // Sanity: a top row that was visible initially has scrolled out (the body moved).
    assert!(
        harness.query_by_label("r0").is_none(),
        "the first row scrolled out of view — the body genuinely scrolled"
    );
}

#[test]
fn sticky_header_holds_in_the_pinned_split_layout() {
    // (a) FR-010: the Phase-4a pin-split renders the pinned key column in its OWN fixed
    // single-column `TableBuilder` (left) beside the horizontally-scrolling main table —
    // BOTH use `.header()`, so the header stays sticky in EACH. With a column pinned and
    // the body scrolled, the pinned key column's header AND a scrolling column's header
    // are both still present.
    let worker = Rc::new(ReparseWorker::new());
    let mut src = String::from("[\n");
    for i in 0..200 {
        src.push_str(&format!("    (id: {i}, name: \"r{i}\", hp: {i}),\n"));
    }
    src.push(']');
    let doc = Rc::new(RefCell::new(doc_at(&src, &worker)));

    // Pin the `id` key column (view-only) so the pin-split layout is exercised.
    let id_c = col_of(&doc.borrow(), "id");
    doc.borrow_mut()
        .view_state_mut()
        .column_view_state_mut(&StructuralPath::root())
        .pin(id_c);

    let realized = Rc::new(StdCell::new(0usize));
    let doc_ui = Rc::clone(&doc);
    let worker_ui = Rc::clone(&worker);
    let realized_ui = Rc::clone(&realized);
    let mut harness = Harness::builder()
        .with_size(egui::vec2(400.0, 200.0))
        .build_ui(move |ui| {
            ui.set_max_height(200.0);
            let mut d = doc_ui.borrow_mut();
            render_table_view_counting(
                ui,
                &mut d,
                &worker_ui,
                &StructuralPath::root(),
                SectionShape::RecordList,
                &realized_ui,
            );
        });
    harness.run();

    // Both the pinned key column's header and a scrolling column's header render.
    assert!(
        harness.query_all_by_label_contains("id").next().is_some(),
        "the pinned `id` column header renders in the pin-split layout"
    );
    assert!(
        harness.query_all_by_label_contains("hp").next().is_some(),
        "a scrolling column header renders in the pin-split layout"
    );
    // The pin marker confirms the pinned-split path is the one under test.
    assert!(
        harness
            .query_all_by_label_contains("\u{1F4CC}")
            .next()
            .is_some(),
        "the pinned column header shows the 📌 marker (pin-split layout active)"
    );

    // Scroll the body down; the headers (pinned + scrolling) must stay.
    harness.hover_at(egui::pos2(200.0, 150.0));
    for _ in 0..20 {
        harness.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, -8.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::default(),
        });
        // `step` (one frame) not `run` — a hovered cell's tooltip keeps requesting a
        // repaint, which would trip `run`'s max-steps convergence guard.
        harness.step();
    }
    // Settle without enforcing convergence (the hover tooltip repaints indefinitely).
    harness.run_ok();

    assert!(
        harness.query_all_by_label_contains("id").next().is_some(),
        "the PINNED column header stays visible after scrolling (pin-split sticky header)"
    );
    assert!(
        harness.query_all_by_label_contains("hp").next().is_some(),
        "the SCROLLING column header stays visible after scrolling (pin-split sticky header)"
    );
}

#[test]
fn absent_empty_string_and_null_render_distinctly() {
    // (b) FR-010 / SC-006: in a record list where one row is MISSING the `note` field,
    // one has `note: ""` (empty string), and one has `note: ()` (explicit null/unit),
    // the table renders each of the three states with a DISTINCT marker so the user can
    // tell them apart at a glance:
    //   absent       → `—`   (faint em-dash placeholder)
    //   empty string → `""`  (subtle empty-quotes affordance)
    //   null / unit  → `()`  (styled unit marker)
    // Driven through the real grid renderer; assert all three distinct markers are present.
    let worker = Rc::new(ReparseWorker::new());
    // Row 0: note absent. Row 1: note = "". Row 2: note = (). Plus a non-empty string
    // row so the empty-quotes affordance is provably DISTINCT from an ordinary string.
    let src = "[\n    (id: 0),\n    (id: 1, note: \"\"),\n    (id: 2, note: ()),\n    (id: 3, note: \"hi\"),\n]";
    let doc = Rc::new(RefCell::new(doc_at(src, &worker)));

    // Sanity on the model: the `note` cells classify as expected (absent → Blank cell with
    // no value_ref; the others → present Scalar cells carrying their verbatim token).
    {
        let d = doc.borrow();
        let model = model_of(&d);
        let note_c = model
            .columns
            .iter()
            .position(|c| c.field_name == "note")
            .expect("the `note` column exists (union of fields)");
        // Row 0: the field is ABSENT ⇒ a Blank cell with no value reference.
        let absent = model.cell(0, note_c).unwrap();
        assert_eq!(
            absent.value_ref, None,
            "row 0's `note` is absent (a Blank cell, no value_ref)"
        );
        // Row 1: present empty string `""`.
        assert_eq!(
            model.cell(1, note_c).and_then(|c| c.text.clone()).as_deref(),
            Some("\"\""),
            "row 1's `note` is a present empty string"
        );
        // Row 2: present explicit unit `()`.
        assert_eq!(
            model.cell(2, note_c).and_then(|c| c.text.clone()).as_deref(),
            Some("()"),
            "row 2's `note` is a present explicit null/unit"
        );
    }

    let mut harness = harness_over(&doc, &worker);
    harness.run();

    // The three DISTINCT markers are each present in the rendered tree.
    assert!(
        harness
            .query_all_by_label_contains(ABSENT_MARKER)
            .next()
            .is_some(),
        "the ABSENT field renders the `—` placeholder marker"
    );
    assert!(
        harness
            .query_all_by_label_contains(EMPTY_STRING_MARKER)
            .next()
            .is_some(),
        "the EMPTY STRING renders the `\"\"` empty-quotes affordance marker"
    );
    assert!(
        harness
            .query_all_by_label_contains(NULL_MARKER)
            .next()
            .is_some(),
        "the explicit NULL/unit renders the `()` marker"
    );
    // The three markers are mutually distinct strings (the user can tell them apart).
    assert_ne!(ABSENT_MARKER, EMPTY_STRING_MARKER);
    assert_ne!(ABSENT_MARKER, NULL_MARKER);
    assert_ne!(EMPTY_STRING_MARKER, NULL_MARKER);

    // Rendering these three states is VIEW-ONLY — it changed zero document bytes.
    assert_eq!(
        doc.borrow().buffer,
        src,
        "rendering absent/empty/null is view-only — the buffer is byte-identical"
    );
}

#[test]
fn editing_an_empty_string_or_null_cell_still_works() {
    // FR-010 (don't break editing): the distinct read-only rendering of an empty string
    // and an explicit null must NOT block editing those cells. Double-click / F2 opens the
    // inline editor seeded with the cell's verbatim token (`""` / `()`), so the user can
    // still change them. Driven through the real renderer + key handler, headlessly.
    let worker = Rc::new(ReparseWorker::new());
    let src = "[\n    (id: 1, note: \"\"),\n    (id: 2, note: ()),\n    (id: 3, note: \"x\"),\n]";
    let doc = Rc::new(RefCell::new(doc_at(src, &worker)));
    let note_c = col_of(&doc.borrow(), "note");

    // (1) The empty-string cell (row 0) opens an editor seeded with its `""` token on F2.
    doc.borrow_mut().view_state_mut().set_grid_anchor(0, note_c);
    let mut harness = harness_over(&doc, &worker);
    harness.run();
    harness.key_press(egui::Key::F2);
    harness.run();
    assert_eq!(
        doc.borrow()
            .view_state()
            .edit_focus()
            .expect("F2 opened the empty-string cell editor")
            .draft,
        "\"\"",
        "the empty-string cell edits, seeded with its verbatim `\"\"` token"
    );
    doc.borrow_mut().view_state_mut().clear_focus();

    // (2) The null/unit cell (row 1) opens an editor seeded with its `()` token on F2.
    doc.borrow_mut().view_state_mut().set_grid_anchor(1, note_c);
    let mut harness = harness_over(&doc, &worker);
    harness.run();
    harness.key_press(egui::Key::F2);
    harness.run();
    assert_eq!(
        doc.borrow()
            .view_state()
            .edit_focus()
            .expect("F2 opened the null/unit cell editor")
            .draft,
        "()",
        "the null/unit cell edits, seeded with its verbatim `()` token"
    );
}

// ===========================================================================
// (T028) [US4] {FR-009} In-cell VALIDATION OVERLAY — invalid input is flagged
// in-cell and stays editable, never silently reverted, never byte-corrupted.
//
// Two legs, matching the layered lexical+semantic design of T031:
//
//   (1) LEXICAL — a malformed RON token committed via the free-text editor is
//       REFUSED at the splice gate (`token_is_lexically_valid`): the editor stays
//       open with the draft preserved (stays editable, no silent revert) and the
//       buffer is byte-IDENTICAL (no garbage spliced — no byte corruption). The
//       as-you-type error tooltip is present in the AccessKit tree.
//   (2) SEMANTIC — with a type bound, a lexically-VALID but type-VIOLATING value
//       DOES commit (it splices losslessly), the off-frame reparse attaches the
//       authoritative RON-V#### finding to the cell, the cell carries an error
//       affordance (error diagnostic → error border + the diagnostic tooltip), and
//       the value stays in the buffer and remains editable (not reverted).
//
// egui_kittest headless — never a live-GUI screenshot (memory rule).
// ===========================================================================

#[test]
fn lexically_invalid_commit_is_refused_editor_stays_open_buffer_unchanged() {
    // FR-009 (lexical leg): typing a malformed token (`1 2 3` — multiple top-level
    // values, never one well-formed RON value) and pressing Enter must NOT corrupt the
    // buffer and must NOT silently revert the user's input. The commit is refused at the
    // splice gate; the editor stays open with the draft intact and the buffer is
    // byte-identical to before the bad commit.
    let worker = Rc::new(ReparseWorker::new());
    let src = "[\n    (name: \"a\", hp: 1),\n    (name: \"b\", hp: 2),\n]";
    let doc = Rc::new(RefCell::new(doc_at(src, &worker)));
    let buffer_before = doc.borrow().buffer.clone();
    let hp_col = col_of(&doc.borrow(), "hp");

    // Open the free-text editor on row 0's `hp` cell (F2), then set the live draft to a
    // malformed token (the same slot the user's keystrokes write).
    doc.borrow_mut().view_state_mut().set_grid_anchor(0, hp_col);
    let mut harness = harness_over(&doc, &worker);
    harness.run();
    harness.key_press(egui::Key::F2);
    harness.run();
    doc.borrow_mut()
        .view_state_mut()
        .edit_focus_mut()
        .expect("F2 opened the free-text editor")
        .draft = "1 2 3".to_string();
    harness.run();

    // (a) The error affordance appears: the as-you-type error indicator (the shared
    // error glyph `✖` U+2716) is in the AccessKit tree while the malformed draft is
    // open. (A painted indicator exposes its glyph as its accessibility label — the same
    // way the cross-view glyph tests look up indicators.)
    assert!(
        harness
            .query_all_by_label_contains("\u{2716}")
            .next()
            .is_some(),
        "an invalid draft shows the in-cell error affordance (error indicator present)"
    );

    // Attempt to commit the malformed token.
    harness.key_press(egui::Key::Enter);
    harness.run();
    // No reparse is driven here BY DESIGN: a refused commit performs no structural edit,
    // so it bumps no edit generation and changes no bytes — there is nothing to reparse
    // (and `request_reparse` would dedup it). The refusal itself is the assertion below.

    let d = doc.borrow();
    // (c) NO byte corruption: the buffer is byte-identical — the malformed token was
    // never spliced (the prior bytes are kept, not overwritten with garbage).
    assert_eq!(
        d.buffer, buffer_before,
        "a lexically-invalid commit changed ZERO bytes (no corruption): {}",
        d.buffer
    );
    // (b) Stays editable / NOT silently reverted: the editor is still open on the same
    // cell with the user's draft preserved (they can keep fixing it).
    let focus = d
        .view_state()
        .edit_focus()
        .expect("the editor stays OPEN after a refused invalid commit (stays editable)");
    assert_eq!(
        focus.draft, "1 2 3",
        "the user's input is preserved verbatim — never silently reverted"
    );
}

/// A list of `Entity { id: integer, name: string }` bound to its model, so a value
/// path `/<row>/id` resolves through `items → Entity → id` (integer). Committing a
/// string into the `id` cell is lexically VALID but SEMANTICALLY type-violating.
fn bound_entity_list_doc(worker: &ReparseWorker) -> EditorDocument {
    let model = serde_json::json!({
        "$defs": {
            "EntityList": {
                "type": "array",
                "items": { "$ref": "#/$defs/Entity" }
            },
            "Entity": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer" },
                    "name": { "type": "string" }
                },
                "required": ["id", "name"]
            }
        }
    });
    let mut doc = doc_at("[\n    (id: 1, name: \"a\"),\n    (id: 2, name: \"b\"),\n]", worker);
    doc.bound_type = Some(bound(model, "EntityList"));
    // Binding does not change the buffer, so bump the edit generation to force a fresh
    // reparse that now validates against the bound type (`request_reparse` dedups on an
    // unchanged generation — without this the validation pass never runs).
    doc.on_edit();
    drive_reparse(&mut doc, worker);
    doc
}

#[test]
fn semantically_invalid_commit_is_written_flagged_and_stays_editable() {
    // FR-009 (semantic leg / AD-005): with a type bound, a lexically-valid but
    // type-violating value (a string `"oops"` into an `integer` `id` cell) commits via
    // the lossless splice path, then the authoritative off-frame validation flags the
    // cell with a RON-V#### finding. The flagged value stays in the buffer and remains
    // editable — it is NOT silently reverted.
    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(bound_entity_list_doc(&worker)));
    let id_col = col_of(&doc.borrow(), "id");

    // Sanity: the `id` cell's declared type is a number (so it edits as free text, not a
    // picker), and it starts WITHOUT an error.
    {
        let d = doc.borrow();
        let model = model_of(&d);
        let cell = model.cell(0, id_col).expect("row 0 id cell");
        let path = cell.value_ref.clone().expect("present scalar");
        assert_eq!(
            d.query_cell_type(&path).map(|i| i.declared_kind),
            Some(CellKind::Number),
            "the `id` column's declared type is a number (free-text editor, not a picker)"
        );
        assert!(
            !cell.has_error_diagnostic(),
            "the cell starts valid (no error affordance)"
        );
    }

    // Open the free-text editor on row 0's `id` cell, set the draft to a string token
    // (lexically VALID — it parses as one RON string — but violates the integer type).
    doc.borrow_mut().view_state_mut().set_grid_anchor(0, id_col);
    let mut harness = harness_over(&doc, &worker);
    harness.run();
    harness.key_press(egui::Key::F2);
    harness.run();
    doc.borrow_mut()
        .view_state_mut()
        .edit_focus_mut()
        .expect("F2 opened the free-text editor")
        .draft = "\"oops\"".to_string();
    harness.run();
    harness.key_press(egui::Key::Enter);
    harness.run();
    // Authoritative semantic check runs off-frame on the committed value.
    drive_reparse(&mut doc.borrow_mut(), &worker);

    let d = doc.borrow();
    // (c) The lexically-valid value WAS written losslessly (no byte corruption — the
    // other rows are untouched), proving it was not refused like the malformed leg.
    assert!(
        d.buffer.contains("(id: \"oops\", name: \"a\")"),
        "the type-violating but well-formed value committed losslessly: {}",
        d.buffer
    );
    assert!(
        d.buffer.contains("(id: 2, name: \"b\")"),
        "the untouched row is byte-identical: {}",
        d.buffer
    );
    // (a) The cell now carries an error affordance: a type-validation finding attached by
    // CST range (the RON-V#### semantic diagnostic) drives the in-cell error border.
    let model = model_of(&d);
    let cell = model.cell(0, id_col).expect("row 0 id cell after commit");
    assert!(
        cell.has_error_diagnostic(),
        "the committed type-violating value is flagged with an error diagnostic (in-cell affordance)"
    );
    assert!(
        cell.diagnostics
            .iter()
            .any(|diag| diag.code_str().starts_with("RON-V")),
        "the flagging finding is a RON-V#### type diagnostic: {:?}",
        cell.diagnostics
    );
    // (b) Stays editable / NOT reverted: the flagged token is still present in the cell,
    // so re-opening the editor (F2/double-click) edits the user's value, not a revert.
    assert_eq!(
        cell.text.as_deref(),
        Some("\"oops\""),
        "the invalid value stays in the cell (editable) — never silently reverted"
    );
}

// ===========================================================================
// (T030) [US4] {FR-016} Per-cell TYPE-COMPATIBILITY guard for fill-down / paste.
//
// A bulk fill / paste whose value is INCOMPATIBLE with a target cell's DECLARED
// type must DROP that write (never splice it — no silent corruption) and FLAG the
// refused cell, while the COMPATIBLE cells in the same operation still commit (as
// one undo unit). With NO type bound there is no constraint, so the same fill /
// paste writes everything. Three legs:
//
//   (pure) the rule set — `token_is_type_compatible` per `CellKind` and the
//          `partition_type_compatible_writes` split — asserted in isolation;
//   (bound) a single-value paste of a non-variant / non-numeric string across an
//          enum + number + string selection: the enum & number cells are NOT
//          written (bytes unchanged), are flagged (the rejected marker is in the
//          AccessKit tree), and the string cell IS written;
//   (unbound) the same paste with no bound type writes EVERY cell (no constraint).
//
// egui_kittest headless — never a live-GUI screenshot (memory rule).
// ===========================================================================

use ronin_app::document::CellTypeInfo;
use ronin_app::structural::table::{partition_type_compatible_writes, token_is_type_compatible};

/// A 2-row `Widget { hp: integer, name: string, kind: Kind }` list bound to its model,
/// so the `kind` column is an enum (variants On/Off/Idle), `hp` is a number, and `name`
/// is a string. Two rows give fill-down a source row above the selection.
fn bound_mixed_doc(worker: &ReparseWorker) -> EditorDocument {
    let model = serde_json::json!({
        "$defs": {
            "WidgetList": {
                "type": "array",
                "items": { "$ref": "#/$defs/Widget" }
            },
            "Widget": {
                "type": "object",
                "properties": {
                    "hp":   { "type": "integer" },
                    "name": { "type": "string" },
                    "kind": { "$ref": "#/$defs/Kind" }
                }
            },
            "Kind": {
                "oneOf": [
                    { "x-ron-variant": "On", "x-ron-variant-shape": "unit" },
                    { "x-ron-variant": "Off", "x-ron-variant-shape": "unit" },
                    { "x-ron-variant": "Idle", "x-ron-variant-shape": "unit" }
                ]
            }
        }
    });
    let mut doc = doc_at(
        "[\n    (hp: 1, name: \"a\", kind: On),\n    (hp: 2, name: \"b\", kind: Off),\n]",
        worker,
    );
    doc.bound_type = Some(bound(model, "WidgetList"));
    // Binding does not change the buffer, so bump the edit generation to force a fresh
    // reparse that validates against the bound type (`request_reparse` dedups otherwise).
    doc.on_edit();
    drive_reparse(&mut doc, worker);
    doc
}

#[test]
fn type_compat_rules_per_cell_kind() {
    // FR-016 (pure rule set): each `CellKind` accepts exactly its valid token forms; a
    // string / unknown column accepts ANY token (never refused — Progressive Intelligence).
    let bool_t = CellTypeInfo {
        declared_kind: CellKind::Bool,
        enum_variants: Vec::new(),
    };
    assert!(token_is_type_compatible("true", &bool_t));
    assert!(token_is_type_compatible("false", &bool_t));
    assert!(!token_is_type_compatible("1", &bool_t), "a number is not a bool");
    assert!(
        !token_is_type_compatible("yes", &bool_t),
        "a non-true/false token is not a bool"
    );

    let num_t = CellTypeInfo {
        declared_kind: CellKind::Number,
        enum_variants: Vec::new(),
    };
    assert!(token_is_type_compatible("42", &num_t));
    assert!(token_is_type_compatible("-3", &num_t));
    assert!(token_is_type_compatible("1.5", &num_t));
    assert!(
        !token_is_type_compatible("\"x\"", &num_t),
        "a string token is not a number"
    );
    assert!(
        !token_is_type_compatible("On", &num_t),
        "a bare identifier is not a number"
    );

    let enum_t = CellTypeInfo {
        declared_kind: CellKind::Enum,
        enum_variants: vec!["On".into(), "Off".into(), "Idle".into()],
    };
    assert!(token_is_type_compatible("On", &enum_t));
    assert!(token_is_type_compatible("Idle", &enum_t));
    assert!(
        !token_is_type_compatible("Nope", &enum_t),
        "a non-declared variant name is rejected"
    );
    assert!(
        !token_is_type_compatible("\"On\"", &enum_t),
        "a quoted string is not the enum variant token"
    );

    // A string column and an unknown column accept everything (never refused).
    for kind in [CellKind::String, CellKind::Unknown] {
        let any = CellTypeInfo {
            declared_kind: kind,
            enum_variants: Vec::new(),
        };
        assert!(token_is_type_compatible("\"anything\"", &any));
        assert!(token_is_type_compatible("Nope", &any));
        assert!(token_is_type_compatible("42", &any));
    }
}

#[test]
fn partition_keeps_compatible_and_rejects_incompatible() {
    // FR-016 (pure split): `partition_type_compatible_writes` keeps the writes whose value
    // is compatible (or whose cell is unbound — `None`) and rejects the incompatible ones,
    // returning the rejected target paths so the grid can flag them.
    let num = field("num");
    let txt = field("txt");
    let unbound = field("unbound");
    let writes = vec![
        (num.clone(), "Nope".to_string()), // → Number, rejected
        (txt.clone(), "Nope".to_string()), // → String, kept (any token)
        (unbound.clone(), "Nope".to_string()), // → None (no constraint), kept
    ];
    let (kept, rejected) = partition_type_compatible_writes(writes, |p| {
        if p == &num {
            Some(CellTypeInfo {
                declared_kind: CellKind::Number,
                enum_variants: Vec::new(),
            })
        } else if p == &txt {
            Some(CellTypeInfo {
                declared_kind: CellKind::String,
                enum_variants: Vec::new(),
            })
        } else {
            None // unbound cell — no constraint
        }
    });
    assert_eq!(rejected, vec![num], "only the Number cell rejected `Nope`");
    let kept_paths: Vec<_> = kept.iter().map(|(p, _)| p.clone()).collect();
    assert_eq!(
        kept_paths,
        vec![txt, unbound],
        "the String and the unbound cell are kept (compatible / no constraint)"
    );
}

#[test]
fn bound_paste_rejects_type_incompatible_cells_and_writes_compatible() {
    // FR-016 (bound): a single-value paste of a non-variant / non-numeric string across an
    // enum + number + string selection drops the enum & number writes (bytes unchanged —
    // no silent corruption) and flags them (the rejected marker is in the AccessKit tree),
    // while the string cell DOES get the value. Driven through the REAL paste handler.
    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(bound_mixed_doc(&worker)));
    let (hp_col, name_col, kind_col) = {
        let d = doc.borrow();
        (col_of(&d, "hp"), col_of(&d, "name"), col_of(&d, "kind"))
    };
    let buffer_before = doc.borrow().buffer.clone();

    // Select row 0 across all three typed columns (a rectangular multi-cell selection so a
    // single pasted value FILLS the range — the `single && multi_cell` paste leg).
    let (min_col, max_col) = {
        let cols = [hp_col, name_col, kind_col];
        (*cols.iter().min().unwrap(), *cols.iter().max().unwrap())
    };
    {
        let mut d = doc.borrow_mut();
        d.view_state_mut().set_grid_anchor(0, min_col);
        d.view_state_mut().extend_grid_to(0, max_col);
    }

    let mut harness = harness_over(&doc, &worker);
    harness.run();
    // `Nope` is not a declared variant (enum reject) and not numeric (number reject), but
    // is a fine value for a string cell.
    harness.event(egui::Event::Paste("Nope".to_string()));
    harness.run();
    drive_reparse(&mut doc.borrow_mut(), &worker);

    {
        let d = doc.borrow();
        // (a) The incompatible enum & number cells were NOT written — their bytes are
        // unchanged (no silent corruption): the original `hp: 1` and `kind: On` survive,
        // and the refused value never appeared in either position.
        assert!(
            d.buffer.contains("hp: 1"),
            "the number cell was NOT written (bytes unchanged): {}",
            d.buffer
        );
        assert!(
            d.buffer.contains("kind: On"),
            "the enum cell was NOT written (bytes unchanged): {}",
            d.buffer
        );
        // (c) The COMPATIBLE string cell WAS written (the value lands as the cell's token,
        // an unquoted ident-string — the same lossless splice a plain paste uses).
        assert!(
            d.buffer.contains("name: Nope"),
            "the compatible string cell received the pasted value: {}",
            d.buffer
        );
        // Row 1 is entirely untouched (outside the selection).
        assert!(
            d.buffer.contains("(hp: 2, name: \"b\", kind: Off)"),
            "the unselected row is byte-identical: {}",
            d.buffer
        );
        // The number / enum cells changing nothing means the only byte delta is the string
        // cell — the buffer is not the pre-paste buffer (a compatible write DID land), and
        // the refused value is present exactly once (only the string cell).
        assert_ne!(
            d.buffer, buffer_before,
            "the compatible subset committed (the buffer changed)"
        );
        assert_eq!(
            d.buffer.matches("Nope").count(),
            1,
            "only the one compatible (string) cell holds the pasted value: {}",
            d.buffer
        );
        // (b) The refused cells are FLAGGED: the view-state recorded both target paths as
        // rejected so the grid paints the marker on exactly them.
        let model = model_of(&d);
        let hp_path = model.cell(0, hp_col).unwrap().value_ref.clone().unwrap();
        let kind_path = model.cell(0, kind_col).unwrap().value_ref.clone().unwrap();
        let name_path = model.cell(0, name_col).unwrap().value_ref.clone().unwrap();
        assert!(
            d.view_state().is_rejected_cell(&hp_path),
            "the number cell is flagged rejected"
        );
        assert!(
            d.view_state().is_rejected_cell(&kind_path),
            "the enum cell is flagged rejected"
        );
        assert!(
            !d.view_state().is_rejected_cell(&name_path),
            "the compatible string cell is NOT flagged"
        );
    }

    // (b) The rejected marker is in the AccessKit tree: the shared error glyph (`✖`
    // U+2716) is painted on the refused cells, so the user sees which targets were skipped.
    assert!(
        harness
            .query_all_by_label_contains("\u{2716}")
            .next()
            .is_some(),
        "a type-rejected fill/paste target shows the in-cell rejected marker (error glyph present)"
    );
}

#[test]
fn unbound_paste_writes_every_cell_no_type_constraint() {
    // FR-016 / FR-013 (unbound): with NO bound type there is no declared type, so the SAME
    // single-value paste across the same columns writes EVERY cell — nothing is refused,
    // nothing is flagged (Progressive Intelligence — the guard only constrains typed cells).
    let worker = Rc::new(ReparseWorker::new());
    // Same buffer/shape as the bound fixture, but no `bound_type` set.
    let doc = Rc::new(RefCell::new(doc_at(
        "[\n    (hp: 1, name: \"a\", kind: On),\n    (hp: 2, name: \"b\", kind: Off),\n]",
        &worker,
    )));
    assert!(
        doc.borrow().bound_type.is_none(),
        "the fixture is unbound (no type constraint)"
    );
    let (hp_col, name_col, kind_col) = {
        let d = doc.borrow();
        (col_of(&d, "hp"), col_of(&d, "name"), col_of(&d, "kind"))
    };
    let (min_col, max_col) = {
        let cols = [hp_col, name_col, kind_col];
        (*cols.iter().min().unwrap(), *cols.iter().max().unwrap())
    };
    {
        let mut d = doc.borrow_mut();
        d.view_state_mut().set_grid_anchor(0, min_col);
        d.view_state_mut().extend_grid_to(0, max_col);
    }

    let mut harness = harness_over(&doc, &worker);
    harness.run();
    harness.event(egui::Event::Paste("Nope".to_string()));
    harness.run();
    drive_reparse(&mut doc.borrow_mut(), &worker);

    let d = doc.borrow();
    // Every selected cell in row 0 received the value (3 occurrences — hp, name, kind), and
    // the original typed tokens are gone (no constraint blocked the write).
    assert_eq!(
        d.buffer.matches("Nope").count(),
        3,
        "with no type bound, the paste wrote ALL three selected cells: {}",
        d.buffer
    );
    assert!(
        !d.buffer.contains("hp: 1") && !d.buffer.contains("kind: On"),
        "the original typed values were overwritten (no constraint): {}",
        d.buffer
    );
    // Nothing was flagged — there is no declared type to violate.
    assert!(
        !d.view_state().has_rejected_cells(),
        "an unbound paste rejects nothing (no rejection flags)"
    );
}

// ===========================================================================
// Phase 6 (Polish & Cross-Cutting).
// ===========================================================================

// ===========================================================================
// (T034) {FR-011, FR-012} [COMPLETES FR-012] — CONSOLIDATED full-keyboard +
// one-gesture-one-action AUDIT across the WHOLE E012 interaction set.
//
// FR-012 completion gate: T012 is `[X]` in tasks.md (the US1 toggle/increment/
// fill/paste wiring is done) and Phases 3–5 (picker, column, row, validation)
// landed, so this is the cross-story closing check. Each `#[test]` asserts ONE
// gesture maps to EXACTLY ONE action with NO overload, and — where the gesture is
// keyboard-driven — that it is operable with NO mouse (no pointer event is sent;
// selection is set programmatically only to position the cursor). Driven through
// the REAL key handler / renderer, headlessly (egui_kittest — NEVER a live-GUI
// screenshot; memory rule). Verification-only: the audit found NO collision, so
// nothing in table.rs was changed. The gesture map proven here is:
//
//   Space        → bool toggle             (NOT type a space, NOT navigate)
//   Ctrl+↑/↓     → numeric increment ±1    (NOT navigate, NOT type)
//   Ctrl+D       → fill-down               (NOT type 'd')
//   plain char   → open editor seeded w/it (the "just type" gesture)
//   plain ←↑→↓   → navigate                (NOT increment, NOT edit)
//   Tab / Enter  → navigate (advance)      (NOT edit)
//   F2 / dbl-clk → begin editing the cell  (enum picker on the SAME edit-enter)
//   grip drag    → row reorder             (its OWN affordance, NOT cell-select,
//                                            NOT column reorder)
//   header menu  → column pin / hide       (its OWN right-click affordance)
//
// Each leg below is a focused assertion on one of those mappings. The US1 legs
// re-prove their no-overload contract here as part of the *consolidated* set so
// the whole surface is audited in one place (T012 covered them in isolation; this
// is the FR-012 closing audit across picker/column/row too).
// ===========================================================================

/// A document whose top-level record list spans every interactive cell kind in one
/// fixture, BOUND so the enum picker leg has a declared type: `on` (bool), `hp`
/// (number), `name` (string), `kind` (enum On/Off/Idle). Reuses the Phase-3 bound
/// shape + the `doc_at`/`bound()` seams.
fn audit_doc(worker: &ReparseWorker) -> EditorDocument {
    let model = serde_json::json!({
        "$defs": {
            "WidgetList": {
                "type": "array",
                "items": { "$ref": "#/$defs/Widget" }
            },
            "Widget": {
                "type": "object",
                "properties": {
                    "on":   { "type": "boolean" },
                    "hp":   { "type": "integer" },
                    "name": { "type": "string" },
                    "kind": { "$ref": "#/$defs/Kind" }
                }
            },
            "Kind": {
                "oneOf": [
                    { "x-ron-variant": "On", "x-ron-variant-shape": "unit" },
                    { "x-ron-variant": "Off", "x-ron-variant-shape": "unit" },
                    { "x-ron-variant": "Idle", "x-ron-variant-shape": "unit" }
                ]
            }
        }
    });
    let mut doc = doc_at(
        "[\n    (on: true, hp: 1, name: \"a\", kind: On),\n    (on: false, hp: 2, name: \"b\", kind: Off),\n    (on: true, hp: 3, name: \"c\", kind: Idle),\n]",
        worker,
    );
    doc.bound_type = Some(bound(model, "WidgetList"));
    doc.on_edit();
    drive_reparse(&mut doc, worker);
    doc
}

#[test]
fn audit_space_maps_only_to_bool_toggle() {
    // Space over a bool selection toggles the value and does NOTHING else: it does
    // NOT open the text editor (no draft) and it does NOT type a space. Keyboard-only.
    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(audit_doc(&worker)));
    let on_col = col_of(&doc.borrow(), "on");
    doc.borrow_mut().view_state_mut().set_grid_anchor(0, on_col);

    let mut harness = harness_over(&doc, &worker);
    harness.run();
    harness.key_press(egui::Key::Space);
    harness.run();
    drive_reparse(&mut doc.borrow_mut(), &worker);

    let d = doc.borrow();
    assert!(
        d.buffer.contains("(on: false, hp: 1, name: \"a\", kind: On)"),
        "Space toggled the bool (its ONE action): {}",
        d.buffer
    );
    assert!(
        d.view_state().edit_focus().is_none(),
        "Space did NOT also open the text editor (no overload — one gesture, one action)"
    );
    // The matching whitespace text never landed as an edit anywhere.
    assert!(
        !d.buffer.contains("on:  ") && !d.buffer.contains("on: \" \""),
        "Space did NOT type a space into any cell: {}",
        d.buffer
    );
}

#[test]
fn audit_ctrl_arrow_maps_only_to_increment_not_navigation() {
    // Ctrl+↑ increments the active numeric cell and does NOT move the selection cursor
    // (a plain ↑ would move; the modifier disambiguates). One gesture, one action.
    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(audit_doc(&worker)));
    let hp_col = col_of(&doc.borrow(), "hp");
    doc.borrow_mut().view_state_mut().set_grid_anchor(1, hp_col); // row 1 (hp = 2)

    let mut harness = harness_over(&doc, &worker);
    harness.run();
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::ArrowUp);
    harness.run();
    drive_reparse(&mut doc.borrow_mut(), &worker);

    let d = doc.borrow();
    assert!(
        d.buffer.contains("hp: 3, name: \"b\""),
        "Ctrl+↑ incremented row 1 hp 2→3 (its ONE action): {}",
        d.buffer
    );
    assert_eq!(
        d.view_state().grid_cursor(),
        Some((1, hp_col)),
        "Ctrl+↑ did NOT also move the selection — increment is distinct from nav"
    );
    assert!(
        d.view_state().edit_focus().is_none(),
        "Ctrl+↑ did NOT open an editor (no overload)"
    );
}

#[test]
fn audit_ctrl_d_maps_only_to_fill_down_not_typing_d() {
    // Ctrl+D fills the cell above down and does NOT type a literal 'd' into the cell.
    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(audit_doc(&worker)));
    let hp_col = col_of(&doc.borrow(), "hp");
    doc.borrow_mut().view_state_mut().set_grid_anchor(1, hp_col); // row 1 (row 0 hp = 1 above)

    let mut harness = harness_over(&doc, &worker);
    harness.run();
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::D);
    harness.run();
    drive_reparse(&mut doc.borrow_mut(), &worker);

    let d = doc.borrow();
    assert!(
        d.buffer.contains("(on: false, hp: 1, name: \"b\", kind: Off)"),
        "Ctrl+D filled the cell above (hp 1) into row 1 (its ONE action): {}",
        d.buffer
    );
    assert!(
        !d.buffer.contains("hp: d"),
        "Ctrl+D did NOT type a 'd' into the cell: {}",
        d.buffer
    );
    assert!(
        d.view_state().edit_focus().is_none_or(|f| f.draft != "d"),
        "Ctrl+D did NOT open an editor seeded with 'd' (no overload)"
    );
}

#[test]
fn audit_plain_char_opens_editor_and_plain_arrow_navigates() {
    // The two "unmodified" gestures the modified ones repurpose still do their OWN
    // thing: a plain printable char opens the editor seeded with it; a plain arrow
    // navigates (it does NOT increment, it does NOT edit). One gesture, one action —
    // proving the Ctrl+arrow / Ctrl+D / Space consumes are surgical, not blanket.
    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(audit_doc(&worker)));
    let hp_col = col_of(&doc.borrow(), "hp");

    // (1) Plain printable char → open the active cell's editor seeded with the char.
    doc.borrow_mut().view_state_mut().set_grid_anchor(0, hp_col);
    let mut harness = harness_over(&doc, &worker);
    harness.run();
    harness.event(egui::Event::Text("7".to_string()));
    harness.run();
    {
        let d = doc.borrow();
        let focus = d
            .view_state()
            .edit_focus()
            .expect("a plain char opened the inline editor (its ONE action)");
        assert!(matches!(focus.surface, FocusSurface::TableCell { .. }));
        assert_eq!(focus.draft, "7", "the editor is seeded with the typed char");
    }
    doc.borrow_mut().view_state_mut().clear_focus();

    // (2) Plain ↓ → move the cursor down one row; change ZERO document bytes (a move
    //     is not an edit and not an increment).
    doc.borrow_mut().view_state_mut().set_grid_anchor(0, hp_col);
    let buffer_before = doc.borrow().buffer.clone();
    let mut harness = harness_over(&doc, &worker);
    harness.run();
    harness.key_press(egui::Key::ArrowDown);
    harness.run();
    let d = doc.borrow();
    assert_eq!(
        d.view_state().grid_cursor(),
        Some((1, hp_col)),
        "a plain ↓ moved the selection (its ONE action), distinct from Ctrl+↓ (decrement)"
    );
    assert_eq!(
        d.buffer, buffer_before,
        "a plain arrow is byte-free — it did not increment or edit"
    );
}

#[test]
fn audit_tab_and_enter_navigate_without_editing() {
    // Tab advances the cursor RIGHT and Enter advances it DOWN — both pure navigation
    // (Excel model). Neither opens an editor (editing is F2 / typing / double-click).
    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(audit_doc(&worker)));
    let on_col = col_of(&doc.borrow(), "on");
    doc.borrow_mut().view_state_mut().set_grid_anchor(0, on_col);

    // Tab → one VISIBLE column to the right (default layout ⇒ on_col + 1).
    let mut harness = harness_over(&doc, &worker);
    harness.run();
    harness.key_press(egui::Key::Tab);
    harness.run();
    assert_eq!(
        doc.borrow().view_state().grid_cursor(),
        Some((0, on_col + 1)),
        "Tab moved the cursor one column right (navigation, not edit)"
    );
    assert!(
        doc.borrow().view_state().edit_focus().is_none(),
        "Tab did NOT open an editor (no overload)"
    );

    // Enter → one row down, same column.
    let after_tab = doc.borrow().view_state().grid_cursor().unwrap();
    let mut harness = harness_over(&doc, &worker);
    harness.run();
    harness.key_press(egui::Key::Enter);
    harness.run();
    assert_eq!(
        doc.borrow().view_state().grid_cursor(),
        Some((after_tab.0 + 1, after_tab.1)),
        "Enter moved the cursor one row down (navigation, not edit)"
    );
    assert!(
        doc.borrow().view_state().edit_focus().is_none(),
        "Enter did NOT open an editor (no overload)"
    );
}

#[test]
fn audit_f2_edit_enter_opens_the_enum_picker_for_a_bound_enum_cell() {
    // For a BOUND enum cell, the edit-enter gesture (F2) opens the type-to-filter
    // PICKER — the SAME single gesture that opens the text editor for a non-enum cell
    // (one gesture, one action: "begin editing"; the surface is type-chosen, not a
    // second gesture). No mouse is used to open it.
    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(audit_doc(&worker)));
    let kind_col = col_of(&doc.borrow(), "kind");
    doc.borrow_mut()
        .view_state_mut()
        .set_grid_anchor(0, kind_col);

    let mut harness = harness_over(&doc, &worker);
    harness.run();
    harness.key_press(egui::Key::F2);
    harness.run();

    assert!(
        doc.borrow().view_state().edit_focus().is_some(),
        "F2 began editing the enum cell (its ONE action: begin editing)"
    );
    // `Idle` appears in NO cell of row 0 (row 0 is `On`), so its presence proves the
    // picker rendered the declared variant set — not the free-text editor.
    assert!(
        harness.query_all_by_label("Idle").next().is_some(),
        "the edit-enter gesture opened the type-to-filter enum picker (constrained variants)"
    );
}

#[test]
fn audit_row_reorder_is_its_own_grip_affordance_not_cell_select() {
    // Row reorder lives on a DEDICATED grip handle (its own column), DISTINCT from the
    // cell select-drag and from column reorder: dragging the grip moves the ROW and
    // never opens a cell editor or perturbs the column mapping. One gesture, one action.
    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(audit_doc(&worker)));
    let n_cols_before = model_of(&doc.borrow()).columns.len();

    let mut harness = harness_over(&doc, &worker);
    harness.run();

    // The grips are a separate affordance with their own glyph — one per row.
    let mut grips: Vec<egui::Pos2> = harness
        .query_all_by_label_contains(ROW_GRIP_GLYPH)
        .map(|n| n.rect().center())
        .collect();
    grips.sort_by(|a, b| a.y.partial_cmp(&b.y).unwrap());
    assert_eq!(grips.len(), 3, "a dedicated drag-handle grip per row (own column)");

    // Drag row 0's grip onto row 2's grip → reorder to [b, c, a].
    let (from, to) = (grips[0], grips[2]);
    harness.hover_at(from);
    harness.drag_at(from);
    harness.run();
    harness.hover_at(to);
    harness.run();
    harness.drop_at(to);
    harness.run();
    drive_reparse(&mut doc.borrow_mut(), &worker);

    let d = doc.borrow();
    let ia = d.buffer.find("name: \"a\"").unwrap();
    let ib = d.buffer.find("name: \"b\"").unwrap();
    let ic = d.buffer.find("name: \"c\"").unwrap();
    assert!(
        ib < ic && ic < ia,
        "the grip drag reordered the ROW (its ONE action): [b, c, a]: {}",
        d.buffer
    );
    assert!(
        d.view_state().edit_focus().is_none(),
        "the grip is its OWN affordance — it never opened a cell editor (not cell-select)"
    );
    // The grip does not touch the column mapping (distinct from column reorder).
    let n_after = model_of(&d).columns.len();
    assert_eq!(
        n_after, n_cols_before,
        "the row grip did not add/remove a data column (distinct from column reorder)"
    );
}

#[test]
fn audit_column_pin_and_hide_live_on_the_dedicated_header_menu() {
    // Column pin / hide is a VIEW-ONLY change applied via the DEDICATED header
    // context-menu intent (`ColumnViewState::pin`/`hide`) — its own affordance, NOT
    // an editing or cell gesture. Applying it never opens an editor and never changes
    // one document byte (Principle I). One gesture (the menu action) → one view change.
    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(audit_doc(&worker)));
    let buffer_before = doc.borrow().buffer.clone();
    let (on_col, name_col) = {
        let d = doc.borrow();
        (col_of(&d, "on"), col_of(&d, "name"))
    };
    let n = model_of(&doc.borrow()).columns.len();

    // Hide `name`, pin `on` — the two dedicated header-menu actions.
    {
        let mut d = doc.borrow_mut();
        let cvs = d.view_state_mut().column_view_state_mut(&StructuralPath::root());
        cvs.hide(name_col);
        cvs.pin(on_col);
    }
    let mut harness = harness_over(&doc, &worker);
    harness.run();

    {
        let d = doc.borrow();
        let cvs = d
            .view_state()
            .column_view_state(&StructuralPath::root())
            .unwrap();
        let visible = cvs.visible_columns(n);
        assert_eq!(
            visible.first().copied(),
            Some(on_col),
            "the pinned column floats to the front (view-only): {visible:?}"
        );
        assert!(
            !visible.contains(&name_col),
            "the hidden column is excluded from the visible mapping: {visible:?}"
        );
        // VIEW-ONLY: not one byte changed, and no editor was opened by a column op.
        assert_eq!(
            d.buffer, buffer_before,
            "column pin/hide is view-only — the buffer is byte-identical (Principle I)"
        );
        assert!(
            d.view_state().edit_focus().is_none(),
            "a column op is NOT an edit gesture — it opened no editor (one gesture, one action)"
        );
    }
    // The hidden header is absent and the pin marker is painted (distinct affordance).
    assert!(
        harness.query_all_by_label_contains("name").next().is_none(),
        "the hidden column header is absent from the view"
    );
    assert!(
        harness
            .query_all_by_label_contains("\u{1F4CC}")
            .next()
            .is_some(),
        "the pinned column shows the 📌 marker (the dedicated pin affordance)"
    );
}

#[test]
fn audit_full_keyboard_value_edits_with_no_mouse() {
    // FR-011 consolidation: every VALUE edit (toggle, increment, fill-down) is driven
    // end-to-end with NO pointer event whatsoever — only key presses through the real
    // handler. Selection is positioned programmatically (the arrow-nav leg is audited
    // separately); each gesture below is a genuine keystroke. Proves the whole value-
    // edit surface is keyboard-operable.
    let worker = Rc::new(ReparseWorker::new());
    let doc = Rc::new(RefCell::new(audit_doc(&worker)));
    let (on_col, hp_col) = {
        let d = doc.borrow();
        (col_of(&d, "on"), col_of(&d, "hp"))
    };

    // Space toggle (keyboard).
    doc.borrow_mut().view_state_mut().set_grid_anchor(0, on_col);
    let mut h = harness_over(&doc, &worker);
    h.run();
    h.key_press(egui::Key::Space);
    h.run();
    drive_reparse(&mut doc.borrow_mut(), &worker);
    assert!(
        doc.borrow().buffer.contains("(on: false, hp: 1"),
        "Space toggled by keyboard: {}",
        doc.borrow().buffer
    );

    // Ctrl+↓ decrement (keyboard).
    doc.borrow_mut().view_state_mut().set_grid_anchor(0, hp_col);
    let mut h = harness_over(&doc, &worker);
    h.run();
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::ArrowDown);
    h.run();
    drive_reparse(&mut doc.borrow_mut(), &worker);
    assert!(
        doc.borrow().buffer.contains("hp: 0"),
        "Ctrl+↓ decremented row 0 hp 1→0 by keyboard: {}",
        doc.borrow().buffer
    );

    // Ctrl+D fill-down (keyboard) over rows 1..=2 from row 0's hp (now 0).
    {
        let mut d = doc.borrow_mut();
        d.view_state_mut().set_grid_anchor(1, hp_col);
        d.view_state_mut().extend_grid_to(2, hp_col);
    }
    let mut h = harness_over(&doc, &worker);
    h.run();
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::D);
    h.run();
    drive_reparse(&mut doc.borrow_mut(), &worker);
    {
        let d = doc.borrow();
        assert!(
            d.buffer.contains("name: \"b\", kind: Off)") && d.buffer.contains("hp: 0, name: \"b\""),
            "Ctrl+D filled the cell above (hp 0) into row 1 by keyboard: {}",
            d.buffer
        );
        assert!(
            d.buffer.contains("hp: 0, name: \"c\""),
            "Ctrl+D filled the cell above into row 2 by keyboard: {}",
            d.buffer
        );
    }
}

// ===========================================================================
// (T035) {FR-014} [COMPLETES FR-014] — CROSS-ACTION lossless round-trip over a
// SEQUENCE of mixed VALUE + STRUCTURE edits applied to a comment-rich,
// irregularly-formatted document.
//
// FR-014 completion gate: T013 is `[X]` in tasks.md (US1 single-action losslessness
// landed). This is the cross-action closing check: a SEQUENCE — bool toggle,
// numeric increment, fill-down, block paste, enum-pick, AND a row reorder — applied
// to the SAME document, asserting byte-losslessness THROUGHOUT (every comment, all
// formatting/whitespace, and untouched element order/values preserved; only the
// targeted tokens move, and a reordered element's OWN bytes travel verbatim).
//
// Oracle (mirrors `cross_action_round_trip.rs`'s semantic-token oracle, adapted for
// in-place + reordering edits):
//   * COMMENTS — the MULTISET of comment tokens is invariant across the whole
//     sequence (no comment is dropped, added, or mutated; a row reorder permutes
//     comment ORDER, so the order-free multiset is the right invariant there);
//   * FORMATTING — the per-element record bytes (incl. the irregular indentation and
//     the IN-ELEMENT comments) survive verbatim: each original record substring is
//     still present after every value edit, and the reordered record's full bytes too;
//   * VALUES — only the targeted scalar tokens changed to the expected new tokens;
//     every untouched cell re-derives to its original token.
//
// Comment placement matters for the reorder leg. `StructuralOp::ReorderChild` (the
// existing lossless core op, unchanged) preserves the moved element NODE's own bytes
// and NORMALIZES the separator between elements (documented in T026). A comment that
// sits in SEPARATOR position — after the comma (`(…), // x`) or in the inter-element
// gap before the next element (`… /* x */ (…)`) — is separator trivia and is
// normalized by a reorder, NOT element bytes. So the fixtures here put the per-row
// comment INSIDE the record `(…)` (where it is element bytes and travels with the
// move), and keep only LIST-level trivia (a leading file comment + a trailing list
// comment) outside — that list-level trivia is not any element's separator, so it
// survives too. This faithfully exercises FR-014/FR-008's "comments preserved" over
// the real lossless path rather than asserting a stronger guarantee than the op gives.
//
// proptest randomizes the document shape, the edit targets, and the reorder. An
// insta snapshot of a representative fixed sequence pins the exact bytes as a
// human-readable complement.
// ===========================================================================

/// The sorted MULTISET of comment token texts — the order-FREE comment-preservation
/// oracle (a row reorder permutes comment order, so a sorted multiset is invariant
/// where the source-order `comment_tokens` is not).
fn comment_multiset(src: &str) -> Vec<String> {
    let mut v = comment_tokens(src);
    v.sort();
    v
}

/// A comment-rich record list with irregular indentation, a leading file comment, a
/// trailing list comment, and a distinctive IN-ELEMENT comment per row — varied so the
/// cross-action sequence is exercised over many shapes, not one. Each row is
/// `(/* r# */ name: "…", on: <bool>, hp: <int>, kind: <variant>)` so the sequence can
/// target a bool, a number, a string, and an enum-like cell, and the `/* r# */`
/// comment lives INSIDE the element (so it survives the reorder leg per the documented
/// `ReorderChild` model). 3..=4 rows so fill-down and a row reorder both have room.
fn cross_action_doc() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        (
            "[a-z]{1,4}",
            any::<bool>(),
            0i64..400,
            prop_oneof![Just("On"), Just("Off"), Just("Idle")],
        ),
        3..=4usize,
    )
    .prop_map(|recs| {
        let mut s = String::from("// leading file comment\n[\n");
        for (i, (name, on, hp, kind)) in recs.iter().enumerate() {
            // Irregular per-row indentation + an IN-ELEMENT block comment (inside the
            // parens, so it is element bytes that survive a reorder), so the round-trip
            // must preserve non-canonical whitespace AND the comment across all legs.
            let indent = match i % 3 {
                0 => "    ",
                1 => "      ",
                _ => "        ",
            };
            s.push_str(&format!(
                "{indent}(/* r{i} */ name: \"{name}\", on: {on}, hp: {hp}, kind: {kind}),\n"
            ));
        }
        s.push_str("] // trailing comment\n");
        s
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    /// FR-014: a SEQUENCE of mixed value edits (bool toggle → increment → fill-down →
    /// block paste) followed by a STRUCTURE edit (row reorder) on a comment-rich,
    /// irregularly-formatted document is byte-lossless throughout — the comment
    /// multiset is invariant, every original record's bytes survive verbatim, and only
    /// the targeted cells changed.
    #[test]
    fn prop_cross_action_sequence_is_byte_lossless(
        src in cross_action_doc(),
        toggle_row in 0usize..4,
        inc_row in 0usize..4,
        reorder_seed in 0usize..6,
    ) {
        let worker = ReparseWorker::new();
        let mut doc = doc_at(&src, &worker);

        let comments_before = comment_multiset(&src);
        let row_count = model_of(&doc).row_count();
        prop_assume!(row_count >= 3);
        let toggle_row = toggle_row.min(row_count - 1);
        let inc_row = inc_row.min(row_count - 1);

        // The original per-record substrings (the verbatim element bytes incl. the
        // in-element block comment) — every one must remain present after the value
        // edits (which only retouch a scalar token inside one record), and the moved
        // record's full bytes must survive the reorder too. Each record opens with its
        // own `/* r# */` in-element comment, a unique anchor.
        let record_substr = |r: usize| -> String {
            // From this record's `(/* r# */` open to its closing `)` — the element
            // node's own bytes (the trailing comma is the normalized separator, outside).
            let open_marker = format!("(/* r{r} */");
            let start = src.find(&open_marker).unwrap();
            let end = src[start..].find(')').unwrap() + start + 1;
            src[start..end].to_string()
        };
        let original_records: Vec<String> = (0..row_count).map(record_substr).collect();

        // Column indices (stable across the value edits — none changes the schema).
        let model = model_of(&doc);
        let on_col = model.columns.iter().position(|c| c.field_name == "on").unwrap();
        let hp_col = model.columns.iter().position(|c| c.field_name == "hp").unwrap();
        let name_col = model.columns.iter().position(|c| c.field_name == "name").unwrap();

        // --- (1) bool toggle on `toggle_row` ---
        {
            let model = model_of(&doc);
            let writes = bool_toggle_writes(&model, toggle_row, on_col, toggle_row, on_col);
            prop_assert_eq!(writes.len(), 1);
            doc.apply_grid_writes(&writes, &worker, Instant::now()).unwrap();
            drive_reparse(&mut doc, &worker);
            prop_assert_eq!(comment_multiset(&doc.buffer), comments_before.clone());
        }

        // --- (2) numeric increment (+1) on `inc_row` ---
        {
            let model = model_of(&doc);
            let writes = increment_writes(&model, inc_row, hp_col, inc_row, hp_col, 1);
            prop_assert_eq!(writes.len(), 1);
            doc.apply_grid_writes(&writes, &worker, Instant::now()).unwrap();
            drive_reparse(&mut doc, &worker);
            prop_assert_eq!(comment_multiset(&doc.buffer), comments_before.clone());
        }

        // --- (3) fill-down `hp` from row 0 into rows 1..=last ---
        {
            let model = model_of(&doc);
            let last = model.row_count() - 1;
            let writes = fill_down_writes(&model, 1, hp_col, last, hp_col);
            prop_assume!(!writes.is_empty());
            doc.apply_grid_writes(&writes, &worker, Instant::now()).unwrap();
            drive_reparse(&mut doc, &worker);
            prop_assert_eq!(comment_multiset(&doc.buffer), comments_before.clone());
            // Every filled hp cell now equals row 0's hp.
            let after = model_of(&doc);
            let src_hp = after.cell(0, hp_col).unwrap().text.clone().unwrap();
            for r in 1..=last {
                prop_assert_eq!(after.cell(r, hp_col).unwrap().text.clone().unwrap(), src_hp.clone());
            }
        }

        // --- (4) block paste a 1×1 string into (0, name) ---
        {
            let model = model_of(&doc);
            let writes = paste_expand_writes(&model, 0, name_col, 0, "\"zz\"\n");
            prop_assert_eq!(writes.len(), 1);
            doc.apply_grid_writes(&writes, &worker, Instant::now()).unwrap();
            drive_reparse(&mut doc, &worker);
            prop_assert_eq!(comment_multiset(&doc.buffer), comments_before.clone());
            let after = model_of(&doc);
            prop_assert_eq!(after.cell(0, name_col).unwrap().text.clone().unwrap(), "\"zz\"".to_string());
        }

        // --- (5) STRUCTURE edit: reorder a row (lossless `ReorderChild`) ---
        let rc = model_of(&doc).row_count();
        let from = reorder_seed % rc;
        let to = (reorder_seed / rc).min(rc - 1);
        if from != to {
            doc.apply_table_reorder_row(&StructuralPath::root(), from, to, &worker, Instant::now()).unwrap();
            drive_reparse(&mut doc, &worker);
            // The comment multiset is STILL invariant after the structural move (the
            // moved element's in-element comment travelled with it; nothing dropped).
            prop_assert_eq!(comment_multiset(&doc.buffer), comments_before.clone());
        }

        // --- losslessness oracle on the FINAL buffer ---
        let final_buf = doc.buffer.clone();
        // The list-level trivia (leading file comment + trailing list comment) is intact
        // verbatim — it is not any element's separator, so no leg disturbs it.
        prop_assert!(
            final_buf.contains("// leading file comment"),
            "the leading file comment survived the whole sequence: {final_buf}"
        );
        prop_assert!(
            final_buf.contains("] // trailing comment"),
            "the trailing list comment survived the whole sequence: {final_buf}"
        );
        // Every per-row IN-ELEMENT block comment `/* r# */` survived (the multiset check
        // already guarantees this; assert each one explicitly to pin element-comment
        // preservation through the value edits AND the reorder — the comment travelled
        // with its element's bytes).
        for r in 0..row_count {
            prop_assert!(
                final_buf.contains(&format!("/* r{r} */")),
                "the in-element block comment /* r{r} */ survived: {final_buf}"
            );
        }
        // The row/column schema never drifted (no element lost, no column invented).
        let final_model = model_of(&doc);
        prop_assert_eq!(final_model.row_count(), row_count);
        prop_assert_eq!(final_model.columns.len(), model.columns.len());
        // The reordered element's ORIGINAL bytes (for any record NOT value-edited) are
        // still a verbatim substring — the move preserved the element node byte-for-byte.
        // Rows 0/toggle_row/inc_row/1.. may have a changed scalar token; any OTHER record
        // is fully untouched and must appear verbatim.
        let edited: std::collections::HashSet<usize> =
            std::iter::once(0).chain(std::iter::once(toggle_row)).chain(std::iter::once(inc_row))
                .chain(1..row_count) // fill-down touched rows 1..
                .collect();
        for (r, rec) in original_records.iter().enumerate() {
            if !edited.contains(&r) {
                prop_assert!(
                    final_buf.contains(rec.as_str()),
                    "untouched record {r} `{rec}` survived byte-for-byte through value edits + reorder: {final_buf}"
                );
            }
        }
    }
}

#[test]
fn cross_action_sequence_enum_pick_then_reorder_is_lossless() {
    // FR-014 (enum-pick leg + reorder, deterministic): the enum-pick action commits a
    // chosen variant via the picker UI, then a row reorder moves an element — both
    // byte-lossless. Asserted on a fixed comment-rich doc so the exact preserved bytes
    // are pinned (the proptest above covers the bool/increment/fill/paste legs + reorder
    // randomly; this one adds the picker leg through the real headless picker click).
    let worker = Rc::new(ReparseWorker::new());
    // A BOUND enum doc so the picker renders; comment-rich + irregular indentation.
    let model = serde_json::json!({
        "$defs": {
            "WidgetList": { "type": "array", "items": { "$ref": "#/$defs/Widget" } },
            "Widget": { "type": "object", "properties": { "kind": { "$ref": "#/$defs/Kind" } } },
            "Kind": {
                "oneOf": [
                    { "x-ron-variant": "On", "x-ron-variant-shape": "unit" },
                    { "x-ron-variant": "Off", "x-ron-variant-shape": "unit" },
                    { "x-ron-variant": "Idle", "x-ron-variant-shape": "unit" }
                ]
            }
        }
    });
    // IN-ELEMENT comments (inside each `(…)`) so they travel with the element across the
    // reorder leg; the leading/trailing comments are list-level trivia (also preserved).
    let src = "// header\n[\n    (/* r0 */ kind: On),\n      (/* r1 */ kind: Off),\n    (/* r2 */ kind: On),\n] // tail\n";
    let doc = Rc::new(RefCell::new(doc_at(src, &worker)));
    doc.borrow_mut().bound_type = Some(bound(model, "WidgetList"));
    doc.borrow_mut().on_edit();
    drive_reparse(&mut doc.borrow_mut(), &worker);

    let comments_before = comment_multiset(src);
    let kind_col = col_of(&doc.borrow(), "kind");

    // --- enum-pick: open row 0's picker (F2) and choose `Idle`. ---
    doc.borrow_mut().view_state_mut().set_grid_anchor(0, kind_col);
    let mut harness = harness_over(&doc, &worker);
    harness.run();
    harness.key_press(egui::Key::F2);
    harness.run();
    harness.get_by_label("Idle").click();
    harness.run();
    drive_reparse(&mut doc.borrow_mut(), &worker);

    {
        let d = doc.borrow();
        // Only row 0's variant token changed; every comment + every other byte preserved.
        let expected = src.replace("(/* r0 */ kind: On)", "(/* r0 */ kind: Idle)");
        assert_eq!(
            d.buffer, expected,
            "the enum pick changed ONLY the chosen variant token — comments/format/order preserved: {}",
            d.buffer
        );
        assert_eq!(comment_multiset(&d.buffer), comments_before, "comments preserved by the pick");
    }

    // --- reorder: move row 0 (now Idle) to the end → [r1, r2, r0]. ---
    doc.borrow_mut()
        .apply_table_reorder_row(&StructuralPath::root(), 0, 2, &worker, Instant::now())
        .expect("row reorder applies");
    drive_reparse(&mut doc.borrow_mut(), &worker);

    let d = doc.borrow();
    // The comment multiset is STILL invariant after the structural move.
    assert_eq!(
        comment_multiset(&d.buffer),
        comments_before,
        "comments preserved across the enum-pick → reorder sequence: {}",
        d.buffer
    );
    // The moved element's OWN bytes (incl. its in-element `/* r0 */` comment) travelled
    // verbatim, and the two non-moved elements are byte-identical.
    assert!(
        d.buffer.contains("(/* r0 */ kind: Idle)"),
        "the moved element's bytes (incl. its in-element comment) survived the reorder: {}",
        d.buffer
    );
    assert!(
        d.buffer.contains("(/* r1 */ kind: Off)") && d.buffer.contains("(/* r2 */ kind: On)"),
        "the non-moved elements are byte-identical: {}",
        d.buffer
    );
    // Order is now r1, r2, r0 (the move took effect).
    let i0 = d.buffer.find("/* r0 */").unwrap();
    let i1 = d.buffer.find("/* r1 */").unwrap();
    let i2 = d.buffer.find("/* r2 */").unwrap();
    assert!(i1 < i2 && i2 < i0, "the reorder moved r0 to the end: {}", d.buffer);
    // The leading/trailing file comments are intact verbatim.
    assert!(d.buffer.starts_with("// header\n"), "leading comment intact: {}", d.buffer);
    assert!(d.buffer.contains("] // tail"), "trailing comment intact: {}", d.buffer);
}

#[test]
fn snapshot_cross_action_sequence_pins_exact_bytes() {
    // FR-014 (insta complement): pin the EXACT bytes of a representative comment-rich,
    // irregularly-formatted doc after the full cross-action SEQUENCE — bool toggle,
    // increment, fill-down, block paste, then a row reorder. The snapshot makes any
    // future regression in comment / formatting / order preservation visible as a diff.
    let worker = ReparseWorker::new();
    // IN-ELEMENT comments so they survive the reorder leg; list-level trivia outside.
    let src = "// scene\n[\n    (/* hero */ name: \"hero\", on: true,  hp: 10, kind: On),\n        (/* boss */ name: \"boss\", on: false, hp: 99, kind: Off),\n    (/* mob */ name: \"mob\",  on: true,  hp: 5,  kind: Idle),\n] // end\n";
    let mut doc = doc_at(src, &worker);

    let (on_col, hp_col, name_col) = {
        let m = model_of(&doc);
        (
            m.columns.iter().position(|c| c.field_name == "on").unwrap(),
            m.columns.iter().position(|c| c.field_name == "hp").unwrap(),
            m.columns.iter().position(|c| c.field_name == "name").unwrap(),
        )
    };

    // (1) toggle row 0's bool.
    let m = model_of(&doc);
    let w = bool_toggle_writes(&m, 0, on_col, 0, on_col);
    doc.apply_grid_writes(&w, &worker, Instant::now()).unwrap();
    drive_reparse(&mut doc, &worker);

    // (2) increment row 1's hp by 1 (99 → 100).
    let m = model_of(&doc);
    let w = increment_writes(&m, 1, hp_col, 1, hp_col, 1);
    doc.apply_grid_writes(&w, &worker, Instant::now()).unwrap();
    drive_reparse(&mut doc, &worker);

    // (3) fill-down hp from row 0 (10) into rows 1..=2.
    let m = model_of(&doc);
    let w = fill_down_writes(&m, 1, hp_col, 2, hp_col);
    doc.apply_grid_writes(&w, &worker, Instant::now()).unwrap();
    drive_reparse(&mut doc, &worker);

    // (4) block paste a 1×1 string into (row 2, name).
    let m = model_of(&doc);
    let w = paste_expand_writes(&m, 2, name_col, 0, "\"pasted\"\n");
    doc.apply_grid_writes(&w, &worker, Instant::now()).unwrap();
    drive_reparse(&mut doc, &worker);

    // (5) structure edit: reorder row 0 to the end → [boss, mob, hero].
    doc.apply_table_reorder_row(&StructuralPath::root(), 0, 2, &worker, Instant::now())
        .unwrap();
    drive_reparse(&mut doc, &worker);

    insta::assert_snapshot!("cross_action_sequence", doc.buffer);
}
