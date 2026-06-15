//! Bevy mode (E009) — scene-aware validation and defaults elision for Bevy
//! `DynamicScene` `.scn.ron` files.
//!
//! RONin consumes an **exported** Bevy type registry as data (no `bevy` crate
//! dependency anywhere; FR-003/FR-017). The pieces:
//!
//! * [`scene`] — interprets a `.scn.ron` CST into resources + entities +
//!   per-entity components keyed by fully-qualified type path (FR-004).
//! * [`validate`] — drives the generic, Bevy-agnostic `ron-validate`
//!   subtree-vs-type entry per component/resource, with progressive degradation
//!   and the staleness advisory (FR-005/FR-006/FR-008).
//! * [`mode`] — per-document mode selection (`{serde, Bevy}`) and the
//!   per-pattern registry binding config (FR-009/FR-010/FR-012/FR-013).
//! * [`elision`] — lossless defaults elision / expand-to-explicit via the E008
//!   structural transforms (FR-014/FR-015/FR-016).
//!
//! The Bevy registry **ingestion** itself lives native-side in `ron-types`
//! (`BevySource`); this module is the application-layer scene interpretation and
//! orchestration that the WASM-clean core never sees.

pub mod elision;
pub mod mode;
pub mod scene;
pub mod validate;

pub use elision::{
    expand_to_explicit, reduce_verbosity, render_json_as_ron, ron_value_equals_json,
    ElisionOutcome, Scope, SkipReason, SkippedField,
};
pub use scene::{SceneEntity, SceneModel, SceneValueKind, SceneValueRef};
pub use validate::{
    staleness_advisory, validate_scene, SceneDiagnostic, SceneDiagnosticCode, SceneSeverity,
};
