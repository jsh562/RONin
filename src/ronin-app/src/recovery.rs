//! Crash-recovery autosave sidecar (E007 / OBJ2).
//!
//! This module implements the [`RecoverySidecar`] — the **one** on-disk artifact
//! E007 introduces beyond the user's own file (data-model.md §RecoverySidecar).
//! It is a small local file written *beside* the user's file holding the latest
//! autosaved in-progress buffer plus enough identity to detect stale-vs-live
//! recovery on reopen. It is **never** the user's file (TR-007): an autosave tick
//! writes only the sidecar, leaving the user file's bytes and modification time
//! untouched (SC-004).
//!
//! # The pieces
//!
//! * [`RecoverySidecar`] — the serializable sidecar record: `source_path`,
//!   `buffer`, a `content_marker` (a cheap content hash used for divergence
//!   detection), a `timestamp`, and a `fidelity_hint` (enough of the
//!   [`ByteFidelityProfile`](crate::document::ByteFidelityProfile) to restore a
//!   buffer byte-faithfully — TR-021). [`sidecar_path`] derives the sibling
//!   `.<name>.ronin-recovery` path (AD-005). [`RecoverySidecar::write`] serializes
//!   the record and writes it **crash-safely/atomically** by reusing the Phase-3
//!   atomic primitive ([`crate::fileio::save_atomic`]) — so a fault during a
//!   sidecar write leaves either the prior intact sidecar or no sidecar, never a
//!   corrupt/partial one (TR-022), and the user's file is never touched (TR-007).
//!
//! * [`AutosaveDebounce`] — the deterministic, frame-driven debounce
//!   (AD-004/TR-006/TR-025): it fires an autosave when an idle interval has
//!   elapsed since the last buffer change **or** an edit-count threshold has
//!   accumulated (whichever first, from
//!   [`AutosaveConfig`](crate::settings::AutosaveConfig)), and **only** when the
//!   buffer actually changed since the last sidecar write (tracked via the
//!   document's `edit_generation`). The debounce takes an injectable `now:
//!   Instant` and exposes a `force_tick` test hook so debounce behavior is
//!   exercisable without wall-clock waits (TR-020).
//!
//! * [`detect_recovery`] — the reopen-time detection (TR-008/TR-009): given a
//!   target file and the on-disk file's content, it loads a sibling sidecar (if
//!   any) and decides whether to offer restore — **only on live divergence**, not
//!   on a stale or same-content sidecar.
//!
//! # Off-frame (TR-016/TR-023, SC-008)
//!
//! The per-frame `update` path performs only the cheap [`AutosaveDebounce::poll`]
//! check; the actual sidecar write/flush is handed to an off-frame worker
//! ([`AutosaveWorker`], modelled on
//! [`ReparseWorker`](crate::reparse::ReparseWorker)) so no autosave I/O ever runs
//! on the render path.
//!
//! # Local-only (TR-015)
//!
//! Everything here touches only the local filesystem (via
//! [`crate::fileio::save_atomic`] / `std::fs`) and `serde_json` for the record
//! body; it introduces **no** network or transport dependency. The `network_audit`
//! regression test (`tests/offline_logging.rs`) and `cargo deny` guard the
//! dependency graph (TR-027).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::document::{ByteFidelityProfile, LineEnding};
use crate::fileio::{save_atomic, SaveError};
use crate::settings::AutosaveConfig;

/// The filename suffix used for a recovery sidecar (AD-005).
///
/// The sidecar for `foo.ron` is the sibling hidden file `.foo.ron.ronin-recovery`
/// (a leading dot + the source file name + this suffix), so it is per-file,
/// discoverable, and project-local.
const SIDECAR_SUFFIX: &str = ".ronin-recovery";

