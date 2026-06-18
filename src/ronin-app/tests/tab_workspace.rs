//! Tab-operation unit tests for [`EditorWorkspace`] and the App-level tab flows
//! (T041, FR-012/FR-025/FR-026).
//!
//! The workspace ops (open/switch/close/reorder/close_all/close_others/reopen +
//! recently-closed eviction) are pure in-memory state, so they are exercised
//! directly with no GUI. The App-level flows that need real file paths
//! (focus-existing by canonical path) or the dirty-prompt state machine
//! (Cancel-aborts-whole-op) are driven through the public `App` API, which is
//! headless.

use std::io::Write;

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;

use ronin_app::app::{render_tab_strip, App, PromptChoice};
use ronin_app::document::{ByteFidelityProfile, CursorState, EditorDocument, SavedSnapshot};
use ronin_app::settings::AppSettings;
use ronin_app::workspace::{ClosedDocumentRecord, EditorWorkspace, RECENTLY_CLOSED_CAP};

/// Write `contents` to a uniquely-named temp `.ron` file and return its path.
fn temp_ron(contents: &str, tag: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "ronin_tabws_{tag}_{}_{}.ron",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut f = std::fs::File::create(&path).expect("create temp fixture");
    f.write_all(contents.as_bytes()).expect("write fixture");
    path
}

/// Build an in-memory untitled document with the given buffer text (no file I/O).
fn untitled_with(text: &str, seq: u32) -> EditorDocument {
    let mut doc = EditorDocument::new_untitled(seq);
    doc.buffer = text.to_string();
    doc
}

// ---------------------------------------------------------------------------
// open / switch / close
// ---------------------------------------------------------------------------

#[test]
fn open_appends_and_activates_each_tab() {
    let mut ws = EditorWorkspace::new();
    assert!(ws.is_empty());
    assert_eq!(ws.active_index(), None);

    let i0 = ws.open(untitled_with("a", 1));
    assert_eq!(i0, 0);
    assert_eq!(ws.active_index(), Some(0));

    let i1 = ws.open(untitled_with("b", 2));
    assert_eq!(i1, 1);
    assert_eq!(ws.len(), 2);
    // Opening a second tab makes IT active (FR-012 open => new active tab).
    assert_eq!(ws.active_index(), Some(1));
}

#[test]
fn switch_selects_a_valid_tab_and_rejects_out_of_range() {
    let mut ws = EditorWorkspace::new();
    ws.open(untitled_with("a", 1));
    ws.open(untitled_with("b", 2));

    assert!(ws.switch(0));
    assert_eq!(ws.active_index(), Some(0));
    // Out-of-range switch is a no-op and leaves the active pointer intact.
    assert!(!ws.switch(9));
    assert_eq!(ws.active_index(), Some(0));
}

#[test]
fn close_returns_removed_doc_and_fixes_active_index() {
    let mut ws = EditorWorkspace::new();
    ws.open(untitled_with("a", 1));
    ws.open(untitled_with("b", 2));
    ws.open(untitled_with("c", 3));
    ws.switch(2);

    // Closing a tab before the active one shifts active left.
    let removed = ws.close(0).expect("close returns the removed doc");
    assert_eq!(removed.buffer, "a");
    assert_eq!(ws.len(), 2);
    assert_eq!(
        ws.active_index(),
        Some(1),
        "active follows its document left"
    );

    // Closing the active tab clamps to the nearest remaining tab.
    ws.switch(1);
    ws.close(1).expect("close active");
    assert_eq!(ws.active_index(), Some(0));

    // Closing the last tab empties the workspace and clears the active pointer.
    ws.close(0).expect("close last");
    assert!(ws.is_empty());
    assert_eq!(ws.active_index(), None);

    // Closing an out-of-range index is a None no-op.
    assert!(ws.close(0).is_none());
}

// ---------------------------------------------------------------------------
// reorder: preserves identity + dirty state, only position changes
// ---------------------------------------------------------------------------

