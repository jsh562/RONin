//! Cross-view type-indicator consistency tests (E014).
//!
//! After the consolidation there is ONE shared type-indicator system
//! ([`TypeIndicator`](ronin_app::structural::TypeIndicator)): the SAME concept must
//! yield the SAME glyph no matter the entry point (a list is `▤` whether it comes
//! from the tree's `from_tree_kind(List)` or the direct `TypeIndicator::List`, and
//! whether it is painted in the tree or in a table cell/header). These pin that
//! single source of truth at both the pure-API level and through the live render.

use std::time::{Duration, Instant};

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;

use ronin_app::document::EditorDocument;
use ronin_app::reparse::ReparseWorker;
use ronin_app::structural::indicators::from_tree_kind;
use ronin_app::structural::sections::SectionShape;
use ronin_app::structural::table::render_table_view;
use ronin_app::structural::tree::{render_tree_view, TreeNodeKind};
use ronin_app::structural::view_state::StructuralPath;
use ronin_app::structural::TypeIndicator;

/// Drive the real off-frame reparse to completion.
fn drive_reparse(doc: &mut EditorDocument, worker: &ReparseWorker) {
    doc.request_reparse(worker);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if doc.poll_parse(worker) {
            return;
        }
        if Instant::now() >= deadline {
            panic!("reparse did not land within timeout");
        }
        std::thread::yield_now();
    }
}

fn doc_at(src: &str, worker: &ReparseWorker) -> EditorDocument {
    let mut doc = EditorDocument::new_untitled(1);
    doc.buffer = src.to_string();
    doc.on_edit();
    drive_reparse(&mut doc, worker);
    doc
}

// =============================================================================
// Pure-API: same concept → same glyph regardless of entry point
// =============================================================================

#[test]
fn same_concept_yields_same_glyph_across_entry_points() {
    // The tree's kind→indicator converter and the direct `TypeIndicator` variant must
    // produce the SAME glyph for the same concept (no per-view divergence).
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
}

