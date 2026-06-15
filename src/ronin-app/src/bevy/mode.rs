//! Per-document mode selection (`{serde, Bevy}`) and the per-pattern registry
//! binding config (FR-009/FR-010/FR-012/FR-013, AD-003/AD-004, HINT-004).
//!
//! This module is the *pure* mode + registry-binding core. It defines:
//!
//! * [`Mode`] — the mutually-exclusive `{Serde, Bevy}` mode set (FR-013) and
//!   [`ModeOrigin`] (auto-detect vs explicit override);
//! * [`RegistryBindingConfig`] / [`RegistryBindingRule`] — the **single persisted
//!   artifact** of E009 (a project-scoped local file), mapping a glob `pattern`
//!   (with optional `exclude`) to a `registry_export_path` (+ optional per-pattern
//!   `mode` and `expected_bevy_version`) (FR-010);
//! * [`ResolvedRegistryBinding`] — the deterministic answer to "which registry
//!   export, if any, does this document bind to?" produced by
//!   [`RegistryBindingConfig::resolve`];
//! * [`ModeState`] — the per-document, transient state recording the active mode,
//!   its origin, the resolved bound registry (or none), and the registry-load
//!   state distinguishing **NoRegistry** from a loaded registry (FR-006/009/012).
//!
//! It performs **no** UI and **no** type acquisition. The only IO it does is the
//! E006-style config load/save (mirroring [`crate::binding::BindingConfig`]) and
//! a single **read-only** open of a `registry_export_path` to acquire its
//! [`BevyRegistry`] — never widening file access beyond that one named file
//! (CHK029, FR-010). Everything else is pure and unit-testable.
//!
//! # Why a *parallel* config to E006 (HINT-004)
//!
//! Bevy mode **replaces** the active type source rather than composing with the
//! E006 binding (AD-003): a Bevy-mode document validates against its bound
//! *registry*, not an E006 type+source. So this is a **parallel**
//! [`RegistryBindingConfig`] (glob → registry-export-path), distinct from E006's
//! [`BindingConfig`](crate::binding::BindingConfig) (glob → type + source) — but
//! it deliberately **reuses E006's resolution mechanism**: it shares E006's
//! [`glob_matches`](crate::binding::glob_matches) /
//! [`specificity`](crate::binding::pattern_specificity) primitives and mirrors the
//! exact resolution shape (exclusions first → most-specific pattern wins →
//! later-declared tie-break → per-document override applied by the caller). The
//! glob/specificity logic is **not** re-implemented here.
//!
//! # Resolution algorithm (mirrors E006 `binding.rs`)
//!
//! 1. **Exclusions are absolute and applied first.** A rule whose `pattern`
//!    matches the document path but whose *any* `exclude` glob also matches is
//!    removed from candidacy entirely (FR-010).
//! 2. **Most-specific wins.** Among the remaining matching rules, the one with the
//!    most *literal* (non-wildcard) characters in its pattern is chosen (FR-010).
//! 3. **Tie-break = later-declared.** Equal specificity ⇒ the rule declared later
//!    in [`RegistryBindingConfig::rules`] wins; resolution is otherwise
//!    order-independent.
//! 4. **No match → [`ResolvedRegistryBinding::none`].** A first-class, valid state
//!    — never an error (FR-010, SC-002).
//! 5. **Per-document override wins absolutely** (applied by the caller via
//!    [`RegistryBindingConfig::resolve_with_override`]).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use ron_types::{BevyRegistry, BevySource, TypeSource};

use crate::binding::{contain_type_source, glob_matches, pattern_specificity};

/// Current on-disk [`RegistryBindingConfig`] format version.
///
/// Persisted so a future format change can be detected; an absent/mismatched
/// version degrades to an **empty** config rather than crashing (FR-010, SC-002).
/// Mirrors [`crate::binding::BINDING_CONFIG_VERSION`].
pub const REGISTRY_BINDING_CONFIG_VERSION: u32 = 1;

/// The project-local registry-binding-config file name inside the `.ronin/` dir.
///
/// Stored **separately** from E006's `bindings.json` because it is a *parallel*
/// config with a different mapping (glob → registry-export, not glob →
/// type+source); the two never share a file (HINT-004).
const REGISTRY_BINDING_CONFIG_FILE: &str = "bevy-registries.json";

/// The project-local directory RONin stores per-project state in (mirrors
/// [`crate::binding`]'s `.ronin`). Project-scoped, not the OS config dir.
const PROJECT_STATE_DIR: &str = ".ronin";

// ===========================================================================
// Mode + origin (FR-009, FR-013)
// ===========================================================================

/// The per-document mode — exactly **`{Serde, Bevy}`**, mutually exclusive (no
/// composition) (FR-013).
///
/// The active mode selects which type source feeds the type model: **Bevy mode
/// replaces the active source with the bound Bevy registry** (scene-aware
/// validation); **Serde mode uses the E006 type binding-config** as today. A
/// document is validated under exactly one mode's type source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Mode {
    /// Serde mode — the E006 binding-config selects the type source (today's
    /// behavior); the default for every extension except `.scn`/`.scn.ron`.
    #[default]
    Serde,
    /// Bevy mode — the bound Bevy registry replaces the active type source and
    /// scene-aware validation/affordances apply.
    Bevy,
}

impl Mode {
    /// The stable lowercase label for this mode.
    #[inline]
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Serde => "serde",
            Mode::Bevy => "bevy",
        }
    }

    /// Auto-detect the mode from a document path by **file extension ONLY** —
    /// `.scn.ron` and `.scn` ⇒ [`Mode::Bevy`]; **every other extension** ⇒
    /// [`Mode::Serde`] (FR-009).
    ///
    /// There is **no content sniffing**: the detection looks only at the path's
    /// suffix. A misnamed file (a Bevy scene saved `.ron`, or a non-scene
    /// `.scn.ron`) is handled via the explicit per-document override, not here.
    #[must_use]
    pub fn detect_from_path(path: &Path) -> Self {
        if is_scene_path(path) {
            Mode::Bevy
        } else {
            Mode::Serde
        }
    }
}

/// `true` iff `path` ends with the Bevy scene suffix `.scn.ron` or `.scn`
/// (case-insensitive on the extension), the **only** signal for auto-detection
/// (FR-009 — extension only, no content sniffing).
///
/// `.scn.ron` is matched ahead of `.ron` so a `foo.scn.ron` is a scene, while a
/// plain `foo.ron` is not.
#[must_use]
fn is_scene_path(path: &Path) -> bool {
    // Compare on the lowercased final file name so `.SCN.RON` etc. still detect.
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".scn.ron") || lower.ends_with(".scn")
}

/// Where a document's [`Mode`] came from (FR-009).
///
/// An explicit per-document override (`Override`) always wins over extension
/// auto-detection (`AutoDetected`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum ModeOrigin {
    /// The mode was auto-detected from the file extension (or the project default
    /// when configured) (FR-009).
    #[default]
    AutoDetected,
    /// The mode was set by an explicit per-document toggle — wins over detection
    /// (FR-009).
    Override,
}

