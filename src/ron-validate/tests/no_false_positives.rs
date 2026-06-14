//! No-false-positive corpus (E006/T033, FR-015/FR-016, SC-004) `[COMPLETES FR-016]`.
//!
//! Principle III is non-negotiable: with no type info, an empty `TypeModel`, or a
//! partial/`unknown` type, validation MUST raise **exactly zero** type
//! diagnostics — never "few", never "minimal", a literal count of zero (SC-004).
//! This suite is the pass/fail oracle for that guarantee, exercised across the
//! whole E001 valid corpus plus hand-authored multibyte + deeply-nested fixtures.
//!
//! Three branches mirror SC-004's enumerated cases:
//!
//! * **(a) no-match / NoBinding** — the document has no binding, so there is
//!   nothing to validate against. Modeled as the public [`validate`] entry called
//!   with a `null` model (the NoBinding/structural-only path, FR-015).
//! * **(b) empty `TypeModel`** — a binding resolves but the model is empty
//!   (`{}` / empty `$defs`). The validator MUST treat this as structural-only
//!   (FR-015).
//! * **(c) partial / `unknown`** — the bound type (or a field within a known
//!   parent) is `x-ron-kind: "unknown"`, the unconstrained def. Its subtree is
//!   left unconstrained (FR-016) while sibling/ancestor resolved constraints still
//!   apply.
//!
//! Branch (c) carries a **mandatory positive control**: a sibling field whose
//! value *does* violate its resolved type MUST still emit its `RON-V####`, so a
//! green run cannot mask a silently-broken (trivially-silent) validator.
//!
//! Each corpus document is parsed with `ron_core::parse`; documents that the
//! parser flags as structurally malformed are skipped *for the count assertion's
//! benefit* only via the public `validate` dedup against the structural set — but
//! the E001 *valid* corpus is structurally clean, so in practice every swept file
//! contributes a clean parseable instance. The sweep count is printed so the
//! reviewer can confirm the corpus was actually walked (not silently empty).

use std::path::{Path, PathBuf};

use ron_core::{parse, CstDocument, Diagnostic, DiagnosticCode};
use ron_validate::{validate, validate_against};
use serde_json::{json, Value};

/// Absolute path to the E001 valid corpus directory, resolved from this crate's
/// manifest dir so the test is location-independent.
fn valid_corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../ron-core/tests/corpus/valid")
}

/// Read every `*.ron` file in the E001 valid corpus, returning `(name, source)`
/// pairs sorted by file name for deterministic iteration.
fn read_valid_corpus() -> Vec<(String, String)> {
    let dir = valid_corpus_dir();
    let mut files: Vec<(String, String)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read corpus dir {}: {e}", dir.display()))
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            // Accept any file ending in `.ron` (covers `36_large_scene.scn.ron`).
            if path.extension().and_then(|e| e.to_str()) != Some("ron") {
                return None;
            }
            let name = path.file_name()?.to_string_lossy().into_owned();
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            Some((name, src))
        })
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(
        files.len() >= 35,
        "expected the full E001 valid corpus (>=35 files), found {} in {}",
        files.len(),
        dir.display()
    );
    files
}

/// Hand-authored multibyte + deeply-nested fixtures added on top of the corpus so
/// every branch covers those two named conditions explicitly (FR-003 / SC-004),
/// independent of which corpus files happen to be multibyte/nested.
fn extra_fixtures() -> Vec<(&'static str, &'static str)> {
    vec![
        // Multibyte: non-ASCII field names + string values + a char.
        (
            "extra_multibyte",
            "Café(naïve: \"résumé ☃\", emoji: '🦀', tags: {\"clé\": \"é\"})",
        ),
        // Deeply nested: structs in lists in maps in options, several levels down.
        (
            "extra_deeply_nested",
            "World(layers: [Layer(cells: {\"a\": Some([Node(kids: [Leaf(v: 1)])])})])",
        ),
    ]
}

/// All documents the branches sweep: the E001 valid corpus plus the extra
/// multibyte/nested fixtures, each parsed to a `CstDocument`.
fn all_docs() -> Vec<(String, CstDocument)> {
    let mut docs: Vec<(String, CstDocument)> = read_valid_corpus()
        .into_iter()
        .map(|(name, src)| (name, parse(&src)))
        .collect();
    for (name, src) in extra_fixtures() {
        docs.push((name.to_owned(), parse(src)));
    }
    docs
}

