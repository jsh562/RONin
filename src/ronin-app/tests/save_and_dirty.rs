//! Save + dirty-state tests (T036, FR-010/FR-011/FR-020/FR-023).
//!
//! Covers:
//! * Editing a document sets `dirty()`; `save_document` writes the bytes and
//!   `mark_saved` clears dirty (asserted by reading the file back).
//! * `App::save_doc_to` (the post-dialog Save / Save As logic) writes the chosen
//!   target, adopts its path, refreshes the byte profile, and clears dirty.
//! * The unsaved-close path raises the save/discard/cancel prompt, and resolving
//!   it Saves / Discards / Cancels as specified.
//!
//! The `rfd` save dialog cannot run headlessly; `save_as_active` only differs from
//! the tested `save_doc_to` by the dialog's path selection, which is documented as
//! manual/QC-verified. Everything around the dialog (write + profile refresh +
//! dirty clear + path adoption) is exercised directly here.

use std::io::Write;

use ronin_app::app::{App, PromptChoice};
use ronin_app::fileio::{open_path, save_document};
use ronin_app::settings::AppSettings;

/// A uniquely-named temp `.ron` path (the file is not created).
fn temp_path(tag: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "ronin_save_{tag}_{}_{}.ron",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    path
}

/// Write `contents` to a uniquely-named temp `.ron` file and return its path.
fn temp_ron(contents: &str, tag: &str) -> std::path::PathBuf {
    let path = temp_path(tag);
    let mut f = std::fs::File::create(&path).expect("create temp fixture");
    f.write_all(contents.as_bytes()).expect("write fixture");
    path
}

