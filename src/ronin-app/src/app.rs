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

use crate::bevy::mode::{Mode, ModeState, RegistryBindingConfig, RegistryBindingRule};
use crate::binding::{
    BindingConfig, BindingRule, DocumentOverride, JsonToRonConsultation, TypeSourceLocator,
};
use crate::diagnostics_map::map_loss_report;
use crate::document::{ByteFidelityProfile, EditorDocument};
use crate::editor_view::{editor_view, structural_form_view};
use crate::fileio::{
    export_json, import_json, open_path, reconstruct_ron_from_bytes, save_bytes, save_document,
    ExportError, ImportError, OpenError, SaveError,
};
use crate::interop::loss::{LossKind, LossRecovery, LossReport, LossyConstruct};
use crate::interop::{
    derive_scaffold, render_json, ron_to_json, CommentCarrier, CommentMode, DeriveScaffold,
    JsoncStyle,
};
use crate::panels::{mode_selector_seam_stub, tree_table_seam_stub};
use crate::problems_panel::problems_panel;
use crate::reparse::ReparseWorker;
use crate::settings::{AppSettings, BlankLinePolicy, FormattingConfig, WindowGeometry};
use crate::settings::{JsonFormat, StrictCommentCarrier};
use crate::snippets::{SnippetSet, UserSnippetFile, USER_SNIPPET_TEMPLATE};
use crate::structural::view_state::ActiveView;
use crate::type_acquire::resolve_and_acquire;
use crate::workspace::{ClosedDocumentRecord, EditorWorkspace};
use ronin_core::{FormatResult, SyntaxKind, SyntaxNode};

/// How long an informational (auto-dismiss) notice stays on screen.
const INFO_NOTICE_TTL: Duration = Duration::from_secs(4);

/// The bundled showcase samples surfaced under **File ▸ Open Sample ▸** (E015).
///
/// Each entry is `(file_name, embedded_text)`: the text is shipped in the binary
/// via [`include_str!`] (path relative to this file: `../samples/<name>` —
/// `app.rs` is `src/ronin-app/src/`, so one `..` reaches the crate's `samples/`),
/// so a sample loads one-click in ANY working directory. The `file_name` becomes
/// the opened document's title and (for `.scn.ron`) drives extension-based Bevy
/// mode auto-detection. Authored + self-checked by `tests/showcase_samples.rs`.
const SHOWCASE_SAMPLES: &[(&str, &str)] = &[
    ("sample.ron", include_str!("../samples/sample.ron")),
    ("ships.ron", include_str!("../samples/ships.ron")),
    (
        "showcase_tree.ron",
        include_str!("../samples/showcase_tree.ron"),
    ),
    (
        "showcase_tables.ron",
        include_str!("../samples/showcase_tables.ron"),
    ),
    (
        "showcase_fallbacks.ron",
        include_str!("../samples/showcase_fallbacks.ron"),
    ),
    (
        "showcase_interop.ron",
        include_str!("../samples/showcase_interop.ron"),
    ),
    (
        "showcase_highlight.ron",
        include_str!("../samples/showcase_highlight.ron"),
    ),
    (
        "showcase_bevy.scn.ron",
        include_str!("../samples/showcase_bevy.scn.ron"),
    ),
    (
        "showcase_kitchen_sink.ron",
        include_str!("../samples/showcase_kitchen_sink.ron"),
    ),
];

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

/// Which defaults-elision direction an explicit user command runs (E009 US3 —
/// FR-014/FR-015).
///
/// Both directions share the [`run_elision`](App::run_elision) pipeline; this
/// distinguishes the pure entry point called and the user-facing wording for the
/// command's notices (the no-op status + the per-field skip advisory).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ElisionKind {
    /// Reduce verbosity (shrink): elide fields whose value equals the default.
    Reduce,
    /// Expand to explicit: materialize absent default-bearing fields.
    Expand,
}

impl ElisionKind {
    /// The user-facing command label.
    fn label(self) -> &'static str {
        match self {
            ElisionKind::Reduce => "Reduce verbosity",
            ElisionKind::Expand => "Expand to explicit",
        }
    }

    /// The informational status when nothing in scope was elidable/expandable
    /// (a zero-byte no-op, no undo unit — FR-014).
    fn noop_message(self) -> &'static str {
        match self {
            ElisionKind::Reduce => "Nothing to reduce: no field provably equals its default.",
            ElisionKind::Expand => "Nothing to expand: every default-bearing field is explicit.",
        }
    }

    /// The per-field skip advisory for this direction, or `None` when there is
    /// nothing to advise (FR-014/FR-015).
    ///
    /// On **expand** the relevant skips are `DefaultUnknownOnExpand` — a registry
    /// drift partial expand (a previously-elided field's default is no longer
    /// carried), which the user must be told about because exact recovery is then
    /// only via undo (FR-015). On **shrink** the relevant skips are
    /// `ValueDiffersFromDefault` — fields left explicit because their value did not
    /// equal the default (informational; FR-014).
    fn advisory_message(self, skipped: &[crate::bevy::SkippedField]) -> Option<String> {
        use crate::bevy::SkipReason;
        let reason = match self {
            ElisionKind::Reduce => SkipReason::ValueDiffersFromDefault,
            ElisionKind::Expand => SkipReason::DefaultUnknownOnExpand,
        };
        let names: Vec<String> = skipped
            .iter()
            .filter(|s| s.reason == reason)
            .map(|s| format!("{}.{}", s.type_path, s.field))
            .collect();
        if names.is_empty() {
            return None;
        }
        match self {
            ElisionKind::Expand => Some(format!(
                "Partial expand: {} field(s) left absent (registry default no longer known): {}. \
                 Exact pre-drift recovery is available via Undo.",
                names.len(),
                names.join(", ")
            )),
            ElisionKind::Reduce => Some(format!(
                "Left explicit (value differs from default): {} field(s): {}.",
                names.len(),
                names.join(", ")
            )),
        }
    }
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

/// The user's choice on a crash-recovery restore offer (E007 OBJ2 — TR-008).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryChoice {
    /// Restore the in-progress (autosaved) buffer from the recovery sidecar.
    Restore,
    /// Decline: open the on-disk file instead (the recovered content is not
    /// silently discarded — the user chose the on-disk version).
    Decline,
}

/// A live crash-recovery restore offer awaiting the user's decision (E007 OBJ2 —
/// TR-008, SC-003).
///
/// Raised on reopen when a **live, content-divergent** recovery sidecar is detected
/// for a file being opened (modelled on [`DirtyPrompt`]). Accepting restores the
/// in-progress buffer; declining opens the on-disk file. A stale / same-content
/// sidecar never produces an offer (TR-009).
#[derive(Debug, Clone)]
pub struct RecoveryOffer {
    /// The user file path that was being opened.
    pub path: PathBuf,
    /// The detected recovery sidecar holding the in-progress work.
    pub sidecar: crate::recovery::RecoverySidecar,
}

// ===========================================================================
// E010 US1 — RON→JSON convert command + loss-report dialog (FR-001/005/013).
// ===========================================================================

/// Where an in-flight RON→JSON conversion commits when the user confirms (E010
/// US1 — T014/T015/T016, FR-001/003).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvertTarget {
    /// Replace the active buffer in place as ONE E007 undo unit (FR-003); the
    /// converted buffer is a normal dirty E007 buffer (AD-005).
    InPlace,
    /// Write the converted JSON/JSONC to a chosen file, leaving the source document
    /// untouched (FR-003); a strict-mode sidecar is written as a deterministic
    /// sibling (FR-008).
    Export(PathBuf),
}

/// The per-conversion output-format override surfaced in the convert/loss-report
/// dialog (E010 US1 — T016, FR-008, NEW-CONFIG).
///
/// Defaults are seeded from the persisted
/// [`ConversionSettings`](crate::settings::ConversionSettings); the user may flip
/// JSONC↔strict (and the strict comment carrier) for a single conversion without
/// changing the persisted default. The override resolves to a [`CommentMode`] for
/// the converter + renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConvertFormatOverride {
    /// JSONC (comments inline) vs strict standard JSON for this conversion (FR-008).
    pub format: JsonFormat,
    /// In strict mode: carry comments via the sibling sidecar, or drop them (and
    /// report each drop as a loss) (FR-008).
    pub strict_carrier: StrictCommentCarrier,
}

impl ConvertFormatOverride {
    /// Resolve the effective [`CommentMode`] for this override (FR-008):
    ///
    /// * JSONC → [`CommentMode::JsoncInline`] (comments inline, the primary carrier);
    /// * strict + sidecar → [`CommentMode::Sidecar`] (comments survive via the
    ///   sibling map);
    /// * strict + pure → [`CommentMode::None`] (comments dropped → each reported).
    #[must_use]
    pub fn comment_mode(self) -> CommentMode {
        match (self.format, self.strict_carrier) {
            (JsonFormat::Jsonc, _) => CommentMode::JsoncInline,
            (JsonFormat::StrictJson, StrictCommentCarrier::Sidecar) => CommentMode::Sidecar,
            (JsonFormat::StrictJson, StrictCommentCarrier::PureNoComments) => CommentMode::None,
        }
    }
}

/// An in-flight RON→JSON conversion awaiting the user's confirm/cancel decision in
/// the loss-report dialog (E010 US1 — T016/T017, FR-005, data-model
/// §ConversionResult).
///
/// Built **read-only** over the source document — nothing is written until the user
/// confirms (FR-005, SC-002/003). The SAME [`loss_report`](Self::loss_report)
/// `constructs()` list drives BOTH this dialog AND the inline diagnostics (FR-007,
/// T017): the inline views are published onto the document the moment the dialog
/// opens, so a loss can never reach one surface but not the other.
pub struct PendingConversion {
    /// The active document index this conversion targets.
    doc_index: usize,
    /// The deterministic converted output text (JSON / JSONC), already serialized.
    text: String,
    /// The lossy-construct map — the single source of truth for both surfaces
    /// (FR-007).
    loss_report: LossReport,
    /// The comment carrier used (drives the strict-mode sidecar on an export).
    comments: CommentCarrier,
    /// Where the conversion commits on confirm (in-place / export).
    target: ConvertTarget,
    /// The per-conversion format override (the user may flip it in the dialog).
    format: ConvertFormatOverride,
}

/// The user's choice on the unparseable-RON block-vs-convert-remainder prompt
/// (E010 US1 — T016, FR-013, SC-008).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartialRonChoice {
    /// Abort the conversion with a clear error locating the offending region; no
    /// output, zero bytes written (FR-013, SC-008).
    Block,
    /// Convert the parseable remainder; each unparseable region is rendered as a
    /// flagged placeholder also recorded in the loss report (FR-013, SC-008).
    ConvertRemainder,
}

/// A live unparseable-RON prompt awaiting the block-vs-convert-remainder decision
/// (E010 US1 — T016, FR-013, SC-008).
///
/// Raised when a RON→JSON convert command is invoked on a buffer the parser could
/// not fully parse. Choosing block aborts (a clear error pointing at the first
/// offending region); choosing convert-remainder proceeds with the parseable
/// portion (the resolution re-runs the conversion build, which records each
/// unparseable region as an [`LossKind::UnparseableRegion`] loss).
#[derive(Debug, Clone)]
pub struct PartialRonPrompt {
    /// The active document index the prompt concerns.
    doc_index: usize,
    /// Where the conversion would commit if the user proceeds.
    target: ConvertTarget,
    /// The per-conversion format override seeded from settings.
    format: ConvertFormatOverride,
    /// The byte range of the first unparseable region (for the block-branch locate).
    first_error: ronin_core::TextRange,
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
    /// Whether the Settings window is open (FR-007/FR-023). Toggled from the menu;
    /// hosts the adjustable formatter controls (indent width, blank-line policy,
    /// format-on-save).
    show_settings: bool,
    /// The effective snippet set: built-ins overlaid by the user file (FR-017).
    ///
    /// Loaded at construction from the OS config dir; a missing/malformed user file
    /// degrades to built-ins-only and surfaces an explanatory notice (FR-017). The
    /// trigger/menu lists these for discoverability (FR-025).
    snippets: SnippetSet,
    /// Whether the Snippets browser window is open (FR-025). Lists each effective
    /// snippet's prefix + description and offers the open-user-file command.
    show_snippets: bool,
    /// The project-scoped binding configuration (glob → type + source) loaded from
    /// the active project's `.ronin/bindings.json` (E006 US2 — FR-008/FR-013).
    ///
    /// This is the **per-project** config — distinct from the OS-global
    /// [`AppSettings`]. It is loaded from / written to
    /// [`binding_root`](Self::binding_root)`/.ronin/bindings.json`; an absent or
    /// corrupt file degrades to an empty config (no rules → no-binding,
    /// structural-only) per [`BindingConfig::load_from`] (FR-013).
    binding_config: BindingConfig,
    /// The project root the [`binding_config`](Self::binding_config) was loaded from
    /// and is written back to (E006 US2 — FR-013).
    ///
    /// **Derivation choice (pragmatic):** RONin's shell has no explicit "open
    /// project" concept yet, so the project root is derived as the parent directory
    /// of the *first opened document with a path*, falling back to the current
    /// working directory when no such document exists (e.g. only untitled buffers).
    /// This keeps the binding config alongside the file the user is actually editing
    /// — the common single-folder-of-`.ron`-files case — without inventing a
    /// project-picker UI. A future "workspace root" concept (E008) can refine this;
    /// the binding core ([`crate::binding`]) is already root-agnostic. Once set from
    /// a document path it is **not** moved by later opens, so the config stays
    /// stable for the session.
    binding_root: PathBuf,
    /// Whether the binding-config window is open (E006 US2 — FR-009/FR-011). Hosts
    /// the per-document override control and the project rule editor.
    show_bindings: bool,
    /// Editable draft for the per-document override form (E006 US2 — FR-009). Pure
    /// UI input; committed into the active doc's `override_` via the override
    /// control. Never persisted.
    override_draft: crate::settings::BindingFormDraft,
    /// Editable draft for the project rule add/edit form (E006 US2 — FR-008). Pure
    /// UI input; committed into [`binding_config`](Self::binding_config). Never
    /// persisted (only the resulting config is).
    rule_draft: crate::settings::BindingFormDraft,
    /// The index of the rule currently being edited in-place, or `None` when the
    /// rule form is adding a new rule (E006 US2 — FR-008).
    editing_rule: Option<usize>,
    /// The off-frame autosave worker that performs recovery-sidecar writes off the
    /// per-frame path (E007 OBJ2 — TR-016/TR-023, SC-008).
    ///
    /// The frame loop runs only the cheap [`EditorDocument::should_autosave`] check
    /// (`maybe_autosave`); when it fires, a [`RecoverySidecar`](crate::recovery::RecoverySidecar)
    /// snapshot is handed to this worker, which does the atomic, crash-safe write
    /// (the `fsync` cost) on its own thread — never on the render thread.
    autosave_worker: crate::recovery::AutosaveWorker,
    /// A live crash-recovery restore offer, when one is open (E007 OBJ2 — TR-008).
    ///
    /// Raised on reopen when a live, divergent recovery sidecar is detected; the
    /// user accepts (restore in-progress work) or declines (open on-disk file).
    recovery_offer: Option<RecoveryOffer>,
    /// The project-scoped Bevy registry-binding configuration (glob →
    /// registry-export-path) loaded from the active project's
    /// `.ronin/bevy-registries.json` (E009 — FR-009/FR-010/FR-012).
    ///
    /// A **parallel** config to E006's [`binding_config`](Self::binding_config)
    /// (HINT-004): it maps scenes to a *registry export*, not a type + source. Loaded
    /// from / written to [`binding_root`](Self::binding_root) — the same project root
    /// E006 uses — but a **separate file**. An absent/corrupt file degrades to an empty
    /// config (no rules → auto-detect mode, no registry) per
    /// [`RegistryBindingConfig::load_from`] (FR-010, SC-002).
    registry_binding_config: RegistryBindingConfig,
    /// Whether the registry-binding-config window is open (E009 — FR-009/FR-011).
    /// Hosts the per-document mode toggle, the active-mode/registry indicator, and the
    /// project registry-rule editor.
    show_registries: bool,
    /// Editable draft for the registry-rule add/edit form (E009 — FR-010). Pure UI
    /// input; committed into [`registry_binding_config`](Self::registry_binding_config).
    /// Never persisted (only the resulting config is).
    registry_rule_draft: crate::settings::RegistryBindingFormDraft,
    /// The index of the registry rule currently being edited in-place, or `None` when
    /// the rule form is adding a new rule (E009 — FR-010).
    editing_registry_rule: Option<usize>,
    /// An in-flight RON→JSON conversion awaiting the loss-report confirm/cancel
    /// decision, or `None` when no convert dialog is open (E010 US1 — FR-005).
    ///
    /// Built read-only over the source; a Cancel discards it with zero side effects
    /// (SC-002/003). While set, the loss dialog renders and the inline loss
    /// diagnostics are published on the target document (one list → both surfaces,
    /// FR-007).
    pending_conversion: Option<PendingConversion>,
    /// A live unparseable-RON block-vs-convert-remainder prompt, or `None` (E010
    /// US1 — FR-013, SC-008).
    partial_ron_prompt: Option<PartialRonPrompt>,
}

/// The deferred tab-strip mutations collected during one [`render_tab_strip`] pass.
///
/// The tab loop must not reshape the document list mid-iteration, so the click /
/// reorder / close intents are collected here and applied by the caller after the
/// loop (preserving the existing deferred-mutation structure).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TabBarActions {
    /// A non-drag click selected this tab index (click-to-switch — FR-012).
    pub switch_to: Option<usize>,
    /// A tab was dropped over another: move `from` to `to` (drag-to-reorder).
    pub reorder: Option<(usize, usize)>,
    /// The per-tab close button was clicked for this index.
    pub close_idx: Option<usize>,
}

/// Render the horizontal tab strip for `workspace` and return the user's intents.
///
/// Pure rendering: it touches no document bytes and mutates no workspace state —
/// the caller applies the returned [`TabBarActions`] after the pass (FR-012).
///
/// Click-to-switch is detected by a **dedicated `Sense::click()` interact placed on
/// the tab rect with its own id**, *not* by reading `.clicked()` off the drag source
/// or its inner widget. In egui 0.34 `dnd_drag_source` registers a `Sense::drag()`
/// interaction on the same id/rect; that drag interaction wins pointer arbitration
/// and **swallows the click**, so neither `inner.inner.clicked()` nor
/// `inner.response.clicked()` ever fires (the regression this guards against — tabs
/// never switched). A second interact under a distinct id senses the click cleanly
/// in both the live app and the headless egui_kittest harness. The drag payload is
/// still read off the drag-source response.
pub fn render_tab_strip(ui: &mut egui::Ui, workspace: &EditorWorkspace) -> TabBarActions {
    let active = workspace.active_index();
    let mut actions = TabBarActions::default();

    ui.horizontal(|ui| {
        for idx in 0..workspace.len() {
            let Some(doc) = workspace.get(idx) else {
                continue;
            };
            let dot = if doc.dirty() { "\u{25CF} " } else { "" };
            let label = format!("{dot}{}", doc.title());
            let selected = active == Some(idx);

            let id = egui::Id::new(("ronin_tab", doc.id()));
            // `dnd_drag_source` returns `InnerResponse<R>`: `.inner` is the closure's
            // return (the `selectable_label` response), `.response` is the drag-source's
            // `Sense::drag()` response that carries the drag payload.
            let inner = ui.dnd_drag_source(id, idx, |ui| ui.selectable_label(selected, label));
            let rect = inner.response.rect;

            // Click-to-switch: the drag source's `Sense::drag()` interact on the same
            // id/rect swallows the click (egui 0.34), so neither response reports
            // `.clicked()`. Sense the click with a dedicated `Sense::click()` interact
            // under its OWN id over the same rect — it fires in both the live app and the
            // headless harness (the regression this guards against: tabs never switched).
            let click_id = id.with("ronin_tab_click");
            if ui.interact(rect, click_id, egui::Sense::click()).clicked() {
                actions.switch_to = Some(idx);
            }
            // Drag-to-reorder: if a tab payload is released over this tab, move the
            // dragged tab to this position (FR-012). Read off the drag-source response.
            if let Some(payload) = inner.response.dnd_release_payload::<usize>() {
                let from = *payload;
                if from != idx {
                    actions.reorder = Some((from, idx));
                }
            }

            if ui.small_button("\u{00D7}").clicked() {
                actions.close_idx = Some(idx);
            }
            ui.separator();
        }
    });

    actions
}

/// Render the always-visible type-indicator **legend strip** (E015).
///
/// A compact, glyph-only key for the shared [`TypeIndicator`](crate::structural::TypeIndicator)
/// system: each concept's canonical glyph at its theme-aware color (reusing
/// [`TypeIndicator::rich`](crate::structural::TypeIndicator::rich), so the legend uses
/// the SAME glyphs/colors the tree + table paint), with its full name on hover. It is
/// meant to be called inside a right-to-left layout on the existing view-switcher row
/// so it costs **zero** extra vertical space and right-aligns/clips gracefully when the
/// row is narrow (it never force-wraps into a new row).
///
/// In a right-to-left layout widgets are placed from the right edge leftward, so the
/// groups are emitted in reverse — status, then scalars, then containers — which lands
/// the containers group on the **left** and the status group on the **right** (the
/// natural reading order of [`TypeIndicator::ALL`](crate::structural::TypeIndicator::ALL)).
/// A small [`Ui::add_space`] separates the three groups.
/// The small gap inserted between the legend's three indicator groups (E015).
const LEGEND_GROUP_GAP: f32 = 10.0;

/// The approximate width the [`legend_strip`] needs: one fixed slot per glyph plus the
/// two inter-group gaps (and a little slack). Used by the view-switcher to decide whether
/// the legend fits on the tab row or must wrap to its own row (E021 — avoids the legend
/// overlapping the tabs).
fn legend_min_width() -> f32 {
    use crate::structural::TypeIndicator;
    TypeIndicator::ALL.len() as f32 * crate::structural::indicators::SLOT_WIDTH
        + 2.0 * LEGEND_GROUP_GAP
        + 8.0
}