/// Render diagnostics compactly for assertion failure messages.
fn render(diags: &[Diagnostic]) -> Vec<(&'static str, usize, usize, String)> {
    diags
        .iter()
        .map(|d| {
            (
                d.code().code(),
                d.range().start(),
                d.range().end(),
                d.message().to_owned(),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Branch (a): no-match / NoBinding -> structural-only, zero type diagnostics.
// ---------------------------------------------------------------------------

#[test]
fn branch_a_no_binding_yields_zero_type_diagnostics() {
    let docs = all_docs();
    let mut swept = 0usize;
    // The NoBinding path: the public entry called with a `null` model. The
    // structural set is the document's own parse diagnostics (the valid corpus is
    // clean, so it is empty — but we pass it through the real public API).
    let null_model = Value::Null;
    for (name, doc) in &docs {
        let structural = doc.diagnostics();
        let diags = validate(&null_model, doc, structural);
        assert!(
            diags.is_empty(),
            "branch (a) NoBinding produced type diagnostics for `{name}`: {:?}",
            render(&diags)
        );
        swept += 1;
    }
    eprintln!("[no_false_positives] branch (a) NoBinding swept {swept} documents");
    assert!(
        swept >= 37,
        "expected the corpus + extras swept, got {swept}"
    );
}

// ---------------------------------------------------------------------------
// Branch (b): empty TypeModel (binding resolves, model empty) -> zero.
// ---------------------------------------------------------------------------

#[test]
fn branch_b_empty_type_model_yields_zero_type_diagnostics() {
    let docs = all_docs();
    let mut swept = 0usize;

    // Two flavors of "empty": an empty object `{}`, and an object whose `$defs`
    // map is empty. Both must degrade to structural-only (FR-015).
    let empty_obj = json!({});
    let empty_defs = json!({ "$defs": {} });

    for (name, doc) in &docs {
        let structural = doc.diagnostics();

        let diags = validate(&empty_obj, doc, structural);
        assert!(
            diags.is_empty(),
            "branch (b) empty `{{}}` model produced type diagnostics for `{name}`: {:?}",
            render(&diags)
        );

        // Empty `$defs`: validate the root against an empty model, AND validate by
        // an absent named def (the typical binding path with a missing type).
        let diags_defs = validate(&empty_defs, doc, structural);
        assert!(
            diags_defs.is_empty(),
            "branch (b) empty `$defs` model produced type diagnostics for `{name}`: {:?}",
            render(&diags_defs)
        );

        let diags_named = validate_against(&empty_defs, "AnyMissingType", doc);
        assert!(
            diags_named.is_empty(),
            "branch (b) empty `$defs` named lookup produced type diagnostics for `{name}`: {:?}",
            render(&diags_named)
        );

        swept += 1;
    }
    eprintln!("[no_false_positives] branch (b) empty TypeModel swept {swept} documents");
    assert!(
        swept >= 37,
        "expected the corpus + extras swept, got {swept}"
    );
}

// ---------------------------------------------------------------------------
// Branch (c): partial / `unknown` types -> unconstrained subtree, zero.
// ---------------------------------------------------------------------------

/// A model whose single bound type is the unconstrained `unknown` def. Validating
/// any document against this MUST yield zero diagnostics regardless of shape
/// (FR-016).
fn unknown_root_model() -> Value {
    json!({
        "$defs": {
            "Unknown": { "x-ron-kind": "unknown" }
        }
    })
}

#[test]
fn branch_c_unknown_bound_type_yields_zero_type_diagnostics() {
    let model = unknown_root_model();
    let docs = all_docs();
    let mut swept = 0usize;
    for (name, doc) in &docs {
        // Bind every corpus doc to the `unknown` def: its whole subtree is
        // unconstrained, so no shape can produce a finding.
        let diags = validate_against(&model, "Unknown", doc);
        assert!(
            diags.is_empty(),
            "branch (c) `unknown`-bound produced type diagnostics for `{name}`: {:?}",
            render(&diags)
        );
        swept += 1;
    }
    eprintln!("[no_false_positives] branch (c) `unknown`-bound swept {swept} documents");
    assert!(
        swept >= 37,
        "expected the corpus + extras swept, got {swept}"
    );
}

/// Branch (c), scoping case: a known parent struct with one field bound to an
/// `unknown` type and a sibling field bound to a resolved type. The `unknown`
/// field's subtree is unconstrained; the sibling's constraint still applies.
///
/// Here the sibling value is *correct*, so the whole document must validate clean:
/// the `unknown` field does not spuriously fire and the resolved sibling does not
/// fire either (it conforms).
#[test]
fn branch_c_unknown_field_scopes_to_subtree_siblings_clean() {
    let model = json!({
        "$defs": {
            "Parent": {
                "type": "object",
                "properties": {
                    // Unconstrained subtree: anything goes here.
                    "blob": { "x-ron-kind": "unknown" },
                    // Resolved sibling: must be an integer.
                    "count": { "type": "integer" }
                },
                "required": ["count"]
            }
        }
    });

    // The `blob` field holds a wildly-shaped, multibyte, deeply-nested value that
    // would violate almost any real schema — but it is `unknown`, so it is fine.
    // `count` is a correct integer.
    let src = "Parent(blob: Wild(naïve: [Some('🦀'), {\"é\": (1, 2.0, true)}]), count: 7)";
    let doc = parse(src);
    let diags = validate_against(&model, "Parent", &doc);
    assert!(
        diags.is_empty(),
        "the `unknown` subtree leaked a finding or the clean sibling fired: {:?}",
        render(&diags)
    );
}

/// Branch (c) **positive control (mandatory, SC-004)**: same parent shape, but the
/// resolved sibling field's value DOES violate its type. The validator MUST still
/// emit exactly the expected `RON-V####` for the sibling — proving the suite is
/// not trivially silent. The `unknown` subtree still contributes zero.
#[test]
fn branch_c_positive_control_resolved_sibling_violation_still_fires() {
    let model = json!({
        "$defs": {
            "Parent": {
                "type": "object",
                "properties": {
                    "blob": { "x-ron-kind": "unknown" },
                    "count": { "type": "integer" }
                },
                "required": ["count"]
            }
        }
    });

    // `blob` is again an arbitrary unconstrained value; `count` is a string where
    // an integer is required -> the resolved sibling MUST still be flagged.
    let src = "Parent(blob: Anything(deep: [1, 2, {\"k\": 'x'}]), count: \"not-an-int\")";
    let doc = parse(src);
    let diags = validate_against(&model, "Parent", &doc);

    let mismatches: Vec<_> = diags
        .iter()
        .filter(|d| d.code() == DiagnosticCode::TypeMismatch)
        .collect();
    assert_eq!(
        mismatches.len(),
        1,
        "positive control: expected exactly one TypeMismatch on the resolved sibling, got: {:?}",
        render(&diags)
    );

    // The finding must land on the sibling's value span, never inside the
    // `unknown` blob's subtree.
    let needle = "\"not-an-int\"";
    let start = src.find(needle).expect("sibling value present");
    let expected = (start, start + needle.len());
    let d = mismatches[0];
    assert_eq!(
        (d.range().start(), d.range().end()),
        expected,
        "positive control finding must point at the resolved sibling's value span (msg: {})",
        d.message()
    );

    // Nothing may fire inside the `unknown` blob's span.
    let blob_start = src.find("Anything").expect("blob present");
    let blob_end = src.find(", count").expect("blob end present");
    for d in &diags {
        let s = d.range().start();
        assert!(
            !(s >= blob_start && s < blob_end),
            "a finding leaked into the `unknown` subtree: {:?}",
            render(&diags)
        );
    }
}

/// Branch (c), multibyte + nested positive scoping: an `unknown` field nested
/// under multibyte keys, with a resolved sibling that is correct -> zero. Confirms
/// the unconstrained scoping holds under multibyte/nesting too (FR-003/SC-004).
#[test]
fn branch_c_unknown_scoping_multibyte_nested_clean() {
    let model = json!({
        "$defs": {
            "Outer": {
                "type": "object",
                "properties": {
                    "inner": { "$ref": "#/$defs/Inner" }
                },
                "required": ["inner"]
            },
            "Inner": {
                "type": "object",
                "properties": {
                    "café": { "x-ron-kind": "unknown" },
                    "naïve": { "type": "string" }
                },
                "required": ["naïve"]
            }
        }
    });
    let src = "Outer(inner: Inner(café: [Some({\"é\": 1}), '🦀'], naïve: \"résumé\"))";
    let doc = parse(src);
    let diags = validate_against(&model, "Outer", &doc);
    assert!(
        diags.is_empty(),
        "multibyte/nested `unknown` scoping leaked a finding: {:?}",
        render(&diags)
    );
}