// ===========================================================================
// RegistryBindingConfig + RegistryBindingRule (PERSISTED — FR-010)
// ===========================================================================

/// One rule in a [`RegistryBindingConfig`]: a glob `pattern` (with optional
/// `exclude` globs) binding matching scenes to a `registry_export_path` (+ an
/// optional per-pattern `mode` and `expected_bevy_version`) (FR-010).
///
/// The E006 analogue is [`BindingRule`](crate::binding::BindingRule); this rule
/// maps to a **registry export** rather than a type + source (HINT-004).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryBindingRule {
    /// Glob pattern matched against the document path (FR-010, E006-style).
    pub pattern: String,
    /// Optional subtractive exclusion globs. A document matching `pattern` but
    /// also matching *any* of these is removed from this rule's candidacy
    /// entirely (FR-010, E006-style). `None`/empty ⇒ no exclusions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
    /// The registry-schema-export path this rule binds matching scenes to —
    /// handed to [`BevySource`] to acquire a [`BevyRegistry`]. Resolved relative
    /// to the project root, but MAY be a user-specified absolute / out-of-tree
    /// local path (explicit local config — a game build dir, a shared registry);
    /// it is opened **read-only as data** only (FR-010, CHK029).
    pub registry_export_path: PathBuf,
    /// An optional per-pattern mode hint applied where extension auto-detect is
    /// ambiguous; `None` ⇒ no hint (FR-009/010).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    /// An optional expected Bevy version compared against the export's apparent
    /// version to raise the FR-008 staleness advisory; `None` ⇒ no staleness
    /// advisory (validation/elision unaffected) (FR-008/010).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_bevy_version: Option<String>,
}

impl RegistryBindingRule {
    /// Number of **literal** (non-wildcard) characters in `pattern` — the E006
    /// specificity score (most-specific wins), computed via the shared
    /// [`pattern_specificity`] primitive (NOT re-implemented here) (FR-010).
    #[must_use]
    pub fn specificity(&self) -> usize {
        pattern_specificity(&self.pattern)
    }
}

/// The project-level, persisted mapping from scene patterns to registry exports
/// — **the single artifact E009 persists** (FR-010).
///
/// Everything else in Bevy mode (the ingested registry, the scene model, the mode
/// state) is transient. [`RegistryBindingConfig::default`] is an **empty** config
/// (zero rules, no project default) so a missing/corrupt file resolves to defaults
/// (auto-detect mode, no registry) rather than crashing (FR-010, SC-002).
///
/// Reuses E006's [`BindingConfig`](crate::binding::BindingConfig) *resolution
/// mechanism* (most-specific wins + per-document override) but is a **parallel**
/// config with a registry-export mapping (HINT-004).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryBindingConfig {
    /// The ordered rules. Order breaks **only** exact specificity ties
    /// (later-declared wins); specificity is compared first and is
    /// order-independent (FR-010, E006-style).
    #[serde(default)]
    pub rules: Vec<RegistryBindingRule>,
    /// The project-level default mode applied where extension auto-detect is
    /// ambiguous (FR-009). `None` ⇒ no project default (extension detection
    /// governs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_default_mode: Option<Mode>,
    /// Format version / schema tag; lets a future change be detected and degraded
    /// safely (FR-010).
    #[serde(default = "default_version")]
    pub version: u32,
}

fn default_version() -> u32 {
    REGISTRY_BINDING_CONFIG_VERSION
}

impl Default for RegistryBindingConfig {
    fn default() -> Self {
        RegistryBindingConfig {
            rules: Vec::new(),
            project_default_mode: None,
            version: REGISTRY_BINDING_CONFIG_VERSION,
        }
    }
}

impl RegistryBindingConfig {
    /// The project-scoped on-disk path for the registry-binding config:
    /// `<project_root>/.ronin/bevy-registries.json`.
    ///
    /// **Project-local, NOT the OS config directory** (mirrors
    /// [`crate::binding::BindingConfig::project_config_path`]): the registry
    /// binding is a property of *the project*, so it travels with the project
    /// tree. Stored in a **separate** file from E006's `bindings.json` (HINT-004).
    #[must_use]
    pub fn project_config_path(project_root: &Path) -> PathBuf {
        project_root
            .join(PROJECT_STATE_DIR)
            .join(REGISTRY_BINDING_CONFIG_FILE)
    }

    /// Load a registry-binding config from `path`, falling back to
    /// [`RegistryBindingConfig::default`] (an **empty** config) when the file is
    /// absent, unreadable, corrupt, or carries an unknown format version.
    ///
    /// Never panics and never errors — a missing/bad config degrades to defaults
    /// (auto-detect mode, no registry) rather than locking the user out (FR-010,
    /// SC-002, project-instructions §I). Mirrors
    /// [`crate::binding::BindingConfig::load_from`].
    #[must_use]
    pub fn load_from(path: &Path) -> Self {
        let Ok(bytes) = std::fs::read(path) else {
            return Self::default();
        };
        let config = match serde_json::from_slice::<RegistryBindingConfig>(&bytes) {
            Ok(config) => config,
            // Corrupt / unparseable bytes ⇒ empty config (no panic, no error).
            Err(_) => return Self::default(),
        };
        if config.version != REGISTRY_BINDING_CONFIG_VERSION {
            // Unknown / incompatible format version ⇒ degrade to empty rather than
            // risk misinterpreting a future schema (FR-010).
            return Self::default();
        }
        config
    }

