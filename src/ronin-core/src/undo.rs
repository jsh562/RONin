//! Bounded, WASM-clean CST-backed undo/redo history (E007 OBJ3, TR-010..014).
//!
//! [`UndoStack`] is the reusable undo/redo model the editor surfaces wire a
//! document into. Each reversible unit is an [`UndoEntry`]: a snapshot of the
//! **exact prior document bytes** (`source_text`), the lossless CST at that
//! state ([`CstDocument`], cheap to clone — see below), and a caller-supplied
//! cursor/metadata value. Restoring an entry therefore reproduces the prior
//! buffer byte-for-byte with no reflow or normalization (TR-010, SC-005).
//!
//! # WASM-clean (TR-014, HINT-002)
//!
//! This module adds **no** filesystem / native / I/O / network dependency to
//! `ronin-core` — it holds only CST + text + a generic cursor value, so the crate
//! stays `rowan`-only and the `wasm32-unknown-unknown` build gate keeps passing.
//! It also measures **no** wall-clock time: `std::time::Instant` is unavailable
//! on `wasm32-unknown-unknown`, so the *timing* of a coalesce boundary is decided
//! **caller-side** (the native `ronin-app` measures `Instant` elapsed against the
//! configured window) and handed in as the `coalesce` flag on [`record`](UndoStack::record).
//! [`coalesce_window`](UndoStack::coalesce_window) is retained only as reference
//! metadata; this module never reads a clock. All persistence (atomic save, the
//! recovery sidecar) lives in the native `ronin-app`; the undo model deliberately
//! knows nothing about it. The stack is reusable by downstream epics (E005/E008,
//! TR-013).
//!
//! # Cheap snapshots via structural sharing (AD-002, ADR-0001)
//!
//! [`CstDocument`] is `Clone`, and a rowan green tree is **immutable /
//! structurally shared**: cloning the document bumps an `Arc` refcount on the
//! green root, and an edit reuses every untouched subtree verbatim. Snapshotting
//! the CST per undo unit is therefore cheap (no deep copy of the tree), while the
//! retained `source_text` guarantees the exact-prior-byte restore. This is why
//! the undo unit is a CST + text snapshot rather than a reverse-delta log.
//!
//! # Cursor / metadata generic (`C`)
//!
//! [`UndoStack`] and [`UndoEntry`] are generic over a cursor/metadata type `C`
//! so `ronin-core` stays decoupled from any surface's cursor representation: the
//! native editor supplies its own `CursorState`, a future headless surface could
//! supply `()`. `C` is restored alongside the document on undo/redo (TR-010).
//!
//! # Bounds and coalescing (TR-011, TR-024, TR-027)
//!
//! The undo ring is bounded by **both** a count cap and a byte-size cap
//! ([`UndoCap`]); when either binds, the oldest unit is dropped so memory stays
//! predictable on large files (SC-006/SC-009). A rapid run of edits the caller
//! marks `coalesce: true` collapses into a single unit (SC-006/SC-010); the first
//! edit after a pause (`coalesce: false`) commits the prior boundary and starts a
//! new unit (TR-027). A new edit after an undo clears the redo stack (TR-012).
//!
//! # State model (current / undo ring / redo)
//!
//! The stack tracks the **current committed document state** ([`current`](UndoStack::current))
//! plus a bounded [`undo`](UndoStack) ring of prior boundaries and a [`redo`](UndoStack)
//! stack. The caller owns the live buffer; the stack owns the *history*. The
//! contract is:
//!
//! * [`record(entry, coalesce)`](UndoStack::record): `entry` is a snapshot of the
//!   document state **after** the edit just applied. When `coalesce` is `false`,
//!   the previous `current` is pushed onto the `undo` ring as a discrete
//!   boundary and `entry` becomes the new `current` (a new undo unit). When
//!   `coalesce` is `true`, `entry` replaces `current` **in place** without
//!   pushing a boundary, so a run of coalesced edits collapses to the single
//!   boundary captured before the run began. Either way, recording an edit clears
//!   `redo` (TR-012). The very first `record` on an empty stack just seeds
//!   `current` (there is no prior boundary to keep).
//! * [`undo()`](UndoStack::undo): pops the most recent boundary off the `undo`
//!   ring, pushes the *current* state onto `redo`, makes the popped boundary the
//!   new `current`, and returns it for the caller to apply to the buffer. Returns
//!   `None` when there is nothing to undo.
//! * [`redo()`](UndoStack::redo): pops the most recent state off `redo`, pushes
//!   the current state back onto the `undo` ring, makes the popped state the new
//!   `current`, and returns it. Returns `None` when there is nothing to redo.
//!
//! This keeps restore byte-faithful (the caller writes the returned entry's
//! `source_text` verbatim — no reflow) and the history strictly bounded.

