//! `ronin-app` — RONin desktop application shell (library surface).
//!
//! This crate is built as **both** a library and a binary. The library exposes
//! the editor-shell building blocks so integration tests under `tests/` (which
//! can only `use` a library crate) can exercise the real public API, while the
//! thin [`main`](../main.rs) binary will wire them into an `eframe` app in a
//! later wave.
//!
//! # Wave 1 scope (E003 "Desktop Editor Shell")
//!
//! Foundational, UI-agnostic-where-possible modules:
//!
//! * [`document`] — the editor document model and byte-fidelity profile (FR-007,
//!   FR-020).
//! * [`reparse`] — off-thread parsing against `ron-core` with generation-keyed
//!   staleness (FR-006).
//! * [`diagnostics_map`] — byte→char/line-column diagnostic projection (FR-008).
//! * [`fileio`] — UTF-8-validated file open (FR-018).
//! * [`settings`] — persisted app settings in the OS config dir (FR-016).
//! * [`logging`] — non-blocking rolling-file tracing setup (FR-014).
//! * [`panels`] — the diagnostics panel region plus reserved layout seams.
//!
//! Wave 2 adds the running shell: [`app`] (the `eframe::App` shell, FR-003,
//! FR-006) and [`editor_view`] (the highlight model + the multiline editor
//! widget with a line-number gutter, FR-004/FR-005/FR-019). Wave 3 adds
//! [`problems_panel`]. Wave 4 adds [`workspace`] — the multi-tab
//! [`EditorWorkspace`](workspace::EditorWorkspace) (open documents, the active
//! tab, the recently-closed stack) backing the tab bar, focus-existing-on-open,
//! and sequential multi-dirty close/quit (FR-012/FR-013/FR-022/FR-025/FR-026).

pub mod app;
pub mod bevy;
pub mod binding;
/// Shared byte→char offset resolution used by the highlight + structural surfaces.
mod byte_to_char;
pub mod completion;
pub mod diagnostics_map;
pub mod document;
pub mod editor_view;
pub mod fileio;
pub mod interop;
pub mod logging;
pub mod panels;
pub mod problems_panel;
pub mod recovery;
pub mod reparse;
pub mod settings;
pub mod snippets;
pub mod structural;
pub mod type_acquire;
pub mod workspace;
