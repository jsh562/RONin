//! The single, shared **type-indicator** system for every structural surface
//! (E014 — visual source-of-truth consolidation).
//!
//! Before this module there were TWO inconsistent indicator systems: the tree
//! view's per-[`TreeNodeKind`](super::tree::TreeNodeKind) geometric glyphs
//! (`kind_icon` / `kind_color` / `kind_word`) and the table view's per-cell
//! punctuation glyphs rendered `.small()` (`scalar_type_icon` / `scalar_type_color`
//! / `scalar_type_word`) plus inline nested markers (`▸` / `▦`). The SAME concept
//! got DIFFERENT glyphs across views (a list was `▤` in the tree but `▦` in a table
//! cell; a tuple was `◇` in the tree but `▸` in a cell), and `severity_color` was
//! duplicated in both files.
//!
//! [`TypeIndicator`] replaces all of it: ONE enum, ONE glyph per concept, ONE
//! theme-aware color palette, rendered at ONE consistent size (never `.small()`),
//! `.strong()`, color-coded. Every call site routes through a converter
//! ([`from_tree_kind`] / [`from_scalar_class`] / [`from_severity`]) so the tree, the
//! table, and the section boundary all draw the same glyph for the same concept.
//!
//! # Canonical glyph set
//!
//! Each glyph is covered by the bundled Noto fallback faces (asserted against the
//! live font chain in `tests/font_install.rs`):
//!
//! | concept | glyph | codepoint |
//! |---------|-------|-----------|
//! | Struct  | ▢ | U+25A2 |
//! | Map     | ▦ | U+25A6 |
//! | List    | ▤ | U+25A4 |
//! | Tuple   | ◇ | U+25C7 |
//! | Enum    | ◈ | U+25C8 |
//! | Unit    | ∅ | U+2205 |
//! | Integer | # | U+0023 |
//! | Float   | ≈ | U+2248 |
//! | Str     | " | U+0022 |
//! | Char    | ' | U+0027 |
//! | Bool    | ☑ | U+2611 |
//! | Scalar  | • | U+2022 |
//! | Error   | ✖ | U+2716 |
//! | Warning | ⚠ | U+26A0 |

use egui::{Color32, RichText, Ui};

use ron_core::Severity;

use super::classifier::ScalarClass;
use super::tree::TreeNodeKind;

/// The single, view-agnostic type indicator a structural surface renders for a
/// value's concept (E014). One [`glyph`](Self::glyph) + one [`color`](Self::color) +
/// one [`word`](Self::word) per concept, shared by the tree, the table, and the
/// section-boundary badges so the SAME concept always reads identically.
///
/// `#[non_exhaustive]` so a future concept can be added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TypeIndicator {
    /// A named or anonymous struct.
    Struct,
    /// A map.
    Map,
    /// A list / sequence.
    List,
    /// A positional tuple.
    Tuple,
    /// An enum variant.
    Enum,
    /// The unit value `()`.
    Unit,
    /// An integer scalar.
    Integer,
    /// A floating-point scalar.
    Float,
    /// A string scalar.
    Str,
    /// A character scalar.
    Char,
    /// A boolean scalar.
    Bool,
    /// A generic / unclassified scalar leaf.
    Scalar,
    /// An error diagnostic (or an unparseable region).
    Error,
    /// A warning diagnostic.
    Warning,
}

/// The single rendered size for **every** indicator glyph (E014): the indicator is
/// rendered at one consistent size across all views — never `.small()` — so a list
/// reads the same in the tree as in a table cell. Equal to the body text size; the
/// `.strong()` weight is what distinguishes the glyph, not a size change.
const INDICATOR_SIZE: f32 = 14.0;

/// The fixed leading-slot width for [`TypeIndicator::show`] (E014): icons in a column
/// align vertically because each is drawn into a slot of this width.
const SLOT_WIDTH: f32 = 18.0;