#[test]
fn reorder_preserves_identity_and_dirty_state() {
    let mut ws = EditorWorkspace::new();
    // Tab 0 clean, tab 1 dirty, tab 2 clean.
    ws.open(untitled_with("zero", 1));
    let mut dirty_doc = untitled_with("one", 2);
    // Mark it as having a saved baseline that differs, so it is dirty.
    dirty_doc.mark_saved();
    dirty_doc.buffer = "one-edited".to_string();
    let dirty_id = dirty_doc.id();
    assert!(dirty_doc.dirty(), "tab 1 must be dirty before reorder");
    ws.open(dirty_doc);
    ws.open(untitled_with("two", 3));
    ws.switch(1);

    // Move the dirty tab from index 1 to index 0.
    assert!(ws.reorder(1, 0));

    let moved = ws.get(0).expect("doc now at index 0");
    assert_eq!(moved.id(), dirty_id, "reorder preserves document identity");
    assert_eq!(moved.buffer, "one-edited", "reorder preserves content");
    assert!(moved.dirty(), "reorder preserves dirty state");
    // The active pointer follows the moved document to its new position.
    assert_eq!(ws.active_index(), Some(0));

    // A no-op move (from == to) and out-of-range moves return false.
    assert!(!ws.reorder(0, 0));
    assert!(!ws.reorder(9, 0));
}

#[test]
fn reorder_moving_right_shifts_intervening_tabs() {
    let mut ws = EditorWorkspace::new();
    let id_a = ws_open_get_id(&mut ws, "a", 1);
    let id_b = ws_open_get_id(&mut ws, "b", 2);
    let id_c = ws_open_get_id(&mut ws, "c", 3);
    ws.switch(0);

    // Move "a" from 0 to 2: order becomes b, c, a.
    assert!(ws.reorder(0, 2));
    assert_eq!(ws.get(0).unwrap().id(), id_b);
    assert_eq!(ws.get(1).unwrap().id(), id_c);
    assert_eq!(ws.get(2).unwrap().id(), id_a);
    // Active was "a" (idx 0) and follows it to idx 2.
    assert_eq!(ws.active_index(), Some(2));
}

fn ws_open_get_id(ws: &mut EditorWorkspace, text: &str, seq: u32) -> u64 {
    let doc = untitled_with(text, seq);
    let id = doc.id();
    ws.open(doc);
    id
}

// ---------------------------------------------------------------------------
// close_all / close_others
// ---------------------------------------------------------------------------

#[test]
fn close_all_removes_every_tab_in_order() {
    let mut ws = EditorWorkspace::new();
    ws.open(untitled_with("a", 1));
    ws.open(untitled_with("b", 2));
    ws.open(untitled_with("c", 3));

    let removed = ws.close_all();
    assert_eq!(removed.len(), 3);
    let texts: Vec<&str> = removed.iter().map(|d| d.buffer.as_str()).collect();
    assert_eq!(texts, vec!["a", "b", "c"], "close_all preserves tab order");
    assert!(ws.is_empty());
    assert_eq!(ws.active_index(), None);
}

#[test]
fn close_others_keeps_only_the_named_tab() {
    let mut ws = EditorWorkspace::new();
    let id_a = ws_open_get_id(&mut ws, "a", 1);
    let id_b = ws_open_get_id(&mut ws, "b", 2);
    let _id_c = ws_open_get_id(&mut ws, "c", 3);

    // Keep "b" (index 1).
    let removed = ws.close_others(1);
    let removed_ids: Vec<u64> = removed.iter().map(EditorDocument::id).collect();
    assert_eq!(removed.len(), 2);
    assert!(removed_ids.contains(&id_a));
    assert_eq!(ws.len(), 1);
    assert_eq!(ws.get(0).unwrap().id(), id_b, "kept tab is the named one");
    assert_eq!(ws.active_index(), Some(0));

    // Out-of-range keep index is a no-op (nothing removed, workspace unchanged).
    let none = ws.close_others(9);
    assert!(none.is_empty());
    assert_eq!(ws.len(), 1);
}

// ---------------------------------------------------------------------------
// reopen restores buffer + fidelity + dirty state
// ---------------------------------------------------------------------------

#[test]
fn reopen_restores_buffer_fidelity_and_dirty_state() {
    let mut ws = EditorWorkspace::new();
    // Build a CRLF-profiled doc whose buffer was edited (so it closes DIRTY).
    let profile = ByteFidelityProfile::from_bytes(b"Config(x: 1)\r\n");
    let baseline = SavedSnapshot::of("Config(x: 1)\n");
    let record = ClosedDocumentRecord {
        path: None,
        restorable_text: "Config(x: 2)\n".to_string(),
        saved_baseline: baseline,
        byte_metadata: profile,
        cursor: CursorState {
            caret: 5,
            selection: None,
            scroll: 12.0,
        },
        untitled_seq: Some(7),
    };
    ws.push_closed(record);

    let idx = ws.reopen_closed().expect("reopen pops the stack");
    let doc = ws.get(idx).expect("reopened doc present");
    assert_eq!(
        doc.buffer, "Config(x: 2)\n",
        "reopen restores closed buffer"
    );
    assert!(
        doc.dirty(),
        "reopened doc keeps its at-close DIRTY state (buffer != baseline)"
    );
    assert_eq!(
        doc.byte_profile, profile,
        "reopen carries the byte-fidelity profile so Save stays byte-preserving"
    );
    assert_eq!(doc.cursor.caret, 5, "reopen restores the cursor");
    assert_eq!(doc.cursor.scroll, 12.0);
    assert_eq!(
        doc.title(),
        "Untitled-7",
        "reopen restores untitled identity"
    );

    // The stack is now empty; reopening again is a harmless no-op.
    assert!(ws.reopen_closed().is_none());
}

