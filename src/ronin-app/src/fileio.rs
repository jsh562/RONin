//! Filesystem entry points for opening RON files (FR-018).
//!
//! [`open_path`] reads a file's raw bytes, validates UTF-8 at the boundary, and
//! builds an [`EditorDocument`]. Non-UTF-8 input is rejected cleanly with
//! [`OpenError::NotUtf8`] — **no document is created** — honouring "never corrupt
//! user data" (project-instructions §I): the editor refuses to silently lossy-
//! decode a binary or non-UTF-8 file.
//!
//! # Atomic save (E007 / OBJ1)
//!
//! The Save path is the crash-safe persistence contract from project-instructions
//! §I — **atomic save**: serialize through the byte-fidelity re-emission
//! ([`save_bytes`]), write to a temp file in the **same directory** as the target,
//! flush it durably, then atomically replace the target ([`save_atomic`], TR-001/
//! TR-002). The original file is never modified until the replace commits, so any
//! failure (disk full, permission denied, partial write, crash) leaves it
//! byte-identical and the failure is surfaced ([`SaveError`], TR-003). Sidecar
//! crash recovery and undo/redo land in later E007 objectives.

use std::path::Path;

use atomicwrites::{AtomicFile, Error as AtomicError, OverwriteBehavior};

use crate::document::{ByteFidelityProfile, EditorDocument, LineEnding};

/// Why opening a file failed (FR-018).
///
/// All variants are error-severity. `NotUtf8` means the bytes were read but are
/// not valid UTF-8; `Io` wraps any filesystem read failure (missing file,
/// permission denied, etc.).
#[derive(Debug)]
#[non_exhaustive]
pub enum OpenError {
    /// The file's bytes are not valid UTF-8; no document was created.
    NotUtf8,
    /// A filesystem read error occurred.
    Io(std::io::Error),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::NotUtf8 => f.write_str("not valid UTF-8"),
            OpenError::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for OpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            OpenError::NotUtf8 => None,
            OpenError::Io(e) => Some(e),
        }
    }
}

impl From<std::io::Error> for OpenError {
    fn from(e: std::io::Error) -> Self {
        OpenError::Io(e)
    }
}

/// Read `path` and build an [`EditorDocument`], rejecting non-UTF-8 (FR-018).
///
/// # Errors
///
/// * [`OpenError::Io`] if the file cannot be read.
/// * [`OpenError::NotUtf8`] if the bytes are not valid UTF-8 (no document is
///   created in this case).
pub fn open_path(path: &Path) -> Result<EditorDocument, OpenError> {
    let raw = std::fs::read(path)?;

    // Validate UTF-8 at the boundary using `ron-core`'s validator; reject cleanly
    // without constructing a document if the bytes are not valid UTF-8.
    if ron_core::validate_utf8(&raw).is_err() {
        return Err(OpenError::NotUtf8);
    }

    // UTF-8 is confirmed; `from_loaded` re-decodes (infallibly here) and captures
    // the byte-fidelity profile.
    EditorDocument::from_loaded(path, &raw).map_err(|_| OpenError::NotUtf8)
}

/// The UTF-8 BOM byte sequence (`EF BB BF`).
const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Why an atomic save failed (TR-003, E007/OBJ1).
///
/// Every variant carries the same hard guarantee: **the original target file is
/// byte-identical to before the save** (project-instructions §I). The atomic
/// pipeline ([`save_atomic`]) writes a same-directory temp file and only ever
/// touches the target through the atomic replace primitive, so a failure at any
/// stage leaves the original untouched and the buffer dirty (no silent success).
///
/// The variants name the atomic-save failure surface required by TR-003: disk
/// full, permission denied, a partial/interrupted temp write, a failed atomic
/// replace, and the same-filesystem-impossible degrade-and-surface case (TR-005).
/// The set is `#[non_exhaustive]` so later save modes can be added without a
/// breaking change. Every variant wraps the underlying [`std::io::Error`] so the
/// original OS detail is preserved for the user-facing notice and `source()`.
#[derive(Debug)]
#[non_exhaustive]
pub enum SaveError {
    /// The target's filesystem is full (no space left to write the temp file).
    DiskFull(std::io::Error),
    /// The target (or its directory) denied write/replace permission.
    PermissionDenied(std::io::Error),
    /// The temp-file write was interrupted before it was fully written; the
    /// target was never touched, so the original is intact.
    PartialWrite(std::io::Error),
    /// The temp file was written and flushed, but the atomic replace of the
    /// target failed; the original target still holds its pre-save bytes.
    ReplaceFailed(std::io::Error),
    /// The same-filesystem temp could not be established (e.g. the parent
    /// directory is unwritable / missing), so an atomic replace cannot be
    /// performed; surfaced rather than falling back to a non-atomic write
    /// (TR-005). The original target, if any, is untouched.
    SameFilesystemImpossible(std::io::Error),
    /// Any other filesystem write failure; the on-disk file may be unchanged.
    Io(std::io::Error),
}

