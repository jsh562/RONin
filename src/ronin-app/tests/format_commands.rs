//! Format-command app-layer smoke tests (E005 Wave 2, T021, SC-001).
//!
//! Drives the non-rendering `App` formatting logic headlessly (calling
//! `App::format_document` / `App::format_selection` / the save path directly), per
//! the E003 test boundary: the `rfd` dialog and full-frame rendering are
//! manual/QC, but every buffer-mutating decision is exercised here.
//!
//! Coverage:
//! * Format Document on a messy-but-valid fixture reformats the buffer; a second
//!   Format is a no-op at the app layer (idempotent).
//! * Format on invalid / in-progress RON leaves the buffer byte-unchanged and
//!   pushes a persist-until-dismissed error notice (FR-005/FR-021).
//! * Format Selection reformats only the selected subtree, leaving the rest
//!   byte-unchanged (FR-002/FR-023).
//! * With format-on-save OFF (default) a save does not reformat; with it ON a save
//!   formats the buffer first (FR-006).
//! * No silent reformat: marking an edit (typing) never reformats the buffer
//!   (FR-009).

use std::path::PathBuf;

use ronin_app::app::{App, NoticeKind};
use ronin_app::settings::AppSettings;

fn app() -> App {
    App::new(AppSettings::default(), None)
}

/// A unique temp path for a save target (never actually relying on its prior
/// contents; the test reads back the bytes it wrote).
fn temp_ron(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("ronin_format_commands_test");
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir.join(format!("{name}-{}.ron", std::process::id()))
}

fn set_buffer(app: &mut App, text: &str) {
    let doc = app.active_document_mut().expect("active document");
    doc.buffer = text.to_string();
}

fn buffer(app: &App) -> String {
    app.active_document()
        .expect("active document")
        .buffer
        .clone()
}

fn has_error_notice(app: &App) -> bool {
    app.notices().iter().any(|n| n.kind == NoticeKind::Error)
}

// ---- Format Document: reformat + idempotence (SC-001) -----------------------

#[test]
fn format_document_reformats_then_is_idempotent() {
    let mut app = app();
    app.new_untitled();
    set_buffer(&mut app, "Foo(\nx:1,\n\n\n\ny:2\n)");

    app.format_document();
    let once = buffer(&app);
    assert_eq!(once, "Foo(\n    x: 1,\n\n    y: 2,\n)\n");
    assert!(!has_error_notice(&app), "first format must not error");

    // Second format on the now-canonical buffer is a byte-level no-op (idempotent
    // at the app layer): the buffer does not change.
    app.format_document();
    let twice = buffer(&app);
    assert_eq!(twice, once, "format must be idempotent at the app layer");
    assert!(
        !has_error_notice(&app),
        "idempotent re-format must not error"
    );
}

// ---- Format Document: invalid input is byte-unchanged + error notice --------

#[test]
fn format_invalid_ron_leaves_buffer_unchanged_and_errors() {
    let mut app = app();
    app.new_untitled();
    // Incomplete / in-progress RON: an unterminated list. The formatter no-ops.
    let original = "[1, 2";
    set_buffer(&mut app, original);

    app.format_document();

    assert_eq!(
        buffer(&app),
        original,
        "invalid input must leave the buffer byte-unchanged"
    );
    assert!(
        has_error_notice(&app),
        "a format no-op must surface a persist-until-dismissed error notice"
    );
}

#[test]
fn format_invalid_ron_does_not_mark_dirty() {
    let mut app = app();
    app.new_untitled();
    set_buffer(&mut app, "[1, 2");
    // A fresh untitled buffer with this content is dirty already (differs from the
    // empty saved baseline); re-baseline it so we can assert format added no edit.
    if let Some(doc) = app.active_document_mut() {
        doc.mark_saved();
    }
    assert!(!app.active_document().unwrap().dirty());

    app.format_document();

    assert!(
        !app.active_document().unwrap().dirty(),
        "a no-op format must not mark the document dirty"
    );
}

// ---- Format Selection: only the selected subtree changes --------------------

#[test]
fn format_selection_reformats_only_the_selected_subtree() {
    let mut app = app();
    app.new_untitled();
    // Two values inside a list; select only the inner messy struct `Foo(x:1)`.
    let original = "[Foo(x:1), [9,8,7]]";
    set_buffer(&mut app, original);

    // Select the `Foo(x:1)` substring as a char-offset range (the buffer is
    // all-ASCII, so char offset == byte offset here).
    let foo = "Foo(x:1)";
    let foo_start = original.find(foo).unwrap();
    let foo_end = foo_start + foo.len();
    if let Some(doc) = app.active_document_mut() {
        doc.cursor.selection = Some((foo_start, foo_end));
    }

    app.format_selection();

    let after = buffer(&app);
    // The selected struct is canonicalized (`x:1` -> `x: 1`); the rest of the line
    // (the `[9,8,7]` list and outer brackets) is byte-unchanged.
    assert_eq!(after, "[Foo(x: 1), [9,8,7]]");
    assert!(!has_error_notice(&app), "valid selection must not error");
}

