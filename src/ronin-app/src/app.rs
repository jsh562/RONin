//! The running editor shell: an [`eframe::App`] that opens, displays, and edits
//! RON documents with live diagnostics (FR-001/FR-002/FR-003/FR-006).
//!
//! [`App`] holds the multi-tab [`EditorWorkspace`] (open documents, the active-tab
//! pointer, the recently-closed stack, and untitled numbering — FR-012), the
//! off-thread [`ReparseWorker`], persisted [`AppSettings`], and a small stack of
//! dismissible [`Notice`]s. Each frame it:
//!
//! 1. drains finished parse results into every document (off the parse path);
//! 2. requests a coalesced reparse for any document whose buffer changed;
//! 3. renders the menu bar, the reserved E008/E009 layout seams (FR-013), the tab
//!    strip (FR-012), the active editor (or the empty-workspace placeholder,
//!    FR-022), and the diagnostics panel.
//!
//! Multi-tab behavior lives in [`crate::workspace`]; the `App` delegates tab
//! operations there and layers the dirty-prompt / sequential-close state machine
//! (FR-010/FR-026) and the focus-existing-on-open rule (FR-025) on top.
//!
//! **Event-driven repaint policy (FR-024):** the shell is *reactive* — it never
//! requests a continuous per-frame repaint while idle. egui repaints on its own
//! whenever input arrives; on top of that, the shell schedules a repaint only on a
//! **discrete trigger**:
//!
//! * keyboard / mouse / IME input or a window/OS event — handled by egui itself
//!   (it only re-runs the app when something changed);
//! * an **off-frame parse result** landing — the [`ReparseWorker`], holding the
//!   [`egui::Context`] installed via [`App::set_repaint_ctx`], calls
//!   [`egui::Context::request_repaint`] exactly when a result is ready;
//! * a **file open / drop / Save** — these happen inside an input-driven frame, so
//!   the resulting state change is already painted that frame;
//! * a **pending auto-dismiss info notice** — while (and only while) one is on
//!   screen, [`App::render_notices`] schedules a single
//!   [`egui::Context::request_repaint_after`] for its TTL so it can expire; once
//!   no info notice remains, nothing is scheduled and the UI returns to idle.
//!
//! There is deliberately **no** unconditional `request_repaint()` in the
//! update/render path, so an editor at rest consumes no frames.
//!
//! **Settings persistence (FR-016):** the live window geometry is folded into
//! [`AppSettings`] every frame ([`App::capture_geometry`]) and written to the OS
//! config dir on `eframe`'s periodic `save` tick and again on `on_exit`. Only
//! geometry and the preferences map are persisted — **no** open-document set,
//! paths, or tab order (no session restore by design).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::document::{ByteFidelityProfile, EditorDocument};
use crate::editor_view::editor_view;
use crate::fileio::{open_path, save_bytes, save_document, OpenError, SaveError};
use crate::panels::{mode_selector_seam_stub, tree_table_seam_stub};
use crate::problems_panel::problems_panel;
use crate::reparse::ReparseWorker;
use crate::settings::{AppSettings, WindowGeometry};
use crate::workspace::{ClosedDocumentRecord, EditorWorkspace};

/// How long an informational (auto-dismiss) notice stays on screen.
const INFO_NOTICE_TTL: Duration = Duration::from_secs(4);

/// The severity of a user-facing [`Notice`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    /// A blocking failure the user must acknowledge (e.g. open failed,
    /// non-UTF-8). Stays until dismissed; renders with error emphasis (FR-018).
    Error,
    /// A transient, informational status (e.g. an ignored drop). Auto-dismisses
    /// after [`INFO_NOTICE_TTL`]; distinct from [`NoticeKind::Error`] (FR-002).
    Info,
}

/// A single user-facing notice rendered in the shell's notice area.
#[derive(Debug, Clone)]
pub struct Notice {
    /// Whether this is a blocking error or a transient info toast.
    pub kind: NoticeKind,
    /// The human-readable message.
    pub message: String,
    /// When the notice was created (drives auto-dismiss for [`NoticeKind::Info`]).
    created: Instant,
}

impl Notice {
    /// A blocking error notice the user must dismiss.
    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            kind: NoticeKind::Error,
            message: message.into(),
            created: Instant::now(),
        }
    }

    /// A transient informational notice that auto-dismisses.
    #[must_use]
    pub fn info(message: impl Into<String>) -> Self {
        Self {
            kind: NoticeKind::Info,
            message: message.into(),
            created: Instant::now(),
        }
    }

    /// `true` when an [`NoticeKind::Info`] notice has outlived its TTL.
    #[must_use]
    fn expired(&self) -> bool {
        matches!(self.kind, NoticeKind::Info) && self.created.elapsed() >= INFO_NOTICE_TTL
    }
}

/// What the shell should do once an unsaved-changes prompt is resolved (FR-010).
///
/// The prompt is raised whenever a dirty document is about to be dropped — closing
/// its tab or quitting the app. The pending action records *what* to proceed with
/// after a Save or Discard; Cancel discards the action and leaves the document
/// open and dirty.
///
/// [`CloseDoc`](Self::CloseDoc) drives the single-tab close path (FR-010).
/// [`Batch`](Self::Batch) drives the sequential multi-dirty quit / close-all /
/// close-others path (FR-026): the prompt walks one dirty tab at a time and a
/// Cancel at any step aborts the whole operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingAction {
    /// Close the tab at this document index after the prompt resolves.
    CloseDoc(usize),
    /// Quit the application after the prompt resolves.
    Quit,
    /// One step of a sequential batch operation (FR-026); the active batch state
    /// on the [`App`] records the remaining tabs and the operation kind.
    Batch,
}

/// The kind of batch close/quit operation in flight (FR-026).
///
/// Each variant prompts save/discard for every dirty tab it affects, one prompt
/// at a time; a Cancel at any prompt aborts the whole operation and leaves all
/// remaining tabs open and unchanged (no partial close).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchKind {
    /// Quit the app: every open tab is affected.
    Quit,
    /// Close every tab.
    CloseAll,
    /// Close every tab except the one to keep.
    CloseOthers,
}

