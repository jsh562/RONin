//! The structured-diagnostic model for error-tolerant parsing (OBJ2).
//!
//! A [`Diagnostic`] records a single recovery decision the parser made while
//! building a lossless tree over malformed or incomplete input (TR-005). It
//! never alters the tree's byte coverage — diagnostics are a parallel,
//! side-channel report (INV-3): removing every diagnostic leaves the round-trip
//! identity untouched.
//!
//! # Stable public contract (AD-003 / TR-013)
//!
//! [`Severity`] and [`DiagnosticCode`] are part of `ronin-core`'s 0.x public API.
//! Each code is a stable, namespaced string. Two namespaces exist:
//!
//! * `RON-Pxxxx` — *parse/recovery* diagnostics emitted by `ronin-core`'s
//!   error-tolerant parser; their [`source`](DiagnosticCode::source) is
//!   `"ronin-core"`.
//! * `RON-Vxxxx` — *type/validation* diagnostics emitted by the downstream
//!   `ronin-validate` crate (E006) over a bound `TypeModel`; their
//!   [`source`](DiagnosticCode::source) is `"ronin-types"`. `ronin-core` itself
//!   never produces these (it stays `rowan`-only and acquires no schema), but it
//!   owns the stable code registry so both crates agree on the strings.
//!
//! Codes and their severities MUST NOT be renumbered or repurposed; new
//! situations get new codes appended to the registry within their namespace.
//!
//! # One diagnostic per recovery point (TR-013)
//!
//! The parser emits exactly one [`Diagnostic`] per distinct recovery point, with
//! a precise source byte [`TextRange`] inside `[0, source_len)` (TR-006). The
//! range identifies the offending span (the unexpected token, the unclosed
//! delimiter's open bracket, or the construct that breached the depth guard).

use crate::syntax::TextRange;

/// Fixed severity classification for a [`Diagnostic`] (TR-013).
///
/// Part of the stable public API: the set of variants is closed and their
/// meaning does not change across 0.x releases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// A recovery was required: the input is malformed or incomplete at this
    /// span. The tree still covers all input via `Error`/missing nodes.
    Error,
    /// A non-fatal concern: the input parsed, but something is suspect. Reserved
    /// for future lints; the OBJ2 recovery parser emits only [`Severity::Error`].
    Warning,
}

impl Severity {
    /// The stable lowercase label for this severity (`"error"` / `"warning"`).
    #[inline]
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A stable, namespaced diagnostic code from the `RON-Pxxxx` parse registry or
/// the `RON-Vxxxx` type/validation registry (AD-003 / TR-013, E006/FR-007).
///
/// Every variant maps 1:1 to a fixed `RON-Pxxxx` / `RON-Vxxxx` string via
/// [`DiagnosticCode::code`], and to a producing crate via
/// [`DiagnosticCode::source`] (`"ronin-core"` for `RON-P`, `"ronin-types"` for
/// `RON-V`). The enum is `#[non_exhaustive]` so new codes can be appended
/// without a breaking change, but **existing** variants, their code strings, and
/// their default severities are stable across 0.x. The `RON-V` validation codes
/// are owned here as a shared registry; `ronin-core` never emits them (it stays
/// `rowan`-only) — the downstream `ronin-validate` crate does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum DiagnosticCode {
    /// `RON-P0001` — an unexpected token at a position where a value (or other
    /// construct) was expected; the token was wrapped in an `Error` node.
    UnexpectedToken,
    /// `RON-P0002` — a delimiter (`(`, `[`, `{`) was opened but never closed
    /// before end-of-input; the matching close was synthesized as missing.
    UnclosedDelimiter,
    /// `RON-P0003` — nesting/recursion depth exceeded the configured guard
    /// (default 128); descent stopped and the remaining bytes were tokenized
    /// into `Error` nodes (no stack overflow, INV-5).
    NestingDepthExceeded,
    /// `RON-P0004` — a `:` separator was expected in a struct field or map
    /// entry but was absent; recovery continued with a missing separator.
    MissingSeparator,
    /// `RON-P0005` — a struct field or map entry was missing its value after a
    /// separator; an empty/missing value node was recorded.
    MissingValue,
    /// `RON-V0001` — a value's type does not match the bound type model (e.g. a
    /// string where an integer is expected). Emitted by `ronin-validate`
    /// (FR-002); [`Severity::Error`].
    TypeMismatch,
    /// `RON-V0002` — a field required by the bound type model is absent from a
    /// struct/map. Emitted by `ronin-validate` (FR-002); [`Severity::Error`].
    MissingRequiredField,
    /// `RON-V0003` — an enum variant is not one of the variants the bound type
    /// model allows (invalid/unknown variant). Emitted by `ronin-validate`
    /// (FR-002); [`Severity::Error`].
    InvalidEnumVariant,
    /// `RON-V0004` — a tuple (or tuple-struct/tuple-variant) has the wrong
    /// arity for the bound type model. Emitted by `ronin-validate` (FR-002);
    /// [`Severity::Error`].
    WrongTupleArity,
    /// `RON-V0005` — a value violates a value constraint the bound type model
    /// expresses (out-of-range numeric, length/pattern bound, etc.). Emitted by
    /// `ronin-validate` (FR-002); [`Severity::Error`].
    ValueConstraintViolation,
    /// `RON-V0006` — an extra/unknown field is present on a struct/map the bound
    /// type model marks `deny_unknown_fields`. Serde-faithful: only flagged for
    /// strict types (FR-018). Emitted by `ronin-validate`; [`Severity::Warning`].
    UnknownField,
}

