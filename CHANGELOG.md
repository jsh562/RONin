# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

This is the curated, human-facing release log for the RONin workspace. Going
forward, `release-plz` also maintains a per-crate `CHANGELOG.md` generated from
Conventional Commits.

## [0.1.0] - 2026-06-19

Initial public release of **RONin** — a local-first, lossless desktop editor for
RON (Rusty Object Notation).

### Added

#### Core engine — `ronin-core`

- Lossless, error-tolerant RON concrete syntax tree (CST) on [rowan]: parse →
  diagnostics → format → transform, with comments, key/field ordering, struct
  names, and formatting preserved byte-for-byte across every edit.
- I/O-free and WASM-clean (no filesystem, UI, async runtime, or network);
  compiles to both native and `wasm32`.

#### Type model — `ronin-types`

- Schema-optional, progressive type model normalized to JSON Schema 2020-12 with
  `x-ron-*` extensions.
- Type acquisition from static Rust source (syn), schemars-derived or
  user-supplied JSON Schema, and Bevy type-registry exports — merged into one
  internal model. Native-gated, never pulled into the WASM core.

#### Validation — `ronin-validate`

- WASM-clean, type-aware validation over a serialized type model, mapping JSON
  Pointer findings to CST text ranges under the `RON-V####` code namespace.
- Schema-optional and gracefully degrading: missing or unresolved types never
  produce false-positive errors. Offline — no network or TLS.

#### Desktop editor — `ronin-app`

- **Five projections of one document:** Tree/form (with uniform lists
  auto-routed into embedded virtualized tables), Table (outline), Table
  (sections), Table (grouped pivot), and raw Text — all backed by the same
  lossless CST; switching views changes zero bytes.
- **Excel-like table editing:** rectangular range selection (click/drag,
  `Ctrl+A`), edit via double-click / `F2` / type-over, `Enter`/`Tab` commit and
  move the selection, TSV copy / cut / paste / fill (`Ctrl+C`/`X`/`V`), and
  `Delete` to clear cells to their type defaults — each action a single undo unit.
- **Type-indicator legend** and **table navigation** (back / forward / up,
  breadcrumb, combine-child / union, group-by, show-columns) with auto-fitting
  columns and panels.
- **Type-aware validation** surfaced inline and in a Problems panel.
- **Non-destructive persistence:** atomic save (temp + fsync + rename) with
  crash-recovery sidecars and bounded CST-backed undo/redo.
- **Bevy mode:** scene-aware validation and defaults elision (reduce verbosity /
  expand to explicit) driven by an exported Bevy type registry — consumed as
  data, with no `bevy` dependency.
- **RON⇄JSON interop:** bidirectional conversion with explicit, never-silent loss
  reporting, plus derive-RON-from-type scaffolding.
- **Local-first:** no telemetry and no network calls at runtime.

### Distribution

- Prebuilt binaries for Windows, macOS (Intel + Apple Silicon), and Linux, with
  SHA-256 checksums and keyless build-provenance attestations; installable via
  `cargo binstall ronin-app` or `cargo install ronin-app`.

[rowan]: https://crates.io/crates/rowan
[0.1.0]: https://github.com/jsh562/RONin/releases/tag/v0.1.0
