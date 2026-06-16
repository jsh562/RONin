//! `ronin-app` — RONin desktop editor binary (FR-003).
//!
//! Thin entry point: parse an optional file-path CLI argument, start non-blocking
//! file logging (holding the [`WorkerGuard`] for the whole run), build the [`App`]
//! shell (opening the CLI path when present), and run it on `eframe`'s **glow**
//! backend. A missing / unreadable / non-UTF-8 CLI path does **not** abort launch
//! — [`App::new`] records a notice and still starts into an empty workspace.

use std::path::PathBuf;

use ronin_app::app::App;
use ronin_app::settings::AppSettings;

fn main() -> eframe::Result<()> {
    // Hold the logging guard for the program lifetime (dropping it may lose
    // buffered log lines). `verbose` follows the `RONIN_VERBOSE` env var.
    let _log_guard = ronin_app::logging::init_logging(std::env::var_os("RONIN_VERBOSE").is_some());

    // First positional argument, if any, is a file to open at launch (FR-003).
    let cli_path: Option<PathBuf> = std::env::args_os().nth(1).map(PathBuf::from);

    let settings = AppSettings::load();
    let native_options = native_options(&settings);

    eframe::run_native(
        "RONin",
        native_options,
        Box::new(move |cc| {
            // Install the bundled Noto symbol/math fonts as glyph fallbacks before
            // any text is laid out, so authored UI glyphs (symbols, math) render.
            App::install_fonts(&cc.egui_ctx);
            let app = App::new(settings, cli_path);
            // Let the reparse worker wake an idle UI when results land (FR-024).
            app.set_repaint_ctx(cc.egui_ctx.clone());
            Ok(Box::new(app))
        }),
    )
}

/// Build `eframe` native options, restoring window geometry from settings (FR-016).
fn native_options(settings: &AppSettings) -> eframe::NativeOptions {
    let mut viewport = egui::ViewportBuilder::default().with_title("RONin");

    if let Some(geometry) = &settings.window_geometry {
        viewport = viewport.with_inner_size(egui::vec2(geometry.size.0, geometry.size.1));
        if let Some((x, y)) = geometry.pos {
            viewport = viewport.with_position(egui::pos2(x, y));
        }
    }

    eframe::NativeOptions {
        viewport,
        ..Default::default()
    }
}
