//! Cross-action lossless round-trip — the engine-level half (E005 Wave 5, T040 —
//! COMPLETES FR-019).
//!
//! FR-019's guarantee is that **no authoring action drops comments, reorders
//! content, or changes values**. The three authoring actions are:
//!
//! * **format** — `ronin_core::format` / `format_node` (Wave 1);
//! * **completion-accept insertion** — a splice of a `CompletionItem::insert_text`
//!   over the in-progress prefix (Wave 3);
//! * **snippet insertion** — a splice of an expanded snippet body (Wave 4).
//!
//! The formatter half of this guarantee is pinned in `format_preservation.rs`
//! (every comment / name / value / order preserved across the E001 corpus). The
//! insertion halves are app-layer logic (`ronin-app`'s `completion::accept_item`
//! and `snippets::insert_snippet`), so their app-level coverage lives in
//! `ronin-app/tests/cross_action_round_trip.rs`.
//!
//! This file pins the *engine-level* cross-action invariants the insertion paths
//! depend on, over the representative SC-005 corpus (the E001 `tests/corpus/`,
//! which spans every collection kind, line / block comments, boundary +
//! dangling-in-empty comments, and nesting depth ≥ 3):
//!
//! 1. **format is lossless on the whole corpus** — comments, names, values, and
//!    order are byte-identical (modulo whitespace) before and after; and
//! 2. **a completion `insert_text` spliced into a corpus document at a value slot
//!    re-parses and preserves every pre-existing comment, name, value, and their
//!    order** — the splice only *adds* the inserted token(s); it never drops or
//!    reorders what was already there.
//!
//! Reading corpus fixtures via `std::fs` is fine here — this is a test, not the
//! WASM-clean core.

use std::fs;
use std::path::{Path, PathBuf};

use ronin_core::{
    completion_context, format, parse, BlankLinePolicy, FormatConfig, FormatResult, SyntaxKind,
};

// ---- corpus loading ---------------------------------------------------------

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

/// Every clean (diagnostic-free, reasonably-sized) corpus fixture.
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

// ---- semantic token streams (the FR-019 oracle) -----------------------------

/// The verbatim text of every comment token, in source order.
fn comments(src: &str) -> Vec<String> {
    parse(src)
        .root()
        .descendant_tokens()
        .filter(|t| matches!(t.kind(), SyntaxKind::LineComment | SyntaxKind::BlockComment))
        .map(|t| t.text().to_string())
        .collect()
}

/// The verbatim text of every significant name / value token, in source order.
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

/// `true` if every element of `sub` appears in `sup` in the same relative order
/// (an order-preserving subsequence).
///
/// This is the precise FR-019 oracle for an *additive* authoring action: an
/// insertion may add new tokens anywhere, but must never **drop** a pre-existing
/// token nor **reorder** the ones already present. So the pre-insertion stream must
/// survive as a (not-necessarily-contiguous) subsequence of the post-insertion
/// stream — the added tokens are the only difference, and the original ones keep
/// their order. (A value *change* would alter a token's text and break the match.)
fn is_subsequence(sup: &[String], sub: &[String]) -> bool {
    let mut it = sup.iter();
    sub.iter().all(|needle| it.any(|hay| hay == needle))
}

// ---- 1. format is lossless on the whole corpus (FR-019, format leg) ----------

#[test]
fn format_preserves_comments_names_values_order_across_corpus() {
    for cfg in [
        FormatConfig::new(4, BlankLinePolicy::Collapse),
        FormatConfig::new(2, BlankLinePolicy::Preserve),
    ] {
        for (path, src) in clean_fixtures() {
            let FormatResult::Formatted(out) = format(&parse(&src), &cfg) else {
                panic!("format no-op for {} (cfg {cfg:?})", path.display());
            };
            assert_eq!(
                comments(&src),
                comments(&out),
                "format dropped/reordered comments for {} (cfg {cfg:?})",
                path.display()
            );
            assert_eq!(
                names_and_values(&src),
                names_and_values(&out),
                "format changed names/values/order for {} (cfg {cfg:?})",
                path.display()
            );
        }
    }
}

// ---- 2. completion-accept insertion is additive-only (FR-019, completion leg) -

/// Splice `insert_text` over the (empty) prefix at byte `offset`, mirroring the
/// app layer's `accept_item` splice, and return the new buffer.
fn splice(buffer: &str, offset: usize, insert_text: &str) -> String {
    let mut out = String::with_capacity(buffer.len() + insert_text.len());
    out.push_str(&buffer[..offset]);
    out.push_str(insert_text);
    out.push_str(&buffer[offset..]);
    out
}

