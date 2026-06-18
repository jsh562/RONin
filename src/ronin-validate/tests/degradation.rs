//! Degradation behavior: serde-faithful extras, dedup vs structural, and
//! skip-unparseable (E006/T034, FR-017/FR-018/FR-019) `[COMPLETES FR-019]`.
//!
//! These are the no-false-positive degradation oracles from US3:
//!
//! * **Extra/unknown field (FR-018).** Flagged (`UnknownField`/RON-V0006 Warning)
//!   ONLY for a type modeled `deny_unknown_fields` (`additionalProperties: false`).
//!   A non-strict type allows the extra silently (zero diagnostics).
//! * **Dedup vs structural (FR-017).** A type finding whose range intersects a
//!   structural diagnostic's range is suppressed (structural precedence) by the
//!   public [`validate`] entry / [`dedup_against_structural`]; non-overlapping type
//!   findings are kept; the structural set is never dropped or mutated.
//! * **Skip-unparseable (FR-019).** A document with one malformed region and an
//!   otherwise-valid-but-type-violating remainder yields ZERO cascaded type errors
//!   inside the parse-error span, the expected type diagnostic(s) on the parseable
//!   remainder, and the structural diagnostics still present for the malformed
//!   span.

use ronin_core::{parse, Diagnostic, DiagnosticCode, Severity, TextRange};
use ronin_validate::{dedup_against_structural, validate, validate_against};
use serde_json::{json, Value};

/// Byte range `(start, end)` of the first occurrence of `needle` in `src`.
fn span_of(src: &str, needle: &str) -> (usize, usize) {
    let s = src
        .find(needle)
        .unwrap_or_else(|| panic!("`{needle}` not in source"));
    (s, s + needle.len())
}

/// Render diagnostics compactly for failure messages.
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
// FR-018 — serde-faithful extra/unknown field handling.
// ---------------------------------------------------------------------------

#[test]
fn extra_field_flagged_only_for_deny_unknown_fields() {
    // Strict type: additionalProperties:false -> the extra field is a Warning.
    let strict = json!({
        "$defs": {
            "Entity": {
                "type": "object",
                "properties": { "id": { "type": "integer" } },
                "required": ["id"],
                "additionalProperties": false
            }
        }
    });
    let src = "Entity(id: 1, extra: 2)";
    let doc = parse(src);
    let strict_diags = validate_against(&strict, "Entity", &doc);
    let unknown: Vec<_> = strict_diags
        .iter()
        .filter(|d| d.code() == DiagnosticCode::UnknownField)
        .collect();
    assert_eq!(
        unknown.len(),
        1,
        "strict type must flag the extra field exactly once: {:?}",
        render(&strict_diags)
    );
    assert_eq!(
        unknown[0].severity(),
        Severity::Warning,
        "extra/unknown field severity must be Warning (FR-005/FR-018)"
    );
    assert_eq!(
        (unknown[0].range().start(), unknown[0].range().end()),
        span_of(src, "extra"),
        "extra-field finding attaches to the field-key span"
    );
}

#[test]
fn extra_field_allowed_silently_for_non_strict_type() {
    // Non-strict type: no additionalProperties:false -> the extra is allowed,
    // ZERO diagnostics (FR-018).
    let lax = json!({
        "$defs": {
            "Entity": {
                "type": "object",
                "properties": { "id": { "type": "integer" } },
                "required": ["id"]
            }
        }
    });
    let src = "Entity(id: 1, extra: 2)";
    let doc = parse(src);
    let diags = validate_against(&lax, "Entity", &doc);
    assert!(
        diags.is_empty(),
        "non-strict type must allow extra fields silently (zero diagnostics): {:?}",
        render(&diags)
    );
}

// ---------------------------------------------------------------------------
// FR-017 — dedup vs structural (structural precedence).
// ---------------------------------------------------------------------------

