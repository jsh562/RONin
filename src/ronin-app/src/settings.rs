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
/// setter, mirroring `ronin_core::FormatConfig`'s own clamp so the two never diverge.
const MIN_INDENT_WIDTH: u32 = 1;
const MAX_INDENT_WIDTH: u32 = 16;

// --- E007 non-destructive-persistence NEW-CONFIG (TR-024/025/026/027) -------
//
// Undo-history cap, autosave debounce, and the undo coalesce window. Each knob
// is clamped on load and via its setter to a sane range so a corrupt / out-of-
// range / hand-edited settings file can NEVER disable the bound, the debounce,
// or coalescing (it falls back to the default or the nearest range edge, never
// "off"; TR-026). The undo cap defaults mirror `ronin_core::undo`'s constants so
// the two never diverge.

/// Default undo-history unit-count cap: 200 units (TR-024).
const DEFAULT_UNDO_COUNT_CAP: usize = 200;
/// Sane range for the undo unit-count cap: `1..=10_000` (TR-024). Never 0 (0
/// would disable undo entirely) and never unbounded.
const MIN_UNDO_COUNT_CAP: usize = 1;
const MAX_UNDO_COUNT_CAP: usize = 10_000;

/// Default undo-history byte-size cap: 64 MiB (TR-024).
const DEFAULT_UNDO_BYTE_CAP: usize = 64 * 1024 * 1024;
/// Sane range for the undo byte-size cap: 1 MiB..=1 GiB (TR-024).
const MIN_UNDO_BYTE_CAP: usize = 1024 * 1024;
const MAX_UNDO_BYTE_CAP: usize = 1024 * 1024 * 1024;

/// Default autosave idle debounce: 4 s (TR-025). After this much idle time on a
/// dirty+changed buffer the recovery sidecar is written.
const DEFAULT_AUTOSAVE_IDLE_MS: u64 = 4_000;
/// Sane range for the autosave idle debounce: 250 ms..=300 s (TR-025). Clamped
/// up from 0/below-min so autosave is never disabled by a tiny/zero value.
const MIN_AUTOSAVE_IDLE_MS: u64 = 250;
const MAX_AUTOSAVE_IDLE_MS: u64 = 300_000;

/// Default autosave edit-count trigger: 50 edits (TR-025). Whichever of idle or
/// edit-count fires first triggers the sidecar write.
const DEFAULT_AUTOSAVE_EDIT_COUNT: u32 = 50;
/// Sane range for the autosave edit-count trigger: `1..=10_000` (TR-025). Never
/// 0 (0 would never fire on edit count) and never unbounded.
const MIN_AUTOSAVE_EDIT_COUNT: u32 = 1;
const MAX_AUTOSAVE_EDIT_COUNT: u32 = 10_000;

/// Default undo coalesce window: 500 ms (TR-027). Edits closer together than
/// this fold into a single undo unit.
const DEFAULT_COALESCE_WINDOW_MS: u64 = 500;
/// Sane range for the coalesce window: 50 ms..=5 s (TR-027). Clamped up from
/// 0/below-min so coalescing is never disabled.
const MIN_COALESCE_WINDOW_MS: u64 = 50;
const MAX_COALESCE_WINDOW_MS: u64 = 5_000;

// --- E010 RON⇄JSON interop NEW-CONFIG (FR-008) ------------------------------
//
// The ONLY persisted artifact E010 adds (data-model §ConversionSettings): the
// RON→JSON output-format default (JSONC vs strict JSON), the JSON pretty-print
// indent, and the strict-mode comment carrier default. It rides this existing
// settings store — no new store. Each value is a *default* overridable per
// conversion by the convert/loss-report dialog (the override is applied by the
// caller later — US1 T016). An absent / corrupt / out-of-range on-disk value
// falls back to JSONC + a default indent and never crashes (data-model
// §ConversionSettings "Absent/corrupt → safe defaults").

/// Default JSON pretty-print indent width for RON→JSON: 2 spaces (NEW-CONFIG).
const DEFAULT_JSON_INDENT: u32 = 2;
/// Sane range for the JSON indent width: `0..=16`. Zero means a compact /
/// minimally-indented output; the upper bound mirrors the formatter's clamp so a
/// hand-edited value can never produce an absurd indent.
const MIN_JSON_INDENT: u32 = 0;
const MAX_JSON_INDENT: u32 = 16;

/// How the formatter treats runs of blank lines between elements (FR-007).
///
/// Mirrors `ronin_core::BlankLinePolicy`; kept as its own type so `ronin-app` does
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

