//! The navigable Problems panel (FR-009).
//!
//! [`problems_panel`] renders the document's diagnostics as a click-to-navigate
//! list, ordered by source location (line, then column). Selecting a row returns
//! its index so the shell can move the editor caret to that diagnostic's range
//! (the actual caret jump is applied in `editor_view`). When there are no
//! diagnostics the panel shows an explicit "No problems" empty state.
//!
//! The panel never mutates its input: it sorts a copy of the *indices*, so the
//! caller's diagnostic order (which mirrors the buffer's storage order) is
//! untouched and the returned index always refers to the original slice.

use ron_core::Severity;

use crate::diagnostics_map::DiagnosticView;

/// Render the Problems panel and report a clicked row (FR-009).
///
/// Rows are presented ordered by source location — line ascending, then column
/// ascending — by sorting a copy of the diagnostic indices (the input slice is
/// never reordered). Each row shows a severity icon, the stable `RON-Pxxxx` code,
/// the message, and the one-based `line:col`. Returns `Some(i)`, where `i` indexes
/// the **original** `diagnostics` slice, when the user clicks a row this frame;
/// otherwise `None`. An empty slice renders a faint "No problems" state.
pub fn problems_panel(ui: &mut egui::Ui, diagnostics: &[DiagnosticView]) -> Option<usize> {
    if diagnostics.is_empty() {
        ui.weak("No problems");
        return None;
    }

    // Sort a copy of the indices by (line, column) of each diagnostic's start.
    // The input slice is never mutated, so the returned index stays valid for it.
    let mut order: Vec<usize> = (0..diagnostics.len()).collect();
    order.sort_by_key(|&i| {
        let (line, col) = diagnostics[i].line_col.0;
        (line, col, i)
    });

    let mut clicked: Option<usize> = None;
    for &idx in &order {
        let d = &diagnostics[idx];
        let (line, col) = d.line_col.0;
        // Lines/columns are zero-based internally; present them one-based.
        let label = format!(
            "{} {}  {}  [{}:{}]",
            severity_icon(d.severity),
            d.code,
            d.message,
            line + 1,
            col + 1
        );
        // A selectable, full-width row so the whole entry is the click target.
        if ui.selectable_label(false, label).clicked() {
            clicked = Some(idx);
        }
    }

    clicked
}

/// A compact severity glyph for a Problems row.
fn severity_icon(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "\u{2716}",   // ✖ heavy multiplication x
        Severity::Warning => "\u{26A0}", // ⚠ warning sign
    }
}