fn legend_strip(ui: &mut egui::Ui) {
    use crate::structural::TypeIndicator;

    /// The small gap inserted between the three indicator groups.
    const GROUP_GAP: f32 = LEGEND_GROUP_GAP;

    let containers = TypeIndicator::CONTAINER_COUNT;
    let scalars = TypeIndicator::SCALAR_COUNT;
    // Group boundaries within `ALL`: [0, containers) = containers,
    // [containers, containers+scalars) = scalars, [..] = status.
    let scalar_end = containers + scalars;

    // Emit FORWARD (containers → scalars → status), inserting a gap *before* each new
    // group. Forward order reads left→right correctly in BOTH the same-row right-aligned
    // case (wrapped in a `left_to_right` inside a `right_to_left`) and the wrapped own-row
    // `left_to_right` case — so the legend never flips order when it wraps (E023).
    let all = TypeIndicator::ALL;
    for (i, &indicator) in all.iter().enumerate() {
        // A new group starts at `containers` (scalars) and `scalar_end` (status).
        if i == containers || i == scalar_end {
            ui.add_space(GROUP_GAP);
        }
        // Each legend glyph goes through the shared fixed-width slot (E014) so the
        // strip shares a uniform per-glyph slot + baseline with the views' icons.
        indicator.show(ui).on_hover_text(indicator.word());
    }
}

/// The egui font-data keys for the three bundled Noto fallback faces, in the order
/// they are appended to each family's fallback chain.
const NOTO_FALLBACK_FONTS: [&str; 3] = ["noto_symbols", "noto_symbols2", "noto_math"];