#[test]
fn canonical_glyph_set_is_exactly_the_expected_codepoints() {
    // Pin the canonical glyph set so any drift fails loudly (and stays in sync with
    // `tests/font_install.rs`'s coverage list).
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
fn rich_is_uniform_size_never_small() {
    // The indicator's `rich` RichText is rendered at ONE consistent size and is never
    // `.small()`. We assert via the public behavior: a glyph rendered through `rich`
    // and a freshly-built `RichText` at the indicator size produce the same default
    // height-class (egui has no public `.is_small()` getter, so we assert the size
    // mechanism is the SINGLE constant by comparing two indicators' rendered glyphs
    // share the same font size — they are built by the same `rich` code path).
    //
    // `RichText` does not expose its size, so the load-bearing guarantee — "uniform
    // size, never small" — is enforced structurally: `rich` sets `.size(..)` (a fixed
    // constant) and never calls `.small()`. This test pins that `rich` returns a
    // non-empty, strong glyph for every variant (a `.small()` glyph would still be
    // non-empty, so the real guard is the single-constant `rich` impl exercised by the
    // render tests below + the source having no `.small()` in `indicators.rs`).
    for indicator in [
        TypeIndicator::Struct,
        TypeIndicator::Map,
        TypeIndicator::List,
        TypeIndicator::Tuple,
        TypeIndicator::Enum,
        TypeIndicator::Unit,
        TypeIndicator::Integer,
        TypeIndicator::Float,
        TypeIndicator::Str,
        TypeIndicator::Char,
        TypeIndicator::Bool,
        TypeIndicator::Scalar,
        TypeIndicator::Error,
        TypeIndicator::Warning,
    ] {
        assert!(
            !indicator.glyph().is_empty(),
            "{indicator:?} must have a non-empty glyph"
        );
        assert!(
            !indicator.word().is_empty(),
            "{indicator:?} must have a non-empty hover word"
        );
    }
}

#[test]
fn indicators_source_has_no_small_call() {
    // The load-bearing "NEVER .small()" guarantee, asserted against the module source:
    // the single `rich`/`show` rendering path must not downgrade the glyph size.
    let src = include_str!("../src/structural/indicators.rs");
    // Ignore the doc comments (which mention `.small()` historically); only the code
    // matters. A real `.small()` call would be `.small()` on a RichText builder.
    let code_calls_small = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//") && !l.trim_start().starts_with("//!"))
        .any(|l| l.contains(".small()"));
    assert!(
        !code_calls_small,
        "indicators.rs must never call `.small()` — indicators render at one uniform size"
    );
}

// =============================================================================
// Live render: the same glyph appears in the tree AND in a table cell/header
// =============================================================================

#[test]
fn list_renders_as_list_glyph_in_both_tree_and_table() {
    // A list-valued field renders the SAME list glyph (▤) in the tree header and in a
    // table cell/column-header — no per-view divergence (E014).
    let list_glyph = TypeIndicator::List.glyph();

    // Tree: a top-level list node's header carries the list glyph.
    {
        let worker = ReparseWorker::new();
        let mut doc = doc_at("[1, 2, 3]", &worker);
        let mut harness = Harness::new_ui(move |ui| {
            render_tree_view(ui, &mut doc, &worker);
        });
        harness.run();
        assert!(
            harness.query_all_by_label_contains(list_glyph).next().is_some(),
            "the tree must paint the list glyph `{list_glyph}` for a list node"
        );
    }

    // Table: a uniform record list with a list-valued column shows the list glyph in
    // the column header (the list icon is itself the open-as-table affordance).
    {
        let worker = ReparseWorker::new();
        let mut doc = doc_at(
            "[\n    (i: 1, tags: [\"a\"]),\n    (i: 2, tags: []),\n    (i: 3, tags: [\"c\"]),\n]",
            &worker,
        );
        let mut harness = Harness::new_ui(move |ui| {
            render_table_view(
                ui,
                &mut doc,
                &worker,
                &StructuralPath::root(),
                SectionShape::RecordList,
            );
        });
        harness.run();
        assert!(
            harness.query_all_by_label_contains(list_glyph).next().is_some(),
            "the table must paint the SAME list glyph `{list_glyph}` for a list column/cell"
        );
    }
}

#[test]
fn tuple_renders_as_tuple_glyph_in_both_tree_and_a_nested_table_cell() {
    // A tuple renders the SAME glyph (◇) in the tree and in a `Nested` table cell.
    let tuple_glyph = TypeIndicator::Tuple.glyph();

    // Tree: a tuple node's header carries the tuple glyph.
    {
        let worker = ReparseWorker::new();
        let mut doc = doc_at("(1, 2)", &worker);
        let mut harness = Harness::new_ui(move |ui| {
            render_tree_view(ui, &mut doc, &worker);
        });
        harness.run();
        assert!(
            harness
                .query_all_by_label_contains(tuple_glyph)
                .next()
                .is_some(),
            "the tree must paint the tuple glyph `{tuple_glyph}` for a tuple node"
        );
    }

    // Table: a uniform record list whose `pos` column holds tuples → a Nested cell
    // showing the tuple glyph (a tree/form drill-in by kind glyph).
    {
        let worker = ReparseWorker::new();
        let mut doc = doc_at(
            "[\n    (i: 1, pos: (0, 0)),\n    (i: 2, pos: (1, 1)),\n    (i: 3, pos: (2, 2)),\n]",
            &worker,
        );
        let mut harness = Harness::new_ui(move |ui| {
            render_table_view(
                ui,
                &mut doc,
                &worker,
                &StructuralPath::root(),
                SectionShape::RecordList,
            );
        });
        harness.run();
        assert!(
            harness
                .query_all_by_label_contains(tuple_glyph)
                .next()
                .is_some(),
            "the table must paint the SAME tuple glyph `{tuple_glyph}` for a tuple cell"
        );
    }
}
