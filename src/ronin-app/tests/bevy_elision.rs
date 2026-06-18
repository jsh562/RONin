//! Integration + property tests for the Bevy defaults-elision core (E009 US3
//! cluster 5A — T030/T031, FR-014/FR-015/FR-016, SC-005/SC-006).
//!
//! Drives [`reduce_verbosity`] / [`expand_to_explicit`]
//! (`ronin_app::bevy::elision`) over the real verbose `.scn.ron` fixture against
//! the hand-authored **defaults-carrying** registry export
//! (`tests/fixtures/bevy_registry_defaults.json`). The registry is parsed exactly
//! as production does (`BevyRegistry::from_schema_json`) so the per-type concrete
//! defaults are available for the provable-default rule.
//!
//! T030 `[COMPLETES FR-014]` — the provable-default rule: the bit-for-bit float
//! equality (`1.0`/`1.00`/`1e0` all elidable against `1.0`, no epsilon), unknown /
//! no-`Default` / unregistered types left explicit, the no-op (zero-byte) case,
//! and unparseable spans skipped.
//!
//! T031 `[COMPLETES FR-016]` — lossless / stability: untouched regions byte-for-
//! byte preserved (comments / order / trailing commas), shrink→expand→shrink
//! byte-identical, and the transform is a pure CST→CST single document.

use std::path::PathBuf;

use proptest::prelude::*;
use ronin_core::{ast, parse, print, CstDocument};
use ronin_types::BevyRegistry;
use ronin_app::bevy::{
    expand_to_explicit, reduce_verbosity, ron_value_equals_json, ElisionOutcome, SceneModel, Scope,
    SkipReason,
};
use serde_json::json;

// ---------------------------------------------------------------------------
// Fixture loading
// ---------------------------------------------------------------------------

/// Load the verbose scene fixture source text.
fn verbose_scene() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("scenes")
        .join("verbose_defaults.scn.ron");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Load + parse the defaults-carrying registry fixture.
fn defaults_registry() -> BevyRegistry {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("bevy_registry_defaults.json");
    let json =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let (registry, diags) = BevyRegistry::from_schema_json(&json, "test", "<defaults>");
    assert!(diags.is_empty(), "defaults fixture parses clean: {diags:?}");
    registry
}

/// Shrink the verbose-scene fixture once; return (original-src, shrunk-doc, outcome).
fn shrink_fixture() -> (String, CstDocument, ElisionOutcome) {
    let src = verbose_scene();
    let doc = parse(&src);
    let model = SceneModel::from_cst(&doc);
    let registry = defaults_registry();
    let outcome = reduce_verbosity(&doc, &model, &registry, Scope::WholeDocument);
    let shrunk = outcome.document().cloned().unwrap_or_else(|| doc.clone());
    (src, shrunk, outcome)
}

/// The single top-level value of a RON source string.
fn value_of(src: &str) -> ast::Value {
    ast::Document::cast(parse(src).root())
        .and_then(|d| d.value())
        .expect("a top-level value")
}

// ===========================================================================
// T030 [COMPLETES FR-014] — the provable-default rule
// ===========================================================================

#[test]
fn float_text_variants_all_elidable_bit_for_bit() {
    // FR-014: `1.0`, `1.00`, `1e0` all parse to the same f64 and so all equal a
    // default of `1.0` — bit-for-bit, NO epsilon, NOT a source-text comparison.
    for src in ["1.0", "1.00", "1e0", "1.0e0", "1.000", "01.0"] {
        assert!(
            ron_value_equals_json(&value_of(src), &json!(1.0)),
            "{src} must equal default 1.0 (bit-for-bit)"
        );
    }
    // An integer literal also matches a float default of the same value.
    assert!(ron_value_equals_json(&value_of("1"), &json!(1.0)));
    // A genuinely different float does NOT match.
    assert!(!ron_value_equals_json(&value_of("1.0000001"), &json!(1.0)));
}

#[test]
fn no_epsilon_tolerance_distinguishes_close_floats() {
    // The rule is bit-for-bit: a value epsilon-close to the default is NOT equal.
    let near = 1.0_f64 + f64::EPSILON;
    let src = format!("{near}");
    assert!(
        !ron_value_equals_json(&value_of(&src), &json!(1.0)),
        "1.0+EPSILON must not be elidable against 1.0 (no tolerance)"
    );
}

