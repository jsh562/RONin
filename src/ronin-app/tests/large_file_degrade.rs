//! Large-file degrade reuse for E005 authoring (Wave 5, T041 — COMPLETES FR-026).
//!
//! Past E003's `large_file_threshold` the always-on E005 intelligence (the
//! structural completion popup) must degrade on the **same** signal E003 already
//! uses for highlighting / squiggles — the document being `oversize` — and reuse
//! E003's existing non-blocking degrade indicator + wording (no separate E005
//! message). The explicit Format commands are one-shot, verify-before-replace
//! actions, not per-frame intelligence, so they stay available on an oversize
//! document (the E003-consistent choice: degrade the always-on layer, not on-demand
//! commands).
//!
//! Per the E003 test boundary, full-frame rendering is manual/QC; the gating
//! *decision* ([`completion_enabled`]) and the Format-command behavior on an
//! oversize document are exercised here headlessly.

use ronin_app::app::{App, NoticeKind};
use ronin_app::document::EditorDocument;
use ronin_app::editor_view::completion_enabled;
use ronin_app::settings::AppSettings;

// ---- completion is gated on the E003 oversize signal (FR-026) ----------------

#[test]
fn completion_suppressed_when_oversize() {
    // Oversize ⇒ completion off (mirrors highlighting/squiggles suppression).
    assert!(
        !completion_enabled(true, false),
        "completion must be suppressed on an oversize file (E003 degrade reuse)"
    );
    // Not oversize ⇒ completion on (when no snippet session is active).
    assert!(
        completion_enabled(false, false),
        "completion runs normally below the large-file threshold"
    );
}

#[test]
fn completion_also_suppressed_during_snippet_session() {
    // While a snippet tab-stop session is active, completion is off so `Tab`
    // unambiguously drives snippet navigation — independent of oversize.
    assert!(!completion_enabled(false, true));
    assert!(!completion_enabled(true, true));
}

#[test]
fn oversize_decision_matches_e003_threshold() {
    // The gating signal is exactly `EditorDocument::oversize(threshold)` — the same
    // predicate E003 uses for highlighting/squiggles. A buffer past the threshold is
    // oversize (strict greater-than), so completion is gated off for it.
    let mut doc = EditorDocument::new_untitled(1);
    doc.buffer = "x".repeat(1_000);
    let threshold = 100u64;
    assert!(doc.oversize(threshold), "1000 bytes > 100 threshold");
    assert!(
        !completion_enabled(doc.oversize(threshold), doc.snippet_session.is_some()),
        "an oversize document gates completion off via the E003 signal"
    );

    // A small buffer under the threshold is not oversize, so completion runs.
    doc.buffer = "x".repeat(10);
    assert!(!doc.oversize(threshold));
    assert!(completion_enabled(
        doc.oversize(threshold),
        doc.snippet_session.is_some()
    ));
}

// ---- Format commands stay available on an oversize document (FR-026) ---------

/// The smallest effective large-file threshold (64 KiB; the app floors any lower
/// configured value to this). A test buffer must exceed it to be `oversize`.
fn min_threshold() -> u64 {
    AppSettings::min_large_file_threshold()
}

/// Settings whose large-file threshold is the floored minimum (64 KiB), so a
/// modestly-sized test buffer is enough to push a document `oversize`.
fn min_threshold_settings() -> AppSettings {
    let mut s = AppSettings::default();
    // Any value below the minimum is floored to it by the settings layer.
    s.set_large_file_threshold(1);
    s
}

/// A valid-but-messy RON list whose byte length exceeds `min`, so the document is
/// `oversize` once opened. The list is `[1,2,3,1,2,3,...]` with no canonical
/// spacing, so the formatter has work to do.
fn oversize_valid_buffer() -> String {
    // Each "1,2,3," chunk is 6 bytes; ~12k chunks comfortably exceed 64 KiB.
    let repeats = (min_threshold() as usize / 6) + 100;
    let mut s = String::with_capacity(repeats * 6 + 2);
    s.push('[');
    for _ in 0..repeats {
        s.push_str("1,2,3,");
    }
    s.push(']');
    s
}

#[test]
fn format_document_still_works_on_oversize_doc() {
    let mut app = App::new(min_threshold_settings(), None);
    app.new_untitled();

    let messy = oversize_valid_buffer();
    if let Some(doc) = app.active_document_mut() {
        doc.buffer = messy.clone();
    }
    let threshold = app.large_file_threshold();
    assert!(
        app.active_document().unwrap().oversize(threshold),
        "the test buffer must be oversize ({} bytes > {threshold})",
        messy.len()
    );

    // Format is an explicit command; it still reformats an oversize document.
    app.format_document();
    let after = app.active_document().unwrap().buffer.clone();
    assert_ne!(
        after, messy,
        "format must still run on an oversize document"
    );
    assert!(
        after.contains("1, 2, 3"),
        "format produced canonical spacing on the oversize buffer (head: {:?})",
        &after[..after.char_indices().nth(40).map_or(after.len(), |(b, _)| b)]
    );
    assert!(
        app.notices().iter().all(|n| n.kind != NoticeKind::Error),
        "a successful format on an oversize doc must not raise an error notice"
    );
}

