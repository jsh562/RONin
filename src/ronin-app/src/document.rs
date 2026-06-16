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
//! # E007 — Autosave / crash recovery (OBJ2)
//!
//! The document carries an autosave/recovery lifecycle hook
//! ([`EditorDocument::recovery`], an [`AutosaveDebounce`](crate::recovery::AutosaveDebounce))
//! that the shell drives off the per-frame path: it debounces a recovery-sidecar
//! write while the buffer is dirty + actually changed, and the shell removes the
//! sidecar on a clean save / clean exit. An **untitled** buffer (no `path`) has
//! **no** sidecar (TR-017).
//!
//! # E007 — Bounded CST-backed undo/redo (OBJ3)
//!
//! The document owns a WASM-clean [`UndoStack`](ron_core::UndoStack) keyed to its
//! [`CursorState`] ([`EditorDocument::undo`]). The shell records a snapshot at
//! coalesce-unit boundaries off the per-frame hot path
//! ([`EditorDocument::record_undo_snapshot`]) — at most once per coalesce window,
//! not per keystroke (TR-016/TR-023, SC-008) — and [`EditorDocument::undo`] /
//! [`EditorDocument::redo`] restore the **exact prior in-memory bytes** + cursor
//! by replacing the buffer with the entry's `source_text` and bumping
//! `edit_generation` so a reparse runs and dirty-tracking recomputes. Undo/redo
//! operate **solely** on the in-memory buffer/CST/cursor and never read or write
//! the file (TR-018). The coalesce *timing* decision is computed here
//! (`Instant` elapsed since the last edit vs the configured window) and passed to
//! `ron-core` as a plain `bool`, keeping `ron-core` clock-free (TR-014).
//!
//! The dirty-tracking and `edit_generation` machinery here is the seam both OBJ2
//! and OBJ3 build on.

use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use ron_core::transform::{apply_structural, StructuralOp, TransformOutcome};
use ron_core::{BlockedReason, UndoEntry, UndoStack};

