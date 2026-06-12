//! Wave 1 foundational integration tests (E003 "Desktop Editor Shell").
//!
//! Exercises the public library surface (`ronin_app::...`) only:
//! 1. [`ByteFidelityProfile::from_bytes`] line-ending/BOM/trailing-newline detection.
//! 2. [`EditorDocument::dirty`] derivation across load → edit → re-snapshot.
//! 3. [`EditorDocument::oversize`] strict-greater-than boundary.
//! 4. Byte→char diagnostic mapping across multibyte UTF-8.
//! 5. Stale-parse discard logic at the `ParseResult.generation` level.

use ronin_app::diagnostics_map::map_diagnostic;
use ronin_app::document::{ByteFidelityProfile, EditorDocument, LineEnding};
use ronin_app::reparse::ParseResult;

// ---------------------------------------------------------------------------
// 1. ByteFidelityProfile::from_bytes
// ---------------------------------------------------------------------------

#[test]
fn uniform_crlf_is_detected() {
    let p = ByteFidelityProfile::from_bytes(b"a\r\nb\r\nc\r\n");
    assert_eq!(p.line_ending, LineEnding::Crlf);
    assert_eq!(p.dominant, LineEnding::Crlf);
    assert!(p.had_trailing_newline);
    assert!(!p.had_bom);
}

#[test]
fn uniform_lf_is_detected() {
    let p = ByteFidelityProfile::from_bytes(b"a\nb\nc\n");
    assert_eq!(p.line_ending, LineEnding::Lf);
    assert_eq!(p.dominant, LineEnding::Lf);
    assert!(p.had_trailing_newline);
    assert!(!p.had_bom);
}

#[test]
fn mixed_endings_pick_crlf_dominant() {
    // 2x CRLF, 1x lone LF → Mixed, dominant CRLF.
    let p = ByteFidelityProfile::from_bytes(b"a\r\nb\r\nc\nd");
    assert_eq!(p.line_ending, LineEnding::Mixed);
    assert_eq!(p.dominant, LineEnding::Crlf);
    assert!(!p.had_trailing_newline); // ends in 'd'
}

#[test]
fn mixed_endings_pick_lf_dominant() {
    // 1x CRLF, 2x lone LF → Mixed, dominant LF.
    let p = ByteFidelityProfile::from_bytes(b"a\r\nb\nc\nd");
    assert_eq!(p.line_ending, LineEnding::Mixed);
    assert_eq!(p.dominant, LineEnding::Lf);
}

#[test]
fn mixed_tie_resolves_to_lf() {
    // 1x CRLF, 1x lone LF → tie → dominant LF.
    let p = ByteFidelityProfile::from_bytes(b"a\r\nb\nc");
    assert_eq!(p.line_ending, LineEnding::Mixed);
    assert_eq!(p.dominant, LineEnding::Lf);
}

#[test]
fn no_newline_defaults_to_lf() {
    let p = ByteFidelityProfile::from_bytes(b"single line, no newline");
    assert_eq!(p.line_ending, LineEnding::Lf);
    assert_eq!(p.dominant, LineEnding::Lf);
    assert!(!p.had_trailing_newline);
}

#[test]
fn trailing_newline_presence_and_absence() {
    assert!(ByteFidelityProfile::from_bytes(b"x\n").had_trailing_newline);
    assert!(!ByteFidelityProfile::from_bytes(b"x").had_trailing_newline);
}

#[test]
fn bom_presence_and_absence() {
    let with_bom = [0xEF, 0xBB, 0xBF, b'(', b')'];
    assert!(ByteFidelityProfile::from_bytes(&with_bom).had_bom);
    assert!(!ByteFidelityProfile::from_bytes(b"()").had_bom);
}

#[test]
fn hash_distinguishes_different_content() {
    let a = ByteFidelityProfile::from_bytes(b"hello");
    let b = ByteFidelityProfile::from_bytes(b"world");
    assert_ne!(a.original_hash, b.original_hash);
}

