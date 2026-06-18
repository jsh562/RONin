//! Snippets: explicit-trigger templates that expand into RON constructs (E005
//! Wave 4, US3, FR-015/FR-016/FR-017/FR-018/FR-025).
//!
//! Snippets are **entirely** a `ronin-app` (native) concern: the built-in set is
//! compiled in here, the optional user file is read from the OS config dir via
//! `serde_json`, and expansion / tab-stop navigation live here too. `ronin-core`
//! owns no snippet logic (project-instructions §II); this module only *consumes*
//! `ronin_core::parse` to verify that an expanded body round-trips before it is ever
//! committed to a buffer (§I, never corrupt user data).
//!
//! # Pieces
//!
//! * [`Snippet`] — one named template: `name`, `prefix` (the trigger text),
//!   `body` (a VS Code-style template string), `description`, and an optional
//!   [`SnippetScope`] tag.
//! * [`BUILT_INS`] (via [`built_ins`]) — the compiled-in default set covering the
//!   common RON constructs (named / tuple / unit struct, enum unit / tuple /
//!   struct variants, map, list, tuple, `Some(...)`) plus a couple of generic
//!   Bevy scene/component patterns.
//! * [`UserSnippetFile`] — a load of the optional local JSON file (VS Code snippet
//!   syntax) with a [`SnippetParseStatus`] (`Ok` / `Missing` / `Malformed`).
//! * [`SnippetSet`] — the effective set: built-ins overlaid by user-defined
//!   snippets (override-by-name, user wins), plus an optional degrade notice.
//! * [`expand_snippet`] — parse a body's `$1` / `${1:default}` / `${1|a,b|}` /
//!   `$0` markers into literal text + ordered [`TabStop`]s for the editor's
//!   tab-stop navigation.
//!
//! # Never corrupt (project-instructions §I, FR-018)
//!
//! A snippet is *useful* only if every expansion (even with the default
//! placeholder values left untouched) produces parseable, round-trippable RON.
//! [`Snippet::default_expansion_round_trips`] and [`SnippetSet`]'s merge use
//! [`body_round_trips`] (which calls `ronin_core::parse`) to keep a snippet whose
//! default expansion fails to parse out of the effective set — a malformed entry
//! is dropped at load, never allowed to insert corrupt text.
//!
//! # Explicit trigger only (FR-015, §III/§VI)
//!
//! Nothing here auto-expands: snippets surface through an explicit trigger / menu
//! ([`SnippetSet::menu_entries`]) and only insert when the user picks one. The
//! user file is local (read from the OS config dir, never the network) and a
//! missing / malformed file degrades gracefully to built-ins plus a notice
//! (FR-017).
//!
//! # Deferred seams
//!
//! Snippets are structural, explicit-trigger templates; the deeper intelligence is
//! deferred to later epics:
//!
//! * **type-aware snippet filtering** — offering only the snippets legal for an
//!   expected type → **E006** (schema-optional type model);
//! * **semantic / CST-backed undo-redo** of a snippet insertion → **E007** (the
//!   verified splice in [`insert_snippet`] is the seam an undo stack records);
//! * **tree / table structured editing** that would emit snippet-shaped subtrees →
//!   **E008**;
//! * **deep Bevy-registry integration** — real component templates resolved from a
//!   loaded registry → **E009** (the [`SnippetScope::Bevy`] built-ins here are
//!   generic scene/component *shapes*, not registry-resolved);
//! * **RON⇄JSON / `derive`-driven** snippet generation → **E010** (interop is
//!   outside this module; snippets reuse only `serde_json` to *read* the user file).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

/// `ProjectDirs` qualifier/organization/application triple identifying RONin's
/// OS-specific config directory — kept in step with [`crate::settings`] so the
/// snippet file sits beside `settings.json`.
const QUALIFIER: &str = "dev";
const ORGANIZATION: &str = "ronin";
const APPLICATION: &str = "RONin";

/// The file name of the user snippet file in the OS config directory.
const USER_SNIPPET_FILE: &str = "snippets.json";

/// Which contexts a snippet is offered in (FR-015).
///
/// Built-ins ship both [`SnippetScope::Generic`] RON constructs and a few generic
/// [`SnippetScope::Bevy`] scene/component shapes. The scope is a coarse tag for the
/// trigger UI / future filtering; it carries no registry knowledge (that is E009).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SnippetScope {
    /// A general-purpose RON construct (struct, list, map, …).
    #[default]
    Generic,
    /// A generic Bevy scene / component pattern (shape only, no registry).
    Bevy,
}

impl SnippetScope {
    /// Parse a scope tag from the user file's optional `"scope"` string.
    ///
    /// Unknown / absent values resolve to [`SnippetScope::Generic`] so a typo in a
    /// user snippet never drops the entry — it just lands in the generic bucket.
    #[must_use]
    fn from_tag(tag: Option<&str>) -> Self {
        match tag.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("bevy") => SnippetScope::Bevy,
            _ => SnippetScope::Generic,
        }
    }
}

/// One named, reusable snippet template (FR-015).
///
/// The `body` is a VS Code-style template: literal text interleaved with tab-stop
/// markers (`$1`, `$2`, …), placeholders (`${1:default}`), choice placeholders
/// (`${1|a,b|}`), and a final cursor (`$0`). [`expand_snippet`] turns it into
/// literal text plus the ordered tab-stops the editor navigates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    /// The stable identity key used for override-by-name merge (FR-017).
    pub name: String,
    /// The trigger text the user types / chooses to invoke the snippet.
    pub prefix: String,
    /// The VS Code-style template body.
    pub body: String,
    /// A human-readable label shown at the trigger UI (FR-025).
    pub description: String,
    /// Which contexts the snippet is offered in (FR-015).
    pub scope: SnippetScope,
}

impl Snippet {
    /// Construct a snippet from its parts (interns the string fields).
    fn new(name: &str, prefix: &str, body: &str, description: &str, scope: SnippetScope) -> Self {
        Self {
            name: name.to_string(),
            prefix: prefix.to_string(),
            body: body.to_string(),
            description: description.to_string(),
            scope,
        }
    }