#[test]
fn dedup_suppresses_overlapping_type_findings_keeps_non_overlapping() {
    // Hand-construct type findings + a structural set so the overlap rule is
    // exercised directly against the published `dedup_against_structural` path.
    let structural = vec![Diagnostic::new(
        DiagnosticCode::UnexpectedToken,
        TextRange::new(10, 20),
        "structural error region",
    )];
    let type_diags = vec![
        Diagnostic::new(DiagnosticCode::TypeMismatch, TextRange::new(0, 5), "kept"),
        Diagnostic::new(
            DiagnosticCode::TypeMismatch,
            TextRange::new(12, 18),
            "contained -> suppressed",
        ),
        Diagnostic::new(
            DiagnosticCode::ValueConstraintViolation,
            TextRange::new(18, 25),
            "partial overlap -> suppressed",
        ),
        Diagnostic::new(
            DiagnosticCode::MissingRequiredField,
            TextRange::new(30, 35),
            "kept",
        ),
    ];

    let structural_before = structural.clone();
    let kept = dedup_against_structural(type_diags, &structural);

    assert_eq!(
        kept.len(),
        2,
        "only the two non-overlapping type findings survive: {:?}",
        render(&kept)
    );
    assert_eq!((kept[0].range().start(), kept[0].range().end()), (0, 5));
    assert_eq!((kept[1].range().start(), kept[1].range().end()), (30, 35));
    // Structural set is never mutated by the dedup (FR-017/FR-020).
    assert_eq!(
        structural, structural_before,
        "dedup must not mutate the structural diagnostic set"
    );
}

#[test]
fn dedup_via_public_validate_suppresses_type_finding_over_structural_region() {
    // End-to-end through the public `validate` entry: a type finding whose range
    // overlaps a structural diagnostic's range is suppressed; the structural
    // diagnostics are passed in (and never returned/dropped) by this call.
    //
    // The public `validate` entry validates the projected value against the whole
    // model as a self-contained root schema, so the root itself must carry the
    // constraint (a `$defs`-only model has no root keyword and matches anything).
    // `$defs: {}` is included so the effective schema's internal `$defs` slot is a
    // valid (empty) object rather than absent.
    let model = json!({
        "type": "object",
        "properties": { "id": { "type": "integer" } },
        "$defs": {}
    });
    // `id: "x"` would normally be a TypeMismatch on `"x"`. We declare a structural
    // diagnostic spanning that value's range so the type finding overlaps it.
    let src = "Entity(id: \"x\")";
    let doc = parse(src);
    let (vs, ve) = span_of(src, "\"x\"");
    let structural = [Diagnostic::new(
        DiagnosticCode::UnexpectedToken,
        TextRange::new(vs, ve),
        "structural over the same value",
    )];

    let published = validate(&model, &doc, &structural);
    assert!(
        published.is_empty(),
        "the type finding overlapping the structural region must be suppressed: {:?}",
        render(&published)
    );

    // Control: without an overlapping structural set, the type finding survives.
    let published_no_struct = validate(&model, &doc, &[]);
    assert!(
        published_no_struct
            .iter()
            .any(|d| d.code() == DiagnosticCode::TypeMismatch),
        "without overlap the type finding must survive: {:?}",
        render(&published_no_struct)
    );
}

#[test]
fn dedup_keeps_type_finding_disjoint_from_structural() {
    // A structural diagnostic on an unrelated region must NOT suppress a disjoint
    // type finding.
    let model = json!({
        "type": "object",
        "properties": { "id": { "type": "integer" } },
        "$defs": {}
    });
    let src = "Entity(id: \"x\")";
    let doc = parse(src);
    // Structural diagnostic on `Entity` (the head ident), disjoint from `"x"`.
    let (hs, he) = span_of(src, "Entity");
    let structural = [Diagnostic::new(
        DiagnosticCode::UnexpectedToken,
        TextRange::new(hs, he),
        "structural elsewhere",
    )];
    let published = validate(&model, &doc, &structural);
    assert!(
        published
            .iter()
            .any(|d| d.code() == DiagnosticCode::TypeMismatch),
        "a type finding disjoint from the structural region must be kept: {:?}",
        render(&published)
    );
}

// ---------------------------------------------------------------------------
// FR-019 — skip-unparseable region (the deterministic oracle).
// ---------------------------------------------------------------------------

