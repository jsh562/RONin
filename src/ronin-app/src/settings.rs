//! Persisted application settings (FR-016).
//!
//! [`AppSettings`] holds window geometry, a free-form preferences map, and the
//! large-file warning threshold. It is loaded from / saved to a small JSON file
//! in the OS **config** directory (via [`directories::ProjectDirs`]).
//!
//! Robustness contract (project-instructions §I, "never corrupt user data"):
//! a missing **or corrupt** settings file never panics and never blocks startup —
//! [`AppSettings::load`] falls back to [`AppSettings::default`]. Settings store
//! **no session state**: no open-document set, paths, or tab layout (no session
//! restore).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// `ProjectDirs` qualifier/organization/application triple identifying RONin's
/// OS-specific config and data directories. Stable across the app.
const QUALIFIER: &str = "dev";
const ORGANIZATION: &str = "ronin";
const APPLICATION: &str = "RONin";

/// Default large-file warning threshold: 5 MiB.
const DEFAULT_LARGE_FILE_THRESHOLD: u64 = 5_242_880;

/// Sane floor for the large-file threshold: 64 KiB (FR-017).
///
/// The threshold is user-configurable, but it MUST NOT drop below this floor —
/// otherwise an ordinary small file could be wrongly degraded (highlighting /
/// squiggles disabled). Both [`AppSettings::load_from`] and
/// [`AppSettings::set_large_file_threshold`] clamp to this minimum, so a
/// hand-edited settings file can never push the effective threshold below it.
const MIN_LARGE_FILE_THRESHOLD: u64 = 65_536;

/// Default indent width for the formatter: 4 spaces (FR-007).
const DEFAULT_INDENT_WIDTH: u32 = 4;

/// Sane minimum / maximum indent width (FR-007). A hand-edited settings file
/// specifying an absurd indent width is clamped to this range on load and via the
/// setter, mirroring `ron_core::FormatConfig`'s own clamp so the two never diverge.
const MIN_INDENT_WIDTH: u32 = 1;
const MAX_INDENT_WIDTH: u32 = 16;

/// How the formatter treats runs of blank lines between elements (FR-007).
///
/// Mirrors `ron_core::BlankLinePolicy`; kept as its own type so `ronin-app` does
/// not leak the engine enum into its persisted settings schema (and so the JSON
/// representation is owned here). [`FormattingConfig::to_engine_config`] maps it to
/// the engine value when a format is actually invoked (Wave 2).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlankLinePolicy {
    /// Collapse any run of blank lines to at most one (the default).
    #[default]
    Collapse,
    /// Preserve the original blank-line count between elements.
    Preserve,
}

/// Saved window position and size (logical pixels).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowGeometry {
    /// Top-left position `(x, y)`, if the platform reported one.
    pub pos: Option<(f32, f32)>,
    /// Window size `(width, height)`.
    pub size: (f32, f32),
}

impl Default for WindowGeometry {
    fn default() -> Self {
        // A sensible default editor window size; position left to the WM.
        Self {
            pos: None,
            size: (1280.0, 800.0),
        }
    }
}

/// Formatter configuration persisted with the app settings (FR-007).
///
/// The formatter-facing knobs the user can adjust on the settings surface: the
/// indent width, the blank-line policy, and whether to format automatically on
/// save. Mirrors `ron_core::FormatConfig` (the engine value type) plus the
/// surface-only `format_on_save` flag. Robustness contract (project-instructions
/// §I): an absent or corrupt on-disk value falls back to the defaults and an
/// out-of-range indent width is clamped — a hand-edited settings file can never
/// produce an unusable formatter config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FormattingConfig {
    /// Spaces of indent per nesting depth (default 4, clamped to `1..=16`).
    pub indent_width: u32,
    /// How runs of blank lines are treated (default [`BlankLinePolicy::Collapse`]).
    pub blank_line_policy: BlankLinePolicy,
    /// When `true`, the document is formatted automatically on every save
    /// (default `false`). Wired into the save path in Wave 2.
    pub format_on_save: bool,
}