impl SaveError {
    /// The underlying I/O error this save failure wraps.
    #[must_use]
    pub fn io(&self) -> &std::io::Error {
        match self {
            SaveError::DiskFull(e)
            | SaveError::PermissionDenied(e)
            | SaveError::PartialWrite(e)
            | SaveError::ReplaceFailed(e)
            | SaveError::SameFilesystemImpossible(e)
            | SaveError::Io(e) => e,
        }
    }

    /// Classify a raw [`std::io::Error`] from the atomic pipeline into the most
    /// specific [`SaveError`] variant (disk-full / permission / partial / generic).
    ///
    /// `stage` records whether the error came from establishing or writing the
    /// same-directory temp file ([`Stage::Temp`]) or from the atomic replace of
    /// the target ([`Stage::Replace`]), so a permission/space failure is reported
    /// with the right replace-vs-write framing while the original-intact guarantee
    /// holds either way.
    fn from_io(e: std::io::Error, stage: Stage) -> Self {
        use std::io::ErrorKind;
        // Disk-full has its own ErrorKind on recent toolchains; older kernels may
        // surface it via the raw errno (ENOSPC = 28), so check both.
        let is_disk_full = matches!(e.kind(), ErrorKind::StorageFull)
            || e.raw_os_error() == Some(28)
            // Windows: ERROR_DISK_FULL (112), ERROR_HANDLE_DISK_FULL (39).
            || matches!(e.raw_os_error(), Some(112) | Some(39));
        if is_disk_full {
            return SaveError::DiskFull(e);
        }
        if e.kind() == ErrorKind::PermissionDenied {
            return SaveError::PermissionDenied(e);
        }
        match stage {
            // A temp-write failure that is neither disk-full nor permission is an
            // interrupted/partial temp write — the target is still untouched.
            Stage::Temp => SaveError::PartialWrite(e),
            // A replace-stage failure means the temp was written but the atomic
            // swap did not commit; the original target keeps its pre-save bytes.
            Stage::Replace => SaveError::ReplaceFailed(e),
        }
    }
}

/// Which stage of the atomic pipeline an I/O error came from, used to frame the
/// resulting [`SaveError`] (temp-write vs. atomic replace).
#[derive(Debug, Clone, Copy)]
enum Stage {
    /// Establishing or writing the same-directory temp file.
    Temp,
    /// The atomic replace of the target.
    Replace,
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::DiskFull(e) => write!(f, "disk full: {e}"),
            SaveError::PermissionDenied(e) => write!(f, "permission denied: {e}"),
            SaveError::PartialWrite(e) => write!(f, "write interrupted: {e}"),
            SaveError::ReplaceFailed(e) => write!(f, "atomic replace failed: {e}"),
            SaveError::SameFilesystemImpossible(e) => {
                write!(f, "atomic save not possible at this location: {e}")
            }
            SaveError::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for SaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.io())
    }
}

impl From<std::io::Error> for SaveError {
    fn from(e: std::io::Error) -> Self {
        SaveError::Io(e)
    }
}

/// Re-emit `buffer` to raw bytes per the load-time fidelity `profile` (FR-020/FR-023).
///
/// The editor's `TextEdit` normalises every line ending in the live buffer to a
/// single `\n`. To honour "never corrupt user data" (project-instructions §I) on
/// save, this re-applies the file's original byte fidelity:
///
/// * **Line endings.** For a uniform file the original style is re-emitted
///   verbatim (`\n` → `\r\n` for a CRLF file; left as `\n` for an LF file). For a
///   genuinely `Mixed` file the [`dominant`](ByteFidelityProfile::dominant) style
///   is re-emitted (ties resolve to LF), so a mixed input is normalised to a
///   single, predictable convention — the documented, intended limitation of
///   FR-020/FR-023 (a mixed file does **not** round-trip byte-for-byte).
/// * **Trailing newline.** Re-applied iff the original ended in one; if the
///   buffer already ends in a newline but the original did not, the trailing
///   newline is dropped, and vice versa.
/// * **BOM.** A leading UTF-8 BOM is re-emitted iff the original carried one.
///
/// The output is always valid UTF-8.
#[must_use]
pub fn save_bytes(buffer: &str, profile: &ByteFidelityProfile) -> Vec<u8> {
    // Choose the concrete EOL to emit. Uniform files keep their style; a Mixed
    // file normalises to `dominant` (which is never `Mixed`; ties → LF).
    let emit_crlf = match profile.line_ending {
        LineEnding::Crlf => true,
        LineEnding::Lf => false,
        LineEnding::Mixed => matches!(profile.dominant, LineEnding::Crlf),
    };

    // Normalise the buffer to bare `\n` first (defensive: the widget already does
    // this, but a stray `\r\n` in the buffer must not become `\r\r\n`), then
    // re-emit each `\n` in the chosen style.
    let normalised = normalise_to_lf(buffer);

    // Decide the trailing newline: strip any the buffer carries, then re-add iff
    // the original had one. This makes "no trailing newline" round-trip too.
    let core = normalised.strip_suffix('\n').unwrap_or(&normalised);
    let mut emitted = if emit_crlf {
        core.replace('\n', "\r\n")
    } else {
        core.to_string()
    };
    if profile.had_trailing_newline {
        emitted.push_str(if emit_crlf { "\r\n" } else { "\n" });
    }

    let mut out = Vec::with_capacity(emitted.len() + if profile.had_bom { BOM.len() } else { 0 });
    if profile.had_bom {
        out.extend_from_slice(&BOM);
    }
    out.extend_from_slice(emitted.as_bytes());
    out
}

