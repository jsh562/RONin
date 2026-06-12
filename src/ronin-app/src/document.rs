//! The in-memory editor document model and its byte-fidelity profile.
//!
//! Two concerns live here:
//!
//! * [`ByteFidelityProfile`] (FR-020) captures everything a *lossless* save needs
//!   to re-emit a file exactly as the user expects: original line-ending style,
//!   whether the file had a trailing newline, whether it carried a UTF-8 BOM, and
//!   a cheap content hash of the loaded bytes. RONin's first principle is "never
//!   corrupt user data" (project-instructions §I); this profile is how the shell
//!   honours that on round-trip.
//! * [`EditorDocument`] (FR-007) is the per-tab document: the editable buffer, its
//!   on-disk identity, a saved snapshot for dirty-tracking, cursor/scroll state,
//!   and optional derived parse/highlight artifacts produced off the UI thread.
//!
//! # Deferred scope (E007)
//!
//! The document carries no edit history: **undo / redo** (alongside the atomic
//! save + crash-recovery pipeline) is deferred to **E007**. The dirty-tracking
//! and saved-snapshot machinery here is the seam an undo stack will build on.

use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::diagnostics_map::{map_diagnostic, DiagnosticView};
use crate::editor_view::build_highlight_model;
use crate::reparse::{ParseResult, ReparseWorker};

/// Process-wide monotonic source of per-document identity tokens.
///
/// Each [`EditorDocument`] takes one at construction; the token is stable for the
/// document's lifetime and is never reused, so batch tab operations can track a
/// tab by identity even as tab indices shift around it (FR-026).
static NEXT_DOC_ID: AtomicU64 = AtomicU64::new(1);

/// Mint the next process-unique document identity token.
fn mint_doc_id() -> u64 {
    NEXT_DOC_ID.fetch_add(1, Ordering::Relaxed)
}

/// The newline convention detected in a loaded file (FR-020).
///
/// `Crlf`/`Lf` describe a file that uses one style uniformly. `Mixed` marks a
/// file that contained *both* `\r\n` and lone `\n`; the dominant style is then
/// carried separately on [`ByteFidelityProfile::dominant`] so a later save can
/// normalise to a single, predictable convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineEnding {
    /// Windows-style carriage-return + line-feed (`\r\n`).
    Crlf,
    /// Unix-style line-feed (`\n`).
    Lf,
    /// The file mixed `\r\n` and lone `\n`.
    Mixed,
}

/// Byte-level fidelity metadata captured when a file is loaded (FR-020).
///
/// Everything here is needed to reproduce the user's file faithfully on save:
/// the line-ending style (with a never-`Mixed` [`dominant`](Self::dominant) for
/// re-emission), trailing-newline presence, BOM presence, and a content hash of
/// the originally loaded bytes for cheap change detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteFidelityProfile {
    /// The detected line-ending style of the loaded file.
    pub line_ending: LineEnding,
    /// For `Mixed` files, the more frequent concrete style to normalise to on
    /// save; for uniform files this equals [`line_ending`](Self::line_ending).
    /// Invariant: this is always [`LineEnding::Crlf`] or [`LineEnding::Lf`],
    /// never [`LineEnding::Mixed`]. Ties resolve to [`LineEnding::Lf`].
    pub dominant: LineEnding,
    /// `true` when the loaded file ended with a newline (`\n`).
    pub had_trailing_newline: bool,
    /// `true` when the loaded bytes began with a UTF-8 BOM (`EF BB BF`).
    pub had_bom: bool,
    /// A cheap hash of the originally loaded raw bytes, for change detection.
    pub original_hash: u64,
}

/// The UTF-8 BOM byte sequence (`EF BB BF`).
const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