#[test]
fn shrink_elides_exactly_the_provable_default_fields() {
    // SC-005: only fields whose value provably equals a known default are elided.
    let (src, shrunk, outcome) = shrink_fixture();
    assert!(!outcome.is_no_op(), "the verbose scene has elidable fields");

    let out = print(&shrunk);
    assert_ne!(out, src, "shrink changed bytes");

    // Entity 0 Transform: `scale` and `weight` equal their defaults -> elided;
    // `translation` differs -> kept.
    assert!(
        out.contains("translation: (x: 5.0, y: 0.0, z: 0.0)"),
        "differing field kept"
    );
    assert!(!out.contains("weight: 1.0"), "default float field elided");
    assert!(
        !out.contains("scale: (x: 1.0"),
        "default nested-struct field elided"
    );

    // Health: only `current` (differs) remains.
    assert!(out.contains("current: 50"), "differing int field kept");
    assert!(
        !out.contains("label: \"player\""),
        "default string field elided"
    );
    assert!(!out.contains("regen: false"), "default bool field elided");
    assert!(!out.contains("max: 100"), "default int field elided");

    // PartialDefaults: `known` (== default) elided; `mystery` (no default) kept.
    assert!(
        out.contains("mystery: 7.0"),
        "no-default field always explicit"
    );
    assert!(
        !out.contains("known: 1.0"),
        "field with known matching default elided"
    );

    // The all-default Transform on entity 1 (and the all-default Tags resource)
    // lose every field -> a clean empty `()`.
    assert!(
        out.contains("\"demo::Transform\": ()"),
        "an all-default struct elides to an empty struct"
    );
    assert!(
        out.contains("\"demo::Tags\": ()"),
        "an all-default resource elides to an empty struct"
    );
}

#[test]
fn no_default_and_unregistered_types_are_left_explicit() {
    // FR-014: a type with no reflected Default and an unregistered type are never
    // elided, regardless of their values matching anything.
    let (_src, shrunk, _outcome) = shrink_fixture();
    let out = print(&shrunk);
    // `demo::NoDefaultComponent` reflects no Default -> its `value: 0.0` stays.
    assert!(
        out.contains("value: 0.0, // untouched: never elided"),
        "no-Default type kept explicit"
    );
    // `demo::Unknown` is not in the registry -> its field stays.
    assert!(
        out.contains("anything: 1.0, // untouched: unconstrained"),
        "unregistered type kept explicit"
    );
}

#[test]
fn unknown_field_default_is_left_explicit() {
    // FR-014: a field whose default is absent from the registry (drift / partial
    // export) is never elided — surfaced as a value-differs/explicit skip.
    let registry = defaults_registry();
    let src = "(known: 1.0, mystery: 1.0)"; // both look like 1.0
    let doc = parse(src);
    // Build a one-component scene around it.
    let scene_src =
        format!("(entities: {{0: (components: {{\"demo::PartialDefaults\": {src}}})}})");
    let scene_doc = parse(&scene_src);
    let model = SceneModel::from_cst(&scene_doc);
    let outcome = reduce_verbosity(&scene_doc, &model, &registry, Scope::WholeDocument);
    let out = print(outcome.document().unwrap_or(&scene_doc));
    // `known` (default 1.0) elided; `mystery` (no default) kept even though it
    // also reads 1.0.
    assert!(
        out.contains("mystery: 1.0"),
        "field with no known default left explicit"
    );
    assert!(
        !out.contains("known: 1.0"),
        "field with known matching default elided"
    );
    let _ = doc;
}

#[test]
fn nothing_elidable_is_a_zero_byte_no_op() {
    // FR-014: when no field in scope is elidable, the command is a no-op — zero
    // bytes change and NO undo unit (no Applied document) is produced.
    let registry = defaults_registry();
    // A scene whose only registered component has all-non-default values.
    let src = "(entities: {0: (components: {\"demo::Health\": (current: 1, max: 2, label: \"x\", regen: true)})})";
    let doc = parse(src);
    let model = SceneModel::from_cst(&doc);
    let outcome = reduce_verbosity(&doc, &model, &registry, Scope::WholeDocument);
    assert!(outcome.is_no_op(), "nothing elidable -> no-op");
    assert!(
        outcome.document().is_none(),
        "no Applied document on a no-op"
    );
    // The source is unchanged (we never produced a new doc).
    assert_eq!(print(&doc), src);
}

