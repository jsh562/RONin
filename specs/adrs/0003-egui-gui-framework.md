---
adr_id: ADR-0003
status: accepted
date: 2026-06-10
tags: [ui, frontend, portability]
supersedes: []
superseded_by: ""
related_artifacts: [specs/prd.md, specs/sad.md]
---

# ADR-0003: egui/eframe as the GUI Framework

## Status

Accepted.

## Context

The reference editor is a desktop GUI (PRD CAP-006). It must reuse the Rust core, ideally reuse the UI in a future browser/VSCode-webview frontend, and provide strong table/grid editing for the spreadsheet view of uniform sections (CAP-004). We must choose the GUI framework that best supports single-language reuse, native plus WASM targets, and virtualized large-table editing.

## Decision Drivers

- Single-language (Rust) reuse of core and UI.
- Native + WASM from one UI codebase.
- Virtualized large-table support for big RON data.
- Speed of building tool/inspector-style UI.

## Considered Options

### Option A: egui / eframe

- **Pros**: Pure Rust; eframe compiles to native + WASM; egui_extras TableBuilder offers virtualized rows (100k+ rows interactive); fast for data/inspector UIs; the same core + UI can target a webview later.
- **Cons**: Immediate-mode styling is utilitarian; webview embedding is a canvas (weaker accessibility/DOM than HTML).

### Option B: Tauri (Rust + web UI)

- **Pros**: Native shell; web UI ports naturally to a VSCode webview; rich DOM/accessibility.
- **Cons**: Introduces a second language (TS/JS), a heavier toolchain, and a split stack.

### Option C: Iced

- **Pros**: Pure Rust, Elm-style architecture.
- **Cons**: Weaker ecosystem for complex grids/tables; less mature WASM story.

### Option D: Slint

- **Pros**: Polished declarative widgets.
- **Cons**: Custom DSL and licensing considerations; arbitrary-data grid editing needs bespoke work.

## Decision Outcome

Chosen option: **A: egui/eframe** (user-selected) — it maximizes single-language native + WASM reuse and provides virtualized tables out of the box.

## Consequences

### Positive

- Single-language native + WASM reuse.
- Virtualized tables.
- Fast iteration on tool UI.

### Negative

- Utilitarian theming.
- Canvas-based webview imposes accessibility limits versus a DOM UI.

### Neutral

- Heavy work (full reparse/validation) must run off the per-frame `update()` path to keep the immediate-mode loop responsive.

## Links

- PRD capability CAP-004 (Structural & Table Editing)
- PRD capability CAP-006 (Reference Desktop Editor)
- Related ADR-0002 (app is a workspace adapter crate)
- External: egui_extras virtualized tables (https://docs.rs/egui_extras/latest/egui_extras/struct.TableBody.html)
