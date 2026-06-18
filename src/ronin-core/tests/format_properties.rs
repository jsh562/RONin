//! Formatter property + snapshot tests (E005 Wave 1, T019 — COMPLETES FR-004).
//!
//! These tests pin the formatter's load-bearing invariants (project-instructions
//! §I "Never Corrupt User Data", §V "Verified Quality"):
//!
//! * **Idempotence (SC-001)** — `format(format(x)) == format(x)` for BOTH the
//!   whole document and a single subtree, across every config combination
//!   (indent widths × blank-line policies). Re-running the formatter on its own
//!   output never changes it.
//! * **Semantic round-trip (SC-002)** — formatting changes ONLY layout: the
//!   significant-token + comment stream is identical before and after, over a
//!   comment-heavy generated corpus and the on-disk corpus reused from E001.
//! * **Canonical-output snapshots** — `insta` snapshots lock the exact canonical
//!   layout of representative inputs so an unintended style change is caught.
//!
//! Reading corpus fixtures via `std::fs` here is fine: this is a **test**, not the
//! WASM-clean `ronin-core` core (the core itself touches no filesystem).

use std::fs;
use std::path::{Path, PathBuf};

use insta::assert_snapshot;
use proptest::prelude::*;
use ronin_core::{
    format, format_node, parse, BlankLinePolicy, FormatConfig, FormatResult, SyntaxKind,
};

// =============================================================================
// Helpers
// =============================================================================

/// Every config combination the properties run under (indent × blank-line policy).
fn all_configs() -> Vec<FormatConfig> {
    let mut out = Vec::new();
    for width in [1u32, 2, 4, 8, 16] {
        for policy in [BlankLinePolicy::Collapse, BlankLinePolicy::Preserve] {
            out.push(FormatConfig::new(width, policy));
        }
    }
    out
}

/// The semantic token stream of `src`: significant tokens + comments (verbatim),
/// in order, with whitespace / BOM / commas dropped. Two RON fragments are
/// semantically equal (modulo layout AND trailing-comma canonicalization) iff
/// their streams are equal — the same oracle the formatter uses internally.
fn semantic_stream(src: &str) -> Vec<(SyntaxKind, String)> {
    parse(src)
        .root()
        .descendant_tokens()
        .filter(|t| {
            !matches!(
                t.kind(),
                SyntaxKind::Whitespace | SyntaxKind::Bom | SyntaxKind::Comma
            )
        })
        .map(|t| (t.kind(), t.text().to_string()))
        .collect()
}

/// Format the whole document; `None` on a no-op.
fn fmt_doc(src: &str, cfg: &FormatConfig) -> Option<String> {
    match format(&parse(src), cfg) {
        FormatResult::Formatted(s) => Some(s),
        FormatResult::NoOp { .. } => None,
    }
}

// =============================================================================
// Strategies — including a comment-heavy generator (for the round-trip property).
// =============================================================================

/// Recursive strategy producing valid-RON-shaped source, optionally sprinkled with
/// line/block comments and varied whitespace so the formatter's comment-attachment
/// + reflow paths are exercised by generated input (not just the corpus).
fn ron_with_comments() -> impl Strategy<Value = String> {
    let leaf = prop_oneof![
        any::<i32>().prop_map(|n| n.to_string()),
        any::<bool>().prop_map(|b| b.to_string()),
        Just("()".to_string()),
        "[a-z]{1,6}".prop_map(|s| format!("\"{s}\"")),
        "[a-z]".prop_map(|c| format!("'{c}'")),
        (0i32..1000, 0u32..1000).prop_map(|(a, b)| format!("{a}.{b}")),
        // A bare enum variant ident.
        "[A-Z][a-z]{0,4}".prop_map(|s| s),
    ];

    leaf.prop_recursive(4, 48, 5, |inner| {
        // An element optionally carrying a leading and/or trailing comment + some
        // newlines, so collections become multi-line with comments to thread.
        let commented = (
            prop::option::of(Just("// lead\n")),
            inner.clone(),
            prop::option::of(Just(" // trail")),
            0usize..3, // blank lines after
        )
            .prop_map(|(lead, v, trail, blanks)| {
                let lead = lead.unwrap_or("");
                let trail = trail.unwrap_or("");
                let nl = "\n".repeat(blanks);
                format!("{lead}{v}{trail}{nl}")
            });

        let items = prop::collection::vec(commented.clone(), 0..4);
        let kvs = prop::collection::vec(
            (
                "[a-z]{1,5}",
                prop::option::of(Just("// c\n")),
                inner.clone(),
            ),
            0..4,
        );

        prop_oneof![
            // list, often multi-line via embedded newlines/comments
            items.clone().prop_map(|v| {
                let body = v.join(",\n");
                format!("[\n{body}\n]")
            }),
            // tuple (≥1 element to stay unambiguous)
            prop::collection::vec(inner.clone(), 1..4)
                .prop_map(|v| format!("(\n{}\n)", v.join(",\n"))),
            // named struct with optional per-field comments
            ("[A-Z][a-z]{0,5}", kvs.clone()).prop_map(|(name, kvs)| {
                let body = kvs
                    .into_iter()
                    .map(|(k, c, v)| format!("{}{k}: {v}", c.unwrap_or("")))
                    .collect::<Vec<_>>()
                    .join(",\n");
                format!("{name}(\n{body}\n)")
            }),
            // map with ident keys
            kvs.prop_map(|kvs| {
                let body = kvs
                    .into_iter()
                    .map(|(k, c, v)| format!("{}\"{k}\": {v}", c.unwrap_or("")))
                    .collect::<Vec<_>>()
                    .join(",\n");
                format!("{{\n{body}\n}}")
            }),
        ]
    })
}