/// Build the [`egui::FontDefinitions`] that [`App::install_fonts`] applies.
///
/// Factored out of `set_fonts` so the chain membership is unit-testable without a
/// live [`egui::Context`]. It starts from the egui defaults, registers the three
/// bundled Noto faces (`include_bytes!` from `assets/`), then appends each — in
/// order — to the *end* (fallback position) of both the `Proportional` and
/// `Monospace` family chains. In egui 0.34 `font_data` values are `Arc<FontData>`,
/// so each `FontData` is converted with `.into()`.
#[must_use]
pub fn build_font_definitions() -> egui::FontDefinitions {
    let mut fonts = egui::FontDefinitions::default();

    fonts.font_data.insert(
        "noto_symbols".to_owned(),
        egui::FontData::from_static(include_bytes!("../assets/noto-symbols.ttf")).into(),
    );
    fonts.font_data.insert(
        "noto_symbols2".to_owned(),
        egui::FontData::from_static(include_bytes!("../assets/noto-symbols2.ttf")).into(),
    );
    fonts.font_data.insert(
        "noto_math".to_owned(),
        egui::FontData::from_static(include_bytes!("../assets/noto-math.ttf")).into(),
    );

    // Append all three to the END (fallback position) of both family chains so the
    // default fonts still drive normal text and only missing glyphs fall through.
    for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let chain = fonts
            .families
            .get_mut(&fam)
            .expect("egui default FontDefinitions always defines Proportional and Monospace");
        for name in NOTO_FALLBACK_FONTS {
            chain.push(name.to_owned());
        }
    }

    fonts
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
        // Load the effective snippet set from the OS config dir; a missing/malformed
        // user file degrades to built-ins and surfaces a notice below (FR-017).
        let snippets = SnippetSet::load();
        // Derive the initial project root from the CLI path's parent when given,
        // else the CWD; the binding config is loaded from `<root>/.ronin/bindings.json`
        // (FR-013). `open_file` below re-derives + reloads from the opened doc.
        let binding_root = cli_path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let binding_config =
            BindingConfig::load_from(&BindingConfig::project_config_path(&binding_root));
        // The parallel E009 registry-binding config (glob → registry-export) lives in
        // the SAME project root but a separate file; absent/corrupt → empty (FR-010).
        let registry_binding_config = crate::settings::load_registry_binding_config(&binding_root);
        let mut app = Self {
            workspace: EditorWorkspace::new(),
            worker: ReparseWorker::new(),
            settings,
            notices: Vec::new(),
            dirty_prompt: None,
            pending_batch: None,
            quit_requested: false,
            show_settings: false,
            snippets,
            show_snippets: false,
            binding_config,
            binding_root,
            show_bindings: false,
            override_draft: crate::settings::BindingFormDraft::default(),
            rule_draft: crate::settings::BindingFormDraft::default(),
            editing_rule: None,
            autosave_worker: crate::recovery::AutosaveWorker::new(),
            recovery_offer: None,
            registry_binding_config,
            show_registries: false,
            registry_rule_draft: crate::settings::RegistryBindingFormDraft::default(),
            editing_registry_rule: None,
            pending_conversion: None,
            partial_ron_prompt: None,
        };
        // Surface any snippet-file degrade notice once at startup (FR-017/FR-034).
        app.surface_snippet_notice();
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

    // --- E012 / US3: Table column-layout persistence sync (FR-007 / FR-015) ---

    /// The section paths whose Table column layout the persistence sync reconciles for
    /// the document at `idx`: the root section (the auto-routed structural view and the
    /// table seam's default) plus the selected table section (the table navigator's
    /// current level), de-duplicated.
    ///
    /// Returns an empty vec when the index is invalid. View-only / byte-free — reading
    /// the selection never touches the document.
    fn synced_sections(&self, idx: usize) -> Vec<crate::structural::view_state::StructuralPath> {
        use crate::structural::view_state::StructuralPath;
        let Some(doc) = self.workspace.get(idx) else {
            return Vec::new();
        };
        let mut sections = vec![StructuralPath::root()];
        if let Some(sel) = doc.view_state().selected_table_section() {
            if !sections.contains(sel) {
                sections.push(sel.clone());
            }
        }
        sections
    }

    /// Seed the live column view-state of the document at `idx` from the persisted
    /// settings for each shown section (E012 / US3 / FR-007 — the (a) load leg of T025).
    ///
    /// For each synced section it derives the live model's column count, then loads any
    /// persisted layout into the live view-state ONLY when no live state exists yet
    /// (first-show); stale / out-of-range indices are dropped on materialization. A
    /// no-op for an untitled document (no stable key). View-only / byte-free.
    fn load_active_column_layouts(&mut self, idx: usize) {
        for section in self.synced_sections(idx) {
            let Some(doc) = self.workspace.get_mut(idx) else {
                return;
            };
            // Already-live state → first-show load already happened; skip the model derive.
            if doc.view_state().column_view_state(&section).is_some() {
                continue;
            }
            let Some(model) = doc.cached_table_model_any(&section) else {
                continue; // not a table-able section at this path right now.
            };
            let model_cols = model.columns.len();
            crate::structural::load_persisted_column_layout(
                doc,
                &self.settings,
                &section,
                model_cols,
            );
        }
    }

    /// Persist the live column view-state of the document at `idx` back to settings for
    /// every section it customized this session, plus the shown sections (E012 / US3 —
    /// the (b) save leg of T025 and FR-015's reset-clears-persisted).
    ///
    /// A non-default live layout is stored; a section whose live state was reset/dropped
    /// is saved as the DEFAULT layout, which removes its persisted entry — so a reset
    /// returns the section to its first-seen default across launches (FR-015). A no-op
    /// for an untitled document. PRESENTATION config only — never a CST edit.
    fn save_active_column_layouts(&mut self, idx: usize) {
        let Some(doc) = self.workspace.get(idx) else {
            return;
        };
        // Sections with live customization PLUS the shown sections (so a just-reset
        // section, no longer live, still gets its persisted entry cleared).
        let mut sections = doc.view_state().column_view_state_paths();
        for s in self.synced_sections(idx) {
            if !sections.contains(&s) {
                sections.push(s);
            }
        }
        for section in sections {
            let Some(doc) = self.workspace.get(idx) else {
                return;
            };
            crate::structural::save_persisted_column_layout(doc, &mut self.settings, &section);
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

    /// The open document at tab index `idx`, if it exists (read-only; for tests and
    /// host integration of per-tab state such as the per-document
    /// [`ModeState`](crate::bevy::mode::ModeState) — FR-012).
    #[must_use]
    pub fn document_at(&self, idx: usize) -> Option<&EditorDocument> {
        self.workspace.documents().get(idx)
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

    /// The project-scoped binding configuration, read-only (E006 US2 — FR-008).
    ///
    /// This is the per-project glob→type+source mapping (NOT the OS-global
    /// [`AppSettings`]). Loaded from / written to
    /// [`binding_root`](Self::binding_root)`/.ronin/bindings.json` (FR-013).
    #[must_use]
    pub fn binding_config(&self) -> &BindingConfig {
        &self.binding_config
    }

    /// The project root the binding config is loaded from / written to (FR-013).
    #[must_use]
    pub fn binding_root(&self) -> &Path {
        &self.binding_root
    }

    /// Whether the binding-config / override window is open (for tests/hosts).
    #[must_use]
    pub fn bindings_open(&self) -> bool {
        self.show_bindings
    }

    /// Open or close the binding-config / override window (E006 US2 — FR-009/FR-011).
    pub fn set_bindings_open(&mut self, open: bool) {
        self.show_bindings = open;
    }

    /// Resolve + acquire the active document's binding and (re-)apply it (E006 US2 —
    /// FR-011/FR-014).
    ///
    /// Takes the active document's path + its per-document override + the project
    /// [`binding_config`](Self::binding_config), runs [`resolve_and_acquire`], and:
    ///
    /// 1. stores the resolved [`TypeBinding`](crate::binding::TypeBinding) on the doc
    ///    for **display** (the active-binding indicator reads this — FR-011), and
    /// 2. installs the acquired [`Option<BoundType>`](crate::reparse::BoundType) into
    ///    `doc.bound_type` so the off-frame worker validates against it, then
    /// 3. bumps the edit generation and requests an off-frame reparse so type
    ///    validation re-runs against the new (or absent) binding immediately.
    ///
    /// This is invoked when a document becomes active / is opened and on the explicit
    /// override / config edits below. It is the single re-resolve + re-acquire +
    /// re-validate primitive shared by the FR-021 trigger set (T037): a binding
    /// change (becomes-active / open), a per-document override change
    /// ([`set_active_override`](Self::set_active_override) /
    /// [`clear_active_override`](Self::clear_active_override)), and the explicit
    /// type-info re-acquire ([`reacquire_active_binding`](Self::reacquire_active_binding))
    /// all route through here so recomputation is **immediate** — never deferred to
    /// the next edit — and no stale type finding survives (the worker's full-set
    /// replace on the next landed result, driven by the reparse requested here,
    /// guarantees this). The `BindingConfig`-change trigger walks every open doc via
    /// [`reapply_bindings_to_all_docs`](Self::reapply_bindings_to_all_docs); the
    /// document-edit trigger is the editor's own `on_edit` → `request_reparse` (the
    /// bound model travels with each request via
    /// [`EditorDocument::request_reparse`](crate::document::EditorDocument::request_reparse)).
    /// A no-op when no tab is open.
    pub fn apply_binding_to_active(&mut self) {
        let Some(idx) = self.workspace.active_index() else {
            return;
        };
        // Snapshot the inputs `resolve_and_acquire` needs (path + override) so the
        // borrow of `self.binding_config` does not collide with the later mut borrow
        // of the document.
        let Some(doc) = self.workspace.get(idx) else {
            return;
        };
        let path = doc.path.clone();
        let override_ = doc.override_.clone();
        // Thread the project root so an out-of-project / path-traversal type_source
        // degrades that binding to structural-only and is never read (FR-025).
        let (binding, bound) = resolve_and_acquire(
            &self.binding_config,
            path.as_deref(),
            override_.as_ref(),
            &self.binding_root,
        );
        let threshold = self.settings.effective_large_file_threshold();
        let Some(doc) = self.workspace.get_mut(idx) else {
            return;
        };
        doc.binding = binding;
        doc.bound_type = bound;
        // Degrade type validation on E003's oversize signal, exactly like
        // highlighting/squiggles (T040 — FR-015/FR-024): an oversize document ships
        // no bound type to the worker, so it produces zero type diagnostics. Set the
        // flag here so the immediate reparse below already honors it (the per-frame
        // pump also keeps it reconciled as the buffer changes).
        doc.validation_suppressed = doc.oversize(threshold);
        // Re-run validation against the (possibly changed) binding immediately.
        doc.on_edit();
        doc.request_reparse(&self.worker);
    }

    /// Explicitly re-acquire the active document's bound `TypeModel` from its source
    /// and re-validate — the **type-info change** trigger of FR-021 (b) / FR-014
    /// (T037).
    ///
    /// FR-021 (b) requires that when the bound `type_source` is re-acquired and
    /// yields a different [`TypeModel`](ronin_types::TypeModel) the document
    /// re-validates against it. RONin does **not** auto-watch the filesystem for an
    /// externally edited source file in this MVP (filesystem-watch auto-detection is
    /// out of scope); instead this is the explicit, on-demand re-acquire the user (or
    /// host) invokes to pick up a changed source/mapping. It re-runs
    /// [`resolve_and_acquire`] for the active document (re-reading the `type_source`
    /// from disk through E004) and drives an immediate off-frame reparse, so a source
    /// that changed since it was last acquired is picked up and re-validated now —
    /// not deferred to the next edit.
    ///
    /// Mechanically this is [`apply_binding_to_active`](Self::apply_binding_to_active)
    /// (the shared re-resolve + re-acquire + re-validate primitive); it is exposed
    /// under an intent-revealing name so the type-info re-acquire trigger is a
    /// first-class, callable action. A no-op when no tab is open.
    pub fn reacquire_active_binding(&mut self) {
        self.apply_binding_to_active();
    }

    /// Re-acquire **every** open document's bound `TypeModel` from its source and
    /// re-validate (E006 US3 — FR-014/FR-021 (b)).
    ///
    /// The all-documents companion to
    /// [`reacquire_active_binding`](Self::reacquire_active_binding): when the user
    /// asks to reload types after editing a shared source/schema, every open doc
    /// re-runs acquisition and re-validates immediately. Delegates to
    /// [`reapply_bindings_to_all_docs`](Self::reapply_bindings_to_all_docs).
    pub fn reload_types(&mut self) {
        self.reapply_bindings_to_all_docs();
    }

    /// Set the active document's per-document override and re-apply its binding
    /// (E006 US2 — FR-009).
    ///
    /// The override binds the active document to `type_name` from `type_source` for
    /// the session, taking precedence over any project rule (override > config). It
    /// is never persisted. After setting it, [`apply_binding_to_active`](Self::apply_binding_to_active)
    /// re-resolves so the override takes effect immediately. A no-op when no tab is
    /// open.
    pub fn set_active_override(&mut self, type_name: String, type_source: TypeSourceLocator) {
        let Some(doc) = self.workspace.active_document_mut() else {
            return;
        };
        doc.override_ = Some(DocumentOverride {
            type_name,
            type_source,
        });
        self.apply_binding_to_active();
    }

    /// Clear the active document's per-document override and re-apply its binding
    /// (E006 US2 — FR-009).
    ///
    /// After clearing, resolution falls back to the project config (or no binding),
    /// applied immediately via [`apply_binding_to_active`](Self::apply_binding_to_active).
    /// A no-op when no tab is open or no override was set.
    pub fn clear_active_override(&mut self) {
        let Some(doc) = self.workspace.active_document_mut() else {
            return;
        };
        if doc.override_.is_none() {
            return;
        }
        doc.override_ = None;
        self.apply_binding_to_active();
    }

    // --- E009: per-document mode + registry binding (FR-009/011/012/013) -------

    /// The project-scoped Bevy registry-binding configuration, read-only (E009 —
    /// FR-010).
    ///
    /// The parallel-to-E006 glob→registry-export mapping (NOT the OS-global
    /// [`AppSettings`], NOT E006's [`binding_config`](Self::binding_config)). Loaded
    /// from / written to [`binding_root`](Self::binding_root)`/.ronin/bevy-registries.json`.
    #[must_use]
    pub fn registry_binding_config(&self) -> &RegistryBindingConfig {
        &self.registry_binding_config
    }

    /// The active document's active [`Mode`] (`{Serde, Bevy}`), or `None` when no tab
    /// is open (E009 — FR-009/FR-013; for tests/hosts).
    #[must_use]
    pub fn active_mode(&self) -> Option<Mode> {
        self.active_document().map(|d| d.mode_state().active_mode())
    }

    /// The always-visible active-mode indicator label for the active document, or a
    /// "no document open" placeholder (E009 — FR-011; for tests/hosts).
    #[must_use]
    pub fn mode_indicator_label(&self) -> String {
        self.active_document().map_or_else(
            || "no document open".to_string(),
            EditorDocument::mode_label,
        )
    }

    /// The always-visible bound-registry indicator label for the active document, or a
    /// "no document open" placeholder (E009 — FR-011; for tests/hosts).
    #[must_use]
    pub fn registry_indicator_label(&self) -> String {
        self.active_document().map_or_else(
            || "no document open".to_string(),
            EditorDocument::registry_label,
        )
    }

    /// Whether the registry-binding-config / mode window is open (for tests/hosts).
    #[must_use]
    pub fn registries_open(&self) -> bool {
        self.show_registries
    }

    /// Open or close the registry-binding-config / mode window (E009 — FR-009/FR-011).
    pub fn set_registries_open(&mut self, open: bool) {
        self.show_registries = open;
    }

    /// Resolve + load the active document's [`ModeState`] from the project
    /// [`registry_binding_config`](Self::registry_binding_config) (extension
    /// auto-detect → per-pattern hint → project default), honoring any explicit
    /// per-document mode override the document already carries, then re-validate
    /// (E009 — FR-009/FR-011/FR-012/FR-013).
    ///
    /// This is the E009 analogue of
    /// [`apply_binding_to_active`](Self::apply_binding_to_active): it (1) re-resolves
    /// the document's mode + registry binding against the current project config,
    /// (2) loads the bound registry export **read-only as data** (degrading to
    /// `NoRegistry` on any failure — never a crash, SC-002), and (3) requests an
    /// off-frame reparse so validation re-runs under the (possibly new) mode's source.
    ///
    /// **Preserving an explicit toggle:** when the document's current mode origin is
    /// [`ModeOrigin::Override`](crate::bevy::mode::ModeOrigin), the user has explicitly
    /// chosen a mode via the toggle — that choice is carried through so re-resolution
    /// (e.g. after a becomes-active switch) never silently reverts it. Otherwise the
    /// mode is re-derived from the extension / config.
    ///
    /// Changes **zero** document bytes (mode/registry resolution is a behavior
    /// selection, not an edit — FR-011). A no-op when no tab is open.
    pub fn apply_mode_to_active(&mut self) {
        let Some(idx) = self.workspace.active_index() else {
            return;
        };
        let Some(doc) = self.workspace.get(idx) else {
            return;
        };
        let path = doc.path.clone();
        // Carry an explicit toggle through a re-resolve so it is never reverted.
        let mode_override = match doc.mode_state().mode_origin() {
            crate::bevy::mode::ModeOrigin::Override => Some(doc.mode_state().active_mode()),
            crate::bevy::mode::ModeOrigin::AutoDetected => None,
        };
        let mut state = ModeState::resolve(
            &self.registry_binding_config,
            path.as_deref(),
            mode_override,
            None,
        );
        // Load the bound registry read-only as data (NoRegistry on any failure).
        state.load_registry(&self.binding_root);
        let Some(doc) = self.workspace.get_mut(idx) else {
            return;
        };
        doc.set_mode_state(state);
        // Re-validate under the (possibly new) mode's source without touching bytes.
        doc.revalidate(&self.worker);
    }

    /// Toggle the active document's mode serde ⇄ Bevy via an explicit per-document
    /// override, then re-resolve its registry binding + re-validate (E009 —
    /// FR-009/FR-011/FR-013, SC-003).
    ///
    /// The explicit toggle **overrides** extension auto-detect
    /// ([`ModeState::set_mode_override`](crate::bevy::mode::ModeState::set_mode_override))
    /// — a Bevy scene saved as `.ron`, or a non-scene `.scn.ron`, is handled here
    /// (FR-009). After flipping the mode it re-resolves the bound registry (so a Bevy
    /// toggle picks up the per-pattern registry / loads it) and re-validates, all
    /// **without changing a single document byte** (FR-011, SC-003): the toggle only
    /// flips which validator runs. A no-op when no tab is open.
    pub fn toggle_active_mode(&mut self) {
        let Some(doc) = self.active_document() else {
            return;
        };
        let next = match doc.mode_state().active_mode() {
            Mode::Serde => Mode::Bevy,
            Mode::Bevy => Mode::Serde,
        };
        self.set_active_mode(next);
    }

    /// Set the active document's mode to `mode` via an explicit per-document override,
    /// re-resolve its registry binding, and re-validate (E009 — FR-009/FR-011/FR-013,
    /// SC-003).
    ///
    /// Sets the override (which wins over extension auto-detect, FR-009), then routes
    /// through [`apply_mode_to_active`](Self::apply_mode_to_active) to re-resolve the
    /// bound registry (loading it for a Bevy toggle) and re-validate. Changes **zero**
    /// document bytes (FR-011, SC-003). A no-op when no tab is open or the document is
    /// already in `mode`.
    pub fn set_active_mode(&mut self, mode: Mode) {
        let Some(doc) = self.workspace.active_document_mut() else {
            return;
        };
        if doc.mode_state().active_mode() == mode
            && doc.mode_state().mode_origin() == crate::bevy::mode::ModeOrigin::Override
        {
            return;
        }
        // Flip the mode via the explicit override (wins over auto-detect, FR-009);
        // re-resolution below preserves it because the origin is now Override.
        doc.mode_state_mut().set_mode_override(mode);
        self.apply_mode_to_active();
    }

    // --- E009 US3: defaults-elision commands (FR-014/FR-015/FR-016) ------------

    /// `true` iff the active document is in Bevy mode **with a loaded registry** —
    /// the precondition that gates both elision commands (E009 US3 — FR-014/FR-015).
    ///
    /// Both "Reduce verbosity" and "Expand to explicit" are **explicit user
    /// commands** (never automatic) and are enabled only here: a serde-mode document,
    /// a Bevy document with no registry resolved, or a Bevy document whose bound
    /// registry degraded to `NoRegistry` cannot supply the per-type concrete defaults
    /// the provable-default rule needs, so the commands are disabled (the menu shape
    /// stays stable). Exposed for tests/hosts to assert the gate.
    #[must_use]
    pub fn elision_available(&self) -> bool {
        self.active_document()
            .is_some_and(|d| d.is_bevy_mode() && d.mode_state().has_registry())
    }

    /// **Reduce verbosity** — elide every in-scope field whose value provably equals
    /// its known registry default, as ONE undo unit (E009 US3 — T029, FR-014/FR-016).
    ///
    /// An explicit, never-automatic command (FR-014). It is a no-op unless the active
    /// document is in Bevy mode with a loaded registry ([`elision_available`](Self::elision_available)).
    /// `scope` selects whole-document (the default) or a single entity / the current
    /// selection ([`Scope::Entity`]). See
    /// [`run_elision`](Self::run_elision) for the shared pipeline.
    pub fn reduce_verbosity_active(&mut self, scope: crate::bevy::Scope) {
        self.run_elision(ElisionKind::Reduce, scope);
    }

    /// **Expand to explicit** — materialize every in-scope registered default-bearing
    /// field currently absent whose default is known, as ONE undo unit (E009 US3 —
    /// T029, FR-015/FR-016).
    ///
    /// An explicit, never-automatic command (FR-015). No-op unless the active document
    /// is in Bevy mode with a loaded registry. On **registry drift** (a previously-
    /// elided field's default no longer carried) it performs a **partial expand** and
    /// surfaces the skipped fields as an advisory (FR-015). See
    /// [`run_elision`](Self::run_elision) for the shared pipeline.
    pub fn expand_to_explicit_active(&mut self, scope: crate::bevy::Scope) {
        self.run_elision(ElisionKind::Expand, scope);
    }

    /// The shared shrink/expand command pipeline (E009 US3 — T029, FR-014/015/016).
    ///
    /// Mirrors the 4B-2 command idiom: gate on the Bevy-mode + loaded-registry
    /// precondition, then parse the active document's live buffer → derive the
    /// [`SceneModel`](crate::bevy::SceneModel) from that same CST → call the pure
    /// elision entry point against the document's loaded
    /// [`BevyRegistry`](ronin_types::BevyRegistry). On
    /// [`ElisionOutcome::Applied`](crate::bevy::ElisionOutcome::Applied) the resulting
    /// CST is committed as exactly **one** E007 undo unit via
    /// [`EditorDocument::commit_transformed_cst`] (so a single undo restores the exact
    /// prior bytes — SC-006); any per-field skip advisories (the partial-expand-on-
    /// drift list, or value-differs notes) are surfaced through the existing transient
    /// [`push_authoring_notice`](Self::push_authoring_notice) channel. A
    /// [`NoOp`](crate::bevy::ElisionOutcome::NoOp) changes **zero** bytes and pushes
    /// no undo unit, only an informational "nothing to do" notice (FR-014).
    fn run_elision(&mut self, kind: ElisionKind, scope: crate::bevy::Scope) {
        use crate::bevy::{expand_to_explicit, reduce_verbosity, ElisionOutcome, SceneModel};

        let Some(idx) = self.workspace.active_index() else {
            return;
        };
        let Some(doc) = self.workspace.get(idx) else {
            return;
        };
        // Gate: explicit command, Bevy mode + a loaded registry only (FR-014/FR-015).
        if !doc.is_bevy_mode() {
            self.push_authoring_notice(
                NoticeKind::Error,
                format!("{} is only available in Bevy mode.", kind.label()),
            );
            return;
        }
        let Some(registry) = doc.mode_state().registry() else {
            self.push_authoring_notice(
                NoticeKind::Error,
                format!("{} needs a loaded Bevy registry.", kind.label()),
            );
            return;
        };

        // Parse the live buffer once and derive the scene model from that same CST so
        // every elision target maps to a real, current span (FR-014).
        let cst = ronin_core::parse(&doc.buffer);
        let model = SceneModel::from_cst(&cst);
        let outcome: ElisionOutcome = match kind {
            ElisionKind::Reduce => reduce_verbosity(&cst, &model, registry, scope),
            ElisionKind::Expand => expand_to_explicit(&cst, &model, registry, scope),
        };

        // Surface the per-field skip advisories (the partial-expand drift list on
        // expand; value-differs notes on shrink) before committing — they explain why
        // a decided-on field was left untouched (FR-015). Never a blocking modal.
        if let Some(advisory) = kind.advisory_message(outcome.skipped()) {
            self.push_authoring_notice(NoticeKind::Info, advisory);
        }

        match outcome {
            // Applied: commit the whole transformed CST as ONE undo unit (FR-016).
            ElisionOutcome::Applied { document, .. } => {
                let Some(doc) = self.workspace.get_mut(idx) else {
                    return;
                };
                doc.commit_transformed_cst(&document, &self.worker, Instant::now());
            }
            // No-op: zero bytes, no undo entry — only an informational status (FR-014).
            ElisionOutcome::NoOp { .. } => {
                self.push_authoring_notice(NoticeKind::Info, kind.noop_message());
            }
        }
    }

    // --- E010 US1: RON→JSON convert commands (FR-001/003/005/007/013) ----------

    /// `true` iff a document is open to convert (the gate for the Convert menu).
    #[must_use]
    pub fn convert_available(&self) -> bool {
        self.active_document().is_some()
    }

    /// The per-conversion format override seeded from the persisted
    /// [`ConversionSettings`](crate::settings::ConversionSettings) (E010 US1 —
    /// FR-008, NEW-CONFIG).
    #[must_use]
    fn default_format_override(&self) -> ConvertFormatOverride {
        ConvertFormatOverride {
            format: self.settings.conversion.default_format,
            strict_carrier: self.settings.conversion.strict_default_comment_carrier,
        }
    }

    /// **Convert to JSON (in place)** — replace the active buffer with its JSON/JSONC
    /// projection as ONE E007 undo unit, after the loss-report confirm (E010 US1 —
    /// T014/T016, FR-001/003/005).
    ///
    /// The source bytes are unchanged until the user confirms; a Cancel changes zero
    /// bytes (SC-002/003). A single Undo restores the exact prior RON (FR-003).
    pub fn convert_to_json_in_place(&mut self) {
        let format = self.default_format_override();
        self.begin_conversion(ConvertTarget::InPlace, format);
    }

    /// **Export to JSON…** — pick a target via the file dialog and convert RON→JSON
    /// non-destructively (the source document is untouched), after the loss-report
    /// confirm (E010 US1 — T015/T016, FR-001/003/008).
    ///
    /// The `rfd` dialog cannot run headless, so the post-dialog flow (build +
    /// confirm + atomic write) is also reachable directly via
    /// [`begin_conversion`](Self::begin_conversion) with a chosen
    /// [`ConvertTarget::Export`] for tests.
    pub fn export_to_json_active(&mut self) {
        if self.active_document().is_none() {
            return;
        }
        let default = self.settings.conversion.default_format;
        let extension = if default.is_jsonc() { "jsonc" } else { "json" };
        let Some(target) = rfd::FileDialog::new()
            .add_filter("JSON", &["json", "jsonc"])
            .set_file_name(format!("export.{extension}"))
            .save_file()
        else {
            return;
        };
        let format = self.default_format_override();
        self.begin_conversion(ConvertTarget::Export(target), format);
    }

    /// Build a RON→JSON conversion read-only over the active document and either
    /// open the loss-report dialog (when lossy) or commit immediately (when within
    /// the round-trip-safe tier) (E010 US1 — T016/T017, FR-005/007/013).
    ///
    /// The shared entry point for both in-place and export commands (and for tests,
    /// which call it directly to sidestep the `rfd` dialog). Flow:
    ///
    /// 1. **Unparseable-RON gate (FR-013, SC-008).** If the live buffer has parse
    ///    errors, raise the block-vs-convert-remainder prompt instead of converting;
    ///    the user's choice routes back here (block → abort + locate;
    ///    convert-remainder → re-enter with the error tolerated).
    /// 2. **Build read-only (FR-005, §I).** Run [`ron_to_json`] + serialize the
    ///    output text + collect the loss report — all without touching a source byte.
    /// 3. **Publish the inline losses (FR-007, T017).** Map the SAME
    ///    `loss_report.constructs()` list onto the target document's diagnostics, so
    ///    the inline surface and the dialog are driven by one list.
    /// 4. **Confirm gate (FR-005).** A lossy conversion stashes a
    ///    [`PendingConversion`] and opens the dialog; a loss-free conversion commits
    ///    immediately (nothing to confirm — the round-trip-safe tier).
    pub fn begin_conversion(&mut self, target: ConvertTarget, format: ConvertFormatOverride) {
        self.begin_conversion_inner(target, format, false);
    }

    /// The conversion builder; `tolerate_errors` is set when re-entered from the
    /// convert-remainder branch so the unparseable-RON gate is skipped (FR-013).
    fn begin_conversion_inner(
        &mut self,
        target: ConvertTarget,
        format: ConvertFormatOverride,
        tolerate_errors: bool,
    ) {
        let Some(idx) = self.workspace.active_index() else {
            return;
        };
        let Some(doc) = self.workspace.get(idx) else {
            return;
        };

        // 1. Unparseable-RON gate: prompt block vs convert-remainder (FR-013/SC-008).
        let cst = ronin_core::parse(&doc.buffer);
        if !tolerate_errors {
            if let Some(first) = cst.diagnostics().first() {
                self.partial_ron_prompt = Some(PartialRonPrompt {
                    doc_index: idx,
                    target,
                    format,
                    first_error: first.range,
                });
                return;
            }
        }

        // 2. Build the conversion READ-ONLY over the source (no byte changes yet).
        let comment_mode = format.comment_mode();
        // US1 keeps the binding `None` (best-effort emit conventions); US2 wires the
        // schema-aware binding. The loss report's recovery flag reflects unbound.
        let converted = ron_to_json(&cst, None, comment_mode);
        let mut loss_report = converted.loss_report;

        // Convert-remainder branch: each unparseable region is a flagged placeholder
        // ALSO recorded in the loss report (FR-013, SC-008) — never silently dropped.
        if tolerate_errors {
            for diag in cst.diagnostics() {
                loss_report.push(LossyConstruct::with_detail(
                    LossKind::UnparseableRegion,
                    diag.range,
                    LossRecovery::LossyToExternal,
                    "unparseable RON region — emitted as a flagged placeholder",
                ));
            }
        }

        let style = JsoncStyle::from_comment_mode(comment_mode);
        let indent = self.settings.conversion.effective_json_indent() as usize;
        let text = render_json(&converted.value, &converted.comments, indent, style);

        // 3. Publish the inline losses from the SAME list (FR-007, T017).
        let source = doc.buffer.clone();
        if let Some(doc) = self.workspace.get_mut(idx) {
            doc.diagnostics = map_loss_report(&loss_report, &source);
        }

        // 4. Confirm gate: lossy → dialog; loss-free → commit now (round-trip-safe).
        if loss_report.requires_confirmation() {
            self.pending_conversion = Some(PendingConversion {
                doc_index: idx,
                text,
                loss_report,
                comments: converted.comments,
                target,
                format,
            });
        } else {
            self.commit_conversion(idx, text, &converted.comments, &target, format);
        }
    }

    /// The loss-report dialog's per-conversion format override is in flight when a
    /// dialog is open; this exposes whether the convert/loss dialog is showing (for
    /// tests / hosts).
    #[must_use]
    pub fn conversion_pending(&self) -> bool {
        self.pending_conversion.is_some()
    }

    /// `true` when the unparseable-RON block-vs-convert-remainder prompt is open
    /// (E010 US1 — FR-013, SC-008).
    #[must_use]
    pub fn partial_ron_prompt_open(&self) -> bool {
        self.partial_ron_prompt.is_some()
    }

    /// The per-kind loss counts of the pending conversion, for the dialog summary +
    /// tests (E010 US1 — FR-005, SC-002). Empty when no dialog is open.
    #[must_use]
    pub fn pending_conversion_counts(&self) -> std::collections::BTreeMap<LossKind, usize> {
        self.pending_conversion
            .as_ref()
            .map(|c| c.loss_report.counts_by_kind())
            .unwrap_or_default()
    }

    /// Resolve the open loss-report dialog (E010 US1 — T016/T017, FR-005, SC-002/003).
    ///
    /// * `confirm == true` → commit the conversion (in-place one undo unit, or the
    ///   non-destructive atomic export). The bytes change for the first time here.
    /// * `confirm == false` → **Cancel**: discard the pending conversion with zero
    ///   side effects — no document is modified and no file is written (FR-005,
    ///   SC-002/003). The inline loss diagnostics fall away on the next reparse.
    pub fn resolve_conversion(&mut self, confirm: bool) {
        let Some(pending) = self.pending_conversion.take() else {
            return;
        };
        if !confirm {
            // Cancel: zero bytes changed, nothing written (SC-002/003). Clear the
            // inline loss diagnostics by requesting a fresh reparse of the unchanged
            // buffer (the loss views were transient).
            if let Some(doc) = self.workspace.get_mut(pending.doc_index) {
                doc.request_reparse(&self.worker);
            }
            return;
        }
        self.commit_conversion(
            pending.doc_index,
            pending.text,
            &pending.comments,
            &pending.target,
            pending.format,
        );
    }

    /// Commit a (confirmed or loss-free) conversion to its target (E010 US1 —
    /// T014/T015, FR-003/008).
    ///
    /// In-place → install the converted text as ONE E007 undo unit
    /// ([`EditorDocument::commit_converted_text`]); export → write atomically via
    /// [`export_json`] with the strict-mode sidecar, leaving the source untouched.
    fn commit_conversion(
        &mut self,
        idx: usize,
        text: String,
        comments: &CommentCarrier,
        target: &ConvertTarget,
        format: ConvertFormatOverride,
    ) {
        match target {
            ConvertTarget::InPlace => {
                let Some(doc) = self.workspace.get_mut(idx) else {
                    return;
                };
                // One E007 undo unit; a single Undo restores the exact prior RON.
                doc.commit_converted_text(text, &self.worker, Instant::now());
                self.push_authoring_notice(
                    NoticeKind::Info,
                    "Converted to JSON in place (one Undo restores the original RON).",
                );
            }
            ConvertTarget::Export(path) => {
                // Strict + sidecar carries comments in the deterministic sibling map;
                // JSONC carries them inline (no sidecar); pure-JSON writes none.
                let sidecar = if format.comment_mode() == CommentMode::Sidecar {
                    Some(comments.sidecar_map())
                } else {
                    None
                };
                match export_json(&text, path, sidecar.as_ref()) {
                    Ok(()) => {
                        self.push_authoring_notice(
                            NoticeKind::Info,
                            format!("Exported JSON to {} (source unchanged).", path.display()),
                        );
                    }
                    Err(e) => self.push_export_error(&e),
                }
            }
        }
    }

    /// Push an error notice for a failed JSON export (E010 US1 — T015, FR-003).
    ///
    /// A failed export never touches the source document; the notice names the
    /// failure surface (atomic-save / sidecar / unsafe sidecar path).
    fn push_export_error(&mut self, err: &ExportError) {
        self.push_authoring_notice(NoticeKind::Error, format!("Export failed: {err}"));
    }

    /// Resolve the open unparseable-RON prompt (E010 US1 — T016, FR-013, SC-008).
    ///
    /// * [`PartialRonChoice::Block`] → abort with a clear error locating the first
    ///   offending region; no output, zero bytes (SC-008).
    /// * [`PartialRonChoice::ConvertRemainder`] → re-enter the conversion build with
    ///   parse errors tolerated; each unparseable region becomes a flagged
    ///   [`LossKind::UnparseableRegion`] placeholder also recorded in the loss
    ///   report (SC-008).
    pub fn resolve_partial_ron_prompt(&mut self, choice: PartialRonChoice) {
        let Some(prompt) = self.partial_ron_prompt.take() else {
            return;
        };
        match choice {
            PartialRonChoice::Block => {
                // Abort + locate: a clear, non-crashing error pointing at the region.
                let start = prompt.first_error.start();
                self.push_authoring_notice(
                    NoticeKind::Error,
                    format!(
                        "Conversion blocked: the RON has an unparseable region at byte {start}. \
                         Fix it, or choose \"Convert remainder\" to emit the parseable portion."
                    ),
                );
            }
            PartialRonChoice::ConvertRemainder => {
                // Re-enter with errors tolerated, keeping the original target/format.
                let _ = prompt.doc_index;
                self.begin_conversion_inner(prompt.target, prompt.format, true);
            }
        }
    }

    // --- E010 US2: JSON→RON convert commands (FR-002/008/009/013) ---------------

    /// Build the schema-aware reconstruction consultation from the **active**
    /// document's bound type, when one resolved (E010 US2 — T021/T024, FR-009).
    ///
    /// The active document carries its bound type as a serialized E004 interchange
    /// (`bound_type`); when present this deserializes it into a consultable
    /// [`JsonToRonConsultation`] so JSON→RON reconstructs the RON-specific shapes
    /// schema-aware. `None` ⇒ no type bound ⇒ the converter applies the documented
    /// best-effort mapping (FR-009, schema-optional / §III).
    #[must_use]
    fn active_reconstruction_consultation(&self) -> Option<JsonToRonConsultation> {
        let doc = self.active_document()?;
        let bound = doc.bound_type.as_ref()?;
        JsonToRonConsultation::from_serialized_model(&bound.model, &bound.type_name)
    }

    /// **Import JSON / JSONC…** — pick a JSON/JSONC file and reconstruct it into a
    /// **new** editor tab, leaving the source file untouched (E010 US2 — T023/T024,
    /// FR-002/008/009/013).
    ///
    /// The `rfd` dialog cannot run headless, so the post-pick flow (read + sidecar
    /// read-back + reconstruct + open-new-tab) is also reachable directly via
    /// [`import_json_path`](Self::import_json_path) for tests. Malformed JSON / a
    /// non-UTF-8 file surfaces a clear error notice and creates **no** tab (FR-013).
    pub fn import_json_to_new_tab(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("JSON", &["json", "jsonc"])
            .pick_file()
        else {
            return;
        };
        self.import_json_path(&path);
    }

    /// Import a JSON/JSONC file at `path` into a new tab (the headless-reachable core
    /// of [`import_json_to_new_tab`]) (E010 US2 — T023/T024, FR-002/013).
    ///
    /// Reads + reconstructs schema-aware when a type is bound to the active document,
    /// best-effort otherwise (FR-009); the reconstructed RON opens in a fresh dirty
    /// untitled tab and the source JSON is never modified (FR-002). A read /
    /// malformed-JSON / non-UTF-8 failure surfaces a clear error notice and creates
    /// **no** tab (FR-013).
    pub fn import_json_path(&mut self, path: &std::path::Path) {
        let consultation = self.active_reconstruction_consultation();
        match import_json(path, consultation.as_ref()) {
            Ok(imported) => {
                self.open_reconstructed_ron(imported.ron_text, &imported.notes);
            }
            Err(e) => self.push_import_error(&e, Some(path)),
        }
    }

    /// **Convert JSON→RON (in place)** — reconstruct the active JSON/JSONC buffer to
    /// RON as ONE E007 undo unit (E010 US2 — T024, FR-002/003/013) `[COMPLETES FR-002]`.
    ///
    /// Reconstructs schema-aware when a type is bound, best-effort otherwise (FR-009);
    /// the reconstructed RON replaces the buffer as a single undo unit (one Undo
    /// restores the exact prior JSON bytes — reusing
    /// [`commit_converted_text`](EditorDocument::commit_converted_text), FR-003). A
    /// malformed JSON buffer surfaces a clear error notice and changes **zero** bytes
    /// — no doc created/corrupted (FR-013).
    pub fn convert_json_to_ron_in_place(&mut self) {
        let Some(idx) = self.workspace.active_index() else {
            return;
        };
        let Some(doc) = self.workspace.get(idx) else {
            return;
        };
        let raw = doc.buffer.clone().into_bytes();
        let consultation = self.active_reconstruction_consultation();
        // Reconstruct read-only over the source — zero bytes change until commit.
        match reconstruct_ron_from_bytes(&raw, None, consultation.as_ref()) {
            Ok(imported) => {
                let Some(doc) = self.workspace.get_mut(idx) else {
                    return;
                };
                // One E007 undo unit; a single Undo restores the exact prior JSON.
                doc.commit_converted_text(imported.ron_text, &self.worker, Instant::now());
                self.surface_reconstruction_notes(&imported.notes);
                self.push_authoring_notice(
                    NoticeKind::Info,
                    "Converted JSON to RON in place (one Undo restores the original JSON).",
                );
            }
            // Malformed JSON / non-UTF-8: a clear error, zero bytes changed (FR-013).
            Err(e) => self.push_import_error(&e, None),
        }
    }

    /// Open reconstructed RON in a new tab + surface any residual-ambiguity notes
    /// (E010 US2 — T023, FR-002/009).
    fn open_reconstructed_ron(&mut self, ron_text: String, notes: &[String]) {
        let idx = self.workspace.open_imported_ron(ron_text);
        // Parse the new tab so it gets diagnostics/highlighting like a fresh open.
        if let Some(doc) = self.workspace.get_mut(idx) {
            doc.request_reparse(&self.worker);
        }
        self.apply_binding_to_active();
        self.apply_mode_to_active();
        self.surface_reconstruction_notes(notes);
        self.push_authoring_notice(
            NoticeKind::Info,
            "Imported JSON as RON in a new tab (the source JSON is unchanged).",
        );
    }

    /// Surface each residual-ambiguity reconstruction note as an info notice (FR-009).
    ///
    /// The best-effort / unbound path notes where it had to guess (array-as-list,
    /// string-keys, external-tag assumption); these are informational, never blocking.
    fn surface_reconstruction_notes(&mut self, notes: &[String]) {
        for note in notes {
            self.push_authoring_notice(NoticeKind::Info, format!("JSON→RON: {note}"));
        }
    }

    /// Push a clear, non-crashing error notice for a failed JSON import (E010 US2 —
    /// T024, FR-013).
    ///
    /// Every import failure leaves the source untouched and creates no tab/doc — the
    /// notice names the failure (not valid UTF-8 / malformed JSON / I/O) so the user
    /// knows why nothing happened.
    fn push_import_error(&mut self, err: &ImportError, path: Option<&std::path::Path>) {
        let where_ = path
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(|n| format!(" {n}"))
            .unwrap_or_default();
        self.push_authoring_notice(NoticeKind::Error, format!("Cannot import{where_}: {err}"));
    }

    // --- E010 US3: derive a RON document from a Rust type (FR-010) ---------------

    /// **Derive RON from type…** — derive an initial, parseable RON scaffold from the
    /// active document's **bound** Rust type, open it in a NEW tab, and surface its
    /// "placeholder — fill in" diagnostics (E010 US3 — T027, FR-010)
    /// `[COMPLETES FR-010]`.
    ///
    /// The registered type comes from the active document's resolved binding
    /// (`bound_type`, the E004/E006 type-pick surface — FR-009/010): when a type is
    /// bound, its serialized `TypeModel` is deserialized (consulted strictly as
    /// **data**, ADR-0004) and walked into a deterministic typed-placeholder scaffold
    /// via [`derive_scaffold`] (FR-010); the scaffold opens in a fresh dirty untitled
    /// tab (the active document is untouched) and its fill-in diagnostics are
    /// published inline through the SAME E006 surface as conversion losses.
    ///
    /// An **unknown / unregistered** type — no document open, no type bound, or a
    /// malformed/absent type model — surfaces a clear, non-crashing "no type model
    /// available" notice and creates **no** document (US3 AS2, FR-010/FR-013).
    pub fn derive_ron_from_type(&mut self) {
        // Resolve the active document's bound type model + root name (the type-pick
        // surface). No active doc / no bound type → a clear message, no document.
        let Some((model, root_type)) = self.active_bound_type_model() else {
            self.push_authoring_notice(
                NoticeKind::Error,
                "Cannot derive: no type model is available — bind a Rust type to the active \
                 document first (Type Bindings…).",
            );
            return;
        };

        // A registered type but a malformed/absent root in the model degrades to the
        // same clear message rather than a partial/corrupt scaffold (FR-010 / §III).
        if !model.contains(&root_type) {
            self.push_authoring_notice(
                NoticeKind::Error,
                format!("Cannot derive: type \"{root_type}\" is not in the bound type model."),
            );
            return;
        }

        // Walk the type's shape into a parseable typed-placeholder scaffold (FR-010).
        let DeriveScaffold {
            text,
            fill_in_diagnostics,
            ..
        } = derive_scaffold(&model, &root_type);

        // Open the scaffold in a NEW tab (the active document is untouched) and parse
        // it so it gets highlighting/diagnostics like a freshly opened buffer.
        let idx = self.workspace.open_imported_ron(text);
        if let Some(doc) = self.workspace.get_mut(idx) {
            doc.request_reparse(&self.worker);
            // Publish the "placeholder — fill in" diagnostics inline from the SAME
            // list, through the E006 surface (FR-006/010). They are transient: the
            // next reparse of the (edited) buffer recomputes diagnostics.
            let source = doc.buffer.clone();
            doc.diagnostics = map_loss_report(&fill_in_diagnostics, &source);
        }
        // Refresh the active-binding indicator + mode for the new tab.
        self.apply_binding_to_active();
        self.apply_mode_to_active();

        let fill_in = fill_in_diagnostics.len();
        let suffix = if fill_in == 0 {
            String::new()
        } else {
            format!(
                " ({fill_in} placeholder{} to fill in)",
                if fill_in == 1 { "" } else { "s" }
            )
        };
        self.push_authoring_notice(
            NoticeKind::Info,
            format!("Derived a RON scaffold from \"{root_type}\" in a new tab{suffix}."),
        );
    }

    /// `true` when a type is bound to the active document, so the Derive command can
    /// produce a scaffold (the menu-enable gate, US3).
    #[must_use]
    pub fn derive_available(&self) -> bool {
        self.active_document()
            .and_then(|d| d.bound_type.as_ref())
            .is_some()
    }

    /// Deserialize the active document's bound type into an in-memory `TypeModel` +
    /// root type name for derive (E010 US3 — T027, FR-010).
    ///
    /// The active document carries its bound type as a **serialized** E004
    /// interchange (`bound_type`); this deserializes it once (consulted strictly as
    /// **data**, ADR-0004). Returns `None` when no document is open, no type is bound,
    /// or the interchange is malformed — the caller then surfaces the clear
    /// "no type model available" message (FR-010 / §III, no false certainty).
    #[must_use]
    fn active_bound_type_model(&self) -> Option<(ronin_types::model::TypeModel, String)> {
        let doc = self.active_document()?;
        let bound = doc.bound_type.as_ref()?;
        let model = ronin_types::from_json(&bound.model).ok()?;
        Some((model, bound.type_name.clone()))
    }

    /// Replace the whole project registry-binding config, persist it, and re-resolve
    /// modes/registries for every open document (E009 — FR-010/FR-012).
    ///
    /// Mirrors [`set_binding_config`](Self::set_binding_config): the new config is
    /// written best-effort to `<root>/.ronin/bevy-registries.json` (a failed write is
    /// logged and swallowed — persistence must never crash the editor), then each open
    /// document re-resolves its mode + registry so a rule change takes effect
    /// immediately.
    pub fn set_registry_binding_config(&mut self, config: RegistryBindingConfig) {
        self.registry_binding_config = config;
        self.persist_registry_binding_config();
        self.reapply_modes_to_all_docs();
    }

    /// Add a registry-binding rule to the project config (E009 — FR-010).
    pub fn add_registry_rule(&mut self, rule: RegistryBindingRule) {
        let mut config = self.registry_binding_config.clone();
        config.rules.push(rule);
        self.set_registry_binding_config(config);
    }

    /// Remove the registry-binding rule at `idx`, if in range (E009 — FR-010).
    pub fn remove_registry_rule(&mut self, idx: usize) {
        if idx >= self.registry_binding_config.rules.len() {
            return;
        }
        let mut config = self.registry_binding_config.clone();
        config.rules.remove(idx);
        self.set_registry_binding_config(config);
    }

    /// Replace the registry-binding rule at `idx` with `rule`, if in range (E009 —
    /// FR-010).
    pub fn replace_registry_rule(&mut self, idx: usize, rule: RegistryBindingRule) {
        if idx >= self.registry_binding_config.rules.len() {
            return;
        }
        let mut config = self.registry_binding_config.clone();
        config.rules[idx] = rule;
        self.set_registry_binding_config(config);
    }

    /// Persist the project registry-binding config to
    /// `<root>/.ronin/bevy-registries.json`, swallowing any IO error (E009 — FR-010).
    ///
    /// Best-effort, exactly like [`persist_binding_config`](Self::persist_binding_config):
    /// a failed write is logged and ignored so persistence can never crash the editor.
    fn persist_registry_binding_config(&self) {
        if let Err(e) = crate::settings::save_registry_binding_config(
            &self.registry_binding_config,
            &self.binding_root,
        ) {
            tracing::warn!(error = %e, "failed to persist project registry-binding config");
        }
    }

    /// Re-resolve + re-load mode/registry state for every open document (E009 —
    /// FR-012).
    ///
    /// The all-documents companion to
    /// [`apply_mode_to_active`](Self::apply_mode_to_active): when the project registry
    /// config changes, every open tab re-resolves its mode + registry from the new
    /// config (preserving any explicit per-document toggle) and re-validates
    /// immediately. Each document holds its own [`ModeState`] (no global state), so
    /// this never cross-contaminates two open documents (FR-012).
    fn reapply_modes_to_all_docs(&mut self) {
        for idx in 0..self.workspace.len() {
            let Some(doc) = self.workspace.get(idx) else {
                continue;
            };
            let path = doc.path.clone();
            let mode_override = match doc.mode_state().mode_origin() {
                crate::bevy::mode::ModeOrigin::Override => Some(doc.mode_state().active_mode()),
                crate::bevy::mode::ModeOrigin::AutoDetected => None,
            };
            let mut state = ModeState::resolve(
                &self.registry_binding_config,
                path.as_deref(),
                mode_override,
                None,
            );
            state.load_registry(&self.binding_root);
            if let Some(doc) = self.workspace.get_mut(idx) {
                doc.set_mode_state(state);
                doc.revalidate(&self.worker);
            }
        }
    }

    /// Replace the whole project binding config, persist it, and re-apply bindings
    /// to every open document (E006 US2 — FR-008/FR-013).
    ///
    /// Used by the rule editor's add / edit / remove operations. The new config is
    /// written best-effort to `<root>/.ronin/bindings.json` (an IO error is logged
    /// and swallowed, mirroring [`persist_settings`](Self::persist_settings) — a
    /// failed write must never crash the editor), then bindings are re-resolved for
    /// all open docs so a rule change takes effect immediately (FR-014).
    pub fn set_binding_config(&mut self, config: BindingConfig) {
        self.binding_config = config;
        self.persist_binding_config();
        self.reapply_bindings_to_all_docs();
    }

    /// Add a rule to the project binding config (E006 US2 — FR-008).
    pub fn add_binding_rule(&mut self, rule: BindingRule) {
        let mut config = self.binding_config.clone();
        config.rules.push(rule);
        self.set_binding_config(config);
    }

    /// Remove the binding rule at `idx`, if in range (E006 US2 — FR-008).
    pub fn remove_binding_rule(&mut self, idx: usize) {
        if idx >= self.binding_config.rules.len() {
            return;
        }
        let mut config = self.binding_config.clone();
        config.rules.remove(idx);
        self.set_binding_config(config);
    }

    /// Replace the binding rule at `idx` with `rule`, if in range (E006 US2 —
    /// FR-008).
    pub fn replace_binding_rule(&mut self, idx: usize, rule: BindingRule) {
        if idx >= self.binding_config.rules.len() {
            return;
        }
        let mut config = self.binding_config.clone();
        config.rules[idx] = rule;
        self.set_binding_config(config);
    }

    /// Persist the project binding config to `<root>/.ronin/bindings.json`,
    /// swallowing any IO error (E006 US2 — FR-013).
    ///
    /// Best-effort, exactly like [`persist_settings`](Self::persist_settings): a
    /// failed write is logged and ignored so binding persistence can never crash the
    /// editor or block editing.
    fn persist_binding_config(&self) {
        let path = BindingConfig::project_config_path(&self.binding_root);
        if let Err(e) = self.binding_config.save_to(&path) {
            tracing::warn!(error = %e, "failed to persist project binding config");
        }
    }

    /// Re-resolve + re-acquire bindings for every open document (E006 US2 —
    /// FR-014).
    ///
    /// Walks all open tabs, recomputes each document's display binding and bound
    /// type from the current project config + that document's override, and requests
    /// an off-frame reparse so a config change re-validates open docs immediately.
    fn reapply_bindings_to_all_docs(&mut self) {
        let threshold = self.settings.effective_large_file_threshold();
        for idx in 0..self.workspace.len() {
            let Some(doc) = self.workspace.get(idx) else {
                continue;
            };
            let path = doc.path.clone();
            let override_ = doc.override_.clone();
            // Thread the project root for FR-025 containment (see
            // `apply_binding_to_active`): an unsafe type_source degrades that rule.
            let (binding, bound) = resolve_and_acquire(
                &self.binding_config,
                path.as_deref(),
                override_.as_ref(),
                &self.binding_root,
            );
            if let Some(doc) = self.workspace.get_mut(idx) {
                doc.binding = binding;
                doc.bound_type = bound;
                // Same E003-consistent oversize degrade as `apply_binding_to_active`
                // (T040): suppress type validation for an oversize document.
                doc.validation_suppressed = doc.oversize(threshold);
                doc.on_edit();
                doc.request_reparse(&self.worker);
            }
        }
    }

    /// Re-derive the project root from a newly opened document's path when the root
    /// has not yet been pinned to a real file (E006 US2 — FR-013).
    ///
    /// The shell has no explicit project picker, so the first opened *on-disk*
    /// document's parent directory becomes the project root and its
    /// `.ronin/bindings.json` is loaded. Subsequent opens do not move an
    /// already-file-derived root (the config stays stable for the session). When the
    /// root changes, the config is reloaded from the new location.
    fn maybe_adopt_project_root(&mut self, doc_path: &Path) {
        // Only adopt a root from a document that lives under a parent directory.
        let Some(parent) = doc_path.parent() else {
            return;
        };
        // If we already point at this parent, nothing to do.
        if self.binding_root == parent {
            return;
        }
        // Pin the root to the opened document's directory and reload the config from
        // that location (absent/corrupt → empty, no crash — FR-013).
        self.binding_root = parent.to_path_buf();
        self.binding_config =
            BindingConfig::load_from(&BindingConfig::project_config_path(&self.binding_root));
        // Reload the parallel E009 registry-binding config from the same new root
        // (separate file; absent/corrupt → empty, no crash — FR-010).
        self.registry_binding_config =
            crate::settings::load_registry_binding_config(&self.binding_root);
    }

    /// The live notices (for tests and host integration).
    #[must_use]
    pub fn notices(&self) -> &[Notice] {
        &self.notices
    }

    /// The bundled showcase samples surfaced under **File ▸ Open Sample ▸**, as
    /// `(file_name, embedded_text)` pairs (for tests and host integration).
    ///
    /// Exposes the same list the Open Sample menu iterates, so a regression test can
    /// assert every menu entry opens a rendering tab without littering a recovery
    /// sidecar (see `tests/all_samples_load.rs`).
    #[must_use]
    pub fn showcase_samples() -> &'static [(&'static str, &'static str)] {
        SHOWCASE_SAMPLES
    }

    /// Push an explanatory authoring notice over the dismissible-notice channel
    /// (FR-024).
    ///
    /// This is the smart-authoring surface's user-feedback path (E005): a format
    /// no-op, a "no clean selection boundary", a "selection has parse errors", etc.
    /// are reported here rather than via a blocking dialog. It reuses the existing
    /// [`Notice`] / [`NoticeKind`] channel so behavior is uniform:
    ///
    /// * [`NoticeKind::Error`] — persists until the user dismisses it (a failure
    ///   the user should see and acknowledge);
    /// * [`NoticeKind::Info`] — auto-dismisses after the standard TTL (a transient
    ///   "nothing to do" / "already formatted" status).
    ///
    /// It is NEVER a blocking modal — authoring feedback must not interrupt editing
    /// (FR-024). Identical consecutive notices are de-duplicated so a repeated
    /// command (e.g. Format again on an already-canonical document) does not stack
    /// the same message.
    pub fn push_authoring_notice(&mut self, kind: NoticeKind, message: impl Into<String>) {
        let message = message.into();
        // De-dupe: if the most recent live notice is identical, don't stack it.
        if let Some(last) = self.notices.last() {
            if last.kind == kind && last.message == message {
                return;
            }
        }
        let notice = match kind {
            NoticeKind::Error => Notice::error(message),
            NoticeKind::Info => Notice::info(message),
        };
        self.notices.push(notice);
    }

    /// The current formatter configuration (FR-007), read-only.
    #[must_use]
    pub fn formatting(&self) -> &FormattingConfig {
        &self.settings.formatting
    }

    /// The current formatter configuration mutably (FR-007).
    ///
    /// Used by the settings surface to adjust indent width / blank-line policy /
    /// format-on-save; changes are folded into [`AppSettings`] and persisted on the
    /// next save tick (FR-016).
    pub fn formatting_mut(&mut self) -> &mut FormattingConfig {
        &mut self.settings.formatting
    }

    /// Whether the Settings window is currently open (for tests/hosts).
    #[must_use]
    pub fn settings_open(&self) -> bool {
        self.show_settings
    }

    /// Open or close the Settings window (FR-023).
    pub fn set_settings_open(&mut self, open: bool) {
        self.show_settings = open;
    }

    /// The effective snippet set: built-ins overlaid by the user file (FR-017),
    /// read-only for tests/hosts and the trigger UI.
    #[must_use]
    pub fn snippets(&self) -> &SnippetSet {
        &self.snippets
    }

    /// Whether the Snippets browser window is open (for tests/hosts).
    #[must_use]
    pub fn snippets_open(&self) -> bool {
        self.show_snippets
    }

    /// Open or close the Snippets browser window (FR-025).
    pub fn set_snippets_open(&mut self, open: bool) {
        self.show_snippets = open;
    }

    /// Reload the snippet set from the OS config dir and re-surface any degrade
    /// notice (FR-017).
    ///
    /// Lets the user re-pick up an edited user file without restarting. A
    /// missing/malformed file still leaves built-ins available; the explanatory
    /// notice is pushed through the standard authoring-notice channel.
    pub fn reload_snippets(&mut self) {
        self.snippets = SnippetSet::load();
        self.surface_snippet_notice();
    }

    /// Push the snippet set's degrade notice, if any, through the authoring channel
    /// (FR-017/FR-024).
    ///
    /// A healthy set (built-ins only, or a clean user file) has no notice and pushes
    /// nothing. A Malformed file (or dropped bad entries) surfaces an explanatory,
    /// non-blocking error notice — built-ins always keep working.
    fn surface_snippet_notice(&mut self) {
        if let Some(message) = self.snippets.notice() {
            self.push_authoring_notice(NoticeKind::Error, message.to_string());
        }
    }

    /// The discoverability menu: `(prefix, description)` per effective snippet,
    /// name-sorted (FR-025).
    #[must_use]
    pub fn snippet_menu_entries(&self) -> Vec<(String, String)> {
        self.snippets.menu_entries()
    }

    /// The path the user snippet file is read from in the OS config dir (FR-025).
    ///
    /// `None` when the platform exposes no config directory.
    #[must_use]
    pub fn user_snippet_path(&self) -> Option<PathBuf> {
        UserSnippetFile::location()
    }

    /// Open (and thereby locate) the user snippet file, creating it from a starter
    /// template if absent (FR-025).
    ///
    /// Best-effort and non-blocking: it resolves the file location in the OS config
    /// dir, writes the [`USER_SNIPPET_TEMPLATE`] starter when no file exists yet (so
    /// the user lands in a valid example), then launches the OS default handler for
    /// the file. Any failure (no config dir, write error, no opener) is surfaced as a
    /// non-blocking error notice rather than crashing. Returns the resolved path on
    /// success, `None` when no location could be determined.
    pub fn open_user_snippet_file(&mut self) -> Option<PathBuf> {
        let Some(path) = UserSnippetFile::location() else {
            self.push_authoring_notice(
                NoticeKind::Error,
                "No OS config directory is available to store the user snippet file.",
            );
            return None;
        };
        // Create the file (and its parent dir) from the starter template if absent,
        // so "open" always lands the user in an editable, valid example (FR-025).
        if !path.exists() {
            if let Some(parent) = path.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    self.push_authoring_notice(
                        NoticeKind::Error,
                        format!("Could not create the snippet directory: {e}"),
                    );
                    return Some(path);
                }
            }
            if let Err(e) = std::fs::write(&path, USER_SNIPPET_TEMPLATE) {
                self.push_authoring_notice(
                    NoticeKind::Error,
                    format!("Could not create the user snippet file: {e}"),
                );
                return Some(path);
            }
        }
        // Launch the OS default handler so the user can locate/edit the file. A
        // failure here is informative, never fatal (the path is still returned).
        if let Err(e) = open_in_os(&path) {
            self.push_authoring_notice(
                NoticeKind::Info,
                format!("The user snippet file is at {} ({e})", path.display()),
            );
        }
        Some(path)
    }

    /// Insert the snippet named `name` at the active document's caret, verifying the
    /// expansion round-trips before committing (FR-016/FR-018).
    ///
    /// Looks up the snippet in the effective set, expands its body, and splices it at
    /// the active caret through [`crate::snippets::insert_snippet`], which re-parses
    /// the candidate buffer and refuses any splice that would introduce a new parse
    /// error (verify-before-replace, project-instructions §I). On success the buffer
    /// is replaced, the caret moves to the first tab-stop, the document is marked
    /// dirty and a reparse is requested, and the live tab-stop session is installed
    /// on the document for `editor_view` to drive. On a refusal (or no active
    /// document / unknown name) a non-blocking notice explains and nothing changes.
    /// Returns `true` when a snippet was inserted.
    pub fn insert_snippet_by_name(&mut self, name: &str) -> bool {
        let Some(snippet) = self.snippets.get(name) else {
            return false;
        };
        let body = snippet.body.clone();
        let Some(idx) = self.workspace.active_index() else {
            self.push_authoring_notice(
                NoticeKind::Info,
                "Open a document before inserting a snippet.",
            );
            return false;
        };
        let Some(doc) = self.workspace.get(idx) else {
            return false;
        };
        let caret = doc.cursor.caret.min(doc.char_len());
        match crate::snippets::insert_snippet(&doc.buffer, caret, &body) {
            Some(insertion) => {
                let Some(doc) = self.workspace.get_mut(idx) else {
                    return false;
                };
                doc.buffer = insertion.new_buffer;
                // Move the caret to the first tab-stop (or leave it at the insertion
                // when the snippet has no stops).
                if let Some(c) = insertion.session.caret() {
                    doc.cursor.caret = c;
                    doc.cursor.selection = insertion.session.selection();
                }
                doc.snippet_session = Some(insertion.session);
                doc.on_edit();
                doc.request_reparse(&self.worker);
                true
            }
            None => {
                self.push_authoring_notice(
                    NoticeKind::Error,
                    "Snippet insertion skipped: the result would not parse.",
                );
                false
            }
        }
    }

    /// Format the active document (FR-001/FR-023).
    ///
    /// Parses the active buffer with `ronin_core::parse`, runs `ronin_core::format`
    /// against the live [`FormattingConfig`], and routes the outcome through the
    /// single safe apply path ([`apply_whole_document_format`](Self::apply_whole_document_format)):
    ///
    /// * [`FormatResult::Formatted`] → the active buffer is replaced with the
    ///   canonical text (marked dirty, reparse triggered) **only** when it actually
    ///   differs; an already-canonical document reports a non-error "already
    ///   formatted" status and leaves the bytes untouched.
    /// * [`FormatResult::NoOp`] → the document is left **byte-unchanged** and the
    ///   formatter's explanatory reason is surfaced as a persist-until-dismissed
    ///   error notice (FR-005/FR-021).
    ///
    /// The formatter performs verify-before-replace + no-op-on-failure internally
    /// (AD-008); the buffer is therefore replaced only on `Formatted`, never on a
    /// failure path. With no document open this is a harmless no-op.
    pub fn format_document(&mut self) {
        let Some(idx) = self.workspace.active_index() else {
            return;
        };
        let Some(doc) = self.workspace.get(idx) else {
            return;
        };
        let config = self.settings.formatting.to_engine_config();
        let parsed = ronin_core::parse(&doc.buffer);
        let result = ronin_core::format(&parsed, &config);
        self.apply_whole_document_format(idx, result);
    }

    /// Format the current selection's smallest enclosing CST subtree (FR-002/FR-023).
    ///
    /// Maps the active document's character-offset selection to its byte range,
    /// parses the buffer, and walks the CST for the **smallest enclosing
    /// value-position node** that fully covers the selection. `ronin_core::format_node`
    /// then produces the canonical text for just that subtree, which is spliced back
    /// over the node's exact source range — the rest of the buffer stays
    /// byte-unchanged (FR-023). The outcome goes through the same single safe apply
    /// path as Format Document:
    ///
    /// * [`FormatResult::Formatted`] → only the node's range is replaced (dirty +
    ///   reparse), and only when it actually changed; an already-canonical subtree
    ///   reports a non-error status with no byte change.
    /// * [`FormatResult::NoOp`] → byte-unchanged + a persist-until-dismissed error
    ///   notice (FR-005/FR-021).
    ///
    /// When there is **no active selection**, this falls back to
    /// [`format_document`](Self::format_document) (formatting the whole buffer) so
    /// the command is never a silent dead-end. When the selection has no clean
    /// subtree boundary the formatter's reason is surfaced and nothing changes.
    pub fn format_selection(&mut self) {
        let Some(idx) = self.workspace.active_index() else {
            return;
        };
        let Some(doc) = self.workspace.get(idx) else {
            return;
        };
        // No selection → format the whole document (never a silent no-op).
        let Some((anchor, head)) = doc.cursor.selection else {
            self.format_document();
            return;
        };
        // An empty (caret-only) selection has no subtree to act on → whole document.
        if anchor == head {
            self.format_document();
            return;
        }
        let (char_start, char_end) = (anchor.min(head), anchor.max(head));
        // Map the char-offset selection to byte offsets into the live buffer.
        let Some((byte_start, byte_end)) = char_range_to_bytes(&doc.buffer, char_start, char_end)
        else {
            self.push_authoring_notice(
                NoticeKind::Error,
                "Could not map the selection to the buffer; formatting skipped.",
            );
            return;
        };
        let config = self.settings.formatting.to_engine_config();
        let parsed = ronin_core::parse(&doc.buffer);
        let Some(node) = smallest_enclosing_value_node(&parsed, byte_start, byte_end) else {
            self.push_authoring_notice(
                NoticeKind::Error,
                "No clean subtree boundary at the selection; formatting skipped.",
            );
            return;
        };
        let node_range = node.text_range();
        let result = ronin_core::format_node(&node, &config);
        self.apply_selection_format(idx, node_range.start(), node_range.end(), result);
    }

    /// The single safe apply path for a whole-document format outcome (T016).
    ///
    /// Centralizes the verify-before-replace contract at the app boundary: the
    /// buffer is replaced **only** on [`FormatResult::Formatted`], and even then
    /// only when the canonical text actually differs from the current buffer (so a
    /// re-run on an already-canonical document is idempotent and never marks it
    /// dirty). On [`FormatResult::NoOp`] the document is left byte-unchanged and the
    /// reason is surfaced as an error notice (FR-005/FR-021).
    fn apply_whole_document_format(&mut self, idx: usize, result: FormatResult) {
        match result {
            FormatResult::Formatted(text) => {
                let Some(doc) = self.workspace.get_mut(idx) else {
                    return;
                };
                if doc.buffer == text {
                    self.push_authoring_notice(NoticeKind::Info, "Document is already formatted.");
                    return;
                }
                doc.buffer = text;
                doc.on_edit();
                doc.request_reparse(&self.worker);
            }
            FormatResult::NoOp { reason } => {
                self.push_authoring_notice(NoticeKind::Error, format!("Format skipped: {reason}"));
            }
        }
    }

    /// The single safe apply path for a Format-Selection outcome (T016).
    ///
    /// Splices the formatted subtree text over `[byte_start, byte_end)` of the
    /// active buffer, leaving the rest byte-unchanged. As with
    /// [`apply_whole_document_format`](Self::apply_whole_document_format) the buffer
    /// is touched **only** on [`FormatResult::Formatted`] and only when the spliced
    /// result actually differs from the current buffer; a [`FormatResult::NoOp`]
    /// leaves the document unchanged and surfaces the reason as an error notice
    /// (FR-005/FR-021). The byte bounds come from the matched CST node's range, so
    /// they always fall on UTF-8 boundaries within the buffer the parse ran on.
    fn apply_selection_format(
        &mut self,
        idx: usize,
        byte_start: usize,
        byte_end: usize,
        result: FormatResult,
    ) {
        match result {
            FormatResult::Formatted(text) => {
                let Some(doc) = self.workspace.get_mut(idx) else {
                    return;
                };
                // Defensive bounds + UTF-8-boundary check: the range came from a CST
                // node parsed from this exact buffer, but never splice on a non-char
                // boundary or out of range (would panic / corrupt) — no-op instead.
                if byte_end > doc.buffer.len()
                    || byte_start > byte_end
                    || !doc.buffer.is_char_boundary(byte_start)
                    || !doc.buffer.is_char_boundary(byte_end)
                {
                    self.push_authoring_notice(
                        NoticeKind::Error,
                        "Format skipped: selection range no longer maps to the buffer.",
                    );
                    return;
                }
                let mut next =
                    String::with_capacity(doc.buffer.len() - (byte_end - byte_start) + text.len());
                next.push_str(&doc.buffer[..byte_start]);
                next.push_str(&text);
                next.push_str(&doc.buffer[byte_end..]);
                if doc.buffer == next {
                    self.push_authoring_notice(NoticeKind::Info, "Selection is already formatted.");
                    return;
                }
                doc.buffer = next;
                doc.on_edit();
                doc.request_reparse(&self.worker);
            }
            FormatResult::NoOp { reason } => {
                self.push_authoring_notice(NoticeKind::Error, format!("Format skipped: {reason}"));
            }
        }
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
        // Blank-proofing invariant (no silent blanks): `open_file` MUST always end
        // in a *visible* outcome — a focused/created tab, the recovery dialog, or an
        // error notice. The snapshots below let the debug assertion at the end of
        // this method catch any future branch that would return having produced
        // none of those (which is what a "blank view" looks like to the user).
        let tabs_before = self.workspace.documents().len();
        let notices_before = self.notices.len();

        // FR-025: focus an already-open tab for the same (canonical) path rather
        // than opening a duplicate. Path-less buffers never match (they have no
        // path), so never-saved buffers stay exempt.
        if let Some(idx) = self.find_open_tab_for(path) {
            self.workspace.switch(idx);
            // Re-applying the binding for the now-active tab keeps the indicator
            // accurate after a focus-existing switch (FR-011).
            self.apply_binding_to_active();
            // Re-resolve the mode/registry too so the E009 indicator stays accurate
            // (FR-011); preserves any explicit per-document toggle.
            self.apply_mode_to_active();
            // Outcome: an existing tab is now focused (visible) — done.
            return;
        }
        // E007 OBJ2 (TR-008/SC-003): on reopen, detect a live, content-divergent
        // recovery sidecar for this file. If one exists, raise a restore offer and
        // defer the open — the user decides restore (in-progress work) vs. decline
        // (on-disk file). A stale / same-content sidecar produces no offer.
        if let Ok(on_disk) = std::fs::read(path) {
            if let crate::recovery::RecoveryDetection::Offer(sidecar) =
                crate::recovery::detect_recovery(path, &on_disk)
            {
                self.recovery_offer = Some(RecoveryOffer {
                    path: path.to_path_buf(),
                    sidecar,
                });
                // Outcome: the recovery dialog (rendered unconditionally by
                // `render_shell` → `render_recovery_offer` whenever `recovery_offer`
                // is set) shows this frame — a visible outcome, not a blank. The
                // user's choice then creates a tab via `resolve_recovery_offer`.
                debug_assert!(
                    self.recovery_offer.is_some(),
                    "open_file deferred to recovery but left no offer to render"
                );
                return;
            }
        }
        match open_path(path) {
            Ok(mut doc) => {
                // Bump the edit generation once and request an initial parse so the
                // freshly opened buffer gets diagnostics/highlighting without an
                // edit. (Generation 0 is the empty "never requested" baseline.)
                doc.on_edit();
                doc.request_reparse(&self.worker);
                self.workspace.open(doc);
                // Adopt a project root from this on-disk document (reloading the
                // config if the root moved), then resolve + apply the binding so the
                // freshly opened doc validates against its mapped type and the
                // active-binding indicator is populated (E006 US2 — FR-011/FR-013).
                self.maybe_adopt_project_root(path);
                self.apply_binding_to_active();
                // Resolve the per-document mode + bound registry from the project
                // registry config (extension auto-detect; load read-only) so the
                // active-mode/registry indicator is populated and a Bevy scene
                // validates against its registry on open (E009 — FR-009/FR-011).
                self.apply_mode_to_active();
            }
            Err(e) => {
                self.push_open_error(&e, path);
            }
        }

        // Enforce the no-silent-blank invariant: by here `open_file` must have
        // either created a tab or pushed an error notice. (The early returns above
        // cover the focus-existing and recovery-defer outcomes.)
        debug_assert!(
            self.workspace.documents().len() > tabs_before || self.notices.len() > notices_before,
            "open_file produced neither a tab nor an error notice (silent blank)"
        );
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

    /// The live crash-recovery restore offer, if one is open (for tests/hosts)
    /// (E007 OBJ2 — TR-008).
    #[must_use]
    pub fn recovery_offer(&self) -> Option<&RecoveryOffer> {
        self.recovery_offer.as_ref()
    }

    /// Resolve an open crash-recovery restore offer with the user's `choice` (E007
    /// OBJ2 — TR-008, SC-003).
    ///
    /// * [`RecoveryChoice::Restore`] → open the on-disk file as the document
    ///   identity (path + load-time fidelity) but replace its buffer with the
    ///   recovered in-progress content, leaving it **dirty** so the recovered work is
    ///   not silently treated as saved. The recovered buffer's byte fidelity is
    ///   preserved via the sidecar's `fidelity_hint` (TR-021).
    /// * [`RecoveryChoice::Decline`] → open the on-disk file normally; the recovered
    ///   content is dropped (the user chose the on-disk version — never silently
    ///   loaded, never silently discarded).
    ///
    /// Either way the offer is cleared. A no-op when no offer is open.
    pub fn resolve_recovery_offer(&mut self, choice: RecoveryChoice) {
        let Some(offer) = self.recovery_offer.take() else {
            return;
        };
        match choice {
            RecoveryChoice::Restore => self.restore_from_sidecar(&offer),
            RecoveryChoice::Decline => self.open_on_disk_no_recovery(&offer.path),
        }
    }

    /// Open `path`'s on-disk file but replace its buffer with the recovered
    /// in-progress content from `offer.sidecar`, leaving it dirty (E007 OBJ2 —
    /// TR-008/TR-021).
    fn restore_from_sidecar(&mut self, offer: &RecoveryOffer) {
        // Open the on-disk file to establish the document identity + load-time
        // fidelity baseline, then splice in the recovered buffer.
        match open_path(&offer.path) {
            Ok(mut doc) => {
                // Replace the buffer with the recovered in-progress content and keep
                // the recovered byte-fidelity profile so a subsequent save re-emits
                // it byte-for-byte (TR-021). The saved baseline stays the on-disk
                // one, so the restored doc is correctly **dirty**.
                doc.buffer = offer.sidecar.buffer.clone();
                doc.byte_profile = offer.sidecar.restored_profile();
                doc.on_edit();
                doc.request_reparse(&self.worker);
                self.workspace.open(doc);
                self.maybe_adopt_project_root(&offer.path);
                self.apply_binding_to_active();
                self.apply_mode_to_active();
            }
            Err(e) => {
                // The on-disk file vanished/became unreadable since the offer; surface
                // it. (The sidecar is left in place so a later reopen can still offer.)
                self.push_open_error(&e, &offer.path);
            }
        }
    }

    /// Open `path`'s on-disk file directly, bypassing recovery-sidecar detection
    /// (the Decline path) (E007 OBJ2 — TR-008).
    fn open_on_disk_no_recovery(&mut self, path: &std::path::Path) {
        match open_path(path) {
            Ok(mut doc) => {
                doc.on_edit();
                doc.request_reparse(&self.worker);
                self.workspace.open(doc);
                self.maybe_adopt_project_root(path);
                self.apply_binding_to_active();
                self.apply_mode_to_active();
            }
            Err(e) => self.push_open_error(&e, path),
        }
    }

    /// Create a fresh, empty untitled document and make it the active tab.
    pub fn new_untitled(&mut self) {
        self.workspace.push_untitled();
        // A path-less untitled buffer resolves to NoBinding; applying it populates
        // the active-binding indicator ("no type bound") for the new tab (FR-011).
        self.apply_binding_to_active();
        // A path-less buffer resolves to Serde mode, no registry; applying it
        // populates the active-mode/registry indicator for the new tab (FR-011).
        self.apply_mode_to_active();
    }

    /// Open a bundled showcase sample as a NEW active tab from embedded text
    /// (path-independent loading).
    ///
    /// The sample's RON text is shipped in the binary via `include_str!`, so it
    /// opens identically in any working directory (fixing the moved-/wrong-cwd
    /// foot-gun a disk read would have). It mirrors the freshly-opened-document
    /// seam — a new tab made active, an initial off-frame parse requested so it
    /// gets diagnostics/highlighting, then binding + mode resolution applied
    /// (E006 US2 / E009 — FR-011) — but sources the bytes from `text` instead of
    /// disk.
    ///
    /// The opened document is an **untitled buffer with NO on-disk
    /// [`path`](EditorDocument::path)** — only a display-only
    /// [`display_title`](EditorDocument::display_title) carrying `file_name` for the
    /// tab. This is deliberate: a sidecar is derived from `path`, so a path-less
    /// buffer is never autosaved and never litters a `.ronin-recovery` file into the
    /// working directory (the prior implementation set `file_name` as a bare on-disk
    /// path, which dropped stale sidecars into the CWD). The buffer is **dirty**
    /// (never-saved): the user chooses where, if anywhere, to save it.
    ///
    /// Bevy mode is still auto-detected from the sample's **name** (a `.scn.ron`
    /// sample enters Bevy mode, FR-009): because there is no `path` to key the
    /// shell's extension auto-detect on, the mode is resolved here from
    /// `file_name` (the same [`ModeState::resolve`] the shell uses) and set
    /// explicitly on the document as an auto-detected mode — so Bevy detection is
    /// preserved WITHOUT a real on-disk path that would trigger sidecar writes.
    pub fn open_sample(&mut self, file_name: &str, text: &str) {
        let seq = self.workspace.next_untitled_seq();
        let mut doc = EditorDocument::new_untitled(seq);
        doc.buffer = text.to_string();
        // Display-only title so the tab shows the sample name; `path` stays `None`
        // so autosave/crash-recovery never writes a sidecar for it (no litter).
        doc.display_title = Some(file_name.to_string());
        // Bump the edit generation + request an initial parse exactly as a fresh
        // open does, so the new tab gets diagnostics/highlighting without an edit.
        doc.on_edit();
        doc.request_reparse(&self.worker);
        self.workspace.open(doc);
        // Resolve + apply the binding for the new tab so the active-binding
        // indicator is populated (FR-011).
        self.apply_binding_to_active();
        // Preserve extension-based mode auto-detection (e.g. `.scn.ron` → Bevy)
        // off the sample NAME rather than a real `path` (which would otherwise
        // drive sidecar writes). Resolve the mode the same way the shell does, mark
        // it on the active document, then run the standard mode/registry apply +
        // re-validate so a Bevy sample validates against its registry on open
        // (E009 — FR-009/FR-011).
        self.apply_sample_mode(file_name);
    }

    /// Resolve the auto-detected mode from a bundled sample's `file_name` and set it
    /// on the active document, then run the standard mode/registry apply.
    ///
    /// Used by [`open_sample`](Self::open_sample): a sample opens path-less (so it
    /// never litters a recovery sidecar), but extension-based mode auto-detect keys
    /// on a path. This resolves the mode from the sample name via the same
    /// [`ModeState::resolve`] the shell's [`apply_mode_to_active`](Self::apply_mode_to_active)
    /// uses, records it as an auto-detected mode override on the active document
    /// (so the subsequent re-resolve in `apply_mode_to_active` preserves it via the
    /// `ModeOrigin::Override` carry-through), then applies the mode/registry +
    /// re-validates. A `.scn.ron` sample thus enters Bevy mode with NO on-disk path.
    fn apply_sample_mode(&mut self, file_name: &str) {
        let detected = ModeState::resolve(
            &self.registry_binding_config,
            Some(std::path::Path::new(file_name)),
            None,
            None,
        );
        let mode = detected.active_mode();
        if let Some(doc) = self.active_document_mut() {
            // Record the name-detected mode so `apply_mode_to_active`'s
            // path-less re-resolve (which has no `path` to detect from) carries it
            // through rather than reverting to the default Serde mode.
            doc.mode_state_mut().set_mode_override(mode);
        }
        self.apply_mode_to_active();
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

    /// Apply opt-in format-on-save to document `idx`'s in-memory buffer (FR-006).
    ///
    /// A no-op unless [`FormattingConfig::format_on_save`] is set. When enabled it
    /// formats the live buffer (the same engine path Format Document uses) **before**
    /// the caller writes bytes:
    ///
    /// * [`FormatResult::Formatted`] → the buffer is updated to the canonical text
    ///   (when it differs) and the reparse trigger fires, so what is written to disk
    ///   and what the views show stay consistent.
    /// * [`FormatResult::NoOp`] → the buffer is left **byte-unchanged**, an info
    ///   notice explains that formatting was skipped, and the save proceeds with the
    ///   original bytes. Format-on-save must NEVER block a save or corrupt the file
    ///   (project-instructions §I), so a formatter decline only informs, never aborts.
    fn maybe_format_on_save(&mut self, idx: usize) {
        if !self.settings.formatting.format_on_save {
            return;
        }
        let Some(doc) = self.workspace.get(idx) else {
            return;
        };
        let config = self.settings.formatting.to_engine_config();
        let parsed = ronin_core::parse(&doc.buffer);
        match ronin_core::format(&parsed, &config) {
            FormatResult::Formatted(text) => {
                let Some(doc) = self.workspace.get_mut(idx) else {
                    return;
                };
                if doc.buffer != text {
                    doc.buffer = text;
                    doc.on_edit();
                    doc.request_reparse(&self.worker);
                }
            }
            FormatResult::NoOp { reason } => {
                // Do not block the save; inform and write the buffer as-is.
                self.push_authoring_notice(
                    NoticeKind::Info,
                    format!("Format on save skipped: {reason}"),
                );
            }
        }
    }

    /// Write document `idx` to `path`, refresh its identity, and clear dirty.
    ///
    /// Shared by Save and Save As (and tests, since it sidesteps the `rfd` dialog):
    /// runs opt-in format-on-save **before** the byte-write (FR-006, AD-005), then
    /// writes via [`save_document`], and on success sets the document's path,
    /// refreshes its byte-fidelity profile from the bytes actually written, and
    /// marks it saved. On failure pushes an error notice and keeps the doc dirty.
    /// Returns `true` only on a successful write.
    pub fn save_doc_to(&mut self, idx: usize, path: &Path) -> bool {
        // Opt-in format-on-save runs on the in-memory buffer first, so the formatted
        // text is what reaches disk (FR-006). It NEVER blocks or fails the save: on a
        // formatter no-op/failure the buffer is saved as-is and a notice is surfaced.
        self.maybe_format_on_save(idx);
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
                // E007 OBJ2 (TR-009): a clean save removes the recovery sidecar so a
                // stale/orphan sidecar is never offered on the next open. Reset the
                // debounce bookkeeping to the now-saved generation so a fresh edit is
                // required before the next autosave. Best-effort: a removal error is
                // logged, never fatal.
                doc.mark_autosaved();
                if let Err(e) = crate::recovery::remove_sidecar(path) {
                    tracing::warn!(error = %e, "failed to remove recovery sidecar after save");
                }
                true
            }
            Err(e) => {
                self.push_save_error(&e, path);
                false
            }
        }
    }

    /// Push an **error** notice for a failed atomic save (FR-011 / E007 TR-003).
    ///
    /// A failed save NEVER clears dirty or re-baselines `last_saved` — the original
    /// file is byte-identical (the atomic pipeline only ever swaps the target on a
    /// committed replace), so the caller leaves the buffer dirty and the user can
    /// retry. The notice names the failure surface (disk-full, permission, etc.) so
    /// "never corrupt user data" never degrades into a silent success.
    fn push_save_error(&mut self, err: &SaveError, path: &Path) {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        let detail = match err {
            SaveError::DiskFull(io) => format!("disk full ({io})"),
            SaveError::PermissionDenied(io) => format!("permission denied ({io})"),
            SaveError::PartialWrite(io) => format!("write interrupted ({io})"),
            SaveError::ReplaceFailed(io) => format!("atomic replace failed ({io})"),
            SaveError::SameFilesystemImpossible(io) => {
                format!("atomic save not possible at this location ({io})")
            }
            SaveError::Io(io) => format!("{io}"),
        };
        self.notices
            .push(Notice::error(format!("Cannot save {name}: {detail}")));
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
                self.apply_binding_to_active();
                return true;
            }
        }
        // Request an initial parse for the freshly reopened buffer.
        if let Some(doc) = self.workspace.get_mut(idx) {
            doc.on_edit();
            doc.request_reparse(&self.worker);
        }
        // Resolve + apply the binding for the reopened (now-active) doc so its
        // active-binding indicator is populated (E006 US2 — FR-011).
        self.apply_binding_to_active();
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

    /// Replace the active document's buffer and trigger an off-frame reparse, as a
    /// real edit would (test / host integration helper for the FR-021 (a) trigger).
    ///
    /// Mirrors the editor's own edit path exactly: it sets the buffer, bumps the
    /// edit generation via [`EditorDocument::on_edit`], and requests a coalesced
    /// off-frame reparse via [`EditorDocument::request_reparse`] (which carries the
    /// document's currently-bound type with the request, so type validation re-runs
    /// against the bound model — FR-021 (a)). A headless test then drives the App's
    /// real worker to completion with [`poll_documents`](Self::poll_documents). A
    /// no-op when no tab is open.
    pub fn replace_active_buffer_for_test(&mut self, buffer: &str) {
        {
            let Some(doc) = self.workspace.active_document_mut() else {
                return;
            };
            doc.buffer = buffer.to_string();
            doc.on_edit();
        }
        // Mirror the real per-frame edit path: the frame loop's `pump_documents`
        // reconciles the oversize type-validation degrade flag against the (now
        // edited) buffer *before* requesting a reparse, so an edit that crosses the
        // large-file threshold flips validation on/off correctly (T040). Reconcile
        // here too so this test/host edit helper exercises the same gate.
        self.reconcile_validation_degrade();
        if let Some(doc) = self.workspace.active_document_mut() {
            doc.request_reparse(&self.worker);
        }
    }

    /// Drain ready parse results into every open document, returning whether any
    /// installed (for tests / host integration of the off-frame pump).
    ///
    /// This is the same routing drain the frame loop runs via
    /// [`pump_documents`](Self::pump_documents), exposed so a headless test can drive
    /// the App's *own* off-frame [`ReparseWorker`] to completion (request → poll
    /// until install) and observe binding-driven type diagnostics land on the active
    /// document — the real end-to-end path (E006 US2 — FR-013).
    pub fn poll_documents(&mut self) -> bool {
        self.dispatch_parse_results()
    }

    /// Drain the shared worker's ready results **once** and route each to its owning
    /// document by [`doc_id`](crate::reparse::ParseResult::doc_id) (FR-006).
    ///
    /// The [`ReparseWorker`] is shared across every open tab and its result channel is
    /// a single FIFO; `generation` is a *per-document* edit counter, so two tabs at the
    /// same generation (e.g. two freshly opened files, each at generation 1) are
    /// indistinguishable by generation alone. The previous design had each document
    /// independently drain the worker, so a result could be consumed and installed by
    /// the **wrong** tab — leaving the originating tab with no parse, i.e. a blank
    /// view (the user-reported "blank when switching back and forth"). Here the App
    /// drains the worker once and dispatches each result to the document whose `id`
    /// matches (via [`EditorDocument::install_parse`]), so no tab ever steals another
    /// tab's result. Returns `true` if any document installed a fresh result.
    fn dispatch_parse_results(&mut self) -> bool {
        let mut any_installed = false;
        // Drain the shared FIFO once; route each result to its owning document.
        while let Some(result) = self.worker.poll() {
            let owner = self
                .workspace
                .documents_mut()
                .iter_mut()
                .find(|d| d.id() == result.doc_id);
            if let Some(doc) = owner {
                if doc.install_parse(result) {
                    any_installed = true;
                }
            }
            // A result whose owning document has since closed has no home; dropping
            // it is correct (the document is gone, nothing to render).
        }
        any_installed
    }

    /// Request reparses for any document whose buffer advanced, then drain + route the
    /// shared worker's ready results to their owning documents (FR-006).
    ///
    /// Returns `true` if any document installed a fresh result (the caller can
    /// repaint to show it). Runs for all documents — not just the active one — so
    /// background tabs stay current; none of this touches `ronin_core::parse`
    /// directly (the worker owns parsing).
    ///
    /// Before requesting reparses it reconciles each document's type-validation
    /// degrade flag against the live buffer size (T040), so type validation degrades
    /// on E003's oversize threshold exactly like highlighting/squiggles and resumes
    /// automatically once an oversize document is edited back below the threshold.
    /// Result delivery is routed centrally by [`dispatch_parse_results`](Self::dispatch_parse_results)
    /// so a shared-worker result always lands on the tab that requested it.
    fn pump_documents(&mut self) -> bool {
        self.reconcile_validation_degrade();
        for doc in self.workspace.documents_mut() {
            doc.request_reparse(&self.worker);
        }
        self.dispatch_parse_results()
    }

    /// Reconcile every open document's type-validation degrade flag against the live
    /// buffer size (E006 T040 — FR-015/FR-024).
    ///
    /// Type validation degrades on the **same** signal E003 uses to disable
    /// highlighting and squiggles: the document being
    /// [`oversize`](EditorDocument::oversize) past
    /// [`AppSettings::effective_large_file_threshold`](crate::settings::AppSettings::effective_large_file_threshold).
    /// When a document is oversize its
    /// [`validation_suppressed`](EditorDocument::validation_suppressed) flag is set so
    /// the next [`request_reparse`](EditorDocument::request_reparse) ships no bound
    /// type to the worker (structural-only, zero type diagnostics — FR-015); when it
    /// drops back below the threshold the flag clears and validation resumes on the
    /// next reparse. Flipping the flag bumps the edit generation so the reconciled
    /// state is actually re-requested (otherwise a coalesced reparse would not fire).
    /// Reconciling here — once per frame, against the live buffer — keeps the type
    /// degrade boundary byte-for-byte consistent with E003's per-frame squiggle gate
    /// in [`render_central`](Self::render_central). The display binding
    /// ([`binding`](EditorDocument::binding)) is left untouched so the active-binding
    /// indicator still shows the *intended* type even while validation is degraded.
    fn reconcile_validation_degrade(&mut self) {
        let threshold = self.settings.effective_large_file_threshold();
        for doc in self.workspace.documents_mut() {
            let oversize = doc.oversize(threshold);
            if doc.validation_suppressed != oversize {
                doc.validation_suppressed = oversize;
                // Force the changed degrade decision to be re-requested: without a
                // generation bump a coalesced reparse for the same text would be
                // skipped, so a doc edited across the threshold would keep stale
                // type diagnostics (or none) until the next unrelated edit.
                doc.on_edit();
            }
        }
    }

    /// The cheap per-frame autosave debounce check (E007 OBJ2 — AD-004/TR-006/
    /// TR-016/TR-023).
    ///
    /// This is the **only** autosave work the per-frame `update` path performs, and
    /// it does **no** I/O: for each open document it (1) syncs the debounce with the
    /// live (clamped) [`AutosaveConfig`](crate::settings::AutosaveConfig), (2) notes a
    /// buffer change when the document is dirty (keyed on `edit_generation`, idempotent
    /// per generation), and (3) runs the cheap [`EditorDocument::should_autosave`]
    /// check against `now`. When it fires, it hands a
    /// [`RecoverySidecar`](crate::recovery::RecoverySidecar) snapshot to the off-frame
    /// [`AutosaveWorker`](crate::recovery::AutosaveWorker) — the actual atomic,
    /// crash-safe sidecar write (and its `fsync` cost) runs on the worker thread,
    /// never here (TR-016/TR-023, SC-008). An **untitled** buffer is never autosaved
    /// (TR-017); a **clean** buffer never autosaves (the only-when-changed gate), so a
    /// no-op tick writes nothing (SC-010).
    ///
    /// `now` is injectable so the debounce is deterministically testable
    /// (TR-020); the live frame loop passes [`Instant::now`].
    fn maybe_autosave(&mut self, now: Instant) {
        let config = self.settings.autosave;
        for doc in self.workspace.documents_mut() {
            doc.set_autosave_config(config);
            // Note a change only while the document is dirty (an untitled or
            // already-saved doc has nothing to recover). Idempotent per generation.
            if doc.dirty() {
                doc.note_change(now);
            }
            if doc.should_autosave(now) {
                if let Some(sidecar) = doc.recovery_snapshot() {
                    self.autosave_worker.enqueue(sidecar);
                    // One write per debounce window: record the dispatch so the next
                    // tick only fires after a new change (SC-010).
                    doc.mark_autosaved();
                }
            }
        }
        // Drain any finished off-frame write outcomes (non-blocking) so the channel
        // does not grow unbounded; the result is advisory (the buffer's own dirty
        // state is the source of truth, not the sidecar).
        while self.autosave_worker.poll().is_some() {}
    }

    /// Handle the undo/redo keyboard shortcuts (E007 OBJ3 — TR-010, SC-005).
    ///
    /// Consumes the standard editor shortcuts and drives the active document's
    /// CST-backed history:
    ///
    /// * **Undo** — `Ctrl+Z` (Windows/Linux) / `Cmd+Z` (macOS).
    /// * **Redo** — `Ctrl+Y` / `Cmd+Y`, or `Ctrl+Shift+Z` / `Cmd+Shift+Z`.
    ///
    /// The shortcuts are consumed via [`egui::InputState::consume_shortcut`] so they
    /// take precedence over egui's built-in `TextEdit` undo and drive RONin's own
    /// byte-faithful, bounded history instead. A repaint is requested when a step is
    /// taken so the restored buffer + a fresh reparse paint immediately. A no-op
    /// when no tab is open or there is nothing to (re)do.
    fn handle_undo_shortcuts(&mut self, ctx: &egui::Context) {
        use egui::{Key, KeyboardShortcut, Modifiers};
        // `Modifiers::COMMAND` is Cmd on macOS and Ctrl elsewhere — the portable
        // primary-modifier, matching the platform-native undo chord.
        let undo = KeyboardShortcut::new(Modifiers::COMMAND, Key::Z);
        let redo_y = KeyboardShortcut::new(Modifiers::COMMAND, Key::Y);
        let redo_shift_z = KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z);

        let (do_undo, do_redo) = ctx.input_mut(|i| {
            // Check redo (the more specific Shift+Z) first so Ctrl+Shift+Z is not
            // swallowed by the plain Ctrl+Z matcher.
            let redo = i.consume_shortcut(&redo_shift_z) || i.consume_shortcut(&redo_y);
            let undo = i.consume_shortcut(&undo);
            (undo, redo)
        });

        if do_redo {
            self.redo_active();
        }
        if do_undo {
            self.undo_active();
        }
    }

    /// The cheap per-frame undo-snapshot bookkeeping (E007 OBJ3 — TR-016/TR-023,
    /// SC-008).
    ///
    /// The undo counterpart to [`maybe_autosave`](Self::maybe_autosave): for each
    /// open document it (1) syncs the undo stack with the live (clamped)
    /// [`UndoConfig`](crate::settings::UndoConfig) and (2) records a snapshot **only
    /// when the buffer advanced** since the last snapshot, coalesced into one undo
    /// unit per coalesce window (the parse + text-clone cost is paid at most once
    /// per window, never per keystroke). This keeps undo bookkeeping off the
    /// per-frame render path and bounded: the per-frame work is a generation
    /// comparison + an `Instant` subtraction, and the snapshot itself fires at
    /// coalesce-unit boundaries leveraging the cheap structurally-shared CST clone
    /// (AD-002/ADR-0001), so the per-frame `update` undo cost stays within the frame
    /// budget on a large buffer (SC-008).
    ///
    /// `now` is injectable so the coalesce timing is deterministically testable
    /// (TR-020 pattern); the live frame loop passes [`Instant::now`].
    fn record_undo_snapshots(&mut self, now: Instant) {
        let config = self.settings.undo;
        for doc in self.workspace.documents_mut() {
            doc.set_undo_config(config);
            doc.record_undo_snapshot(now);
        }
    }

    /// Edit the active document's buffer and record an undo snapshot at an explicit
    /// `now` (E007 OBJ3 deterministic test/host seam — TR-020 pattern, T036).
    ///
    /// Mirrors the real per-frame edit + undo-bookkeeping path but takes an injected
    /// `Instant` so a test can drive coalescing deterministically without depending
    /// on wall-clock timing: an edit `now` within the configured coalesce window of
    /// the prior one extends the current undo unit; one after it starts a new unit
    /// (TR-027, SC-010). Sets the buffer, bumps the edit generation, syncs the live
    /// undo config, records the coalesced snapshot, and requests an off-frame
    /// reparse. A no-op when no tab is open.
    pub fn edit_active_buffer_at(&mut self, buffer: &str, now: Instant) {
        let config = self.settings.undo;
        {
            let Some(doc) = self.workspace.active_document_mut() else {
                return;
            };
            doc.buffer = buffer.to_string();
            doc.on_edit();
            doc.set_undo_config(config);
            doc.record_undo_snapshot(now);
        }
        self.reconcile_validation_degrade();
        if let Some(doc) = self.workspace.active_document_mut() {
            doc.request_reparse(&self.worker);
        }
    }

    /// Undo the active document at an explicit `now` (E007 OBJ3 test/host seam).
    ///
    /// Like [`undo_active`](Self::undo_active) but takes an injected `Instant` for
    /// the pre-undo pending-run flush so the coalesce timing stays deterministic in
    /// tests. Returns `true` when a step was taken.
    pub fn undo_active_at(&mut self, now: Instant) -> bool {
        let Some(doc) = self.workspace.active_document_mut() else {
            return false;
        };
        if doc.undo(now) {
            doc.request_reparse(&self.worker);
            true
        } else {
            false
        }
    }

    /// Undo the active document's last change (E007 OBJ3 — TR-010/TR-018, SC-005).
    ///
    /// Restores the exact prior **in-memory** bytes + cursor and requests an
    /// off-frame reparse so highlighting/diagnostics re-run against the restored
    /// text; dirty-tracking recomputes automatically. In-memory only — never reads
    /// or writes the file (TR-018). A no-op (returns `false`) when no tab is open or
    /// there is nothing to undo. `now` drives the pre-undo pending-run flush.
    pub fn undo_active(&mut self) -> bool {
        let Some(doc) = self.workspace.active_document_mut() else {
            return false;
        };
        if doc.undo(Instant::now()) {
            doc.request_reparse(&self.worker);
            true
        } else {
            false
        }
    }

    /// Redo the active document's last undone change (E007 OBJ3 — TR-010/TR-018,
    /// SC-005).
    ///
    /// Replays the exact bytes + cursor and requests an off-frame reparse. In-memory
    /// only (TR-018). A no-op (returns `false`) when no tab is open or there is
    /// nothing to redo.
    pub fn redo_active(&mut self) -> bool {
        let Some(doc) = self.workspace.active_document_mut() else {
            return false;
        };
        if doc.redo() {
            doc.request_reparse(&self.worker);
            true
        } else {
            false
        }
    }

    /// Force an autosave tick on every open document now, bypassing the idle / edit-
    /// count thresholds (E007 OBJ2 — TR-020 deterministic seam; for tests/hosts).
    ///
    /// Honours the only-when-changed gate and the untitled-no-sidecar rule, so a
    /// forced tick on a clean / untitled document writes nothing (SC-010). Returns
    /// the number of sidecar writes dispatched. The write itself still runs off-frame
    /// on the [`AutosaveWorker`](crate::recovery::AutosaveWorker); call
    /// [`flush_autosaves`](Self::flush_autosaves) to await completion in a test.
    pub fn force_autosave_all(&mut self) -> usize {
        let config = self.settings.autosave;
        let mut dispatched = 0;
        for doc in self.workspace.documents_mut() {
            doc.set_autosave_config(config);
            if doc.dirty() {
                doc.note_change(Instant::now());
            }
            if doc.force_autosave_tick() {
                if let Some(sidecar) = doc.recovery_snapshot() {
                    self.autosave_worker.enqueue(sidecar);
                    doc.mark_autosaved();
                    dispatched += 1;
                }
            }
        }
        dispatched
    }

    /// Block until `count` off-frame autosave writes have completed (E007 OBJ2 — for
    /// tests/hosts that need to observe the sidecar on disk after a forced tick).
    ///
    /// Drains [`count`] outcomes from the [`AutosaveWorker`](crate::recovery::AutosaveWorker),
    /// blocking on each, so a test can assert the sidecar file exists after
    /// [`force_autosave_all`](Self::force_autosave_all). Returns the number of
    /// outcomes that reported a committed write.
    pub fn flush_autosaves(&mut self, count: usize) -> usize {
        let mut committed = 0;
        for _ in 0..count {
            // Spin-poll the non-blocking channel until the worker reports the write.
            loop {
                if let Some(outcome) = self.autosave_worker.poll() {
                    if outcome.committed {
                        committed += 1;
                    }
                    break;
                }
                std::thread::yield_now();
            }
        }
        committed
    }

    /// Remove the recovery sidecars for every cleanly-saved (non-dirty) document on
    /// a clean exit (E007 OBJ2 — TR-009).
    ///
    /// Called from [`on_exit`](eframe::App::on_exit) so a stale/orphan sidecar is
    /// never offered on the next launch. A **dirty** document's sidecar is left in
    /// place (it IS the recovery insurance for unsaved work after this exit — SC-003);
    /// only clean, titled documents have their sidecar removed here. Best-effort: a
    /// removal error is logged, never fatal.
    fn cleanup_sidecars_on_exit(&self) {
        for doc in self.workspace.documents() {
            if doc.dirty() {
                // Dirty: keep the sidecar — it is the crash-recovery copy.
                continue;
            }
            if let Some(path) = &doc.path {
                if let Err(e) = crate::recovery::remove_sidecar(path) {
                    tracing::warn!(error = %e, "failed to remove recovery sidecar on exit");
                }
            }
        }
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
                    // Open Sample ▸ — one-click load of a bundled showcase sample
                    // from text embedded in the binary (include_str!), so a sample
                    // opens in ANY working directory (path-independent). Each opens
                    // as a NEW active tab through the same open seam (open_sample).
                    ui.menu_button("Open Sample", |ui| {
                        for (file_name, text) in SHOWCASE_SAMPLES {
                            if ui.button(*file_name).clicked() {
                                self.open_sample(file_name, text);
                                ui.close();
                            }
                        }
                    });
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
                    if ui.button("Settings\u{2026}").clicked() {
                        self.show_settings = true;
                        ui.close();
                    }
                    // Type bindings window (E006 US2 — FR-009/FR-011): per-document
                    // override control + the project rule editor.
                    if ui.button("Type Bindings\u{2026}").clicked() {
                        self.show_bindings = true;
                        ui.close();
                    }
                    // Bevy registries window (E009 — FR-009/FR-010/FR-011): the
                    // per-document mode toggle + active-mode/registry indicator + the
                    // project registry-rule editor.
                    if ui.button("Bevy Registries\u{2026}").clicked() {
                        self.show_registries = true;
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Quit").clicked() {
                        self.request_quit();
                        ui.close();
                    }
                });
                // Edit menu (E007 OBJ3 — TR-010): Undo / Redo of the active
                // document's CST-backed history. Both show their keyboard chord and
                // are enabled only when a step is actually available, so the menu
                // shape stays stable. The commands operate in-memory only (TR-018).
                ui.menu_button("Edit", |ui| {
                    let can_undo = self.active_document().is_some_and(EditorDocument::can_undo);
                    let can_redo = self.active_document().is_some_and(EditorDocument::can_redo);
                    ui.add_enabled_ui(can_undo, |ui| {
                        if ui.button("Undo\t\u{2303}Z").clicked() {
                            self.undo_active();
                            ui.close();
                        }
                    });
                    ui.add_enabled_ui(can_redo, |ui| {
                        if ui.button("Redo\t\u{2303}Y").clicked() {
                            self.redo_active();
                            ui.close();
                        }
                    });
                });
                // Format menu (FR-001/FR-002/FR-023): Format Document / Format
                // Selection. Both invoke the real `ronin_core` formatter through the
                // shell's single safe apply path (buffer replaced only on a
                // verified `Formatted` result; `NoOp` surfaces an error notice and
                // changes nothing). Both are disabled only when no tab is open so the
                // menu shape stays stable.
                ui.menu_button("Format", |ui| {
                    let has_active = self.active_document().is_some();
                    ui.add_enabled_ui(has_active, |ui| {
                        if ui.button("Format Document").clicked() {
                            self.format_document();
                            ui.close();
                        }
                        if ui.button("Format Selection").clicked() {
                            self.format_selection();
                            ui.close();
                        }
                    });
                });
                // Bevy menu (E009 US3 — FR-014/FR-015/FR-016): the explicit,
                // never-automatic defaults-elision commands. Both are enabled ONLY for
                // a Bevy-mode document with a loaded registry (the provable-default
                // rule needs the registry's per-type concrete defaults) — a serde
                // document or a Bevy document with no registry shows them disabled so
                // the menu shape stays stable. Each commits the whole transform as one
                // E007 undo unit (SC-006); the partial-expand-on-drift advisory is
                // surfaced through the dismissible-notice channel.
                ui.menu_button("Bevy", |ui| {
                    let can_elide = self.elision_available();
                    ui.add_enabled_ui(can_elide, |ui| {
                        if ui.button("Reduce Verbosity").clicked() {
                            self.reduce_verbosity_active(crate::bevy::Scope::WholeDocument);
                            ui.close();
                        }
                        if ui.button("Expand to Explicit").clicked() {
                            self.expand_to_explicit_active(crate::bevy::Scope::WholeDocument);
                            ui.close();
                        }
                    });
                    if !can_elide {
                        ui.separator();
                        ui.weak("(Bevy mode + a loaded registry required)");
                    }
                });
                // Convert menu (E010 US1 — FR-001/003/005/008): RON→JSON conversion,
                // offered as a non-destructive export AND as an in-place transform
                // (one E007 undo unit). Both run the pre-conversion loss-report
                // confirm/cancel dialog when the conversion is lossy; a loss-free
                // (round-trip-safe-tier) conversion commits without a prompt. Both
                // are enabled only when a document is open so the menu shape stays
                // stable. The JSON→RON import direction is US2.
                ui.menu_button("Convert", |ui| {
                    let can_convert = self.convert_available();
                    ui.add_enabled_ui(can_convert, |ui| {
                        if ui.button("Convert to JSON (in place)").clicked() {
                            self.convert_to_json_in_place();
                            ui.close();
                        }
                        if ui.button("Export to JSON\u{2026}").clicked() {
                            self.export_to_json_active();
                            ui.close();
                        }
                        // JSON→RON direction (E010 US2 — FR-002): reconstruct the
                        // active JSON buffer in place (one undo unit). Import-from-file
                        // is offered unconditionally below (it opens a NEW tab).
                        if ui.button("Convert JSON\u{2192}RON (in place)").clicked() {
                            self.convert_json_to_ron_in_place();
                            ui.close();
                        }
                    });
                    ui.separator();
                    // Import-to-new-tab is always available — it opens a fresh tab and
                    // needs no currently-open document (FR-002).
                    if ui.button("Import JSON / JSONC\u{2026}").clicked() {
                        self.import_json_to_new_tab();
                        ui.close();
                    }
                    ui.separator();
                    // Derive-from-type (E010 US3 — FR-010): scaffold a parseable RON
                    // document from the active doc's bound Rust type into a NEW tab.
                    // Enabled only when a type is bound; an unbound/unknown type
                    // surfaces a clear non-crashing message and creates no document.
                    let can_derive = self.derive_available();
                    ui.add_enabled_ui(can_derive, |ui| {
                        if ui.button("Derive RON from type\u{2026}").clicked() {
                            self.derive_ron_from_type();
                            ui.close();
                        }
                    });
                    if !can_derive {
                        ui.weak("(bind a Rust type to derive a RON scaffold)");
                    }
                    if !can_convert {
                        ui.separator();
                        ui.weak("(open a RON document to convert to JSON)");
                    }
                });
                // Snippets menu (FR-015/FR-025): each effective snippet is listed by
                // its prefix + description (discoverability), insertable into the
                // active document via the explicit trigger; plus a Browse window and
                // the open/locate-user-file command. Snippet insertion is verified to
                // round-trip before it touches the buffer (FR-018).
                ui.menu_button("Snippets", |ui| {
                    let has_active = self.active_document().is_some();
                    // Snapshot the (name, prefix, description) triples so the menu can
                    // render while we later mutate the document on a click.
                    let entries: Vec<(String, String, String)> = self
                        .snippets
                        .iter()
                        .map(|s| (s.name.clone(), s.prefix.clone(), s.description.clone()))
                        .collect();
                    ui.add_enabled_ui(has_active, |ui| {
                        let mut insert: Option<String> = None;
                        egui::ScrollArea::vertical()
                            .max_height(360.0)
                            .show(ui, |ui| {
                                for (name, prefix, description) in &entries {
                                    if ui
                                        .button(format!("{prefix}  \u{2014}  {description}"))
                                        .clicked()
                                    {
                                        insert = Some(name.clone());
                                    }
                                }
                            });
                        if let Some(name) = insert {
                            self.insert_snippet_by_name(&name);
                            ui.close();
                        }
                    });
                    ui.separator();
                    if ui.button("Browse Snippets\u{2026}").clicked() {
                        self.show_snippets = true;
                        ui.close();
                    }
                    if ui.button("Open User Snippet File\u{2026}").clicked() {
                        self.open_user_snippet_file();
                        ui.close();
                    }
                    if ui.button("Reload Snippets").clicked() {
                        self.reload_snippets();
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
                    // The Dismiss button is laid out first (right-stable), then the
                    // message wraps in the remaining width so a long notice never
                    // overflows the panel (Part C — scrollbars/wrapping).
                    if ui.small_button("Dismiss").clicked() {
                        dismiss = Some(idx);
                    }
                    match notice.kind {
                        NoticeKind::Error => {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&notice.message)
                                        .color(ui.visuals().error_fg_color),
                                )
                                .wrap(),
                            );
                        }
                        NoticeKind::Info => {
                            ui.add(
                                egui::Label::new(egui::RichText::new(&notice.message).weak())
                                    .wrap(),
                            );
                        }
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
        let actions = egui::Panel::top("tab_bar")
            .show_inside(ui, |ui| render_tab_strip(ui, &self.workspace))
            .inner;

        // Apply the deferred mutations *after* the loop so we never reshape the
        // tab list mid-iteration. A reorder takes precedence over a stray click on
        // the same frame; a click-to-switch refreshes the active-binding indicator.
        if let Some((from, to)) = actions.reorder {
            self.workspace.reorder(from, to);
        } else if let Some(idx) = actions.switch_to {
            self.workspace.switch(idx);
            // Refresh the active-binding indicator for the now-active tab
            // (E006 US2 — FR-011); cheap (re-resolve + Arc-shared model).
            self.apply_binding_to_active();
        }
        if let Some(idx) = actions.close_idx {
            self.request_close_doc(idx);
        }
    }

    /// Install the bundled Noto symbol/math fonts as **fallbacks** on `ctx`.
    ///
    /// Starts from egui's default [`egui::FontDefinitions`] (so normal text keeps
    /// the default proportional/monospace faces) and appends the three Noto faces
    /// to the *end* of both the `Proportional` and `Monospace` family chains — the
    /// fallback position — so only glyphs the default fonts lack (symbols, math)
    /// fall through to them. No glyph in any authored UI string is swapped.
    ///
    /// Call once during startup where `cc.egui_ctx` is available (see `main.rs`).
    pub fn install_fonts(ctx: &egui::Context) {
        ctx.set_fonts(build_font_definitions());
    }

    /// Render the central editor region for the active document, or the
    /// empty-workspace placeholder when no tab is open (FR-022).
    ///
    /// Hosts the **per-document view switcher** (E008 — T014/FR-017): a control to
    /// switch among Text / Tree-form / Table for the active document, defaulting to
    /// the structural (tree/form) view on open. Switching is **lossless** — it
    /// changes zero document bytes (FR-020) and keeps any in-progress draft/focus
    /// (re-resolved by structural-path identity across reparse — FR-016). The actual
    /// tree/table rendering lands in US1/US2; the structural panes are placeholders
    /// here while the switcher + view state work end-to-end.
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
                // The per-document view switcher (FR-017). Reading/writing the
                // active view is byte-free (FR-020).
                Self::render_view_switcher(ui, self.workspace.get_mut(idx));
                ui.separator();
                let view = self
                    .workspace
                    .get(idx)
                    .map(|d| d.view_state().active_view())
                    .unwrap_or(ActiveView::TreeForm);
                // E012 / US3 / FR-007: seed the active document's column layout from the
                // persisted settings BEFORE rendering, so a section's saved hide/order/pin
                // is in effect the first time it is shown this session. View-only / byte-free.
                self.load_active_column_layouts(idx);
                match view {
                    ActiveView::Text => {
                        if let Some(doc) = self.workspace.get_mut(idx) {
                            editor_view(ui, doc, oversize);
                        }
                    }
                    ActiveView::TreeForm => {
                        // Host the **auto-routing structural view** (E008 / US3 —
                        // T039/T040, [COMPLETES FR-010]). This is the document's
                        // default structural surface (FR-017): it renders the
                        // always-visible active-binding indicator (FR-011), then
                        // classifies the document's section and routes it — a
                        // table-eligible uniform list renders as an embedded
                        // virtualized table, everything else as tree/form, with a
                        // persistent per-section boundary indicator + a reversible
                        // per-section override (FR-010/FR-011/FR-012). The worker is a
                        // separate field from the workspace, so the split borrow
                        // (`&mut doc` + `&self.worker`) is sound.
                        let worker = &self.worker;
                        if let Some(doc) = self.workspace.get_mut(idx) {
                            structural_form_view(ui, doc, worker);
                        }
                    }
                    ActiveView::Table => {
                        // Host the structural table view (E008 / US2 — T035,
                        // [COMPLETES FR-005]). It renders the always-visible
                        // active-binding indicator (FR-011) then the virtualized
                        // grid with inline scalar cell editing, discoverable row
                        // add/remove, and a nested-cell drill-in — each routed
                        // through the one-undo-unit edit pipeline. The worker is a
                        // separate field from the workspace, so the split borrow
                        // (`&mut doc` + `&self.worker`) is sound.
                        let worker = &self.worker;
                        if let Some(doc) = self.workspace.get_mut(idx) {
                            crate::panels::render_table_seam(ui, doc, worker);
                        }
                    }
                    ActiveView::TableSections => {
                        // Comparison variant of the Table view: the same central grid +
                        // breadcrumb + back/forward, but a scanner-driven grouped-sections
                        // navigator on the left instead of the tree outline. Split borrow
                        // (`&mut doc` + `&self.worker`) is sound (separate fields).
                        let worker = &self.worker;
                        if let Some(doc) = self.workspace.get_mut(idx) {
                            crate::panels::render_table_sections_seam(ui, doc, worker);
                        }
                    }
                    ActiveView::TableGrouped => {
                        // Pivot-style comparison variant (E021): same section navigator +
                        // breadcrumb, but the selected collection's rows are grouped by 1–2
                        // chosen fields into collapsible groups. Split borrow is sound.
                        let worker = &self.worker;
                        if let Some(doc) = self.workspace.get_mut(idx) {
                            crate::panels::render_table_grouped_seam(ui, doc, worker);
                        }
                    }
                }
                // E012 / US3 / FR-007+FR-015: persist the active document's live column
                // layout back to settings AFTER rendering, so a hide/reorder/pin made this
                // frame survives the next launch — and a reset (which dropped the live state)
                // clears the persisted entry, returning the section to its default (FR-015).
                self.save_active_column_layouts(idx);
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

    /// Render the per-document view switcher (E008 — T014/FR-017).
    ///
    /// A three-way selector (Text / Tree-form / Table) bound to the document's
    /// [`ViewSelectionAndFocus::active_view`](crate::structural::view_state::ViewSelectionAndFocus::active_view).
    /// Switching is lossless: it changes zero document bytes (FR-020) and never
    /// clears an in-progress draft/focus (FR-017) — the view state keeps the focus,
    /// which is re-resolved by structural-path identity across reparse (FR-016). A
    /// no-op when no document is supplied.
    fn render_view_switcher(ui: &mut egui::Ui, doc: Option<&mut EditorDocument>) {
        let Some(doc) = doc else {
            return;
        };
        let mut view = doc.view_state().active_view();
        // E021: the legend goes on the SAME row as the tabs when there's room, else it
        // wraps to its own row below — egui reserves no space between a left-aligned run
        // and a right-aligned one, so a too-narrow row would overlap the tabs.
        let mut legend_wrapped = false;
        ui.horizontal(|ui| {
            ui.label("View:");
            // Switching only changes the active-view selection; the buffer is never
            // touched, so every view projects the same lossless CST source (FR-020).
            ui.selectable_value(&mut view, ActiveView::TreeForm, "Tree/form");
            ui.selectable_value(&mut view, ActiveView::Table, "Table (outline)");
            ui.selectable_value(&mut view, ActiveView::TableSections, "Table (sections)");
            ui.selectable_value(&mut view, ActiveView::TableGrouped, "Table (grouped)");
            ui.selectable_value(&mut view, ActiveView::Text, "Text");
            if doc.view_state().is_stale() {
                // FR-015: a user-perceivable stale marker while a reparse is pending.
                ui.weak("(updating\u{2026})");
            }
            // The always-visible type-indicator legend (E015): right-aligned on this SAME
            // row when it fits (zero extra vertical space), using the SAME glyphs/colors
            // the tree + table paint. If the remaining width is too small it would overlap
            // the tabs, so defer it to its own row below (E021).
            if ui.available_width() >= legend_min_width() {
                // Right-align the legend on this row while keeping its forward (left→right)
                // order: a `left_to_right` group placed by an outer `right_to_left` (E023).
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        legend_strip(ui);
                    });
                });
            } else {
                legend_wrapped = true;
            }
        });
        if legend_wrapped {
            ui.horizontal(legend_strip);
        }
        if view != doc.view_state().active_view() {
            // Keep any in-progress draft/focus across the switch (FR-017) — we only
            // change the active view; focus is preserved and re-resolved on reparse.
            doc.view_state_mut().set_active_view(view);
        }
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

    /// Render the crash-recovery restore offer as a modal window when one is open
    /// (E007 OBJ2 — TR-008, SC-003).
    ///
    /// Offers Restore (recover the in-progress autosaved work) vs. Open On-Disk File
    /// (decline). Modelled on [`render_dirty_prompt`](Self::render_dirty_prompt); the
    /// recovered content is never silently loaded nor silently discarded — the user
    /// chooses.
    fn render_recovery_offer(&mut self, ui: &mut egui::Ui) {
        let Some(offer) = &self.recovery_offer else {
            return;
        };
        let name = offer
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("this file")
            .to_string();

        let mut choice: Option<RecoveryChoice> = None;
        egui::Window::new("Recover unsaved work")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                ui.label(format!(
                    "RONin found autosaved in-progress work for \"{name}\" that differs \
                     from the file on disk."
                ));
                ui.label("Restore the recovered work, or open the file as it is on disk?");
                ui.horizontal(|ui| {
                    if ui.button("Restore recovered work").clicked() {
                        choice = Some(RecoveryChoice::Restore);
                    }
                    if ui.button("Open on-disk file").clicked() {
                        choice = Some(RecoveryChoice::Decline);
                    }
                });
            });
        if let Some(choice) = choice {
            self.resolve_recovery_offer(choice);
        }
    }

    /// Render the pre-conversion loss-report dialog as a modal window when a
    /// RON→JSON conversion is pending (E010 US1 — T016/T017, FR-005, SC-002).
    ///
    /// Lists the lossy constructs (per-kind counts + the per-construct detail), hosts
    /// the JSONC-vs-strict **per-conversion override** control (FR-008), and requires
    /// an explicit Convert / Cancel. Cancel changes zero bytes and writes nothing
    /// (SC-002/003). The SAME `loss_report.constructs()` list drives this dialog AND
    /// the inline diagnostics already published on the document (FR-007).
    fn render_conversion_dialog(&mut self, ui: &mut egui::Ui) {
        let Some(pending) = self.pending_conversion.as_ref() else {
            return;
        };
        let counts = pending.loss_report.counts_by_kind();
        let total = pending.loss_report.len();
        let is_export = matches!(pending.target, ConvertTarget::Export(_));
        let mut format = pending.format;

        let mut confirm: Option<bool> = None;
        let mut format_changed = false;
        egui::Window::new("Convert to JSON — review losses")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                ui.label(format!(
                    "This conversion has {total} construct(s) that JSON cannot represent losslessly:"
                ));
                // Per-kind summary (count + the kind's stable label).
                egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                    for (kind, n) in &counts {
                        ui.label(format!("\u{2022} {n} \u{00D7} {} [{}]", kind.label(), kind.code()));
                    }
                });
                ui.separator();
                // The per-conversion JSONC-vs-strict override (FR-008, NEW-CONFIG).
                ui.label("Output format (this conversion only):");
                ui.horizontal(|ui| {
                    format_changed |= ui
                        .radio_value(&mut format.format, JsonFormat::Jsonc, "JSONC (comments inline)")
                        .changed();
                    format_changed |= ui
                        .radio_value(
                            &mut format.format,
                            JsonFormat::StrictJson,
                            "Strict JSON",
                        )
                        .changed();
                });
                if format.format == JsonFormat::StrictJson {
                    ui.horizontal(|ui| {
                        format_changed |= ui
                            .radio_value(
                                &mut format.strict_carrier,
                                StrictCommentCarrier::Sidecar,
                                "Comments \u{2192} sidecar",
                            )
                            .changed();
                        format_changed |= ui
                            .radio_value(
                                &mut format.strict_carrier,
                                StrictCommentCarrier::PureNoComments,
                                "Drop comments (reported)",
                            )
                            .changed();
                    });
                }
                ui.separator();
                ui.horizontal(|ui| {
                    let confirm_label = if is_export { "Export" } else { "Convert" };
                    if ui.button(confirm_label).clicked() {
                        confirm = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        confirm = Some(false);
                    }
                });
            });

        // A format flip re-builds the conversion (text + losses) under the new mode,
        // keeping the same target — the override beats the persisted default for this
        // run only (FR-008). Re-entry preserves the open dialog.
        if format_changed && confirm.is_none() {
            if let Some(p) = self.pending_conversion.take() {
                self.begin_conversion(p.target, format);
            }
            return;
        }
        if let Some(confirm) = confirm {
            self.resolve_conversion(confirm);
        }
    }

    /// Render the unparseable-RON block-vs-convert-remainder prompt as a modal
    /// window when one is open (E010 US1 — T016, FR-013, SC-008).
    ///
    /// Block aborts with a clear error locating the region (no output, zero bytes);
    /// convert-remainder emits the parseable portion with each unparseable region as
    /// a flagged, loss-reported placeholder.
    fn render_partial_ron_prompt(&mut self, ui: &mut egui::Ui) {
        let Some(prompt) = self.partial_ron_prompt.as_ref() else {
            return;
        };
        let start = prompt.first_error.start();
        let mut choice: Option<PartialRonChoice> = None;
        egui::Window::new("Unparseable RON")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                ui.label(format!(
                    "This RON has an unparseable region (near byte {start})."
                ));
                ui.label(
                    "Block the conversion to fix it first, or convert the parseable \
                     remainder (each unparseable region becomes a flagged placeholder).",
                );
                ui.horizontal(|ui| {
                    if ui.button("Block (locate)").clicked() {
                        choice = Some(PartialRonChoice::Block);
                    }
                    if ui.button("Convert remainder").clicked() {
                        choice = Some(PartialRonChoice::ConvertRemainder);
                    }
                });
            });
        if let Some(choice) = choice {
            self.resolve_partial_ron_prompt(choice);
        }
    }

    /// Render the Settings window when open, with the adjustable formatter
    /// controls (FR-007/FR-023).
    ///
    /// A non-modal, dismissible window (never blocks editing). It exposes the three
    /// formatter knobs — indent width (clamped `1..=16`), blank-line policy, and the
    /// format-on-save toggle — bound directly to [`AppSettings::formatting`], so a
    /// change is folded into settings and persisted on the next save tick (FR-016).
    fn render_settings_window(&mut self, ui: &mut egui::Ui) {
        if !self.show_settings {
            return;
        }
        let mut open = self.show_settings;
        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                ui.heading("Formatting");
                ui.add_space(4.0);

                let fmt = &mut self.settings.formatting;

                // Indent width — clamped to the sane range by the widget bounds and,
                // belt-and-suspenders, by `set_indent_width` on read.
                ui.horizontal(|ui| {
                    ui.label("Indent width");
                    let mut width = fmt.indent_width;
                    if ui
                        .add(egui::Slider::new(
                            &mut width,
                            FormattingConfig::min_indent_width()
                                ..=FormattingConfig::max_indent_width(),
                        ))
                        .changed()
                    {
                        fmt.set_indent_width(width);
                    }
                });

                // Blank-line policy.
                ui.horizontal(|ui| {
                    ui.label("Blank lines");
                    egui::ComboBox::from_id_salt("blank_line_policy")
                        .selected_text(match fmt.blank_line_policy {
                            BlankLinePolicy::Collapse => "Collapse to one",
                            BlankLinePolicy::Preserve => "Preserve",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut fmt.blank_line_policy,
                                BlankLinePolicy::Collapse,
                                "Collapse to one",
                            );
                            ui.selectable_value(
                                &mut fmt.blank_line_policy,
                                BlankLinePolicy::Preserve,
                                "Preserve",
                            );
                        });
                });

                // Format-on-save toggle.
                ui.checkbox(&mut fmt.format_on_save, "Format on save");

                ui.add_space(6.0);
                ui.weak("Run Format \u{25B8} Format Document or Format Selection from the menu.");
            });
        self.show_settings = open;
    }

    /// Render the Snippets browser window when open (FR-025).
    ///
    /// A non-modal, dismissible window listing every effective snippet by its prefix
    /// and description (discoverability), with an Insert button (active document
    /// only) that routes through the round-trip-verified insertion path, plus the
    /// open/locate-user-file and reload commands. Never blocks editing.
    fn render_snippets_window(&mut self, ui: &mut egui::Ui) {
        if !self.show_snippets {
            return;
        }
        let mut open = self.show_snippets;
        let has_active = self.active_document().is_some();
        // Snapshot so the window can render while a click later mutates the document.
        let entries: Vec<(String, String, String)> = self
            .snippets
            .iter()
            .map(|s| (s.name.clone(), s.prefix.clone(), s.description.clone()))
            .collect();
        let path_label = self.user_snippet_path().map_or_else(
            || "(no config dir)".to_string(),
            |p| p.display().to_string(),
        );

        let mut insert: Option<String> = None;
        let mut open_file = false;
        let mut reload = false;
        egui::Window::new("Snippets")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                ui.label("Available snippets (built-in + user):");
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .max_height(360.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for (name, prefix, description) in &entries {
                            ui.horizontal(|ui| {
                                ui.add_enabled_ui(has_active, |ui| {
                                    if ui.button("Insert").clicked() {
                                        insert = Some(name.clone());
                                    }
                                });
                                ui.monospace(prefix);
                                ui.weak(description);
                            });
                        }
                    });
                ui.separator();
                ui.weak(format!("User file: {path_label}"));
                ui.horizontal(|ui| {
                    if ui.button("Open User Snippet File\u{2026}").clicked() {
                        open_file = true;
                    }
                    if ui.button("Reload").clicked() {
                        reload = true;
                    }
                });
            });
        self.show_snippets = open;
        if let Some(name) = insert {
            self.insert_snippet_by_name(&name);
        }
        if open_file {
            self.open_user_snippet_file();
        }
        if reload {
            self.reload_snippets();
        }
    }

    /// Render the Type Bindings window when open (E006 US2 — FR-009/FR-011).
    ///
    /// A non-modal, dismissible window with two surfaces:
    ///
    /// * **Per-document override** ("validate against type X") — the active document's
    ///   binding state, a Set/Update control that commits the override draft, and a
    ///   Clear control. Setting/clearing re-applies the binding immediately so the
    ///   override (override > config) takes effect on the next worker pass (FR-009).
    /// * **Project rules** — the [`BindingConfig`] rule list with remove / edit and an
    ///   add/edit form (pattern, optional excludes, type name, source kind + path).
    ///   Any change persists best-effort to `<root>/.ronin/bindings.json` and
    ///   re-applies bindings to open docs (FR-008/FR-013).
    ///
    /// Mutations are collected into local variables during the closure (which borrows
    /// `self` fields for the form drafts) and applied after the window closes, so the
    /// borrow checker stays happy and the render does not reshape the config
    /// mid-iteration.
    fn render_bindings_window(&mut self, ui: &mut egui::Ui) {
        if !self.show_bindings {
            return;
        }
        let mut open = self.show_bindings;

        // Snapshot read-only display state the closure needs.
        let has_active = self.active_document().is_some();
        let binding_label = self.active_document().map_or_else(
            || "no document open".to_string(),
            EditorDocument::binding_label,
        );
        let binding_source = self
            .active_document()
            .and_then(EditorDocument::binding_source_label);
        let has_override = self
            .active_document()
            .is_some_and(|d| d.override_.is_some());
        let rules: Vec<BindingRule> = self.binding_config.rules.clone();

        // Deferred actions decided inside the closure, applied after it returns.
        let mut set_override = false;
        let mut clear_override = false;
        let mut remove_rule: Option<usize> = None;
        let mut load_rule_for_edit: Option<usize> = None;
        let mut commit_rule = false;

        egui::Window::new("Type Bindings")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                // ---- Per-document override (FR-009/FR-011) ----
                ui.heading("Active document");
                ui.add_space(2.0);
                ui.label(format!("Active binding: {binding_label}"));
                if let Some(source) = &binding_source {
                    ui.weak(source);
                }
                ui.add_space(4.0);
                ui.label("Override (validate against type X):");
                binding_source_form(ui, &mut self.override_draft, false);
                ui.horizontal(|ui| {
                    let can_set = has_active && self.override_draft.to_override().is_some();
                    ui.add_enabled_ui(can_set, |ui| {
                        if ui.button("Set Override").clicked() {
                            set_override = true;
                        }
                    });
                    ui.add_enabled_ui(has_active && has_override, |ui| {
                        if ui.button("Clear Override").clicked() {
                            clear_override = true;
                        }
                    });
                });

                ui.separator();

                // ---- Project rules (FR-008/FR-013) ----
                ui.heading("Project rules");
                ui.weak(format!(
                    "Stored at {}",
                    BindingConfig::project_config_path(&self.binding_root).display()
                ));
                ui.add_space(2.0);
                if rules.is_empty() {
                    ui.weak("(no rules — documents resolve to \"no type bound\")");
                }
                egui::ScrollArea::vertical()
                    .max_height(200.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for (i, rule) in rules.iter().enumerate() {
                            ui.horizontal(|ui| {
                                let src = match &rule.type_source {
                                    TypeSourceLocator::SchemaFile(p) => {
                                        format!("schema: {}", p.display())
                                    }
                                    TypeSourceLocator::RustSource(p) => {
                                        format!("rust: {}", p.display())
                                    }
                                };
                                ui.monospace(&rule.pattern);
                                ui.label(format!("\u{2192} {} ({src})", rule.type_name));
                                if ui.small_button("Edit").clicked() {
                                    load_rule_for_edit = Some(i);
                                }
                                if ui.small_button("Remove").clicked() {
                                    remove_rule = Some(i);
                                }
                            });
                        }
                    });

                ui.add_space(4.0);
                let editing = self.editing_rule;
                ui.label(match editing {
                    Some(i) => format!("Edit rule #{i}"),
                    None => "Add rule".to_string(),
                });
                binding_source_form(ui, &mut self.rule_draft, true);
                ui.horizontal(|ui| {
                    let can_commit = self.rule_draft.to_rule().is_some();
                    ui.add_enabled_ui(can_commit, |ui| {
                        let label = if editing.is_some() { "Update" } else { "Add" };
                        if ui.button(label).clicked() {
                            commit_rule = true;
                        }
                    });
                    if editing.is_some() && ui.button("Cancel Edit").clicked() {
                        load_rule_for_edit = None;
                        self.editing_rule = None;
                        self.rule_draft = crate::settings::BindingFormDraft::default();
                    }
                });
            });
        self.show_bindings = open;

        // ---- Apply deferred actions ----
        if set_override {
            if let Some(ov) = self.override_draft.to_override() {
                self.set_active_override(ov.type_name, ov.type_source);
            }
        }
        if clear_override {
            self.clear_active_override();
        }
        if let Some(i) = remove_rule {
            // If we were editing the removed rule, drop the edit context.
            if self.editing_rule == Some(i) {
                self.editing_rule = None;
                self.rule_draft = crate::settings::BindingFormDraft::default();
            }
            self.remove_binding_rule(i);
        }
        if let Some(i) = load_rule_for_edit {
            if let Some(rule) = self.binding_config.rules.get(i) {
                self.rule_draft = crate::settings::BindingFormDraft {
                    pattern: rule.pattern.clone(),
                    exclude: rule
                        .exclude
                        .as_ref()
                        .map(|e| e.join(", "))
                        .unwrap_or_default(),
                    type_name: rule.type_name.clone(),
                    source_path: rule.type_source.path().display().to_string(),
                    source_kind: crate::settings::SourceKind::of(&rule.type_source),
                };
                self.editing_rule = Some(i);
            }
        }
        if commit_rule {
            if let Some(rule) = self.rule_draft.to_rule() {
                match self.editing_rule {
                    Some(i) => self.replace_binding_rule(i, rule),
                    None => self.add_binding_rule(rule),
                }
                self.editing_rule = None;
                self.rule_draft = crate::settings::BindingFormDraft::default();
            }
        }
    }

    /// Render the Bevy registries window: the per-document mode toggle, the
    /// active-mode/registry/staleness indicator, and the project registry-rule editor
    /// (E009 — FR-009/FR-010/FR-011).
    ///
    /// Modeled on [`render_bindings_window`](Self::render_bindings_window): mutations
    /// are collected into local flags inside the closure (which borrows `self` fields
    /// for the form draft) and applied after the window closes so the borrow checker
    /// stays happy. The toggle changes **zero** document bytes (FR-011, SC-003).
    fn render_registries_window(&mut self, ui: &mut egui::Ui) {
        if !self.show_registries {
            return;
        }
        let mut open = self.show_registries;

        // Snapshot read-only display state the closure needs.
        let has_active = self.active_document().is_some();
        let mode_label = self.mode_indicator_label();
        let registry_label = self.registry_indicator_label();
        let staleness = self
            .active_document()
            .and_then(EditorDocument::staleness_label);
        let is_bevy = self
            .active_document()
            .is_some_and(EditorDocument::is_bevy_mode);
        let rules: Vec<RegistryBindingRule> = self.registry_binding_config.rules.clone();
        let config_path = RegistryBindingConfig::project_config_path(&self.binding_root);

        // Deferred actions decided inside the closure, applied after it returns.
        let mut toggle_to: Option<Mode> = None;
        let mut remove_rule: Option<usize> = None;
        let mut load_rule_for_edit: Option<usize> = None;
        let mut commit_rule = false;

        egui::Window::new("Bevy Registries")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ui.ctx(), |ui| {
                // ---- Active document mode + registry (FR-009/FR-011) ----
                ui.heading("Active document");
                ui.add_space(2.0);
                ui.label(&mode_label);
                ui.label(&registry_label);
                if let Some(stale) = &staleness {
                    ui.weak(stale);
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    // The explicit serde ⇄ Bevy toggle (FR-009): zero-byte switch.
                    let caption = if is_bevy {
                        "Switch to serde"
                    } else {
                        "Switch to Bevy"
                    };
                    let target = if is_bevy { Mode::Serde } else { Mode::Bevy };
                    ui.add_enabled_ui(has_active, |ui| {
                        if ui.button(caption).clicked() {
                            toggle_to = Some(target);
                        }
                    });
                });

                ui.separator();

                // ---- Project registry rules (FR-010) ----
                ui.heading("Project registry rules");
                ui.weak(format!("Stored at {}", config_path.display()));
                ui.add_space(2.0);
                if rules.is_empty() {
                    ui.weak("(no rules — scenes resolve to \"no registry\")");
                }
                egui::ScrollArea::vertical()
                    .max_height(200.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for (i, rule) in rules.iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.monospace(&rule.pattern);
                                ui.label(format!(
                                    "\u{2192} {}",
                                    rule.registry_export_path.display()
                                ));
                                if ui.small_button("Edit").clicked() {
                                    load_rule_for_edit = Some(i);
                                }
                                if ui.small_button("Remove").clicked() {
                                    remove_rule = Some(i);
                                }
                            });
                        }
                    });

                ui.add_space(4.0);
                let editing = self.editing_registry_rule;
                ui.label(match editing {
                    Some(i) => format!("Edit rule #{i}"),
                    None => "Add rule".to_string(),
                });
                registry_rule_form(ui, &mut self.registry_rule_draft);
                ui.horizontal(|ui| {
                    let can_commit = self.registry_rule_draft.to_rule().is_some();
                    ui.add_enabled_ui(can_commit, |ui| {
                        let label = if editing.is_some() { "Update" } else { "Add" };
                        if ui.button(label).clicked() {
                            commit_rule = true;
                        }
                    });
                    if editing.is_some() && ui.button("Cancel Edit").clicked() {
                        load_rule_for_edit = None;
                        self.editing_registry_rule = None;
                        self.registry_rule_draft =
                            crate::settings::RegistryBindingFormDraft::default();
                    }
                });
            });
        self.show_registries = open;

        // ---- Apply deferred actions ----
        if let Some(mode) = toggle_to {
            self.set_active_mode(mode);
        }
        if let Some(i) = remove_rule {
            if self.editing_registry_rule == Some(i) {
                self.editing_registry_rule = None;
                self.registry_rule_draft = crate::settings::RegistryBindingFormDraft::default();
            }
            self.remove_registry_rule(i);
        }
        if let Some(i) = load_rule_for_edit {
            if let Some(rule) = self.registry_binding_config.rules.get(i) {
                self.registry_rule_draft = crate::settings::RegistryBindingFormDraft {
                    pattern: rule.pattern.clone(),
                    exclude: rule
                        .exclude
                        .as_ref()
                        .map(|e| e.join(", "))
                        .unwrap_or_default(),
                    registry_export_path: rule.registry_export_path.display().to_string(),
                    mode: rule.mode,
                    expected_bevy_version: rule.expected_bevy_version.clone().unwrap_or_default(),
                };
                self.editing_registry_rule = Some(i);
            }
        }
        if commit_rule {
            if let Some(rule) = self.registry_rule_draft.to_rule() {
                match self.editing_registry_rule {
                    Some(i) => self.replace_registry_rule(i, rule),
                    None => self.add_registry_rule(rule),
                }
                self.editing_registry_rule = None;
                self.registry_rule_draft = crate::settings::RegistryBindingFormDraft::default();
            }
        }
    }

    /// Render the always-visible active-mode/registry indicator + explicit toggle in
    /// the top mode-selector region (E009 — FR-009/FR-011, SC-003).
    ///
    /// This populates the seam E008/E009 reserved (replacing
    /// [`mode_selector_seam_stub`], kept only when no tab is open so the layout stays
    /// visible). For the active document it shows, always-on:
    ///
    /// * the **active mode** + origin ([`EditorDocument::mode_label`]) — `Mode: bevy
    ///   (auto)` / `Mode: serde (override)`;
    /// * the **bound registry** + registry state ([`EditorDocument::registry_label`]) —
    ///   the NoRegistry-vs-loaded distinction (the three-state model surfaced as
    ///   `none` / `<name> (no registry loaded)` / `<name> (loaded)`, FR-006);
    /// * a **staleness** marker when the configured expected version disagrees with the
    ///   loaded registry's apparent version ([`EditorDocument::staleness_label`],
    ///   FR-008/FR-011);
    /// * an explicit **serde ⇄ Bevy toggle** button that flips the per-document mode
    ///   (overriding extension auto-detect, FR-009) — clicking it changes **zero**
    ///   document bytes (FR-011, SC-003), it only re-routes which validator runs.
    ///
    /// The toggle decision is collected during the closure and applied after it
    /// returns so the borrow of `self.active_document()` does not collide with the
    /// `&mut self` toggle call.
    fn render_mode_selector(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("mode_selector").show_inside(ui, |ui| {
            // No tab open: keep the reserved seam placeholder so the layout is visible.
            let Some(doc) = self.active_document() else {
                ui.horizontal(|ui| {
                    mode_selector_seam_stub(ui);
                });
                return;
            };
            let mode_label = doc.mode_label();
            // The active type-source label (E012): serde → `Types: <bound type | none>`
            // (the E006 binding), Bevy → the existing `Registry: …` registry label.
            let registry_label = doc.type_source_label();
            let staleness = doc.staleness_label();
            let is_bevy = doc.is_bevy_mode();
            // The toggle's target mode + its button caption.
            let (target, caption) = if is_bevy {
                (Mode::Serde, "Switch to serde")
            } else {
                (Mode::Bevy, "Switch to Bevy")
            };

            let mut do_toggle = false;
            ui.horizontal(|ui| {
                // Always-visible active mode (emphasized) + bound registry + state.
                ui.strong(&mode_label);
                ui.separator();
                // The active type-source label: a real type/registry source in both
                // modes now (serde `Types: …` / Bevy `Registry: …`), so it reads as a
                // regular label rather than a weak placeholder.
                ui.label(&registry_label);
                // Staleness advisory (FR-008): shown only when warranted, as a weak
                // marker — it is an advisory, never an error.
                if let Some(stale) = &staleness {
                    ui.separator();
                    ui.weak(stale);
                }
                // The explicit per-document toggle (FR-009): zero-byte mode switch.
                if ui.button(caption).clicked() {
                    do_toggle = true;
                }
            });

            // Applied after the closure so the immutable borrow above is released.
            if do_toggle {
                self.set_active_mode(target);
            }
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
        // The crash-recovery restore offer floats above everything when open (E007
        // OBJ2 — TR-008).
        self.render_recovery_offer(ui);
        // The RON→JSON loss-report dialog + the unparseable-RON prompt float above
        // everything when open (E010 US1 — FR-005/013, SC-002/008).
        self.render_conversion_dialog(ui);
        self.render_partial_ron_prompt(ui);
        // The Settings window floats above everything when open (FR-007/FR-023).
        self.render_settings_window(ui);
        // The Snippets browser floats above everything when open (FR-025).
        self.render_snippets_window(ui);
        // The Type Bindings window floats above everything when open (E006 US2 —
        // FR-009/FR-011).
        self.render_bindings_window(ui);
        // The Bevy Registries window floats above everything when open (E009 —
        // FR-009/FR-010/FR-011).
        self.render_registries_window(ui);
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Dropped-file intake before any rendering so a new tab shows this frame.
        let ctx = ui.ctx().clone();
        // Show tooltips instantly (the legend + every hover tooltip): zero egui's
        // hover-delay and the grace window so a tooltip appears the moment the
        // pointer rests on a widget, with no wait. Global by intent
        // (`global_style_mut`, the non-deprecated 0.34 spelling of `style_mut`).
        ctx.global_style_mut(|s| {
            s.interaction.tooltip_delay = 0.0;
            s.interaction.tooltip_grace_time = 0.0;
        });
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

        // E007 OBJ2 (AD-004/TR-016/TR-023): the cheap per-frame autosave debounce
        // check. It does NO I/O on this path — when it fires it hands a snapshot to
        // the off-frame autosave worker, which performs the atomic sidecar write on
        // its own thread (SC-008). `Instant::now()` is the live clock; tests drive
        // the deterministic seam (`force_autosave_all`) instead (TR-020).
        self.maybe_autosave(Instant::now());

        // E007 OBJ3 (TR-016/TR-023, SC-008): the cheap per-frame undo-snapshot
        // bookkeeping. Like autosave it does the heavy work (parse + snapshot) at
        // most once per coalesce window, never per keystroke, so the per-frame
        // `update` undo cost stays within the frame budget on a large buffer.
        self.record_undo_snapshots(Instant::now());

        // Undo/redo keyboard commands (E007 OBJ3 — TR-010, SC-005). Standard
        // shortcuts: Ctrl/Cmd+Z undo, Ctrl/Cmd+Y or Ctrl/Cmd+Shift+Z redo. Consumed
        // before the editor so they drive the document history, not egui's own
        // TextEdit undo. A no-op when no tab is open / nothing to (re)do.
        self.handle_undo_shortcuts(&ctx);

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
        // E007 OBJ2 (TR-009): a clean exit removes the recovery sidecars of cleanly-
        // saved documents so a stale/orphan sidecar is never offered next launch. A
        // dirty document's sidecar is intentionally kept — it is the crash-recovery
        // copy of unsaved work this exit leaves behind (SC-003).
        self.cleanup_sidecars_on_exit();
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

/// Render the shared type-name + source (kind + path) input rows of a binding form
/// into `ui`, optionally including the rule-only pattern / exclude rows (E006 US2 —
/// FR-008/FR-009).
///
/// `include_pattern` is `true` for the project-rule form (which needs a glob pattern
/// and optional excludes) and `false` for the per-document override form (which
/// targets the active document directly). Mirrors the existing settings-surface
/// widgets (labelled rows, a `ComboBox` for the enum). The draft is mutated in
/// place; the caller decides when to commit it.
fn binding_source_form(
    ui: &mut egui::Ui,
    draft: &mut crate::settings::BindingFormDraft,
    include_pattern: bool,
) {
    use crate::settings::SourceKind;
    if include_pattern {
        ui.horizontal(|ui| {
            ui.label("Pattern");
            ui.text_edit_singleline(&mut draft.pattern);
        });
        ui.horizontal(|ui| {
            ui.label("Exclude");
            ui.text_edit_singleline(&mut draft.exclude);
        });
    }
    ui.horizontal(|ui| {
        ui.label("Type name");
        ui.text_edit_singleline(&mut draft.type_name);
    });
    ui.horizontal(|ui| {
        ui.label("Source kind");
        egui::ComboBox::from_id_salt(if include_pattern {
            "rule_source_kind"
        } else {
            "override_source_kind"
        })
        .selected_text(match draft.source_kind {
            SourceKind::Schema => "Schema file",
            SourceKind::Rust => "Rust source",
        })
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut draft.source_kind, SourceKind::Schema, "Schema file");
            ui.selectable_value(&mut draft.source_kind, SourceKind::Rust, "Rust source");
        });
    });
    ui.horizontal(|ui| {
        ui.label("Source path");
        ui.text_edit_singleline(&mut draft.source_path);
    });
}

