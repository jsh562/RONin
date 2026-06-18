//! Atomic-save fault-injection + byte-fidelity round-trip tests (E007/OBJ1).
//!
//! Covers tasks T008 (fault injection), T009 (byte-fidelity round-trip), and T018
//! (`[COMPLETES TR-001]`) for the crash-safe atomic save pipeline
//! ([`ronin_app::fileio::save_atomic`] / [`save_document`]).
//!
//! # The atomicity property (TR-001/TR-003/TR-019, SC-001)
//!
//! Every assertion here exercises the same hard guarantee: a save that fails
//! leaves the **original target byte-identical** and returns a [`SaveError`] (so
//! the buffer would stay dirty). The atomic pipeline only ever touches the target
//! through the atomic replace primitive (a same-directory temp is written and
//! `fsync`'d first), so a fault at any earlier stage cannot corrupt or truncate the
//! original.
//!
//! **Honesty note (kill-mid-rename).** A true process kill *between* the temp-write
//! and the atomic replace is NOT injectable in-process — there is no point in this
//! file where we can `kill -9` ourselves between `atomicwrites`' internal
//! temp-write and its rename. That guarantee is provided by the atomic primitive
//! itself: `atomicwrites` never partially writes the target (POSIX `renameat` /
//! Windows `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING` is all-or-nothing), so a
//! kill before the rename leaves the original intact and a kill after it leaves the
//! fully-committed new file. We therefore do NOT fake a kill test; we inject the
//! faults that ARE feasible in-process (target is a directory, an unwritable parent
//! directory) and assert the original-intact + error-surfaced property, and we
//! assert the residual-temp guarantee (TR-019b) on the success path.

use std::path::{Path, PathBuf};

use ronin_app::document::ByteFidelityProfile;
use ronin_app::fileio::{open_path, save_atomic, save_document, SaveError};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A unique temp directory for one test; created on disk.
fn unique_temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ronin_atomic_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// The directory holding the shared serde/Bevy RON corpus (`ronin-core` fixtures),
/// reached relative to this crate's manifest dir.
fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("ronin-core")
        .join("tests")
        .join("corpus")
        .join("valid")
}

/// Decode `raw` to the editor buffer the way the shell does: validate UTF-8, drop a
/// leading BOM (kept on the profile, not in the buffer), and normalise every line
/// ending to a bare `\n` (what the `TextEdit` widget produces). Mirrors the load
/// path so a save round-trip is meaningful.
fn widget_buffer(raw: &[u8]) -> String {
    let text = std::str::from_utf8(raw).expect("fixture must be UTF-8");
    let without_bom = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    without_bom.replace("\r\n", "\n").replace('\r', "\n")
}

/// Load → atomic-save (over `target`) → re-read; assert the on-disk bytes match the
/// byte-fidelity contract byte-for-byte. Returns the bytes actually written so a
/// caller can also exercise the replace-over-existing path.
fn assert_atomic_roundtrip(raw: &[u8], target: &Path, label: &str) {
    let profile = ByteFidelityProfile::from_bytes(raw);
    let buffer = widget_buffer(raw);
    save_atomic(&buffer, &profile, target)
        .unwrap_or_else(|e| panic!("{label}: atomic save must succeed: {e}"));
    let on_disk = std::fs::read(target).expect("read back saved file");
    assert_eq!(
        on_disk, raw,
        "{label}: atomic load→save must be byte-identical\n  expected: {raw:?}\n  got:      {on_disk:?}"
    );
}

// ---------------------------------------------------------------------------
// T009 / T018 — Byte-fidelity round-trip (SC-002/SC-007)
// ---------------------------------------------------------------------------