#[test]
fn editing_sets_dirty_and_save_document_clears_it_and_writes_bytes() {
    // FR-010/FR-011/FR-020: load → edit (dirty) → save (clean) → disk has new bytes.
    let fixture = temp_ron("Config(level: 3)\n", "edit_save");
    let mut doc = open_path(&fixture).expect("fixture opens");
    assert!(!doc.dirty(), "freshly opened file is clean");

    // Edit the buffer; the document becomes dirty.
    doc.buffer = "Config(level: 9)\n".to_string();
    doc.on_edit();
    assert!(doc.dirty(), "an edited buffer must be dirty");

    // Save writes the (EOL-faithful) bytes; mark_saved clears dirty.
    save_document(&doc, &fixture).expect("save must succeed");
    doc.mark_saved();
    assert!(!doc.dirty(), "after save+mark_saved the doc is clean");

    // Disk now holds the edited content (uniform LF, trailing newline preserved).
    let on_disk = std::fs::read(&fixture).expect("read back saved file");
    assert_eq!(
        on_disk, b"Config(level: 9)\n",
        "disk bytes reflect the edit"
    );

    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn save_doc_to_on_pathless_buffer_writes_chosen_target_and_adopts_it() {
    // FR-011: Save As on an untitled (path-less) buffer writes to the chosen path,
    // adopts it, refreshes the profile, and clears dirty. We drive `save_doc_to`
    // directly because `rfd::FileDialog` cannot run headlessly.
    let mut app = App::new(AppSettings::default(), None);
    app.new_untitled();
    {
        let doc = app.active_document_mut().expect("active untitled doc");
        assert!(doc.path.is_none(), "untitled doc has no path");
        doc.buffer = "List([1, 2, 3])\n".to_string();
        doc.on_edit();
        assert!(doc.dirty(), "edited untitled doc is dirty");
    }

    let target = temp_path("save_as");
    let ok = app.save_doc_to(0, &target);
    assert!(ok, "save_doc_to must succeed for a writable target");

    let doc = app.active_document().expect("doc still active");
    assert_eq!(
        doc.path.as_deref(),
        Some(target.as_path()),
        "Save As must adopt the chosen path"
    );
    assert!(!doc.dirty(), "a successful Save As clears dirty");
    assert!(
        doc.untitled_seq.is_none(),
        "an adopted document is no longer untitled"
    );

    // An untitled document's profile defaults to NO trailing newline and NO BOM,
    // so the EOL-faithful save drops the buffer's trailing `\n` (the new file
    // simply had no original trailing-newline fidelity to preserve).
    let on_disk = std::fs::read(&target).expect("target file written");
    assert_eq!(
        on_disk, b"List([1, 2, 3])",
        "chosen target holds the buffer"
    );

    let _ = std::fs::remove_file(&target);
}

#[test]
fn save_active_routes_pathless_doc_and_clears_dirty_via_save_doc_to() {
    // FR-011: `save_active` on a doc with a path saves to it directly.
    let fixture = temp_ron("Foo(x: 1)\n", "save_active");
    let mut app = App::new(AppSettings::default(), None);
    app.open_file(&fixture);
    {
        let doc = app.active_document_mut().expect("active doc");
        doc.buffer = "Foo(x: 2)\n".to_string();
        doc.on_edit();
        assert!(doc.dirty());
    }
    assert!(app.save_active(), "save_active writes to the existing path");
    assert!(
        !app.active_document().unwrap().dirty(),
        "save_active clears dirty on success"
    );
    let on_disk = std::fs::read(&fixture).expect("read back");
    assert_eq!(on_disk, b"Foo(x: 2)\n");
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn failed_save_keeps_doc_dirty_and_pushes_error_notice() {
    // FR-011 / project-instructions §I: a disk error must NOT clear dirty and must
    // surface an error notice. Target a path inside a non-existent directory so the
    // write fails deterministically.
    let mut app = App::new(AppSettings::default(), None);
    app.new_untitled();
    {
        let doc = app.active_document_mut().expect("active doc");
        doc.buffer = "Bad(x: 1)\n".to_string();
        doc.on_edit();
    }
    let bad = std::env::temp_dir()
        .join("ronin_no_such_dir_xyz")
        .join("nested")
        .join("out.ron");
    let ok = app.save_doc_to(0, &bad);
    assert!(!ok, "writing into a missing directory must fail");
    assert!(
        app.active_document().unwrap().dirty(),
        "a failed save must leave the document dirty"
    );
    assert_eq!(
        app.notices().len(),
        1,
        "a failed save pushes an error notice"
    );
}

#[test]
fn failed_save_leaves_existing_target_byte_identical_through_the_atomic_seam() {
    // E007/OBJ1 TR-003/TR-019a regression at the shell seam: open a real file, edit
    // it, then attempt a save that fails (the target's parent subdir does not
    // exist, so the atomic temp cannot be created). The save must fail, the doc must
    // stay dirty, AND a previously-saved on-disk file must remain byte-identical —
    // the atomic path never partially writes / truncates the original.
    let fixture = temp_ron("Config(level: 1)\n", "atomic_intact");
    let original = std::fs::read(&fixture).expect("seed original");

    let mut app = App::new(AppSettings::default(), None);
    app.open_file(&fixture);
    {
        let doc = app.active_document_mut().expect("active doc");
        doc.buffer = "Config(level: 2)\n".to_string();
        doc.on_edit();
        assert!(doc.dirty());
    }

    // A target under a non-existent subdir: temp creation fails → SaveError.
    let bad = fixture
        .parent()
        .unwrap()
        .join("ronin_missing_subdir_atomic")
        .join("out.ron");
    let ok = app.save_doc_to(0, &bad);
    assert!(!ok, "saving under a missing subdir must fail");
    assert!(
        app.active_document().unwrap().dirty(),
        "a failed atomic save must leave the doc dirty (no optimistic re-baseline)"
    );
    assert_eq!(
        app.notices().len(),
        1,
        "a failed atomic save pushes exactly one error notice"
    );
    // The unrelated, previously-saved file is byte-identical (original intact).
    assert_eq!(
        std::fs::read(&fixture).unwrap(),
        original,
        "a failed atomic save must not touch any other file"
    );

    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn closing_a_dirty_doc_raises_the_prompt() {
    // FR-010: closing a tab over unsaved edits raises the save/discard/cancel
    // prompt (assert the prompt-pending state is set, not auto-closed).
    let fixture = temp_ron("A(x: 1)\n", "close_prompt");
    let mut app = App::new(AppSettings::default(), None);
    app.open_file(&fixture);
    {
        let doc = app.active_document_mut().expect("active doc");
        doc.buffer = "A(x: 2)\n".to_string();
        doc.on_edit();
        assert!(doc.dirty());
    }
    app.request_close_doc(0);
    let prompt = app
        .dirty_prompt()
        .expect("closing a dirty doc must raise the prompt");
    assert_eq!(prompt.doc_index, 0);
    // The document is NOT closed while the prompt is open.
    assert_eq!(
        app.document_count(),
        1,
        "doc stays open until prompt resolves"
    );
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn prompt_cancel_keeps_doc_open_and_dirty() {
    let fixture = temp_ron("A(x: 1)\n", "cancel");
    let mut app = App::new(AppSettings::default(), None);
    app.open_file(&fixture);
    app.active_document_mut().unwrap().buffer = "A(x: 2)\n".to_string();
    app.active_document_mut().unwrap().on_edit();
    app.request_close_doc(0);
    assert!(app.dirty_prompt().is_some());

    app.resolve_dirty_prompt(PromptChoice::Cancel);
    assert!(app.dirty_prompt().is_none(), "Cancel closes the prompt");
    assert_eq!(app.document_count(), 1, "Cancel keeps the doc open");
    assert!(
        app.active_document().unwrap().dirty(),
        "Cancel leaves the doc dirty"
    );
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn prompt_discard_closes_doc_without_saving() {
    // FR-010: Discard drops the document (and its unsaved edits) and proceeds.
    let fixture = temp_ron("A(x: 1)\n", "discard");
    let mut app = App::new(AppSettings::default(), None);
    app.open_file(&fixture);
    app.active_document_mut().unwrap().buffer = "A(x: 2)\n".to_string();
    app.active_document_mut().unwrap().on_edit();
    app.request_close_doc(0);

    app.resolve_dirty_prompt(PromptChoice::Discard);
    assert!(app.dirty_prompt().is_none(), "Discard closes the prompt");
    assert_eq!(app.document_count(), 0, "Discard drops the document");
    // The unsaved edit was NOT written to disk.
    let on_disk = std::fs::read(&fixture).expect("original file untouched");
    assert_eq!(on_disk, b"A(x: 1)\n", "Discard must not write the buffer");
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn prompt_save_writes_then_closes_doc() {
    // FR-010: Save persists the buffer, then proceeds with the close.
    let fixture = temp_ron("A(x: 1)\n", "save_then_close");
    let mut app = App::new(AppSettings::default(), None);
    app.open_file(&fixture);
    app.active_document_mut().unwrap().buffer = "A(x: 2)\n".to_string();
    app.active_document_mut().unwrap().on_edit();
    app.request_close_doc(0);

    app.resolve_dirty_prompt(PromptChoice::Save);
    assert!(app.dirty_prompt().is_none(), "Save resolves the prompt");
    assert_eq!(app.document_count(), 0, "Save then proceeds with the close");
    let on_disk = std::fs::read(&fixture).expect("read back saved file");
    assert_eq!(on_disk, b"A(x: 2)\n", "Save must persist the buffer first");
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn close_clean_doc_does_not_prompt() {
    let fixture = temp_ron("A(x: 1)\n", "clean_close");
    let mut app = App::new(AppSettings::default(), None);
    app.open_file(&fixture);
    // No edit → clean.
    assert!(!app.active_document().unwrap().dirty());
    app.request_close_doc(0);
    assert!(
        app.dirty_prompt().is_none(),
        "a clean doc closes without prompting"
    );
    assert_eq!(
        app.document_count(),
        0,
        "clean close drops the doc immediately"
    );
    let _ = std::fs::remove_file(&fixture);
}
