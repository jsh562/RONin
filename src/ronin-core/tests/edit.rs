//! Edit-locality tests (TR-011 / SC-007 / INV-8, COMPLETES TR-011).
//!
//! After an insert / replace / remove under any trivia policy, the regions the
//! edit does not touch MUST print byte-identically, and the whole tree MUST
//! remain printable. These are unit + property tests over the public edit API.
//!
//! The "unaffected region" oracle: an edit targets a child-index span
//! `[start..=end]` of one parent node. The source bytes **before** that span's
//! start offset (the *prefix*) and **after** its end offset (the *suffix*) are
//! untouched by a KEEP_ALL edit — the result must still begin with the prefix
//! and end with the suffix. Trivia policies only ever shrink, never grow, the
//! preserved trivia, so the prefix/suffix taken at the trivia-excluded core span
//! remain preserved under every policy.

use proptest::prelude::*;
use ronin_core::{
    apply_edit, parse, print, CstDocument, EditOperation, EditTarget, SyntaxKind, SyntaxNode,
    TriviaPolicy,
};

/// Every direct-child node of every node in the tree (skips the root itself and
/// tokens). These are the editable node targets.
fn all_editable_nodes(doc: &CstDocument) -> Vec<SyntaxNode> {
    fn walk(n: SyntaxNode, out: &mut Vec<SyntaxNode>) {
        for c in n.children() {
            out.push(c.clone());
            walk(c, out);
        }
    }
    let mut out = Vec::new();
    walk(doc.root(), &mut out);
    out
}

/// The four trivia policies to exercise.
fn policies() -> [TriviaPolicy; 4] {
    [
        TriviaPolicy::KEEP_ALL,
        TriviaPolicy::DISCARD_ALL,
        TriviaPolicy {
            keep_leading: true,
            keep_trailing: false,
        },
        TriviaPolicy {
            keep_leading: false,
            keep_trailing: true,
        },
    ]
}

/// For a KEEP_ALL edit on `target`, the prefix before the target's start byte
/// and the suffix after its end byte must be preserved verbatim in the result.
fn assert_keep_all_locality(src: &str, target: &SyntaxNode, edited: &CstDocument, kind_desc: &str) {
    let r = target.text_range();
    let prefix = &src[..r.start()];
    let suffix = &src[r.end()..];
    let out = print(edited);
    assert!(
        out.starts_with(prefix),
        "{kind_desc}: prefix not preserved\n  prefix={prefix:?}\n  out={out:?}"
    );
    assert!(
        out.ends_with(suffix),
        "{kind_desc}: suffix not preserved\n  suffix={suffix:?}\n  out={out:?}"
    );
}

#[test]
fn replace_keep_all_preserves_surrounding_bytes() {
    let src = "Outer(a: Inner(x: 1), b: [10, 20, 30], c: { k: 'v' })";
    let doc = parse(src);
    for target in all_editable_nodes(&doc) {
        // Skip the whole top-level struct (replacing it changes everything).
        if target.parent().map(|p| p.kind()) == Some(SyntaxKind::Root) {
            continue;
        }
        let edited = apply_edit(
            &doc,
            EditOperation::replace(
                EditTarget::Node(target.clone()),
                "REPLACED",
                TriviaPolicy::KEEP_ALL,
            ),
        )
        .expect("replace applies");
        assert_keep_all_locality(src, &target, &edited, "replace");
        // The replacement text must appear in the output.
        assert!(print(&edited).contains("REPLACED"));
        // Tree stays printable; original untouched.
        let _ = print(&edited);
        assert_eq!(print(&doc), src, "original untouched");
    }
}

#[test]
fn remove_keep_all_preserves_surrounding_bytes() {
    let src = "Outer(a: Inner(x: 1), b: [10, 20, 30])";
    let doc = parse(src);
    for target in all_editable_nodes(&doc) {
        if target.parent().map(|p| p.kind()) == Some(SyntaxKind::Root) {
            continue;
        }
        let edited = apply_edit(
            &doc,
            EditOperation::remove(EditTarget::Node(target.clone()), TriviaPolicy::KEEP_ALL),
        )
        .expect("remove applies");
        assert_keep_all_locality(src, &target, &edited, "remove");
        assert_eq!(print(&doc), src, "original untouched");
    }
}

#[test]
fn insert_keep_all_preserves_surrounding_bytes() {
    let src = "[1, 2, 3]";
    let doc = parse(src);
    for target in all_editable_nodes(&doc) {
        if target.parent().map(|p| p.kind()) == Some(SyntaxKind::Root) {
            continue;
        }
        let r = target.text_range();
        let prefix = &src[..r.start()];
        let edited = apply_edit(
            &doc,
            EditOperation::insert(
                EditTarget::Node(target.clone()),
                "X",
                TriviaPolicy::KEEP_ALL,
            ),
        )
        .expect("insert applies");
        let out = print(&edited);
        // Insert-before keeps everything from the target's start onward, with `X`
        // spliced in just before it ⇒ prefix preserved, suffix (target+rest) too.
        assert!(
            out.starts_with(prefix),
            "insert prefix not preserved: {out:?}"
        );
        assert!(out.contains('X'));
        assert_eq!(print(&doc), src, "original untouched");
    }
}

