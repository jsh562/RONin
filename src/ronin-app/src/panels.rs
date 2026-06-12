//! Panel layout seams for the editor shell.
//!
//! This module is the *single place* later epics extend the shell's side/bottom
//! panels **without** editing shell-core code. It exposes:
//!
//! * an **active** diagnostics-panel region ([`render_diagnostics_seam`]) — the
//!   only seam wired up in Wave 1; and
//! * two **reserved** seams rendered as labeled, disabled placeholders:
//!   [`tree_table_seam_stub`] (reserved for **E008** — structural tree/table
//!   views) and [`mode_selector_seam_stub`] (reserved for **E009** — the Bevy
//!   mode selector).
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
