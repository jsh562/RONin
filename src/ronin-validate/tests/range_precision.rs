//! Range-precision tests (E006/T014 — [COMPLETES FR-002], FR-001/FR-002/FR-003).
//!
//! Multibyte + nested fixtures per error kind, each with an exact oracle. The
//! validator emits **byte** `TextRange`s (consistent with `ronin-core`'s structural
//! diagnostics); these tests assert those byte ranges precisely. As a
//! cross-check, each fixture also verifies the UTF-8 **char** offset oracle that
//! E003's `DiagnosticView` would derive (chars before the byte offset), so
//! multibyte correctness has the same unambiguous oracle as structural
//! diagnostics.

use ronin_core::{parse, DiagnosticCode};
use ronin_validate::validate_against;
use serde_json::{json, Value};

/// Byte range `(start, end)` of the first occurrence of `needle` in `src`.
fn byte_span(src: &str, needle: &str) -> (usize, usize) {
    let s = src
        .find(needle)
        .unwrap_or_else(|| panic!("`{needle}` not in source"));
    (s, s + needle.len())
}

/// The E003-convention char offset for a byte offset: the number of `char`s in
/// `src[..byte]`.
fn char_offset(src: &str, byte: usize) -> usize {
    src.char_indices().take_while(|(i, _)| *i < byte).count()
}

/// Validate and return the single diagnostic with `code` (panics otherwise).
fn one(model: &Value, type_name: &str, src: &str, code: DiagnosticCode) -> ronin_core::Diagnostic {
    let diags = validate_against(model, type_name, &parse(src));
    let mut matching: Vec<_> = diags.into_iter().filter(|d| d.code() == code).collect();
    assert_eq!(
        matching.len(),
        1,
        "expected exactly one {} diagnostic",
        code.code()
    );
    matching.remove(0)
}

/// Assert a diagnostic's byte range equals `byte` AND its char-offset projection
/// (E003 convention) equals the chars-before oracle.
fn assert_precise(src: &str, d: &ronin_core::Diagnostic, byte: (usize, usize)) {
    assert_eq!(
        (d.range().start(), d.range().end()),
        byte,
        "byte range mismatch (msg: {})",
        d.message()
    );
    // Char-offset oracle (what E003's DiagnosticView would compute).
    let expected_chars = (char_offset(src, byte.0), char_offset(src, byte.1));
    let got_chars = (
        char_offset(src, d.range().start()),
        char_offset(src, d.range().end()),
    );
    assert_eq!(got_chars, expected_chars, "char-offset projection mismatch");
}

#[test]
fn type_mismatch_multibyte_nested() {
    // A multibyte sibling field (`café`) precedes the offending value so the
    // byte offset of `id` differs from its char offset.
    let model = json!({
        "$defs": {
            "Doc": {
                "type": "object",
                "properties": {
                    "café": { "type": "string" },
                    "inner": { "$ref": "#/$defs/Inner" }
                },
                "required": ["café", "inner"]
            },
            "Inner": {
                "type": "object",
                "properties": { "id": { "type": "integer" } },
                "required": ["id"]
            }
        }
    });
    let src = "Doc(café: \"naïve\", inner: Inner(id: \"x\"))";
    let d = one(&model, "Doc", src, DiagnosticCode::TypeMismatch);
    let span = byte_span(src, "\"x\"");
    assert_precise(src, &d, span);
    // The multibyte content guarantees byte != char offsets here.
    assert_ne!(span.0, char_offset(src, span.0));
}