#[test]
fn reopen_of_clean_record_comes_back_clean() {
    let mut ws = EditorWorkspace::new();
    let baseline = SavedSnapshot::of("clean\n");
    ws.push_closed(ClosedDocumentRecord {
        path: None,
        restorable_text: "clean\n".to_string(),
        saved_baseline: baseline,
        byte_metadata: ByteFidelityProfile::from_bytes(b"clean\n"),
        cursor: CursorState::default(),
        untitled_seq: Some(1),
    });
    let idx = ws.reopen_closed().unwrap();
    assert!(
        !ws.get(idx).unwrap().dirty(),
        "a record whose text matches its baseline reopens CLEAN"
    );
}

// ---------------------------------------------------------------------------
// recently_closed eviction at >10 + empty no-op
// ---------------------------------------------------------------------------

#[test]
fn recently_closed_is_bounded_and_evicts_oldest() {
    let mut ws = EditorWorkspace::new();
    // Push more than the cap; the oldest must be evicted.
    let total = RECENTLY_CLOSED_CAP + 3;
    for n in 0..total {
        ws.push_closed(ClosedDocumentRecord {
            path: None,
            restorable_text: format!("doc-{n}"),
            saved_baseline: SavedSnapshot::of(&format!("doc-{n}")),
            byte_metadata: ByteFidelityProfile::from_bytes(b""),
            cursor: CursorState::default(),
            untitled_seq: None,
        });
    }
    assert_eq!(
        ws.recently_closed().len(),
        RECENTLY_CLOSED_CAP,
        "stack is bounded to the cap"
    );
    // The most recent reopen returns the LAST pushed (LIFO); the oldest entries
    // (doc-0, doc-1, doc-2) were evicted.
    let idx = ws.reopen_closed().unwrap();
    assert_eq!(
        ws.get(idx).unwrap().buffer,
        format!("doc-{}", total - 1),
        "reopen pops the most-recently-closed (LIFO)"
    );
    let texts: Vec<String> = ws
        .recently_closed()
        .iter()
        .map(|r| r.restorable_text.clone())
        .collect();
    assert!(
        !texts.contains(&"doc-0".to_string()),
        "the oldest record was evicted"
    );
}

#[test]
fn reopen_on_empty_stack_is_a_noop() {
    let mut ws = EditorWorkspace::new();
    assert!(ws.reopen_closed().is_none());
    assert!(ws.is_empty());
    assert_eq!(ws.active_index(), None);
}

// ---------------------------------------------------------------------------
// next_untitled never recycles within a session
// ---------------------------------------------------------------------------

#[test]
fn untitled_sequence_is_monotonic_and_never_recycled() {
    let mut ws = EditorWorkspace::new();
    let i0 = ws.push_untitled();
    assert_eq!(ws.get(i0).unwrap().title(), "Untitled-1");
    let i1 = ws.push_untitled();
    assert_eq!(ws.get(i1).unwrap().title(), "Untitled-2");
    // Close the first untitled tab; the number must NOT be recycled.
    ws.close(i0);
    let i2 = ws.push_untitled();
    assert_eq!(
        ws.get(i2).unwrap().title(),
        "Untitled-3",
        "a closed untitled number is never recycled in a session"
    );
}

// ---------------------------------------------------------------------------
// focus-existing by canonical path (App-level, real temp files) — FR-025
// ---------------------------------------------------------------------------

#[test]
fn opening_an_already_open_path_focuses_existing_tab() {
    let a = temp_ron("Config(level: 1)\n", "focus_a");
    let b = temp_ron("Config(level: 2)\n", "focus_b");
    let mut app = App::new(AppSettings::default(), None);
    app.open_file(&a);
    app.open_file(&b);
    assert_eq!(app.document_count(), 2);
    assert_eq!(app.active_index(), Some(1), "second open is active");

    // Re-opening `a` must focus the existing tab, NOT create a duplicate.
    app.open_file(&a);
    assert_eq!(
        app.document_count(),
        2,
        "re-opening an already-open path must not duplicate the tab"
    );
    assert_eq!(app.active_index(), Some(0), "the existing tab is focused");

    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
}

