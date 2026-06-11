# Product Requirements Document: RONin

> Date: 2026-06-10 | Status: Draft

## Product Overview

RONin is an intelligent editor for RON (Rusty Object Notation) files. It is built around a reusable Rust "RON intelligence" core engine — parse, validate, format, and transform — with a desktop GUI reference editor as its first surface. The core is designed to later power additional frontends (a language server, a VSCode extension) without rework. RONin serves two audiences from one product through distinct modes: general Rust developers editing serde-based RON config/data, and Bevy game developers editing scenes and asset data. Its value is turning RON from an error-prone, hand-edited text format into a structured, type-aware, navigable editing experience.

## Vision and Why Now

RON is the de-facto human-friendly data format of the Rust ecosystem, yet it is edited almost exclusively in general-purpose text editors with no semantic help. Validation happens at `cargo`/runtime, structure is buried in nested text, and the Bevy community routinely fights scene verbosity. The format has rich semantics that plain editors ignore. The vision is to make RON a first-class, tool-assisted editing experience — where errors surface as you type, structure is navigable and editable as trees and tables, and the editor understands the types behind the data. Now is the moment: RON adoption is broad (notably in Bevy), the only semantic competitor requires manual type annotations in comments, and a single Rust core can be reused across native and WASM surfaces, making a multi-editor strategy viable from day one.

## Problem Statement

Editing RON today is unstructured and unsafe. Authors discover type and structural errors only when Rust code runs, get no autocomplete or schema awareness, navigate deeply nested data as raw text, and — in Bevy — wade through verbose, default-laden scene files. The cost is wasted time, runtime failures from avoidable typos, and friction that discourages RON's structured-data strengths. No existing tool delivers automatic, low-setup type awareness combined with structural/table editing and format interop.

## Background and Evidence

- **RON expresses more than JSON**: named structs, enums/variants with readable syntax, tuples (distinct from lists), char literals, raw strings, comments, trailing commas, unit types, non-string map keys, and extensions (`implicit_some`, `unwrap_newtypes`, `unwrap_variant_newtypes`). It targets Serde's full data model but is explicitly **not self-describing and has no native schema language**.
- **Bevy pain is documented**: `.scn.ron` scenes are extremely verbose, serialize defaults and computed types, and carry redundant type/value keys — a recurring, upstream-acknowledged complaint that validates a dedicated Bevy mode.
- **Tooling is fragmented and shallow**: multiple tree-sitter grammars and several syntax-only VSCode extensions (TextMate grammar, no semantics). The one semantic tool, `ron-lsp`, offers diagnostics/completion/hover/code-actions but **requires manual type annotations in comments**, has no Bevy support, no table editing, and no RON⇄JSON.
- **The reuse strategy is proven**: shipping one Rust core compiled to native (editor, CLI) and WASM (LSP/VSCode via a thin wrapper) is an established pattern (e.g., Prisma), directly supporting RONin's "editor now, plugins later" path.

## Target Users, Stakeholders, and Core Personas

### Target Users

- Rust developers who hand-author serde-based RON (config, assets, fixtures, test data).
- Bevy game developers editing scenes, assets, and ECS data in RON.
- (Secondary) Anyone editing large, uniform RON data who wants spreadsheet-style editing.

### Stakeholders

- Project maintainer/owner (solo/OSS delivery).
- Future frontend consumers (LSP/VSCode users and integrators) who will reuse the core.
- The RON and Bevy open-source communities (corpora, feedback, adoption).

### Core Personas

- **Rina — Rust Config Author** — Maintains serde-deserialized RON config and data. Goals: edit confidently and fast. Pain: type mismatches and typos only caught at runtime; no autocomplete or field awareness; nested structures hard to scan.
- **Bevan — Bevy Scene Builder** — Edits `.scn.ron` scenes and asset RON. Goals: find and change the right field quickly without breaking the scene. Pain: extreme verbosity, default/computed noise, type-registry mismatches, fragile manual edits.
- **Dana — Data Wrangler** — Edits large, homogeneous RON collections (lists of uniform records). Goals: review and bulk-edit rows efficiently. Pain: editing tabular data as deeply indented text is slow and error-prone.

## User Needs / Jobs To Be Done

- When I open a RON file, I want errors flagged immediately so I don't discover them at runtime.
- When I type, I want context-aware completion and formatting so I write correct RON faster.
- When data is deeply nested, I want to navigate and edit it as a tree or form, not raw text.
- When a section is a uniform collection, I want to edit it as a table/grid so review and bulk edits are fast.
- As a Bevy user, I want the editor to understand my scene's types and hide default noise so I can focus on what matters.
- I want to move data between RON and JSON and stay in sync with my Rust types, with any losses made explicit.
- I want value from the first file with no mandatory setup, and more power when I provide type information.

## Product Principles or UX Principles

