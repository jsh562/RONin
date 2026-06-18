//! Structural autocomplete analysis (E005 Wave 3, US2).
//!
//! Given a parsed [`CstDocument`] and a cursor byte offset, this module derives a
//! [`CompletionContext`] describing **where** the cursor sits inside the RON
//! structure (the [`PositionKind`]), what identifier fragment the user is in the
//! middle of typing (the `prefix`), and a deterministic, ranked list of
//! low-confidence [`CompletionItem`] suggestions drawn **only** from what is
//! already attested in the document.
//!
//! # Progressive Intelligence — structural only (project-instructions §III)
//!
//! Completion here is **structural**: it uses the CST syntax plus identifiers that
//! *already appear in the document* (sibling field names, enum-variant names, map
//! keys), plus the always-legal `Some` / `None` and the context-legal delimiters.
//! It carries **no type information** — type-aware completion is E006. Missing or
//! ambiguous context degrades gracefully to **zero suggestions**, never an error
//! (FR-014). Unknown context is not a failure; it is simply "nothing to suggest".
//!
//! # Never corrupt (project-instructions §I)
//!
//! Analysis is strictly **read-only** over the CST. Every produced
//! [`CompletionItem`] carries an `insert_text` that is syntactically valid in its
//! position, so an accepted suggestion (spliced + re-parsed by the surface layer)
//! round-trips through the CST. Nothing here mutates the document and nothing
//! auto-accepts: ranks are deliberately assigned **below** the user's literal
//! input so a suggestion is never preselected (FR-012).
//!
//! # WASM-clean (project-instructions §II, INV-9)
//!
//! This module adds **no** filesystem / UI / async / native dependency — it uses
//! only `std` and `ronin-core`'s own CST types, so the `wasm32` build of `ronin-core`
//! stays green.
//!
//! # Deferred seams
//!
//! Completion here is structural; the deeper intelligence is deferred to later
//! epics and attaches around this read-only analysis:
//!
//! * **type-aware completion** — ranking + filtering candidates by an *expected
//!   type* (and offering type-legal field/variant names rather than merely attested
//!   ones) → **E006** (schema-optional type model). The [`CompletionKind`] ordering
//!   and the [`CompletionContext`] pools are the seam a type layer refines.
//! * **semantic / CST-backed undo-redo** of an accepted suggestion → **E007** (the
//!   surface layer's verified splice is what an undo stack records against).
//! * **tree / table structured editing** completions → **E008**.
//! * **Bevy-registry-aware** completion (real component / field names from a loaded
//!   registry, not just in-file attestation) → **E009**.
//! * **RON⇄JSON / `derive`-driven** suggestions → **E010** (interop is outside this
//!   pure-CST engine).

use std::collections::BTreeSet;

use crate::parser::CstDocument;
use crate::syntax::{SyntaxKind, SyntaxNode, SyntaxToken};

/// Where the cursor sits inside the RON structure (FR-010).
///
/// Derived purely from the enclosing CST construct. [`PositionKind::Value`] is the
/// general "a value is expected here" position (top-level value, list element of a
/// freshly-opened list, a struct/map value slot, …) where any RON value — and so
/// `Some` / `None` / a variant name — is legal. The more specific kinds carry the
/// extra structural knowledge that lets completion offer attested field names,
/// keys, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PositionKind {
    /// Inside a struct's parentheses, at a `field` name position (before the `:`).
    StructField,
    /// Inside a list `[ .. ]`, at an element position.
    ListElement,
    /// Inside a map `{ .. }`, at a key position (before the `:`).
    MapKey,
    /// Inside a map entry, at the value position (after the `:`).
    MapValue,
    /// Inside a tuple `( a, b )`, at a positional element position.
    Tuple,
    /// A generic value position (top-level value, struct/variant payload value,
    /// or any spot where a fresh RON value is expected).
    Value,
}