impl ByteFidelityProfile {
    /// Analyse raw file bytes and capture the fidelity profile (FR-020).
    ///
    /// Detection rules:
    /// * BOM: leading `EF BB BF`.
    /// * Line endings: count `\r\n` pairs versus lone `\n` (a `\n` not preceded
    ///   by `\r`). All-CRLF → [`LineEnding::Crlf`]; all-LF → [`LineEnding::Lf`];
    ///   both present → [`LineEnding::Mixed`]. A file with no newlines is treated
    ///   as [`LineEnding::Lf`] (the safe default for re-emission).
    /// * `dominant`: the more frequent of CRLF/LF; ties (including the
    ///   no-newline case) resolve to [`LineEnding::Lf`]; never `Mixed`.
    /// * `had_trailing_newline`: the bytes end in `\n`.
    /// * `original_hash`: hash of the raw bytes (length-sensitive).
    #[must_use]
    pub fn from_bytes(raw: &[u8]) -> Self {
        let had_bom = raw.starts_with(&BOM);

        // Count CRLF pairs and lone LFs in a single pass.
        let mut crlf = 0usize;
        let mut lone_lf = 0usize;
        let mut prev_cr = false;
        for &b in raw {
            if b == b'\n' {
                if prev_cr {
                    crlf += 1;
                } else {
                    lone_lf += 1;
                }
            }
            prev_cr = b == b'\r';
        }

        let line_ending = match (crlf > 0, lone_lf > 0) {
            (true, true) => LineEnding::Mixed,
            (true, false) => LineEnding::Crlf,
            (false, true) => LineEnding::Lf,
            // No newlines at all: default to LF for predictable re-emission.
            (false, false) => LineEnding::Lf,
        };

        // Dominant is the more frequent concrete style; ties (and the
        // no-newline case) resolve to LF. Never `Mixed`.
        let dominant = if crlf > lone_lf {
            LineEnding::Crlf
        } else {
            LineEnding::Lf
        };

        let had_trailing_newline = raw.last() == Some(&b'\n');

        let original_hash = hash_bytes(raw);

        Self {
            line_ending,
            dominant,
            had_trailing_newline,
            had_bom,
            original_hash,
        }
    }
}

/// Hash arbitrary bytes with the standard library's default hasher.
///
/// Used for cheap content-change detection; the exact algorithm is an
/// implementation detail (not stable across toolchains) and is never persisted.
fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// A minimal placeholder for computed syntax-highlight spans.
///
/// Wave 1 only needs a concrete, real type so [`EditorDocument`] can hold an
/// `Option<HighlightModel>`; later waves (editor view) will populate it with
/// actual highlight spans derived from the CST. It is intentionally cheap and
/// inert for now — not a `// TODO` stub but a real, empty-by-default model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HighlightModel {
    /// The reparse generation the spans were computed from, when populated.
    /// `None` means "no highlight computed yet".
    pub generation: Option<u64>,
    /// Highlight spans as `(char_start, char_end, class)` triples. Empty until a
    /// later wave computes them from the CST.
    pub spans: Vec<HighlightSpan>,
}

/// A single highlight span over character offsets. Reserved for the editor view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightSpan {
    /// Inclusive start char offset.
    pub start: usize,
    /// Exclusive end char offset.
    pub end: usize,
    /// A stable, human-readable highlight class name (e.g. `"string"`).
    pub class: String,
}

/// A snapshot of the last-saved (or last-loaded) document state, used to derive
/// the dirty flag without retaining a second full copy of the buffer.
///
/// We store a content hash plus length: comparison is O(1) and false-positives
/// are astronomically unlikely for editor-sized buffers. Length is included so
/// trivially-different buffers never alias on hash alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SavedSnapshot {
    /// Hash of the buffer contents at save/load time.
    content_hash: u64,
    /// Byte length of the buffer at save/load time.
    len: usize,
}

impl SavedSnapshot {
    /// Capture a snapshot of the given buffer contents.
    #[must_use]
    pub fn of(buffer: &str) -> Self {
        Self {
            content_hash: hash_bytes(buffer.as_bytes()),
            len: buffer.len(),
        }
    }

    /// `true` if `buffer` still matches this snapshot (same length and hash).
    #[must_use]
    pub fn matches(&self, buffer: &str) -> bool {
        self.len == buffer.len() && self.content_hash == hash_bytes(buffer.as_bytes())
    }
}

/// Caret, selection, and scroll state for a document, in **character** offsets.
///
/// Character offsets (not byte offsets) are used throughout the editor surface so
/// the model is independent of UTF-8 encoding width.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CursorState {
    /// Caret position as a character offset into the buffer.
    pub caret: usize,
    /// Active selection as an ordered `(anchor, head)` char-offset pair, if any.
    pub selection: Option<(usize, usize)>,
    /// Vertical scroll offset (logical pixels), preserved across reparses.
    pub scroll: f32,
}