#[test]
fn empty_registry_is_a_no_op() {
    // No registry loaded -> nothing is elidable -> a zero-byte no-op.
    let registry = BevyRegistry::default();
    let src = verbose_scene();
    let doc = parse(&src);
    let model = SceneModel::from_cst(&doc);
    let outcome = reduce_verbosity(&doc, &model, &registry, Scope::WholeDocument);
    assert!(outcome.is_no_op());
    assert!(outcome.document().is_none());
}

#[test]
fn unparseable_span_is_skipped_never_crashes() {
    // FR-014/FR-008: the `9: @@@` entry is unparseable; the scene model skips it,
    // so elision never targets it and never crashes — the rest still shrinks.
    let (src, shrunk, outcome) = shrink_fixture();
    assert!(!outcome.is_no_op());
    // The garbled span is preserved verbatim in the output (never elided/edited).
    let out = print(&shrunk);
    assert!(
        out.contains("9: @@@"),
        "unparseable span preserved verbatim"
    );
    // The original also contained it (sanity).
    assert!(src.contains("9: @@@"));
}

#[test]
fn nested_struct_field_elides_with_mixed_float_text() {
    // The nested `scale: (x: 1.0, y: 1.00, z: 1e0)` equals the default Vec3
    // {1.0,1.0,1.0} (every element bit-for-bit) -> the whole `scale` field elides.
    let registry = defaults_registry();
    let comp =
        "(translation: (x: 0.0, y: 0.0, z: 0.0), scale: (x: 1.0, y: 1.00, z: 1e0), weight: 1.0)";
    let scene_src = format!("(entities: {{0: (components: {{\"demo::Transform\": {comp}}})}})");
    let doc = parse(&scene_src);
    let model = SceneModel::from_cst(&doc);
    let outcome = reduce_verbosity(&doc, &model, &registry, Scope::WholeDocument);
    let out = print(outcome.document().expect("elided"));
    assert!(
        out.contains("\"demo::Transform\": ()"),
        "all-default Transform -> empty"
    );
}

// ===========================================================================
// T031 [COMPLETES FR-016] — lossless / stability
// ===========================================================================

#[test]
fn untouched_regions_are_byte_for_byte_preserved() {
    // FR-016: comments, ordering, and trailing commas in untouched regions stay
    // byte-identical. We assert the leading comment block + a kept field's exact
    // surrounding bytes survive.
    let (src, shrunk, _outcome) = shrink_fixture();
    let out = print(&shrunk);

    // The fixture's leading comment block is verbatim in both.
    let header = "// A VERBOSE Bevy scene for E009 US3 cluster 5A";
    assert!(
        src.contains(header) && out.contains(header),
        "leading comment preserved"
    );

    // An UNTOUCHED component (no field elided) keeps its inline field comment
    // byte-for-byte.
    assert!(
        out.contains("value: 0.0, // untouched: never elided"),
        "untouched component's inline comment preserved"
    );
    assert!(
        out.contains("anything: 1.0, // untouched: unconstrained"),
        "untouched unregistered component's inline comment preserved"
    );
    // A line-comment above a touched component (untouched region) also survives.
    assert!(
        out.contains("// PartialDefaults: `known` equals its default 1.0;"),
        "line comments in untouched regions preserved"
    );
}

#[test]
fn transform_is_pure_cst_to_cst_single_document() {
    // FR-016 / data-model: each whole invocation is ONE CST→CST result the caller
    // pushes as a single undo unit. The original document is never mutated.
    let src = verbose_scene();
    let doc = parse(&src);
    let model = SceneModel::from_cst(&doc);
    let registry = defaults_registry();
    let outcome = reduce_verbosity(&doc, &model, &registry, Scope::WholeDocument);
    // The input doc is untouched (still prints the original bytes).
    assert_eq!(print(&doc), src, "input CST never mutated");
    // The outcome is exactly one resulting document.
    assert!(outcome.document().is_some(), "exactly one new CST result");
}

#[test]
fn shrink_expand_shrink_is_byte_identical() {
    // SC-006 / FR-016: shrink→expand→shrink is STABLE — the second shrink is
    // byte-identical to the first shrink (not merely value-equivalent).
    let registry = defaults_registry();
    let src = verbose_scene();

    // First shrink.
    let doc0 = parse(&src);
    let model0 = SceneModel::from_cst(&doc0);
    let shrink1 = reduce_verbosity(&doc0, &model0, &registry, Scope::WholeDocument);
    let doc1 = shrink1.document().expect("shrink changed bytes").clone();
    let bytes1 = print(&doc1);

    // Expand.
    let model1 = SceneModel::from_cst(&doc1);
    let expand = expand_to_explicit(&doc1, &model1, &registry, Scope::WholeDocument);
    let doc2 = expand.document().expect("expand restored fields").clone();

    // Second shrink.
    let model2 = SceneModel::from_cst(&doc2);
    let shrink2 = reduce_verbosity(&doc2, &model2, &registry, Scope::WholeDocument);
    let doc3 = shrink2
        .document()
        .expect("second shrink changed bytes")
        .clone();
    let bytes3 = print(&doc3);

    assert_eq!(
        bytes1, bytes3,
        "shrink->expand->shrink must be byte-identical to the first shrink"
    );
}