/// The RON→JSON output form (FR-008, NEW-CONFIG).
///
/// JSONC (JSON-with-comments) is the default and primary comment carrier; strict
/// standard JSON is also available, carrying comments via a sibling sidecar map
/// by default (see [`StrictCommentCarrier`]). Kept as `ronin-app`'s own enum so
/// the persisted JSON schema is owned here and no interop type leaks into the
/// settings file.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JsonFormat {
    /// JSON-with-comments — comments preserved inline (the default/primary
    /// carrier, FR-008).
    #[default]
    Jsonc,
    /// Strict standard JSON — comments carried via a sibling sidecar by default,
    /// or dropped (and reported) only when the sidecar is also declined (FR-008).
    StrictJson,
}

impl JsonFormat {
    /// The stable lowercase label for this format.
    #[inline]
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            JsonFormat::Jsonc => "jsonc",
            JsonFormat::StrictJson => "strict-json",
        }
    }

    /// `true` when this is the JSONC (comment-preserving) form.
    #[inline]
    #[must_use]
    pub fn is_jsonc(self) -> bool {
        matches!(self, JsonFormat::Jsonc)
    }
}

/// How comments are carried when the output form is strict standard JSON
/// (FR-008, NEW-CONFIG).
///
/// In strict mode JSONC inline comments are not permitted, so comments survive in
/// a **sibling sidecar map** by default; only [`PureNoComments`](Self::PureNoComments)
/// drops them — and then each dropped comment is reported as a loss (FR-007).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StrictCommentCarrier {
    /// Carry comments in a deterministic sibling sidecar comment map (the strict
    /// default so comments survive strict mode, FR-008).
    #[default]
    Sidecar,
    /// Pure standard JSON: comments are dropped and each drop is reported as a
    /// [`DroppedComment`](crate::interop::LossKind::DroppedComment) loss (FR-008,
    /// FR-007).
    PureNoComments,
}

impl StrictCommentCarrier {
    /// The stable lowercase label for this carrier.
    #[inline]
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            StrictCommentCarrier::Sidecar => "sidecar",
            StrictCommentCarrier::PureNoComments => "pure-no-comments",
        }
    }
}

/// RON⇄JSON conversion preferences persisted with the app settings — the **only**
/// on-disk artifact E010 adds (FR-008, NEW-CONFIG, data-model §ConversionSettings).
///
/// Holds the RON→JSON output-format default ([`JsonFormat`]), the JSON
/// pretty-print indent, and the strict-mode comment-carrier default. Every value
/// is a **default, not a lock**: the convert/loss-report dialog overrides it for a
/// single run without changing the persisted default (the override is applied by
/// the caller later, US1 T016). Robustness contract (data-model
/// §ConversionSettings, project-instructions §I): an absent / corrupt block falls
/// back to JSONC + a default indent (serde `default`), and an out-of-range indent
/// is clamped on load and via the setter — a hand-edited settings file can never
/// produce an unusable conversion config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConversionSettings {
    /// The persisted RON→JSON output default (JSONC by default, FR-008).
    pub default_format: JsonFormat,
    /// The persisted JSON pretty-print indent width (default 2, clamped to
    /// `0..=16`).
    pub json_indent: u32,
    /// The persisted strict-mode comment-carrier default (sidecar by default so
    /// comments survive strict mode, FR-008).
    pub strict_default_comment_carrier: StrictCommentCarrier,
}

impl Default for ConversionSettings {
    fn default() -> Self {
        Self {
            default_format: JsonFormat::default(),
            json_indent: DEFAULT_JSON_INDENT,
            strict_default_comment_carrier: StrictCommentCarrier::default(),
        }
    }
}

impl ConversionSettings {
    /// The JSON indent width clamped to the sane `0..=16` range (NEW-CONFIG).
    ///
    /// Read the indent through this rather than the raw field so a corrupt /
    /// hand-edited value can never push the effective indent out of range.
    #[must_use]
    pub fn effective_json_indent(&self) -> u32 {
        self.json_indent.clamp(MIN_JSON_INDENT, MAX_JSON_INDENT)
    }

    /// Set the JSON indent width, clamping to the sane `0..=16` range
    /// (NEW-CONFIG).
    pub fn set_json_indent(&mut self, width: u32) {
        self.json_indent = width.clamp(MIN_JSON_INDENT, MAX_JSON_INDENT);
    }

    /// The smallest permitted JSON indent width (0).
    #[must_use]
    pub const fn min_json_indent() -> u32 {
        MIN_JSON_INDENT
    }

    /// The largest permitted JSON indent width (16).
    #[must_use]
    pub const fn max_json_indent() -> u32 {
        MAX_JSON_INDENT
    }
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
/// save. Mirrors `ronin_core::FormatConfig` (the engine value type) plus the
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

