//! Scene-aware validation — drives the generic, Bevy-agnostic `ronin-validate`
//! subtree-vs-type entry per component/resource, with progressive degradation
//! and the staleness advisory (FR-005/FR-006/FR-008).
//!
//! # Where the Bevy awareness lives (AD-001, IP-002, HINT-002)
//!
//! All `.scn.ron` interpretation (resources / entities / components keyed by type
//! path) is native [`ronin-app`] code: this module reads a [`SceneModel`] off the
//! CST and, for each value, looks the fully-qualified type path up in the bound
//! [`BevyRegistry`]. The *engine* it drives —
//! [`ronin_validate::validate::validate_subtree_against_type`] — is **generic and
//! Bevy-agnostic** (it knows only "validate this CST sub-tree against this named
//! model type"); the WASM-clean core (`ronin-core`/`ronin-validate`) gains no Bevy or
//! registry dependency.
//!
//! # The interchange model (mirrors serde mode)
//!
//! Serde mode serializes its acquired E004 `TypeModel` to the frozen JSON-Schema
//! 2020-12 + `x-ron-*` interchange via [`ronin_types::to_json`] and hands the
//! resulting [`serde_json::Value`] to the validator (see
//! [`crate::type_acquire`]). Bevy mode reuses that **exact** serialization: a
//! [`BevySource`](ronin_types::BevySource) `acquire()` yields a `TypeModel`, which
//! `ronin_types::to_json` turns into the same `Value`. [`validate_scene`] therefore
//! takes the already-serialized `&Value` (its `$defs` keyed by fully-qualified
//! Bevy type path) and the parsed [`BevyRegistry`] (the cheap, in-memory presence
//! lookup), so each scene value can be routed to `validate_subtree_against_type`
//! by its type path.
//!
//! # The three registry states (FR-006, data-model §4 `registry_state`)
//!
//! The validator distinguishes — by [`SceneSeverity`] **and** [`SceneDiagnosticCode`] —
//! the three states the data-model `ModeState.registry_state` enumerates:
//!
//! * **`NoRegistry`** — the bound registry is empty (none loaded, or it degraded to
//!   empty at ingest): ONE document-level [`SceneSeverity::Hint`]
//!   ([`SceneDiagnosticCode::NoRegistry`]) and **no** type errors. The structural
//!   diagnostic set is untouched (FR-006/FR-008).
//! * **`TypeNotInRegistry`** — a scene value's type path is not registered: ONE
//!   [`SceneSeverity::Hint`] ([`SceneDiagnosticCode::TypeNotInRegistry`]) at that
//!   value's range — unconstrained, never a hard error (FR-006).
//! * **`RegisteredMismatch`** — a registered value disagrees with its reflect
//!   schema: the precise `RON-V####` findings from `validate_subtree_against_type`,
//!   carried verbatim as [`SceneSeverity::Error`]/[`SceneSeverity::Warning`]
//!   ([`SceneDiagnosticCode::Mismatch`]) at the offending construct's range
//!   (FR-005).
//!
//! A separate, optional [`SceneSeverity::Advisory`]
//! ([`SceneDiagnosticCode::StalenessAdvisory`]) is raised when the registry's
//! apparent version disagrees with a caller-supplied expected version (FR-008) —
//! never an error.
//!
//! # No `bevy` dependency, never a crash (FR-003/FR-008/FR-011)
//!
//! The registry is consumed strictly as data. Validation is **read-only** over the
//! CST (zero bytes, FR-011) and never panics: [`SceneModel::from_cst`] already
//! skips unparseable regions and `validate_subtree_against_type` fails soft (an
//! empty/absent/`unknown` def → no findings), so a malformed registry (which
//! degrades to empty/partial at ingest) or an unparseable scene region yields only
//! the structural-remainder findings, never a panic.

use ronin_core::CstDocument;
use ronin_types::BevyRegistry;
use serde_json::Value;

use crate::bevy::scene::SceneModel;

