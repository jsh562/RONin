//! Layout + empty-state smoke tests for the composed `App` shell (T044,
//! FR-013/FR-022, SC-006).
//!
//! These drive [`App::render_shell`] — the renderer-only layout path that takes
//! just `&mut egui::Ui` — through `egui_kittest`'s plain (renderer-free)
//! [`Harness::new_ui`], and assert the AccessKit-exposed labels of the docked
//! regions appear: the reserved E008 tree/table seam, the reserved E009
//! mode-selector seam, the active diagnostics (Problems) panel, and the
//! empty-workspace placeholder.
//!
//! Boundary note: `Harness::new_ui` lays out widgets without a GPU/`eframe::Frame`,
//! so the viewport/quit handling in `eframe::App::ui` (CancelClose / Close
//! viewport commands) is NOT exercised here — that requires a live `eframe`
//! viewport and is verified manually / in QC. Drag-to-reorder is likewise a live
//! pointer-gesture interaction and is covered behaviorally via the
//! `EditorWorkspace::reorder` unit tests, not pixel/gesture simulation here.

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;

use ronin_app::app::App;
use ronin_app::settings::AppSettings;

#[test]
fn empty_workspace_shows_seams_and_nonblank_placeholder() {
    // FR-022 + FR-013: with zero tabs the shell still composes the reserved seams
    // and the active diagnostics panel, and shows a NON-BLANK placeholder.
    let mut harness = Harness::new_ui(|ui| {
        let mut app = App::new(AppSettings::default(), None);
        app.render_shell(ui);
    });
    harness.run();

    // Reserved E008 tree/table seam present (label + placeholder). `query_all_*`
    // is used because the seam contributes more than one node containing the text
    // (the panel header label and the placeholder), which the single-result
    // `query_by_label_contains` treats as ambiguous.
    assert!(
        harness
            .query_all_by_label_contains("Structure")
            .next()
            .is_some(),
        "tree/table seam (E008) must be docked in the shell layout"
    );
    // Reserved E009 mode-selector seam present.
    assert!(
        harness.query_all_by_label_contains("Mode").next().is_some(),
        "mode-selector seam (E009) must be docked in the shell layout"
    );
    // Active diagnostics panel present (the 'Problems' heading), with its
    // empty-state since no tab is open.
    assert!(
        harness
            .query_all_by_label_contains("Problems")
            .next()
            .is_some(),
        "the active diagnostics (Problems) panel must be docked in the shell"
    );
    // FR-022: the empty central area is a non-blank welcome/empty-state.
    assert!(
        harness
            .query_all_by_label_contains("No file open")
            .next()
            .is_some(),
        "empty workspace must show a non-blank placeholder, not a blank area"
    );
}

#[test]
fn open_tab_shows_tab_strip_and_editor_region() {
    // FR-012/FR-013: with a tab open the tab strip renders the title, and the
    // reserved seams + diagnostics panel are still present alongside the editor.
    let fixture = write_temp("Config(level: 3)\n");
    let path = fixture.clone();
    let mut harness = Harness::new_ui(move |ui| {
        let mut app = App::new(AppSettings::default(), Some(path.clone()));
        app.render_shell(ui);
    });
    harness.run();

    // The opened file's name appears as a tab title (and in the central header),
    // so it contributes multiple matching nodes — use `query_all_*`.
    let name = fixture.file_name().unwrap().to_str().unwrap().to_string();
    assert!(
        harness.query_all_by_label_contains(&name).next().is_some(),
        "the open document's tab title must render in the shell"
    );
    // Seams remain present with a tab open (layout is stable across states).
    assert!(
        harness
            .query_all_by_label_contains("Structure")
            .next()
            .is_some(),
        "tree/table seam stays docked when a tab is open"
    );
    assert!(
        harness.query_all_by_label_contains("Mode").next().is_some(),
        "mode-selector seam stays docked when a tab is open"
    );
    assert!(
        harness
            .query_all_by_label_contains("Problems")
            .next()
            .is_some(),
        "diagnostics panel stays docked when a tab is open"
    );

    let _ = std::fs::remove_file(&fixture);
}

/// Write `contents` to a uniquely-named temp `.ron` file and return its path.
fn write_temp(contents: &str) -> std::path::PathBuf {
    use std::io::Write;
    let mut path = std::env::temp_dir();
    path.push(format!(
        "ronin_layout_{}_{}.ron",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut f = std::fs::File::create(&path).expect("create temp fixture");
    f.write_all(contents.as_bytes()).expect("write fixture");
    path
}