impl TypeIndicator {
    /// Every [`TypeIndicator`] variant, grouped for the always-visible legend strip
    /// (E015): containers first (Struct, Map, List, Tuple, Enum, Unit), then scalars
    /// (Integer, Float, Str, Char, Bool, Scalar), then status (Error, Warning). The
    /// legend renders each glyph (glyph-only, name on hover) so the SAME glyphs the
    /// tree + table paint carry a discoverable key directly above the views.
    ///
    /// The group boundaries are at indices [`CONTAINER_COUNT`] and
    /// [`CONTAINER_COUNT`]` + `[`SCALAR_COUNT`] so the strip can insert a small gap
    /// between the three groups without re-listing the variants.
    pub const ALL: &'static [TypeIndicator] = &[
        // Containers.
        Self::Struct,
        Self::Map,
        Self::List,
        Self::Tuple,
        Self::Enum,
        Self::Unit,
        // Scalars.
        Self::Integer,
        Self::Float,
        Self::Str,
        Self::Char,
        Self::Bool,
        Self::Scalar,
        // Status.
        Self::Error,
        Self::Warning,
    ];

    /// The number of leading [`ALL`](Self::ALL) entries that are containers (the first
    /// legend group): Struct, Map, List, Tuple, Enum, Unit.
    pub const CONTAINER_COUNT: usize = 6;

    /// The number of [`ALL`](Self::ALL) entries that are scalars (the second legend
    /// group, after the containers): Integer, Float, Str, Char, Bool, Scalar.
    pub const SCALAR_COUNT: usize = 6;

    /// The canonical glyph for this concept (E014) — the SAME glyph in every view.
    #[must_use]
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Struct => "\u{25A2}",  // ▢ white square with rounded corners
            Self::Map => "\u{25A6}",     // ▦ square with horizontal+vertical fill
            Self::List => "\u{25A4}",    // ▤ square with horizontal fill
            Self::Tuple => "\u{25C7}",   // ◇ white diamond
            Self::Enum => "\u{25C8}",    // ◈ white diamond containing small black diamond
            Self::Unit => "\u{2205}",    // ∅ empty set (the unit value)
            Self::Integer => "\u{0023}", // # number sign
            Self::Float => "\u{2248}",   // ≈ almost-equal-to (approximate / real)
            Self::Str => "\u{0022}",     // " quotation mark
            Self::Char => "\u{0027}",    // ' apostrophe
            Self::Bool => "\u{2611}",    // ☑ ballot box with check
            Self::Scalar => "\u{2022}",  // • bullet
            Self::Error => "\u{2716}",   // ✖ heavy multiplication x
            Self::Warning => "\u{26A0}", // ⚠ warning sign
        }
    }

    /// The theme-aware color for this concept (E014), consolidating the former
    /// `tree::kind_color` + `table::scalar_type_color` + duplicated `severity_color`
    /// palettes into one source of truth. Each concept keeps its prior dark/light
    /// pair so the consolidation is visually neutral.
    #[must_use]
    pub fn color(self, ui: &Ui) -> Color32 {
        let dark = ui.visuals().dark_mode;
        match self {
            Self::Struct => {
                if dark {
                    Color32::from_rgb(0x6C, 0xB6, 0xFF)
                } else {
                    Color32::from_rgb(0x1F, 0x5F, 0xBF)
                }
            }
            Self::Map => {
                if dark {
                    Color32::from_rgb(0x8B, 0xD5, 0x9E)
                } else {
                    Color32::from_rgb(0x2E, 0x7D, 0x46)
                }
            }
            Self::List => {
                if dark {
                    Color32::from_rgb(0x9C, 0xD0, 0x6C)
                } else {
                    Color32::from_rgb(0x4F, 0x7A, 0x1F)
                }
            }
            Self::Tuple => {
                if dark {
                    Color32::from_rgb(0xD8, 0xB4, 0xFE)
                } else {
                    Color32::from_rgb(0x7A, 0x40, 0xBF)
                }
            }
            Self::Enum => {
                if dark {
                    Color32::from_rgb(0xF0, 0xB8, 0x6C)
                } else {
                    Color32::from_rgb(0xB5, 0x6A, 0x10)
                }
            }
            // Integers + floats share a numeric family (blue-greens) but stay distinct.
            Self::Integer => {
                if dark {
                    Color32::from_rgb(0x6C, 0xB6, 0xFF)
                } else {
                    Color32::from_rgb(0x1F, 0x5F, 0xBF)
                }
            }
            Self::Float => {
                if dark {
                    Color32::from_rgb(0x5F, 0xD0, 0xD8)
                } else {
                    Color32::from_rgb(0x16, 0x6E, 0x77)
                }
            }
            Self::Str => {
                if dark {
                    Color32::from_rgb(0xC8, 0xA0, 0x6C)
                } else {
                    Color32::from_rgb(0x8A, 0x53, 0x10)
                }
            }
            Self::Char => {
                if dark {
                    Color32::from_rgb(0xE0, 0xB0, 0x80)
                } else {
                    Color32::from_rgb(0xA0, 0x64, 0x1A)
                }
            }
            Self::Bool => {
                if dark {
                    Color32::from_rgb(0xD8, 0xB4, 0xFE)
                } else {
                    Color32::from_rgb(0x7A, 0x40, 0xBF)
                }
            }
            // The unit value reads weakly (no strong type cue).
            Self::Unit => ui.visuals().weak_text_color(),
            // A generic / unclassified scalar reads as plain text (no false cue).
            Self::Scalar => ui.visuals().text_color(),
            Self::Error => {
                if dark {
                    Color32::from_rgb(0xF4, 0x47, 0x47)
                } else {
                    Color32::from_rgb(0xCD, 0x31, 0x31)
                }
            }
            Self::Warning => {
                if dark {
                    Color32::from_rgb(0xCC, 0xA7, 0x00)
                } else {
                    Color32::from_rgb(0xBF, 0x83, 0x03)
                }
            }
        }
    }

    /// A short, stable, user-facing word for this concept (E014) — the indicator's
    /// hover text + the tree header's bracketed kind word.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::Struct => "struct",
            Self::Map => "map",
            Self::List => "list",
            Self::Tuple => "tuple",
            Self::Enum => "enum",
            Self::Unit => "unit",
            Self::Integer => "integer",
            Self::Float => "float",
            Self::Str => "string",
            Self::Char => "char",
            Self::Bool => "bool",
            Self::Scalar => "scalar",
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }

    /// The [`RichText`] for this concept's glyph (E014): the canonical glyph at ONE
    /// consistent size ([`INDICATOR_SIZE`]), `.strong()`, colored by [`color`](Self::color)
    /// — NEVER `.small()`, so the indicator reads identically across every view.
    #[must_use]
    pub fn rich(self, ui: &Ui) -> RichText {
        RichText::new(self.glyph())
            .size(INDICATOR_SIZE)
            .strong()
            .color(self.color(ui))
    }

    /// Render the indicator's glyph in a **fixed-width leading slot** (E014) so icons
    /// align vertically when several rows place one in their leading column. Allocates
    /// a slot of [`SLOT_WIDTH`] and draws [`rich`](Self::rich) into it.
    pub fn show(self, ui: &mut Ui) {
        ui.allocate_ui_with_layout(
            egui::vec2(SLOT_WIDTH, ui.spacing().interact_size.y),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.label(self.rich(ui));
            },
        );
    }
}