// ---------------------------------------------------------------------------
// 2. dirty derivation
// ---------------------------------------------------------------------------

#[test]
fn document_is_clean_after_load() {
    let doc = EditorDocument::from_loaded("a.ron", b"Foo(x: 1)\n").unwrap();
    assert!(!doc.dirty(), "freshly loaded document must be clean");
}

#[test]
fn document_is_dirty_after_buffer_change() {
    let mut doc = EditorDocument::from_loaded("a.ron", b"Foo(x: 1)\n").unwrap();
    doc.buffer.push_str("// edit");
    assert!(doc.dirty(), "edited document must be dirty");
}

#[test]
fn document_is_clean_again_after_marking_saved() {
    let mut doc = EditorDocument::from_loaded("a.ron", b"Foo(x: 1)\n").unwrap();
    doc.buffer.push_str("// edit");
    assert!(doc.dirty());
    doc.mark_saved();
    assert!(!doc.dirty(), "document must be clean after re-snapshot");
}

#[test]
fn reverting_buffer_to_saved_content_is_clean() {
    let mut doc = EditorDocument::from_loaded("a.ron", b"Foo(x: 1)\n").unwrap();
    let original = doc.buffer.clone();
    doc.buffer.push_str("zzz");
    assert!(doc.dirty());
    doc.buffer = original;
    assert!(!doc.dirty(), "restoring exact saved content clears dirty");
}

#[test]
fn bom_is_stripped_from_buffer_but_remembered() {
    let raw = [0xEF, 0xBB, 0xBF, b'(', b')'];
    let doc = EditorDocument::from_loaded("b.ron", &raw).unwrap();
    assert_eq!(doc.buffer, "()", "BOM must not enter the editable buffer");
    assert!(doc.byte_profile.had_bom, "BOM presence must be recorded");
    assert!(!doc.dirty(), "BOM-stripped load is still clean");
}

#[test]
fn untitled_document_title_is_stable() {
    let doc = EditorDocument::new_untitled(3);
    assert_eq!(doc.title(), "Untitled-3");
    assert!(!doc.dirty());
}

#[test]
fn loaded_document_title_is_file_name() {
    let doc = EditorDocument::from_loaded("dir/sub/scene.ron", b"()").unwrap();
    assert_eq!(doc.title(), "scene.ron");
}

// ---------------------------------------------------------------------------
// 3. oversize boundary (strict greater-than)
// ---------------------------------------------------------------------------

#[test]
fn oversize_boundary_is_strict_greater_than() {
    let threshold: u64 = 10;

    // len == threshold → NOT oversize.
    let mut doc = EditorDocument::new_untitled(1);
    doc.buffer = "x".repeat(10);
    assert_eq!(doc.buffer.len(), 10);
    assert!(!doc.oversize(threshold), "len == threshold is not oversize");

    // len == threshold + 1 → oversize.
    doc.buffer = "x".repeat(11);
    assert!(doc.oversize(threshold), "len == threshold+1 is oversize");

    // len < threshold → not oversize.
    doc.buffer = "x".repeat(9);
    assert!(!doc.oversize(threshold));
}

// ---------------------------------------------------------------------------
// 4. byte→char multibyte diagnostic mapping
// ---------------------------------------------------------------------------

