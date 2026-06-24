//! Self-checking showcase-sample tests (E015 — Part D).
//!
//! Each `samples/showcase_*.ron` is loaded **from disk** (so the file on disk —
//! the same bytes the binary embeds via `include_str!` — is what is verified) and
//! driven through the *real* off-frame [`ReparseWorker`] round-trip (the same
//! `doc_at`/`drive_reparse` harness the structural tests use). The asserts make a
//! sample that does NOT actually exercise its target feature **fail**:
//!
//!  * every sample parses with **zero error diagnostics** (valid RON);
//!  * `scan_table_sections` over `showcase_tables.ron` finds the expected
//!    RecordList + RecordMap + TupleList + nested sections;
//!  * `classifier::classify` over each target list in `showcase_fallbacks.ron`
//!    returns each expected `FallbackReason` (one assertion per reason);
//!  * the RON→JSON converter over `showcase_interop.ron` emits each expected loss
//!    code (one assertion per achievable `LossKind`);
//!  * `showcase_bevy.scn.ron` is recognized as a Bevy scene by the app's detection;
//!  * `showcase_tree.ron` yields a tree model containing each `TreeNodeKind`.
//!
//! Every loop is bounded; nothing hangs.

use std::path::Path;
use std::time::{Duration, Instant};

use ronin_core::ast;
use ronin_core::{parse, Severity, SyntaxNode};

use ronin_app::app::App;
use ronin_app::bevy::mode::Mode;
use ronin_app::bevy::SceneModel;
use ronin_app::document::EditorDocument;
use ronin_app::interop::{ron_to_json, CommentMode, LossKind};
use ronin_app::reparse::ReparseWorker;
use ronin_app::settings::AppSettings;
use ronin_app::structural::classifier::{classify, FallbackReason};
use ronin_app::structural::sections::{scan_table_sections, SectionShape};
use ronin_app::structural::table::{CellClass, TableModel};
use ronin_app::structural::tree::{TreeFormModel, TreeNode, TreeNodeKind};

// =============================================================================
// Harness
// =============================================================================

/// The list of every bundled showcase sample file name, in menu order.
const SAMPLES: &[&str] = &[
    "showcase_tree.ron",
    "showcase_tables.ron",
    "showcase_fallbacks.ron",
    "showcase_interop.ron",
    "showcase_highlight.ron",
    "showcase_bevy.scn.ron",
    "showcase_kitchen_sink.ron",
];

/// Load a `samples/<name>` file from disk relative to the crate manifest (robust
/// regardless of the test's working directory).
fn sample(name: &str) -> String {
    let path = format!("{}/samples/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// Request a reparse and spin-poll until a current result installs, or panic on a
/// bounded timeout. Drives the *real* off-frame worker to completion.
fn drive_reparse(doc: &mut EditorDocument, worker: &ReparseWorker) {
    doc.request_reparse(worker);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if doc.poll_parse(worker) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "reparse did not land within timeout"
        );
        std::thread::yield_now();
    }
}

/// Build a document over `src`, drive a reparse so a projection lands, and return it.
fn doc_at(src: &str, worker: &ReparseWorker) -> EditorDocument {
    let mut doc = EditorDocument::new_untitled(1);
    doc.buffer = src.to_string();
    doc.on_edit();
    drive_reparse(&mut doc, worker);
    doc
}

/// The number of **error**-severity parse diagnostics in `src` (a valid RON
/// document recovers cleanly, so this is zero). Parse-recovery diagnostics are all
/// `Severity::Error`, so this is the authoritative "is it valid RON" check.
fn parse_error_count(src: &str) -> usize {
    parse(src)
        .diagnostics()
        .iter()
        .filter(|d| d.severity() == Severity::Error)
        .count()
}

/// The CST `SyntaxNode` of a top-level struct field's value, by field name.
fn field_value_node(src: &str, field: &str) -> SyntaxNode {
    let cst = parse(src);
    let top = ast::Document::cast(cst.root())
        .and_then(|d| d.value())
        .expect("a top-level value");
    let ast::Value::Struct(s) = top else {
        panic!("top-level value is not a struct");
    };
    let f = s
        .fields()
        .find(|f| f.name_text().as_deref() == Some(field))
        .unwrap_or_else(|| panic!("field `{field}` not found"));
    f.value()
        .unwrap_or_else(|| panic!("field `{field}` has no value"))
        .syntax()
        .clone()
}

// =============================================================================
// Every sample is valid RON (zero error diagnostics) — through the real worker
// =============================================================================