#[test]
fn missing_required_nested_after_multibyte() {
    let model = json!({
        "$defs": {
            "Doc": {
                "type": "object",
                "properties": {
                    "tag": { "type": "string" },
                    "inner": { "$ref": "#/$defs/Inner" }
                },
                "required": ["tag", "inner"]
            },
            "Inner": {
                "type": "object",
                "properties": { "id": { "type": "integer" }, "name": { "type": "string" } },
                "required": ["id", "name"]
            }
        }
    });
    // Inner omits `name`; the finding attaches to the inner struct's value span.
    let src = "Doc(tag: \"café-η\", inner: Inner(id: 1))";
    let d = one(&model, "Doc", src, DiagnosticCode::MissingRequiredField);
    let span = byte_span(src, "Inner(id: 1)");
    assert_precise(src, &d, span);
}

#[test]
fn invalid_variant_nested_after_multibyte() {
    let model = json!({
        "$defs": {
            "Doc": {
                "type": "object",
                "properties": {
                    "label": { "type": "string" },
                    "kind": { "$ref": "#/$defs/Kind" }
                },
                "required": ["label", "kind"]
            },
            "Kind": {
                "oneOf": [
                    { "x-ron-variant": "On", "x-ron-variant-shape": "unit" },
                    { "x-ron-variant": "Off", "x-ron-variant-shape": "unit" }
                ]
            }
        }
    });
    let src = "Doc(label: \"日本語\", kind: Whoops)";
    let d = one(&model, "Doc", src, DiagnosticCode::InvalidEnumVariant);
    let span = byte_span(src, "Whoops");
    assert_precise(src, &d, span);
    assert_ne!(span.0, char_offset(src, span.0));
}

#[test]
fn wrong_tuple_arity_nested_after_multibyte() {
    let model = json!({
        "$defs": {
            "Doc": {
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "pos": { "$ref": "#/$defs/Coord" }
                },
                "required": ["name", "pos"]
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
    let src = "Doc(name: \"αβγ\", pos: (1.0, 2.0, 3.0))";
    let d = one(&model, "Doc", src, DiagnosticCode::WrongTupleArity);
    let span = byte_span(src, "(1.0, 2.0, 3.0)");
    assert_precise(src, &d, span);
    assert_ne!(span.0, char_offset(src, span.0));
}

#[test]
fn out_of_range_nested_after_multibyte() {
    let model = json!({
        "$defs": {
            "Doc": {
                "type": "object",
                "properties": {
                    "note": { "type": "string" },
                    "level": { "type": "integer", "minimum": 0, "maximum": 10 }
                },
                "required": ["note", "level"]
            }
        }
    });
    let src = "Doc(note: \"€uro\", level: 99)";
    let d = one(&model, "Doc", src, DiagnosticCode::ValueConstraintViolation);
    let span = byte_span(src, "99");
    assert_precise(src, &d, span);
    assert_ne!(span.0, char_offset(src, span.0));
}

#[test]
fn unknown_field_key_span_after_multibyte() {
    let model = json!({
        "$defs": {
            "Doc": {
                "type": "object",
                "properties": { "title": { "type": "string" } },
                "required": ["title"],
                "additionalProperties": false
            }
        }
    });
    let src = "Doc(title: \"naïveté\", surprise: 1)";
    let d = one(&model, "Doc", src, DiagnosticCode::UnknownField);
    // Key span (field-name) for the unknown-field finding (FR-003).
    let span = byte_span(src, "surprise");
    assert_precise(src, &d, span);
    assert_ne!(span.0, char_offset(src, span.0));
}

#[test]
fn deeply_nested_list_element_type_mismatch() {
    let model = json!({
        "$defs": {
            "Doc": {
                "type": "object",
                "properties": { "items": { "$ref": "#/$defs/Items" } },
                "required": ["items"]
            },
            "Items": { "type": "array", "items": { "type": "integer" } }
        }
    });
    // The 3rd list element (index 2) is a string — multibyte earlier elements
    // shift the byte offset away from the char offset.
    let src = "Doc(items: [1, 2, \"oops\"])";
    let d = one(&model, "Doc", src, DiagnosticCode::TypeMismatch);
    let span = byte_span(src, "\"oops\"");
    assert_precise(src, &d, span);
}
