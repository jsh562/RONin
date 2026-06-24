//! Pure CST→CST structural-edit transforms (E008 Phase 1a, ADR-0007).
//!
//! This module gives `ronin-core` the **named** structural-edit vocabulary the
//! E008 tree/form and table surfaces build on — insert / remove / reorder a
//! struct field, map entry, or list/tuple element; set a value; rename a
//! field/key; swap an enum variant; and add a field across every record of a
//! list. Each op is a single, non-destructive [`apply_structural`] call that
//! returns a fresh [`CstDocument`] sharing every untouched green subtree with
//! the original (structural sharing), so every region the edit did not touch
//! prints **byte-for-byte** identically (FR-013) and adjacent trivia on
//! surviving siblings is preserved (FR-021).
//!
//! # Placement (ADR-0007)
//!
//! These are the *pure CST→CST transform functions* of the split decided by
//! ADR-0007: they live in `ronin-core` (reusable by a future LSP/web surface),
//! navigate the document only through the typed [`crate::syntax::ast`] accessors,
//! and compose over the existing non-destructive [`apply_edit`] primitive — they
//! introduce **no** parallel edit engine. The *selection→target resolution* and
//! the *view/undo orchestration* stay in `ronin-app`; selection, focus,
//! view-state, and undo wiring MUST NOT enter this module.
//!
//! # WASM-clean (ADR-0007 / HINT-001 / project-instructions §II)
//!
//! This module adds **no** filesystem / UI / async-runtime / native / time
//! dependency. It uses only `std` and `ronin-core`'s own CST types + `apply_edit`,
//! so the `wasm32-unknown-unknown` build of `ronin-core` stays green.
//!
//! # Addressing scheme
//!
//! Each [`StructuralOp`] carries its target as an **`ast`-navigable address**: a
//! parent collection node ([`ParentRef`]) plus, where a specific element is
//! addressed, a **child index** into that parent's elements (fields / entries /
//! items, in source order). The caller (ronin-app, later) resolves a tree/table
//! selection to one of these addresses; this module then re-resolves the address
//! to a located CST node and composes the appropriate [`apply_edit`] call(s).
//! Addressing by *index into the located parent* (rather than by a raw
//! `SyntaxNode` handle) keeps a multi-step op (reorder, variant swap,
//! add-field-across-rows) re-resolvable against each intermediate green tree the
//! composition produces.
//!
//! # Outcome
//!
//! Every op returns a [`TransformOutcome`]: [`TransformOutcome::Applied`] with the
//! new document, or [`TransformOutcome::Blocked`] with a [`BlockedReason`]. A
//! `Blocked` outcome leaves the input CST **unchanged** and produces no edit (a
//! no-op never corrupts the document — project-instructions §I).

use crate::edit::{apply_edit, EditOperation, EditTarget, TriviaPolicy};
use crate::parser::CstDocument;
use crate::syntax::ast;
use crate::syntax::{SyntaxKind, SyntaxNode, SyntaxToken};

/// Which collection a structural op targets, addressed by an `ast`-navigable
/// reference (parent kind + index into the document).
///
/// A [`ParentRef`] names *one* collection node in the document being
/// transformed. The caller resolves a selection to a parent + index; this module
/// re-resolves the parent (by walking the document's value tree to the node whose
/// byte range matches) and then addresses elements by index within it.
///
/// `#[non_exhaustive]` so future container kinds can be added without a breaking
/// change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParentRef {
    /// A struct / anonymous struct node (its `field: value` entries).
    Struct(SyntaxNode),
    /// A map node (its `key: value` entries).
    Map(SyntaxNode),
    /// A list node (its elements).
    List(SyntaxNode),
    /// A tuple node (its positional elements).
    Tuple(SyntaxNode),
    /// An enum variant's struct-like payload (its `field: value` entries).
    EnumVariant(SyntaxNode),
}

impl ParentRef {
    /// The underlying parent [`SyntaxNode`].
    #[must_use]
    pub fn node(&self) -> &SyntaxNode {
        match self {
            Self::Struct(n)
            | Self::Map(n)
            | Self::List(n)
            | Self::Tuple(n)
            | Self::EnumVariant(n) => n,
        }
    }
}