/// State for a sequential multi-dirty close/quit operation (FR-026).
///
/// Built when a batch operation starts: it records the operation kind, the set of
/// tabs to close (by stable document identity, tracked via a token so reorders /
/// removals during the sequence stay correct), and which of those still need a
/// dirty prompt. The operation processes one prompt at a time; Cancel clears this
/// state and aborts, leaving all tabs untouched.
#[derive(Debug, Clone)]
struct PendingBatch {
    /// Which batch operation this is (drives the final terminal action).
    kind: BatchKind,
    /// Identity tokens of the tabs this operation will close, in order. Tokens
    /// (not indices) are used so the set stays correct even though indices shift
    /// as clean tabs are removed between prompts.
    targets: Vec<u64>,
    /// The identity token currently being prompted, if a prompt is open for this
    /// batch (so a Save/Discard resolution knows which tab it concerns).
    prompting: Option<u64>,
}

/// The user's choice on the unsaved-changes prompt (FR-010).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptChoice {
    /// Save the document, then proceed with the pending action.
    Save,
    /// Drop the unsaved changes, then proceed with the pending action.
    Discard,
    /// Abort: keep the document open and dirty, take no action.
    Cancel,
}

/// A live unsaved-changes prompt awaiting the user's decision (FR-010).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirtyPrompt {
    /// The index of the dirty document the prompt concerns.
    pub doc_index: usize,
    /// What to do after the prompt resolves with Save/Discard.
    pub action: PendingAction,
}

/// The RONin desktop editor shell (FR-003).
pub struct App {
    /// The multi-tab workspace: open documents, the active-tab pointer, the
    /// recently-closed stack, and untitled numbering (FR-012).
    workspace: EditorWorkspace,
    /// The off-thread reparse worker shared by all documents.
    worker: ReparseWorker,
    /// Persisted application settings (window geometry, large-file threshold).
    settings: AppSettings,
    /// The live stack of dismissible notices.
    notices: Vec<Notice>,
    /// A pending unsaved-changes prompt, when one is open (FR-010).
    dirty_prompt: Option<DirtyPrompt>,
    /// State for an in-flight sequential close/quit batch operation (FR-026), or
    /// `None` when no batch is running.
    pending_batch: Option<PendingBatch>,
    /// Set once the user has confirmed a quit (no dirty doc, or after Save/Discard)
    /// so the next frame can close the viewport (FR-010 quit guard).
    quit_requested: bool,
}

impl App {
    /// Construct the shell, optionally opening `cli_path` at launch (FR-003).
    ///
    /// If `cli_path` is given and opens successfully it becomes the active tab; on
    /// failure (missing / unreadable / non-UTF-8) an **error** notice is pushed
    /// and the shell still constructs into an empty workspace — launch never
    /// aborts (FR-003/FR-019).
    #[must_use]
    pub fn new(settings: AppSettings, cli_path: Option<PathBuf>) -> Self {
        let mut app = Self {
            workspace: EditorWorkspace::new(),
            worker: ReparseWorker::new(),
            settings,
            notices: Vec::new(),
            dirty_prompt: None,
            pending_batch: None,
            quit_requested: false,
        };
        if let Some(path) = cli_path {
            app.open_file(&path);
        }
        app
    }

    /// Install the [`egui::Context`] the worker wakes on result delivery (FR-024).
    ///
    /// Call once from the `eframe` creator so the background worker can repaint an
    /// idle UI when a parse result is ready.
    pub fn set_repaint_ctx(&self, ctx: egui::Context) {
        self.worker.set_repaint_ctx(ctx);
    }

    /// The configured large-file threshold (bytes).
    #[must_use]
    pub fn large_file_threshold(&self) -> u64 {
        self.settings.effective_large_file_threshold()
    }

    /// The persisted settings the shell will restore on the next launch (FR-016).
    ///
    /// Read-only view for tests and host integration; the live geometry is folded
    /// in by [`capture_geometry`](Self::capture_geometry) before a save.
    #[must_use]
    pub fn settings(&self) -> &AppSettings {
        &self.settings
    }

    /// Record the current window geometry into the in-memory settings (FR-016).
    ///
    /// Reads the live viewport rectangles from the egui input state — the content
    /// size from `inner_rect` (so it round-trips with the `with_inner_size`
    /// restore in `main`) and the window position from `outer_rect.min` (the
    /// chrome-inclusive top-left). When the platform reports no rectangle (e.g.
    /// Wayland / Android, where window geometry is unavailable) the previous value
    /// is kept rather than overwritten with a bogus default. Persists **no**
    /// session/document state — only geometry and the preferences map are saved.
    pub fn capture_geometry(&mut self, ctx: &egui::Context) {
        let (inner, outer) = ctx.input(|i| {
            let vp = i.viewport();
            (vp.inner_rect, vp.outer_rect)
        });
        if let Some(inner) = inner {
            let size = inner.size();
            // Guard against a zero/degenerate rect (e.g. a minimized window) so we
            // never persist an unusable window size.
            if size.x.is_finite() && size.y.is_finite() && size.x > 0.0 && size.y > 0.0 {
                let pos = outer
                    .map(|r| (r.min.x, r.min.y))
                    .or_else(|| self.settings.window_geometry.as_ref().and_then(|g| g.pos));
                self.settings.window_geometry = Some(WindowGeometry {
                    pos,
                    size: (size.x, size.y),
                });
            }
        }
    }

    /// Persist the current settings to the OS config directory, swallowing any I/O
    /// error (FR-016).
    ///
    /// Best-effort: settings persistence must never crash the editor or block exit
    /// — a failed write is logged and ignored. Captures **no** open-document set,
    /// paths, or tab order (no session restore by design).
    fn persist_settings(&self) {
        if let Err(e) = self.settings.save() {
            tracing::warn!(error = %e, "failed to persist app settings");
        }
    }

