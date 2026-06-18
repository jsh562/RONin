//! Projection unit tests (E006/T009, FR-003).
//!
//! These verify the CST→JSON projection's instance encoding and, critically, the
//! JSON-Pointer→CST `TextRange` reverse index: pointer→range round-trips on
//! multibyte and nested fixtures, key-span vs value-span selection, the
//! innermost-node rule for nesting, and the invariant that no pointer ever maps
//! to an empty/fabricated range.

use ronin_core::parse;
use ronin_validate::projection::CstJsonProjection;
use serde_json::json;

/// Build a projection for `src`.
fn project(src: &str) -> CstJsonProjection {
    CstJsonProjection::from_document(&parse(src))
}

/// The byte range of the first occurrence of `needle` in `src`, as `(start, end)`.
fn span_of(src: &str, needle: &str) -> (usize, usize) {
    let start = src
        .find(needle)
        .unwrap_or_else(|| panic!("`{needle}` not in source"));
    (start, start + needle.len())
}

/// The byte range of the `nth` (0-based) occurrence of `needle` in `src`.
fn nth_span_of(src: &str, needle: &str, nth: usize) -> (usize, usize) {
    let mut from = 0;
    for _ in 0..nth {
        let i = src[from..]
            .find(needle)
            .map(|p| from + p)
            .unwrap_or_else(|| panic!("not enough `{needle}` in source"));
        from = i + needle.len();
    }
    let start = src[from..]
        .find(needle)
        .map(|p| from + p)
        .unwrap_or_else(|| panic!("not enough `{needle}` in source"));
    (start, start + needle.len())
}

#[test]
fn scalar_literals_project_to_json() {
    assert_eq!(project("1").instance, json!(1));
    assert_eq!(project("-2").instance, json!(-2));
    assert_eq!(project("3.5").instance, json!(3.5));
    assert_eq!(project("true").instance, json!(true));
    assert_eq!(project("false").instance, json!(false));
    assert_eq!(project("\"hello\"").instance, json!("hello"));
    assert_eq!(project("'c'").instance, json!("c"));
    assert_eq!(project("()").instance, json!(null));
}

#[test]
fn int_radix_and_suffix() {
    assert_eq!(project("0xFF").instance, json!(255));
    assert_eq!(project("0o17").instance, json!(15));
    assert_eq!(project("0b1010").instance, json!(10));
    assert_eq!(project("1_000").instance, json!(1000));
    assert_eq!(project("42i32").instance, json!(42));
    assert_eq!(project("7u8").instance, json!(7));
}

#[test]
fn struct_projects_to_object_with_field_spans() {
    let src = "Point(x: 1, y: -2.0)";
    let p = project(src);
    assert_eq!(p.instance, json!({"x": 1, "y": -2.0}));

    // Value spans: the literal `1` and `-2.0`.
    let (vs, ve) = span_of(src, "1");
    let r = p.index.value_range("/x").expect("/x value span");
    assert_eq!((r.start(), r.end()), (vs, ve));

    let (vs2, ve2) = span_of(src, "-2.0");
    let r2 = p.index.value_range("/y").expect("/y value span");
    assert_eq!((r2.start(), r2.end()), (vs2, ve2));

    // Key spans: the field-name tokens.
    let (ks, ke) = span_of(src, "x");
    let rk = p.index.key_range("/x").expect("/x key span");
    assert_eq!((rk.start(), rk.end()), (ks, ke));

    let (ks2, ke2) = span_of(src, "y");
    let rk2 = p.index.key_range("/y").expect("/y key span");
    assert_eq!((rk2.start(), rk2.end()), (ks2, ke2));
}

#[test]
fn root_pointer_maps_to_whole_value() {
    let src = "Point(x: 1)";
    let p = project(src);
    let r = p.index.value_range("").expect("root value span");
    assert_eq!((r.start(), r.end()), (0, src.len()));
}

#[test]
fn nested_struct_innermost_node_rule() {
    let src = "Outer(inner: Inner(deep: 5))";
    let p = project(src);
    assert_eq!(p.instance, json!({"inner": {"deep": 5}}));

    // /inner value span is the whole `Inner(deep: 5)` node.
    let (is, ie) = span_of(src, "Inner(deep: 5)");
    let ri = p.index.value_range("/inner").expect("/inner value span");
    assert_eq!((ri.start(), ri.end()), (is, ie));

    // /inner/deep value span is the innermost `5`.
    let (ds, de) = span_of(src, "5");
    let rd = p
        .index
        .value_range("/inner/deep")
        .expect("/inner/deep value span");
    assert_eq!((rd.start(), rd.end()), (ds, de));

    // Key span for the innermost field.
    let (ks, ke) = span_of(src, "deep");
    let rk = p
        .index
        .key_range("/inner/deep")
        .expect("/inner/deep key span");
    assert_eq!((rk.start(), rk.end()), (ks, ke));
}

#[test]
fn list_and_tuple_index_pointers() {
    let src = "[10, 20, 30]";
    let p = project(src);
    assert_eq!(p.instance, json!([10, 20, 30]));
    let (s1, e1) = span_of(src, "20");
    let r1 = p.index.value_range("/1").expect("/1 value span");
    assert_eq!((r1.start(), r1.end()), (s1, e1));

    let tsrc = "(1, \"two\", 'c')";
    let tp = project(tsrc);
    assert_eq!(tp.instance, json!([1, "two", "c"]));
    let r2 = tp.index.value_range("/2").expect("/2 value span");
    let (s2, e2) = span_of(tsrc, "'c'");
    assert_eq!((r2.start(), r2.end()), (s2, e2));
}