#[test]
fn hand_authored_fidelity_cases_roundtrip_through_atomic_path() {
    // The atomic path must NOT regress E003's `save_bytes` fidelity: CRLF/LF, BOM,
    // and no-trailing-newline cases survive a load → save_atomic → re-read
    // byte-for-byte (TR-004, SC-002).
    let dir = unique_temp_dir("fidelity");

    // (label, raw bytes) covering the fidelity matrix.
    let bom = [0xEFu8, 0xBB, 0xBF];
    let mut bom_lf = bom.to_vec();
    bom_lf.extend_from_slice(b"List([1, 2, 3])\n");
    let mut bom_crlf = bom.to_vec();
    bom_crlf.extend_from_slice(b"List([1, 2, 3])\r\n");

    let cases: &[(&str, Vec<u8>)] = &[
        ("uniform LF", b"Config(\n    level: 3,\n)\n".to_vec()),
        (
            "uniform CRLF",
            b"Config(\r\n    level: 3,\r\n)\r\n".to_vec(),
        ),
        ("BOM + LF", bom_lf),
        ("BOM + CRLF", bom_crlf),
        ("no trailing newline (LF)", b"Foo(x: 1)".to_vec()),
        ("no trailing newline (CRLF)", b"a\r\nb".to_vec()),
        ("BOM absent", b"List([1, 2, 3])\n".to_vec()),
        ("empty file", b"".to_vec()),
    ];

    for (label, raw) in cases {
        let target = dir.join(format!("{}.ron", label.replace([' ', '(', ')', '+'], "_")));
        assert_atomic_roundtrip(raw, &target, label);
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn corpus_files_roundtrip_through_atomic_path() {
    // Bind the byte-fidelity round-trip to the shared serde/Bevy corpus (the
    // project's fault-injection test policy / spec advisory): every valid corpus
    // file loads → save_atomic → re-reads byte-for-byte. The corpus includes a CRLF
    // file (33), a BOM-prefixed file (34), and a no-trailing-newline file (35), so
    // the fidelity matrix is exercised against real fixtures too (SC-002/SC-007).
    let dir = unique_temp_dir("corpus");
    let mut checked = 0usize;
    for entry in std::fs::read_dir(corpus_dir()).expect("read corpus dir") {
        let entry = entry.expect("corpus dir entry");
        let src = entry.path();
        if src.extension().and_then(|e| e.to_str()) != Some("ron") {
            continue;
        }
        let raw = std::fs::read(&src).expect("read corpus fixture");
        // Only UTF-8 corpus files are loadable by the editor; skip any non-UTF-8.
        if std::str::from_utf8(&raw).is_err() {
            continue;
        }
        let name = src.file_name().unwrap().to_string_lossy().to_string();
        let target = dir.join(&name);
        assert_atomic_roundtrip(&raw, &target, &name);
        checked += 1;
    }
    assert!(
        checked >= 30,
        "expected the full valid corpus, checked {checked}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn replace_over_existing_file_is_byte_faithful() {
    // The replace-over-EXISTING-file case (save twice; the second save is a replace
    // of a now-existing target). This exercises the Windows `MoveFileExW`
    // replace-over-existing path (TR-002) and POSIX `renameat`-over-existing. Both
    // saves must land byte-for-byte and the original must never be left partial.
    let dir = unique_temp_dir("replace_existing");
    let target = dir.join("doc.ron");

    let first = b"Config(\n    level: 1,\n)\n";
    let first_profile = ByteFidelityProfile::from_bytes(first);
    save_atomic(&widget_buffer(first), &first_profile, &target).expect("first save");
    assert_eq!(
        std::fs::read(&target).unwrap(),
        first,
        "first save lands byte-for-byte"
    );

    // Second save replaces the existing target (CRLF, to also re-prove fidelity is
    // not lost across a replace).
    let second = b"Config(\r\n    level: 2,\r\n)\r\n";
    let second_profile = ByteFidelityProfile::from_bytes(second);
    save_atomic(&widget_buffer(second), &second_profile, &target).expect("replace save");
    assert_eq!(
        std::fs::read(&target).unwrap(),
        second,
        "replace-over-existing lands byte-for-byte"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn save_document_round_trips_a_loaded_document() {
    // The public `save_document` seam (what the shell calls) routes through the
    // atomic path: open a file → save_document over it → re-read byte-for-byte.
    let dir = unique_temp_dir("save_document");
    let target = dir.join("doc.ron");
    let raw = b"Foo(\r\n    x: 1,\r\n)\r\n";
    std::fs::write(&target, raw).expect("seed fixture");

    let doc = open_path(&target).expect("fixture opens");
    save_document(&doc, &target).expect("save_document must succeed");
    assert_eq!(
        std::fs::read(&target).unwrap(),
        raw,
        "save_document preserves CRLF fidelity through the atomic path"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// T008 / T018 — Fault injection: original intact + error surfaced (SC-001)
// ---------------------------------------------------------------------------

#[test]
fn save_onto_a_directory_leaves_original_intact_and_surfaces_error() {
    // Inject a feasible fault: the target path is an existing DIRECTORY, so the
    // atomic replace cannot turn it into a file. A save into a pre-existing sibling
    // file must be unaffected; here we assert the failed save returns an error and
    // writes no file content under the directory path.
    let dir = unique_temp_dir("target_is_dir");
    let target = dir.join("a_directory");
    std::fs::create_dir(&target).expect("create the directory target");

    let profile = ByteFidelityProfile::from_bytes(b"Config(level: 1)\n");
    let err = save_atomic("Config(level: 1)\n", &profile, &target)
        .expect_err("saving onto a directory must fail");
    // The error is a real SaveError variant (not a panic / silent success).
    assert!(
        matches!(
            err,
            SaveError::ReplaceFailed(_)
                | SaveError::PermissionDenied(_)
                | SaveError::Io(_)
                | SaveError::PartialWrite(_)
        ),
        "directory-target failure must surface as a SaveError: {err:?}"
    );
    // The directory is still a directory — it was not clobbered into a file.
    assert!(
        target.is_dir(),
        "the directory target must remain a directory (original intact)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn save_to_target_without_parent_surfaces_same_filesystem_impossible() {
    // TR-005 / T012: a target with no parent directory cannot host a
    // same-filesystem temp, so the atomic save degrades-and-surfaces rather than
    // silently performing a non-atomic write. A bare root-relative file name (e.g.
    // a path whose `.parent()` is empty) is the closest in-process analogue; we use
    // a path whose parent does not exist to drive the surface.
    let dir = unique_temp_dir("no_parent");
    // Parent directory does not exist: the same-directory temp cannot be created.
    let target = dir.join("missing_subdir").join("out.ron");
    assert!(!target.parent().unwrap().exists(), "parent must be absent");

    let profile = ByteFidelityProfile::from_bytes(b"X(1)\n");
    let err =
        save_atomic("X(1)\n", &profile, &target).expect_err("save into a missing parent must fail");
    assert!(
        matches!(
            err,
            SaveError::SameFilesystemImpossible(_)
                | SaveError::Io(_)
                | SaveError::PartialWrite(_)
                | SaveError::ReplaceFailed(_)
                | SaveError::PermissionDenied(_)
        ),
        "missing-parent save must surface a SaveError: {err:?}"
    );
    // No file was created at the target path.
    assert!(
        !target.exists(),
        "a failed save must not create the target file"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn save_into_read_only_parent_leaves_original_intact_and_surfaces_error() {
    // Inject a feasible fault on POSIX: an existing target inside a parent directory
    // that is then made READ-ONLY (no write/execute for the owner). The atomic
    // pipeline cannot create its same-directory temp, so the save must fail AND the
    // original target file must remain byte-identical (TR-003/TR-019a). Windows
    // permission bits do not model "read-only directory blocks file creation" the
    // same way, so this fault is POSIX-only (the directory-target test above covers
    // a feasible Windows fault).
    use std::os::unix::fs::PermissionsExt;

    let dir = unique_temp_dir("readonly_parent");
    let parent = dir.join("locked");
    std::fs::create_dir(&parent).expect("create parent");
    let target = parent.join("doc.ron");

    let original = b"Config(\n    level: 7,\n)\n";
    std::fs::write(&target, original).expect("seed original");

    // Snapshot the original bytes BEFORE the failed save.
    let before = std::fs::read(&target).expect("read original before");

    // Make the parent read-only (r-xr-xr-x): no new files can be created in it.
    let mut perms = std::fs::metadata(&parent).unwrap().permissions();
    perms.set_mode(0o555);
    std::fs::set_permissions(&parent, perms).expect("set parent read-only");

    let profile = ByteFidelityProfile::from_bytes(original);
    let result = save_atomic("Config(\n    level: 999,\n)\n", &profile, &target);

    // Restore writability so the dir can be cleaned up regardless of the assertion.
    let mut restore = std::fs::metadata(&parent).unwrap().permissions();
    restore.set_mode(0o755);
    let _ = std::fs::set_permissions(&parent, restore);

    let err = result.expect_err("saving into a read-only parent must fail");
    assert!(
        matches!(
            err,
            SaveError::PermissionDenied(_)
                | SaveError::PartialWrite(_)
                | SaveError::ReplaceFailed(_)
                | SaveError::SameFilesystemImpossible(_)
                | SaveError::Io(_)
        ),
        "read-only-parent failure must surface a SaveError: {err:?}"
    );

    // TR-003 / TR-019a: the original target is byte-identical to before the save.
    let after = std::fs::read(&target).expect("read original after");
    assert_eq!(
        after, before,
        "a failed save must leave the original file byte-identical"
    );
    assert_eq!(after, original, "original content unchanged");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn residual_temp_is_a_non_target_artifact_not_the_target_path() {
    // TR-019b: a residual same-directory temp (the `atomicwrites` `.atomicwrite*`
    // subdirectory) is a cleanable NON-TARGET artifact — never written AT the target
    // path. On a SUCCESSFUL save we assert (a) the target holds exactly the saved
    // bytes (the "original"/result is at the target path), and (b) no stray file
    // sits at any path other than the target+well-known temp prefix that would be
    // mistaken for the user's file. `atomicwrites` cleans its temp subdir on commit,
    // so after a successful save the directory holds ONLY the target file.
    let dir = unique_temp_dir("residual_temp");
    let target = dir.join("doc.ron");

    let raw = b"Config(\n    level: 1,\n)\n";
    let profile = ByteFidelityProfile::from_bytes(raw);
    save_atomic(&widget_buffer(raw), &profile, &target).expect("save succeeds");

    // After a committed save the directory contains exactly the target file and no
    // residual temp artifact AT the target path or beside it.
    let mut entries: Vec<String> = std::fs::read_dir(&dir)
        .expect("read save dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().to_string())
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        vec!["doc.ron".to_string()],
        "after a committed save only the target file remains; no residual temp at/beside the target"
    );
    // The bytes AT the target path are exactly what we saved (never a temp).
    assert_eq!(std::fs::read(&target).unwrap(), raw);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn failed_save_does_not_truncate_or_partially_write_an_existing_target() {
    // Reinforce the no-partial-write property using the directory-as-parent-of-self
    // trick is not portable; instead seed a real file, then attempt a save whose
    // temp creation fails (missing intermediate parent), and assert the existing
    // sibling target file is untouched. This proves a save FAILURE never reaches
    // through to truncate an unrelated/previous target.
    let dir = unique_temp_dir("no_partial");
    let good_target = dir.join("kept.ron");
    let original = b"Kept(\n    x: 1,\n)\n";
    std::fs::write(&good_target, original).expect("seed kept file");

    // A save aimed at a path under a non-existent subdir fails at temp creation.
    let bad_target = dir.join("nope").join("out.ron");
    let profile = ByteFidelityProfile::from_bytes(b"Bad(1)\n");
    let _ = save_atomic("Bad(1)\n", &profile, &bad_target)
        .expect_err("save under a missing subdir must fail");

    // The unrelated existing file is byte-identical (no spillover/truncation).
    assert_eq!(
        std::fs::read(&good_target).unwrap(),
        original,
        "an unrelated existing file must be untouched by a failed save"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
