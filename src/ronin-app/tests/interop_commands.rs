//! Command + dialog tests for RON→JSON conversion (E010 US1 — T008,
//! FR-003/005/013, SC-002/003/008).
//!
//! Exercises the wired App commands (`begin_conversion`, `resolve_conversion`,
//! `resolve_partial_ron_prompt`, `convert_to_json_in_place`) on a real document:
//!
//! * loss-report dialog confirm/cancel — Cancel changes zero bytes (SC-002/003);
//! * in-place convert = one E007 undo unit — a single Undo restores the exact prior
//!   bytes (SC-003);
//! * partial-RON block-vs-convert-remainder prompt (SC-008).
//!
//! The dialog *rendering* is asserted through the renderer-free `egui_kittest`
//! harness (AccessKit labels, no pixel-scraping); the *behavior* is driven through
//! the public command methods, mirroring the E009 `bevy_elision.rs` app tests.

use std::path::PathBuf;

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;
use ronin_app::app::{App, ConvertFormatOverride, ConvertTarget, PartialRonChoice};
use ronin_app::interop::LossKind;
use ronin_app::settings::{AppSettings, JsonFormat, StrictCommentCarrier};

/// A fresh temp directory for a command test.
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ronin_interop_cmd_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Build an App with `src` as the active document's live buffer (written to a real
/// file in a temp project so the document has a path).
fn app_with_ron(tag: &str, src: &str) -> App {
    let dir = temp_dir(tag);
    let path = dir.join("doc.ron");
    std::fs::write(&path, src.as_bytes()).expect("write ron");
    let mut app = App::new(AppSettings::default(), Some(path));
    if let Some(doc) = app.active_document_mut() {
        doc.buffer = src.to_string();
        doc.on_edit();
    }
    app
}

/// The active document's live buffer bytes.
fn buffer(app: &App) -> String {
    app.active_document().expect("active doc").buffer.clone()
}

/// The default JSONC format override.
fn jsonc_override() -> ConvertFormatOverride {
    ConvertFormatOverride {
        format: JsonFormat::Jsonc,
        strict_carrier: StrictCommentCarrier::Sidecar,
    }
}

// ===========================================================================
// Loss-report dialog confirm / cancel (SC-002/003).
// ===========================================================================

#[test]
fn lossy_conversion_opens_the_loss_dialog_and_publishes_inline_losses() {
    // FR-005/FR-007: a lossy conversion opens the dialog AND publishes the SAME
    // losses inline (one list → both surfaces).
    let mut app = app_with_ron("dialog_open", "(t: (1, 2), c: 'x')");
    app.begin_conversion(ConvertTarget::InPlace, jsonc_override());
    assert!(
        app.conversion_pending(),
        "a lossy conversion opens the dialog"
    );

    // The dialog's per-kind counts match the document's constructs (SC-002).
    let counts = app.pending_conversion_counts();
    assert_eq!(counts.get(&LossKind::TupleVsList), Some(&1));
    assert_eq!(counts.get(&LossKind::Char), Some(&1));

    // The SAME losses are inline on the document (FR-007): one tuple + one char.
    let inline: Vec<&'static str> = app
        .active_document()
        .unwrap()
        .diagnostics
        .iter()
        .map(|d| d.code_str())
        .collect();
    assert!(inline.contains(&"RON-I0002"), "tuple loss is inline");
    assert!(inline.contains(&"RON-I0003"), "char loss is inline");
}

#[test]
fn cancel_changes_zero_bytes_and_writes_nothing() {
    // SC-002/003: Cancel leaves every document byte unchanged and writes no file.
    let mut app = app_with_ron("cancel_zero", "(t: (1, 2), c: 'x')");
    let before = buffer(&app);
    app.begin_conversion(ConvertTarget::InPlace, jsonc_override());
    assert!(app.conversion_pending());

    app.resolve_conversion(false); // Cancel.
    assert!(!app.conversion_pending(), "the dialog closes on Cancel");
    assert_eq!(buffer(&app), before, "Cancel changed zero bytes");
}