impl Default for FormattingConfig {
    fn default() -> Self {
        Self {
            indent_width: DEFAULT_INDENT_WIDTH,
            blank_line_policy: BlankLinePolicy::default(),
            format_on_save: false,
        }
    }
}

impl FormattingConfig {
    /// The indent width clamped to the sane `1..=16` range (FR-007).
    ///
    /// Read formatter config through this rather than the raw field so a corrupt /
    /// hand-edited value can never push the effective indent out of range.
    #[must_use]
    pub fn effective_indent_width(&self) -> u32 {
        self.indent_width.clamp(MIN_INDENT_WIDTH, MAX_INDENT_WIDTH)
    }

    /// Set the indent width, clamping to the sane `1..=16` range (FR-007).
    pub fn set_indent_width(&mut self, width: u32) {
        self.indent_width = width.clamp(MIN_INDENT_WIDTH, MAX_INDENT_WIDTH);
    }

    /// The smallest permitted indent width (1).
    #[must_use]
    pub const fn min_indent_width() -> u32 {
        MIN_INDENT_WIDTH
    }

    /// The largest permitted indent width (16).
    #[must_use]
    pub const fn max_indent_width() -> u32 {
        MAX_INDENT_WIDTH
    }

    /// Build the `ron-core` [`FormatConfig`](ron_core::FormatConfig) this surface
    /// config maps to (the engine value used when a format is actually invoked,
    /// Wave 2). The engine clamps the indent width too, so the two stay in sync.
    #[must_use]
    pub fn to_engine_config(&self) -> ron_core::FormatConfig {
        let policy = match self.blank_line_policy {
            BlankLinePolicy::Collapse => ron_core::BlankLinePolicy::Collapse,
            BlankLinePolicy::Preserve => ron_core::BlankLinePolicy::Preserve,
        };
        ron_core::FormatConfig::new(self.effective_indent_width(), policy)
    }
}

/// User-facing application settings persisted between sessions (FR-016).
///
/// Deliberately excludes any session/document state. Unknown fields in an older
/// or newer on-disk file are ignored on load (serde defaults fill the gaps),
/// keeping forward/backward compatibility from being a crash source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Last-known window geometry, restored on next launch when present.
    pub window_geometry: Option<WindowGeometry>,
    /// Free-form string preferences (theme, font choice, etc.).
    pub preferences: BTreeMap<String, String>,
    /// Files larger than this many bytes trigger the large-file warning path.
    pub large_file_threshold: u64,
    /// Formatter configuration (FR-007): indent width, blank-line policy, and the
    /// format-on-save toggle. Defaults on absent / corrupt (serde `default`).
    pub formatting: FormattingConfig,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            window_geometry: None,
            preferences: BTreeMap::new(),
            large_file_threshold: DEFAULT_LARGE_FILE_THRESHOLD,
            formatting: FormattingConfig::default(),
        }
    }
}

impl AppSettings {
    /// Load settings from the OS config directory.
    ///
    /// Returns [`AppSettings::default`] when the file is absent, unreadable, or
    /// corrupt — never panics, never errors. (Corruption is recovered from by
    /// design so a bad settings file can't lock the user out of the editor.)
    #[must_use]
    pub fn load() -> Self {
        let Some(path) = Self::config_path() else {
            return Self::default();
        };
        Self::load_from(&path)
    }

