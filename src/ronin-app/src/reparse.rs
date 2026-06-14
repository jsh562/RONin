//! Off-thread, generation-keyed reparsing against `ron-core` (FR-006).
//!
//! Parsing and diagnostic collection run **off** the per-frame UI path so the
//! editor stays within its frame budget (project-instructions §Performance). A
//! [`ParseResult`] bundles the lossless CST, the (defensively range-clamped)
//! diagnostics, the source length it was computed from, and a monotonically
//! increasing `generation` that lets the consumer discard stale results
//! (install-when-current / discard-when-stale).
//!
//! [`ReparseWorker`] owns a background thread that turns `(generation, text,
//! bound)` jobs into [`ParseResult`]s — parsing structurally and, when a type
//! binding has resolved, running `ron-validate` off-frame too (E006/FR-006) —
//! and ships them back over a channel. The worker only
//! *computes*; the document decides staleness by comparing generations. The
//! worker is panic-isolated and exits cleanly when dropped. When an
//! [`egui::Context`] is installed via [`ReparseWorker::set_repaint_ctx`], the
//! worker wakes an idle UI after each result so the shell need not repaint
//! continuously at rest (FR-024 groundwork).

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use ron_core::{CstDocument, Diagnostic, TextRange};

/// The type a document is bound to, carried to the off-frame worker so it can run
/// type validation after the structural parse (E006/FR-006, FR-021 groundwork).
///
/// The `model` is wrapped in an [`Arc`] so each per-keystroke
/// [`ReparseWorker::request`] clones only a pointer, never the (potentially large)
/// serialized `TypeModel`. `type_name` selects the named def to validate against
/// inside the model's `#/$defs`.
///
/// The real `BindingConfig`→`BoundType` resolution is Phase 4 (US2); for now this
/// is the seam the document fills (default `None`) and the worker consumes.
#[derive(Clone)]
pub struct BoundType {
    /// E004's serialized `TypeModel` interchange (JSON-Schema 2020-12 + `x-ron-*`),
    /// shared by `Arc` so per-keystroke job sends do not deep-clone it.
    pub model: Arc<serde_json::Value>,
    /// The name of the def in `model.$defs` to validate the document against.
    pub type_name: String,
}

impl std::fmt::Debug for BoundType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `model` can be large; summarise rather than dumping the whole schema.
        f.debug_struct("BoundType")
            .field("type_name", &self.type_name)
            .finish_non_exhaustive()
    }
}

/// The product of one parse: the CST, clamped diagnostics, the source length the
/// parse was run against, and the generation that produced it (FR-006).
///
/// `generation` is the staleness key: a consumer holding generation *g* installs
/// a result only when `result.generation >= g_installed`, discarding anything
/// older (see [`ParseResult::supersedes`]).
#[derive(Clone)]
pub struct ParseResult {
    /// The lossless concrete syntax tree from `ron-core::parse`.
    pub cst: CstDocument,
    /// Parse diagnostics, each with its byte range clamped to `[0, source_len]`.
    pub diagnostics: Vec<Diagnostic>,
    /// Type-validation diagnostics from `ron-validate`, each with its byte range
    /// clamped to `[0, source_len]` (E006/FR-006, FR-021).
    ///
    /// Empty when the document has no resolved binding (`bound` was `None`,
    /// FR-015). Computed on the worker thread after the structural parse, never on
    /// the per-frame path (FR-006). The consumer republishes this whole set each
    /// pass (replace, not merge — FR-006).
    pub type_diagnostics: Vec<Diagnostic>,
    /// Byte length of the text this result was parsed from.
    pub source_len: usize,
    /// Monotonic generation tag identifying which edit produced this result.
    pub generation: u64,
}

impl std::fmt::Debug for ParseResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `CstDocument` is opaque (no Debug); summarise instead of forwarding.
        f.debug_struct("ParseResult")
            .field("generation", &self.generation)
            .field("source_len", &self.source_len)
            .field("diagnostics", &self.diagnostics.len())
            .field("type_diagnostics", &self.type_diagnostics.len())
            .finish()
    }
}

impl ParseResult {
    /// Parse `text` with `ron-core` and capture the result at `generation`, with no
    /// type validation (no resolved binding).
    ///
    /// Each diagnostic's byte range is defensively clamped to `[0, source_len]`
    /// (`ron-core` already keeps ranges in bounds, but the shell must never trust
    /// an out-of-range span when later projecting it onto the buffer).
    #[must_use]
    pub fn parse(text: &str, generation: u64) -> Self {
        Self::parse_with_binding(text, generation, None)
    }

