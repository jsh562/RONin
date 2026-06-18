//! Consumer-API integration test (TR-009 / SC-006, COMPLETES TR-009).
//!
//! This file is a stand-in for a downstream consumer crate: it uses **only** the
//! public `ronin_core` API to parse, navigate, read diagnostics, print, and edit.
//! It statically demonstrates the library-opaque-surface invariant (INV-7):
//!
//! * No `rowan` item is imported or named anywhere below — the only crate in
//!   scope besides `std` is `ronin_core`.
//! * No I/O type appears in the API used here (no `std::fs`, no `std::io`,
//!   no paths, no async); everything operates on in-memory `&str` / `String`.
//!
//! If a future change leaked a `rowan` type (or required `use rowan::…` to call
//! a public method), this file would fail to compile against the public surface
//! alone — which is exactly the regression guard SC-006 asks for.

use ronin_core::ast::{Document, Value};
use ronin_core::{
    apply_edit, parse, parse_bytes, print, CstDocument, Diagnostic, DiagnosticCode, EditOperation,
    EditTarget, Severity, SyntaxKind, SyntaxNode, SyntaxToken, TextRange, TriviaPolicy,
};

/// A typed, public-API-only alias bundling every navigation/diagnostic type the
/// consumer touches. Naming them all in one `ronin_core`-only type is a
/// compile-time witness for INV-7: if any were a leaked `rowan` type, this alias
/// (and the `witness` binding that constructs it) would fail to compile against
/// the public surface alone.
type PublicApiWitness<'a> = (
    &'a CstDocument,
    &'a SyntaxNode,
    &'a SyntaxToken,
    SyntaxKind,
    TextRange,
    &'a Diagnostic,
    Severity,
    DiagnosticCode,
);

#[test]
fn parse_navigate_diagnostics_print_round_trip() {
    let src = "Config(\n    name: \"demo\",\n    count: 3, // trailing comment\n    tags: [\"a\", \"b\"],\n)\n";

    // ---- Parse (public API) -------------------------------------------------
    let doc: CstDocument = parse(src);

    // ---- Print: byte-for-byte round-trip (TR-003 via the public surface) ----
    assert_eq!(
        print(&doc),
        src,
        "consumer round-trip must be byte-identical"
    );
    assert_eq!(doc.source_len(), src.len());

    // ---- Diagnostics are reachable (well-formed input ⇒ empty) --------------
    let diags: &[Diagnostic] = doc.diagnostics();
    assert!(diags.is_empty(), "well-formed input has no diagnostics");

    // ---- Navigate via the untyped public newtypes ---------------------------
    let root: SyntaxNode = doc.root();
    assert_eq!(root.kind(), SyntaxKind::Root);
    let struct_node = root
        .children()
        .find(|n| n.kind() == SyntaxKind::Struct)
        .expect("top-level struct");
    let name_tok: SyntaxToken = struct_node
        .first_token_of(SyntaxKind::Ident)
        .expect("struct name token");
    assert_eq!(name_tok.text(), "Config");
    let _rng: TextRange = name_tok.text_range();

    // ---- Navigate via the typed accessors (TR-010) --------------------------
    let document = Document::cast(doc.root()).expect("root casts to Document");
    let Some(Value::Struct(s)) = document.value() else {
        panic!("expected a struct value");
    };
    assert_eq!(s.name_text().as_deref(), Some("Config"));
    let field_names: Vec<String> = s.fields().filter_map(|f| f.name_text()).collect();
    assert_eq!(field_names, vec!["name", "count", "tags"]);

    // The `tags` field is a list of two string literals.
    let tags = s
        .fields()
        .find(|f| f.name_text().as_deref() == Some("tags"))
        .expect("tags field");
    let Some(Value::List(list)) = tags.value() else {
        panic!("tags should be a list");
    };
    let items: Vec<Value> = list.items().collect();
    assert_eq!(items.len(), 2);

    // Construct the witness so every public type above is statically referenced
    // through the `ronin_core`-only `PublicApiWitness` alias (INV-7).
    let code = DiagnosticCode::UnexpectedToken;
    let dummy = Diagnostic::new(code, TextRange::new(0, 0), "x");
    let witness: PublicApiWitness<'_> = (
        &doc,
        &struct_node,
        &name_tok,
        SyntaxKind::Struct,
        TextRange::new(0, 1),
        &dummy,
        Severity::Error,
        code,
    );
    assert_eq!(witness.0.source_len(), src.len());
    assert_eq!(witness.3, SyntaxKind::Struct);
}

#[test]
fn diagnostics_are_reachable_for_malformed_input() {
    // Malformed input (unclosed list) ⇒ a diagnostic is reachable and the tree
    // still re-prints byte-for-byte (INV-3) — all via the public API.
    let src = "[1, 2, 3";
    let doc = parse(src);
    assert_eq!(print(&doc), src, "error-recovered tree round-trips");

    let diags = doc.diagnostics();
    assert!(!diags.is_empty(), "malformed input yields diagnostics");
    let d = &diags[0];
    assert_eq!(d.code(), DiagnosticCode::UnclosedDelimiter);
    assert_eq!(d.severity(), Severity::Error);
    assert!(d.range().end() <= doc.source_len());
    // The code string is part of the stable public contract.
    assert_eq!(d.code().code(), "RON-P0002");
}

#[test]
fn parse_bytes_and_utf8_rejection_via_public_api() {
    // Valid UTF-8 bytes parse and round-trip.
    let doc = parse_bytes("Some(1)".as_bytes()).expect("valid UTF-8");
    assert_eq!(print(&doc), "Some(1)");

    // Non-UTF-8 is rejected cleanly through the public boundary (no panic).
    let err = parse_bytes(&[0xFF, 0x00]).unwrap_err();
    // `LexError` is a public type with a Display impl.
    assert!(!err.to_string().is_empty());
}

#[test]
fn edit_via_public_api_preserves_unaffected_regions() {
    // The edit capability area is reachable from the public surface and keeps
    // unaffected regions byte-identical (INV-8).
    let src = "Foo(x: 1, y: 2)";
    let doc = parse(src);
    let strukt = doc
        .root()
        .children()
        .find(|n| n.kind() == SyntaxKind::Struct)
        .unwrap();
    let name = strukt.first_token_of(SyntaxKind::Ident).unwrap();

    let edit = EditOperation::replace(
        EditTarget::TokenSpan {
            first: name.clone(),
            last: name,
        },
        "Bar",
        TriviaPolicy::KEEP_ALL,
    );
    let edited = apply_edit(&doc, edit).expect("edit applies");
    assert_eq!(print(&edited), "Bar(x: 1, y: 2)");
    // Original document is untouched (non-destructive).
    assert_eq!(print(&doc), src);
}