/// Normalise any CRLF/CR in `s` to bare LF so re-emission starts from one style.
fn normalise_to_lf(s: &str) -> String {
    // First collapse CRLF, then any remaining lone CR (defensive; RON sources are
    // LF/CRLF in practice). Avoids `\r\r\n` artefacts on re-emit.
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// Atomically save `buffer` to `path`, preserving load-time byte fidelity
/// (E007/OBJ1 — TR-001/TR-002/TR-004; **[COMPLETES TR-002]**).
///
/// The crash-safe save pipeline, the seam project-instructions §I's "never corrupt
/// user data" mandate is built on:
///
/// 1. **Serialize** the buffer through [`save_bytes`] with the load-time
///    [`ByteFidelityProfile`], so the original line-ending style, UTF-8 BOM, and
///    trailing-newline presence are re-emitted byte-for-byte (TR-004). The atomic
///    path never bypasses this re-emission, so it does not regress E003's fidelity.
/// 2. **Temp-write + durable flush + atomic replace** via [`atomicwrites`]: the
///    crate writes a temp file in a randomized subdirectory of the **target's own
///    directory** (so the temp is always on the *same filesystem* as the target —
///    TR-005 — and a cross-filesystem, non-atomic replace can never happen
///    silently), `fsync`s the temp file, then atomically replaces the target. On
///    Windows it uses `MoveFileExW` with `MOVEFILE_WRITE_THROUGH |
///    MOVEFILE_REPLACE_EXISTING` (the platform replace-over-existing primitive,
///    TR-002, AD-001); on POSIX it `renameat`s and `fsync`s the parent
///    directory/-ies (file + directory durable flush, AD-006).
///
/// **Original-intact guarantee (TR-001/TR-003/TR-019).** The original target is
/// never opened for writing; it is only ever swapped by the atomic replace. So if
/// any step fails — disk full, permission denied, an interrupted temp write, or a
/// failed replace — the original is byte-identical to before the call and a
/// [`SaveError`] is returned (the caller keeps the buffer dirty; no silent
/// success). A residual temp file lives in a `.atomicwrite*` subdirectory of the
/// target's directory, never *at* the target path, and is a cleanable non-target
/// artifact (TR-019b).
///
/// **Durable-flush policy (TR-028, AD-006).** This is the *explicit-save* path and
/// it performs the full durable flush (file `fsync` on all platforms; parent
/// directory `fsync` on POSIX; on Windows the `MOVEFILE_WRITE_THROUGH` replace
/// primitive provides the ordering/durability guarantee — there is no directory
/// `fsync` on Windows). It is **not** on the per-keystroke edit path, so there is
/// no per-keystroke `fsync` (the autosave sidecar's reduced-flush path is OBJ2).
///
/// **Same-filesystem constraint (TR-005, T012).** The temp stays in the target's
/// directory, so the same-filesystem requirement holds by construction. A location
/// where the atomic replace cannot hold (e.g. an unwritable/missing parent
/// directory) surfaces as a [`SaveError`] — there is no silent non-atomic
/// `std::fs::write` fallback.
///
/// **Local-only (TR-015).** This path touches only the local filesystem (the
/// [`atomicwrites`] crate and `std::fs`); it introduces no network or transport
/// dependency. The `network_audit` regression test (`tests/offline_logging.rs`)
/// and `cargo deny` guard the dependency graph.
///
/// # Downstream reuse (E005 / E008 — TR-013)
///
/// This is the single, reusable save seam the downstream editing epics build on:
/// **E005** (smart authoring — e.g. format-on-save and other transform-on-write
/// flows) and **E008** (structural / table editing). Call it with the edited
/// buffer plus the document's load-time [`ByteFidelityProfile`] to persist any
/// transformed text *without* re-deriving the atomic / fidelity / durability
/// guarantees. Two contracts those epics may rely on:
///
/// * **Byte-fidelity is preserved by default.** `save_atomic` re-emits through
///   [`save_bytes`] (EOL style, BOM, trailing-newline) and applies **no**
///   reformatting of its own. A transform-on-write feature (E005) supplies the
///   already-transformed `buffer`; the save path stays byte-faithful and never
///   silently rewrites bytes the caller did not change.
/// * **Original-intact-until-commit.** The target is only ever swapped by the
///   atomic replace, so a failed save leaves the user's file byte-identical and
///   returns a [`SaveError`] — downstream callers keep their buffer dirty and
///   surface the error rather than assuming success (no optimistic re-baseline).
///
/// # Errors
///
/// Returns a [`SaveError`] (with the original target intact) when the temp file
/// cannot be created/written (disk full, permission denied, interrupted), when the
/// same-filesystem temp cannot be established (TR-005), or when the atomic replace
/// fails. The variant names the failure surface (TR-003).
pub fn save_atomic(
    buffer: &str,
    profile: &ByteFidelityProfile,
    path: &Path,
) -> Result<(), SaveError> {
    // TR-004 / HINT-003: serialize through the byte-fidelity re-emission so CRLF,
    // BOM, and trailing-newline fidelity survive the atomic path unchanged.
    let bytes = save_bytes(buffer, profile);

    // TR-005 / T012: `AtomicFile::new` keeps the temp in the target's own
    // directory (same filesystem). If the target has no parent directory we cannot
    // establish a same-directory temp, so surface it rather than silently degrade.
    if path.parent().is_none() {
        return Err(SaveError::SameFilesystemImpossible(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "target path has no parent directory; cannot place a same-filesystem temp",
        )));
    }

    // Temp-write (same-dir) → durable flush → atomic replace (TR-001/TR-002).
    let af = AtomicFile::new(path, OverwriteBehavior::AllowOverwrite);
    let result: Result<(), AtomicError<std::io::Error>> =
        af.write(|f| std::io::Write::write_all(f, &bytes));

    match result {
        Ok(()) => Ok(()),
        Err(AtomicError::User(e)) => {
            // The user callback (`write_all` into the temp) failed: the target was
            // never touched — this is a temp-write / partial-write failure.
            Err(SaveError::from_io(e, Stage::Temp))
        }
        Err(AtomicError::Internal(e)) => {
            // Library-internal: either creating the same-dir temp subdir/file, or
            // the atomic move into place. We cannot distinguish the two from the
            // error alone, so classify by errno (disk-full / permission) and frame
            // the residual category as a replace failure — in every case the
            // ORIGINAL target is intact (it is only ever swapped atomically).
            Err(SaveError::from_io(e, Stage::Replace))
        }
    }
}

