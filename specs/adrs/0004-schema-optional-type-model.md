---
adr_id: ADR-0004
status: accepted
date: 2026-06-10
tags: [validation, types, modes]
supersedes: []
superseded_by: ""
related_artifacts: [specs/prd.md, specs/sad.md]
---

# ADR-0004: Schema-Optional Progressive Type-Awareness via a Normalized Type Model

## Status

Accepted.

## Context

RON has no native schema language. The PRD requires the editor to be useful with zero setup (structural intelligence) and to become type-aware automatically when type information is available — the key differentiator versus ron-lsp, which forces manual type annotations in comments. The product must serve serde mode and Bevy mode co-equally. The user selected four type-information sources to support, so a decision is needed on how those sources feed validation without compromising the zero-setup baseline or the co-equal treatment of both modes.

## Decision Drivers

- Zero-setup value with progressive enhancement when types are known.
- Uniform validation behavior across serde and Bevy modes.
- Faithful Rust/serde/Bevy type fidelity.
- Performance on the hot validation path.

## Considered Options

### Option A: Normalize all four sources into one internal JSON-Schema-shaped type model

Normalize all four sources — static Rust source analysis (syn), schemars-derived JSON Schema, Bevy type registry / reflection (BRP or registry export), and user-supplied schema — into one internal JSON-Schema-shaped type model that a single validator (compiled `jsonschema` validators) consumes; fall back to structural-only intelligence when no type info exists.

- **Pros**: uniform validation; co-equal serde and Bevy modes; progressive UX; reuses standard validators.
- **Cons**: an adapter per source; RON-specific semantics (tuples, char, non-string map keys, extensions) do not map cleanly to plain JSON Schema and need a RON-aware extension layer.

### Option B: Single source (e.g., schemars only)

Support a single type-information source rather than normalizing across sources.

- **Pros**: simplest adapter surface; one well-understood pipeline.
- **Cons**: forces a setup step; no Bevy support; weak progressive story.

### Option C: Manual annotations (ron-lsp style)

Require users to declare types via manual annotations in comments.

- **Pros**: no analysis or acquisition machinery required.
- **Cons**: worst UX and no differentiation.

## Decision Outcome

Chosen option: **Option A: a normalized internal type model fed by all four sources** — with structural-only fallback when no types are available. Normalizing all four sources into one JSON-Schema-shaped model behind a single validator delivers the progressive, zero-setup experience the PRD demands while keeping serde and Bevy modes co-equal under uniform validation, which neither the single-source nor manual-annotation options can provide.

## Consequences

### Positive

- Progressive UX that beats the competitor.
- Co-equal serde/Bevy modes.
- A single uniform validator.

### Negative

- Must reconcile serde attributes and RON-only constructs beyond plain JSON Schema.
- Type-acquisition lives in the native-gated `ron-types` crate, so the WASM core consumes a serialized schema rather than running acquisition itself.

### Neutral

- The internal schema model is an evolving internal contract.

## Links

- PRD capability CAP-002 (Type-Aware Validation).
- PRD capability CAP-005 (Bevy Mode).
- PRD capability CAP-007 (Round-Trip & Interop).
- PRD open question "type-info acquisition strategy".
- Related ADR-0002 (type-acquisition crate isolation).
- External: schemars — https://github.com/GREsau/schemars
- External: Bevy registry JSON Schema — https://github.com/bevyengine/bevy/pull/16882