    /// Number of open documents (one per tab).
    #[must_use]
    pub fn document_count(&self) -> usize {
        self.workspace.len()
    }

    /// The active document, if any (read-only; for tests and host integration).
    #[must_use]
    pub fn active_document(&self) -> Option<&EditorDocument> {
        self.workspace.active_document()
    }

    /// The active document mutably, if any.
    #[must_use]
    pub fn active_document_mut(&mut self) -> Option<&mut EditorDocument> {
        self.workspace.active_document_mut()
    }

    /// The active tab index, or `None` when no tab is open (for tests/hosts).
    #[must_use]
    pub fn active_index(&self) -> Option<usize> {
        self.workspace.active_index()
    }

    /// The number of records on the recently-closed stack (for tests/hosts).
    #[must_use]
    pub fn recently_closed_count(&self) -> usize {
        self.workspace.recently_closed().len()
    }

    /// The live notices (for tests and host integration).
    #[must_use]
    pub fn notices(&self) -> &[Notice] {
        &self.notices
    }

    /// Open `path` as a new active tab, focusing an existing tab if the same file
    /// is already open, or push a notice on failure (FR-001/FR-025).
    ///
    /// If a tab whose canonical path matches `path` is already open, that tab is
    /// focused instead of creating a duplicate (FR-025). Otherwise, on success the
    /// document is appended and made active and an initial reparse is requested.
    /// On [`OpenError`] an **error** notice is pushed and **no tab** is created
    /// (FR-018/FR-020), distinct from the auto-dismiss info notice used for ignored
    /// drops.
    pub fn open_file(&mut self, path: &std::path::Path) {
        // FR-025: focus an already-open tab for the same (canonical) path rather
        // than opening a duplicate. Path-less buffers never match (they have no
        // path), so never-saved buffers stay exempt.
        if let Some(idx) = self.find_open_tab_for(path) {
            self.workspace.switch(idx);
            return;
        }
        match open_path(path) {
            Ok(mut doc) => {
                // Bump the edit generation once and request an initial parse so the
                // freshly opened buffer gets diagnostics/highlighting without an
                // edit. (Generation 0 is the empty "never requested" baseline.)
                doc.on_edit();
                doc.request_reparse(&self.worker);
                self.workspace.open(doc);
            }
            Err(e) => {
                self.push_open_error(&e, path);
            }
        }
    }

    /// Find the index of an open tab whose file is the same as `path` by canonical
    /// path, falling back to raw-path equality when canonicalization fails (FR-025).
    ///
    /// Returns `None` when no open tab maps to that file (so a new tab is created).
    /// Never-saved, path-less buffers are exempt: they have no path and so never
    /// match here.
    fn find_open_tab_for(&self, path: &std::path::Path) -> Option<usize> {
        let target = canonicalize_or_raw(path);
        self.workspace.documents().iter().position(|d| {
            d.path
                .as_deref()
                .is_some_and(|p| canonicalize_or_raw(p) == target)
        })
    }

    /// Push the appropriate **error** notice for a failed open (FR-018).
    fn push_open_error(&mut self, err: &OpenError, path: &std::path::Path) {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        let message = match err {
            OpenError::NotUtf8 => format!("Cannot open {name}: not valid UTF-8"),
            OpenError::Io(io) => format!("Cannot open {name}: {io}"),
        };
        self.notices.push(Notice::error(message));
    }

    /// Create a fresh, empty untitled document and make it the active tab.
    pub fn new_untitled(&mut self) {
        self.workspace.push_untitled();
    }

    /// The live unsaved-changes prompt, if one is open (for tests/hosts).
    #[must_use]
    pub fn dirty_prompt(&self) -> Option<DirtyPrompt> {
        self.dirty_prompt
    }

    /// Save the active document to its path, routing to Save As if path-less (FR-011).
    ///
    /// On success the document is re-baselined ([`EditorDocument::mark_saved`]) so
    /// it is no longer dirty. On a disk error an **error** notice is pushed and the
    /// document is left dirty (its saved snapshot is *not* advanced), so the user
    /// can retry — "never corrupt user data" means a failed save must not pretend
    /// to have succeeded. Returns `true` when the document was actually saved.
    pub fn save_active(&mut self) -> bool {
        let Some(idx) = self.workspace.active_index() else {
            return false;
        };
        let Some(doc) = self.workspace.get(idx) else {
            return false;
        };
        match doc.path.clone() {
            Some(path) => self.save_doc_to(idx, &path),
            // Path-less (untitled) buffer: route to Save As to pick a target.
            None => self.save_as_active(),
        }
    }

    /// Pick a target via the file dialog, write the active document, and adopt the
    /// chosen path/profile (FR-011).
    ///
    /// After a successful write the document's [`path`](EditorDocument::path) is set
    /// and its [`byte_profile`](EditorDocument::byte_profile) is refreshed to the
    /// just-written bytes, so subsequent saves round-trip against the new file.
    /// Cancelling the dialog is a no-op. The `rfd` dialog itself cannot run in a
    /// headless test, so its post-dialog logic (write + profile refresh) is also
    /// reachable directly via [`save_doc_to`](Self::save_doc_to) for tests.
    pub fn save_as_active(&mut self) -> bool {
        let Some(idx) = self.workspace.active_index() else {
            return false;
        };
        let Some(target) = rfd::FileDialog::new()
            .add_filter("RON", &["ron"])
            .save_file()
        else {
            return false;
        };
        self.save_doc_to(idx, &target)
    }