/// Compute the sidecar path for `target`: a sibling `.<name>.ronin-recovery`
/// (AD-005).
///
/// The sidecar lives in the **same directory** as the target so it sits beside the
/// user's file (and so the atomic sidecar write's same-dir temp lands on the same
/// filesystem). The name is the target's file name prefixed with `.` and suffixed
/// with [`SIDECAR_SUFFIX`], e.g. `dir/foo.ron` → `dir/.foo.ron.ronin-recovery`.
///
/// A `target` with no file-name component (e.g. a root path) falls back to a bare
/// `.<suffix>` sibling so the function is always total; such degenerate targets are
/// not real save targets in practice.
#[must_use]
pub fn sidecar_path(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let sidecar_name = format!(".{name}{SIDECAR_SUFFIX}");
    match target.parent() {
        Some(parent) => parent.join(sidecar_name),
        None => PathBuf::from(sidecar_name),
    }
}

/// A serializable mirror of the line-ending style carried in the sidecar's
/// [`FidelityHint`].
///
/// `ByteFidelityProfile`/`LineEnding` (in `document.rs`) deliberately carry **no**
/// serde derives — they are live editor state, not a persisted schema. The sidecar
/// owns its own JSON representation here (mirroring how `settings.rs` owns
/// `BlankLinePolicy`), so the persisted shape is stable and the document type stays
/// serde-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SidecarLineEnding {
    /// Windows-style carriage-return + line-feed (`\r\n`).
    Crlf,
    /// Unix-style line-feed (`\n`).
    Lf,
    /// The file mixed `\r\n` and lone `\n`.
    Mixed,
}

impl From<LineEnding> for SidecarLineEnding {
    fn from(e: LineEnding) -> Self {
        match e {
            LineEnding::Crlf => SidecarLineEnding::Crlf,
            LineEnding::Lf => SidecarLineEnding::Lf,
            LineEnding::Mixed => SidecarLineEnding::Mixed,
        }
    }
}

impl From<SidecarLineEnding> for LineEnding {
    fn from(e: SidecarLineEnding) -> Self {
        match e {
            SidecarLineEnding::Crlf => LineEnding::Crlf,
            SidecarLineEnding::Lf => LineEnding::Lf,
            SidecarLineEnding::Mixed => LineEnding::Mixed,
        }
    }
}

/// Enough of a [`ByteFidelityProfile`] to restore a recovered buffer byte-faithfully
/// (TR-021).
///
/// Carries the load-time line-ending style (plus the never-`Mixed` `dominant` for
/// re-emission), trailing-newline presence, and BOM presence — exactly the facts
/// [`save_bytes`](crate::fileio::save_bytes) needs so a buffer restored from the
/// sidecar re-emits byte-for-byte just like the original save path. The
/// `original_hash` is **not** carried: it pertains to the loaded source bytes (not
/// the autosaved buffer) and is recomputed from the recovered content if needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FidelityHint {
    /// The detected line-ending style of the loaded file.
    pub line_ending: SidecarLineEnding,
    /// The never-`Mixed` style to normalise to on re-emission.
    pub dominant: SidecarLineEnding,
    /// `true` when the loaded file ended with a newline.
    pub had_trailing_newline: bool,
    /// `true` when the loaded bytes began with a UTF-8 BOM.
    pub had_bom: bool,
}

impl From<&ByteFidelityProfile> for FidelityHint {
    fn from(p: &ByteFidelityProfile) -> Self {
        Self {
            line_ending: p.line_ending.into(),
            dominant: p.dominant.into(),
            had_trailing_newline: p.had_trailing_newline,
            had_bom: p.had_bom,
        }
    }
}