- **One core, many surfaces**: A single RON intelligence engine is the source of truth; the desktop editor and every future frontend reuse it. No surface-specific re-implementation of RON semantics.
- **Progressive intelligence**: Useful with zero configuration (structural intelligence); automatically type-aware when Rust types or a schema are available. Setup is never a prerequisite to value.
- **Correctness is non-negotiable**: Never silently corrupt a file. Preserve comments, ordering, and formatting through edits; make any interop/semantic loss explicit, never silent.
- **Mode-aware, not mode-locked**: Serde and Bevy modes share the core and differ only where the domain demands it.
- **Structure is editable, not just viewable**: Tree, form, and table representations are first-class editing surfaces, not read-only previews.
- **Fast on real files**: Responsiveness on large, real-world RON/Bevy files is a core experience requirement, not an afterthought.

## Scope Summary

The first release is a desktop GUI RON editor powered by a reusable Rust core, delivering type-aware validation, smart authoring, and structural/table editing, with both a general serde mode and a Bevy mode at launch. Round-trip/interop ships as the first fast-follow. Additional editor frontends (LSP, VSCode) are deliberately deferred to a later phase but the core is architected for that reuse.

### In-Scope Capabilities

- Reusable RON intelligence core engine (parse, validate, format, transform) as the shared foundation.
- Type-aware validation with a schema-optional, progressive model (works with no setup; smarter with type info).
- Smart authoring: context-aware autocomplete, formatting/pretty-print, and snippets.
- Structural and table editing: tree/form navigation plus spreadsheet-style editing of uniform sections, with safe structural edits.
- Bevy mode (co-equal at MVP): type-registry/reflection-aware editing, scene-aware validation, defaults elision, and verbosity reduction.
- Reference desktop GUI editor as the primary user surface.
- Round-trip & interop (first fast-follow): RON⇄JSON and derive-from/sync-with Rust types, with explicit handling of lossy conversions.

### Out-of-Scope Items

- Language Server (LSP) and VSCode/other-editor extensions — deferred to a post-MVP phase (the core is designed to enable them later).
- Editing non-RON formats as primary targets (JSON appears only via interop).
- A plugin marketplace, multi-user/collaborative editing, or cloud/hosted services.
- Becoming a general Rust IDE or replacing `cargo`/the compiler.
- Lossless RON⇄JSON round-tripping (JSON cannot represent several RON semantics; loss is explicit by design).
- Automatic table editing of heterogeneous, enum-variant-heavy, or ragged/deeply-optional data (table view is scoped to uniform sections; other data uses tree/form).

## Product Capability Map

Project-level execution anchors used by `specs/project-plan.md`. Capability clusters, not feature-level user stories.

| Capability ID | Capability | Priority | Outcome |
|---------------|------------|----------|---------|
| CAP-001 | RON Intelligence Core Engine | P1 | A reusable Rust core (parse, validate, format, transform) that is the single source of truth for every RONin surface, native and future WASM. |
| CAP-002 | Type-Aware Validation | P1 | Schema-optional, progressive validation that surfaces structural and type errors in-editor before `cargo`/runtime. |
| CAP-003 | Smart Authoring | P1 | Context-aware autocomplete, formatting/pretty-print, and snippets that make correct RON faster to write. |
| CAP-004 | Structural & Table Editing | P1 | Tree/form navigation plus spreadsheet-style table editing of uniform sections, with non-destructive structural edits. |
| CAP-005 | Bevy Mode | P1 | Bevy type-registry/reflection-aware editing: scene-aware validation, defaults elision, and verbosity reduction for game-dev RON. |
| CAP-006 | Reference Desktop Editor | P1 | A polished desktop GUI that is the primary MVP surface and showcases the core's intelligence. |
| CAP-007 | Round-Trip & Interop | P2 | RON⇄JSON conversion (lossy-by-design with explicit rules) and derive-from/sync-with Rust types, plus schema-safe migrations. |

## Success Metrics / KPIs / Desired Outcomes

Targets are initial hypotheses to validate during piloting; refine after the first cohort.

| Metric | Target | Why It Matters | Measurement Window |
|--------|--------|----------------|--------------------|
| Time-to-first-value | < 5 min from opening a RON file to first useful validation/autocomplete | Onboarding friction predicts adoption for dev tools | First session |
| Activation | ≥ 40% of installs validate/edit a real (non-sample) file | Confirms the tool is used on real work, not just trialed | First 7 days |
| Errors caught before runtime | Majority of structural/type errors surfaced in-editor vs at `cargo`/runtime | Core correctness value vs a plain text editor | Per-session, ongoing |
| Large-file responsiveness | Validation/format/edit remain interactive on large real-world RON/Bevy files | "Fast on real files" is a core principle | Continuous |
| Week-1 retention | 40–60% of activated users return within a week | Signals durable value before broad release | First week |

## Assumptions