    /// Write document `idx` to `path`, refresh its identity, and clear dirty.
    ///
    /// Shared by Save and Save As (and tests, since it sidesteps the `rfd` dialog):
    /// writes via [`save_document`], and on success sets the document's path,
    /// refreshes its byte-fidelity profile from the bytes actually written, and
    /// marks it saved. On failure pushes an error notice and keeps the doc dirty.
    /// Returns `true` only on a successful write.
    pub fn save_doc_to(&mut self, idx: usize, path: &Path) -> bool {
        let Some(doc) = self.workspace.get_mut(idx) else {
            return false;
        };
        match save_document(doc, path) {
            Ok(()) => {
                // Refresh identity from the bytes we just wrote so future saves
                // round-trip against the new file (Save As may target a new path).
                let written = save_bytes(&doc.buffer, &doc.byte_profile);
                doc.path = Some(path.to_path_buf());
                doc.byte_profile = ByteFidelityProfile::from_bytes(&written);
                doc.untitled_seq = None;
                doc.mark_saved();
                true
            }
            Err(e) => {
                self.push_save_error(&e, path);
                false
            }
        }
    }

    /// Push an **error** notice for a failed save (FR-011). The doc stays dirty.
    fn push_save_error(&mut self, err: &SaveError, path: &Path) {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        let message = match err {
            SaveError::Io(io) => format!("Cannot save {name}: {io}"),
        };
        self.notices.push(Notice::error(message));
    }

    /// Hook invoked when a document is dropped after a Discard (FR-010 / FR-012).
    ///
    /// Captures a [`ClosedDocumentRecord`] from the discarded document and pushes
    /// it onto the workspace's bounded recently-closed stack so the tab can be
    /// reopened from memory (FR-012). The document is then dropped — its derived
    /// parse/highlight state is released here.
    pub fn on_document_discarded(&mut self, doc: EditorDocument) {
        let record = ClosedDocumentRecord::capture(&doc);
        self.workspace.push_closed(record);
        // Explicitly drop: the document and its derived state are released here.
        drop(doc);
    }

    /// Begin closing document `idx`, prompting first if it has unsaved changes
    /// (FR-010).
    ///
    /// A clean document closes immediately. A dirty one raises the save/discard/
    /// cancel prompt with a [`PendingAction::CloseDoc`]; the close completes only
    /// after the prompt resolves.
    pub fn request_close_doc(&mut self, idx: usize) {
        match self.workspace.get(idx) {
            Some(doc) if doc.dirty() => {
                self.dirty_prompt = Some(DirtyPrompt {
                    doc_index: idx,
                    action: PendingAction::CloseDoc(idx),
                });
            }
            Some(_) => self.close_doc(idx),
            None => {}
        }
    }

    /// Close (drop) document `idx` without prompting, routing it through the
    /// discard hook (which records it for reopen) and fixing up the active index.
    fn close_doc(&mut self, idx: usize) {
        if let Some(doc) = self.workspace.close(idx) {
            self.on_document_discarded(doc);
        }
    }

    /// Resolve an open unsaved-changes prompt with the user's `choice` (FR-010/FR-026).
    ///
    /// * `Save` → save the document, then (only on a successful save) proceed with
    ///   the pending action.
    /// * `Discard` → proceed with the pending action, dropping the changes.
    /// * `Cancel` → close the prompt and take no action; the document stays open
    ///   and dirty.
    ///
    /// For a [`PendingAction::Batch`] step (FR-026) the resolution drives the
    /// sequential operation: Save/Discard close that tab and advance to the next
    /// dirty tab; Cancel aborts the whole operation, leaving every remaining tab
    /// open and unchanged (no partial close).
    pub fn resolve_dirty_prompt(&mut self, choice: PromptChoice) {
        let Some(prompt) = self.dirty_prompt.take() else {
            return;
        };
        if matches!(prompt.action, PendingAction::Batch) {
            self.resolve_batch_step(choice);
            return;
        }
        match choice {
            PromptChoice::Cancel => {
                // Abort: nothing to do; the doc stays open and dirty.
            }
            PromptChoice::Save => {
                // Save the prompted document specifically (make it active first so
                // `save_active` targets it), and only proceed if it actually saved.
                self.workspace.switch(prompt.doc_index);
                if self.save_active() {
                    self.perform_pending(prompt.action);
                } else {
                    // Save failed (or was cancelled in Save As): re-raise the prompt
                    // so the user is not silently left without resolving it.
                    self.dirty_prompt = Some(prompt);
                }
            }
            PromptChoice::Discard => {
                self.perform_pending(prompt.action);
            }
        }
    }

    /// Carry out a single-tab pending action once its prompt is resolved.
    fn perform_pending(&mut self, action: PendingAction) {
        match action {
            PendingAction::CloseDoc(idx) => self.close_doc(idx),
            PendingAction::Quit => self.quit_requested = true,
            // A batch step is resolved by `resolve_batch_step`, never here.
            PendingAction::Batch => {}
        }
    }

    /// Begin a sequential multi-dirty batch operation (FR-026).
    ///
    /// Builds the set of tabs the operation affects (by stable identity token),
    /// closes the clean ones immediately (no prompt), and then either completes
    /// the operation (if no dirty tabs remain) or opens the first per-tab dirty
    /// prompt and advances one tab at a time. A Cancel at any prompt aborts the
    /// whole operation.
    fn start_batch(&mut self, kind: BatchKind, keep_idx: Option<usize>) {
        // Collect the identity tokens of every tab this operation will close,
        // in tab order, skipping the kept tab for close-others.
        let targets: Vec<u64> = self
            .workspace
            .documents()
            .iter()
            .enumerate()
            .filter(|(i, _)| keep_idx != Some(*i))
            .map(|(_, d)| d.id())
            .collect();
        self.pending_batch = Some(PendingBatch {
            kind,
            targets,
            prompting: None,
        });
        self.advance_batch();
    }