    /// `true` when this snippet's body, expanded with all default placeholder
    /// values left in place, parses to a clean CST (FR-018).
    ///
    /// This is the round-trip gate: a snippet whose *default* expansion would not
    /// parse is never admitted to the effective set, so triggering it can never
    /// insert corrupt text (project-instructions §I).
    #[must_use]
    pub fn default_expansion_round_trips(&self) -> bool {
        let expansion = expand_snippet(&self.body);
        body_round_trips(&expansion.text)
    }
}

/// The compiled-in built-in snippet set (FR-015).
///
/// Covers the common RON constructs plus a couple of generic Bevy patterns. Built
/// fresh on call (cheap; a handful of entries) rather than held in a `static` so the
/// type stays simple and `Snippet` need not be `const`-constructible.
///
/// Naming: `name` is the stable override key, `prefix` is the trigger text. The
/// bodies are deliberately conservative — each one parses cleanly with its default
/// placeholder values (verified by `tests::every_built_in_default_expansion_round_trips`).
#[must_use]
pub fn built_ins() -> Vec<Snippet> {
    use SnippetScope::{Bevy, Generic};
    vec![
        // ---- structs -------------------------------------------------------
        Snippet::new(
            "named-struct",
            "struct",
            "${1:Name}(${2:field}: ${3:value})",
            "Named struct  Name(field: value)",
            Generic,
        ),
        Snippet::new(
            "tuple-struct",
            "tuplestruct",
            "${1:Name}(${2:value})",
            "Tuple struct  Name(value)",
            Generic,
        ),
        Snippet::new(
            "unit-struct",
            "unitstruct",
            "${1:Name}",
            "Unit struct  Name",
            Generic,
        ),
        // ---- enum variants -------------------------------------------------
        Snippet::new(
            "enum-unit-variant",
            "variant",
            "${1:Variant}",
            "Enum unit variant  Variant",
            Generic,
        ),
        Snippet::new(
            "enum-tuple-variant",
            "tuplevariant",
            "${1:Variant}(${2:value})",
            "Enum tuple variant  Variant(value)",
            Generic,
        ),
        Snippet::new(
            "enum-struct-variant",
            "structvariant",
            "${1:Variant}(${2:field}: ${3:value})",
            "Enum struct variant  Variant(field: value)",
            Generic,
        ),
        // ---- collections ---------------------------------------------------
        Snippet::new(
            "map",
            "map",
            "{${1:\"key\"}: ${2:value}}",
            "Map  {\"key\": value}",
            Generic,
        ),
        Snippet::new("list", "list", "[${1:value}]", "List  [value]", Generic),
        Snippet::new(
            "tuple",
            "tuple",
            "(${1:a}, ${2:b})",
            "Tuple  (a, b)",
            Generic,
        ),
        // ---- option --------------------------------------------------------
        Snippet::new(
            "some",
            "some",
            "Some(${1:value})",
            "Option Some  Some(value)",
            Generic,
        ),
        // ---- generic Bevy patterns (shapes only; registry is E009) ---------
        Snippet::new(
            "bevy-transform",
            "transform",
            "Transform(translation: (${1:0.0}, ${2:0.0}, ${3:0.0}))",
            "Bevy Transform component",
            Bevy,
        ),
        Snippet::new(
            "bevy-scene-entity",
            "entity",
            "(components: {${1:\"Name\"}: (${2:name}: ${3:\"value\"})})",
            "Bevy scene entity (components map)",
            Bevy,
        ),
    ]
}

/// The number of compiled-in built-in snippets (stable for tests/hosts).
///
/// Exposed as a constant-like accessor (`BUILT_INS` in the task spec) so callers and
/// tests can assert presence without re-deriving the list. Use [`built_ins`] for the
/// snippets themselves.
#[must_use]
pub fn built_in_count() -> usize {
    built_ins().len()
}

/// Aliased export name from the task contract: the built-in snippet set.
///
/// A function rather than a `static` (each [`Snippet`] owns heap `String`s, so it is
/// not `const`-constructible); call it to get the list.
#[allow(non_snake_case)]
#[must_use]
pub fn BUILT_INS() -> Vec<Snippet> {
    built_ins()
}

/// Whether `text` parses to a clean CST with no diagnostics (FR-018).
///
/// The single round-trip gate used everywhere a snippet body / expansion must be
/// proven safe before it can reach a buffer. Delegates to `ronin_core::parse` and
/// checks for **zero** diagnostics: an expansion that parses cleanly round-trips
/// losslessly through the CST (project-instructions §I).
#[must_use]
pub fn body_round_trips(text: &str) -> bool {
    ronin_core::parse(text).diagnostics().is_empty()
}

/// The status of a [`UserSnippetFile`] load (FR-017).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnippetParseStatus {
    /// The file existed and parsed; user snippets were contributed.
    Ok,
    /// No file was present at the location (the common, non-error case).
    Missing,
    /// The file existed but could not be parsed as snippet JSON.
    Malformed,
}

/// The raw JSON shape of one entry in a VS Code-style snippet file.
///
/// VS Code lets `body` be either a single string or an array of lines; both are
/// accepted here (the array is joined with `\n`). `prefix` may be a single string or
/// an array of triggers (the first is used). Unknown fields are ignored.
#[derive(Debug, Clone, Deserialize)]
struct RawSnippet {
    #[serde(default)]
    prefix: Option<StringOrVec>,
    #[serde(default)]
    body: Option<StringOrVec>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

/// A JSON value that may be a single string or an array of strings.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum StringOrVec {
    /// A single string value.
    One(String),
    /// An array of strings (joined with `\n` for bodies; first used for prefixes).
    Many(Vec<String>),
}

impl StringOrVec {
    /// Join an array body with newlines, or return the single string.
    fn joined(&self) -> String {
        match self {
            StringOrVec::One(s) => s.clone(),
            StringOrVec::Many(v) => v.join("\n"),
        }
    }

    /// The first trigger string (an empty array yields an empty string).
    fn first(&self) -> String {
        match self {
            StringOrVec::One(s) => s.clone(),
            StringOrVec::Many(v) => v.first().cloned().unwrap_or_default(),
        }
    }
}

