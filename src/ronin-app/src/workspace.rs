//! The in-memory multi-tab workspace: the ordered set of open documents, the
//! active-tab pointer, and the bounded recently-closed stack (FR-012/FR-025).
//!
//! [`EditorWorkspace`] is the root of the editor's tab state. It owns every open
//! [`EditorDocument`], tracks which one is active, and keeps a small LIFO of
//! [`ClosedDocumentRecord`]s so a just-closed tab can be reopened from memory
//! (never a fresh disk reload). All tab operations — open, switch, close,
//! reorder, close-all, close-others, reopen — are methods here so the `App`
//! shell can delegate to one well-tested state machine (data-model.md
//! §EditorWorkspace).
//!
//! Invariants enforced throughout:
//!
//! * [`active_index`](EditorWorkspace::active_index), when `Some`, is always a
//!   valid index into [`documents`](EditorWorkspace::documents); when the
//!   document set is empty, `active_index` is `None`.
//! * Reordering preserves each document's identity and dirty state — only its
//!   position changes.
//! * The [`recently_closed`](EditorWorkspace::recently_closed) stack is bounded
//!   to [`RECENTLY_CLOSED_CAP`]; the oldest record is evicted on overflow, and
//!   reopening an empty stack is a harmless no-op.
//! * Untitled sequence numbers are minted monotonically and never recycled
//!   within a session.

use std::path::PathBuf;

use crate::document::{ByteFidelityProfile, CursorState, EditorDocument, SavedSnapshot};

/// The maximum number of records kept on the recently-closed stack (FR-012).
///
/// Older records are evicted FIFO when a new close would exceed this bound, so
/// the stack can never grow without limit.
pub const RECENTLY_CLOSED_CAP: usize = 10;

/// A minimal snapshot retained when a tab is closed, enabling reopen-last-closed
/// without re-reading disk for a still-known buffer (FR-012, data-model.md
/// §ClosedDocumentRecord).
///
/// It carries everything needed to reconstruct an [`EditorDocument`] whose dirty
/// state and byte-fidelity baseline match what it had at close time: the closed
/// buffer text, the saved baseline (so a reopened-but-unsaved buffer comes back
/// *dirty*), the original byte-fidelity profile (so a later Save stays
/// byte-preserving), and the per-tab cursor.
#[derive(Debug, Clone)]
pub struct ClosedDocumentRecord {
    /// The file this document mapped to on disk, or `None` for a never-saved
    /// buffer.
    pub path: Option<PathBuf>,
    /// The buffer contents at close time, so unsaved-then-discarded text returns.
    pub restorable_text: String,
    /// The saved snapshot the document had at close, so its dirty state is
    /// reconstructed faithfully on reopen.
    pub saved_baseline: SavedSnapshot,
    /// The original-on-load byte-fidelity profile, carried so a reopened document
    /// keeps its byte-preserving Save baseline (FR-020).
    pub byte_metadata: ByteFidelityProfile,
    /// The per-tab caret/selection/scroll state at close time.
    pub cursor: CursorState,
    /// The untitled sequence number the document carried, if it was a never-saved
    /// buffer; preserved so a reopened untitled tab keeps its stable title.
    pub untitled_seq: Option<u32>,
}

impl ClosedDocumentRecord {
    /// Capture a record from a document about to be removed (FR-012).
    ///
    /// The buffer is cloned into [`restorable_text`](Self::restorable_text) and
    /// the document's saved baseline, byte profile, and cursor are carried so the
    /// record reconstructs the exact dirty/fidelity state on reopen.
    #[must_use]
    pub fn capture(doc: &EditorDocument) -> Self {
        Self {
            path: doc.path.clone(),
            restorable_text: doc.buffer.clone(),
            saved_baseline: doc.last_saved,
            byte_metadata: doc.byte_profile,
            cursor: doc.cursor,
            untitled_seq: doc.untitled_seq,
        }
    }

    /// Reconstruct an [`EditorDocument`] from this record (FR-012).
    ///
    /// The rebuilt document's buffer is the closed text, its saved baseline is the
    /// one captured at close (so its dirty state matches what it had then), its
    /// byte profile is the carried original-on-load profile (so a subsequent Save
    /// stays byte-preserving), and its cursor is restored. A fresh parse is
    /// requested by the caller after reopen, so derived parse/highlight state
    /// starts empty.
    #[must_use]
    pub fn into_document(self) -> EditorDocument {
        EditorDocument::from_restorable(
            self.path,
            self.restorable_text,
            self.saved_baseline,
            self.byte_metadata,
            self.cursor,
            self.untitled_seq,
        )
    }
}

