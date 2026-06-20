//! US1 live type-validation smoke test (T019, FR-004/FR-006) — [COMPLETES FR-004].
//!
//! Exercises the real off-frame path end-to-end: a document is bound to a small
//! hand-authored `TypeModel`, RON with a wrong-type value AND a missing required
//! field is typed, and the **real** [`ReparseWorker`] round-trip is driven to
//! completion (request → spin-poll until the current result installs). We then
//! assert:
//!
//! * a precise **type** squiggle appears — a [`DiagnosticView`] carrying a
//!   `RON-V####` code whose [`source`](ronin_core::DiagnosticCode::source) is
//!   `"ronin-types"`, at the expected char range (FR-004);
//! * a Problems-panel entry exists for it and is rendered through the real
//!   `problems_panel` widget via the renderer-free `egui_kittest` harness (FR-004);
//! * editing to fix the value refreshes — the type diagnostic disappears on the
//!   next landed result (live refresh, replace-not-merge for the type set, FR-006);
//! * clicking the Problems entry queues a pending caret jump to its range
//!   (click-to-jump, FR-004).
//!
//! The worker round-trip is the true off-frame path (FR-006): the worker thread
//! runs `ronin-validate` after the structural parse and ships the merged result
//! back; the document republishes the whole type set each pass. The worker is
//! driven with a bounded spin-poll (no fixed sleeps), mirroring the existing
//! `edit_feedback.rs` / `problems_nav.rs` harness.

use std::sync::Arc;
use std::time::{Duration, Instant};

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;

use ronin_app::document::EditorDocument;
use ronin_app::problems_panel::problems_panel;
use ronin_app::reparse::{BoundType, ReparseWorker};

/// Request a reparse and spin-poll until a current result installs, or panic on
/// timeout. This drives the *real* off-frame worker (parse + validate) to
/// completion.
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

/// A minimal `TypeModel`: an `Entity { id: integer, name: string }` with both
/// fields required. Serialized as JSON-Schema 2020-12 + `$defs`, the interchange
/// `ronin-validate` consumes (E004).
fn entity_model() -> serde_json::Value {
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": {
            "Entity": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer" },
                    "name": { "type": "string" }
                },
                "required": ["id", "name"],
                "additionalProperties": true
            }
        }
    })
}

/// Bind `doc` to `Entity` from the hand-authored model.
fn bind_entity(doc: &mut EditorDocument) {
    doc.bound_type = Some(BoundType {
        model: Arc::new(entity_model()),
        type_name: "Entity".to_string(),
    });
}