impl DiagnosticCode {
    /// The stable `RON-Pxxxx` / `RON-Vxxxx` string for this code (part of the
    /// public API).
    #[inline]
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            DiagnosticCode::UnexpectedToken => "RON-P0001",
            DiagnosticCode::UnclosedDelimiter => "RON-P0002",
            DiagnosticCode::NestingDepthExceeded => "RON-P0003",
            DiagnosticCode::MissingSeparator => "RON-P0004",
            DiagnosticCode::MissingValue => "RON-P0005",
            DiagnosticCode::TypeMismatch => "RON-V0001",
            DiagnosticCode::MissingRequiredField => "RON-V0002",
            DiagnosticCode::InvalidEnumVariant => "RON-V0003",
            DiagnosticCode::WrongTupleArity => "RON-V0004",
            DiagnosticCode::ValueConstraintViolation => "RON-V0005",
            DiagnosticCode::UnknownField => "RON-V0006",
        }
    }

    /// The default [`Severity`] for this code. All parse-recovery (`RON-P`)
    /// codes are [`Severity::Error`]. Among the type/validation (`RON-V`) codes,
    /// the hard-mismatch classes — type mismatch, missing-required, invalid
    /// variant, wrong arity, value-constraint — are [`Severity::Error`], while
    /// an extra/unknown field is a [`Severity::Warning`] (FR-005, FR-018). The
    /// mapping is part of the stable contract.
    #[inline]
    #[must_use]
    pub fn default_severity(self) -> Severity {
        match self {
            DiagnosticCode::UnexpectedToken
            | DiagnosticCode::UnclosedDelimiter
            | DiagnosticCode::NestingDepthExceeded
            | DiagnosticCode::MissingSeparator
            | DiagnosticCode::MissingValue
            | DiagnosticCode::TypeMismatch
            | DiagnosticCode::MissingRequiredField
            | DiagnosticCode::InvalidEnumVariant
            | DiagnosticCode::WrongTupleArity
            | DiagnosticCode::ValueConstraintViolation => Severity::Error,
            DiagnosticCode::UnknownField => Severity::Warning,
        }
    }

    /// The stable `source` tag identifying which crate produces this code
    /// (E006/FR-007). It is derived from the code-string namespace prefix:
    /// `"ronin-types"` for any `RON-V` validation code and `"ronin-core"` for any
    /// `RON-P` parse/recovery code. This tag lets a surface distinguish
    /// type findings from structural ones when rendering or deduping. Total and
    /// stable across 0.x.
    #[inline]
    #[must_use]
    pub fn source(self) -> &'static str {
        if self.code().starts_with("RON-V") {
            "ronin-types"
        } else {
            "ronin-core"
        }
    }
}

impl std::fmt::Display for DiagnosticCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

