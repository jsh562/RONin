//! `ronin-core` — a lossless, error-tolerant concrete-syntax-tree (CST) engine for
//! RON (Rusty Object Notation).
//!
//! `ronin-core` parses RON source into a CST that preserves **every byte** of the
//! input (comments, whitespace, trailing commas, struct/variant names, raw
//! strings, extension attributes) and re-prints an unmodified tree
//! byte-for-byte. It is the single, portable engine all RONin surfaces build on
//! (project-instructions §II, "One Core, Many Surfaces").
//!
//! # Design invariants
//!
//! * **Lossless round-trip (INV-2):** concatenating every token's verbatim text
//!   reproduces the source exactly. See [`parse`] + [`print`].
//! * **WASM-clean (INV-9):** the only runtime dependency is `rowan` (pure Rust);
//!   no filesystem / UI / async / native dependencies.
//! * **Library-opaque surface (INV-7):** no `rowan` type appears in the public
//!   API; the CST is exposed through `ronin-core`'s own [`SyntaxNode`] /
//!   [`SyntaxToken`] / [`SyntaxKind`] newtypes so the backing library stays
//!   swappable.
//! * **Never panics on input (TR-001):** non-UTF-8 input is rejected at the
//!   boundary with a clean [`LexError`]; malformed UTF-8 RON produces a tree
//!   that still covers all input.
//!
//! * **Error-tolerant (TR-005, INV-3):** malformed / incomplete input never
//!   panics and never drops bytes — unexpected tokens land in [`SyntaxKind::Error`]
//!   nodes and a structured [`Diagnostic`] (stable [`DiagnosticCode`] +
//!   [`Severity`]) is emitted per recovery point. A configurable nesting-depth
//!   guard ([`ParseOptions`], default [`DEFAULT_MAX_DEPTH`]) prevents stack
//!   overflow on deeply nested input (INV-5).
//!
//! # Public API surface (0.x shape-stable, TR-009)
//!
//! `ronin-core`'s public surface is **0.x shape-stable**: the *capability areas* —
//! parse, navigate the CST, read diagnostics, print, and edit — are committed,
//! while concrete signatures and types may still change in breaking ways within
//! `0.x` until downstream epics validate the shape. The surface deliberately
//! exposes **no `rowan` type and no I/O type** (INV-7 / TR-009): the CST is
//! reachable only through `ronin-core`'s own opaque [`SyntaxNode`] / [`SyntaxToken`]
//! / [`SyntaxElement`] / [`SyntaxKind`] / [`TextRange`] newtypes and the typed
//! accessors in [`ast`], so the backing CST library stays swappable. The five
//! capability areas:
//!
//! * **Parse** — [`parse`], [`parse_with_options`], [`parse_bytes`] →
//!   [`CstDocument`].
//! * **Navigate** — untyped via [`CstDocument::root`] +
//!   [`SyntaxNode`] navigation; typed via [`ast::Document`] and the per-construct
//!   accessors ([`ast::Struct`], [`ast::Map`], [`ast::List`], …).
//! * **Diagnostics** — [`CstDocument::diagnostics`] → `&[`[`Diagnostic`]`]`.
//! * **Print** — [`print`] / [`print_node`] (byte-for-byte round-trip).
//! * **Edit** — [`apply_edit`] with [`EditOperation`] / [`EditTarget`] /
//!   [`EditKind`] / [`TriviaPolicy`] (non-destructive; INV-8).
//! * **Structural transform** — [`apply_structural`] with [`StructuralOp`] /
//!   [`TransformOutcome`] / [`BlockedReason`] (E008 / ADR-0007): pure CST→CST
//!   named structural ops (insert / remove / reorder / set-value / rename a
//!   field-or-element, enum-variant swap, add-field-across-rows) composed over
//!   [`apply_edit`] — byte-for-byte lossless on untouched regions (FR-013),
//!   WASM-clean and reusable by a future LSP/web surface.
//! * **Undo/redo** — [`UndoStack`] / [`UndoEntry`] (E007): a bounded,
//!   WASM-clean CST + text + cursor history with exact-prior-byte restore;
//!   reusable across surfaces and adds no filesystem/native dependency (TR-014).
//!   This is the public undo surface the downstream editing epics import — E005
//!   (smart authoring) and E008 (structural / table editing) edit against it via
//!   this re-export (TR-013); see the [`undo`] module docs for the reuse contract.
//!
//! # Status
//!
//! This delivers the full E001 engine: OBJ1 (lossless parse + round-trip),
//! OBJ2 (error-tolerant parsing + diagnostics), OBJ3 (WASM-clean 0.x
//! shape-stable public API + wasm32 build gate), and OBJ4 (typed navigation +
//! non-destructive edit primitives).
//!
//! # WASM-clean build gate (TR-007 / INV-9)
//!
//! `ronin-core`'s only runtime dependency is `rowan` (pure Rust); the crate carries
//! no filesystem / UI / async-runtime / native dependency, so it builds for
//! `wasm32-unknown-unknown`:
//!
//! ```text
//! rustup target add wasm32-unknown-unknown
//! cargo build -p ronin-core --target wasm32-unknown-unknown   # MUST succeed
//! ```
//!
//! This build is the proof of WASM-cleanliness; CI wires it as a mandatory gate
//! (owned by E002).
//!
//! # Example
//!
//! ```
//! let src = "Foo(x: 1, y: 2.0) // keep me\n";
//! let doc = ronin_core::parse(src);
//! assert_eq!(ronin_core::print(&doc), src); // byte-for-byte round-trip
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod completion;
pub mod diagnostics;
pub mod edit;
pub mod formatter;
pub mod lexer;
pub mod parser;
pub mod printer;
pub mod syntax;
pub mod transform;
pub mod undo;

pub use completion::{
    completion_context, completions, CompletionContext, CompletionItem, CompletionKind,
    PositionKind,
};
pub use diagnostics::{Diagnostic, DiagnosticCode, Severity};
pub use edit::{apply_edit, EditError, EditKind, EditOperation, EditTarget, TriviaPolicy};
pub use formatter::{format, format_node, BlankLinePolicy, FormatConfig, FormatResult};
pub use lexer::{validate_utf8, LexError};
pub use parser::{
    parse, parse_bytes, parse_with_options, CstDocument, ParseOptions, DEFAULT_MAX_DEPTH,
};
pub use printer::{print, print_node};
pub use syntax::{SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken, TextRange};
pub use transform::{apply_structural, BlockedReason, ParentRef, StructuralOp, TransformOutcome};
pub use undo::{UndoCap, UndoEntry, UndoStack};

/// Typed accessors over the CST (TR-010): navigate RON constructs by name
/// (`Struct::fields()`, `Map::entries()`, …) through `ronin-core` types only.
pub use syntax::ast;
