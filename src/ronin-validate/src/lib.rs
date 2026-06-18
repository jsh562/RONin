//! `ronin-validate` — WASM-clean type-aware validation for RON documents (E006).
//!
//! This crate runs a [`jsonschema`] validator over a CST→JSON projection of a
//! RON document and surfaces the results as `ronin-core` diagnostics with precise
//! source ranges. It is the single, portable validation engine all RONin
//! surfaces (editor today, LSP later) build on (ADR-0006).
//!
//! # Design invariants
//!
//! * **WASM-clean (ADR-0006 / ADR-0002):** the runtime dependencies are
//!   `ronin-core` (rowan-only CST), `jsonschema`, and `serde_json` only.
//!   `jsonschema` is taken with `default-features = false` so the network/TLS
//!   resolver features (`reqwest`/`rustls`) stay off — the validator is fully
//!   offline (FR-023) and builds for `wasm32-unknown-unknown`.
//! * **Read-only (FR-020):** validation never mutates the CST, the structural
//!   diagnostic set, or the bound `TypeModel`.
//! * **No false positives (Principle III):** unresolved/`unknown` types are
//!   modeled as unconstrained; type diagnostics are deduped against overlapping
//!   structural ones (structural precedence).
//!
//! # Public surface
//!
//! The single entry point is [`validate`], built on the [`projection`] (CST→JSON
//! projection + JSON-Pointer→range index) and [`validate`](self::validate)
//! validation modules. Phase 3 (T008–T012) fills the real projection and
//! validator core; this Phase-2 skeleton wires the modules and the public entry
//! so the crate compiles and stays on the wasm32 build gate.

#![forbid(unsafe_code)]

pub mod projection;
pub mod validate;

pub use projection::{CstJsonProjection, PointerRangeIndex, PointerSpans};
pub use validate::{
    dedup_against_structural, validate_against, validate_root, validate_subtree_against_type,
};

/// Validate a RON document against its bound type model and return the type
/// diagnostics (E006/FR-001).
///
/// # Parameters
///
/// * `model` — E004's serialized `TypeModel` interchange (JSON-Schema 2020-12 +
///   `x-ron-*`), as a [`serde_json::Value`]. An empty/`null` model means there
///   is nothing to validate against.
/// * `doc` — the parsed `ronin-core` [`CstDocument`](ronin_core::CstDocument) the
///   diagnostics' precise ranges are taken from. Read-only (FR-020).
/// * `structural` — the document's `ronin-core` structural diagnostic set, used to
///   dedup type findings against overlapping structural ones (structural
///   precedence, FR-017). The structural set is never cleared or mutated by this
///   call (FR-006).
///
/// # Returns
///
/// The set of type [`Diagnostic`](ronin_core::Diagnostic)s (each carrying a
/// `RON-V####` code), as a full set to publish in place of the prior type set
/// (replace, not merge — FR-006).
///
/// # Behavior
///
/// `model` is treated as a complete (self-contained) JSON-Schema document and the
/// document's projected value is validated against its root. An empty or absent
/// model yields zero type diagnostics, leaving the structural set as the only
/// diagnostics (FR-015). To validate against a *named* def from a multi-type
/// model (the binding path), use [`validate_against`].
///
/// Per the dedup contract (FR-017), any type finding whose byte range intersects
/// a structural diagnostic's range is suppressed (structural precedence); the
/// `structural` set itself is never cleared or mutated (FR-006/FR-020). The
/// returned set is therefore the published type set: the full type-validation
/// result minus findings that overlap a structural error.
#[must_use]
pub fn validate(
    model: &serde_json::Value,
    doc: &ronin_core::CstDocument,
    structural: &[ronin_core::Diagnostic],
) -> Vec<ronin_core::Diagnostic> {
    let type_diags = validate::validate_root(model, doc);
    validate::dedup_against_structural(type_diags, structural)
}
