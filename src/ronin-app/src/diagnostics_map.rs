//! Project a `ron-core` byte-range diagnostic onto the editor's coordinate space
//! (FR-008).
//!
//! `ron-core` reports diagnostics with **byte** ranges into the source. The
//! editor surface works in **character** offsets and `(line, column)` positions.
//! [`map_diagnostic`] performs that conversion in a single pass over the prefix
//! up to each offset, correctly handling multibyte UTF-8 (accented Latin, CJK,
//! emoji) so an offset never lands inside a code point.

use ron_core::{Diagnostic, DiagnosticCode, Severity};

use crate::bevy::{SceneDiagnostic, SceneDiagnosticCode, SceneSeverity};

/// A diagnostic expressed in editor coordinates (FR-008).
///
/// Holds the character range, the start/end `(line, column)` positions (both
/// zero-based), and a copy of the severity, code, and message for rendering in
/// the problems panel without re-borrowing the source diagnostic.
///
/// # Bevy-mode scene findings (E009/FR-007)
///
/// A scene-aware finding (Bevy mode) renders through this **same** view so the
/// E006 surface (squiggles + Problems panel) is reused unchanged. Its
/// [`scene_code`](Self::scene_code) carries the Bevy-specific
/// [`SceneDiagnosticCode`] (stable `BVY-S####` string + `"ronin-bevy"` source tag)
/// when the finding is scene-level (no-registry / type-not-in-registry / staleness
/// advisory); for a registered-mismatch it is `None` and the regular
/// [`code`](Self::code) holds the wrapped generic `RON-V####` finding verbatim, so
/// a mismatch is byte-for-byte identical to a serde-mode type finding. A
/// structural / serde-mode type finding always has `scene_code == None`. Use
/// [`code_str`](Self::code_str) / [`source`](Self::source) for a uniform rendered
/// label that prefers the scene identity when present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticView {
    /// `(start, end)` as **character** offsets into the source.
    pub char_range: (usize, usize),
    /// `(start, end)` as zero-based `(line, column)` positions, where `column`
    /// is the character offset within the line.
    pub line_col: ((usize, usize), (usize, usize)),
    /// Severity copied from the source diagnostic. A Bevy scene-level hint /
    /// advisory (which `ron-core` `Severity` cannot express) collapses onto the
    /// existing non-error [`Severity::Warning`] here, while
    /// [`scene_code`](Self::scene_code) keeps the three states distinguishable.
    pub severity: Severity,
    /// Stable `RON-Pxxxx` / `RON-Vxxxx` code copied from the source diagnostic.
    /// For a Bevy scene-level finding (where [`scene_code`](Self::scene_code) is
    /// `Some`) this is a non-error placeholder ([`DiagnosticCode::UnknownField`]);
    /// rendering should prefer [`code_str`](Self::code_str) /
    /// [`source`](Self::source), which return the scene identity when present.
    pub code: DiagnosticCode,
    /// The Bevy-mode scene code, when this view came from a scene-level finding
    /// (no-registry / type-not-in-registry / staleness advisory); `None` for every
    /// structural, serde-mode, or registered-mismatch finding (E009/FR-007).
    pub scene_code: Option<SceneDiagnosticCode>,
    /// Human-readable message copied from the source diagnostic.
    pub message: String,
}

impl DiagnosticView {
    /// The stable code string to render — the scene code (`BVY-S####`) when this
    /// is a scene-level Bevy finding, else the underlying `RON-P####`/`RON-V####`
    /// (E009/FR-007). Prefer this over reading [`code`](Self::code) directly so a
    /// scene-level finding shows its true identity.
    #[must_use]
    pub fn code_str(&self) -> &'static str {
        match self.scene_code {
            Some(scene) => scene.code(),
            None => self.code.code(),
        }
    }

    /// The producing-component `source` tag to render — `"ronin-bevy"` for a
    /// scene-level Bevy finding, else the wrapped code's own source
    /// (`"ron-core"`/`"ron-types"`) (E009/FR-007).
    #[must_use]
    pub fn source(&self) -> &'static str {
        match self.scene_code {
            Some(scene) => scene.source(),
            None => self.code.source(),
        }
    }
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
        scene_code: None,
        message: diag.message.clone(),
    }
}

/// Convert a Bevy-mode [`SceneDiagnostic`] into a [`DiagnosticView`] in editor
/// coordinates, so scene-aware validation renders through the **same** E006
/// surface (squiggles + Problems panel) as serde-mode type findings
/// (E009/FR-007/IP-005).
///
/// A **registered-mismatch** finding ([`SceneDiagnosticCode::Mismatch`]) carries
/// the wrapped generic `RON-V####` code + its error/warning severity verbatim, so
/// it is byte-for-byte identical to a serde-mode type finding (same code, same
/// `source` tag, same squiggle). A **scene-level** finding (no-registry /
/// type-not-in-registry / staleness advisory) sets
/// [`scene_code`](DiagnosticView::scene_code) to its `BVY-S####`
/// [`SceneDiagnosticCode`] (so the three states stay distinguishable) and
/// collapses its [`SceneSeverity::Hint`]/[`SceneSeverity::Advisory`] onto the
/// existing non-error [`Severity::Warning`] (`ron-core`'s `Severity` has no
/// hint/advisory level); its [`code`](DiagnosticView::code) is a non-error
/// placeholder superseded by [`code_str`](DiagnosticView::code_str) /
/// [`source`](DiagnosticView::source).
///
/// Multibyte-correct identically to [`map_diagnostic`]: the byte range is resolved
/// to char + `(line, column)` over the prefix.
#[must_use]
pub fn map_scene_diagnostic(diag: &SceneDiagnostic, source: &str) -> DiagnosticView {
    let start = resolve_position(source, diag.range.start());
    let end = resolve_position(source, diag.range.end());

    let (severity, code, scene_code) = match diag.code {
        // A registered mismatch keeps the generic finding's exact code + severity.
        SceneDiagnosticCode::Mismatch(inner) => (severity_from(diag.severity), inner, None),
        // A scene-level hint / advisory: collapse to the non-error Warning and
        // carry the distinguishing scene code. The placeholder `code` is a
        // non-error code superseded by `code_str()`/`source()`.
        scene => (Severity::Warning, DiagnosticCode::UnknownField, Some(scene)),
    };

    DiagnosticView {
        char_range: (start.char_offset, end.char_offset),
        line_col: ((start.line, start.column), (end.line, end.column)),
        severity,
        code,
        scene_code,
        message: diag.message.clone(),
    }
}

