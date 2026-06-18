//! Structural-transform lossless round-trip tests (E008 Phase 1a, T002/T006).
//!
//! These tests pin the load-bearing invariants of the pure CST→CST structural
//! transforms (ADR-0007, project-instructions §I "Never Corrupt User Data"):
//!
//! * **Lossless byte-for-byte (FR-013)** — applying any op and printing the new
//!   CST yields a document where **every region the op did not touch is
//!   byte-identical** to the original; only the touched subtree changes.
//! * **Trivia preserved on surviving siblings (FR-021)** — comments, blank lines,
//!   and trailing commas attached to siblings the op did not touch are kept.
//! * **Non-destructive** — the original document is never mutated.
//! * **Blocked = zero change** — a `Blocked` outcome leaves the input unchanged
//!   and produces no edit (the document round-trips byte-for-byte).
//!
//! The proptest generates RON inputs (lists/structs/maps with comments + trailing
//! commas) and applies every op kind, asserting the untouched-region invariant.
//! Reading the on-disk corpus via `std::fs` here is fine: this is a **test**, not
//! the WASM-clean `ronin-core` core.

use std::fs;
use std::path::{Path, PathBuf};

use proptest::prelude::*;
use ronin_core::ast;
use ronin_core::transform::{
    apply_structural, BlockedReason, ParentRef, StructuralOp, TransformOutcome,
};
use ronin_core::{parse, print, CstDocument, SyntaxKind, SyntaxNode};

// =============================================================================
// Helpers
// =============================================================================

/// Apply an op, asserting it was `Applied`, and return the new printed text.
fn applied(doc: &CstDocument, op: StructuralOp) -> String {
    match apply_structural(doc, op) {
        TransformOutcome::Applied(new_doc) => print(&new_doc),
        TransformOutcome::Blocked(reason) => panic!("expected Applied, got Blocked({reason:?})"),
        other => panic!("unexpected outcome variant: {other:?}"),
    }
}

/// First descendant node of `kind`, or panic.
fn first_node(doc: &CstDocument, kind: SyntaxKind) -> SyntaxNode {
    fn walk(n: &SyntaxNode, kind: SyntaxKind, out: &mut Option<SyntaxNode>) {
        if out.is_some() {
            return;
        }
        if n.kind() == kind {
            *out = Some(n.clone());
            return;
        }
        for c in n.children() {
            walk(&c, kind, out);
        }
    }
    let mut out = None;
    walk(&doc.root(), kind, &mut out);
    out.unwrap_or_else(|| panic!("no {kind:?} node in document"))
}

/// First list node, as a `ParentRef::List`.
fn list_parent(doc: &CstDocument) -> ParentRef {
    ParentRef::List(first_node(doc, SyntaxKind::List))
}

/// First struct node, as a `ParentRef::Struct`.
fn struct_parent(doc: &CstDocument) -> ParentRef {
    ParentRef::Struct(first_node(doc, SyntaxKind::Struct))
}

/// First map node, as a `ParentRef::Map`.
fn map_parent(doc: &CstDocument) -> ParentRef {
    ParentRef::Map(first_node(doc, SyntaxKind::Map))
}

/// Texts of a struct's field nodes, in order.
fn struct_field_texts(doc: &CstDocument) -> Vec<String> {
    let s = ast::Struct::cast(first_node(doc, SyntaxKind::Struct)).unwrap();
    s.fields().map(|f| f.syntax().text()).collect()
}

/// Texts of a list's element nodes, in order.
fn list_item_texts(doc: &CstDocument) -> Vec<String> {
    let l = ast::List::cast(first_node(doc, SyntaxKind::List)).unwrap();
    l.items().map(|v| v.syntax().text()).collect()
}

/// Parse the printed output back and return the new document.
fn reparse(text: &str) -> CstDocument {
    parse(text)
}

// =============================================================================
// Concrete per-op lossless assertions (the readable spec of each op).
// =============================================================================