/// The severity of a [`SceneDiagnostic`] — a Bevy-mode superset of `ronin-core`'s
/// two-variant `Severity` that adds the non-error **hint** and **advisory**
/// levels the three registry states + the staleness advisory require (FR-006/008).
///
/// `Error`/`Warning` mirror `ronin_core::Severity` for the `RON-V####` mismatch
/// findings; `Hint` is the unconstrained "no registry" / "type not in registry"
/// level (never a hard error); `Advisory` is the version-staleness level. The
/// rendered [`crate::diagnostics_map::DiagnosticView`] collapses `Hint`/`Advisory`
/// onto the existing non-error `Severity::Warning` while preserving the distinct
/// [`SceneDiagnosticCode`], so the three states stay distinguishable to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum SceneSeverity {
    /// A hard mismatch against the registered reflect schema (a `RON-V` error).
    Error,
    /// A non-fatal schema concern (e.g. an extra field on a strict type).
    Warning,
    /// An unconstrained, informational state — no registry loaded, or a type path
    /// not present in the registry. **Never** a hard error (FR-006).
    Hint,
    /// A version-staleness advisory — the registry's apparent version disagrees
    /// with the configured expected version. **Never** an error (FR-008).
    Advisory,
}

impl SceneSeverity {
    /// The stable lowercase label for this severity.
    #[inline]
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SceneSeverity::Error => "error",
            SceneSeverity::Warning => "warning",
            SceneSeverity::Hint => "hint",
            SceneSeverity::Advisory => "advisory",
        }
    }

    /// `true` for the two non-error levels (hint / advisory) — the unconstrained,
    /// no-false-positive states (FR-006/008). Used by callers that gate on
    /// "are there any hard errors?".
    #[inline]
    #[must_use]
    pub fn is_informational(self) -> bool {
        matches!(self, SceneSeverity::Hint | SceneSeverity::Advisory)
    }
}

/// The stable code of a [`SceneDiagnostic`] (FR-006/FR-007).
///
/// Bevy mode's **own**, `ronin-app`-local diagnostic registry — kept here rather
/// than in `ronin-core`/`ronin-validate` so those stay Bevy-agnostic (HINT-002,
/// AD-001). The three scene-level codes (`BVY-S0001`/`BVY-S0002`/`BVY-S0003`) name
/// the three distinguishable registry states + the staleness advisory; a
/// [`SceneDiagnosticCode::Mismatch`] wraps the underlying generic
/// [`ronin_core::DiagnosticCode`] (a `RON-V####`) verbatim so a registered-mismatch
/// finding keeps its precise, stable identity. The [`source`](Self::source) tag
/// (`"ronin-bevy"` for scene-level codes, the wrapped code's own
/// [`ronin_core::DiagnosticCode::source`] for a mismatch) lets a surface tell scene
/// findings from structural ones, exactly as E006 tags `ronin-types` findings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum SceneDiagnosticCode {
    /// `BVY-S0001` — Bevy mode is active but no registry is loaded (the bound
    /// registry is empty): a single document-level hint, no type errors (FR-006).
    NoRegistry,
    /// `BVY-S0002` — a scene value's fully-qualified type path is not in the
    /// registry: unconstrained (hint), never a hard error (FR-006).
    TypeNotInRegistry,
    /// `BVY-S0003` — the registry's apparent version disagrees with the configured
    /// expected version: a staleness advisory, never an error (FR-008).
    StalenessAdvisory,
    /// A registered value disagrees with its reflect schema — wraps the precise
    /// generic `RON-V####` finding from `validate_subtree_against_type` (FR-005).
    Mismatch(ronin_core::DiagnosticCode),
}

impl SceneDiagnosticCode {
    /// The stable code string (`BVY-S####` for a scene-level code, the wrapped
    /// `RON-V####` for a mismatch). Part of the rendered surface (FR-007).
    #[inline]
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            SceneDiagnosticCode::NoRegistry => "BVY-S0001",
            SceneDiagnosticCode::TypeNotInRegistry => "BVY-S0002",
            SceneDiagnosticCode::StalenessAdvisory => "BVY-S0003",
            SceneDiagnosticCode::Mismatch(inner) => inner.code(),
        }
    }

    /// The producing-component `source` tag (FR-007). Scene-level codes are
    /// `"ronin-bevy"` (this native module); a wrapped mismatch keeps the generic
    /// validator's own source tag (`"ronin-types"`).
    #[inline]
    #[must_use]
    pub fn source(self) -> &'static str {
        match self {
            SceneDiagnosticCode::NoRegistry
            | SceneDiagnosticCode::TypeNotInRegistry
            | SceneDiagnosticCode::StalenessAdvisory => "ronin-bevy",
            SceneDiagnosticCode::Mismatch(inner) => inner.source(),
        }
    }
}