#[test]
fn skip_unparseable_zero_cascade_in_span_diag_on_remainder_structural_present() {
    // The FR-019 oracle, end to end:
    //   * `id: @bad` is malformed -> a `ronin-core` parse-error node span.
    //   * `name: 7` is a valid-but-type-violating remainder (string expected).
    // Required outcomes:
    //   (1) ZERO type findings land inside the malformed/error span.
    //   (2) the expected type diagnostic IS produced on the parseable remainder.
    //   (3) the structural diagnostics for the malformed span are still present
    //       (FR-019: structural validation still runs; type defers to it).
    let model = json!({
        "$defs": {
            "Entity": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer" },
                    "name": { "type": "string" }
                }
            }
        }
    });
    let src = "Entity(id: @bad, name: 7)";
    let doc = parse(src);

    // (3) The parser produced structural diagnostics for the malformed region.
    let structural = doc.diagnostics();
    assert!(
        !structural.is_empty(),
        "expected `ronin-core` structural diagnostics for the malformed `@bad` region"
    );

    // Derive the malformed-region span from the source so the cascade assertion
    // has a concrete oracle: from `@bad` up to (but not including) `,`.
    let bad_start = src.find('@').expect("`@` present");
    let bad_region = TextRange::new(
        bad_start,
        src[bad_start..]
            .find(',')
            .map(|off| bad_start + off)
            .expect("comma after malformed region"),
    );

    // Validate against the bound type (the raw type set before dedup) and the
    // published set (the same dedup the public `validate` applies, for a bound
    // type name). `validate_against` is the named-binding path; the public root
    // entry constrains only when the model's root carries keywords, so the bound
    // path is used here to exercise a constrained document.
    let type_diags = validate_against(&model, "Entity", &doc);
    let published = dedup_against_structural(type_diags.clone(), structural);

    // (1) No type finding may land inside the malformed span (raw OR published).
    for d in type_diags.iter().chain(published.iter()) {
        let s = d.range().start();
        let e = d.range().end();
        let overlaps = s < bad_region.end() && bad_region.start() < e;
        assert!(
            !overlaps,
            "a type finding cascaded into the unparseable span {bad_region:?}: {:?}",
            (d.code().code(), s, e, d.message())
        );
    }

    // (2) The remainder's violation (`name: 7` is not a string) is reported.
    let (ns, ne) = span_of(src, "7");
    let remainder = published
        .iter()
        .find(|d| d.code() == DiagnosticCode::TypeMismatch);
    let remainder = remainder.unwrap_or_else(|| {
        panic!(
            "expected the parseable remainder's TypeMismatch to survive, got: {:?}",
            render(&published)
        )
    });
    assert_eq!(
        (remainder.range().start(), remainder.range().end()),
        (ns, ne),
        "remainder finding must point at the offending value span"
    );

    // (3) Structural diagnostics are untouched by validation: the slice the parser
    // produced is exactly what we passed in (the public entry never returns or
    // mutates the structural set).
    assert_eq!(
        structural,
        doc.diagnostics(),
        "structural diagnostics must remain present and unmodified"
    );
}

#[test]
fn skip_unparseable_with_multibyte_remainder() {
    // Multibyte variant of the FR-019 oracle: the parseable remainder carries a
    // multibyte field name + value so the surviving finding's span is byte-precise
    // past multibyte content.
    let model = json!({
        "$defs": {
            "Doc": {
                "type": "object",
                "properties": {
                    "naïve": { "type": "string" },
                    "café": { "type": "integer" }
                }
            }
        }
    });
    // `naïve: @oops` is malformed; `café: "str"` violates (integer expected).
    let src = "Doc(naïve: @oops, café: \"str\")";
    let doc = parse(src);
    let structural = doc.diagnostics();
    assert!(
        !structural.is_empty(),
        "expected structural diagnostics for the malformed multibyte region"
    );

    let published = dedup_against_structural(validate_against(&model, "Doc", &doc), structural);
    let (vs, ve) = span_of(src, "\"str\"");
    let mismatch = published
        .iter()
        .find(|d| d.code() == DiagnosticCode::TypeMismatch)
        .unwrap_or_else(|| {
            panic!(
                "expected the multibyte remainder TypeMismatch to survive: {:?}",
                render(&published)
            )
        });
    assert_eq!(
        (mismatch.range().start(), mismatch.range().end()),
        (vs, ve),
        "multibyte remainder finding must be byte-precise"
    );
}

/// Sanity guard so the `Value` import stays meaningful even as cases evolve: an
/// empty model never produces diagnostics regardless of document.
#[test]
fn empty_model_is_structural_only_smoke() {
    let model: Value = json!({});
    let doc = parse("Entity(id: 1)");
    assert!(validate(&model, &doc, &[]).is_empty());
}