/// A single named structural-edit operation (the E008 transform vocabulary).
///
/// The set is derived from FR-003 (add / remove / reorder / rename fields and
/// elements; change an enum variant) and FR-007 (add / remove rows = elements
/// with sibling-inferred style). Each op identifies its target by an
/// `ast`-navigable address ([`ParentRef`] + child index); see the module docs.
///
/// `#[non_exhaustive]` so the vocabulary can grow without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StructuralOp {
    /// Insert a `field: value` entry into a struct (or `key: value` into a map)
    /// at `index` (clamped to the entry count; appends past the end). For a map,
    /// `name` is the key's literal RON text (e.g. `"k"` or `1`); for a struct it
    /// is the bare field identifier.
    InsertField {
        /// The struct / map / enum-variant payload to insert into.
        parent: ParentRef,
        /// 0-based insertion index among existing entries (clamped; append at end).
        index: usize,
        /// The field name (struct: bare ident) or key literal (map: RON text).
        name: String,
        /// The new value's literal RON text.
        value: String,
    },

    /// Remove the entry at `index` from a struct / map / enum-variant payload,
    /// taking only the entry and its own trivia and normalizing the separator
    /// (no dangling / doubled / orphaned comma) per FR-021.
    RemoveField {
        /// The struct / map / enum-variant payload to remove from.
        parent: ParentRef,
        /// 0-based index of the entry to remove.
        index: usize,
    },

    /// Rename the struct-field name / map-key at `index` to `new_name` in place.
    /// Blocks with [`BlockedReason::RenameCollision`] if `new_name` already names
    /// another entry in the **same** parent (collision scope = the immediate
    /// enclosing struct/map), leaving the document byte-unchanged (FR-003).
    RenameKey {
        /// The struct / map / enum-variant payload whose entry is renamed.
        parent: ParentRef,
        /// 0-based index of the entry to rename.
        index: usize,
        /// The replacement field name / key literal.
        new_name: String,
    },

    /// Move the child at `from` to be at position `to` within `parent`, composed
    /// as one transform (remove + re-insert) producing one new CST. Works for a
    /// struct/map entry or a list/tuple element.
    ReorderChild {
        /// The collection whose child is moved.
        parent: ParentRef,
        /// 0-based source index of the child to move.
        from: usize,
        /// 0-based destination index (in the pre-move indexing).
        to: usize,
    },

    /// Replace the value at `index` within `parent` with `value` (new literal RON
    /// text). For a struct/map entry this replaces the entry's *value* (the part
    /// after `:`); for a list/tuple it replaces the element.
    SetValue {
        /// The collection whose child value is replaced.
        parent: ParentRef,
        /// 0-based index of the entry/element whose value is set.
        index: usize,
        /// The replacement value's literal RON text.
        value: String,
    },

    /// Insert an element into a list / tuple at `index` (clamped; append past the
    /// end), adopting the collection's existing layout style — indentation and
    /// trailing-comma convention inferred from its siblings, or the document's
    /// default for an empty collection (AD-005, FR-007).
    InsertElement {
        /// The list / tuple to insert into.
        parent: ParentRef,
        /// 0-based insertion index among existing elements (clamped; append).
        index: usize,
        /// The new element's literal RON text.
        value: String,
    },

    /// Remove the element at `index` from a list / tuple, taking only the element
    /// and its own trivia and normalizing the separator (FR-021).
    RemoveElement {
        /// The list / tuple to remove from.
        parent: ParentRef,
        /// 0-based index of the element to remove.
        index: usize,
    },

    /// Swap an enum variant's name + field set in place: rename the variant to
    /// `new_name`, keep a field present in **both** old and new (its value/bytes
    /// preserved), remove a field present **only** in the old variant, and add a
    /// field present **only** in the new variant with `placeholder` as its value
    /// (FR-003).
    SwapEnumVariant {
        /// The enum-variant node to swap.
        variant: SyntaxNode,
        /// The new variant name.
        new_name: String,
        /// The new variant's field set, in order (struct-like variants); empty
        /// for a bare variant.
        new_fields: Vec<String>,
        /// The placeholder value text used for a field present only in the new
        /// variant.
        placeholder: String,
    },

    /// Add the field `name: value` to **every** record (struct element) of a
    /// list, batched into one transform (the multi-node op of ADR-0007 /
    /// data-model). Records that are not structs are skipped. The field is
    /// appended to each record.
    AddFieldAcrossRows {
        /// The list whose struct elements each gain the field.
        list: SyntaxNode,
        /// The new field name (bare ident).
        name: String,
        /// The new field's value text.
        value: String,
    },
}

/// The result of applying a [`StructuralOp`].
///
/// `#[non_exhaustive]` so additional outcome arms can be added without a breaking
/// change.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum TransformOutcome {
    /// The op applied; carries the new document (untouched regions byte-identical).
    Applied(CstDocument),
    /// The op was rejected; the input document is unchanged and no edit was made.
    Blocked(BlockedReason),
}

/// Why a [`StructuralOp`] was [`TransformOutcome::Blocked`].
///
/// A `Blocked` outcome guarantees the input CST is unchanged (FR-003, §I).
///
/// `#[non_exhaustive]` so future block reasons can be added without a breaking
/// change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BlockedReason {
    /// A rename would collide with an existing field/key in the same struct/map.
    RenameCollision,
    /// The addressed parent / element could not be located in the document
    /// (e.g. an out-of-range index, or a node from a different tree).
    TargetNotFound,
    /// The op is not valid for the addressed node (e.g. an op that needs a struct
    /// applied to a list element, or new payload text that does not parse).
    InvalidPayload,
}

// =============================================================================
// Public entry point
// =============================================================================

/// Apply one named structural [`StructuralOp`] to `doc`, returning a
/// [`TransformOutcome`].
///
/// The original `doc` is never mutated. On [`TransformOutcome::Applied`] the
/// returned document shares every untouched green subtree with `doc`, so all
/// untouched regions print byte-for-byte identically (FR-013) and surviving
/// siblings keep their comments / blank lines / trailing commas (FR-021). On
/// [`TransformOutcome::Blocked`] no edit is made and `doc` is unchanged.
#[must_use]
pub fn apply_structural(doc: &CstDocument, op: StructuralOp) -> TransformOutcome {
    let result = match op {
        StructuralOp::InsertField {
            parent,
            index,
            name,
            value,
        } => insert_entry(doc, &parent, index, &name, &value),
        StructuralOp::RemoveField { parent, index } => remove_child(doc, &parent, index),
        StructuralOp::RenameKey {
            parent,
            index,
            new_name,
        } => rename_key(doc, &parent, index, &new_name),
        StructuralOp::ReorderChild { parent, from, to } => reorder_child(doc, &parent, from, to),
        StructuralOp::SetValue {
            parent,
            index,
            value,
        } => set_value(doc, &parent, index, &value),
        StructuralOp::InsertElement {
            parent,
            index,
            value,
        } => insert_element(doc, &parent, index, &value),
        StructuralOp::RemoveElement { parent, index } => remove_child(doc, &parent, index),
        StructuralOp::SwapEnumVariant {
            variant,
            new_name,
            new_fields,
            placeholder,
        } => swap_enum_variant(doc, &variant, &new_name, &new_fields, &placeholder),
        StructuralOp::AddFieldAcrossRows { list, name, value } => {
            add_field_across_rows(doc, &list, &name, &value)
        }
    };
    match result {
        Ok(new_doc) => TransformOutcome::Applied(new_doc),
        Err(reason) => TransformOutcome::Blocked(reason),
    }
}

// =============================================================================
// Located addressing — re-resolve a ParentRef against `doc` by byte range.
//
// A ParentRef holds a node from (typically) `doc`'s own tree; we re-resolve it
// against the live document so that a multi-step op stays valid against each
// intermediate green tree the composition produces. Matching is by kind + byte
// range, which is unique for a given tree.
// =============================================================================

/// Re-resolve a [`ParentRef`] to the live node of the same kind + **start offset**
/// in `doc`.
fn locate_parent(doc: &CstDocument, parent: &ParentRef) -> Option<SyntaxNode> {
    let want = parent.node();
    find_node(&doc.root(), want.kind(), want.text_range().start())
}

/// Re-resolve an arbitrary node to the live node of the same kind + start offset.
fn locate_node(doc: &CstDocument, node: &SyntaxNode) -> Option<SyntaxNode> {
    find_node(&doc.root(), node.kind(), node.text_range().start())
}