impl FidelityHint {
    /// Rebuild a [`ByteFidelityProfile`] from this hint for a recovered `buffer`
    /// (TR-021).
    ///
    /// The `original_hash` is recomputed from the recovered buffer's bytes (re-
    /// emitted through the hint), since the hint deliberately omits the source-byte
    /// hash. The restored profile re-emits the recovered buffer byte-for-byte under
    /// the same [`save_bytes`](crate::fileio::save_bytes) contract as the original.
    #[must_use]
    pub fn to_profile(self, buffer: &str) -> ByteFidelityProfile {
        // Re-emit the recovered buffer through a temporary profile to derive the
        // bytes a faithful save would produce, then capture the canonical profile
        // (so `original_hash` reflects the recovered-and-re-emitted bytes).
        let seed = ByteFidelityProfile {
            line_ending: self.line_ending.into(),
            dominant: self.dominant.into(),
            had_trailing_newline: self.had_trailing_newline,
            had_bom: self.had_bom,
            // Placeholder; replaced by the re-emitted-bytes hash below.
            original_hash: 0,
        };
        let emitted = crate::fileio::save_bytes(buffer, &seed);
        ByteFidelityProfile::from_bytes(&emitted)
    }
}

/// A cheap, stable content marker for divergence detection (TR-008).
///
/// The marker is a length-tagged FNV-1a hash of the UTF-8 bytes. It is **persisted**
/// in the sidecar JSON, so — unlike the document's `DefaultHasher`-based hashes,
/// which are not stable across toolchains — it MUST be computed by a fixed algorithm
/// here so a sidecar written by one build compares correctly on reopen by another.
/// Collisions are astronomically unlikely for editor-sized buffers and the length
/// tag makes trivially-different buffers never alias.
#[must_use]
pub fn content_marker(bytes: &[u8]) -> String {
    // FNV-1a 64-bit — small, dependency-free, and deterministic across builds.
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    // Length-tagged so two different-length buffers never share a marker.
    format!("{:016x}:{}", hash, bytes.len())
}

/// The crash-recovery sidecar record (data-model.md §RecoverySidecar).
///
/// Holds the latest autosaved in-progress buffer plus the identity needed to detect
/// stale-vs-live recovery on reopen. It is serialized to / deserialized from the
/// sibling [`sidecar_path`] as JSON via `serde_json`. The write is crash-safe
/// (TR-022) and never touches the user's file (TR-007).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoverySidecar {
    /// The user file this in-progress work belongs to (the save target). Ties the
    /// sidecar to its source; the sidecar location is derived from it via
    /// [`sidecar_path`]. Untitled buffers have no path → no sidecar (TR-017).
    pub source_path: PathBuf,
    /// The autosaved in-progress UTF-8 buffer — the content offered on restore.
    pub buffer: String,
    /// A content hash of [`buffer`](Self::buffer) for divergence detection: compared
    /// against the on-disk file's content on reopen so same-content recovery is not
    /// offered (TR-008/TR-009).
    pub content_marker: String,
    /// Last-autosave wall-clock time (Unix epoch milliseconds), for the recency /
    /// stale check and the restore-offer affordance (TR-006/TR-008). Best-effort:
    /// `0` if the system clock is before the epoch.
    pub timestamp: u64,
    /// Enough of the load-time [`ByteFidelityProfile`] to restore the recovered
    /// buffer byte-faithfully (TR-021).
    pub fidelity_hint: FidelityHint,
}

impl RecoverySidecar {
    /// Build a sidecar record for `source_path` from the current `buffer` and its
    /// fidelity `profile`.
    ///
    /// The [`content_marker`](Self::content_marker) is computed from the buffer's
    /// UTF-8 bytes and the [`timestamp`](Self::timestamp) is captured now.
    #[must_use]
    pub fn new(source_path: PathBuf, buffer: String, profile: &ByteFidelityProfile) -> Self {
        let content_marker = content_marker(buffer.as_bytes());
        Self {
            source_path,
            content_marker,
            fidelity_hint: FidelityHint::from(profile),
            timestamp: now_millis(),
            buffer,
        }
    }

    /// The byte-fidelity profile to restore the recovered buffer with (TR-021).
    #[must_use]
    pub fn restored_profile(&self) -> ByteFidelityProfile {
        self.fidelity_hint.to_profile(&self.buffer)
    }

