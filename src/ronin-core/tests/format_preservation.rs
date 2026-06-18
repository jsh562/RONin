//! Formatter preservation + trailing-comma oracle tests (E005 Wave 1, T020 —
//! COMPLETES FR-003, COMPLETES FR-008).
//!
//! Two guarantees are pinned here against the E001 corpus and targeted cases:
//!
//! * **Preservation (FR-003, SC-002)** — after formatting, every comment (line
//!   `//` and block `/* */`, including comments at construct boundaries and
//!   dangling comments in empty collections) is retained verbatim; element /
//!   field / entry order is unchanged; and all struct/variant names and scalar
//!   values are unchanged.
//! * **Trailing-comma oracle (FR-008, SC-002)** — a deterministic, fixed rule:
//!   a multi-line collection has a trailing comma after EVERY element including
//!   the last; a single-line collection has NONE after the last. Applied
//!   identically every run.
//!
//! Reading corpus fixtures via `std::fs` is fine here — this is a test, not the
//! WASM-clean core.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use ronin_core::{format, parse, BlankLinePolicy, FormatConfig, FormatResult, SyntaxKind};

/// Format the whole document; `None` on a no-op.
fn fmt(src: &str, cfg: &FormatConfig) -> Option<String> {
    match format(&parse(src), cfg) {
        FormatResult::Formatted(s) => Some(s),
        FormatResult::NoOp { .. } => None,
    }
}

/// Collect the verbatim text of every comment token in `src`, in source order.
fn comments(src: &str) -> Vec<String> {
    parse(src)
        .root()
        .descendant_tokens()
        .filter(|t| matches!(t.kind(), SyntaxKind::LineComment | SyntaxKind::BlockComment))
        .map(|t| t.text().to_string())
        .collect()
}

/// Collect the verbatim text of every significant value token (`Ident`, scalars),
/// in source order — i.e. names + values, ignoring structure / layout.
fn names_and_values(src: &str) -> Vec<String> {
    parse(src)
        .root()
        .descendant_tokens()
        .filter(|t| {
            matches!(
                t.kind(),
                SyntaxKind::Ident
                    | SyntaxKind::Integer
                    | SyntaxKind::Float
                    | SyntaxKind::String
                    | SyntaxKind::RawString
                    | SyntaxKind::Char
                    | SyntaxKind::TrueKw
                    | SyntaxKind::FalseKw
            )
        })
        .map(|t| t.text().to_string())
        .collect()
}

// =============================================================================
// Corpus preservation (FR-003).
// =============================================================================

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("corpus")
}

fn collect_fixtures(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read corpus dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_fixtures(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("ron") {
            out.push(path);
        }
    }
}

fn clean_fixtures() -> Vec<(PathBuf, String)> {
    let mut paths = Vec::new();
    collect_fixtures(&corpus_dir(), &mut paths);
    paths.sort();
    paths
        .into_iter()
        .filter(|p| fs::metadata(p).map(|m| m.len() < 200_000).unwrap_or(false))
        .filter_map(|p| {
            let src = String::from_utf8(fs::read(&p).ok()?).ok()?;
            parse(&src).diagnostics().is_empty().then_some((p, src))
        })
        .collect()
}

/// Every comment in every clean corpus file survives formatting, verbatim and in
/// order, under both blank-line policies (FR-003).
#[test]
fn corpus_comments_preserved() {
    for cfg in [
        FormatConfig::new(4, BlankLinePolicy::Collapse),
        FormatConfig::new(2, BlankLinePolicy::Preserve),
    ] {
        for (path, src) in clean_fixtures() {
            let before = comments(&src);
            let out = fmt(&src, &cfg)
                .unwrap_or_else(|| panic!("no-op for {} (cfg {cfg:?})", path.display()));
            let after = comments(&out);
            assert_eq!(
                before,
                after,
                "comments changed for {} (cfg {cfg:?})",
                path.display()
            );
        }
    }
}

/// Every name and value in every clean corpus file survives formatting, verbatim
/// and in order — names/values/order never change (FR-003).
#[test]
fn corpus_names_values_and_order_preserved() {
    let cfg = FormatConfig::default();
    for (path, src) in clean_fixtures() {
        let before = names_and_values(&src);
        let out =
            fmt(&src, &cfg).unwrap_or_else(|| panic!("no-op for {} (cfg {cfg:?})", path.display()));
        let after = names_and_values(&out);
        assert_eq!(
            before,
            after,
            "names/values/order changed for {}",
            path.display()
        );
    }
}

// =============================================================================
// Targeted preservation: boundary + dangling-in-empty comments (FR-003).
// =============================================================================

/// Boundary comments (just before a closing `)` / `]` / `}`) and dangling
/// comments inside an empty collection are retained (FR-003).
#[test]
fn boundary_and_dangling_comments_preserved() {
    let cases = [
        // dangling-in-empty
        "[\n// only a comment\n]",
        "{\n// dangling map comment\n}",
        "Foo(\n// dangling struct comment\n)",
        // boundary (before close, after last element)
        "[\n1,\n// trailing boundary\n]",
        "Foo(\nx: 1,\n// boundary before paren\n)",
        "{\n\"a\": 1,\n// boundary before brace\n}",
        // block comments at boundaries
        "[\n1,\n/* block boundary */\n]",
        // inline + own-line mix
        "Foo(\nx: 1, // inline\n// own-line\ny: 2,\n)",
    ];
    let cfg = FormatConfig::default();
    for src in cases {
        let before = comments(src);
        let out = fmt(src, &cfg).unwrap_or_else(|| panic!("unexpected no-op for {src:?}"));
        let after = comments(&out);
        assert_eq!(
            before, after,
            "comments lost/reordered for {src:?}\nout:\n{out}"
        );
    }
}