#[test]
fn map_string_and_non_string_keys() {
    let src = "{ \"a\": 1, 2: \"two\" }";
    let p = project(src);
    assert_eq!(p.instance, json!({"a": 1, "2": "two"}));

    // String key value span.
    let (vs, ve) = span_of(src, "1");
    let r = p.index.value_range("/a").expect("/a value span");
    assert_eq!((r.start(), r.end()), (vs, ve));

    // Non-string key stringified to "2"; value span is the `"two"` string.
    let (vs2, ve2) = span_of(src, "\"two\"");
    let r2 = p.index.value_range("/2").expect("/2 value span");
    assert_eq!((r2.start(), r2.end()), (vs2, ve2));
}

#[test]
fn option_some_and_none_unwrap() {
    // Some(x) unwraps to x.
    let p = project("Some(5)");
    assert_eq!(p.instance, json!(5));
    // None projects to null.
    let n = project("None");
    assert_eq!(n.instance, json!(null));
}

#[test]
fn enum_variants_external_tagging() {
    // Bare-ident unit variant -> external-tagged unit.
    assert_eq!(project("Active").instance, json!({"Active": null}));
    // Named newtype tuple (e.g. `Id(7)`) -> external-tagged single payload. The
    // schema-agnostic projection cannot distinguish a tuple-struct from a
    // newtype variant, so it uses the variant encoding; the schema-guided
    // validator path resolves the ambiguity precisely.
    assert_eq!(project("Id(7)").instance, json!({"Id": 7}));
    // Named tuple with multiple elements -> external-tagged array payload.
    assert_eq!(project("Pair(1, 2)").instance, json!({"Pair": [1, 2]}));
    // Brace variant -> external-tagged object payload.
    assert_eq!(
        project("Named { label: \"x\" }").instance,
        json!({"Named": {"label": "x"}})
    );
    // A named struct `Name(field: v)` (struct syntax) drops its name
    // (serde-faithful): it projects to the bare object.
    assert_eq!(
        project("Named(label: \"x\")").instance,
        json!({"label": "x"})
    );
}

#[test]
fn multibyte_field_and_value_spans_are_byte_precise() {
    // A multibyte field name (`café`) and a multibyte string value (`naïve`).
    let src = "Doc(café: \"naïve\", n: 1)";
    let p = project(src);
    assert_eq!(p.instance, json!({"café": "naïve", "n": 1}));

    // Key span: the bytes of `café` (é is 2 bytes).
    let (ks, ke) = span_of(src, "café");
    let rk = p.index.key_range("/café").expect("/café key span");
    assert_eq!((rk.start(), rk.end()), (ks, ke));
    // The recorded span length must equal the UTF-8 byte length of the key.
    assert_eq!(rk.end() - rk.start(), "café".len());

    // Value span: the bytes of the `"naïve"` string literal (with quotes).
    let (vs, ve) = span_of(src, "\"naïve\"");
    let rv = p.index.value_range("/café").expect("/café value span");
    assert_eq!((rv.start(), rv.end()), (vs, ve));

    // The sibling `n: 1` after the multibyte content stays byte-precise.
    let (ns, ne) = nth_span_of(src, "1", 0);
    let rn = p.index.value_range("/n").expect("/n value span");
    assert_eq!((rn.start(), rn.end()), (ns, ne));
}

#[test]
fn no_pointer_maps_to_an_empty_range() {
    // A representative nested + multibyte + map + enum fixture: every recorded
    // span must be non-empty (FR-003 round-trip-faithful invariant).
    let src = "Outer(items: [Some(1), None], méta: { \"k\": Id(3) }, flag: true)";
    let p = project(src);
    assert!(!p.index.is_empty(), "index must record spans");
    // Walk every recorded pointer's spans via the public accessors on a set of
    // pointers we know exist; assert none is empty.
    for ptr in [
        "", "/items", "/items/0", "/items/1", "/méta", "/méta/k", "/flag",
    ] {
        if let Some(spans) = p.index.spans_for(ptr) {
            if let Some(v) = spans.value {
                assert!(!v.is_empty(), "value span for `{ptr}` must be non-empty");
            }
            if let Some(k) = spans.key {
                assert!(!k.is_empty(), "key span for `{ptr}` must be non-empty");
            }
        }
    }
}

#[test]
fn deeply_nested_pointer_round_trip() {
    let src = "A(b: B(c: [C(d: 9)]))";
    let p = project(src);
    assert_eq!(p.instance, json!({"b": {"c": [{"d": 9}]}}));
    // Innermost value `9` at /b/c/0/d.
    let (s, e) = span_of(src, "9");
    let r = p
        .index
        .value_range("/b/c/0/d")
        .expect("/b/c/0/d value span");
    assert_eq!((r.start(), r.end()), (s, e));
    // Innermost key `d`.
    let (ks, ke) = span_of(src, "d");
    let rk = p.index.key_range("/b/c/0/d").expect("/b/c/0/d key span");
    assert_eq!((rk.start(), rk.end()), (ks, ke));
}