#[test]
fn type_error_yields_squiggle_and_problem_then_refreshes_on_fix() {
    let worker = ReparseWorker::new();
    let mut doc = EditorDocument::new_untitled(1);
    bind_entity(&mut doc);

    // `id` is a string where an integer is required (wrong-type value), and the
    // required `name` field is missing. Both should surface as type diagnostics.
    doc.buffer = "(id: \"oops\")".to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);

    // There must be at least one type finding, and a precise type squiggle exists:
    // a DiagnosticView with a RON-V#### code whose source() == "ronin-types".
    let type_views: Vec<_> = doc
        .diagnostics
        .iter()
        .filter(|v| v.code.source() == "ronin-types")
        .cloned()
        .collect();
    assert!(
        !type_views.is_empty(),
        "a bound document with a wrong-type value + missing required field must \
         produce at least one type diagnostic, got {:?}",
        doc.diagnostics
    );
    // Every type view carries a RON-V#### code (distinct namespace from RON-P).
    for v in &type_views {
        assert!(
            v.code.code().starts_with("RON-V"),
            "type diagnostic must carry a RON-V#### code, got {}",
            v.code.code()
        );
        assert!(
            v.code.source() == "ronin-types",
            "type diagnostic source must be ronin-types"
        );
    }

    // The wrong-type value `"oops"` lives at chars 5..11 (`(id: |"oops"|)`); the
    // type-mismatch squiggle must land on that precise value span.
    let buffer = doc.buffer.clone();
    let value_start = buffer.char_indices().position(|(_, c)| c == '"').unwrap();
    let value_end = value_start
        + buffer[value_start..]
            .char_indices()
            .skip(1)
            .find(|(_, c)| *c == '"')
            .map(|(i, _)| i)
            .unwrap()
        + 1;
    let mismatch = type_views.iter().find(|v| {
        v.code == ronin_core::DiagnosticCode::TypeMismatch
            && v.char_range == (value_start, value_end)
    });
    assert!(
        mismatch.is_some(),
        "the type-mismatch squiggle must cover the wrong-type value span \
         {value_start}..{value_end}, got type views {type_views:?}"
    );

    // A missing-required diagnostic must also be present (RON-V0002).
    assert!(
        type_views
            .iter()
            .any(|v| v.code == ronin_core::DiagnosticCode::MissingRequiredField),
        "the missing required `name` field must surface as a type diagnostic, \
         got {type_views:?}"
    );

    // Problems panel: render the real widget through the renderer-free harness and
    // confirm a type entry (tagged `ronin-types`) is listed. Capture the click index.
    let diagnostics = doc.diagnostics.clone();
    let clicked = std::cell::Cell::new(None);
    let mut harness = Harness::new_ui(|ui| {
        if let Some(idx) = problems_panel(ui, &diagnostics) {
            clicked.set(Some(idx));
        }
    });
    harness.run();
    // A type finding's row is tagged with the `ronin-types` source. There may be
    // more than one such row (type mismatch + missing required), so match any.
    assert!(
        harness
            .query_all_by_label_contains("(ronin-types)")
            .next()
            .is_some(),
        "the Problems panel must list a type entry tagged (ronin-types)"
    );
    // The type-mismatch row's RON-V0001 code is shown; click it.
    harness.get_by_label_contains("RON-V0001").click();
    harness.run();
    let idx = clicked
        .get()
        .expect("clicking the type-mismatch Problems row must return its index");
    assert_eq!(
        doc.diagnostics[idx].code,
        ronin_core::DiagnosticCode::TypeMismatch,
        "the clicked row must be the type-mismatch finding"
    );

    // Click-to-jump: applying the selection queues a pending caret jump to the
    // diagnostic's range start (the precise type-error location).
    doc.request_cursor_jump(doc.diagnostics[idx].char_range.0);
    assert!(
        doc.has_pending_cursor_jump(),
        "clicking a type Problems row must queue a cursor jump"
    );
    let jump = doc.take_cursor_jump().expect("a jump must be pending");
    assert_eq!(
        jump, value_start,
        "the cursor jump must target the type diagnostic's char-range start"
    );

    // Live refresh (FR-006): fix the value to a valid integer and add the missing
    // field; the next landed result must drop ALL type diagnostics (replace, not
    // merge for the type set).
    doc.buffer = "(id: 42, name: \"ok\")".to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);
    let remaining_type: Vec<_> = doc
        .diagnostics
        .iter()
        .filter(|v| v.code.source() == "ronin-types")
        .collect();
    assert!(
        remaining_type.is_empty(),
        "fixing the value + adding the required field must clear the type \
         diagnostics on the next landed result (live refresh), got {remaining_type:?}"
    );
}

#[test]
fn no_binding_yields_only_structural_diagnostics() {
    // FR-015: with no resolved binding, only structural diagnostics are produced
    // — the off-frame worker runs no validation.
    let worker = ReparseWorker::new();
    let mut doc = EditorDocument::new_untitled(1);
    // No `bound_type` set: validation must not run.
    doc.buffer = "(id: \"oops\")".to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);

    assert!(
        doc.diagnostics
            .iter()
            .all(|v| v.code.source() == "ronin-core"),
        "without a binding, no type (ronin-types) diagnostics may appear, got {:?}",
        doc.diagnostics
    );
    // The structural parse of well-formed-but-wrong-type RON is itself valid, so
    // there should be no structural diagnostics either.
    assert!(
        doc.diagnostics.is_empty(),
        "structurally-valid RON with no binding must yield zero diagnostics, got {:?}",
        doc.diagnostics
    );
}
