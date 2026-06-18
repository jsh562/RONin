//! Binding configuration and resolution (E006 US2 — FR-008, FR-009, FR-010,
//! FR-012, FR-013).
//!
//! This module is the *pure* binding-config core: it defines the persisted
//! [`BindingConfig`] / [`BindingRule`] shapes, the per-document session
//! [`DocumentOverride`], and the deterministic [`resolve`] / [`resolve_binding`]
//! functions that turn a document path + config (+ optional override) into a
//! [`TypeBinding`]. It performs **no** UI, **no** file IO, and **no** type
//! acquisition — those live in Phase 4b. Everything here is unit-testable.
//!
//! # Resolution algorithm (FR-009, FR-012)
//!
//! 1. **Override wins absolutely.** When a [`DocumentOverride`] is present it
//!    always produces a `Bound` binding with [`BindingOrigin::Override`],
//!    regardless of any matching config rule (FR-009).
//! 2. **Exclusions are absolute and applied first.** A rule whose `pattern`
//!    matches the document path but whose *any* `exclude` glob also matches is
//!    removed from candidacy entirely — it never participates in the
//!    most-specific comparison (FR-012).
//! 3. **Most-specific wins.** Among the remaining matching rules, the one with
//!    the most *literal* (non-wildcard) characters in its pattern is chosen.
//!    Fewer/no wildcards outranks more (FR-012).
//! 4. **Tie-break = later-declared.** When two matching rules are equally
//!    specific, the rule declared *later* in [`BindingConfig::rules`] wins
//!    (FR-012). Resolution is otherwise order-independent.
//! 5. **No match → [`BindingState::NoBinding`].** A first-class, valid state —
//!    never an error (FR-013, FR-015).

use std::path::{Path, PathBuf};

use globset::{Glob, GlobMatcher};
use serde::{Deserialize, Serialize};

/// Current on-disk [`BindingConfig`] format version.
///
/// Persisted so a future format change can be detected; an absent/mismatched
/// version degrades to an empty config rather than crashing (FR-013). Phase 4b
/// (T024) owns the load/migration policy; here it is just the value stamped on a
/// freshly constructed default config.
pub const BINDING_CONFIG_VERSION: u32 = 1;

/// Where a bound type comes from — handed to E004 for (re)acquisition in Phase
/// 4b (FR-008, FR-014).
///
/// This is intentionally a small, serde-friendly, forward-looking locator: it
/// distinguishes a **Rust source path** from a **schema file** so E004 can pick
/// the right acquisition strategy. It stores only a path (data, never executed —
/// FR-024); no model is held here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypeSourceLocator {
    /// A Rust source file/crate path from which E004 extracts a `TypeModel`.
    RustSource(PathBuf),
    /// A schema file (E004's serialized JSON-Schema-2020-12 + `x-ron-*`
    /// interchange) read directly as data.
    SchemaFile(PathBuf),
}

impl TypeSourceLocator {
    /// The underlying path, regardless of source kind. Useful for the UI and for
    /// the Phase 4b acquisition handoff.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            TypeSourceLocator::RustSource(p) | TypeSourceLocator::SchemaFile(p) => p.as_path(),
        }
    }
}

/// One rule in a [`BindingConfig`]: a glob `pattern` (with optional `exclude`
/// globs) mapping matching documents to a `type_name` + `type_source`
/// (FR-008, FR-010 — no inline annotations).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingRule {
    /// Glob pattern matched against the document path (FR-008).
    pub pattern: String,
    /// Optional subtractive exclusion globs. A document matching `pattern` but
    /// also matching *any* of these is removed from this rule's candidacy
    /// entirely (FR-008, FR-012). `None` or empty ⇒ no exclusions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
    /// The named type a matching document should conform to; resolves against
    /// E004's `TypeModel.named_types` in Phase 4b (FR-008).
    pub type_name: String,
    /// Where the type comes from — handed to E004 for (re)acquisition (FR-008,
    /// FR-014).
    pub type_source: TypeSourceLocator,
}

impl BindingRule {
    /// Number of **literal** (non-wildcard) characters in `pattern`.
    ///
    /// This is the specificity score per FR-012: a pattern with more literal
    /// characters (fewer/no wildcards) is more specific. Glob metacharacters
    /// (`* ? [ ] { }`) are not counted; an escaped metacharacter (`\*`) counts
    /// as one literal character (the escaped char), and the backslash itself is
    /// not counted. Computed from the pattern STRING, independent of the matcher.
    #[must_use]
    pub fn specificity(&self) -> usize {
        pattern_specificity(&self.pattern)
    }
}

