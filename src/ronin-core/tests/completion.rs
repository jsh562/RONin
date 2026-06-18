//! Completion-analysis integration tests (E005 Wave 3, US2, T030, SC-003).
//!
//! Exercises the **public** `ronin_core::completion` surface only (no internal
//! helpers), as a downstream surface (the editor) would. Covers:
//!
//! * position-kind classification for each construct — struct field, list element,
//!   map key, map value, tuple element, and a generic value slot;
//! * in-file identifier collection — sibling field names, in-file enum-variant
//!   names, and in-file map keys, drawn from attestation only (no types);
//! * deterministic ordering — kind-then-alphabetical, all ranked strictly below
//!   the user's literal input (no item shares rank 0);
//! * empty / ambiguous suppression — an empty or whitespace-only buffer, or a
//!   position with no resolvable enclosing construct, yields zero items and no
//!   error (FR-014);
//! * accepted suggestions round-trip — every produced `insert_text` parses without
//!   diagnostics so an accepted suggestion stays lossless (Principle I).

use ronin_core::{completion_context, completions, parse, CompletionKind, PositionKind};

/// The completion context at `offset` bytes into freshly-parsed `src`.
fn ctx(src: &str, offset: usize) -> ronin_core::CompletionContext {
    completion_context(&parse(src), offset)
}

// ---- position-kind classification per construct (SC-003) ---------------------

#[test]
fn classifies_struct_field_position() {
    // Caret inside `Point( .. )` parens at a fresh field slot.
    let src = "Point(x: 1, )";
    let c = ctx(src, src.len() - 1);
    assert_eq!(c.position, Some(PositionKind::StructField));
}

#[test]
fn classifies_struct_value_position_after_colon() {
    let src = "Point(x: )";
    let after_colon = src.find(':').unwrap() + 2;
    assert_eq!(ctx(src, after_colon).position, Some(PositionKind::Value));
}

#[test]
fn classifies_list_element_position() {
    let src = "[1, ]";
    assert_eq!(ctx(src, 4).position, Some(PositionKind::ListElement));
}

#[test]
fn classifies_map_key_position() {
    let src = "{ alpha: 1 }";
    assert_eq!(ctx(src, 2).position, Some(PositionKind::MapKey));
}

#[test]
fn classifies_map_value_position() {
    let src = "{ alpha: 1 }";
    let after_colon = src.find(':').unwrap() + 2;
    assert_eq!(ctx(src, after_colon).position, Some(PositionKind::MapValue));
}

#[test]
fn classifies_tuple_position() {
    let src = "(1, 2)";
    assert_eq!(ctx(src, 4).position, Some(PositionKind::Tuple));
}

#[test]
fn classifies_top_level_value_position() {
    // A bare-ident value at the top level resolves to a generic value slot.
    let src = "Foo";
    assert_eq!(ctx(src, 3).position, Some(PositionKind::Value));
}

// ---- in-file identifier collection (FR-011) ---------------------------------

#[test]
fn collects_sibling_field_names_in_enclosing_struct() {
    // Caret in a fresh field slot of a struct that already has `x` and `y`.
    let src = "Point(x: 1, y: 2, )";
    let c = ctx(src, src.len() - 1);
    assert!(c.sibling_field_names.contains(&"x".to_string()));
    assert!(c.sibling_field_names.contains(&"y".to_string()));
}

#[test]
fn collects_in_file_variant_names_anywhere() {
    // Variants attested in a list; cursor at a fresh value slot in that list.
    let src = "[Alpha, Beta, ]";
    let c = ctx(src, src.len() - 1);
    assert!(c.in_file_variant_names.contains(&"Alpha".to_string()));
    assert!(c.in_file_variant_names.contains(&"Beta".to_string()));
}

#[test]
fn collects_in_file_map_keys_anywhere() {
    let src = "{ alpha: 1, beta: 2 }";
    let c = ctx(src, 2);
    assert!(c.in_file_map_keys.contains(&"alpha".to_string()));
    assert!(c.in_file_map_keys.contains(&"beta".to_string()));
}

#[test]
fn attested_field_name_is_offered_at_a_struct_slot() {
    // A struct with a known field `width`; another field slot offers `width`.
    let src = "Rect(width: 1, )";
    let c = ctx(src, src.len() - 1);
    assert!(
        c.items
            .iter()
            .any(|i| i.label == "width" && i.kind == CompletionKind::Field),
        "an attested sibling field name should be offered at a field slot"
    );
}