/// A load of the optional local user snippet file (FR-017).
///
/// The file is VS Code-style snippet JSON: a top-level object mapping a snippet
/// *name* to `{ prefix, body, description?, scope? }`. The load never errors and
/// never panics — a missing file is [`SnippetParseStatus::Missing`] (the common
/// case) and an unparseable one is [`SnippetParseStatus::Malformed`]; both degrade
/// to built-ins-only with a notice (project-instructions §I).
#[derive(Debug, Clone)]
pub struct UserSnippetFile {
    /// The location the file was looked for (for the open-file command, FR-025).
    pub location: Option<PathBuf>,
    /// The parsed user snippets (empty unless `parse_status == Ok`).
    pub snippets: Vec<Snippet>,
    /// The load outcome (drives the degrade path, FR-017).
    pub parse_status: SnippetParseStatus,
}

impl UserSnippetFile {
    /// Load the user snippet file from the OS config dir (FR-017).
    ///
    /// Looks beside `settings.json` for [`USER_SNIPPET_FILE`]. A platform with no
    /// config dir is treated as [`SnippetParseStatus::Missing`] (no location, no
    /// snippets) rather than an error.
    #[must_use]
    pub fn load() -> Self {
        match Self::location() {
            Some(path) => Self::load_from(&path),
            None => Self {
                location: None,
                snippets: Vec::new(),
                parse_status: SnippetParseStatus::Missing,
            },
        }
    }

    /// Load the user snippet file from an explicit `path` (FR-017).
    ///
    /// Shared by [`load`](Self::load) and by tests (which inject a temp fixture).
    /// Recovery contract: absent → [`SnippetParseStatus::Missing`]; unreadable or
    /// unparseable → [`SnippetParseStatus::Malformed`]; both yield an empty snippet
    /// list so the merge falls back to built-ins-only.
    #[must_use]
    pub fn load_from(path: &std::path::Path) -> Self {
        let location = Some(path.to_path_buf());
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            // A genuinely absent file is the ordinary "no user snippets" case.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Self {
                    location,
                    snippets: Vec::new(),
                    parse_status: SnippetParseStatus::Missing,
                };
            }
            // Any other read error (permissions, etc.) is a malformed/unusable file.
            Err(_) => {
                return Self {
                    location,
                    snippets: Vec::new(),
                    parse_status: SnippetParseStatus::Malformed,
                };
            }
        };
        let raw: BTreeMap<String, RawSnippet> = match serde_json::from_slice(&bytes) {
            Ok(map) => map,
            Err(_) => {
                return Self {
                    location,
                    snippets: Vec::new(),
                    parse_status: SnippetParseStatus::Malformed,
                };
            }
        };
        // Convert each entry, skipping any that are incomplete (no body) — a bad
        // entry is dropped, never partially applied (FR-017).
        let snippets = raw
            .into_iter()
            .filter_map(|(name, entry)| Self::convert(&name, &entry))
            .collect();
        Self {
            location,
            snippets,
            parse_status: SnippetParseStatus::Ok,
        }
    }

    /// Convert one raw JSON entry into a [`Snippet`], or `None` if it is unusable.
    ///
    /// An entry with no `body` is dropped (there is nothing to expand). The body's
    /// default expansion is *not* round-trip-checked here — [`SnippetSet`]'s merge
    /// applies that gate so the same drop-bad-entries rule covers built-ins and user
    /// snippets uniformly.
    fn convert(name: &str, entry: &RawSnippet) -> Option<Snippet> {
        let body = entry.body.as_ref()?.joined();
        if body.is_empty() {
            return None;
        }
        let prefix = entry
            .prefix
            .as_ref()
            .map_or_else(|| name.to_string(), StringOrVec::first);
        let description = entry
            .description
            .clone()
            .unwrap_or_else(|| name.to_string());
        let scope = SnippetScope::from_tag(entry.scope.as_deref());
        Some(Snippet {
            name: name.to_string(),
            prefix,
            body,
            description,
            scope,
        })
    }

    /// The location the user snippet file is read from in the OS config dir (FR-025).
    ///
    /// `None` when the platform exposes no config directory.
    #[must_use]
    pub fn location() -> Option<PathBuf> {
        directories::ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
            .map(|dirs| dirs.config_dir().join(USER_SNIPPET_FILE))
    }
}

/// A starter user snippet file template written by the open-file command when the
/// file is absent (FR-025), so the user lands in a valid, editable example.
pub const USER_SNIPPET_TEMPLATE: &str = r#"{
  "example-point": {
    "prefix": "point",
    "body": "Point(x: ${1:0}, y: ${2:0})",
    "description": "Example user snippet: a Point struct",
    "scope": "Generic"
  }
}
"#;

/// The effective set of snippets available at the trigger UI (FR-017).
///
/// Built from the compiled-in built-ins overlaid by any user-defined snippets
/// (override-by-name, user wins). Every member of [`effective`](Self::effective) has
/// a default expansion that round-trips (FR-018) — a snippet (built-in or user)
/// whose default expansion fails to parse is dropped at build time. A Missing /
/// Malformed user file leaves the set equal to the (round-trip-filtered) built-ins
/// and records a [`notice`](Self::notice) (FR-017).
#[derive(Debug, Clone)]
pub struct SnippetSet {
    /// The merged, override-resolved, round-trip-verified snippets, keyed by name.
    effective: BTreeMap<String, Snippet>,
    /// An explanatory degrade notice when the user file was Missing/Malformed.
    notice: Option<String>,
    /// The user file's parse status (for tests/hosts and the degrade path).
    user_status: SnippetParseStatus,
}