/// One scene-aware finding: a Bevy-mode diagnostic with a precise byte range, a
/// distinguishing [`SceneSeverity`] + [`SceneDiagnosticCode`], and a message
/// (FR-005/FR-006/FR-007/FR-008).
///
/// Carried in **document byte** coordinates (the `range` is an absolute span into
/// the source, like every `ronin-core` diagnostic) so it renders through the E006
/// surface unchanged — [`crate::diagnostics_map::map_scene_diagnostic`] maps it to
/// the editor-coordinate `DiagnosticView` (squiggle + Problems entry). A
/// document-level finding (e.g. [`SceneDiagnosticCode::NoRegistry`]) carries a
/// zero-length range at offset 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SceneDiagnostic {
    /// Absolute byte range of the offending construct (document coordinates).
    pub range: ronin_core::TextRange,
    /// The distinguishing severity (error / warning / hint / advisory).
    pub severity: SceneSeverity,
    /// The stable scene / wrapped-validator code (+ source tag).
    pub code: SceneDiagnosticCode,
    /// Human-readable description.
    pub message: String,
}

impl SceneDiagnostic {
    /// Build a scene-level diagnostic with an explicit severity + code.
    #[must_use]
    fn scene_level(
        code: SceneDiagnosticCode,
        severity: SceneSeverity,
        range: ronin_core::TextRange,
        message: impl Into<String>,
    ) -> Self {
        Self {
            range,
            severity,
            code,
            message: message.into(),
        }
    }

    /// This finding's stable code string (FR-007).
    #[inline]
    #[must_use]
    pub fn code(&self) -> SceneDiagnosticCode {
        self.code
    }

    /// This finding's severity (FR-006/008).
    #[inline]
    #[must_use]
    pub fn severity(&self) -> SceneSeverity {
        self.severity
    }

    /// This finding's absolute byte range (FR-005/007).
    #[inline]
    #[must_use]
    pub fn range(&self) -> ronin_core::TextRange {
        self.range
    }

