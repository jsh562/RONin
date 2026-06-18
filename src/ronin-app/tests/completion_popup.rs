//! Completion-popup logic smoke tests (E005 Wave 3, US2, T031, SC-003).
//!
//! Drives the popup's headless decision logic — [`CompletionState`] plus the
//! CST-verified insertion — exactly as `editor_view::completion_popup` does each
//! frame, but without a live UI. Per the E003 test boundary, full popup *rendering*
//! and live keystroke routing are manual/QC; every buffer-mutating decision is
//! exercised here:
//!
//! * an in-struct / mid-identifier prefix yields a suggestion list, with nothing
//!   preselected (the literal stands until an explicit highlight);
//! * highlight + accept inserts valid, round-trippable text and reports the caret
//!   at the end of the insertion (FR-013/FR-022);
//! * `Esc`/dismiss keeps the literal (no buffer mutation);
//! * a no-candidate (empty/ambiguous) context shows no list (FR-014).

use ronin_app::completion::{accept_item, CompletionState, Trigger};
use ronin_app::document::EditorDocument;

use ronin_core::{parse, CompletionItem, CompletionKind};

// ---- in-context prefix yields a never-preselected suggestion list -----------

#[test]
fn in_struct_prefix_yields_suggestions_none_preselected() {
    let mut state = CompletionState::new();
    // `Rect(width: 1, w)` — caret right after the in-progress `w` (a new field).
    let buffer = "Rect(width: 1, w)";
    let caret = buffer.find("w)").unwrap() + 1; // just after the second `w`
    let open = state.recompute(buffer, caret, Trigger::Auto);
    assert!(open, "a mid-identifier struct-field prefix opens the popup");
    assert!(!state.items().is_empty(), "candidates are offered");
    // The attested sibling field `width` is suggested for the `w` prefix.
    assert!(
        state.items().iter().any(|i| i.label == "width"),
        "attested sibling field name `width` should be offered"
    );
    // Nothing is preselected — the literal `w` stands until the user highlights.
    assert_eq!(
        state.highlighted(),
        None,
        "the popup must never preselect (FR-012)"
    );
}

#[test]
fn value_slot_prefix_offers_some_below_the_literal() {
    let mut state = CompletionState::new();
    // `[So]` — mid-identifier `So` at a list-element value slot.
    let buffer = "[So]";
    let caret = 3; // end of `So`
    assert!(state.recompute(buffer, caret, Trigger::Auto));
    let some = state
        .items()
        .iter()
        .find(|i| i.label == "Some")
        .expect("Some offered at a value slot");
    // Every suggestion is ranked strictly below the literal (rank 0).
    assert!(
        some.rank >= 1,
        "a suggestion never occupies the literal's rank 0"
    );
    assert_eq!(state.highlighted(), None);
}

// ---- highlight + accept inserts valid round-trippable text (FR-013/FR-022) ---

#[test]
fn highlight_then_accept_inserts_round_trippable_text() {
    let mut state = CompletionState::new();
    let buffer = "[So]";
    let caret = 3;
    state.recompute(buffer, caret, Trigger::Auto);
    // Explicitly highlight `Some` via arrow navigation (find then step to it).
    // `highlight_next` from `None` selects index 0; walk to the `Some` row.
    let some_idx = state
        .items()
        .iter()
        .position(|i| i.label == "Some")
        .unwrap();
    for _ in 0..=some_idx {
        state.highlight_next();
    }
    assert_eq!(state.highlighted(), Some(some_idx));

    let accepted = state.accept(buffer, caret).expect("accept splices");
    // `So` is replaced by `Some()` → `[Some()]`.
    assert_eq!(accepted.new_buffer, "[Some()]");
    // The caret lands at the end of the inserted text (offset 1 + len("Some()")).
    assert_eq!(accepted.new_caret_byte, 1 + "Some()".len());
    // The result round-trips through the CST with no diagnostics (Principle I).
    assert!(
        parse(&accepted.new_buffer).diagnostics().is_empty(),
        "an accepted suggestion must round-trip cleanly"
    );
}