impl SnippetSet {
    /// Build the effective set from the built-ins and a loaded [`UserSnippetFile`]
    /// (FR-017/FR-018).
    ///
    /// Merge rule: start from the built-ins, then overlay user-defined snippets by
    /// name (user wins). Every candidate (built-in or user) must pass the round-trip
    /// gate ([`Snippet::default_expansion_round_trips`]); one that does not is
    /// dropped so it can never insert corrupt text. A Missing/Malformed user file
    /// records a notice and contributes no snippets, so `effective` reduces to the
    /// built-ins (FR-017).
    #[must_use]
    pub fn build(user: &UserSnippetFile) -> Self {
        let mut effective: BTreeMap<String, Snippet> = BTreeMap::new();

        // Built-ins first. Each is round-trip-checked defensively; the test suite
        // additionally asserts every built-in passes, so this never silently drops
        // one in practice — it is a guard, not the primary contract.
        for snippet in built_ins() {
            if snippet.default_expansion_round_trips() {
                effective.insert(snippet.name.clone(), snippet);
            }
        }

        // Overlay user snippets (override-by-name). Only contribute when the file
        // parsed Ok; a user snippet whose default expansion fails to round-trip is
        // dropped (never a corrupt insertion, FR-018).
        let mut dropped_user = 0usize;
        if matches!(user.parse_status, SnippetParseStatus::Ok) {
            for snippet in &user.snippets {
                if snippet.default_expansion_round_trips() {
                    effective.insert(snippet.name.clone(), snippet.clone());
                } else {
                    dropped_user += 1;
                }
            }
        }

        let notice = match user.parse_status {
            SnippetParseStatus::Ok if dropped_user > 0 => Some(format!(
                "Ignored {dropped_user} user snippet(s) whose default expansion did not parse; \
                 built-ins and the rest are available."
            )),
            SnippetParseStatus::Ok => None,
            SnippetParseStatus::Missing => None,
            SnippetParseStatus::Malformed => Some(
                "Could not read the user snippet file; using built-in snippets only.".to_string(),
            ),
        };

        Self {
            effective,
            notice,
            user_status: user.parse_status,
        }
    }

    /// Build the set from the built-ins plus the user file at the standard location
    /// (FR-017). Convenience wrapper over [`UserSnippetFile::load`] + [`build`].
    ///
    /// [`build`]: Self::build
    #[must_use]
    pub fn load() -> Self {
        Self::build(&UserSnippetFile::load())
    }

    /// The number of effective snippets (after merge + round-trip filtering).
    #[must_use]
    pub fn len(&self) -> usize {
        self.effective.len()
    }

    /// `true` when there are no effective snippets (degenerate; built-ins normally
    /// keep this non-empty).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.effective.is_empty()
    }

    /// Look up an effective snippet by its stable `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Snippet> {
        self.effective.get(name)
    }

    /// Find the first effective snippet whose `prefix` equals `prefix` (the trigger
    /// lookup). Returns `None` when no snippet uses that trigger.
    #[must_use]
    pub fn by_prefix(&self, prefix: &str) -> Option<&Snippet> {
        self.effective.values().find(|s| s.prefix == prefix)
    }

    /// All effective snippets, name-sorted (the iteration order is stable).
    pub fn iter(&self) -> impl Iterator<Item = &Snippet> {
        self.effective.values()
    }

    /// The degrade notice, if the user file was Missing/Malformed or dropped
    /// entries (FR-017). `None` when the set is fully healthy.
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// The user file's parse status that produced this set (for tests/hosts).
    #[must_use]
    pub fn user_status(&self) -> SnippetParseStatus {
        self.user_status
    }

    /// The discoverability menu: `(prefix, description)` for each effective snippet,
    /// name-sorted (FR-025).
    ///
    /// Lets the trigger UI list every available snippet by its trigger and a
    /// human-readable description so users can browse without prior knowledge.
    #[must_use]
    pub fn menu_entries(&self) -> Vec<(String, String)> {
        self.effective
            .values()
            .map(|s| (s.prefix.clone(), s.description.clone()))
            .collect()
    }
}

/// A single tab-stop parsed out of a snippet body (FR-016).
///
/// Tab-stops are navigated in ascending `index` order; `$0` (the final cursor) is
/// index 0 and is always visited last. A [`TabStopKind::Placeholder`] carries a
/// default value that is part of the expanded text; a [`TabStopKind::Choice`] offers
/// an inline pick list (the first option is the default in the expanded text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabStop {
    /// The tab-stop index (`$1` → 1, `${2:..}` → 2, `$0` → 0).
    pub index: u32,
    /// The character offset of the stop within the expanded [`Expansion::text`].
    pub char_start: usize,
    /// The exclusive end character offset of the stop's default text.
    pub char_end: usize,
    /// What kind of stop this is (plain, placeholder default, or a choice list).
    pub kind: TabStopKind,
}

/// The kind of a [`TabStop`] (FR-016).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabStopKind {
    /// A bare `$N` stop with no default text (zero-width in the expansion).
    Plain,
    /// A `${N:default}` placeholder; the default text occupies the stop's range.
    Placeholder { default: String },
    /// A `${N|a,b,c|}` choice; the inline pick list. The first option is the
    /// default text in the expansion.
    Choice { options: Vec<String> },
}

/// The result of expanding a snippet body (FR-016).
///
/// [`text`](Self::text) is the literal text with every default placeholder value
/// inlined (and every plain `$N` collapsed to nothing); [`stops`](Self::stops) are
/// the tab-stops in navigation order (ascending index, with the final `$0` last).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expansion {
    /// The expanded literal text (defaults inlined; markers removed).
    pub text: String,
    /// The tab-stops in navigation order: ascending positive index, then `$0`.
    pub stops: Vec<TabStop>,
}

impl Expansion {
    /// The tab-stops in navigation order (ascending positive index, `$0` last).
    #[must_use]
    pub fn stops(&self) -> &[TabStop] {
        &self.stops
    }

    /// The number of tab-stops in the expansion.
    #[must_use]
    pub fn stop_count(&self) -> usize {
        self.stops.len()
    }
}

