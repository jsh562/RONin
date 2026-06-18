//! Structured, non-fatal acquisition diagnostics {TR-011}.
//!
//! An [`AcquisitionDiagnostic`] is a finding produced while a [`crate::TypeSource`]
//! adapter acquires types, or while the normalizer merges partial models. Like
//! `ronin-core`'s parse diagnostics, these are a *side-channel report*: emitting a
//! diagnostic NEVER aborts construction of the rest of the model (TR-011,
//! Progressive Intelligence). Diagnostics serialize alongside the model so a
//! consumer (E006) receives findings together with the types.

use serde::{Deserialize, Serialize};

/// The class of an [`AcquisitionDiagnostic`] (TR-011).
///
/// Closed set; serializes in `kebab-case` (`"unresolved-type"`, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiagnosticCategory {
    /// A type could not be resolved (foreign crate, generic instantiation,
    /// macro-generated) and was recorded as an `unknown` node (TR-006).
    UnresolvedType,
    /// Two sources described the same named type with conflicting shapes; the
    /// higher-precedence source won (TR-010).
    SourceConflict,
    /// A construct had no clean mapping into the JSON-Schema-2020-12 + `x-ron-*`
    /// vocabulary; a best-effort node was produced.
    UnsupportedConstruct,
}

impl DiagnosticCategory {
    /// The stable `kebab-case` keyword for this category (public contract).
    #[inline]
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DiagnosticCategory::UnresolvedType => "unresolved-type",
            DiagnosticCategory::SourceConflict => "source-conflict",
            DiagnosticCategory::UnsupportedConstruct => "unsupported-construct",
        }
    }
}

impl std::fmt::Display for DiagnosticCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Severity of an acquisition finding. Every severity is non-fatal to the model
/// (TR-011); `Error` marks a finding a consumer should surface prominently, not
/// a build failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    /// Informational note (e.g. provenance auditing).
    Info,
    /// A concern a consumer may want to surface (e.g. a degraded `unknown`).
    Warning,
    /// A serious-but-non-fatal finding (e.g. a source conflict).
    Error,
}

impl DiagnosticSeverity {
    /// The stable lowercase label for this severity.
    #[inline]
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DiagnosticSeverity::Info => "info",
            DiagnosticSeverity::Warning => "warning",
            DiagnosticSeverity::Error => "error",
        }
    }
}

impl std::fmt::Display for DiagnosticSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An optional location for a diagnostic (TR-011).
///
/// For `syn`-origin findings this is a source file/module reference; for schema
/// sources it is a JSON Schema pointer. Kept as plain strings so the model stays
/// serde- and `wasm32`-friendly (no native path types in the serialized form).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticLocation {
    /// Source file or schema document the finding concerns, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// A more precise pointer within the source (module path, JSON pointer,
    /// span), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
}

/// A single structured, non-fatal acquisition finding (TR-011).
///
/// Serde-serializable so it travels with the [`crate::TypeModel`] to consumers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcquisitionDiagnostic {
    /// The class of finding.
    pub category: DiagnosticCategory,
    /// Severity (always non-fatal to the model).
    pub severity: DiagnosticSeverity,
    /// The named type / path the finding concerns.
    pub subject: String,
    /// Human-readable explanation.
    pub detail: String,
    /// Optional source location.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<DiagnosticLocation>,
    /// The producing source id (set for source-origin findings) — supports
    /// provenance and conflict auditing (TR-010).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
}

impl AcquisitionDiagnostic {
    /// Construct a diagnostic with the default severity for its category.
    ///
    /// Default severities: `UnresolvedType` → `Warning`,
    /// `SourceConflict` → `Error`, `UnsupportedConstruct` → `Warning`.
    #[must_use]
    pub fn new(
        category: DiagnosticCategory,
        subject: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        let severity = match category {
            DiagnosticCategory::UnresolvedType => DiagnosticSeverity::Warning,
            DiagnosticCategory::SourceConflict => DiagnosticSeverity::Error,
            DiagnosticCategory::UnsupportedConstruct => DiagnosticSeverity::Warning,
        };
        Self {
            category,
            severity,
            subject: subject.into(),
            detail: detail.into(),
            location: None,
            source_id: None,
        }
    }

    /// Builder: attach a source location.
    #[must_use]
    pub fn with_location(mut self, location: DiagnosticLocation) -> Self {
        self.location = Some(location);
        self
    }

    /// Builder: attach the producing source id.
    #[must_use]
    pub fn with_source_id(mut self, source_id: impl Into<String>) -> Self {
        self.source_id = Some(source_id.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Category and severity labels are stable kebab/lowercase strings.
    #[test]
    fn labels_are_stable() {
        assert_eq!(
            DiagnosticCategory::UnresolvedType.as_str(),
            "unresolved-type"
        );
        assert_eq!(
            DiagnosticCategory::SourceConflict.as_str(),
            "source-conflict"
        );
        assert_eq!(
            DiagnosticCategory::UnsupportedConstruct.as_str(),
            "unsupported-construct"
        );
        assert_eq!(DiagnosticSeverity::Info.as_str(), "info");
        assert_eq!(DiagnosticSeverity::Warning.as_str(), "warning");
        assert_eq!(DiagnosticSeverity::Error.as_str(), "error");
    }

    /// `new` selects the category's default severity.
    #[test]
    fn new_picks_default_severity() {
        let d = AcquisitionDiagnostic::new(DiagnosticCategory::SourceConflict, "Foo", "clash");
        assert_eq!(d.severity, DiagnosticSeverity::Error);
        let d = AcquisitionDiagnostic::new(DiagnosticCategory::UnresolvedType, "Bar", "miss");
        assert_eq!(d.severity, DiagnosticSeverity::Warning);
    }

    /// Diagnostics round-trip through serde unchanged.
    #[test]
    fn diagnostic_round_trips() {
        let d = AcquisitionDiagnostic::new(DiagnosticCategory::UnresolvedType, "Foo", "miss")
            .with_location(DiagnosticLocation {
                source: Some("src/lib.rs".into()),
                pointer: Some("Foo".into()),
            })
            .with_source_id("syn");
        let json = serde_json::to_string(&d).unwrap();
        let back: AcquisitionDiagnostic = serde_json::from_str(&json).unwrap();
        assert_eq!(d, back);
    }
}
