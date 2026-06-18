//! RON→JSON + JSONC + loss-map + emit-convention tests (E010 US1 — T006,
//! FR-001/004/015, SC-002).
//!
//! Per-`LossKind` fixtures (at least one per lossy construct), the canonical-
//! RON-literal non-string-key encoding, and the external-tag enum default. All
//! assertions key on the **stable `RON-I####` codes + `LossKind`**, never on the
//! human-readable `detail` wording (plan "Snapshot vs assertion scope").

use proptest::prelude::*;
use ronin_core::parse;
use ronin_app::interop::{
    render_json, ron_to_json, CommentMode, JsoncStyle, LossKind, LossRecovery, RonToJson,
};

/// Convert `src` RON→JSON with JSONC comments, unbound (the deterministic
/// best-effort path). Unbound is the US1 default — the binding path is US2.
fn convert(src: &str) -> RonToJson {
    let doc = parse(src);
    ron_to_json(&doc, None, CommentMode::JsoncInline)
}

/// Convert with a chosen comment mode (for the dropped-comment fixture).
fn convert_mode(src: &str, mode: CommentMode) -> RonToJson {
    let doc = parse(src);
    ron_to_json(&doc, None, mode)
}

/// The count of a given loss kind in a conversion's report.
fn count(r: &RonToJson, kind: LossKind) -> usize {
    r.loss_report.count_of(kind)
}

// ===========================================================================
// Per-LossKind fixtures (FR-004) — one dedicated fixture per lossy construct.
// Each asserts the construct's stable RON-I#### code is present in the report.
// ===========================================================================

#[test]
fn fixture_struct_name_loss() {
    let r = convert("Player(hp: 10)");
    assert_eq!(count(&r, LossKind::StructName), 1);
    // The reported construct carries the stable code (not the detail wording).
    assert!(r
        .loss_report
        .constructs()
        .iter()
        .any(|c| c.code() == "RON-I0001"));
}

#[test]
fn fixture_tuple_vs_list_loss() {
    let r = convert("(t: (1, 2))");
    assert_eq!(count(&r, LossKind::TupleVsList), 1);
    // The tuple is emitted as a JSON array (the value mapping).
    assert_eq!(
        r.value.get("t"),
        Some(&serde_json::json!([1, 2])),
        "tuple → array"
    );
}

#[test]
fn fixture_char_loss() {
    let r = convert("(c: 'x')");
    assert_eq!(count(&r, LossKind::Char), 1);
    assert_eq!(
        r.value.get("c"),
        Some(&serde_json::json!("x")),
        "char → string"
    );
}

#[test]
fn fixture_enum_tagging_loss_external_tag_default() {
    // Unbound: a named variant keeps the deterministic external-tag default
    // `{"V": payload}` (FR-015) and is reported as an enum-tagging loss.
    let r = convert("(state: Running(5))");
    assert_eq!(count(&r, LossKind::EnumTagging), 1);
    assert_eq!(
        r.value.get("state"),
        Some(&serde_json::json!({ "Running": 5 })),
        "external-tag is the unbound default"
    );
}

#[test]
fn fixture_non_string_key_canonical_literal() {
    // The non-string tuple key is emitted as its CANONICAL RON literal (FR-015):
    // `(1,  2)` (double space) canonicalizes to `(1, 2)`.
    let r = convert("{ (1,  2): \"a\" }");
    assert_eq!(count(&r, LossKind::NonStringKey), 1);
    let obj = r.value.as_object().expect("root object");
    assert!(
        obj.contains_key("(1, 2)"),
        "key canonicalized to `(1, 2)`, keys = {:?}",
        obj.keys().collect::<Vec<_>>()
    );
    assert!(
        !obj.contains_key("(1,  2)"),
        "verbatim spacing must not survive"
    );
}

#[test]
fn fixture_integer_key_canonical_literal() {
    let r = convert("{ 3: \"b\" }");
    assert_eq!(count(&r, LossKind::NonStringKey), 1);
    assert!(r.value.as_object().unwrap().contains_key("3"));
}

#[test]
fn fixture_unit_vs_null_loss() {
    let r = convert("(u: ())");
    assert_eq!(count(&r, LossKind::UnitVsNull), 1);
    assert_eq!(
        r.value.get("u"),
        Some(&serde_json::Value::Null),
        "unit → null"
    );
}

#[test]
fn fixture_raw_string_loss() {
    let r = convert("(s: r#\"raw\"#)");
    assert_eq!(count(&r, LossKind::RawString), 1);
    assert_eq!(
        r.value.get("s"),
        Some(&serde_json::json!("raw")),
        "raw → string value"
    );
}

#[test]
fn fixture_trailing_comma_loss() {
    let r = convert("[1, 2,]");
    assert_eq!(count(&r, LossKind::TrailingComma), 1);
}