#[test]
fn every_sample_parses_with_zero_error_diagnostics() {
    let worker = ReparseWorker::new();
    for name in SAMPLES {
        let src = sample(name);
        // Through the real off-frame worker (the doc_at/drive_reparse pattern).
        let doc = doc_at(&src, &worker);
        let parse = doc.parse.as_ref().expect("a parse landed");
        let errors = parse
            .cst
            .diagnostics()
            .iter()
            .filter(|d| d.severity() == Severity::Error)
            .count();
        assert_eq!(errors, 0, "sample `{name}` has {errors} parse error(s)");
        // And the standalone parse agrees (same bytes, same verdict).
        assert_eq!(
            parse_error_count(&src),
            0,
            "sample `{name}` is not valid RON"
        );
    }
}

// =============================================================================
// showcase_tables.ron — RecordList + RecordMap + TupleList + nested sections
// =============================================================================

#[test]
fn tables_sample_finds_every_table_shape_and_nested_sections() {
    let src = sample("showcase_tables.ron");
    assert_eq!(
        parse_error_count(&src),
        0,
        "tables sample must be valid RON"
    );

    let cst = parse(&src);
    let sections = scan_table_sections(&cst);
    assert!(!sections.is_empty(), "expected table sections, found none");

    let by_shape = |shape: SectionShape| sections.iter().filter(|s| s.shape == shape).count();

    // RecordList: `ships` (top-level, 3 rows) — and the nested `crew` lists are
    // scalar lists (not sections), so the RecordList we care about is `ships`.
    let record_lists: Vec<_> = sections
        .iter()
        .filter(|s| s.shape == SectionShape::RecordList)
        .collect();
    assert!(
        record_lists.iter().any(|s| s.rows == 3),
        "expected a 3-row RecordList (ships), got {record_lists:?}"
    );
    // The `ships` RecordList must expose a column for the field that is absent on
    // one row (`armor`) — the union-of-fields column set keeps the optional field,
    // which renders as a Blank cell. (>= because crew/origin columns are present.)
    let ships = record_lists
        .iter()
        .find(|s| s.rows == 3)
        .expect("the ships RecordList");
    assert!(
        ships.cols >= 5,
        "ships columns (union incl. the missing `armor`) = {} (< 5)",
        ships.cols
    );

    // RecordMap: `hulls` (3 same-named `Hull` records → a key column + fields).
    let record_maps: Vec<_> = sections
        .iter()
        .filter(|s| s.shape == SectionShape::RecordMap)
        .collect();
    assert!(
        record_maps.iter().any(|s| s.rows == 3),
        "expected a 3-row RecordMap (hulls), got {record_maps:?}"
    );

    // TupleList: `coords` (4 equal-arity 2-tuples) — plus the nested `cells` lists
    // inside the hull values are ALSO TupleLists (the nested sections).
    let tuple_lists: Vec<_> = sections
        .iter()
        .filter(|s| s.shape == SectionShape::TupleList)
        .collect();
    assert!(
        tuple_lists.iter().any(|s| s.rows == 4 && s.cols == 2),
        "expected a 4-row arity-2 TupleList (coords), got {tuple_lists:?}"
    );

    // Nested sections exist: the `cells` TupleLists live inside the `hulls`
    // RecordMap values, so there must be MORE than the three top-level sections.
    let nested_tuple_lists = tuple_lists
        .iter()
        .filter(|s| s.label.contains('\u{25B8}'))
        .count();
    assert!(
        nested_tuple_lists >= 1,
        "expected at least one NESTED tuple-list section (a hull's `cells`), \
         got tuple lists {tuple_lists:?}"
    );

    // All three shapes are represented.
    assert!(by_shape(SectionShape::RecordList) >= 1);
    assert!(by_shape(SectionShape::RecordMap) >= 1);
    assert!(by_shape(SectionShape::TupleList) >= 1);
}

// =============================================================================
// showcase_fallbacks.ron — each list triggers exactly one FallbackReason
// =============================================================================