    /// Build the `ronin-core` [`FormatConfig`](ronin_core::FormatConfig) this surface
    /// config maps to (the engine value used when a format is actually invoked,
    /// Wave 2). The engine clamps the indent width too, so the two stay in sync.
    #[must_use]
    pub fn to_engine_config(&self) -> ronin_core::FormatConfig {
        let policy = match self.blank_line_policy {
            BlankLinePolicy::Collapse => ronin_core::BlankLinePolicy::Collapse,
            BlankLinePolicy::Preserve => ronin_core::BlankLinePolicy::Preserve,
        };
        ronin_core::FormatConfig::new(self.effective_indent_width(), policy)
    }
}

/// Bounded undo/redo history configuration persisted with the app settings
/// (E007 TR-024/TR-027).
///
/// Mirrors the `FormattingConfig` robustness pattern: an absent or corrupt
/// on-disk value falls back to the defaults and an out-of-range value is clamped
/// (on load via [`AppSettings::load_from`] and via the effective accessors), so a
/// hand-edited settings file can NEVER disable the undo bound or coalescing — it
/// can only land inside the sane range. Defaults mirror `ronin_core::undo`'s
/// constants (200 units / 64 MiB / 500 ms) so the editor's stack and the
/// persisted config never diverge (TR-024/TR-026/TR-027).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UndoConfig {
    /// Maximum number of undo units retained (default 200, clamped to
    /// `1..=10_000`). Never 0 — undo is never disabled (TR-024).
    pub history_count_cap: usize,
    /// Maximum total retained snapshot byte-size (default 64 MiB, clamped to
    /// 1 MiB..=1 GiB) — bounds memory on large files (TR-024).
    pub history_byte_cap: usize,
    /// Coalesce window in milliseconds: edits closer than this fold into one undo
    /// unit (default 500 ms, clamped to 50 ms..=5 s) (TR-027).
    pub coalesce_window_ms: u64,
}

impl Default for UndoConfig {
    fn default() -> Self {
        Self {
            history_count_cap: DEFAULT_UNDO_COUNT_CAP,
            history_byte_cap: DEFAULT_UNDO_BYTE_CAP,
            coalesce_window_ms: DEFAULT_COALESCE_WINDOW_MS,
        }
    }
}

impl UndoConfig {
    /// The undo unit-count cap clamped to `1..=10_000` (TR-024). Read the cap
    /// through this rather than the raw field so a corrupt value can never
    /// disable or unbound the history.
    #[must_use]
    pub fn effective_history_count_cap(&self) -> usize {
        self.history_count_cap
            .clamp(MIN_UNDO_COUNT_CAP, MAX_UNDO_COUNT_CAP)
    }

    /// The undo byte-size cap clamped to 1 MiB..=1 GiB (TR-024).
    #[must_use]
    pub fn effective_history_byte_cap(&self) -> usize {
        self.history_byte_cap
            .clamp(MIN_UNDO_BYTE_CAP, MAX_UNDO_BYTE_CAP)
    }

    /// The coalesce window clamped to 50 ms..=5 s (TR-027).
    #[must_use]
    pub fn effective_coalesce_window_ms(&self) -> u64 {
        self.coalesce_window_ms
            .clamp(MIN_COALESCE_WINDOW_MS, MAX_COALESCE_WINDOW_MS)
    }

    /// The coalesce window as a [`Duration`](std::time::Duration), clamped to the
    /// sane range — the value to hand to `ronin_core::UndoStack` (TR-027).
    #[must_use]
    pub fn effective_coalesce_window(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.effective_coalesce_window_ms())
    }

    /// Build the `ronin_core::UndoCap` this config maps to, taken through the
    /// effective (clamped) accessors so the editor's stack is always bounded
    /// (TR-024). `ronin_core::UndoCap::new` itself reverts a zero field to default,
    /// so the bound is doubly guarded.
    #[must_use]
    pub fn to_engine_cap(&self) -> ronin_core::UndoCap {
        ronin_core::UndoCap::new(
            self.effective_history_count_cap(),
            self.effective_history_byte_cap(),
        )
    }

    /// Set the undo unit-count cap, clamping to `1..=10_000` (TR-024).
    pub fn set_history_count_cap(&mut self, count: usize) {
        self.history_count_cap = count.clamp(MIN_UNDO_COUNT_CAP, MAX_UNDO_COUNT_CAP);
    }

    /// Set the undo byte-size cap, clamping to 1 MiB..=1 GiB (TR-024).
    pub fn set_history_byte_cap(&mut self, bytes: usize) {
        self.history_byte_cap = bytes.clamp(MIN_UNDO_BYTE_CAP, MAX_UNDO_BYTE_CAP);
    }

    /// Set the coalesce window (ms), clamping to 50 ms..=5 s (TR-027).
    pub fn set_coalesce_window_ms(&mut self, ms: u64) {
        self.coalesce_window_ms = ms.clamp(MIN_COALESCE_WINDOW_MS, MAX_COALESCE_WINDOW_MS);
    }
}