impl Default for CursorState {
    fn default() -> Self {
        Self {
            caret: 0,
            selection: None,
            scroll: 0.0,
        }
    }
}

/// One editor tab's document: editable text plus on-disk identity and derived
/// state (FR-007).
///
/// Construct via [`EditorDocument::from_loaded`] (an existing file) or
/// [`EditorDocument::new_untitled`] (a fresh buffer). The [`dirty`](Self::dirty)
/// and [`oversize`](Self::oversize) predicates are derived, never stored.
#[derive(Debug, Clone)]
pub struct EditorDocument {
    /// A process-unique identity token, stable for this document's lifetime.
    ///
    /// Used by batch tab operations (FR-026) to track a specific tab by identity
    /// while indices shift as other tabs close. Cloning a document copies the
    /// token (a clone is "the same document" for tracking purposes).
    id: u64,
    /// The live, editable text buffer (always valid UTF-8).
    pub buffer: String,
    /// The file this document maps to on disk, or `None` for an unsaved buffer.
    pub path: Option<PathBuf>,
    /// Snapshot of the content at the last save/load, for dirty-tracking.
    pub last_saved: SavedSnapshot,
    /// Byte-fidelity metadata captured at load (or defaults for a new buffer).
    pub byte_profile: ByteFidelityProfile,
    /// Caret/selection/scroll state in character offsets.
    pub cursor: CursorState,
    /// The most recent off-thread parse result, when one has been installed.
    pub parse: Option<ParseResult>,
    /// The most recent computed highlight model, when one has been installed.
    pub highlight: Option<HighlightModel>,
    /// The last-good diagnostics projected into editor coordinates (FR-008).
    ///
    /// Refreshed only when a fresh, current [`ParseResult`] lands via
    /// [`poll_parse`](Self::poll_parse); deliberately **not** cleared on edit so
    /// the views keep showing the last-good problems while a reparse is in flight
    /// (FR-006).
    pub diagnostics: Vec<DiagnosticView>,
    /// For untitled documents, the workspace-assigned sequence number used to
    /// render a stable `Untitled-N` title. `None` for on-disk documents.
    pub untitled_seq: Option<u32>,
    /// Monotonic edit generation: bumped on every buffer mutation (FR-006). A
    /// landed [`ParseResult`] installs only when its generation equals this, so
    /// stale results are discarded.
    edit_generation: u64,
    /// The generation last handed to the [`ReparseWorker`]. Coalesces requests:
    /// a request is only sent when the edit generation has actually advanced past
    /// what was last requested, so rapid keystrokes collapse to the latest text.
    last_requested_generation: u64,
    /// A pending caret jump to apply to the editor on the next frame, expressed
    /// as a **character** offset into the buffer (FR-009).
    ///
    /// Set when the user clicks a Problems-panel row; consumed by `editor_view`,
    /// which pushes it into the live `TextEdit` cursor state and scrolls it into
    /// view. A stale offset (past the current buffer) is clamped to the buffer's
    /// character length when consumed, so navigation never lands out of bounds
    /// even if the buffer shrank since the diagnostic was produced.
    pending_cursor_jump: Option<usize>,
}

impl EditorDocument {
    /// Build a document from a freshly loaded file's raw bytes (FR-007).
    ///
    /// Decodes UTF-8 (rejecting invalid input), captures the byte-fidelity
    /// profile, strips a leading BOM from the editable buffer (its presence is
    /// remembered on the profile for faithful re-emission), and records the
    /// loaded content as the saved snapshot so the document starts clean.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`std::str::Utf8Error`] if `raw` is not valid
    /// UTF-8. (Higher layers — see `fileio` — map this to a user-facing error.)
    pub fn from_loaded(path: impl Into<PathBuf>, raw: &[u8]) -> Result<Self, std::str::Utf8Error> {
        let profile = ByteFidelityProfile::from_bytes(raw);
        let text = std::str::from_utf8(raw)?;
        // The BOM is fidelity metadata, not editable content: keep it out of the
        // buffer but remembered on the profile so save can re-emit it.
        let buffer = text.strip_prefix('\u{FEFF}').unwrap_or(text).to_string();
        let last_saved = SavedSnapshot::of(&buffer);
        Ok(Self {
            id: mint_doc_id(),
            buffer,
            path: Some(path.into()),
            last_saved,
            byte_profile: profile,
            cursor: CursorState::default(),
            parse: None,
            highlight: None,
            diagnostics: Vec::new(),
            untitled_seq: None,
            edit_generation: 0,
            last_requested_generation: 0,
            pending_cursor_jump: None,
        })
    }

