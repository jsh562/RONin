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
use ron_types::BevyRegistry;

use crate::bevy::{validate_scene, SceneDiagnostic};

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

/// The bound Bevy registry a *Bevy-mode* document validates against, carried to
/// the off-frame worker so scene-aware validation runs after the structural parse
/// (E009/FR-013).
///
/// This is the Bevy-mode analogue of [`BoundType`]: where serde mode carries a
/// single named type + its serialized `TypeModel`, Bevy mode carries the whole
/// bound [`BevyRegistry`] plus the **already-serialized** E004 interchange `model`
/// (`ron_types::to_json` of `BevySource::from_registry(registry).acquire().model`)
/// — produced **once** at registry-load time, not per keystroke — and the
/// optional configured expected Bevy version for the FR-008 staleness advisory.
/// Both the `model` and the `registry` are behind an [`Arc`] so each per-keystroke
/// [`ReparseWorker::request`] clones only pointers, never the (potentially large)
/// registry / schema.
///
/// Validation in Bevy mode runs [`validate_scene`] (the multi-subtree scene
/// validator) rather than `ron_validate::validate_against`: a Bevy-mode document
/// **replaces** its active source with the bound registry (AD-003/FR-013).
#[derive(Clone)]
pub struct BoundScene {
    /// E004's serialized `TypeModel` interchange for the bound registry, shared by
    /// `Arc`. Acquired once at registry-load time (`ron_types::to_json` of the
    /// `BevySource::from_registry(..).acquire().model`); the same serialization
    /// serde mode hands the validator.
    pub model: Arc<serde_json::Value>,
    /// The ingested registry the scene validator consults by type path (presence
    /// check), shared by `Arc` so per-keystroke sends do not deep-clone it.
    pub registry: Arc<BevyRegistry>,
    /// The optional configured expected Bevy version; when present and disagreeing
    /// with the registry's apparent version, a staleness advisory is appended by
    /// [`validate_scene`] (FR-008). `None` ⇒ no staleness advisory.
    pub expected_version: Option<String>,
}

impl std::fmt::Debug for BoundScene {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `model` + `registry` can be large; summarise rather than dumping them.
        f.debug_struct("BoundScene")
            .field("expected_version", &self.expected_version)
            .finish_non_exhaustive()
    }
}

/// The validation binding a document carries to the off-frame worker — exactly one
/// of the two mutually-exclusive modes (E009/FR-013).
///
/// A document is validated under exactly one mode's type source: **`Serde`** runs
/// `ron_validate::validate_against` against a single bound type (today's behavior,
/// byte-for-byte unchanged); **`Bevy`** runs [`validate_scene`] against the bound
/// registry, which **replaces** the active source (AD-003/FR-013). The variant the
/// document ships is chosen per-document by its [`ModeState`](crate::bevy::mode::ModeState),
/// so two open documents may carry different variants simultaneously (FR-012). A
/// document with no resolved binding ships `None` (structural-only, FR-015) — no
/// variant at all.
#[derive(Clone, Debug)]
pub enum BoundValidation {
    /// Serde-mode validation against a single bound type (E006 path, unchanged).
    Serde(BoundType),
    /// Bevy-mode scene validation against the bound registry (E009/FR-013).
    Bevy(BoundScene),
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
    /// Empty when the document has no resolved *serde* binding (`bound` was `None`
    /// or was the Bevy variant, FR-015). Computed on the worker thread after the
    /// structural parse, never on the per-frame path (FR-006). The consumer
    /// republishes this whole set each pass (replace, not merge — FR-006).
    pub type_diagnostics: Vec<Diagnostic>,
    /// Scene-aware validation findings from [`validate_scene`], computed only when
    /// the binding was the [`BoundValidation::Bevy`] variant (E009/FR-013).
    ///
    /// Empty for a serde-mode (or unbound) document, mutually exclusive with
    /// [`type_diagnostics`](Self::type_diagnostics): a document is validated under
    /// exactly one mode's source, so at most one of the two sets is non-empty.
    /// Carried in document-byte coordinates (the publish point maps it via
    /// [`map_scene_diagnostic`](crate::diagnostics_map::map_scene_diagnostic)).
    /// Computed on the worker thread, never on the per-frame path (FR-006).
    pub scene_diagnostics: Vec<SceneDiagnostic>,
    /// Byte length of the text this result was parsed from.
    pub source_len: usize,
    /// Monotonic generation tag identifying which edit produced this result.
    pub generation: u64,
    /// The identity of the document this result was requested for.
    ///
    /// The [`ReparseWorker`] is **shared across all open documents**, but its result
    /// channel is a single FIFO and `generation` is the requesting document's
    /// per-document edit counter — so two different documents at the same generation
    /// (e.g. two freshly opened tabs, each at generation 1) would otherwise be
    /// indistinguishable, and one tab's result could be installed into the other,
    /// leaving the originating tab **blank** (the "switching back and forth" empty
    /// view). Stamping each result with the requesting document's process-unique
    /// [`id`](crate::document::EditorDocument::id) lets the consumer
    /// ([`poll_parse`](crate::document::EditorDocument::poll_parse)) discard results
    /// that are not its own, so cross-document contamination — and the blank it
    /// caused — cannot happen. `0` is the "untagged" sentinel used by the
    /// standalone [`ParseResult::parse`] / [`parse_with_binding`] constructors (which
    /// are not routed through the shared worker).
    pub doc_id: u64,
}