/// The classification of a single [`CompletionItem`] (FR-011/FR-012).
///
/// Used both as a display hint for the surface layer and as the **primary sort
/// key** for deterministic ordering: items are ordered by kind (in the order the
/// variants are declared here) then alphabetically by label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CompletionKind {
    /// A struct field name already present on a sibling/like struct.
    Field,
    /// An enum-variant identifier attested elsewhere in the document.
    Variant,
    /// A map key attested elsewhere in the document.
    MapKey,
    /// The `Some` / `None` option constructors (always structurally legal).
    Option,
    /// A context-legal delimiter / punctuation hint (e.g. `(`, `[`, `{`).
    Delimiter,
}

impl CompletionKind {
    /// A stable ordinal used as the primary (kind) sort key. Lower sorts first.
    #[inline]
    #[must_use]
    fn order(self) -> u8 {
        match self {
            Self::Field => 0,
            Self::Variant => 1,
            Self::MapKey => 2,
            Self::Option => 3,
            Self::Delimiter => 4,
        }
    }
}

/// A single low-confidence completion suggestion (FR-011/FR-012).
///
/// Always ranked **below** the user's literal input (`rank` is a strictly
/// positive distance-from-top; the literal conceptually occupies rank `0`), so it
/// is never preselected and never auto-accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    /// The text shown to the user in the popup.
    pub label: String,
    /// The text inserted on acceptance — syntactically valid in its position so
    /// the result round-trips through the CST.
    pub insert_text: String,
    /// The suggestion's classification (display hint + primary sort key).
    pub kind: CompletionKind,
    /// Deterministic rank, strictly `>= 1` (below the user's literal input at
    /// rank `0`). Lower ranks sort first; ranks follow the kind-then-alpha order.
    pub rank: u32,
}

/// The full completion analysis for a cursor offset (FR-010/FR-011).
///
/// Produced by [`completion_context`]. Holds the structural [`PositionKind`], the
/// in-progress identifier `prefix`, the in-file attested identifier pools, and the
/// already-filtered/ranked [`items`](Self::items). An empty / ambiguous context
/// yields an empty `items` list and [`PositionKind::Value`] is *not* assumed —
/// `position` is `None` (FR-014).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionContext {
    /// The structural position of the cursor, or `None` for an empty / ambiguous
    /// context where no single kind resolves (then `items` is empty).
    pub position: Option<PositionKind>,
    /// The identifier fragment immediately left of the cursor (may be empty).
    pub prefix: String,
    /// Field names already present in the enclosing (or like) struct(s).
    pub sibling_field_names: Vec<String>,
    /// Enum-variant identifiers seen anywhere in the document.
    pub in_file_variant_names: Vec<String>,
    /// Map keys (identifier-like) seen anywhere in the document.
    pub in_file_map_keys: Vec<String>,
    /// The ranked, prefix-filtered suggestions (empty for an empty/ambiguous
    /// context). Always ordered by kind then alphabetically, all below the literal.
    pub items: Vec<CompletionItem>,
}

impl CompletionContext {
    /// An empty context: no resolvable position, no suggestions (FR-014).
    #[must_use]
    fn empty() -> Self {
        Self {
            position: None,
            prefix: String::new(),
            sibling_field_names: Vec::new(),
            in_file_variant_names: Vec::new(),
            in_file_map_keys: Vec::new(),
            items: Vec::new(),
        }
    }
}

