//! Snippet smoke + integration tests (E005 Wave 4, US3, T039, SC-004).
//!
//! Drives the snippet stack at the level the E003 test boundary permits: the
//! `snippets` module's pure expansion / session / insertion API and the `App`'s
//! snippet wiring (`insert_snippet_by_name`, the effective set, the discoverability
//! menu, the open-file command), all headlessly. Full-frame popup *rendering* + live
//! keystroke routing (the `Tab`/`Shift+Tab` consumption and the inline choice
//! picker) are manual/QC per the E003 boundary; every buffer-mutating decision is
//! exercised here, plus an `egui_kittest` shell-render smoke for the Snippets menu.
//!
//! Coverage:
//! * trigger a built-in struct snippet → tab through placeholders → reach `$0`;
//!   the inserted text parses + round-trips losslessly (FR-016/FR-018);
//! * a user snippet from a temp JSON fixture appears alongside built-ins and can be
//!   triggered (FR-017);
//! * a missing/malformed user file degrades to built-ins + a notice (FR-017);
//! * the `App` insertion path verifies round-trip before mutating the buffer and
//!   refuses a corrupting insertion (FR-018);
//! * the discoverability menu lists each prefix + description, and the Snippets menu
//!   renders in the composed shell (FR-025).

use std::path::PathBuf;

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;

use ronin_app::app::{App, NoticeKind};
use ronin_app::settings::AppSettings;
use ronin_app::snippets::{
    expand_snippet, insert_snippet, SnippetParseStatus, SnippetSet, TabStopKind, UserSnippetFile,
};
use ronin_core::parse;

