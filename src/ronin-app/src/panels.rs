//! Panel layout seams for the editor shell.
//!
//! This module is the *single place* later epics extend the shell's side/bottom
//! panels **without** editing shell-core code. It exposes:
//!
//! * an **active** diagnostics-panel region ([`render_diagnostics_seam`]);
//! * the **active** structural **table** host ([`render_table_seam`]) — the E008
//!   virtualized spreadsheet view, wired into the per-document view switcher's
//!   Table arm (US2 / T035); and
//! * one **reserved** seam rendered as a labeled, disabled placeholder:
//!   [`mode_selector_seam_stub`] (reserved for **E009** — the Bevy mode selector).
//!   The legacy [`tree_table_seam_stub`] placeholder remains for layout/host
//!   discoverability; the live table now renders through [`render_table_seam`].
//!
//! The reserved seams render a faint "coming soon" placeholder rather than being
//! empty or a `// TODO`, so the layout is visible and the integration point is
//! discoverable in the running app.
//!
//! # Deferred scope (E008 / E009)
//!
//! The two reserved seams here are deliberate, named hand-offs:
//!
//! * [`tree_table_seam_stub`] reserves the structural **tree / virtualized table**
//!   views — deferred to **E008**.
//! * [`mode_selector_seam_stub`] reserves the **Bevy mode** selector — deferred to
//!   **E009**.
//!
//! Those epics populate these seams without editing shell-core layout.

use crate::diagnostics_map::DiagnosticView;
use crate::document::EditorDocument;
use crate::editor_view::render_binding_indicator;
use crate::reparse::ReparseWorker;
use crate::structural::sections::TableSection;
use crate::structural::table::{breadcrumb_segments, render_table_view_any};
use crate::structural::view_state::{resolve_path, PathStep, StructuralPath};

/// Host the structural **table-view navigator** for `doc` (E008 / E012 — T035,
/// [COMPLETES FR-005]).
///
/// Renders the always-visible active-binding indicator (FR-011) at the top, then a
/// **navigator**: it scans the whole document for every table-able section (record
/// lists, record maps, tuple lists) and lists them in a left side panel, each labelled
/// by its path and row-by-column dimensions. The user picks one and the selected
/// section renders as the existing virtualized grid where scalar cells edit inline,
/// RecordList rows add/remove, and a nested cell drills into the tree/form surface,
/// each routed through the one-undo-unit structural-edit pipeline (FR-013/FR-014).
/// When the document has no table-able section, an explanatory empty state is shown.
///
/// This is the [COMPLETES FR-005] host point wired into the per-document view
/// switcher's Table arm (FR-017). The `worker` is the document's off-frame reparse
/// worker, used to re-derive the projection after an edit lands.
pub fn render_table_seam(ui: &mut egui::Ui, doc: &mut EditorDocument, worker: &ReparseWorker) {
    render_binding_indicator(ui, doc);

    // Clone the scan out so the borrow on `doc` is released before the mutable
    // view-state writes below (the scan is cached per parse generation — FR-026).
    let sections = doc.cached_table_sections().to_vec();
    if sections.is_empty() {
        ui.weak(
            "No table-able sections in this document \u{2014} it has no uniform record lists, \
             record maps, or tuple lists. Switch to Tree/Form.",
        );
        return;
    }

    // Resolve the section the grid renders (E013 / Part A5, robustness): the stored
    // selection when it still resolves to a List/Map against the live CST, else fall
    // back to the first detected section. The selection is now a free-form path (it
    // may be a nested collection the user opened that is NOT a scanned section), so we
    // validate it against the live tree rather than only against the scanned set.
    let stored = doc.view_state().selected_table_section().cloned();
    let selected = stored
        .filter(|p| selection_is_openable(doc, p))
        .unwrap_or_else(|| sections[0].path.clone());

    // The grouped, collapsible navigator side list (Part A4).
    let mut clicked: Option<StructuralPath> = None;
    egui::Panel::left("ronin_table_navigator")
        .resizable(true)
        .default_size(240.0)
        .show_inside(ui, |ui| {
            ui.strong("Tables");
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                render_grouped_sections(ui, &sections, &selected, &mut clicked);
            });
        });

    // Persist a click (byte-free view-state write — FR-020).
    if let Some(path) = clicked {
        doc.view_state_mut().set_selected_table_section(Some(path));
        return;
    }

    // The central area: a stateless, path-derived breadcrumb above the grid, then the
    // selected collection projected as a table via `derive_any` (Part A1/A3).
    let mut breadcrumb_clicked: Option<StructuralPath> = None;
    egui::CentralPanel::default().show_inside(ui, |ui| {
        render_breadcrumb(ui, doc, &selected, &mut breadcrumb_clicked);
        ui.separator();
        render_table_view_any(ui, doc, worker, &selected);
    });

    if let Some(path) = breadcrumb_clicked {
        doc.view_state_mut().set_selected_table_section(Some(path));
    }
}