    /// Create a fresh, empty untitled document with a workspace-assigned
    /// sequence number used for its `Untitled-N` title.
    #[must_use]
    pub fn new_untitled(seq: u32) -> Self {
        let buffer = String::new();
        let last_saved = SavedSnapshot::of(&buffer);
        Self {
            id: mint_doc_id(),
            buffer,
            path: None,
            last_saved,
            // A new buffer has no file bytes; default to LF, no BOM, no trailing
            // newline. `original_hash` is the hash of empty content.
            byte_profile: ByteFidelityProfile {
                line_ending: LineEnding::Lf,
                dominant: LineEnding::Lf,
                had_trailing_newline: false,
                had_bom: false,
                original_hash: hash_bytes(&[]),
            },
            cursor: CursorState::default(),
            parse: None,
            highlight: None,
            diagnostics: Vec::new(),
            untitled_seq: Some(seq),
            edit_generation: 0,
            last_requested_generation: 0,
            pending_cursor_jump: None,
        }
    }

    /// Reconstruct a document from a recently-closed record's fields (FR-012).
    ///
    /// Rebuilds a document with the closed buffer text, the saved baseline it had
    /// at close (so its [`dirty`](Self::dirty) state is reconstructed faithfully —
    /// a reopened-but-unsaved buffer comes back dirty), the carried original-on-load
    /// byte-fidelity profile (so a subsequent Save stays byte-preserving), and the
    /// restored cursor. Derived parse/highlight/diagnostic state starts empty; the
    /// caller requests a fresh parse after reopen. The edit generation is reset to
    /// the baseline so that fresh parse is requested exactly once.
    #[must_use]
    pub fn from_restorable(
        path: Option<PathBuf>,
        buffer: String,
        last_saved: SavedSnapshot,
        byte_profile: ByteFidelityProfile,
        cursor: CursorState,
        untitled_seq: Option<u32>,
    ) -> Self {
        Self {
            id: mint_doc_id(),
            buffer,
            path,
            last_saved,
            byte_profile,
            cursor,
            parse: None,
            highlight: None,
            diagnostics: Vec::new(),
            untitled_seq,
            edit_generation: 0,
            last_requested_generation: 0,
            pending_cursor_jump: None,
        }
    }

    /// The process-unique identity token for this document (FR-026).
    ///
    /// Stable for the document's lifetime; used to track a specific tab across
    /// index shifts during batch close/quit operations.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// `true` when the buffer differs from the last saved/loaded snapshot.
    #[must_use]
    pub fn dirty(&self) -> bool {
        !self.last_saved.matches(&self.buffer)
    }

    /// Re-baseline the saved snapshot to the current buffer (call after a save).
    pub fn mark_saved(&mut self) {
        self.last_saved = SavedSnapshot::of(&self.buffer);
    }

    /// `true` when the buffer is strictly larger than `threshold` bytes.
    ///
    /// The comparison is **strict** greater-than: a buffer whose length exactly
    /// equals the threshold is *not* oversize (boundary owned by FR for the
    /// large-file warning).
    #[must_use]
    pub fn oversize(&self, threshold: u64) -> bool {
        self.buffer.len() as u64 > threshold
    }