/// The specificity score of a glob `pattern` — the count of its **literal**
/// (non-wildcard) characters (FR-012).
///
/// This is the shared specificity primitive E006 ([`BindingRule::specificity`])
/// and E009's `RegistryBindingRule` both rank patterns by ("most-specific wins").
/// Exposed (rather than re-implemented in `bevy/mode.rs`) so the two configs use
/// **identical** specificity scoring — see [`crate::bevy::mode`] (HINT-004).
#[must_use]
pub fn pattern_specificity(pattern: &str) -> usize {
    literal_char_count(pattern)
}

/// Counts literal (non-wildcard) characters in a glob pattern string (FR-012).
///
/// Wildcard / glob metacharacters (`*`, `?`, `[`, `]`, `{`, `}`) are excluded.
/// A backslash escape (`\x`) contributes the single escaped character `x` as a
/// literal and the backslash is not counted, so `\*` scores 1 (the literal `*`).
fn literal_char_count(pattern: &str) -> usize {
    let mut count = 0usize;
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                // Escaped: the next char is a literal (the escaped metachar),
                // the backslash itself is not counted.
                if chars.next().is_some() {
                    count += 1;
                }
            }
            '*' | '?' | '[' | ']' | '{' | '}' => {
                // Wildcard / class / alternation metacharacter — not literal.
            }
            _ => count += 1,
        }
    }
    count
}

/// The project-level, persisted mapping from document patterns to types +
/// sources (FR-008, FR-010, FR-013).
///
/// This is the single artifact that survives between sessions; everything
/// downstream ([`TypeBinding`], the validator run, diagnostics) is transient.
/// [`BindingConfig::default`] is an **empty** config (zero rules) so a missing
/// file resolves to [`BindingState::NoBinding`] (FR-013).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingConfig {
    /// The ordered rules. Order is consulted **only** to break exact
    /// specificity ties (later-declared wins); specificity is compared first and
    /// is order-independent (FR-012).
    #[serde(default)]
    pub rules: Vec<BindingRule>,
    /// Format version stamp; lets a future change be detected and degraded
    /// safely (FR-013).
    #[serde(default = "default_version")]
    pub version: u32,
}

fn default_version() -> u32 {
    BINDING_CONFIG_VERSION
}

impl Default for BindingConfig {
    fn default() -> Self {
        BindingConfig {
            rules: Vec::new(),
            version: BINDING_CONFIG_VERSION,
        }
    }
}

/// The project-local directory RONin stores per-project state in.
///
/// Project-scoped (not the OS config dir): the binding config travels with the
/// project tree, so a checkout / clone carries its own type mappings (FR-013).
const PROJECT_STATE_DIR: &str = ".ronin";

/// The project-local binding-config file name inside [`PROJECT_STATE_DIR`].
const BINDING_CONFIG_FILE: &str = "bindings.json";

impl BindingConfig {
    /// The project-scoped on-disk path for the binding config:
    /// `<project_root>/.ronin/bindings.json`.
    ///
    /// **Project-local, NOT the OS config directory.** The binding map is a
    /// property of *the project* (which globs map to which types/sources), so it
    /// is stored alongside the project tree rather than in the per-user OS config
    /// dir — a clone/checkout of the project carries its own mappings, and two
    /// projects never share one binding config (FR-013, data-model "exactly one
    /// persisted artifact, project-scoped, local").
    #[must_use]
    pub fn project_config_path(project_root: &Path) -> PathBuf {
        project_root
            .join(PROJECT_STATE_DIR)
            .join(BINDING_CONFIG_FILE)
    }

    /// Load a binding config from `path`, falling back to
    /// [`BindingConfig::default`] (an **empty** config) when the file is absent,
    /// unreadable, corrupt, or carries an unknown format version.
    ///
    /// Never panics and never errors — a missing or bad config degrades to
    /// no-binding / structural-only rather than locking the user out (FR-013,
    /// project-instructions §I). Mirrors [`crate::settings::AppSettings::load_from`].
    ///
    /// A `version` that does not match [`BINDING_CONFIG_VERSION`] is treated as an
    /// incompatible/unknown format and degrades to the empty default, so a config
    /// written by a future RONin can never be misinterpreted (FR-013).
    #[must_use]
    pub fn load_from(path: &Path) -> Self {
        let Ok(bytes) = std::fs::read(path) else {
            return Self::default();
        };
        let config = match serde_json::from_slice::<BindingConfig>(&bytes) {
            Ok(config) => config,
            // Corrupt / unparseable bytes ⇒ empty config (no panic, no error).
            Err(_) => return Self::default(),
        };
        if config.version != BINDING_CONFIG_VERSION {
            // Unknown / incompatible format version ⇒ degrade to empty rather than
            // risk misinterpreting a future schema (FR-013).
            return Self::default();
        }
        config
    }