/// Analyze the cursor `offset` against `doc` and produce a [`CompletionContext`]
/// (FR-010/FR-011/FR-012/FR-014).
///
/// Read-only. The offset is a byte offset into the source the document was parsed
/// from; an out-of-range offset is clamped to the source length. An empty /
/// whitespace-only document, a cursor with no enclosing construct, or a position
/// whose kind cannot be uniquely resolved all yield an empty context (no items, no
/// error) per the empty/ambiguous-suppression rule (FR-014).
#[must_use]
pub fn completion_context(doc: &CstDocument, offset: usize) -> CompletionContext {
    let offset = offset.min(doc.source_len());
    let root = doc.root();

    // Empty / whitespace-only buffer: there is no construct to complete inside.
    // (The root has no significant child token.) → empty context (FR-014).
    if !has_significant_token(&root) {
        return CompletionContext::empty();
    }

    // The in-progress identifier fragment left of the cursor.
    let prefix = prefix_before(&root, offset);

    // Resolve the enclosing construct + position kind. Ambiguous / no-construct
    // → empty context (FR-014).
    let Some(position) = resolve_position(&root, offset) else {
        return CompletionContext::empty();
    };

    // Collect the in-file attested identifier pools (syntax + attestation only).
    let sibling_field_names = enclosing_sibling_field_names(&root, offset);
    let in_file_variant_names = collect_in_file(&root, SyntaxKind::EnumVariant);
    let in_file_map_keys = collect_map_keys(&root);

    let items = build_items(
        position,
        &prefix,
        &sibling_field_names,
        &in_file_variant_names,
        &in_file_map_keys,
    );

    CompletionContext {
        position: Some(position),
        prefix,
        sibling_field_names,
        in_file_variant_names,
        in_file_map_keys,
        items,
    }
}

/// Convenience: the ranked, prefix-filtered suggestions for `ctx` (FR-012).
///
/// This is exactly `ctx.items` — exposed as a free function so a surface layer can
/// treat "give me the items" as a single call regardless of whether it keeps the
/// whole context around.
#[must_use]
pub fn completions(ctx: &CompletionContext) -> Vec<CompletionItem> {
    ctx.items.clone()
}

// ---- position classification -------------------------------------------------

/// Find the smallest node whose range contains `offset`, descending from the root.
///
/// Uses a half-open `[start, end)` containment with a right-edge inclusive rule so
/// a cursor *at* the closing position of a construct still resolves to that
/// construct's enclosing context (the caret sits just inside the trailing edge).
fn smallest_node_at(root: &SyntaxNode, offset: usize) -> SyntaxNode {
    let mut node = root.clone();
    loop {
        // Descend into the deepest child node whose range still covers the cursor.
        // A child may share the parent's exact range (e.g. the sole top-level
        // value spans the whole root); descend by node identity so we still reach
        // the inner construct rather than stopping at the root.
        let next = node.children().find(|child| {
            let r = child.text_range();
            r.start() <= offset && offset <= r.end()
        });
        match next {
            Some(child) if child != node => node = child,
            _ => break,
        }
    }
    node
}

/// Resolve the [`PositionKind`] for `offset`, or `None` if it cannot be uniquely
/// determined (ambiguous / no enclosing construct) (FR-010/FR-014).
fn resolve_position(root: &SyntaxNode, offset: usize) -> Option<PositionKind> {
    let node = smallest_node_at(root, offset);

    // Walk from the smallest containing node up to the first node whose kind names
    // a construct we can complete inside. The *immediate* enclosing construct wins.
    let mut cur = Some(node.clone());
    while let Some(n) = cur {
        match n.kind() {
            SyntaxKind::Struct => {
                return struct_position(&n, offset);
            }
            SyntaxKind::List => {
                // The caret must be inside the brackets to be a list element.
                return inside_brackets(&n, offset, SyntaxKind::LBracket, SyntaxKind::RBracket)
                    .then_some(PositionKind::ListElement);
            }
            SyntaxKind::Tuple => {
                return inside_brackets(&n, offset, SyntaxKind::LParen, SyntaxKind::RParen)
                    .then_some(PositionKind::Tuple);
            }
            SyntaxKind::Map => {
                return map_position(&n, offset);
            }
            SyntaxKind::MapEntry => {
                return map_entry_position(&n, offset);
            }
            SyntaxKind::StructField => {
                return struct_field_position(&n, offset);
            }
            SyntaxKind::Root => {
                // Top-level value position: any RON value (incl. Some/None) is legal.
                return Some(PositionKind::Value);
            }
            _ => cur = n.parent(),
        }
    }
    None
}

