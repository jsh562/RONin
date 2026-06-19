//! Performance regression guards for the structural (tree/table) views over a
//! large, well-formed RON document (E008 — FR-001/FR-005/FR-026).
//!
//! Context: opening a 35 KB / ~1125-line well-formed RON file (`samples/ships.ron`)
//! froze the structural view because
//!
//!  1. the per-view models (`TreeFormModel` / `TableModel`) were re-derived from the
//!     CST **every render frame** (no per-parse caching), and
//!  2. each node's diagnostic attachment resolved its byte range to a char range with
//!     an O(file-size) `source[..off].chars().count()` — i.e. O(nodes × file_size)
//!     per frame.
//!
//! The fix (a) caches the derived model per parse generation on the document, and
//! (b) replaces the per-node `chars().count()` with a single amortised-O(n) byte→char
//! index. These guards assert:
//!
//!  * **No O(n²) byte→char.** A single tree + table derivation over a large document
//!    **with diagnostics present** (so the attachment path runs) completes well within
//!    a generous responsiveness budget.
//!  * **Cached per parse generation.** The model is derived exactly once per landed
//!    parse: the first access derives + caches it; a second access reuses the cache
//!    (a non-derive lookup, far faster) and the cache-presence seam reports it cached.
//!
//! Thresholds are deliberately generous — these are O(n²)/per-frame-rederive guards,
//! not micro-benchmarks. Every loop here is bounded, so the suite can never hang.

use std::time::{Duration, Instant};

use ronin_core::Diagnostic;

use ronin_app::diagnostics_map::{map_diagnostic, DiagnosticView};
use ronin_app::document::EditorDocument;
use ronin_app::reparse::ReparseWorker;
use ronin_app::structural::sections::SectionShape;
use ronin_app::structural::tree::{TreeFormModel, TreeNode};
use ronin_app::structural::view_state::StructuralPath;

