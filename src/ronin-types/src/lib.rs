//! `ronin-types` — RONin's normalized RON type model (native-gated, E004).
//!
//! This crate acquires type information from external sources (Rust source via
//! `syn`, `schemars`-derived JSON Schema, user-supplied JSON Schema) and
//! normalizes it into one internal [`TypeModel`]. The model is shaped on
//! **JSON Schema 2020-12** plus a closed `x-ron-*` extension layer (see
//! [`extension`]) for RON constructs JSON Schema cannot express (tuples, `char`,
//! unit, bytes, non-string-key maps, `Option`, and the RON extension flags).
//!
//! # Architectural invariants
//!
//! - **Native-gated (ADR-0002/0004).** `ronin-types` MUST NOT be a dependency of
//!   the WASM-clean `ronin-core`. The only artifact that crosses to `wasm32` is
//!   the serialized JSON-Schema-2020-12-shaped interchange produced by
//!   [`serialize::to_json`] / consumed by [`serialize::from_json`]. That JSON
//!   uses only plain-serde types so a WASM-clean consumer can deserialize it.
//! - **Progressive Intelligence (ADR-0004).** An unresolvable type is a
//!   first-class [`model::TypeNode`] of kind `unknown`, never an error and never
//!   a dangling reference. With no source supplied the model is empty /
//!   structural-only, not an error state.
//! - **Recursion-safe.** Named types are referenced through a `$defs`-style
//!   registry via [`model::TypeRef`]; recursive types are never inline-expanded.
//!
//! # Surface
//!
//! The crate lands the model, the RON extension layer, diagnostics, the
//! `TypeSource` adapter contract, and the JSON-Schema-2020-12 serialization
//! round-trip (OBJ1); the concrete source adapters — [`SynSource`] (Rust source
//! via `syn`), [`SchemarsSource`] (schemars-derived schemas), and
//! [`JsonSchemaSource`] (user-supplied JSON Schema) (OBJ3); and the multi-source
//! [`normalize`] step that ranks sources by precedence and merges their partial
//! models into one [`TypeModel`] (OBJ4).
//!
//! # Deferred seams
//!
//! `ronin-types` deliberately stops at *acquiring, normalizing, and serializing*
//! the type model. The following capabilities are intentionally out of scope for
//! E004 and land in later epics; the model and its frozen interchange are the
//! contracts they build on:
//!
//! - **Validation engine → E006.** Compiling the serialized JSON-Schema-2020-12
//!   interchange (see [`serialize`]) into a validator and *running* it (via the
//!   `jsonschema` crate) against RON documents lives in the WASM-clean
//!   `ronin-core`. `ronin-types` only produces the schema; it never executes
//!   validation. This is also where on-device `wasm32` deserialization of the
//!   interchange is exercised end-to-end (see `tests/wasm32_deser.rs` for the
//!   native-side proof that the wire form is pure, WASM-consumable JSON).
//! - **Bevy type-registry [`TypeSource`] → E009 (landed).** [`BevySource`] reads a
//!   Bevy registry-schema-format JSON export (the BRP `bevy/registry/schema`
//!   shape) **as data** — no `bevy` crate — and maps each type into the
//!   [`TypeModel`]. It plugs into the existing [`source`] adapter contract and
//!   [`normalize`] precedence ranking ([`source::SourcePrecedence::Bevy`]) with no
//!   model changes. Live BRP acquisition (FR-002) is deferred; because BRP returns
//!   the same registry-schema format, it slots in later as **another constructor**
//!   on [`BevySource`] (producing the same [`BevyRegistry`]) with no core change.
//! - **Derive-from-types / RON⇄JSON interop → E010.** Generating RON skeletons
//!   from the model and the bidirectional RON⇄JSON bridge (via the `ron` crate,
//!   used for interop only — never as the editing model) build on the normalized
//!   [`TypeModel`] and the frozen interchange rather than re-deriving types.

#![forbid(unsafe_code)]

pub mod diagnostics;
pub mod extension;
pub mod model;
pub mod normalize;
pub mod serialize;
pub mod source;

pub use diagnostics::AcquisitionDiagnostic;
pub use extension::RonTypeExtension;
pub use model::{TypeModel, TypeNode, TypeRef};
pub use normalize::normalize;
pub use serialize::{from_json, from_json_str, to_json, to_json_string, InterchangeError};
pub use source::{
    BevyRegistry, BevySource, JsonSchemaSource, SchemarsSource, SynSource, TypeSource,
};