#[test]
fn multibyte_prefix_makes_char_range_differ_from_byte_range() {
    // "é" is 2 bytes / 1 char; "🦀" is 4 bytes / 1 char; "界" is 3 bytes / 1 char.
    // Build a source where a malformed token appears after multibyte text so the
    // diagnostic's byte range is strictly greater than its char range.
    //
    // Prefix: `(a: "é🦀界", b: ` then an unexpected token `@` triggers a diagnostic.
    let source = "(a: \"é🦀界\", b: @)";

    let cst = ron_core::parse(source);
    let diags = cst.diagnostics();
    assert!(
        !diags.is_empty(),
        "expected at least one diagnostic for the malformed `@`"
    );

    // Take the diagnostic whose byte start is after the multibyte run.
    let diag = diags
        .iter()
        .find(|d| d.range.start() > 4)
        .expect("a diagnostic after the multibyte prefix");

    let view = map_diagnostic(diag, source);

    // The multibyte chars (é=2, 🦀=4, 界=3 bytes but 1 char each) mean the byte
    // offset exceeds the char offset by exactly the extra-byte count before it.
    assert!(
        view.char_range.0 < diag.range.start(),
        "char start ({}) must be less than byte start ({}) past multibyte text",
        view.char_range.0,
        diag.range.start()
    );

    // Verify the char start is exactly correct by independent counting.
    let expected_char_start = source[..diag.range.start()].chars().count();
    assert_eq!(view.char_range.0, expected_char_start);
    let expected_char_end = source[..diag.range.end()].chars().count();
    assert_eq!(view.char_range.1, expected_char_end);
}

#[test]
fn line_column_is_correct_across_newlines_and_multibyte() {
    // Two lines; the second begins with a multibyte char before an error token.
    // Line 0: `(`               -> newline
    // Line 1: `é@`              -> `@` is the error at line 1, column 1 (é is col 0)
    let source = "(\né@";
    let cst = ron_core::parse(source);
    let diags = cst.diagnostics();
    assert!(!diags.is_empty());

    // Find a diagnostic on the second line (byte offset past the '\n').
    let newline_byte = source.find('\n').unwrap();
    let diag = diags
        .iter()
        .find(|d| d.range.start() > newline_byte)
        .expect("a diagnostic on line 1");

    let view = map_diagnostic(diag, source);
    let (line, col) = view.line_col.0;
    assert_eq!(
        line, 1,
        "diagnostic must be on the second (zero-based 1) line"
    );
    // `é` occupies column 0; the error `@` is at column 1 (char column, not byte).
    assert_eq!(col, 1, "column counts characters, not bytes");
}

#[test]
fn ascii_only_mapping_matches_byte_offsets() {
    let source = "(a: @)";
    let cst = ron_core::parse(source);
    let diag = cst
        .diagnostics()
        .iter()
        .find(|d| !d.range.is_empty())
        .or_else(|| cst.diagnostics().first())
        .expect("a diagnostic")
        .clone();
    let view = map_diagnostic(&diag, source);
    // Pure ASCII: char offsets equal byte offsets.
    assert_eq!(view.char_range.0, diag.range.start());
    assert_eq!(view.char_range.1, diag.range.end());
}

// ---------------------------------------------------------------------------
// 5. stale-parse discard logic (generation comparison)
// ---------------------------------------------------------------------------

#[test]
fn newer_generation_supersedes_older() {
    let older = ParseResult::parse("(x: 1)", 1);
    let newer = ParseResult::parse("(x: 2)", 2);

    // The consumer rule: a result supersedes the currently-installed generation
    // iff its generation is >= the installed one.
    assert!(
        newer.supersedes(older.generation),
        "newer generation must supersede older"
    );
    assert!(
        !older.supersedes(newer.generation),
        "older generation must be discarded against a newer install"
    );
}

#[test]
fn same_generation_is_installable() {
    let r = ParseResult::parse("(x: 1)", 5);
    assert!(
        r.supersedes(5),
        "a result of the currently-installed generation may install"
    );
}

#[test]
fn parse_result_records_source_len_and_clamped_diagnostics() {
    let source = "(a: @)";
    let r = ParseResult::parse(source, 7);
    assert_eq!(r.source_len, source.len());
    assert_eq!(r.generation, 7);
    // Every diagnostic range must lie within [0, source_len] after clamping.
    for d in &r.diagnostics {
        assert!(d.range.start() <= r.source_len);
        assert!(d.range.end() <= r.source_len);
        assert!(d.range.start() <= d.range.end());
    }
}

#[test]
fn well_formed_input_has_no_diagnostics() {
    let r = ParseResult::parse("Foo(x: 1, y: 2.0)", 0);
    assert!(
        r.diagnostics.is_empty(),
        "well-formed RON should yield no parse diagnostics"
    );
}