    /// Process the next step of the in-flight batch: close clean targets without a
    /// prompt, and raise a prompt for the next dirty one (FR-026).
    ///
    /// When all targets are processed the terminal action runs (quit, or simply
    /// leaving the workspace in its post-close state).
    fn advance_batch(&mut self) {
        loop {
            let Some(batch) = self.pending_batch.as_mut() else {
                return;
            };
            // Take the next target token off the front of the queue.
            let Some(token) = batch.targets.first().copied() else {
                // No more targets: run the terminal action and clear the batch.
                let kind = batch.kind;
                self.pending_batch = None;
                if matches!(kind, BatchKind::Quit) {
                    self.quit_requested = true;
                }
                return;
            };
            // Resolve the token to a live index; a token with no live tab (already
            // closed) is simply dropped from the queue.
            let Some(idx) = self.index_of_token(token) else {
                if let Some(batch) = self.pending_batch.as_mut() {
                    batch.targets.remove(0);
                }
                continue;
            };
            let is_dirty = self.workspace.get(idx).is_some_and(EditorDocument::dirty);
            if is_dirty {
                // Raise the per-tab prompt and pause the sequence until resolved.
                if let Some(batch) = self.pending_batch.as_mut() {
                    batch.prompting = Some(token);
                }
                self.workspace.switch(idx);
                self.dirty_prompt = Some(DirtyPrompt {
                    doc_index: idx,
                    action: PendingAction::Batch,
                });
                return;
            }
            // Clean target: close it now (no prompt) and continue the sweep.
            if let Some(batch) = self.pending_batch.as_mut() {
                batch.targets.remove(0);
            }
            self.close_doc(idx);
        }
    }

    /// Resolve one prompt within a batch operation (FR-026).
    ///
    /// Save/Discard close the prompted tab and advance to the next dirty tab;
    /// Cancel aborts the entire operation, dropping the batch and leaving every
    /// remaining tab open and unchanged (no partial close beyond tabs already
    /// closed before the cancel — see `start_batch`, which only closes clean tabs
    /// and dirty tabs the user explicitly resolved).
    fn resolve_batch_step(&mut self, choice: PromptChoice) {
        let Some(token) = self.pending_batch.as_ref().and_then(|b| b.prompting) else {
            // No batch / no prompting token: nothing to resolve.
            return;
        };
        match choice {
            PromptChoice::Cancel => {
                // Abort the whole operation: every remaining tab stays open.
                self.pending_batch = None;
            }
            PromptChoice::Save => {
                let Some(idx) = self.index_of_token(token) else {
                    // The tab vanished; just advance.
                    self.batch_drop_current();
                    self.advance_batch();
                    return;
                };
                self.workspace.switch(idx);
                if self.save_active() {
                    // Saved: close that tab and move on.
                    self.batch_drop_current();
                    self.close_doc(idx);
                    self.advance_batch();
                } else {
                    // Save failed (or Save As cancelled): re-raise this prompt so
                    // the operation is not silently stuck.
                    self.dirty_prompt = Some(DirtyPrompt {
                        doc_index: idx,
                        action: PendingAction::Batch,
                    });
                }
            }
            PromptChoice::Discard => {
                if let Some(idx) = self.index_of_token(token) {
                    self.batch_drop_current();
                    self.close_doc(idx);
                }
                self.advance_batch();
            }
        }
    }

    /// Drop the currently-prompting target from the batch queue and clear the
    /// prompting marker, so `advance_batch` moves to the next target.
    fn batch_drop_current(&mut self) {
        if let Some(batch) = self.pending_batch.as_mut() {
            if !batch.targets.is_empty() {
                batch.targets.remove(0);
            }
            batch.prompting = None;
        }
    }

    /// Resolve a document identity `token` to its current tab index, or `None` if
    /// no open tab carries it (it was already closed).
    fn index_of_token(&self, token: u64) -> Option<usize> {
        self.workspace
            .documents()
            .iter()
            .position(|d| d.id() == token)
    }

    /// Close every open tab, prompting per dirty tab in sequence (FR-026).
    ///
    /// Clean tabs close without a prompt; Cancel at any prompt aborts the whole
    /// operation and leaves all remaining tabs open and unchanged.
    pub fn close_all(&mut self) {
        if self.workspace.is_empty() {
            return;
        }
        self.start_batch(BatchKind::CloseAll, None);
    }

    /// Close every tab except `keep_idx`, prompting per dirty tab in sequence
    /// (FR-026).
    ///
    /// Clean tabs close without a prompt; Cancel at any prompt aborts the whole
    /// operation and leaves all remaining tabs open and unchanged. An out-of-range
    /// `keep_idx` is treated as a no-op so a stale index never closes the wrong tab.
    pub fn close_others(&mut self, keep_idx: usize) {
        if keep_idx >= self.workspace.len() {
            return;
        }
        self.start_batch(BatchKind::CloseOthers, Some(keep_idx));
    }

    /// Reopen the most-recently-closed tab from memory, honoring focus-existing
    /// (FR-012/FR-025).
    ///
    /// Pops the recently-closed stack and reconstructs the document with its
    /// closed buffer and dirty state. If the reopened document has a path that is
    /// already open in another tab, that existing tab is focused and the
    /// reconstructed duplicate is dropped (FR-025); never-saved path-less buffers
    /// are exempt and always reopen as their own tab. Reopening an empty stack is a
    /// harmless no-op. Returns `true` when a tab was reopened or focused.
    pub fn reopen_last_closed(&mut self) -> bool {
        let Some(idx) = self.workspace.reopen_closed() else {
            return false;
        };
        // FR-025: if the just-reopened doc duplicates an already-open path, focus
        // the original and drop the duplicate we just opened.
        if let Some(path) = self.workspace.get(idx).and_then(|d| d.path.clone()) {
            if let Some(existing) = self.find_existing_excluding(&path, idx) {
                // Remove the duplicate we just opened (no record — it is identical
                // to a still-open tab), then focus the original.
                let _ = self.workspace.close(idx);
                let focus = if existing > idx {
                    existing - 1
                } else {
                    existing
                };
                self.workspace.switch(focus);
                return true;
            }
        }
        // Request an initial parse for the freshly reopened buffer.
        if let Some(doc) = self.workspace.get_mut(idx) {
            doc.on_edit();
            doc.request_reparse(&self.worker);
        }
        true
    }