#[test]
fn accept_without_highlight_does_not_insert() {
    let mut state = CompletionState::new();
    let buffer = "[So]";
    state.recompute(buffer, 3, Trigger::Auto);
    // Popup open, nothing highlighted: Enter/Tab would insert the literal, so the
    // accept path yields nothing (the host lets the TextEdit keep the literal).
    assert!(
        state.accept(buffer, 3).is_none(),
        "no-highlight accept must not insert a suggestion (FR-012)"
    );
}

// ---- accept on a document mutates the buffer + caret as editor_view would ----

#[test]
fn accept_on_document_mutates_buffer_and_caret() {
    let mut doc = untitled_with("[So]");
    let caret = 3;
    doc.completion.recompute(&doc.buffer, caret, Trigger::Auto);
    let some_idx = doc
        .completion
        .items()
        .iter()
        .position(|i| i.label == "Some")
        .unwrap();
    for _ in 0..=some_idx {
        doc.completion.highlight_next();
    }
    // Mirror the editor_view accept flow.
    let accepted = doc
        .completion
        .accept(&doc.buffer, caret)
        .expect("accept splices");
    doc.buffer = accepted.new_buffer;
    doc.completion.dismiss();
    doc.on_edit();

    assert_eq!(doc.buffer, "[Some()]");
    assert!(!doc.completion.is_open(), "popup closes after acceptance");
    assert!(
        doc.dirty(),
        "the accepted insertion marks the document dirty"
    );
}

// ---- Esc / dismiss keeps the literal (no mutation) --------------------------

#[test]
fn dismiss_keeps_the_literal_unchanged() {
    let mut doc = untitled_with("[So]");
    doc.completion.recompute(&doc.buffer, 3, Trigger::Auto);
    assert!(doc.completion.is_open());
    let before = doc.buffer.clone();
    // Esc path: dismiss without accepting.
    doc.completion.dismiss();
    assert!(!doc.completion.is_open());
    assert_eq!(doc.buffer, before, "dismiss must never mutate the buffer");
}

// ---- no-candidate context shows no list (FR-014) ----------------------------

#[test]
fn empty_buffer_shows_no_list() {
    let mut state = CompletionState::new();
    assert!(!state.recompute("", 0, Trigger::Manual));
    assert!(!state.is_open());
    assert!(state.items().is_empty());
}

#[test]
fn whitespace_only_context_shows_no_list() {
    let mut state = CompletionState::new();
    assert!(!state.recompute("   \n  ", 3, Trigger::Manual));
    assert!(!state.is_open());
    assert!(state.items().is_empty());
}

#[test]
fn empty_prefix_auto_trigger_shows_no_list() {
    // A fresh value slot (empty prefix) does not auto-open; only manual invoke does.
    let mut state = CompletionState::new();
    assert!(!state.recompute("[]", 1, Trigger::Auto));
    assert!(!state.is_open());
    // Manual invoke at the same slot opens it.
    assert!(state.recompute("[]", 1, Trigger::Manual));
    assert!(state.is_open());
}

// ---- verify-before-commit refuses a corrupting splice (Principle I) ---------

#[test]
fn corrupting_splice_is_refused() {
    // A hand-built item whose insert_text would add a new parse error must be
    // refused, leaving the buffer to the caller (no corruption).
    let bad = CompletionItem {
        label: "Some".to_string(),
        insert_text: "Some(".to_string(),
        kind: CompletionKind::Option,
        rank: 1,
    };
    assert!(
        accept_item("[1]", 3, "", &bad).is_none(),
        "a splice that introduces a new parse error must be refused"
    );
}

// ---- helpers ----------------------------------------------------------------

/// A fresh untitled document seeded with `text` (mirrors the app's new-tab path).
fn untitled_with(text: &str) -> EditorDocument {
    // Build via the public constructor, then seed the buffer + re-baseline so the
    // document starts clean (dirty only reflects the completion edit under test).
    let mut doc = EditorDocument::new_untitled(1);
    doc.buffer = text.to_string();
    doc.mark_saved();
    doc
}