/// The in-memory root of the editor's multi-tab state (FR-012).
///
/// Owns the ordered open-document set (tab order = list order), the active-tab
/// index, the bounded recently-closed stack, and the monotonic untitled counter.
/// Created once at startup and never persisted across launches (FR-016).
#[derive(Debug, Default)]
pub struct EditorWorkspace {
    /// The open documents, one per tab; list order is tab order (FR-012).
    documents: Vec<EditorDocument>,
    /// Index of the active tab, or `None` when no tab is open.
    active_index: Option<usize>,
    /// LIFO stack of recently-closed records, bounded to [`RECENTLY_CLOSED_CAP`].
    recently_closed: Vec<ClosedDocumentRecord>,
    /// Next sequence number for an `Untitled-N` document; only ever increments.
    next_untitled: u32,
}

impl EditorWorkspace {
    /// Construct an empty workspace with no open tabs.
    #[must_use]
    pub fn new() -> Self {
        Self {
            documents: Vec::new(),
            active_index: None,
            recently_closed: Vec::new(),
            // Untitled numbering starts at 1 so the first new buffer is
            // `Untitled-1`.
            next_untitled: 1,
        }
    }

    /// The open documents in tab order (read-only).
    #[must_use]
    pub fn documents(&self) -> &[EditorDocument] {
        &self.documents
    }

    /// The open documents in tab order (mutable, for in-place editing).
    pub fn documents_mut(&mut self) -> &mut [EditorDocument] {
        &mut self.documents
    }

    /// The number of open tabs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// `true` when no tabs are open.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    /// The active tab index, or `None` when no tab is open.
    #[must_use]
    pub fn active_index(&self) -> Option<usize> {
        self.active_index
    }

    /// The document at `idx`, if it exists.
    #[must_use]
    pub fn get(&self, idx: usize) -> Option<&EditorDocument> {
        self.documents.get(idx)
    }

    /// The document at `idx` mutably, if it exists.
    #[must_use]
    pub fn get_mut(&mut self, idx: usize) -> Option<&mut EditorDocument> {
        self.documents.get_mut(idx)
    }

    /// The active document, if any.
    #[must_use]
    pub fn active_document(&self) -> Option<&EditorDocument> {
        self.active_index.and_then(|i| self.documents.get(i))
    }

    /// The active document mutably, if any.
    #[must_use]
    pub fn active_document_mut(&mut self) -> Option<&mut EditorDocument> {
        match self.active_index {
            Some(i) => self.documents.get_mut(i),
            None => None,
        }
    }

    /// The recently-closed records (most-recently-closed last; read-only).
    #[must_use]
    pub fn recently_closed(&self) -> &[ClosedDocumentRecord] {
        &self.recently_closed
    }

    /// Open `doc` as a new tab and make it active (FR-012).
    ///
    /// The document is appended (becoming the last tab) and the active index is
    /// set to it. Returns the new tab's index.
    pub fn open(&mut self, doc: EditorDocument) -> usize {
        self.documents.push(doc);
        let idx = self.documents.len() - 1;
        self.active_index = Some(idx);
        idx
    }

    /// Switch the active tab to `idx` (FR-012).
    ///
    /// A no-op (returns `false`) if `idx` is out of range, so an invalid switch
    /// never corrupts the active pointer.
    pub fn switch(&mut self, idx: usize) -> bool {
        if idx < self.documents.len() {
            self.active_index = Some(idx);
            true
        } else {
            false
        }
    }

    /// Remove the tab at `idx` and return its document so the caller can capture a
    /// [`ClosedDocumentRecord`] (FR-012).
    ///
    /// The active index is fixed up to remain valid: a removal before the active
    /// tab shifts it left, removing the active tab clamps it to the nearest
    /// remaining tab, and emptying the set sets it to `None`. Returns `None` if
    /// `idx` is out of range.
    pub fn close(&mut self, idx: usize) -> Option<EditorDocument> {
        if idx >= self.documents.len() {
            return None;
        }
        let doc = self.documents.remove(idx);
        self.fixup_active_after_removal(idx);
        Some(doc)
    }

    /// Recompute [`active_index`](Self::active_index) after the tab at `removed`
    /// was taken out, preserving the invariant that it always points at a valid
    /// tab (or is `None` when empty).
    fn fixup_active_after_removal(&mut self, removed: usize) {
        self.active_index = match self.active_index {
            _ if self.documents.is_empty() => None,
            Some(a) if a > removed => Some(a - 1),
            Some(a) if a == removed => Some(a.min(self.documents.len() - 1)),
            other => other,
        };
    }

    /// Move the tab at `from` to position `to`, preserving identity and dirty
    /// state (FR-012).
    ///
    /// Only the order changes — the moved document is the *same* document (same
    /// buffer, same dirty flag, same cursor). The active index is remapped so the
    /// same document stays active across the move. Out-of-range indices or a
    /// no-op move (`from == to`) leave the workspace unchanged; `to` is clamped to
    /// the last valid slot. Returns `true` if a move occurred.
    pub fn reorder(&mut self, from: usize, to: usize) -> bool {
        let len = self.documents.len();
        if from >= len {
            return false;
        }
        let to = to.min(len - 1);
        if from == to {
            return false;
        }
        // Remember which document is active so we can follow it across the move.
        let active_doc_was = self.active_index;
        let doc = self.documents.remove(from);
        self.documents.insert(to, doc);
        // Remap the active index to keep the same document active.
        if let Some(active) = active_doc_was {
            self.active_index = Some(remap_index_after_move(active, from, to));
        }
        true
    }

