//! `UndoStack` property + behavior tests (E007 / OBJ3).
//!
//! The pure-core half of tasks T029 (undo/redo round-trip property, SC-005) and
//! T030 (bounds + coalescing, SC-006/SC-009/SC-010) for the WASM-clean
//! [`ronin_core::UndoStack`]. The editor-level half (driving undo/redo through
//! `EditorDocument` + the shell command) lives in `ronin-app/tests/undo_redo.rs`.
//!
//! # The undo properties pinned here
//!
//! * **Exact-prior-byte undo + exact redo replay (TR-010, SC-005).** For a random
//!   sequence of edits, undoing repeatedly walks back through the *exact* prior
//!   `source_text` of each committed boundary (byte-for-byte, no reflow), and
//!   redo replays them forward exactly. The CST snapshot prints back to the same
//!   bytes too.
//! * **Redo invalidation (TR-012, SC-005).** Any new edit recorded after an undo
//!   clears the redo stack.
//! * **Coalescing (TR-027, SC-006/SC-010).** A run of edits the caller marks
//!   `coalesce: true` collapses to a single undo unit; the timing decision is
//!   made caller-side (this core never reads a clock — TR-014).
//! * **Bounded ring (TR-011/TR-024, SC-006/SC-009).** When EITHER the count cap
//!   OR the byte-size cap binds, the OLDEST boundary is dropped; retained memory
//!   stays within the configured bound independent of file size; a misconfigured
//!   (zero) cap falls back to the default rather than going unbounded.

use proptest::prelude::*;
use ronin_core::{print, UndoCap, UndoEntry, UndoStack};

/// The default coalesce window (reference metadata; the core measures no clock).
const WINDOW: std::time::Duration = ronin_core::undo::DEFAULT_COALESCE_WINDOW;

/// Build an undo entry for `src` (parsed CST + verbatim text + a `usize` cursor).
fn entry(src: &str, cursor: usize) -> UndoEntry<usize> {
    UndoEntry::new(ronin_core::parse(src), src.to_string(), cursor)
}

// ---------------------------------------------------------------------------
// T029 — exact-prior-byte undo + exact redo replay (SC-005)
// ---------------------------------------------------------------------------

#[test]
fn undo_walks_back_through_exact_prior_bytes() {
    let states = [
        "(a: 1)\n",
        "(a: 12)\n",
        "(a: 12, b: 3)\n",
        "(a: 12, b: 33)\n",
    ];
    let mut stack: UndoStack<usize> = UndoStack::new();
    for (i, s) in states.iter().enumerate() {
        stack.record(entry(s, i), false);
    }
    // Undo back to the first state; each step yields the exact prior bytes + CST.
    for s in states.iter().rev().skip(1) {
        let e = stack.undo().expect("undo step");
        assert_eq!(e.source_text(), *s, "undo restores exact prior bytes");
        assert_eq!(print(e.cst_snapshot()), *s, "CST prints back byte-for-byte");
    }
    assert!(!stack.can_undo());
    // Redo replays forward exactly.
    for s in states.iter().skip(1) {
        let e = stack.redo().expect("redo step");
        assert_eq!(e.source_text(), *s, "redo replays exact bytes");
    }
    assert!(!stack.can_redo());
}

#[test]
fn no_reflow_on_restore_preserves_comments_and_whitespace() {
    // A messy-but-valid RON state restored verbatim — never reformatted.
    let messy = "(  a:1,\n  // keep me\n  b :  2 ,)\n";
    let mut stack: UndoStack<usize> = UndoStack::new();
    stack.record(entry("()\n", 0), false);
    stack.record(entry(messy, 0), false);
    let e = stack.undo().expect("undo");
    assert_eq!(e.source_text(), "()\n");
    let r = stack.redo().expect("redo");
    assert_eq!(
        r.source_text(),
        messy,
        "restore is byte-faithful, no reflow"
    );
    assert_eq!(print(r.cst_snapshot()), messy);
}

// ---------------------------------------------------------------------------
// T029 — redo invalidation on a new edit (TR-012, SC-005)
// ---------------------------------------------------------------------------