    /// Persist this config to `path` as pretty-printed JSON, creating parent
    /// directories (e.g. the project-local `.ronin/`) if needed.
    ///
    /// Mirrors [`crate::binding::BindingConfig::save_to`]: human-inspectable
    /// pretty JSON, parent dirs auto-created.
    ///
    /// # Errors
    ///
    /// Returns an [`std::io::Error`] if a parent directory cannot be created or
    /// the file cannot be written.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Serialization of the plain config cannot fail; map defensively anyway.
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)
    }

    /// Resolve a document path to its [`ResolvedRegistryBinding`] from this config
    /// alone (no override) (FR-010).
    ///
    /// Applies the E006 resolution shape — exclusions first, then most-specific,
    /// then later-declared tie-break — over the rules, reusing the shared
    /// [`glob_matches`] / [`pattern_specificity`] primitives. A document that
    /// matches no rule resolves to [`ResolvedRegistryBinding::none`] (a valid
    /// state, never an error). To layer a per-document override on top, use
    /// [`resolve_with_override`](Self::resolve_with_override).
    #[must_use]
    pub fn resolve(&self, doc_path: &Path) -> ResolvedRegistryBinding {
        self.resolve_with_override(Some(doc_path), None)
    }

    /// Resolve a document to its [`ResolvedRegistryBinding`], honoring an optional
    /// per-document override (FR-010).
    ///
    /// Precedence (mirrors [`crate::binding::resolve`]):
    /// 1. `override_` present ⇒ that binding wins absolutely
    ///    ([`RegistryBindingOrigin::Override`]); config is not consulted.
    /// 2. else config resolution: exclusions first → most-specific → later-declared
    ///    tie-break ([`RegistryBindingOrigin::Config`]).
    /// 3. else [`ResolvedRegistryBinding::none`].
    ///
    /// `doc_path` is `Option` so a not-yet-saved buffer (no path) can still take
    /// an override but resolves to no binding against config (nothing for a glob
    /// to match).
    #[must_use]
    pub fn resolve_with_override(
        &self,
        doc_path: Option<&Path>,
        override_: Option<&RegistryBindingOverride>,
    ) -> ResolvedRegistryBinding {
        // (1) Override wins absolutely (FR-010, mirrors E006).
        if let Some(ov) = override_ {
            return ResolvedRegistryBinding {
                state: ResolvedRegistryState::Bound {
                    registry_export_path: ov.registry_export_path.clone(),
                    expected_bevy_version: ov.expected_bevy_version.clone(),
                    origin: RegistryBindingOrigin::Override,
                },
            };
        }

        // No path ⇒ nothing for a glob to match ⇒ no binding.
        let Some(path) = doc_path else {
            return ResolvedRegistryBinding::none();
        };

        // (2) Config resolution: walk rules, keeping the best candidate.
        //
        // A candidate beats the incumbent when it is strictly more specific, OR it
        // is equally specific and declared later. Iterating in declaration order
        // and using `>=` for the equal case yields "later-declared wins" on ties
        // while keeping the comparison order-independent (FR-010, mirrors E006).
        let mut best: Option<(usize, &RegistryBindingRule)> = None;
        for rule in &self.rules {
            // (a) Exclusions are absolute and applied first.
            if !rule_is_candidate(rule, path) {
                continue;
            }
            let spec = rule.specificity();
            match best {
                Some((best_spec, _)) if spec >= best_spec => best = Some((spec, rule)),
                None => best = Some((spec, rule)),
                _ => {}
            }
        }

        match best {
            Some((_, rule)) => ResolvedRegistryBinding {
                state: ResolvedRegistryState::Bound {
                    registry_export_path: rule.registry_export_path.clone(),
                    expected_bevy_version: rule.expected_bevy_version.clone(),
                    origin: RegistryBindingOrigin::Config,
                },
            },
            None => ResolvedRegistryBinding::none(),
        }
    }

    /// Resolve the active [`Mode`] for a document path, layering the precedence
    /// order extension-detect → per-pattern rule `mode` → project default
    /// (FR-009).
    ///
    /// Returns the mode together with its [`ModeOrigin`]. This is the *config*
    /// view of mode selection; an explicit per-document override (which always
    /// wins) is applied by [`ModeState`], not here.
    ///
    /// Precedence when no per-document override is present:
    /// 1. extension auto-detect ([`Mode::detect_from_path`]) — `.scn`/`.scn.ron`
    ///    is unambiguously Bevy, so it is taken directly;
    /// 2. otherwise (a non-scene extension, i.e. detection said Serde) a matching
    ///    rule's `mode` hint, then the [`project_default_mode`](Self::project_default_mode),
    ///    may upgrade the mode — but the result still carries
    ///    [`ModeOrigin::AutoDetected`] (it was not an explicit per-document toggle).
    #[must_use]
    pub fn resolve_mode(&self, doc_path: &Path) -> (Mode, ModeOrigin) {
        let detected = Mode::detect_from_path(doc_path);
        // A scene extension is unambiguous Bevy — never overridden by a hint.
        if detected == Mode::Bevy {
            return (Mode::Bevy, ModeOrigin::AutoDetected);
        }
        // Non-scene extension: a per-pattern rule hint, then the project default,
        // may select the mode (FR-009). Still AutoDetected (no explicit toggle).
        if let ResolvedRegistryState::Bound { .. } = self.resolve(doc_path).state {
            if let Some(rule) = self.best_rule(doc_path) {
                if let Some(mode) = rule.mode {
                    return (mode, ModeOrigin::AutoDetected);
                }
            }
        }
        if let Some(default_mode) = self.project_default_mode {
            return (default_mode, ModeOrigin::AutoDetected);
        }
        (detected, ModeOrigin::AutoDetected)
    }

    /// The single winning rule for `doc_path` under the E006 resolution shape, or
    /// `None` when no rule matches. Shared by [`resolve`](Self::resolve) and
    /// [`resolve_mode`](Self::resolve_mode).
    #[must_use]
    fn best_rule(&self, doc_path: &Path) -> Option<&RegistryBindingRule> {
        let mut best: Option<(usize, &RegistryBindingRule)> = None;
        for rule in &self.rules {
            if !rule_is_candidate(rule, doc_path) {
                continue;
            }
            let spec = rule.specificity();
            match best {
                Some((best_spec, _)) if spec >= best_spec => best = Some((spec, rule)),
                None => best = Some((spec, rule)),
                _ => {}
            }
        }
        best.map(|(_, rule)| rule)
    }
}

/// Is `rule` a candidate for `path`? — its `pattern` matches AND none of its
/// `exclude` globs match (exclusion is absolute). Reuses the shared
/// [`glob_matches`] primitive (FR-010, mirrors E006 `binding::rule_is_candidate`).
fn rule_is_candidate(rule: &RegistryBindingRule, path: &Path) -> bool {
    if !glob_matches(&rule.pattern, path) {
        return false;
    }
    if let Some(excludes) = &rule.exclude {
        if excludes.iter().any(|ex| glob_matches(ex, path)) {
            return false;
        }
    }
    true
}

// ===========================================================================
// Per-document override (transient — FR-010)
// ===========================================================================

/// An explicit, per-document **session** override of the bound registry (FR-010).
///
/// When set on a document it binds that document to a chosen
/// `registry_export_path` for the session and **always** takes precedence over
/// any config match ([`RegistryBindingOrigin::Override`]). It is never persisted
/// (only [`RegistryBindingConfig`] persists). Mirrors
/// [`crate::binding::DocumentOverride`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryBindingOverride {
    /// The registry-export path the document is forced to bind to (read-only data,
    /// may be out-of-tree — same handling as a rule's path) (FR-010).
    pub registry_export_path: PathBuf,
    /// An optional expected Bevy version for the staleness advisory (FR-008).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_bevy_version: Option<String>,
}

// ===========================================================================
// ResolvedRegistryBinding (transient — the resolution answer, FR-010)
// ===========================================================================

/// Which resolution path produced a bound [`ResolvedRegistryBinding`] (FR-010).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RegistryBindingOrigin {
    /// Produced by a per-document [`RegistryBindingOverride`] (wins over config).
    Override,
    /// Produced by [`RegistryBindingConfig`] resolution.
    Config,
}