    /// Persist this binding config to `path` as pretty-printed JSON, creating
    /// parent directories (e.g. the project-local `.ronin/`) if needed.
    ///
    /// Mirrors [`crate::settings::AppSettings::save_to`]: human-inspectable
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
}

/// An explicit, per-document **session** override (FR-009).
///
/// When set on the active document it binds that document to a chosen
/// `type_name` + `type_source` for the session and **always** takes precedence
/// over any [`BindingConfig`] match (origin = [`BindingOrigin::Override`]). It is
/// never persisted (only [`BindingConfig`] persists). Where it is stored on the
/// document is Phase 4b/T037; this type + the resolution functions are provided
/// here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentOverride {
    /// The named type the document is forced to conform to (FR-009).
    pub type_name: String,
    /// The source E004 (re)acquires from (FR-009, FR-014).
    pub type_source: TypeSourceLocator,
}

/// Which resolution path produced a `Bound` [`TypeBinding`] (FR-009, FR-011).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BindingOrigin {
    /// Produced by a per-document [`DocumentOverride`] (wins over config).
    Override,
    /// Produced by [`BindingConfig`] resolution.
    Config,
}

/// The state of a resolved [`TypeBinding`] (FR-012, FR-015).
///
/// [`BindingState::NoBinding`] is a first-class, valid state — never an error.
/// An unresolved document validates structural-only (FR-015).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BindingState {
    /// The document is bound to a concrete type from a concrete source.
    Bound {
        /// The bound named type (FR-001, FR-008/009).
        type_name: String,
        /// The source E004 (re)acquires from (FR-008/009, FR-014).
        type_source: TypeSourceLocator,
        /// Which path produced the binding (override vs config) (FR-009, FR-011).
        origin: BindingOrigin,
    },
    /// No rule matched and no override applied — structural-only (FR-015).
    NoBinding,
}

/// The resolved answer to "which type, from which source, does this document
/// conform to?" — or the explicit [`BindingState::NoBinding`] (FR-009, FR-011,
/// FR-012).
///
/// Transient: re-resolved on document/config/override change, never persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeBinding {
    /// The resolved state (bound or not).
    pub state: BindingState,
}

impl TypeBinding {
    /// Construct the explicit no-binding state (FR-015).
    #[must_use]
    pub fn none() -> Self {
        TypeBinding {
            state: BindingState::NoBinding,
        }
    }

    /// Construct a bound binding from its parts.
    #[must_use]
    pub fn bound(type_name: String, type_source: TypeSourceLocator, origin: BindingOrigin) -> Self {
        TypeBinding {
            state: BindingState::Bound {
                type_name,
                type_source,
                origin,
            },
        }
    }

    /// `true` iff this binding is [`BindingState::Bound`].
    #[must_use]
    pub fn is_bound(&self) -> bool {
        matches!(self.state, BindingState::Bound { .. })
    }

    /// The bound type name, or `None` when [`BindingState::NoBinding`].
    #[must_use]
    pub fn type_name(&self) -> Option<&str> {
        match &self.state {
            BindingState::Bound { type_name, .. } => Some(type_name.as_str()),
            BindingState::NoBinding => None,
        }
    }

    /// The bound type source, or `None` when [`BindingState::NoBinding`].
    #[must_use]
    pub fn type_source(&self) -> Option<&TypeSourceLocator> {
        match &self.state {
            BindingState::Bound { type_source, .. } => Some(type_source),
            BindingState::NoBinding => None,
        }
    }

    /// The origin (override vs config), or `None` when
    /// [`BindingState::NoBinding`] (FR-009, FR-011).
    #[must_use]
    pub fn origin(&self) -> Option<BindingOrigin> {
        match &self.state {
            BindingState::Bound { origin, .. } => Some(*origin),
            BindingState::NoBinding => None,
        }
    }
}

// ===========================================================================
// E010 US2 — JSON→RON schema-aware reconstruction consultation (T021, FR-009/015).
// ===========================================================================