    /// Load settings from an explicit `path`, falling back to
    /// [`AppSettings::default`] when the file is absent, unreadable, or corrupt.
    ///
    /// Shared by [`load`](Self::load) and by tests, which inject a temp path so
    /// they never read (or depend on) the real OS config file. The same
    /// never-panic recovery contract applies: any failure yields defaults.
    #[must_use]
    pub fn load_from(path: &Path) -> Self {
        let Ok(bytes) = std::fs::read(path) else {
            return Self::default();
        };
        let mut settings = serde_json::from_slice::<AppSettings>(&bytes).unwrap_or_default();
        // Enforce the FR-017 floor: a settings file specifying an absurdly small
        // threshold must not be able to degrade ordinary files.
        settings.large_file_threshold = settings.large_file_threshold.max(MIN_LARGE_FILE_THRESHOLD);
        // Enforce the FR-007 indent clamp on load so a hand-edited / corrupt indent
        // width can never push the effective formatter config out of range.
        settings.formatting.indent_width = settings.formatting.effective_indent_width();
        settings
    }

    /// Persist settings to the OS config directory as JSON.
    ///
    /// Creates the config directory if needed. Pretty-prints for human
    /// inspectability.
    ///
    /// # Errors
    ///
    /// Returns an [`std::io::Error`] if the config directory cannot be located
    /// or the file cannot be created/written.
    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::config_path().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "could not locate an OS config directory for RONin",
            )
        })?;
        self.save_to(&path)
    }

    /// Persist settings to an explicit `path` as pretty-printed JSON, creating
    /// parent directories if needed.
    ///
    /// Shared by [`save`](Self::save) and by tests, which inject a temp path so
    /// they never clobber the real OS config file.
    ///
    /// # Errors
    ///
    /// Returns an [`std::io::Error`] if a parent directory cannot be created or
    /// the file cannot be written.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Serialization of plain settings cannot fail; map defensively anyway.
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)
    }

    /// The absolute path of the settings file in the OS config directory, or
    /// `None` if the platform exposes no config directory.
    #[must_use]
    pub fn config_path() -> Option<PathBuf> {
        directories::ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
            .map(|dirs| dirs.config_dir().join("settings.json"))
    }

    /// Set the large-file threshold, clamping to the [`MIN_LARGE_FILE_THRESHOLD`]
    /// floor (FR-017). Use this rather than assigning the field directly so the
    /// floor can never be bypassed at runtime.
    pub fn set_large_file_threshold(&mut self, bytes: u64) {
        self.large_file_threshold = bytes.max(MIN_LARGE_FILE_THRESHOLD);
    }

    /// The configured large-file threshold, guaranteed to be at or above the
    /// [`MIN_LARGE_FILE_THRESHOLD`] floor (FR-017). Callers gating degrade
    /// behavior should read this rather than the raw field.
    #[must_use]
    pub fn effective_large_file_threshold(&self) -> u64 {
        self.large_file_threshold.max(MIN_LARGE_FILE_THRESHOLD)
    }

    /// The FR-017 floor (64 KiB): the smallest the large-file threshold may be.
    #[must_use]
    pub const fn min_large_file_threshold() -> u64 {
        MIN_LARGE_FILE_THRESHOLD
    }
}

/// Whether a binding source locator names a Rust source or a schema file (E006 US2
/// — FR-008).
///
/// A surface-only enum used by the binding-config form so the source-kind picker
/// has a plain, `Copy`/`PartialEq` value to bind a combo box to; it maps to
/// [`crate::binding::TypeSourceLocator`] when a rule/override is committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceKind {
    /// A schema file (E004's serialized JSON-Schema interchange), read as data.
    #[default]
    Schema,
    /// A Rust source file / crate path E004 extracts a `TypeModel` from.
    Rust,
}

impl SourceKind {
    /// Build the matching [`TypeSourceLocator`](crate::binding::TypeSourceLocator)
    /// for `path`.
    #[must_use]
    pub fn locator(self, path: impl Into<PathBuf>) -> crate::binding::TypeSourceLocator {
        match self {
            SourceKind::Schema => crate::binding::TypeSourceLocator::SchemaFile(path.into()),
            SourceKind::Rust => crate::binding::TypeSourceLocator::RustSource(path.into()),
        }
    }

