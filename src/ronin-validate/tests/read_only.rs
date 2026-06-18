//! Read-only post-condition (E006/T035, FR-020/FR-022, SC-006) `[COMPLETES FR-022]`.
//!
//! Validation MUST be read-only over **all** inputs and transients — not only the
//! document text. For representative documents + bindings (valid and violating,
//! drawn from the E001 corpus plus a couple of hand-authored fixtures) this suite
//! asserts the verifiable post-conditions of SC-006:
//!
//! * The document's **bytes are byte-identical** before and after a full
//!   validation pass (load-then-validate yields a byte-identical buffer).
//! * Re-printing the CST after the pass (`ronin_core::print`) is unchanged — the CST
//!   itself is not mutated during projection.
//! * The **structural diagnostic slice** handed to `validate` is unchanged after
//!   the call (compared against a clone taken before).
//! * The bound **`TypeModel`** (`serde_json::Value`) is unchanged after the call
//!   (compared against a clone taken before).
//! * Running the pass **twice** yields **identical diagnostics** (determinism / no
//!   hidden mutation that would perturb a second run).

use std::path::{Path, PathBuf};

use ronin_core::{parse, print, CstDocument, Diagnostic, DiagnosticCode};
use ronin_validate::{validate, validate_against};
use serde_json::{json, Value};

fn valid_corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../ronin-core/tests/corpus/valid")
}

/// A handful of representative corpus documents spanning structs/enums/tuples/
/// lists/maps/options/extensions/multibyte/deep-nesting.
fn representative_corpus() -> Vec<(String, String)> {
    let names = [
        "01_struct_named.ron",
        "04_enum_unit_variant.ron",
        "07_tuple_simple.ron",
        "09_list_simple.ron",
        "13_map_nonstring_keys.ron",
        "16_strings_escapes.ron",
        "22_option_some_none.ron",
        "26_extension_unknown.ron",
        "30_deep_mixed.ron",
        "33_crlf_line_endings.ron",
    ];
    let dir = valid_corpus_dir();
    names
        .iter()
        .map(|name| {
            let path = dir.join(name);
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            ((*name).to_owned(), src)
        })
        .collect()
}

/// A small set of (model, type_name, source) bindings: each pairs a
/// hand-authored model with a document that is either valid or violating, so the
/// read-only post-condition is checked on both clean and diagnostic-producing
/// passes.
fn hand_authored_bindings() -> Vec<(&'static str, Value, &'static str, &'static str)> {
    let entity = json!({
        "$defs": {
            "Entity": {
                "type": "object",
                "properties": { "id": { "type": "integer" }, "name": { "type": "string" } },
                "required": ["id"],
                "additionalProperties": false
            }
        }
    });
    let multibyte = json!({
        "$defs": {
            "Doc": {
                "type": "object",
                "properties": { "café": { "type": "integer" } }
            }
        }
    });
    vec![
        // Valid document -> a clean pass.
        (
            "Entity",
            entity.clone(),
            "Entity",
            "Entity(id: 1, name: \"ok\")",
        ),
        // Violating document -> a diagnostic-producing pass.
        ("Entity", entity, "Entity", "Entity(id: \"oops\", extra: 9)"),
        // Multibyte violating document.
        ("Doc", multibyte, "Doc", "Doc(café: \"str\")"),
    ]
}

/// Assert the full read-only post-condition for one (model, type_name, doc) and
/// return the resulting diagnostics so the caller can also assert determinism.
fn assert_read_only_validate_against(
    label: &str,
    model: &Value,
    type_name: &str,
    src: &str,
) -> Vec<Diagnostic> {
    let doc = parse(src);

    // Snapshots BEFORE the pass.
    let bytes_before = print(&doc);
    let cst_text_before = doc.root().text();
    let structural_before: Vec<Diagnostic> = doc.diagnostics().to_vec();
    let model_before = model.clone();

    // First pass through both public surfaces (named-def + dedup entry).
    let first = validate_against(model, type_name, &doc);
    let _ = validate(model, &doc, doc.diagnostics());

    // Bytes byte-identical (SC-006): re-print equals the original source AND the
    // pre-pass print.
    let bytes_after = print(&doc);
    assert_eq!(
        bytes_after, src,
        "[{label}] document bytes changed after validation (vs source)"
    );
    assert_eq!(
        bytes_after, bytes_before,
        "[{label}] re-print after validation differs from re-print before"
    );

    // The CST node text is unchanged (no projection mutation).
    assert_eq!(
        doc.root().text(),
        cst_text_before,
        "[{label}] CST text changed after validation"
    );

    // The structural diagnostic slice is unchanged.
    assert_eq!(
        doc.diagnostics(),
        structural_before.as_slice(),
        "[{label}] structural diagnostic set changed after validation"
    );

    // The bound TypeModel is unchanged.
    assert_eq!(
        model, &model_before,
        "[{label}] bound TypeModel mutated by validation"
    );

    // Determinism: a second pass on a freshly-parsed identical document yields
    // identical diagnostics (same codes + ranges + messages).
    let doc2 = parse(src);
    let second = validate_against(model, type_name, &doc2);
    assert_eq!(
        diag_key(&first),
        diag_key(&second),
        "[{label}] validation is not deterministic across passes"
    );

    first
}

