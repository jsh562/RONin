//! Project a `ronin-core` byte-range diagnostic onto the editor's coordinate space
//! (FR-008).
//!
//! `ronin-core` reports diagnostics with **byte** ranges into the source. The
//! editor surface works in **character** offsets and `(line, column)` positions.
//! [`map_diagnostic`] performs that conversion in a single pass over the prefix
//! up to each offset, correctly handling multibyte UTF-8 (accented Latin, CJK,
//! emoji) so an offset never lands inside a code point.

use ronin_core::{Diagnostic, DiagnosticCode, Severity};

use crate::bevy::{SceneDiagnostic, SceneDiagnosticCode, SceneSeverity};
use crate::interop::{LossKind, LossyConstruct};

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
    /// advisory (which `ronin-core` `Severity` cannot express) collapses onto the
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
    /// The E010 interop loss code, when this view came from a RON→JSON
    /// lossy-construct (FR-006); `None` for every structural / serde / Bevy
    /// finding.
    ///
    /// Set by [`map_loss_construct`] from a [`LossyConstruct`] so a conversion loss
    /// renders through this **same** E006 surface (squiggle + Problems panel) as a
    /// structural or type finding, mirroring [`scene_code`](Self::scene_code) for
    /// Bevy. When `Some`, [`code_str`](Self::code_str) / [`source`](Self::source)
    /// return the stable `RON-I####` code + the `"ronin-interop"` source tag; the
    /// regular [`code`](Self::code) field carries a non-error placeholder superseded
    /// by those accessors.
    pub loss_code: Option<LossKind>,
    /// Human-readable message copied from the source diagnostic.
    pub message: String,
}

impl DiagnosticView {
    /// The stable code string to render — the interop loss code (`RON-I####`) when
    /// this is a conversion loss, the scene code (`BVY-S####`) when this is a
    /// scene-level Bevy finding, else the underlying `RON-P####`/`RON-V####`
    /// (E009/FR-007, E010/FR-006). Prefer this over reading [`code`](Self::code)
    /// directly so a loss / scene-level finding shows its true identity.
    #[must_use]
    pub fn code_str(&self) -> &'static str {
        match (self.loss_code, self.scene_code) {
            (Some(loss), _) => loss.code(),
            (None, Some(scene)) => scene.code(),
            (None, None) => self.code.code(),
        }
    }

    /// The producing-component `source` tag to render — `"ronin-interop"` for a
    /// conversion loss, `"ronin-bevy"` for a scene-level Bevy finding, else the
    /// wrapped code's own source (`"ronin-core"`/`"ronin-types"`) (E009/FR-007,
    /// E010/FR-006).
    #[must_use]
    pub fn source(&self) -> &'static str {
        match (self.loss_code, self.scene_code) {
            (Some(loss), _) => loss.source(),
            (None, Some(scene)) => scene.source(),
            (None, None) => self.code.source(),
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
/// or before it (defensive — `ronin-core` ranges are always on boundaries).
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
        loss_code: None,
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
/// existing non-error [`Severity::Warning`] (`ronin-core`'s `Severity` has no
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
        loss_code: None,
        message: diag.message.clone(),
    }
}

/// Convert an E010 [`LossyConstruct`] into a [`DiagnosticView`] in editor
/// coordinates, so a RON→JSON conversion loss renders through the **same** E006
/// surface (squiggles + Problems panel) as a structural / type / Bevy finding
/// (FR-006/FR-007, AD-004).
///
/// This is the RON→JSON-side analogue of [`map_scene_diagnostic`]: it carries the
/// construct's stable `RON-I####` [`LossKind`] in
/// [`loss_code`](DiagnosticView::loss_code) (so [`code_str`](DiagnosticView::code_str)
/// / [`source`](DiagnosticView::source) return the loss identity + the
/// `"ronin-interop"` source tag), collapses the loss onto the non-error
/// [`Severity::Warning`] (a loss is advisory — the conversion still proceeds after
/// the user confirms), and sets a non-error placeholder [`code`](DiagnosticView::code)
/// superseded by the accessors. The message prefers the construct's human
/// [`detail`](LossyConstruct::detail), falling back to the kind's
/// [`label`](LossKind::label).
///
/// **One source of truth (FR-007).** The caller maps every entry of the SAME
/// [`LossReport`](crate::interop::LossReport)`.constructs()` list that drives the
/// pre-conversion loss dialog, so a loss can never reach one surface but not the
/// other. The byte range is resolved to char + `(line, column)` exactly like
/// [`map_diagnostic`] (multibyte-correct over the prefix).
#[must_use]
pub fn map_loss_construct(construct: &LossyConstruct, source: &str) -> DiagnosticView {
    let range = construct.source_range();
    let start = resolve_position(source, range.start());
    let end = resolve_position(source, range.end());
    let kind = construct.kind();

    DiagnosticView {
        char_range: (start.char_offset, end.char_offset),
        line_col: ((start.line, start.column), (end.line, end.column)),
        // A loss is advisory: the conversion proceeds after confirm, so it collapses
        // onto the non-error Warning level (a loss is never a hard parse error).
        severity: Severity::Warning,
        // A non-error placeholder code superseded by `code_str()`/`source()`, which
        // return the `RON-I####` loss identity when `loss_code` is set.
        code: DiagnosticCode::UnknownField,
        scene_code: None,
        loss_code: Some(kind),
        message: construct
            .detail()
            .map_or_else(|| kind.label().to_string(), ToString::to_string),
    }
}

