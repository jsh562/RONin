//! The printer: a [`CstDocument`] → its exact source bytes.
//!
//! [`print`] walks the CST in source order and concatenates every token's
//! verbatim text. For an unmodified tree this reproduces the input byte-for-byte
//! (round-trip identity, INV-2 / TR-003). Because [`print`] reads only token
//! text — which the lexer captured verbatim and the parser never normalized —
//! printing is also idempotent: `print(parse(print(parse(x)))) == print(parse(x))`
//! (TR-017 / INV-2).

use crate::parser::CstDocument;
use crate::syntax::SyntaxNode;

/// Print a [`CstDocument`] back to source text by concatenating all token text.
///
/// For an unmodified tree the result equals the original source bytes exactly.
#[must_use]
pub fn print(doc: &CstDocument) -> String {
    print_node(&doc.root())
}

/// Print an arbitrary [`SyntaxNode`] subtree to text (concatenated token text).
///
/// Useful for printing a fragment of a tree (e.g. for edit primitives in OBJ4).
#[must_use]
pub fn print_node(node: &SyntaxNode) -> String {
    // `SyntaxNode::text()` already concatenates the verbatim text of every
    // descendant token in source order; this is the canonical lossless print.
    node.text()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn assert_roundtrip(src: &str) {
        let doc = parse(src);
        assert_eq!(print(&doc), src, "round-trip identity for {src:?}");
    }

    fn assert_idempotent(src: &str) {
        let first = print(&parse(src));
        let second = print(&parse(&first));
        assert_eq!(second, first, "idempotent print for {src:?}");
    }

    #[test]
    fn round_trip_identity() {
        for src in [
            "",
            "   ",
            "// c\n",
            "Foo(x: 1, y: 2.0) // trailing\n",
            "[1, 2, 3,]",
            "{ 'a': 1, 2: \"b\", }",
            "r#\"raw\"#",
            "#![enable(implicit_some)]\nSome(())",
            "\u{FEFF}true",
            "1\r\n2",
        ] {
            assert_roundtrip(src);
        }
    }

    #[test]
    fn idempotent_print() {
        for src in [
            "Foo(x: 1)",
            "[1, 2, 3]",
            "{ \"k\": 'v' }",
            "#![enable(implicit_some)]\nSome(5)",
            "  spaced  out  ",
        ] {
            assert_idempotent(src);
        }
    }
}