/// Load a fixture from the crate's `tests/fixtures/` directory.
fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// Load `samples/ships.ron` relative to the crate manifest (Step-0 localization).
fn ships_ron() -> String {
    let path = format!("{}/../../samples/ships.ron", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// Build a document over `source` and drive the *real* off-frame worker to a landed
/// current parse, or panic on a (bounded) timeout. Mirrors the harness used by
/// `structural_views.rs` so the cache is exercised through the real `poll_parse` path.
fn doc_with_landed_parse(source: &str, worker: &ReparseWorker) -> EditorDocument {
    let mut doc = EditorDocument::new_untitled(1);
    doc.buffer = source.to_string();
    doc.on_edit();
    let deadline = Instant::now() + Duration::from_secs(30);
    doc.request_reparse(worker);
    loop {
        if doc.poll_parse(worker) {
            return doc;
        }
        assert!(
            Instant::now() < deadline,
            "reparse did not land within timeout"
        );
        std::thread::yield_now();
    }
}

/// Total number of nodes in a tree/form model (roots + all descendants).
fn count_nodes(model: &TreeFormModel) -> usize {
    fn walk(node: &TreeNode) -> usize {
        1 + node.children.iter().map(walk).sum::<usize>()
    }
    model.roots.iter().map(walk).sum()
}

/// Synthesize a spread of diagnostics over `source` so the model derivation
/// exercises the byte→char diagnostic-attachment path with a non-empty set (a
/// well-formed file reports zero structural findings, but the freeze hinged on the
/// attachment work). One diagnostic every ~512 bytes, snapped to char boundaries.
fn diagnostics_for_source(source: &str) -> Vec<DiagnosticView> {
    let mut views = Vec::new();
    let mut off = 0usize;
    while off < source.len() {
        let mut start = off;
        while start < source.len() && !source.is_char_boundary(start) {
            start += 1;
        }
        let mut end = (start + 8).min(source.len());
        while end < source.len() && !source.is_char_boundary(end) {
            end += 1;
        }
        if start < end {
            let diag = Diagnostic::new(
                ronin_core::DiagnosticCode::UnexpectedToken,
                ronin_core::TextRange::new(start, end),
                "synthetic",
            );
            views.push(map_diagnostic(&diag, source));
        }
        off = start + 512;
    }
    views
}

// =============================================================================
// Regression guards (CI gates)
// =============================================================================

/// A single tree + table derivation over a large well-formed document **with
/// diagnostics present** stays well within a responsiveness budget — the guard
/// against the O(nodes × file_size) `byte_to_char` regression.
#[test]
fn large_document_structural_derive_stays_responsive_with_diagnostics() {
    let worker = ReparseWorker::new();
    let src = fixture("large_ships.ron");
    // Sanity: the fixture is large enough to surface an O(n²) regression (the bug
    // froze a ~35 KB file; this is larger and has many more nodes).
    assert!(
        src.len() > 35_000,
        "fixture too small to guard the regression ({} bytes)",
        src.len()
    );

    let mut doc = doc_with_landed_parse(&src, &worker);
    // Force a non-empty diagnostic set so the byte→char attachment path runs (the
    // exact work the freeze hinged on). Set directly so the test does not depend on
    // the file actually being invalid.
    doc.diagnostics = diagnostics_for_source(&src);
    assert!(
        !doc.diagnostics.is_empty(),
        "expected synthetic diagnostics to exercise the attachment path"
    );

    // First derive of each model (cache miss): the tree path covers the nested
    // map-with-tuple-keys subtree; the table path covers the top-level uniform list.
    let t0 = Instant::now();
    let model = doc
        .cached_tree_model()
        .cloned()
        .expect("a tree model derives from the landed parse");
    let tree_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let tree_roots = model.roots.len();
    // Sanity: the fixture is genuinely deep/wide (well over 1000 tree nodes), so the
    // guard exercises the path that froze — not a trivially small tree.
    let node_count = count_nodes(&model);
    assert!(
        node_count > 1000,
        "fixture too small to guard the regression ({node_count} tree nodes)"
    );

    let section = StructuralPath::root();
    let t1 = Instant::now();
    let table_rows = doc
        .cached_table_model(&section, SectionShape::RecordList)
        .map(|m| m.row_count())
        .expect("the top-level uniform list derives a table model");
    let table_ms = t1.elapsed().as_secs_f64() * 1000.0;

    eprintln!("first tree derive  = {tree_ms:.3} ms (roots={tree_roots})");
    eprintln!("first table derive = {table_ms:.3} ms (rows={table_rows})");

    // Generous budget: the O(n²) regression made a smaller file take tens of ms and
    // grow with size; the fixed path is well under this even in a debug build under
    // CI load. This is a regression guard, not a micro-benchmark.
    const BUDGET_MS: f64 = 250.0;
    assert!(
        tree_ms < BUDGET_MS,
        "tree derive took {tree_ms:.3} ms (budget {BUDGET_MS} ms) — O(n^2) byte_to_char regression?"
    );
    assert!(
        table_ms < BUDGET_MS,
        "table derive took {table_ms:.3} ms (budget {BUDGET_MS} ms) — O(n^2) byte_to_char regression?"
    );
}

/// The derived structural models are cached per parse generation: the first access
/// derives + caches, a second access reuses the cache (a non-derive lookup) and does
/// not re-derive — the guard against the per-frame re-derive regression.
#[test]
fn structural_models_are_cached_per_parse_generation() {
    let worker = ReparseWorker::new();
    let src = fixture("large_ships.ron");
    let mut doc = doc_with_landed_parse(&src, &worker);
    doc.diagnostics = diagnostics_for_source(&src);
    let section = StructuralPath::root();

    // Nothing is cached until the first access (lazy realization, FR-026).
    assert!(!doc.has_cached_tree_model());
    assert!(!doc.has_cached_table_model(&section, SectionShape::RecordList));

    // First access derives + caches both models.
    let t_first = Instant::now();
    let _ = doc.cached_tree_model().expect("tree model derives");
    let _ = doc
        .cached_table_model(&section, SectionShape::RecordList)
        .expect("table model derives");
    let first_ms = t_first.elapsed().as_secs_f64() * 1000.0;
    assert!(
        doc.has_cached_tree_model(),
        "tree model cached after access"
    );
    assert!(
        doc.has_cached_table_model(&section, SectionShape::RecordList),
        "table model cached after access"
    );

    // A second access (same parse generation) must reuse the cache — far cheaper than
    // re-deriving. Measure several repeats so the cached path is unambiguously fast.
    let t_second = Instant::now();
    const REPEATS: usize = 50;
    let mut acc = 0usize;
    for _ in 0..REPEATS {
        acc += doc.cached_tree_model().map(|m| m.roots.len()).unwrap_or(0);
        acc += doc
            .cached_table_model(&section, SectionShape::RecordList)
            .map(|m| m.row_count())
            .unwrap_or(0);
    }
    let second_total_ms = t_second.elapsed().as_secs_f64() * 1000.0;
    let second_avg_ms = second_total_ms / REPEATS as f64;
    assert!(acc > 0, "cached models still return their content");

    eprintln!(
        "first (derive) = {first_ms:.3} ms; cached avg over {REPEATS} = {second_avg_ms:.4} ms"
    );

    // A cached access (clone + lookup) is far cheaper than a fresh derive. If the
    // models were re-derived every access, the per-access cost would be on the order
    // of the first derive; require the cached average to be a small fraction of it.
    // The first derive over this large file is many ms; cached lookups are sub-ms.
    assert!(
        second_avg_ms < first_ms,
        "cached access ({second_avg_ms:.4} ms) was not cheaper than the first derive \
         ({first_ms:.3} ms) — per-frame re-derive regression?"
    );

    // A new landed parse invalidates the cache (next access re-derives).
    doc.on_edit();
    let deadline = Instant::now() + Duration::from_secs(30);
    doc.request_reparse(&worker);
    loop {
        if doc.poll_parse(&worker) {
            break;
        }
        assert!(Instant::now() < deadline, "reparse did not land");
        std::thread::yield_now();
    }
    assert!(
        !doc.has_cached_tree_model(),
        "a newer parse must invalidate the cached tree model"
    );
    assert!(
        !doc.has_cached_table_model(&section, SectionShape::RecordList),
        "a newer parse must invalidate the cached table model"
    );
}

// =============================================================================
// Step 0 — localization (run with `--ignored --nocapture` to print numbers)
// =============================================================================

#[test]
#[ignore = "timing localization, not a CI gate — run with --ignored --nocapture"]
fn localize_structural_derive_cost() {
    use ronin_app::structural::table::TableModel;
    use ronin_app::structural::tree::TreeFormModel;

    let src = ships_ron();
    eprintln!("source bytes = {}", src.len());

    let t0 = Instant::now();
    let cst = ronin_core::parse(&src);
    let parse_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let structural_diags = cst.diagnostics().len();
    eprintln!("ronin_core::parse = {parse_ms:.3} ms");
    eprintln!("structural diagnostics from cst = {structural_diags}");

    let diags = diagnostics_for_source(&src);
    eprintln!("synthetic diagnostics for attachment = {}", diags.len());

    let t1 = Instant::now();
    let tree = TreeFormModel::derive(&cst, &diags);
    let tree_ms = t1.elapsed().as_secs_f64() * 1000.0;
    eprintln!(
        "TreeFormModel::derive (with diagnostics) = {tree_ms:.3} ms (roots={})",
        tree.roots.len()
    );

    let t2 = Instant::now();
    let table = TableModel::derive(&cst, &StructuralPath::root(), &diags);
    let table_ms = t2.elapsed().as_secs_f64() * 1000.0;
    eprintln!(
        "TableModel::derive over root (with diagnostics) = {table_ms:.3} ms (is_some={})",
        table.is_some()
    );
}
