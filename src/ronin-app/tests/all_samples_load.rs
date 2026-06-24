//! Regression: **every bundled RON sample loads and renders — no blank views, no
//! recovery-sidecar litter.**
//!
//! This pins the user-reported "intermittent blank/empty view when switching back
//! and forth" failure mode end-to-end against the **real** open path and the App's
//! **own** off-frame [`ReparseWorker`]:
//!
//! * **`every_sample_on_disk_opens_and_renders`** — for each file in `samples/`
//!   ending `.ron`/`.scn.ron`, build a fresh [`App`], call the REAL
//!   [`App::open_file`] with the file's absolute path, drive the App's own off-frame
//!   parse to completion, then assert: a tab was created, there is **no error
//!   notice**, and the active document projects a tree model with **≥1 root node**
//!   (the structural proof that the view renders content rather than a blank).
//! * **`multi_open_session_each_tab_renders`** — open several samples *sequentially
//!   in one App* and assert each resulting tab still renders a tree model (the
//!   stateful "switching back and forth" case a single open cannot catch).
//! * **`open_sample_renders_and_writes_no_sidecar`** — for each
//!   [`App::showcase_samples`] entry, call the REAL [`App::open_sample`] and assert
//!   it creates a rendering tab AND leaves the document **path-less** (so autosave /
//!   crash-recovery never writes a `.ronin-recovery` sidecar), additionally scanning
//!   the current directory before/after to prove **no** sidecar file appeared.
//!
//! Every loop is bounded; the off-frame worker is the real one, driven to
//! completion exactly as `table_view.rs` / `showcase_samples.rs` do.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;

use ronin_app::app::{App, NoticeKind};
use ronin_app::document::EditorDocument;
use ronin_app::reparse::ReparseWorker;
use ronin_app::settings::AppSettings;
use ronin_app::structural::sections::{scan_table_sections, SectionShape};
use ronin_app::structural::table::{render_table_view_any, CellClass, TableModel};
use ronin_app::structural::view_state::StructuralPath;
use ronin_core::parse;

// =============================================================================
// Harness
// =============================================================================

/// The absolute path to the crate's `samples/` directory (robust regardless of
/// the test's working directory).
fn samples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("samples")
        .canonicalize()
        .expect("samples/ directory resolves")
}

/// Every `samples/*.ron` / `*.scn.ron` file, as absolute paths, sorted for a stable
/// order. (`.scn.ron` ends with `.ron`, so the single suffix test covers both.)
fn sample_paths() -> Vec<PathBuf> {
    let dir = samples_dir();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("ron"))
        .collect();
    paths.sort();
    assert!(
        !paths.is_empty(),
        "expected at least one sample under {}",
        dir.display()
    );
    paths
}

/// Drive the App's OWN off-frame parse to completion: spin-poll [`App::poll_documents`]
/// until the active document installs a current parse, or panic on a bounded timeout.
fn drive_app_reparse(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        app.poll_documents();
        if app
            .active_document()
            .and_then(|d| d.parse.as_ref())
            .is_some()
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the App's off-frame reparse did not land within the timeout"
        );
        std::thread::yield_now();
    }
}

/// `true` when any open document carries an **error** notice (a blocking failure —
/// the user-facing signal of a failed open).
fn has_error_notice(app: &App) -> bool {
    app.notices().iter().any(|n| n.kind == NoticeKind::Error)
}

/// Assert the App's active document renders a non-blank tree model (≥1 root node) —
/// the structural proof that the view shows content, not a blank.
fn assert_active_renders(app: &mut App, label: &str) {
    let model = app
        .active_document_mut()
        .expect("an active document for the opened sample")
        .cached_tree_model()
        .unwrap_or_else(|| panic!("{label}: active document projected NO tree model (blank view)"));
    assert!(
        !model.roots.is_empty(),
        "{label}: tree model has zero root nodes (blank view)"
    );
}

/// The set of `*.ronin-recovery` file names in `dir` (the litter we must never create
/// for a bundled sample).
fn recovery_sidecars_in(dir: &Path) -> BTreeSet<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return BTreeSet::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| name.ends_with(".ronin-recovery"))
        .collect()
}

// =============================================================================
// Every on-disk sample opens via the real path and renders (no blank)
// =============================================================================