/// Depth-first search for a node of `kind` whose **start offset** equals `start`.
///
/// A container's start offset (its opening delimiter / name token) is **stable**
/// across edits made *inside* it, so start-offset + kind re-resolves the same
/// container even after a composed edit shrinks/grows its body — unlike an exact
/// byte range, which changes when the body changes. A container's first child
/// node always starts strictly after the container's first token, so (kind,
/// start) is unambiguous for a given container kind.
fn find_node(root: &SyntaxNode, kind: SyntaxKind, start: usize) -> Option<SyntaxNode> {
    fn walk(node: &SyntaxNode, kind: SyntaxKind, start: usize, out: &mut Option<SyntaxNode>) {
        if out.is_some() {
            return;
        }
        if node.kind() == kind && node.text_range().start() == start {
            *out = Some(node.clone());
            return;
        }
        for child in node.children() {
            let cr = child.text_range();
            // Descend only where the wanted start falls within the child's span.
            if cr.start() <= start && start < cr.end() {
                walk(&child, kind, start, out);
            }
        }
    }
    let mut out = None;
    walk(root, kind, start, &mut out);
    out
}

/// The ordered child *entry/element* nodes of a located parent (skipping trivia,
/// punctuation, and — for a struct/map/variant — anything that is not an entry).
fn child_nodes(parent: &ParentRef, located: &SyntaxNode) -> Vec<SyntaxNode> {
    match parent {
        ParentRef::Struct(_) => ast::Struct::cast(located.clone())
            .map(|s| s.fields().map(|f| f.syntax().clone()).collect())
            .unwrap_or_default(),
        ParentRef::Map(_) => ast::Map::cast(located.clone())
            .map(|m| m.entries().map(|e| e.syntax().clone()).collect())
            .unwrap_or_default(),
        ParentRef::EnumVariant(_) => ast::EnumVariant::cast(located.clone())
            .map(|v| v.entries().map(|e| e.syntax().clone()).collect())
            .unwrap_or_default(),
        ParentRef::List(_) => ast::List::cast(located.clone())
            .map(|l| l.items().map(|v| v.syntax().clone()).collect())
            .unwrap_or_default(),
        ParentRef::Tuple(_) => ast::Tuple::cast(located.clone())
            .map(|t| t.items().map(|v| v.syntax().clone()).collect())
            .unwrap_or_default(),
    }
}

/// The closing-delimiter token of a collection (`)`, `]`, or `}`), if any.
fn closing_delimiter(node: &SyntaxNode) -> Option<SyntaxToken> {
    node.children_with_tokens()
        .filter_map(|el| el.as_token().cloned())
        .filter(|t| {
            matches!(
                t.kind(),
                SyntaxKind::RParen | SyntaxKind::RBracket | SyntaxKind::RBrace
            )
        })
        .last()
}

// =============================================================================
// Style inference (AD-005 / FR-007)
// =============================================================================

/// The layout style of a collection, inferred from its siblings (or the document
/// default for an empty collection).
#[derive(Debug, Clone)]
struct CollectionStyle {
    /// `true` if the collection lays out one element per line.
    multiline: bool,
    /// The indentation string for an element (leading whitespace of a sibling).
    element_indent: String,
    /// Indentation of the closing delimiter (one level out from elements).
    closing_indent: String,
    /// `true` if the predominant convention is a trailing comma after each element.
    trailing_comma: bool,
    /// The document's end-of-line sequence (`"\r\n"` or `"\n"`) used for any
    /// synthesized newline, so an insert/append into a CRLF document keeps CRLF
    /// (byte-losslessness — Principle I) instead of flipping edited lines to LF.
    eol: String,
}

/// Infer a collection's append style from its existing element siblings, falling
/// back to the document's prevailing/default style for an empty collection
/// (AD-005, FR-007).
fn infer_style(
    doc: &CstDocument,
    located: &SyntaxNode,
    elements: &[SyntaxNode],
) -> CollectionStyle {
    if elements.is_empty() {
        return document_default_style(doc, located);
    }

    // Leading-whitespace indent of the predominant element (most-frequent;
    // ties broken toward the last sibling — we scan and keep the last seen of
    // the max-count indent by iterating in order and using >= on count update).
    let indents: Vec<String> = elements.iter().map(leading_indent_of).collect();
    let element_indent = predominant(&indents);

    // Multi-line if any element starts on its own line (its leading trivia
    // contains a newline) OR the collection text contains a newline between the
    // open delimiter and the first element.
    let multiline = indents.iter().any(|s| s.contains('\n'))
        || located.text().contains('\n') && !indents.iter().all(String::is_empty);

    // Trailing-comma convention: does a comma immediately follow the LAST element
    // (ignoring trivia) before the closing delimiter? That is the predominant
    // last-position convention.
    let trailing_comma = last_element_has_trailing_comma(located, elements);

    // Closing-delimiter indent: the whitespace before the closing delimiter, or
    // one indent level shallower than an element indent.
    let closing_indent = closing_indent_of(located, &element_indent, multiline);

    CollectionStyle {
        multiline,
        element_indent,
        closing_indent,
        trailing_comma,
        eol: detect_document_eol(doc),
    }
}

/// The leading whitespace run (after the last newline, if multi-line) of a node:
/// the indent the next sibling should mirror.
fn leading_indent_of(node: &SyntaxNode) -> String {
    // Walk left siblings (tokens) collecting the run of whitespace immediately
    // preceding this node, then take the text after the final newline.
    let parent = match node.parent() {
        Some(p) => p,
        None => return String::new(),
    };
    let mut ws = String::new();
    let target_range = node.text_range();
    let mut prev_ws = String::new();
    for el in parent.children_with_tokens() {
        match el {
            crate::syntax::SyntaxElement::Token(t) => {
                if t.kind() == SyntaxKind::Whitespace {
                    prev_ws = t.text().to_string();
                } else {
                    prev_ws.clear();
                }
            }
            crate::syntax::SyntaxElement::Node(n) => {
                if n.text_range() == target_range {
                    ws = prev_ws.clone();
                    break;
                }
                prev_ws.clear();
            }
        }
    }
    // Keep the indent on the element's own line: the text after the last newline.
    match ws.rfind('\n') {
        Some(i) => ws[i + 1..].to_string(),
        None => {
            if ws.contains('\n') {
                ws
            } else {
                // Single-line: no per-line indent.
                ws
            }
        }
    }
}