/// Expand a VS Code-style snippet `body` into literal text + ordered tab-stops
/// (FR-016).
///
/// Recognised markers:
/// * `$N` / `${N}` — a plain tab-stop (zero-width default).
/// * `${N:default}` — a placeholder whose `default` text is inlined.
/// * `${N|a,b,c|}` — a choice; the first option is inlined as the default and the
///   full option list is carried on the [`TabStop`] for the inline picker.
/// * `$0` / `${0:..}` — the final cursor stop (visited last).
/// * `\$` — an escaped literal dollar sign.
///
/// The returned [`Expansion::stops`] are sorted into navigation order: ascending
/// positive index, then the `$0` final stop. Char offsets are over the produced
/// [`Expansion::text`]. An unrecognised `$` (not followed by a digit or `{`) is kept
/// as a literal, so an ordinary body never loses characters.
#[must_use]
pub fn expand_snippet(body: &str) -> Expansion {
    let mut text = String::with_capacity(body.len());
    let mut stops: Vec<TabStop> = Vec::new();
    // Track the current char offset into `text` (not bytes) for tab-stop ranges.
    let mut char_count: usize = 0;

    let chars: Vec<char> = body.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() && chars[i + 1] == '$' {
            // Escaped dollar: emit a literal `$`.
            text.push('$');
            char_count += 1;
            i += 2;
            continue;
        }
        if c == '$' {
            if let Some((marker, consumed)) = parse_marker(&chars, i) {
                // Inline the marker's default text and record the stop.
                let start = char_count;
                let default = marker.default_text();
                text.push_str(&default);
                char_count += default.chars().count();
                stops.push(TabStop {
                    index: marker.index,
                    char_start: start,
                    char_end: char_count,
                    kind: marker.kind,
                });
                i += consumed;
                continue;
            }
            // A `$` not starting a recognised marker: keep it literal.
            text.push('$');
            char_count += 1;
            i += 1;
            continue;
        }
        text.push(c);
        char_count += 1;
        i += 1;
    }

    sort_stops(&mut stops);
    Expansion { text, stops }
}

/// A parsed snippet marker before it becomes a [`TabStop`].
struct Marker {
    index: u32,
    kind: TabStopKind,
}

impl Marker {
    /// The default text this marker contributes to the expansion.
    fn default_text(&self) -> String {
        match &self.kind {
            TabStopKind::Plain => String::new(),
            TabStopKind::Placeholder { default } => default.clone(),
            // The first choice option is the default inlined text.
            TabStopKind::Choice { options } => options.first().cloned().unwrap_or_default(),
        }
    }
}

/// Try to parse a snippet marker starting at `chars[start]` (which must be `$`).
///
/// Returns the parsed [`Marker`] and the number of chars consumed, or `None` if the
/// `$` does not begin a recognised marker (so the caller keeps it literal).
fn parse_marker(chars: &[char], start: usize) -> Option<(Marker, usize)> {
    debug_assert_eq!(chars[start], '$');
    let next = *chars.get(start + 1)?;
    if next.is_ascii_digit() {
        // Bare `$N`: consume the run of digits.
        let (index, len) = parse_index(chars, start + 1)?;
        return Some((
            Marker {
                index,
                kind: TabStopKind::Plain,
            },
            1 + len,
        ));
    }
    if next == '{' {
        return parse_braced_marker(chars, start);
    }
    None
}

/// Parse a `${...}` marker starting at `chars[start]` (`$`). Handles `${N}`,
/// `${N:default}`, and `${N|a,b|}`.
fn parse_braced_marker(chars: &[char], start: usize) -> Option<(Marker, usize)> {
    // start: '$', start+1: '{'
    let (index, idx_len) = parse_index(chars, start + 2)?;
    let mut j = start + 2 + idx_len;
    let sep = *chars.get(j)?;
    match sep {
        '}' => Some((
            Marker {
                index,
                kind: TabStopKind::Plain,
            },
            (j + 1) - start,
        )),
        ':' => {
            // `${N:default}` — read until the matching `}`.
            j += 1;
            let mut default = String::new();
            while j < chars.len() && chars[j] != '}' {
                default.push(chars[j]);
                j += 1;
            }
            if j >= chars.len() {
                return None; // unterminated; keep `$` literal
            }
            Some((
                Marker {
                    index,
                    kind: TabStopKind::Placeholder { default },
                },
                (j + 1) - start,
            ))
        }
        '|' => {
            // `${N|a,b,c|}` — read options until the closing `|}`.
            j += 1;
            let mut raw = String::new();
            while j < chars.len() && chars[j] != '|' {
                raw.push(chars[j]);
                j += 1;
            }
            // Expect `|` then `}`.
            if chars.get(j) != Some(&'|') || chars.get(j + 1) != Some(&'}') {
                return None; // malformed choice; keep `$` literal
            }
            let options: Vec<String> = raw
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            if options.is_empty() {
                return None;
            }
            Some((
                Marker {
                    index,
                    kind: TabStopKind::Choice { options },
                },
                (j + 2) - start,
            ))
        }
        _ => None,
    }
}

/// Parse a run of ASCII digits at `chars[from]` into a `u32` index and its length.
fn parse_index(chars: &[char], from: usize) -> Option<(u32, usize)> {
    let mut len = 0usize;
    let mut value: u32 = 0;
    while let Some(c) = chars.get(from + len) {
        if let Some(d) = c.to_digit(10) {
            value = value.checked_mul(10)?.checked_add(d)?;
            len += 1;
        } else {
            break;
        }
    }
    if len == 0 {
        None
    } else {
        Some((value, len))
    }
}

/// Sort tab-stops into navigation order: ascending positive index, then `$0` last
/// (FR-016). Stops are kept in source order among equal indices (stable sort).
fn sort_stops(stops: &mut [TabStop]) {
    stops.sort_by_key(|s| {
        // `$0` is the final cursor: sort it after every positive index.
        if s.index == 0 {
            u64::from(u32::MAX) + 1
        } else {
            u64::from(s.index)
        }
    });
}

/// The CST-verified result of inserting a snippet expansion into a buffer (FR-018).
///
/// Produced by [`insert_snippet`]: the new full buffer with the expansion spliced at
/// the caret, plus the [`SnippetSession`] that drives tab-stop navigation over the
/// just-inserted text. Returned only when the spliced buffer round-trips (no *new*
/// parse diagnostics versus the original), so an insertion can never corrupt the
/// document (verify-before-replace, AD-008 / project-instructions §I).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnippetInsertion {
    /// The new full buffer text after the verified splice.
    pub new_buffer: String,
    /// The live tab-stop session over the inserted text (FR-016).
    pub session: SnippetSession,
}

/// A live snippet tab-stop navigation session (FR-016).
///
/// Tracks the caret position of each tab-stop *as character offsets into the whole
/// buffer* and which stop is currently active. `Tab` advances to the next stop and
/// `Shift+Tab` returns to the previous one (in the [`Expansion`]'s navigation order);
/// reaching `$0` (or the end) ends navigation. A stop is a caret position plus, for
/// placeholders/choices, the range of default text the editor selects so the user can
/// overtype it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnippetSession {
    /// The buffer-absolute tab-stops in navigation order.
    stops: Vec<SessionStop>,
    /// The index into [`stops`](Self::stops) of the active stop, or `None` when
    /// navigation has ended.
    active: Option<usize>,
}