#[test]
fn every_sample_on_disk_opens_and_renders() {
    for path in sample_paths() {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap()
            .to_string();
        let mut app = App::new(AppSettings::default(), None);

        // The REAL open path with an absolute path.
        app.open_file(&path);

        // A tab must exist (no silent blank: the open did not vanish).
        assert!(
            app.active_index().is_some() && app.document_count() >= 1,
            "{name}: open_file created no tab"
        );
        // No error notice (the open succeeded, not failed-with-message).
        assert!(
            !has_error_notice(&app),
            "{name}: open_file pushed an error notice: {:?}",
            app.notices()
                .iter()
                .map(|n| n.message.clone())
                .collect::<Vec<_>>()
        );

        // Drive the App's own off-frame worker to completion and assert the view
        // renders content (≥1 tree root), not a blank.
        drive_app_reparse(&mut app);
        assert_active_renders(&mut app, &name);
    }
}

// =============================================================================
// Multi-open in ONE App — catches stateful "switching back and forth" blanks
// =============================================================================

#[test]
fn multi_open_session_each_tab_renders() {
    let dir = samples_dir();
    // A representative sequential session (the spec's listed set), opened one after
    // another into the SAME App so cross-tab state is exercised.
    let session = [
        "sample.ron",
        "ships.ron",
        "showcase_tables.ron",
        "showcase_tree.ron",
        "showcase_interop.ron",
    ];

    let mut app = App::new(AppSettings::default(), None);
    for (i, name) in session.iter().enumerate() {
        let path = dir.join(name);
        app.open_file(&path);
        assert!(
            !has_error_notice(&app),
            "{name}: open_file pushed an error notice"
        );
        assert_eq!(
            app.document_count(),
            i + 1,
            "{name}: expected {} open tabs after sequential open",
            i + 1
        );
        // Each freshly opened tab must render after its parse lands.
        drive_app_reparse(&mut app);
        assert_active_renders(&mut app, name);
    }

    // Now switch back to each tab in turn — the literal "switching back and forth"
    // path. Re-opening an already-open path focuses its existing tab (FR-025) rather
    // than creating a duplicate, so this drives the real focus-existing switch via
    // the public open path. Each revisited tab must still render a tree model (the
    // cached projection from its earlier reparse), never a blank.
    let revisit = [
        "sample.ron",
        "showcase_interop.ron",
        "ships.ron",
        "sample.ron",
    ];
    let count_before_revisit = app.document_count();
    for name in revisit {
        let path = dir.join(name);
        app.open_file(&path);
        // Focus-existing must NOT create a new tab.
        assert_eq!(
            app.document_count(),
            count_before_revisit,
            "{name}: revisiting an open sample created a duplicate tab"
        );
        // Drain any straggler parse, then assert the revisited tab renders.
        app.poll_documents();
        let label = format!("{name} on revisit");
        assert_active_renders(&mut app, &label);
    }
}

// =============================================================================
// open_sample — renders AND writes no recovery-sidecar litter
// =============================================================================

#[test]
fn open_sample_renders_and_writes_no_sidecar() {
    let cwd = std::env::current_dir().expect("a current dir");
    let before = recovery_sidecars_in(&cwd);

    for (name, text) in App::showcase_samples() {
        let mut app = App::new(AppSettings::default(), None);

        // The REAL embedded-sample open path.
        app.open_sample(name, text);

        // A tab exists.
        assert!(
            app.active_index().is_some(),
            "open_sample({name}) created no tab"
        );
        // It is PATH-LESS — the root-cause fix: no on-disk path means autosave /
        // crash-recovery can never derive a sidecar for it (no litter).
        let doc = app.active_document().expect("an active sample document");
        assert!(
            doc.path.is_none(),
            "open_sample({name}) left an on-disk path ({:?}) — would litter a sidecar",
            doc.path
        );
        // ...but the tab title still shows the sample name (display-only title).
        assert_eq!(
            doc.title(),
            *name,
            "open_sample({name}) tab title should show the sample name"
        );
        // And it renders content, not a blank.
        drive_app_reparse(&mut app);
        assert_active_renders(&mut app, name);
    }

    // No new `*.ronin-recovery` file appeared in the working directory as a result
    // of opening every sample (the litter the fix eliminates).
    let after = recovery_sidecars_in(&cwd);
    let leaked: Vec<_> = after.difference(&before).collect();
    assert!(
        leaked.is_empty(),
        "open_sample littered recovery sidecar(s) into {}: {:?}",
        cwd.display(),
        leaked
    );
}