#[test]
fn expand_materializes_absent_defaults_and_round_trips() {
    // FR-015: expand materializes every registered default-bearing field currently
    // absent whose default is known; a subsequent shrink reverses it.
    let registry = defaults_registry();
    // A bare all-default Transform with everything elided.
    let src = "(entities: {0: (components: {\"demo::Transform\": ()})})";
    let doc = parse(src);
    let model = SceneModel::from_cst(&doc);
    let expanded = expand_to_explicit(&doc, &model, &registry, Scope::WholeDocument);
    let doc_e = expanded.document().expect("expand inserted fields").clone();
    let out = print(&doc_e);
    // All three default fields are now explicit.
    assert!(out.contains("translation:"), "translation restored");
    assert!(out.contains("scale:"), "scale restored");
    assert!(out.contains("weight:"), "weight restored");

    // And the restored values equal the registry defaults (round-trip semantics):
    // re-shrinking removes them all again, back to `()`.
    let model_e = SceneModel::from_cst(&doc_e);
    let reshrunk = reduce_verbosity(&doc_e, &model_e, &registry, Scope::WholeDocument);
    let out2 = print(reshrunk.document().expect("re-shrink"));
    assert!(
        out2.contains("\"demo::Transform\": ()"),
        "expand then shrink returns to empty"
    );
}

#[test]
fn partial_expand_on_drift_skips_unknown_defaults_never_fabricates() {
    // FR-015: if the registry has drifted so a previously-known default is gone,
    // expand restores only the still-known fields (partial expand) and never
    // fabricates a value the registry did not carry.
    // PartialDefaults carries a default only for `known`, not `mystery`.
    let registry = defaults_registry();
    let src = "(entities: {0: (components: {\"demo::PartialDefaults\": (mystery: 9.0)})})";
    let doc = parse(src);
    let model = SceneModel::from_cst(&doc);
    let expanded = expand_to_explicit(&doc, &model, &registry, Scope::WholeDocument);
    let doc_e = expanded.document().cloned().unwrap_or(doc.clone());
    let out = print(&doc_e);
    // `known` (default 1.0) is restored; `mystery` (no default) is NOT fabricated.
    assert!(out.contains("known: 1.0"), "known default restored");
    // `mystery` keeps only its existing explicit value; no second `mystery` added.
    assert_eq!(
        out.matches("mystery").count(),
        1,
        "no fabricated mystery field"
    );
}

#[test]
fn expand_with_no_absent_defaults_is_a_no_op() {
    // FR-015: if every default-bearing field is already explicit, expand is a
    // zero-byte no-op.
    let registry = defaults_registry();
    let src = "(entities: {0: (components: {\"demo::Transform\": (translation: (x: 0.0, y: 0.0, z: 0.0), scale: (x: 1.0, y: 1.0, z: 1.0), weight: 1.0)})})";
    let doc = parse(src);
    let model = SceneModel::from_cst(&doc);
    let outcome = expand_to_explicit(&doc, &model, &registry, Scope::WholeDocument);
    assert!(
        outcome.is_no_op(),
        "all fields present -> expand is a no-op"
    );
}

#[test]
fn entity_scope_only_touches_that_entity() {
    // FR-014: the optional entity scope restricts the transform. Shrinking entity
    // 1 only must not change entity 0's bytes.
    let registry = defaults_registry();
    let src = verbose_scene();
    let doc = parse(&src);
    let model = SceneModel::from_cst(&doc);
    let outcome = reduce_verbosity(&doc, &model, &registry, Scope::Entity(1));
    let out = print(outcome.document().expect("entity 1 had elidable fields"));
    // Entity 1's all-default Transform is now empty.
    assert!(out.contains("\"demo::Transform\": ()"));
    // Entity 0's Health default fields are UNTOUCHED (still present).
    assert!(
        out.contains("label: \"player\""),
        "entity 0 untouched by entity-1 scope"
    );
    assert!(
        out.contains("max: 100"),
        "entity 0 untouched by entity-1 scope"
    );
}

