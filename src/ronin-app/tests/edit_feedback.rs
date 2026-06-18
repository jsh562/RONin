//! US2 "edit with live feedback" tests (T027, FR-005/FR-006/FR-017/FR-019).
//!
//! Covers:
//! * Typing invalid RON keeps the app responsive (no panic) and updates
//!   diagnostics once an off-thread reparse lands (FR-006).
//! * Highlighting derives from the `ronin-core` CST token stream (FR-019).
//! * An oversize buffer disables highlighting/squiggles while staying editable;
//!   the threshold crossing is observable via the degrade indicator (FR-017).
//!
//! The worker round-trip is driven deterministically with a bounded spin-poll
//! (no fixed sleeps): we request a reparse and drain `poll_parse` until the
//! current result installs or a generous timeout elapses.

use std::time::{Duration, Instant};

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;

use ronin_app::document::EditorDocument;
use ronin_app::editor_view::{build_highlight_model, editor_view, HighlightClass};
use ronin_app::reparse::{ParseResult, ReparseWorker};

/// `true` if any node in the rendered tree has a `value` containing `needle`.
///
/// Multiline `TextEdit` content is exposed as the AccessKit node *value*.
fn node_value_contains(harness: &Harness<'_>, needle: &str) -> bool {
    // `query_all_by` (not `query_by`) because a `TextEdit` and its inner `TextRun`
    // both carry the value, so more than one node legitimately matches.
    harness
        .query_all_by(|node| node.value().is_some_and(|v| v.contains(needle)))
        .next()
        .is_some()
}

/// Request a reparse and spin-poll until a current result installs, or panic on
/// timeout.
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
fn typing_invalid_ron_updates_diagnostics_without_panic() {
    let worker = ReparseWorker::new();
    let mut doc = EditorDocument::new_untitled(1);

    // Type well-formed RON first; expect zero diagnostics after the parse lands.
    doc.buffer = "Foo(x: 1)".to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);
    assert!(
        doc.diagnostics.is_empty(),
        "well-formed RON should yield no diagnostics, got {:?}",
        doc.diagnostics
    );

    // Now type clearly invalid RON; diagnostics must appear, no panic.
    doc.buffer = "Foo(x: @)".to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);
    assert!(
        !doc.diagnostics.is_empty(),
        "invalid RON must produce at least one diagnostic"
    );
}

#[test]
fn last_good_diagnostics_are_kept_until_a_fresh_result_lands() {
    // FR-006: editing must not clear the last-good diagnostics before a new
    // result arrives.
    let worker = ReparseWorker::new();
    let mut doc = EditorDocument::new_untitled(1);
    doc.buffer = "Foo(x: @)".to_string();
    doc.on_edit();
    drive_reparse(&mut doc, &worker);
    let before = doc.diagnostics.clone();
    assert!(!before.is_empty());

    // Mutate the buffer but do NOT poll yet: diagnostics stay put.
    doc.buffer.push_str(" more");
    doc.on_edit();
    assert_eq!(
        doc.diagnostics, before,
        "edits must not clear last-good diagnostics before a fresh parse lands"
    );
}

#[test]
fn stale_results_are_discarded() {
    // A result for an older generation must never overwrite current state.
    let mut doc = EditorDocument::new_untitled(1);
    doc.buffer = "(a: 1)".to_string();
    doc.on_edit(); // generation 1
    doc.on_edit(); // generation 2 (simulates a later edit overtaking the request)
    assert_eq!(doc.edit_generation(), 2);

    let worker = ReparseWorker::new();
    // Request only the *stale* generation-1 text directly (tagged with this
    // document's id so it routes back here). The doc's current generation is 2, so
    // whatever the worker delivers for gen 1 must be discarded by `poll_parse` — it
    // can never install.
    worker.request(doc.id(), 1, "(a: 1)".to_string(), None);

    // Poll across a bounded window; a stale result must never install, regardless
    // of when (or whether) the worker has delivered it yet.
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        assert!(
            !doc.poll_parse(&worker),
            "a stale (older-generation) result must not install"
        );
        std::thread::yield_now();
    }
    assert!(
        doc.parse.is_none(),
        "no parse should be installed from a stale result"
    );
    assert!(
        doc.diagnostics.is_empty(),
        "stale discard must leave diagnostics untouched"
    );
}