/// Autosave (crash-recovery sidecar) debounce configuration persisted with the
/// app settings (E007 TR-025).
///
/// Same robustness contract as [`UndoConfig`]: corrupt / out-of-range values
/// clamp to the sane range on load and via the effective accessors, so autosave
/// can NEVER be disabled by a tiny / zero / hand-edited value (TR-026). Autosave
/// fires on whichever trigger binds first — idle debounce OR edit count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutosaveConfig {
    /// Idle debounce in milliseconds before a dirty+changed buffer is autosaved
    /// to its recovery sidecar (default 4000 ms, clamped to 250 ms..=300 s)
    /// (TR-025).
    pub idle_debounce_ms: u64,
    /// Edit-count trigger: autosave after this many edits since the last sidecar
    /// write, whichever fires first with the idle debounce (default 50, clamped
    /// to `1..=10_000`) (TR-025).
    pub edit_count_trigger: u32,
}

impl Default for AutosaveConfig {
    fn default() -> Self {
        Self {
            idle_debounce_ms: DEFAULT_AUTOSAVE_IDLE_MS,
            edit_count_trigger: DEFAULT_AUTOSAVE_EDIT_COUNT,
        }
    }
}

impl AutosaveConfig {
    /// The idle debounce clamped to 250 ms..=300 s (TR-025). Read through this so
    /// a corrupt / zero value can never disable autosave.
    #[must_use]
    pub fn effective_idle_debounce_ms(&self) -> u64 {
        self.idle_debounce_ms
            .clamp(MIN_AUTOSAVE_IDLE_MS, MAX_AUTOSAVE_IDLE_MS)
    }