    /// Write this sidecar atomically/crash-safely to its sibling path (TR-007/TR-022).
    ///
    /// Serializes the record to JSON (`serde_json`) and writes it through the
    /// **Phase-3 atomic primitive** [`crate::fileio::save_atomic`], which writes a
    /// same-directory temp file and atomically replaces the target. This gives the
    /// sidecar the same crash-safety the user-file save has: a fault during the
    /// write leaves either the prior intact sidecar or no sidecar — never a
    /// corrupt/partial one (TR-022). The target is **always** the sibling
    /// [`sidecar_path`] of [`source_path`](Self::source_path) — this function never
    /// writes the user's file (TR-007).
    ///
    /// Note: `save_atomic` serializes its input through
    /// [`save_bytes`](crate::fileio::save_bytes), which would re-apply line-ending /
    /// trailing-newline transforms. The sidecar body is JSON (no meaningful EOL
    /// fidelity), so we pass a neutral LF/no-trailing-newline/no-BOM profile to keep
    /// the serialized JSON byte-exact; the buffer's own fidelity is preserved
    /// separately in [`fidelity_hint`](Self::fidelity_hint).
    ///
    /// # Errors
    ///
    /// Returns a [`SaveError`] (sidecar prior-intact-or-absent, user file untouched)
    /// if the sidecar's atomic write cannot be committed; see
    /// [`crate::fileio::save_atomic`].
    pub fn write(&self) -> Result<(), SaveError> {
        let path = sidecar_path(&self.source_path);
        self.write_to(&path)
    }

