//! Headless documentation-screenshot generator (opt-in, NO live GUI).
//!
//! Renders the real `eframe::App` through egui_kittest's wgpu renderer and writes
//! PNGs to the repo-root `screenshots/` dir. Driving the app via `build_eframe`
//! means the app's own `update()` pumps the off-thread reparse worker, so the
//! views populate exactly as in the live app — with no window/desktop capture.
//!
//! All shots use `samples/ships.ron`. Tree/form is fully expanded ("Expand all");
//! the table views show the **combined-child** union `hulls ▸ ∗cells` (every hull's
//! `cells` flattened into one spreadsheet).
//!
//! Gated behind the `screenshots` feature and `#[ignore]` so normal
//! `cargo test` / CI never builds wgpu or runs these.
//!
//! Run:
//!   cargo test -p ronin-app --features screenshots --test gen_screenshots \
//!       -- --ignored --test-threads=1 --nocapture
#![cfg(feature = "screenshots")]

use std::path::PathBuf;
use std::time::Duration;

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;
use ronin_app::app::App;
use ronin_app::settings::AppSettings;
use ronin_app::structural::view_state::{ActiveView, PathStep, StructuralPath};

const SAMPLE: &str = include_str!("../samples/sample.ron");
const SHIPS: &str = include_str!("../samples/ships.ron");
const TREE: &str = include_str!("../samples/showcase_tree.ron");
const BEVY: &str = include_str!("../samples/showcase_bevy.scn.ron");

fn screenshots_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../screenshots");
    std::fs::create_dir_all(&dir).expect("create screenshots dir");
    dir
}

fn new_harness(width: f32, height: f32) -> Harness<'static, App> {
    Harness::builder()
        .with_size(egui::vec2(width, height))
        .wgpu()
        .build_eframe(|cc| {
            App::install_fonts(&cc.egui_ctx);
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            App::new(AppSettings::default(), None)
        })
}

/// Run a bounded set of frames so the off-thread reparse worker delivers and the
/// view realizes before capture.
fn settle(h: &mut Harness<'_, App>) {
    for _ in 0..20 {
        h.run();
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn set_view(app: &mut App, view: ActiveView) {
    if let Some(doc) = app.active_document_mut() {
        doc.view_state_mut().set_active_view(view);
    }
}

/// Drill the table navigator to the combined-child union `hulls ▸ ∗cells`.
fn drill_combined_cells(app: &mut App) {
    let path = StructuralPath::root()
        .child(PathStep::Field("hulls".to_string()))
        .child(PathStep::CombinedChild("cells".to_string()));
    if let Some(doc) = app.active_document_mut() {
        doc.view_state_mut().navigate_table_section(path);
    }
}

fn save(img: image::RgbaImage, name: &str) {
    img.save(screenshots_dir().join(name)).expect("save png");
    eprintln!("wrote screenshots/{name}");
}

#[test]
#[ignore = "screenshot generator; run with --features screenshots -- --ignored"]
fn gen_tree_form_view() {
    let mut h = new_harness(1080.0, 760.0);
    h.state_mut().open_sample("ships.ron", SHIPS);
    set_view(h.state_mut(), ActiveView::TreeForm);
    settle(&mut h); // parse + initial render so "Expand all" exists
    h.get_by_label("Expand all").click();
    settle(&mut h); // apply expand-all + render
    save(h.render().expect("wgpu render"), "tree-form-view.png");
}

#[test]
#[ignore = "screenshot generator; run with --features screenshots -- --ignored"]
fn gen_table_view() {
    let mut h = new_harness(1080.0, 760.0);
    h.state_mut().open_sample("ships.ron", SHIPS);
    set_view(h.state_mut(), ActiveView::Table);
    drill_combined_cells(h.state_mut());
    settle(&mut h);
    save(h.render().expect("wgpu render"), "table-view.png");
}

/// The Table (sections) variant: same combined-cells grid, but the left navigator is
/// the scanner-driven grouped-sections list (vs. the outline tree).
#[test]
#[ignore = "screenshot generator; run with --features screenshots -- --ignored"]
fn gen_table_sections_view() {
    let mut h = new_harness(1080.0, 760.0);
    h.state_mut().open_sample("ships.ron", SHIPS);
    set_view(h.state_mut(), ActiveView::TableSections);
    drill_combined_cells(h.state_mut());
    settle(&mut h);
    save(h.render().expect("wgpu render"), "table-sections-view.png");
}

#[test]
#[ignore = "screenshot generator; run with --features screenshots -- --ignored"]
fn gen_table_grouped_view() {
    let mut h = new_harness(1080.0, 760.0);
    h.state_mut().open_sample("ships.ron", SHIPS);
    set_view(h.state_mut(), ActiveView::TableGrouped);
    drill_combined_cells(h.state_mut());
    if let Some(doc) = h.state_mut().active_document_mut() {
        doc.view_state_mut().set_group_by(vec![2]); // group the combined cells by `structural`
    }
    settle(&mut h);
    save(h.render().expect("wgpu render"), "table-grouped-view.png");
}

#[test]
#[ignore = "screenshot generator; run with --features screenshots -- --ignored"]
fn gen_text_view() {
    let mut h = new_harness(1080.0, 760.0);
    h.state_mut().open_sample("ships.ron", SHIPS);
    set_view(h.state_mut(), ActiveView::Text);
    settle(&mut h);
    save(h.render().expect("wgpu render"), "text-view.png");
}

/// Hero: a few tabs open, ships active, the combined-child `cells` spreadsheet in
/// the Table (outline) view, with the legend strip + Problems panel visible.
#[test]
#[ignore = "screenshot generator; run with --features screenshots -- --ignored"]
fn gen_overview() {
    let mut h = new_harness(1280.0, 824.0);
    h.state_mut().open_sample("sample.ron", SAMPLE);
    h.state_mut().open_sample("showcase_tree.ron", TREE);
    h.state_mut().open_sample("showcase_bevy.scn.ron", BEVY);
    h.state_mut().open_sample("ships.ron", SHIPS);
    set_view(h.state_mut(), ActiveView::Table);
    drill_combined_cells(h.state_mut());
    settle(&mut h);
    save(h.render().expect("wgpu render"), "overview.png");
}