/// Render the registry-binding-rule add/edit form into `ui`, editing `draft`
/// in-place (E009 — FR-010).
///
/// Mirrors [`binding_source_form`] for the parallel registry config: a glob
/// `pattern`, optional comma-separated `exclude` globs, the `registry_export_path`
/// (a local data file; may be absolute / out-of-tree), an optional per-pattern mode
/// hint, and an optional expected Bevy version for the staleness advisory. The
/// caller's commit is gated on [`RegistryBindingFormDraft::to_rule`] returning
/// `Some` (a blank pattern or export path is rejected).
fn registry_rule_form(ui: &mut egui::Ui, draft: &mut crate::settings::RegistryBindingFormDraft) {
    ui.horizontal(|ui| {
        ui.label("Pattern");
        ui.text_edit_singleline(&mut draft.pattern);
    });
    ui.horizontal(|ui| {
        ui.label("Exclude");
        ui.text_edit_singleline(&mut draft.exclude);
    });
    ui.horizontal(|ui| {
        ui.label("Registry export");
        ui.text_edit_singleline(&mut draft.registry_export_path);
    });
    ui.horizontal(|ui| {
        ui.label("Mode hint");
        egui::ComboBox::from_id_salt("registry_rule_mode_hint")
            .selected_text(match draft.mode {
                None => "(none)",
                Some(Mode::Serde) => "serde",
                Some(Mode::Bevy) => "bevy",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut draft.mode, None, "(none)");
                ui.selectable_value(&mut draft.mode, Some(Mode::Serde), "serde");
                ui.selectable_value(&mut draft.mode, Some(Mode::Bevy), "bevy");
            });
    });
    ui.horizontal(|ui| {
        ui.label("Expected Bevy version");
        ui.text_edit_singleline(&mut draft.expected_bevy_version);
    });
}