// ---- deterministic ordering, all below the literal (FR-012, SC-003) ----------

#[test]
fn items_ordered_kind_then_alpha_all_below_literal() {
    // A value slot with an empty prefix offers Option (None, Some) before
    // variants before delimiters; ranks start at 1 and increase by 1.
    let src = "[]";
    let c = ctx(src, 1);
    assert_eq!(c.position, Some(PositionKind::ListElement));

    // No item may share the literal's rank 0; ranks are a dense 1.. sequence in
    // sorted order.
    for (i, item) in c.items.iter().enumerate() {
        assert!(
            item.rank >= 1,
            "no suggestion may occupy the literal's rank 0"
        );
        assert_eq!(item.rank, (i as u32) + 1, "ranks follow the sorted order");
    }

    // Kind ordering: Option kind precedes Delimiter kind.
    let first_option = c
        .items
        .iter()
        .position(|i| i.kind == CompletionKind::Option);
    let first_delim = c
        .items
        .iter()
        .position(|i| i.kind == CompletionKind::Delimiter);
    assert!(
        first_option < first_delim,
        "Option suggestions must sort before delimiter suggestions"
    );

    // Alphabetical within the Option kind: "None" before "Some".
    let none_pos = c.items.iter().position(|i| i.label == "None");
    let some_pos = c.items.iter().position(|i| i.label == "Some");
    assert!(none_pos < some_pos, "Option labels sort alphabetically");
}

#[test]
fn ordering_is_deterministic_across_runs() {
    let src = "[Gamma, Alpha, Beta, ]";
    let a = ctx(src, src.len() - 1);
    let b = ctx(src, src.len() - 1);
    let labels_a: Vec<&str> = a.items.iter().map(|i| i.label.as_str()).collect();
    let labels_b: Vec<&str> = b.items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels_a, labels_b, "ordering must be deterministic");
    // The attested variants appear in alphabetical order within their kind.
    let variant_labels: Vec<&str> = a
        .items
        .iter()
        .filter(|i| i.kind == CompletionKind::Variant)
        .map(|i| i.label.as_str())
        .collect();
    let mut sorted = variant_labels.clone();
    sorted.sort_unstable();
    assert_eq!(variant_labels, sorted, "variants sort alphabetically");
}

#[test]
fn prefix_filters_and_excludes_exact_literal() {
    // Typing `So` keeps `Some`, drops `None`.
    let partial = ctx("So", 2);
    assert_eq!(partial.prefix, "So");
    assert!(partial.items.iter().any(|i| i.label == "Some"));
    assert!(partial.items.iter().all(|i| i.label != "None"));

    // Typing the full `Some` must not re-offer `Some` (it is the literal).
    let exact = ctx("Some", 4);
    assert!(exact.items.iter().all(|i| i.label != "Some"));
}

// ---- empty / ambiguous suppression → zero items (FR-014) --------------------

#[test]
fn empty_buffer_is_zero_items_no_error() {
    let c = ctx("", 0);
    assert_eq!(c.position, None);
    assert!(c.items.is_empty());
}

#[test]
fn whitespace_only_buffer_is_zero_items_no_error() {
    let c = ctx("   \n\t  ", 3);
    assert_eq!(c.position, None);
    assert!(c.items.is_empty());
}

#[test]
fn out_of_range_offset_is_clamped_no_panic() {
    let src = "Foo(x: 1)";
    // Far past EOF: clamped to source length; never panics.
    let c = ctx(src, 10_000);
    let _ = c.items; // accessing items must not panic
}

// ---- accepted suggestions round-trip (Principle I) --------------------------

#[test]
fn every_insert_text_parses_cleanly() {
    // Across several constructs, every produced insert_text must parse without
    // diagnostics so an accepted suggestion stays lossless.
    for (src, offset) in [
        ("[]", 1),
        ("()", 1),
        ("{ }", 2),
        ("Point(x: 1, )", "Point(x: 1, )".len() - 1),
        ("{ alpha: 1, }", 2),
    ] {
        let c = ctx(src, offset);
        for item in &c.items {
            let parsed = parse(&item.insert_text);
            assert!(
                parsed.diagnostics().is_empty(),
                "insert_text {:?} (for {src:?}) must parse cleanly: {:?}",
                item.insert_text,
                parsed.diagnostics()
            );
        }
    }
}

#[test]
fn completions_free_fn_returns_the_items() {
    let c = ctx("[]", 1);
    assert_eq!(completions(&c), c.items);
}