/// One tab-stop within a [`SnippetSession`], in buffer-absolute char offsets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStop {
    /// The tab-stop index (`$1` → 1, `$0` → 0).
    pub index: u32,
    /// The buffer-absolute char offset of the stop's selectable text start.
    pub char_start: usize,
    /// The buffer-absolute exclusive char end of the stop's selectable text.
    pub char_end: usize,
    /// What kind of stop this is (drives the inline choice picker).
    pub kind: TabStopKind,
}

impl SnippetSession {
    /// The currently-active stop, if navigation is in progress.
    #[must_use]
    pub fn active_stop(&self) -> Option<&SessionStop> {
        self.active.and_then(|i| self.stops.get(i))
    }

    /// `true` while navigation is in progress (an active stop remains).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// The caret char offset for the active stop, or `None` once navigation ends.
    #[must_use]
    pub fn caret(&self) -> Option<usize> {
        self.active_stop().map(|s| s.char_start)
    }

    /// The selectable char range `(start, end)` of the active stop, if it has
    /// default text to overtype (placeholder / choice). `None` for a plain stop or
    /// once navigation ends.
    #[must_use]
    pub fn selection(&self) -> Option<(usize, usize)> {
        self.active_stop().and_then(|s| {
            if s.char_end > s.char_start {
                Some((s.char_start, s.char_end))
            } else {
                None
            }
        })
    }

    /// The total number of stops in this session.
    #[must_use]
    pub fn stop_count(&self) -> usize {
        self.stops.len()
    }

    /// Advance to the next tab-stop (`Tab`), ending navigation past the last
    /// (FR-016).
    ///
    /// Returns the new active stop's caret offset, or `None` when navigation has
    /// ended (the active stop was the final `$0` / last stop). Ending leaves the
    /// caret wherever the host last placed it.
    pub fn next_stop(&mut self) -> Option<usize> {
        let next = match self.active {
            Some(i) if i + 1 < self.stops.len() => Some(i + 1),
            // Past the last stop (or already ended): navigation ends.
            _ => None,
        };
        self.active = next;
        self.caret()
    }

    /// Return to the previous tab-stop (`Shift+Tab`), clamping at the first
    /// (FR-016).
    ///
    /// Returns the new active stop's caret offset. From the first stop (or when
    /// ended) it stays at / returns to the first stop rather than going negative.
    pub fn prev_stop(&mut self) -> Option<usize> {
        let prev = match self.active {
            Some(0) | None => Some(0),
            Some(i) => Some(i - 1),
        };
        // Guard against an empty session.
        self.active = prev.filter(|&i| i < self.stops.len());
        self.caret()
    }

    /// End navigation immediately (e.g. `Esc` or a click away).
    pub fn end(&mut self) {
        self.active = None;
    }
}