#[test]
fn fallbacks_sample_triggers_each_reason() {
    let src = sample("showcase_fallbacks.ron");
    assert_eq!(
        parse_error_count(&src),
        0,
        "fallbacks sample must be valid RON"
    );

    // Each target field's list classifies to exactly the expected reason.
    let cases = [
        ("name_mismatch", FallbackReason::NameMismatch),
        ("type_conflict", FallbackReason::TypeConflict),
        ("nested_only", FallbackReason::NestedOnly),
        ("not_a_record_list", FallbackReason::NotARecordList),
        ("too_small", FallbackReason::TooSmall),
        ("empty", FallbackReason::Empty),
    ];

    for (field, expected) in cases {
        let node = field_value_node(&src, field);
        let verdict = classify(&node);
        assert!(
            !verdict.table_eligible,
            "field `{field}` should NOT be table-eligible (expected {expected:?})"
        );
        assert_eq!(
            verdict.fallback_reason,
            Some(expected),
            "field `{field}` classified as {:?}, expected {expected:?}",
            verdict.fallback_reason
        );
    }
}

// =============================================================================
// showcase_interop.ron — each achievable RON→JSON loss code fires
// =============================================================================

#[test]
fn interop_sample_emits_each_loss_code() {
    let src = sample("showcase_interop.ron");
    assert_eq!(
        parse_error_count(&src),
        0,
        "interop sample must be valid RON"
    );

    let cst = parse(&src);

    // Pure-standard-JSON (CommentMode::None): the value losses fire AND the file's
    // comments are dropped → DroppedComment (RON-I0009) is reported here too.
    let pure = ron_to_json(&cst, None, CommentMode::None);
    let report = &pure.loss_report;

    // Every achievable code (a valid RON file cannot produce RON-I0010
    // UnparseableRegion — that needs an unparseable region — so it is excluded).
    let expected = [
        LossKind::StructName,     // RON-I0001 — named structs (Showcase, Inner)
        LossKind::TupleVsList,    // RON-I0002 — the anonymous (1, 2, 3) tuple
        LossKind::Char,           // RON-I0003 — 'x'
        LossKind::EnumTagging,    // RON-I0004 — Running
        LossKind::NonStringKey,   // RON-I0005 — integer map keys 1, 2
        LossKind::UnitVsNull,     // RON-I0006 — ()
        LossKind::RawString,      // RON-I0007 — r#"..."#
        LossKind::TrailingComma,  // RON-I0008 — a trailing comma
        LossKind::DroppedComment, // RON-I0009 — a comment, dropped under pure JSON
    ];
    for kind in expected {
        assert!(
            report.count_of(kind) >= 1,
            "interop sample did not emit {} ({:?}); report kinds = {:?}",
            kind.code(),
            kind,
            report
                .constructs()
                .iter()
                .map(|c| c.code())
                .collect::<Vec<_>>()
        );
    }

    // Sanity on the JSONC path: with comments carried, DroppedComment does NOT fire
    // (the only difference between the two carriers), but the value losses still do.
    let jsonc = ron_to_json(&cst, None, CommentMode::JsoncInline);
    assert_eq!(
        jsonc.loss_report.count_of(LossKind::DroppedComment),
        0,
        "JSONC carries comments, so none are dropped"
    );
    assert!(
        jsonc.loss_report.count_of(LossKind::TupleVsList) >= 1,
        "value losses still fire under JSONC"
    );
}

// =============================================================================
// showcase_bevy.scn.ron — recognized as a Bevy scene by the app's detection
// =============================================================================

#[test]
fn bevy_sample_is_recognized_as_a_scene() {
    let name = "showcase_bevy.scn.ron";
    let src = sample(name);
    assert_eq!(parse_error_count(&src), 0, "bevy sample must be valid RON");

    // 1) The app's detection: opening the sample as a tab auto-detects Bevy mode
    //    (extension-based, FR-009) — the end-to-end "recognized as a scene" path.
    let mut app = App::new(AppSettings::default(), None);
    app.open_sample(name, &src);
    assert_eq!(
        app.active_mode(),
        Some(Mode::Bevy),
        "opening `{name}` must auto-detect Bevy mode"
    );

    // 2) The scene SHAPE is interpreted: resources + entities + components, with the
    //    enum-variant / Option / tuple component values present.
    let model = SceneModel::from_cst(&parse(&src));
    assert!(
        !model.resources().is_empty(),
        "the scene must read resources"
    );
    assert!(!model.entities().is_empty(), "the scene must read entities");
    let component_count = model.components().count();
    assert!(
        component_count >= 3,
        "the scene must read components (got {component_count})"
    );
}

// =============================================================================
// showcase_tree.ron — the tree model contains each TreeNodeKind
// =============================================================================

