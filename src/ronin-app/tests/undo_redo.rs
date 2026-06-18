//! Editor-level undo/redo tests (E007 / OBJ3).
//!
//! The `EditorDocument`/`App` half of tasks T029 (undo/redo round-trip + redo
//! invalidation, SC-005), T030 (bounds + coalescing, SC-006/SC-009/SC-010), and
//! T038 (`[COMPLETES TR-010]` — the shell-command-driven undo/redo). The pure
//! `UndoStack` properties live in `ronin-core/tests/undo.rs`; here we drive undo/redo
//! through the real `EditorDocument` seam and the `App` command surface.
//!
//! # The editor-level undo guarantees pinned here
//!
//! * **Exact prior in-memory bytes + cursor (TR-010/TR-018, SC-005).** Undo through
//!   `EditorDocument` restores the exact prior buffer bytes and the cursor; redo
//!   replays them. The restore is byte-faithful (no reflow / normalization).
//! * **In-memory only — never the file (TR-018).** Undo/redo operate solely on the
//!   in-memory buffer/CST/cursor and restore the exact prior in-memory bytes even
//!   when the on-disk file changed after open; they never read or write the file.
//! * **Redo invalidation (TR-012, SC-005).** A new edit after an undo makes redo
//!   unavailable.
//! * **Coalescing (TR-027, SC-010).** A rapid keystroke run within the coalesce
//!   window is a single undo unit; an edit after the window starts a new unit. The
//!   timing decision is driven deterministically via an injected `Instant`.
//! * **Bounded, file-size-independent memory (TR-011/TR-024, SC-009).** Undo memory
//!   stays within the configured cap (oldest dropped) regardless of file size.
//! * **Shell command (T038).** The `App::undo_active`/`redo_active` commands drive
//!   the active document's history.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ronin_app::app::App;
use ronin_app::document::{CursorState, EditorDocument};
use ronin_app::settings::AppSettings;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A unique temp directory for one test; created on disk.
fn unique_temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ronin_undo_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// A loaded document over `text` (seeds the undo baseline at the loaded state).
fn doc_with(text: &str) -> EditorDocument {
    EditorDocument::from_loaded("sample.ron", text.as_bytes()).expect("valid utf-8")
}

/// Apply an edit to `doc` (set buffer + bump generation) and record an undo
/// snapshot at `now`.
fn edit_at(doc: &mut EditorDocument, buffer: &str, cursor: usize, now: Instant) {
    doc.buffer = buffer.to_string();
    doc.cursor = CursorState {
        caret: cursor,
        selection: None,
        scroll: 0.0,
    };
    doc.on_edit();
    doc.record_undo_snapshot(now);
}

// ---------------------------------------------------------------------------
// T029 — exact prior bytes + cursor; redo replays (SC-005)
// ---------------------------------------------------------------------------

#[test]
fn undo_restores_exact_prior_bytes_and_cursor() {
    let base = Instant::now();
    // Non-coalescing gaps: each edit is its own undo unit (far apart in time).
    let gap = Duration::from_secs(10);
    let mut doc = doc_with("(a: 1)\n");
    edit_at(&mut doc, "(a: 12)\n", 5, base + gap);
    edit_at(&mut doc, "(a: 123)\n", 6, base + gap * 2);

    assert_eq!(doc.buffer, "(a: 123)\n");

    // Undo step 1: back to "(a: 12)\n" with its cursor.
    assert!(doc.undo(base + gap * 3));
    assert_eq!(doc.buffer, "(a: 12)\n", "exact prior bytes restored");
    assert_eq!(doc.cursor.caret, 5, "cursor restored");

    // Undo step 2: back to the loaded baseline.
    assert!(doc.undo(base + gap * 4));
    assert_eq!(doc.buffer, "(a: 1)\n");
    assert!(!doc.can_undo());

    // Redo replays them exactly.
    assert!(doc.redo());
    assert_eq!(doc.buffer, "(a: 12)\n");
    assert_eq!(doc.cursor.caret, 5);
    assert!(doc.redo());
    assert_eq!(doc.buffer, "(a: 123)\n");
    assert_eq!(doc.cursor.caret, 6);
    assert!(!doc.can_redo());
}