#[test]
fn format_selection_on_invalid_subtree_is_unchanged_and_errors() {
    let mut app = app();
    app.new_untitled();
    let original = "[1, 2";
    set_buffer(&mut app, original);
    if let Some(doc) = app.active_document_mut() {
        // Select the whole (invalid) buffer.
        doc.cursor.selection = Some((0, original.chars().count()));
    }

    app.format_selection();

    assert_eq!(
        buffer(&app),
        original,
        "invalid selection must leave the buffer byte-unchanged"
    );
    assert!(
        has_error_notice(&app),
        "a selection no-op must surface an error notice"
    );
}

// ---- Format on save: OFF by default, ON formats first (FR-006) --------------

#[test]
fn save_does_not_reformat_when_format_on_save_off() {
    let mut app = app();
    app.new_untitled();
    let messy = "[1,2,3]";
    set_buffer(&mut app, messy);
    // Default settings: format_on_save is false.
    assert!(!app.formatting().format_on_save);

    let path = temp_ron("save_off");
    let idx = app.active_index().expect("active index");
    let saved = app.save_doc_to(idx, &path);
    assert!(saved, "save should succeed to a writable temp path");

    // The in-memory buffer is untouched (no reformat on save).
    assert_eq!(buffer(&app), messy);
    // And the bytes on disk are the un-formatted buffer.
    let on_disk = std::fs::read_to_string(&path).expect("read back");
    assert_eq!(on_disk, messy);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn save_reformats_first_when_format_on_save_on() {
    let mut app = app();
    app.new_untitled();
    let messy = "[1,2,3]";
    set_buffer(&mut app, messy);
    app.formatting_mut().format_on_save = true;

    let path = temp_ron("save_on");
    let idx = app.active_index().expect("active index");
    let saved = app.save_doc_to(idx, &path);
    assert!(saved, "save should succeed to a writable temp path");

    // The buffer was formatted before the write (the formatter adds a canonical
    // trailing newline to the in-memory buffer).
    assert_eq!(buffer(&app), "[1, 2, 3]\n");
    // The formatted bytes reached disk. A fresh untitled buffer carries
    // `had_trailing_newline: false`, so the byte-fidelity layer re-emits without a
    // trailing newline (FR-020) — the formatted *content* is what was written.
    let on_disk = std::fs::read_to_string(&path).expect("read back");
    assert_eq!(on_disk, "[1, 2, 3]");
    // The document is clean after a successful save.
    assert!(!app.active_document().unwrap().dirty());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn format_on_save_skips_and_still_saves_invalid_ron() {
    let mut app = app();
    app.new_untitled();
    let invalid = "[1, 2";
    set_buffer(&mut app, invalid);
    app.formatting_mut().format_on_save = true;

    let path = temp_ron("save_on_invalid");
    let idx = app.active_index().expect("active index");
    let saved = app.save_doc_to(idx, &path);

    // Format-on-save must NEVER block the save: invalid RON saves as-is.
    assert!(
        saved,
        "format-on-save must not block the save on invalid RON"
    );
    assert_eq!(
        buffer(&app),
        invalid,
        "buffer saved as-is on a format no-op"
    );
    let on_disk = std::fs::read_to_string(&path).expect("read back");
    assert_eq!(on_disk, invalid);
    let _ = std::fs::remove_file(&path);
}

// ---- No silent reformat: typing/edits never reformat (FR-009) ---------------

#[test]
fn marking_an_edit_never_reformats_the_buffer() {
    let mut app = app();
    app.new_untitled();
    let messy = "[1,2,3]";
    set_buffer(&mut app, messy);
    // Simulate the editor's per-edit hook (what `editor_view` calls on a keystroke).
    if let Some(doc) = app.active_document_mut() {
        doc.on_edit();
    }
    // The edit hook only bumps the reparse generation; it must not reformat.
    assert_eq!(
        buffer(&app),
        messy,
        "an edit must never reformat the buffer (no silent reformat)"
    );
}

// ---- No-op safety with no active document -----------------------------------

#[test]
fn format_with_no_document_is_harmless() {
    let mut app = app();
    assert!(app.active_document().is_none());
    app.format_document();
    app.format_selection();
    // No panic, no notices, no document created.
    assert!(app.active_document().is_none());
    assert!(app.notices().is_empty());
}