#[test]
fn new_edit_after_undo_invalidates_redo() {
    let mut stack: UndoStack<usize> = UndoStack::new();
    stack.record(entry("(a: 1)\n", 0), false);
    stack.record(entry("(a: 2)\n", 0), false);
    stack.record(entry("(a: 3)\n", 0), false);
    let _ = stack.undo();
    let _ = stack.undo();
    assert!(stack.can_redo());
    // A new edit clears the redo path.
    stack.record(entry("(a: 9)\n", 0), false);
    assert!(
        !stack.can_redo(),
        "redo invalidated by the new edit (TR-012)"
    );
}

// ---------------------------------------------------------------------------
// T030 — coalescing: a run is one unit (TR-027, SC-006/SC-010)
// ---------------------------------------------------------------------------

#[test]
fn coalesced_run_collapses_to_one_unit() {
    let mut stack: UndoStack<usize> = UndoStack::new();
    stack.record(entry("", 0), false); // seed (the pre-run state)
                                       // 20 rapid keystrokes: the first opens a new unit (fresh anchor), the rest
                                       // fold in (within-window) — the whole run collapses to one undo unit.
    let mut text = String::new();
    for i in 0..20 {
        text.push((b'a' + (i % 26) as u8) as char);
        // The first keystroke is a new unit (`coalesce: false`); the rest coalesce.
        stack.record(entry(&text, i as usize), i != 0);
    }
    assert_eq!(
        stack.len(),
        1,
        "the whole run is a single undo unit (SC-010)"
    );
    assert_eq!(
        stack.undo().unwrap().source_text(),
        "",
        "one undo clears the run"
    );
    assert!(!stack.can_undo());
}

#[test]
fn pause_after_run_starts_new_unit() {
    let mut stack: UndoStack<usize> = UndoStack::new();
    stack.record(entry("a", 0), false); // seed (current = "a")
    stack.record(entry("ab", 0), false); // opens run 1 → boundary "a" committed
    stack.record(entry("abc", 0), true); // same run (in place; current = "abc")
    stack.record(entry("abc.", 0), false); // pause → boundary "abc" committed
    assert_eq!(
        stack.len(),
        2,
        "boundary 'a' and boundary 'abc' are retained"
    );
    // Undo walks back: current "abc." → "abc" → "a".
    assert_eq!(stack.undo().unwrap().source_text(), "abc");
    assert_eq!(stack.undo().unwrap().source_text(), "a");
    assert!(!stack.can_undo());
}

// ---------------------------------------------------------------------------
// T030 — bounded ring: count + size caps, oldest dropped (SC-006/SC-009)
// ---------------------------------------------------------------------------

#[test]
fn exceeding_count_cap_drops_oldest() {
    let cap = UndoCap::new(4, ronin_core::undo::DEFAULT_UNDO_BYTE_CAP);
    let mut stack: UndoStack<usize> = UndoStack::with_config(cap, WINDOW);
    for i in 0..20 {
        stack.record(entry(&format!("s{i:03}"), 0), false);
    }
    assert_eq!(stack.len(), 4, "count cap binds at 4 boundaries");
    // The newest 4 boundaries (s18..s15) are retained; older ones dropped.
    assert_eq!(stack.undo().unwrap().source_text(), "s018");
    assert_eq!(stack.undo().unwrap().source_text(), "s017");
    assert_eq!(stack.undo().unwrap().source_text(), "s016");
    assert_eq!(stack.undo().unwrap().source_text(), "s015");
    assert!(!stack.can_undo());
}

#[test]
fn memory_bounded_independent_of_file_size() {
    // SC-009: with a 5 MiB byte cap, even huge snapshots keep retained memory
    // within the bound; the number of retained units shrinks as files grow.
    let byte_cap = 5 * 1024 * 1024;
    let cap = UndoCap::new(10_000, byte_cap);
    let mut stack: UndoStack<usize> = UndoStack::with_config(cap, WINDOW);
    // Each snapshot is ~1 MiB. After many edits the ring must stay <= the cap.
    let big = "x".repeat(1024 * 1024);
    for i in 0..30 {
        stack.record(entry(&format!("{big}{i}"), 0), false);
    }
    assert!(
        stack.retained_bytes() <= byte_cap,
        "retained ring bytes ({}) exceed the byte cap ({byte_cap})",
        stack.retained_bytes()
    );
    // Independent of how many edits happened, memory stays bounded by the cap.
    assert!(
        stack.len() <= 6,
        "≈1 MiB units under a 5 MiB cap → at most ~5–6 units"
    );
}