#[test]
fn highlight_model_derives_classes_from_cst() {
    // FR-019: spans come from the engine's CST token kinds, not a second lexer.
    let parse = ParseResult::parse("Foo(x: \"hi\", y: 42, z: true)", 7);
    let model = build_highlight_model(&parse, 7);
    assert_eq!(model.generation, Some(7));
    assert!(!model.spans.is_empty(), "highlight spans must be produced");

    let classes: std::collections::BTreeSet<&str> =
        model.spans.iter().map(|s| s.class.as_str()).collect();
    // We expect at least an identifier, a string, a number, and a boolean class.
    assert!(classes.contains("ident"), "expected an ident span");
    assert!(classes.contains("string"), "expected a string span");
    assert!(classes.contains("number"), "expected a number span");
    assert!(classes.contains("boolean"), "expected a boolean span");

    // Spans must be in-bounds character offsets, monotonic and non-degenerate.
    let char_count = "Foo(x: \"hi\", y: 42, z: true)".chars().count();
    for span in &model.spans {
        assert!(span.start < span.end, "span must be non-empty");
        assert!(span.end <= char_count, "span end within buffer");
    }
}

#[test]
fn highlight_class_maps_known_kinds() {
    use ronin_core::SyntaxKind;
    assert_eq!(
        HighlightClass::from_kind(SyntaxKind::String),
        HighlightClass::StringLit
    );
    assert_eq!(
        HighlightClass::from_kind(SyntaxKind::Integer),
        HighlightClass::Number
    );
    assert_eq!(
        HighlightClass::from_kind(SyntaxKind::Whitespace),
        HighlightClass::Default
    );
    // Round-trip through the stable name used on a HighlightSpan.
    assert_eq!(
        HighlightClass::from_str_name(HighlightClass::Boolean.as_str()),
        HighlightClass::Boolean
    );
}

#[test]
fn oversize_buffer_shows_degrade_indicator_and_stays_editable() {
    // FR-017: force oversize with a tiny threshold; highlighting/squiggles are
    // suppressed but the buffer is still editable and the degrade label shows.
    let mut doc = EditorDocument::new_untitled(1);
    doc.buffer = "Foo(x: 1, y: 2)".to_string();
    let tiny_threshold: u64 = 4;
    assert!(
        doc.oversize(tiny_threshold),
        "buffer must exceed the tiny threshold to be oversize"
    );

    let oversize = doc.oversize(tiny_threshold);
    let mut harness = Harness::new_ui(move |ui| {
        let _ = editor_view(ui, &mut doc, oversize);
    });
    harness.run();

    assert!(
        harness.query_by_label_contains("Large file").is_some(),
        "oversize editor must show the non-blocking degrade indicator"
    );
    // The buffer content is still rendered (editable surface present): the
    // TextEdit exposes its text as the AccessKit node value.
    assert!(
        node_value_contains(&harness, "Foo"),
        "oversize file must remain visible/editable"
    );
}

#[test]
fn under_threshold_buffer_has_no_degrade_indicator() {
    let mut doc = EditorDocument::new_untitled(1);
    doc.buffer = "Foo(x: 1)".to_string();
    let big_threshold: u64 = 1_000_000;
    assert!(!doc.oversize(big_threshold));

    let oversize = doc.oversize(big_threshold);
    let mut harness = Harness::new_ui(move |ui| {
        let _ = editor_view(ui, &mut doc, oversize);
    });
    harness.run();

    assert!(
        harness.query_by_label_contains("Large file").is_none(),
        "a small file must not show the degrade indicator"
    );
}
