//! Cross-action lossless round-trip — the app-layer half (E005 Wave 5, T040 —
//! COMPLETES FR-019).
//!
//! FR-019's guarantee: **no authoring action drops comments, reorders content, or
//! changes values**. The three actions are format, completion-accept insertion,
//! and snippet insertion. The formatter's round-trip is pinned in `ronin-core`
//! (`format_preservation.rs` + `cross_action_round_trip.rs`); this file adds the
//! two *insertion* legs at the app/logic layer:
//!
//! * **completion-accept** via [`accept_item`] — replacing the in-progress prefix
//!   with a `CompletionItem::insert_text`;
//! * **snippet insertion** via [`insert_snippet`] — splicing an expanded snippet
//!   body at the caret.
//!
//! Both already re-parse + verify before committing (verify-before-replace,
//! project-instructions §I); here we assert the *content* guarantee on
//! representative SC-005 cases spanning all collection kinds, line + block
//! comments, boundary + dangling-in-empty comments, and nesting depth ≥ 3:
//!
//! 1. the spliced buffer re-parses cleanly (no new diagnostics), and
//! 2. every pre-existing comment / name / value survives in order (the original
//!    semantic-token stream is a contiguous subsequence of the spliced one — the
//!    splice only *adds* tokens, never drops or reorders the existing content).

use ronin_core::{completion_context, parse, SyntaxKind};
use ronin_app::completion::accept_item;
use ronin_app::snippets::insert_snippet;

// ---- semantic token oracle --------------------------------------------------

fn comments(src: &str) -> Vec<String> {
    parse(src)
        .root()
        .descendant_tokens()
        .filter(|t| matches!(t.kind(), SyntaxKind::LineComment | SyntaxKind::BlockComment))
        .map(|t| t.text().to_string())
        .collect()
}

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

/// `true` if `sub` appears as a contiguous run inside `sup`.
fn contains_contiguous(sup: &[String], sub: &[String]) -> bool {
    if sub.is_empty() {
        return true;
    }
    if sub.len() > sup.len() {
        return false;
    }
    sup.windows(sub.len()).any(|w| w == sub)
}

/// Byte offset just after the first list/tuple/map/struct open delimiter (a fresh
/// value slot), or end-of-buffer for a bare top-level value.
fn first_value_slot(src: &str) -> usize {
    for tok in parse(src).root().descendant_tokens() {
        if matches!(
            tok.kind(),
            SyntaxKind::LBracket | SyntaxKind::LParen | SyntaxKind::LBrace
        ) {
            return tok.text_range().end();
        }
    }
    src.len()
}

/// Char offset of `byte_offset` within `src` (the app caret model is char-based).
fn byte_to_char(src: &str, byte_offset: usize) -> usize {
    src[..byte_offset.min(src.len())].chars().count()
}

/// Representative SC-005 cases: every collection kind, line + block comments,
/// boundary + dangling-in-empty comments, and nesting depth ≥ 3.
fn cases() -> Vec<&'static str> {
    vec![
        // line + block comments at boundaries, nested struct/list/map
        "Scene(\n    // header comment\n    entities: [\n        Entity(id: 1), // inline\n        Entity(id: 2),\n        /* block boundary */\n    ],\n)",
        // dangling comment in an empty list
        "Outer(\n    items: [\n        // nothing yet\n    ],\n)",
        // map with string keys + comments + nesting depth 3
        "{\n    \"a\": {\n        \"b\": [1, 2, 3], // deep\n    },\n}",
        // tuple + enum variants + a trailing comment
        "(\n    Alpha,\n    Beta(1, 2),\n    Gamma(x: 1), // tail\n)",
        // dangling comment inside an empty struct
        "Config(\n    // to be filled\n)",
        // deeply nested mixed collections (depth 4) with comments throughout
        "Root(\n    a: [\n        {\n            \"k\": (\n                // innermost\n                Some(42),\n            ),\n        },\n    ],\n)",
    ]
}

// ---- completion-accept insertion leg (FR-019) -------------------------------