use crate::bevy::mode::{Mode, ModeState};
use crate::binding::{BindingOrigin, BindingState, DocumentOverride, TypeBinding};
use crate::completion::CompletionState;
use crate::diagnostics_map::{map_diagnostic, map_scene_diagnostic, DiagnosticView};
use crate::editor_view::build_highlight_model;
use crate::reparse::{BoundScene, BoundType, BoundValidation, ParseResult, ReparseWorker};
use crate::structural::projection::{derive_projection, DerivedProjection};
use crate::structural::view_state::ViewSelectionAndFocus;

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
    /// The structural-autocomplete popup state for this document (E005 Wave 3).
    ///
    /// Cross-frame state for the custom completion popup `editor_view` renders over
    /// the editor: open/closed, the candidate items, the explicitly-highlighted
    /// index (never preselected), and the trigger offset. Recomputed from the live
    /// buffer + caret each frame; default is closed.
    pub completion: CompletionState,
    /// The live snippet tab-stop navigation session, when one is in progress (E005
    /// Wave 4, FR-016).
    ///
    /// Set when a snippet is inserted (the buffer is spliced and the caret jumps to
    /// the first tab-stop); `editor_view` drives `Tab`/`Shift+Tab` over it and clears
    /// it once navigation ends (`$0` reached, `Esc`, or the buffer edited out from
    /// under it). `None` when no snippet navigation is active.
    pub snippet_session: Option<crate::snippets::SnippetSession>,
    /// The type this document is currently bound to, when any (E006/FR-006).
    ///
    /// Passed to the off-frame [`ReparseWorker`] on each
    /// [`request_reparse`](Self::request_reparse) so type validation runs against
    /// it on the worker thread. `None` means no binding resolved — only structural
    /// diagnostics are produced (FR-015). The real `BindingConfig`→`BoundType`
    /// resolution that populates this is Phase 4 (US2); for now it defaults to
    /// `None` and is the seam the binding resolver / per-document override will set.
    ///
    /// This carries the **serde-mode** bound type only (E006); the Bevy-mode bound
    /// registry travels separately on [`mode_state`](Self::mode_state) (E009). The
    /// two are mutually exclusive per document (FR-013): [`bound_validation`](Self::bound_validation)
    /// picks exactly one per the active [`Mode`].
    pub bound_type: Option<BoundType>,
    /// The per-document Bevy mode state (E009 — FR-006/009/012/013).
    ///
    /// Held **1:1 per document** so different open documents may simultaneously be
    /// in different modes bound to different registries — there is **no global mode
    /// state**, which is exactly what guarantees per-document coexistence (FR-012).
    /// Records the active `{Serde, Bevy}` mode (extension auto-detect + explicit
    /// override), the resolved bound registry binding, and (after
    /// [`ModeState::load_registry`](crate::bevy::mode::ModeState::load_registry)) the
    /// loaded registry + its serialized interchange model. When the active mode is
    /// [`Mode::Bevy`] with a loaded registry, [`bound_validation`](Self::bound_validation)
    /// selects the scene validator (which **replaces** the serde source, AD-003);
    /// otherwise the serde path runs. Default is [`Mode::Serde`], no registry, which
    /// leaves the serde path byte-for-byte unchanged. Transient/session-only (never
    /// persisted); the shell resolves it from the project `RegistryBindingConfig` +
    /// extension auto-detect + any explicit override.
    pub mode_state: ModeState,
    /// The resolved active binding for this document, for **display** (E006 US2 —
    /// FR-011).
    ///
    /// This is the user-facing answer to "which type, from which source, does this
    /// document conform to?" — or [`BindingState::NoBinding`]. It is recomputed by
    /// the shell's binding-resolution step (`App::apply_binding_to_active`) whenever
    /// the document becomes active / is opened or its override / the project config
    /// changes, then surfaced via [`binding_label`](Self::binding_label). It is
    /// kept *separate* from [`bound_type`](Self::bound_type) (which the worker runs
    /// against): `binding` is always meaningful for the UI even when acquisition
    /// degrades to structural-only (so the indicator shows the *intended* type while
    /// `bound_type` stays `None`). Defaults to [`TypeBinding::none`].
    pub binding: TypeBinding,
    /// The per-document **session** override, when the user has explicitly bound
    /// this document to a chosen type + source (E006 US2 — FR-009).
    ///
    /// When set it takes precedence over any project [`BindingConfig`](crate::binding::BindingConfig)
    /// rule (override > config) and produces a [`BindingOrigin::Override`] binding.
    /// Never persisted — only the project config persists. `None` means the document
    /// falls back to config resolution (or no binding). Set/cleared via the shell's
    /// override control, which then re-applies the binding so it takes effect
    /// immediately.
    pub override_: Option<DocumentOverride>,
    /// Whether type validation is currently degraded for this document because it is
    /// **oversize** past E003's large-file threshold (E006 T040 — FR-015/FR-024).
    ///
    /// This mirrors, for type validation, exactly what E003 already does for
    /// highlighting / squiggles: past
    /// [`AppSettings::effective_large_file_threshold`](crate::settings::AppSettings::effective_large_file_threshold)
    /// the always-on intelligence degrades. When `true`,
    /// [`request_reparse`](Self::request_reparse) ships **no** bound type to the
    /// worker, so the worker produces zero type diagnostics (FR-015's structural-only
    /// behavior) — the document still parses structurally, identical to how an
    /// oversize document still parses but renders no squiggles/highlights. The flag
    /// is reconciled against the *live* buffer size every frame by the shell's
    /// document pump (`App::reconcile_validation_degrade`), so editing an oversize
    /// document back down below the threshold automatically resumes validation on the
    /// next reparse. It is purely derived (never persisted, never user-set).
    pub validation_suppressed: bool,
    /// The autosave / crash-recovery lifecycle hook for this document (E007 OBJ2 —
    /// TR-006..009/TR-016).
    ///
    /// A frame-driven, deterministic [`AutosaveDebounce`](crate::recovery::AutosaveDebounce):
    /// the shell calls [`note_change`](Self::note_change) when the buffer changes and
    /// [`should_autosave`](Self::should_autosave) each frame; when it fires, the shell
    /// hands a [`RecoverySidecar`](crate::recovery::RecoverySidecar) snapshot to the
    /// off-frame [`AutosaveWorker`](crate::recovery::AutosaveWorker) and then calls
    /// [`mark_autosaved`](Self::mark_autosaved). An untitled buffer (no `path`) is
    /// never autosaved (TR-017). Not persisted; rebuilt per session from settings.
    recovery: crate::recovery::AutosaveDebounce,
    /// The bounded, WASM-clean CST-backed undo/redo history for this document
    /// (E007 OBJ3 — TR-010..014/TR-018/TR-024/TR-027).
    ///
    /// Keyed to the document's [`CursorState`]; each [`UndoEntry`] snapshots the
    /// exact buffer bytes + CST + cursor at a coalesce-unit boundary. The shell
    /// records snapshots off the per-frame path via
    /// [`record_undo_snapshot`](Self::record_undo_snapshot) and drives
    /// [`undo`](Self::undo) / [`redo`](Self::redo), which restore the exact prior
    /// **in-memory** bytes (never the on-disk file, TR-018). Constructed with the
    /// default config; the shell syncs the live cap + coalesce window each frame
    /// via [`set_undo_config`](Self::set_undo_config). In-memory / session-scoped:
    /// never persisted (no revision log; Scope/Excluded).
    undo: UndoStack<CursorState>,
    /// The edit generation the last undo snapshot was recorded for (E007 OBJ3).
    ///
    /// Coalesces undo bookkeeping off the per-keystroke path: a snapshot is taken
    /// only when the live [`edit_generation`](Self::edit_generation) has advanced
    /// past this, so a burst of edits collapses to the latest text — the same
    /// generation-keyed pattern the reparse/autosave seams use. `None` until the
    /// initial state is seeded by [`seed_undo`](Self::seed_undo).
    last_undo_generation: Option<u64>,
    /// The instant the last undo snapshot was recorded (E007 OBJ3 — TR-027).
    ///
    /// The caller-side coalesce timing source (`ron-core` measures no clock,
    /// TR-014): [`record_undo_snapshot`](Self::record_undo_snapshot) compares the
    /// elapsed time since this against the configured coalesce window to decide
    /// whether the new edit extends the current undo unit or starts a new one.
    /// `None` until the first snapshot is recorded.
    last_undo_instant: Option<Instant>,
    /// The coalesce window the undo stack was configured with, as a `Duration`
    /// (E007 OBJ3 — TR-027). Kept here so the caller-side coalesce decision uses
    /// the same (clamped) window the stack carries; synced by
    /// [`set_undo_config`](Self::set_undo_config).
    undo_coalesce_window: std::time::Duration,
    /// The per-document structural view selection + edit focus + section overrides
    /// + stale marker (E008 — FR-015/FR-016/FR-017).
    ///
    /// Transient/session-only (never persisted). Default-constructed to the
    /// structural (tree/form) view on open (FR-017); focus is keyed to a
    /// [`StructuralPath`](crate::structural::view_state::StructuralPath) identity so
    /// it survives an off-frame reparse and a virtualization scroll, or drops
    /// gracefully when its node vanishes (FR-016).
    view_state: ViewSelectionAndFocus,
    /// The shared structural projection of the document's top-level value (E008 —
    /// AD-003/FR-015/FR-026), or `None` until the first reparse lands.
    ///
    /// Re-derived **once per landed reparse** off the per-frame path
    /// ([`poll_parse`](Self::poll_parse)), never per keystroke or per frame; a read
    /// projection over the CST that changes zero bytes (FR-020). The per-view models
    /// (tree/table) realize lazily on top of it in US1/US2.
    projection: Option<DerivedProjection>,
    /// The last structural-op inline error message, when one is showing (E008 —
    /// FR-003), or `None`.
    ///
    /// Set when a tree/table op returns [`BlockedReason`] (e.g. a rename collision)
    /// so the structural view can surface it inline using the same field/cell
    /// indicator model as the diagnostics surfacing (FR-003/FR-018). A blocked op
    /// changes no bytes and pushes no undo entry, so this is the only state it
    /// touches. Cleared on the next successful op. Transient/session-only.
    tree_error: Option<String>,
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
        let mut doc = Self {
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
            completion: CompletionState::new(),
            snippet_session: None,
            bound_type: None,
            mode_state: ModeState::default(),
            binding: TypeBinding::none(),
            override_: None,
            validation_suppressed: false,
            recovery: crate::recovery::AutosaveDebounce::new(
                crate::settings::AutosaveConfig::default(),
            ),
            undo: UndoStack::new(),
            last_undo_generation: None,
            last_undo_instant: None,
            undo_coalesce_window: ron_core::undo::DEFAULT_COALESCE_WINDOW,
            view_state: ViewSelectionAndFocus::new(),
            projection: None,
            tree_error: None,
        };
        // Seed the undo baseline at the loaded (generation-0) state so the first
        // edit's snapshot pushes the original as the first undo boundary (TR-010).
        doc.seed_undo();
        Ok(doc)
    }

    /// Create a fresh, empty untitled document with a workspace-assigned
    /// sequence number used for its `Untitled-N` title.
    #[must_use]
    pub fn new_untitled(seq: u32) -> Self {
        let buffer = String::new();
        let last_saved = SavedSnapshot::of(&buffer);
        let mut doc = Self {
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
            completion: CompletionState::new(),
            snippet_session: None,
            bound_type: None,
            mode_state: ModeState::default(),
            binding: TypeBinding::none(),
            override_: None,
            validation_suppressed: false,
            recovery: crate::recovery::AutosaveDebounce::new(
                crate::settings::AutosaveConfig::default(),
            ),
            undo: UndoStack::new(),
            last_undo_generation: None,
            last_undo_instant: None,
            undo_coalesce_window: ron_core::undo::DEFAULT_COALESCE_WINDOW,
            view_state: ViewSelectionAndFocus::new(),
            projection: None,
            tree_error: None,
        };
        // Seed the undo baseline at the empty (generation-0) state.
        doc.seed_undo();
        doc
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
        let mut doc = Self {
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
            completion: CompletionState::new(),
            snippet_session: None,
            bound_type: None,
            mode_state: ModeState::default(),
            binding: TypeBinding::none(),
            override_: None,
            validation_suppressed: false,
            recovery: crate::recovery::AutosaveDebounce::new(
                crate::settings::AutosaveConfig::default(),
            ),
            undo: UndoStack::new(),
            last_undo_generation: None,
            last_undo_instant: None,
            undo_coalesce_window: ron_core::undo::DEFAULT_COALESCE_WINDOW,
            view_state: ViewSelectionAndFocus::new(),
            projection: None,
            tree_error: None,
        };
        // Seed the undo baseline at the restored (generation-0) state so the first
        // post-reopen edit pushes the restored content as the first undo boundary.
        doc.seed_undo();
        doc
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

    // --- E007 OBJ2: autosave / crash-recovery lifecycle hooks ----------------

    /// Sync the autosave debounce with the live [`AutosaveConfig`](crate::settings::AutosaveConfig)
    /// (E007 TR-025/TR-026).
    ///
    /// The document is constructed with the default config; the shell calls this so
    /// the debounce honours the user's persisted (and clamped) idle interval /
    /// edit-count threshold. Cheap; safe to call every frame.
    pub fn set_autosave_config(&mut self, config: crate::settings::AutosaveConfig) {
        self.recovery.set_config(config);
    }

    /// Record that the buffer changed at `now`, keyed on the current
    /// [`edit_generation`](Self::edit_generation) (E007 TR-006).
    ///
    /// Idempotent per generation, so the shell may call it every frame: only a
    /// genuinely new generation resets the idle timer and advances the edit-count
    /// accumulator. This is the *only-when-changed* signal the debounce gates on.
    pub fn note_change(&mut self, now: std::time::Instant) {
        self.recovery.note_change(self.edit_generation, now);
    }

    /// The cheap per-frame check: should this document autosave its sidecar at `now`
    /// (E007 TR-006/TR-016)?
    ///
    /// Returns `true` only when the buffer changed since the last sidecar write AND
    /// an autosave trigger binds (idle OR edit-count). An **untitled** buffer (no
    /// `path`) is never autosaved (TR-017), so this is always `false` for it. Performs
    /// no I/O; the caller hands a snapshot to the off-frame writer when it fires.
    #[must_use]
    pub fn should_autosave(&self, now: std::time::Instant) -> bool {
        self.path.is_some() && self.recovery.poll(now)
    }

    /// The deterministic test/force hook (E007 TR-020): `true` when there is a
    /// changed buffer to autosave for a titled document, bypassing the thresholds.
    ///
    /// Honours the only-when-changed gate and the untitled-no-sidecar rule, so a
    /// forced tick on an unchanged or untitled document still writes nothing.
    #[must_use]
    pub fn force_autosave_tick(&self) -> bool {
        self.path.is_some() && self.recovery.force_tick()
    }

    /// Build the [`RecoverySidecar`](crate::recovery::RecoverySidecar) snapshot for
    /// this document, or `None` for an untitled buffer (no `path` → no sidecar,
    /// TR-017).
    ///
    /// Captures the live buffer + fidelity profile against the document's `path`. The
    /// shell hands this to the off-frame [`AutosaveWorker`](crate::recovery::AutosaveWorker).
    #[must_use]
    pub fn recovery_snapshot(&self) -> Option<crate::recovery::RecoverySidecar> {
        let path = self.path.clone()?;
        Some(crate::recovery::RecoverySidecar::new(
            path,
            self.buffer.clone(),
            &self.byte_profile,
        ))
    }

    /// Mark that a sidecar write for the current generation has been dispatched
    /// (E007 TR-006).
    ///
    /// Resets the debounce's edit-count accumulator and records the written
    /// generation so the next [`should_autosave`](Self::should_autosave) only fires
    /// after a *new* change — one write per debounce window, never per keystroke
    /// (SC-010).
    pub fn mark_autosaved(&mut self) {
        self.recovery.mark_written();
    }

    // --- E007 OBJ3: bounded CST-backed undo/redo (TR-010..014/018/024/027) ----

    /// Sync the undo stack with the live [`UndoConfig`](crate::settings::UndoConfig)
    /// (E007 TR-024/TR-026/TR-027).
    ///
    /// The document is constructed with the default undo config; the shell calls
    /// this so the stack honours the user's persisted (and clamped) history cap and
    /// coalesce window. Rebuilding the stack would lose history, so this updates the
    /// cap/window **in place** by reconstructing only when the config actually
    /// changed and otherwise leaving the existing history intact. Cheap; safe to
    /// call every frame.
    pub fn set_undo_config(&mut self, config: crate::settings::UndoConfig) {
        let cap = config.to_engine_cap();
        let window = config.effective_coalesce_window();
        // Only rebuild when the effective config changed, so calling this every
        // frame does not discard the accumulated history. A change in cap/window
        // is rare (settings edit), so dropping history then is acceptable and keeps
        // the bound authoritative.
        if self.undo.cap() != cap || self.undo_coalesce_window != window {
            self.undo = UndoStack::with_config(cap, window);
            self.undo_coalesce_window = window;
            // Re-seed from the current buffer so undo has a valid baseline after a
            // config-driven rebuild (no prior boundary; just the current state).
            self.last_undo_generation = None;
            self.last_undo_instant = None;
            self.seed_undo();
        }
    }

    /// Seed the undo stack with the document's current state as the baseline
    /// (E007 OBJ3 — TR-010).
    ///
    /// Records the current buffer + CST + cursor as the stack's `current` with no
    /// prior boundary (the first `record` just seeds). Idempotent per generation:
    /// only seeds when no snapshot has been recorded yet. Call once after a load /
    /// reopen / config rebuild so the first edit's snapshot has a baseline to push.
    pub fn seed_undo(&mut self) {
        if self.last_undo_generation.is_some() {
            return;
        }
        let entry = self.undo_entry_of_current();
        self.undo.record(entry, false);
        self.last_undo_generation = Some(self.edit_generation);
        // Leave `last_undo_instant` as `None` so the very first real edit starts a
        // fresh unit (no coalesce against the seed).
    }

    /// Build an [`UndoEntry`] snapshotting the document's current in-memory state.
    ///
    /// Parses the live buffer into a CST and captures the exact buffer bytes +
    /// cursor. The CST snapshot is structurally shared and cheap to retain
    /// (AD-002/ADR-0001); the parse cost is paid at most once per coalesce window
    /// (see [`record_undo_snapshot`](Self::record_undo_snapshot)), never per
    /// keystroke, so it stays off the per-frame hot path (TR-016/TR-023, SC-008).
    fn undo_entry_of_current(&self) -> UndoEntry<CursorState> {
        UndoEntry::new(
            ron_core::parse(&self.buffer),
            self.buffer.clone(),
            self.cursor,
        )
    }

    /// Record an undo snapshot for the document's current state at `now`, if the
    /// buffer advanced since the last snapshot (E007 OBJ3 — TR-010/TR-027, T035..T037).
    ///
    /// This is the **only** undo bookkeeping the shell drives, and it is coalesced
    /// off the per-keystroke path: it does work only when the live
    /// [`edit_generation`](Self::edit_generation) has advanced past the last
    /// recorded generation (a burst of edits in one frame snapshots once, the
    /// latest text). The coalesce *timing* decision is made here, caller-side
    /// (`ron-core` measures no clock, TR-014): the new edit **extends** the current
    /// undo unit when it falls within the configured coalesce window of the prior
    /// snapshot, and **starts a new unit** otherwise (TR-027). A new edit after an
    /// undo clears the redo stack inside `ron-core` (TR-012).
    ///
    /// Returns `true` when a snapshot was recorded (the buffer had advanced).
    pub fn record_undo_snapshot(&mut self, now: Instant) -> bool {
        // Seed the baseline lazily so the first edit always has a prior state.
        if self.last_undo_generation.is_none() {
            self.seed_undo();
        }
        if self.last_undo_generation == Some(self.edit_generation) {
            return false; // no new edit since the last snapshot
        }
        // Caller-side coalesce decision: within the window → extend the unit.
        let coalesce = self
            .last_undo_instant
            .is_some_and(|prev| now.duration_since(prev) < self.undo_coalesce_window);
        let entry = self.undo_entry_of_current();
        self.undo.record(entry, coalesce);
        self.last_undo_generation = Some(self.edit_generation);
        self.last_undo_instant = Some(now);
        true
    }

    /// Whether an undo step is currently available (E007 OBJ3).
    #[must_use]
    pub fn can_undo(&self) -> bool {
        self.undo.can_undo()
    }

    /// Whether a redo step is currently available (E007 OBJ3).
    #[must_use]
    pub fn can_redo(&self) -> bool {
        self.undo.can_redo()
    }

    /// Undo the last change, restoring the **exact prior in-memory bytes** + cursor
    /// (E007 OBJ3 — TR-010/TR-018, SC-005).
    ///
    /// Operates **solely** on the in-memory document: it replaces the buffer with
    /// the prior boundary's `source_text` byte-for-byte (no reflow), restores the
    /// cursor, and bumps [`edit_generation`](Self::edit_generation) so a reparse
    /// runs and dirty-tracking recomputes against the restored bytes. It NEVER
    /// reads or writes the on-disk file — undo of a buffer whose file changed on
    /// disk still restores the in-memory prior bytes (TR-018). Before stepping it
    /// flushes any open coalescing run by recording a pending snapshot, so the run
    /// in progress is itself undoable. Returns `true` when a step was taken.
    pub fn undo(&mut self, now: Instant) -> bool {
        // Flush any pending coalesced edits into the stack first so the current
        // run is a recoverable boundary before we step back.
        self.record_undo_snapshot(now);
        let Some(entry) = self.undo.undo() else {
            return false;
        };
        self.apply_restored(&entry);
        true
    }

    /// Redo the last undone change, replaying its exact bytes + cursor (E007 OBJ3 —
    /// TR-010/TR-018, SC-005).
    ///
    /// The inverse of [`undo`](Self::undo): replaces the buffer with the replayed
    /// state's exact bytes, restores the cursor, and bumps the edit generation so a
    /// reparse runs. In-memory only; never touches the file (TR-018). Returns `true`
    /// when a step was taken.
    pub fn redo(&mut self) -> bool {
        let Some(entry) = self.undo.redo() else {
            return false;
        };
        self.apply_restored(&entry);
        true
    }

    /// Apply a restored undo/redo entry to the live in-memory document state.
    ///
    /// Replaces the buffer with the entry's exact bytes, restores the cursor, and
    /// bumps the edit generation so the off-frame reparse re-runs against the
    /// restored text and dirty-tracking recomputes. The undo bookkeeping
    /// generation is synced to the post-restore generation so the restore itself is
    /// not re-snapshotted as a fresh edit (the stack already tracks it as
    /// `current`). The restore is byte-faithful — no normalization (TR-010).
    fn apply_restored(&mut self, entry: &UndoEntry<CursorState>) {
        self.buffer = entry.source_text().to_string();
        self.cursor = *entry.cursor();
        self.on_edit();
        // The restored state is already the stack's `current`; mark it recorded so
        // the next `record_undo_snapshot` does not push it back as a new edit.
        self.last_undo_generation = Some(self.edit_generation);
        // A restore is a discrete action: start a fresh coalesce unit after it by
        // clearing the timing anchor (the next real edit will not coalesce into it).
        self.last_undo_instant = None;
    }

    /// The number of committed undo boundaries currently retained (E007 OBJ3; for
    /// tests / host integration).
    #[must_use]
    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }

    /// The total retained undo+redo snapshot byte-size (E007 OBJ3 — SC-009; for
    /// tests asserting the bound is independent of file size).
    #[must_use]
    pub fn undo_total_bytes(&self) -> usize {
        self.undo.total_bytes()
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
    ///
    /// It also marks the structural projection **stale** (E008 — FR-015): an edit
    /// has been requested but the off-frame reparse that re-derives the projection
    /// has not yet landed, so the structural views show a stale marker rather than
    /// inconsistent state. The marker is cleared when a current reparse lands and
    /// the projection is re-derived ([`poll_parse`](Self::poll_parse)).
    pub fn on_edit(&mut self) {
        self.edit_generation = self.edit_generation.wrapping_add(1);
        // E008 FR-015: an edit is requested but the projection's reparse has not
        // landed — mark stale until `poll_parse` re-derives against the new CST.
        self.view_state.mark_stale();
    }

    /// Queue an off-thread reparse of the current buffer, coalesced (FR-006).
    ///
    /// Sends `(edit_generation, buffer.clone())` to the worker only when the edit
    /// generation has advanced past the last requested one — so a burst of
    /// keystrokes collapses to a single request for the newest text (only the
    /// latest generation matters). The per-frame UI path never parses inline; all
    /// parsing happens on the worker thread.
    ///
    /// Type validation degrades on E003's oversize signal exactly like highlighting
    /// and squiggles (E006 T040 — FR-015/FR-024): when
    /// [`validation_suppressed`](Self::validation_suppressed) is set (the document is
    /// oversize), **no** bound type is shipped to the worker, so it produces zero type
    /// diagnostics (structural-only, FR-015). The structural parse still runs, mirroring
    /// how an oversize document still parses but renders no squiggles. The shell
    /// reconciles the flag against the live buffer size every frame, so editing the
    /// document back below the threshold resumes validation on the next reparse.
    pub fn request_reparse(&mut self, worker: &ReparseWorker) {
        if self.edit_generation == self.last_requested_generation {
            return;
        }
        self.last_requested_generation = self.edit_generation;
        // Carry the active binding so the worker validates against it off-frame
        // (E006/FR-006, E009/FR-013). Cloning is cheap — the `TypeModel` / registry
        // are behind `Arc`s. When validation is degraded for an oversize document,
        // ship `None` so the worker runs structural-only (no type/scene diagnostics),
        // consistent with E003 disabling highlighting/squiggles past the same
        // threshold (T040).
        let bound = if self.validation_suppressed {
            None
        } else {
            self.bound_validation()
        };
        worker.request(self.edit_generation, self.buffer.clone(), bound);
    }

    /// Select the single [`BoundValidation`] for this document under its active
    /// mode — the one place serde-vs-Bevy validation is chosen (E009/FR-013).
    ///
    /// **Exactly one source per document** (mutually exclusive, FR-013):
    ///
    /// * [`Mode::Bevy`] with a **loaded** registry → [`BoundValidation::Bevy`]: the
    ///   bound registry **replaces** the active source (AD-003), so the worker runs
    ///   the multi-subtree scene validator (`validate_scene`) against the registry +
    ///   its serialized interchange model (both acquired once at registry-load time
    ///   and shared by `Arc`). The serde [`bound_type`](Self::bound_type) is ignored
    ///   in this branch — Bevy mode does not compose with the E006 binding.
    /// * **otherwise** → the serde path: [`BoundValidation::Serde`] wrapping the
    ///   E006 [`bound_type`](Self::bound_type) when one resolved, else `None`. This
    ///   includes Bevy mode with **no** loaded registry (NoRegistry): the scene's
    ///   structural-only / no-registry hint is produced by the validator only when a
    ///   registry is present, so a registry-less Bevy document degrades to
    ///   structural-only here exactly like an unbound serde document (FR-006/FR-015).
    ///
    /// Returns `None` when neither a registry nor a serde type is bound (the
    /// structural-only state). The serde branch is byte-for-byte the prior behavior
    /// (`self.bound_type.clone()` mapped through `Serde`).
    #[must_use]
    fn bound_validation(&self) -> Option<BoundValidation> {
        if self.mode_state.active_mode() == Mode::Bevy {
            // Bevy mode REPLACES the active source with the bound registry, but only
            // when one is actually loaded; a NoRegistry Bevy document falls through
            // to structural-only (the serde branch yields its `bound_type`, which is
            // `None` for a Bevy document — never the E006 type, FR-013).
            if let (Some(model), Some(registry)) =
                (self.mode_state.registry_model(), self.mode_state.registry())
            {
                return Some(BoundValidation::Bevy(BoundScene {
                    model: Arc::clone(model),
                    registry: Arc::new(registry.clone()),
                    expected_version: self.mode_state.expected_bevy_version().map(str::to_owned),
                }));
            }
        }
        // Serde mode (or Bevy-with-no-registry): the E006 bound type, unchanged.
        self.bound_type.clone().map(BoundValidation::Serde)
    }

    /// Drain finished reparse results from `worker` and install the current one
    /// (FR-006, FR-019).
    ///
    /// Discards stale results (generation older than the current edit) and only
    /// acts on a result matching the live [`edit_generation`](Self::edit_generation).
    /// When a current result lands it (1) becomes the installed [`parse`](Self::parse),
    /// (2) rebuilds [`diagnostics`](Self::diagnostics) from BOTH the structural and
    /// type sets via [`merge_type_diagnostics`], and (3) recomputes the
    /// [`highlight`](Self::highlight) model from the CST. Old diagnostics and
    /// highlights are kept until a fresh result lands — never cleared on edit.
    /// Returns `true` if a result was installed (so the caller can repaint).
    ///
    /// The type set is **replaced** wholesale on each landed result while the
    /// structural set is recomputed and preserved (replace-not-merge for the type
    /// set, FR-006). Overlap dedup between the two sets is a later task (T031) — for
    /// now the merged view is simply structural-then-type concatenation.
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
            self.diagnostics = merge_type_diagnostics(&result, &self.buffer);
            self.highlight = Some(build_highlight_model(&result, generation));
            // E008 (AD-003/FR-015/FR-026): re-derive the shared structural
            // projection ONCE per landed current reparse, off the per-frame path.
            // This is a single read pass over the landed CST (zero bytes, FR-020).
            self.projection = Some(derive_projection(&result.cst));
            // E008 (T013/FR-016/FR-027): re-resolve edit focus + section overrides
            // against the fresh CST by structural-path identity. If the focused
            // node still resolves, focus is kept; if it vanished, edit mode is
            // dropped gracefully (never edit the wrong node). Cost is proportional
            // to path depth, not row/node count.
            self.view_state.reresolve(&result.cst.root());
            // The current projection now matches the landed CST — clear stale.
            self.view_state.clear_stale();
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

    /// A short, human-readable label for the active binding, for the status
    /// indicator and tests (E006 US2 — FR-011).
    ///
    /// * [`BindingState::Bound`] → `Type: <name> (<origin>)` where `<origin>` is
    ///   `override` or `config`, e.g. `Type: Entity (config)` /
    ///   `Type: Entity (override)`.
    /// * [`BindingState::NoBinding`] → `no type bound`.
    ///
    /// The source locator is *not* in this short label (it can be a long path); the
    /// UI shows the source separately (see [`binding_source_label`](Self::binding_source_label)).
    /// When multiple config patterns matched, [`binding`](Self::binding) already
    /// holds the resolved (most-specific) one, so this reflects that single chosen
    /// binding (FR-011).
    #[must_use]
    pub fn binding_label(&self) -> String {
        match &self.binding.state {
            BindingState::Bound {
                type_name, origin, ..
            } => {
                let origin = match origin {
                    BindingOrigin::Override => "override",
                    BindingOrigin::Config => "config",
                };
                format!("Type: {type_name} ({origin})")
            }
            BindingState::NoBinding => "no type bound".to_string(),
        }
    }

    /// The bound type's source locator as a display string, or `None` when
    /// [`BindingState::NoBinding`] (E006 US2 — FR-011).
    ///
    /// Prefixes the path with its source kind so the user can tell a Rust source
    /// from a schema file, e.g. `schema: schemas/app.json` /
    /// `rust: src/scene.rs`. The full source locator is surfaced alongside the
    /// short [`binding_label`](Self::binding_label) so the active binding is fully
    /// visible (FR-011, data-model "active binding visible").
    #[must_use]
    pub fn binding_source_label(&self) -> Option<String> {
        match &self.binding.state {
            BindingState::Bound { type_source, .. } => {
                let (kind, path) = match type_source {
                    crate::binding::TypeSourceLocator::RustSource(p) => ("rust", p),
                    crate::binding::TypeSourceLocator::SchemaFile(p) => ("schema", p),
                };
                Some(format!("{kind}: {}", path.display()))
            }
            BindingState::NoBinding => None,
        }
    }

    // --- E009: per-document Bevy mode state ------------------------------------

    /// The per-document Bevy mode state (E009 — FR-009/012/013), read-only.
    ///
    /// The shell's active-mode/registry indicator and the mode-switch / registry
    /// controls read this; it is held 1:1 per document so two open documents may be
    /// in different modes bound to different registries simultaneously (FR-012).
    #[must_use]
    pub fn mode_state(&self) -> &ModeState {
        &self.mode_state
    }

    /// The per-document Bevy mode state mutably (E009 — FR-009/012).
    ///
    /// The shell drives mode toggling
    /// ([`set_mode_override`](crate::bevy::mode::ModeState::set_mode_override)) and
    /// registry loading
    /// ([`load_registry`](crate::bevy::mode::ModeState::load_registry)) through this.
    /// Mutating mode state changes **zero** document bytes (FR-011): mode is a
    /// behavior selection, not an edit. The caller re-validates by requesting an
    /// off-frame reparse after a mode/registry change (see
    /// [`revalidate`](Self::revalidate)).
    #[must_use]
    pub fn mode_state_mut(&mut self) -> &mut ModeState {
        &mut self.mode_state
    }

    /// Replace this document's [`ModeState`] wholesale (E009 — FR-009/012/013).
    ///
    /// Used by the shell after it resolves the document's mode + registry binding
    /// from the project `RegistryBindingConfig` + extension auto-detect + any
    /// explicit override (and loads the registry). Per-document: setting one
    /// document's mode never touches another's (no global state, FR-012). Changes
    /// **zero** document bytes — the caller re-validates via [`revalidate`](Self::revalidate).
    pub fn set_mode_state(&mut self, mode_state: ModeState) {
        self.mode_state = mode_state;
    }

    /// `true` iff this document is currently in [`Mode::Bevy`] (E009 — FR-013).
    #[must_use]
    pub fn is_bevy_mode(&self) -> bool {
        self.mode_state.active_mode() == Mode::Bevy
    }

    /// A short, human-readable label for the **active mode** + its origin, for the
    /// always-visible mode indicator and tests (E009 — FR-011).
    ///
    /// `Mode: serde` / `Mode: bevy` with an `(auto)` / `(override)` suffix
    /// distinguishing extension auto-detection from an explicit per-document toggle
    /// (the [`ModeOrigin`](crate::bevy::mode::ModeOrigin)). The mode is a behavior
    /// selection only — reading or rendering it changes zero document bytes (FR-011).
    #[must_use]
    pub fn mode_label(&self) -> String {
        let origin = match self.mode_state.mode_origin() {
            crate::bevy::mode::ModeOrigin::AutoDetected => "auto",
            crate::bevy::mode::ModeOrigin::Override => "override",
        };
        format!(
            "Mode: {} ({origin})",
            self.mode_state.active_mode().as_str()
        )
    }

    /// A short, human-readable label for the **bound registry** + registry state,
    /// for the always-visible mode/registry indicator and tests (E009 — FR-011).
    ///
    /// Surfaces the bound registry export (file name) and the NoRegistry-vs-loaded
    /// distinction the diagnostics already carry (the three-state model — FR-006):
    ///
    /// * **Serde mode** → `Registry: n/a (serde mode)` — the registry is irrelevant
    ///   to a serde document (it validates via the E006 binding, FR-013).
    /// * **Bevy, no binding resolved** → `Registry: none` — no rule matched and no
    ///   override applied (the `NoRegistry` state, hint-only, FR-006/FR-010).
    /// * **Bevy, bound but not loaded** → `Registry: <name> (no registry loaded)` —
    ///   a path resolved but the export was missing/unparseable/empty, so it degraded
    ///   to `NoRegistry` (SC-002): only the "no registry loaded" hint, never an error.
    /// * **Bevy, loaded** → `Registry: <name> (loaded)` — a non-empty registry loaded
    ///   from the bound export, so scene-aware validation engages.
    ///
    /// The export's file name (not the full, possibly long path) is shown so the
    /// indicator stays compact; the full path lives in the registry-binding config
    /// window. A staleness advisory is surfaced separately via
    /// [`staleness_label`](Self::staleness_label) (FR-008/FR-011).
    #[must_use]
    pub fn registry_label(&self) -> String {
        if !self.is_bevy_mode() {
            return "Registry: n/a (serde mode)".to_string();
        }
        match self.mode_state.bound_registry_path() {
            None => "Registry: none".to_string(),
            Some(path) => {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("<registry>");
                if self.mode_state.has_registry() {
                    format!("Registry: {name} (loaded)")
                } else {
                    // A path resolved but the export degraded to NoRegistry — show the
                    // exact "no registry loaded" hint (no error, SC-002, FR-006).
                    format!("Registry: {name} (no registry loaded)")
                }
            }
        }
    }

    /// The staleness advisory label, when one is warranted, or `None` (E009 —
    /// FR-008/FR-011).
    ///
    /// Returns `Some("Stale: expected <x>, registry <y>")` only when a configured
    /// expected Bevy version disagrees with the loaded registry's apparent version
    /// ([`ModeState::is_stale`](crate::bevy::mode::ModeState::is_stale)); otherwise
    /// `None`. A version skew is an advisory, never an error.
    #[must_use]
    pub fn staleness_label(&self) -> Option<String> {
        if !self.mode_state.is_stale() {
            return None;
        }
        let expected = self.mode_state.expected_bevy_version().unwrap_or("?");
        let apparent = self.mode_state.apparent_bevy_version().unwrap_or("?");
        Some(format!("Stale: expected {expected}, registry {apparent}"))
    }

    /// Bump the edit generation and request an off-frame reparse so validation
    /// re-runs against the current mode/binding **without** changing any bytes
    /// (E009 — FR-011/FR-013).
    ///
    /// This is the re-validate primitive the shell calls after a mode toggle or a
    /// registry (re)load: switching mode is a behavior selection, not an edit, so it
    /// touches zero document bytes (FR-011) — it only forces the worker to re-run the
    /// (now possibly different) mode's validation path and republish the diagnostic
    /// set wholesale (replace-not-merge, FR-006). Mirrors how
    /// [`App::apply_binding_to_active`](crate::app::App) re-validates a serde binding
    /// change immediately rather than deferring to the next edit.
    pub fn revalidate(&mut self, worker: &ReparseWorker) {
        self.on_edit();
        self.request_reparse(worker);
    }

    // --- E008: structural view state + projection + one-undo-unit edit ---------

    /// The per-document structural view selection + edit focus + overrides + stale
    /// marker (E008 — FR-015/FR-016/FR-017), read-only.
    #[must_use]
    pub fn view_state(&self) -> &ViewSelectionAndFocus {
        &self.view_state
    }

    /// The per-document structural view state mutably (E008).
    ///
    /// The view switcher (`app.rs`) and the structural surfaces drive view
    /// switching, focus, and overrides through this. Mutating view state changes
    /// **zero** document bytes (FR-020).
    #[must_use]
    pub fn view_state_mut(&mut self) -> &mut ViewSelectionAndFocus {
        &mut self.view_state
    }

    /// The shared structural projection of the document's top-level value (E008 —
    /// AD-003/FR-015), or `None` until the first reparse lands.
    ///
    /// Re-derived once per landed reparse off the per-frame path
    /// ([`poll_parse`](Self::poll_parse)); never recomputed per frame (FR-026).
    #[must_use]
    pub fn projection(&self) -> Option<&DerivedProjection> {
        self.projection.as_ref()
    }

    /// The last structural-op inline error message, if one is showing (E008 —
    /// FR-003).
    ///
    /// Set by a tree/table op blocked at commit (e.g. a rename collision); surfaced
    /// inline by the structural view using the same indicator model as diagnostics
    /// (FR-003/FR-018). `None` when no error is pending.
    #[must_use]
    pub fn tree_error(&self) -> Option<&str> {
        self.tree_error.as_deref()
    }

    /// Set the structural-op inline error message (E008 — FR-003). The error is a
    /// byte-free notice: a blocked op never changes the document.
    pub fn set_tree_error(&mut self, message: String) {
        self.tree_error = Some(message);
    }

    /// Clear the structural-op inline error message (E008 — FR-003), e.g. after a
    /// successful op.
    pub fn clear_tree_error(&mut self) {
        self.tree_error = None;
    }

    /// Apply one structural edit as a single E007 undo unit (E008 — T012,
    /// FR-013/FR-014).
    ///
    /// The one-undo-unit structural-edit pipeline:
    ///
    /// 1. Parse the **live buffer** into a CST (the resolved source of truth the
    ///    caller's [`StructuralOp`] addresses against).
    /// 2. Call `ron-core`'s pure [`apply_structural`]; the op's [`ParentRef`]
    ///    /node addressing is `(kind + start-offset)`-based and is re-resolved
    ///    inside the transform against this exact CST.
    /// 3. On [`TransformOutcome::Applied`] install the new document text by
    ///    **printing** the new CST into the buffer (untouched regions stay
    ///    byte-for-byte — FR-013), record exactly **one** E007 undo unit (a
    ///    multi-node op like reorder is still one unit — FR-014), bump the edit
    ///    generation, and request an off-frame reparse so the projection
    ///    re-derives. Returns `Ok(())`.
    /// 4. On [`TransformOutcome::Blocked`] change **nothing** — no buffer change,
    ///    no undo entry (a no-op never pollutes the undo stack — FR-014) — and
    ///    return `Err(reason)` so the caller can surface an inline error.
    ///
    /// The single undo unit is achieved by forcing the recorded snapshot to start a
    /// **fresh** undo unit (no coalesce): the op is one logical action, distinct
    /// from a typing run, so its undo restores exactly the pre-edit bytes and its
    /// redo replays exactly the post-edit bytes (FR-014, SC-002).
    ///
    /// `now` is the caller-side clock for the undo coalesce decision (`ron-core`
    /// measures no clock — TR-014); the structural snapshot is always recorded as a
    /// non-coalescing boundary regardless of timing.
    ///
    /// [`ParentRef`]: ron_core::ParentRef
    pub fn apply_structural_edit(
        &mut self,
        op: StructuralOp,
        worker: &ReparseWorker,
        now: Instant,
    ) -> Result<(), BlockedReason> {
        // 1. The resolved CST the op addresses against is the live buffer's parse.
        let cst = ron_core::parse(&self.buffer);
        // 2/3/4. Apply the pure transform; only an `Applied` outcome mutates.
        match apply_structural(&cst, op) {
            TransformOutcome::Applied(new_cst) => {
                // Ensure any in-flight coalescing typing run is flushed to its own
                // boundary first, so the structural op is a *separate* undo unit
                // and never merges with a prior keystroke run (FR-014).
                self.record_undo_snapshot(now);
                // Install the new text by printing the new CST: untouched regions
                // are byte-for-byte identical (FR-013); only the touched subtree
                // changed.
                self.buffer = ron_core::print(&new_cst);
                // One logical op = one undo unit. Bump the generation so the next
                // `record_undo_snapshot` snapshots THIS new state, and force a fresh
                // (non-coalescing) unit so undo restores exactly the prior bytes.
                self.on_edit();
                self.last_undo_instant = None;
                self.record_undo_snapshot(now);
                // Re-validate + re-derive the projection off-frame against the new
                // text (FR-015/FR-026): the reparse will re-derive in `poll_parse`.
                self.request_reparse(worker);
                Ok(())
            }
            // 4. Blocked: no buffer change, no undo entry — surface the reason.
            TransformOutcome::Blocked(reason) => Err(reason),
            // `TransformOutcome` is `#[non_exhaustive]`: a future non-`Applied`
            // outcome is treated conservatively as a block — change nothing, record
            // no undo entry (never corrupt the document — §I / FR-014).
            _ => Err(BlockedReason::TargetNotFound),
        }
    }

    /// Commit a **pre-computed final [`CstDocument`]** (e.g. a defaults-elision
    /// result) as exactly **one** E007 undo unit (E009 US3 — T029, FR-016, SC-006).
    ///
    /// This is the multi-op sibling of
    /// [`apply_structural_edit`](Self::apply_structural_edit): where that method
    /// applies a single [`StructuralOp`] internally, this takes a whole, already-
    /// transformed CST (the one CST→CST result the elision pass produced by routing
    /// every field removal/insertion through `apply_structural` — the
    /// [`ElisionOutcome::Applied`](crate::bevy::ElisionOutcome::Applied) document) and
    /// installs it. The body **mirrors `apply_structural_edit` exactly** so the whole
    /// invocation is one logical action / one undo unit (FR-016):
    ///
    /// 1. Flush any in-flight coalescing typing run to its own boundary
    ///    ([`record_undo_snapshot`](Self::record_undo_snapshot)) so the commit is a
    ///    **separate** undo unit and never merges with a prior keystroke run.
    /// 2. Install the new text by **printing** the new CST: every untouched region is
    ///    byte-for-byte identical (the elision pass reused the green tree for
    ///    untouched subtrees — FR-016), only the touched fields changed.
    /// 3. Bump the edit generation ([`on_edit`](Self::on_edit)), force a fresh
    ///    (non-coalescing) unit by clearing the timing anchor, and snapshot so undo
    ///    restores **exactly** the pre-commit bytes / redo replays exactly the
    ///    post-commit bytes (one undo unit — SC-006).
    /// 4. Request an off-frame reparse so validation + the projection re-derive
    ///    against the new text.
    ///
    /// `now` is the caller-side clock for the undo coalesce decision (`ron-core`
    /// measures no clock — TR-014); the commit snapshot is always recorded as a
    /// non-coalescing boundary regardless of timing.
    ///
    /// A [`NoOp`](crate::bevy::ElisionOutcome::NoOp) outcome must **not** reach this
    /// method — nothing in scope was elidable/expandable, so the caller changes zero
    /// bytes and pushes no undo unit (FR-014). Only the `Applied` document is
    /// committed here.
    pub fn commit_transformed_cst(
        &mut self,
        new_cst: &ron_core::CstDocument,
        worker: &ReparseWorker,
        now: Instant,
    ) {
        // Flush any in-flight coalescing typing run first so this commit is a
        // *separate* undo unit and never merges with a prior keystroke run (FR-016).
        self.record_undo_snapshot(now);
        // Install the new text by printing the new CST: untouched regions are
        // byte-for-byte identical (FR-016); only the elided/expanded fields changed.
        self.buffer = ron_core::print(new_cst);
        // One logical op = one undo unit. Bump the generation so the next snapshot
        // captures THIS new state, and force a fresh (non-coalescing) unit so undo
        // restores exactly the prior bytes (SC-006).
        self.on_edit();
        self.last_undo_instant = None;
        self.record_undo_snapshot(now);
        // Re-validate + re-derive the projection off-frame against the new text.
        self.request_reparse(worker);
    }

    /// Install a **pre-serialized converted text buffer** (e.g. a RON→JSON / JSONC
    /// conversion) as exactly **one** E007 undo unit (E010 US1 — T014, FR-003,
    /// AD-005).
    ///
    /// This is the text-installing sibling of
    /// [`commit_transformed_cst`](Self::commit_transformed_cst): where that method
    /// prints a transformed RON [`CstDocument`], an in-place RON→JSON conversion
    /// produces **JSON/JSONC text** that is *not* a RON tree, so the already-
    /// serialized `text` is installed directly (printing it as RON would corrupt
    /// it). The body **mirrors `commit_transformed_cst` exactly** so the whole
    /// conversion is one logical action / one undo unit (FR-003):
    ///
    /// 1. Flush any in-flight coalescing typing run to its own boundary
    ///    ([`record_undo_snapshot`](Self::record_undo_snapshot)) so the conversion
    ///    is a **separate** undo unit and never merges with a prior keystroke run.
    /// 2. Install the converted `text` as the live buffer (zero bytes change until
    ///    this line — the caller built the conversion read-only over the source, so
    ///    a Cancel before commit leaves the document byte-identical, SC-002/003).
    /// 3. Bump the edit generation ([`on_edit`](Self::on_edit)), force a fresh
    ///    (non-coalescing) unit by clearing the timing anchor, and snapshot so undo
    ///    restores **exactly** the pre-conversion bytes / redo replays exactly the
    ///    converted bytes (one undo unit — SC-003).
    /// 4. Request an off-frame reparse so the converted buffer is a **normal dirty
    ///    E007 buffer** covered by the existing autosave/recovery sidecar — no new
    ///    persistence path (AD-005).
    ///
    /// `now` is the caller-side clock for the undo coalesce decision; the commit
    /// snapshot is always recorded as a non-coalescing boundary regardless of
    /// timing.
    pub fn commit_converted_text(&mut self, text: String, worker: &ReparseWorker, now: Instant) {
        // Flush any in-flight coalescing typing run first so this commit is a
        // *separate* undo unit and never merges with a prior keystroke run (FR-003).
        self.record_undo_snapshot(now);
        // Install the converted text directly — it is JSON/JSONC, not a RON tree, so
        // it is NOT routed through `ron_core::print` (that would re-parse + corrupt
        // it). The source bytes are unchanged until exactly this assignment.
        self.buffer = text;
        // One logical op = one undo unit. Bump the generation so the next snapshot
        // captures THIS new state, and force a fresh (non-coalescing) unit so undo
        // restores exactly the prior bytes (SC-003).
        self.on_edit();
        self.last_undo_instant = None;
        self.record_undo_snapshot(now);
        // Re-validate off-frame against the new text; the converted buffer is a
        // normal dirty E007 buffer (autosave/recovery covers it — AD-005).
        self.request_reparse(worker);
    }
}