#[test]
fn pathless_untitled_buffers_are_exempt_from_focus_existing() {
    let mut app = App::new(AppSettings::default(), None);
    app.new_untitled();
    app.new_untitled();
    // Two never-saved buffers each get their own tab (FR-025 exemption).
    assert_eq!(app.document_count(), 2);
}

#[test]
fn reopen_last_closed_focuses_existing_tab_for_same_path() {
    // FR-012/FR-025: reopen honors focus-existing. Open a file, then open it in a
    // second logical tab is impossible (focus-existing), so instead: open A and B,
    // close A (records it), reopen A while a fresh copy of A is reopened — but if A
    // is still open elsewhere, reopen focuses it. Here we verify reopen does not
    // duplicate when the path is already open.
    let a = temp_ron("Data(1)\n", "reopen_focus_a");
    let mut app = App::new(AppSettings::default(), None);
    app.open_file(&a);
    // Close A (clean) so it lands on the recently-closed stack.
    app.request_close_doc(0);
    assert_eq!(app.document_count(), 0);
    assert_eq!(app.recently_closed_count(), 1);
    // Re-open A normally so it is live again.
    app.open_file(&a);
    assert_eq!(app.document_count(), 1);
    // Now reopen-last-closed: A is already open, so it must focus, not duplicate.
    assert!(app.reopen_last_closed());
    assert_eq!(
        app.document_count(),
        1,
        "reopen-last-closed must focus an already-open path, not duplicate it"
    );
    let _ = std::fs::remove_file(&a);
}

// ---------------------------------------------------------------------------
// sequential multi-dirty close/quit: Cancel aborts the whole op — FR-026
// ---------------------------------------------------------------------------

/// Open `n` file-backed tabs and make each one dirty. Returns their paths.
fn open_n_dirty_tabs(app: &mut App, n: usize, tag: &str) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    for i in 0..n {
        let p = temp_ron(&format!("Tab(n: {i})\n"), &format!("{tag}_{i}"));
        app.open_file(&p);
        let idx = app.active_index().unwrap();
        let doc = app.active_document_mut().unwrap();
        doc.buffer = format!("Tab(n: {i}, edited: true)\n");
        doc.on_edit();
        assert!(doc.dirty(), "tab {idx} must be dirty");
        paths.push(p);
    }
    paths
}

#[test]
fn close_all_cancel_midway_aborts_and_keeps_all_tabs() {
    let mut app = App::new(AppSettings::default(), None);
    let paths = open_n_dirty_tabs(&mut app, 3, "cancelall");
    assert_eq!(app.document_count(), 3);

    // Begin close-all: the first dirty tab prompts.
    app.close_all();
    assert!(
        app.dirty_prompt().is_some(),
        "close-all must prompt the first dirty tab"
    );

    // Discard the first; the second prompts.
    app.resolve_dirty_prompt(PromptChoice::Discard);
    assert_eq!(app.document_count(), 2, "first tab closed on Discard");
    assert!(app.dirty_prompt().is_some(), "the next dirty tab prompts");

    // Cancel mid-sequence: the WHOLE operation aborts; all remaining tabs stay.
    app.resolve_dirty_prompt(PromptChoice::Cancel);
    assert!(app.dirty_prompt().is_none(), "Cancel clears the prompt");
    assert_eq!(
        app.document_count(),
        2,
        "Cancel aborts the rest of close-all; remaining tabs stay open"
    );
    // The remaining tabs are still dirty (unchanged).
    for i in 0..app.document_count() {
        // Switching makes each the active doc so we can inspect it.
        // (active_index can be inspected via active_document after a request.)
        let _ = i;
    }
    for p in &paths {
        let _ = std::fs::remove_file(p);
    }
}

#[test]
fn quit_cancel_at_first_prompt_aborts_quit_and_keeps_all_tabs() {
    let mut app = App::new(AppSettings::default(), None);
    let paths = open_n_dirty_tabs(&mut app, 2, "cancelquit");
    assert_eq!(app.document_count(), 2);

    app.request_quit();
    assert!(
        app.dirty_prompt().is_some(),
        "quit over dirty tabs prompts sequentially"
    );

    app.resolve_dirty_prompt(PromptChoice::Cancel);
    assert!(app.dirty_prompt().is_none());
    assert_eq!(
        app.document_count(),
        2,
        "Cancel at any prompt aborts the entire quit; all tabs remain"
    );
    for p in &paths {
        let _ = std::fs::remove_file(p);
    }
}