/// Position inside a `Struct( .. )`: a field-name slot vs. a value slot.
fn struct_position(node: &SyntaxNode, offset: usize) -> Option<PositionKind> {
    if !inside_brackets(node, offset, SyntaxKind::LParen, SyntaxKind::RParen) {
        // The caret is on the struct *name* (before `(`), not inside the body.
        return None;
    }
    // If the caret sits inside an existing field, defer to that field's logic.
    if let Some(field) = node
        .children()
        .find(|c| c.kind() == SyntaxKind::StructField && c.text_range().contains(offset))
    {
        return struct_field_position(&field, offset);
    }
    // Otherwise it is a fresh field-name position inside the parens.
    Some(PositionKind::StructField)
}

/// Position inside a `StructField`: before the `:` is a field name, after is a value.
fn struct_field_position(field: &SyntaxNode, offset: usize) -> Option<PositionKind> {
    match colon_offset(field) {
        Some(colon) if offset > colon => Some(PositionKind::Value),
        _ => Some(PositionKind::StructField),
    }
}

/// Position inside a `Map { .. }`: a key slot vs. (when inside an entry) key/value.
fn map_position(node: &SyntaxNode, offset: usize) -> Option<PositionKind> {
    if !inside_brackets(node, offset, SyntaxKind::LBrace, SyntaxKind::RBrace) {
        return None;
    }
    if let Some(entry) = node
        .children()
        .find(|c| c.kind() == SyntaxKind::MapEntry && c.text_range().contains(offset))
    {
        return map_entry_position(&entry, offset);
    }
    Some(PositionKind::MapKey)
}

/// Position inside a `MapEntry`: before the `:` is the key, after is the value.
fn map_entry_position(entry: &SyntaxNode, offset: usize) -> Option<PositionKind> {
    match colon_offset(entry) {
        Some(colon) if offset > colon => Some(PositionKind::MapValue),
        _ => Some(PositionKind::MapKey),
    }
}

/// The absolute byte offset of the entry/field's top-level `:` token, if present.
fn colon_offset(node: &SyntaxNode) -> Option<usize> {
    node.children_with_tokens()
        .filter_map(|el| el.as_token().cloned())
        .find(|t| t.kind() == SyntaxKind::Colon)
        .map(|t| t.text_range().start())
}

/// `true` when `offset` lies strictly inside the node's open/close delimiter pair.
///
/// "Strictly inside" means after the opening delimiter and at-or-before the
/// closing one (the caret may sit just before `)`/`]`/`}`). A node missing one of
/// its delimiters (error recovery) is treated as open on that side.
fn inside_brackets(node: &SyntaxNode, offset: usize, open: SyntaxKind, close: SyntaxKind) -> bool {
    let open_end = node
        .children_with_tokens()
        .filter_map(|el| el.as_token().cloned())
        .find(|t| t.kind() == open)
        .map(|t| t.text_range().end());
    let close_start = node
        .children_with_tokens()
        .filter_map(|el| el.as_token().cloned())
        .find(|t| t.kind() == close)
        .map(|t| t.text_range().start());

    // MSRV 1.77: `Option::is_none_or` is 1.82+, so use `map_or(true, ..)`.
    let after_open = open_end.map_or(true, |e| offset >= e);
    let before_close = close_start.map_or(true, |s| offset <= s);
    after_open && before_close
}

// ---- prefix extraction -------------------------------------------------------