    /// Write this sidecar to an explicit `path` (the testable core of
    /// [`write`](Self::write)).
    ///
    /// Shared by [`write`](Self::write) and by tests so a temp sidecar path can be
    /// injected. The same crash-safe atomic-write guarantee applies.
    ///
    /// # Errors
    ///
    /// Returns a [`SaveError`] if the atomic write cannot be committed.
    pub fn write_to(&self, path: &Path) -> Result<(), SaveError> {
        // Serialization of the plain record cannot fail in practice; map a
        // hypothetical failure to a generic I/O SaveError rather than panicking.
        let json = serde_json::to_string(self)
            .map_err(|e| SaveError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
        // A neutral profile so `save_bytes` leaves the JSON body byte-exact (the
        // sidecar body is JSON; buffer fidelity is preserved in `fidelity_hint`).
        let neutral = ByteFidelityProfile {
            line_ending: LineEnding::Lf,
            dominant: LineEnding::Lf,
            had_trailing_newline: false,
            had_bom: false,
            original_hash: 0,
        };
        save_atomic(&json, &neutral, path)
    }

    /// Load a sidecar record from `path`, or `None` when it is absent or unreadable
    /// / corrupt.
    ///
    /// A missing sidecar is the common (clean) case → `None`. A corrupt/unparsable
    /// sidecar is treated as absent (never offered as recovery) rather than an error
    /// — consistent with the project's corrupt→ignore robustness (project-
    /// instructions §I): a bad sidecar must never block opening the user's file.
    #[must_use]
    pub fn load(path: &Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice::<RecoverySidecar>(&bytes).ok()
    }
}

/// Remove the sidecar for `target`, if present (TR-009).
///
/// Called on a clean save and on a clean exit so a stale/orphan sidecar is never
/// offered on the next open. A missing sidecar is a no-op (the common clean case);
/// any other removal error is returned so the caller can log it best-effort.
///
/// # Errors
///
/// Returns the underlying [`std::io::Error`] if the sidecar exists but cannot be
/// removed.
pub fn remove_sidecar(target: &Path) -> std::io::Result<()> {
    let path = sidecar_path(target);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        // Absent is the clean case — not an error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// The outcome of reopen-time recovery detection (TR-008).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryDetection {
    /// No sidecar present, or the sidecar is stale/same-content — open normally,
    /// offer nothing.
    None,
    /// A live, content-divergent sidecar exists — offer to restore its buffer.
    Offer(RecoverySidecar),
}

/// Detect a live recovery sidecar for `target` and decide whether to offer restore
/// (TR-008/TR-009).
///
/// `on_disk_bytes` is the user file's current on-disk content (the bytes that would
/// otherwise be opened). The decision:
///
/// * No sidecar file → [`RecoveryDetection::None`] (open normally).
/// * Sidecar's `content_marker` matches the on-disk file's content → same content,
///   nothing to recover → [`RecoveryDetection::None`] (suppress stale/same-content,
///   TR-009).
/// * Sidecar diverges from the on-disk file → [`RecoveryDetection::Offer`] (genuine
///   in-progress work to recover, TR-008).
///
/// The comparison is on the **autosaved buffer vs. the on-disk file's editor text**:
/// the on-disk bytes are decoded the way the load path does (drop a leading BOM,
/// normalise EOLs to `\n`) before marking, so a sidecar that holds exactly what is
/// already on disk (e.g. the user saved, the sidecar lingered) is correctly judged
/// same-content and not offered.
#[must_use]
pub fn detect_recovery(target: &Path, on_disk_bytes: &[u8]) -> RecoveryDetection {
    let path = sidecar_path(target);
    let Some(sidecar) = RecoverySidecar::load(&path) else {
        return RecoveryDetection::None;
    };
    // Decode the on-disk bytes into the same editor-buffer form the sidecar holds
    // (drop BOM, normalise EOLs to `\n`) so the markers are comparable.
    let on_disk_buffer = decode_editor_buffer(on_disk_bytes);
    let on_disk_marker = content_marker(on_disk_buffer.as_bytes());
    if sidecar.content_marker == on_disk_marker {
        // Same content as what's on disk → nothing to recover (TR-009).
        RecoveryDetection::None
    } else {
        // Live divergence → offer restore (TR-008).
        RecoveryDetection::Offer(sidecar)
    }
}

/// Decode raw file bytes into the editor buffer form (drop a leading BOM, normalise
/// EOLs to `\n`) — mirrors the document load path so sidecar markers are comparable.
fn decode_editor_buffer(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let without_bom = text.strip_prefix('\u{FEFF}').unwrap_or(&text);
    without_bom.replace("\r\n", "\n").replace('\r', "\n")
}

/// Current wall-clock time as Unix epoch milliseconds (best-effort).
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// The deterministic, frame-driven autosave debounce (AD-004/TR-006/TR-020/TR-025).
///
/// Drives the "should I autosave now?" decision per frame from an **injectable**
/// clock so debounce behavior is testable without wall-clock waits (TR-020). It
/// fires when **either** trigger binds (TR-025):
///
/// * an **idle interval** ([`AutosaveConfig::effective_idle_debounce`]) has elapsed
///   since the last recorded buffer change, **or**
/// * an **edit-count threshold** ([`AutosaveConfig::effective_edit_count_trigger`])
///   of changes has accumulated since the last sidecar write,
///
/// and **only** when the buffer has actually changed since the last sidecar write —
/// tracked via the document's monotonic `edit_generation` (TR-006). A no-op tick
/// (no change since the last write) fires nothing (SC-010).
///
/// The debounce holds only bookkeeping (generations + the last-change instant); the
/// cheap [`poll`](Self::poll) check is what the per-frame path runs, handing the
/// snapshot to an off-frame writer (TR-016/TR-023). [`force_tick`](Self::force_tick)
/// is the deterministic test hook (TR-020).
#[derive(Debug, Clone)]
pub struct AutosaveDebounce {
    /// The autosave configuration (idle interval + edit-count threshold), clamped
    /// through its effective accessors so a corrupt value can never disable the
    /// debounce (TR-025/TR-026).
    config: AutosaveConfig,
    /// The document `edit_generation` recorded at the last buffer-change
    /// observation, or `None` before any change has been noted.
    last_seen_generation: Option<u64>,
    /// The document `edit_generation` recorded at the last sidecar write, or `None`
    /// before any sidecar has been written.
    last_written_generation: Option<u64>,
    /// The instant of the most recent buffer change (for the idle-interval check),
    /// or `None` before any change has been noted.
    last_change_at: Option<Instant>,
    /// Number of edit-generation steps accumulated since the last sidecar write
    /// (for the edit-count trigger).
    edits_since_write: u32,
}

impl AutosaveDebounce {
    /// Create a debounce from the autosave `config`.
    #[must_use]
    pub fn new(config: AutosaveConfig) -> Self {
        Self {
            config,
            last_seen_generation: None,
            last_written_generation: None,
            last_change_at: None,
            edits_since_write: 0,
        }
    }

