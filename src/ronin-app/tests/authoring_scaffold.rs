//! Smart-authoring foundational scaffolding tests (E005 Wave 1, T004/T005).
//!
//! Drives the non-rendering `App` logic for the authoring surface:
//!
//! * **Authoring notices (T004, FR-024)** — `push_authoring_notice` routes through
//!   the existing dismissible-notice channel; error notices persist, info notices
//!   auto-dismiss, identical consecutive notices de-dupe, and it is never a
//!   blocking dialog.
//! * **Formatter settings + format commands (T005/T014/T015, FR-007/FR-023)** — the
//!   adjustable `FormattingConfig` controls mutate persisted settings (clamped),
//!   the Settings window toggles, and the now-live `format_document` /
//!   `format_selection` commands reformat the active buffer through the shell's
//!   single safe apply path (Wave 2). The deeper format-command behavior (idempotence,
//!   no-op safety, format-on-save) is covered by `tests/format_commands.rs`.

use ronin_app::app::{App, NoticeKind};
use ronin_app::settings::{AppSettings, BlankLinePolicy, FormattingConfig};

fn app() -> App {
    App::new(AppSettings::default(), None)
}

// ---- T004: authoring notices -------------------------------------------------

#[test]
fn authoring_error_notice_persists() {
    let mut app = app();
    app.push_authoring_notice(NoticeKind::Error, "selection has parse errors");
    assert_eq!(app.notices().len(), 1);
    assert_eq!(app.notices()[0].kind, NoticeKind::Error);
    assert_eq!(app.notices()[0].message, "selection has parse errors");
}

#[test]
fn authoring_info_notice_is_info_kind() {
    let mut app = app();
    app.push_authoring_notice(NoticeKind::Info, "already formatted");
    assert_eq!(app.notices().len(), 1);
    assert_eq!(app.notices()[0].kind, NoticeKind::Info);
}

#[test]
fn identical_consecutive_authoring_notices_dedupe() {
    let mut app = app();
    app.push_authoring_notice(NoticeKind::Info, "nothing to format");
    app.push_authoring_notice(NoticeKind::Info, "nothing to format");
    assert_eq!(
        app.notices().len(),
        1,
        "identical consecutive notices must de-dupe"
    );
    // A different message stacks.
    app.push_authoring_notice(NoticeKind::Info, "different message");
    assert_eq!(app.notices().len(), 2);
}

// ---- T005: formatter settings controls --------------------------------------

#[test]
fn formatting_config_defaults_are_exposed() {
    let app = app();
    assert_eq!(app.formatting().indent_width, 4);
    assert_eq!(
        app.formatting().blank_line_policy,
        BlankLinePolicy::Collapse
    );
    assert!(!app.formatting().format_on_save);
}

#[test]
fn formatting_controls_mutate_and_clamp() {
    let mut app = app();
    app.formatting_mut().set_indent_width(2);
    assert_eq!(app.formatting().indent_width, 2);
    // Clamp on the high end.
    app.formatting_mut().set_indent_width(1000);
    assert_eq!(
        app.formatting().indent_width,
        FormattingConfig::max_indent_width()
    );
    app.formatting_mut().blank_line_policy = BlankLinePolicy::Preserve;
    assert_eq!(
        app.formatting().blank_line_policy,
        BlankLinePolicy::Preserve
    );
    app.formatting_mut().format_on_save = true;
    assert!(app.formatting().format_on_save);
}

#[test]
fn settings_window_toggles() {
    let mut app = app();
    assert!(!app.settings_open());
    app.set_settings_open(true);
    assert!(app.settings_open());
    app.set_settings_open(false);
    assert!(!app.settings_open());
}

// ---- T014/T015: format commands are now live (Wave 2) -----------------------

#[test]
fn format_document_command_reformats_messy_buffer() {
    let mut app = app();
    app.new_untitled();
    // A valid-but-messy buffer: missing spacing the formatter canonicalizes.
    if let Some(doc) = app.active_document_mut() {
        doc.buffer = "[1,2,3]".to_string();
    }
    app.format_document();
    let after = app.active_document().map(|d| d.buffer.clone());
    // Wave 2: the command now reformats the buffer through the safe apply path.
    assert_eq!(
        after.as_deref(),
        Some("[1, 2, 3]\n"),
        "format_document must canonicalize the buffer"
    );
    // A successful reformat does not raise an error notice.
    assert!(
        app.notices().iter().all(|n| n.kind != NoticeKind::Error),
        "successful format must not raise an error notice"
    );
}

#[test]
fn format_selection_with_no_selection_falls_back_to_document() {
    let mut app = app();
    app.new_untitled();
    if let Some(doc) = app.active_document_mut() {
        doc.buffer = "Foo(x:1)".to_string();
        // No selection set → Format Selection falls back to Format Document.
        doc.cursor.selection = None;
    }
    app.format_selection();
    let after = app.active_document().map(|d| d.buffer.clone());
    assert_eq!(
        after.as_deref(),
        Some("Foo(x: 1)\n"),
        "format_selection with no selection formats the whole document"
    );
}