// ===========================================================================
// Property tests — the bit-for-bit float rule + shrink/expand/shrink stability
// ===========================================================================

proptest! {
    /// Any finite f64, written in several equivalent RON float spellings, equals
    /// its own value as a JSON default — bit-for-bit (FR-014). The default is
    /// derived FROM the same f64 so the equality is the parsed-value rule, not a
    /// text rule.
    #[test]
    fn prop_float_spellings_equal_same_default(x in proptest::num::f64::NORMAL) {
        let default = json!(x);
        // Canonical and a few re-spellings that parse to the same f64.
        let spellings = [
            format!("{x}"),
            format!("{x:?}"),
            format!("{:e}", x),     // scientific
        ];
        for s in spellings {
            // Skip a spelling that isn't a RON float literal (e.g. integer-looking).
            let v = value_of(&s);
            if let ast::Value::Literal(_) = v {
                prop_assert!(
                    ron_value_equals_json(&v, &default),
                    "spelling {s} of {x} must equal its own default"
                );
            }
        }
    }

    /// A float that differs from the default in even one ULP is NOT elidable
    /// (no epsilon tolerance).
    #[test]
    fn prop_one_ulp_off_is_not_equal(x in proptest::num::f64::NORMAL) {
        let next = f64::from_bits(x.to_bits().wrapping_add(1));
        // Only meaningful when `next` is still finite and distinct.
        if next.is_finite() && next != x {
            let v = value_of(&format!("{next:?}"));
            prop_assert!(
                !ron_value_equals_json(&v, &json!(x)),
                "{next:?} (one ULP off {x}) must not equal {x}"
            );
        }
    }

    /// shrink→expand→shrink is byte-identical for a generated all-default
    /// Transform scene with `n` entities (FR-016 stability), regardless of count.
    #[test]
    fn prop_shrink_expand_shrink_stable(n in 1usize..6) {
        let registry = defaults_registry();
        // Build a scene of `n` entities each carrying an all-default Transform plus
        // a non-default Health (so both kept and elided fields exist).
        let mut entities = String::new();
        for i in 0..n {
            entities.push_str(&format!(
                "        {i}: (components: {{\n            \"demo::Transform\": (translation: (x: 0.0, y: 0.0, z: 0.0), scale: (x: 1.0, y: 1.0, z: 1.0), weight: 1.0),\n            \"demo::Health\": (current: {i}, max: 100, label: \"player\", regen: false),\n        }}),\n",
            ));
        }
        let src = format!("(\n    entities: {{\n{entities}    }},\n)\n");

        let doc0 = parse(&src);
        let model0 = SceneModel::from_cst(&doc0);
        let s1 = reduce_verbosity(&doc0, &model0, &registry, Scope::WholeDocument);
        let doc1 = s1.document().expect("first shrink").clone();
        let bytes1 = print(&doc1);

        let model1 = SceneModel::from_cst(&doc1);
        let e = expand_to_explicit(&doc1, &model1, &registry, Scope::WholeDocument);
        let doc2 = e.document().cloned().unwrap_or(doc1.clone());

        let model2 = SceneModel::from_cst(&doc2);
        let s2 = reduce_verbosity(&doc2, &model2, &registry, Scope::WholeDocument);
        let doc3 = s2.document().cloned().unwrap_or(doc2.clone());
        let bytes3 = print(&doc3);

        prop_assert_eq!(bytes1, bytes3, "shrink->expand->shrink must be byte-identical");
    }
}

// A reference to keep `SkipReason` used in the test crate (advisory surface).
#[test]
fn skip_reasons_are_observable_on_outcome() {
    let (_src, _shrunk, outcome) = shrink_fixture();
    // The verbose scene has fields that differ from the default (e.g. translation)
    // recorded as value-differs skips on the shrink outcome.
    assert!(
        outcome
            .skipped()
            .iter()
            .any(|s| s.reason == SkipReason::ValueDiffersFromDefault),
        "value-differs skips are surfaced as advisories"
    );
}

// ===========================================================================
// T032 [COMPLETES FR-015] — round-trip integration through the App command path
// (cluster 5B). These exercise the *wired* commands (`reduce_verbosity_active` /
// `expand_to_explicit_active`) on a real Bevy-mode document with a loaded
// registry, committed as ONE E007 undo unit (FR-016, SC-006).
// ===========================================================================