// =============================================================================
// Trailing-comma oracle (FR-008).
// =============================================================================

/// A multi-line collection gets a trailing comma after every element including the
/// last; the close delimiter sits on its own line preceded by a `,` line (FR-008).
#[test]
fn multiline_has_trailing_comma_after_last() {
    let cfg = FormatConfig::default();
    let cases = [
        ("[\n1,\n2,\n3\n]", "]"),
        ("Foo(\nx: 1,\ny: 2\n)", ")"),
        ("{\n\"a\": 1,\n\"b\": 2\n}", "}"),
        ("(\n1,\n2\n)", ")"),
    ];
    for (src, close) in cases {
        let out = fmt(src, &cfg).unwrap_or_else(|| panic!("no-op for {src:?}"));
        // The last element line must end in a comma: find the line right before the
        // closing-delimiter line.
        let lines: Vec<&str> = out.lines().collect();
        let close_idx = lines
            .iter()
            .rposition(|l| l.trim() == close)
            .unwrap_or_else(|| panic!("no close line in {out:?}"));
        assert!(
            close_idx >= 1,
            "close delimiter has no preceding element line: {out:?}"
        );
        let last_element_line = lines[close_idx - 1].trim_end();
        assert!(
            last_element_line.ends_with(','),
            "multi-line last element missing trailing comma: {out:?}"
        );
    }
}

/// A single-line collection has NO trailing comma after the last element (FR-008).
#[test]
fn single_line_has_no_trailing_comma() {
    let cfg = FormatConfig::default();
    let cases = [
        ("[1, 2, 3]", "[1, 2, 3]\n"),
        ("[1, 2, 3,]", "[1, 2, 3]\n"), // a present trailing comma is removed
        ("Foo(x: 1, y: 2)", "Foo(x: 1, y: 2)\n"),
        ("{\"a\": 1, \"b\": 2}", "{\"a\": 1, \"b\": 2}\n"),
        ("(1, 2)", "(1, 2)\n"),
    ];
    for (src, expected) in cases {
        let out = fmt(src, &cfg).unwrap_or_else(|| panic!("no-op for {src:?}"));
        assert_eq!(out, expected, "single-line canonical mismatch for {src:?}");
    }
}

/// The oracle is FIXED: formatting the same input twice yields byte-identical
/// trailing-comma placement (deterministic, FR-008).
#[test]
fn trailing_comma_oracle_is_deterministic() {
    let cfg = FormatConfig::default();
    for src in [
        "[\n1,\n2,\n3\n]",
        "[1, 2, 3]",
        "Foo(\nx: 1,\ny: 2\n)",
        "{\n\"a\": 1\n}",
    ] {
        let a = fmt(src, &cfg).unwrap();
        let b = fmt(src, &cfg).unwrap();
        assert_eq!(a, b, "non-deterministic format for {src:?}");
    }
}

// =============================================================================
// Order preservation under a structural model (FR-003) — defends against any
// future reordering bug beyond the flat-token check above.
// =============================================================================

/// Format must NOT reorder struct fields / map entries (FR-003). We compare the
/// ordered key sequence of each struct/map before and after.
#[test]
fn field_and_entry_order_unchanged() {
    let cfg = FormatConfig::default();
    let cases = [
        "Config(zeta: 1, alpha: 2, mid: 3)",
        "{\"z\": 1, \"a\": 2, \"m\": 3}",
        "Outer(b: Inner(y: 1, x: 2), a: [3, 1, 2])",
    ];
    for src in cases {
        let before = ordered_keys(src);
        let out = fmt(src, &cfg).unwrap();
        let after = ordered_keys(&out);
        assert_eq!(before, after, "order changed for {src:?}");
        // Sanity: the key sets are non-trivial.
        assert!(!before.is_empty(), "no keys extracted for {src:?}");
    }
}

/// Extract the ordered field/entry key tokens (idents + string keys) per source.
fn ordered_keys(src: &str) -> Vec<String> {
    // The first significant token of every StructField / MapEntry key, in source
    // order. A BTreeMap is NOT used (it would sort) — order must be source order.
    let doc = parse(src);
    let mut keys = Vec::new();
    collect_keys(&doc.root(), &mut keys);
    // Guard the helper below is actually exercised.
    let _ = BTreeMap::<(), ()>::new();
    keys
}

fn collect_keys(node: &ronin_core::SyntaxNode, out: &mut Vec<String>) {
    use ronin_core::SyntaxElement;
    if node.kind() == SyntaxKind::StructField {
        if let Some(t) = node.first_token_of(SyntaxKind::Ident) {
            out.push(t.text().to_string());
        }
    }
    for child in node.children_with_tokens() {
        match child {
            SyntaxElement::Node(n) => {
                if n.kind() == SyntaxKind::MapEntry {
                    // The map key is the first value node's first significant token.
                    if let Some(key_node) = n.children().next() {
                        if let Some(tok) = key_node.descendant_tokens().find(|t| !t.is_trivia()) {
                            out.push(tok.text().to_string());
                        }
                    }
                }
                collect_keys(&n, out);
            }
            SyntaxElement::Token(_) => {}
        }
    }
}
