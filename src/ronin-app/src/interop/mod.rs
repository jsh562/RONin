//! RON⇄JSON round-trip & interop (E010) — bidirectional conversion, derive-from-type,
//! and the lossy-construct map, at the **native interop boundary** (ADR-0008).
//!
//! The primary converter is **CST-based**: RON→JSON reuses `ronin-validate`'s
//! `CstJsonProjection` (value mapping + the Pointer→`TextRange` index for
//! source-located losses) plus a CST trivia walk for JSONC comments; JSON→RON is a
//! `TypeModel`-guided RON-text builder reparsed via `ronin_core::parse`. The pinned
//! serde `ron` crate is the boundary **grammar verifier / round-trip cross-check**,
//! never the primary path — so `ronin-core`/`ronin-validate` gain no `ron`/JSON-conversion
//! dependency and stay wasm32-clean (FR-012).
//!
//! Modules:
//! * [`loss`] — the lossy-construct map (kinds + source range + recovery flag) that
//!   feeds BOTH the pre-conversion loss report and the inline diagnostics (FR-004/007).
//! * [`ron_to_json`] — RON→JSON value mapping + emit conventions (FR-001/015).
//! * [`json_to_ron`] — JSON→RON schema-aware / best-effort reconstruction (FR-002/009/015).
//! * [`comments`] — JSONC + sidecar comment carrier (FR-008).
//! * [`scaffold`] — derive an initial RON document from a `TypeModel` (FR-010).
//!
//! ## Two-tier round-trip-safe subset (FR-011)
//!
//! This is the **authoritative reference** for which RON constructs survive a
//! `RON → JSON → RON` round trip without semantic change, and under what condition.
//! It is split into two tiers by whether round-trip safety needs a bound type. A
//! construct's tier is decided here; the per-construct emit/reconstruct mechanics
//! that *realize* the safety live in [`ron_to_json`] / [`json_to_ron`] and the
//! FR-015 emit conventions ([`emit`]).
//!
//! ### Tier 1 — type-agnostic **base tier** (ALWAYS round-trip-safe)
//!
//! These map onto standard-JSON shapes that already carry their full meaning, so
//! they round-trip with **no `TypeModel` bound** (the unbound, best-effort path is
//! sufficient). The base tier is, by construction, **loss-free** — a document built
//! only from base-tier constructs produces an empty [`LossReport`]:
//!
//! * **Scalars** — booleans, integers, floats, and (UTF-8) strings map 1:1 to JSON
//!   `true`/`false`, numbers, and strings.
//! * **Sequences as lists** — a RON list `[a, b, c]` ⇄ a JSON array. (A RON *tuple*
//!   `(a, b)` also emits as an array, but that is the **expanded** tier — see below —
//!   because the tuple-vs-list distinction is lost to an external consumer.)
//! * **String-keyed maps / structs** — a RON map with string keys, and a RON struct
//!   `(field: …)`, both map onto a JSON object. The struct *name* (when present) is
//!   the expanded tier ([`LossKind::StructName`]); the field/key→value structure
//!   itself is base-tier. Map entries are compared order-insensitively by the oracle.
//! * **Comments via JSONC** — comments are preserved across the round trip through the
//!   JSONC inline carrier (default) or the sidecar comment map (strict mode), anchored
//!   to the nearest value (FR-008, [`comments`]). Comments are base-tier ONLY when a
//!   carrier is in effect; pure-standard-JSON output (no JSONC, no sidecar) drops them
//!   and is reported as [`LossKind::DroppedComment`].
//!
//! ### Tier 2 — expanded **type-bound tier** (round-trip-safe ONLY when a `TypeModel` is bound)
//!
//! These are RON-native shapes with **no faithful standard-JSON form**: projecting
//! them to JSON is *lossy to an external consumer*, so each is ALWAYS reported in the
//! [`LossReport`] (FR-004/007, HINT-004) even though RONin can recover it. Recovery
//! on `JSON → RON` requires the **bound `TypeModel`** plus the FR-015 emit
//! conventions; **without** a binding the inverse keeps the best-effort JSON shape
//! and notes residual ambiguity (FR-009) — i.e. it is NOT guaranteed round-trip-safe.
//! Each member, its loss kind, and the convention that makes it recoverable:
//!
//! * **Tuples** ([`LossKind::TupleVsList`]) — emitted as a JSON array; recovered as a
//!   tuple by the bound type's tuple arity (vs. a homogeneous list).
//! * **Named enum variants** ([`LossKind::EnumTagging`]) — emitted using the type's
//!   recorded serde tagging when bound (external / internal / adjacent / untagged, per
//!   E004 TR-005) and **external tagging** `{"Variant": payload}` as the deterministic
//!   default when unbound; the bound type recovers the variant identity (FR-015).
//! * **`char`** ([`LossKind::Char`]) — emitted as a one-character JSON string;
//!   recovered as a `char` (vs. a `String`) by the bound field type.
//! * **`Option`** (`Some`/`None`) — `None` emits as JSON `null`, `Some(x)` as `x`;
//!   the bound `Option<T>` field recovers the `Some`/`None` wrapping (vs. a bare value
//!   or a literal `null`).
//! * **Non-string map keys** ([`LossKind::NonStringKey`]) — emitted as their
//!   **canonical RON-literal string** (e.g. `1` → `"1"`, `(1, 2)` → `"(1, 2)"`) with
//!   the key's source type recorded, so a bound import re-parses the literal back to
//!   the typed key (an unbound import keeps string keys and notes the ambiguity)
//!   (FR-015, HINT-002).
//!
//! Constructs outside both tiers — e.g. unit-vs-null ([`LossKind::UnitVsNull`]),
//! raw-string syntax ([`LossKind::RawString`]), trailing commas
//! ([`LossKind::TrailingComma`]), and tuple-/struct-*name* identity
//! ([`LossKind::StructName`]) — are reported losses that the round-trip oracle treats
//! as either non-semantic (trailing commas, raw-vs-plain string *form*) or as
//! genuinely lossy to an external consumer; they are documented in [`loss`] and are
//! NOT claimed round-trip-safe here.
//!
//! ### FR-011 round-trip oracle (the measurable equality used by the tests)
//!
//! "Value-equivalent" / "value-stable" / "no semantic change" is defined precisely so
//! a round trip can be *checked*, not eyeballed. Given an original RON text `O` and the
//! round-tripped RON text `O' = json_to_ron(ron_to_json(O))`, the oracle holds iff:
//!
//! 1. **Semantic value-tree equality** — the re-parsed value trees of `O` and `O'` are
//!    equal: same typed values (a `char` is distinct from a one-char string; an enum
//!    variant identity is distinct), same structure, same tuple **arity**, same map
//!    entries compared as key/value pairs **order-insensitively for maps**, same
//!    enum-variant identity. (Operationally the tests use the serde `ron` crate's
//!    `ron::Value` as the value tree — the boundary **grammar verifier / cross-check**,
//!    never the primary converter, ADR-0008 / FR-012.)
//! 2. **Comment fidelity** — the same comment **content**, compared by **text and
//!    attachment anchor** (which value a comment hangs on), not by byte position.
//!
//! The oracle **ignores pure-formatting differences** as non-semantic: whitespace and
//! indentation, line-vs-block comment style, trailing commas, and the canonical
//! printer's spacing/layout. A difference in any **typed value, structural shape,
//! enum-variant identity, tuple arity, map entry, or comment content** is a
//! **semantic change** — a round-trip failure. The oracle is exercised in
//! `tests/interop_roundtrip.rs` (base tier unbound; expanded tier bound).