use std::collections::VecDeque;
use std::time::Duration;

use crate::parser::CstDocument;

/// Default maximum number of undo units retained (TR-024).
///
/// Mirrors the `ronin-app` NEW-CONFIG default so a stack constructed without an
/// explicit cap behaves like the configured editor default.
pub const DEFAULT_UNDO_COUNT_CAP: usize = 200;

/// Default maximum total snapshot byte-size retained: 64 MiB (TR-024).
///
/// Bounds memory on large files independently of the unit count; whichever cap
/// binds first triggers dropping the oldest unit.
pub const DEFAULT_UNDO_BYTE_CAP: usize = 64 * 1024 * 1024;

/// Default coalesce window: edits less than 500 ms apart fold into one unit
/// (TR-027). A pause longer than this boundary starts a new undo unit.
///
/// This module never measures elapsed time against this value (it stays
/// WASM-clean — see the module docs). It is retained as reference/config
/// metadata; the caller measures elapsed time and supplies the coalesce decision.
pub const DEFAULT_COALESCE_WINDOW: Duration = Duration::from_millis(500);

/// The history bound for an [`UndoStack`]: a unit-count cap **and** a total
/// snapshot byte-size cap (TR-011, TR-024).
///
/// Both caps are enforced — whichever binds first drops the oldest unit — so
/// history stays bounded by count on small edits and by size on large buffers.
/// A misconfigured (zero) cap is never honored as "unbounded": construct via
/// [`UndoCap::new`], which falls back to the defaults for a zero field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UndoCap {
    /// Maximum number of undo units retained (`>= 1`).
    pub max_count: usize,
    /// Maximum total retained snapshot byte-size, in bytes (`>= 1`).
    pub max_bytes: usize,
}

impl Default for UndoCap {
    fn default() -> Self {
        Self {
            max_count: DEFAULT_UNDO_COUNT_CAP,
            max_bytes: DEFAULT_UNDO_BYTE_CAP,
        }
    }
}

impl UndoCap {
    /// Build a cap, falling back to the default for any zero field so the bound
    /// is **never** unbounded (TR-024: a misconfigured cap reverts to default).
    #[must_use]
    pub fn new(max_count: usize, max_bytes: usize) -> Self {
        Self {
            max_count: if max_count == 0 {
                DEFAULT_UNDO_COUNT_CAP
            } else {
                max_count
            },
            max_bytes: if max_bytes == 0 {
                DEFAULT_UNDO_BYTE_CAP
            } else {
                max_bytes
            },
        }
    }
}

/// One reversible unit — a snapshot of a single document state (TR-010).
///
/// Restoring an entry reproduces the document at this state: the CST snapshot,
/// the exact prior buffer bytes (`source_text`), and the caller's cursor value
/// (`cursor`). Generic over the cursor/metadata type `C` so `ronin-core` stays
/// decoupled from any surface's cursor representation (see module docs).
///
/// # Downstream reuse (E005 / E008 — TR-013)
///
/// Downstream editing epics build an entry with [`new`](Self::new) after applying
/// an edit, then hand it to [`UndoStack::record`]. On undo/redo they read back the
/// snapshot through [`source_text`](Self::source_text) (write to the buffer
/// verbatim — exact-prior-byte restore), [`cst_snapshot`](Self::cst_snapshot)
/// (reuse the structurally-shared tree without re-parsing), and
/// [`cursor`](Self::cursor) (restore caret/selection).
#[derive(Debug, Clone)]
pub struct UndoEntry<C> {
    /// The lossless CST at this state (cheap to clone — structural sharing).
    cst_snapshot: CstDocument,
    /// The exact prior buffer text this unit restores — the byte-faithful state
    /// (guarantees exact-prior-byte restore with no reflow, TR-010).
    source_text: String,
    /// The caret/selection (or other surface metadata) restored with this state.
    cursor: C,
}