/// The state of a resolved registry binding (FR-010).
///
/// [`ResolvedRegistryState::None`] is a first-class, valid state (no rule matched
/// and no override applied) — never an error; the document is then in the
/// `NoRegistry` registry state (structural-only / hint).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolvedRegistryState {
    /// The document is bound to a registry export at this path.
    Bound {
        /// The bound registry-export path (read-only data; may be out-of-tree).
        registry_export_path: PathBuf,
        /// The optional configured expected Bevy version (staleness advisory).
        expected_bevy_version: Option<String>,
        /// Which path produced the binding (override vs config) (FR-010).
        origin: RegistryBindingOrigin,
    },
    /// No rule matched and no override applied — the `NoRegistry` state (FR-010).
    None,
}

/// The resolved answer to "which registry export, if any, does this document bind
/// to?" — or the explicit [`ResolvedRegistryState::None`] (FR-010).
///
/// Transient: re-resolved on document/config/override change, never persisted.
/// The actual [`BevyRegistry`] is acquired on demand from the bound path by
/// [`ModeState`] (references, not copies — a changed export re-acquires).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedRegistryBinding {
    /// The resolved state (bound or none).
    pub state: ResolvedRegistryState,
}

impl ResolvedRegistryBinding {
    /// Construct the explicit no-binding state (FR-010).
    #[must_use]
    pub fn none() -> Self {
        ResolvedRegistryBinding {
            state: ResolvedRegistryState::None,
        }
    }

    /// `true` iff this binding is [`ResolvedRegistryState::Bound`].
    #[must_use]
    pub fn is_bound(&self) -> bool {
        matches!(self.state, ResolvedRegistryState::Bound { .. })
    }

    /// The bound registry-export path, or `None` when not bound.
    #[must_use]
    pub fn registry_export_path(&self) -> Option<&Path> {
        match &self.state {
            ResolvedRegistryState::Bound {
                registry_export_path,
                ..
            } => Some(registry_export_path.as_path()),
            ResolvedRegistryState::None => None,
        }
    }

    /// The configured expected Bevy version, or `None` when not bound / not
    /// configured (FR-008).
    #[must_use]
    pub fn expected_bevy_version(&self) -> Option<&str> {
        match &self.state {
            ResolvedRegistryState::Bound {
                expected_bevy_version,
                ..
            } => expected_bevy_version.as_deref(),
            ResolvedRegistryState::None => None,
        }
    }

    /// The origin (override vs config), or `None` when not bound (FR-010).
    #[must_use]
    pub fn origin(&self) -> Option<RegistryBindingOrigin> {
        match &self.state {
            ResolvedRegistryState::Bound { origin, .. } => Some(*origin),
            ResolvedRegistryState::None => None,
        }
    }
}

// ===========================================================================
// RegistryLoad — the NoRegistry-vs-loaded distinction (FR-006)
// ===========================================================================

/// The registry-load state [`ModeState`] holds: the **NoRegistry-vs-loaded**
/// distinction the data-model `registry_state` requires at the *binding* level
/// (FR-006).
///
/// The fine-grained per-value states (`TypeNotInRegistry` / `RegisteredMismatch`)
/// are produced by [`crate::bevy::validate::validate_scene`] per scene value;
/// [`ModeState`] only needs to distinguish **NoRegistry** (none bound, or the
/// bound path is unloadable / access-denied / missing) from a **loaded** registry
/// handle (FR-006/010, CHK029). An empty loaded registry (degraded at ingest) is
/// also surfaced as `NoRegistry` so the UI shows the "no registry" hint
/// consistently with [`crate::bevy::validate`].
#[derive(Debug, Clone, Default)]
pub enum RegistryLoad {
    /// No registry: none bound, or the bound export path could not be opened /
    /// resolved / parsed into any registered type (degrades to no-registry, never
    /// an error) (FR-006/010, CHK029).
    #[default]
    NoRegistry,
    /// A non-empty registry loaded from the bound export path, with its apparent
    /// version (when the export carried one) for the staleness advisory (FR-008).
    Loaded {
        /// The ingested registry (the in-memory presence lookup the scene
        /// validator/elision consult by type path). Transient — re-acquired on a
        /// config/source change, never persisted.
        registry: BevyRegistry,
        /// The **already-serialized** E004 interchange for `registry` — the
        /// `ron_types::to_json` of `BevySource::from_registry(registry).acquire().model`,
        /// the same serialization serde mode hands the validator (E009/IP-004).
        ///
        /// Acquired **once** at [`ModeState::load_registry`] time so the per-frame /
        /// per-keystroke validation path never re-acquires it; shared by [`Arc`] so
        /// the per-keystroke reparse request clones only a pointer, never the
        /// (potentially large) schema. `validate_scene` consumes this `Value` (its
        /// `$defs` keyed by fully-qualified Bevy type path) paired with `registry`.
        model: Arc<Value>,
    },
}

impl RegistryLoad {
    /// `true` iff a non-empty registry is loaded.
    #[must_use]
    pub fn is_loaded(&self) -> bool {
        matches!(self, RegistryLoad::Loaded { .. })
    }

    /// The loaded registry, or `None` in the [`RegistryLoad::NoRegistry`] state.
    #[must_use]
    pub fn registry(&self) -> Option<&BevyRegistry> {
        match self {
            RegistryLoad::Loaded { registry, .. } => Some(registry),
            RegistryLoad::NoRegistry => None,
        }
    }

    /// The loaded registry's serialized E004 interchange `model` (shared by `Arc`),
    /// or `None` in the [`RegistryLoad::NoRegistry`] state (E009/IP-004).
    ///
    /// Acquired once at load time; this is the `Value` `validate_scene` consumes.
    #[must_use]
    pub fn model(&self) -> Option<&Arc<Value>> {
        match self {
            RegistryLoad::Loaded { model, .. } => Some(model),
            RegistryLoad::NoRegistry => None,
        }
    }

    /// The loaded registry's apparent Bevy version, if any — surfaced (together
    /// with the configured expected version) for the FR-008 staleness advisory.
    #[must_use]
    pub fn apparent_version(&self) -> Option<&str> {
        self.registry().and_then(BevyRegistry::apparent_version)
    }
}

// ===========================================================================
// ModeState (transient per-document — FR-006/009/012/013)
// ===========================================================================

/// The per-document, transient mode state (FR-006/009/011/012/013).
///
/// Records the **active mode** (`{Serde, Bevy}`, extension-auto-detected with an
/// explicit override winning), the **resolved bound registry** (or none), and the
/// **registry-load state** ([`RegistryLoad`]) distinguishing NoRegistry from a
/// loaded registry. Held **1:1 per document** — there is **no global state**, so
/// different open documents may be in different modes bound to different
/// registries simultaneously (coexistence, FR-012).
///
/// Switching mode changes **zero bytes** (mode is a behavior selection, not an
/// edit — FR-011); this type holds no document bytes.
#[derive(Debug, Clone, Default)]
pub struct ModeState {
    /// The active mode (mutually exclusive `{Serde, Bevy}`) (FR-013).
    active_mode: Mode,
    /// Whether the active mode came from extension auto-detection or an explicit
    /// per-document toggle (override wins) (FR-009).
    mode_origin: ModeOrigin,
    /// The resolved bound registry binding (the export path + expected version, or
    /// none) — override > config (FR-010).
    binding: ResolvedRegistryBinding,
    /// The registry-load state: NoRegistry vs a loaded registry handle (FR-006).
    load: RegistryLoad,
}