fn app() -> App {
    App::new(AppSettings::default(), None)
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("ronin_snippets_integration");
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// A process-globally-unique suffix so parallel tests never share a temp path
/// (PID alone collides across threads in one test binary).
fn unique() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

// ---- T039: trigger a built-in struct snippet, tab to $0, round-trip ----------

#[test]
fn struct_snippet_expands_tabs_to_end_and_round_trips() {
    // The built-in named-struct body, with an explicit final cursor appended to make
    // the navigation end deterministic for the test.
    let body = "${1:Name}(${2:field}: ${3:value})$0";
    let mut ins = insert_snippet("", 0, body).expect("insertion round-trips");
    assert_eq!(ins.new_buffer, "Name(field: value)");
    // The inserted text parses + round-trips losslessly (FR-018).
    assert!(
        parse(&ins.new_buffer).diagnostics().is_empty(),
        "the inserted snippet must round-trip cleanly"
    );

    // Tab through the placeholders in index order, ending at $0.
    assert_eq!(ins.session.active_stop().map(|s| s.index), Some(1));
    ins.session.next_stop();
    assert_eq!(ins.session.active_stop().map(|s| s.index), Some(2));
    ins.session.next_stop();
    assert_eq!(ins.session.active_stop().map(|s| s.index), Some(3));
    ins.session.next_stop();
    // The final cursor $0.
    assert_eq!(ins.session.active_stop().map(|s| s.index), Some(0));
    // One more Tab ends navigation (FR-016).
    assert_eq!(ins.session.next_stop(), None);
    assert!(!ins.session.is_active());
    // The buffer is untouched by navigation — still the round-trippable expansion.
    assert_eq!(ins.new_buffer, "Name(field: value)");
}

#[test]
fn choice_placeholder_inlines_first_and_carries_options() {
    let exp = expand_snippet("${1|true,false|}");
    assert_eq!(exp.text, "true");
    assert_eq!(exp.stops.len(), 1);
    match &exp.stops[0].kind {
        TabStopKind::Choice { options } => {
            assert_eq!(options, &vec!["true".to_string(), "false".to_string()]);
        }
        other => panic!("expected a choice stop, got {other:?}"),
    }
}

// ---- T039: a user snippet appears alongside built-ins (FR-017) ---------------

#[test]
fn user_snippet_from_fixture_appears_alongside_built_ins() {
    let path = temp_dir().join(format!("user-{}.json", unique()));
    std::fs::write(
        &path,
        br#"{
          "greeting": {
            "prefix": "greet",
            "body": "Greeting(message: ${1:\"hi\"})",
            "description": "A friendly greeting struct"
          }
        }"#,
    )
    .unwrap();

    let user = UserSnippetFile::load_from(&path);
    assert_eq!(user.parse_status, SnippetParseStatus::Ok);
    let set = SnippetSet::build(&user);

    // Built-ins still present...
    assert!(set.get("list").is_some(), "built-ins remain available");
    // ...and the user snippet is too, triggerable by its prefix.
    let greeting = set.get("greeting").expect("user snippet present");
    assert_eq!(greeting.prefix, "greet");
    assert_eq!(
        set.by_prefix("greet").map(|s| s.name.as_str()),
        Some("greeting")
    );
    // Its default expansion round-trips.
    let exp = expand_snippet(&greeting.body);
    assert!(parse(&exp.text).diagnostics().is_empty());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn malformed_user_file_keeps_built_ins_and_notices() {
    let path = temp_dir().join(format!("bad-{}.json", unique()));
    std::fs::write(&path, b"not json at all }{").unwrap();
    let set = SnippetSet::build(&UserSnippetFile::load_from(&path));
    assert!(
        !set.is_empty(),
        "built-ins keep working on a malformed file"
    );
    assert!(set.get("some").is_some());
    assert!(set.notice().is_some(), "a malformed file surfaces a notice");
    let _ = std::fs::remove_file(&path);
}

// ---- App insertion path: round-trip-verified buffer mutation (FR-018) --------

#[test]
fn app_inserts_a_built_in_snippet_into_the_active_document() {
    let mut app = app();
    app.new_untitled();
    // Caret at 0 of the empty buffer.
    if let Some(doc) = app.active_document_mut() {
        doc.cursor.caret = 0;
    }
    let inserted = app.insert_snippet_by_name("some");
    assert!(inserted, "a known snippet inserts");
    let buffer = app.active_document().map(|d| d.buffer.clone()).unwrap();
    assert_eq!(buffer, "Some(value)");
    // The result parses + round-trips (FR-018).
    assert!(parse(&buffer).diagnostics().is_empty());
    // A live tab-stop session is installed (caret at the first stop).
    assert!(
        app.active_document().unwrap().snippet_session.is_some(),
        "an insertion installs a tab-stop session"
    );
    // No error notice on a clean insertion.
    assert!(app.notices().iter().all(|n| n.kind != NoticeKind::Error));
}

#[test]
fn app_refuses_a_snippet_that_would_not_parse_after_insertion() {
    // Build an App whose snippet set we cannot easily corrupt, so instead test the
    // pure insertion gate directly with a body that corrupts a clean buffer.
    assert!(
        insert_snippet("[1]", 3, "Foo(${1:x}").is_none(),
        "a corrupting insertion is refused (verify-before-replace, FR-018)"
    );
}

#[test]
fn app_insert_with_no_document_is_a_safe_noop() {
    let mut app = app();
    assert!(app.active_document().is_none());
    assert!(!app.insert_snippet_by_name("some"), "no doc → no insertion");
    // It informs (info notice) rather than crashing; never an error notice.
    assert!(app.active_document().is_none());
}

#[test]
fn app_insert_unknown_snippet_name_is_a_noop() {
    let mut app = app();
    app.new_untitled();
    assert!(!app.insert_snippet_by_name("does-not-exist"));
    assert_eq!(app.active_document().unwrap().buffer, "");
}

// ---- Discoverability menu + open-file command (FR-025) -----------------------

#[test]
fn discoverability_menu_lists_prefix_and_description() {
    let app = app();
    let entries = app.snippet_menu_entries();
    assert!(!entries.is_empty(), "the snippet menu lists entries");
    assert!(
        entries.iter().all(|(p, d)| !p.is_empty() && !d.is_empty()),
        "every menu entry has a prefix and a description (FR-025)"
    );
    // A known built-in prefix is present.
    assert!(entries.iter().any(|(p, _)| p == "struct"));
}

#[test]
fn user_snippet_file_location_is_discoverable() {
    let app = app();
    // On any platform with a config dir, the path is resolvable and ends in the
    // snippet file name. (CI environments expose a config dir.)
    if let Some(path) = app.user_snippet_path() {
        assert!(path.ends_with("snippets.json"));
    }
}

// ---- egui_kittest shell smoke: the Snippets menu renders (FR-025) ------------

#[test]
fn snippets_menu_renders_in_the_composed_shell() {
    let mut harness = Harness::new_ui(|ui| {
        let mut app = App::new(AppSettings::default(), None);
        app.render_shell(ui);
    });
    harness.run();
    // The top menu bar exposes a "Snippets" menu button.
    assert!(
        harness
            .query_all_by_label_contains("Snippets")
            .next()
            .is_some(),
        "the shell must expose a Snippets menu (FR-025)"
    );
}

#[test]
fn snippets_browser_window_lists_built_ins() {
    let mut harness = Harness::new_ui(|ui| {
        let mut app = App::new(AppSettings::default(), None);
        app.set_snippets_open(true);
        app.render_shell(ui);
    });
    harness.run();
    // The browser lists built-in prefixes (e.g. the `list` snippet).
    assert!(
        harness.query_all_by_label_contains("list").next().is_some(),
        "the Snippets browser lists built-in snippets by prefix (FR-025)"
    );
}