/// Atomically write `doc` to `path`, re-emitting the load-time byte fidelity
/// (E007/OBJ1 — TR-001; **[COMPLETES TR-001 via T018]**).
///
/// Routes through [`save_atomic`]: serialize via [`save_bytes`] (so EOL style, BOM,
/// and trailing-newline presence are honoured — TR-004), then write atomically
/// (same-directory temp → durable flush → atomic replace). Success is reported
/// only after the durable atomic replace commits; on any failure the original file
/// is byte-identical and a [`SaveError`] is returned (the caller keeps the buffer
/// dirty — TR-003). This is the explicit-save durable-flush path (TR-028); it is
/// not on the per-keystroke edit path.
///
/// The untitled / Save-As path uses this same function once a target path is
/// chosen, so it follows the identical atomic, byte-faithful contract (TR-017).
///
/// # Errors
///
/// Returns a [`SaveError`] (original target intact) if the atomic save cannot be
/// committed; see [`save_atomic`].
pub fn save_document(doc: &EditorDocument, path: &Path) -> Result<(), SaveError> {
    save_atomic(&doc.buffer, &doc.byte_profile, path)
}

// ===========================================================================
// E010 US1 — non-destructive RON→JSON export (T015, FR-003/008/013).
// ===========================================================================
//
// The export path writes the converted JSON/JSONC text to a user-chosen target
// via the SAME crash-safe atomic pipeline as a RON save (`save_atomic`), so the
// SOURCE document is never touched (FR-003) — the converted text is supplied by
// the caller, not read from the source's buffer. In strict mode, comments survive
// in a deterministic sibling sidecar map (`<target>.comments.json`, same
// directory) written next to the target; the sidecar path is derived purely from
// the target file name and can never resolve outside the target's directory
// (FR-008). Non-UTF-8 cannot occur on this path — the converted text is always
// valid UTF-8 String — but the sidecar JSON is serialized losslessly.

/// The fixed sidecar suffix appended to the export target's file name (FR-008).
///
/// The sidecar is a deterministic sibling: `world.json` → `world.json.comments.json`
/// in the SAME directory. A fixed suffix on the target's own name keeps the path
/// derivation local and predictable.
const SIDECAR_SUFFIX: &str = ".comments.json";