/// A bound `TypeModel` + its root type name, owned so the JSON→RON converter can
/// borrow a [`JsonToRonBinding`](crate::interop::JsonToRonBinding) view from it
/// (E010 US2 — T021, FR-009/015).
///
/// The E010 JSON→RON reconstruction is **schema-aware when a type is bound** — it
/// consults the bound `TypeModel` strictly as **data** (ADR-0004; never executed,
/// never mutated) to recover tuples-vs-lists, named enum variants (via the recorded
/// serde [`Discriminator`](ronin_types::model::Discriminator)), `char`, `Option`, and
/// non-string map keys (via the [`RonTypeExtension`](ronin_types::extension::RonTypeExtension)
/// tuple-arity / char / non-string-key / option facts). This struct is the seam
/// that turns a document's resolved binding into that consultable model: the worker
/// carries the bound type as a **serialized** E004 interchange
/// (`reparse::BoundType.model`), and [`from_serialized_model`](Self::from_serialized_model)
/// deserializes it once into an in-memory [`TypeModel`] paired with the root type
/// name — exactly the data `json_to_ron` needs (HINT-005). When no type is bound the
/// converter receives `None` and applies the documented best-effort mapping (FR-009).
#[derive(Debug, Clone)]
pub struct JsonToRonConsultation {
    /// The in-memory `TypeModel` consulted as data for reconstruction (FR-009).
    pub model: ronin_types::model::TypeModel,
    /// The bound root type name — a key into
    /// [`TypeModel::named_types`](ronin_types::model::TypeModel::named_types).
    pub root_type: String,
}

impl JsonToRonConsultation {
    /// Build a consultation from an owned `TypeModel` + root type name.
    #[must_use]
    pub fn new(model: ronin_types::model::TypeModel, root_type: impl Into<String>) -> Self {
        Self {
            model,
            root_type: root_type.into(),
        }
    }

    /// Build a consultation from a document's **serialized** E004 `TypeModel`
    /// interchange (`reparse::BoundType.model`) + the bound root type name (T021).
    ///
    /// Deserializes the JSON-Schema-2020-12 + `x-ron-*` interchange back into an
    /// in-memory [`TypeModel`] via [`ronin_types::from_json`]; returns `None` when the
    /// interchange is malformed or the root type is absent — the caller then falls
    /// back to the unbound best-effort path (FR-009, schema-optional / no false
    /// certainty, §III). The model is consulted strictly as **data** (ADR-0004).
    #[must_use]
    pub fn from_serialized_model(serialized: &serde_json::Value, root_type: &str) -> Option<Self> {
        let model = ronin_types::from_json(serialized).ok()?;
        // Only a consultation whose root type is actually registered is useful; an
        // absent root degrades to unbound best-effort rather than a false binding.
        if !model.contains(root_type) {
            return None;
        }
        Some(Self {
            model,
            root_type: root_type.to_string(),
        })
    }

    /// Borrow a [`JsonToRonBinding`](crate::interop::JsonToRonBinding) view over this
    /// consultation for the converter (T021).
    #[must_use]
    pub fn as_binding(&self) -> crate::interop::JsonToRonBinding<'_> {
        crate::interop::JsonToRonBinding::new(&self.model, &self.root_type)
    }
}

/// Upper bound on the length (bytes) of a glob `pattern` we will hand to the
/// compiler (FR-025).
///
/// `globset` matching is linear in the pattern, so an over-long pattern cannot
/// itself cause catastrophic backtracking; this cap is a defensive guard against
/// an *adversarial* config carrying a multi-megabyte "pattern" purely to waste
/// compile/match work. An over-cap pattern is treated as a malformed glob →
/// no-match (the rule degrades to no-candidacy / `NoBinding`), exactly like an
/// invalid pattern. 64 KiB is far larger than any legitimate path glob.
const MAX_GLOB_PATTERN_LEN: usize = 64 * 1024;

/// Compile a glob pattern into a matcher, returning `None` on a malformed
/// pattern.
///
/// A pathological/invalid glob degrades that rule to no-candidacy rather than
/// crashing — the building block for FR-025's adversarial-config hardening. The
/// matcher is configured for path semantics (`literal_separator(true)`) so a
/// `*` segment does not cross directory boundaries, matching typical
/// `.gitignore`-style expectations for path globs.
///
/// An over-[`MAX_GLOB_PATTERN_LEN`] pattern is rejected up front (`None`) so an
/// adversarial config cannot waste resources compiling a multi-megabyte
/// "pattern"; such a rule degrades to no-candidacy (FR-025).
fn compile_glob(pattern: &str) -> Option<GlobMatcher> {
    if pattern.len() > MAX_GLOB_PATTERN_LEN {
        // Pathological / huge pattern: treat as malformed → no match (FR-025).
        return None;
    }
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .ok()
        .map(|g: Glob| g.compile_matcher())
}