impl Default for ResolvedRegistryBinding {
    fn default() -> Self {
        ResolvedRegistryBinding::none()
    }
}

impl ModeState {
    /// Build the per-document mode state for `doc_path` from `config`, with an
    /// optional explicit per-document mode override and registry override
    /// (FR-009/010/012).
    ///
    /// Mode precedence: an explicit `mode_override` wins ([`ModeOrigin::Override`]);
    /// otherwise [`RegistryBindingConfig::resolve_mode`] (extension auto-detect →
    /// per-pattern hint → project default). Registry precedence: a
    /// `registry_override` wins over config ([`RegistryBindingConfig::resolve_with_override`]).
    ///
    /// The bound registry is **not** loaded here (loading is IO — see
    /// [`load_registry`](Self::load_registry)); the new state starts in
    /// [`RegistryLoad::NoRegistry`]. `doc_path` is `Option` so an unsaved buffer
    /// (no path) still gets a state (Serde, no binding) and can take overrides.
    #[must_use]
    pub fn resolve(
        config: &RegistryBindingConfig,
        doc_path: Option<&Path>,
        mode_override: Option<Mode>,
        registry_override: Option<&RegistryBindingOverride>,
    ) -> Self {
        let (active_mode, mode_origin) = match (mode_override, doc_path) {
            // An explicit per-document toggle always wins (FR-009).
            (Some(mode), _) => (mode, ModeOrigin::Override),
            // No override + a path ⇒ config-driven auto-detect (FR-009).
            (None, Some(path)) => config.resolve_mode(path),
            // No override + no path ⇒ the default mode (Serde), auto-detected.
            (None, None) => (Mode::default(), ModeOrigin::AutoDetected),
        };
        let binding = config.resolve_with_override(doc_path, registry_override);
        ModeState {
            active_mode,
            mode_origin,
            binding,
            load: RegistryLoad::NoRegistry,
        }
    }

    /// The active mode (FR-013).
    #[must_use]
    pub fn active_mode(&self) -> Mode {
        self.active_mode
    }

    /// `true` iff the active mode is [`Mode::Bevy`].
    #[must_use]
    pub fn is_bevy(&self) -> bool {
        self.active_mode == Mode::Bevy
    }

    /// Where the active mode came from (auto-detect vs explicit override) (FR-009).
    #[must_use]
    pub fn mode_origin(&self) -> ModeOrigin {
        self.mode_origin
    }

    /// Apply an explicit per-document mode toggle. The override **wins** over
    /// auto-detection ([`ModeOrigin::Override`]); switching changes zero bytes
    /// (FR-009/011).
    pub fn set_mode_override(&mut self, mode: Mode) {
        self.active_mode = mode;
        self.mode_origin = ModeOrigin::Override;
    }

    /// The resolved bound registry binding (export path + expected version, or
    /// none); override > config (FR-010).
    #[must_use]
    pub fn binding(&self) -> &ResolvedRegistryBinding {
        &self.binding
    }

    /// The bound registry-export path, or `None` when no registry is bound.
    #[must_use]
    pub fn bound_registry_path(&self) -> Option<&Path> {
        self.binding.registry_export_path()
    }

    /// The registry-load state (NoRegistry vs loaded) (FR-006).
    #[must_use]
    pub fn registry_load(&self) -> &RegistryLoad {
        &self.load
    }

    /// `true` iff a non-empty registry is currently loaded (FR-006).
    #[must_use]
    pub fn has_registry(&self) -> bool {
        self.load.is_loaded()
    }

    /// The loaded registry, or `None` in the NoRegistry state (FR-006).
    #[must_use]
    pub fn registry(&self) -> Option<&BevyRegistry> {
        self.load.registry()
    }

    /// The loaded registry's serialized E004 interchange `model` (shared by `Arc`),
    /// or `None` in the NoRegistry state (E009/IP-004).
    ///
    /// Acquired once at [`load_registry`](Self::load_registry) time; this is the
    /// `Value` the off-frame scene validator (`validate_scene`) consumes, paired
    /// with [`registry`](Self::registry). The document clones the `Arc` into the
    /// per-keystroke reparse request (cheap — a pointer, never the schema).
    #[must_use]
    pub fn registry_model(&self) -> Option<&Arc<Value>> {
        self.load.model()
    }

    /// The configured expected Bevy version for the bound registry, or `None`
    /// (FR-008).
    #[must_use]
    pub fn expected_bevy_version(&self) -> Option<&str> {
        self.binding.expected_bevy_version()
    }

    /// The loaded registry's apparent Bevy version, if any (FR-008).
    #[must_use]
    pub fn apparent_bevy_version(&self) -> Option<&str> {
        self.load.apparent_version()
    }

    /// Whether a staleness advisory is warranted: a configured `expected` version
    /// disagrees with the loaded registry's `apparent` version (FR-008).
    ///
    /// Returns `false` when no expected version is configured, when no registry is
    /// loaded (or it carries no apparent version), or when the two agree. A skew
    /// is an advisory, never an error — this only reports *whether* to surface it.
    #[must_use]
    pub fn is_stale(&self) -> bool {
        match (self.expected_bevy_version(), self.apparent_bevy_version()) {
            (Some(expected), Some(apparent)) => expected != apparent,
            _ => false,
        }
    }

