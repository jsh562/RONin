//! Per-error-kind validator tests (E006/T013, FR-002/FR-005/FR-007).
//!
//! One fixture per FR-002 error kind plus the extra/unknown-field kind, each
//! asserting the EXACT `RON-V####` code, the severity, and a precise byte range.
//! TypeModel JSON fixtures are hand-authored inline (E004 not needed): the
//! `serde_json::json!` schema plus the RON source fully define each case.

use ronin_core::{parse, DiagnosticCode, Severity};
use ronin_validate::validate_against;
use serde_json::{json, Value};

/// Byte range `(start, end)` of the first occurrence of `needle` in `src`.
fn span_of(src: &str, needle: &str) -> (usize, usize) {
    let s = src
        .find(needle)
        .unwrap_or_else(|| panic!("`{needle}` not in source"));
    (s, s + needle.len())
}

/// Run the validator for `src` against `model`'s `type_name` def.
fn validate(model: &Value, type_name: &str, src: &str) -> Vec<ronin_core::Diagnostic> {
    validate_against(model, type_name, &parse(src))
}

/// Assert there is exactly one diagnostic with `code`, `severity`, and the byte
/// range `expect`. Returns it.
fn expect_one(
    diags: &[ronin_core::Diagnostic],
    code: DiagnosticCode,
    severity: Severity,
    expect: (usize, usize),
) {
    let matching: Vec<_> = diags.iter().filter(|d| d.code() == code).collect();
    assert_eq!(
        matching.len(),
        1,
        "expected exactly one {} diagnostic, got: {:?}",
        code.code(),
        diags
            .iter()
            .map(|d| (
                d.code().code(),
                d.range().start(),
                d.range().end(),
                d.message()
            ))
            .collect::<Vec<_>>()
    );
    let d = matching[0];
    assert_eq!(d.severity(), severity, "severity for {}", code.code());
    assert_eq!(
        (d.range().start(), d.range().end()),
        expect,
        "range for {} (msg: {})",
        code.code(),
        d.message()
    );
    assert_eq!(d.code().source(), "ronin-types");
}

#[test]
fn type_mismatch_is_ron_v0001_at_value_span() {
    // Entity.id is an integer; the document gives a string.
    let model = json!({
        "$defs": {
            "Entity": {
                "type": "object",
                "properties": { "id": { "type": "integer" } },
                "required": ["id"]
            }
        }
    });
    let src = "Entity(id: \"oops\")";
    let diags = validate(&model, "Entity", src);
    expect_one(
        &diags,
        DiagnosticCode::TypeMismatch,
        Severity::Error,
        span_of(src, "\"oops\""),
    );
}

#[test]
fn missing_required_is_ron_v0002() {
    // Entity requires `id`; the document omits it.
    let model = json!({
        "$defs": {
            "Entity": {
                "type": "object",
                "properties": { "id": { "type": "integer" }, "name": { "type": "string" } },
                "required": ["id", "name"]
            }
        }
    });
    let src = "Entity(name: \"x\")";
    let diags = validate(&model, "Entity", src);
    // Missing `id` has no node -> attached to the containing struct's value span.
    expect_one(
        &diags,
        DiagnosticCode::MissingRequiredField,
        Severity::Error,
        (0, src.len()),
    );
}

#[test]
fn invalid_enum_variant_is_ron_v0003_at_variant_span() {
    // Kind is an enum with variants None/Active; the document uses `Bogus`.
    let model = json!({
        "$defs": {
            "Kind": {
                "oneOf": [
                    { "x-ron-variant": "None", "x-ron-variant-shape": "unit" },
                    { "x-ron-variant": "Active", "x-ron-variant-shape": "unit" }
                ]
            }
        }
    });
    let src = "Bogus";
    let diags = validate(&model, "Kind", src);
    expect_one(
        &diags,
        DiagnosticCode::InvalidEnumVariant,
        Severity::Error,
        span_of(src, "Bogus"),
    );
}