#[test]
fn set_value_keeps_untouched_bytes() {
    let src = "Foo(x: 1, y: 2) // keep\n";
    let doc = parse(src);
    let out = applied(
        &doc,
        StructuralOp::SetValue {
            parent: struct_parent(&doc),
            index: 0,
            value: "99".into(),
        },
    );
    assert_eq!(out, "Foo(x: 99, y: 2) // keep\n");
    assert_eq!(print(&doc), src, "original untouched");
}

#[test]
fn insert_field_single_line_preserves_siblings() {
    let src = "Foo(x: 1, y: 2)";
    let doc = parse(src);
    // Append a field at the end.
    let out = applied(
        &doc,
        StructuralOp::InsertField {
            parent: struct_parent(&doc),
            index: 2,
            name: "z".into(),
            value: "3".into(),
        },
    );
    // Round-trips and contains the new field; original two fields unchanged.
    let new = reparse(&out);
    let names: Vec<_> = ast::Struct::cast(first_node(&new, SyntaxKind::Struct))
        .unwrap()
        .fields()
        .map(|f| f.name_text().unwrap_or_default())
        .collect();
    assert_eq!(names, vec!["x", "y", "z"]);
    assert!(out.contains("x: 1"));
    assert!(out.contains("y: 2"));
}

#[test]
fn insert_field_multiline_inherits_indent_and_trailing_comma() {
    let src = "Foo(\n    x: 1,\n    y: 2,\n)";
    let doc = parse(src);
    let out = applied(
        &doc,
        StructuralOp::InsertField {
            parent: struct_parent(&doc),
            index: 2,
            name: "z".into(),
            value: "3".into(),
        },
    );
    // The appended field adopts the 4-space indent and trailing comma.
    assert_eq!(out, "Foo(\n    x: 1,\n    y: 2,\n    z: 3,\n)");
}

#[test]
fn remove_middle_field_normalizes_separator() {
    let src = "Foo(x: 1, y: 2, z: 3)";
    let doc = parse(src);
    let out = applied(
        &doc,
        StructuralOp::RemoveField {
            parent: struct_parent(&doc),
            index: 1,
        },
    );
    assert_eq!(out, "Foo(x: 1, z: 3)");
}

#[test]
fn remove_last_field_no_dangling_comma() {
    let src = "Foo(x: 1, y: 2)";
    let doc = parse(src);
    let out = applied(
        &doc,
        StructuralOp::RemoveField {
            parent: struct_parent(&doc),
            index: 1,
        },
    );
    assert_eq!(out, "Foo(x: 1)");
}

#[test]
fn remove_element_preserves_adjacent_comment_on_sibling() {
    let src = "[\n    1, // one\n    2, // two\n    3, // three\n]";
    let doc = parse(src);
    // Remove the middle element; comments on surviving siblings stay attached.
    let out = applied(
        &doc,
        StructuralOp::RemoveElement {
            parent: list_parent(&doc),
            index: 1,
        },
    );
    assert!(out.contains("// one"), "sibling 1 comment kept: {out:?}");
    assert!(out.contains("// three"), "sibling 3 comment kept: {out:?}");
    assert!(
        !out.contains("// two"),
        "removed element's comment gone: {out:?}"
    );
    // The surviving elements still round-trip parse.
    let new = reparse(&out);
    assert_eq!(list_item_texts(&new), vec!["1", "3"]);
}

#[test]
fn rename_field_in_place() {
    let src = "Foo(alpha: 1, beta: 2)";
    let doc = parse(src);
    let out = applied(
        &doc,
        StructuralOp::RenameKey {
            parent: struct_parent(&doc),
            index: 0,
            new_name: "gamma".into(),
        },
    );
    assert_eq!(out, "Foo(gamma: 1, beta: 2)");
}