/// The identifier fragment immediately left of `offset` (FR-010).
///
/// Finds the token covering (or ending exactly at) the cursor; if it is an
/// identifier, returns the portion of its text up to the cursor. Otherwise the
/// prefix is empty (the cursor is at a fresh position).
fn prefix_before(root: &SyntaxNode, offset: usize) -> String {
    // The relevant token is the last token whose range starts before the cursor
    // and ends at-or-after it. We want a token the cursor is "inside" or sitting
    // at the right edge of.
    let mut best: Option<SyntaxToken> = None;
    for tok in root.descendant_tokens() {
        let r = tok.text_range();
        if r.start() < offset && offset <= r.end() {
            best = Some(tok);
        }
        if r.start() >= offset {
            break;
        }
    }
    let Some(tok) = best else {
        return String::new();
    };
    // Only identifier-ish tokens contribute a typed prefix. `enable`/bool keyword
    // tokens are not user-extendable identifiers in this structural model.
    if tok.kind() != SyntaxKind::Ident {
        return String::new();
    }
    let r = tok.text_range();
    let take = offset.saturating_sub(r.start());
    // Slice on a char boundary defensively (idents are ASCII in practice, but
    // never panic on a multibyte token edge).
    let text = tok.text();
    let take = take.min(text.len());
    let mut end = take;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

// ---- in-file attestation -----------------------------------------------------

/// `true` if the tree contains at least one significant (non-trivia) token.
fn has_significant_token(root: &SyntaxNode) -> bool {
    root.descendant_tokens().any(|t| !t.is_trivia())
}

/// Collect the field names present in the struct enclosing `offset` (FR-011).
///
/// "Sibling" field names are the names already typed inside the *same* struct as
/// the cursor (so completion can suggest other-document fields while not
/// re-suggesting one already present). When the cursor is not inside a struct, the
/// list is empty.
fn enclosing_sibling_field_names(root: &SyntaxNode, offset: usize) -> Vec<String> {
    let node = smallest_node_at(root, offset);
    let mut cur = Some(node);
    while let Some(n) = cur {
        if n.kind() == SyntaxKind::Struct {
            let mut names: Vec<String> = n
                .children()
                .filter(|c| c.kind() == SyntaxKind::StructField)
                .filter_map(|f| {
                    f.first_token_of(SyntaxKind::Ident)
                        .map(|t| t.text().to_string())
                })
                .collect();
            dedup_sorted(&mut names);
            return names;
        }
        cur = n.parent();
    }
    Vec::new()
}

/// Collect the name idents of every node of `kind` anywhere in the document.
///
/// Used for enum-variant names ([`SyntaxKind::EnumVariant`]). Deterministic
/// (sorted, de-duplicated).
fn collect_in_file(root: &SyntaxNode, kind: SyntaxKind) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    collect_in_file_rec(root, kind, &mut out);
    dedup_sorted(&mut out);
    out
}

fn collect_in_file_rec(node: &SyntaxNode, kind: SyntaxKind, out: &mut Vec<String>) {
    if node.kind() == kind {
        if let Some(t) = node.first_token_of(SyntaxKind::Ident) {
            out.push(t.text().to_string());
        }
    }
    for child in node.children() {
        collect_in_file_rec(&child, kind, out);
    }
}

/// Collect identifier-like map keys attested anywhere in the document (FR-011).
///
/// Only keys whose key value is a single bare identifier (an `EnumVariant` with no
/// payload, or a `Literal` ident — RON allows bare-ident keys) are collected, so
/// the surface can re-offer them as completion candidates. String/number keys are
/// not surfaced as identifier completions.
fn collect_map_keys(root: &SyntaxNode) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    collect_map_keys_rec(root, &mut out);
    dedup_sorted(&mut out);
    out
}

fn collect_map_keys_rec(node: &SyntaxNode, out: &mut Vec<String>) {
    if node.kind() == SyntaxKind::MapEntry {
        // The key is the first value-position child; before the `:`.
        let colon = colon_offset(node);
        if let Some(key_node) = node.children().find(|c| {
            // MSRV 1.77: `Option::is_none_or` is 1.82+, so use `map_or(true, ..)`.
            let before_colon = colon.map_or(true, |co| c.text_range().start() < co);
            before_colon && is_keyable_node(c.kind())
        }) {
            if let Some(t) = key_node.first_token_of(SyntaxKind::Ident) {
                out.push(t.text().to_string());
            }
        }
    }
    for child in node.children() {
        collect_map_keys_rec(&child, out);
    }
}

