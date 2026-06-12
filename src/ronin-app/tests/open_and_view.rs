//! US1 "open & view" headless smoke test (T021, FR-001).
//!
//! Drives the real open path (`fileio::open_path`) on a temp `.ron` fixture, then
//! lays the resulting document out through the renderer-free `egui_kittest`
//! [`Harness::new_ui`] driving the public [`editor_view`] widget, and asserts the
//! file's text is present in the rendered (AccessKit) tree. This exercises the
//! open → render path end-to-end without a GPU/wgpu backend.

use std::io::Write;

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;

use ronin_app::editor_view::editor_view;
use ronin_app::fileio::open_path;

/// `true` if any node in the rendered tree has a `value` containing `needle`.
///
/// Multiline `TextEdit` content is exposed as the AccessKit node *value*, so this
/// is how we assert the editor's text rendered.
fn node_value_contains(harness: &Harness<'_>, needle: &str) -> bool {
    // `query_all_by` (not `query_by`) because a `TextEdit` and its inner `TextRun`
    // both carry the value, so more than one node legitimately matches.
    harness
        .query_all_by(|node| node.value().is_some_and(|v| v.contains(needle)))
        .next()
        .is_some()
}

/// Write `contents` to a uniquely-named temp `.ron` file and return its path.
fn temp_ron(contents: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    let unique = format!(
        "ronin_test_{}_{}.ron",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    path.push(unique);
    let mut f = std::fs::File::create(&path).expect("create temp fixture");
    f.write_all(contents.as_bytes()).expect("write fixture");
    path
}

#[test]
fn open_renders_buffer_text_in_editor() {
    let fixture = temp_ron("Scene(name: \"hello-ronin\", value: 42)\n");

    // Real open path: reads bytes, validates UTF-8, builds the document.
    let mut doc = open_path(&fixture).expect("temp .ron must open cleanly");
    assert_eq!(
        doc.title(),
        fixture.file_name().unwrap().to_str().unwrap(),
        "opened tab title is the file name"
    );

    // Render the editor widget for the opened document and assert its text shows.
    // A multiline TextEdit exposes its content as the AccessKit node *value*
    // (not a label), so match on the value containing the fixture text.
    let mut harness = Harness::new_ui(move |ui| {
        let _ = editor_view(ui, &mut doc, false);
    });
    harness.run();

    assert!(
        node_value_contains(&harness, "hello-ronin"),
        "the opened file's text must render in the editor"
    );

    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn open_rejects_non_utf8_with_error() {
    // 0xFF 0xFE is not valid UTF-8; open must fail cleanly (no document).
    let mut path = std::env::temp_dir();
    path.push(format!("ronin_test_badutf8_{}.ron", std::process::id()));
    std::fs::write(&path, [0xFF, 0xFE, 0x00, 0x01]).expect("write bad fixture");

    let err = open_path(&path).expect_err("non-UTF-8 file must be rejected");
    assert_eq!(err.to_string(), "not valid UTF-8");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn empty_file_opens_as_empty_editable_buffer() {
    // FR-021: an empty file is a valid, editable, no-diagnostics state.
    let fixture = temp_ron("");
    let doc = open_path(&fixture).expect("empty .ron opens");
    assert_eq!(doc.buffer, "", "empty file yields an empty buffer");
    assert!(doc.diagnostics.is_empty(), "empty file has no diagnostics");
    assert!(!doc.dirty(), "freshly opened empty file is clean");
    let _ = std::fs::remove_file(&fixture);
}