/// Map every [`LossyConstruct`] in a [`LossReport`](crate::interop::LossReport) to a
/// [`DiagnosticView`], in the report's source order (FR-006/FR-007).
///
/// The convenience the app layer uses to drive BOTH surfaces from one list: the
/// report's `constructs()` (the single source of truth) feed both the
/// pre-conversion dialog AND, through this function, the inline diagnostics — so a
/// loss can never appear in one surface but not the other (FR-007, AD-004).
#[must_use]
pub fn map_loss_report(report: &crate::interop::LossReport, source: &str) -> Vec<DiagnosticView> {
    report
        .constructs()
        .iter()
        .map(|c| map_loss_construct(c, source))
        .collect()
}

/// Map a Bevy [`SceneSeverity`] onto the rendered `ronin-core` [`Severity`]: a hard
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
    use ronin_core::TextRange;

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
        // the real RON-V code + its error severity + the ronin-types source tag,
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
        assert_eq!(view.source(), "ronin-types");
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

    // --- T013: E010 interop loss constructs render through the E006 surface ----

    use crate::interop::{LossKind, LossRecovery, LossyConstruct};

    #[test]
    fn loss_construct_renders_with_its_stable_ron_i_code_and_interop_source() {
        // A RON→JSON loss maps to a DiagnosticView whose code_str/source surface the
        // stable RON-I#### identity + the "ronin-interop" tag (FR-006), exactly the
        // way a Bevy scene finding surfaces its BVY-S#### identity.
        let construct = LossyConstruct::with_detail(
            LossKind::TupleVsList,
            TextRange::new(4, 10),
            LossRecovery::RoundTripSafeWithinRonin,
            "tuple → JSON array",
        );
        let view = map_loss_construct(&construct, "(t: (1, 2))");
        // A loss is advisory, not a hard parse error.
        assert_eq!(view.severity, Severity::Warning);
        assert_eq!(view.loss_code, Some(LossKind::TupleVsList));
        assert!(view.scene_code.is_none());
        // The rendered identity is the stable loss code + the interop source tag.
        assert_eq!(view.code_str(), "RON-I0002");
        assert_eq!(view.source(), "ronin-interop");
        // The detail wording drives the message; the range maps to char offsets.
        assert_eq!(view.message, "tuple → JSON array");
        assert_eq!(view.char_range, (4, 10));
    }

    #[test]
    fn loss_construct_without_detail_falls_back_to_the_kind_label() {
        let construct = LossyConstruct::new(
            LossKind::Char,
            TextRange::new(0, 3),
            LossRecovery::LossyToExternal,
        );
        let view = map_loss_construct(&construct, "'x'");
        assert_eq!(view.code_str(), "RON-I0003");
        assert_eq!(view.message, LossKind::Char.label());
    }

    #[test]
    fn map_loss_report_maps_every_construct_in_source_order() {
        // The one report list drives the inline surface: each construct → one view,
        // in order, with its own stable code (FR-007 — one list, both surfaces).
        let report = crate::interop::LossReport::from_constructs(vec![
            LossyConstruct::new(
                LossKind::TupleVsList,
                TextRange::new(0, 4),
                LossRecovery::RoundTripSafeWithinRonin,
            ),
            LossyConstruct::new(
                LossKind::DroppedComment,
                TextRange::new(6, 12),
                LossRecovery::LossyToExternal,
            ),
        ]);
        let views = map_loss_report(&report, "(t: (1, 2))// c");
        assert_eq!(views.len(), report.len(), "one view per construct");
        assert_eq!(views[0].code_str(), "RON-I0002");
        assert_eq!(views[1].code_str(), "RON-I0009");
    }
}