/// A single structured diagnostic produced during error-tolerant parsing.
///
/// Carries a precise source byte [`TextRange`] (TR-006), a human-readable
/// `message`, a [`Severity`], and a stable [`DiagnosticCode`] (TR-013). One
/// `Diagnostic` is emitted per recovery point; diagnostics never change the
/// tree's byte coverage (INV-3).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Diagnostic {
    /// Byte range the diagnostic refers to (a sub-range of `[0, source_len)`).
    pub range: TextRange,
    /// Human-readable description of the recovery.
    pub message: String,
    /// Fixed severity classification.
    pub severity: Severity,
    /// Stable namespaced `RON-Pxxxx` code.
    pub code: DiagnosticCode,
}

impl Diagnostic {
    /// Construct a diagnostic with the [`DiagnosticCode`]'s default severity.
    #[inline]
    #[must_use]
    pub fn new(code: DiagnosticCode, range: TextRange, message: impl Into<String>) -> Self {
        Self {
            range,
            message: message.into(),
            severity: code.default_severity(),
            code,
        }
    }

    /// This diagnostic's stable [`DiagnosticCode`].
    #[inline]
    #[must_use]
    pub fn code(&self) -> DiagnosticCode {
        self.code
    }

    /// This diagnostic's [`Severity`].
    #[inline]
    #[must_use]
    pub fn severity(&self) -> Severity {
        self.severity
    }

    /// This diagnostic's source byte [`TextRange`].
    #[inline]
    #[must_use]
    pub fn range(&self) -> TextRange {
        self.range
    }

    /// This diagnostic's human-readable message.
    #[inline]
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TR-013: the two-variant severity enum is fixed and its labels stable.
    #[test]
    fn severity_values_are_stable() {
        assert_eq!(Severity::Error.as_str(), "error");
        assert_eq!(Severity::Warning.as_str(), "warning");
        assert_eq!(Severity::Error.to_string(), "error");
        // Ord is well-defined (Error < Warning by declaration order).
        assert!(Severity::Error < Severity::Warning);
    }

    /// AD-003/TR-013: every registry code maps to a stable `RON-Pxxxx` string,
    /// all codes are unique, and each has a defined default severity.
    #[test]
    fn codes_are_namespaced_unique_and_have_severity() {
        let all = [
            DiagnosticCode::UnexpectedToken,
            DiagnosticCode::UnclosedDelimiter,
            DiagnosticCode::NestingDepthExceeded,
            DiagnosticCode::MissingSeparator,
            DiagnosticCode::MissingValue,
        ];
        let mut seen = std::collections::BTreeSet::new();
        for c in all {
            let s = c.code();
            assert!(
                s.starts_with("RON-P"),
                "code {s:?} must be in the RON-P parse namespace"
            );
            assert_eq!(s.len(), "RON-P0000".len(), "codes are RON-Pxxxx (4 digits)");
            assert!(
                s["RON-P".len()..].chars().all(|ch| ch.is_ascii_digit()),
                "code {s:?} must end in 4 decimal digits"
            );
            assert!(seen.insert(s), "duplicate code string {s:?}");
            // default_severity must be total (no panic).
            let _ = c.default_severity();
            assert_eq!(c.to_string(), s);
        }
    }

    /// Specific code-string assertions (these strings are a public contract and
    /// must not drift).
    #[test]
    fn code_strings_are_pinned() {
        assert_eq!(DiagnosticCode::UnexpectedToken.code(), "RON-P0001");
        assert_eq!(DiagnosticCode::UnclosedDelimiter.code(), "RON-P0002");
        assert_eq!(DiagnosticCode::NestingDepthExceeded.code(), "RON-P0003");
        assert_eq!(DiagnosticCode::MissingSeparator.code(), "RON-P0004");
        assert_eq!(DiagnosticCode::MissingValue.code(), "RON-P0005");
    }

    /// The full set of `RON-P` parse codes (used by the cross-namespace tests).
    const PARSE_CODES: [DiagnosticCode; 5] = [
        DiagnosticCode::UnexpectedToken,
        DiagnosticCode::UnclosedDelimiter,
        DiagnosticCode::NestingDepthExceeded,
        DiagnosticCode::MissingSeparator,
        DiagnosticCode::MissingValue,
    ];

    /// The full set of `RON-V` type/validation codes (E006).
    const VALIDATION_CODES: [DiagnosticCode; 6] = [
        DiagnosticCode::TypeMismatch,
        DiagnosticCode::MissingRequiredField,
        DiagnosticCode::InvalidEnumVariant,
        DiagnosticCode::WrongTupleArity,
        DiagnosticCode::ValueConstraintViolation,
        DiagnosticCode::UnknownField,
    ];