/// Why a JSON/JSONC export (or its sidecar) failed (E010 US1 — T015).
///
/// Wraps the [`SaveError`] from the atomic pipeline plus the boundary rejections
/// the export adds: an export target with no parent directory (no place for a
/// deterministic sibling sidecar) and a sidecar path that would escape the
/// target's directory (defensive — the suffix derivation makes this impossible,
/// but it is checked rather than assumed, FR-008).
#[derive(Debug)]
#[non_exhaustive]
pub enum ExportError {
    /// The atomic write of the JSON/JSONC target failed; the original (if any) is
    /// byte-identical (the atomic pipeline only swaps on a committed replace).
    Save(SaveError),
    /// The atomic write of the sibling sidecar comment map failed; the JSON target
    /// was written successfully, but its companion comment map was not.
    Sidecar(SaveError),
    /// The export target has no parent directory, so a deterministic same-directory
    /// sibling sidecar cannot be placed (and the atomic save cannot run either).
    NoParentDirectory,
    /// The derived sidecar path would resolve outside the target's own directory
    /// (via `..`, an absolute component, or a name separator) — refused so the
    /// converter never widens scope beyond the chosen target (FR-008).
    SidecarEscapesDirectory,
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::Save(e) => write!(f, "JSON export failed: {e}"),
            ExportError::Sidecar(e) => write!(f, "comment sidecar write failed: {e}"),
            ExportError::NoParentDirectory => f.write_str("export target has no parent directory"),
            ExportError::SidecarEscapesDirectory => {
                f.write_str("comment sidecar path would escape the target directory")
            }
        }
    }
}

impl std::error::Error for ExportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ExportError::Save(e) | ExportError::Sidecar(e) => Some(e),
            ExportError::NoParentDirectory | ExportError::SidecarEscapesDirectory => None,
        }
    }
}

/// Derive the deterministic sibling sidecar path for an export `target`, refusing
/// any path that would escape the target's directory (FR-008, E010 US1 — T015).
///
/// The sidecar is the target's file name plus [`SIDECAR_SUFFIX`], placed in the
/// **same directory** as the target: `dir/world.json` →
/// `dir/world.json.comments.json`. The derivation is purely lexical on the target's
/// own file name, so it can never be taken from document content. As a defensive
/// belt-and-suspenders check it verifies the result stays in the target's directory
/// — a target file name containing a separator / `..` / an absolute component
/// (which a legitimate `file_name()` cannot, but a hostile path might) is refused.
///
/// # Errors
///
/// * [`ExportError::NoParentDirectory`] when `target` has no parent directory.
/// * [`ExportError::SidecarEscapesDirectory`] when the derived sidecar would not be
///   a direct child of the target's directory.
pub fn sidecar_path(target: &Path) -> Result<std::path::PathBuf, ExportError> {
    let dir = target.parent().ok_or(ExportError::NoParentDirectory)?;
    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or(ExportError::SidecarEscapesDirectory)?;
    // The sibling name is purely the target's own file name + the fixed suffix.
    let sidecar_name = format!("{file_name}{SIDECAR_SUFFIX}");
    // Defensive: the sibling name must be a single path component (no separator,
    // no `..`, not absolute) so it can only land directly in the target's dir.
    let candidate = Path::new(&sidecar_name);
    if candidate.is_absolute() || candidate.components().count() != 1 || sidecar_name.contains("..")
    {
        return Err(ExportError::SidecarEscapesDirectory);
    }
    let sidecar = dir.join(&sidecar_name);
    // Final guard: the resolved sidecar's parent must be the target's directory.
    if sidecar.parent() != Some(dir) {
        return Err(ExportError::SidecarEscapesDirectory);
    }
    Ok(sidecar)
}