/// A short display name for a path (file/dir name, falling back to the full path).
fn display_name(path: &std::path::Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| path.display().to_string())
}

/// Launch the OS default handler for `path` (the snippet open-file command, FR-025).
///
/// Uses the platform's "open with default app" command (`cmd /c start` on Windows,
/// `open` on macOS, `xdg-open` elsewhere). Best-effort: a non-zero exit or a missing
/// opener is reported as an error so the caller can fall back to showing the path.
/// Never executes any user RON (project-instructions §VI) — it only hands the file
/// path to the OS shell to open in the user's editor of choice.
fn open_in_os(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    let mut cmd = {
        // `start` is a cmd builtin; the empty "" is the (ignored) window title so a
        // quoted path is not mistaken for one.
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]).arg(path);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(path);
        c
    };
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(path);
        c
    };

    let status = cmd.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "opener exited with status {status}"
        )))
    }
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

/// Map a `[char_start, char_end)` selection to absolute byte offsets in `buffer`
/// (FR-002).
///
/// The editor surface tracks the selection in **character** offsets (encoding-width
/// independent), while the `ronin-core` CST works in **byte** offsets; Format
/// Selection must bridge the two. Returns `None` only when an offset falls past the
/// buffer's character count (a stale selection), so the caller can decline rather
/// than splice on a bogus range. `char_end == char_len` maps to `buffer.len()`.
fn char_range_to_bytes(buffer: &str, char_start: usize, char_end: usize) -> Option<(usize, usize)> {
    let mut byte_start: Option<usize> = None;
    let mut byte_end: Option<usize> = None;
    // `char_indices` yields (byte_offset, char) in order; the char *count* index `i`
    // maps to that char's starting byte offset. The end-of-buffer index maps to len.
    for (i, (byte, _)) in buffer.char_indices().enumerate() {
        if i == char_start {
            byte_start = Some(byte);
        }
        if i == char_end {
            byte_end = Some(byte);
        }
    }
    let char_len = buffer.chars().count();
    if char_start == char_len {
        byte_start = Some(buffer.len());
    }
    if char_end == char_len {
        byte_end = Some(buffer.len());
    }
    match (byte_start, byte_end) {
        (Some(s), Some(e)) if s <= e => Some((s, e)),
        _ => None,
    }
}