pub mod comments;
pub mod emit;
pub mod json_to_ron;
pub mod jsonc;
pub mod loss;
pub mod pointer;
pub mod ron_to_json;
pub mod scaffold;

// The lossy-construct map is the shared driver of BOTH the loss dialog and the
// inline diagnostics (FR-004/007), so its core types are re-exported at the
// module root for ergonomic access from the app/diagnostics surfaces.
pub use loss::{build_loss_report, LossKind, LossRecovery, LossReport, LossyConstruct};

// The comment carrier (FR-008) and the RON→JSON converter (FR-001/015) are the
// US1 surface the app/file-IO layers and US2 read-back consume.
pub use comments::{Comment, CommentCarrier, CommentKind, CommentMode};
pub use ron_to_json::{ron_to_json, RonToJson, RonToJsonBinding};

// The JSON→RON reconstruction (FR-002/009/015) — the US2 inverse the import /
// in-place-convert commands and the round-trip tests consume.
pub use json_to_ron::{json_to_ron, JsonToRon, JsonToRonBinding, MAX_JSON_DEPTH};

// The JSONC reader (FR-008) — parses JSON-with-comments + anchors comments for the
// JSON→RON comment read-back that file IO / import consumes.
pub use jsonc::{parse_jsonc, JsoncDocument, JsoncError};

// The deterministic JSON / JSONC renderer (FR-001/008) the in-place convert
// (T014), the export (T015), and the dialog preview (T016) all share.
pub use emit::{render_json, JsoncStyle};

// The derive-from-type scaffold generator (FR-010) — the US3 surface the derive
// command (T027) opens in a new tab + whose fill-in diagnostics it publishes.
pub use scaffold::{derive_scaffold, DeriveScaffold, MAX_DERIVE_DEPTH, UNKNOWN_SENTINEL};