    /// Parse `text` with `ron-core` and, when `bound` is `Some`, additionally run
    /// `ron-validate` against the bound type — all on the calling (worker) thread,
    /// never the per-frame path (E006/FR-006).
    ///
    /// The structural diagnostics are always computed; the type diagnostics are
    /// computed only when a binding is present (`None` ⇒ empty, FR-015). Both sets
    /// are defensively clamped to `[0, source_len]`. The CST is the structural
    /// parse's CST in either case (type validation is read-only over it, FR-022).
    #[must_use]
    pub fn parse_with_binding(text: &str, generation: u64, bound: Option<&BoundType>) -> Self {
        let source_len = text.len();
        let cst = ron_core::parse(text);
        let diagnostics: Vec<Diagnostic> = cst
            .diagnostics()
            .iter()
            .map(|d| clamp_diagnostic(d, source_len))
            .collect();
        // Type validation runs only when a binding resolved (FR-015). It is
        // read-only over the CST and the bound model (FR-022) and yields a full
        // set to publish in place of the prior type set (replace, not merge —
        // FR-006). `ron_validate` internally skips findings inside `ron-core`
        // parse-error node spans, so a malformed region never produces a cascade
        // of false type errors while the parseable remainder is still validated
        // (FR-019); the structural diagnostics above still cover the malformed
        // span. Overlap dedup against structural is applied at the publish point
        // (`document::merge_type_diagnostics`, FR-017).
        let type_diagnostics = match bound {
            Some(bound) => ron_validate::validate_against(&bound.model, &bound.type_name, &cst)
                .iter()
                .map(|d| clamp_diagnostic(d, source_len))
                .collect(),
            None => Vec::new(),
        };
        Self {
            cst,
            diagnostics,
            type_diagnostics,
            source_len,
            generation,
        }
    }

    /// `true` when this result should replace one currently at `installed_gen`.
    ///
    /// Newer-or-equal generations win; strictly older ones are stale and must be
    /// discarded. This encodes the install-when-current / discard-when-stale rule
    /// the consumer applies to worker output.
    #[must_use]
    pub fn supersedes(&self, installed_gen: u64) -> bool {
        self.generation >= installed_gen
    }
}

/// Clamp a diagnostic's byte range into `[0, source_len]`, preserving everything
/// else. Defensive: guards the shell against ever indexing past the buffer.
fn clamp_diagnostic(diag: &Diagnostic, source_len: usize) -> Diagnostic {
    let start = diag.range.start().min(source_len);
    let end = diag.range.end().min(source_len);
    // `start <= end` always holds: both are clamped by the same monotone min.
    let mut clamped = diag.clone();
    clamped.range = TextRange::new(start, end);
    clamped
}

/// A reparse job handed to the background worker.
struct ReparseJob {
    generation: u64,
    text: String,
    /// The resolved type binding to validate against, when any (E006/FR-006). The
    /// `Arc` inside [`BoundType`] keeps this cheap to send per keystroke.
    bound: Option<BoundType>,
}

/// Shared slot holding the [`egui::Context`] used to wake the UI when a result
/// lands (FR-024 groundwork). Cloned between the worker handle and its thread so
/// the UI can install the context *after* the worker is constructed.
type RepaintSlot = Arc<Mutex<Option<egui::Context>>>;

/// A background reparse thread (FR-006).
///
/// Send work with [`request`](Self::request); collect finished results
/// non-blockingly with [`poll`](Self::poll). The worker runs each parse inside
/// [`std::panic::catch_unwind`] so a hypothetical parser panic (which `ron-core`
/// guarantees never happens) cannot take down the worker thread — it simply
/// drops that job and keeps serving. Dropping the worker closes the job channel,
/// which lets the thread observe a disconnect and exit cleanly.
pub struct ReparseWorker {
    /// Outbound job channel to the worker thread. Held in an `Option` so it can
    /// be dropped *before* joining on shutdown (dropping it closes the channel,
    /// which lets the worker's blocking `recv` return and the thread exit).
    job_tx: Option<Sender<ReparseJob>>,
    /// Inbound result channel from the worker thread.
    result_rx: Receiver<ParseResult>,
    /// The worker thread handle, joined on drop after the channel closes.
    handle: Option<JoinHandle<()>>,
    /// Shared repaint context: the UI installs an [`egui::Context`] here so the
    /// worker thread can wake an idle UI when a result is ready (FR-024).
    repaint: RepaintSlot,
}