    /// The display title: the file name when saved, else a stable `Untitled-N`
    /// placeholder built from the workspace-assigned sequence number.
    #[must_use]
    pub fn title(&self) -> String {
        if let Some(path) = &self.path {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                return name.to_string();
            }
        }
        match self.untitled_seq {
            Some(n) => format!("Untitled-{n}"),
            // A document with neither a path nor a sequence number is degenerate;
            // fall back to a stable label rather than panicking.
            None => "Untitled".to_string(),
        }
    }

    /// The current edit generation (monotonic; bumped by [`on_edit`](Self::on_edit)).
    #[must_use]
    pub fn edit_generation(&self) -> u64 {
        self.edit_generation
    }

    /// Record that the buffer was mutated (FR-006).
    ///
    /// Bumps the monotonic [`edit_generation`](Self::edit_generation) so the next
    /// [`request_reparse`](Self::request_reparse) ships the latest text and any
    /// in-flight stale result is later discarded by generation comparison. This is
    /// the *only* per-frame edit hook; it never calls `ron_core::parse` directly.
    pub fn on_edit(&mut self) {
        self.edit_generation = self.edit_generation.wrapping_add(1);
    }

    /// Queue an off-thread reparse of the current buffer, coalesced (FR-006).
    ///
    /// Sends `(edit_generation, buffer.clone())` to the worker only when the edit
    /// generation has advanced past the last requested one — so a burst of
    /// keystrokes collapses to a single request for the newest text (only the
    /// latest generation matters). The per-frame UI path never parses inline; all
    /// parsing happens on the worker thread.
    pub fn request_reparse(&mut self, worker: &ReparseWorker) {
        if self.edit_generation == self.last_requested_generation {
            return;
        }
        self.last_requested_generation = self.edit_generation;
        worker.request(self.edit_generation, self.buffer.clone());
    }

    /// Drain finished reparse results from `worker` and install the current one
    /// (FR-006, FR-019).
    ///
    /// Discards stale results (generation older than the current edit) and only
    /// acts on a result matching the live [`edit_generation`](Self::edit_generation).
    /// When a current result lands it (1) becomes the installed [`parse`](Self::parse),
    /// (2) refreshes [`diagnostics`](Self::diagnostics) via [`map_diagnostic`], and
    /// (3) recomputes the [`highlight`](Self::highlight) model from the CST. Old
    /// diagnostics and highlights are kept until a fresh result lands — never
    /// cleared on edit. Returns `true` if a result was installed (so the caller can
    /// repaint).
    pub fn poll_parse(&mut self, worker: &ReparseWorker) -> bool {
        let mut installed = false;
        // Drain everything queued this frame; keep only the latest *current* one.
        while let Some(result) = worker.poll() {
            // Stale: an edit happened after this parse was requested. Discard it
            // but keep the last-good parse/diagnostics/highlight intact.
            if result.generation != self.edit_generation {
                continue;
            }
            let generation = result.generation;
            self.diagnostics = result
                .diagnostics
                .iter()
                .map(|d| map_diagnostic(d, &self.buffer))
                .collect();
            self.highlight = Some(build_highlight_model(&result, generation));
            self.parse = Some(result);
            installed = true;
        }
        installed
    }

    /// The number of Unicode scalar values (characters) in the buffer.
    ///
    /// Cursor jumps and the editor's `TextEdit` cursor work in **character**
    /// offsets, so this is the inclusive upper bound for a valid caret position.
    #[must_use]
    pub fn char_len(&self) -> usize {
        self.buffer.chars().count()
    }

    /// Request the editor to move its caret to `char_offset` on the next frame
    /// (FR-009).
    ///
    /// Used by the Problems panel's click-to-navigate. The offset is stored as-is
    /// and clamped to the live buffer only when consumed by
    /// [`take_cursor_jump`](Self::take_cursor_jump), so a range that became stale
    /// after edits still resolves to the nearest valid caret position (best-effort,
    /// self-correcting once a fresh parse lands).
    pub fn request_cursor_jump(&mut self, char_offset: usize) {
        self.pending_cursor_jump = Some(char_offset);
    }

    /// Take the pending caret jump, if any, clamped to `[0, char_len]` (FR-009).
    ///
    /// Returns `None` when no jump is pending. A pending offset beyond the current
    /// buffer length is clamped down to the buffer's character length so a stale
    /// diagnostic range can never move the caret out of bounds. Consuming clears
    /// the pending jump so it applies exactly once.
    #[must_use]
    pub fn take_cursor_jump(&mut self) -> Option<usize> {
        self.pending_cursor_jump
            .take()
            .map(|offset| offset.min(self.char_len()))
    }

    /// `true` when a caret jump is queued for the next frame (for tests/hosts).
    #[must_use]
    pub fn has_pending_cursor_jump(&self) -> bool {
        self.pending_cursor_jump.is_some()
    }
}
