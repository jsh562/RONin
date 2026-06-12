//! Shell-level tests for the [`App`] (T024/T017/T019/T020/T025).
//!
//! These drive the App's non-rendering logic directly — construction, the CLI /
//! open path, and the open-failure notice contract — which needs no `eframe::Frame`
//! and so runs headlessly. The full `eframe::App::ui` render pass requires a live
//! `eframe::Frame` that `egui_kittest` does not synthesize in this version; the
//! widget-level rendering it *does* drive is covered by the `open_and_view` and
//! `edit_feedback` harness tests. This is an honest coverage boundary, not a gap
//! in behavior tested.

use std::io::Write;

use ronin_app::app::{classify_drop, App, DropDecision, NoticeKind};
use ronin_app::settings::AppSettings;

/// Write `contents` to a uniquely-named temp `.ron` file and return its path.
fn temp_ron(contents: &str, tag: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "ronin_app_{tag}_{}_{}.ron",
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

#[test]
fn cli_path_opens_into_active_tab() {
    // FR-003: a valid CLI path opens as the active document at launch.
    let fixture = temp_ron("Config(level: 3)\n", "cli_ok");
    let app = App::new(AppSettings::default(), Some(fixture.clone()));
    assert_eq!(app.document_count(), 1, "CLI file must open one tab");
    let active = app.active_document().expect("active document present");
    assert_eq!(active.buffer, "Config(level: 3)\n");
    assert!(
        app.notices().is_empty(),
        "a successful CLI open must not push a notice"
    );
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn missing_cli_path_still_launches_into_empty_workspace() {
    // FR-003: a missing/unreadable path must NOT abort launch; it pushes an error
    // notice and starts empty.
    let missing = std::env::temp_dir().join("definitely_not_here_ronin.ron");
    let _ = std::fs::remove_file(&missing); // ensure absent
    let app = App::new(AppSettings::default(), Some(missing));
    assert_eq!(app.document_count(), 0, "no tab for an unreadable CLI path");
    assert_eq!(app.notices().len(), 1, "an error notice must be pushed");
    assert_eq!(
        app.notices()[0].kind,
        NoticeKind::Error,
        "open failure is an error-severity notice"
    );
}

#[test]
fn non_utf8_cli_path_pushes_error_notice_and_no_tab() {
    // FR-018/FR-020: non-UTF-8 reject is a dismissible error notice; no tab.
    let mut path = std::env::temp_dir();
    path.push(format!("ronin_app_badutf8_{}.ron", std::process::id()));
    std::fs::write(&path, [0xFF, 0xFE, 0x00]).expect("write bad fixture");

    let app = App::new(AppSettings::default(), Some(path.clone()));
    assert_eq!(app.document_count(), 0, "non-UTF-8 file creates no tab");
    assert_eq!(app.notices().len(), 1);
    let notice = &app.notices()[0];
    assert_eq!(notice.kind, NoticeKind::Error);
    assert!(
        notice.message.contains("not valid UTF-8"),
        "non-UTF-8 notice must say 'not valid UTF-8', got: {}",
        notice.message
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn no_cli_path_launches_empty_with_no_notices() {
    let app = App::new(AppSettings::default(), None);
    assert_eq!(app.document_count(), 0);
    assert!(app.notices().is_empty());
    assert!(app.active_document().is_none());
}

#[test]
fn open_file_adds_active_tab_and_requests_parse() {
    // T017/T025: opening a valid file adds an active tab; the document is set up
    // for an initial reparse (edit generation advanced past the baseline).
    let fixture = temp_ron("List([1, 2, 3])\n", "open_ok");
    let mut app = App::new(AppSettings::default(), None);
    app.open_file(&fixture);
    assert_eq!(app.document_count(), 1);
    let doc = app.active_document().expect("active doc");
    assert!(
        doc.edit_generation() > 0,
        "opening a file must request an initial parse (generation advanced)"
    );
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn new_untitled_creates_active_blank_tab() {
    let mut app = App::new(AppSettings::default(), None);
    app.new_untitled();
    assert_eq!(app.document_count(), 1);
    let doc = app.active_document().expect("active doc");
    assert_eq!(doc.buffer, "");
    assert_eq!(doc.title(), "Untitled-1");
}

#[test]
fn large_file_threshold_is_exposed_from_settings() {
    // An above-floor value passes through the accessor unchanged.
    let settings = AppSettings {
        large_file_threshold: 2_000_000,
        ..AppSettings::default()
    };
    let app = App::new(settings, None);
    assert_eq!(app.large_file_threshold(), 2_000_000);
}

#[test]
fn large_file_threshold_is_floored_below_64_kib() {
    // FR-017: a sub-floor configured value is clamped at the degrade gate so
    // ordinary files are never wrongly degraded.
    let settings = AppSettings {
        large_file_threshold: 1234,
        ..AppSettings::default()
    };
    let app = App::new(settings, None);
    assert_eq!(
        app.large_file_threshold(),
        AppSettings::min_large_file_threshold()
    );
    assert_eq!(app.large_file_threshold(), 65_536);
}

// ---------------------------------------------------------------------------
// Dropped-file handling (T018/T020, FR-002)
// ---------------------------------------------------------------------------

#[test]
fn drop_classification_distinguishes_ron_folder_and_other() {
    let ron = temp_ron("()\n", "drop_class");
    assert_eq!(classify_drop(&ron), DropDecision::OpenRon);
    let _ = std::fs::remove_file(&ron);

    let folder = std::env::temp_dir();
    assert_eq!(classify_drop(&folder), DropDecision::IgnoreFolder);

    let txt = std::env::temp_dir().join("ronin_drop_not_ron.txt");
    std::fs::write(&txt, b"hi").unwrap();
    assert_eq!(classify_drop(&txt), DropDecision::IgnoreNonRon);
    let _ = std::fs::remove_file(&txt);
}

#[test]
fn dropping_a_ron_file_opens_a_tab_without_notice() {
    let ron = temp_ron("Foo(x: 1)\n", "drop_ron");
    let mut app = App::new(AppSettings::default(), None);
    app.apply_drop(&ron);
    assert_eq!(app.document_count(), 1, "dropped .ron must open a tab");
    assert!(
        app.notices().is_empty(),
        "a successful drop-open must not push a notice"
    );
    let _ = std::fs::remove_file(&ron);
}

#[test]
fn dropping_a_non_ron_file_shows_auto_dismiss_info_notice_and_no_tab() {
    let txt = std::env::temp_dir().join(format!("ronin_drop_{}.txt", std::process::id()));
    std::fs::write(&txt, b"not ron").unwrap();
    let mut app = App::new(AppSettings::default(), None);
    app.apply_drop(&txt);
    assert_eq!(app.document_count(), 0, "non-.ron drop creates no tab");
    assert_eq!(app.notices().len(), 1, "an info notice must be shown");
    assert_eq!(
        app.notices()[0].kind,
        NoticeKind::Info,
        "ignored-drop notice is INFO (auto-dismiss), distinct from open-error"
    );
    let _ = std::fs::remove_file(&txt);
}

#[test]
fn dropping_a_folder_shows_info_notice_and_no_tab() {
    let folder = std::env::temp_dir();
    let mut app = App::new(AppSettings::default(), None);
    app.apply_drop(&folder);
    assert_eq!(app.document_count(), 0, "dropped folder creates no tab");
    assert_eq!(app.notices().len(), 1);
    assert_eq!(app.notices()[0].kind, NoticeKind::Info);
}