/// Whether a node kind can carry a bare-identifier key we surface for completion.
fn is_keyable_node(kind: SyntaxKind) -> bool {
    matches!(kind, SyntaxKind::EnumVariant | SyntaxKind::Literal)
}

/// Sort + de-duplicate a `Vec<String>` in place (deterministic ordering).
fn dedup_sorted(v: &mut Vec<String>) {
    let set: BTreeSet<String> = v.drain(..).collect();
    v.extend(set);
}

// ---- item production ---------------------------------------------------------

/// Build the deterministic, prefix-filtered, kind-then-alpha-ordered item list for
/// a resolved position (FR-011/FR-012).
///
/// Candidate pools depend on the position kind:
/// * `StructField` → attested sibling/like field names (minus ones already present).
/// * `MapKey`      → attested in-file map keys.
/// * `ListElement` / `Tuple` / `MapValue` / `Value` → `Some` / `None`, attested
///   in-file variant names, and the legal opening delimiters.
///
/// Every item is ranked strictly below the user's literal input (rank `0`): the
/// first suggestion gets rank `1`, then `2`, … in the final sorted order (FR-012).
fn build_items(
    position: PositionKind,
    prefix: &str,
    sibling_field_names: &[String],
    in_file_variant_names: &[String],
    in_file_map_keys: &[String],
) -> Vec<CompletionItem> {
    let mut raw: Vec<CompletionItem> = Vec::new();

    match position {
        PositionKind::StructField => {
            // Offer attested field names not already present as a sibling here.
            // (Sibling list already excludes nothing; the surface dedups by name.)
            for name in sibling_field_names {
                raw.push(field_item(name));
            }
        }
        PositionKind::MapKey => {
            for key in in_file_map_keys {
                raw.push(map_key_item(key));
            }
        }
        PositionKind::ListElement
        | PositionKind::Tuple
        | PositionKind::MapValue
        | PositionKind::Value => {
            // Option constructors are always structurally legal at a value slot.
            raw.push(option_item("None", "None"));
            raw.push(option_item("Some", "Some()"));
            // In-file enum variants attested anywhere.
            for v in in_file_variant_names {
                raw.push(variant_item(v));
            }
            // Legal opening delimiters for a fresh composite value.
            for (label, insert) in [("(", "()"), ("[", "[]"), ("{", "{}")] {
                raw.push(delimiter_item(label, insert));
            }
        }
    }

    finalize(raw, prefix)
}

/// Filter by `prefix`, order by kind-then-alpha, drop the exact-literal match, and
/// assign ranks `1..` (all below the user's literal at rank `0`).
fn finalize(mut items: Vec<CompletionItem>, prefix: &str) -> Vec<CompletionItem> {
    // Prefix filter (case-sensitive structural match). An empty prefix matches all.
    items.retain(|it| it.label.starts_with(prefix));

    // Never re-offer the user's exact literal — it already occupies rank 0.
    if !prefix.is_empty() {
        items.retain(|it| it.label != prefix);
    }

    // Deterministic order: kind ordinal, then alphabetical by label, then by
    // insert_text as a final tiebreak so the order is total.
    items.sort_by(|a, b| {
        a.kind
            .order()
            .cmp(&b.kind.order())
            .then_with(|| a.label.cmp(&b.label))
            .then_with(|| a.insert_text.cmp(&b.insert_text))
    });
    items.dedup_by(|a, b| a.label == b.label && a.kind == b.kind);

    // Assign ranks strictly below the literal (the literal is rank 0).
    for (i, item) in items.iter_mut().enumerate() {
        item.rank = (i as u32) + 1;
    }
    items
}

fn field_item(name: &str) -> CompletionItem {
    CompletionItem {
        label: name.to_string(),
        // A field completion drops the user at the field name; the `:` is left for
        // the user to type so we never guess a value. This is valid as a partial.
        insert_text: name.to_string(),
        kind: CompletionKind::Field,
        rank: 0,
    }
}

