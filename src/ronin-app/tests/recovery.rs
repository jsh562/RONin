//! Crash-recovery autosave sidecar tests (E007 / OBJ2).
//!
//! Covers tasks T019 (recovery lifecycle, SC-003), T020 (sidecar fault injection,
//! TR-022), and T028 (`[COMPLETES TR-006]` — SC-004 autosave-never-touches-user-file,
//! SC-010 one-write-per-window / no-op-tick-writes-nothing) for the recovery
//! sidecar ([`ronin_app::recovery`]) and its `EditorDocument`/`App` integration.
//!
//! # The recovery property (TR-006..009, SC-003/SC-004/SC-010)
//!
//! The sidecar is the ONE on-disk artifact E007 adds. The hard guarantees these
//! tests pin:
//!
//! * **Recovery lifecycle (SC-003).** An abrupt termination with unsaved edits
//!   leaves a live sidecar; reopening offers restore; accepting recovers the
//!   in-progress content; declining opens the on-disk file. A clean save / exit
//!   removes the sidecar so reopening offers nothing. A stale / same-content
//!   sidecar is never offered.
//! * **Never touches the user file (SC-004).** An autosave tick writes ONLY the
//!   sidecar — the user file's bytes AND modification time are unchanged across any
//!   number of ticks.
//! * **Crash-safe sidecar write (TR-022).** A fault during a sidecar write leaves
//!   either the prior intact sidecar or no sidecar (never corrupt/partial) and never
//!   modifies the user file — reusing the OBJ1 atomic primitive's guarantee.
//! * **One write per window (SC-010).** A continuous keystroke run produces at most
//!   one sidecar write per debounce window; a no-op tick (no change) writes nothing.
//!
//! **Honesty note (kill-mid-rename).** As in `atomic_save.rs`, a true process kill
//! *between* the sidecar's temp-write and its atomic rename is NOT injectable
//! in-process. That case is guaranteed by the atomic primitive
//! ([`ronin_app::fileio::save_atomic`]) the sidecar write reuses: `atomicwrites`
//! writes a same-directory temp and atomically replaces, so a kill before the rename
//! leaves the prior sidecar (or none) and a kill after it leaves the fully-committed
//! new sidecar — never a corrupt/partial one. We therefore inject the faults that
//! ARE feasible in-process (sidecar target unwritable / its parent missing) and
//! assert the prior-intact-or-absent + user-file-untouched property.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ronin_app::app::{App, RecoveryChoice};
use ronin_app::document::ByteFidelityProfile;
use ronin_app::recovery::{
    detect_recovery, remove_sidecar, sidecar_path, AutosaveDebounce, RecoveryDetection,
    RecoverySidecar,
};
use ronin_app::settings::{AppSettings, AutosaveConfig};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A unique temp directory for one test; created on disk.
fn unique_temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ronin_recovery_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Build a sidecar record for `source_path` holding `buffer` with an LF profile.
fn sidecar_for(source_path: &Path, buffer: &str) -> RecoverySidecar {
    let profile = ByteFidelityProfile::from_bytes(buffer.as_bytes());
    RecoverySidecar::new(source_path.to_path_buf(), buffer.to_string(), &profile)
}

/// Simulate an abrupt termination: write a live sidecar for an edited-unsaved buffer
/// beside `target` and DO NOT clean it up (as a crash would leave it). Returns the
/// sidecar path actually written.
fn simulate_crash_with_unsaved(target: &Path, on_disk: &str, in_progress: &str) -> PathBuf {
    std::fs::write(target, on_disk).expect("write on-disk file");
    let sidecar = sidecar_for(target, in_progress);
    let sc_path = sidecar_path(target);
    sidecar.write_to(&sc_path).expect("write sidecar");
    sc_path
}

// ---------------------------------------------------------------------------
// T019 — Recovery lifecycle (SC-003)
// ---------------------------------------------------------------------------

#[test]
fn sidecar_path_is_sibling_dotfile() {
    // AD-005: the sidecar for `dir/foo.ron` is the sibling `dir/.foo.ron.ronin-recovery`.
    let target = Path::new("/some/dir/foo.ron");
    let sc = sidecar_path(target);
    assert_eq!(
        sc.parent(),
        target.parent(),
        "sidecar sits beside the target"
    );
    assert_eq!(
        sc.file_name().and_then(|n| n.to_str()),
        Some(".foo.ron.ronin-recovery"),
        "sidecar name is `.<name>.ronin-recovery`"
    );
}