/// Export converted JSON/JSONC `text` to `target` atomically (source untouched),
/// optionally writing the strict-mode comment sidecar (E010 US1 — T015, FR-003/008).
///
/// The non-destructive RON→JSON export seam:
///
/// 1. **Atomic JSON write.** The converted `text` is written to `target` through
///    [`save_atomic`] — the same crash-safe temp-write + durable-flush + atomic-
///    replace pipeline a RON save uses. The byte-fidelity profile is derived from
///    the converted text itself (it is fresh output, valid UTF-8, LF), so the bytes
///    reach disk verbatim. The SOURCE document is never opened — only `target` is
///    written (FR-003).
/// 2. **Sidecar comment map (strict mode only).** When `sidecar` is `Some` (the
///    caller passes the carrier's [`sidecar_map`](crate::interop::CommentCarrier::sidecar_map)
///    in strict-with-sidecar mode), it is serialized to deterministic JSON and
///    written — also atomically — to the deterministic sibling
///    [`sidecar_path`] (`<target>.comments.json`, same directory). A `None` / empty
///    sidecar writes nothing (JSONC carries comments inline; pure-JSON drops them).
///
/// # Errors
///
/// * [`ExportError::Save`] if the JSON target write fails (the original target, if
///   any, is byte-identical — atomic guarantee).
/// * [`ExportError::Sidecar`] if the sidecar write fails (the JSON target was
///   written successfully).
/// * [`ExportError::NoParentDirectory`] / [`ExportError::SidecarEscapesDirectory`]
///   from [`sidecar_path`] when a sidecar is requested but its path is unsafe.
pub fn export_json(
    text: &str,
    target: &Path,
    sidecar: Option<&std::collections::BTreeMap<String, Vec<String>>>,
) -> Result<(), ExportError> {
    // The export is non-destructive over the source: only `target` is written.
    // The converted text is fresh output — build its fidelity profile from its own
    // bytes so the atomic re-emission writes it verbatim (no reflow).
    let bytes = text.as_bytes().to_vec();
    let profile = ByteFidelityProfile::from_bytes(&bytes);
    save_atomic(text, &profile, target).map_err(ExportError::Save)?;

    // Strict-mode sidecar: write the comment map as a deterministic sibling. An
    // empty map writes nothing (the carrier supplied no comments to preserve).
    if let Some(map) = sidecar {
        if !map.is_empty() {
            let sidecar = sidecar_path(target)?;
            // The sidecar is canonical pretty JSON (deterministic; map key order is
            // the BTreeMap's sorted order, so the file is reproducible).
            let json = serde_json::to_string_pretty(map).unwrap_or_else(|_| "{}".to_string());
            let json_with_newline = format!("{json}\n");
            let sidecar_bytes = json_with_newline.as_bytes().to_vec();
            let sidecar_profile = ByteFidelityProfile::from_bytes(&sidecar_bytes);
            save_atomic(&json_with_newline, &sidecar_profile, &sidecar)
                .map_err(ExportError::Sidecar)?;
        }
    }
    Ok(())
}

// ===========================================================================
// E010 US2 — JSON/JSONC import → reconstructed RON (T022, FR-002/008/009/013).
// ===========================================================================
//
// The import path reads a JSON / JSONC file, validates UTF-8 at the boundary
// (rejecting non-UTF-8 with no document created, like `open_path`), parses it
// JSONC-tolerantly (stripping + anchoring inline comments), reads comments back
// from BOTH the JSONC inline stream AND a deterministic sibling sidecar
// (`<input>.comments.json`) when present (round-trip symmetry, FR-008), and
// reconstructs RON via `json_to_ron` — schema-aware when a binding consultation is
// supplied, deterministic best-effort otherwise (FR-009). Malformed JSON degrades
// to a clear error and NO document (FR-013).

use crate::binding::JsonToRonConsultation;
use crate::interop::comments::{Comment, CommentCarrier, CommentKind, CommentMode};
use crate::interop::{json_to_ron, JsonToRon};

/// Why a JSON/JSONC import failed (E010 US2 — T022, FR-013).
///
/// Every variant creates **no** document and corrupts nothing — the source JSON is
/// only read (FR-013). `NotUtf8` rejects a non-UTF-8 file at the boundary;
/// `MalformedJson` carries the JSONC reader's parse message; `Io` wraps a read
/// failure.
#[derive(Debug)]
#[non_exhaustive]
pub enum ImportError {
    /// The input file's bytes are not valid UTF-8; no document was created.
    NotUtf8,
    /// The input could not be parsed as JSON/JSONC; no document was created.
    MalformedJson(String),
    /// A filesystem read error occurred reading the input file.
    Io(std::io::Error),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::NotUtf8 => f.write_str("not valid UTF-8"),
            ImportError::MalformedJson(msg) => write!(f, "malformed JSON: {msg}"),
            ImportError::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for ImportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ImportError::NotUtf8 | ImportError::MalformedJson(_) => None,
            ImportError::Io(e) => Some(e),
        }
    }
}

/// The successful outcome of a JSON/JSONC import: the reconstructed RON text plus
/// the converter's residual-ambiguity notes (E010 US2 — T022, FR-002/009).
#[derive(Debug)]
pub struct ImportedRon {
    /// The reconstructed RON source text (ready to open in a new tab or commit in
    /// place).
    pub ron_text: String,
    /// Residual-ambiguity notes from the best-effort / unbound reconstruction
    /// (FR-009). Empty on a fully schema-aware import.
    pub notes: Vec<String>,
}