    /// Acquire the bound registry from its export path **read-only as data**,
    /// updating the [`RegistryLoad`] state in place (FR-006/010, CHK029).
    ///
    /// `project_root` is the root the bound path is resolved relative to. The path
    /// is opened **read-only**; a user-specified **absolute / out-of-tree** local
    /// path is allowed (explicit local config — a game build dir / shared
    /// registry, FR-010), but a `..`-traversal that would *escape* the root is
    /// rejected (it could only be a mistake/attack, and we never widen access
    /// silently). A non-resolvable / access-denied / missing / unparseable path
    /// degrades to [`RegistryLoad::NoRegistry`] (no error, no crash) — never
    /// widening file access beyond reading the single named export (CHK029).
    ///
    /// No-op (leaves the state `NoRegistry`) when no registry is bound. Returns
    /// `true` iff a non-empty registry was loaded.
    pub fn load_registry(&mut self, project_root: &Path) -> bool {
        self.load = RegistryLoad::NoRegistry;
        let Some(path) = self.binding.registry_export_path() else {
            return false;
        };
        let Some(resolved) = resolve_registry_path(project_root, path) else {
            // Non-resolvable / escaping path ⇒ NoRegistry (no error) (CHK029).
            return false;
        };
        // Read the single named export **read-only as data** and ingest it into the
        // in-memory `BevyRegistry` the validator / elision consult by type path
        // (the same `from_schema_json` `BevySource` uses internally). An
        // access-denied / missing path returns an Err at read time; ingestion of
        // malformed JSON never errors (it degrades to an empty registry) — either
        // way an empty registry is the NoRegistry state (FR-006, CHK029).
        let Ok(text) = std::fs::read_to_string(&resolved) else {
            // Access-denied / missing ⇒ NoRegistry (no error).
            return false;
        };
        let (registry, _diagnostics) =
            BevyRegistry::from_schema_json(&text, "bevy-registry", &resolved.display().to_string());
        if registry.is_empty() {
            // Malformed / partial / empty export degrades to NoRegistry.
            return false;
        }
        // Acquire the serialized E004 interchange ONCE, here at load time, so the
        // per-frame / per-keystroke validation path never re-serializes the schema
        // (E009/IP-004). `BevySource::from_registry` reuses the SAME serialization
        // serde mode hands the validator: `acquire()` → `ron_types::to_json`. The
        // registry is cloned into the source (it consumes one) and kept alongside
        // for the cheap presence lookup `validate_scene` pairs with the model.
        let model =
            ron_types::to_json(&BevySource::from_registry(registry.clone()).acquire().model);
        self.load = RegistryLoad::Loaded {
            registry,
            model: Arc::new(model),
        };
        true
    }
}

