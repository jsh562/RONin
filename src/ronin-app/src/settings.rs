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
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            window_geometry: None,
            preferences: BTreeMap::new(),
            large_file_threshold: DEFAULT_LARGE_FILE_THRESHOLD,
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