#[test]
fn completion_accept_insertion_round_trips_losslessly() {
    for src in cases() {
        // A clean fixture is the precondition (completion only fires on parseable
        // context); every case here parses cleanly.
        assert!(
            parse(src).diagnostics().is_empty(),
            "test case must be clean RON: {src:?}"
        );
        let slot_byte = first_value_slot(src);
        let caret_char = byte_to_char(src, slot_byte);
        let ctx = completion_context(&parse(src), slot_byte);
        if ctx.items.is_empty() || !ctx.prefix.is_empty() {
            // No additive completion at this slot for this case; skip.
            continue;
        }
        let before_comments = comments(src);
        let before_names = names_and_values(src);
        let before_diags = parse(src).diagnostics().len();

        for item in &ctx.items {
            // Accept at an empty prefix (purely additive insertion).
            let Some(accepted) = accept_item(src, slot_byte, "", item) else {
                // A refused splice (verify-before-replace) is allowed — it simply
                // never corrupts; nothing to assert on a refusal.
                continue;
            };
            // (1) the spliced buffer adds no new parse diagnostic.
            assert!(
                parse(&accepted.new_buffer).diagnostics().len() <= before_diags,
                "completion `{}` introduced a parse error in {src:?}",
                item.insert_text
            );
            // (2) every pre-existing comment / name / value survives in order.
            assert!(
                contains_contiguous(&comments(&accepted.new_buffer), &before_comments),
                "completion `{}` dropped/reordered a comment in {src:?}",
                item.insert_text
            );
            assert!(
                contains_contiguous(&names_and_values(&accepted.new_buffer), &before_names),
                "completion `{}` dropped/reordered a name/value in {src:?}",
                item.insert_text
            );
        }

        let _ = caret_char; // documented mapping; not needed for accept_item.
    }
}

// ---- snippet insertion leg (FR-019) -----------------------------------------

#[test]
fn snippet_insertion_round_trips_losslessly() {
    // Snippet bodies whose default expansion is valid RON in a value slot.
    let bodies = [
        "Some(${1:value})",
        "[${1:1}, ${2:2}]",
        "(${1:a}, ${2:b})",
        "${1:Name}(${2:field}: ${3:value})",
        "{${1:\"key\"}: ${2:value}}",
    ];

    for src in cases() {
        let slot_byte = first_value_slot(src);
        let caret_char = byte_to_char(src, slot_byte);
        let before_comments = comments(src);
        let before_names = names_and_values(src);
        let before_diags = parse(src).diagnostics().len();

        for body in bodies {
            let Some(insertion) = insert_snippet(src, caret_char, body) else {
                // A refused splice never corrupts; nothing to assert on refusal.
                continue;
            };
            // (1) no new parse diagnostic versus the original.
            assert!(
                parse(&insertion.new_buffer).diagnostics().len() <= before_diags,
                "snippet body {body:?} introduced a parse error in {src:?}"
            );
            // (2) every pre-existing comment / name / value survives in order.
            assert!(
                contains_contiguous(&comments(&insertion.new_buffer), &before_comments),
                "snippet body {body:?} dropped/reordered a comment in {src:?}"
            );
            assert!(
                contains_contiguous(&names_and_values(&insertion.new_buffer), &before_names),
                "snippet body {body:?} dropped/reordered a name/value in {src:?}"
            );
        }
    }
}

// ---- combined: a completion accept followed by a format stays lossless -------

#[test]
fn completion_then_format_chain_is_lossless() {
    use ronin_app::app::App;
    use ronin_app::settings::AppSettings;

    let src = "Scene(\n    // keep me\n    items: [\n        Foo(x: 1),\n    ],\n)";
    let slot_byte = first_value_slot(src);
    let ctx = completion_context(&parse(src), slot_byte);
    // Accept the first additive suggestion (an opening delimiter / Option ctor).
    let after_completion = if ctx.prefix.is_empty() {
        ctx.items
            .first()
            .and_then(|item| accept_item(src, slot_byte, "", item))
            .map_or_else(|| src.to_string(), |a| a.new_buffer)
    } else {
        src.to_string()
    };
    // The completion result is still parseable.
    assert!(parse(&after_completion).diagnostics().is_empty());

    // Now format the completion result via the real app path and assert the
    // original comment + names survive the whole chain.
    let mut app = App::new(AppSettings::default(), None);
    app.new_untitled();
    if let Some(doc) = app.active_document_mut() {
        doc.buffer = after_completion.clone();
    }
    app.format_document();
    let final_buffer = app
        .active_document()
        .map(|d| d.buffer.clone())
        .unwrap_or_default();

    assert!(
        comments(&final_buffer).contains(&"// keep me".to_string()),
        "the comment must survive completion → format: {final_buffer:?}"
    );
    // The original struct names + values are all still present and in order.
    let original_names = names_and_values(src);
    assert!(
        contains_contiguous(&names_and_values(&final_buffer), &original_names),
        "names/values must survive completion → format: {final_buffer:?}"
    );
}