/// The [`TypeIndicator`] for a tree node's [`TreeNodeKind`](super::tree::TreeNodeKind)
/// (E014). A [`Leaf`](super::tree::TreeNodeKind::Leaf) maps to the generic
/// [`Scalar`](TypeIndicator::Scalar) (the leaf's specific scalar icon, when known, is
/// chosen via [`from_scalar_class`]).
#[must_use]
pub fn from_tree_kind(k: TreeNodeKind) -> TypeIndicator {
    match k {
        TreeNodeKind::Struct => TypeIndicator::Struct,
        TreeNodeKind::Map => TypeIndicator::Map,
        TreeNodeKind::List => TypeIndicator::List,
        TreeNodeKind::Tuple => TypeIndicator::Tuple,
        TreeNodeKind::EnumVariant => TypeIndicator::Enum,
        TreeNodeKind::Leaf => TypeIndicator::Scalar,
        TreeNodeKind::Error => TypeIndicator::Error,
    }
}

/// The [`TypeIndicator`] for a scalar value's [`ScalarClass`](super::classifier::ScalarClass)
/// (E014). [`Other`](super::classifier::ScalarClass::Other) maps to the generic
/// [`Scalar`](TypeIndicator::Scalar) so an unclassified value carries no false type cue.
#[must_use]
pub(crate) fn from_scalar_class(c: ScalarClass) -> TypeIndicator {
    match c {
        ScalarClass::Integer => TypeIndicator::Integer,
        ScalarClass::Float => TypeIndicator::Float,
        ScalarClass::Str => TypeIndicator::Str,
        ScalarClass::Char => TypeIndicator::Char,
        ScalarClass::Bool => TypeIndicator::Bool,
        ScalarClass::Unit => TypeIndicator::Unit,
        ScalarClass::Other => TypeIndicator::Scalar,
    }
}