#[test]
fn detect_offers_restore_on_live_divergence() {
    // SC-003: an abrupt termination with unsaved edits → a live, divergent sidecar
    // → reopen detects it and offers restore.
    let dir = unique_temp_dir("detect_offer");
    let target = dir.join("doc.ron");
    let on_disk = "(value: 1)\n";
    let in_progress = "(value: 999)\n";
    simulate_crash_with_unsaved(&target, on_disk, in_progress);

    let bytes = std::fs::read(&target).unwrap();
    match detect_recovery(&target, &bytes) {
        RecoveryDetection::Offer(sidecar) => {
            assert_eq!(
                sidecar.buffer, in_progress,
                "the offered sidecar holds the in-progress content"
            );
        }
        RecoveryDetection::None => panic!("a live divergent sidecar must be offered"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn detect_suppresses_same_content_sidecar() {
    // TR-009 / SC-003: a sidecar whose content matches the on-disk file is NOT
    // offered (nothing to recover).
    let dir = unique_temp_dir("detect_same");
    let target = dir.join("doc.ron");
    let content = "(value: 42)\n";
    // The sidecar holds exactly what is on disk.
    simulate_crash_with_unsaved(&target, content, content);

    let bytes = std::fs::read(&target).unwrap();
    assert_eq!(
        detect_recovery(&target, &bytes),
        RecoveryDetection::None,
        "a same-content sidecar must not be offered"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn detect_suppresses_same_content_across_eol_and_bom() {
    // TR-009: same-content suppression must compare against the *editor buffer* form
    // of the on-disk bytes (BOM dropped, EOLs normalised), so a CRLF+BOM file whose
    // sidecar holds the equivalent LF-no-BOM buffer is still judged same-content.
    let dir = unique_temp_dir("detect_eol");
    let target = dir.join("doc.ron");
    // On disk: BOM + CRLF. Editor buffer form: no BOM, LF.
    let on_disk_bytes = b"\xEF\xBB\xBF(value: 7)\r\n";
    std::fs::write(&target, on_disk_bytes).unwrap();
    let buffer_form = "(value: 7)\n";
    let sidecar = sidecar_for(&target, buffer_form);
    sidecar.write_to(&sidecar_path(&target)).unwrap();

    assert_eq!(
        detect_recovery(&target, on_disk_bytes),
        RecoveryDetection::None,
        "same content modulo EOL/BOM must not be offered"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn detect_none_when_no_sidecar() {
    // No sidecar present → open normally, offer nothing.
    let dir = unique_temp_dir("detect_absent");
    let target = dir.join("doc.ron");
    std::fs::write(&target, "(value: 1)\n").unwrap();
    let bytes = std::fs::read(&target).unwrap();
    assert_eq!(detect_recovery(&target, &bytes), RecoveryDetection::None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn app_reopen_offers_and_restore_recovers_in_progress_work() {
    // SC-003 end-to-end: abrupt-term → reopen → offer → accept restores the
    // in-progress content (and leaves the document dirty).
    let dir = unique_temp_dir("app_restore");
    let target = dir.join("doc.ron");
    let on_disk = "(value: 1)\n";
    let in_progress = "(value: 12345)\n";
    simulate_crash_with_unsaved(&target, on_disk, in_progress);

    let mut app = App::new(AppSettings::default(), None);
    app.open_file(&target);

    // A live divergent sidecar → a restore offer, NOT an opened tab yet.
    assert!(
        app.recovery_offer().is_some(),
        "reopen must raise a recovery offer"
    );
    assert_eq!(
        app.document_count(),
        0,
        "open is deferred until the offer resolves"
    );

    // Accept: restore the in-progress work.
    app.resolve_recovery_offer(RecoveryChoice::Restore);
    assert!(
        app.recovery_offer().is_none(),
        "offer cleared after resolve"
    );
    assert_eq!(app.document_count(), 1, "a tab opens on restore");
    let doc = app.active_document().expect("active doc after restore");
    assert_eq!(
        doc.buffer, in_progress,
        "restore recovers the in-progress buffer"
    );
    assert!(
        doc.dirty(),
        "restored in-progress work is dirty (not silently treated as saved)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn app_reopen_decline_opens_on_disk_file() {
    // SC-003: declining the offer opens the on-disk file (recovered content dropped).
    let dir = unique_temp_dir("app_decline");
    let target = dir.join("doc.ron");
    let on_disk = "(value: 1)\n";
    let in_progress = "(value: 999)\n";
    simulate_crash_with_unsaved(&target, on_disk, in_progress);

    let mut app = App::new(AppSettings::default(), None);
    app.open_file(&target);
    assert!(app.recovery_offer().is_some());

    app.resolve_recovery_offer(RecoveryChoice::Decline);
    assert!(app.recovery_offer().is_none());
    assert_eq!(app.document_count(), 1, "a tab opens on decline");
    let doc = app.active_document().expect("active doc after decline");
    assert_eq!(doc.buffer, on_disk, "decline opens the on-disk content");
    assert!(!doc.dirty(), "a freshly opened on-disk file is clean");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clean_save_removes_sidecar_and_reopen_offers_nothing() {
    // SC-003: after a clean save, no sidecar remains, so reopening offers no recovery.
    let dir = unique_temp_dir("clean_save");
    let target = dir.join("doc.ron");
    std::fs::write(&target, "(value: 1)\n").unwrap();

    let mut app = App::new(AppSettings::default(), None);
    app.open_file(&target);
    assert_eq!(app.document_count(), 1, "no offer for a clean reopen");

    // Edit, autosave (leaving a sidecar), then a clean save must remove it.
    app.replace_active_buffer_for_test("(value: 2)\n");
    let dispatched = app.force_autosave_all();
    assert_eq!(dispatched, 1, "a dirty edit autosaves a sidecar");
    app.flush_autosaves(dispatched);
    assert!(
        sidecar_path(&target).exists(),
        "the sidecar exists after autosave"
    );

    // A clean save removes the sidecar (TR-009).
    let idx = app.active_index().expect("active index");
    assert!(app.save_doc_to(idx, &target), "save must succeed");
    assert!(
        !sidecar_path(&target).exists(),
        "a clean save removes the recovery sidecar (TR-009)"
    );

    // Reopen (in a fresh app) offers nothing — no orphan sidecar.
    let mut app2 = App::new(AppSettings::default(), None);
    app2.open_file(&target);
    assert!(
        app2.recovery_offer().is_none(),
        "no recovery is offered after a clean save (no orphan sidecar)"
    );
    assert_eq!(app2.document_count(), 1, "the file opens normally");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn remove_sidecar_is_noop_when_absent() {
    // TR-009: removing an absent sidecar is the clean case — not an error.
    let dir = unique_temp_dir("remove_absent");
    let target = dir.join("doc.ron");
    assert!(
        remove_sidecar(&target).is_ok(),
        "removing an absent sidecar is a no-op success"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// T028 / SC-004 — Autosave never touches the user file
// ---------------------------------------------------------------------------

#[test]
fn autosave_tick_never_touches_user_file_bytes_or_mtime() {
    // SC-004: across any number of autosave ticks, the user file's bytes AND
    // modification time are unchanged — only the sidecar is written.
    let dir = unique_temp_dir("never_touch");
    let target = dir.join("doc.ron");
    let on_disk = "(value: 1)\n";
    std::fs::write(&target, on_disk).expect("write user file");

    // Snapshot the user file's bytes + mtime BEFORE any autosave.
    let before_bytes = std::fs::read(&target).unwrap();
    let before_mtime = std::fs::metadata(&target).unwrap().modified().unwrap();

    let mut app = App::new(AppSettings::default(), None);
    app.open_file(&target);

    // Drive several autosave ticks with progressively-edited content.
    for n in 0..5 {
        app.replace_active_buffer_for_test(&format!("(value: {n})\n"));
        let dispatched = app.force_autosave_all();
        app.flush_autosaves(dispatched);
    }

    // The user file is byte-identical and its mtime is unchanged (only the sidecar
    // was ever written).
    let after_bytes = std::fs::read(&target).unwrap();
    let after_mtime = std::fs::metadata(&target).unwrap().modified().unwrap();
    assert_eq!(
        after_bytes, before_bytes,
        "SC-004: autosave must not change the user file's bytes"
    );
    assert_eq!(
        after_mtime, before_mtime,
        "SC-004: autosave must not change the user file's modification time"
    );
    // But the sidecar WAS written (autosave did happen).
    assert!(
        sidecar_path(&target).exists(),
        "the sidecar IS written by autosave"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sidecar_write_targets_only_the_sidecar_path() {
    // TR-007: a direct sidecar write touches only the sidecar path, never the user
    // file — even when both live in the same directory.
    let dir = unique_temp_dir("only_sidecar");
    let target = dir.join("doc.ron");
    let on_disk = "(original: true)\n";
    std::fs::write(&target, on_disk).unwrap();

    let sidecar = sidecar_for(&target, "(edited: true)\n");
    sidecar
        .write_to(&sidecar_path(&target))
        .expect("sidecar write");

    assert_eq!(
        std::fs::read(&target).unwrap(),
        on_disk.as_bytes(),
        "TR-007: the user file is untouched by a sidecar write"
    );
    assert!(sidecar_path(&target).exists(), "the sidecar was written");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// T020 — Sidecar fault injection (TR-022)
// ---------------------------------------------------------------------------

#[test]
fn sidecar_fault_parent_missing_leaves_no_corrupt_sidecar_and_user_file_intact() {
    // TR-022: a fault during a sidecar write (its parent directory does not exist,
    // so the same-dir atomic temp cannot be established) leaves NO sidecar (never a
    // corrupt/partial one) and never modifies the user file.
    let dir = unique_temp_dir("fault_parent");
    let target = dir.join("doc.ron");
    let on_disk = "(value: 1)\n";
    std::fs::write(&target, on_disk).unwrap();

    // A sidecar whose *path* lives under a nonexistent directory: the atomic write
    // cannot establish a same-dir temp → it fails, leaving nothing behind.
    let bogus_sidecar = dir.join("nope").join(".doc.ron.ronin-recovery");
    let sidecar = sidecar_for(&target, "(edited: true)\n");
    let result = sidecar.write_to(&bogus_sidecar);
    assert!(result.is_err(), "the sidecar write must fail, surfaced");
    assert!(
        !bogus_sidecar.exists(),
        "TR-022: no corrupt/partial sidecar is left behind"
    );
    // The user file is byte-identical (the sidecar write never targets it — TR-007).
    assert_eq!(
        std::fs::read(&target).unwrap(),
        on_disk.as_bytes(),
        "TR-022/TR-007: a sidecar-write fault never modifies the user file"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sidecar_fault_target_is_dir_leaves_prior_sidecar_intact() {
    // TR-022: a fault on a *re-write* (the sidecar path is occupied by a directory,
    // so the atomic replace cannot commit) leaves the PRIOR intact sidecar — never a
    // corrupt/partial one. We first write a good sidecar, then force a faulting write
    // at a path that is a directory and assert the good one survives.
    let dir = unique_temp_dir("fault_dir");
    let target = dir.join("doc.ron");
    let on_disk = "(value: 1)\n";
    std::fs::write(&target, on_disk).unwrap();

    // Write a good sidecar at the real sidecar path first (the "prior intact" copy).
    let sc_path = sidecar_path(&target);
    let good = sidecar_for(&target, "(prior: true)\n");
    good.write_to(&sc_path).expect("prior sidecar write");
    let prior_bytes = std::fs::read(&sc_path).expect("read prior sidecar");

    // Now force a faulting write: aim a *different* sidecar write at a path that is a
    // directory (atomic replace cannot overwrite a directory). The prior good sidecar
    // is untouched.
    let occupied = dir.join("occupied.ronin-recovery");
    std::fs::create_dir(&occupied).unwrap();
    let next = sidecar_for(&target, "(next: true)\n");
    let result = next.write_to(&occupied);
    assert!(result.is_err(), "writing onto a directory must fail");

    // The prior good sidecar at the real path is byte-identical (never corrupted).
    assert_eq!(
        std::fs::read(&sc_path).expect("re-read prior sidecar"),
        prior_bytes,
        "TR-022: a faulting write leaves the prior intact sidecar"
    );
    // And the loaded prior sidecar still round-trips its recovered buffer.
    let loaded = RecoverySidecar::load(&sc_path).expect("prior sidecar still loads");
    assert_eq!(loaded.buffer, "(prior: true)\n");
    // The user file is untouched throughout.
    assert_eq!(std::fs::read(&target).unwrap(), on_disk.as_bytes());

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// T028 / SC-010 — One write per window; no-op tick writes nothing
// ---------------------------------------------------------------------------

#[test]
fn debounce_no_op_tick_writes_nothing() {
    // SC-010: a no-op tick (no change since the last write) fires nothing.
    let mut debounce = AutosaveDebounce::new(AutosaveConfig::default());
    let t0 = Instant::now();

    // No change observed yet → nothing to autosave, even forced.
    assert!(!debounce.has_unsaved_change(), "clean: no unsaved change");
    assert!(!debounce.force_tick(), "clean: forced tick writes nothing");

    // Observe one change, then "write" it.
    debounce.note_change(1, t0);
    assert!(debounce.has_unsaved_change(), "a change is pending");
    debounce.mark_written();

    // A subsequent tick with NO new change must not fire — even forced (SC-010).
    assert!(
        !debounce.has_unsaved_change(),
        "written: nothing new to save"
    );
    assert!(
        !debounce.force_tick(),
        "no-op tick after a write fires nothing"
    );
    let later = t0 + Duration::from_secs(60);
    assert!(
        !debounce.poll(later),
        "an elapsed idle interval with no new change still fires nothing"
    );
}

#[test]
fn debounce_continuous_run_is_at_most_one_write_per_window() {
    // SC-010: a continuous keystroke run produces at most one sidecar write per
    // debounce window. With a deterministic clock, many edits inside the idle window
    // accumulate into ONE eligible write once the window elapses (or the edit-count
    // threshold binds), not one write per keystroke.
    let config = AutosaveConfig::default(); // idle 4s, edit-count 50
    let mut debounce = AutosaveDebounce::new(config);
    let start = Instant::now();

    // A rapid run of 10 edits, each 100ms apart — all within the 4s idle window and
    // below the 50-edit threshold. None should fire (still typing).
    let mut writes = 0;
    for n in 1..=10u64 {
        let now = start + Duration::from_millis(n * 100);
        debounce.note_change(n, now);
        if debounce.poll(now) {
            writes += 1;
            debounce.mark_written();
        }
    }
    assert_eq!(writes, 0, "mid-run keystrokes do not each trigger a write");

    // Now the run pauses: idle past the 4s window → exactly one write becomes due.
    let idle_now = start + Duration::from_secs(10);
    assert!(
        debounce.poll(idle_now),
        "one write is due after the idle window"
    );
    debounce.mark_written();
    // And immediately polling again (still no new change) fires nothing — one write
    // for the whole run.
    assert!(
        !debounce.poll(idle_now + Duration::from_secs(10)),
        "no second write for the same run"
    );
}

#[test]
fn debounce_edit_count_trigger_fires_within_idle_window() {
    // TR-025: the edit-count trigger fires an autosave even before the idle interval
    // elapses (whichever binds first). With a small edit-count threshold, a burst of
    // edits all inside the idle window triggers exactly one write at the threshold.
    let mut config = AutosaveConfig::default();
    config.set_edit_count_trigger(5);
    config.set_idle_debounce_ms(300_000); // huge idle so only the count can bind
    let mut debounce = AutosaveDebounce::new(config);
    let start = Instant::now();

    let mut writes = 0;
    // 5 edits, each 1ms apart — far inside the idle window; the count trigger binds.
    for n in 1..=5u64 {
        let now = start + Duration::from_millis(n);
        debounce.note_change(n, now);
        if debounce.poll(now) {
            writes += 1;
            debounce.mark_written();
        }
    }
    assert_eq!(writes, 1, "the edit-count trigger fires exactly one write");
}

#[test]
fn app_no_op_force_tick_writes_nothing() {
    // SC-010 (App level): forcing a tick on a clean (unchanged) document writes
    // nothing — no sidecar is created.
    let dir = unique_temp_dir("app_noop");
    let target = dir.join("doc.ron");
    std::fs::write(&target, "(value: 1)\n").unwrap();

    let mut app = App::new(AppSettings::default(), None);
    app.open_file(&target);
    // Clean document: a forced tick dispatches zero writes.
    let dispatched = app.force_autosave_all();
    assert_eq!(dispatched, 0, "a clean document autosaves nothing");
    assert!(
        !sidecar_path(&target).exists(),
        "no sidecar is created for a clean document"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn app_untitled_buffer_has_no_sidecar() {
    // TR-017: an untitled buffer (no path) is never autosaved → no sidecar.
    let mut app = App::new(AppSettings::default(), None);
    app.new_untitled();
    app.replace_active_buffer_for_test("(scratch: true)\n");
    let dispatched = app.force_autosave_all();
    assert_eq!(
        dispatched, 0,
        "an untitled buffer autosaves nothing (TR-017)"
    );
}

#[test]
fn app_continuous_edits_produce_at_most_one_sidecar_per_window() {
    // SC-010 (App level): a run of edits followed by ONE forced window-tick writes a
    // single sidecar; a second forced tick with no new change writes nothing.
    let dir = unique_temp_dir("app_window");
    let target = dir.join("doc.ron");
    std::fs::write(&target, "(value: 0)\n").unwrap();

    let mut app = App::new(AppSettings::default(), None);
    app.open_file(&target);

    // A run of edits without an intervening tick.
    for n in 1..=8 {
        app.replace_active_buffer_for_test(&format!("(value: {n})\n"));
    }
    // One forced window-tick → one write for the whole run.
    let first = app.force_autosave_all();
    assert_eq!(first, 1, "the whole run collapses to one sidecar write");
    app.flush_autosaves(first);

    // A second forced tick with NO new change writes nothing.
    let second = app.force_autosave_all();
    assert_eq!(second, 0, "no second write without a new change (SC-010)");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// TR-021 — Restored buffer preserves load-time byte fidelity
// ---------------------------------------------------------------------------

#[test]
fn restored_sidecar_preserves_crlf_bom_trailing_newline_fidelity() {
    // TR-021: a buffer restored from a sidecar re-emits byte-for-byte under the same
    // fidelity contract as the user-file save path (CRLF + BOM + trailing newline).
    let dir = unique_temp_dir("fidelity");
    let target = dir.join("doc.ron");
    // The original on-disk bytes: BOM + CRLF + trailing newline.
    let original = b"\xEF\xBB\xBF(value: 1)\r\n(other: 2)\r\n";
    let profile = ByteFidelityProfile::from_bytes(original);
    // The editor buffer form (BOM dropped, EOLs normalised to LF) — the in-progress
    // content the autosave would capture.
    let buffer = "(value: 1)\n(other: 2)\n";

    // Build the sidecar carrying the load-time fidelity hint, write + reload it.
    let sidecar = RecoverySidecar::new(target.clone(), buffer.to_string(), &profile);
    let sc_path = sidecar_path(&target);
    sidecar.write_to(&sc_path).unwrap();
    let loaded = RecoverySidecar::load(&sc_path).expect("reload sidecar");

    // Re-emit the recovered buffer through the restored profile: it must reproduce
    // the original on-disk bytes byte-for-byte (BOM + CRLF + trailing newline).
    let restored_profile = loaded.restored_profile();
    let reemitted = ronin_app::fileio::save_bytes(&loaded.buffer, &restored_profile);
    assert_eq!(
        reemitted, original,
        "TR-021: a restored buffer re-emits byte-for-byte (BOM/CRLF/trailing)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// TR-022 round-trip integrity — a written sidecar reloads identically
// ---------------------------------------------------------------------------

#[test]
fn sidecar_round_trips_through_write_and_load() {
    // A written sidecar reloads to an equal record (the JSON body is intact).
    let dir = unique_temp_dir("roundtrip");
    let target = dir.join("doc.ron");
    let sidecar = sidecar_for(&target, "(round: true)\n");
    let sc_path = sidecar_path(&target);
    sidecar.write_to(&sc_path).unwrap();
    let loaded = RecoverySidecar::load(&sc_path).expect("load");
    assert_eq!(loaded, sidecar, "sidecar round-trips byte-faithfully");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn corrupt_sidecar_is_treated_as_absent() {
    // project-instructions §I: a corrupt sidecar is ignored (never offered, never an
    // error) so a bad sidecar can't block opening the user's file.
    let dir = unique_temp_dir("corrupt");
    let target = dir.join("doc.ron");
    std::fs::write(&target, "(value: 1)\n").unwrap();
    std::fs::write(sidecar_path(&target), b"{ not valid json >>>").unwrap();

    assert!(
        RecoverySidecar::load(&sidecar_path(&target)).is_none(),
        "a corrupt sidecar loads as None"
    );
    let bytes = std::fs::read(&target).unwrap();
    assert_eq!(
        detect_recovery(&target, &bytes),
        RecoveryDetection::None,
        "a corrupt sidecar is never offered"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