    /// Replace the autosave configuration (e.g. after a settings change).
    pub fn set_config(&mut self, config: AutosaveConfig) {
        self.config = config;
    }

    /// Record that the buffer changed at `now`, identified by `generation` (the
    /// document's current `edit_generation`).
    ///
    /// Idempotent per generation: re-observing the same generation does not reset
    /// the idle timer or inflate the edit count, so calling this every frame (even
    /// when nothing changed) is safe and cheap. Only a genuinely new generation
    /// advances the bookkeeping.
    pub fn note_change(&mut self, generation: u64, now: Instant) {
        if self.last_seen_generation == Some(generation) {
            return;
        }
        // A real change: reset the idle timer and bump the edit-count accumulator.
        self.last_seen_generation = Some(generation);
        self.last_change_at = Some(now);
        self.edits_since_write = self.edits_since_write.saturating_add(1);
    }

    /// `true` when the buffer has changed since the last sidecar write (TR-006).
    ///
    /// This is the *only-when-changed* gate: a clean / unchanged document never
    /// autosaves, so a no-op tick writes nothing (SC-010).
    #[must_use]
    pub fn has_unsaved_change(&self) -> bool {
        match (self.last_seen_generation, self.last_written_generation) {
            (Some(seen), Some(written)) => seen != written,
            (Some(_), None) => true,
            // No change observed yet → nothing to write.
            (None, _) => false,
        }
    }

    /// The cheap per-frame check: should an autosave fire at `now`?
    ///
    /// Returns `true` when there is an unsaved change AND either trigger binds (idle
    /// interval elapsed OR edit-count threshold reached) — whichever first (TR-025).
    /// This performs **no** I/O; the caller hands the snapshot to the off-frame
    /// writer when it returns `true` (TR-016/TR-023).
    #[must_use]
    pub fn poll(&self, now: Instant) -> bool {
        if !self.has_unsaved_change() {
            return false;
        }
        // Edit-count trigger: enough changes accumulated since the last write.
        if self.edits_since_write >= self.config.effective_edit_count_trigger() {
            return true;
        }
        // Idle trigger: the configured idle interval has elapsed since the last
        // change. `saturating_duration_since` guards against a `now` earlier than
        // the recorded change instant (a non-monotonic injected clock).
        if let Some(changed_at) = self.last_change_at {
            if now.saturating_duration_since(changed_at) >= self.config.effective_idle_debounce() {
                return true;
            }
        }
        false
    }

    /// Mark that a sidecar write for the current observed generation has committed.
    ///
    /// Resets the edit-count accumulator and records the written generation so the
    /// next [`poll`](Self::poll) only fires again after a *new* change (SC-010). Call
    /// after the off-frame writer reports success.
    pub fn mark_written(&mut self) {
        self.last_written_generation = self.last_seen_generation;
        self.edits_since_write = 0;
    }