/// Does `pattern` match `doc_path`? A malformed pattern never matches.
///
/// Matching is attempted against the path as given; on Windows the matcher also
/// normalizes separators, so forward-slash patterns match backslash paths.
///
/// Shared with E009's `RegistryBindingConfig` resolution (rather than
/// re-implemented) so both configs glob-match identically — see
/// [`crate::bevy::mode`] (HINT-004).
#[must_use]
pub fn glob_matches(pattern: &str, doc_path: &Path) -> bool {
    compile_glob(pattern).is_some_and(|m| m.is_match(doc_path))
}

/// Resolve a candidate `type_source` path against `project_root` and confirm it
/// stays **within** the project tree, returning the contained absolute path or
/// `None` when it escapes the root (FR-025).
///
/// This is the project-root containment check that hardens binding-config trust
/// against an *adversarial* `type_source`: a `..` traversal, or an absolute path
/// pointing outside the project, must NOT be read. A rejected path degrades the
/// rule to no-binding (structural-only) rather than widening RONin's read surface
/// beyond the project the user opened.
///
/// # Algorithm
///
/// 1. Make the candidate absolute relative to `project_root` (a relative
///    `type_source` is interpreted *inside* the project, where the config lives).
/// 2. Lexically normalize *both* the root and the candidate — collapsing `.` and
///    resolving `..` against accumulated components **without touching the
///    filesystem** — so a path that escapes via `..` is rejected even when the
///    target does not exist (the file is precisely what we must not read).
/// 3. When both the root and the candidate also exist on disk, additionally
///    require the canonicalized candidate to start with the canonicalized root,
///    so a symlink that points outside the tree is rejected too.
/// 4. Require the normalized candidate to start with the normalized root.
///
/// Returns the normalized, contained path on success; `None` when the candidate
/// escapes the root (caller degrades that rule to `NoBinding`).
#[must_use]
pub fn contain_type_source(project_root: &Path, candidate: &Path) -> Option<PathBuf> {
    // (1) Interpret a relative type_source inside the project root.
    let joined: PathBuf = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        project_root.join(candidate)
    };

    // (2) Lexical normalization (no filesystem access): reject any `..` that would
    //     climb above the root. This catches an escaping path even when the file
    //     does not exist — which is the whole point, since we must not read it.
    let norm_root = lexically_normalize(project_root)?;
    let norm_candidate = lexically_normalize(&joined)?;
    if !norm_candidate.starts_with(&norm_root) {
        return None;
    }

    // (3) Belt-and-suspenders for symlinks: when both ends exist on disk, require
    //     the *canonical* candidate to stay under the canonical root. A symlink
    //     pointing outside the tree is thereby rejected. If either side cannot be
    //     canonicalized (e.g. the file does not exist yet), the lexical check in
    //     (2) already governed containment, so we accept based on that.
    if let (Ok(real_root), Ok(real_candidate)) =
        (project_root.canonicalize(), joined.canonicalize())
    {
        if !real_candidate.starts_with(&real_root) {
            return None;
        }
    }

    Some(norm_candidate)
}

/// Lexically normalize `path` — collapse `.`, resolve `..` against accumulated
/// components, and drop redundant separators — **without** touching the
/// filesystem (FR-025).
///
/// Returns `None` if a `..` would climb above the path's root/prefix (a clear
/// escape attempt that cannot be normalized into a contained path). Filesystem
/// state is never consulted, so a not-yet-existing `type_source` is still
/// checkable — exactly the case we must guard, since the file would otherwise be
/// read.
fn lexically_normalize(path: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut out: Vec<Component> = Vec::new();
    for comp in path.components() {
        match comp {
            // Root, drive prefix (Windows), and plain names accumulate.
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => out.push(comp),
            // `.` is a no-op.
            Component::CurDir => {}
            // `..` pops the last *normal* component; popping past a root/prefix is
            // an escape → reject.
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // A `..` at (or above) the root cannot be contained.
                _ => return None,
            },
        }
    }
    let mut normalized = PathBuf::new();
    for comp in out {
        normalized.push(comp.as_os_str());
    }
    Some(normalized)
}