#[test]
fn restore_is_byte_faithful_no_reflow() {
    let base = Instant::now();
    let gap = Duration::from_secs(10);
    // A messy-but-valid intermediate state must come back byte-for-byte.
    let messy = "(  a:1 ,\n // comment\n  b:2,)\n";
    let mut doc = doc_with("()\n");
    edit_at(&mut doc, messy, 0, base + gap);
    assert!(doc.undo(base + gap * 2));
    assert_eq!(doc.buffer, "()\n");
    assert!(doc.redo());
    assert_eq!(doc.buffer, messy, "no reflow / normalization on restore");
}

#[test]
fn edit_generation_bumps_on_undo_so_reparse_reruns() {
    let base = Instant::now();
    let gap = Duration::from_secs(10);
    let mut doc = doc_with("(a: 1)\n");
    edit_at(&mut doc, "(a: 2)\n", 0, base + gap);
    let before = doc.edit_generation();
    assert!(doc.undo(base + gap * 2));
    assert!(
        doc.edit_generation() > before,
        "undo bumps edit_generation so the off-frame reparse re-runs"
    );
}

// ---------------------------------------------------------------------------
// T029 — redo invalidation on a new edit (TR-012, SC-005)
// ---------------------------------------------------------------------------

#[test]
fn new_edit_after_undo_invalidates_redo() {
    let base = Instant::now();
    let gap = Duration::from_secs(10);
    let mut doc = doc_with("(a: 1)\n");
    edit_at(&mut doc, "(a: 2)\n", 0, base + gap);
    edit_at(&mut doc, "(a: 3)\n", 0, base + gap * 2);

    assert!(doc.undo(base + gap * 3));
    assert!(doc.can_redo(), "redo available after an undo");

    // A new edit after the undo invalidates redo (TR-012).
    edit_at(&mut doc, "(a: 9)\n", 0, base + gap * 4);
    assert!(!doc.can_redo(), "redo unavailable after a new edit");
}

// ---------------------------------------------------------------------------
// T030 — coalescing: a rapid run is one unit (TR-027, SC-010)
// ---------------------------------------------------------------------------

#[test]
fn rapid_run_within_window_is_one_undo_unit() {
    let base = Instant::now();
    let mut doc = doc_with("");
    // Type "hello" one char at a time, each within the 500 ms coalesce window.
    let step = Duration::from_millis(50);
    edit_at(&mut doc, "h", 1, base + step);
    edit_at(&mut doc, "he", 2, base + step * 2);
    edit_at(&mut doc, "hel", 3, base + step * 3);
    edit_at(&mut doc, "hell", 4, base + step * 4);
    edit_at(&mut doc, "hello", 5, base + step * 5);

    assert_eq!(doc.undo_depth(), 1, "the whole run is a single undo unit");
    // One undo returns to the pre-run (empty) state, not one char back.
    assert!(doc.undo(base + Duration::from_secs(10)));
    assert_eq!(doc.buffer, "");
    assert!(!doc.can_undo());
}

#[test]
fn edit_after_window_starts_new_unit() {
    let base = Instant::now();
    let mut doc = doc_with("");
    let near = Duration::from_millis(50);
    let far = Duration::from_secs(10);
    // Run 1: two coalesced edits.
    edit_at(&mut doc, "a", 1, base + near);
    edit_at(&mut doc, "ab", 2, base + near * 2);
    // A long pause → a new unit.
    edit_at(&mut doc, "ab cd", 5, base + far);

    assert_eq!(
        doc.undo_depth(),
        2,
        "two units: the run and the post-pause edit"
    );
    assert!(doc.undo(base + far * 2));
    assert_eq!(doc.buffer, "ab");
    assert!(doc.undo(base + far * 3));
    assert_eq!(doc.buffer, "");
}

// ---------------------------------------------------------------------------
// T030 / SC-009 — bounded, file-size-independent undo memory
// ---------------------------------------------------------------------------

