//! Property + example tests for the load-bearing invariant: byte-for-byte
//! lossless round-trip (TR-003, SC-001/SC-002) and idempotent printing
//! (TR-017, SC-009). Because the lexer covers every source byte, `parse → print`
//! is identity for *any* accepted UTF-8 input; these properties exercise that
//! across the full RON grammar surface (TR-004) and arbitrary text.

use proptest::prelude::*;
use ronin_core::{parse, parse_bytes, parse_with_options, print, DiagnosticCode, ParseOptions};

/// Concatenate every token's verbatim text under the document root. Equals the
/// source bytes for an unmodified tree (INV-2/INV-3), valid or malformed.
fn token_concat(doc: &ronin_core::CstDocument) -> String {
    doc.root()
        .descendant_tokens()
        .map(|t| t.text().to_string())
        .collect()
}

/// A recursive strategy generating valid-RON-shaped source strings across the
/// TR-004 construct surface: scalars (ints, floats, bools, chars, strings,
/// unit), lists, tuples, maps (incl. non-string keys), and named structs.
fn ron_strategy() -> impl Strategy<Value = String> {
    let leaf = prop_oneof![
        any::<i32>().prop_map(|n| n.to_string()),
        any::<bool>().prop_map(|b| b.to_string()),
        Just("()".to_string()),
        "[a-z]{1,6}".prop_map(|s| format!("\"{s}\"")),
        "[a-z]".prop_map(|c| format!("'{c}'")),
        (0i32..1000, 0u32..1000).prop_map(|(a, b)| format!("{a}.{b}")),
    ];
    leaf.prop_recursive(4, 64, 6, |inner| {
        let kvs = prop::collection::vec(("[a-z]{1,5}", inner.clone()), 0..5);
        prop_oneof![
            // list
            prop::collection::vec(inner.clone(), 0..5).prop_map(|v| format!("[{}]", v.join(", "))),
            // tuple (at least one element to stay unambiguous)
            prop::collection::vec(inner.clone(), 1..5).prop_map(|v| format!("({})", v.join(", "))),
            // map with non-string (ident) keys
            kvs.clone().prop_map(|kvs| {
                let body = kvs
                    .into_iter()
                    .map(|(k, v)| format!("{k}: {v}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{{{body}}}")
            }),
            // named struct
            ("[A-Z][a-z]{0,5}", kvs).prop_map(|(name, kvs)| {
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

proptest! {
    /// parse → print reproduces the input exactly (round-trip identity).
    #[test]
    fn roundtrip_identity(s in ron_strategy()) {
        let doc = parse(&s);
        prop_assert_eq!(print(&doc), s);
    }

    /// Printing is idempotent: a second parse→print of the output equals the first.
    #[test]
    fn idempotent_print(s in ron_strategy()) {
        let once = print(&parse(&s));
        let twice = print(&parse(&once));
        prop_assert_eq!(twice, once);
    }

    /// Losslessness holds for arbitrary UTF-8 text, not just valid RON
    /// (the lexer covers every byte; malformed input still round-trips).
    #[test]
    fn arbitrary_text_roundtrips(s in ".{0,200}") {
        let doc = parse(&s);
        prop_assert_eq!(print(&doc), s);
    }
}

/// Explicit trivia-preservation cases (comments, CRLF, BOM, empty / ws-only /
/// comments-only files) — the AD-001 trivia rule must keep every byte.
#[test]
fn preserves_trivia_and_edge_cases() {
    for s in [
        "// lead\nFoo(x: 1) // trail\n",
        "/* block */ [1, 2, 3,]",
        "\r\n  {a: 1,\r\n b: 2}\r\n",
        "\u{feff}42",        // leading BOM
        "",                  // empty file
        "   \n\t ",          // whitespace-only
        "// only a comment", // comments-only, no trailing newline
        "(1, 2, 3)",         // tuple
    ] {
        let doc = parse(s);
        assert_eq!(print(&doc), s, "round-trip failed for {s:?}");
    }
}

// =====================================================================
// OBJ2 — error-tolerant parsing + diagnostics (T026, T027, T028)
// =====================================================================

/// T026 / SC-004 / INV-3: ERROR-node coverage on malformed samples — token
/// concatenation equals the input byte-for-byte even for unclosed delimiters,
/// stray tokens, and partial top-level values, and parsing never panics.
#[test]
fn malformed_samples_cover_all_input() {
    for s in [
        // Unclosed delimiters.
        "[1, 2, 3",
        "Foo(x: 1, y: 2",
        "{ \"a\": 1, \"b\": 2",
        "Some(",
        "[[[",
        // Stray / unbalanced closers.
        "]]]",
        ")})",
        "Foo(x: 1))",
        // Stray tokens at value position and inside groups.
        "@",
        "@#$ %^&",
        "[1 @ 2 # 3]",
        "{a 1 b 2}",
        // Partial top-level value.
        "Foo(",
        "Variant {",
        "Enum::",
        // Trailing content after a complete value.
        "Foo(x: 1) extra junk",
        "42 43 44",
        // Mixed and nested malformation.
        "[1, [2, {k: ", // deeply unclosed
        "#![enable(implicit_some)\nSome(5",
    ] {
        let doc = parse(s);
        // INV-3: every byte is still covered by exactly one token.
        assert_eq!(token_concat(&doc), s, "token coverage failed for {s:?}");
        // print() is the canonical lossless serialization and must agree.
        assert_eq!(print(&doc), s, "print round-trip failed for {s:?}");
        // Diagnostic ranges must lie within [0, source_len] and be ordered.
        for d in doc.diagnostics() {
            assert!(d.range().start() <= d.range().end());
            assert!(d.range().end() <= doc.source_len(), "range OOB for {s:?}");
        }
    }
}

/// T027 / SC-008 / INV-5: at the configured bound + 1 the depth guard trips —
/// no stack overflow, an over-limit diagnostic is emitted, and the tree still
/// round-trips byte-for-byte. Exercised across several small bounds.
#[test]
fn depth_limit_at_bound_plus_one() {
    for bound in [0usize, 1, 2, 5, 16] {
        let opts = ParseOptions::default().with_max_depth(bound);
        // bound + 1 nested lists → must trip the guard exactly once at the limit.
        let n = bound + 1;
        let src = format!("{}1{}", "[".repeat(n), "]".repeat(n));
        let doc = parse_with_options(&src, opts);

        assert_eq!(
            print(&doc),
            src,
            "depth-limited round-trip failed (bound {bound})"
        );
        assert_eq!(
            token_concat(&doc),
            src,
            "byte coverage failed (bound {bound})"
        );

        let over: Vec<_> = doc
            .diagnostics()
            .iter()
            .filter(|d| d.code() == DiagnosticCode::NestingDepthExceeded)
            .collect();
        assert_eq!(
            over.len(),
            1,
            "expected exactly one over-limit diagnostic (bound {bound}), got {:?}",
            doc.diagnostics()
        );
    }
}

/// The default depth guard (128) tolerates very deep input without overflowing
/// the stack and still round-trips (INV-5). 4000 levels far exceeds 128, so the
/// guard converts the overflow risk into a single diagnostic.
#[test]
fn default_depth_guard_handles_pathological_nesting() {
    let n = 4000usize;
    let src = format!("{}1{}", "[".repeat(n), "]".repeat(n));
    let doc = parse(&src); // default ParseOptions (max_depth = 128)
    assert_eq!(print(&doc), src, "deep-nesting round-trip failed");
    assert!(doc
        .diagnostics()
        .iter()
        .any(|d| d.code() == DiagnosticCode::NestingDepthExceeded));
}

// ---------------------------------------------------------------------
// T028 — stable-toolchain fuzz fallback.
//
// SC-003 specifies a seeded cargo-fuzz run (≥ 1,000,000 iterations) that needs
// the nightly toolchain + libFuzzer (see src/ronin-core/fuzz/). That cannot run on
// this stable-only machine, so these proptests verify SC-003's *intent* on
// stable in CI: feed arbitrary bytes / strings through parse / parse_bytes and
// assert no panic + byte-for-byte round-trip + clean non-UTF-8 rejection. The
// cargo-fuzz target shares the same assertions for the nightly gate.
// ---------------------------------------------------------------------

proptest! {
    // Run more cases here than the default to better approximate fuzzing on
    // stable. Still deterministic and fast.
    #![proptest_config(ProptestConfig::with_cases(4096))]

    /// Arbitrary UTF-8 strings: parse never panics and round-trips (INV-2/INV-3).
    #[test]
    fn arbitrary_str_no_panic_roundtrip(s in ".{0,300}") {
        let doc = parse(&s);
        prop_assert_eq!(token_concat(&doc), s.as_str());
        prop_assert_eq!(print(&doc), s.as_str());
    }

    /// Arbitrary RON-ish strings with structural punctuation injected so the
    /// recovery paths (unclosed/stray/depth) are exercised heavily.
    #[test]
    fn arbitrary_structural_no_panic_roundtrip(
        s in r#"[\[\](){}\x20,:@#!a1"'.\-]{0,120}"#
    ) {
        let doc = parse(&s);
        prop_assert_eq!(token_concat(&doc), s.as_str());
        prop_assert_eq!(print(&doc), s.as_str());
        // No diagnostic may point outside the source.
        for d in doc.diagnostics() {
            prop_assert!(d.range().end() <= doc.source_len());
        }
    }

    /// Arbitrary raw bytes through parse_bytes: valid UTF-8 round-trips; invalid
    /// UTF-8 is rejected cleanly (Err, never a panic) — INV-4 / SC-003.
    #[test]
    fn arbitrary_bytes_no_panic(bytes in prop::collection::vec(any::<u8>(), 0..300)) {
        match parse_bytes(&bytes) {
            Ok(doc) => {
                // Accepted only for valid UTF-8; must round-trip to that text.
                let s = std::str::from_utf8(&bytes).expect("Ok implies valid UTF-8");
                prop_assert_eq!(token_concat(&doc), s);
            }
            Err(_) => {
                // Rejected ⇒ the bytes were not valid UTF-8.
                prop_assert!(std::str::from_utf8(&bytes).is_err());
            }
        }
    }
}