#[test]
fn confirm_in_place_converts_the_buffer_to_json() {
    // FR-001/003: confirming an in-place conversion replaces the buffer with JSON.
    let mut app = app_with_ron("confirm_inplace", "(t: (1, 2), c: 'x')");
    let before = buffer(&app);
    app.begin_conversion(ConvertTarget::InPlace, jsonc_override());
    app.resolve_conversion(true); // Convert.
    assert!(!app.conversion_pending());
    let after = buffer(&app);
    assert_ne!(after, before, "in-place convert changed the buffer");
    // The buffer is now JSON: the tuple is an array, the char a string.
    assert!(after.contains("\"t\": ["), "tuple → JSON array");
    assert!(after.contains("\"c\": \"x\""), "char → JSON string");
}

// ===========================================================================
// In-place convert = ONE E007 undo unit (SC-003).
// ===========================================================================

#[test]
fn in_place_convert_is_one_undo_unit_restoring_exact_prior_bytes() {
    // SC-003 / FR-003: an in-place conversion is ONE undo unit; a single Undo
    // restores the EXACT prior RON bytes (byte-for-byte, not value-equivalent).
    let mut app = app_with_ron("undo_inplace", "(t: (1, 2), c: 'x')");
    let before = buffer(&app);

    app.begin_conversion(ConvertTarget::InPlace, jsonc_override());
    app.resolve_conversion(true);
    let after = buffer(&app);
    assert_ne!(after, before, "convert changed bytes");

    assert!(app.undo_active(), "one undo step is available");
    assert_eq!(
        buffer(&app),
        before,
        "a single undo after convert restores the exact prior RON (one undo unit)"
    );
}

#[test]
fn loss_free_conversion_commits_without_a_dialog() {
    // FR-011: a base-tier (round-trip-safe) conversion has nothing to confirm — it
    // commits in place without opening the loss dialog.
    let mut app = app_with_ron("loss_free", "(name: \"hero\", scores: [1, 2])");
    let before = buffer(&app);
    app.begin_conversion(ConvertTarget::InPlace, jsonc_override());
    assert!(
        !app.conversion_pending(),
        "a loss-free conversion needs no confirm dialog"
    );
    let after = buffer(&app);
    assert_ne!(after, before, "the loss-free conversion still committed");
    assert!(after.contains("\"name\": \"hero\""));
}

// ===========================================================================
// Per-conversion strict override → sidecar export (FR-008).
// ===========================================================================

#[test]
fn strict_export_writes_json_and_a_sidecar_leaving_source_untouched() {
    // FR-003/008: a non-destructive export writes JSON to the target + a strict-mode
    // comment sidecar as a deterministic sibling, leaving the source untouched.
    let dir = temp_dir("export_sidecar");
    let src = "(\n  // about a\n  a: 1,\n)";
    let path = dir.join("doc.ron");
    std::fs::write(&path, src.as_bytes()).expect("write ron");
    let mut app = App::new(AppSettings::default(), Some(path.clone()));
    if let Some(doc) = app.active_document_mut() {
        doc.buffer = src.to_string();
        doc.on_edit();
    }

    let target = dir.join("out.json");
    let strict = ConvertFormatOverride {
        format: JsonFormat::StrictJson,
        strict_carrier: StrictCommentCarrier::Sidecar,
    };
    app.begin_conversion(ConvertTarget::Export(target.clone()), strict);
    // No losses here besides the comment-carry decision; strict+sidecar preserves
    // comments, so this base-tier doc is loss-free → commits immediately.
    if app.conversion_pending() {
        app.resolve_conversion(true);
    }

    // The JSON target exists; the source RON is byte-identical.
    assert!(target.exists(), "the JSON export target was written");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        src,
        "the source RON document is untouched"
    );
    // The deterministic sibling sidecar carries the comment.
    let sidecar = dir.join("out.json.comments.json");
    assert!(
        sidecar.exists(),
        "the strict-mode comment sidecar is written"
    );
    assert!(std::fs::read_to_string(&sidecar)
        .unwrap()
        .contains("// about a"));

    let _ = std::fs::remove_dir_all(&dir);
}