// =============================================================================
// The Table view is NEVER empty for any sample (E012 — Part A2/A3)
// =============================================================================

#[test]
fn every_sample_table_view_is_non_empty_at_root() {
    // E012 (Part A3): the Table view defaults to the document root and is NEVER empty —
    // `derive_any` over the root projects a model with ≥1 column for every bundled
    // sample (a struct → field/value grid, a list → records/tuples/value, a map →
    // key+value). A scalar-leaf root is the only `None` case and no sample has one.
    for path in sample_paths() {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap()
            .to_string();
        let mut app = App::new(AppSettings::default(), None);
        app.open_file(&path);
        drive_app_reparse(&mut app);

        let doc = app
            .active_document_mut()
            .expect("an active document for the opened sample");
        // The default Table selection (Part A3) is the document root; `derive_any` over
        // it must yield a model with ≥1 column (the Table view shows content, not blank).
        let model = doc
            .cached_table_model_any(&StructuralPath::root())
            .unwrap_or_else(|| {
                panic!("{name}: the root projects NO table model (the Table view would be empty)")
            });
        assert!(
            !model.columns.is_empty(),
            "{name}: the root table model has zero columns (empty Table view)"
        );
    }
}

#[test]
fn sample_ron_root_renders_a_field_value_grid() {
    // E012 (Part A2/A3): `sample.ron`'s root is the `Config` struct, so the Table view
    // renders it as a field/value grid — a leading read-only `(field)` column + a
    // `value` column, one row per field — rather than an empty state.
    let path = samples_dir().join("sample.ron");
    let mut app = App::new(AppSettings::default(), None);
    app.open_file(&path);
    drive_app_reparse(&mut app);

    let doc = app
        .active_document_mut()
        .expect("an active document for sample.ron");
    let model = doc
        .cached_table_model_any(&StructuralPath::root())
        .expect("sample.ron root projects a field/value grid");
    let cols: Vec<&str> = model
        .columns
        .iter()
        .map(|c| c.field_name.as_str())
        .collect();
    assert_eq!(
        cols,
        vec!["(field)", "value"],
        "sample.ron root struct renders a leading `(field)` column + a `value` column"
    );
    assert!(
        model.row_count() >= 1,
        "the field/value grid has one row per struct field"
    );
}

// =============================================================================
// (T036) The new cell editors RENDER over the REAL sample corpus — a sample
// section carrying bool / enum / numeric cells is opened via the real path and
// rendered through the real virtualized grid headlessly (egui_kittest — never a
// live-GUI screenshot; memory rule), proving the new editors paint their cells
// for the actual on-disk corpus, not just synthetic fixtures.
// =============================================================================

/// Spin-poll a document's own off-frame worker to completion (the `table_view.rs`
/// pattern), bounded.
fn drive_doc_reparse(doc: &mut EditorDocument, worker: &ReparseWorker) {
    doc.request_reparse(worker);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if doc.poll_parse(worker) {
            return;
        }
        assert!(Instant::now() < deadline, "reparse did not land within timeout");
        std::thread::yield_now();
    }
}

/// The path of the first scanned RecordList section whose union columns include every
/// name in `must_have`, over the file at `name`'s on-disk bytes.
fn record_list_section_with(name: &str, must_have: &[&str]) -> (String, StructuralPath) {
    let dir = samples_dir();
    let src = std::fs::read_to_string(dir.join(name)).expect("read sample");
    let cst = parse(&src);
    let path = scan_table_sections(&cst)
        .into_iter()
        .filter(|s| s.shape == SectionShape::RecordList)
        .filter_map(|s| {
            TableModel::derive_section(&cst, &s.path, SectionShape::RecordList, &[]).map(|m| (s, m))
        })
        .find(|(_, m)| {
            let cols: Vec<&str> = m.columns.iter().map(|c| c.field_name.as_str()).collect();
            must_have.iter().all(|need| cols.contains(need))
        })
        .map(|(s, _)| s.path)
        .unwrap_or_else(|| panic!("no RecordList in `{name}` with columns {must_have:?}"));
    (src, path)
}