// =============================================================================
// Idempotence (SC-001) — whole document and subtree, across all configs.
// =============================================================================

proptest! {
    /// `format(format(x)) == format(x)` for the whole document, every config.
    #[test]
    fn idempotent_whole_document(src in ron_with_comments()) {
        for cfg in all_configs() {
            let Some(once) = fmt_doc(&src, &cfg) else { continue };
            let Some(twice) = fmt_doc(&once, &cfg) else {
                prop_assert!(false, "second format no-op'd for {src:?}");
                unreachable!();
            };
            prop_assert_eq!(&twice, &once, "not idempotent (cfg {:?}) for {:?}", cfg, src);
        }
    }

    /// The format is a pure function of layout: the semantic token stream is
    /// preserved across formatting (SC-002) for the whole document.
    #[test]
    fn semantic_roundtrip_whole_document(src in ron_with_comments()) {
        let before = semantic_stream(&src);
        for cfg in all_configs() {
            let Some(out) = fmt_doc(&src, &cfg) else { continue };
            let after = semantic_stream(&out);
            prop_assert_eq!(&after, &before, "semantics changed (cfg {:?}) for {:?}", cfg, src);
        }
    }

    /// `format_node(format_node(x)) == format_node(x)` for the top-level value
    /// subtree, every config (subtree idempotence, SC-001).
    #[test]
    fn idempotent_subtree(src in ron_with_comments()) {
        for cfg in all_configs() {
            let doc = parse(&src);
            let Some(value) = doc.root().children().find(|n| is_value_node(n.kind())) else {
                continue;
            };
            let FormatResult::Formatted(once) = format_node(&value, &cfg) else { continue };
            // Re-parse the formatted subtree as a standalone document and re-format
            // its top-level value: the subtree formatter must be idempotent.
            let doc2 = parse(&once);
            let Some(value2) = doc2.root().children().find(|n| is_value_node(n.kind())) else {
                continue;
            };
            let FormatResult::Formatted(twice) = format_node(&value2, &cfg) else {
                prop_assert!(false, "subtree second format no-op'd for {src:?}");
                unreachable!();
            };
            prop_assert_eq!(&twice, &once, "subtree not idempotent (cfg {:?}) for {:?}", cfg, src);
        }
    }
}

/// `true` for value-position node kinds.
fn is_value_node(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::Struct
            | SyntaxKind::Tuple
            | SyntaxKind::List
            | SyntaxKind::Map
            | SyntaxKind::EnumVariant
            | SyntaxKind::Unit
            | SyntaxKind::Literal
    )
}

// =============================================================================
// Corpus semantic round-trip + idempotence (reuse the E001 corpus, SC-001/SC-002).
// =============================================================================

/// The corpus root (`tests/corpus/`).
fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("corpus")
}

/// Collect every `.ron` fixture under `dir`.
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

/// All clean (diagnostic-free) corpus fixtures, sorted, excluding the giant file
/// (kept out of the per-config sweep for test speed; covered once separately).
fn clean_corpus_fixtures() -> Vec<(PathBuf, String)> {
    let mut paths = Vec::new();
    collect_fixtures(&corpus_dir(), &mut paths);
    paths.sort();
    paths
        .into_iter()
        .filter(|p| fs::metadata(p).map(|m| m.len() < 200_000).unwrap_or(false))
        .filter_map(|p| {
            let bytes = fs::read(&p).ok()?;
            let src = String::from_utf8(bytes).ok()?;
            // Only diagnostic-free files are in the format domain; malformed
            // fixtures must (and do) no-op, asserted elsewhere.
            if parse(&src).diagnostics().is_empty() {
                Some((p, src))
            } else {
                None
            }
        })
        .collect()
}