impl std::fmt::Debug for ParseResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `CstDocument` is opaque (no Debug); summarise instead of forwarding.
        f.debug_struct("ParseResult")
            .field("generation", &self.generation)
            .field("source_len", &self.source_len)
            .field("diagnostics", &self.diagnostics.len())
            .field("type_diagnostics", &self.type_diagnostics.len())
            .field("scene_diagnostics", &self.scene_diagnostics.len())
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
    /// the bound validation — all on the calling (worker) thread, never the
    /// per-frame path (E006/FR-006, E009/FR-013).
    ///
    /// The structural diagnostics are always computed. The mode-specific findings
    /// are computed only when a binding is present (`None` ⇒ both sets empty,
    /// FR-015), and the variant selects **which** path runs — exactly one (FR-013):
    ///
    /// * [`BoundValidation::Serde`] → `ron_validate::validate_against` against the
    ///   single bound type, populating [`type_diagnostics`](Self::type_diagnostics)
    ///   (today's behavior, byte-for-byte unchanged);
    /// * [`BoundValidation::Bevy`] → [`validate_scene`] against the bound registry,
    ///   populating [`scene_diagnostics`](Self::scene_diagnostics) — the bound
    ///   registry **replaces** the active source (AD-003/FR-013).
    ///
    /// All diagnostic ranges are defensively clamped to `[0, source_len]`. The CST
    /// is the structural parse's CST in either case (validation is read-only over
    /// it, zero bytes — FR-011/FR-022).
    #[must_use]
    pub fn parse_with_binding(
        text: &str,
        generation: u64,
        bound: Option<&BoundValidation>,
    ) -> Self {
        let source_len = text.len();
        let cst = ron_core::parse(text);
        let diagnostics: Vec<Diagnostic> = cst
            .diagnostics()
            .iter()
            .map(|d| clamp_diagnostic(d, source_len))
            .collect();
        // Mode-specific validation runs only when a binding resolved (FR-015), and
        // exactly one mode's path runs (FR-013). Both paths are read-only over the
        // CST + the bound model (zero bytes, FR-011/FR-022) and each yields a full
        // set to publish in place of the prior set (replace, not merge — FR-006).
        // `ron_validate` (driven by both `validate_against` and, inside
        // `validate_scene`, `validate_subtree_against_type`) skips findings inside
        // `ron-core` parse-error node spans, so a malformed region never produces a
        // cascade of false type errors while the parseable remainder is still
        // validated (FR-019); the structural diagnostics above still cover the
        // malformed span. Overlap dedup against structural is applied at the publish
        // point (`document::merge_type_diagnostics`, FR-017).
        let mut type_diagnostics = Vec::new();
        let mut scene_diagnostics = Vec::new();
        match bound {
            // Serde mode (E006): unchanged — validate the single bound type.
            Some(BoundValidation::Serde(bound)) => {
                type_diagnostics =
                    ron_validate::validate_against(&bound.model, &bound.type_name, &cst)
                        .iter()
                        .map(|d| clamp_diagnostic(d, source_len))
                        .collect();
            }
            // Bevy mode (E009/FR-013): the bound registry REPLACES the active
            // source — scene-aware validation against it, never `validate_against`.
            Some(BoundValidation::Bevy(scene)) => {
                scene_diagnostics = validate_scene(
                    &scene.model,
                    &scene.registry,
                    &cst,
                    scene.expected_version.as_deref(),
                )
                .into_iter()
                .map(|d| clamp_scene_diagnostic(d, source_len))
                .collect();
            }
            // No binding: structural-only (FR-015).
            None => {}
        }
        Self {
            cst,
            diagnostics,
            type_diagnostics,
            scene_diagnostics,
            source_len,
            generation,
            // Untagged by default (these standalone constructors are not routed
            // through the shared worker); the worker stamps the requesting document's
            // id onto each result it ships (see `worker_loop`).
            doc_id: 0,
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

/// Clamp a [`SceneDiagnostic`]'s byte range into `[0, source_len]`, preserving
/// everything else (the Bevy-mode analogue of [`clamp_diagnostic`]). Defensive:
/// the scene validator already keeps ranges in bounds, but the shell must never
/// trust an out-of-range span when later projecting it onto the buffer.
fn clamp_scene_diagnostic(mut diag: SceneDiagnostic, source_len: usize) -> SceneDiagnostic {
    let start = diag.range.start().min(source_len);
    let end = diag.range.end().min(source_len);
    diag.range = TextRange::new(start, end);
    diag
}

/// A reparse job handed to the background worker.
struct ReparseJob {
    /// The requesting document's process-unique identity, stamped onto the result so
    /// the shared worker's output is routed back only to the document that asked for
    /// it (prevents cross-document result contamination / blank views).
    doc_id: u64,
    generation: u64,
    text: String,
    /// The resolved validation binding to run against, when any (E006/FR-006,
    /// E009/FR-013). The `Arc`s inside [`BoundType`] / [`BoundScene`] keep this
    /// cheap to send per keystroke regardless of mode.
    bound: Option<BoundValidation>,
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
    /// `bound` when a binding has resolved (E006/FR-006, E009/FR-013).
    ///
    /// Non-blocking. If the worker thread has gone away (only possible after a
    /// panic in this process's teardown), the job is silently dropped — the
    /// consumer keeps whatever result it last installed, never wedging the UI.
    /// When `bound` is `None`, only the structural parse runs (no type / scene
    /// diagnostics, FR-015); the [`BoundValidation`] variant selects serde-vs-Bevy
    /// validation per document (FR-013).
    ///
    /// `doc_id` is the requesting document's process-unique
    /// [`id`](crate::document::EditorDocument::id); the worker stamps it onto the
    /// shipped [`ParseResult`] so the consumer installs **only** results requested for
    /// that document. Because the worker is shared across all tabs and `generation` is
    /// per-document, two tabs at the same generation would otherwise collide and one
    /// tab could steal the other's result, leaving a blank tab — `doc_id` prevents it.
    pub fn request(
        &self,
        doc_id: u64,
        generation: u64,
        text: String,
        bound: Option<BoundValidation>,
    ) {
        if let Some(tx) = &self.job_tx {
            let _ = tx.send(ReparseJob {
                doc_id,
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
            doc_id,
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
            Ok(mut result) => {
                // Stamp the requesting document's identity so the consumer routes
                // this result back to the right tab only (no cross-tab steal).
                result.doc_id = doc_id;
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

#[cfg(test)]
mod tests {
    //! E009 US2 (T021) — `parse_with_binding` runs exactly one mode's validation
    //! path per the [`BoundValidation`] variant (FR-013), populating the matching
    //! diagnostic set and leaving the other empty.

    use super::*;
    use ron_types::{BevyRegistry, BevySource, TypeSource};

    /// A serde `Entity { id: integer }` interchange model (required `id`).
    fn entity_model() -> serde_json::Value {
        serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$defs": {
                "Entity": {
                    "type": "object",
                    "properties": { "id": { "type": "integer" } },
                    "required": ["id"],
                    "additionalProperties": true
                }
            }
        })
    }

    const REGISTRY: &str = r##"{
        "bevyVersion": "0.16.0",
        "$defs": { "game::Vec3": { "kind": "Struct", "properties": {} } }
    }"##;

    fn bevy_binding() -> BoundValidation {
        let (registry, _diags) = BevyRegistry::from_schema_json(REGISTRY, "test", "<test>");
        let model =
            ron_types::to_json(&BevySource::from_registry(registry.clone()).acquire().model);
        BoundValidation::Bevy(BoundScene {
            model: Arc::new(model),
            registry: Arc::new(registry),
            expected_version: None,
        })
    }

    #[test]
    fn no_binding_runs_structural_only() {
        let result = ParseResult::parse_with_binding(r#"(id: "x")"#, 1, None);
        assert!(result.type_diagnostics.is_empty(), "no serde validation");
        assert!(result.scene_diagnostics.is_empty(), "no scene validation");
    }

    #[test]
    fn serde_variant_populates_type_diagnostics_only() {
        let bound = BoundValidation::Serde(BoundType {
            model: Arc::new(entity_model()),
            type_name: "Entity".to_string(),
        });
        // `id` is a string where an integer is required ⇒ a serde type finding.
        let result = ParseResult::parse_with_binding(r#"(id: "oops")"#, 1, Some(&bound));
        assert!(
            !result.type_diagnostics.is_empty(),
            "serde validation produces a type finding"
        );
        assert!(
            result.scene_diagnostics.is_empty(),
            "the scene set stays empty in serde mode (mutually exclusive)"
        );
    }

    #[test]
    fn bevy_variant_populates_scene_diagnostics_only() {
        let bound = bevy_binding();
        // An unregistered component path ⇒ a scene-level hint (BVY-S0002).
        let scene = r#"(entities: {0: (components: {"game::Unknown": (a: 1)})})"#;
        let result = ParseResult::parse_with_binding(scene, 1, Some(&bound));
        assert!(
            !result.scene_diagnostics.is_empty(),
            "scene validation produces a finding"
        );
        assert!(
            result.type_diagnostics.is_empty(),
            "the type set stays empty in Bevy mode (mutually exclusive)"
        );
    }

    #[test]
    fn diagnostic_ranges_are_clamped_to_source_len() {
        // Defensive clamp covers both the structural and scene sets.
        let bound = bevy_binding();
        let scene = r#"(entities: {0: (components: {"game::Unknown": (a: 1)})})"#;
        let result = ParseResult::parse_with_binding(scene, 1, Some(&bound));
        for d in &result.scene_diagnostics {
            assert!(d.range.end() <= result.source_len, "scene range in bounds");
        }
    }
}