mod app_round_trip {
    use std::path::{Path, PathBuf};

    use ronin_core::{ast, parse};
    use ronin_app::app::App;
    use ronin_app::bevy::mode::Mode;
    use ronin_app::bevy::{SceneModel, Scope};
    use ronin_app::settings::AppSettings;
    use serde_json::json;

    use super::ron_value_equals_json;

    /// The defaults-carrying registry export text (the same fixture 5A parses).
    fn registry_json() -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("bevy_registry_defaults.json");
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    /// A drift registry: like the defaults fixture but `demo::Transform`'s `weight`
    /// default can no longer be materialized — the registry now carries it as a
    /// `null` placeholder (a previously-elidable field whose concrete default text is
    /// no longer known: registry drift). Expanding an all-default Transform then
    /// PARTIALLY restores `translation` + `scale`, leaving `weight` absent and
    /// recording it as a `DefaultUnknownOnExpand` skip (the partial-expand advisory).
    fn drift_registry_json() -> String {
        // Minimal hand-authored export: Transform with translation/scale defaults but
        // a non-materializable (null) weight default; Vec3 for the nested struct.
        r##"{
            "bevyVersion": "0.16.0",
            "$defs": {
                "demo::Transform": {
                    "kind": "Struct",
                    "additionalProperties": false,
                    "properties": {
                        "translation": { "$ref": "#/$defs/demo::Vec3" },
                        "scale": { "$ref": "#/$defs/demo::Vec3" },
                        "weight": { "type": "number" }
                    },
                    "required": ["translation", "scale", "weight"],
                    "reflectTypes": ["Default", "Component"],
                    "default": {
                        "translation": { "x": 0.0, "y": 0.0, "z": 0.0 },
                        "scale": { "x": 1.0, "y": 1.0, "z": 1.0 },
                        "weight": null
                    }
                },
                "demo::Vec3": {
                    "kind": "Struct",
                    "additionalProperties": false,
                    "properties": {
                        "x": { "type": "number" },
                        "y": { "type": "number" },
                        "z": { "type": "number" }
                    },
                    "required": ["x", "y", "z"],
                    "reflectTypes": ["Default"],
                    "default": { "x": 0.0, "y": 0.0, "z": 0.0 }
                }
            }
        }"##
        .to_string()
    }

    /// A fresh temp project directory.
    fn temp_project(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ronin_bevy_elision_5b_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp project");
        dir
    }

    /// Write the project's `.ronin/bevy-registries.json` binding every `.scn.ron`
    /// to `registry.json`.
    fn write_registry_config(project: &Path) {
        let ronin = project.join(".ronin");
        std::fs::create_dir_all(&ronin).expect("create .ronin");
        let json = r#"{
            "rules": [
                { "pattern": "**/*.scn.ron", "registry_export_path": "registry.json" }
            ],
            "version": 1
        }"#;
        std::fs::write(ronin.join("bevy-registries.json"), json.as_bytes())
            .expect("write bevy-registries.json");
    }

    /// Build an App over a `.scn.ron` document in a project bound to `registry_text`,
    /// with `scene_text` as the document body. Returns the App with the scene as the
    /// active, Bevy-mode, registry-loaded document.
    fn app_with_scene(tag: &str, registry_text: &str, scene_text: &str) -> App {
        let project = temp_project(tag);
        std::fs::write(project.join("registry.json"), registry_text).expect("write registry");
        write_registry_config(&project);
        let scene_path = project.join("world.scn.ron");
        std::fs::write(&scene_path, scene_text.as_bytes()).expect("write scene");
        let mut app = App::new(AppSettings::default(), Some(scene_path));
        // Set the live buffer to the scene text (open_file already loaded it, but be
        // explicit so the test owns the bytes), then bump so a reparse is queued.
        if let Some(doc) = app.active_document_mut() {
            doc.buffer = scene_text.to_string();
            doc.on_edit();
        }
        app
    }

    /// The active document's live buffer bytes.
    fn buffer(app: &App) -> String {
        app.active_document().expect("active doc").buffer.clone()
    }

    /// Assert the precondition: the active document is Bevy mode + a loaded registry,
    /// so both elision commands are enabled (the explicit, Bevy-only gate).
    fn assert_elision_ready(app: &App) {
        assert_eq!(
            app.active_mode(),
            Some(Mode::Bevy),
            "scene auto-selects Bevy"
        );
        assert!(
            app.active_document()
                .is_some_and(|d| d.mode_state().has_registry()),
            "the bound registry must load"
        );
        assert!(app.elision_available(), "the commands are enabled");
    }

    #[test]
    fn reduce_then_expand_is_value_equivalent_to_the_original() {
        // FR-015 / SC-005: reduce → expand restores each field's exact prior/default
        // value (the scene is value-equivalent to the original). We compare the
        // structural value (not raw bytes — expand re-renders canonical default text).
        let scene = r#"(entities: {0: (components: {"demo::Transform": (translation: (x: 5.0, y: 0.0, z: 0.0), scale: (x: 1.0, y: 1.0, z: 1.0), weight: 1.0)})})"#;
        let mut app = app_with_scene("rt_value_equiv", &registry_json(), scene);
        assert_elision_ready(&app);

        // Reduce: scale + weight (== defaults) elided; translation kept.
        app.reduce_verbosity_active(Scope::WholeDocument);
        let reduced = buffer(&app);
        assert!(
            reduced.contains("translation: (x: 5.0"),
            "differing field kept"
        );
        assert!(!reduced.contains("weight:"), "default weight elided");
        assert!(!reduced.contains("scale:"), "default scale elided");

        // Expand: the absent default-bearing fields are materialized again.
        app.expand_to_explicit_active(Scope::WholeDocument);

        // Value-equivalence: every original component field equals the round-tripped
        // value. We check translation (kept verbatim) and scale/weight (restored to
        // the registry defaults, which equal the original since the original WAS the
        // default).
        let transform = transform_value(&buffer(&app)).expect("Transform present after expand");
        let ast::Value::Struct(t) = &transform else {
            panic!("Transform is a struct, got {transform:?}");
        };
        let field_val = |name: &str| {
            t.fields()
                .find(|f| f.name_text().as_deref() == Some(name))
                .and_then(|f| f.value())
        };
        // translation kept exactly (value-equivalent to the original).
        assert!(
            ron_value_equals_json(
                &field_val("translation").expect("translation present"),
                &json!({"x": 5.0, "y": 0.0, "z": 0.0})
            ),
            "translation is value-equivalent to the original"
        );
        // scale + weight restored to their (original == default) values.
        assert!(
            ron_value_equals_json(
                &field_val("scale").expect("scale restored"),
                &json!({"x": 1.0, "y": 1.0, "z": 1.0})
            ),
            "scale restored value-equivalent to the original default"
        );
        assert!(
            ron_value_equals_json(&field_val("weight").expect("weight restored"), &json!(1.0)),
            "weight restored value-equivalent to the original default"
        );
    }

    /// Locate the `demo::Transform` component value within a scene's source text.
    fn transform_value(src: &str) -> Option<ast::Value> {
        let doc = parse(src);
        let model = SceneModel::from_cst(&doc);
        let node = model
            .entries()
            .find(|v| v.type_path() == "demo::Transform")
            .map(|v| v.value_node().clone());
        node.and_then(ast::Value::cast)
    }

    #[test]
    fn one_undo_after_reduce_restores_exact_prior_bytes() {
        // SC-006 / FR-016: a reduce is committed as ONE undo unit. A single undo
        // restores the EXACT prior bytes (byte-for-byte, not value-equivalent).
        let scene = r#"(entities: {0: (components: {"demo::Transform": (translation: (x: 5.0, y: 0.0, z: 0.0), scale: (x: 1.0, y: 1.0, z: 1.0), weight: 1.0)})})"#;
        let mut app = app_with_scene("undo_reduce", &registry_json(), scene);
        assert_elision_ready(&app);

        let before = buffer(&app);
        app.reduce_verbosity_active(Scope::WholeDocument);
        let after = buffer(&app);
        assert_ne!(after, before, "reduce changed bytes");

        // Exactly one undo restores the prior bytes.
        assert!(app.undo_active(), "one undo step is available");
        assert_eq!(
            buffer(&app),
            before,
            "a single undo after reduce restores the exact prior bytes (one undo unit)"
        );
    }

    #[test]
    fn one_undo_after_expand_restores_exact_prior_bytes() {
        // SC-006 / FR-016: an expand is also ONE undo unit.
        let scene = r#"(entities: {0: (components: {"demo::Transform": ()})})"#;
        let mut app = app_with_scene("undo_expand", &registry_json(), scene);
        assert_elision_ready(&app);

        let before = buffer(&app);
        app.expand_to_explicit_active(Scope::WholeDocument);
        let after = buffer(&app);
        assert_ne!(after, before, "expand materialized fields");
        assert!(after.contains("translation:") && after.contains("weight:"));

        assert!(app.undo_active(), "one undo step is available");
        assert_eq!(
            buffer(&app),
            before,
            "a single undo after expand restores the exact prior bytes (one undo unit)"
        );
    }

    #[test]
    fn no_op_reduce_pushes_no_undo_unit_and_changes_zero_bytes() {
        // FR-014: a reduce with nothing elidable changes zero bytes AND pushes no
        // undo unit (a no-op never pollutes the undo stack).
        let scene = r#"(entities: {0: (components: {"demo::Health": (current: 1, max: 2, label: "x", regen: true)})})"#;
        let mut app = app_with_scene("noop_reduce", &registry_json(), scene);
        assert_elision_ready(&app);
        let before = buffer(&app);
        let depth_before = app.active_document().map(|d| d.undo_depth()).unwrap_or(0);

        app.reduce_verbosity_active(Scope::WholeDocument);
        assert_eq!(buffer(&app), before, "no-op changed zero bytes");
        assert_eq!(
            app.active_document().map(|d| d.undo_depth()).unwrap_or(0),
            depth_before,
            "a no-op reduce pushes no undo unit"
        );
        // The "nothing to reduce" status is surfaced (informational, never an error).
        assert!(
            app.notices()
                .iter()
                .any(|n| n.message.contains("Nothing to reduce")),
            "no-op surfaces an informational status, got {:?}",
            app.notices()
        );
    }

    #[test]
    fn registry_drift_partial_expand_surfaces_advisory_and_undo_recovers() {
        // FR-015: registry drift — a previously-elided field's default is no longer
        // carried. Expand is a PARTIAL expand (only still-known fields restored), the
        // advisory is surfaced, and exact pre-drift recovery is available via undo.
        //
        // Scene: an all-elided Transform `()`. Under the DRIFT registry, Transform's
        // `weight` default is gone, so expand restores translation + scale only and
        // records `weight` as a DefaultUnknownOnExpand skip.
        let scene = r#"(entities: {0: (components: {"demo::Transform": ()})})"#;
        let mut app = app_with_scene("drift_partial", &drift_registry_json(), scene);
        assert_elision_ready(&app);

        let before = buffer(&app);
        app.expand_to_explicit_active(Scope::WholeDocument);
        let expanded = buffer(&app);

        // Partial expand: translation + scale restored; weight NOT fabricated.
        assert!(expanded.contains("translation:"), "translation restored");
        assert!(expanded.contains("scale:"), "scale restored");
        assert!(
            !expanded.contains("weight:"),
            "weight is NOT fabricated (its default is no longer carried)"
        );

        // The partial-expand advisory is surfaced (mentions weight + that undo recovers).
        assert!(
            app.notices().iter().any(|n| {
                n.message.contains("Partial expand")
                    && n.message.contains("demo::Transform.weight")
                    && n.message.contains("Undo")
            }),
            "the partial-expand drift advisory is surfaced, got {:?}",
            app.notices()
        );

        // Exact pre-drift recovery via a single undo (one undo unit).
        assert!(app.undo_active(), "one undo step is available");
        assert_eq!(
            buffer(&app),
            before,
            "a single undo restores the exact pre-expand bytes (pre-drift recovery)"
        );
    }

    #[test]
    fn commands_are_bevy_only_and_explicit() {
        // FR-014/FR-015: the commands are explicit + Bevy-only. A serde-mode document
        // (or one with no registry) disables them and a manual invocation is a guarded
        // no-op that never edits bytes.
        let scene = r#"(entities: {0: (components: {"demo::Transform": (weight: 1.0)})})"#;
        let mut app = app_with_scene("bevy_only", &registry_json(), scene);
        assert_elision_ready(&app);

        // Toggle to serde — the commands are now disabled.
        app.toggle_active_mode();
        assert_eq!(app.active_mode(), Some(Mode::Serde));
        assert!(!app.elision_available(), "serde mode disables elision");

        let before = buffer(&app);
        app.reduce_verbosity_active(Scope::WholeDocument);
        assert_eq!(
            buffer(&app),
            before,
            "a guarded invocation edits zero bytes"
        );
        assert!(
            app.notices()
                .iter()
                .any(|n| n.message.contains("only available in Bevy mode")),
            "the serde-mode guard surfaces an explanatory notice, got {:?}",
            app.notices()
        );
    }
}