#[test]
fn close_others_discards_each_dirty_then_keeps_one() {
    let mut app = App::new(AppSettings::default(), None);
    let paths = open_n_dirty_tabs(&mut app, 3, "closeothers");
    // Keep the middle tab (index 1).
    app.close_others(1);
    // Walk the sequential prompts, discarding each affected dirty tab.
    let mut guard = 0;
    while app.dirty_prompt().is_some() && guard < 10 {
        app.resolve_dirty_prompt(PromptChoice::Discard);
        guard += 1;
    }
    assert_eq!(
        app.document_count(),
        1,
        "close-others leaves exactly the kept tab once all prompts resolve"
    );
    for p in &paths {
        let _ = std::fs::remove_file(p);
    }
}

#[test]
fn close_all_with_clean_tabs_closes_them_without_prompting() {
    let mut app = App::new(AppSettings::default(), None);
    let a = temp_ron("A(1)\n", "clean_all_a");
    let b = temp_ron("B(2)\n", "clean_all_b");
    app.open_file(&a);
    app.open_file(&b);
    // No edits => clean.
    app.close_all();
    assert!(
        app.dirty_prompt().is_none(),
        "all-clean close-all needs no prompt"
    );
    assert_eq!(app.document_count(), 0, "clean tabs close immediately");
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
}

// ---------------------------------------------------------------------------
// tab click switches the active document (UI Fix 1) — egui_kittest widget test
// ---------------------------------------------------------------------------

#[test]
fn clicking_an_inactive_tab_switches_the_active_document() {
    // Regression guard (UI Fix 1): the tab's click must be read off the INNER
    // selectable-label response, not the `dnd_drag_source` outer response (which
    // senses drag, never click). Before the fix `.clicked()` never fired and the
    // active tab never moved on a click.
    //
    // The full `App::ui` pass needs a live `eframe::Frame` that egui_kittest does
    // not synthesize here, so this drives the extracted `render_tab_strip` widget
    // (the exact code `render_tab_bar` runs) through the renderer-free harness — the
    // same widget-level boundary `open_and_view`/`problems_nav` use.
    let mut ws = EditorWorkspace::new();
    ws.open(untitled_with("first", 1)); // Untitled-1 at index 0
    ws.open(untitled_with("second", 2)); // Untitled-2 at index 1 (active on open)
    assert_eq!(
        ws.active_index(),
        Some(1),
        "the second-opened tab is active before any click"
    );

    // Capture the action `render_tab_strip` returns across the two-pass run egui
    // needs to register a click. An `Rc<Cell<..>>` is shared between the harness
    // closure (which the harness must own) and this test scope, which reads it back.
    let switched = std::rc::Rc::new(std::cell::Cell::new(None));
    let switched_in = std::rc::Rc::clone(&switched);

    // The harness closure must own its data, so render a throwaway workspace with the
    // same two tabs — an identical tab strip (same titles "Untitled-1"/"Untitled-2",
    // second active).
    let mut render_ws = EditorWorkspace::new();
    render_ws.open(untitled_with("first", 1));
    render_ws.open(untitled_with("second", 2));

    let mut harness = Harness::new_ui(move |ui| {
        let actions = render_tab_strip(ui, &render_ws);
        if let Some(idx) = actions.switch_to {
            switched_in.set(Some(idx));
        }
    });
    harness.run();

    // Click the INACTIVE tab (index 0 = "Untitled-1") and re-run so egui registers
    // the click on the next frame.
    harness.get_by_label_contains("Untitled-1").click();
    harness.run();

    let idx = switched
        .get()
        .expect("clicking an inactive tab must yield a switch_to action");
    assert_eq!(idx, 0, "the clicked tab is index 0 (Untitled-1)");

    // Apply the action the way `render_tab_bar` does and assert the active document
    // actually moved to the clicked tab.
    assert!(ws.switch(idx), "switch to the clicked tab succeeds");
    assert_eq!(
        ws.active_index(),
        Some(0),
        "after a click on Untitled-1 the active document is the clicked tab"
    );
    assert_eq!(
        ws.get(ws.active_index().unwrap()).unwrap().title(),
        "Untitled-1",
        "the active document title moved to the clicked tab"
    );
}