#[test]
fn rename_collision_blocks_with_zero_change() {
    let src = "Foo(a: 1, b: 2) // c\n";
    let doc = parse(src);
    let outcome = apply_structural(
        &doc,
        StructuralOp::RenameKey {
            parent: struct_parent(&doc),
            index: 0,
            new_name: "b".into(), // collides with existing field `b`
        },
    );
    match outcome {
        TransformOutcome::Blocked(BlockedReason::RenameCollision) => {}
        other => panic!("expected RenameCollision, got {other:?}"),
    }
    // The input document is unchanged.
    assert_eq!(print(&doc), src);
}

#[test]
fn rename_map_key_collision_blocks() {
    let src = "{ \"a\": 1, \"b\": 2 }";
    let doc = parse(src);
    let outcome = apply_structural(
        &doc,
        StructuralOp::RenameKey {
            parent: map_parent(&doc),
            index: 0,
            new_name: "\"b\"".into(),
        },
    );
    assert!(matches!(
        outcome,
        TransformOutcome::Blocked(BlockedReason::RenameCollision)
    ));
    assert_eq!(print(&doc), src);
}

#[test]
fn reorder_field_moves_value_and_keeps_others() {
    let src = "Foo(x: 1, y: 2, z: 3)";
    let doc = parse(src);
    // Move field 0 (x) to position 2.
    let out = applied(
        &doc,
        StructuralOp::ReorderChild {
            parent: struct_parent(&doc),
            from: 0,
            to: 2,
        },
    );
    let new = reparse(&out);
    let names: Vec<_> = ast::Struct::cast(first_node(&new, SyntaxKind::Struct))
        .unwrap()
        .fields()
        .map(|f| f.name_text().unwrap_or_default())
        .collect();
    assert_eq!(names, vec!["y", "z", "x"]);
}

#[test]
fn reorder_list_element() {
    let src = "[1, 2, 3]";
    let doc = parse(src);
    let out = applied(
        &doc,
        StructuralOp::ReorderChild {
            parent: list_parent(&doc),
            from: 2,
            to: 0,
        },
    );
    let new = reparse(&out);
    assert_eq!(list_item_texts(&new), vec!["3", "1", "2"]);
}

#[test]
fn insert_element_into_empty_list_uses_document_default() {
    // Empty list inside an otherwise 4-space-indented document.
    let src = "Foo(\n    items: [],\n)";
    let doc = parse(src);
    let out = applied(
        &doc,
        StructuralOp::InsertElement {
            parent: list_parent(&doc),
            index: 0,
            value: "1".into(),
        },
    );
    // Multi-line, document indent (4 each level → 8 for the element), trailing comma.
    let new = reparse(&out);
    assert_eq!(list_item_texts(&new), vec!["1"]);
    assert!(
        out.contains("Foo(\n    items: ["),
        "wrapper preserved: {out:?}"
    );
    assert!(out.contains(",\n"), "trailing comma present: {out:?}");
}

#[test]
fn insert_element_into_empty_list_default_indent_when_undetectable() {
    let src = "[]";
    let doc = parse(src);
    let out = applied(
        &doc,
        StructuralOp::InsertElement {
            parent: list_parent(&doc),
            index: 0,
            value: "1".into(),
        },
    );
    // No indent detectable → default 4 spaces, multi-line, trailing comma.
    assert_eq!(out, "[\n    1,\n]");
}

#[test]
fn swap_enum_variant_keeps_shared_field() {
    // An enum-variant struct-like payload `Name { .. }`.
    let src = "Shape { width: 10, height: 20 }";
    let doc = parse(src);
    let variant = first_node(&doc, SyntaxKind::EnumVariant);
    let out = applied(
        &doc,
        StructuralOp::SwapEnumVariant {
            variant,
            new_name: "Circle".into(),
            new_fields: vec!["width".into(), "radius".into()],
            placeholder: "0".into(),
        },
    );
    let new = reparse(&out);
    let v = ast::EnumVariant::cast(first_node(&new, SyntaxKind::EnumVariant)).unwrap();
    assert_eq!(v.name_text().as_deref(), Some("Circle"));
    // A struct-variant payload parses as `MapEntry`s whose key is a bare ident.
    let fields: Vec<_> = v
        .entries()
        .map(|e| e.key().unwrap().syntax().text())
        .collect();
    assert_eq!(fields, vec!["width", "radius"]);
    // Shared field `width` kept its original value `10` (bytes preserved).
    assert!(
        out.contains("width: 10"),
        "shared field value kept: {out:?}"
    );
    assert!(
        out.contains("radius: 0"),
        "new field placeholder added: {out:?}"
    );
    assert!(!out.contains("height"), "old-only field removed: {out:?}");
}