    /// Find an open tab (other than `exclude`) whose file matches `path` by
    /// canonical path, falling back to raw equality (FR-025 helper for reopen).
    fn find_existing_excluding(&self, path: &std::path::Path, exclude: usize) -> Option<usize> {
        let target = canonicalize_or_raw(path);
        self.workspace
            .documents()
            .iter()
            .enumerate()
            .find(|(i, d)| {
                *i != exclude
                    && d.path
                        .as_deref()
                        .is_some_and(|p| canonicalize_or_raw(p) == target)
            })
            .map(|(i, _)| i)
    }

    /// Open the standard file-picker and open the chosen file (FR-001).
    fn open_via_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("RON", &["ron"])
            .pick_file()
        {
            self.open_file(&path);
        }
    }

    /// Process files dropped onto the window this frame (FR-002).
    ///
    /// `.ron` files are opened as tabs. A dropped **non-`.ron`** file or a folder
    /// creates **no tab** and instead shows a brief, auto-dismissing info notice —
    /// distinct from the blocking error notice used for open failures (FR-002).
    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if dropped.is_empty() {
            return;
        }
        for file in dropped {
            match &file.path {
                Some(path) => self.apply_drop(path),
                // No path (e.g. web bytes-only drop) — cannot open from disk.
                None => self
                    .notices
                    .push(Notice::info(format!("Ignored dropped item: {}", file.name))),
            }
        }
    }

    /// Process a single dropped path: open `.ron` files, info-notice everything
    /// else (FR-002). Public so the drop contract is testable without a live
    /// egui `Context`.
    pub fn apply_drop(&mut self, path: &std::path::Path) {
        match classify_drop(path) {
            DropDecision::OpenRon => self.open_file(path),
            DropDecision::IgnoreFolder => self.notices.push(Notice::info(format!(
                "Ignored dropped folder: {}",
                display_name(path)
            ))),
            DropDecision::IgnoreNonRon => self.notices.push(Notice::info(format!(
                "Ignored non-RON file: {}",
                display_name(path)
            ))),
        }
    }

    /// Drain ready parse results into every document and request reparses for any
    /// document whose buffer advanced (FR-006).
    ///
    /// Returns `true` if any document installed a fresh result (the caller can
    /// repaint to show it). Runs for all documents — not just the active one — so
    /// background tabs stay current; none of this touches `ron_core::parse`
    /// directly (the worker owns parsing).
    fn pump_documents(&mut self) -> bool {
        let mut any_installed = false;
        for doc in self.workspace.documents_mut() {
            doc.request_reparse(&self.worker);
            if doc.poll_parse(&self.worker) {
                any_installed = true;
            }
        }
        any_installed
    }

    /// Render the top menu bar (File: New, Open, Save, Save As, Quit).
    fn render_menu_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("menu_bar").show_inside(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("New").clicked() {
                        self.new_untitled();
                        ui.close();
                    }
                    if ui.button("Open…").clicked() {
                        self.open_via_dialog();
                        ui.close();
                    }
                    // Save / Save As act on the active document (FR-011). Both are
                    // disabled when no document is open so the menu shape is stable.
                    let has_active = self.active_document().is_some();
                    ui.add_enabled_ui(has_active, |ui| {
                        if ui.button("Save").clicked() {
                            self.save_active();
                            ui.close();
                        }
                        if ui.button("Save As…").clicked() {
                            self.save_as_active();
                            ui.close();
                        }
                    });
                    ui.separator();
                    // Tab operations (FR-012). Close-all/close-others run the
                    // sequential multi-dirty prompt (FR-026).
                    ui.add_enabled_ui(has_active, |ui| {
                        if ui.button("Close Tab").clicked() {
                            if let Some(idx) = self.workspace.active_index() {
                                self.request_close_doc(idx);
                            }
                            ui.close();
                        }
                        if ui.button("Close Others").clicked() {
                            if let Some(idx) = self.workspace.active_index() {
                                self.close_others(idx);
                            }
                            ui.close();
                        }
                        if ui.button("Close All").clicked() {
                            self.close_all();
                            ui.close();
                        }
                    });
                    let can_reopen = self.recently_closed_count() > 0;
                    ui.add_enabled_ui(can_reopen, |ui| {
                        if ui.button("Reopen Closed Tab").clicked() {
                            self.reopen_last_closed();
                            ui.close();
                        }
                    });
                    ui.separator();
                    if ui.button("Quit").clicked() {
                        self.request_quit();
                        ui.close();
                    }
                });
            });
        });
    }

    /// Begin a quit, prompting save/discard for each dirty tab in sequence
    /// (FR-010/FR-026).
    ///
    /// A clean (or empty) workspace quits immediately. When one or more tabs are
    /// dirty the quit becomes a sequential batch operation: each dirty tab is
    /// prompted one at a time, clean tabs close without a prompt, and the app quits
    /// only once every tab is resolved. A Cancel at any prompt aborts the whole
    /// quit and leaves all remaining tabs open and unchanged (no partial close).
    pub fn request_quit(&mut self) {
        let any_dirty = self.workspace.documents().iter().any(EditorDocument::dirty);
        if any_dirty {
            self.start_batch(BatchKind::Quit, None);
        } else {
            self.quit_requested = true;
        }
    }

    /// Render the dismissible-notice area, dropping expired info notices.
    fn render_notices(&mut self, ui: &mut egui::Ui) {
        // Auto-dismiss expired info notices first.
        self.notices.retain(|n| !n.expired());
        if self.notices.is_empty() {
            return;
        }
        // If any info notice is still pending, schedule a repaint so it can expire
        // even with no other UI activity (otherwise we'd sit idle at rest).
        let has_info = self
            .notices
            .iter()
            .any(|n| matches!(n.kind, NoticeKind::Info));

        let mut dismiss: Option<usize> = None;
        egui::Panel::bottom("notices").show_inside(ui, |ui| {
            for (idx, notice) in self.notices.iter().enumerate() {
                ui.horizontal(|ui| {
                    match notice.kind {
                        NoticeKind::Error => {
                            ui.colored_label(ui.visuals().error_fg_color, &notice.message);
                        }
                        NoticeKind::Info => {
                            ui.weak(&notice.message);
                        }
                    }
                    if ui.small_button("Dismiss").clicked() {
                        dismiss = Some(idx);
                    }
                });
            }
        });
        if let Some(idx) = dismiss {
            if idx < self.notices.len() {
                self.notices.remove(idx);
            }
        }
        if has_info {
            ui.ctx().request_repaint_after(INFO_NOTICE_TTL);
        }
    }

    /// Render the docked Problems panel for the active document and apply a click
    /// as a cursor jump (FR-009).
    ///
    /// The panel is docked as a bottom panel so it is always visible beneath the
    /// editor; clicking a row queues a caret jump on the active document, which
    /// `editor_view` applies on the next frame (moving the caret to the
    /// diagnostic's start and scrolling it into view).
    fn render_problems_panel(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("problems_panel")
            .resizable(true)
            .default_size(140.0)
            .show_inside(ui, |ui| {
                ui.label("Problems");
                let Some(idx) = self.workspace.active_index() else {
                    ui.weak("No problems");
                    return;
                };
                // Snapshot the diagnostics so the panel can render while we later
                // mutate the document (the click result indexes this snapshot).
                let diagnostics = self
                    .workspace
                    .get(idx)
                    .map(|d| d.diagnostics.clone())
                    .unwrap_or_default();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        if let Some(clicked) = problems_panel(ui, &diagnostics) {
                            // Queue a caret jump to the clicked diagnostic's start
                            // char offset; `editor_view` applies it next frame.
                            if let Some(d) = diagnostics.get(clicked) {
                                if let Some(doc) = self.workspace.get_mut(idx) {
                                    doc.request_cursor_jump(d.char_range.0);
                                }
                            }
                        }
                    });
            });
    }

    /// Render the horizontal tab strip: one tab per open document, with a dirty
    /// dot, click-to-switch, drag-to-reorder, and a per-tab close button (FR-012).
    ///
    /// Each tab shows the document title prefixed by a filled dot (`●`) when dirty.
    /// Clicking a tab makes it active. Dragging a tab over another and releasing
    /// reorders the two (preserving identity + dirty state via
    /// [`EditorWorkspace::reorder`]). The per-tab `×` button routes through the
    /// dirty prompt when the tab is dirty. Renders nothing when no tab is open
    /// (the empty-workspace placeholder owns that state — FR-022).
    fn render_tab_bar(&mut self, ui: &mut egui::Ui) {
        if self.workspace.is_empty() {
            return;
        }
        egui::Panel::top("tab_bar").show_inside(ui, |ui| {
            let active = self.workspace.active_index();
            // Deferred mutations so we don't reshape the list mid-iteration.
            let mut switch_to: Option<usize> = None;
            let mut close_idx: Option<usize> = None;
            let mut reorder: Option<(usize, usize)> = None;

            // Track which tab is being dragged this frame (egui drag id state).
            ui.horizontal(|ui| {
                for idx in 0..self.workspace.len() {
                    let Some(doc) = self.workspace.get(idx) else {
                        continue;
                    };
                    let dot = if doc.dirty() { "\u{25CF} " } else { "" };
                    let label = format!("{dot}{}", doc.title());
                    let selected = active == Some(idx);

                    let id = egui::Id::new(("ronin_tab", doc.id()));
                    let response = ui
                        .dnd_drag_source(id, idx, |ui| {
                            let _ = ui.selectable_label(selected, label);
                        })
                        .response;

                    // Click-to-switch (a non-drag click selects the tab).
                    if response.clicked() {
                        switch_to = Some(idx);
                    }
                    // Drag-to-reorder: if a tab payload is released over this tab,
                    // move the dragged tab to this position (FR-012).
                    if let Some(payload) = response.dnd_release_payload::<usize>() {
                        let from = *payload;
                        if from != idx {
                            reorder = Some((from, idx));
                        }
                    }

                    if ui.small_button("\u{00D7}").clicked() {
                        close_idx = Some(idx);
                    }
                    ui.separator();
                }
            });

            if let Some((from, to)) = reorder {
                self.workspace.reorder(from, to);
            } else if let Some(idx) = switch_to {
                self.workspace.switch(idx);
            }
            if let Some(idx) = close_idx {
                self.request_close_doc(idx);
            }
        });
    }

    /// Render the central editor region for the active document, or the
    /// empty-workspace placeholder when no tab is open (FR-022).
    fn render_central(&mut self, ui: &mut egui::Ui) {
        let threshold = self.settings.effective_large_file_threshold();
        let active = self.workspace.active_index();
        egui::CentralPanel::default().show_inside(ui, |ui| match active {
            Some(idx) if self.workspace.get(idx).is_some() => {
                let doc = self.workspace.get(idx).expect("checked above");
                let oversize = doc.oversize(threshold);
                // A dirty document shows a leading dot in its title (FR-010).
                let dot = if doc.dirty() { "\u{25CF} " } else { "" };
                let title = doc.title();
                ui.label(format!("{dot}{title}"));
                ui.separator();
                if let Some(doc) = self.workspace.get_mut(idx) {
                    editor_view(ui, doc, oversize);
                }
            }
            _ => {
                // FR-022: a non-blank welcome/empty-state, never an ambiguous blank
                // area. Menus, drag-drop, and Open stay operable with no active tab.
                ui.centered_and_justified(|ui| {
                    ui.weak(
                        "No file open — use File \u{25B8} Open\u{2026} or drag a .ron file here",
                    );
                });
            }
        });
    }

    /// Render the unsaved-changes prompt as a modal window when one is open (FR-010).
    fn render_dirty_prompt(&mut self, ui: &mut egui::Ui) {
        let Some(prompt) = self.dirty_prompt else {
            return;
        };
        let title = self
            .workspace
            .get(prompt.doc_index)
            .map_or_else(|| "this document".to_string(), EditorDocument::title);

        let mut choice: Option<PromptChoice> = None;
        egui::Window::new("Unsaved changes")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                ui.label(format!("\"{title}\" has unsaved changes."));
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        choice = Some(PromptChoice::Save);
                    }
                    if ui.button("Discard").clicked() {
                        choice = Some(PromptChoice::Discard);
                    }
                    if ui.button("Cancel").clicked() {
                        choice = Some(PromptChoice::Cancel);
                    }
                });
            });
        if let Some(choice) = choice {
            self.resolve_dirty_prompt(choice);
        }
    }

    /// Render the reserved mode-selector seam in the top region (FR-013).
    ///
    /// Mounts [`mode_selector_seam_stub`] (reserved for **E009**) as its own top
    /// panel so a later epic can populate it without touching shell-core layout.
    fn render_mode_selector(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("mode_selector").show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                mode_selector_seam_stub(ui);
            });
        });
    }

    /// Render the reserved tree/table seam in a left side panel (FR-013).
    ///
    /// Mounts [`tree_table_seam_stub`] (reserved for **E008**) as a resizable left
    /// `SidePanel` so a later epic can populate it without touching shell-core
    /// layout. The seam is present regardless of whether a tab is open.
    fn render_tree_table(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("tree_table")
            .resizable(true)
            .default_size(180.0)
            .show_inside(ui, |ui| {
                ui.label("Structure");
                ui.separator();
                tree_table_seam_stub(ui);
            });
    }
}