- The product is open-source and distributed as a Rust core plus a desktop editor; licensing/distribution specifics are an open question, not a blocker.
- Users have RON files of real value to edit and (often, not always) access to the Rust types behind them.
- Bevy users are on reasonably current Bevy versions whose type registry/reflection can supply type information.
- A single Rust core can serve both the native desktop editor and future WASM-based frontends.
- The two audiences are well served by shared core intelligence with mode-specific behavior layered on top.

## Constraints

- Core engine is Rust-based and must be reusable across native and (future) WASM targets.
- RON has no native schema; type awareness must be derived from external sources (Rust types, Bevy reflection, or a supplied schema).
- RON⇄JSON interop is inherently lossy; the product must preserve RON semantics and surface losses explicitly.
- Solo/OSS delivery capacity — MVP scope must stay disciplined; deferred items are committed but sequenced.

## Dependencies

- The RON format/specification and its reference implementation (the `ron` crate / serde data model).
- Sources of type information: users' Rust types, the Bevy type registry/reflection, or user-supplied schemas (e.g., schema derived from Rust types).
- The desktop GUI runtime/toolkit (selected during system design).
- Representative RON corpora (serde config/data and Bevy scenes) for dogfooding and validation.

## Risks

- **Type-info acquisition is hard**: Automatically learning the types behind a RON file without manual annotation is the central differentiator and the central technical risk; weak coverage undercuts the value proposition.
- **Bevy coupling**: Reflection/type-registry integration is heavy and tied to Bevy versions; co-equal Bevy mode at MVP raises scope and maintenance risk.
- **Scope breadth**: Three P1 capability clusters plus two modes plus a GUI is a large MVP for solo delivery; risk of diluted polish.
- **Interop expectations**: Users may expect lossless RON⇄JSON; mismanaged expectations erode trust.
- **Table-editing edge cases**: Real data is often non-uniform; over-promising table editing leads to disappointment if boundaries aren't clear.
- **Competitive/positioning**: `ron-lsp` exists; RONin must clearly out-deliver on automatic type-awareness, Bevy support, and structural/table editing.

## Open Questions

- How is type information acquired in the schema-optional model (parse Rust source, consume derived schemas, Bevy reflection at runtime) — and what coverage is "good enough" for MVP? (Needs technical input.)
- What desktop GUI runtime best serves rich tree/table editing while easing later WASM/VSCode-webview reuse? (System-design decision.)
- What exactly defines a "uniform section" eligible for table editing, and how is the boundary communicated to users?
- What are the explicit rules and UI for lossy RON⇄JSON conversion (CAP-007)?
- Licensing, naming/availability, and distribution channel for the open-source release.
- How do serde mode and Bevy mode differ in UX, and how is the mode selected/detected?

## Release or Validation Approach

Validate by dogfooding the reference desktop editor against real serde RON and real Bevy scene corpora, then a small private beta cohort drawn from both audiences. Gate broader release on activation and week-1 retention signals plus demonstrated error-reduction and large-file responsiveness. Round-trip/interop (CAP-007) ships as the first fast-follow once the P1 editing experience is validated. Only after the editor is proven do the deferred LSP/VSCode frontends enter scope, reusing the same validated core.

## Domain Glossary / Terminology

- **RON (Rusty Object Notation)**: Human-friendly data serialization format for the Rust/serde ecosystem; richer than JSON, not self-describing, no native schema.
- **serde**: Rust's serialization/deserialization framework; defines the data model RON targets.
- **Bevy**: Rust game engine whose scenes/assets are commonly authored as RON (`.scn.ron`).
- **Reflection / type registry (Bevy)**: Runtime type information Bevy exposes, usable as a schema source.
- **Schema-optional / progressive**: The model where the editor is useful with no setup and becomes type-aware automatically when type info is available.
- **Round-trip / interop**: Converting between RON and JSON (and syncing with Rust types); lossy by design due to RON-only semantics.
- **Uniform section**: A homogeneous RON collection (e.g., a list of same-type records) eligible for table/grid editing.
- **Reference editor**: The first-party desktop GUI that demonstrates and exercises the core engine.

## Handoff Guidance

Context that downstream architecture design or governance work must preserve.

- **Product intent to preserve**: One reusable Rust core powers all surfaces; the desktop editor is the MVP showcase, not the end state. Progressive (schema-optional) type-awareness with zero-setup value is the core differentiator.
- **Scope boundaries to respect**: LSP/VSCode frontends are deferred but must remain enabled by the core's design. Table editing is scoped to uniform sections. RON⇄JSON is lossy-by-design with explicit loss handling. Bevy mode is co-equal at MVP.
- **Critical constraints**: Rust core reusable native + future WASM; no native RON schema (type info is external); never silently corrupt files or hide interop loss.
- **Open decisions needing technical input**: Type-info acquisition strategy and MVP coverage bar; GUI runtime choice; "uniform section" definition; lossy-interop rules.

## Project Context Baseline Updates

- [Reserved for reusable project-level product context promoted from downstream runs.]