#[test]
fn wrong_tuple_arity_is_ron_v0004_at_tuple_span() {
    // Coord is a 2-tuple; the document gives 3 elements.
    let model = json!({
        "$defs": {
            "Coord": {
                "type": "array",
                "prefixItems": [ { "type": "number" }, { "type": "number" } ],
                "items": false,
                "x-ron-kind": "tuple",
                "x-ron-tuple-arity": 2
            }
        }
    });
    let src = "(1.0, 2.0, 3.0)";
    let diags = validate(&model, "Coord", src);
    expect_one(
        &diags,
        DiagnosticCode::WrongTupleArity,
        Severity::Error,
        (0, src.len()),
    );
}

#[test]
fn out_of_range_is_ron_v0005_at_value_span() {
    // A bare integer with a maximum of 100; document gives 250.
    let model = json!({
        "$defs": {
            "Level": { "type": "integer", "minimum": 0, "maximum": 100 }
        }
    });
    let src = "250";
    let diags = validate(&model, "Level", src);
    expect_one(
        &diags,
        DiagnosticCode::ValueConstraintViolation,
        Severity::Error,
        span_of(src, "250"),
    );
}

#[test]
fn unknown_field_is_ron_v0006_warning_only_when_deny_unknown_fields() {
    // deny_unknown_fields -> additionalProperties:false. Extra field flagged.
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
    let diags = validate(&strict, "Entity", src);
    expect_one(
        &diags,
        DiagnosticCode::UnknownField,
        Severity::Warning,
        span_of(src, "extra"),
    );

    // Non-strict (no additionalProperties:false) -> extra silently allowed.
    let lax = json!({
        "$defs": {
            "Entity": {
                "type": "object",
                "properties": { "id": { "type": "integer" } },
                "required": ["id"]
            }
        }
    });
    let lax_diags = validate(&lax, "Entity", src);
    assert!(
        lax_diags
            .iter()
            .all(|d| d.code() != DiagnosticCode::UnknownField),
        "non-strict type must not flag extra fields: {lax_diags:?}"
    );
}

#[test]
fn enum_value_constraint_const_is_ron_v0005() {
    // An enum-of-values (string `const`/`enum`) violation maps to V0005.
    let model = json!({
        "$defs": {
            "Mode": { "type": "string", "enum": ["read", "write"] }
        }
    });
    let src = "\"delete\"";
    let diags = validate(&model, "Mode", src);
    expect_one(
        &diags,
        DiagnosticCode::ValueConstraintViolation,
        Severity::Error,
        span_of(src, "\"delete\""),
    );
}

#[test]
fn nested_enum_newtype_variant_payload_validates() {
    // Kind::Id(integer) inside an Entity; document gives Id("x") -> type mismatch
    // on the newtype payload.
    let model = json!({
        "$defs": {
            "Entity": {
                "type": "object",
                "properties": { "kind": { "$ref": "#/$defs/Kind" } },
                "required": ["kind"]
            },
            "Kind": {
                "oneOf": [
                    { "x-ron-variant": "None", "x-ron-variant-shape": "unit" },
                    {
                        "x-ron-variant": "Id",
                        "x-ron-variant-shape": "newtype",
                        "x-ron-payload": { "type": "integer" }
                    }
                ]
            }
        }
    });
    let src = "Entity(kind: Id(\"x\"))";
    let diags = validate(&model, "Entity", src);
    expect_one(
        &diags,
        DiagnosticCode::TypeMismatch,
        Severity::Error,
        span_of(src, "\"x\""),
    );
}

#[test]
fn valid_document_yields_no_diagnostics() {
    let model = json!({
        "$defs": {
            "Entity": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer" },
                    "pos": { "$ref": "#/$defs/Coord" }
                },
                "required": ["id", "pos"],
                "additionalProperties": false
            },
            "Coord": {
                "type": "array",
                "prefixItems": [ { "type": "number" }, { "type": "number" } ],
                "items": false,
                "x-ron-kind": "tuple",
                "x-ron-tuple-arity": 2
            }
        }
    });
    let src = "Entity(id: 1, pos: (1.0, 2.0))";
    let diags = validate(&model, "Entity", src);
    assert!(
        diags.is_empty(),
        "valid document produced diagnostics: {diags:?}"
    );
}
