//! Snapshot tests (insta) locking the CST shape per literal kind (TR-004): raw
//! strings with embedded quotes/hashes, numeric/escape forms, char literals,
//! non-string map keys, and extension attributes. Snapshots guard against
//! tree-structure regressions; run `cargo insta review` to accept intended changes.

use insta::assert_snapshot;
use ronin_core::{parse, SyntaxElement, SyntaxNode};

/// Render the CST as an indented `Kind@range` / token dump for snapshotting.
fn dump(node: &SyntaxNode, depth: usize, out: &mut String) {
    use std::fmt::Write;
    let _ = writeln!(out, "{}{node:?}", "  ".repeat(depth));
    for child in node.children_with_tokens() {
        match child {
            SyntaxElement::Node(n) => dump(&n, depth + 1, out),
            SyntaxElement::Token(t) => {
                let _ = writeln!(out, "{}{t:?}", "  ".repeat(depth + 1));
            }
        }
    }
}

fn tree(src: &str) -> String {
    let mut out = String::new();
    dump(&parse(src).root(), 0, &mut out);
    out
}

#[test]
fn raw_string_with_hashes() {
    assert_snapshot!(tree(r##"r#"a "quoted" b"#"##));
}

#[test]
fn numeric_and_escape_forms() {
    assert_snapshot!(tree(r#"(0xFF, 1_000, 3.14, -2, "tab\tnl\n")"#));
}

#[test]
fn char_literals() {
    assert_snapshot!(tree(r#"['a', '\n', '\'']"#));
}

#[test]
fn non_string_map_keys() {
    assert_snapshot!(tree("{1: \"a\", true: \"b\"}"));
}

#[test]
fn extension_attributes() {
    assert_snapshot!(tree("#![enable(implicit_some)]\nFoo(x: 1)"));
}