#[test]
fn fixture_dropped_comment_loss_only_in_pure_json() {
    // JSONC carries the comment → no drop; pure standard JSON drops it → reported.
    let jsonc = convert_mode("// c\n(x: 1)", CommentMode::JsoncInline);
    assert_eq!(count(&jsonc, LossKind::DroppedComment), 0);
    let pure = convert_mode("// c\n(x: 1)", CommentMode::None);
    assert_eq!(count(&pure, LossKind::DroppedComment), 1);
}

// ===========================================================================
// Recovery flag (HINT-004 / STF-001) — lossy ≠ unrecoverable.
// ===========================================================================

#[test]
fn expanded_tier_kinds_are_lossy_to_external_when_unbound() {
    // Unbound: tuple/char are lossy-to-external (no bound type to recover them).
    let r = convert("(t: (1, 2), c: 'x')");
    for c in r.loss_report.constructs() {
        if matches!(c.kind(), LossKind::TupleVsList | LossKind::Char) {
            assert_eq!(
                c.recovery(),
                LossRecovery::LossyToExternal,
                "{:?} is lossy-to-external when unbound",
                c.kind()
            );
        }
    }
}

#[test]
fn every_reported_loss_keeps_a_real_source_range() {
    // No fabricated/empty ranges — data-model §LossyConstruct.
    let r = convert("Player(t: (1, 2), c: 'x', u: (), s: r#\"r\"#, m: { 1: \"a\", }, e: Run)");
    assert!(!r.loss_report.is_empty());
    for c in r.loss_report.constructs() {
        assert!(
            !c.source_range().is_empty(),
            "{:?} has an empty span",
            c.kind()
        );
    }
}

// ===========================================================================
// JSONC output convention — comments survive, value is faithful.
// ===========================================================================

#[test]
fn jsonc_output_preserves_a_comment() {
    let r = convert("// header\n(a: 1)");
    let text = render_json(&r.value, &r.comments, 2, JsoncStyle::Jsonc);
    assert!(text.contains("// header"), "JSONC preserves the comment");
    assert!(text.contains("\"a\": 1"));
}

#[test]
fn strict_output_drops_inline_comments() {
    let r = convert_mode("// header\n(a: 1)", CommentMode::None);
    let text = render_json(&r.value, &r.comments, 2, JsoncStyle::Strict);
    assert!(
        !text.contains("//"),
        "strict JSON carries no inline comments"
    );
}

#[test]
fn round_trip_safe_base_tier_reports_no_losses() {
    // Scalars + string-keyed map + list: the type-agnostic base tier (FR-011).
    let r = convert("(n: 1, s: \"x\", l: [1, 2], m: {\"k\": 3})");
    assert!(
        r.loss_report.is_empty(),
        "base-tier doc reports no losses: {:?}",
        r.loss_report
            .constructs()
            .iter()
            .map(|c| c.kind())
            .collect::<Vec<_>>()
    );
    assert!(!r.loss_report.requires_confirmation());
}

// ===========================================================================
// Property tests — canonical key encoding + stable-code invariant.
// ===========================================================================

proptest! {
    /// Any anonymous tuple of two integers used as a map key encodes to the
    /// canonical `(a, b)` literal regardless of source spacing, and is reported as
    /// exactly one NonStringKey loss (FR-015, SC-002).
    #[test]
    fn prop_tuple_key_canonicalizes_and_is_reported(a in -1000i64..1000, b in -1000i64..1000) {
        // Vary the source spacing; the canonical literal is spacing-independent.
        let src = format!("{{ ({a},  {b}) : 1 }}");
        let r = convert(&src);
        prop_assert_eq!(count(&r, LossKind::NonStringKey), 1);
        let expected = format!("({a}, {b})");
        prop_assert!(
            r.value.as_object().unwrap().contains_key(&expected),
            "canonical key `{}` expected, keys = {:?}",
            expected,
            r.value.as_object().unwrap().keys().collect::<Vec<_>>()
        );
    }

    /// Every reported construct in any conversion carries a code in the stable
    /// `RON-I####` namespace tagged `"ronin-interop"` (SC-002).
    #[test]
    fn prop_every_loss_has_a_stable_ron_i_code(n in 1usize..5) {
        // A doc with `n` tuples + `n` chars (both reported).
        let mut body = String::new();
        for i in 0..n {
            body.push_str(&format!("t{i}: ({i}, {i}), c{i}: 'x', "));
        }
        let src = format!("({body})");
        let r = convert(&src);
        prop_assert!(!r.loss_report.is_empty());
        for c in r.loss_report.constructs() {
            prop_assert!(c.code().starts_with("RON-I"), "code `{}` in namespace", c.code());
            prop_assert_eq!(c.kind().source(), "ronin-interop");
        }
    }
}