/// The most-frequent string in `items`, ties broken toward the **last** item's
/// value (FR-007 deterministic tie-break).
fn predominant(items: &[String]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut best = items[items.len() - 1].clone();
    let mut best_count = 0usize;
    // Iterate in order; use strict `>` so an earlier value only wins if it is
    // strictly more frequent, leaving ties resolved toward the later (last) item
    // which we seed as the initial best.
    for candidate in items {
        let count = items.iter().filter(|s| *s == candidate).count();
        if count > best_count {
            best_count = count;
            best = candidate.clone();
        }
    }
    best
}

/// Does a comma immediately follow the last element (ignoring trivia) before the
/// closing delimiter?
fn last_element_has_trailing_comma(located: &SyntaxNode, elements: &[SyntaxNode]) -> bool {
    let Some(last) = elements.last() else {
        return false;
    };
    let last_end = last.text_range().end();
    // Find the first non-trivia token after the last element.
    located
        .children_with_tokens()
        .filter_map(|el| match el {
            crate::syntax::SyntaxElement::Token(t) => Some(t),
            crate::syntax::SyntaxElement::Node(_) => None,
        })
        .filter(|t| t.text_range().start() >= last_end)
        .find(|t| !t.is_trivia())
        .map(|t| t.kind() == SyntaxKind::Comma)
        .unwrap_or(false)
}

/// The indentation of the closing delimiter (the whitespace after the last
/// newline preceding it), or a derived value when not multi-line.
fn closing_indent_of(located: &SyntaxNode, element_indent: &str, multiline: bool) -> String {
    if !multiline {
        return String::new();
    }
    if let Some(close) = closing_delimiter(located) {
        let close_start = close.text_range().start();
        // The whitespace token immediately before the closing delimiter.
        let prev_ws = located
            .children_with_tokens()
            .filter_map(|el| match el {
                crate::syntax::SyntaxElement::Token(t) => Some(t),
                crate::syntax::SyntaxElement::Node(_) => None,
            })
            .filter(|t| t.text_range().end() <= close_start && t.kind() == SyntaxKind::Whitespace)
            .last();
        if let Some(ws) = prev_ws {
            if let Some(i) = ws.text().rfind('\n') {
                return ws.text()[i + 1..].to_string();
            }
        }
    }
    // Fallback: one level shallower than the element indent (drop one unit, best
    // effort by removing a 4-space / tab prefix).
    derive_outer_indent(element_indent)
}

/// Best-effort one-level-shallower indent of `inner` (drop a trailing 4 spaces
/// or one tab from the front).
fn derive_outer_indent(inner: &str) -> String {
    if let Some(stripped) = inner.strip_suffix("    ") {
        stripped.to_string()
    } else if let Some(stripped) = inner.strip_suffix('\t') {
        stripped.to_string()
    } else {
        String::new()
    }
}

/// Document default style for an empty collection (AD-005, FR-007): multi-line,
/// document-detected indent (default 4 spaces), trailing-comma per document
/// convention (default present).
fn document_default_style(doc: &CstDocument, located: &SyntaxNode) -> CollectionStyle {
    let unit = detect_document_indent_unit(doc);
    // The indent of the empty collection's own opening line (best effort: indent
    // of the line the collection sits on, plus one unit for its elements).
    let base_indent = leading_indent_of(located);
    let element_indent = format!("{base_indent}{unit}");
    let trailing_comma = detect_document_trailing_comma(doc);
    CollectionStyle {
        multiline: true,
        element_indent,
        closing_indent: base_indent,
        trailing_comma,
        eol: detect_document_eol(doc),
    }
}

/// Detect the document's predominant end-of-line sequence so any *synthesized*
/// newline (an inserted/appended element's separator + indent) matches the file's
/// existing convention instead of a hardcoded LF — otherwise inserting into a CRLF
/// document would silently flip edited lines to LF (Principle I, "never corrupt").
///
/// Counts `\r\n` pairs vs lone `\n` over the printed document and returns the
/// dominant; ties (and a document with no newline) resolve to `"\n"`. Mirrors the
/// app-layer `ByteFidelityProfile` dominant rule, kept here so `ronin-core` stays
/// WASM-clean (no app dependency).
fn detect_document_eol(doc: &CstDocument) -> String {
    let text = crate::printer::print(doc);
    let bytes = text.as_bytes();
    let mut crlf = 0usize;
    let mut lone_lf = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            if i > 0 && bytes[i - 1] == b'\r' {
                crlf += 1;
            } else {
                lone_lf += 1;
            }
        }
    }
    if crlf > lone_lf {
        "\r\n".to_string()
    } else {
        "\n".to_string()
    }
}

/// Detect the document's indentation unit from the first indented line; default
/// to four spaces when none can be detected (FR-007).
fn detect_document_indent_unit(doc: &CstDocument) -> String {
    let text = crate::printer::print(doc);
    for line in text.lines() {
        let trimmed = line.trim_start_matches([' ', '\t']);
        if trimmed.is_empty() || trimmed.len() == line.len() {
            continue; // blank or unindented line
        }
        let indent = &line[..line.len() - trimmed.len()];
        if !indent.is_empty() {
            return indent.to_string();
        }
    }
    "    ".to_string()
}