/// Resolve a document to its [`TypeBinding`] from a [`BindingConfig`] alone
/// (no override) (FR-008, FR-012).
///
/// This is the config-only entry point. To layer a per-document override on top,
/// use [`resolve`], which delegates here when no override is present.
#[must_use]
pub fn resolve_binding(config: &BindingConfig, doc_path: &Path) -> TypeBinding {
    resolve(config, Some(doc_path), None)
}

/// Resolve a document to its [`TypeBinding`], honouring an optional per-document
/// override (FR-009, FR-012).
///
/// Precedence:
/// 1. `override_` present ⇒ always `Bound` with [`BindingOrigin::Override`]
///    (FR-009) — config is not consulted.
/// 2. else config resolution: exclusions first, then most-specific, then
///    later-declared tie-break (FR-012).
/// 3. else [`BindingState::NoBinding`] (FR-013, FR-015).
///
/// `doc_path` is `Option` so a not-yet-saved buffer (no path) can still take an
/// override but resolves to [`BindingState::NoBinding`] against config (nothing
/// to match a glob against).
#[must_use]
pub fn resolve(
    config: &BindingConfig,
    doc_path: Option<&Path>,
    override_: Option<&DocumentOverride>,
) -> TypeBinding {
    // (1) Override wins absolutely (FR-009).
    if let Some(ov) = override_ {
        return TypeBinding::bound(
            ov.type_name.clone(),
            ov.type_source.clone(),
            BindingOrigin::Override,
        );
    }

    // No path ⇒ nothing for a glob to match ⇒ NoBinding.
    let Some(path) = doc_path else {
        return TypeBinding::none();
    };

    // (2) Config resolution. Walk rules, keeping the best candidate.
    //
    // A candidate beats the incumbent when it is strictly more specific, OR it
    // is equally specific and declared later. Iterating in declaration order and
    // using `>=` for the equal case yields "later-declared wins" on ties while
    // keeping the comparison itself order-independent: any permutation of equally
    // ranked rules selects the same *winner identity* only via the documented
    // tie-break (declaration order), and a strictly-more-specific rule always
    // wins regardless of position. (FR-012)
    let mut best: Option<(usize, &BindingRule)> = None;
    for rule in &config.rules {
        // (a) Exclusions are absolute and applied first: if the include matches
        //     but any exclude also matches, the rule is removed from candidacy
        //     entirely (FR-012).
        if !rule_is_candidate(rule, path) {
            continue;
        }
        let spec = rule.specificity();
        match best {
            // Strictly more specific OR equal specificity (later-declared, since
            // we iterate in order) ⇒ this rule becomes the new best.
            Some((best_spec, _)) if spec >= best_spec => best = Some((spec, rule)),
            None => best = Some((spec, rule)),
            _ => {}
        }
    }

    match best {
        Some((_, rule)) => TypeBinding::bound(
            rule.type_name.clone(),
            rule.type_source.clone(),
            BindingOrigin::Config,
        ),
        None => TypeBinding::none(),
    }
}