/// Read + reconstruct RON from the bytes of a JSON/JSONC document, honouring an
/// optional sidecar comment map (E010 US2 — T022, FR-002/008/009/013).
///
/// * `raw` — the input file bytes (UTF-8 validated here).
/// * `sidecar` — the deterministic sibling sidecar map (JSON-Pointer → comment
///   texts) when one exists beside the input, else `None` (FR-008).
/// * `consultation` — the bound `TypeModel` + root type for schema-aware
///   reconstruction, or `None` for best-effort (FR-009).
///
/// Comments are read back from BOTH the JSONC inline stream AND the sidecar (their
/// anchored pointers merge into one carrier), so a RON→JSON→RON round trip restores
/// comments in either carrier (FR-008).
///
/// # Errors
///
/// * [`ImportError::NotUtf8`] when the bytes are not valid UTF-8 (no document).
/// * [`ImportError::MalformedJson`] when the JSONC reader rejects the input (no
///   document) — degrade-safe (FR-013).
pub fn reconstruct_ron_from_bytes(
    raw: &[u8],
    sidecar: Option<&std::collections::BTreeMap<String, Vec<String>>>,
    consultation: Option<&JsonToRonConsultation>,
) -> Result<ImportedRon, ImportError> {
    // Validate UTF-8 at the boundary; reject without creating anything (FR-013).
    if ron_core::validate_utf8(raw).is_err() {
        return Err(ImportError::NotUtf8);
    }
    let text = std::str::from_utf8(raw).map_err(|_| ImportError::NotUtf8)?;

    // Parse JSONC: strip + anchor inline comments; serde_json validates the JSON.
    let parsed =
        crate::interop::parse_jsonc(text).map_err(|e| ImportError::MalformedJson(e.to_string()))?;

    // Merge inline comments with any sidecar comments into one anchored list. JSONC
    // is the primary carrier; the sidecar supplies anchors a strict-JSON file kept
    // separately (FR-008). Both are read back (round-trip symmetry).
    let mut comments: Vec<Comment> = parsed.comments;
    if let Some(map) = sidecar {
        for (pointer, texts) in map {
            for ct in texts {
                comments.push(Comment {
                    text: ct.clone(),
                    kind: classify_comment(ct),
                    source_range: ron_core::TextRange::new(0usize, 0usize),
                    anchor_pointer: pointer.clone(),
                });
            }
        }
    }
    let carrier = CommentCarrier::from_comments(CommentMode::JsoncInline, comments);

    // Reconstruct RON — schema-aware when bound, best-effort otherwise (FR-009).
    let binding = consultation.map(JsonToRonConsultation::as_binding);
    let JsonToRon {
        text: ron_text,
        notes,
        ..
    } = json_to_ron(&parsed.value, binding, Some(&carrier));
    Ok(ImportedRon { ron_text, notes })
}

/// Read a JSON/JSONC file from `path`, consult its deterministic sibling sidecar
/// when present, and reconstruct RON (E010 US2 — T022, FR-002/008/009/013).
///
/// The on-disk entry point: reads `path`, looks for `<path>.comments.json` beside it
/// (the deterministic sibling derived by [`sidecar_path`]), and forwards both to
/// [`reconstruct_ron_from_bytes`]. The input JSON is never modified — only read
/// (FR-002). A missing / unreadable / malformed sidecar is simply ignored (the
/// inline JSONC comments still round-trip); the input itself failing is an error
/// with no document created (FR-013).
///
/// # Errors
///
/// * [`ImportError::Io`] when the input file cannot be read.
/// * [`ImportError::NotUtf8`] / [`ImportError::MalformedJson`] from
///   [`reconstruct_ron_from_bytes`].
pub fn import_json(
    path: &Path,
    consultation: Option<&JsonToRonConsultation>,
) -> Result<ImportedRon, ImportError> {
    let raw = std::fs::read(path).map_err(ImportError::Io)?;
    // Read the deterministic sibling sidecar comment map when one exists (FR-008).
    let sidecar = read_sidecar(path);
    reconstruct_ron_from_bytes(&raw, sidecar.as_ref(), consultation)
}

/// Read the deterministic sibling sidecar comment map for an input `path`, when one
/// exists and parses (E010 US2 — T022, FR-008).
///
/// The sidecar consulted on read is the deterministic sibling of the input file
/// (`<input>.comments.json`, same directory) — the exact path [`export_json`] writes
/// in strict mode. A missing / unreadable / malformed sidecar returns `None` (the
/// inline JSONC comments still round-trip); the sidecar never widens the read scope
/// beyond the input's directory (the path is derived purely from the input name).
#[must_use]
pub fn read_sidecar(path: &Path) -> Option<std::collections::BTreeMap<String, Vec<String>>> {
    let sidecar = sidecar_path(path).ok()?;
    let bytes = std::fs::read(&sidecar).ok()?;
    serde_json::from_slice::<std::collections::BTreeMap<String, Vec<String>>>(&bytes).ok()
}

/// Classify a sidecar comment's text into line vs block by its delimiters (FR-008).
fn classify_comment(text: &str) -> CommentKind {
    if text.trim_start().starts_with("/*") {
        CommentKind::Block
    } else {
        CommentKind::Line
    }
}

#[cfg(test)]
mod export_tests {
    //! T015 — non-destructive JSON/JSONC export + deterministic sidecar (FR-003/008).

    use super::*;
    use std::collections::BTreeMap;

    /// A fresh unique temp directory for an export test.
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ronin_export_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn sidecar_path_is_a_deterministic_same_dir_sibling() {
        let target = Path::new("/some/dir/world.json");
        let sidecar = sidecar_path(target).expect("derived");
        assert_eq!(
            sidecar,
            Path::new("/some/dir/world.json.comments.json"),
            "the sidecar is `<target>.comments.json` in the same directory"
        );
    }