/// The [`TypeIndicator`] for a diagnostic [`Severity`] (E014).
#[must_use]
pub fn from_severity(s: Severity) -> TypeIndicator {
    match s {
        Severity::Error => TypeIndicator::Error,
        Severity::Warning => TypeIndicator::Warning,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyph_is_canonical_and_stable() {
        assert_eq!(TypeIndicator::Struct.glyph(), "\u{25A2}");
        assert_eq!(TypeIndicator::Map.glyph(), "\u{25A6}");
        assert_eq!(TypeIndicator::List.glyph(), "\u{25A4}");
        assert_eq!(TypeIndicator::Tuple.glyph(), "\u{25C7}");
        assert_eq!(TypeIndicator::Enum.glyph(), "\u{25C8}");
        assert_eq!(TypeIndicator::Unit.glyph(), "\u{2205}");
        assert_eq!(TypeIndicator::Integer.glyph(), "\u{0023}");
        assert_eq!(TypeIndicator::Float.glyph(), "\u{2248}");
        assert_eq!(TypeIndicator::Str.glyph(), "\u{0022}");
        assert_eq!(TypeIndicator::Char.glyph(), "\u{0027}");
        assert_eq!(TypeIndicator::Bool.glyph(), "\u{2611}");
        assert_eq!(TypeIndicator::Scalar.glyph(), "\u{2022}");
        assert_eq!(TypeIndicator::Error.glyph(), "\u{2716}");
        assert_eq!(TypeIndicator::Warning.glyph(), "\u{26A0}");
    }

    #[test]
    fn from_tree_kind_matches_the_direct_indicator() {
        // Cross-view consistency: a kind→indicator conversion yields the same glyph as
        // the direct indicator variant for the same concept.
        assert_eq!(
            from_tree_kind(TreeNodeKind::List).glyph(),
            TypeIndicator::List.glyph()
        );
        assert_eq!(
            from_tree_kind(TreeNodeKind::Tuple).glyph(),
            TypeIndicator::Tuple.glyph()
        );
        assert_eq!(
            from_tree_kind(TreeNodeKind::Map).glyph(),
            TypeIndicator::Map.glyph()
        );
        assert_eq!(
            from_tree_kind(TreeNodeKind::Struct).glyph(),
            TypeIndicator::Struct.glyph()
        );
        assert_eq!(
            from_tree_kind(TreeNodeKind::EnumVariant).glyph(),
            TypeIndicator::Enum.glyph()
        );
        assert_eq!(
            from_tree_kind(TreeNodeKind::Leaf).glyph(),
            TypeIndicator::Scalar.glyph()
        );
    }

    #[test]
    fn from_scalar_class_matches_the_direct_indicator() {
        assert_eq!(
            from_scalar_class(ScalarClass::Integer).glyph(),
            TypeIndicator::Integer.glyph()
        );
        assert_eq!(
            from_scalar_class(ScalarClass::Float).glyph(),
            TypeIndicator::Float.glyph()
        );
        assert_eq!(
            from_scalar_class(ScalarClass::Other).glyph(),
            TypeIndicator::Scalar.glyph()
        );
    }

    #[test]
    fn from_severity_maps_to_error_and_warning() {
        assert_eq!(from_severity(Severity::Error), TypeIndicator::Error);
        assert_eq!(from_severity(Severity::Warning), TypeIndicator::Warning);
    }

    #[test]
    fn all_is_grouped_and_complete() {
        // The legend strip iterates `ALL`; assert the group boundaries + ordering so a
        // future reorder/addition keeps the container/scalar/status grouping coherent.
        use TypeIndicator::*;
        assert_eq!(
            TypeIndicator::ALL,
            &[
                Struct, Map, List, Tuple, Enum, Unit, // containers
                Integer, Float, Str, Char, Bool, Scalar, // scalars
                Error, Warning, // status
            ]
        );
        assert_eq!(
            TypeIndicator::CONTAINER_COUNT + TypeIndicator::SCALAR_COUNT + 2,
            TypeIndicator::ALL.len(),
            "container + scalar + status (2) groups cover every ALL entry"
        );
        // Every entry has a non-empty glyph + hover word (the legend renders both).
        for ind in TypeIndicator::ALL {
            assert!(!ind.glyph().is_empty(), "{ind:?} glyph");
            assert!(!ind.word().is_empty(), "{ind:?} word");
        }
    }
}
