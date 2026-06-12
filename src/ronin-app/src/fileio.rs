//! Filesystem entry points for opening RON files (FR-018).
//!
//! [`open_path`] reads a file's raw bytes, validates UTF-8 at the boundary, and
//! builds an [`EditorDocument`]. Non-UTF-8 input is rejected cleanly with
//! [`OpenError::NotUtf8`] — **no document is created** — honouring "never corrupt
//! user data" (project-instructions §I): the editor refuses to silently lossy-
//! decode a binary or non-UTF-8 file.
//!
//! # Deferred scope (E007)
//!
//! The Save path here is a direct `std::fs::write`. The crash-safe persistence
//! contract from project-instructions §I — **atomic save** (temp-write + fsync +
//! rename) with sidecar crash recovery, and full **undo/redo** — is deferred to
//! **E007** and intentionally not implemented in this shell. This module is the
//! seam where E007 replaces the plain write with the atomic-save pipeline.

use std::path::Path;

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

/// Why saving a file failed (FR-020/FR-023).
///
/// `Io` wraps any filesystem write failure (permission denied, disk full, etc.).
/// The variant set is `#[non_exhaustive]` so future save modes (e.g. a distinct
/// atomic-rename failure) can be added without a breaking change.
#[derive(Debug)]
#[non_exhaustive]
pub enum SaveError {
    /// A filesystem write error occurred; the on-disk file may be unchanged.
    Io(std::io::Error),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for SaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SaveError::Io(e) => Some(e),
        }
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

/// Write `doc` to `path`, re-emitting the load-time byte fidelity (FR-020/FR-023).
///
/// Serialises the document buffer with [`save_bytes`] (so the original EOL style,
/// BOM, and trailing-newline presence are honoured) and writes it. The write is
/// non-atomic (`std::fs::write`); a later wave introduces atomic temp-write +
/// rename per project-instructions §I — this task delivers the byte-faithful
/// re-emission FR-020/FR-023 requires.
///
/// # Errors
///
/// Returns [`SaveError::Io`] if the file cannot be written.
pub fn save_document(doc: &EditorDocument, path: &Path) -> Result<(), SaveError> {
    let bytes = save_bytes(&doc.buffer, &doc.byte_profile);
    std::fs::write(path, bytes)?;
    Ok(())
}