impl ReparseWorker {
    /// Spawn the background reparse thread.
    #[must_use]
    pub fn new() -> Self {
        let (job_tx, job_rx) = mpsc::channel::<ReparseJob>();
        let (result_tx, result_rx) = mpsc::channel::<ParseResult>();

        let repaint: RepaintSlot = Arc::new(Mutex::new(None));
        let worker_repaint = Arc::clone(&repaint);

        let handle = std::thread::Builder::new()
            .name("ronin-reparse".to_string())
            .spawn(move || worker_loop(&job_rx, &result_tx, &worker_repaint))
            // If the OS cannot spawn the thread, that is an environment failure
            // we cannot paper over; surfacing it at startup is correct.
            .expect("failed to spawn reparse worker thread");

        Self {
            job_tx: Some(job_tx),
            result_rx,
            handle: Some(handle),
            repaint,
        }
    }

    /// Install the [`egui::Context`] the worker should wake on result delivery
    /// (FR-024 groundwork).
    ///
    /// After this is set, the worker thread calls [`egui::Context::request_repaint`]
    /// immediately after shipping each [`ParseResult`], so an idle UI (which does
    /// not repaint at rest) is woken exactly when fresh diagnostics/highlighting
    /// are ready — no continuous polling required. Idempotent; the latest context
    /// wins.
    pub fn set_repaint_ctx(&self, ctx: egui::Context) {
        if let Ok(mut slot) = self.repaint.lock() {
            *slot = Some(ctx);
        }
    }

    /// Queue a reparse of `text` tagged with `generation`, validating against
    /// `bound` when a binding has resolved (E006/FR-006).
    ///
    /// Non-blocking. If the worker thread has gone away (only possible after a
    /// panic in this process's teardown), the job is silently dropped — the
    /// consumer keeps whatever result it last installed, never wedging the UI.
    /// When `bound` is `None`, only the structural parse runs (no type
    /// diagnostics, FR-015).
    pub fn request(&self, generation: u64, text: String, bound: Option<BoundType>) {
        if let Some(tx) = &self.job_tx {
            let _ = tx.send(ReparseJob {
                generation,
                text,
                bound,
            });
        }
    }

    /// Return the next finished [`ParseResult`] if one is ready, else `None`.
    ///
    /// Non-blocking (`try_recv`); intended to be drained once per UI frame.
    #[must_use]
    pub fn poll(&self) -> Option<ParseResult> {
        self.result_rx.try_recv().ok()
    }
}

impl Default for ReparseWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ReparseWorker {
    fn drop(&mut self) {
        // Close the job channel first: dropping the sender makes the worker's
        // blocking `recv` return `Err`, so the thread breaks its loop and exits.
        self.job_tx = None;
        // Then join for a clean, deterministic shutdown.
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The worker thread body: block on jobs, parse panic-isolated, ship results,
/// then wake the UI (FR-024).
fn worker_loop(
    job_rx: &Receiver<ReparseJob>,
    result_tx: &Sender<ParseResult>,
    repaint: &RepaintSlot,
) {
    // `recv` returns `Err` once every `Sender` is dropped — our exit signal.
    while let Ok(job) = job_rx.recv() {
        let ReparseJob {
            generation,
            text,
            bound,
        } = job;
        // Panic-isolate the parse *and* the type-validation pass. `ron-core`
        // guarantees parsing never panics, and `ron-validate` fails soft, so this
        // is belt-and-suspenders: a bug in either would drop one result rather
        // than kill the worker (FR-006 panic isolation covers validation too).
        let parsed = std::panic::catch_unwind(|| {
            ParseResult::parse_with_binding(&text, generation, bound.as_ref())
        });
        match parsed {
            Ok(result) => {
                // If the consumer is gone, stop working.
                if result_tx.send(result).is_err() {
                    break;
                }
                // Wake the (possibly idle) UI so it polls the new result. If no
                // context is installed yet, this is a no-op — the next frame will
                // still pick the result up via `poll`.
                if let Ok(slot) = repaint.lock() {
                    if let Some(ctx) = slot.as_ref() {
                        ctx.request_repaint();
                    }
                }
            }
            Err(_) => {
                // Swallow the panic payload and keep serving subsequent jobs.
                continue;
            }
        }
    }
}