/// Detect the document's predominant trailing-comma convention; default to
/// *present* when none can be detected (FR-007).
fn detect_document_trailing_comma(doc: &CstDocument) -> bool {
    // Scan every collection node: does its last element carry a trailing comma?
    let mut present = 0usize;
    let mut absent = 0usize;
    fn walk(node: &SyntaxNode, present: &mut usize, absent: &mut usize) {
        let elements: Vec<SyntaxNode> = match node.kind() {
            SyntaxKind::Struct => ast::Struct::cast(node.clone())
                .map(|s| s.fields().map(|f| f.syntax().clone()).collect())
                .unwrap_or_default(),
            SyntaxKind::Map => ast::Map::cast(node.clone())
                .map(|m| m.entries().map(|e| e.syntax().clone()).collect())
                .unwrap_or_default(),
            SyntaxKind::List => ast::List::cast(node.clone())
                .map(|l| l.items().map(|v| v.syntax().clone()).collect())
                .unwrap_or_default(),
            SyntaxKind::Tuple => ast::Tuple::cast(node.clone())
                .map(|t| t.items().map(|v| v.syntax().clone()).collect())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        if !elements.is_empty() {
            if last_element_has_trailing_comma(node, &elements) {
                *present += 1;
            } else {
                *absent += 1;
            }
        }
        for child in node.children() {
            walk(&child, present, absent);
        }
    }
    walk(&doc.root(), &mut present, &mut absent);
    if present == 0 && absent == 0 {
        true // document default: trailing-comma-present
    } else {
        present >= absent
    }
}

// =============================================================================
// Op implementations
// =============================================================================

/// Insert a `name: value` entry into a struct / map / enum-variant payload.
fn insert_entry(
    doc: &CstDocument,
    parent: &ParentRef,
    index: usize,
    name: &str,
    value: &str,
) -> Result<CstDocument, BlockedReason> {
    let located = locate_parent(doc, parent).ok_or(BlockedReason::TargetNotFound)?;
    let elements = child_nodes(parent, &located);
    let is_map = matches!(parent, ParentRef::Map(_));
    let entry_text = format!("{name}: {value}");

    insert_child_text(doc, &located, &elements, index, &entry_text, is_map)
}

/// Insert an element into a list / tuple at `index`.
fn insert_element(
    doc: &CstDocument,
    parent: &ParentRef,
    index: usize,
    value: &str,
) -> Result<CstDocument, BlockedReason> {
    if !matches!(parent, ParentRef::List(_) | ParentRef::Tuple(_)) {
        return Err(BlockedReason::InvalidPayload);
    }
    let located = locate_parent(doc, parent).ok_or(BlockedReason::TargetNotFound)?;
    let elements = child_nodes(parent, &located);
    insert_child_text(doc, &located, &elements, index, value, false)
}

/// Shared insertion: splice `child_text` (a fully-formed entry/element) at
/// `index` among `elements`, adopting the collection's inferred style for
/// indentation + separators (AD-005, FR-007). When inserting before an existing
/// element we insert *before* that element's own leading trivia preserved; when
/// appending we insert before the closing delimiter.
fn insert_child_text(
    doc: &CstDocument,
    located: &SyntaxNode,
    elements: &[SyntaxNode],
    index: usize,
    child_text: &str,
    _is_map: bool,
) -> Result<CstDocument, BlockedReason> {
    let style = infer_style(doc, located, elements);
    let idx = index.min(elements.len());

    if idx < elements.len() {
        // Insert before the element currently at `idx`.
        let target = &elements[idx];
        let payload = if style.multiline {
            format!("{child_text},{}{}", style.eol, style.element_indent)
        } else {
            format!("{child_text}, ")
        };
        let edit = EditOperation::insert(
            EditTarget::Node(target.clone()),
            payload,
            TriviaPolicy::KEEP_ALL,
        );
        apply_edit(doc, edit).map_err(|_| BlockedReason::TargetNotFound)
    } else {
        // Append after the last element (or into an empty collection).
        append_child_text(doc, located, elements, child_text, &style)
    }
}

/// Append `child_text` as the last element/entry of a collection, adopting the
/// inferred style. Inserts before the closing delimiter.
fn append_child_text(
    doc: &CstDocument,
    located: &SyntaxNode,
    elements: &[SyntaxNode],
    child_text: &str,
    style: &CollectionStyle,
) -> Result<CstDocument, BlockedReason> {
    let close = closing_delimiter(located).ok_or(BlockedReason::InvalidPayload)?;

    if let Some(last) = elements.last() {
        let has_trailing = last_element_has_trailing_comma(located, elements);
        let payload = build_append_after_last(child_text, style, has_trailing);
        if style.multiline {
            // Multi-line: insert before the closing delimiter (which sits on its
            // own line), so the new element lands on a fresh indented line and the
            // closing line is preserved.
            let edit = EditOperation::insert(
                EditTarget::TokenSpan {
                    first: close.clone(),
                    last: close,
                },
                payload,
                TriviaPolicy::KEEP_ALL,
            );
            apply_edit(doc, edit).map_err(|_| BlockedReason::TargetNotFound)
        } else {
            // Single-line: insert immediately after the last element's last token
            // (before the first following token), so any trailing ` }` / ` ]`
            // spacing before the close stays byte-identical.
            let last_end = last.text_range().end();
            let after = first_direct_token_at_or_after(located, last_end);
            match after {
                Some(tok) => {
                    let edit = EditOperation::insert(
                        EditTarget::TokenSpan {
                            first: tok.clone(),
                            last: tok,
                        },
                        payload,
                        TriviaPolicy::KEEP_ALL,
                    );
                    apply_edit(doc, edit).map_err(|_| BlockedReason::TargetNotFound)
                }
                None => {
                    let edit = EditOperation::insert(
                        EditTarget::TokenSpan {
                            first: close.clone(),
                            last: close,
                        },
                        payload,
                        TriviaPolicy::KEEP_ALL,
                    );
                    apply_edit(doc, edit).map_err(|_| BlockedReason::TargetNotFound)
                }
            }
        }
    } else {
        // Empty collection: lay out the first element per the document default.
        let payload = if style.multiline {
            let comma = if style.trailing_comma { "," } else { "" };
            format!(
                "{eol}{}{child_text}{comma}{eol}{}",
                style.element_indent,
                style.closing_indent,
                eol = style.eol
            )
        } else {
            child_text.to_string()
        };
        let edit = EditOperation::insert(
            EditTarget::TokenSpan {
                first: close.clone(),
                last: close,
            },
            payload,
            TriviaPolicy::KEEP_ALL,
        );
        apply_edit(doc, edit).map_err(|_| BlockedReason::TargetNotFound)
    }
}

/// Build the payload inserted before the closing delimiter when appending after
/// an existing last element. The last element keeps its bytes; we add the new
/// element with a leading separator that respects the trailing-comma convention.
fn build_append_after_last(
    child_text: &str,
    style: &CollectionStyle,
    last_has_trailing_comma: bool,
) -> String {
    if style.multiline {
        // If the last element already has a trailing comma, the text before the
        // close is `<elem>,\n<closing_indent>`. We want to land a new element on
        // its own line. Insert before the close: `<elem_indent><new>,\n<close>`.
        let new_trailing = if style.trailing_comma { "," } else { "" };
        if last_has_trailing_comma {
            format!(
                "{}{child_text}{new_trailing}{eol}{}",
                style.element_indent,
                style.closing_indent,
                eol = style.eol
            )
        } else {
            // Last element has no trailing comma; add one to it then the new one.
            format!(
                ",{eol}{}{child_text}{new_trailing}{eol}{}",
                style.element_indent,
                style.closing_indent,
                eol = style.eol
            )
        }
    } else {
        // Single-line, inserted immediately AFTER the last element's last token.
        // Add the separator before the new element. When the last element already
        // carries a trailing comma, only a space precedes the new element.
        let new_trailing = if style.trailing_comma { "," } else { "" };
        if last_has_trailing_comma {
            format!(" {child_text}{new_trailing}")
        } else {
            format!(", {child_text}{new_trailing}")
        }
    }
}

/// The first direct-child token of `located` whose range starts at or after
/// `offset` (the token immediately following an element).
fn first_direct_token_at_or_after(located: &SyntaxNode, offset: usize) -> Option<SyntaxToken> {
    located
        .children_with_tokens()
        .filter_map(|el| el.as_token().cloned())
        .find(|t| t.text_range().start() >= offset)
}

/// Remove the child at `index` from a parent, taking only the child + its own
/// trivia and normalizing the trailing-comma separator (FR-021).
///
/// Composed as two non-destructive edits over [`apply_edit`], producing one new
/// CST: first the separator run (comma + adjacent same-line trivia, all *direct
/// child tokens* of the parent), then the element node itself (with its leading
/// whitespace absorbed). All separator pieces are direct-child tokens, so each
/// step is a well-formed [`EditTarget`]; the element is re-resolved by index
/// between the two steps against the intermediate tree.
fn remove_child(
    doc: &CstDocument,
    parent: &ParentRef,
    index: usize,
) -> Result<CstDocument, BlockedReason> {
    let located = locate_parent(doc, parent).ok_or(BlockedReason::TargetNotFound)?;
    let elements = child_nodes(parent, &located);
    if index >= elements.len() {
        return Err(BlockedReason::TargetNotFound);
    }
    let is_last = index + 1 == elements.len();
    let sole = elements.len() == 1;
    let last_trailing_comma = last_element_has_trailing_comma(&located, &elements);

    if !is_last {
        // --- Non-last element ---------------------------------------------
        // Delete: [element][following comma][following trivia up to and
        // INCLUDING the newline-indent that leads element i+1]; KEEP element i's
        // own leading trivia, which becomes element i+1's new leading run. This
        // collapses to one clean separator for both single-line and multi-line.
        let after_sep = remove_following_separator(doc, &located, &elements, index)?;
        let located2 = locate_parent(&after_sep, parent).ok_or(BlockedReason::TargetNotFound)?;
        let elements2 = child_nodes(parent, &located2);
        let target = elements2.get(index).ok_or(BlockedReason::TargetNotFound)?;
        let edit = EditOperation::remove(
            EditTarget::Node(target.clone()),
            TriviaPolicy {
                keep_leading: true,
                keep_trailing: true,
            },
        );
        apply_edit(&after_sep, edit).map_err(|_| BlockedReason::TargetNotFound)
    } else if sole {
        // --- Sole element -------------------------------------------------
        // Remove the element node; KEEP surrounding trivia so an empty
        // collection retains its delimiters' own layout (lossless).
        let edit = EditOperation::remove(
            EditTarget::Node(elements[index].clone()),
            TriviaPolicy::KEEP_ALL,
        );
        apply_edit(doc, edit).map_err(|_| BlockedReason::TargetNotFound)
    } else if last_trailing_comma {
        // --- Last element WITH a trailing comma (multi-line style) --------
        // Keep the previous element's comma; delete element i's leading trivia +
        // element + its trailing comma + same-line trivia, keeping the final
        // newline before the closing delimiter.
        let after_trail = remove_trailing_comma_run(doc, &located, &elements, index)?;
        let located2 = locate_parent(&after_trail, parent).ok_or(BlockedReason::TargetNotFound)?;
        let elements2 = child_nodes(parent, &located2);
        let target = elements2.get(index).ok_or(BlockedReason::TargetNotFound)?;
        let edit = EditOperation::remove(
            EditTarget::Node(target.clone()),
            TriviaPolicy {
                keep_leading: false,
                keep_trailing: true,
            },
        );
        apply_edit(&after_trail, edit).map_err(|_| BlockedReason::TargetNotFound)
    } else {
        // --- Last element WITHOUT a trailing comma (single-line style) ----
        // Delete the preceding comma + trivia, then the element + its leading.
        let after_sep = remove_preceding_separator(doc, &located, &elements, index)?;
        let located2 = locate_parent(&after_sep, parent).ok_or(BlockedReason::TargetNotFound)?;
        let elements2 = child_nodes(parent, &located2);
        let target = elements2.get(index).ok_or(BlockedReason::TargetNotFound)?;
        let edit = EditOperation::remove(
            EditTarget::Node(target.clone()),
            TriviaPolicy {
                keep_leading: false,
                keep_trailing: true,
            },
        );
        apply_edit(&after_sep, edit).map_err(|_| BlockedReason::TargetNotFound)
    }
}

/// Remove the separator run *following* element `index` (the comma and **all**
/// trivia up to the next element's first token), as a direct-child token span.
/// Element i's own leading trivia is kept and becomes element i+1's leading run.
fn remove_following_separator(
    doc: &CstDocument,
    located: &SyntaxNode,
    elements: &[SyntaxNode],
    index: usize,
) -> Result<CstDocument, BlockedReason> {
    let between = direct_tokens_between(
        located,
        elements[index].text_range().end(),
        elements[index + 1].text_range().start(),
    );
    remove_token_run(doc, &between)
}

/// Remove the separator run *preceding* element `index` (the comma + trivia back
/// to the previous element's end), as a direct-child token span. For `index == 0`
/// there is nothing to remove.
fn remove_preceding_separator(
    doc: &CstDocument,
    located: &SyntaxNode,
    elements: &[SyntaxNode],
    index: usize,
) -> Result<CstDocument, BlockedReason> {
    if index == 0 {
        return Ok(doc.clone());
    }
    let between = direct_tokens_between(
        located,
        elements[index - 1].text_range().end(),
        elements[index].text_range().start(),
    );
    remove_token_run(doc, &between)
}

/// Remove the trailing-comma run that *follows* the last element `index` (its
/// comma + same-line trivia), keeping the final newline-indent before the closing
/// delimiter so the collection's closing line stays put.
fn remove_trailing_comma_run(
    doc: &CstDocument,
    located: &SyntaxNode,
    elements: &[SyntaxNode],
    index: usize,
) -> Result<CstDocument, BlockedReason> {
    let elem_end = elements[index].text_range().end();
    let close_start = closing_delimiter(located)
        .map(|t| t.text_range().start())
        .unwrap_or_else(|| located.text_range().end());
    let between = direct_tokens_between(located, elem_end, close_start);
    // Keep a trailing newline-ws run (it leads the closing delimiter's line).
    let mut last_idx = between.len();
    while last_idx > 0 {
        let t = &between[last_idx - 1];
        if t.kind() == SyntaxKind::Whitespace && t.text().contains('\n') {
            last_idx -= 1;
        } else {
            break;
        }
    }
    remove_token_run(doc, &between[..last_idx])
}

/// Direct-child tokens of `located` whose ranges fall within `[lo, hi)`.
fn direct_tokens_between(located: &SyntaxNode, lo: usize, hi: usize) -> Vec<SyntaxToken> {
    located
        .children_with_tokens()
        .filter_map(|el| el.as_token().cloned())
        .filter(|t| t.text_range().start() >= lo && t.text_range().end() <= hi)
        .collect()
}

/// Remove a contiguous run of direct-child tokens via one [`apply_edit`]; a no-op
/// (returns the document unchanged) when the run is empty.
fn remove_token_run(doc: &CstDocument, run: &[SyntaxToken]) -> Result<CstDocument, BlockedReason> {
    let Some(first) = run.first().cloned() else {
        return Ok(doc.clone());
    };
    let last = run[run.len() - 1].clone();
    let edit = EditOperation::remove(
        EditTarget::TokenSpan { first, last },
        TriviaPolicy::KEEP_ALL,
    );
    apply_edit(doc, edit).map_err(|_| BlockedReason::TargetNotFound)
}

/// Rename the key/field at `index` to `new_name`, blocking on a same-parent
/// collision (FR-003).
fn rename_key(
    doc: &CstDocument,
    parent: &ParentRef,
    index: usize,
    new_name: &str,
) -> Result<CstDocument, BlockedReason> {
    let located = locate_parent(doc, parent).ok_or(BlockedReason::TargetNotFound)?;
    let elements = child_nodes(parent, &located);
    let target = elements.get(index).ok_or(BlockedReason::TargetNotFound)?;

    // Collision check: does any *other* entry already use `new_name`?
    for (i, el) in elements.iter().enumerate() {
        if i == index {
            continue;
        }
        if entry_key_text(parent, el).as_deref() == Some(new_name) {
            return Err(BlockedReason::RenameCollision);
        }
    }

    // The key token to replace. A struct field's key is its name `Ident`. A map
    // entry — and an enum-variant struct-payload entry, which parses as a
    // `MapEntry` whose key is a bare-ident value — replaces the whole key node.
    let key_target = match parent {
        ParentRef::Struct(_) => {
            let field =
                ast::StructField::cast(target.clone()).ok_or(BlockedReason::InvalidPayload)?;
            let name = field.name().ok_or(BlockedReason::InvalidPayload)?;
            EditTarget::TokenSpan {
                first: name.clone(),
                last: name,
            }
        }
        ParentRef::Map(_) | ParentRef::EnumVariant(_) => {
            let entry = ast::MapEntry::cast(target.clone()).ok_or(BlockedReason::InvalidPayload)?;
            let key = entry.key().ok_or(BlockedReason::InvalidPayload)?;
            // Replace the whole key value node (covers literal/ident keys).
            EditTarget::Node(key.syntax().clone())
        }
        _ => return Err(BlockedReason::InvalidPayload),
    };

    let edit = EditOperation::replace(key_target, new_name.to_string(), TriviaPolicy::KEEP_ALL);
    apply_edit(doc, edit).map_err(|_| BlockedReason::TargetNotFound)
}

/// The verbatim key text of a [`ast::MapEntry`] (its key value's source text).
fn map_entry_key_text(entry: &ast::MapEntry) -> Option<String> {
    entry.key().map(|k| k.syntax().text())
}

/// The key text of an entry (struct field name / map key literal / enum-variant
/// payload field name), for collision comparison.
fn entry_key_text(parent: &ParentRef, entry: &SyntaxNode) -> Option<String> {
    match parent {
        ParentRef::Struct(_) => ast::StructField::cast(entry.clone()).and_then(|f| f.name_text()),
        ParentRef::Map(_) | ParentRef::EnumVariant(_) => ast::MapEntry::cast(entry.clone())
            .and_then(|e| e.key())
            .map(|k| k.syntax().text()),
        _ => None,
    }
}

/// Replace the value of an entry / element at `index` with `value` text.
fn set_value(
    doc: &CstDocument,
    parent: &ParentRef,
    index: usize,
    value: &str,
) -> Result<CstDocument, BlockedReason> {
    let located = locate_parent(doc, parent).ok_or(BlockedReason::TargetNotFound)?;
    let elements = child_nodes(parent, &located);
    let target = elements.get(index).ok_or(BlockedReason::TargetNotFound)?;

    let value_node = match parent {
        ParentRef::Struct(_) => ast::StructField::cast(target.clone())
            .and_then(|f| f.value())
            .map(|v| v.syntax().clone()),
        ParentRef::Map(_) | ParentRef::EnumVariant(_) => ast::MapEntry::cast(target.clone())
            .and_then(|e| e.value())
            .map(|v| v.syntax().clone()),
        // For list/tuple the element node *is* the value.
        ParentRef::List(_) | ParentRef::Tuple(_) => Some(target.clone()),
    }
    .ok_or(BlockedReason::InvalidPayload)?;

    let edit = EditOperation::replace(
        EditTarget::Node(value_node),
        value.to_string(),
        TriviaPolicy::KEEP_ALL,
    );
    apply_edit(doc, edit).map_err(|_| BlockedReason::TargetNotFound)
}

/// Reorder: move the child at `from` to position `to` within the parent, composed
/// as remove + re-insert producing one new CST (FR-003).
fn reorder_child(
    doc: &CstDocument,
    parent: &ParentRef,
    from: usize,
    to: usize,
) -> Result<CstDocument, BlockedReason> {
    let located = locate_parent(doc, parent).ok_or(BlockedReason::TargetNotFound)?;
    let elements = child_nodes(parent, &located);
    if from >= elements.len() || to >= elements.len() {
        return Err(BlockedReason::TargetNotFound);
    }
    if from == to {
        // No-op move: return an identical (re-rooted) document so the op is still
        // "applied" but byte-unchanged.
        return Ok(doc.clone());
    }

    // Capture the moved element's verbatim text (its own bytes, preserved).
    let moved_text = elements[from].text();

    // Step 1: remove the element at `from`.
    let after_remove = remove_child(doc, parent, from)?;

    // Step 2: re-resolve the parent against the new tree and insert the captured
    // text at the destination index (adjusted for the removal).
    let located2 = locate_parent(&after_remove, parent).ok_or(BlockedReason::TargetNotFound)?;
    let elements2 = child_nodes(parent, &located2);
    // Post-removal insertion index that lands the moved element at FINAL index
    // `to`. Removing `from` shifts every later element down by one, so inserting
    // at `to` (clamped to append) places it at final position `to` in both the
    // `from < to` and `from > to` directions.
    let dest = to.min(elements2.len());

    match parent {
        ParentRef::List(_) | ParentRef::Tuple(_) => insert_child_text(
            &after_remove,
            &located2,
            &elements2,
            dest,
            &moved_text,
            false,
        ),
        ParentRef::Struct(_) | ParentRef::Map(_) | ParentRef::EnumVariant(_) => {
            let is_map = matches!(parent, ParentRef::Map(_));
            insert_child_text(
                &after_remove,
                &located2,
                &elements2,
                dest,
                &moved_text,
                is_map,
            )
        }
    }
}

/// Swap an enum variant's name + field set in place (FR-003).
fn swap_enum_variant(
    doc: &CstDocument,
    variant: &SyntaxNode,
    new_name: &str,
    new_fields: &[String],
    placeholder: &str,
) -> Result<CstDocument, BlockedReason> {
    let located = locate_node(doc, variant).ok_or(BlockedReason::TargetNotFound)?;
    let variant_ast =
        ast::EnumVariant::cast(located.clone()).ok_or(BlockedReason::InvalidPayload)?;

    // Existing field names of the struct-like payload (entries parse as
    // `MapEntry`s whose key is a bare-ident value).
    let old_names: Vec<String> = variant_ast
        .entries()
        .filter_map(|e| map_entry_key_text(&e))
        .collect();

    // 1) Rename the variant name token.
    let name_tok = variant_ast.name().ok_or(BlockedReason::InvalidPayload)?;
    let mut current = apply_edit(
        doc,
        EditOperation::replace(
            EditTarget::TokenSpan {
                first: name_tok.clone(),
                last: name_tok,
            },
            new_name.to_string(),
            TriviaPolicy::KEEP_ALL,
        ),
    )
    .map_err(|_| BlockedReason::TargetNotFound)?;

    let parent_ref = ParentRef::EnumVariant(located.clone());

    // 2) Remove old-only fields (those not in `new_fields`). Remove from the end
    // backward so earlier indices stay valid across removals.
    let remove_names: Vec<String> = old_names
        .iter()
        .filter(|n| !new_fields.contains(n))
        .cloned()
        .collect();
    for name in remove_names.iter().rev() {
        // Re-resolve indices each iteration against the live tree.
        let located_now =
            locate_parent(&current, &parent_ref).ok_or(BlockedReason::TargetNotFound)?;
        let entries = child_nodes(&parent_ref, &located_now);
        if let Some(pos) = entries.iter().position(|e| {
            ast::MapEntry::cast(e.clone())
                .as_ref()
                .and_then(map_entry_key_text)
                .as_deref()
                == Some(name)
        }) {
            current = remove_child(&current, &parent_ref, pos)?;
        }
    }

    // 3) Add new-only fields with the placeholder value, in `new_fields` order.
    for name in new_fields {
        if old_names.contains(name) {
            continue; // shared field: keep existing value/bytes.
        }
        let located_now =
            locate_parent(&current, &parent_ref).ok_or(BlockedReason::TargetNotFound)?;
        let entries = child_nodes(&parent_ref, &located_now);
        let at = entries.len();
        current = insert_entry(&current, &parent_ref, at, name, placeholder)?;
    }

    Ok(current)
}

/// Add `name: value` to every struct element of `list`, batched into one
/// transform (ADR-0007 multi-node op).
fn add_field_across_rows(
    doc: &CstDocument,
    list: &SyntaxNode,
    name: &str,
    value: &str,
) -> Result<CstDocument, BlockedReason> {
    let located = locate_node(doc, list).ok_or(BlockedReason::TargetNotFound)?;
    if ast::List::cast(located.clone()).is_none() {
        return Err(BlockedReason::InvalidPayload);
    }

    // Number of struct rows to touch (captured up-front; we re-resolve each).
    let row_count = ast::List::cast(located.clone())
        .map(|l| {
            l.items()
                .filter(|v| matches!(v, ast::Value::Struct(_)))
                .count()
        })
        .unwrap_or(0);

    let mut current = doc.clone();
    for row_idx in 0..row_count {
        // Re-resolve the list against the live tree each iteration.
        let located_now = locate_node(&current, list).ok_or(BlockedReason::TargetNotFound)?;
        let list_ast = ast::List::cast(located_now.clone()).ok_or(BlockedReason::InvalidPayload)?;
        let struct_nodes: Vec<SyntaxNode> = list_ast
            .items()
            .filter_map(|v| match v {
                ast::Value::Struct(s) => Some(s.syntax().clone()),
                _ => None,
            })
            .collect();
        let Some(struct_node) = struct_nodes.get(row_idx) else {
            break;
        };
        let parent_ref = ParentRef::Struct(struct_node.clone());
        let entries = child_nodes(&parent_ref, struct_node);
        let at = entries.len();
        current = insert_entry(&current, &parent_ref, at, name, value)?;
    }

    Ok(current)
}