#[test]
fn format_on_oversize_invalid_doc_is_byte_unchanged_and_errors() {
    // Even oversize, an invalid buffer is left byte-unchanged by the formatter's
    // no-op-on-failure path, with the standard format-skip error notice — the
    // oversize state changes nothing about that contract.
    let mut app = App::new(min_threshold_settings(), None);
    app.new_untitled();
    // An unterminated list (invalid) larger than the threshold.
    let repeats = (min_threshold() as usize / 2) + 100;
    let invalid = format!("[{}", "1,".repeat(repeats));
    if let Some(doc) = app.active_document_mut() {
        doc.buffer = invalid.clone();
    }
    let threshold = app.large_file_threshold();
    assert!(app.active_document().unwrap().oversize(threshold));

    app.format_document();
    assert_eq!(
        app.active_document().unwrap().buffer,
        invalid,
        "invalid oversize buffer must be byte-unchanged"
    );
    assert!(
        app.notices().iter().any(|n| n.kind == NoticeKind::Error),
        "a format no-op surfaces a persist-until-dismissed error notice"
    );
}

// ---- E006 T040: type validation degrades on the SAME oversize signal -----------
//
// E003 disables highlighting/squiggles once a document is `oversize` past
// `effective_large_file_threshold()`. E006's type validation MUST degrade on
// exactly that signal: an oversize bound document runs structural-only (zero type
// diagnostics, no off-frame validation work), identical in spirit to how an
// oversize document still parses but renders no squiggles. A same-binding document
// UNDER the threshold still validates — proving the gate. The structural behavior
// (parse + structural diagnostics) is unchanged either way. Driven through the real
// `App` off-frame worker round-trip, mirroring `config_trust.rs`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Create a unique temp project directory for a degrade test (fresh each run).
fn temp_degrade_project(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ronin_degrade_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp project dir");
    dir
}

/// Write a project `.ronin/bindings.json` mapping `**/*.ron` to `Entity` from an
/// in-project schema (so containment accepts the source). `Entity` requires an
/// integer `id`, so a string `id` is a type mismatch the validator WOULD flag.
fn write_entity_binding(project: &std::path::Path) {
    let schema_path = project.join("entity.schema.json");
    let schema = br#"{
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": { "Entity": { "type": "object",
            "properties": { "id": { "type": "integer" } },
            "required": ["id"], "additionalProperties": true } }
    }"#;
    std::fs::write(&schema_path, schema).expect("write entity schema");

    let ronin = project.join(".ronin");
    std::fs::create_dir_all(&ronin).expect("create .ronin");
    let escaped = schema_path.display().to_string().replace('\\', "\\\\");
    let json = format!(
        r#"{{ "rules": [ {{ "pattern": "**/*.ron", "type_name": "Entity",
            "type_source": {{ "SchemaFile": "{escaped}" }} }} ], "version": 1 }}"#
    );
    std::fs::write(ronin.join("bindings.json"), json.as_bytes()).expect("write bindings");
}

/// Settings whose large-file threshold is the floored minimum (64 KiB), so a
/// modestly-sized test buffer is enough to push a document `oversize`. (Named
/// distinctly from the file's existing `min_threshold_settings` to avoid a clash.)
fn min_threshold_app_settings() -> AppSettings {
    let mut s = AppSettings::default();
    s.set_large_file_threshold(1); // floored to 64 KiB by the settings layer
    s
}

/// Drive the App's real off-frame worker to completion for the active document.
fn drive_app_reparse(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if app.poll_documents() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("App reparse did not land within timeout");
        }
        std::thread::yield_now();
    }
}

/// Count `ronin-types` (type) diagnostics on the active document.
fn active_type_diag_count(app: &App) -> usize {
    app.active_document()
        .map(|d| {
            d.diagnostics
                .iter()
                .filter(|v| v.code.source() == "ronin-types")
                .count()
        })
        .unwrap_or(0)
}

