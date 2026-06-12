//! Project a `ron-core` byte-range diagnostic onto the editor's coordinate space
//! (FR-008).
//!
//! `ron-core` reports diagnostics with **byte** ranges into the source. The
//! editor surface works in **character** offsets and `(line, column)` positions.
//! [`map_diagnostic`] performs that conversion in a single pass over the prefix
//! up to each offset, correctly handling multibyte UTF-8 (accented Latin, CJK,
//! emoji) so an offset never lands inside a code point.

use ron_core::{Diagnostic, DiagnosticCode, Severity};

/// A diagnostic expressed in editor coordinates (FR-008).
///
/// Holds the character range, the start/end `(line, column)` positions (both
/// zero-based), and a copy of the severity, code, and message for rendering in
/// the problems panel without re-borrowing the source diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticView {
    /// `(start, end)` as **character** offsets into the source.
    pub char_range: (usize, usize),
    /// `(start, end)` as zero-based `(line, column)` positions, where `column`
    /// is the character offset within the line.
    pub line_col: ((usize, usize), (usize, usize)),
    /// Severity copied from the source diagnostic.
    pub severity: Severity,
    /// Stable `RON-Pxxxx` code copied from the source diagnostic.
    pub code: DiagnosticCode,
    /// Human-readable message copied from the source diagnostic.
    pub message: String,
}

/// A resolved position for one byte offset: its char offset and `(line, column)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Position {
    char_offset: usize,
    line: usize,
    column: usize,
}

/// Resolve a single byte offset into `(char_offset, line, column)`.
///
/// Walks the prefix `source[..offset_clamped]` once, counting characters and
/// newlines. An offset past the end of `source` clamps to the end; an offset
/// that does not fall on a char boundary clamps down to the nearest boundary at
/// or before it (defensive — `ron-core` ranges are always on boundaries).
fn resolve_position(source: &str, byte_offset: usize) -> Position {
    let target = byte_offset.min(source.len());

    let mut char_offset = 0usize;
    let mut line = 0usize;
    let mut column = 0usize;

    for (idx, ch) in source.char_indices() {
        if idx >= target {
            break;
        }
        char_offset += 1;
        if ch == '\n' {
            line += 1;
            column = 0;
        } else {
            column += 1;
        }
    }

    Position {
        char_offset,
        line,
        column,
    }
}

/// Convert a byte-range [`Diagnostic`] into a [`DiagnosticView`] in editor
/// coordinates (FR-008).
///
/// Correct for multibyte UTF-8: the returned `char_range` counts code points, so
/// it differs from the byte range whenever the prefix contains non-ASCII text.
#[must_use]
pub fn map_diagnostic(diag: &Diagnostic, source: &str) -> DiagnosticView {
    let start = resolve_position(source, diag.range.start());
    // Resolve the end independently; for editor-sized ranges the duplicated walk
    // is negligible and keeps the function simple and obviously correct.
    let end = resolve_position(source, diag.range.end());

    DiagnosticView {
        char_range: (start.char_offset, end.char_offset),
        line_col: ((start.line, start.column), (end.line, end.column)),
        severity: diag.severity,
        code: diag.code,
        message: diag.message.clone(),
    }
}