/// Map a Bevy [`SceneSeverity`] onto the rendered `ron-core` [`Severity`]: a hard
/// `Error` stays `Error`; everything else (warning / hint / advisory) renders as
/// the non-error [`Severity::Warning`] (the only non-error level the surface knows).
#[inline]
fn severity_from(severity: SceneSeverity) -> Severity {
    match severity {
        SceneSeverity::Error => Severity::Error,
        _ => Severity::Warning,
    }
}

#[cfg(test)]
mod tests {
    //! T015 — Bevy scene diagnostics render through the E006 `DiagnosticView`
    //! surface exactly like serde-mode type findings (FR-007).

    use super::*;
    use ron_core::TextRange;

    fn scene_diag(
        code: SceneDiagnosticCode,
        sev: SceneSeverity,
        range: (usize, usize),
    ) -> SceneDiagnostic {
        SceneDiagnostic {
            range: TextRange::new(range.0, range.1),
            severity: sev,
            code,
            message: "scene finding".to_string(),
        }
    }

    #[test]
    fn registered_mismatch_renders_as_a_ron_v_type_finding() {
        // A mismatch is byte-for-byte identical to a serde-mode type finding:
        // the real RON-V code + its error severity + the ron-types source tag,
        // and NO scene_code (so it is treated as a regular type finding).
        let diag = scene_diag(
            SceneDiagnosticCode::Mismatch(DiagnosticCode::TypeMismatch),
            SceneSeverity::Error,
            (5, 9),
        );
        let view = map_scene_diagnostic(&diag, "abcde\"no\"fg");
        assert_eq!(view.severity, Severity::Error);
        assert_eq!(view.code, DiagnosticCode::TypeMismatch);
        assert!(view.scene_code.is_none());
        assert_eq!(view.code_str(), "RON-V0001");
        assert_eq!(view.source(), "ron-types");
        assert_eq!(view.char_range, (5, 9));
    }

    #[test]
    fn scene_level_hint_preserves_distinct_code_and_non_error_severity() {
        // A type-not-in-registry hint renders as a non-error finding whose
        // scene_code keeps it distinguishable (BVY-S0002, ronin-bevy).
        let diag = scene_diag(
            SceneDiagnosticCode::TypeNotInRegistry,
            SceneSeverity::Hint,
            (3, 7),
        );
        let view = map_scene_diagnostic(&diag, "abcdefghij");
        assert_eq!(view.severity, Severity::Warning, "hint is not a hard error");
        assert_eq!(
            view.scene_code,
            Some(SceneDiagnosticCode::TypeNotInRegistry)
        );
        assert_eq!(view.code_str(), "BVY-S0002");
        assert_eq!(view.source(), "ronin-bevy");
        assert_eq!(view.char_range, (3, 7));
    }

    #[test]
    fn no_registry_and_staleness_are_distinguishable_non_errors() {
        let no_reg = map_scene_diagnostic(
            &scene_diag(SceneDiagnosticCode::NoRegistry, SceneSeverity::Hint, (0, 0)),
            "",
        );
        let stale = map_scene_diagnostic(
            &scene_diag(
                SceneDiagnosticCode::StalenessAdvisory,
                SceneSeverity::Advisory,
                (0, 0),
            ),
            "",
        );
        assert_eq!(no_reg.severity, Severity::Warning);
        assert_eq!(stale.severity, Severity::Warning);
        // The three scene states + the serde-mode finding all carry distinct codes.
        assert_eq!(no_reg.code_str(), "BVY-S0001");
        assert_eq!(stale.code_str(), "BVY-S0003");
        assert_ne!(no_reg.code_str(), stale.code_str());
    }

    #[test]
    fn multibyte_prefix_maps_to_char_offsets() {
        // A 2-byte é before the finding offsets bytes vs chars; the rendered
        // char_range must count code points, like map_diagnostic.
        let src = "é\"x\"";
        // byte range of the `"x"` token: é is 2 bytes, so it starts at byte 2.
        let diag = scene_diag(
            SceneDiagnosticCode::Mismatch(DiagnosticCode::TypeMismatch),
            SceneSeverity::Error,
            (2, 5),
        );
        let view = map_scene_diagnostic(&diag, src);
        // One char (é) precedes, so the char range starts at 1, not 2.
        assert_eq!(view.char_range.0, 1);
    }
}
