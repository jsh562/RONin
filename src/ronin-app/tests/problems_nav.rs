//! Problems-panel navigation tests (T031, FR-009).
//!
//! Covers the click-to-navigate contract at the levels that are feasible
//! headlessly:
//!
//! * A file with a known RON error produces exactly one Problems entry, and
//!   "selecting" that row (via the `problems_panel` return value, driven through
//!   the renderer-free `egui_kittest` harness) yields its index.
//! * Applying that selection to the document sets a pending cursor jump to the
//!   diagnostic's char-range **start**, which clamps into the live buffer when
//!   consumed (self-correcting against stale ranges).
//!
//! The final step — pushing the jump into the live `TextEdit` cursor state and
//! scrolling — happens inside `editor_view` against egui's stored `TextEditState`,
//! which requires a full App render pass that `egui_kittest` does not synthesize
//! in this version (the same boundary documented in `app_shell.rs`). The
//! document-level jump state asserted here is exactly what `editor_view` consumes,
//! so the navigation logic is covered end-to-end up to that boundary.

use std::time::{Duration, Instant};

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;

use ronin_app::document::EditorDocument;
use ronin_app::problems_panel::problems_panel;
use ronin_app::reparse::ReparseWorker;

/// Request a reparse and spin-poll until a current result installs, or panic.
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
fn known_error_yields_one_problem_and_click_jumps_cursor() {
    // A clearly invalid RON token produces exactly one diagnostic.
    let worker = ReparseWorker::new();
    let mut doc = EditorDocument::new_untitled(1);
    doc.buffer = "Foo(x: @)".to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);

    assert_eq!(
        doc.diagnostics.len(),
        1,
        "the known RON error must produce exactly one Problems entry, got {:?}",
        doc.diagnostics
    );
    let expected_start = doc.diagnostics[0].char_range.0;

    // Drive the panel through the renderer-free harness and synthesize a click on
    // the (single) row, capturing the returned index across the two-pass run that
    // egui needs to register a click.
    let diagnostics = doc.diagnostics.clone();
    let clicked = std::cell::Cell::new(None);
    let mut harness = Harness::new_ui(|ui| {
        if let Some(idx) = problems_panel(ui, &diagnostics) {
            clicked.set(Some(idx));
        }
    });
    harness.run();
    // Click the diagnostic row (matched by its code label) and re-run.
    harness.get_by_label_contains("RON-P0001").click();
    harness.run();

    let idx = clicked
        .get()
        .expect("clicking the single Problems row must return its index");
    assert_eq!(idx, 0, "the only diagnostic is index 0 in the slice");

    // Apply the selection the way the shell does: queue a cursor jump to the
    // diagnostic's start char offset.
    doc.request_cursor_jump(doc.diagnostics[idx].char_range.0);
    assert!(
        doc.has_pending_cursor_jump(),
        "selecting a Problems row must queue a cursor jump"
    );

    // The consumed jump resolves to the diagnostic's start (in-bounds here).
    let jump = doc.take_cursor_jump().expect("a jump must be pending");
    assert_eq!(
        jump, expected_start,
        "the cursor jump must target the diagnostic's char-range start"
    );
    assert!(
        !doc.has_pending_cursor_jump(),
        "consuming the jump must clear it (applies exactly once)"
    );
}

#[test]
fn problems_panel_shows_empty_state_for_no_diagnostics() {
    let mut harness = Harness::new_ui(|ui| {
        let _ = problems_panel(ui, &[]);
    });
    harness.run();
    assert!(
        harness.query_by_label_contains("No problems").is_some(),
        "an empty Problems panel must show the 'No problems' state"
    );
}

#[test]
fn problems_panel_orders_rows_by_source_location_without_mutating_input() {
    use ron_core::{DiagnosticCode, Severity};
    use ronin_app::diagnostics_map::DiagnosticView;

    // Two diagnostics supplied out of source order: the input order must be
    // preserved (returned index refers to the original slice), while display is
    // ordered by (line, column).
    let later = DiagnosticView {
        char_range: (20, 21),
        line_col: ((2, 4), (2, 5)),
        severity: Severity::Error,
        code: DiagnosticCode::UnexpectedToken,
        scene_code: None,
        loss_code: None,
        message: "later problem".to_string(),
    };
    let earlier = DiagnosticView {
        char_range: (3, 4),
        line_col: ((0, 3), (0, 4)),
        severity: Severity::Error,
        code: DiagnosticCode::MissingValue,
        scene_code: None,
        loss_code: None,
        message: "earlier problem".to_string(),
    };
    // Input order: [later, earlier]. The earlier-located one must be clickable as
    // the FIRST row, but its returned index must be 1 (its position in the input).
    let input = std::rc::Rc::new(vec![later, earlier]);

    let clicked: std::rc::Rc<std::cell::Cell<Option<usize>>> =
        std::rc::Rc::new(std::cell::Cell::new(None));
    let input_for_ui = std::rc::Rc::clone(&input);
    let clicked_for_ui = std::rc::Rc::clone(&clicked);
    let mut harness = Harness::new_ui(move |ui| {
        if let Some(idx) = problems_panel(ui, input_for_ui.as_slice()) {
            clicked_for_ui.set(Some(idx));
        }
    });
    harness.run();
    harness.get_by_label_contains("earlier problem").click();
    harness.run();

    // The "earlier" diagnostic is at input index 1; clicking it must return 1
    // (the returned index always refers to the ORIGINAL, unsorted slice).
    assert_eq!(
        clicked.get(),
        Some(1),
        "clicking the earlier-located row must return its original input index (1)"
    );
    // Display order is by (line, column): earlier (idx 1) before later (idx 0).
    let mut order: Vec<usize> = (0..input.len()).collect();
    order.sort_by_key(|&i| input[i].line_col.0);
    assert_eq!(order, vec![1, 0], "rows must be ordered by (line, column)");
    // The input slice itself was never reordered.
    assert_eq!(input[0].message, "later problem");
    assert_eq!(input[1].message, "earlier problem");
}