#[test]
fn tree_sample_yields_every_tree_node_kind() {
    let src = sample("showcase_tree.ron");
    assert_eq!(parse_error_count(&src), 0, "tree sample must be valid RON");

    let cst = parse(&src);
    let model = TreeFormModel::derive(&cst, &[]);

    // Collect every TreeNodeKind present anywhere in the tree.
    fn collect(node: &TreeNode, kinds: &mut Vec<TreeNodeKind>) {
        kinds.push(node.kind);
        for child in &node.children {
            collect(child, kinds);
        }
    }
    let mut kinds = Vec::new();
    for root in &model.roots {
        collect(root, &mut kinds);
    }

    for expected in [
        TreeNodeKind::Struct,
        TreeNodeKind::Map,
        TreeNodeKind::List,
        TreeNodeKind::Tuple,
        TreeNodeKind::EnumVariant,
        TreeNodeKind::Leaf,
    ] {
        assert!(
            kinds.contains(&expected),
            "tree sample is missing a {expected:?} node; present = {kinds:?}"
        );
    }
}

// =============================================================================
// The menu list and the on-disk samples agree (no stale embed / missing file)
// =============================================================================

#[test]
fn sample_files_all_exist_on_disk() {
    for name in SAMPLES {
        let path = format!("{}/samples/{name}", env!("CARGO_MANIFEST_DIR"));
        assert!(
            Path::new(&path).exists(),
            "expected showcase sample `{name}` at {path}"
        );
    }
}

// =============================================================================
// (T036) The new cell editors over the REAL sample corpus — bool / enum / numeric
// cells in the bundled samples PROJECT and RENDER correctly with the E012 editors,
// and the samples still load losslessly through the real worker.
//
// Not a duplicate of `table_ergonomics.rs` (which uses synthetic fixtures): this
// asserts the new editors classify the cells in the actual on-disk corpus the app
// ships. A sample that does NOT actually carry the targeted cell kinds FAILS.
// =============================================================================

/// The first column index whose every present cell's `scalar_type_name` equals `word`
/// (an `integer` / `float` / `bool` column), or `None`.
fn column_of_scalar_word(model: &TableModel, word: &str) -> Option<usize> {
    (0..model.columns.len()).find(|&c| {
        let present: Vec<_> = (0..model.row_count())
            .filter_map(|r| model.cell(r, c))
            .filter(|cell| cell.class == CellClass::Scalar)
            .collect();
        !present.is_empty() && present.iter().all(|cell| cell.scalar_type_name() == Some(word))
    })
}

#[test]
fn kitchen_sink_events_projects_numeric_and_enum_cells() {
    // `showcase_kitchen_sink.ron` → the top-level `events` RecordList carries the new
    // editors' target cell kinds: `at` is a NUMERIC cell (integer) and `kind` is an
    // ENUM-like cell (a bare-identifier variant: Spawn / Move / Despawn). Both project
    // as editable Scalar cells with the right lexical type — the numeric-increment and
    // (text-fallback) enum editors apply to the real corpus.
    let src = sample("showcase_kitchen_sink.ron");
    assert_eq!(parse_error_count(&src), 0, "kitchen-sink must be valid RON");

    // The `events` section is the LAST top-level RecordList; locate it by its `at` /
    // `kind` / `payload` columns rather than by scan order.
    let cst = parse(&src);
    let events = scan_table_sections(&cst)
        .into_iter()
        .filter(|s| s.shape == SectionShape::RecordList)
        .filter_map(|s| {
            TableModel::derive_section(&cst, &s.path, SectionShape::RecordList, &[]).map(|m| (s, m))
        })
        .find(|(_, m)| {
            let names: Vec<&str> = m.columns.iter().map(|c| c.field_name.as_str()).collect();
            names.contains(&"at") && names.contains(&"kind")
        })
        .map(|(_, m)| m)
        .expect("the `events` RecordList (at / kind / payload) is found");

    assert_eq!(events.row_count(), 3, "events has 3 rows (Spawn / Move / Despawn)");

    // The `at` column is a NUMERIC cell on every row (the numeric-increment editor).
    let at_col = events
        .columns
        .iter()
        .position(|c| c.field_name == "at")
        .expect("an `at` column");
    for r in 0..events.row_count() {
        let cell = events.cell(r, at_col).expect("an `at` cell");
        assert_eq!(cell.class, CellClass::Scalar, "the `at` cell is an editable scalar");
        assert_eq!(
            cell.scalar_type_name(),
            Some("integer"),
            "the `at` cell projects as a numeric (integer) cell: {:?}",
            cell.text
        );
    }

    // The `kind` column is an ENUM-like cell — a bare identifier variant token on every
    // row (Spawn / Move / Despawn). It projects as an editable Scalar; with no type bound
    // it edits as free text (the enum picker is the type-aware enhancement).
    let kind_col = events
        .columns
        .iter()
        .position(|c| c.field_name == "kind")
        .expect("a `kind` column");
    let variants: Vec<String> = (0..events.row_count())
        .map(|r| {
            let cell = events.cell(r, kind_col).expect("a `kind` cell");
            assert_eq!(cell.class, CellClass::Scalar, "the `kind` cell is an editable scalar");
            cell.text.clone().expect("a present `kind` token")
        })
        .collect();
    for v in ["Spawn", "Move", "Despawn"] {
        assert!(
            variants.iter().any(|t| t == v),
            "the `kind` enum column carries the `{v}` variant token: {variants:?}"
        );
    }
}