impl<C> UndoEntry<C> {
    /// Snapshot a document state into a new undo unit.
    ///
    /// `cst_snapshot` should correspond to `source_text` (the CST whose printout
    /// is those exact bytes); `cursor` is the surface state to restore alongside.
    #[must_use]
    pub fn new(cst_snapshot: CstDocument, source_text: String, cursor: C) -> Self {
        Self {
            cst_snapshot,
            source_text,
            cursor,
        }
    }

    /// The CST snapshot at this state.
    #[must_use]
    pub fn cst_snapshot(&self) -> &CstDocument {
        &self.cst_snapshot
    }

    /// The exact prior buffer text this unit restores.
    #[must_use]
    pub fn source_text(&self) -> &str {
        &self.source_text
    }

    /// The cursor/metadata value restored with this state.
    #[must_use]
    pub fn cursor(&self) -> &C {
        &self.cursor
    }

    /// The retained byte-size this entry contributes to the stack's size cap —
    /// the length of the snapshot's `source_text` in bytes (TR-024).
    ///
    /// The CST snapshot itself is structurally shared and is not counted here;
    /// the source text is the dominant, deterministic per-unit cost.
    #[must_use]
    pub fn byte_size(&self) -> usize {
        self.source_text.len()
    }
}

/// The bounded undo/redo history for a single document (TR-010..014).
///
/// Newest reversible boundary is at the back/top of [`undo`](Self::undo). Generic
/// over the cursor/metadata type `C` carried in each [`UndoEntry`]. See the module
/// docs for the WASM-clean, structural-sharing, current/ring state model, and the
/// caller-side coalesce decision.
///
/// # Downstream reuse (E005 / E008 — TR-013)
///
/// This is the reusable undo/redo model the downstream editing epics edit against:
/// **E005** (smart authoring) and **E008** (structural / table editing). It lives
/// in `ronin-core` precisely so any surface can own a per-document handle, while the
/// editor-specific glue (cursor type, coalesce timing, dirty tracking) stays in the
/// host. The key reuse contract:
///
/// * **Construct per document** via [`with_config`](Self::with_config) (host's
///   NEW-CONFIG cap + coalesce window) or [`new`](Self::new) for the defaults.
/// * **After each edit**, [`record`](Self::record) an [`UndoEntry`] snapshot
///   (CST, exact bytes, cursor) with the **caller-measured** `coalesce` flag —
///   this module reads no clock, so the host decides run boundaries.
/// * **On undo/redo**, apply the returned entry's `source_text` to the buffer
///   **verbatim** for an exact-prior-byte restore (no reflow / normalization,
///   TR-010 / SC-005); redo is invalidated automatically by the next `record`.
///
/// Choose `C` to match the surface (the native editor uses its `CursorState`; a
/// headless consumer can use `()`). The history is always bounded (count + bytes)
/// and adds no filesystem / native dependency, so it is WASM-clean for every
/// downstream surface.
#[derive(Debug, Clone)]
pub struct UndoStack<C> {
    /// The current committed document state. `None` before the first
    /// [`record`](Self::record) seeds it. Restored-into on undo/redo; this is the
    /// state pushed onto `redo` (on undo) or back onto `undo` (on redo).
    current: Option<UndoEntry<C>>,
    /// The reversible history (a bounded ring), newest at the back/top. Bounded
    /// by `cap`; the oldest unit is dropped when either cap binds (TR-011).
    undo: VecDeque<UndoEntry<C>>,
    /// States popped by undo and replay-able by redo. **Cleared** on a new edit
    /// recorded after an undo (TR-012).
    redo: Vec<UndoEntry<C>>,
    /// The count + byte-size history bound (TR-011, TR-024).
    cap: UndoCap,
    /// The idle/run boundary that ends a coalesced keystroke run so one rapid
    /// run collapses to a single unit (TR-027). Retained as reference metadata
    /// only — this module never measures elapsed time against it; the caller
    /// supplies the coalesce decision (see module docs / TR-014).
    coalesce_window: Duration,
    /// Whether a coalescing keystroke run is currently open. While `true`, a
    /// `record(.., true)` updates `current` in place instead of pushing a new
    /// boundary, so the whole run collapses to one undo unit (TR-027). Reset by a
    /// non-coalescing record and by undo/redo (which close any open run).
    pending: bool,
}