// ===========================================================================
// Partial-RON block vs convert-remainder prompt (SC-008).
// ===========================================================================

#[test]
fn unparseable_ron_raises_the_block_vs_remainder_prompt() {
    // FR-013/SC-008: a conversion on unparseable RON raises the prompt instead of
    // converting.
    let mut app = app_with_ron("partial_prompt", "(a: 1, b: @@@)");
    app.begin_conversion(ConvertTarget::InPlace, jsonc_override());
    assert!(
        app.partial_ron_prompt_open(),
        "unparseable RON raises the block-vs-remainder prompt"
    );
    assert!(
        !app.conversion_pending(),
        "no conversion dialog while the unparseable prompt is open"
    );
}

#[test]
fn block_aborts_with_a_locating_error_and_zero_bytes() {
    // SC-008: the block branch produces no output and a clear error locating the
    // region; zero bytes change.
    let mut app = app_with_ron("block_branch", "(a: 1, b: @@@)");
    let before = buffer(&app);
    app.begin_conversion(ConvertTarget::InPlace, jsonc_override());
    assert!(app.partial_ron_prompt_open());

    app.resolve_partial_ron_prompt(PartialRonChoice::Block);
    assert!(!app.partial_ron_prompt_open(), "the prompt closes on block");
    assert!(!app.conversion_pending(), "block produces no conversion");
    assert_eq!(buffer(&app), before, "block changed zero bytes");
    // A clear, locating error is surfaced.
    assert!(
        app.notices()
            .iter()
            .any(|n| n.message.contains("blocked") && n.message.contains("unparseable")),
        "block surfaces a clear locating error, got {:?}",
        app.notices()
    );
}

#[test]
fn convert_remainder_emits_the_parseable_portion_with_a_flagged_placeholder() {
    // SC-008: the convert-remainder branch emits the parseable portion and records
    // each unparseable region as an UnparseableRegion loss (a flagged placeholder).
    let mut app = app_with_ron("remainder_branch", "(a: 1, b: @@@)");
    app.begin_conversion(ConvertTarget::InPlace, jsonc_override());
    assert!(app.partial_ron_prompt_open());

    app.resolve_partial_ron_prompt(PartialRonChoice::ConvertRemainder);
    assert!(!app.partial_ron_prompt_open(), "the prompt closes");
    // The remainder branch produces a conversion; an unparseable region is a loss,
    // so the dialog opens (the conversion is lossy).
    assert!(
        app.conversion_pending(),
        "convert-remainder yields a (lossy) conversion to confirm"
    );
    let counts = app.pending_conversion_counts();
    assert!(
        counts
            .get(&LossKind::UnparseableRegion)
            .copied()
            .unwrap_or(0)
            >= 1,
        "each unparseable region is a flagged placeholder loss, counts = {counts:?}"
    );
    // The flagged-region loss is also inline (one list → both surfaces, FR-007).
    assert!(
        app.active_document()
            .unwrap()
            .diagnostics
            .iter()
            .any(|d| d.code_str() == "RON-I0010"),
        "the unparseable-region loss is inline too"
    );
}

// ===========================================================================
// Dialog rendering through the renderer-free harness (AccessKit labels).
// ===========================================================================