/// Insert a snippet `body` at `caret_char` in `buffer`, verifying the result
/// round-trips before returning it (FR-016/FR-018).
///
/// Expands the body ([`expand_snippet`]), splices the expansion's literal text at the
/// caret, and **re-parses the candidate buffer**: the insertion is returned only if
/// it introduces **no new** parse diagnostic versus the original buffer (so the
/// inserted snippet round-trips through the CST — project-instructions §I). On a
/// refusal (the splice would corrupt) it returns `None` and the caller leaves the
/// buffer untouched.
///
/// The returned [`SnippetSession`] carries the tab-stops re-based to buffer-absolute
/// char offsets, in navigation order, with the first stop active (or no active stop
/// when the body has none). The caret/selection for the first stop come from
/// [`SnippetSession::caret`] / [`SnippetSession::selection`].
#[must_use]
pub fn insert_snippet(buffer: &str, caret_char: usize, body: &str) -> Option<SnippetInsertion> {
    let expansion = expand_snippet(body);

    // Map the char caret to a byte offset, guarding a stale caret (never panic).
    let buffer_chars = buffer.chars().count();
    if caret_char > buffer_chars {
        return None;
    }
    let byte_at = |c: usize| -> usize {
        buffer
            .char_indices()
            .nth(c)
            .map_or(buffer.len(), |(b, _)| b)
    };
    let caret_byte = byte_at(caret_char);

    let mut new_buffer = String::with_capacity(buffer.len() + expansion.text.len());
    new_buffer.push_str(&buffer[..caret_byte]);
    new_buffer.push_str(&expansion.text);
    new_buffer.push_str(&buffer[caret_byte..]);

    // Verify-before-commit (Principle I / FR-018): the splice must not add a NEW
    // parse diagnostic. We allow it to reduce diagnostics (completing an in-progress
    // construct) but never to introduce one the original buffer did not have.
    let before = ronin_core::parse(buffer).diagnostics().len();
    let after = ronin_core::parse(&new_buffer).diagnostics().len();
    if after > before {
        return None;
    }

    // Re-base the expansion's (expansion-local) char offsets to buffer-absolute by
    // adding the insertion's char caret.
    let stops: Vec<SessionStop> = expansion
        .stops
        .into_iter()
        .map(|s| SessionStop {
            index: s.index,
            char_start: caret_char + s.char_start,
            char_end: caret_char + s.char_end,
            kind: s.kind,
        })
        .collect();

    let active = if stops.is_empty() { None } else { Some(0) };
    Some(SnippetInsertion {
        new_buffer,
        session: SnippetSession { stops, active },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- T038: built-ins present + round-trip (FR-015) ----------------------

    #[test]
    fn built_ins_cover_the_common_constructs() {
        let names: Vec<String> = built_ins().into_iter().map(|s| s.name).collect();
        for expected in [
            "named-struct",
            "tuple-struct",
            "unit-struct",
            "enum-unit-variant",
            "enum-tuple-variant",
            "enum-struct-variant",
            "map",
            "list",
            "tuple",
            "some",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "built-ins must include `{expected}`"
            );
        }
        // At least one Bevy-scoped pattern is present.
        assert!(
            built_ins().iter().any(|s| s.scope == SnippetScope::Bevy),
            "built-ins must include a generic Bevy pattern"
        );
        assert_eq!(built_in_count(), built_ins().len());
        assert_eq!(BUILT_INS().len(), built_ins().len());
    }

    #[test]
    fn every_built_in_default_expansion_round_trips() {
        for snippet in built_ins() {
            assert!(
                snippet.default_expansion_round_trips(),
                "built-in `{}` default expansion must round-trip: body={:?} -> {:?}",
                snippet.name,
                snippet.body,
                expand_snippet(&snippet.body).text
            );
        }
    }

    #[test]
    fn every_built_in_is_in_the_effective_set() {
        let set = SnippetSet::build(&UserSnippetFile {
            location: None,
            snippets: Vec::new(),
            parse_status: SnippetParseStatus::Missing,
        });
        assert_eq!(set.len(), built_in_count());
        assert!(set.get("list").is_some());
        // Triggerable by prefix.
        assert_eq!(set.by_prefix("list").map(|s| s.name.as_str()), Some("list"));
    }

    // ---- T038: expansion markers + navigation order (FR-016) ----------------

    #[test]
    fn plain_tab_stops_have_zero_width_and_sort_by_index() {
        let exp = expand_snippet("$2-$1-$0");
        assert_eq!(exp.text, "--");
        let order: Vec<u32> = exp.stops.iter().map(|s| s.index).collect();
        assert_eq!(order, vec![1, 2, 0], "ascending index, then $0 last");
        // Plain stops are zero-width.
        for stop in &exp.stops {
            assert_eq!(stop.char_start, stop.char_end);
            assert_eq!(stop.kind, TabStopKind::Plain);
        }
    }

    #[test]
    fn placeholder_default_is_inlined_and_ranged() {
        let exp = expand_snippet("Foo(${1:bar}: ${2:1})");
        assert_eq!(exp.text, "Foo(bar: 1)");
        let first = &exp.stops[0];
        assert_eq!(first.index, 1);
        assert_eq!(
            first.kind,
            TabStopKind::Placeholder {
                default: "bar".into()
            }
        );
        // The stop's char range covers the inlined default `bar` (offsets 4..7).
        assert_eq!(
            &exp.text[exp_byte(&exp.text, first.char_start)..exp_byte(&exp.text, first.char_end)],
            "bar"
        );
    }

    #[test]
    fn choice_placeholder_offers_options_first_is_default() {
        let exp = expand_snippet("${1|true,false|}");
        assert_eq!(exp.text, "true");
        let stop = &exp.stops[0];
        assert_eq!(
            stop.kind,
            TabStopKind::Choice {
                options: vec!["true".into(), "false".into()]
            }
        );
        // The default inlined text is the first option.
        assert_eq!(&exp.text[..stop.char_end], "true");
    }

    #[test]
    fn final_cursor_sorts_last_even_with_high_indices() {
        let exp = expand_snippet("a$0b$3c$1");
        let order: Vec<u32> = exp.stops.iter().map(|s| s.index).collect();
        assert_eq!(order, vec![1, 3, 0]);
        assert_eq!(exp.text, "abc");
    }

    #[test]
    fn escaped_dollar_is_literal_and_unknown_dollar_kept() {
        let exp = expand_snippet("price: \\$5 and $notamarker");
        assert_eq!(exp.text, "price: $5 and $notamarker");
        assert!(exp.stops.is_empty());
    }

    // ---- T038: user file load + override-by-name (FR-017) --------------------

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join("ronin_snippets_test");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A process-globally-unique suffix so parallel tests never share a temp path
    /// (PID alone collides across threads in one test binary).
    fn unique() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        format!(
            "{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        )
    }

    #[test]
    fn missing_user_file_degrades_to_built_ins() {
        let path = temp_dir().join(format!("missing-{}.json", unique()));
        let _ = std::fs::remove_file(&path);
        let user = UserSnippetFile::load_from(&path);
        assert_eq!(user.parse_status, SnippetParseStatus::Missing);
        let set = SnippetSet::build(&user);
        assert_eq!(set.len(), built_in_count(), "built-ins still available");
        // Missing is the ordinary case: no degrade notice.
        assert!(set.notice().is_none());
    }

    #[test]
    fn malformed_user_file_degrades_to_built_ins_with_notice() {
        let path = temp_dir().join(format!("malformed-{}.json", unique()));
        std::fs::write(&path, b"{ this is not valid json ").unwrap();
        let user = UserSnippetFile::load_from(&path);
        assert_eq!(user.parse_status, SnippetParseStatus::Malformed);
        let set = SnippetSet::build(&user);
        assert_eq!(set.len(), built_in_count(), "built-ins still available");
        assert!(
            set.notice().is_some(),
            "a malformed file must surface a degrade notice (FR-017)"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn user_snippet_overrides_built_in_by_name() {
        let path = temp_dir().join(format!("override-{}.json", unique()));
        // Override the built-in `list` with a different body/description.
        std::fs::write(
            &path,
            br#"{
              "list": {
                "prefix": "mylist",
                "body": "[${1:1}, ${2:2}]",
                "description": "My custom list"
              }
            }"#,
        )
        .unwrap();
        let user = UserSnippetFile::load_from(&path);
        assert_eq!(user.parse_status, SnippetParseStatus::Ok);
        let set = SnippetSet::build(&user);
        let listed = set.get("list").expect("merged list snippet");
        assert_eq!(listed.prefix, "mylist", "user override wins (FR-017)");
        assert_eq!(listed.description, "My custom list");
        // The non-overridden built-ins are still present, so the count is unchanged.
        assert_eq!(set.len(), built_in_count());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn user_snippet_adds_a_new_name() {
        let path = temp_dir().join(format!("add-{}.json", unique()));
        std::fs::write(
            &path,
            br#"{
              "my-point": {
                "prefix": "pt",
                "body": "Point(x: ${1:0}, y: ${2:0})",
                "description": "A point"
              }
            }"#,
        )
        .unwrap();
        let set = SnippetSet::build(&UserSnippetFile::load_from(&path));
        assert_eq!(set.len(), built_in_count() + 1, "a new name adds one entry");
        assert!(set.get("my-point").is_some());
        assert_eq!(
            set.by_prefix("pt").map(|s| s.name.as_str()),
            Some("my-point")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn user_snippet_with_unparseable_default_is_dropped_with_notice() {
        let path = temp_dir().join(format!("baddefault-{}.json", unique()));
        // A body whose default expansion is invalid RON (unbalanced paren).
        std::fs::write(
            &path,
            br#"{ "broken": { "prefix": "brk", "body": "Foo(${1:x}" } }"#,
        )
        .unwrap();
        let user = UserSnippetFile::load_from(&path);
        assert_eq!(user.parse_status, SnippetParseStatus::Ok);
        let set = SnippetSet::build(&user);
        assert!(
            set.get("broken").is_none(),
            "a non-round-trippable user snippet must be dropped (FR-018)"
        );
        assert!(
            set.notice().is_some(),
            "dropping a bad entry surfaces a notice"
        );
        assert_eq!(set.len(), built_in_count());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn menu_entries_list_prefix_and_description() {
        let set = SnippetSet::build(&UserSnippetFile {
            location: None,
            snippets: Vec::new(),
            parse_status: SnippetParseStatus::Missing,
        });
        let entries = set.menu_entries();
        assert_eq!(entries.len(), built_in_count());
        assert!(
            entries.iter().any(|(p, d)| p == "list" && !d.is_empty()),
            "menu lists each prefix with a description (FR-025)"
        );
    }

    #[test]
    fn user_snippet_template_round_trips_as_json_and_snippet() {
        // The starter template is valid JSON that loads cleanly.
        let path = temp_dir().join(format!("template-{}.json", unique()));
        std::fs::write(&path, USER_SNIPPET_TEMPLATE).unwrap();
        let user = UserSnippetFile::load_from(&path);
        assert_eq!(user.parse_status, SnippetParseStatus::Ok);
        assert!(user.snippets.iter().any(|s| s.name == "example-point"));
        // And the template snippet's default expansion round-trips (FR-018).
        let set = SnippetSet::build(&user);
        assert!(set.get("example-point").is_some());
        let _ = std::fs::remove_file(&path);
    }

    /// Map a char offset to a byte offset in `text` (test helper).
    fn exp_byte(text: &str, char_off: usize) -> usize {
        text.char_indices()
            .nth(char_off)
            .map_or(text.len(), |(b, _)| b)
    }

    // ---- T035/T036: insertion + tab-stop session (FR-016/FR-018) ------------

    #[test]
    fn insert_into_empty_buffer_round_trips_and_starts_at_first_stop() {
        // Insert a named-struct snippet at the start of an empty buffer.
        let body = "${1:Name}(${2:field}: ${3:value})";
        let ins = insert_snippet("", 0, body).expect("insertion round-trips");
        assert_eq!(ins.new_buffer, "Name(field: value)");
        assert!(body_round_trips(&ins.new_buffer));
        // First stop active: caret at `Name`, selection covers it.
        assert_eq!(ins.session.caret(), Some(0));
        assert_eq!(ins.session.selection(), Some((0, 4))); // "Name"
        assert!(ins.session.is_active());
        assert_eq!(ins.session.stop_count(), 3); // $1,$2,$3 (no $0 in this body)
    }

    #[test]
    fn tab_navigates_stops_in_index_order_then_ends() {
        // `Some(value)` with an explicit final cursor after.
        let body = "Some(${1:value})$0";
        let mut ins = insert_snippet("[]", 1, body).expect("insertion round-trips");
        // Spliced inside the list: `[Some(value)]`.
        assert_eq!(ins.new_buffer, "[Some(value)]");
        // First stop: the `value` placeholder.
        assert_eq!(ins.session.active_stop().map(|s| s.index), Some(1));
        // Tab → final cursor `$0` (index 0).
        ins.session.next_stop();
        assert_eq!(ins.session.active_stop().map(|s| s.index), Some(0));
        // Tab past the last stop ends navigation.
        assert_eq!(ins.session.next_stop(), None);
        assert!(!ins.session.is_active());
    }

    #[test]
    fn shift_tab_returns_to_the_previous_stop() {
        let body = "(${1:a}, ${2:b})$0";
        let mut ins = insert_snippet("", 0, body).expect("round-trips");
        assert_eq!(ins.session.active_stop().map(|s| s.index), Some(1));
        ins.session.next_stop(); // → $2
        assert_eq!(ins.session.active_stop().map(|s| s.index), Some(2));
        ins.session.prev_stop(); // ← $1
        assert_eq!(ins.session.active_stop().map(|s| s.index), Some(1));
        // Shift+Tab at the first stop stays at the first.
        ins.session.prev_stop();
        assert_eq!(ins.session.active_stop().map(|s| s.index), Some(1));
    }

    #[test]
    fn choice_stop_carries_its_options_for_the_inline_picker() {
        let body = "${1|true,false|}";
        let ins = insert_snippet("", 0, body).expect("round-trips");
        assert_eq!(ins.new_buffer, "true");
        match &ins.session.active_stop().unwrap().kind {
            TabStopKind::Choice { options } => {
                assert_eq!(options, &vec!["true".to_string(), "false".to_string()]);
            }
            other => panic!("expected a choice stop, got {other:?}"),
        }
    }

    #[test]
    fn insertion_that_would_corrupt_is_refused() {
        // A body whose expansion introduces a new parse error into an otherwise
        // clean buffer must be refused (verify-before-replace, FR-018).
        // Splicing `Foo(` (unbalanced) into the clean `[1]` adds an error.
        assert!(
            insert_snippet("[1]", 3, "Foo(${1:x}").is_none(),
            "a corrupting insertion must be refused"
        );
    }

    #[test]
    fn insertion_with_stale_caret_does_not_panic() {
        assert!(
            insert_snippet("[]", 999, "Some(${1:x})").is_none(),
            "a caret past the buffer end is refused, never panics"
        );
    }

    #[test]
    fn buffer_absolute_offsets_account_for_the_caret() {
        // Insert mid-buffer; the session offsets must be buffer-absolute.
        let ins = insert_snippet("[, 2]", 1, "${1:1}").expect("round-trips");
        assert_eq!(ins.new_buffer, "[1, 2]");
        // The stop covers `1` at buffer offset 1..2.
        assert_eq!(ins.session.selection(), Some((1, 2)));
    }
}
