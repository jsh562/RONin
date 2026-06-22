# ronin-app

**RONin** — a local-first desktop editor for **RON** (Rusty Object Notation), built on [egui/eframe](https://crates.io/crates/eframe).

![RONin — the Tree/form structural view (fully expanded) with the type legend and Problems panel](https://raw.githubusercontent.com/jsh562/RONin/main/screenshots/tree-form-view.png)

`ronin-app` is the application shell that wires the RONin workspace into an interactive editor over a single lossless document model. Open a `.ron` file and edit it as a tree/form, as a spreadsheet, or as raw text — every projection edits the same CST, so nothing is ever reflowed or lost.

## Highlights

- **Lossless editing** over the `ronin-core` CST — comments, key/field ordering, struct names, and formatting are preserved on every save.
- **Type-aware validation** (`ronin-validate`) with inline diagnostics and a Problems panel — schema-optional, degrading gracefully when no types are bound.
- **Five projections of one document** (see [Views](#views)) — switch freely; switching changes zero bytes.
- **Excel-like table editing** — a keyboard- and mouse-driven cell grid (see [below](#excel-like-table-editing)).
- **Non-destructive persistence** — atomic save (temp + fsync + rename) with crash-recovery sidecars; bounded CST-backed undo/redo (one action = one undo).
- **Bevy mode** — scene-aware validation and defaults elision driven by an exported Bevy type registry (consumed as data; no `bevy` dependency).
- **RON⇄JSON interop** — bidirectional conversion with explicit, never-silent loss reporting, plus derive-from-type scaffolding.
- **Local-first** — no telemetry, no network calls at runtime.


## Views

RONin projects the active document through five views, selectable from the **View** switcher:

| View | What it shows |
|------|---------------|
| **Tree/form** | Structural outline with inline editors. Uniform lists are auto-routed into embedded virtualized tables; everything else renders as a tree/form. A per-section override forces or unforces the table treatment. |
| **Table (outline)** | A virtualized spreadsheet grid with a collapsible tree navigator on the left and a breadcrumb above. Drill into nested cells; add and remove rows. |
| **Table (sections)** | The same grid, with the navigator grouped into scanner-detected sections (by top-level ancestor, largest first) for quick comparison. |
| **Table (grouped)** | Pivot-style variant: rows grouped by one or two chosen fields into collapsible groups. |
| **Text** | The raw RON source, backed by the same lossless CST. |

| Table (outline) | Table (sections) | Table (grouped) | Text |
|-----------------|------------------|-----------------|------|
| ![Table outline view](https://raw.githubusercontent.com/jsh562/RONin/main/screenshots/table-view.png) | ![Table sections view](https://raw.githubusercontent.com/jsh562/RONin/main/screenshots/table-sections-view.png) | ![Table grouped view](https://raw.githubusercontent.com/jsh562/RONin/main/screenshots/table-grouped-view.png) | ![Text view](https://raw.githubusercontent.com/jsh562/RONin/main/screenshots/text-view.png) |

## Excel-like table editing

The table grid behaves like a spreadsheet:

- **Select** a cell with a click, or **drag** to select a rectangular range; **Ctrl+A** selects all.
- **Edit** with double-click, **F2**, or by simply typing over the active cell.
- **Enter / Tab commit and move** the selection (down / right; hold **Shift** to reverse); **Esc** cancels.
- **Copy / cut / paste** ranges as **TSV** (`Ctrl+C` / `Ctrl+X` / `Ctrl+V`) — interoperates with real spreadsheets; a single-value paste fills the whole selection.
- **Delete / Backspace** clears the selected cells to their type defaults (`0`, `0.0`, `false`, `""`, …).
- Columns and side panels **auto-fit** their content; navigate with **back / forward / up**, the breadcrumb, **combine-child / union**, **group-by**, and **show-columns**.
- Every edit — including a block paste or a range clear — is a **single undo unit**.

A **type-indicator legend** strip (top-right of the view row) keys the glyphs used for containers, scalars, and cell status.

## Menus

- **File** — New, Open, Open Sample, Save / Save As, tab management (close / reopen), Settings, Type Bindings, Bevy Registries, Quit.
- **Edit** — Undo / Redo.
- **Format** — Format Document / Format Selection.
- **Bevy** — Reduce Verbosity / Expand to Explicit (active in Bevy mode with a registry loaded).
- **Convert** — RON⇄JSON conversion and import/export, plus Derive RON from type.
- **Snippets** — insert effective snippets; browse or edit the user snippet file.

## Install

`ronin-app` ships as a prebuilt binary on every [GitHub Release](https://github.com/jsh562/RONin/releases), and is also installable from source:

```bash
cargo binstall ronin-app   # prebuilt binary via the GitHub Release tarball
cargo install ronin-app    # or compile from source
```

See the [workspace README](https://github.com/jsh562/RONin#install) for binary verification (SHA-256 + build provenance) and the unsigned-binary OS prompts.

Licensed under MIT OR Apache-2.0.