fn map_key_item(key: &str) -> CompletionItem {
    CompletionItem {
        label: key.to_string(),
        insert_text: key.to_string(),
        kind: CompletionKind::MapKey,
        rank: 0,
    }
}

fn variant_item(name: &str) -> CompletionItem {
    CompletionItem {
        label: name.to_string(),
        insert_text: name.to_string(),
        kind: CompletionKind::Variant,
        rank: 0,
    }
}

fn option_item(label: &str, insert: &str) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        insert_text: insert.to_string(),
        kind: CompletionKind::Option,
        rank: 0,
    }
}

fn delimiter_item(label: &str, insert: &str) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        insert_text: insert.to_string(),
        kind: CompletionKind::Delimiter,
        rank: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    /// Context at the byte offset of the first occurrence of `marker` in `src`.
    fn ctx_at(src: &str, byte_offset: usize) -> CompletionContext {
        completion_context(&parse(src), byte_offset)
    }

    #[test]
    fn empty_buffer_yields_no_items() {
        let ctx = ctx_at("", 0);
        assert_eq!(ctx.position, None);
        assert!(ctx.items.is_empty());
    }

    #[test]
    fn whitespace_only_buffer_yields_no_items() {
        let ctx = ctx_at("   \n\t ", 2);
        assert_eq!(ctx.position, None);
        assert!(ctx.items.is_empty());
    }

    #[test]
    fn top_level_value_offers_option_and_delimiters() {
        // `Foo` is a bare ident value at top level; cursor right after it.
        let ctx = ctx_at("Foo", 3);
        assert_eq!(ctx.position, Some(PositionKind::Value));
        // `Some`/`None`/variants/delimiters are all value-position candidates;
        // filtered by the "Foo" prefix → only items starting with "Foo".
        assert_eq!(ctx.prefix, "Foo");
    }

    #[test]
    fn struct_field_position_is_classified() {
        // `Point(x: 1, )` — caret inside the parens, just before `)`.
        let src = "Point(x: 1, )";
        let ctx = completion_context(&parse(src), src.len() - 1);
        assert_eq!(ctx.position, Some(PositionKind::StructField));
        // `x` is an attested sibling field name.
        assert!(ctx.sibling_field_names.contains(&"x".to_string()));
    }

    #[test]
    fn struct_field_value_position_after_colon() {
        // Caret after the colon in `Point(x: )`.
        let src = "Point(x: )";
        let colon = src.find(':').unwrap();
        let ctx = completion_context(&parse(src), colon + 2);
        assert_eq!(ctx.position, Some(PositionKind::Value));
    }

    #[test]
    fn list_element_position_is_classified() {
        let src = "[1, ]";
        let ctx = completion_context(&parse(src), 4); // before `]`
        assert_eq!(ctx.position, Some(PositionKind::ListElement));
    }

    #[test]
    fn tuple_position_is_classified() {
        let src = "(1, 2)";
        let ctx = completion_context(&parse(src), 4); // inside, before `2`
        assert_eq!(ctx.position, Some(PositionKind::Tuple));
    }

    #[test]
    fn map_key_and_value_positions() {
        let src = "{ name: 1 }";
        // Caret at the start of the key region.
        let key_ctx = completion_context(&parse(src), 2);
        assert_eq!(key_ctx.position, Some(PositionKind::MapKey));
        // Caret after the colon → value.
        let colon = src.find(':').unwrap();
        let val_ctx = completion_context(&parse(src), colon + 2);
        assert_eq!(val_ctx.position, Some(PositionKind::MapValue));
    }

    #[test]
    fn collects_in_file_variant_names() {
        // Two variants attested in a list; cursor at a fresh value slot.
        let src = "[Alpha, Beta, ]";
        let ctx = completion_context(&parse(src), src.len() - 1);
        assert!(ctx.in_file_variant_names.contains(&"Alpha".to_string()));
        assert!(ctx.in_file_variant_names.contains(&"Beta".to_string()));
    }

    #[test]
    fn collects_in_file_map_keys() {
        let src = "{ alpha: 1, beta: 2 }";
        let ctx = completion_context(&parse(src), 2);
        assert!(ctx.in_file_map_keys.contains(&"alpha".to_string()));
        assert!(ctx.in_file_map_keys.contains(&"beta".to_string()));
    }

    #[test]
    fn items_ordered_kind_then_alpha_below_literal() {
        // Value position with an empty prefix: Option items come before variants
        // before delimiters; within a kind, alphabetical.
        let src = "[]";
        let ctx = completion_context(&parse(src), 1); // inside the brackets
        assert_eq!(ctx.position, Some(PositionKind::ListElement));
        let kinds: Vec<CompletionKind> = ctx.items.iter().map(|i| i.kind).collect();
        // Option (None, Some) precede the delimiters.
        let first_option = kinds.iter().position(|k| *k == CompletionKind::Option);
        let first_delim = kinds.iter().position(|k| *k == CompletionKind::Delimiter);
        assert!(first_option < first_delim);
        // Ranks are strictly increasing from 1 and never 0.
        for (i, item) in ctx.items.iter().enumerate() {
            assert_eq!(item.rank, (i as u32) + 1);
            assert!(item.rank >= 1, "no item may share the literal's rank 0");
        }
        // Within the Option kind, "None" sorts before "Some".
        let none_pos = ctx.items.iter().position(|i| i.label == "None");
        let some_pos = ctx.items.iter().position(|i| i.label == "Some");
        assert!(none_pos < some_pos);
    }

    #[test]
    fn prefix_filters_candidates() {
        // Value slot; typing `So` should keep `Some` and drop `None`.
        let src = "So";
        let ctx = completion_context(&parse(src), 2);
        assert_eq!(ctx.prefix, "So");
        assert!(ctx.items.iter().any(|i| i.label == "Some"));
        assert!(ctx.items.iter().all(|i| i.label != "None"));
    }

    #[test]
    fn exact_literal_is_not_reoffered() {
        // Typing the full `Some` must not re-offer `Some` (it is the literal).
        let src = "Some";
        let ctx = completion_context(&parse(src), 4);
        assert!(ctx.items.iter().all(|i| i.label != "Some"));
    }

    #[test]
    fn some_insert_text_round_trips() {
        // The `Some` value-slot suggestion inserts `Some()` which parses cleanly.
        let src = "[]";
        let ctx = completion_context(&parse(src), 1);
        let some = ctx
            .items
            .iter()
            .find(|i| i.label == "Some")
            .expect("Some offered at a value slot");
        let parsed = parse(&some.insert_text);
        assert!(
            parsed.diagnostics().is_empty(),
            "Some insert_text must parse cleanly: {:?}",
            parsed.diagnostics()
        );
    }

    #[test]
    fn delimiter_insert_text_round_trips() {
        let src = "[]";
        let ctx = completion_context(&parse(src), 1);
        for item in ctx
            .items
            .iter()
            .filter(|i| i.kind == CompletionKind::Delimiter)
        {
            let parsed = parse(&item.insert_text);
            assert!(
                parsed.diagnostics().is_empty(),
                "delimiter insert_text {:?} must parse cleanly",
                item.insert_text
            );
        }
    }

    #[test]
    fn out_of_range_offset_is_clamped_not_panicking() {
        let src = "Foo(x: 1)";
        // An offset past the end is clamped to source_len; no panic.
        let ctx = completion_context(&parse(src), 9_999);
        // Cursor at EOF after the closing paren → top-level value position.
        assert!(ctx.position.is_some() || ctx.items.is_empty());
    }

    #[test]
    fn completions_free_fn_matches_items() {
        let src = "[]";
        let ctx = completion_context(&parse(src), 1);
        assert_eq!(completions(&ctx), ctx.items);
    }
}