#[test]
fn undo_memory_bounded_by_count_cap_oldest_dropped() {
    let base = Instant::now();
    let gap = Duration::from_secs(10);
    let mut settings = AppSettings::default();
    settings.undo.set_history_count_cap(3);
    // Drive a fresh document configured with the small cap.
    let mut doc = doc_with("(n: 0)\n");
    doc.set_undo_config(settings.undo);

    for i in 1..=10 {
        edit_at(&mut doc, &format!("(n: {i})\n"), 0, base + gap * i);
    }
    assert!(
        doc.undo_depth() <= 3,
        "count cap binds the retained history"
    );

    // The retained boundaries are the newest ones; oldest were dropped.
    assert!(doc.undo(base + gap * 100));
    assert_eq!(doc.buffer, "(n: 9)\n", "newest prior boundary first");
}

#[test]
fn undo_memory_bounded_independent_of_file_size() {
    // SC-009: with a tight byte cap, even large snapshots keep retained memory
    // within the bound — independent of file size and session length.
    let base = Instant::now();
    let gap = Duration::from_secs(10);
    let mut settings = AppSettings::default();
    // 5 MiB byte cap; high count cap so the SIZE cap is what binds.
    settings.undo.set_history_byte_cap(5 * 1024 * 1024);
    settings.undo.set_history_count_cap(10_000);

    let mut doc = doc_with("seed\n");
    doc.set_undo_config(settings.undo);

    // Each snapshot is ~1 MiB.
    let big = "x".repeat(1024 * 1024);
    for i in 0..20 {
        edit_at(&mut doc, &format!("{big}{i}"), 0, base + gap * i);
    }
    assert!(
        doc.undo_total_bytes() <= 5 * 1024 * 1024 + big.len() + 2,
        "retained undo+redo memory ({}) stays within the byte bound",
        doc.undo_total_bytes()
    );
}

#[test]
fn misconfigured_cap_never_unbounded() {
    // A 0 cap in settings clamps to the minimum on the config and the engine cap
    // reverts a 0 field to default — undo is never unbounded.
    let base = Instant::now();
    let gap = Duration::from_secs(10);
    let mut settings = AppSettings::default();
    settings.undo.set_history_count_cap(0); // clamps to 1
    let mut doc = doc_with("(v: 0)\n");
    doc.set_undo_config(settings.undo);
    for i in 1..=50 {
        edit_at(&mut doc, &format!("(v: {i})\n"), 0, base + gap * i);
    }
    assert!(doc.undo_depth() >= 1, "undo is never disabled");
    assert!(
        doc.undo_depth() <= 1,
        "a count cap of 0 clamps to 1, never unbounded"
    );
}

// ---------------------------------------------------------------------------
// TR-018 — undo is in-memory only, never reads/writes the file
// ---------------------------------------------------------------------------