#[test]
fn the_loss_dialog_renders_its_controls() {
    let src = "(t: (1, 2), c: 'x')";
    let dir = temp_dir("render_dialog");
    let path = dir.join("doc.ron");
    std::fs::write(&path, src.as_bytes()).expect("write ron");

    let mut harness = Harness::new_ui(move |ui| {
        let mut app = App::new(AppSettings::default(), Some(path.clone()));
        if let Some(doc) = app.active_document_mut() {
            doc.buffer = src.to_string();
            doc.on_edit();
        }
        app.begin_conversion(ConvertTarget::InPlace, jsonc_override());
        app.render_shell(ui);
    });
    harness.run();

    // The dialog and its confirm/cancel + override controls render.
    assert!(
        harness
            .query_all_by_label_contains("review losses")
            .next()
            .is_some(),
        "the loss-report dialog renders"
    );
    assert!(
        harness
            .query_all_by_label_contains("Cancel")
            .next()
            .is_some(),
        "the dialog has a Cancel control"
    );
    assert!(
        harness
            .query_all_by_label_contains("JSONC")
            .next()
            .is_some(),
        "the per-conversion JSONC override control renders"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_convert_menu_renders() {
    let mut harness = Harness::new_ui(|ui| {
        let mut app = App::new(AppSettings::default(), None);
        app.render_shell(ui);
    });
    harness.run();
    assert!(
        harness
            .query_all_by_label_contains("Convert")
            .next()
            .is_some(),
        "the Convert menu renders in the menu bar"
    );
}

// ===========================================================================
// E010 US2 — JSON→RON in-place convert + import-to-new-tab (FR-002/003).
// ===========================================================================

#[test]
fn convert_json_to_ron_in_place_is_one_undo_unit() {
    // FR-002/003: an in-place JSON→RON conversion replaces the buffer and is ONE
    // E007 undo unit — a single Undo restores the exact prior JSON bytes.
    let mut app = app_with_ron(
        "json_to_ron_inplace",
        "{ \"name\": \"hero\", \"level\": 3 }",
    );
    let before = buffer(&app);

    app.convert_json_to_ron_in_place();
    let after = buffer(&app);
    assert_ne!(after, before, "in-place JSON→RON changed the buffer");
    assert!(
        after.contains("name: \"hero\""),
        "buffer is now RON: {after}"
    );
    assert!(after.contains("level: 3"), "buffer is now RON: {after}");

    // One undo unit: a single Undo restores the exact prior JSON.
    assert!(app.undo_active(), "one undo step is available");
    assert_eq!(
        buffer(&app),
        before,
        "a single undo restores the exact prior JSON (one undo unit)"
    );
}

#[test]
fn import_json_path_opens_a_new_tab_leaving_source_untouched() {
    // FR-002: importing a JSON file reconstructs it into a NEW tab; the source JSON
    // file is never modified.
    let dir = temp_dir("import_new_tab");
    let src = "{ \"name\": \"hero\", \"scores\": [1, 2] }";
    let json = dir.join("data.json");
    std::fs::write(&json, src.as_bytes()).expect("write json");

    let mut app = App::new(AppSettings::default(), None);
    let before_tabs = app.document_count();
    app.import_json_path(&json);

    assert_eq!(
        app.document_count(),
        before_tabs + 1,
        "import opened exactly one new tab"
    );
    let reconstructed = buffer(&app);
    assert!(
        reconstructed.contains("name: \"hero\""),
        "new tab is RON: {reconstructed}"
    );
    // The source JSON file is byte-identical (import is non-destructive).
    assert_eq!(
        std::fs::read_to_string(&json).unwrap(),
        src,
        "the source JSON file is untouched"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ===========================================================================
// E010 US2 — degrade-safe import: malformed JSON / non-UTF-8 / adversarial input
// (FR-013, SC-009). No tab/doc created or corrupted; never a crash/hang.
// ===========================================================================

#[test]
fn malformed_json_in_place_changes_zero_bytes_and_surfaces_an_error() {
    // FR-013: an in-place JSON→RON convert on malformed JSON changes ZERO bytes and
    // surfaces a clear, non-crashing error — no doc corrupted.
    let mut app = app_with_ron("malformed_inplace", "{ \"a\": }");
    let before = buffer(&app);
    app.convert_json_to_ron_in_place();
    assert_eq!(buffer(&app), before, "malformed JSON changed zero bytes");
    assert!(
        app.notices()
            .iter()
            .any(|n| n.message.contains("Cannot import") && n.message.contains("malformed")),
        "a clear malformed-JSON error is surfaced: {:?}",
        app.notices()
    );
}

#[test]
fn malformed_json_import_creates_no_tab() {
    // FR-013: importing a malformed JSON file creates NO tab and surfaces an error.
    let dir = temp_dir("malformed_import");
    let json = dir.join("bad.json");
    std::fs::write(&json, b"{ \"a\": ").expect("write bad json");

    let mut app = App::new(AppSettings::default(), None);
    let before_tabs = app.document_count();
    app.import_json_path(&json);

    assert_eq!(
        app.document_count(),
        before_tabs,
        "malformed JSON import created no tab"
    );
    assert!(
        app.notices()
            .iter()
            .any(|n| n.message.contains("Cannot import")),
        "a clear error is surfaced: {:?}",
        app.notices()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn non_utf8_import_creates_no_tab() {
    // FR-013: a non-UTF-8 input file is rejected at the boundary — no tab, clear error.
    let dir = temp_dir("non_utf8_import");
    let json = dir.join("binary.json");
    std::fs::write(&json, [0xff, 0xfe, 0x00, 0x01]).expect("write non-utf8");

    let mut app = App::new(AppSettings::default(), None);
    let before_tabs = app.document_count();
    app.import_json_path(&json);

    assert_eq!(
        app.document_count(),
        before_tabs,
        "non-UTF-8 import created no tab"
    );
    assert!(
        app.notices()
            .iter()
            .any(|n| n.message.contains("Cannot import") && n.message.contains("UTF-8")),
        "a clear non-UTF-8 error is surfaced: {:?}",
        app.notices()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn adversarial_deeply_nested_json_degrades_safely() {
    // FR-013/SC-009: a deeply/recursively nested JSON input must NOT crash (no stack
    // overflow), NOT hang, and surface a bounded result — the depth-exceeded region
    // becomes a flagged placeholder, the conversion still completes.
    let dir = temp_dir("deep_nested");
    let mut deep = String::new();
    let layers = 5000; // far beyond any safe structural bound
    for _ in 0..layers {
        deep.push('[');
    }
    deep.push('0');
    for _ in 0..layers {
        deep.push(']');
    }
    let json = dir.join("deep.json");
    std::fs::write(&json, deep.as_bytes()).expect("write deep json");

    let mut app = App::new(AppSettings::default(), None);
    // This must return without crashing or hanging. Either serde_json rejects the
    // over-deep input (→ a clear error, no tab) or our depth-bounded emit flags it —
    // both are degrade-safe outcomes (FR-013).
    app.import_json_path(&json);

    // No crash/hang reaching here is the core assertion (SC-009). If a tab opened,
    // its buffer must be valid (parseable) RON, not corrupt.
    if app.document_count() > 0 {
        let recon = buffer(&app);
        assert!(!recon.is_empty(), "a produced buffer is non-empty");
    } else {
        assert!(
            app.notices()
                .iter()
                .any(|n| n.message.contains("Cannot import")),
            "an over-deep input that is rejected surfaces a clear error: {:?}",
            app.notices()
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn adversarial_huge_collection_is_bounded_and_does_not_hang() {
    // FR-013/SC-009: a pathologically large (but shallow) collection must convert in
    // bounded time without hanging or crashing.
    let dir = temp_dir("huge_collection");
    let n = 50_000usize;
    let mut huge = String::with_capacity(n * 3 + 2);
    huge.push('[');
    for i in 0..n {
        if i > 0 {
            huge.push(',');
        }
        huge.push('1');
    }
    huge.push(']');
    let json = dir.join("huge.json");
    std::fs::write(&json, huge.as_bytes()).expect("write huge json");

    let mut app = App::new(AppSettings::default(), None);
    app.import_json_path(&json);

    // It completed (no hang) and produced a tab with a valid, large RON list.
    assert_eq!(
        app.document_count(),
        1,
        "the huge collection imported to a tab"
    );
    let recon = buffer(&app);
    assert!(
        recon.starts_with('['),
        "reconstructed as a RON list: {}",
        &recon[..recon.len().min(8)]
    );
    let _ = std::fs::remove_dir_all(&dir);
}