#[test]
fn oversize_bound_document_runs_no_type_validation() {
    // An oversize document bound to a type produces ZERO type diagnostics: type
    // validation degrades on E003's oversize signal exactly like squiggles. The
    // wrong-type `id` value WOULD flag if validated, so zero findings proves the
    // off-frame validation was suppressed (not merely absent for lack of a binding).
    let project = temp_degrade_project("oversize_bound");
    write_entity_binding(&project);

    // A structurally-valid RON struct whose `id` is a string (type mismatch vs the
    // required integer), padded with a big valid list so it exceeds the 64 KiB
    // floor. `additionalProperties: true` lets the `pad` field through structurally.
    let min = AppSettings::min_large_file_threshold() as usize;
    let pad_chunks = (min / 6) + 100; // each "1,2,3," chunk is 6 bytes
    let mut buf = String::with_capacity(pad_chunks * 6 + 64);
    buf.push_str("(id: \"oops\", pad: [");
    for _ in 0..pad_chunks {
        buf.push_str("1,2,3,");
    }
    buf.push_str("])\n");
    let doc_path = project.join("big.ron");
    std::fs::write(&doc_path, buf.as_bytes()).expect("write oversize doc");

    let mut app = App::new(min_threshold_app_settings(), Some(doc_path));
    drive_app_reparse(&mut app);

    let threshold = app.large_file_threshold();
    let doc = app.active_document().expect("the doc is open");
    assert!(
        doc.oversize(threshold),
        "the test buffer must be oversize ({} bytes > {threshold})",
        doc.buffer.len()
    );
    // The display binding is still meaningful (the indicator shows the intended
    // type) even though validation is degraded — `binding` is kept, only the
    // worker-facing `bound_type` is suppressed.
    assert!(
        doc.binding.is_bound(),
        "the active-binding indicator still shows the intended type when oversize"
    );
    assert!(
        doc.validation_suppressed,
        "an oversize document degrades type validation (E003-consistent)"
    );
    // The whole point: zero type diagnostics despite the wrong-type value.
    assert_eq!(
        active_type_diag_count(&app),
        0,
        "an oversize bound document must run NO type validation (zero type diagnostics)"
    );

    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn under_threshold_same_binding_document_does_validate() {
    // The companion control: the SAME binding on an UNDER-threshold document DOES
    // validate — the wrong-type `id` surfaces a type diagnostic. This proves the
    // oversize gate (above) is what suppresses validation, not the binding itself.
    let project = temp_degrade_project("under_threshold");
    write_entity_binding(&project);

    // A tiny (well under 64 KiB) doc with the same wrong-type `id`.
    let doc_path = project.join("small.ron");
    std::fs::write(&doc_path, b"(id: \"oops\")\n").expect("write small doc");

    let mut app = App::new(min_threshold_app_settings(), Some(doc_path));
    drive_app_reparse(&mut app);

    let threshold = app.large_file_threshold();
    let doc = app.active_document().expect("the doc is open");
    assert!(
        !doc.oversize(threshold),
        "the control buffer must be UNDER the threshold ({} bytes <= {threshold})",
        doc.buffer.len()
    );
    assert!(
        !doc.validation_suppressed,
        "an under-threshold document does NOT degrade type validation"
    );
    assert!(
        doc.binding.is_bound(),
        "the under-threshold document is bound to the same Entity type"
    );
    assert!(
        active_type_diag_count(&app) >= 1,
        "an under-threshold bound document with a wrong-type value MUST validate \
         (at least one type diagnostic), proving the oversize gate is the cause of \
         the degrade above; got {:?}",
        app.active_document().map(|d| d.diagnostics.clone())
    );

    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn editing_an_oversize_doc_below_threshold_resumes_validation() {
    // Resume case: a document that starts oversize (validation degraded) and is then
    // edited DOWN below the threshold resumes type validation on the next reparse —
    // mirroring how E003 re-enables squiggles once a document is no longer oversize.
    let project = temp_degrade_project("resume");
    write_entity_binding(&project);

    // Start oversize with the wrong-type `id`.
    let min = AppSettings::min_large_file_threshold() as usize;
    let pad_chunks = (min / 6) + 100;
    let mut buf = String::with_capacity(pad_chunks * 6 + 64);
    buf.push_str("(id: \"oops\", pad: [");
    for _ in 0..pad_chunks {
        buf.push_str("1,2,3,");
    }
    buf.push_str("])\n");
    let doc_path = project.join("shrink.ron");
    std::fs::write(&doc_path, buf.as_bytes()).expect("write oversize doc");

    let mut app = App::new(min_threshold_app_settings(), Some(doc_path));
    drive_app_reparse(&mut app);
    assert_eq!(
        active_type_diag_count(&app),
        0,
        "oversize ⇒ no type validation (degraded)"
    );

    // Edit the buffer down to a tiny, still-wrong-type document. The edit helper
    // mirrors the real per-frame edit path: it reconciles the oversize degrade flag
    // against the now-small buffer before requesting the reparse, so the bound type
    // is shipped again and validation runs.
    app.replace_active_buffer_for_test("(id: \"oops\")\n");
    drive_app_reparse(&mut app);

    let threshold = app.large_file_threshold();
    let doc = app.active_document().expect("the doc is open");
    assert!(
        !doc.oversize(threshold),
        "the doc was edited down below the threshold"
    );
    assert!(
        active_type_diag_count(&app) >= 1,
        "editing an oversize doc below the threshold resumes type validation \
         (the wrong-type value flags again); got {:?}",
        app.active_document().map(|d| d.diagnostics.clone())
    );

    let _ = std::fs::remove_dir_all(&project);
}