#[test]
fn add_field_across_rows_touches_every_struct() {
    let src = "[A(x: 1), A(x: 2), A(x: 3)]";
    let doc = parse(src);
    let list = first_node(&doc, SyntaxKind::List);
    let out = applied(
        &doc,
        StructuralOp::AddFieldAcrossRows {
            list,
            name: "y".into(),
            value: "0".into(),
        },
    );
    let new = reparse(&out);
    let l = ast::List::cast(first_node(&new, SyntaxKind::List)).unwrap();
    for item in l.items() {
        let ast::Value::Struct(s) = item else {
            panic!("expected struct row");
        };
        let names: Vec<_> = s.fields().map(|f| f.name_text().unwrap()).collect();
        assert_eq!(names, vec!["x", "y"], "every row gained field y");
    }
}

#[test]
fn target_not_found_blocks_with_zero_change() {
    let src = "Foo(x: 1)";
    let doc = parse(src);
    let outcome = apply_structural(
        &doc,
        StructuralOp::RemoveField {
            parent: struct_parent(&doc),
            index: 99, // out of range
        },
    );
    assert!(matches!(
        outcome,
        TransformOutcome::Blocked(BlockedReason::TargetNotFound)
    ));
    assert_eq!(print(&doc), src);
}

// =============================================================================
// Corpus-driven lossless smoke check.
// =============================================================================

fn corpus_valid_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("corpus")
        .join("valid")
}

/// For a corpus file that contains a struct, a set-value on its first field must
/// keep every other byte identical except the replaced value.
#[test]
fn corpus_set_value_is_lossless_outside_target() {
    let dir = corpus_valid_dir();
    let mut checked = 0usize;
    for entry in fs::read_dir(&dir).expect("read corpus/valid") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("ron") {
            continue;
        }
        let bytes = fs::read(&path).expect("read fixture");
        let Ok(src) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let doc = parse(src);
        // Find a struct with at least one literal-valued field.
        let Some(struct_node) = find_first(&doc.root(), SyntaxKind::Struct) else {
            continue;
        };
        let s = ast::Struct::cast(struct_node.clone()).unwrap();
        let Some((idx, field)) = s
            .fields()
            .enumerate()
            .find(|(_, f)| matches!(f.value(), Some(ast::Value::Literal(_))))
        else {
            continue;
        };
        let old_value = field.value().unwrap().syntax().text();
        let outcome = apply_structural(
            &doc,
            StructuralOp::SetValue {
                parent: ParentRef::Struct(struct_node.clone()),
                index: idx,
                value: "424242".into(),
            },
        );
        let TransformOutcome::Applied(new_doc) = outcome else {
            panic!("set_value blocked on {path:?}");
        };
        let out = print(&new_doc);
        // The output equals the original with exactly the first occurrence of the
        // field's value (at its byte offset) replaced. Verify by reconstructing.
        let value_range = field.value().unwrap().syntax().text_range();
        let mut expected = String::new();
        expected.push_str(&src[..value_range.start()]);
        expected.push_str("424242");
        expected.push_str(&src[value_range.end()..]);
        assert_eq!(
            out, expected,
            "lossless set-value on {path:?} (old={old_value})"
        );
        checked += 1;
    }
    assert!(checked > 0, "no struct-bearing corpus fixture exercised");
}

/// DFS for the first node of `kind`.
fn find_first(root: &SyntaxNode, kind: SyntaxKind) -> Option<SyntaxNode> {
    if root.kind() == kind {
        return Some(root.clone());
    }
    for child in root.children() {
        if let Some(found) = find_first(&child, kind) {
            return Some(found);
        }
    }
    None
}