    /// Close every tab, returning the removed documents in their former tab order
    /// (FR-012).
    ///
    /// The active index is cleared. The caller is responsible for any per-document
    /// bookkeeping (e.g. pushing close records). After this the workspace is empty.
    pub fn close_all(&mut self) -> Vec<EditorDocument> {
        let removed = std::mem::take(&mut self.documents);
        self.active_index = None;
        removed
    }

    /// Close every tab except `keep_idx`, returning the removed documents in
    /// former tab order (FR-012).
    ///
    /// The kept document becomes the sole, active tab. If `keep_idx` is out of
    /// range nothing is removed (returns an empty vec) and the workspace is
    /// unchanged, so a stale index can never empty the workspace by surprise.
    pub fn close_others(&mut self, keep_idx: usize) -> Vec<EditorDocument> {
        if keep_idx >= self.documents.len() {
            return Vec::new();
        }
        // Drain everything except the kept index, preserving tab order in the
        // returned list and leaving the kept document in place.
        let mut removed = Vec::with_capacity(self.documents.len().saturating_sub(1));
        let mut kept: Option<EditorDocument> = None;
        for (i, doc) in std::mem::take(&mut self.documents).into_iter().enumerate() {
            if i == keep_idx {
                kept = Some(doc);
            } else {
                removed.push(doc);
            }
        }
        if let Some(doc) = kept {
            self.documents.push(doc);
            self.active_index = Some(0);
        } else {
            self.active_index = None;
        }
        removed
    }

    /// Push a [`ClosedDocumentRecord`] onto the recently-closed stack, evicting
    /// the oldest record if the bound is exceeded (FR-012).
    pub fn push_closed(&mut self, record: ClosedDocumentRecord) {
        self.recently_closed.push(record);
        if self.recently_closed.len() > RECENTLY_CLOSED_CAP {
            // Evict the oldest (front) record so the stack stays bounded.
            self.recently_closed.remove(0);
        }
    }

    /// Pop the most-recently-closed record and reopen it as a new active tab
    /// (FR-012).
    ///
    /// The record is reconstructed into an [`EditorDocument`] whose dirty state
    /// and byte-fidelity baseline match what it had at close. Reopening an empty
    /// stack is a harmless no-op (returns `None`). Focus-existing (FR-025) is
    /// applied by the `App` caller, not here, since canonicalization is I/O.
    /// Returns the reopened tab's index on success.
    pub fn reopen_closed(&mut self) -> Option<usize> {
        let record = self.recently_closed.pop()?;
        let doc = record.into_document();
        Some(self.open(doc))
    }

    /// Mint a fresh, monotonic untitled sequence number (FR-012).
    ///
    /// The counter only ever increments, so a number is never recycled within a
    /// session even if intervening untitled tabs are closed.
    pub fn next_untitled_seq(&mut self) -> u32 {
        let seq = self.next_untitled;
        self.next_untitled = self.next_untitled.saturating_add(1);
        seq
    }

    /// Create a fresh, empty untitled document, open it as the active tab, and
    /// return its index (FR-012).
    pub fn push_untitled(&mut self) -> usize {
        let seq = self.next_untitled_seq();
        self.open(EditorDocument::new_untitled(seq))
    }

    /// Find the index of an open document whose path equals `target` (FR-025).
    ///
    /// Used for focus-existing: never-saved (path-less) buffers are exempt and so
    /// never match. The caller supplies an already-canonicalized `target` (or the
    /// raw path when canonicalization failed) and is responsible for canonicalizing
    /// the stored paths the same way for a correct comparison; this helper does the
    /// raw equality scan.
    #[must_use]
    pub fn find_by_path(&self, target: &std::path::Path) -> Option<usize> {
        self.documents
            .iter()
            .position(|d| d.path.as_deref() == Some(target))
    }
}

/// Remap an index after the element at `from` is moved to `to` (used to keep the
/// active tab following its document across a reorder).
///
/// Mirrors the `Vec::remove` + `Vec::insert` shift performed by
/// [`EditorWorkspace::reorder`].
fn remap_index_after_move(index: usize, from: usize, to: usize) -> usize {
    if index == from {
        // The moved element itself lands at `to`.
        to
    } else if from < to {
        // Elements in (from, to] shift left by one.
        if index > from && index <= to {
            index - 1
        } else {
            index
        }
    } else {
        // from > to: elements in [to, from) shift right by one.
        if index >= to && index < from {
            index + 1
        } else {
            index
        }
    }
}
