---
adr_id: ADR-0001
status: accepted
date: 2026-06-10
tags: [parsing, core, data-integrity]
supersedes: []
superseded_by: ""
related_artifacts: [specs/prd.md, specs/sad.md]
---

# ADR-0001: Custom Lossless CST as the Editing Model

## Status

Accepted.

## Context

RONin must never corrupt or reflow files and must preserve comments, original formatting, key/field ordering, and struct names through every edit (PRD principle "Correctness is non-negotiable"). The editor also needs error-tolerant parsing so that invalid or in-progress files still yield a usable tree for validation and structural editing. We must choose the authoritative in-memory representation for editing.

## Decision Drivers

- Non-destructive editing: preserve comments, whitespace/formatting, ordering, and struct names.
- Error-resilient parsing for in-progress/invalid files.
- Incremental reparse for large-file responsiveness.
- WASM-clean, pure-Rust (core must compile to WASM for future frontends).
- Full control over formatting and structural transforms.

## Considered Options

### Option A: Custom lossless CST via rowan/cstree (red-green tree)

- **Pros**: Lossless (retains all tokens/trivia/comments/ordering); error-tolerant; rust-analyzer-proven; pure Rust and WASM-clean; full mutation+format control; supports incremental reparse.
- **Cons**: Must author the RON grammar/parser; more upfront work than reusing serde.

### Option B: serde-based `ron` crate as the editing model

- **Pros**: Ready-made and maintained.
- **Cons**: NOT lossless — discards comments, whitespace/formatting, and struct names (`ron::Value`); maintainers confirm whitespace-preserving parsing must not be serde-based (ron issue #216); violates "never corrupt."

### Option C: tree-sitter-ron grammar

- **Pros**: Incremental parsing with error recovery; grammars already exist.
- **Cons**: C runtime adds WASM/build friction; dual data model; weak mutation/format-rewrite control — better suited to highlighting than to an authoritative edit-and-rewrite tree.

## Decision Outcome

Chosen option: **A: Custom lossless CST (rowan/cstree)** — it is the only option that guarantees non-destructive edits while remaining pure-Rust, WASM-clean, error-tolerant, and incrementally reparsable. rowan is the primary library (cstree is an acceptable alternative). The serde-based `ron` crate is retained ONLY as an optional interop/JSON conversion path (PRD CAP-007), never as the editing model.

## Consequences

### Positive

- Guarantees non-destructive edits; enables incremental reparse, deterministic formatting, and safe structural transforms.
- Pure-Rust core reusable in native + WASM frontends.

### Negative

- Must build and maintain a RON parser/grammar.
- Two RON readers exist (the CST for editing and serde `ron` for interop) and must be kept semantically consistent.

### Neutral

- Aligns RONin's core with rust-analyzer architecture conventions.

## Links

- PRD capability CAP-001 (RON Intelligence Core Engine)
- PRD capability CAP-007 (Round-Trip & Interop)
- PRD principle "Correctness is non-negotiable"
- Related ADR-0002 (core hosts the CST)
- External: ron issue #216 (https://github.com/ron-rs/ron/issues/216)
- External: rowan (https://github.com/rust-analyzer/rowan)