/// For each clean corpus fixture, find an **empty** value slot, take the offered
/// completion suggestions, splice each `insert_text` in under the same
/// verify-before-replace guard the app uses, and assert the splice:
///
/// * adds no new parse diagnostic (verify-before-replace, project-instructions §I);
///   a splice that would corrupt is refused (skipped), exactly as `accept_item`
///   refuses it — never asserted as a loss; and, when accepted,
/// * never drops/reorders any pre-existing comment, name, or value (the original
///   semantic-token stream is an order-preserving subsequence of the spliced one).
///
/// An *empty* slot (next significant char is a close delimiter) is required so the
/// inserted token never fuses with an adjacent value token — that fusion is a
/// malformed-edit artifact the editor never produces (completion fires at a fresh
/// slot or replaces an in-progress prefix), and the verify guard catches it anyway.
#[test]
fn completion_insert_text_is_additive_only_across_corpus() {
    for (path, src) in clean_fixtures() {
        let Some(offset) = first_empty_value_slot(&src) else {
            continue;
        };
        let ctx = completion_context(&parse(&src), offset);
        if ctx.items.is_empty() || !ctx.prefix.is_empty() {
            continue;
        }
        let before_comments = comments(&src);
        let before_names = names_and_values(&src);
        let before_diags = parse(&src).diagnostics().len();
        for item in &ctx.items {
            let spliced = splice(&src, offset, &item.insert_text);
            // Verify-before-replace: a splice that would add a parse error is
            // refused by the app, so it is never a "loss" to assert against.
            if parse(&spliced).diagnostics().len() > before_diags {
                continue;
            }
            assert!(
                is_subsequence(&comments(&spliced), &before_comments),
                "completion `{}` dropped/reordered a comment in {}\nbefore: {before_comments:?}\nafter:  {:?}",
                item.insert_text,
                path.display(),
                comments(&spliced),
            );
            assert!(
                is_subsequence(&names_and_values(&spliced), &before_names),
                "completion `{}` dropped/reordered a name/value in {}\nbefore: {before_names:?}\nafter:  {:?}",
                item.insert_text,
                path.display(),
                names_and_values(&spliced),
            );
        }
    }
}

/// The byte offset of the first **empty** value slot in `src`: just after an
/// opening delimiter (`[` / `(` / `{`) whose next significant token is the matching
/// close delimiter, so an inserted value cannot fuse with an existing one.
fn first_empty_value_slot(src: &str) -> Option<usize> {
    let bytes = src.as_bytes();
    let doc = parse(src);
    for tok in doc.root().descendant_tokens() {
        let open = tok.kind();
        if !matches!(
            open,
            SyntaxKind::LBracket | SyntaxKind::LParen | SyntaxKind::LBrace
        ) {
            continue;
        }
        let after = tok.text_range().end();
        // Scan forward over whitespace; the slot is empty when the next
        // non-whitespace byte is the matching close delimiter.
        let mut i = after;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let close = match open {
            SyntaxKind::LBracket => b']',
            SyntaxKind::LParen => b')',
            _ => b'}',
        };
        if i < bytes.len() && bytes[i] == close {
            return Some(after);
        }
    }
    None
}

// ---- targeted: comment-dense fixtures specifically (FR-019) ------------------

/// A focused check on the comment-heavy fixtures: format then a completion splice
/// at a value slot both keep every comment.
#[test]
fn comment_dense_fixtures_keep_every_comment_under_both_actions() {
    let cfg = FormatConfig::default();
    for name in ["27_comments_line.ron", "28_comments_block.ron"] {
        let path = corpus_dir().join("valid").join(name);
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let Ok(src) = String::from_utf8(bytes) else {
            continue;
        };
        if !parse(&src).diagnostics().is_empty() {
            continue;
        }
        let before = comments(&src);

        // Format leg.
        if let FormatResult::Formatted(out) = format(&parse(&src), &cfg) {
            assert_eq!(before, comments(&out), "format lost a comment in {name}");
        }

        // Completion leg: splice the first additive suggestion at an empty value
        // slot, under the verify-before-replace guard.
        if let Some(offset) = first_empty_value_slot(&src) {
            let ctx = completion_context(&parse(&src), offset);
            let before_diags = parse(&src).diagnostics().len();
            if ctx.prefix.is_empty() {
                if let Some(item) = ctx.items.first() {
                    let spliced = splice(&src, offset, &item.insert_text);
                    if parse(&spliced).diagnostics().len() <= before_diags {
                        assert!(
                            is_subsequence(&comments(&spliced), &before),
                            "completion splice lost a comment in {name}"
                        );
                    }
                }
            }
        }
    }
}