    /// E006/FR-007: every `RON-V` code is in the `RON-V` namespace, is 4-digit,
    /// unique, and never collides with a `RON-P` parse code.
    #[test]
    fn validation_codes_are_namespaced_unique_and_disjoint_from_parse() {
        let mut seen = std::collections::BTreeSet::new();
        for c in VALIDATION_CODES {
            let s = c.code();
            assert!(
                s.starts_with("RON-V"),
                "code {s:?} must be in the RON-V validation namespace"
            );
            assert_eq!(s.len(), "RON-V0000".len(), "codes are RON-Vxxxx (4 digits)");
            assert!(
                s["RON-V".len()..].chars().all(|ch| ch.is_ascii_digit()),
                "code {s:?} must end in 4 decimal digits"
            );
            assert!(seen.insert(s), "duplicate validation code string {s:?}");
            // default_severity must be total (no panic).
            let _ = c.default_severity();
            assert_eq!(c.to_string(), s);
        }
    }

    /// E006/FR-007: the combined P+V code set has no duplicates — the two
    /// namespaces are globally unique across the registry.
    #[test]
    fn all_codes_are_globally_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for c in PARSE_CODES.into_iter().chain(VALIDATION_CODES) {
            assert!(
                seen.insert(c.code()),
                "duplicate code string {:?} across P+V namespaces",
                c.code()
            );
        }
        assert_eq!(
            seen.len(),
            PARSE_CODES.len() + VALIDATION_CODES.len(),
            "combined registry size must equal P + V counts"
        );
    }

    /// E006/FR-007: `source()` is `"ronin-types"` for every V code and
    /// `"ronin-core"` for every P code, consistent with the namespace prefix.
    #[test]
    fn source_tag_matches_namespace() {
        for c in PARSE_CODES {
            assert_eq!(
                c.source(),
                "ronin-core",
                "parse code {} must be ronin-core",
                c.code()
            );
        }
        for c in VALIDATION_CODES {
            assert_eq!(
                c.source(),
                "ronin-types",
                "validation code {} must be ronin-types",
                c.code()
            );
        }
    }

    /// E006: the new validation code strings are a public contract — pin them.
    #[test]
    fn validation_code_strings_are_pinned() {
        assert_eq!(DiagnosticCode::TypeMismatch.code(), "RON-V0001");
        assert_eq!(DiagnosticCode::MissingRequiredField.code(), "RON-V0002");
        assert_eq!(DiagnosticCode::InvalidEnumVariant.code(), "RON-V0003");
        assert_eq!(DiagnosticCode::WrongTupleArity.code(), "RON-V0004");
        assert_eq!(DiagnosticCode::ValueConstraintViolation.code(), "RON-V0005");
        assert_eq!(DiagnosticCode::UnknownField.code(), "RON-V0006");
    }

    /// E006/FR-005/FR-018: the V severities follow the policy — the five
    /// hard-mismatch classes are Error, the extra/unknown field is Warning.
    #[test]
    fn validation_default_severities_match_policy() {
        assert_eq!(
            DiagnosticCode::TypeMismatch.default_severity(),
            Severity::Error
        );
        assert_eq!(
            DiagnosticCode::MissingRequiredField.default_severity(),
            Severity::Error
        );
        assert_eq!(
            DiagnosticCode::InvalidEnumVariant.default_severity(),
            Severity::Error
        );
        assert_eq!(
            DiagnosticCode::WrongTupleArity.default_severity(),
            Severity::Error
        );
        assert_eq!(
            DiagnosticCode::ValueConstraintViolation.default_severity(),
            Severity::Error
        );
        assert_eq!(
            DiagnosticCode::UnknownField.default_severity(),
            Severity::Warning
        );
    }

    /// `Diagnostic::new` adopts the code's default severity and stores the range.
    #[test]
    fn new_uses_default_severity() {
        let r = TextRange::new(2, 5);
        let d = Diagnostic::new(DiagnosticCode::UnexpectedToken, r, "boom");
        assert_eq!(d.code(), DiagnosticCode::UnexpectedToken);
        assert_eq!(d.severity(), Severity::Error);
        assert_eq!(d.range(), r);
        assert_eq!(d.message(), "boom");
    }
}