    /// Classify an existing locator back into a [`SourceKind`] (for editing a rule).
    #[must_use]
    pub fn of(locator: &crate::binding::TypeSourceLocator) -> Self {
        match locator {
            crate::binding::TypeSourceLocator::SchemaFile(_) => SourceKind::Schema,
            crate::binding::TypeSourceLocator::RustSource(_) => SourceKind::Rust,
        }
    }
}

/// Editable form state backing the binding-config window's add/edit controls (E006
/// US2 — FR-008/FR-009).
///
/// Pure UI-input state — **not** persisted (only [`crate::binding::BindingConfig`]
/// persists). The window reads/writes these text fields each frame and, on
/// commit, validates them into a [`BindingRule`](crate::binding::BindingRule) or a
/// [`DocumentOverride`](crate::binding::DocumentOverride). An empty `type_name` or
/// `source_path` is rejected (the commit button is disabled) so a blank rule is
/// never created.
#[derive(Debug, Clone, Default)]
pub struct BindingFormDraft {
    /// The glob pattern for a rule (unused by the override form).
    pub pattern: String,
    /// Comma-separated exclude globs for a rule (unused by the override form).
    pub exclude: String,
    /// The named type to bind to.
    pub type_name: String,
    /// The source path (interpreted per [`source_kind`](Self::source_kind)).
    pub source_path: String,
    /// Whether the source is a schema file or a Rust source.
    pub source_kind: SourceKind,
}

impl BindingFormDraft {
    /// Build a [`BindingRule`](crate::binding::BindingRule) from this draft, or
    /// `None` when the `pattern`, `type_name`, or `source_path` are blank (FR-008).
    ///
    /// `exclude` is split on commas; empty entries are dropped, and an empty list
    /// becomes `None` (no exclusions).
    #[must_use]
    pub fn to_rule(&self) -> Option<crate::binding::BindingRule> {
        let pattern = self.pattern.trim();
        let type_name = self.type_name.trim();
        let source_path = self.source_path.trim();
        if pattern.is_empty() || type_name.is_empty() || source_path.is_empty() {
            return None;
        }
        let excludes: Vec<String> = self
            .exclude
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        Some(crate::binding::BindingRule {
            pattern: pattern.to_string(),
            exclude: if excludes.is_empty() {
                None
            } else {
                Some(excludes)
            },
            type_name: type_name.to_string(),
            type_source: self.source_kind.locator(source_path),
        })
    }