impl App {
    /// Render the full shell layout into `ui` (FR-012/FR-013/FR-022).
    ///
    /// This is the renderer-only path: it lays out the menu bar, the reserved
    /// E009 mode-selector and E008 tree/table seams, the tab strip, the active
    /// diagnostics panel, and the central editor (or the empty-workspace
    /// placeholder when no tab is open). It takes only `&mut egui::Ui`, so it can
    /// be driven by a headless `egui_kittest` harness; the viewport/quit handling
    /// that needs an [`eframe::Frame`] stays in the [`eframe::App`] impl.
    pub fn render_shell(&mut self, ui: &mut egui::Ui) {
        // Layout order (FR-013): top panels, then side, then bottom, then the
        // central editor claims the remainder. The reserved E008/E009 seams and the
        // active diagnostics panel are docked here so later epics populate them
        // without editing shell-core.
        self.render_menu_bar(ui);
        // Reserved mode-selector seam (E009) sits under the menu bar.
        self.render_mode_selector(ui);
        // Tab strip below the menu/mode region (FR-012).
        self.render_tab_bar(ui);
        self.render_notices(ui);
        // Reserved tree/table seam (E008) as a left side panel.
        self.render_tree_table(ui);
        // Dock the Problems panel below before the central editor claims the rest.
        self.render_problems_panel(ui);
        self.render_central(ui);
        // The unsaved-changes prompt floats above everything when open.
        self.render_dirty_prompt(ui);
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Dropped-file intake before any rendering so a new tab shows this frame.
        let ctx = ui.ctx().clone();
        // Fold the live window geometry into settings each frame so the periodic
        // `save` (and the on-exit save) persists the current size/position (FR-016).
        self.capture_geometry(&ctx);
        self.handle_dropped_files(&ctx);

        // Window-close intake (the X button / OS quit): if any dirty tab needs a
        // prompt, cancel the close and run the sequential quit; otherwise allow it
        // (FR-010/FR-026 quit guard).
        if ctx.input(|i| i.viewport().close_requested()) && !self.quit_requested {
            self.request_quit();
            // A prompt or batch is now in flight: veto this close; the sequential
            // resolution drives the quit once every dirty tab is handled.
            if self.dirty_prompt.is_some() || self.pending_batch.is_some() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            }
        }