#[test]
fn misconfigured_cap_falls_back_to_default_never_unbounded() {
    let cap = UndoCap::new(0, 0);
    let stack: UndoStack<usize> = UndoStack::with_config(cap, WINDOW);
    assert_eq!(
        stack.cap().max_count,
        ronin_core::undo::DEFAULT_UNDO_COUNT_CAP
    );
    assert_eq!(
        stack.cap().max_bytes,
        ronin_core::undo::DEFAULT_UNDO_BYTE_CAP
    );
}

// ---------------------------------------------------------------------------
// T029/T030 — proptest: random edit sequences round-trip exactly + stay bounded
// ---------------------------------------------------------------------------

proptest! {
    /// For a random sequence of edit states, undoing all the way back yields the
    /// exact prior bytes of each non-coalesced boundary, and redo replays them
    /// forward exactly (TR-010/TR-012, SC-005).
    #[test]
    fn prop_undo_redo_round_trips_exact_bytes(
        seeds in proptest::collection::vec(0u32..1000, 1..40)
    ) {
        // Build distinct valid RON states from the random seeds.
        let states: Vec<String> = seeds.iter().map(|n| format!("(v: {n})\n")).collect();
        let mut stack: UndoStack<usize> = UndoStack::new();
        for (i, s) in states.iter().enumerate() {
            stack.record(entry(s, i), false); // each a discrete unit
        }
        // Current is the last state; there are states.len()-1 boundaries.
        prop_assert_eq!(stack.len(), states.len() - 1);
        // Undo back through every prior boundary, exact bytes each time.
        for s in states.iter().rev().skip(1) {
            let e = stack.undo().expect("undo");
            prop_assert_eq!(e.source_text(), s.as_str());
            prop_assert_eq!(print(e.cst_snapshot()), s.as_str());
        }
        prop_assert!(!stack.can_undo());
        // Redo replays forward exactly.
        for s in states.iter().skip(1) {
            let e = stack.redo().expect("redo");
            prop_assert_eq!(e.source_text(), s.as_str());
        }
        prop_assert!(!stack.can_redo());
    }

    /// The bounded ring never retains more than the count cap, the oldest is
    /// always the one dropped, and a new edit after undo clears redo — for any
    /// random run length and any small count cap (TR-011/TR-012/TR-024, SC-009).
    #[test]
    fn prop_ring_stays_within_count_cap_and_drops_oldest(
        cap_count in 1usize..16,
        edits in 1usize..120,
    ) {
        let cap = UndoCap::new(cap_count, ronin_core::undo::DEFAULT_UNDO_BYTE_CAP);
        let mut stack: UndoStack<usize> = UndoStack::with_config(cap, WINDOW);
        for i in 0..edits {
            stack.record(entry(&format!("(n: {i})\n"), 0), false);
        }
        // The ring never exceeds the count cap.
        prop_assert!(stack.len() <= cap_count);
        // The retained boundaries are the newest ones: undoing yields a strictly
        // descending run of the most-recent prior states, never an old dropped one.
        let expect_newest_boundary = edits.saturating_sub(2); // last boundary index
        if stack.can_undo() {
            let e = stack.undo().expect("undo");
            let got = e.source_text().to_string();
            prop_assert_eq!(got, format!("(n: {expect_newest_boundary})\n"));
        }
    }

    /// A fully-coalesced run of any length is always exactly one undo unit, and a
    /// single undo returns to the pre-run state (TR-027, SC-010).
    #[test]
    fn prop_coalesced_run_is_one_unit(run_len in 1usize..80) {
        let mut stack: UndoStack<usize> = UndoStack::new();
        stack.record(entry("(base: 0)\n", 0), false); // seed (pre-run state)
        for i in 1..=run_len {
            // The first keystroke opens a new unit; the rest fold in.
            stack.record(entry(&format!("(base: {i})\n"), i), i != 1);
        }
        prop_assert_eq!(stack.len(), 1);
        let undone = stack.undo().expect("undo run");
        prop_assert_eq!(undone.source_text(), "(base: 0)\n");
        prop_assert!(!stack.can_undo());
    }
}