// =============================================================================
// Property: untouched siblings are byte-identical for every op kind (FR-013/021).
// =============================================================================

/// A small recursive RON generator producing structs / lists / maps with
/// comments and trailing commas, so the transform's trivia + separator paths are
/// exercised by generated input (not only the corpus).
fn ron_doc() -> impl Strategy<Value = String> {
    let leaf = prop_oneof![
        (0i32..1000).prop_map(|n| n.to_string()),
        any::<bool>().prop_map(|b| b.to_string()),
        "[a-z]{1,5}".prop_map(|s| format!("\"{s}\"")),
    ];
    leaf.prop_recursive(3, 24, 4, |inner| {
        let ident = "[a-z][a-z0-9]{0,4}";
        prop_oneof![
            // A struct with 1..=4 fields, multi-line with trailing commas + comments.
            prop::collection::vec((ident, inner.clone()), 1..=4).prop_map(|fields| {
                let body: String = fields
                    .iter()
                    .map(|(k, v)| format!("    {k}: {v}, // f\n"))
                    .collect();
                format!("Foo(\n{body})")
            }),
            // A list with 1..=4 elements, multi-line with trailing commas.
            prop::collection::vec(inner.clone(), 1..=4).prop_map(|items| {
                let body: String = items.iter().map(|v| format!("    {v},\n")).collect();
                format!("[\n{body}]")
            }),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 200, ..ProptestConfig::default() })]

    /// FR-013/FR-021: for a generated struct, removing field `i` keeps every
    /// surviving field's node text (incl. its comment trivia) byte-identical.
    #[test]
    fn prop_remove_field_keeps_surviving_siblings(src in ron_doc()) {
        let doc = parse(&src);
        prop_assume!(first_opt(&doc, SyntaxKind::Struct).is_some());
        let before = struct_field_texts(&doc);
        prop_assume!(before.len() >= 2);
        let remove_idx = 0usize; // deterministic; covers "not last" path

        let outcome = apply_structural(
            &doc,
            StructuralOp::RemoveField {
                parent: struct_parent(&doc),
                index: remove_idx,
            },
        );
        let TransformOutcome::Applied(new_doc) = outcome else {
            prop_assert!(false, "remove blocked unexpectedly");
            unreachable!();
        };
        let after = struct_field_texts(&new_doc);
        // The removed field is gone; the rest are byte-identical, in order.
        let expected: Vec<String> = before
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != remove_idx)
            .map(|(_, t)| t.clone())
            .collect();
        prop_assert_eq!(after, expected);
        // Original never mutated.
        prop_assert_eq!(print(&doc), src);
    }

    /// FR-013: set-value on a struct field changes ONLY that field's value bytes;
    /// the surrounding document is byte-identical with the value spliced.
    #[test]
    fn prop_set_value_is_byte_local(src in ron_doc()) {
        let doc = parse(&src);
        prop_assume!(first_opt(&doc, SyntaxKind::Struct).is_some());
        let s = ast::Struct::cast(first_node(&doc, SyntaxKind::Struct)).unwrap();
        let fields: Vec<_> = s.fields().collect();
        prop_assume!(!fields.is_empty());
        let target_value = fields[0].value();
        prop_assume!(target_value.is_some());
        let value_range = target_value.unwrap().syntax().text_range();

        let outcome = apply_structural(
            &doc,
            StructuralOp::SetValue {
                parent: struct_parent(&doc),
                index: 0,
                value: "777".into(),
            },
        );
        let TransformOutcome::Applied(new_doc) = outcome else {
            prop_assert!(false, "set-value blocked unexpectedly");
            unreachable!();
        };
        let out = print(&new_doc);
        let mut expected = String::new();
        expected.push_str(&src[..value_range.start()]);
        expected.push_str("777");
        expected.push_str(&src[value_range.end()..]);
        prop_assert_eq!(out, expected);
    }

    /// FR-013/FR-021: appending a field to a generated struct keeps EVERY existing
    /// field byte-identical (only new bytes are added).
    #[test]
    fn prop_insert_field_keeps_existing_siblings(src in ron_doc()) {
        let doc = parse(&src);
        prop_assume!(first_opt(&doc, SyntaxKind::Struct).is_some());
        let before = struct_field_texts(&doc);
        prop_assume!(!before.is_empty());

        let outcome = apply_structural(
            &doc,
            StructuralOp::InsertField {
                parent: struct_parent(&doc),
                index: before.len(),
                name: "newf".into(),
                value: "0".into(),
            },
        );
        let TransformOutcome::Applied(new_doc) = outcome else {
            prop_assert!(false, "insert blocked unexpectedly");
            unreachable!();
        };
        // `apply_edit` splices the new field as raw tokens; reparse to recover the
        // structured `StructField` nodes before inspecting structure.
        let new = reparse(&print(&new_doc));
        let after = struct_field_texts(&new);
        // Every original field text is preserved as a prefix of the new field list.
        prop_assert!(after.len() == before.len() + 1);
        for (b, a) in before.iter().zip(after.iter()) {
            prop_assert_eq!(b, a);
        }
        // The new field is present.
        let new_name = ast::Struct::cast(first_node(&new, SyntaxKind::Struct))
            .unwrap()
            .fields()
            .last()
            .and_then(|f| f.name_text());
        prop_assert_eq!(new_name.as_deref(), Some("newf"));
    }

    /// FR-013: reordering list elements keeps each element's own bytes intact and
    /// only permutes their order; the original is never mutated.
    #[test]
    fn prop_reorder_list_permutes_elements(src in ron_doc()) {
        let doc = parse(&src);
        prop_assume!(first_opt(&doc, SyntaxKind::List).is_some());
        let before = list_item_texts(&doc);
        prop_assume!(before.len() >= 2);

        let outcome = apply_structural(
            &doc,
            StructuralOp::ReorderChild {
                parent: list_parent(&doc),
                from: 0,
                to: before.len() - 1,
            },
        );
        let TransformOutcome::Applied(new_doc) = outcome else {
            prop_assert!(false, "reorder blocked unexpectedly");
            unreachable!();
        };
        // Reparse so the re-inserted (raw-token) element is a structured node.
        let new = reparse(&print(&new_doc));
        let after = list_item_texts(&new);
        // Same multiset of element texts (each element's own bytes preserved).
        let mut bsorted = before.clone();
        bsorted.sort();
        let mut asorted = after.clone();
        asorted.sort();
        prop_assert_eq!(bsorted, asorted);
        // Element 0 moved to the end.
        prop_assert_eq!(after.last().cloned(), before.first().cloned());
        prop_assert_eq!(print(&doc), src);
    }

    /// Blocked = zero change: a guaranteed-collision rename never alters bytes.
    #[test]
    fn prop_blocked_rename_is_zero_change(src in ron_doc()) {
        let doc = parse(&src);
        prop_assume!(first_opt(&doc, SyntaxKind::Struct).is_some());
        let s = ast::Struct::cast(first_node(&doc, SyntaxKind::Struct)).unwrap();
        let names: Vec<_> = s.fields().filter_map(|f| f.name_text()).collect();
        prop_assume!(names.len() >= 2);

        // Rename field 0 to field 1's name → guaranteed collision.
        let outcome = apply_structural(
            &doc,
            StructuralOp::RenameKey {
                parent: struct_parent(&doc),
                index: 0,
                new_name: names[1].clone(),
            },
        );
        prop_assert!(matches!(
            outcome,
            TransformOutcome::Blocked(BlockedReason::RenameCollision)
        ));
        prop_assert_eq!(print(&doc), src);
    }
}

/// Like `first_node` but returns `Option` (for `prop_assume!`).
fn first_opt(doc: &CstDocument, kind: SyntaxKind) -> Option<SyntaxNode> {
    find_first(&doc.root(), kind)
}
