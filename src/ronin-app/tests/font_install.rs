//! Font-fallback installation tests (UI Fix 2).
//!
//! [`App::install_fonts`] registers the three bundled Noto faces and appends them to
//! the *end* (fallback position) of both the `Proportional` and `Monospace` family
//! chains, so normal text keeps the default faces and only missing glyphs (symbols,
//! math) fall through. These assert that membership + ordering on the
//! [`build_font_definitions`] factory (which `install_fonts` feeds to
//! `ctx.set_fonts`) so the chain is checked without a live `egui::Context`, and that
//! the three `include_bytes!` blobs are present and plausibly real `.ttf` data.

use ronin_app::app::build_font_definitions;

/// The three font-data keys the fix installs as fallbacks, in append order.
const NOTO_KEYS: [&str; 3] = ["noto_symbols", "noto_symbols2", "noto_math"];

/// Every symbol glyph the structural surfaces render. This is the canonical
/// [`TypeIndicator`](ronin_app::structural::TypeIndicator) glyph set (E014 — the ONE
/// shared type-indicator system the tree, table, and section boundaries all draw)
/// plus the remaining control / breadcrumb / summary glyphs. All must be covered by
/// the bundled Noto fallback faces (else they render as tofu boxes). The codepoints
/// are asserted against the LIVE installed font chain via egui's `FontsView::has_glyph`,
/// so a regression that drops a face or picks an uncovered glyph fails here.
const STRUCTURAL_GLYPHS: &[(char, &str)] = &[
    // The canonical `TypeIndicator` glyph set (one glyph per concept, every view).
    ('\u{25A2}', "▢ Struct indicator"),
    ('\u{25A6}', "▦ Map indicator"),
    ('\u{25A4}', "▤ List indicator"),
    ('\u{25C7}', "◇ Tuple indicator"),
    ('\u{25C8}', "◈ Enum indicator"),
    ('\u{2205}', "∅ Unit indicator"),
    ('\u{0023}', "# Integer indicator"),
    ('\u{2248}', "≈ Float indicator"),
    ('\u{0022}', "\" Str indicator"),
    ('\u{0027}', "' Char indicator"),
    ('\u{2611}', "☑ Bool indicator"),
    (
        '\u{2022}',
        "• Scalar indicator (generic / unclassified leaf)",
    ),
    ('\u{2716}', "✖ Error indicator / delete control"),
    ('\u{26A0}', "⚠ Warning indicator"),
    // Remaining control / navigation / summary glyphs (not type indicators).
    ('\u{25B8}', "▸ breadcrumb separator"),
    ('\u{2190}', "← back-to-table control"),
    ('\u{2191}', "↑ move-up control"),
    ('\u{2193}', "↓ move-down control"),
    ('\u{2026}', "… ellipsis (summary truncation)"),
    // E016 — Table view Back / Forward / Up navigation buttons.
    ('\u{25C0}', "◀ Table view Back button"),
    ('\u{25B6}', "▶ Table view Forward button"),
    ('\u{25B2}', "▲ Table view Up-a-level button"),
];

#[test]
fn structural_glyphs_are_covered_by_the_installed_font_chain() {
    use egui::{FontFamily, FontId};
    use egui_kittest::Harness;

    // Build a ticked Context (via the kittest harness) with the bundled fonts installed
    // (the same definitions `App::install_fonts` applies), then assert every structural
    // glyph has a real glyph in BOTH family chains via `FontsView::has_glyph`.
    let mut harness = Harness::new_ui(|ui| {
        // Install the bundled fonts on the first frame; the harness ticks the context
        // so the font atlas realizes for the `has_glyph` queries below.
        ui.ctx().set_fonts(build_font_definitions());
        ui.label("font coverage probe");
    });
    harness.run();
    let ctx = harness.ctx.clone();

    for fam in [FontFamily::Proportional, FontFamily::Monospace] {
        let font_id = FontId::new(14.0, fam.clone());
        for (glyph, label) in STRUCTURAL_GLYPHS {
            let covered = ctx.fonts_mut(|f| f.has_glyph(&font_id, *glyph));
            assert!(
                covered,
                "{fam:?}: glyph U+{:04X} ({label}) must be covered by the bundled font chain",
                *glyph as u32
            );
        }
    }
}

#[test]
fn three_noto_faces_are_registered_in_font_data() {
    let fonts = build_font_definitions();
    for key in NOTO_KEYS {
        assert!(
            fonts.font_data.contains_key(key),
            "font_data must register the bundled face `{key}`"
        );
    }
}

#[test]
fn noto_faces_are_appended_to_both_family_fallback_chains() {
    let fonts = build_font_definitions();

    for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let chain = fonts
            .families
            .get(&fam)
            .unwrap_or_else(|| panic!("default FontDefinitions must define {fam:?}"));

        // All three must be present in the chain.
        for key in NOTO_KEYS {
            assert!(
                chain.iter().any(|n| n == key),
                "{fam:?} fallback chain must contain `{key}`"
            );
        }

        // They must sit at the END (fallback position), in registration order, so
        // the default fonts still drive normal text — assert the chain's last three
        // entries are exactly the three Noto faces in order.
        let tail = &chain[chain.len() - NOTO_KEYS.len()..];
        assert_eq!(
            tail, NOTO_KEYS,
            "the three Noto faces must be the LAST (fallback) entries of {fam:?}, in order"
        );

        // The chain must keep a non-Noto primary font ahead of the fallbacks, i.e.
        // the fix did not clobber the default faces.
        assert!(
            chain.len() > NOTO_KEYS.len(),
            "{fam:?} must retain its default primary font(s) ahead of the Noto fallbacks"
        );
    }
}

#[test]
fn bundled_font_blobs_are_present_and_plausible_ttf() {
    // The same blobs `build_font_definitions` registers via `include_bytes!`. egui
    // panics at `set_fonts` on a malformed font, so at minimum assert the bytes are
    // present and begin with a recognized sfnt/TrueType signature.
    let blobs: [(&str, &[u8]); 3] = [
        (
            "noto-symbols.ttf",
            include_bytes!("../assets/noto-symbols.ttf"),
        ),
        (
            "noto-symbols2.ttf",
            include_bytes!("../assets/noto-symbols2.ttf"),
        ),
        ("noto-math.ttf", include_bytes!("../assets/noto-math.ttf")),
    ];

    for (name, bytes) in blobs {
        assert!(
            bytes.len() > 1024,
            "`{name}` should be a real font (got {} bytes)",
            bytes.len()
        );
        // A TrueType/OpenType file starts with one of: 0x00010000 (TrueType),
        // "true", "ttcf", "OTTO", or "wOFF". The bundled Noto faces are TrueType.
        let sig = &bytes[..4.min(bytes.len())];
        let recognized = sig == [0x00, 0x01, 0x00, 0x00]
            || sig == b"true"
            || sig == b"ttcf"
            || sig == b"OTTO"
            || sig == b"wOFF";
        assert!(
            recognized,
            "`{name}` must start with a recognized sfnt signature, got {sig:02X?}"
        );
    }
}