#[test]
fn undo_is_in_memory_only_ignores_on_disk_change() {
    let dir = unique_temp_dir("inmem");
    let target = dir.join("doc.ron");
    std::fs::write(&target, "(a: 1)\n").expect("write on-disk file");

    let mut app = App::new(AppSettings::default(), None);
    app.open_file(&target);

    let base = Instant::now();
    let gap = Duration::from_secs(10);
    // Edit in memory (one undo unit), far apart so each is its own unit.
    app.edit_active_buffer_at("(a: 2)\n", base + gap);
    app.edit_active_buffer_at("(a: 3)\n", base + gap * 2);

    // Now the file on disk is changed by "another process".
    std::fs::write(&target, "EXTERNALLY CHANGED\n").expect("external write");
    let on_disk_before_undo = std::fs::read_to_string(&target).unwrap();

    // Undo restores the exact prior IN-MEMORY bytes, ignoring the on-disk change.
    assert!(app.undo_active_at(base + gap * 3));
    let doc = app.active_document().expect("active doc");
    assert_eq!(
        doc.buffer, "(a: 2)\n",
        "undo restores in-memory prior bytes (TR-018)"
    );

    // Undo did NOT touch the on-disk file.
    let on_disk_after_undo = std::fs::read_to_string(&target).unwrap();
    assert_eq!(
        on_disk_after_undo, on_disk_before_undo,
        "undo never reads or writes the file (TR-018)"
    );
    assert_eq!(on_disk_after_undo, "EXTERNALLY CHANGED\n");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// T038 — the shell undo/redo command drives the document history
// ---------------------------------------------------------------------------

#[test]
fn shell_command_drives_undo_and_redo() {
    let dir = unique_temp_dir("shellcmd");
    let target = dir.join("doc.ron");
    std::fs::write(&target, "(x: 0)\n").expect("write");

    let mut app = App::new(AppSettings::default(), None);
    app.open_file(&target);

    let base = Instant::now();
    let gap = Duration::from_secs(10);
    app.edit_active_buffer_at("(x: 1)\n", base + gap);
    app.edit_active_buffer_at("(x: 2)\n", base + gap * 2);

    // The shell Undo command (Ctrl+Z) steps back through the history.
    assert!(app.undo_active());
    assert_eq!(app.active_document().unwrap().buffer, "(x: 1)\n");
    assert!(app.undo_active());
    assert_eq!(app.active_document().unwrap().buffer, "(x: 0)\n");
    assert!(!app.active_document().unwrap().can_undo());
    // A further undo is a no-op (nothing left to undo).
    assert!(!app.undo_active());

    // The shell Redo command (Ctrl+Y) replays forward.
    assert!(app.redo_active());
    assert_eq!(app.active_document().unwrap().buffer, "(x: 1)\n");
    assert!(app.redo_active());
    assert_eq!(app.active_document().unwrap().buffer, "(x: 2)\n");
    assert!(!app.redo_active());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn shell_undo_is_a_noop_with_no_open_tab() {
    let mut app = App::new(AppSettings::default(), None);
    assert!(!app.undo_active(), "no tab → undo is a no-op");
    assert!(!app.redo_active(), "no tab → redo is a no-op");
}

// ---------------------------------------------------------------------------
// T037 / SC-008 — undo bookkeeping is off the per-frame path and bounded
// ---------------------------------------------------------------------------

#[test]
fn per_frame_snapshot_is_a_noop_without_a_new_edit() {
    // The structural off-frame signal: a per-frame `record_undo_snapshot` call
    // when the buffer has NOT advanced does no work (no new boundary, no parse).
    let base = Instant::now();
    let mut doc = doc_with("(a: 1)\n");
    edit_at(&mut doc, "(a: 2)\n", 0, base);
    let depth = doc.undo_depth();
    // Many idle frames: no edit since the last snapshot → no work each frame.
    for f in 1..1000 {
        let recorded = doc.record_undo_snapshot(base + Duration::from_millis(f));
        assert!(
            !recorded,
            "an idle frame records nothing (off-frame, bounded)"
        );
    }
    assert_eq!(
        doc.undo_depth(),
        depth,
        "no boundaries added by idle frames"
    );
}

#[test]
fn rapid_run_on_large_buffer_is_one_snapshot_per_window() {
    // SC-008/SC-010: a sustained run of rapid edits on a large buffer produces at
    // most one undo unit per coalesce window — undo bookkeeping does NOT snapshot
    // per keystroke, so the per-frame cost stays bounded on a large file.
    let base = Instant::now();
    let fill = "x".repeat(2 * 1024 * 1024); // ~2 MiB payload
    let large = format!("(data: \"{fill}\")\n");
    let mut doc = doc_with(&large);

    // A sustained run of rapid edits, all within a single 500 ms coalesce window.
    let step = Duration::from_millis(1);
    for i in 0..40u32 {
        let edited = format!("(data: \"{fill}\", n: {i})\n");
        edit_at(&mut doc, &edited, 0, base + step * i);
    }
    // The whole within-window run is a SINGLE undo unit (not 40), so retained
    // memory is one boundary (the pre-run state), not one per keystroke.
    assert_eq!(
        doc.undo_depth(),
        1,
        "a rapid run on a large buffer is one unit"
    );
}

#[test]
fn dirty_state_recomputes_after_undo_back_to_baseline() {
    let base = Instant::now();
    let gap = Duration::from_secs(10);
    let mut doc = doc_with("(a: 1)\n");
    assert!(!doc.dirty(), "freshly loaded document is clean");
    edit_at(&mut doc, "(a: 2)\n", 0, base + gap);
    assert!(doc.dirty(), "an edit makes it dirty");
    // Undo back to the loaded baseline: dirty-tracking recomputes to clean.
    assert!(doc.undo(base + gap * 2));
    assert_eq!(doc.buffer, "(a: 1)\n");
    assert!(
        !doc.dirty(),
        "undo back to the saved baseline is clean again"
    );
}