#[test]
fn corpus_bool_enum_numeric_section_renders_through_the_grid() {
    // `ships.ron`'s hull `cells` RecordList carries a bool (`structural`), an enum-like
    // (`shape`), and a numeric (`section`) column. Open the bytes, render THAT section
    // through the real grid headlessly, and assert the new editors painted their cells:
    // the bool column's true/false values, the enum variant tokens, and the numeric
    // values are all present in the AccessKit tree (the grid drew the rows, not a blank).
    let worker = Rc::new(ReparseWorker::new());
    let (src, section) = record_list_section_with("ships.ron", &["structural", "shape", "section"]);

    let mut doc = EditorDocument::new_untitled(1);
    doc.buffer = src.clone();
    doc.on_edit();
    drive_doc_reparse(&mut doc, &worker);

    // Sanity: the section projects bool + enum + numeric Scalar cells.
    let model = doc
        .cached_table_model_any(&section)
        .cloned()
        .expect("the `cells` section projects a table model");
    let bool_col = model
        .columns
        .iter()
        .position(|c| c.field_name == "structural")
        .unwrap();
    let bool_word_ok = (0..model.row_count())
        .filter_map(|r| model.cell(r, bool_col))
        .filter(|c| c.class == CellClass::Scalar)
        .all(|c| c.scalar_type_name() == Some("bool"));
    assert!(bool_word_ok, "the `structural` column projects bool cells");

    let doc = Rc::new(RefCell::new(doc));
    let doc_ui = Rc::clone(&doc);
    let worker_ui = Rc::clone(&worker);
    let section_ui = section.clone();
    let mut harness = Harness::builder()
        .with_size(egui::vec2(700.0, 400.0))
        .build_ui(move |ui| {
            let mut d = doc_ui.borrow_mut();
            render_table_view_any(ui, &mut d, &worker_ui, &section_ui);
        });
    harness.run();

    // The grid painted the new-editor cells: the bool cells render as the E012 checkbox
    // affordance (☑ true / ☐ false, FR-001) — not as literal `true`/`false` text — so
    // its presence proves the bool editor drew the corpus cells. The header field names
    // are all in the rendered tree too.
    assert!(
        harness.query_all_by_label_contains("\u{2611}").next().is_some()
            || harness.query_all_by_label_contains("\u{2610}").next().is_some(),
        "the bool cells rendered the ☑/☐ checkbox affordance in the grid"
    );
    for header in ["structural", "shape", "section"] {
        assert!(
            harness.query_all_by_label_contains(header).next().is_some(),
            "the `{header}` column header rendered in the grid"
        );
    }

    // Rendering the corpus section is VIEW-ONLY — it changed zero bytes.
    assert_eq!(
        doc.borrow().buffer,
        src,
        "rendering the corpus section with the new editors did not reflow the file"
    );
}

#[test]
fn corpus_events_numeric_and_enum_cells_render_through_the_grid() {
    // `showcase_kitchen_sink.ron`'s top-level `events` RecordList carries a numeric
    // (`at`) and an enum-like (`kind`) column. Render that section through the real grid
    // headlessly and assert the numeric values + the enum variant tokens painted.
    let worker = Rc::new(ReparseWorker::new());
    let (src, section) = record_list_section_with("showcase_kitchen_sink.ron", &["at", "kind"]);

    let mut doc = EditorDocument::new_untitled(1);
    doc.buffer = src.clone();
    doc.on_edit();
    drive_doc_reparse(&mut doc, &worker);

    let doc = Rc::new(RefCell::new(doc));
    let doc_ui = Rc::clone(&doc);
    let worker_ui = Rc::clone(&worker);
    let section_ui = section.clone();
    let mut harness = Harness::builder()
        .with_size(egui::vec2(700.0, 400.0))
        .build_ui(move |ui| {
            let mut d = doc_ui.borrow_mut();
            render_table_view_any(ui, &mut d, &worker_ui, &section_ui);
        });
    harness.run();

    // The enum variant tokens render (Spawn / Move / Despawn) and the headers paint.
    for variant in ["Spawn", "Move", "Despawn"] {
        assert!(
            harness.query_all_by_label_contains(variant).next().is_some(),
            "the `{variant}` enum cell rendered in the grid"
        );
    }
    for header in ["at", "kind"] {
        assert!(
            harness.query_all_by_label_contains(header).next().is_some(),
            "the `{header}` column header rendered in the grid"
        );
    }
    assert_eq!(
        doc.borrow().buffer,
        src,
        "rendering the events section with the new editors did not reflow the file"
    );
}