#[test]
fn discard_leading_trivia_drops_preceding_whitespace() {
    // `b` field is preceded by a space (leading trivia of the StructField).
    let src = "Foo(a: 1, b: 2)";
    let doc = parse(src);
    let fields: Vec<_> = doc
        .root()
        .children()
        .find(|n| n.kind() == SyntaxKind::Struct)
        .unwrap()
        .children()
        .filter(|n| n.kind() == SyntaxKind::StructField)
        .collect();
    let b = fields[1].clone();
    // Remove `b: 2` discarding the leading space ⇒ the `, ` collapses to `,`.
    let edited = apply_edit(
        &doc,
        EditOperation::remove(
            EditTarget::Node(b),
            TriviaPolicy {
                keep_leading: true, // keep, so the space stays for this check below
                keep_trailing: true,
            },
        ),
    )
    .unwrap();
    // With keep_leading the space before `b` remains: "Foo(a: 1, )".
    assert_eq!(print(&edited), "Foo(a: 1, )");

    // Now discard the leading trivia of the field's value token span instead.
    let field = doc
        .root()
        .children()
        .find(|n| n.kind() == SyntaxKind::Struct)
        .unwrap()
        .children()
        .filter(|n| n.kind() == SyntaxKind::StructField)
        .nth(1)
        .unwrap();
    let val = field
        .children()
        .find(|n| n.kind() == SyntaxKind::Literal)
        .unwrap();
    // The literal `2` is preceded by a space (after the colon). Removing it with
    // keep_leading=false also removes that space.
    let edited2 = apply_edit(
        &doc,
        EditOperation::remove(
            EditTarget::Node(val),
            TriviaPolicy {
                keep_leading: false,
                keep_trailing: true,
            },
        ),
    )
    .unwrap();
    assert_eq!(print(&edited2), "Foo(a: 1, b:)");
}

proptest! {
    /// Across generated RON, every edit kind under every trivia policy keeps the
    /// tree printable and never panics; for KEEP_ALL the surrounding bytes are
    /// preserved verbatim (INV-8 / SC-007).
    #[test]
    fn edits_are_local_and_printable(src in ron_strategy()) {
        let doc = parse(&src);
        let nodes = all_editable_nodes(&doc);
        for target in nodes {
            // Never target a direct child of Root (would replace the whole value).
            if target.parent().map(|p| p.kind()) == Some(SyntaxKind::Root) {
                continue;
            }
            for policy in policies() {
                // Replace
                let e = apply_edit(
                    &doc,
                    EditOperation::replace(EditTarget::Node(target.clone()), "0", policy),
                ).expect("replace applies");
                let _ = print(&e); // printable, no panic
                if policy == TriviaPolicy::KEEP_ALL {
                    assert_keep_all_locality(&src, &target, &e, "prop-replace");
                }

                // Remove
                let e = apply_edit(
                    &doc,
                    EditOperation::remove(EditTarget::Node(target.clone()), policy),
                ).expect("remove applies");
                let _ = print(&e);
                if policy == TriviaPolicy::KEEP_ALL {
                    assert_keep_all_locality(&src, &target, &e, "prop-remove");
                }

                // Insert
                let e = apply_edit(
                    &doc,
                    EditOperation::insert(EditTarget::Node(target.clone()), "9", policy),
                ).expect("insert applies");
                let _ = print(&e);
            }
            // The original document is never mutated by any edit.
            prop_assert_eq!(print(&doc), src.clone());
        }
    }
}

/// Generate valid-RON-shaped source strings across the TR-004 construct surface.
/// (Mirrors the strategy in `roundtrip.rs`; kept local so this test file is
/// self-contained.)
fn ron_strategy() -> impl Strategy<Value = String> {
    let leaf = prop_oneof![
        any::<i32>().prop_map(|n| n.to_string()),
        any::<bool>().prop_map(|b| b.to_string()),
        Just("()".to_string()),
        "[a-z]{1,4}".prop_map(|s| format!("\"{s}\"")),
        "[a-z]".prop_map(|c| format!("'{c}'")),
    ];
    leaf.prop_recursive(3, 32, 4, |inner| {
        let kvs = prop::collection::vec(("[a-z]{1,4}", inner.clone()), 1..4);
        prop_oneof![
            prop::collection::vec(inner.clone(), 1..4).prop_map(|v| format!("[{}]", v.join(", "))),
            prop::collection::vec(inner.clone(), 1..4).prop_map(|v| format!("({})", v.join(", "))),
            kvs.clone().prop_map(|kvs| {
                let body = kvs
                    .into_iter()
                    .map(|(k, v)| format!("{k}: {v}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{{{body}}}")
            }),
            ("[A-Z][a-z]{0,4}", kvs).prop_map(|(name, kvs)| {
                let body = kvs
                    .into_iter()
                    .map(|(k, v)| format!("{k}: {v}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{name}({body})")
            }),
        ]
    })
}