    /// Deterministic test hook (TR-020): force an autosave decision now, bypassing
    /// the idle/edit-count thresholds but **not** the only-when-changed gate.
    ///
    /// Returns `true` exactly when there is an unsaved change to write — so a forced
    /// tick on an unchanged document still writes nothing (SC-010). This lets a test
    /// exercise the write path without injecting a clock past the idle interval.
    #[must_use]
    pub fn force_tick(&self) -> bool {
        self.has_unsaved_change()
    }
}

/// A request handed to the off-frame autosave worker: the sidecar to write.
struct AutosaveJob {
    sidecar: RecoverySidecar,
}

/// The result of an off-frame sidecar write: the generation it was for and whether
/// the atomic write committed.
#[derive(Debug, Clone, Copy)]
pub struct AutosaveOutcome {
    /// `true` when the sidecar's atomic write committed.
    pub committed: bool,
}

/// A background autosave worker that performs the sidecar write **off** the
/// per-frame path (TR-016/TR-023, SC-008).
///
/// Modelled on [`ReparseWorker`](crate::reparse::ReparseWorker): the per-frame path
/// runs only the cheap [`AutosaveDebounce::poll`] check and, when it fires, hands a
/// [`RecoverySidecar`] snapshot to this worker via [`enqueue`](Self::enqueue). The
/// worker thread performs the atomic, crash-safe sidecar write (the durable-flush /
/// `fsync` cost lives here, never on the render thread) and ships back an
/// [`AutosaveOutcome`] the caller drains with [`poll`](Self::poll). The user's file
/// is never written (TR-007); only the sidecar path is.
///
/// Dropping the worker closes the job channel, so the thread exits cleanly.
pub struct AutosaveWorker {
    /// Outbound job channel; `Option` so it can be dropped before join on shutdown.
    job_tx: Option<Sender<AutosaveJob>>,
    /// Inbound outcome channel from the worker thread.
    result_rx: Receiver<AutosaveOutcome>,
    /// The worker thread handle, joined on drop after the channel closes.
    handle: Option<JoinHandle<()>>,
}

impl AutosaveWorker {
    /// Spawn the background autosave thread.
    #[must_use]
    pub fn new() -> Self {
        let (job_tx, job_rx) = mpsc::channel::<AutosaveJob>();
        let (result_tx, result_rx) = mpsc::channel::<AutosaveOutcome>();

        let handle = std::thread::Builder::new()
            .name("ronin-autosave".to_string())
            .spawn(move || autosave_loop(&job_rx, &result_tx))
            .expect("failed to spawn autosave worker thread");

        Self {
            job_tx: Some(job_tx),
            result_rx,
            handle: Some(handle),
        }
    }

    /// Hand a sidecar snapshot to the worker for an off-frame atomic write
    /// (TR-016/TR-023).
    ///
    /// Non-blocking: the per-frame caller returns immediately; the actual
    /// write/flush happens on the worker thread. If the worker has gone away (only
    /// possible during teardown) the job is silently dropped.
    pub fn enqueue(&self, sidecar: RecoverySidecar) {
        if let Some(tx) = &self.job_tx {
            let _ = tx.send(AutosaveJob { sidecar });
        }
    }

    /// Drain the next finished [`AutosaveOutcome`], if any (non-blocking).
    #[must_use]
    pub fn poll(&self) -> Option<AutosaveOutcome> {
        self.result_rx.try_recv().ok()
    }
}

impl Default for AutosaveWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AutosaveWorker {
    fn drop(&mut self) {
        // Close the job channel so the worker's blocking `recv` returns and exits.
        self.job_tx = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The autosave worker thread body: block on jobs, write the sidecar atomically
/// off-frame, ship the outcome back.
fn autosave_loop(job_rx: &Receiver<AutosaveJob>, result_tx: &Sender<AutosaveOutcome>) {
    while let Ok(job) = job_rx.recv() {
        // The atomic, crash-safe sidecar write — off the render thread (TR-016). A
        // failure leaves the prior sidecar intact-or-absent and never the user file
        // (TR-007/TR-022); we report it as a non-committed outcome rather than
        // panicking the worker.
        let committed = job.sidecar.write().is_ok();
        if result_tx.send(AutosaveOutcome { committed }).is_err() {
            // Consumer gone — stop working.
            break;
        }
    }
}