    /// Build a [`DocumentOverride`](crate::binding::DocumentOverride) from this
    /// draft, or `None` when the `type_name` or `source_path` are blank (FR-009).
    ///
    /// The override ignores `pattern` / `exclude` (it targets the active document
    /// directly, not by glob).
    #[must_use]
    pub fn to_override(&self) -> Option<crate::binding::DocumentOverride> {
        let type_name = self.type_name.trim();
        let source_path = self.source_path.trim();
        if type_name.is_empty() || source_path.is_empty() {
            return None;
        }
        Some(crate::binding::DocumentOverride {
            type_name: type_name.to_string(),
            type_source: self.source_kind.locator(source_path),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_threshold_is_5_mib() {
        assert_eq!(AppSettings::default().large_file_threshold, 5_242_880);
    }

    #[test]
    fn load_from_clamps_tiny_threshold_to_floor() {
        let dir = std::env::temp_dir().join("ronin_settings_floor_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tiny.json");
        // A hand-edited settings file demanding a 1-byte threshold.
        std::fs::write(&path, br#"{"large_file_threshold": 1}"#).unwrap();
        let loaded = AppSettings::load_from(&path);
        assert_eq!(
            loaded.large_file_threshold,
            AppSettings::min_large_file_threshold()
        );
        assert_eq!(loaded.large_file_threshold, 65_536);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_from_preserves_a_normal_threshold() {
        let dir = std::env::temp_dir().join("ronin_settings_floor_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("normal.json");
        std::fs::write(&path, br#"{"large_file_threshold": 10485760}"#).unwrap();
        let loaded = AppSettings::load_from(&path);
        assert_eq!(loaded.large_file_threshold, 10_485_760);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn setter_clamps_below_floor_but_keeps_valid_values() {
        let mut s = AppSettings::default();
        s.set_large_file_threshold(10);
        assert_eq!(s.large_file_threshold, 65_536);
        s.set_large_file_threshold(2_000_000);
        assert_eq!(s.large_file_threshold, 2_000_000);
        assert_eq!(s.effective_large_file_threshold(), 2_000_000);
    }

    #[test]
    fn formatting_config_defaults() {
        let f = FormattingConfig::default();
        assert_eq!(f.indent_width, 4);
        assert_eq!(f.blank_line_policy, BlankLinePolicy::Collapse);
        assert!(!f.format_on_save);
        // The default app settings embed the default formatting config.
        assert_eq!(
            AppSettings::default().formatting,
            FormattingConfig::default()
        );
    }

    #[test]
    fn formatting_config_clamps_indent_width() {
        let mut f = FormattingConfig::default();
        f.set_indent_width(0);
        assert_eq!(f.indent_width, FormattingConfig::min_indent_width());
        f.set_indent_width(999);
        assert_eq!(f.indent_width, FormattingConfig::max_indent_width());
        f.set_indent_width(8);
        assert_eq!(f.indent_width, 8);
        assert_eq!(f.effective_indent_width(), 8);
    }

    #[test]
    fn load_from_clamps_absurd_indent_width() {
        let dir = std::env::temp_dir().join("ronin_settings_fmt_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("indent.json");
        std::fs::write(&path, br#"{"formatting": {"indent_width": 9999}}"#).unwrap();
        let loaded = AppSettings::load_from(&path);
        assert_eq!(
            loaded.formatting.indent_width,
            FormattingConfig::max_indent_width()
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_from_uses_formatting_defaults_when_absent() {
        // An older settings file with no `formatting` block loads with the defaults
        // (serde `default`), never a parse error (project-instructions §I).
        let dir = std::env::temp_dir().join("ronin_settings_fmt_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("absent.json");
        std::fs::write(&path, br#"{"large_file_threshold": 1048576}"#).unwrap();
        let loaded = AppSettings::load_from(&path);
        assert_eq!(loaded.formatting, FormattingConfig::default());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn formatting_round_trips_through_save_and_load() {
        let dir = std::env::temp_dir().join("ronin_settings_fmt_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("roundtrip.json");
        let mut s = AppSettings::default();
        s.formatting.set_indent_width(2);
        s.formatting.blank_line_policy = BlankLinePolicy::Preserve;
        s.formatting.format_on_save = true;
        s.save_to(&path).unwrap();
        let loaded = AppSettings::load_from(&path);
        assert_eq!(loaded.formatting, s.formatting);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn formatting_maps_to_engine_config() {
        let mut f = FormattingConfig::default();
        f.set_indent_width(2);
        f.blank_line_policy = BlankLinePolicy::Preserve;
        let engine = f.to_engine_config();
        assert_eq!(engine.indent_width(), 2);
        assert_eq!(
            engine.blank_line_policy(),
            ron_core::BlankLinePolicy::Preserve
        );
    }

    #[test]
    fn changing_threshold_reevaluates_document_oversize() {
        // FR-017: a threshold change must re-evaluate degrade state. `oversize`
        // reads the threshold by argument and is recomputed every frame, so the
        // same document flips its degrade verdict when the threshold changes.
        use crate::document::EditorDocument;
        let doc = EditorDocument::from_loaded("sample.ron", b"(value: 1234567890)").unwrap();
        let len = doc.buffer.len() as u64;
        assert!(doc.oversize(len - 1), "tiny threshold => oversize");
        assert!(!doc.oversize(len + 1), "large threshold => not oversize");
    }
}