/// Is `rule` a candidate for `path`? — i.e. its `pattern` matches AND none of
/// its `exclude` globs match (exclusion is absolute) (FR-012).
fn rule_is_candidate(rule: &BindingRule, path: &Path) -> bool {
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

#[cfg(test)]
mod consultation_tests {
    //! T021 — the JSON→RON schema-aware reconstruction consultation (FR-009/015).

    use super::*;
    use ronin_types::model::{TypeModel, TypeNode, TypeRef};

    fn tuple_model() -> TypeModel {
        let mut model = TypeModel::new();
        model.insert_named(
            "Pos2",
            TypeNode::tuple(vec![
                TypeRef::inline(TypeNode::primitive(ronin_types::model::Primitive::Integer)),
                TypeRef::inline(TypeNode::primitive(ronin_types::model::Primitive::Integer)),
            ]),
        );
        model
    }

    #[test]
    fn from_serialized_model_round_trips_and_binds() {
        // A serialized E004 interchange deserializes back to a consultable model.
        let model = tuple_model();
        let serialized = ronin_types::to_json(&model);
        let consult = JsonToRonConsultation::from_serialized_model(&serialized, "Pos2")
            .expect("a registered root type binds");
        assert_eq!(consult.root_type, "Pos2");
        // The borrowed binding view reconstructs a JSON array as a tuple by arity.
        let json = serde_json::json!([1, 2]);
        let r = crate::interop::json_to_ron(&json, Some(consult.as_binding()), None);
        assert!(r.text.contains("(1, 2)"), "tuple by arity, got: {}", r.text);
    }

    #[test]
    fn from_serialized_model_absent_root_degrades_to_none() {
        let serialized = ronin_types::to_json(&tuple_model());
        // An unregistered root type degrades to unbound best-effort (no false bind).
        assert!(
            JsonToRonConsultation::from_serialized_model(&serialized, "NotThere").is_none(),
            "an absent root type yields no consultation"
        );
    }

    #[test]
    fn from_serialized_model_malformed_interchange_degrades_to_none() {
        // A non-interchange JSON value cannot deserialize → no consultation, no panic.
        let bogus = serde_json::json!({ "not": "a type model" });
        assert!(JsonToRonConsultation::from_serialized_model(&bogus, "X").is_none());
    }

    #[test]
    fn owned_constructor_yields_a_usable_binding() {
        let consult = JsonToRonConsultation::new(tuple_model(), "Pos2");
        let json = serde_json::json!([3, 4]);
        let r = crate::interop::json_to_ron(&json, Some(consult.as_binding()), None);
        assert!(r.text.contains("(3, 4)"), "got: {}", r.text);
    }
}

#[cfg(test)]
mod persistence_tests {
    //! T024 — project-scoped persistence of [`BindingConfig`] (FR-013).
    //!
    //! Mirrors the temp-file pattern in `settings.rs`: round-trip save→load,
    //! absent → empty, corrupt bytes → empty (no panic), version mismatch → empty.

    use super::*;

    /// A fresh temp directory for a persistence test.
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ronin_binding_persist_{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A two-rule config used to prove round-trip fidelity.
    fn sample_config() -> BindingConfig {
        BindingConfig {
            rules: vec![
                BindingRule {
                    pattern: "**/*.ron".to_string(),
                    exclude: Some(vec!["target/**".to_string()]),
                    type_name: "AppConfig".to_string(),
                    type_source: TypeSourceLocator::SchemaFile(PathBuf::from("schemas/app.json")),
                },
                BindingRule {
                    pattern: "scenes/*.scn.ron".to_string(),
                    exclude: None,
                    type_name: "Scene".to_string(),
                    type_source: TypeSourceLocator::RustSource(PathBuf::from("src/scene.rs")),
                },
            ],
            version: BINDING_CONFIG_VERSION,
        }
    }

    #[test]
    fn project_config_path_is_project_local() {
        let root = Path::new("/home/user/myproject");
        let path = BindingConfig::project_config_path(root);
        assert_eq!(path, root.join(".ronin").join("bindings.json"));
    }

    #[test]
    fn round_trips_through_save_and_load() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("bindings.json");
        let config = sample_config();
        config.save_to(&path).unwrap();
        let loaded = BindingConfig::load_from(&path);
        assert_eq!(loaded, config);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_to_creates_parent_dirs() {
        // The project-local `.ronin/` dir may not exist yet; save_to creates it.
        let dir = temp_dir("mkdir");
        let path = BindingConfig::project_config_path(&dir);
        assert!(!path.parent().unwrap().exists() || path.parent().unwrap().exists());
        sample_config().save_to(&path).unwrap();
        assert!(
            path.exists(),
            "config written under the auto-created .ronin/"
        );
        let loaded = BindingConfig::load_from(&path);
        assert_eq!(loaded, sample_config());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn absent_file_loads_empty_default() {
        let dir = temp_dir("absent");
        let path = dir.join("never-written.json");
        let loaded = BindingConfig::load_from(&path);
        assert_eq!(loaded, BindingConfig::default());
        assert!(loaded.rules.is_empty());
    }

    #[test]
    fn corrupt_bytes_load_empty_default_no_panic() {
        let dir = temp_dir("corrupt");
        let path = dir.join("corrupt.json");
        std::fs::write(&path, b"\x00\x01 not json at all }{][").unwrap();
        let loaded = BindingConfig::load_from(&path);
        assert_eq!(loaded, BindingConfig::default());
        assert!(loaded.rules.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn version_mismatch_loads_empty_default() {
        // A config from a future RONin (unknown version) degrades to empty rather
        // than being misinterpreted (FR-013).
        let dir = temp_dir("version");
        let path = dir.join("future.json");
        let future = BINDING_CONFIG_VERSION + 1;
        let json = format!(
            r#"{{ "rules": [ {{ "pattern": "**/*.ron", "type_name": "X", "type_source": {{ "SchemaFile": "x.json" }} }} ], "version": {future} }}"#
        );
        std::fs::write(&path, json.as_bytes()).unwrap();
        let loaded = BindingConfig::load_from(&path);
        assert_eq!(loaded, BindingConfig::default());
        assert!(
            loaded.rules.is_empty(),
            "future-version rules are discarded"
        );
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod containment_tests {
    //! T038 — project-root containment + pathological-glob cap (FR-025).
    //!
    //! Pure, filesystem-light unit coverage of the adversarial-config guards:
    //! [`contain_type_source`] rejects path-traversal / out-of-project sources and
    //! [`compile_glob`] rejects an over-cap pathological pattern. The end-to-end
    //! degrade-to-`NoBinding` behavior is proven via the App in
    //! `tests/config_trust.rs` (T039).

    use super::*;

    #[test]
    fn relative_source_inside_root_is_contained() {
        let root = Path::new("/proj");
        let contained = contain_type_source(root, Path::new("schemas/app.json"))
            .expect("a relative path inside the root is contained");
        assert!(
            contained.starts_with(lexically_normalize(root).unwrap()),
            "the contained path stays under the project root"
        );
    }

    #[test]
    fn nested_relative_source_is_contained() {
        let root = Path::new("/proj");
        assert!(
            contain_type_source(root, Path::new("a/b/c/types.rs")).is_some(),
            "a deeply nested in-project source is contained"
        );
    }

    #[test]
    fn parent_traversal_escapes_root_is_rejected() {
        let root = Path::new("/proj");
        // `../outside.json` climbs above the project root → reject (never read).
        assert!(
            contain_type_source(root, Path::new("../outside.json")).is_none(),
            "a `..` traversal above the root must be rejected"
        );
    }

    #[test]
    fn deep_parent_traversal_is_rejected() {
        let root = Path::new("/proj/sub");
        // `../../etc/passwd` climbs two levels, above the root → reject.
        assert!(
            contain_type_source(root, Path::new("../../etc/passwd")).is_none(),
            "a multi-level `..` escape must be rejected"
        );
    }

    #[test]
    fn interior_parent_traversal_that_stays_in_root_is_contained() {
        let root = Path::new("/proj");
        // `sub/../schemas/app.json` normalizes to `schemas/app.json` — still inside.
        assert!(
            contain_type_source(root, Path::new("sub/../schemas/app.json")).is_some(),
            "a `..` that nets out inside the root is contained"
        );
    }

    #[test]
    fn absolute_source_outside_root_is_rejected() {
        let root = Path::new("/proj");
        // An absolute path outside the project tree → reject.
        let outside = if cfg!(windows) {
            Path::new("C:\\Windows\\System32\\drivers\\etc\\hosts")
        } else {
            Path::new("/etc/passwd")
        };
        assert!(
            contain_type_source(root, outside).is_none(),
            "an absolute out-of-project source must be rejected"
        );
    }

    #[test]
    fn pathological_glob_over_cap_never_matches() {
        // A multi-megabyte "pattern" must compile to nothing (no-match), not hang.
        let huge = "a".repeat(MAX_GLOB_PATTERN_LEN + 1);
        assert!(
            compile_glob(&huge).is_none(),
            "an over-cap pattern degrades to no-match (FR-025)"
        );
        // And resolution against it produces NoBinding, no panic/hang.
        let config = BindingConfig {
            rules: vec![BindingRule {
                pattern: huge,
                exclude: None,
                type_name: "X".to_string(),
                type_source: TypeSourceLocator::SchemaFile(PathBuf::from("x.json")),
            }],
            version: BINDING_CONFIG_VERSION,
        };
        let binding = resolve_binding(&config, Path::new("/proj/doc.ron"));
        assert!(
            !binding.is_bound(),
            "an over-cap glob rule never matches → NoBinding"
        );
    }

    #[test]
    fn pathological_glob_in_exclude_is_ignored_safely() {
        // A pathological exclude pattern must not crash; it simply never matches, so
        // it cannot exclude anything (the include still governs).
        let huge = "*".repeat(MAX_GLOB_PATTERN_LEN + 1);
        let config = BindingConfig {
            rules: vec![BindingRule {
                pattern: "**/*.ron".to_string(),
                exclude: Some(vec![huge]),
                type_name: "X".to_string(),
                type_source: TypeSourceLocator::SchemaFile(PathBuf::from("x.json")),
            }],
            version: BINDING_CONFIG_VERSION,
        };
        // Resolution must not panic/hang; the include matches and the over-cap
        // exclude is a no-match, so the rule stays a candidate.
        let binding = resolve_binding(&config, Path::new("/proj/doc.ron"));
        assert!(
            binding.is_bound(),
            "the include matches; over-cap exclude is inert"
        );
    }
}
