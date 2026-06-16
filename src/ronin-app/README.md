# ronin-app

**RONin** — a desktop editor for **RON** (Rusty Object Notation), built on [egui/eframe](https://crates.io/crates/eframe).

`ronin-app` is the application shell that wires the RONin workspace into an interactive editor:

- **Lossless editing** over the `ron-core` CST — comments, ordering, and formatting are preserved on every save.
- **Type-aware validation** (`ron-validate`) with inline diagnostics + a Problems panel, schema-optional and progressively degrading.
- **Structural & table editing** — tree/form and spreadsheet views over the same document.
- **Non-destructive persistence** — atomic save + crash-recovery sidecars; bounded CST-backed undo/redo.
- **Bevy mode** — scene-aware validation + defaults elision driven by an exported Bevy type registry (consumed as data; no `bevy` dependency).
- **RON⇄JSON interop** — bidirectional conversion with explicit, never-silent loss reporting, plus derive-from-type scaffolding.

Fully local-first: no telemetry, no network calls at runtime.

Licensed under MIT OR Apache-2.0.