/// Build the merged editor-coordinate diagnostic view for a landed
/// [`ParseResult`] against `buffer` (E006/FR-006, FR-017; E009/FR-007/FR-013).
///
/// The structural set (`result.diagnostics`) is always published in full and comes
/// first so it takes visual/list precedence. The **mode-specific** set is then
/// appended — exactly one of the two mutually-exclusive sets is non-empty per the
/// document's active mode (FR-013):
///
/// * **Serde mode** (`result.type_diagnostics`) → unchanged: deduped against the
///   structural set via [`ron_validate::dedup_against_structural`] (a type finding
///   overlapping a structural one is suppressed — structural wins, FR-017), then
///   mapped via [`map_diagnostic`]. Byte-for-byte the prior behavior.
/// * **Bevy mode** (`result.scene_diagnostics`) → each [`SceneDiagnostic`] is
///   mapped via [`map_scene_diagnostic`], so a registered-mismatch renders as a
///   regular `RON-V####` type finding and a scene-level hint/advisory keeps its
///   distinguishing `BVY-S####` code (E009/FR-007/IP-005). Scene findings render
///   through this **same** E006 surface (squiggles + Problems panel).
///
/// This is the single "replace, not merge" publish point for the mode-specific
/// set: every landed result recomputes the whole view, so no stale finding
/// survives (FR-006). The structural set is NEVER dropped here.
#[must_use]
pub fn merge_type_diagnostics(result: &ParseResult, buffer: &str) -> Vec<DiagnosticView> {
    // Structural findings always lead and are never dropped (FR-017).
    let mut views: Vec<DiagnosticView> = result
        .diagnostics
        .iter()
        .map(|d| map_diagnostic(d, buffer))
        .collect();

    // Serde mode: append the deduped type set (structural wins on overlap, FR-017).
    // Suppress type findings that overlap a structural one. The structural set is
    // passed by reference and is never mutated.
    let type_diags = ron_validate::dedup_against_structural(
        result.type_diagnostics.clone(),
        &result.diagnostics,
    );
    views.extend(type_diags.iter().map(|d| map_diagnostic(d, buffer)));

    // Bevy mode: append the scene findings (mutually exclusive with the type set —
    // at most one is non-empty, FR-013). A registered-mismatch renders as a RON-V
    // type finding; a scene-level hint/advisory keeps its BVY-S code (FR-007).
    views.extend(
        result
            .scene_diagnostics
            .iter()
            .map(|d| map_scene_diagnostic(d, buffer)),
    );

    views
}