impl<C> Default for UndoStack<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C> UndoStack<C> {
    /// Create an empty stack with the default cap and coalesce window.
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(UndoCap::default(), DEFAULT_COALESCE_WINDOW)
    }

    /// Create an empty stack with an explicit history cap and coalesce window.
    ///
    /// Use this to apply the editor's NEW-CONFIG values (`ronin-app`
    /// `AppSettings`). The cap is taken as-is; build it through [`UndoCap::new`]
    /// first so a zero/misconfigured field reverts to default (never unbounded).
    #[must_use]
    pub fn with_config(cap: UndoCap, coalesce_window: Duration) -> Self {
        Self {
            current: None,
            undo: VecDeque::new(),
            redo: Vec::new(),
            cap,
            coalesce_window,
            pending: false,
        }
    }

    /// The current history bound (count + byte-size cap).
    #[must_use]
    pub fn cap(&self) -> UndoCap {
        self.cap
    }

    /// The coalesce window: reference metadata for the caller's timing decision.
    ///
    /// This module never measures elapsed time against it (it stays WASM-clean);
    /// the caller compares its own `Instant` elapsed against this and passes the
    /// result as the `coalesce` flag to [`record`](Self::record).
    #[must_use]
    pub fn coalesce_window(&self) -> Duration {
        self.coalesce_window
    }

    /// The current committed document state, or `None` before the first record.
    #[must_use]
    pub fn current(&self) -> Option<&UndoEntry<C>> {
        self.current.as_ref()
    }

    /// Number of committed undo boundaries currently retained (the steps `undo`
    /// can take back from the current state).
    #[must_use]
    pub fn len(&self) -> usize {
        self.undo.len()
    }

    /// Whether there are no committed undo boundaries to step back to.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.undo.is_empty()
    }

    /// Whether a coalescing keystroke run is currently open. The document seam
    /// (T036) closes it implicitly on the next non-coalescing record or on
    /// undo/redo; exposed for tests and host wiring.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.pending
    }

    /// Whether an undo step is currently available.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// Whether a redo step is currently available.
    #[must_use]
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Number of states currently replay-able by redo (cleared on a new edit).
    #[must_use]
    pub fn redo_len(&self) -> usize {
        self.redo.len()
    }

    /// Total retained snapshot byte-size across the committed undo boundaries
    /// (compared against the byte-size cap, TR-024).
    ///
    /// Counts the `undo` ring only — the boundaries the cap drops oldest-first.
    /// The live `current` is the caller's buffer (not history) and `redo` is
    /// cleared by any new edit, so the ring is the bounded retained history.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        self.undo.iter().map(UndoEntry::byte_size).sum()
    }

    /// Total retained snapshot byte-size across **both** the undo ring and the
    /// redo stack plus the current state (the full in-memory history footprint).
    ///
    /// Used by SC-009 to assert the whole undo/redo memory stays bounded by the
    /// configured cap independent of file size; the per-cap drop is enforced
    /// against [`retained_bytes`](Self::retained_bytes) (the bounded ring).
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.undo.iter().map(UndoEntry::byte_size).sum::<usize>()
            + self.redo.iter().map(UndoEntry::byte_size).sum::<usize>()
            + self.current.as_ref().map_or(0, UndoEntry::byte_size)
    }

    // -- Mutating behavior (E007 Phase 5: T031..T034) -----------------------

    /// Record a new committed document state (T031/T032/T033/T034).
    ///
    /// `entry` is a snapshot of the document **after** the edit just applied
    /// (its `source_text` is the new exact buffer bytes, its `cst_snapshot` the
    /// matching CST, and `cursor` the post-edit caret). `coalesce` is the
    /// **caller-supplied** timing decision (this module measures no clock,
    /// TR-014):
    ///
    /// * `coalesce == false` — start a **new** undo unit: the previous `current`
    ///   boundary is pushed onto the bounded `undo` ring and `entry` becomes the
    ///   new `current` (TR-010). The first record on an empty stack just seeds
    ///   `current` with no boundary to keep. This is the first edit of a run (the
    ///   caller measured a pause, or a restore reset the timing anchor), so it is
    ///   its own discrete undo step that a following within-window edit may then
    ///   extend.
    /// * `coalesce == true` — **continue** the current run: `entry` replaces
    ///   `current` in place without pushing a boundary, so the whole keystroke run
    ///   collapses to the single boundary captured before the run began (TR-027,
    ///   one unit per run — SC-006/SC-010).
    ///
    /// In both forms `entry` becomes the live `current` and the run stays open, so
    /// the next within-window edit folds in. Either form **clears the redo stack**
    /// — a new edit after an undo invalidates redo (TR-012). After pushing a
    /// boundary the bounded ring is trimmed oldest-first by BOTH the count and
    /// byte-size cap (TR-011/TR-024).
    pub fn record(&mut self, entry: UndoEntry<C>, coalesce: bool) {
        // Any new edit invalidates the redo path (TR-012).
        self.redo.clear();

        match self.current.take() {
            // First state ever: just seed `current`; there is no prior boundary
            // and no run is open yet (the seed is not an edit).
            None => {
                self.current = Some(entry);
                self.pending = false;
            }
            Some(prev) => {
                if coalesce {
                    // Continue the open run: collapse into the current unit. The
                    // unit's boundary is already on the ring; advance the live
                    // state in place. `prev` (a within-run intermediate) is dropped.
                    self.current = Some(entry);
                } else {
                    // A new unit: the previous state becomes a discrete undo step.
                    self.push_boundary(prev);
                    self.current = Some(entry);
                }
                // After any real edit a coalescable run is open: a following
                // within-window edit folds into this unit.
                self.pending = true;
            }
        }
    }

    /// Undo one step: restore the most recent prior boundary (T031).
    ///
    /// Pops the newest boundary off the `undo` ring, pushes the current state
    /// onto `redo`, makes the popped boundary the new `current`, and returns it so
    /// the caller can apply its exact-prior `source_text`/`cst`/`cursor` to the
    /// buffer byte-for-byte (no reflow — TR-010/SC-005). Closes any open coalesce
    /// run. Returns `None` when there is nothing to undo.
    ///
    /// Requires `C: Clone` so the restored boundary can become the new `current`
    /// and also be returned to the caller (`CursorState` is `Clone`).
    #[must_use]
    pub fn undo(&mut self) -> Option<UndoEntry<C>>
    where
        C: Clone,
    {
        // An undo closes any open coalescing run: a following edit starts fresh.
        self.pending = false;
        let prior = self.undo.pop_back()?;
        if let Some(current) = self.current.take() {
            self.redo.push(current);
        }
        self.current = Some(prior.clone());
        Some(prior)
    }

    /// Redo one step: replay the most recently undone state (T031).
    ///
    /// Pops the newest state off `redo`, pushes the current state back onto the
    /// `undo` ring, makes the popped state the new `current`, and returns it for
    /// the caller to apply exactly (TR-010/SC-005). Closes any open coalesce run.
    /// Returns `None` when there is nothing to redo.
    ///
    /// Requires `C: Clone` so the replayed state can become the new `current`
    /// and also be returned to the caller (`CursorState` is `Clone`).
    #[must_use]
    pub fn redo(&mut self) -> Option<UndoEntry<C>>
    where
        C: Clone,
    {
        self.pending = false;
        let next = self.redo.pop()?;
        if let Some(current) = self.current.take() {
            // Pushing back onto the ring re-applies the cap (trim oldest if the
            // ring grew past the bound during a long undo/redo dance).
            self.push_boundary(current);
        }
        self.current = Some(next.clone());
        Some(next)
    }

    /// Push a boundary onto the bounded `undo` ring and trim to the cap (T034).
    ///
    /// Enforces BOTH the count cap AND the byte-size cap (TR-011/TR-024): after
    /// appending, the oldest unit is dropped while EITHER the unit count exceeds
    /// `max_count` OR the retained byte-size exceeds `max_bytes` (whichever binds
    /// first). The cap is never unbounded — [`UndoCap::new`] reverts a zero field
    /// to the default, so a misconfigured cap still bounds memory.
    fn push_boundary(&mut self, entry: UndoEntry<C>) {
        self.undo.push_back(entry);
        // Drop oldest while over the count cap.
        while self.undo.len() > self.cap.max_count {
            self.undo.pop_front();
        }
        // Drop oldest while over the byte-size cap, but always retain at least the
        // single newest boundary so an undo of the last edit never silently
        // disappears even if one snapshot alone exceeds the byte cap.
        while self.undo.len() > 1 && self.retained_bytes() > self.cap.max_bytes {
            self.undo.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an entry whose `source_text` is `src`, parsed CST, and cursor `cur`.
    fn entry(src: &str, cur: usize) -> UndoEntry<usize> {
        UndoEntry::new(crate::parse(src), src.to_string(), cur)
    }

    #[test]
    fn cap_new_falls_back_to_default_on_zero() {
        let cap = UndoCap::new(0, 0);
        assert_eq!(cap.max_count, DEFAULT_UNDO_COUNT_CAP);
        assert_eq!(cap.max_bytes, DEFAULT_UNDO_BYTE_CAP);

        let cap = UndoCap::new(10, 0);
        assert_eq!(cap.max_count, 10);
        assert_eq!(cap.max_bytes, DEFAULT_UNDO_BYTE_CAP);

        let cap = UndoCap::new(0, 1024);
        assert_eq!(cap.max_count, DEFAULT_UNDO_COUNT_CAP);
        assert_eq!(cap.max_bytes, 1024);
    }

    #[test]
    fn cap_default_matches_constants() {
        let cap = UndoCap::default();
        assert_eq!(cap.max_count, 200);
        assert_eq!(cap.max_bytes, 64 * 1024 * 1024);
    }

    #[test]
    fn new_stack_is_empty_with_default_config() {
        let stack: UndoStack<()> = UndoStack::new();
        assert!(stack.is_empty());
        assert_eq!(stack.len(), 0);
        assert!(!stack.can_undo());
        assert!(!stack.can_redo());
        assert_eq!(stack.cap(), UndoCap::default());
        assert_eq!(stack.coalesce_window(), DEFAULT_COALESCE_WINDOW);
        assert_eq!(stack.retained_bytes(), 0);
        assert!(stack.current().is_none());
    }

    #[test]
    fn with_config_applies_cap_and_window() {
        let cap = UndoCap::new(5, 4096);
        let window = Duration::from_millis(120);
        let stack: UndoStack<()> = UndoStack::with_config(cap, window);
        assert_eq!(stack.cap(), cap);
        assert_eq!(stack.coalesce_window(), window);
    }

    #[test]
    fn undo_entry_exposes_snapshot_text_and_cursor() {
        let src = "Foo(x: 1)\n";
        let doc = crate::parse(src);
        let entry = UndoEntry::new(doc, src.to_string(), 7usize);
        assert_eq!(entry.source_text(), src);
        assert_eq!(*entry.cursor(), 7usize);
        assert_eq!(entry.byte_size(), src.len());
        assert_eq!(crate::print(entry.cst_snapshot()), src);
    }

    // --- T031: snapshot push + undo/redo exact-prior-byte restore (SC-005) ---

    #[test]
    fn first_record_seeds_current_with_no_boundary() {
        let mut stack: UndoStack<usize> = UndoStack::new();
        stack.record(entry("(a: 1)\n", 0), false);
        assert_eq!(stack.len(), 0, "seeding does not create an undo boundary");
        assert!(!stack.can_undo());
        assert_eq!(stack.current().unwrap().source_text(), "(a: 1)\n");
    }

    #[test]
    fn undo_restores_exact_prior_bytes_and_redo_replays() {
        let mut stack: UndoStack<usize> = UndoStack::new();
        stack.record(entry("(a: 1)\n", 1), false);
        stack.record(entry("(a: 12)\n", 2), false);
        stack.record(entry("(a: 123)\n", 3), false);

        // Undo restores the exact prior bytes + cursor, in order.
        let u1 = stack.undo().expect("undo 1");
        assert_eq!(u1.source_text(), "(a: 12)\n");
        assert_eq!(*u1.cursor(), 2);
        assert_eq!(crate::print(u1.cst_snapshot()), "(a: 12)\n");

        let u2 = stack.undo().expect("undo 2");
        assert_eq!(u2.source_text(), "(a: 1)\n");
        assert_eq!(*u2.cursor(), 1);

        assert!(
            !stack.can_undo(),
            "back to the original; nothing more to undo"
        );

        // Redo replays them exactly in the reverse order.
        let r1 = stack.redo().expect("redo 1");
        assert_eq!(r1.source_text(), "(a: 12)\n");
        assert_eq!(*r1.cursor(), 2);

        let r2 = stack.redo().expect("redo 2");
        assert_eq!(r2.source_text(), "(a: 123)\n");
        assert_eq!(*r2.cursor(), 3);

        assert!(!stack.can_redo());
    }

    #[test]
    fn undo_on_empty_history_is_none() {
        let mut stack: UndoStack<usize> = UndoStack::new();
        assert!(stack.undo().is_none());
        stack.record(entry("(a: 1)\n", 0), false);
        // Only the seed exists; there is no prior boundary to undo to.
        assert!(stack.undo().is_none());
    }

    // --- T032: redo invalidation on a new edit (TR-012) ---------------------

    #[test]
    fn new_edit_after_undo_clears_redo() {
        let mut stack: UndoStack<usize> = UndoStack::new();
        stack.record(entry("(a: 1)\n", 0), false);
        stack.record(entry("(a: 2)\n", 0), false);
        stack.record(entry("(a: 3)\n", 0), false);

        let _ = stack.undo();
        let _ = stack.undo();
        assert!(stack.can_redo(), "two states are now redo-able");
        assert_eq!(stack.redo_len(), 2);

        // A brand-new edit invalidates the redo stack (TR-012).
        stack.record(entry("(a: 9)\n", 0), false);
        assert!(!stack.can_redo());
        assert_eq!(stack.redo_len(), 0);
    }

    // --- T033: coalescing — a run collapses to one unit (TR-027/SC-010) -----

    #[test]
    fn coalesced_run_is_a_single_undo_unit() {
        let mut stack: UndoStack<usize> = UndoStack::new();
        // Pre-run committed (seed) state.
        stack.record(entry("", 0), false);
        // A rapid keystroke run: the FIRST keystroke is a new unit (the caller
        // measured a pause / fresh anchor → `coalesce: false`), the rest fold in.
        stack.record(entry("h", 1), false);
        stack.record(entry("he", 2), true);
        stack.record(entry("hel", 3), true);
        stack.record(entry("hell", 4), true);
        stack.record(entry("hello", 5), true);

        assert_eq!(stack.len(), 1, "the whole run is a single undo unit");
        // One undo returns to the pre-run state (empty), not one char back.
        let u = stack.undo().expect("undo the run");
        assert_eq!(u.source_text(), "");
        assert!(!stack.can_undo());
    }

    #[test]
    fn coalesce_false_starts_a_new_unit() {
        let mut stack: UndoStack<usize> = UndoStack::new();
        stack.record(entry("a", 0), false); // seed
        stack.record(entry("ab", 0), false); // run 1 opens (boundary "a" kept)
        stack.record(entry("abc", 0), true); // same run (folds in place)
        stack.record(entry("abc d", 0), false); // pause: new unit (boundary "abc")

        assert_eq!(stack.len(), 2);
        assert_eq!(stack.undo().unwrap().source_text(), "abc");
        assert_eq!(stack.undo().unwrap().source_text(), "a");
        assert!(!stack.can_undo());
    }

    #[test]
    fn coalesce_run_closed_by_undo_then_new_run() {
        let mut stack: UndoStack<usize> = UndoStack::new();
        stack.record(entry("x", 0), false); // seed
        stack.record(entry("xy", 0), false); // opens a run (a real edit)
        stack.record(entry("xyz", 0), true); // folds into the run
        assert!(stack.has_pending());
        let _ = stack.undo();
        assert!(!stack.has_pending(), "undo closes the coalescing run");
    }

    // --- T034: bounded ring — count + byte caps, oldest dropped (SC-009) ----

    #[test]
    fn count_cap_drops_oldest_boundary() {
        let cap = UndoCap::new(3, DEFAULT_UNDO_BYTE_CAP);
        let mut stack: UndoStack<usize> = UndoStack::with_config(cap, DEFAULT_COALESCE_WINDOW);
        // 6 discrete edits → 5 boundaries pushed, capped to the newest 3.
        for i in 0..6 {
            stack.record(entry(&format!("v{i}"), 0), false);
        }
        assert_eq!(stack.len(), 3, "count cap binds at 3 boundaries");
        // The retained boundaries are the newest ones (v4, v3, v2 — v0/v1 dropped).
        assert_eq!(stack.undo().unwrap().source_text(), "v4");
        assert_eq!(stack.undo().unwrap().source_text(), "v3");
        assert_eq!(stack.undo().unwrap().source_text(), "v2");
        assert!(!stack.can_undo(), "oldest (v0, v1) were dropped");
    }

    #[test]
    fn byte_cap_drops_oldest_boundary_independent_of_count() {
        // A tight byte cap: each boundary's source_text is 10 bytes; a 25-byte
        // cap retains at most 2 boundaries by size even though the count cap is high.
        let cap = UndoCap::new(1000, 25);
        let mut stack: UndoStack<usize> = UndoStack::with_config(cap, DEFAULT_COALESCE_WINDOW);
        for i in 0..6 {
            // Exactly 10 bytes each: "aaaaaaaaaN".
            stack.record(entry(&format!("aaaaaaaaa{i}"), 0), false);
        }
        assert!(stack.retained_bytes() <= 25, "byte cap binds memory");
        assert!(
            stack.len() <= 2,
            "size cap retains at most 2 ten-byte units"
        );
    }

    #[test]
    fn byte_cap_retains_at_least_one_oversize_boundary() {
        // One snapshot alone exceeds the byte cap; we still retain it so the last
        // edit is undoable (never drop to zero on a single oversize unit).
        let cap = UndoCap::new(1000, 4);
        let mut stack: UndoStack<usize> = UndoStack::with_config(cap, DEFAULT_COALESCE_WINDOW);
        stack.record(entry("aaaaaaaaaa", 0), false); // 10 bytes (seed, no boundary)
        stack.record(entry("bbbbbbbbbb", 0), false); // pushes the 10-byte boundary
        assert_eq!(stack.len(), 1, "the single oversize boundary is retained");
        assert_eq!(stack.undo().unwrap().source_text(), "aaaaaaaaaa");
    }

    #[test]
    fn misconfigured_zero_cap_falls_back_to_default() {
        // A zero cap must never mean "unbounded": UndoCap::new reverts to default.
        let cap = UndoCap::new(0, 0);
        let stack: UndoStack<usize> = UndoStack::with_config(cap, DEFAULT_COALESCE_WINDOW);
        assert_eq!(stack.cap().max_count, DEFAULT_UNDO_COUNT_CAP);
        assert_eq!(stack.cap().max_bytes, DEFAULT_UNDO_BYTE_CAP);
    }

    #[test]
    fn total_bytes_tracks_full_footprint_and_stays_bounded() {
        let cap = UndoCap::new(3, DEFAULT_UNDO_BYTE_CAP);
        let mut stack: UndoStack<usize> = UndoStack::with_config(cap, DEFAULT_COALESCE_WINDOW);
        for i in 0..10 {
            stack.record(entry(&format!("value-{i:03}"), 0), false);
        }
        // The undo ring is bounded to 3 boundaries regardless of session length.
        assert_eq!(stack.len(), 3);
        // total_bytes counts ring + redo + current; after the edits redo is empty.
        let bound = 4 * 9; // (3 ring + 1 current) entries of 9 bytes "value-00N"
        assert!(stack.total_bytes() <= bound);
    }
}