    #[test]
    fn sidecar_path_rejects_a_target_with_no_parent() {
        // A bare relative file name with no directory component has a parent of "",
        // which is still a (current-dir) parent; a root has no parent.
        assert!(matches!(
            sidecar_path(Path::new("/")),
            Err(ExportError::NoParentDirectory)
        ));
    }

    #[test]
    fn export_writes_json_and_leaves_no_source_touched() {
        let dir = temp_dir("json_only");
        let target = dir.join("out.json");
        export_json("{\n  \"a\": 1\n}\n", &target, None).expect("export ok");
        let written = std::fs::read_to_string(&target).expect("target written");
        assert!(written.contains("\"a\": 1"));
        // No sidecar was requested → none exists.
        assert!(
            !dir.join("out.json.comments.json").exists(),
            "no sidecar without a comment map"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strict_export_writes_the_deterministic_sidecar() {
        let dir = temp_dir("with_sidecar");
        let target = dir.join("strict.json");
        let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        map.insert("/a".to_string(), vec!["// about a".to_string()]);
        export_json("{\n  \"a\": 1\n}\n", &target, Some(&map)).expect("export ok");
        let sidecar = dir.join("strict.json.comments.json");
        assert!(sidecar.exists(), "the deterministic sidecar is written");
        let body = std::fs::read_to_string(&sidecar).expect("sidecar readable");
        assert!(body.contains("/a") && body.contains("// about a"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_sidecar_map_writes_nothing() {
        let dir = temp_dir("empty_sidecar");
        let target = dir.join("strict.json");
        let map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        export_json("{}\n", &target, Some(&map)).expect("export ok");
        assert!(
            !dir.join("strict.json.comments.json").exists(),
            "an empty comment map writes no sidecar"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod import_tests {
    //! T022 — JSON/JSONC import → reconstructed RON + sidecar read-back (FR-002/008/013).

    use super::*;
    use std::collections::BTreeMap;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ronin_import_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn imports_plain_json_to_ron() {
        let r = reconstruct_ron_from_bytes(b"{ \"name\": \"hero\", \"level\": 3 }", None, None)
            .expect("import ok");
        assert!(r.ron_text.contains("name: \"hero\""), "got: {}", r.ron_text);
        assert!(r.ron_text.contains("level: 3"));
    }

    #[test]
    fn imports_jsonc_and_reattaches_inline_comments() {
        let jsonc = "{\n  // about a\n  \"a\": 1\n}";
        let r = reconstruct_ron_from_bytes(jsonc.as_bytes(), None, None).expect("import ok");
        assert!(
            r.ron_text.contains("// about a"),
            "inline comment re-attached: {}",
            r.ron_text
        );
    }

    #[test]
    fn malformed_json_is_rejected_with_no_document() {
        let err = reconstruct_ron_from_bytes(b"{ \"a\": }", None, None)
            .expect_err("malformed JSON is rejected");
        assert!(matches!(err, ImportError::MalformedJson(_)));
    }

    #[test]
    fn non_utf8_is_rejected_with_no_document() {
        // An invalid UTF-8 byte sequence is rejected at the boundary (FR-013).
        let err = reconstruct_ron_from_bytes(&[0xff, 0xfe, 0x00], None, None)
            .expect_err("non-UTF-8 is rejected");
        assert!(matches!(err, ImportError::NotUtf8));
    }

    #[test]
    fn sidecar_comments_are_read_back() {
        let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        map.insert("/a".to_string(), vec!["// sidecar note".to_string()]);
        let r = reconstruct_ron_from_bytes(b"{ \"a\": 1 }", Some(&map), None).expect("import ok");
        assert!(
            r.ron_text.contains("// sidecar note"),
            "sidecar comment read back: {}",
            r.ron_text
        );
    }

    #[test]
    fn import_json_reads_the_deterministic_sibling_sidecar() {
        let dir = temp_dir("sidecar_readback");
        let input = dir.join("in.json");
        std::fs::write(&input, b"{ \"a\": 1 }").expect("write json");
        // Write the deterministic sibling sidecar the strict-mode export would write.
        let sidecar = dir.join("in.json.comments.json");
        std::fs::write(&sidecar, b"{ \"/a\": [\"// from sidecar\"] }").expect("write sidecar");
        let r = import_json(&input, None).expect("import ok");
        assert!(
            r.ron_text.contains("// from sidecar"),
            "the sibling sidecar is read back: {}",
            r.ron_text
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_json_missing_sidecar_is_fine() {
        let dir = temp_dir("no_sidecar");
        let input = dir.join("in.json");
        std::fs::write(&input, b"{ \"a\": 1 }").expect("write json");
        let r = import_json(&input, None).expect("import ok");
        assert!(r.ron_text.contains("a: 1"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