#[cfg(test)]
mod mode_selection_tests {
    //! E009 US2 (T021/T022) — the per-document serde-vs-Bevy validation selection
    //! (`bound_validation`) and per-document coexistence, unit-tested in isolation
    //! from the off-frame worker (deterministic; no threads).

    use super::*;
    use crate::bevy::mode::{RegistryBindingConfig, RegistryBindingRule};
    use crate::reparse::BoundValidation;
    use std::path::{Path, PathBuf};

    const REGISTRY: &str = r##"{
        "bevyVersion": "0.16.0",
        "$defs": { "game::Vec3": { "kind": "Struct", "properties": {} } }
    }"##;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ronin_doc_mode_sel_{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn scene_config() -> RegistryBindingConfig {
        RegistryBindingConfig {
            rules: vec![RegistryBindingRule {
                pattern: "**/*.scn.ron".to_string(),
                exclude: None,
                registry_export_path: PathBuf::from("registry.json"),
                mode: None,
                expected_bevy_version: None,
            }],
            ..Default::default()
        }
    }

    fn serde_bound() -> BoundType {
        BoundType {
            model: Arc::new(serde_json::json!({ "$defs": { "Entity": { "type": "object" } } })),
            type_name: "Entity".to_string(),
        }
    }

    #[test]
    fn serde_mode_selects_serde_validation() {
        // A default (Serde) document with a serde binding selects the Serde variant
        // — byte-for-byte the prior behavior.
        let mut doc = EditorDocument::new_untitled(1);
        doc.bound_type = Some(serde_bound());
        assert!(!doc.is_bevy_mode());
        match doc.bound_validation() {
            Some(BoundValidation::Serde(b)) => assert_eq!(b.type_name, "Entity"),
            other => panic!("expected Serde validation, got {other:?}"),
        }
    }

    #[test]
    fn serde_mode_without_binding_selects_none() {
        let doc = EditorDocument::new_untitled(1);
        assert!(
            doc.bound_validation().is_none(),
            "no binding ⇒ structural-only"
        );
    }

    #[test]
    fn bevy_mode_with_loaded_registry_selects_scene_validation() {
        // A Bevy document with a loaded registry selects the Bevy variant: the
        // registry REPLACES the serde source (the serde bound_type is ignored).
        let root = temp_dir("bevy_loaded");
        std::fs::write(root.join("registry.json"), REGISTRY).unwrap();
        let config = scene_config();
        let path = root.join("w.scn.ron");

        let mut doc = EditorDocument::from_loaded(&path, b"").unwrap();
        // Even with a serde binding present, Bevy mode replaces it.
        doc.bound_type = Some(serde_bound());
        let mut state = ModeState::resolve(&config, Some(&path), None, None);
        assert!(state.load_registry(&root));
        doc.set_mode_state(state);

        assert!(doc.is_bevy_mode());
        match doc.bound_validation() {
            Some(BoundValidation::Bevy(scene)) => {
                assert!(scene.registry.contains("game::Vec3"));
                assert!(scene.model.get("$defs").is_some());
            }
            other => panic!("expected Bevy validation, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bevy_mode_without_registry_falls_through_to_serde() {
        // Bevy mode but NO registry loaded (NoRegistry): the scene validator only
        // runs when a registry is present, so this degrades to the serde branch —
        // its bound_type (which for a real Bevy doc is None ⇒ structural-only).
        let config = RegistryBindingConfig::default();
        let mut doc = EditorDocument::new_untitled(1);
        let state = ModeState::resolve(&config, Some(Path::new("w.scn.ron")), None, None);
        assert!(state.is_bevy());
        assert!(!state.has_registry());
        doc.set_mode_state(state);
        assert!(doc.is_bevy_mode());
        // No serde bound_type and no registry ⇒ structural-only.
        assert!(doc.bound_validation().is_none());
    }

    #[test]
    fn two_documents_hold_independent_mode_state_no_global() {
        // FR-012 — per-document coexistence: one Bevy doc and one serde doc hold
        // independent ModeStates; setting one never affects the other.
        let root = temp_dir("coexist_unit");
        std::fs::write(root.join("registry.json"), REGISTRY).unwrap();
        let config = scene_config();
        let scene_path = root.join("a.scn.ron");

        let mut bevy_doc = EditorDocument::from_loaded(&scene_path, b"").unwrap();
        let mut state = ModeState::resolve(&config, Some(&scene_path), None, None);
        assert!(state.load_registry(&root));
        bevy_doc.set_mode_state(state);

        let mut serde_doc = EditorDocument::new_untitled(1);
        serde_doc.bound_type = Some(serde_bound());

        assert!(bevy_doc.is_bevy_mode());
        assert!(!serde_doc.is_bevy_mode());
        assert!(matches!(
            bevy_doc.bound_validation(),
            Some(BoundValidation::Bevy(_))
        ));
        assert!(matches!(
            serde_doc.bound_validation(),
            Some(BoundValidation::Serde(_))
        ));
        // Toggling the serde doc to Bevy leaves the bevy doc untouched, and the
        // bevy doc's registry is unchanged (no shared/global state).
        serde_doc.mode_state_mut().set_mode_override(Mode::Bevy);
        assert!(serde_doc.is_bevy_mode());
        assert!(bevy_doc.is_bevy_mode(), "the other doc is unaffected");
        // The bevy doc keeps its own loaded registry and scene validation — the
        // toggle on the serde doc did not touch it (no shared/global state).
        assert!(bevy_doc.mode_state().has_registry());
        assert!(matches!(
            bevy_doc.bound_validation(),
            Some(BoundValidation::Bevy(_))
        ));
        // The serde doc, now in Bevy mode but with NO registry of its own, falls
        // through to its serde branch (it never borrows the other doc's registry).
        assert!(!serde_doc.mode_state().has_registry());
        let _ = std::fs::remove_dir_all(&root);
    }
}