/// Every clean corpus file formats to a SEMANTICALLY identical document under
/// every config — comments, names, order, and values preserved (SC-002).
#[test]
fn corpus_format_preserves_semantics_all_configs() {
    let fixtures = clean_corpus_fixtures();
    assert!(!fixtures.is_empty(), "expected clean corpus fixtures");
    for cfg in all_configs() {
        for (path, src) in &fixtures {
            let before = semantic_stream(src);
            let out = fmt_doc(src, &cfg)
                .unwrap_or_else(|| panic!("unexpected no-op for {} (cfg {cfg:?})", path.display()));
            let after = semantic_stream(&out);
            assert_eq!(
                after,
                before,
                "semantics changed for {} (cfg {cfg:?})",
                path.display()
            );
        }
    }
}

/// Every clean corpus file formats idempotently under every config (SC-001).
#[test]
fn corpus_format_is_idempotent_all_configs() {
    for cfg in all_configs() {
        for (path, src) in clean_corpus_fixtures() {
            let once = fmt_doc(&src, &cfg)
                .unwrap_or_else(|| panic!("no-op for {} (cfg {cfg:?})", path.display()));
            let twice = fmt_doc(&once, &cfg).unwrap_or_else(|| {
                panic!("second-pass no-op for {} (cfg {cfg:?})", path.display())
            });
            assert_eq!(
                once,
                twice,
                "not idempotent for {} (cfg {cfg:?})",
                path.display()
            );
        }
    }
}

/// The large (≥ 1 MB) fixture formats and is idempotent (default config) —
/// exercises the size axis once without bloating the per-config sweep.
#[test]
fn large_fixture_formats_idempotently() {
    let mut paths = Vec::new();
    collect_fixtures(&corpus_dir(), &mut paths);
    let large = paths
        .into_iter()
        .find(|p| {
            fs::metadata(p)
                .map(|m| m.len() >= 1_000_000)
                .unwrap_or(false)
        })
        .expect("a ≥ 1 MB fixture exists");
    let src = fs::read_to_string(&large).unwrap();
    let doc = parse(&src);
    if !doc.diagnostics().is_empty() {
        // If the big fixture isn't clean it's out of the format domain — skip.
        return;
    }
    let cfg = FormatConfig::default();
    let once = fmt_doc(&src, &cfg).expect("large fixture formats");
    let twice = fmt_doc(&once, &cfg).expect("large fixture re-formats");
    assert_eq!(once, twice, "large fixture not idempotent");
    assert_eq!(
        semantic_stream(&src),
        semantic_stream(&once),
        "large semantics changed"
    );
}

// =============================================================================
// Canonical-output snapshots (insta) — lock the exact layout.
// =============================================================================

fn snap(src: &str) -> String {
    match format(&parse(src), &FormatConfig::default()) {
        FormatResult::Formatted(s) => s,
        FormatResult::NoOp { reason } => format!("<NO-OP: {reason}>"),
    }
}

#[test]
fn snapshot_single_line_list() {
    assert_snapshot!(snap("[1,2,3]"));
}

#[test]
fn snapshot_multiline_list_trailing_comma() {
    assert_snapshot!(snap("[\n1,\n2,\n3\n]"));
}

#[test]
fn snapshot_nested_struct() {
    assert_snapshot!(snap(
        "Config(\nretries: 3,\nnested: Inner(\na: [1, 2],\nb: {\"k\": true}\n)\n)"
    ));
}

#[test]
fn snapshot_comments_threaded() {
    assert_snapshot!(snap(
        "// header\nFoo(\n// before x\nx: 1, // inline x\ny: 2,\n// before close\n)"
    ));
}

#[test]
fn snapshot_extension_attrs_and_comment() {
    assert_snapshot!(snap(
        "#![enable(implicit_some)]\n#![enable(unwrap_newtypes)]\n// note\n(a: 1, b: 2)"
    ));
}

#[test]
fn snapshot_dangling_comment_empty_collection() {
    assert_snapshot!(snap("[\n// nothing here\n]"));
}

#[test]
fn snapshot_map_non_string_keys() {
    assert_snapshot!(snap("{1:\"one\",'c':true}"));
}