    /// The idle debounce as a [`Duration`](std::time::Duration), clamped to the
    /// sane range — the value the autosave timer compares elapsed idle against.
    #[must_use]
    pub fn effective_idle_debounce(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.effective_idle_debounce_ms())
    }

    /// The edit-count trigger clamped to `1..=10_000` (TR-025).
    #[must_use]
    pub fn effective_edit_count_trigger(&self) -> u32 {
        self.edit_count_trigger
            .clamp(MIN_AUTOSAVE_EDIT_COUNT, MAX_AUTOSAVE_EDIT_COUNT)
    }

    /// Set the idle debounce (ms), clamping to 250 ms..=300 s (TR-025).
    pub fn set_idle_debounce_ms(&mut self, ms: u64) {
        self.idle_debounce_ms = ms.clamp(MIN_AUTOSAVE_IDLE_MS, MAX_AUTOSAVE_IDLE_MS);
    }

    /// Set the edit-count trigger, clamping to `1..=10_000` (TR-025).
    pub fn set_edit_count_trigger(&mut self, count: u32) {
        self.edit_count_trigger = count.clamp(MIN_AUTOSAVE_EDIT_COUNT, MAX_AUTOSAVE_EDIT_COUNT);
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
    /// Bounded undo/redo history configuration (E007 TR-024/TR-027): history
    /// count + byte caps and the coalesce window. Defaults on absent / corrupt;
    /// clamped on load so the bound and coalescing are never disabled (TR-026).
    pub undo: UndoConfig,
    /// Autosave (recovery sidecar) debounce configuration (E007 TR-025): idle
    /// debounce and edit-count trigger. Defaults on absent / corrupt; clamped on
    /// load so autosave is never disabled (TR-026).
    pub autosave: AutosaveConfig,
    /// RON⇄JSON conversion preferences (E010 NEW-CONFIG / FR-008): the output
    /// format default (JSONC vs strict), JSON indent, and strict-mode comment
    /// carrier. Defaults on absent / corrupt (JSONC + default indent); the indent
    /// is clamped on load. The only on-disk artifact E010 adds.
    pub conversion: ConversionSettings,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            window_geometry: None,
            preferences: BTreeMap::new(),
            large_file_threshold: DEFAULT_LARGE_FILE_THRESHOLD,
            formatting: FormattingConfig::default(),
            undo: UndoConfig::default(),
            autosave: AutosaveConfig::default(),
            conversion: ConversionSettings::default(),
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
        // Enforce the E007 NEW-CONFIG clamps on load (TR-026): a corrupt /
        // out-of-range / zero undo or autosave value is pulled back into its sane
        // range so the bound, the debounce, and coalescing are NEVER disabled —
        // it can only land at a valid in-range value, never "off"/unbounded.
        settings.undo.history_count_cap = settings.undo.effective_history_count_cap();
        settings.undo.history_byte_cap = settings.undo.effective_history_byte_cap();
        settings.undo.coalesce_window_ms = settings.undo.effective_coalesce_window_ms();
        settings.autosave.idle_debounce_ms = settings.autosave.effective_idle_debounce_ms();
        settings.autosave.edit_count_trigger = settings.autosave.effective_edit_count_trigger();
        // Enforce the E010 NEW-CONFIG clamp on load (data-model §ConversionSettings):
        // a corrupt / out-of-range / hand-edited JSON indent is pulled back into the
        // sane `0..=16` range so the effective conversion config is always usable.
        settings.conversion.json_indent = settings.conversion.effective_json_indent();
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

// ===========================================================================
// E009 registry-binding project config wiring (T019 — FR-010)
// ===========================================================================
//
// The Bevy registry binding (glob → registry-export-path) is persisted exactly
// like E006's `BindingConfig`: a single small project-scoped local file under the
// project's `.ronin/` dir (NOT the OS config dir, which holds only `AppSettings`).
// `RegistryBindingConfig` owns its own `load_from`/`save_to`/`project_config_path`
// (mirroring `binding::BindingConfig`); these thin wrappers route the project-open
// load and the on-edit save through that exact mechanism so there is no new
// storage system — only the one small local file.

use crate::bevy::mode::{Mode, RegistryBindingConfig, RegistryBindingRule};

/// Load the project's [`RegistryBindingConfig`] when a project opens, defaulting to
/// an **empty** config when the file is absent or corrupt (T019, FR-010).
///
/// Reads `<project_root>/.ronin/bevy-registries.json` via
/// [`RegistryBindingConfig::load_from`] — the same project-scoped local file
/// mechanism E006 uses for `bindings.json`. Never panics and never errors: a
/// missing/corrupt/unknown-version file degrades to
/// [`RegistryBindingConfig::default`] (zero rules ⇒ auto-detect mode, no registry)
/// rather than blocking the project (FR-010, SC-002, project-instructions §I).
#[must_use]
pub fn load_registry_binding_config(project_root: &Path) -> RegistryBindingConfig {
    let path = RegistryBindingConfig::project_config_path(project_root);
    RegistryBindingConfig::load_from(&path)
}

/// Persist the project's [`RegistryBindingConfig`] on edit as the one small
/// project-scoped local file (T019, FR-010).
///
/// Writes `<project_root>/.ronin/bevy-registries.json` via
/// [`RegistryBindingConfig::save_to`] (pretty JSON, auto-creating `.ronin/`) — the
/// same E006-style mechanism, no new storage. Call this after any edit to the
/// registry-binding rules / project default mode.
///
/// # Errors
///
/// Returns an [`std::io::Error`] if the `.ronin/` directory cannot be created or
/// the file cannot be written.
pub fn save_registry_binding_config(
    config: &RegistryBindingConfig,
    project_root: &Path,
) -> std::io::Result<()> {
    let path = RegistryBindingConfig::project_config_path(project_root);
    config.save_to(&path)
}

/// Editable form state backing the registry-binding-config window's add/edit
/// controls (E009 T019 — FR-010), mirroring [`BindingFormDraft`].
///
/// Pure UI-input state — **not** persisted (only [`RegistryBindingConfig`]
/// persists). On commit it validates into a [`RegistryBindingRule`]: a blank
/// `pattern` or `registry_export_path` is rejected (the commit button is disabled)
/// so a blank rule is never created. `mode` and `expected_bevy_version` are
/// optional.
#[derive(Debug, Clone, Default)]
pub struct RegistryBindingFormDraft {
    /// The glob pattern for the rule.
    pub pattern: String,
    /// Comma-separated exclude globs for the rule.
    pub exclude: String,
    /// The registry-export path (read-only data; may be absolute / out-of-tree).
    pub registry_export_path: String,
    /// An optional per-pattern mode hint (`None` ⇒ no hint).
    pub mode: Option<Mode>,
    /// An optional expected Bevy version (blank ⇒ no staleness advisory).
    pub expected_bevy_version: String,
}

impl RegistryBindingFormDraft {
    /// Build a [`RegistryBindingRule`] from this draft, or `None` when the
    /// `pattern` or `registry_export_path` are blank (FR-010).
    ///
    /// `exclude` is split on commas (empty entries dropped; an empty list ⇒
    /// `None`); a blank `expected_bevy_version` ⇒ `None`.
    #[must_use]
    pub fn to_rule(&self) -> Option<RegistryBindingRule> {
        let pattern = self.pattern.trim();
        let export = self.registry_export_path.trim();
        if pattern.is_empty() || export.is_empty() {
            return None;
        }
        let excludes: Vec<String> = self
            .exclude
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        let expected = self.expected_bevy_version.trim();
        Some(RegistryBindingRule {
            pattern: pattern.to_string(),
            exclude: if excludes.is_empty() {
                None
            } else {
                Some(excludes)
            },
            registry_export_path: PathBuf::from(export),
            mode: self.mode,
            expected_bevy_version: if expected.is_empty() {
                None
            } else {
                Some(expected.to_string())
            },
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
            ronin_core::BlankLinePolicy::Preserve
        );
    }

    // --- E007 NEW-CONFIG: undo + autosave (TR-024/025/026/027) -------------

    #[test]
    fn undo_config_defaults() {
        let u = UndoConfig::default();
        assert_eq!(u.history_count_cap, 200);
        assert_eq!(u.history_byte_cap, 64 * 1024 * 1024);
        assert_eq!(u.coalesce_window_ms, 500);
        // The default app settings embed the default undo config.
        assert_eq!(AppSettings::default().undo, UndoConfig::default());
        // Defaults mirror the ronin_core::undo constants (never diverge).
        assert_eq!(u.history_count_cap, ronin_core::undo::DEFAULT_UNDO_COUNT_CAP);
        assert_eq!(u.history_byte_cap, ronin_core::undo::DEFAULT_UNDO_BYTE_CAP);
        assert_eq!(
            u.effective_coalesce_window(),
            ronin_core::undo::DEFAULT_COALESCE_WINDOW
        );
    }

    #[test]
    fn autosave_config_defaults() {
        let a = AutosaveConfig::default();
        assert_eq!(a.idle_debounce_ms, 4_000);
        assert_eq!(a.edit_count_trigger, 50);
        assert_eq!(AppSettings::default().autosave, AutosaveConfig::default());
    }

    #[test]
    fn undo_config_clamps_tiny_and_huge_to_range() {
        let mut u = UndoConfig::default();
        // Zero/below-min never disables the bound — clamps up to the minimum.
        u.set_history_count_cap(0);
        assert_eq!(u.history_count_cap, MIN_UNDO_COUNT_CAP);
        u.set_history_byte_cap(0);
        assert_eq!(u.history_byte_cap, MIN_UNDO_BYTE_CAP);
        u.set_coalesce_window_ms(0);
        assert_eq!(u.coalesce_window_ms, MIN_COALESCE_WINDOW_MS);
        // Absurdly large clamps down to the maximum.
        u.set_history_count_cap(usize::MAX);
        assert_eq!(u.history_count_cap, MAX_UNDO_COUNT_CAP);
        u.set_history_byte_cap(usize::MAX);
        assert_eq!(u.history_byte_cap, MAX_UNDO_BYTE_CAP);
        u.set_coalesce_window_ms(u64::MAX);
        assert_eq!(u.coalesce_window_ms, MAX_COALESCE_WINDOW_MS);
        // A normal value is preserved.
        u.set_history_count_cap(500);
        assert_eq!(u.history_count_cap, 500);
        assert_eq!(u.effective_history_count_cap(), 500);
    }

    #[test]
    fn autosave_config_clamps_tiny_and_huge_to_range() {
        let mut a = AutosaveConfig::default();
        a.set_idle_debounce_ms(0);
        assert_eq!(a.idle_debounce_ms, MIN_AUTOSAVE_IDLE_MS);
        a.set_edit_count_trigger(0);
        assert_eq!(a.edit_count_trigger, MIN_AUTOSAVE_EDIT_COUNT);
        a.set_idle_debounce_ms(u64::MAX);
        assert_eq!(a.idle_debounce_ms, MAX_AUTOSAVE_IDLE_MS);
        a.set_edit_count_trigger(u32::MAX);
        assert_eq!(a.edit_count_trigger, MAX_AUTOSAVE_EDIT_COUNT);
        a.set_idle_debounce_ms(10_000);
        assert_eq!(a.idle_debounce_ms, 10_000);
        assert_eq!(a.effective_idle_debounce_ms(), 10_000);
    }

    #[test]
    fn undo_config_maps_to_engine_cap() {
        let mut u = UndoConfig::default();
        u.set_history_count_cap(42);
        u.set_history_byte_cap(2 * 1024 * 1024);
        let cap = u.to_engine_cap();
        assert_eq!(cap.max_count, 42);
        assert_eq!(cap.max_bytes, 2 * 1024 * 1024);
    }

    #[test]
    fn load_from_clamps_out_of_range_undo_and_autosave() {
        let dir = std::env::temp_dir().join("ronin_settings_e007_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clamp.json");
        // A hand-edited file demanding disabled/out-of-range persistence knobs:
        // 0 undo cap, 0 byte cap, 0 coalesce window, 0 autosave idle, 0 edits.
        std::fs::write(
            &path,
            br#"{"undo":{"history_count_cap":0,"history_byte_cap":0,"coalesce_window_ms":0},
                "autosave":{"idle_debounce_ms":0,"edit_count_trigger":0}}"#,
        )
        .unwrap();
        let loaded = AppSettings::load_from(&path);
        // Never disabled: every knob is pulled up to its minimum, not left at 0.
        assert_eq!(loaded.undo.history_count_cap, MIN_UNDO_COUNT_CAP);
        assert_eq!(loaded.undo.history_byte_cap, MIN_UNDO_BYTE_CAP);
        assert_eq!(loaded.undo.coalesce_window_ms, MIN_COALESCE_WINDOW_MS);
        assert_eq!(loaded.autosave.idle_debounce_ms, MIN_AUTOSAVE_IDLE_MS);
        assert_eq!(loaded.autosave.edit_count_trigger, MIN_AUTOSAVE_EDIT_COUNT);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_from_clamps_huge_undo_and_autosave() {
        let dir = std::env::temp_dir().join("ronin_settings_e007_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("huge.json");
        std::fs::write(
            &path,
            br#"{"undo":{"history_count_cap":1000000,"history_byte_cap":9999999999999,"coalesce_window_ms":999999},
                "autosave":{"idle_debounce_ms":999999999,"edit_count_trigger":999999}}"#,
        )
        .unwrap();
        let loaded = AppSettings::load_from(&path);
        assert_eq!(loaded.undo.history_count_cap, MAX_UNDO_COUNT_CAP);
        assert_eq!(loaded.undo.history_byte_cap, MAX_UNDO_BYTE_CAP);
        assert_eq!(loaded.undo.coalesce_window_ms, MAX_COALESCE_WINDOW_MS);
        assert_eq!(loaded.autosave.idle_debounce_ms, MAX_AUTOSAVE_IDLE_MS);
        assert_eq!(loaded.autosave.edit_count_trigger, MAX_AUTOSAVE_EDIT_COUNT);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_from_uses_undo_autosave_defaults_when_absent() {
        // An older settings file with no `undo`/`autosave` block loads with the
        // defaults (serde `default`), never a parse error (TR-026).
        let dir = std::env::temp_dir().join("ronin_settings_e007_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("absent.json");
        std::fs::write(&path, br#"{"large_file_threshold": 1048576}"#).unwrap();
        let loaded = AppSettings::load_from(&path);
        assert_eq!(loaded.undo, UndoConfig::default());
        assert_eq!(loaded.autosave, AutosaveConfig::default());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn undo_autosave_round_trip_through_save_and_load() {
        let dir = std::env::temp_dir().join("ronin_settings_e007_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("roundtrip.json");
        let mut s = AppSettings::default();
        s.undo.set_history_count_cap(123);
        s.undo.set_history_byte_cap(8 * 1024 * 1024);
        s.undo.set_coalesce_window_ms(750);
        s.autosave.set_idle_debounce_ms(6_000);
        s.autosave.set_edit_count_trigger(25);
        s.save_to(&path).unwrap();
        let loaded = AppSettings::load_from(&path);
        assert_eq!(loaded.undo, s.undo);
        assert_eq!(loaded.autosave, s.autosave);
        let _ = std::fs::remove_file(&path);
    }

    // --- E009 T019: registry-binding project config wiring (FR-010) --------

    #[test]
    fn registry_binding_config_absent_loads_empty_default() {
        // A project with no `.ronin/bevy-registries.json` loads an empty config
        // (auto-detect mode, no registry) — never a crash (FR-010, SC-002).
        let root = std::env::temp_dir().join("ronin_regbind_absent");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let loaded = load_registry_binding_config(&root);
        assert_eq!(loaded, RegistryBindingConfig::default());
        assert!(loaded.rules.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn registry_binding_config_round_trips_through_project_file() {
        // Save → load via the project-scoped local file (the one small artifact).
        let root = std::env::temp_dir().join("ronin_regbind_roundtrip");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let config = RegistryBindingConfig {
            rules: vec![RegistryBindingRule {
                pattern: "levels/*.scn.ron".to_string(),
                exclude: None,
                registry_export_path: PathBuf::from("registries/world.json"),
                mode: Some(Mode::Bevy),
                expected_bevy_version: Some("0.16.0".to_string()),
            }],
            project_default_mode: None,
            version: crate::bevy::mode::REGISTRY_BINDING_CONFIG_VERSION,
        };
        save_registry_binding_config(&config, &root).unwrap();
        // The file lands under the project-local `.ronin/`, not the OS config dir.
        assert!(root.join(".ronin").join("bevy-registries.json").exists());
        let loaded = load_registry_binding_config(&root);
        assert_eq!(loaded, config);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn registry_binding_form_draft_rejects_blank_and_builds_rule() {
        let mut draft = RegistryBindingFormDraft::default();
        assert!(draft.to_rule().is_none(), "blank draft ⇒ no rule");
        draft.pattern = "  ".to_string();
        draft.registry_export_path = "r.json".to_string();
        assert!(draft.to_rule().is_none(), "blank pattern ⇒ no rule");
        draft.pattern = "**/*.scn.ron".to_string();
        draft.registry_export_path = "  ".to_string();
        assert!(draft.to_rule().is_none(), "blank export path ⇒ no rule");

        draft.registry_export_path = "registries/r.json".to_string();
        draft.exclude = "wip/** , , tmp/**".to_string();
        draft.mode = Some(Mode::Bevy);
        draft.expected_bevy_version = " 0.16.0 ".to_string();
        let rule = draft.to_rule().expect("a valid rule");
        assert_eq!(rule.pattern, "**/*.scn.ron");
        assert_eq!(
            rule.exclude,
            Some(vec!["wip/**".to_string(), "tmp/**".to_string()])
        );
        assert_eq!(
            rule.registry_export_path,
            PathBuf::from("registries/r.json")
        );
        assert_eq!(rule.mode, Some(Mode::Bevy));
        assert_eq!(rule.expected_bevy_version, Some("0.16.0".to_string()));
    }

    // --- E010 NEW-CONFIG: ConversionSettings (FR-008) ----------------------

    #[test]
    fn conversion_settings_defaults() {
        let c = ConversionSettings::default();
        assert_eq!(c.default_format, JsonFormat::Jsonc);
        assert!(c.default_format.is_jsonc());
        assert_eq!(c.json_indent, 2);
        assert_eq!(
            c.strict_default_comment_carrier,
            StrictCommentCarrier::Sidecar
        );
        // The default app settings embed the default conversion config — JSONC.
        assert_eq!(
            AppSettings::default().conversion,
            ConversionSettings::default()
        );
    }

    #[test]
    fn conversion_settings_clamp_json_indent() {
        let mut c = ConversionSettings::default();
        // Zero is permitted (compact output) — it is in range, not clamped up.
        c.set_json_indent(0);
        assert_eq!(c.json_indent, ConversionSettings::min_json_indent());
        assert_eq!(c.json_indent, 0);
        // Absurdly large clamps down to the maximum.
        c.set_json_indent(9999);
        assert_eq!(c.json_indent, ConversionSettings::max_json_indent());
        assert_eq!(c.json_indent, 16);
        // A normal value is preserved.
        c.set_json_indent(4);
        assert_eq!(c.json_indent, 4);
        assert_eq!(c.effective_json_indent(), 4);
    }

    #[test]
    fn load_from_uses_conversion_defaults_when_absent() {
        // An older settings file with no `conversion` block loads with the
        // defaults (serde `default`) — JSONC + default indent, never a parse
        // error (data-model §ConversionSettings, project-instructions §I).
        let dir = std::env::temp_dir().join("ronin_settings_e010_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("absent.json");
        std::fs::write(&path, br#"{"large_file_threshold": 1048576}"#).unwrap();
        let loaded = AppSettings::load_from(&path);
        assert_eq!(loaded.conversion, ConversionSettings::default());
        assert_eq!(loaded.conversion.default_format, JsonFormat::Jsonc);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_from_clamps_absurd_json_indent() {
        // A hand-edited file demanding an absurd indent is pulled into range on
        // load — never an unusable conversion config (data-model §ConversionSettings).
        let dir = std::env::temp_dir().join("ronin_settings_e010_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("indent.json");
        std::fs::write(&path, br#"{"conversion": {"json_indent": 9999}}"#).unwrap();
        let loaded = AppSettings::load_from(&path);
        assert_eq!(
            loaded.conversion.json_indent,
            ConversionSettings::max_json_indent()
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_from_recovers_corrupt_conversion_block_to_defaults() {
        // A wholly corrupt settings file (not valid JSON) recovers to defaults —
        // JSONC + default indent — never a crash (data-model §ConversionSettings).
        let dir = std::env::temp_dir().join("ronin_settings_e010_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("corrupt.json");
        std::fs::write(&path, b"{ this is not valid json").unwrap();
        let loaded = AppSettings::load_from(&path);
        assert_eq!(loaded.conversion, ConversionSettings::default());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn conversion_settings_round_trip_through_save_and_load() {
        let dir = std::env::temp_dir().join("ronin_settings_e010_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("roundtrip.json");
        let mut s = AppSettings::default();
        s.conversion.default_format = JsonFormat::StrictJson;
        s.conversion.set_json_indent(4);
        s.conversion.strict_default_comment_carrier = StrictCommentCarrier::PureNoComments;
        s.save_to(&path).unwrap();
        let loaded = AppSettings::load_from(&path);
        assert_eq!(loaded.conversion, s.conversion);
        assert_eq!(loaded.conversion.default_format, JsonFormat::StrictJson);
        assert_eq!(loaded.conversion.json_indent, 4);
        assert_eq!(
            loaded.conversion.strict_default_comment_carrier,
            StrictCommentCarrier::PureNoComments
        );
        let _ = std::fs::remove_file(&path);
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
