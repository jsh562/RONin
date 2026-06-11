---
adr_id: ADR-0002
status: accepted
date: 2026-06-10
tags: [workspace, modularity, portability]
supersedes: []
superseded_by: ""
related_artifacts: [specs/prd.md, specs/sad.md]
---

# ADR-0002: Hexagonal Cargo Workspace with a WASM-Clean Core

## Status

Accepted.

## Context

RONin's strategy is one reusable RON intelligence core that powers the egui desktop editor now and LSP/VSCode (WASM) frontends later. We must choose module boundaries that keep the core portable and reusable across all surfaces while isolating heavy or platform-specific dependencies.

## Decision Drivers

- Core reuse across native, WASM, and a future LSP.
- Portability: no filesystem, UI, async-runtime, or platform dependencies in the core.
- A single, explicit public API contract every frontend consumes.
- Keep the WASM build lean by isolating heavy/native type-acquisition dependencies.

## Considered Options

### Option A: Cargo workspace using hexagonal / ports-and-adapters

Crates: `ron-core` (lossless CST, validation, formatting, transforms, internal type model — no fs/UI/tokio, WASM-clean) + `ron-types` (syn/schemars/Bevy type acquisition, native-gated) + `ronin-app` (egui GUI) + a reserved `ron-lsp` slot. The core exposes one public API contract; native/WASM/LSP are adapters.

- **Pros**: Portable core; lean WASM; parallel frontends; proven pattern (Crux, Prisma).
- **Cons**: More crates and boundary discipline upfront.

### Option B: Single monolithic crate

- **Pros**: Simplest initially.
- **Cons**: UI/IO/native type-acquisition deps leak into the core, blocking WASM reuse and forcing later rework.

### Option C: Split by layer but allow std/UI in the core

- **Pros**: Layered separation without the full crate count of Option A.
- **Cons**: Not WASM-clean; defeats the reuse goal.

## Decision Outcome

Chosen option: **Option A: hexagonal Cargo workspace with a WASM-clean `ron-core`** — it is the only option that keeps the core portable and reusable across native, WASM, and a future LSP while isolating heavy/native type-acquisition dependencies behind adapters, satisfying the "one core, many surfaces" strategy. All project source resides under `/src` (workspace crates rooted accordingly).

## Consequences

### Positive

- Portable, reusable core.
- Lean WASM build.
- Future frontends slot in without touching the core.

### Negative

- Requires boundary discipline.
- More crates to manage.

### Neutral

- Because type-acquisition is isolated (and may be native-only), feeding the WASM core may require a serialization bridge (acquire schema natively, hand serialized schema to the core).

## Links

- PRD capability CAP-001 (RON Intelligence Core Engine).
- PRD capability CAP-006 (Reference Desktop Editor).
- PRD handoff "one core, many surfaces".
- Related ADR-0001 (core hosts the CST).
- Related ADR-0003 (egui app crate).
- Related ADR-0004 (type-acquisition crate).
- External: Crux shared-core pattern — https://redbadger.github.io/crux/getting_started/core.html
- External: Cargo workspaces — https://doc.rust-lang.org/book/ch14-03-cargo-workspaces.html