        // Off-thread parse pump: drain results, request coalesced reparses.
        self.pump_documents();

        self.render_shell(ui);

        // A confirmed quit (clean workspace, or resolved prompt) closes the window.
        if self.quit_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// Persist app settings on `eframe`'s periodic save tick (FR-016).
    ///
    /// `eframe` calls this on a timer (default ~30 s) and at shutdown. We do not
    /// use egui's key/value [`eframe::Storage`] for our settings — RONin owns its
    /// own JSON file in the OS config dir — so we write that file here. Geometry
    /// is already folded into `self.settings` each frame by `capture_geometry`.
    /// **No** open-document set / paths / tab order is persisted (no session
    /// restore by design).
    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        self.persist_settings();
    }

    /// Final settings persistence on shutdown, after [`Self::save`] (FR-016).
    ///
    /// Belt-and-suspenders: ensures the last-captured geometry and preferences
    /// reach disk even if the periodic `save` tick had not fired recently.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.persist_settings();
    }
}

/// The outcome of classifying a dropped path (FR-002).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropDecision {
    /// A `.ron` file — open it as a new tab.
    OpenRon,
    /// A folder — ignore with a transient info notice (no tab).
    IgnoreFolder,
    /// A non-`.ron` file — ignore with a transient info notice (no tab).
    IgnoreNonRon,
}

/// Decide how to handle a dropped `path` (FR-002).
///
/// Folders and non-`.ron` files are ignored (info notice, no tab); only `.ron`
/// files open. The folder check uses the filesystem, so a path that no longer
/// exists is treated by extension alone.
#[must_use]
pub fn classify_drop(path: &std::path::Path) -> DropDecision {
    if path.is_dir() {
        return DropDecision::IgnoreFolder;
    }
    if is_ron_path(path) {
        DropDecision::OpenRon
    } else {
        DropDecision::IgnoreNonRon
    }
}

/// `true` when `path`'s extension is `ron` (case-insensitive).
fn is_ron_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("ron"))
}

/// A short display name for a path (file/dir name, falling back to the full path).
fn display_name(path: &std::path::Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| path.display().to_string())
}

/// Canonicalize `path`, falling back to its raw form when canonicalization fails
/// (FR-025).
///
/// Used for focus-existing-tab matching: two paths refer to the same file when
/// their canonical forms are equal. Canonicalization requires the file to exist;
/// when it cannot resolve (e.g. a never-written path), the raw path is used so
/// matching still works by literal equality.
fn canonicalize_or_raw(path: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