/// Resolve a bound `registry_export_path` against `project_root` for a
/// **read-only** open, allowing an absolute / out-of-tree local path but rejecting
/// a `..`-traversal that escapes the root (CHK029, FR-010).
///
/// Returns the path to open on success, or `None` when the path escapes the root
/// via `..` (caller degrades to NoRegistry). Unlike E006's
/// [`contain_type_source`] — which *requires* containment inside the project — a
/// registry export MAY legitimately live **outside** the tree (a game build dir, a
/// shared registry), so an explicit **absolute** path is accepted as-is. A
/// *relative* path is resolved inside the project and must not climb above it
/// (a relative `..` escape is the only rejected case).
#[must_use]
fn resolve_registry_path(project_root: &Path, candidate: &Path) -> Option<PathBuf> {
    if candidate.is_absolute() {
        // Explicit absolute local path: allowed (read-only, out-of-tree OK).
        return Some(candidate.to_path_buf());
    }
    // A relative path is interpreted inside the project root; reject only a
    // `..`-escape (which `contain_type_source` lexically detects). When contained,
    // use the joined-and-normalized path it returns.
    contain_type_source(project_root, candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Mode detection (FR-009, extension only) ----------------------------

    #[test]
    fn detects_bevy_for_scn_ron_and_scn_only() {
        assert_eq!(
            Mode::detect_from_path(Path::new("levels/world.scn.ron")),
            Mode::Bevy
        );
        assert_eq!(Mode::detect_from_path(Path::new("a/b.scn")), Mode::Bevy);
        // Case-insensitive on the extension.
        assert_eq!(Mode::detect_from_path(Path::new("X.SCN.RON")), Mode::Bevy);
    }

    #[test]
    fn detects_serde_for_every_other_extension() {
        assert_eq!(Mode::detect_from_path(Path::new("config.ron")), Mode::Serde);
        assert_eq!(Mode::detect_from_path(Path::new("data.json")), Mode::Serde);
        assert_eq!(Mode::detect_from_path(Path::new("noext")), Mode::Serde);
        // `.scn` only as a true suffix, not mid-name.
        assert_eq!(Mode::detect_from_path(Path::new("a.scn.txt")), Mode::Serde);
    }

    #[test]
    fn explicit_override_wins_over_detection() {
        let config = RegistryBindingConfig::default();
        // A `.scn.ron` auto-detects Bevy...
        let auto = ModeState::resolve(&config, Some(Path::new("w.scn.ron")), None, None);
        assert_eq!(auto.active_mode(), Mode::Bevy);
        assert_eq!(auto.mode_origin(), ModeOrigin::AutoDetected);
        // ...but an explicit Serde override wins.
        let forced = ModeState::resolve(
            &config,
            Some(Path::new("w.scn.ron")),
            Some(Mode::Serde),
            None,
        );
        assert_eq!(forced.active_mode(), Mode::Serde);
        assert_eq!(forced.mode_origin(), ModeOrigin::Override);

        // And a `.ron` (auto Serde) can be forced to Bevy via the toggle.
        let mut state = ModeState::resolve(&config, Some(Path::new("scene.ron")), None, None);
        assert_eq!(state.active_mode(), Mode::Serde);
        state.set_mode_override(Mode::Bevy);
        assert_eq!(state.active_mode(), Mode::Bevy);
        assert_eq!(state.mode_origin(), ModeOrigin::Override);
    }

    #[test]
    fn project_default_mode_applies_for_ambiguous_extension() {
        let config = RegistryBindingConfig {
            project_default_mode: Some(Mode::Bevy),
            ..Default::default()
        };
        // A `.ron` (non-scene) takes the project default when no override.
        let (mode, origin) = config.resolve_mode(Path::new("thing.ron"));
        assert_eq!(mode, Mode::Bevy);
        assert_eq!(origin, ModeOrigin::AutoDetected);
        // A `.scn.ron` is still unambiguous Bevy regardless of default.
        let (mode, _) = config.resolve_mode(Path::new("w.scn.ron"));
        assert_eq!(mode, Mode::Bevy);
    }

    #[test]
    fn per_pattern_mode_hint_selects_mode_for_non_scene_extension() {
        let config = RegistryBindingConfig {
            rules: vec![RegistryBindingRule {
                pattern: "levels/*.ron".to_string(),
                exclude: None,
                registry_export_path: PathBuf::from("registry.json"),
                mode: Some(Mode::Bevy),
                expected_bevy_version: None,
            }],
            ..Default::default()
        };
        let (mode, origin) = config.resolve_mode(Path::new("levels/a.ron"));
        assert_eq!(mode, Mode::Bevy, "the rule's mode hint upgrades a .ron");
        assert_eq!(origin, ModeOrigin::AutoDetected);
    }

    // -- Per-pattern resolution (FR-010, E006-style) ------------------------

    fn rule(pattern: &str, export: &str) -> RegistryBindingRule {
        RegistryBindingRule {
            pattern: pattern.to_string(),
            exclude: None,
            registry_export_path: PathBuf::from(export),
            mode: None,
            expected_bevy_version: None,
        }
    }

    #[test]
    fn most_specific_pattern_wins() {
        let config = RegistryBindingConfig {
            rules: vec![
                rule("**/*.scn.ron", "broad.json"),
                rule("levels/boss.scn.ron", "specific.json"),
            ],
            ..Default::default()
        };
        let resolved = config.resolve(Path::new("levels/boss.scn.ron"));
        assert_eq!(
            resolved.registry_export_path(),
            Some(Path::new("specific.json")),
            "the literal-heavy pattern wins"
        );
        assert_eq!(resolved.origin(), Some(RegistryBindingOrigin::Config));
        // A different scene still binds the broad rule.
        let other = config.resolve(Path::new("levels/other.scn.ron"));
        assert_eq!(other.registry_export_path(), Some(Path::new("broad.json")));
    }

    #[test]
    fn exclusion_is_applied_first() {
        let mut excluded = rule("levels/**/*.scn.ron", "reg.json");
        excluded.exclude = Some(vec!["levels/wip/**".to_string()]);
        let config = RegistryBindingConfig {
            rules: vec![excluded],
            ..Default::default()
        };
        // An excluded scene is not bound by the rule (NoRegistry).
        let wip = config.resolve(Path::new("levels/wip/draft.scn.ron"));
        assert!(!wip.is_bound(), "excluded path resolves to no binding");
        // A non-excluded scene binds it.
        let ok = config.resolve(Path::new("levels/final/boss.scn.ron"));
        assert_eq!(ok.registry_export_path(), Some(Path::new("reg.json")));
    }

    #[test]
    fn later_declared_wins_on_specificity_tie() {
        let config = RegistryBindingConfig {
            rules: vec![
                rule("levels/*.scn.ron", "first.json"),
                rule("levels/*.scn.ron", "second.json"),
            ],
            ..Default::default()
        };
        let resolved = config.resolve(Path::new("levels/a.scn.ron"));
        assert_eq!(
            resolved.registry_export_path(),
            Some(Path::new("second.json")),
            "equal specificity ⇒ later-declared wins (E006 tie-break)"
        );
    }

    #[test]
    fn per_document_override_wins_over_config() {
        let config = RegistryBindingConfig {
            rules: vec![rule("**/*.scn.ron", "config.json")],
            ..Default::default()
        };
        let ov = RegistryBindingOverride {
            registry_export_path: PathBuf::from("override.json"),
            expected_bevy_version: Some("0.16.0".to_string()),
        };
        let resolved = config.resolve_with_override(Some(Path::new("levels/a.scn.ron")), Some(&ov));
        assert_eq!(
            resolved.registry_export_path(),
            Some(Path::new("override.json"))
        );
        assert_eq!(resolved.origin(), Some(RegistryBindingOrigin::Override));
        assert_eq!(resolved.expected_bevy_version(), Some("0.16.0"));
    }

    #[test]
    fn no_match_resolves_to_no_binding() {
        let config = RegistryBindingConfig {
            rules: vec![rule("levels/*.scn.ron", "reg.json")],
            ..Default::default()
        };
        let resolved = config.resolve(Path::new("ui/menu.scn.ron"));
        assert!(!resolved.is_bound());
        assert_eq!(resolved.registry_export_path(), None);
        assert_eq!(resolved.origin(), None);
    }

    #[test]
    fn unsaved_buffer_no_path_resolves_to_no_binding_but_takes_override() {
        let config = RegistryBindingConfig {
            rules: vec![rule("**/*.scn.ron", "reg.json")],
            ..Default::default()
        };
        // No path ⇒ nothing to glob ⇒ no config binding.
        assert!(!config.resolve_with_override(None, None).is_bound());
        // But an override still binds.
        let ov = RegistryBindingOverride {
            registry_export_path: PathBuf::from("o.json"),
            expected_bevy_version: None,
        };
        assert!(config.resolve_with_override(None, Some(&ov)).is_bound());
    }

    #[test]
    fn expected_version_is_surfaced_for_staleness() {
        let mut r = rule("**/*.scn.ron", "reg.json");
        r.expected_bevy_version = Some("0.16.0".to_string());
        let config = RegistryBindingConfig {
            rules: vec![r],
            ..Default::default()
        };
        let state = ModeState::resolve(&config, Some(Path::new("a.scn.ron")), None, None);
        assert_eq!(state.expected_bevy_version(), Some("0.16.0"));
    }

    // -- Absent / corrupt config (FR-010, SC-002, no crash) -----------------

    #[test]
    fn absent_config_loads_empty_default() {
        let dir = temp_dir("absent");
        let path = dir.join("never-written.json");
        let loaded = RegistryBindingConfig::load_from(&path);
        assert_eq!(loaded, RegistryBindingConfig::default());
        assert!(loaded.rules.is_empty());
        // Defaults: a scene auto-detects Bevy but binds no registry (NoRegistry).
        let state = ModeState::resolve(&loaded, Some(Path::new("a.scn.ron")), None, None);
        assert_eq!(state.active_mode(), Mode::Bevy);
        assert!(!state.binding().is_bound());
    }

    #[test]
    fn corrupt_config_loads_empty_default_no_panic() {
        let dir = temp_dir("corrupt");
        let path = dir.join("corrupt.json");
        std::fs::write(&path, b"\x00 not json at all }{][").unwrap();
        let loaded = RegistryBindingConfig::load_from(&path);
        assert_eq!(loaded, RegistryBindingConfig::default());
        assert!(loaded.rules.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn version_mismatch_loads_empty_default() {
        let dir = temp_dir("version");
        let path = dir.join("future.json");
        let future = REGISTRY_BINDING_CONFIG_VERSION + 1;
        let json = format!(
            r#"{{ "rules": [ {{ "pattern": "**/*.scn.ron", "registry_export_path": "r.json" }} ], "version": {future} }}"#
        );
        std::fs::write(&path, json.as_bytes()).unwrap();
        let loaded = RegistryBindingConfig::load_from(&path);
        assert_eq!(loaded, RegistryBindingConfig::default());
        assert!(loaded.rules.is_empty(), "future-version rules discarded");
        let _ = std::fs::remove_file(&path);
    }

    // -- Round-trip persistence (save → load) -------------------------------

    #[test]
    fn round_trips_through_save_and_load() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("bevy-registries.json");
        let mut r = rule("levels/*.scn.ron", "registries/world.json");
        r.exclude = Some(vec!["levels/wip/**".to_string()]);
        r.mode = Some(Mode::Bevy);
        r.expected_bevy_version = Some("0.16.0".to_string());
        let config = RegistryBindingConfig {
            rules: vec![r],
            project_default_mode: Some(Mode::Serde),
            version: REGISTRY_BINDING_CONFIG_VERSION,
        };
        config.save_to(&path).unwrap();
        let loaded = RegistryBindingConfig::load_from(&path);
        assert_eq!(loaded, config);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn project_config_path_is_project_local_and_separate_from_e006() {
        let root = Path::new("/home/user/myproject");
        let path = RegistryBindingConfig::project_config_path(root);
        assert_eq!(path, root.join(".ronin").join("bevy-registries.json"));
        // Distinct from E006's bindings.json (a parallel config, HINT-004).
        assert_ne!(
            path,
            crate::binding::BindingConfig::project_config_path(root)
        );
    }

    // -- load_registry path handling (CHK029, FR-006/010) -------------------

    /// A tiny valid registry export written for the load tests.
    const TINY_REGISTRY: &str = r#"{
        "bevyVersion": "0.16.0",
        "$defs": { "game::Marker": { "kind": "Struct", "properties": {} } }
    }"#;

    #[test]
    fn loads_registry_from_in_tree_relative_path() {
        let root = temp_dir("load_in_tree");
        let export = root.join("registry.json");
        std::fs::write(&export, TINY_REGISTRY).unwrap();
        let config = RegistryBindingConfig {
            rules: vec![rule("**/*.scn.ron", "registry.json")],
            ..Default::default()
        };
        let mut state = ModeState::resolve(&config, Some(Path::new("w.scn.ron")), None, None);
        assert!(!state.has_registry(), "starts NoRegistry before load");
        assert!(state.registry_model().is_none(), "no model before load");
        assert!(state.load_registry(&root), "in-tree export loads");
        assert!(state.has_registry());
        assert!(state.registry().is_some_and(|r| r.contains("game::Marker")));
        assert_eq!(state.apparent_bevy_version(), Some("0.16.0"));
        // The serialized interchange `model` is acquired once at load time and
        // carries the registered type under `$defs` (E009/IP-004).
        let model = state.registry_model().expect("model acquired at load time");
        let defs = model
            .get("$defs")
            .and_then(|d| d.as_object())
            .expect("interchange has a $defs object");
        assert!(
            defs.contains_key("game::Marker"),
            "the acquired model's $defs carries the registered type"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn loads_registry_from_out_of_tree_absolute_path() {
        // The export lives OUTSIDE the project root (a shared / build-dir registry):
        // an explicit absolute path is allowed, read-only (FR-010, CHK029).
        let elsewhere = temp_dir("load_out_of_tree_export");
        let export = elsewhere.join("shared.json");
        std::fs::write(&export, TINY_REGISTRY).unwrap();
        let root = temp_dir("load_out_of_tree_root");
        let ov = RegistryBindingOverride {
            registry_export_path: export.clone(),
            expected_bevy_version: None,
        };
        let config = RegistryBindingConfig::default();
        let mut state = ModeState::resolve(&config, Some(Path::new("w.scn.ron")), None, Some(&ov));
        assert!(
            state.load_registry(&root),
            "an out-of-tree absolute export is allowed read-only"
        );
        assert!(state.has_registry());
        let _ = std::fs::remove_dir_all(&elsewhere);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_or_access_denied_path_degrades_to_no_registry() {
        let root = temp_dir("load_missing");
        let config = RegistryBindingConfig {
            rules: vec![rule("**/*.scn.ron", "does-not-exist.json")],
            ..Default::default()
        };
        let mut state = ModeState::resolve(&config, Some(Path::new("w.scn.ron")), None, None);
        assert!(
            !state.load_registry(&root),
            "a missing export degrades to NoRegistry (no error)"
        );
        assert!(!state.has_registry());
        assert!(state.registry().is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn escaping_relative_path_degrades_to_no_registry() {
        // A relative `..` escape above the project root is rejected (never widens
        // access); it degrades to NoRegistry rather than reading outside (CHK029).
        let root = temp_dir("load_escape");
        let config = RegistryBindingConfig {
            rules: vec![rule("**/*.scn.ron", "../../etc/passwd")],
            ..Default::default()
        };
        let mut state = ModeState::resolve(&config, Some(Path::new("w.scn.ron")), None, None);
        assert!(!state.load_registry(&root));
        assert!(!state.has_registry());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn empty_registry_export_degrades_to_no_registry() {
        // A malformed / empty export ingests to an empty registry ⇒ NoRegistry.
        let root = temp_dir("load_empty");
        let export = root.join("empty.json");
        std::fs::write(&export, "{ not json").unwrap();
        let config = RegistryBindingConfig {
            rules: vec![rule("**/*.scn.ron", "empty.json")],
            ..Default::default()
        };
        let mut state = ModeState::resolve(&config, Some(Path::new("w.scn.ron")), None, None);
        assert!(!state.load_registry(&root), "malformed export ⇒ NoRegistry");
        assert!(!state.has_registry());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unbound_document_load_is_noop_no_registry() {
        let root = temp_dir("load_unbound");
        let config = RegistryBindingConfig::default();
        let mut state = ModeState::resolve(&config, Some(Path::new("w.scn.ron")), None, None);
        assert!(!state.binding().is_bound());
        assert!(
            !state.load_registry(&root),
            "no bound path ⇒ no-op NoRegistry"
        );
        assert!(!state.has_registry());
        let _ = std::fs::remove_dir_all(&root);
    }

    // -- Staleness surfacing (FR-008) ---------------------------------------

    #[test]
    fn is_stale_only_when_configured_and_disagreeing() {
        let root = temp_dir("stale");
        let export = root.join("registry.json"); // apparent "0.16.0"
        std::fs::write(&export, TINY_REGISTRY).unwrap();

        // Configured expected disagrees with apparent ⇒ stale.
        let mut r = rule("**/*.scn.ron", "registry.json");
        r.expected_bevy_version = Some("0.15.0".to_string());
        let config = RegistryBindingConfig {
            rules: vec![r],
            ..Default::default()
        };
        let mut state = ModeState::resolve(&config, Some(Path::new("w.scn.ron")), None, None);
        assert!(state.load_registry(&root));
        assert!(state.is_stale(), "0.15 expected vs 0.16 apparent ⇒ stale");

        // No expected version configured ⇒ not stale.
        let config2 = RegistryBindingConfig {
            rules: vec![rule("**/*.scn.ron", "registry.json")],
            ..Default::default()
        };
        let mut state2 = ModeState::resolve(&config2, Some(Path::new("w.scn.ron")), None, None);
        assert!(state2.load_registry(&root));
        assert!(!state2.is_stale(), "no expected version ⇒ never stale");
        let _ = std::fs::remove_dir_all(&root);
    }

    // -- Per-document coexistence (FR-012) ----------------------------------

    #[test]
    fn two_documents_coexist_in_different_modes_and_registries() {
        let config = RegistryBindingConfig {
            rules: vec![rule("a/*.scn.ron", "a.json"), rule("b/*.scn.ron", "b.json")],
            ..Default::default()
        };
        let doc_a = ModeState::resolve(&config, Some(Path::new("a/x.scn.ron")), None, None);
        let doc_b = ModeState::resolve(&config, Some(Path::new("b/y.ron")), None, None);
        // Different modes (a is a scene, b is serde) and different bindings —
        // no global state, fully independent (FR-012).
        assert_eq!(doc_a.active_mode(), Mode::Bevy);
        assert_eq!(doc_b.active_mode(), Mode::Serde);
        assert_eq!(doc_a.bound_registry_path(), Some(Path::new("a.json")));
        assert!(!doc_b.binding().is_bound());
    }

    /// A fresh temp directory for a test, named by `tag`.
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ronin_registry_binding_{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