#[test]
fn ships_cells_section_projects_bool_and_enum_cells() {
    // `ships.ron` → each hull's `cells` RecordList carries a `structural` BOOL column
    // and a `shape` ENUM-like column (the Full / … variants) alongside a numeric
    // `section`. This is the real-corpus witness for the bool toggle editor (no
    // synthetic fixture): the bool cells project with the `bool` scalar type so Space
    // toggles them, and the enum-like cells project as editable Scalars.
    let src = sample("ships.ron");
    assert_eq!(parse_error_count(&src), 0, "ships.ron must be valid RON");

    let cst = parse(&src);
    // Find the first RecordList whose columns include `structural` (bool) + `shape`.
    let cells = scan_table_sections(&cst)
        .into_iter()
        .filter(|s| s.shape == SectionShape::RecordList)
        .filter_map(|s| {
            TableModel::derive_section(&cst, &s.path, SectionShape::RecordList, &[]).map(|m| (s, m))
        })
        .find(|(_, m)| {
            let names: Vec<&str> = m.columns.iter().map(|c| c.field_name.as_str()).collect();
            names.contains(&"structural") && names.contains(&"shape")
        })
        .map(|(_, m)| m)
        .expect("a hull `cells` RecordList (structural / shape) is found");

    // The `structural` column is a BOOL cell on every present row (the Space-toggle /
    // checkbox editor) — `true`/`false` tokens classify as `bool`.
    let bool_col = column_of_scalar_word(&cells, "bool").expect("a bool column in `cells`");
    assert_eq!(
        cells.columns[bool_col].field_name, "structural",
        "the bool column is `structural`"
    );
    let bool_tokens: Vec<String> = (0..cells.row_count())
        .filter_map(|r| cells.cell(r, bool_col))
        .filter_map(|c| c.text.clone())
        .collect();
    assert!(
        bool_tokens.iter().any(|t| t == "true") || bool_tokens.iter().any(|t| t == "false"),
        "the `structural` bool column carries true/false tokens: {bool_tokens:?}"
    );
    for t in &bool_tokens {
        assert!(t == "true" || t == "false", "a bool cell holds only true/false: {t}");
    }

    // The `shape` column is an ENUM-like cell — a bare-identifier variant on every row.
    let shape_col = cells
        .columns
        .iter()
        .position(|c| c.field_name == "shape")
        .expect("a `shape` column");
    for r in 0..cells.row_count() {
        if let Some(cell) = cells.cell(r, shape_col) {
            if cell.class == CellClass::Scalar {
                let tok = cell.text.clone().expect("a present `shape` token");
                assert!(
                    tok.chars().next().is_some_and(char::is_alphabetic),
                    "the `shape` enum cell is a bare-identifier variant token: {tok}"
                );
            }
        }
    }
}

#[test]
fn samples_with_new_cell_kinds_load_losslessly_through_the_worker() {
    // The samples carrying the new cell kinds still LOAD losslessly: parsed + driven
    // through the real off-frame worker, the buffer round-trips byte-for-byte (the new
    // editors are projection-only reads — opening a sample never reflows it; FR-014).
    let worker = ReparseWorker::new();
    for name in ["showcase_kitchen_sink.ron", "ships.ron"] {
        let src = sample(name);
        let doc = doc_at(&src, &worker);
        assert_eq!(
            doc.buffer, src,
            "`{name}` loaded byte-losslessly (projecting bool/enum/numeric cells did not reflow it)"
        );
        // And it parses with zero errors after the round-trip (still valid RON).
        let parse = doc.parse.as_ref().expect("a parse landed");
        let errors = parse
            .cst
            .diagnostics()
            .iter()
            .filter(|d| d.severity() == Severity::Error)
            .count();
        assert_eq!(errors, 0, "`{name}` is still valid RON after the worker round-trip");
    }
}