    /// This finding's message.
    #[inline]
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Validate a Bevy `.scn.ron` document against a bound registry (FR-005/FR-006/FR-008).
///
/// Interprets `doc` into a [`SceneModel`] and, for each resource / component value
/// (each a `SceneValueRef` with its precise CST range):
///
/// * **type path NOT in `registry`** → ONE [`SceneSeverity::Hint`]
///   ([`SceneDiagnosticCode::TypeNotInRegistry`]) at that value's range —
///   unconstrained, **never** a hard error (FR-006);
/// * **type path IN `registry`** → the generic
///   [`ronin_validate::validate::validate_subtree_against_type`] run against `model`,
///   whose `RON-V####` findings are carried verbatim as
///   [`SceneDiagnosticCode::Mismatch`] at their precise ranges (FR-005).
///
/// When `registry` is empty ([`BevyRegistry::is_empty`]) the document is in the
/// **`NoRegistry`** state: a single document-level [`SceneSeverity::Hint`]
/// ([`SceneDiagnosticCode::NoRegistry`]) is emitted and **no** type errors are
/// produced (FR-006) — the structural diagnostic set (computed elsewhere) is left
/// intact.
///
/// `expected_version` is the optional configured expected Bevy version (the wiring
/// of that config is US2/T020 — here it is a plain parameter): when present and it
/// disagrees with the registry's [`apparent_version`](BevyRegistry::apparent_version),
/// a single [`SceneSeverity::Advisory`]
/// ([`SceneDiagnosticCode::StalenessAdvisory`]) is appended (FR-008). When `None`,
/// or when the registry carries no apparent version, no staleness advisory is
/// raised (validation is unaffected).
///
/// `model` is the already-serialized E004 interchange (`ronin_types::to_json` of a
/// `BevySource.acquire().model`) — the **same** serialization serde mode hands the
/// validator. Pairing it with the parsed `registry` keeps the presence check cheap
/// (`registry.contains`) and the schema lookup precise (`model.$defs.<path>`).
///
/// Read-only and crash-free (FR-008/FR-011): the CST is borrowed and never
/// mutated (zero bytes); `SceneModel::from_cst` skips unparseable regions and the
/// validator fails soft, so a malformed registry / unparseable scene region yields
/// only the structural-remainder findings, never a panic.
#[must_use]
pub fn validate_scene(
    model: &Value,
    registry: &BevyRegistry,
    doc: &CstDocument,
    expected_version: Option<&str>,
) -> Vec<SceneDiagnostic> {
    let mut out = Vec::new();

    // NoRegistry (FR-006): an empty bound registry → a single document-level hint
    // and NO type errors. A staleness advisory is still meaningful in principle,
    // but with no registry there is no apparent version to compare, so this path
    // emits the hint alone.
    if registry.is_empty() {
        out.push(SceneDiagnostic::scene_level(
            SceneDiagnosticCode::NoRegistry,
            SceneSeverity::Hint,
            document_origin_range(),
            "no Bevy type registry loaded; scene types are unconstrained",
        ));
        return out;
    }

    // Per resource + component (in source order), route by registry membership.
    let scene = SceneModel::from_cst(doc);
    for value in scene.entries() {
        let type_path = value.type_path();
        if registry.contains(type_path) {
            // RegisteredMismatch (FR-005): drive the generic, Bevy-agnostic
            // validator against the serialized model, keyed by the type path. Each
            // RON-V#### finding lands at its precise construct range; an `unknown`
            // / absent def fails soft (no findings) — no false positives.
            let findings = ronin_validate::validate::validate_subtree_against_type(
                model,
                type_path,
                value.value_node(),
            );
            out.extend(findings.into_iter().map(|d| SceneDiagnostic {
                range: d.range(),
                severity: severity_from(d.severity()),
                code: SceneDiagnosticCode::Mismatch(d.code()),
                message: d.message().to_owned(),
            }));
        } else {
            // TypeNotInRegistry (FR-006): unconstrained → a hint at the value's
            // precise range, never a hard error.
            out.push(SceneDiagnostic::scene_level(
                SceneDiagnosticCode::TypeNotInRegistry,
                SceneSeverity::Hint,
                value.range(),
                format!("type path `{type_path}` is not in the loaded registry; unconstrained"),
            ));
        }
    }

    // Staleness advisory (FR-008/T016): append when configured + disagreeing.
    if let Some(advisory) = staleness_advisory(registry, expected_version) {
        out.push(advisory);
    }

    out
}

/// Build the FR-008 staleness **advisory** when the registry's apparent version
/// disagrees with a caller-supplied `expected_version` (T016).
///
/// Returns `None` — i.e. **no** advisory — when no expected version is configured
/// (`expected_version == None`), when the registry carries no apparent version, or
/// when the two agree. The actual expected-version config wiring is US2/T020; this
/// helper takes the expected version as a parameter so the same logic serves the
/// inline [`validate_scene`] call and any standalone caller.
///
/// A staleness advisory is **never** an error ([`SceneSeverity::Advisory`]): a
/// version skew does not invalidate the scene — it only flags that the registry
/// may not match the scene's Bevy version (FR-008). The advisory is document-level
/// (a zero-length range at offset 0).
#[must_use]
pub fn staleness_advisory(
    registry: &BevyRegistry,
    expected_version: Option<&str>,
) -> Option<SceneDiagnostic> {
    let expected = expected_version?;
    let apparent = registry.apparent_version()?;
    if apparent == expected {
        return None;
    }
    Some(SceneDiagnostic::scene_level(
        SceneDiagnosticCode::StalenessAdvisory,
        SceneSeverity::Advisory,
        document_origin_range(),
        format!(
            "registry apparent version `{apparent}` differs from the configured \
             expected version `{expected}`; the registry may be stale"
        ),
    ))
}

/// Map a generic `ronin-core` severity (Error/Warning) onto a [`SceneSeverity`].
/// The validator only ever emits Error/Warning; a future variant degrades to
/// Warning (conservative — never a false hard error).
#[inline]
fn severity_from(severity: ronin_core::Severity) -> SceneSeverity {
    match severity {
        ronin_core::Severity::Error => SceneSeverity::Error,
        _ => SceneSeverity::Warning,
    }
}

/// The document-origin range for a document-level finding (no single offending
/// construct): a zero-length span at offset 0. Rendering collapses it to the file
/// start, matching how a whole-document advisory is surfaced.
#[inline]
fn document_origin_range() -> ronin_core::TextRange {
    ronin_core::TextRange::new(0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ronin_core::parse;
    use ronin_types::{BevySource, TypeSource};

    /// The registry-schema fixture shared with the integration suite — a tiny
    /// hand-authored export covering a struct (Transform), a referenced struct
    /// (Vec3), a tuple-struct (Quat), an enum (Visibility), and a future/unknown
    /// reflect kind, plus an `apparentVersion`.
    const REGISTRY: &str = r##"{
        "bevyVersion": "0.16.0",
        "$defs": {
            "game::Transform": {
                "kind": "Struct",
                "additionalProperties": false,
                "properties": {
                    "translation": { "$ref": "#/$defs/game::Vec3" }
                },
                "required": ["translation"],
                "reflectTypes": ["Default"]
            },
            "game::Vec3": {
                "kind": "Struct",
                "additionalProperties": false,
                "properties": {
                    "x": { "type": "number" },
                    "y": { "type": "number" },
                    "z": { "type": "number" }
                },
                "required": ["x", "y", "z"],
                "reflectTypes": ["Default"]
            },
            "game::Visibility": {
                "kind": "Enum",
                "oneOf": [
                    { "kind": "Unit", "shortPath": "Inherited" },
                    { "kind": "Unit", "shortPath": "Hidden" }
                ],
                "reflectTypes": ["Default"]
            }
        }
    }"##;

    /// Acquire the fixture registry + its serialized interchange the way
    /// production does: `BevySource::acquire()` → `ronin_types::to_json`.
    fn registry_and_model() -> (BevyRegistry, Value) {
        let (registry, _diags) = BevyRegistry::from_schema_json(REGISTRY, "test", "<test>");
        let acquired = BevySource::from_schema_json(REGISTRY).acquire();
        let model = ronin_types::to_json(&acquired.model);
        (registry, model)
    }

    #[test]
    fn no_registry_emits_single_hint_and_no_type_errors() {
        let (_r, model) = registry_and_model();
        let empty = BevyRegistry::default();
        let doc = parse(r#"(entities: {0: (components: {"game::Vec3": (x: "bad")})})"#);
        let diags = validate_scene(&model, &empty, &doc, None);
        assert_eq!(diags.len(), 1, "exactly one no-registry hint");
        assert_eq!(diags[0].code(), SceneDiagnosticCode::NoRegistry);
        assert_eq!(diags[0].severity(), SceneSeverity::Hint);
        assert!(
            diags.iter().all(|d| d.severity() != SceneSeverity::Error),
            "no type errors in the NoRegistry state"
        );
    }

    #[test]
    fn unregistered_type_is_a_hint_not_an_error() {
        let (registry, model) = registry_and_model();
        let doc = parse(r#"(entities: {0: (components: {"game::Unknown": (a: 1)})})"#);
        let diags = validate_scene(&model, &registry, &doc, None);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code(), SceneDiagnosticCode::TypeNotInRegistry);
        assert_eq!(diags[0].severity(), SceneSeverity::Hint);
        // The hint lands on the value's precise range, not a fabricated one.
        assert!(!diags[0].range().is_empty());
    }

    #[test]
    fn registered_mismatch_is_an_error_at_precise_range() {
        let (registry, model) = registry_and_model();
        // `x` is a string where a number is required.
        let src = r#"(entities: {0: (components: {"game::Vec3": (x: "no", y: 0.0, z: 0.0)})})"#;
        let doc = parse(src);
        let diags = validate_scene(&model, &registry, &doc, None);
        let mismatch = diags
            .iter()
            .find(|d| matches!(d.code(), SceneDiagnosticCode::Mismatch(_)))
            .expect("a registered mismatch finding");
        assert_eq!(mismatch.severity(), SceneSeverity::Error);
        // The finding addresses the offending `"no"` token's span.
        let r = mismatch.range();
        assert_eq!(&src[r.start()..r.end()], "\"no\"");
    }

    #[test]
    fn valid_registered_scene_has_no_errors() {
        let (registry, model) = registry_and_model();
        let src = r#"(entities: {0: (components: {
            "game::Vec3": (x: 1.0, y: 2.0, z: 3.0),
            "game::Visibility": Inherited,
        })})"#;
        let doc = parse(src);
        let diags = validate_scene(&model, &registry, &doc, None);
        assert!(
            diags.iter().all(|d| d.severity() != SceneSeverity::Error),
            "a fully-valid registered scene shows zero errors, got: {diags:?}"
        );
    }

    #[test]
    fn three_states_are_distinguishable() {
        let (registry, model) = registry_and_model();
        // NoRegistry vs TypeNotInRegistry vs RegisteredMismatch each carry a
        // distinct (severity, code) pair.
        let empty = BevyRegistry::default();
        let no_reg = validate_scene(&model, &empty, &parse("()"), None);
        assert_eq!(no_reg[0].code(), SceneDiagnosticCode::NoRegistry);

        let unreg = validate_scene(
            &model,
            &registry,
            &parse(r#"(entities: {0: (components: {"game::Nope": (a: 1)})})"#),
            None,
        );
        assert_eq!(unreg[0].code(), SceneDiagnosticCode::TypeNotInRegistry);

        let mismatch = validate_scene(
            &model,
            &registry,
            &parse(r#"(entities: {0: (components: {"game::Vec3": (x: "s", y: 0.0, z: 0.0)})})"#),
            None,
        );
        assert!(mismatch
            .iter()
            .any(|d| matches!(d.code(), SceneDiagnosticCode::Mismatch(_))));

        // The code strings are globally distinct.
        assert_ne!(
            SceneDiagnosticCode::NoRegistry.code(),
            SceneDiagnosticCode::TypeNotInRegistry.code()
        );
        assert_eq!(SceneDiagnosticCode::NoRegistry.source(), "ronin-bevy");
        assert_eq!(
            SceneDiagnosticCode::Mismatch(ronin_core::DiagnosticCode::TypeMismatch).source(),
            "ronin-types"
        );
    }

    #[test]
    fn staleness_advisory_only_when_configured_and_disagreeing() {
        let (registry, _model) = registry_and_model(); // apparent "0.16.0"
                                                       // No expected version → no advisory.
        assert!(staleness_advisory(&registry, None).is_none());
        // Agreeing → no advisory.
        assert!(staleness_advisory(&registry, Some("0.16.0")).is_none());
        // Disagreeing → an advisory (not an error).
        let adv = staleness_advisory(&registry, Some("0.15.0")).expect("an advisory");
        assert_eq!(adv.code(), SceneDiagnosticCode::StalenessAdvisory);
        assert_eq!(adv.severity(), SceneSeverity::Advisory);
        assert!(adv.severity().is_informational());
    }

    #[test]
    fn unparseable_scene_region_does_not_panic() {
        let (registry, model) = registry_and_model();
        // A garbled component value must not crash interpretation/validation; the
        // well-formed sibling still validates.
        let src = r#"(entities: {0: (components: {
            "game::Vec3": @@@,
            "game::Visibility": Inherited,
        })})"#;
        let doc = parse(src);
        let diags = validate_scene(&model, &registry, &doc, None);
        // No panic reaching here is the core invariant; the parseable remainder is
        // validated (Visibility::Inherited is valid → no error from it).
        assert!(diags.iter().all(|d| d.severity() != SceneSeverity::Error));
    }
}