/// Find the smallest CST value-position node fully enclosing `[byte_start, byte_end)`
/// (FR-002/FR-023).
///
/// Walks the parsed CST from the root, descending into the deepest child whose byte
/// range fully covers the selection, and returns the smallest such node whose kind is
/// a clean subtree boundary for `ronin_core::format_node` (a struct / tuple / list /
/// map / enum-variant / unit / literal). When the smallest covering node is not itself
/// a value node (e.g. a bare `StructField` or `MapEntry`), the nearest enclosing value
/// ancestor is returned instead, so a selection landing inside a field still maps to a
/// formattable subtree. Returns `None` when no value node covers the selection (the
/// caller then declines and notifies).
fn smallest_enclosing_value_node(
    doc: &ronin_core::CstDocument,
    byte_start: usize,
    byte_end: usize,
) -> Option<SyntaxNode> {
    let mut node = doc.root();
    // Descend greedily into the deepest child that still fully covers the selection.
    loop {
        let next = node.children().find(|child| {
            let r = child.text_range();
            r.start() <= byte_start && byte_end <= r.end()
        });
        match next {
            Some(child) => node = child,
            None => break,
        }
    }
    // `node` is now the smallest node covering the selection. Climb to the nearest
    // value-position ancestor (including `node` itself) so `format_node` gets a clean
    // subtree boundary.
    let mut candidate = Some(node);
    while let Some(n) = candidate {
        if is_value_node_kind(n.kind()) {
            return Some(n);
        }
        candidate = n.parent();
    }
    None
}

/// Whether `kind` is a value-position node kind accepted by `ronin_core::format_node`
/// as a clean Format-Selection subtree boundary.
fn is_value_node_kind(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::Struct
            | SyntaxKind::Tuple
            | SyntaxKind::List
            | SyntaxKind::Map
            | SyntaxKind::EnumVariant
            | SyntaxKind::Unit
            | SyntaxKind::Literal
    )
}