/// A comparable key for a diagnostic set (code + range + message), order-sensitive
/// — determinism includes stable ordering.
fn diag_key(diags: &[Diagnostic]) -> Vec<(&'static str, usize, usize, String)> {
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

#[test]
fn corpus_documents_are_byte_identical_after_pass_no_binding() {
    // Read-only must hold even on the NoBinding (null-model) path over the corpus:
    // no mutation, byte-identical, structural set preserved.
    for (name, src) in representative_corpus() {
        let doc: CstDocument = parse(&src);
        let bytes_before = print(&doc);
        let structural_before: Vec<Diagnostic> = doc.diagnostics().to_vec();

        let model = Value::Null;
        let model_before = model.clone();
        let diags = validate(&model, &doc, doc.diagnostics());

        assert!(
            diags.is_empty(),
            "[{name}] NoBinding pass produced diagnostics: {:?}",
            diag_key(&diags)
        );
        assert_eq!(
            print(&doc),
            src,
            "[{name}] bytes changed after NoBinding pass"
        );
        assert_eq!(
            print(&doc),
            bytes_before,
            "[{name}] re-print changed after NoBinding pass"
        );
        assert_eq!(
            doc.diagnostics(),
            structural_before.as_slice(),
            "[{name}] structural set changed after NoBinding pass"
        );
        assert_eq!(model, model_before, "[{name}] null model mutated");
    }
}

#[test]
fn hand_authored_bindings_are_read_only_and_deterministic() {
    let bindings = hand_authored_bindings();
    let mut total = 0usize;
    let mut producing = 0usize;
    for (label, model, type_name, src) in &bindings {
        let diags = assert_read_only_validate_against(label, model, type_name, src);
        total += 1;
        if !diags.is_empty() {
            producing += 1;
        }
    }
    // The set must include at least one clean pass AND at least one
    // diagnostic-producing pass so read-only is proven on both kinds of run.
    assert!(total >= 3, "expected >=3 representative bindings");
    assert!(
        producing >= 1,
        "expected at least one diagnostic-producing pass among the bindings"
    );
    assert!(
        producing < total,
        "expected at least one clean (zero-diagnostic) pass among the bindings"
    );
}

#[test]
fn violating_corpus_binding_is_read_only() {
    // Bind a corpus document to a constrained type that it violates, and assert
    // the full read-only post-condition on that (diagnostic-producing) corpus pass.
    let src = std::fs::read_to_string(valid_corpus_dir().join("01_struct_named.ron"))
        .expect("read 01_struct_named.ron");
    // Force a violation: require a field the struct doesn't have.
    let model = json!({
        "$defs": {
            "Forced": {
                "type": "object",
                "properties": { "definitely_absent_field": { "type": "integer" } },
                "required": ["definitely_absent_field"]
            }
        }
    });
    let doc = parse(&src);
    let model_before = model.clone();
    let structural_before: Vec<Diagnostic> = doc.diagnostics().to_vec();

    let diags = validate_against(&model, "Forced", &doc);
    assert!(
        diags
            .iter()
            .any(|d| d.code() == DiagnosticCode::MissingRequiredField),
        "expected a forced MissingRequiredField on the corpus doc: {:?}",
        diag_key(&diags)
    );

    assert_eq!(
        print(&doc),
        src,
        "corpus bytes changed after violating pass"
    );
    assert_eq!(model, model_before, "model mutated after violating pass");
    assert_eq!(
        doc.diagnostics(),
        structural_before.as_slice(),
        "structural set changed after violating pass"
    );
}