/// `true` when the navigator selection at `path` still resolves to a List or Map
/// against the live CST (an openable table target) — the Part-A5 robustness check.
fn selection_is_openable(doc: &EditorDocument, path: &StructuralPath) -> bool {
    let Some(parse) = doc.parse.as_ref() else {
        return false;
    };
    let root = parse.cst.root();
    matches!(
        resolve_path(&root, path).and_then(ron_core::ast::Value::cast),
        Some(ron_core::ast::Value::List(_) | ron_core::ast::Value::Map(_))
    )
}

/// Render the grouped, collapsible navigator list (Part A4): the scanned sections are
/// grouped by their **top-level ancestor** (the first path step's label, or `(root)`
/// for a root-level section), each group an [`egui::CollapsingHeader`] whose leaves
/// are `selectable_label`s showing `label  (R\u{00D7}C)` that select the section.
fn render_grouped_sections(
    ui: &mut egui::Ui,
    sections: &[TableSection],
    selected: &StructuralPath,
    clicked: &mut Option<StructuralPath>,
) {
    // Group indices by their top-level ancestor key, preserving first-seen group order
    // and document order within each group.
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, s) in sections.iter().enumerate() {
        let key = group_key(s);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, members)) => members.push(i),
            None => groups.push((key, vec![i])),
        }
    }

    for (key, members) in &groups {
        egui::CollapsingHeader::new(key)
            .default_open(true)
            .id_salt(("ronin_table_group", key))
            .show(ui, |ui| {
                for &i in members {
                    let s = &sections[i];
                    let is_selected = s.path == *selected;
                    let label = format!("{}   ({}\u{00D7}{})", s.label, s.rows, s.cols);
                    if ui
                        .selectable_label(is_selected, label)
                        .on_hover_text(format!("{:?}", s.shape))
                        .clicked()
                    {
                        *clicked = Some(s.path.clone());
                    }
                }
            });
    }
}

/// The grouping key for a section (Part A4): its top-level ancestor — the first path
/// step's display label, or `(root)` for a root-level section.
fn group_key(section: &TableSection) -> String {
    match section.path.steps().first() {
        Some(PathStep::Field(name) | PathStep::VariantField(name)) => name.clone(),
        Some(PathStep::Key(text)) => text.clone(),
        Some(PathStep::Index(i)) => format!("[{i}]"),
        None => "(root)".to_string(),
    }
}

/// Render the stateless, path-derived breadcrumb above the grid (Part A3): one segment
/// per prefix of `selected`, each a clickable button iff its prefix resolves to a
/// List/Map (clicking re-selects that prefix), otherwise shown weak / non-clickable.
fn render_breadcrumb(
    ui: &mut egui::Ui,
    doc: &EditorDocument,
    selected: &StructuralPath,
    clicked: &mut Option<StructuralPath>,
) {
    let Some(parse) = doc.parse.as_ref() else {
        return;
    };
    let segments = breadcrumb_segments(&parse.cst, selected);
    ui.horizontal_wrapped(|ui| {
        for (i, seg) in segments.iter().enumerate() {
            if i > 0 {
                ui.weak("\u{25B8}"); // U+25B8 separator
            }
            if seg.clickable && seg.path != *selected {
                if ui.button(&seg.label).clicked() {
                    *clicked = Some(seg.path.clone());
                }
            } else {
                // The current segment (or a non-openable ancestor) is non-clickable.
                ui.weak(&seg.label);
            }
        }
    });
}

/// Render the active diagnostics-panel region.
///
/// Lists the supplied [`DiagnosticView`]s (already projected into editor
/// coordinates) one per row: severity, code, `line:column`, and message. An
/// empty list shows a faint "No problems" state. This is the live seam — later
/// waves replace the row rendering with a richer, navigable problems panel.
pub fn render_diagnostics_seam(ui: &mut egui::Ui, diagnostics: &[DiagnosticView]) {
    if diagnostics.is_empty() {
        ui.weak("No problems");
        return;
    }
    for d in diagnostics {
        let (line, col) = d.line_col.0;
        // Lines/columns are zero-based internally; present them one-based.
        ui.label(format!(
            "{} {} [{}:{}] {}",
            d.severity,
            d.code,
            line + 1,
            col + 1,
            d.message
        ));
    }
}

/// Reserved seam for the **E008** structural tree/table views.
///
/// Renders a faint, disabled placeholder. Replace the body in E008 to mount the
/// tree/table widgets here without touching shell-core layout.
pub fn tree_table_seam_stub(ui: &mut egui::Ui) {
    ui.add_enabled_ui(false, |ui| {
        ui.weak("Structure (coming soon)");
    });
}

/// Reserved seam for the **E009** Bevy mode selector.
///
/// Renders a faint, disabled placeholder. Replace the body in E009 to mount the
/// mode selector here without touching shell-core layout.
pub fn mode_selector_seam_stub(ui: &mut egui::Ui) {
    ui.add_enabled_ui(false, |ui| {
        ui.weak("Mode (coming soon)");
    });
}
