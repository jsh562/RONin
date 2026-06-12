//! Headless egui harness tests for the panel seams (T012).
//!
//! Uses `egui_kittest`'s plain (renderer-free) [`Harness::new_ui`] to lay out the
//! seam render functions and assert their AccessKit-exposed labels appear. This
//! is a *layout-level* check: it needs no GPU/wgpu backend, so it runs headless
//! in CI. Snapshot/pixel testing (the `snapshot`/`wgpu` egui_kittest features) is
//! intentionally not used.

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;

use ronin_app::diagnostics_map::DiagnosticView;
use ronin_app::panels::{mode_selector_seam_stub, render_diagnostics_seam, tree_table_seam_stub};

#[test]
fn reserved_seams_render_their_placeholder_labels() {
    let mut harness = Harness::new_ui(|ui| {
        tree_table_seam_stub(ui);
        mode_selector_seam_stub(ui);
    });
    harness.run();

    assert!(
        harness.query_by_label_contains("Structure").is_some(),
        "tree/table seam must render its 'Structure (coming soon)' placeholder"
    );
    assert!(
        harness.query_by_label_contains("Mode").is_some(),
        "mode-selector seam must render its 'Mode (coming soon)' placeholder"
    );
}

#[test]
fn diagnostics_seam_shows_empty_state_when_no_problems() {
    let mut harness = Harness::new_ui(|ui| {
        render_diagnostics_seam(ui, &[]);
    });
    harness.run();

    assert!(
        harness.query_by_label_contains("No problems").is_some(),
        "empty diagnostics seam must show the 'No problems' state"
    );
}

#[test]
fn diagnostics_seam_lists_a_diagnostic_row() {
    use ron_core::{DiagnosticCode, Severity};

    let view = DiagnosticView {
        char_range: (4, 5),
        line_col: ((0, 4), (0, 5)),
        severity: Severity::Error,
        code: DiagnosticCode::UnexpectedToken,
        message: "unexpected token".to_string(),
    };

    let mut harness = Harness::new_ui(move |ui| {
        render_diagnostics_seam(ui, std::slice::from_ref(&view));
    });
    harness.run();

    assert!(
        harness
            .query_by_label_contains("unexpected token")
            .is_some(),
        "diagnostics seam must render the diagnostic message"
    );
    assert!(
        harness.query_by_label_contains("RON-P0001").is_some(),
        "diagnostics seam must render the stable diagnostic code"
    );
}
