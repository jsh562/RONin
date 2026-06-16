//! The tree/form structural view: a navigable, expand/collapse projection of the
//! CST with inline value editing and discoverable add / remove / reorder / rename
//! / variant-swap ops (E008 Phase 2 / US1 — FR-001..FR-004/FR-018/FR-019/FR-022/
//! FR-023).
//!
//! # The model is a read projection (FR-001/FR-020)
//!
//! [`TreeFormModel`] is a transient, navigable projection of the document's CST:
//! one [`TreeNode`] per value, each carrying its [`StructuralPath`] node identity
//! ([`TreeNode::node_ref`]) so an edit can be re-resolved against the LIVE CST at
//! commit time (AD-004 / HINT-002). Building, expanding, and navigating the tree
//! change **zero** document bytes — only an explicit edit does (FR-020). It is
//! re-derived from the off-frame projection / CST, never the source of truth.
//!
//! # How an edit flows (FR-002/FR-003/FR-013/FR-014)
//!
//! The view never mutates the buffer directly. Each op resolves the target node's
//! [`StructuralPath`] against the live CST, derives a `ron-core`
//! [`StructuralOp`](ron_core::StructuralOp) — a [`ParentRef`](ron_core::ParentRef)
//! plus a child index — and calls
//! [`EditorDocument::apply_structural_edit`](crate::document::EditorDocument::apply_structural_edit),
//! which records ONE E007 undo unit, prints the new CST byte-losslessly, and
//! requests an off-frame reparse. A [`BlockedReason`](ron_core::BlockedReason)
//! (e.g. a rename collision) is surfaced inline with no byte change and no undo
//! entry (FR-003/FR-014). The path→op resolution lives here in `ronin-app`
//! (ADR-0007); the pure CST→CST transform lives in `ron-core`.
//!
//! # Inline editors, never a modal (FR-002)
//!
//! A leaf value edits in place with a type-appropriate widget — a text field for
//! strings / numbers / chars (and the fallback for any unmapped scalar editing its
//! literal RON token), a toggle for bools, a non-blocking selector for an enum
//! variant, an `Option` edited as its inner value with a `Some`/`None` selector,
//! and `()`/empty collections as non-editable structural leaves. None of these is
//! a blocking modal: edits commit on confirm (Enter / focus-leave) and discard on
//! cancel (Esc).
//!
//! # Diagnostics surface consistently with the text view (FR-018 / SC-008)
//!
//! Each node's CST byte range is matched against the document's
//! [`DiagnosticView`]s (the same E006 set the text view squiggles); an overlapping
//! finding is attached to the node and shown as an inline indicator with the same
//! severity + code, its detail revealed on focus/hover (FR-018).

use std::time::Instant;

use egui::{Key, RichText, Ui};

use ron_core::ast;
use ron_core::transform::{ParentRef, StructuralOp};
use ron_core::{BlockedReason, CstDocument, Severity, SyntaxNode};

use crate::byte_to_char::ByteCharIndex;
use crate::diagnostics_map::DiagnosticView;
use crate::document::EditorDocument;
use crate::reparse::ReparseWorker;
use crate::structural::view_state::{resolve_path, FocusSurface, PathStep, StructuralPath};

/// The structural classification a [`TreeNode`] projects (mirrors the CST shape).
///
/// `#[non_exhaustive]` so future RON shapes can be added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TreeNodeKind {
    /// A named or anonymous struct `Name( field: v, .. )`.
    Struct,
    /// A map `{ k: v, .. }`.
    Map,
    /// A list / sequence `[ a, b, c ]`.
    List,
    /// A positional tuple `( a, b, c )`.
    Tuple,
    /// An enum variant `Ident { .. }` / bare `Ident`.
    EnumVariant,
    /// A scalar / simple leaf value (int / float / string / char / bool / unit).
    Leaf,
    /// An unparseable / recovered region — read-only (FR-019).
    Error,
}

impl TreeNodeKind {
    /// Classify a value-position [`SyntaxNode`].
    #[must_use]
    fn of(node: &SyntaxNode) -> Self {
        match ast::Value::cast(node.clone()) {
            Some(ast::Value::Struct(_)) => Self::Struct,
            Some(ast::Value::Map(_)) => Self::Map,
            Some(ast::Value::List(_)) => Self::List,
            Some(ast::Value::Tuple(_)) => Self::Tuple,
            Some(ast::Value::EnumVariant(_)) => Self::EnumVariant,
            // Unit and any scalar literal are inline leaves.
            Some(ast::Value::Unit(_)) | Some(ast::Value::Literal(_)) => Self::Leaf,
            // Error-recovered or non-value node degrades to a read-only region.
            Some(ast::Value::Error(_)) | None => Self::Error,
        }
    }

    /// `true` for a collection whose immediate children form tree rows.
    #[must_use]
    fn is_collection(self) -> bool {
        matches!(
            self,
            Self::Struct | Self::Map | Self::List | Self::Tuple | Self::EnumVariant
        )
    }
}

/// How a [`TreeNode`] may be edited (data-model `editable`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TreeEditable {
    /// A scalar leaf — edited inline with a type-appropriate widget (FR-002).
    ScalarLeaf,
    /// A structural node — exposes add / remove / reorder / rename / variant ops
    /// (FR-003/FR-022) but has no inline value editor.
    Structural,
    /// A read-only error / unparseable region or a non-editable structural leaf
    /// (`()` / empty collection) — no edit affordance (FR-019).
    ReadOnly,
}

/// The inline-editor widget kind for a scalar leaf (FR-002).
///
/// The type→widget mapping is **total** over the scalar/simple value types: a text
/// field for strings / numbers / chars (and the fallback for any unmapped scalar),
/// a toggle for bools, a non-blocking selector for an enum variant, and an `Option`
/// edited as its inner value with a `Some`/`None` selector. The unit value `()` and
/// an empty struct/tuple stay non-editable structural leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LeafWidget {
    /// A free-text field editing the value's literal RON token (string / number /
    /// char / fallback for any unmapped scalar — FR-002).
    Text,
    /// A boolean toggle (`true` / `false`).
    Bool,
    /// A non-blocking selector over an enum variant's candidate names (FR-002): a
    /// bare variant (`Unit`, `None`, …) edits inline as a variant selector. A
    /// struct-like variant (`Variant { .. }`) carries the same selector in its
    /// collapsible header rather than a leaf row.
    Variant,
    /// An `Option` value (`Some(x)` / `None`): a `Some`/`None` selector plus the
    /// inner-value editor when `Some` (FR-002).
    Option,
}

/// The `Option` shape a value projects, for the inline `Some`/`None` editor (FR-002).
///
/// In RON an `Option` is either the bare variant `None` or `Some(inner)` (which
/// parses as a one-element tuple named `Some`). This captures which arm a value is
/// in plus the inner value's literal RON text when `Some`, so the editor can offer a
/// `Some`/`None` selector and an inner-value text field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionShape {
    /// The `None` arm (no inner value).
    None,
    /// The `Some(inner)` arm, carrying the inner value's verbatim RON text.
    Some(String),
}

/// One node of the tree/form projection (data-model `TreeNode`).
///
/// Carries enough to render a row and the [`StructuralPath`] node identity
/// ([`node_ref`](Self::node_ref)) to re-resolve and edit it against the live CST.
/// Children are realized eagerly within a derivation but expanded lazily in the
/// UI (FR-026): a collapsed node's subtree is built but only painted when
/// [`expanded`](Self::expanded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode {
    /// The construct this node projects.
    pub kind: TreeNodeKind,
    /// The display label: a struct field name, a map key's text, an element index
    /// as a string, an enum-variant name, or `value` for the root.
    pub label: String,
    /// A compact one-line preview of the value (display-only; no byte changes).
    pub value_summary: String,
    /// The child nodes, in source order (empty for a leaf).
    pub children: Vec<TreeNode>,
    /// The expansion state (default expanded for the root, collapsed deeper).
    pub expanded: bool,
    /// The cross-reparse identity of this node, used to resolve + edit it (FR-016).
    pub node_ref: StructuralPath,
    /// How this node may be edited.
    pub editable: TreeEditable,
    /// The inline-editor widget for a scalar leaf, or `None` for a non-leaf.
    pub leaf_widget: Option<LeafWidget>,
    /// For an enum-variant node: its current variant name (FR-002/FR-003). `None`
    /// for a non-variant node.
    pub variant_name: Option<String>,
    /// For an enum-variant node: the candidate variant names the inline selector
    /// offers (FR-002) — the value's own variant plus, where derivable, its
    /// sibling/peer variants in the same list. Always contains at least the value's
    /// own variant; empty for a non-variant node.
    pub variant_candidates: Vec<String>,
    /// For an `Option` value (`Some(x)` / `None`): its arm + inner text (FR-002).
    /// `None` for a non-`Option` node.
    pub option_shape: Option<OptionShape>,
    /// Inline diagnostics attached to this node by CST range (FR-018 / SC-008).
    pub diagnostics: Vec<DiagnosticView>,
}

impl TreeNode {
    /// `true` when this node exposes structural operations (FR-022): add / remove
    /// / reorder a child, rename a key, swap a variant — only for a collection.
    #[must_use]
    pub fn supports_structural_ops(&self) -> bool {
        self.kind.is_collection()
    }

    /// `true` when a rename op applies to this node's *key* — only a struct field
    /// or map entry has a renameable key (FR-022: never offer rename on a list
    /// index). A node is renameable when its own last path step is a field/key.
    #[must_use]
    pub fn supports_rename(&self) -> bool {
        matches!(
            self.node_ref.steps().last(),
            Some(PathStep::Field(_) | PathStep::Key(_) | PathStep::VariantField(_))
        )
    }

    /// `true` when a variant-swap applies (only an enum-variant node — FR-022).
    #[must_use]
    pub fn supports_variant_swap(&self) -> bool {
        self.kind == TreeNodeKind::EnumVariant
    }
}

/// The tree/form view model: the document's top-level value(s) projected as a tree
/// (data-model `TreeFormModel`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TreeFormModel {
    /// The document's top-level value(s) as tree roots (one root for a single
    /// top-level value; empty for an empty / trivia-only document).
    pub roots: Vec<TreeNode>,
}

impl TreeFormModel {
    /// Derive the tree/form model from a document's CST + its diagnostics (FR-001).
    ///
    /// A pure read over the CST (zero bytes, FR-020). Each node's CST range is
    /// matched against `diagnostics` so a field/cell with a finding carries an
    /// inline indicator consistent with the text view (FR-018 / SC-008). Degrades
    /// safely over error-recovered trees: an unparseable value classifies as
    /// [`TreeNodeKind::Error`] (read-only) rather than crashing (FR-019).
    #[must_use]
    pub fn derive(cst: &CstDocument, diagnostics: &[DiagnosticView]) -> Self {
        let root = cst.root();
        // Map node byte ranges → char ranges for diagnostic attachment in a single
        // amortised-O(n) forward pass: build a byte→char index over every token
        // boundary (a superset of all node boundaries, since each node's range is the
        // union of its tokens) instead of an O(file-size) `chars().count()` per node
        // (the O(nodes × file_size) cost that froze the view). Skip the index entirely
        // when there are no diagnostics — the only consumer of the char mapping.
        let index = build_byte_char_index(&root, diagnostics);
        let Some(top) = ast::Document::cast(root)
            .and_then(|d| d.value())
            .map(|v| v.syntax().clone())
        else {
            return Self::default();
        };
        let node = build_node(
            &top,
            "value",
            StructuralPath::root(),
            true,
            diagnostics,
            &index,
            &[],
        );
        Self { roots: vec![node] }
    }

    /// Find the node addressed by `path`, if present (depth-bounded lookup).
    #[must_use]
    pub fn node_at(&self, path: &StructuralPath) -> Option<&TreeNode> {
        for root in &self.roots {
            if let Some(found) = node_at_in(root, path) {
                return Some(found);
            }
        }
        None
    }
}

/// Recursively search `node`'s subtree for the node whose `node_ref == path`.
fn node_at_in<'a>(node: &'a TreeNode, path: &StructuralPath) -> Option<&'a TreeNode> {
    if &node.node_ref == path {
        return Some(node);
    }
    for child in &node.children {
        if let Some(found) = node_at_in(child, path) {
            return Some(found);
        }
    }
    None
}

/// Build a [`TreeNode`] for the value `node` addressed by `path`, with `label`.
///
/// `expanded` seeds the node's initial expansion (the root expands by default;
/// deeper nodes collapse). Children are realized in source order; a leaf / unit /
/// error node has none. `peer_variants` is the candidate variant-name set derived
/// from the node's sibling list elements (empty when there are none); it seeds the
/// inline variant selector's candidate list (FR-002).
fn build_node(
    node: &SyntaxNode,
    label: &str,
    path: StructuralPath,
    expanded: bool,
    diagnostics: &[DiagnosticView],
    index: &ByteCharIndex,
    peer_variants: &[String],
) -> TreeNode {
    let kind = TreeNodeKind::of(node);
    let diags = diagnostics_for(node, diagnostics, index);
    let value_summary = summarize(node);
    let option_shape = option_shape_of(node);
    let (editable, leaf_widget) = classify_editable(node, kind, option_shape.as_ref());
    let (variant_name, variant_candidates) = variant_info_of(node, kind, peer_variants);

    let children = if kind.is_collection() {
        build_children(node, kind, &path, diagnostics, index)
    } else {
        Vec::new()
    };

    TreeNode {
        kind,
        label: label.to_string(),
        value_summary,
        children,
        expanded,
        node_ref: path,
        editable,
        leaf_widget,
        variant_name,
        variant_candidates,
        option_shape,
        diagnostics: diags,
    }
}

/// Build the child tree nodes of a collection `node`, in source order.
fn build_children(
    node: &SyntaxNode,
    kind: TreeNodeKind,
    path: &StructuralPath,
    diagnostics: &[DiagnosticView],
    index: &ByteCharIndex,
) -> Vec<TreeNode> {
    match kind {
        TreeNodeKind::Struct => ast::Struct::cast(node.clone())
            .map(|s| {
                s.fields()
                    .filter_map(|f| {
                        let name = f.name_text()?;
                        let value = f.value()?;
                        let child_path = path.child(PathStep::Field(name.clone()));
                        Some(build_node(
                            value.syntax(),
                            &name,
                            child_path,
                            false,
                            diagnostics,
                            index,
                            &[],
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        TreeNodeKind::Map => ast::Map::cast(node.clone())
            .map(|m| {
                m.entries()
                    .filter_map(|e| {
                        let key = e.key()?.syntax().text();
                        let value = e.value()?;
                        let child_path = path.child(PathStep::Key(key.clone()));
                        Some(build_node(
                            value.syntax(),
                            &key,
                            child_path,
                            false,
                            diagnostics,
                            index,
                            &[],
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        TreeNodeKind::EnumVariant => ast::EnumVariant::cast(node.clone())
            .map(|v| {
                v.entries()
                    .filter_map(|e| {
                        let key = e.key()?.syntax().text();
                        let value = e.value()?;
                        let child_path = path.child(PathStep::VariantField(key.clone()));
                        Some(build_node(
                            value.syntax(),
                            &key,
                            child_path,
                            false,
                            diagnostics,
                            index,
                            &[],
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        TreeNodeKind::List => ast::List::cast(node.clone())
            .map(|l| {
                // The peer variant-name set for this list's elements seeds each
                // element's inline variant selector (FR-002): the union of every
                // sibling element's own variant name, in first-seen order.
                let peers = peer_variant_names(l.items());
                l.items()
                    .enumerate()
                    .map(|(i, v)| {
                        let child_path = path.child(PathStep::Index(i));
                        build_node(
                            v.syntax(),
                            &i.to_string(),
                            child_path,
                            false,
                            diagnostics,
                            index,
                            &peers,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default(),
        TreeNodeKind::Tuple => ast::Tuple::cast(node.clone())
            .map(|t| {
                t.items()
                    .enumerate()
                    .map(|(i, v)| {
                        let child_path = path.child(PathStep::Index(i));
                        build_node(
                            v.syntax(),
                            &i.to_string(),
                            child_path,
                            false,
                            diagnostics,
                            index,
                            &[],
                        )
                    })
                    .collect()
            })
            .unwrap_or_default(),
        TreeNodeKind::Leaf | TreeNodeKind::Error => Vec::new(),
    }
}

/// The union of the variant names of a list's elements, in first-seen order, for
/// the inline variant selector's candidate set (FR-002). Only enum-variant
/// elements contribute a name; non-variant elements are skipped.
fn peer_variant_names(items: impl Iterator<Item = ast::Value>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for item in items {
        if let ast::Value::EnumVariant(v) = &item {
            if let Some(name) = v.name_text() {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
    }
    names
}

/// The variant name + candidate selector set for a node (FR-002/FR-003).
///
/// Only an [`TreeNodeKind::EnumVariant`] node carries variant info. The candidate
/// set is the value's own variant name plus any `peer_variants` (sibling/peer
/// variants in the same list) that are not already present; it always contains at
/// least the value's own variant so the selector can render even with no peers. A
/// non-variant node returns `(None, vec![])`.
fn variant_info_of(
    node: &SyntaxNode,
    kind: TreeNodeKind,
    peer_variants: &[String],
) -> (Option<String>, Vec<String>) {
    if kind != TreeNodeKind::EnumVariant {
        return (None, Vec::new());
    }
    let own = ast::EnumVariant::cast(node.clone()).and_then(|v| v.name_text());
    let Some(own_name) = own else {
        return (None, Vec::new());
    };
    let mut candidates = vec![own_name.clone()];
    for peer in peer_variants {
        if !candidates.contains(peer) {
            candidates.push(peer.clone());
        }
    }
    (Some(own_name), candidates)
}

/// The `Option` shape a value node projects (FR-002), or `None` when it is not an
/// `Option`.
///
/// In RON, `None` is a bare enum variant named `None`, and `Some(inner)` parses as a
/// one-element tuple named `Some`. Anything else is not an `Option`.
fn option_shape_of(node: &SyntaxNode) -> Option<OptionShape> {
    match ast::Value::cast(node.clone())? {
        ast::Value::EnumVariant(v) if v.name_text().as_deref() == Some("None") => {
            Some(OptionShape::None)
        }
        ast::Value::Tuple(t) => {
            // `Some(inner)`: a named tuple `Some` with one positional item.
            let name = t
                .syntax()
                .first_token_of(ron_core::SyntaxKind::Ident)
                .map(|tok| tok.text().to_string());
            if name.as_deref() == Some("Some") {
                let inner = t.items().next().map(|v| v.syntax().text())?;
                Some(OptionShape::Some(inner))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Classify the node's editability + the leaf-editor widget (FR-002 / FR-019).
///
/// `option_shape` is the node's pre-computed [`OptionShape`] (an `Option` value
/// edits inline as a `Some`/`None` selector regardless of whether it parses as a
/// bare variant or a `Some(..)` tuple, FR-002).
fn classify_editable(
    node: &SyntaxNode,
    kind: TreeNodeKind,
    option_shape: Option<&OptionShape>,
) -> (TreeEditable, Option<LeafWidget>) {
    // An `Option` is always editable inline with the Some/None selector (FR-002),
    // taking precedence over the structural classification of its `None` variant /
    // `Some(..)` tuple shape.
    if option_shape.is_some() {
        return (TreeEditable::ScalarLeaf, Some(LeafWidget::Option));
    }
    match kind {
        TreeNodeKind::Error => (TreeEditable::ReadOnly, None),
        // A bare enum variant (no payload fields) edits inline as a variant selector
        // (FR-002); a struct-like variant keeps its collapsible structural row but
        // also exposes the selector in its header (rendered separately).
        TreeNodeKind::EnumVariant => {
            if ast::EnumVariant::cast(node.clone())
                .map(|v| v.entries().next().is_none())
                .unwrap_or(false)
            {
                (TreeEditable::ScalarLeaf, Some(LeafWidget::Variant))
            } else {
                (TreeEditable::Structural, None)
            }
        }
        TreeNodeKind::Struct | TreeNodeKind::Map | TreeNodeKind::List | TreeNodeKind::Tuple => {
            (TreeEditable::Structural, None)
        }
        TreeNodeKind::Leaf => match ast::Value::cast(node.clone()) {
            // The unit value `()` is a non-editable structural leaf (FR-002).
            Some(ast::Value::Unit(_)) => (TreeEditable::ReadOnly, None),
            Some(ast::Value::Literal(lit)) => {
                // Bool → toggle; everything else (strings / numbers / chars /
                // unmapped scalars) → the text-field fallback (FR-002 totality).
                let widget = match lit.token_kind() {
                    Some(ron_core::SyntaxKind::TrueKw | ron_core::SyntaxKind::FalseKw) => {
                        LeafWidget::Bool
                    }
                    _ => LeafWidget::Text,
                };
                (TreeEditable::ScalarLeaf, Some(widget))
            }
            // Defensive: anything classified Leaf but not a literal/unit falls back
            // to the text editor over its token rather than being left unreachable.
            _ => (TreeEditable::ScalarLeaf, Some(LeafWidget::Text)),
        },
    }
}

/// A compact one-line preview of a value node (display-only; never normalized for
/// editing — this is only the collapsed-row summary).
///
/// Builds the preview from the node's descendant tokens, collapsing internal
/// whitespace runs to single spaces and **stopping early** once enough characters
/// have accumulated to decide truncation. This avoids materializing the node's
/// entire subtree text per node (`node.text()` is O(subtree-size)) — which, summed
/// over every node in a large nested tree, was an O(n²) cost in `derive`.
fn summarize(node: &SyntaxNode) -> String {
    /// Maximum preview length before eliding.
    const MAX: usize = 48;

    let mut out = String::new();
    let mut chars = 0usize; // number of chars pushed to `out`
    let mut pending_space = false; // a whitespace run seen but not yet emitted
    let mut truncated = false;

    'outer: for token in node.descendant_tokens() {
        for ch in token.text().chars() {
            if ch.is_whitespace() {
                // Defer a single collapsed space; never leads (chars == 0) and is
                // dropped if it would trail (no following non-whitespace emitted).
                pending_space = chars > 0;
                continue;
            }
            if pending_space {
                out.push(' ');
                chars += 1;
                pending_space = false;
            }
            if chars == MAX {
                // We already have MAX chars and another non-whitespace char follows:
                // the preview is longer than MAX, so elide.
                truncated = true;
                break 'outer;
            }
            out.push(ch);
            chars += 1;
        }
    }

    if truncated {
        out.push('\u{2026}');
    }
    out
}

/// Collect the diagnostics whose char range overlaps `node`'s char range (FR-018).
///
/// `ron-core` ranges are byte ranges; the [`DiagnosticView`] carries char ranges,
/// so we compare against the node's char extent (computed from its byte range over
/// the document text). Overlap (not containment) is used so a finding that lands on
/// the field name or spans the value still attaches to the node.
fn diagnostics_for(
    node: &SyntaxNode,
    diagnostics: &[DiagnosticView],
    index: &ByteCharIndex,
) -> Vec<DiagnosticView> {
    if diagnostics.is_empty() {
        return Vec::new();
    }
    // The node's byte range; map to a char range via the precomputed byte→char
    // index (amortised O(1) per query) — the diagnostics carry char ranges, FR-018.
    let range = node.text_range();
    let node_start = index.char_at(range.start());
    let node_end = index.char_at(range.end());

    diagnostics
        .iter()
        .filter(|d| ranges_overlap(d.char_range, (node_start, node_end)))
        .cloned()
        .collect()
}

/// `true` when half-open char ranges `[a0,a1)` and `[b0,b1)` overlap.
fn ranges_overlap(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// Build a byte→char index covering every node boundary in `root`, for the
/// diagnostic-attachment char mapping (FR-018), in a single forward pass.
///
/// Token boundaries are a superset of node boundaries (each node's range is the
/// union of its descendant tokens), so registering every token's start/end resolves
/// any node's `(start, end)` exactly. Returns an empty index when there are no
/// diagnostics, since the char mapping is then never queried — the index is only
/// consulted by [`diagnostics_for`], which short-circuits on an empty set.
fn build_byte_char_index(root: &SyntaxNode, diagnostics: &[DiagnosticView]) -> ByteCharIndex {
    let source = root.text();
    if diagnostics.is_empty() {
        return ByteCharIndex::build(&source, std::iter::empty());
    }
    let offsets = root.descendant_tokens().flat_map(|t| {
        let range = t.text_range();
        [range.start(), range.end()]
    });
    ByteCharIndex::build(&source, offsets)
}

// =============================================================================
// Path → StructuralOp resolution (ADR-0007 ronin-app side)
// =============================================================================

/// Resolve a value node's [`StructuralPath`] to its enclosing parent collection as
/// a [`ParentRef`] plus the child's 0-based index, against the live `cst`.
///
/// The node's parent is the path with its last step dropped; the child index is
/// derived from that last step (a field/key by name → its index among the parent's
/// entries; a list/tuple index → itself). Returns `None` when the path no longer
/// resolves (the node vanished — FR-016) or addresses the root (no parent).
#[must_use]
fn resolve_parent_and_index(
    cst: &CstDocument,
    path: &StructuralPath,
) -> Option<(ParentRef, usize)> {
    let root = cst.root();
    let steps = path.steps();
    let (last, parent_steps) = steps.split_last()?;
    let parent_path = StructuralPath::from_steps(parent_steps.to_vec());
    let parent_node = resolve_path(&root, &parent_path)?;
    let parent_ref = parent_ref_of(&parent_node)?;
    let index = index_of_step(&parent_ref, &parent_node, last)?;
    Some((parent_ref, index))
}

/// Wrap a resolved collection [`SyntaxNode`] in the matching [`ParentRef`].
#[must_use]
fn parent_ref_of(node: &SyntaxNode) -> Option<ParentRef> {
    Some(match ast::Value::cast(node.clone())? {
        ast::Value::Struct(_) => ParentRef::Struct(node.clone()),
        ast::Value::Map(_) => ParentRef::Map(node.clone()),
        ast::Value::List(_) => ParentRef::List(node.clone()),
        ast::Value::Tuple(_) => ParentRef::Tuple(node.clone()),
        ast::Value::EnumVariant(_) => ParentRef::EnumVariant(node.clone()),
        _ => return None,
    })
}

/// The 0-based index a [`PathStep`] addresses within a resolved parent collection.
#[must_use]
fn index_of_step(parent: &ParentRef, node: &SyntaxNode, step: &PathStep) -> Option<usize> {
    match (parent, step) {
        (ParentRef::Struct(_), PathStep::Field(name)) => ast::Struct::cast(node.clone())?
            .fields()
            .position(|f| f.name_text().as_deref() == Some(name.as_str())),
        (ParentRef::Map(_), PathStep::Key(text)) => ast::Map::cast(node.clone())?
            .entries()
            .position(|e| e.key().map(|k| k.syntax().text()).as_deref() == Some(text.as_str())),
        (ParentRef::EnumVariant(_), PathStep::VariantField(name)) => {
            ast::EnumVariant::cast(node.clone())?
                .entries()
                .position(|e| e.key().map(|k| k.syntax().text()).as_deref() == Some(name.as_str()))
        }
        (ParentRef::List(_), PathStep::Index(i)) => {
            (*i < ast::List::cast(node.clone())?.items().count()).then_some(*i)
        }
        (ParentRef::Tuple(_), PathStep::Index(i)) => {
            (*i < ast::Tuple::cast(node.clone())?.items().count()).then_some(*i)
        }
        _ => None,
    }
}

// =============================================================================
// Document-side op entry points (the path→op→apply_structural_edit pipeline)
// =============================================================================

impl EditorDocument {
    /// Resolve the parent + index for `path` against the live buffer, or
    /// [`BlockedReason::TargetNotFound`] when it no longer resolves (FR-016).
    fn tree_resolve_parent(
        &self,
        path: &StructuralPath,
    ) -> Result<(ParentRef, usize), BlockedReason> {
        let cst = ron_core::parse(&self.buffer);
        resolve_parent_and_index(&cst, path).ok_or(BlockedReason::TargetNotFound)
    }

    /// Set the value of the leaf at `path` to `value` (literal RON text) as one
    /// undo unit (FR-002 / SC-001 / SC-002).
    pub fn apply_tree_set_value(
        &mut self,
        path: &StructuralPath,
        value: String,
        worker: &ReparseWorker,
        now: Instant,
    ) -> Result<(), BlockedReason> {
        let (parent, index) = self.tree_resolve_parent(path)?;
        self.apply_structural_edit(
            StructuralOp::SetValue {
                parent,
                index,
                value,
            },
            worker,
            now,
        )
    }

    /// Insert a `name: value` field/entry into the struct/map/variant addressed by
    /// `parent_path` at `index` (clamped), as one undo unit (FR-003).
    pub fn apply_tree_insert_field(
        &mut self,
        parent_path: &StructuralPath,
        index: usize,
        name: String,
        value: String,
        worker: &ReparseWorker,
        now: Instant,
    ) -> Result<(), BlockedReason> {
        let cst = ron_core::parse(&self.buffer);
        let parent_node =
            resolve_path(&cst.root(), parent_path).ok_or(BlockedReason::TargetNotFound)?;
        let parent = parent_ref_of(&parent_node).ok_or(BlockedReason::InvalidPayload)?;
        self.apply_structural_edit(
            StructuralOp::InsertField {
                parent,
                index,
                name,
                value,
            },
            worker,
            now,
        )
    }

    /// Insert a list/tuple element into the collection addressed by `parent_path`
    /// at `index` (clamped), adopting sibling style, as one undo unit (FR-003/007).
    pub fn apply_tree_insert_element(
        &mut self,
        parent_path: &StructuralPath,
        index: usize,
        value: String,
        worker: &ReparseWorker,
        now: Instant,
    ) -> Result<(), BlockedReason> {
        let cst = ron_core::parse(&self.buffer);
        let parent_node =
            resolve_path(&cst.root(), parent_path).ok_or(BlockedReason::TargetNotFound)?;
        let parent = parent_ref_of(&parent_node).ok_or(BlockedReason::InvalidPayload)?;
        self.apply_structural_edit(
            StructuralOp::InsertElement {
                parent,
                index,
                value,
            },
            worker,
            now,
        )
    }

    /// Remove the field/element addressed by `path` as one undo unit (FR-003).
    pub fn apply_tree_remove(
        &mut self,
        path: &StructuralPath,
        worker: &ReparseWorker,
        now: Instant,
    ) -> Result<(), BlockedReason> {
        let (parent, index) = self.tree_resolve_parent(path)?;
        // RemoveField / RemoveElement share the same removal path in ron-core;
        // pick the op matching the parent so the addressing is type-correct.
        let op = match parent {
            ParentRef::List(_) | ParentRef::Tuple(_) => {
                StructuralOp::RemoveElement { parent, index }
            }
            _ => StructuralOp::RemoveField { parent, index },
        };
        self.apply_structural_edit(op, worker, now)
    }

    /// Reorder a child of the collection addressed by `parent_path` from `from` to
    /// `to`, as one undo unit (FR-003 / FR-023).
    pub fn apply_tree_reorder(
        &mut self,
        parent_path: &StructuralPath,
        from: usize,
        to: usize,
        worker: &ReparseWorker,
        now: Instant,
    ) -> Result<(), BlockedReason> {
        let cst = ron_core::parse(&self.buffer);
        let parent_node =
            resolve_path(&cst.root(), parent_path).ok_or(BlockedReason::TargetNotFound)?;
        let parent = parent_ref_of(&parent_node).ok_or(BlockedReason::InvalidPayload)?;
        self.apply_structural_edit(StructuralOp::ReorderChild { parent, from, to }, worker, now)
    }

    /// Rename the key of the field/entry addressed by `path` to `new_name`, as one
    /// undo unit; a same-parent collision returns
    /// [`BlockedReason::RenameCollision`] with no byte change and no undo entry
    /// (FR-003).
    pub fn apply_tree_rename(
        &mut self,
        path: &StructuralPath,
        new_name: String,
        worker: &ReparseWorker,
        now: Instant,
    ) -> Result<(), BlockedReason> {
        let (parent, index) = self.tree_resolve_parent(path)?;
        self.apply_structural_edit(
            StructuralOp::RenameKey {
                parent,
                index,
                new_name,
            },
            worker,
            now,
        )
    }

    /// Swap the enum variant addressed by `path` to `new_name` with `new_fields`,
    /// adding any new-only field with `placeholder`, as one undo unit (FR-003).
    pub fn apply_tree_swap_variant(
        &mut self,
        path: &StructuralPath,
        new_name: String,
        new_fields: Vec<String>,
        placeholder: String,
        worker: &ReparseWorker,
        now: Instant,
    ) -> Result<(), BlockedReason> {
        let cst = ron_core::parse(&self.buffer);
        let variant = resolve_path(&cst.root(), path).ok_or(BlockedReason::TargetNotFound)?;
        if ast::EnumVariant::cast(variant.clone()).is_none() {
            return Err(BlockedReason::InvalidPayload);
        }
        self.apply_structural_edit(
            StructuralOp::SwapEnumVariant {
                variant,
                new_name,
                new_fields,
                placeholder,
            },
            worker,
            now,
        )
    }
}

// =============================================================================
// Rendering (egui) — inline editors, per-node ops, keyboard nav (FR-002/004/022/023)
// =============================================================================

/// A deferred, one-per-frame structural action the render pass collected from the
/// UI; applied after the immutable tree walk so the borrow of `doc` is clean.
enum PendingAction {
    /// Commit an inline leaf edit: set the value at `path` to the draft text.
    SetValue { path: StructuralPath, value: String },
    /// Insert a placeholder field/element into the collection at `parent`.
    InsertChild {
        parent: StructuralPath,
        kind: TreeNodeKind,
    },
    /// Remove the field/element at `path`.
    Remove { path: StructuralPath },
    /// Reorder a child of `parent` from `from` to `to`.
    Reorder {
        parent: StructuralPath,
        from: usize,
        to: usize,
    },
    /// Rename the field/key at `path` to `new_name` (FR-003/FR-022). A collision is
    /// surfaced inline with no byte change and no undo entry.
    Rename {
        path: StructuralPath,
        new_name: String,
    },
    /// Swap the enum variant at `path` to `new_name`, keeping shared fields
    /// (FR-002/FR-003/FR-022). The new field set is inferred from the node's current
    /// fields so a struct-like variant keeps its existing payload by name.
    SwapVariant {
        path: StructuralPath,
        new_name: String,
        new_fields: Vec<String>,
    },
}

/// Render the tree/form view for `doc`, driving inline edits + structural ops
/// through the one-undo-unit pipeline (E008 / US1 — FR-001/002/003/004/022/023).
///
/// The render walks the live [`TreeFormModel`] (derived from the document's CST +
/// diagnostics) and paints each node as a collapsible row with an inline value
/// editor for leaves and discoverable per-node op controls for collections. User
/// actions are collected as [`PendingAction`]s during the walk and applied after
/// it, so each op is one [`EditorDocument::apply_structural_edit`] undo unit. A
/// blocked op (e.g. a rename collision) is surfaced inline via the per-document
/// error notice without changing bytes or pushing an undo entry (FR-003/FR-014).
///
/// # Headless-rendering boundary (E003)
///
/// The node rows + labels paint through the renderer-free egui_kittest harness
/// (asserted in `tree_form_view.rs`); the buffer-mutating edits are covered
/// headlessly via the document-side `apply_tree_*` API (T017).
pub fn render_tree_view(ui: &mut Ui, doc: &mut EditorDocument, worker: &ReparseWorker) {
    // The owning document's id, captured up front (Copy) so it can be mixed into
    // each collapsible node's egui Id without re-borrowing `doc` during the walk —
    // this is what keeps collapse state distinct across open files (Fix 3).
    let doc_id = doc.id();

    // Stale marker (FR-015): a user-perceivable notice while a reparse is pending.
    if doc.view_state().is_stale() {
        ui.weak("(updating\u{2026})");
    }

    // Reuse the per-parse cached model (derived once per parse generation), instead
    // of re-deriving from the CST every render frame (zero bytes, FR-020). The clone
    // is a cheap structural copy taken so the borrow on `doc` is released before the
    // mutable view-state writes later in this function.
    let Some(model) = doc.cached_tree_model().cloned() else {
        ui.weak("Parsing\u{2026}");
        return;
    };
    if model.roots.is_empty() {
        ui.weak("(empty document)");
        return;
    }

    // The discoverable drill-in **back** control (FR-006): when this tree/form view
    // was reached by a table-cell drill-in, render a control that restores the table
    // view + re-focuses the originating row/cell. Rendered above the tree so it is
    // always discoverable. Byte-free (FR-020).
    let mut go_back = false;
    if doc.view_state().drill_in_return().is_some() {
        ui.horizontal(|ui| {
            if ui
                .button("\u{2190} Back to table")
                .on_hover_text("Return to the table and re-focus the originating cell")
                .clicked()
            {
                go_back = true;
            }
        });
    }
    if go_back {
        return_to_table(doc);
        return;
    }

    // Expand-All / Collapse-All (FR-026): a small button row above the tree. On
    // click we walk the *cached* model (never re-derived — FR-020) and set every
    // collection node's egui CollapsingState open/closed using the same
    // `collapse_id`, so the next frame's renderer reads the updated state. Mutating
    // egui memory here is byte-free and borrows nothing on `doc` (the model is a
    // clone), keeping the deferred-edit borrow discipline below intact.
    ui.horizontal(|ui| {
        if ui
            .small_button("Expand all")
            .on_hover_text("Expand every collapsible node")
            .clicked()
        {
            for root in &model.roots {
                set_subtree_open(ui.ctx(), doc_id, root, true);
            }
        }
        if ui
            .small_button("Collapse all")
            .on_hover_text("Collapse every collapsible node")
            .clicked()
        {
            for root in &model.roots {
                set_subtree_open(ui.ctx(), doc_id, root, false);
            }
        }
    });

    // The current draft for an in-progress leaf edit (carried on the view-state).
    let mut draft: Option<(StructuralPath, String)> = doc
        .view_state()
        .edit_focus()
        .map(|f| (f.path.clone(), f.draft.clone()));
    // The current in-progress rename draft, distinct from a value edit (FR-003).
    let mut rename_draft: Option<(StructuralPath, String)> =
        doc.view_state().rename_draft().cloned();

    let mut pending: Option<PendingAction> = None;
    let mut new_focus: Option<(StructuralPath, String)> = None;
    let mut clear_focus = false;
    // A request to begin (or cancel) a rename, collected during the walk.
    let mut begin_rename: Option<(StructuralPath, String)> = None;
    let mut clear_rename = false;

    let mut ctx = RenderCtx {
        doc_id,
        draft: &mut draft,
        rename_draft: &mut rename_draft,
        pending: &mut pending,
        new_focus: &mut new_focus,
        clear_focus: &mut clear_focus,
        begin_rename: &mut begin_rename,
        clear_rename: &mut clear_rename,
    };

    for root in &model.roots {
        render_node(ui, root, None, &mut ctx);
    }

    // Apply rename-draft changes (byte-free — FR-020).
    if clear_rename {
        doc.view_state_mut().clear_rename_draft();
    } else if let Some((path, text)) = begin_rename {
        doc.view_state_mut().set_rename_draft(path, text);
    } else if let Some((path, text)) = rename_draft {
        doc.view_state_mut().update_rename_draft(&path, text);
    }

    // Apply view-state focus changes (byte-free — FR-020).
    if clear_focus {
        doc.view_state_mut().clear_focus();
    } else if let Some((path, text)) = new_focus {
        doc.view_state_mut()
            .set_focus(path, FocusSurface::TreeNode, text);
    } else if let Some((path, text)) = draft {
        // Keep the live draft updated as the user types (carried across reparse).
        if let Some(focus) = doc.view_state_mut().edit_focus_mut() {
            if focus.path == path {
                focus.draft = text;
            }
        }
    }

    // Apply at most one structural op this frame, as one undo unit. A blocked op is
    // surfaced as an inline error notice (FR-003) without changing bytes.
    if let Some(action) = pending {
        let now = Instant::now();
        let result = match action {
            PendingAction::SetValue { path, value } => {
                doc.view_state_mut().clear_focus();
                doc.apply_tree_set_value(&path, value, worker, now)
            }
            PendingAction::InsertChild { parent, kind } => match kind {
                TreeNodeKind::List | TreeNodeKind::Tuple => {
                    doc.apply_tree_insert_element(&parent, usize::MAX, "0".to_string(), worker, now)
                }
                _ => doc.apply_tree_insert_field(
                    &parent,
                    usize::MAX,
                    "field".to_string(),
                    "0".to_string(),
                    worker,
                    now,
                ),
            },
            PendingAction::Remove { path } => doc.apply_tree_remove(&path, worker, now),
            PendingAction::Reorder { parent, from, to } => {
                doc.apply_tree_reorder(&parent, from, to, worker, now)
            }
            PendingAction::Rename { path, new_name } => {
                // A successful rename closes the rename draft; a collision keeps it
                // open so the user can correct the name (FR-003).
                let res = doc.apply_tree_rename(&path, new_name, worker, now);
                if res.is_ok() {
                    doc.view_state_mut().clear_rename_draft();
                }
                res
            }
            PendingAction::SwapVariant {
                path,
                new_name,
                new_fields,
            } => doc.apply_tree_swap_variant(
                &path,
                new_name,
                new_fields,
                "0".to_string(),
                worker,
                now,
            ),
        };
        if let Err(reason) = result {
            doc.set_tree_error(blocked_message(reason));
        } else {
            doc.clear_tree_error();
        }
    }

    // Surface the last inline error (e.g. a rename collision) consistently with the
    // diagnostics indicator model (FR-003).
    if let Some(msg) = doc.tree_error() {
        ui.colored_label(error_color(ui), msg);
    }
}

/// The mutable render-pass scratch state threaded through the immutable tree walk.
///
/// Collects the user's actions (a value/rename draft, one structural [`PendingAction`],
/// focus changes) during the walk so they apply *after* it, keeping the borrow of
/// `doc` clean and each op one undo unit (FR-014).
struct RenderCtx<'a> {
    /// The owning document's process-unique id, mixed into each node's collapse
    /// [`egui::Id`] so identical structural paths in different open files never
    /// share collapse state ([`collapse_id`]).
    doc_id: u64,
    /// The in-progress value-edit draft `(path, text)`, if any.
    draft: &'a mut Option<(StructuralPath, String)>,
    /// The in-progress rename draft `(path, text)`, distinct from a value edit.
    rename_draft: &'a mut Option<(StructuralPath, String)>,
    /// At most one structural op collected this frame.
    pending: &'a mut Option<PendingAction>,
    /// A request to begin editing a leaf value, keyed to its path + seed text.
    new_focus: &'a mut Option<(StructuralPath, String)>,
    /// A request to clear the active value-edit focus (cancel).
    clear_focus: &'a mut bool,
    /// A request to begin a rename, keyed to the node path + seed (current) name.
    begin_rename: &'a mut Option<(StructuralPath, String)>,
    /// A request to cancel the active rename.
    clear_rename: &'a mut bool,
}

/// The sibling context a child row needs to offer reorder / remove affordances
/// (FR-022/FR-023): which collection it belongs to, its index, and the count.
#[derive(Clone)]
struct SiblingCtx {
    /// The parent collection's structural path (the reorder/remove parent).
    parent: StructuralPath,
    /// This child's 0-based index among its siblings.
    index: usize,
    /// The total sibling count (so the last child hides "move down").
    count: usize,
}

/// Restore the [`ActiveView::Table`] view and re-focus the originating cell of a
/// drill-in (FR-006). A no-op if no drill-in return target is recorded.
fn return_to_table(doc: &mut EditorDocument) {
    let Some(ret) = doc.view_state().drill_in_return().cloned() else {
        return;
    };
    doc.view_state_mut().clear_drill_in_return();
    doc.view_state_mut().set_focus(
        ret.cell_path,
        FocusSurface::TableCell {
            row: ret.row,
            column: ret.column,
        },
        String::new(),
    );
    doc.view_state_mut()
        .set_active_view(crate::structural::view_state::ActiveView::Table);
}

/// The persistent egui [`Id`](egui::Id) for a collection node's [`CollapsingState`].
///
/// Keyed by the owning document's id plus the node's full [`StructuralPath`]
/// ([`TreeNode::node_ref`]) — which is itself `Hash` — so the id is:
///
/// * **path-keyed / nesting-independent**: two distinct collection nodes that
///   happen to share depth + label live at different paths and so get distinct ids
///   (no cross-subtree collapse-state collision);
/// * **persistent across reparse**: the same source re-derives the same path, so a
///   node's expand/collapse state survives an off-frame reparse (FR-016);
/// * **per-document distinct**: identical paths in two open files differ by `doc_id`.
///
/// Pure (no `ui`/`ctx`), so it is unit-testable without an egui Harness.
#[must_use]
pub fn collapse_id(doc_id: u64, node: &TreeNode) -> egui::Id {
    egui::Id::new(("ronin_tree", doc_id, &node.node_ref))
}

/// `true` when `node` renders as a collapsible collection header (and so owns a
/// [`CollapsingState`](egui::collapsing_header::CollapsingState) keyed by
/// [`collapse_id`]) — the exact condition the renderer uses at the show site.
///
/// An `Option` value and a *bare* enum variant render as leaf rows (no collapsing
/// header) even though their kind is a collection, so they are excluded here too.
///
/// `pub` so the Expand/Collapse-All tests can assert exactly which nodes own a
/// [`CollapsingState`](egui::collapsing_header::CollapsingState) keyed by
/// [`collapse_id`].
#[must_use]
pub fn is_collapsible_collection(node: &TreeNode) -> bool {
    let is_option_leaf = node.option_shape.is_some();
    let is_bare_variant =
        node.kind == TreeNodeKind::EnumVariant && node.children.is_empty() && !is_option_leaf;
    node.kind.is_collection() && !is_option_leaf && !is_bare_variant
}

/// Set `node` and its entire subtree's collapsing state to `open` in egui memory.
///
/// Used by Expand-All / Collapse-All: it walks the cached model and, for every node
/// that renders as a collapsible collection ([`is_collapsible_collection`]), loads
/// the [`CollapsingState`](egui::collapsing_header::CollapsingState) under the same
/// [`collapse_id`], sets it open/closed, and stores it back — so the next render
/// frame reads the updated state. Leaf rows have no collapsing state and are
/// skipped (but their would-be children, if any, are still recursed into).
///
/// `pub` so the Expand/Collapse-All tests can drive the same walk the header
/// buttons run and then read each node's stored state back via `collapse_id`.
pub fn set_subtree_open(ctx: &egui::Context, doc_id: u64, node: &TreeNode, open: bool) {
    if is_collapsible_collection(node) {
        let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
            ctx,
            collapse_id(doc_id, node),
            node.expanded,
        );
        state.set_open(open);
        state.store(ctx);
    }
    for child in &node.children {
        set_subtree_open(ctx, doc_id, child, open);
    }
}

/// Render one tree node row + (recursively) its expanded children. `sibling` is
/// `Some` for a child within a collection (it then offers reorder/remove controls),
/// `None` for the root.
fn render_node(ui: &mut Ui, node: &TreeNode, sibling: Option<&SiblingCtx>, ctx: &mut RenderCtx) {
    let header = node_header(node);
    // An Option value (`Some(x)` / `None`) edits inline as a Some/None selector
    // regardless of its underlying variant/tuple shape (FR-002) — render it as a
    // leaf-style row, never a collapsible collection. A *bare* enum variant (no
    // payload fields) likewise renders as a leaf row carrying its inline variant
    // selector, rather than an empty collapsible header. `is_collapsible_collection`
    // encodes exactly this condition (shared with the Expand/Collapse-All walk).
    if is_collapsible_collection(node) {
        // A collapsible collection: header + per-node op controls + children.
        // The collapse Id is keyed by the node's full structural path (+ the doc id),
        // so it is nesting-independent and never collides between unrelated subtrees
        // that happen to share depth+label, and it persists across reparse (Fix 3).
        let id = collapse_id(ctx.doc_id, node);
        egui::collapsing_header::CollapsingState::load_with_default_open(
            ui.ctx(),
            id,
            node.expanded,
        )
        .show_header(ui, |ui| {
            ui.label(header);
            render_node_diagnostics(ui, node);
            // The inline variant selector (FR-002) for a struct-like enum variant
            // sits in its header so the user can swap the variant in place.
            render_variant_selector(ui, node, ctx.pending);
            render_node_ops(ui, node, ctx.pending);
            render_rename_control(ui, node, sibling, ctx);
            render_sibling_controls(ui, node, sibling, ctx.pending);
        })
        .body(|ui| {
            let count = node.children.len();
            for (i, child) in node.children.iter().enumerate() {
                let sib = SiblingCtx {
                    parent: node.node_ref.clone(),
                    index: i,
                    count,
                };
                render_node(ui, child, Some(&sib), ctx);
            }
        });
    } else {
        // A leaf row: label + inline editor (or read-only summary) + sibling ops.
        ui.horizontal(|ui| {
            ui.label(format!("{}:", node.label));
            render_leaf_editor(ui, node, ctx);
            render_node_diagnostics(ui, node);
            render_rename_control(ui, node, sibling, ctx);
            render_sibling_controls(ui, node, sibling, ctx.pending);
        });
    }
}

/// Render the discoverable **rename** control for a node whose key is renameable
/// (FR-003/FR-022): a struct field or map/variant entry, never a list index. When a
/// rename is open it renders an inline text field that commits on Enter (a collision
/// is surfaced inline, no undo entry — FR-003) and cancels on Esc; otherwise a small
/// "rename" button (a keyboard-reachable affordance) begins one.
fn render_rename_control(
    ui: &mut Ui,
    node: &TreeNode,
    sibling: Option<&SiblingCtx>,
    ctx: &mut RenderCtx,
) {
    // Rename is scoped to a renameable key, and only a child node (a field/entry)
    // can be renamed (the root has no key) — FR-022 never offers it on a list index.
    if sibling.is_none() || !node.supports_rename() {
        return;
    }
    let editing = ctx
        .rename_draft
        .as_ref()
        .is_some_and(|(p, _)| p == &node.node_ref);
    if editing {
        let mut text = ctx
            .rename_draft
            .as_ref()
            .map(|(_, t)| t.clone())
            .unwrap_or_default();
        let resp = ui.text_edit_singleline(&mut text);
        *ctx.rename_draft = Some((node.node_ref.clone(), text.clone()));
        if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            *ctx.pending = Some(PendingAction::Rename {
                path: node.node_ref.clone(),
                new_name: text,
            });
        } else if ui.input(|i| i.key_pressed(Key::Escape)) {
            *ctx.clear_rename = true;
        }
    } else if ui
        .small_button("rename")
        .on_hover_text("Rename this field/key")
        .clicked()
    {
        *ctx.begin_rename = Some((node.node_ref.clone(), node.label.clone()));
    }
}

/// Render the inline **enum-variant selector** (FR-002/FR-003/FR-022) for a variant
/// node: a non-blocking dropdown over the node's candidate variant names plus a
/// free-text field to type any other variant identifier (so a variant with no
/// derivable peers can still be changed — the task's "at minimum allow editing the
/// variant identifier" fallback). Picking/typing a different name swaps the variant
/// in place, keeping shared payload fields by name (the new field set = the node's
/// current payload fields). A no-op for a non-variant node.
fn render_variant_selector(ui: &mut Ui, node: &TreeNode, pending: &mut Option<PendingAction>) {
    if !node.supports_variant_swap() {
        return;
    }
    let Some(current) = node.variant_name.clone() else {
        return;
    };
    // The variant's current payload field names are preserved on a swap (shared
    // fields keep their value/bytes; FR-003).
    let payload_fields: Vec<String> = node.children.iter().map(|c| c.label.clone()).collect();
    let mut selected = current.clone();
    // A transient buffer for the free-text "other variant name" field, held in egui
    // memory keyed by the node's path so typing persists while the popup is open.
    let custom_id = egui::Id::new((
        "ronin_variant_custom",
        node.node_ref.steps().len(),
        &node.label,
    ));
    let mut commit_custom: Option<String> = None;
    egui::ComboBox::from_id_salt(("ronin_variant", node.node_ref.steps().len(), &node.label))
        .selected_text(format!("variant: {current}"))
        .show_ui(ui, |ui| {
            for cand in &node.variant_candidates {
                ui.selectable_value(&mut selected, cand.clone(), cand.clone());
            }
            ui.separator();
            // Free-text: type any other variant identifier (FR-002 fallback).
            let mut custom: String = ui
                .data(|d| d.get_temp::<String>(custom_id))
                .unwrap_or_default();
            let resp =
                ui.add(egui::TextEdit::singleline(&mut custom).hint_text("other variant\u{2026}"));
            if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && !custom.is_empty() {
                commit_custom = Some(custom.clone());
            }
            ui.data_mut(|d| d.insert_temp(custom_id, custom));
        });
    let new_name = commit_custom.or_else(|| (selected != current).then_some(selected));
    if let Some(name) = new_name {
        if name != current {
            *pending = Some(PendingAction::SwapVariant {
                path: node.node_ref.clone(),
                new_name: name,
                new_fields: payload_fields,
            });
        }
    }
}

/// Render a child row's reorder + remove affordances (FR-022/FR-023): move-up /
/// move-down (only when a neighbour exists) and remove, scoped to this one node.
/// A no-op for the root (no `sibling` context).
fn render_sibling_controls(
    ui: &mut Ui,
    node: &TreeNode,
    sibling: Option<&SiblingCtx>,
    pending: &mut Option<PendingAction>,
) {
    let Some(ctx) = sibling else {
        return;
    };
    // Move up (FR-023): swap with the previous sibling.
    if ctx.index > 0
        && ui
            .small_button("\u{2191}")
            .on_hover_text("Move up")
            .clicked()
    {
        *pending = Some(PendingAction::Reorder {
            parent: ctx.parent.clone(),
            from: ctx.index,
            to: ctx.index - 1,
        });
    }
    // Move down (FR-023): swap with the next sibling.
    if ctx.index + 1 < ctx.count
        && ui
            .small_button("\u{2193}")
            .on_hover_text("Move down")
            .clicked()
    {
        *pending = Some(PendingAction::Reorder {
            parent: ctx.parent.clone(),
            from: ctx.index,
            to: ctx.index + 1,
        });
    }
    // Remove this field/element (FR-022).
    if ui
        .small_button("\u{2716}")
        .on_hover_text("Remove")
        .clicked()
    {
        *pending = Some(PendingAction::Remove {
            path: node.node_ref.clone(),
        });
    }
}

/// The collapsed-row header text for a collection node.
fn node_header(node: &TreeNode) -> String {
    let kind = match node.kind {
        TreeNodeKind::Struct => "struct",
        TreeNodeKind::Map => "map",
        TreeNodeKind::List => "list",
        TreeNodeKind::Tuple => "tuple",
        TreeNodeKind::EnumVariant => "enum",
        TreeNodeKind::Leaf => "leaf",
        TreeNodeKind::Error => "error",
    };
    format!("{} [{kind}] {}", node.label, node.value_summary)
}

/// Render the inline leaf editor for a node (FR-002), or a read-only summary.
fn render_leaf_editor(ui: &mut Ui, node: &TreeNode, ctx: &mut RenderCtx) {
    match (node.editable, node.leaf_widget) {
        (TreeEditable::ReadOnly, _) => {
            // `()` / empty / error: a non-editable structural leaf (FR-002/019).
            ui.weak(&node.value_summary);
        }
        (TreeEditable::ScalarLeaf, Some(LeafWidget::Bool)) => {
            // A bool toggle: flipping commits immediately (a one-shot edit).
            let mut on = node.value_summary == "true";
            if ui.checkbox(&mut on, "").changed() {
                *ctx.pending = Some(PendingAction::SetValue {
                    path: node.node_ref.clone(),
                    value: if on {
                        "true".to_string()
                    } else {
                        "false".to_string()
                    },
                });
            }
        }
        (TreeEditable::ScalarLeaf, Some(LeafWidget::Variant)) => {
            // A bare enum variant (`Unit`, `None`, …): the non-blocking variant
            // selector edits it in place (FR-002).
            render_variant_selector(ui, node, ctx.pending);
        }
        (TreeEditable::ScalarLeaf, Some(LeafWidget::Option)) => {
            render_option_editor(ui, node, ctx);
        }
        (TreeEditable::Structural, _) => {
            // Unreachable for a leaf row (only non-collection nodes reach here), but
            // handled defensively: a structural node shows its summary, never a leaf
            // editor.
            ui.weak(&node.value_summary);
        }
        (TreeEditable::ScalarLeaf, _) => {
            // A text field over the literal token. The draft is held on the view
            // state so it survives a reparse; commit on Enter / focus-leave,
            // discard on Esc (FR-002, no modal).
            let editing = ctx.draft.as_ref().is_some_and(|(p, _)| p == &node.node_ref);
            if editing {
                let mut text = ctx
                    .draft
                    .as_ref()
                    .map(|(_, t)| t.clone())
                    .unwrap_or_default();
                let resp = ui.text_edit_singleline(&mut text);
                *ctx.draft = Some((node.node_ref.clone(), text.clone()));
                if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    *ctx.pending = Some(PendingAction::SetValue {
                        path: node.node_ref.clone(),
                        value: text,
                    });
                } else if ui.input(|i| i.key_pressed(Key::Escape)) {
                    *ctx.clear_focus = true;
                }
            } else {
                // Click the value summary to begin editing.
                if ui.button(&node.value_summary).clicked() {
                    *ctx.new_focus = Some((node.node_ref.clone(), leaf_draft_text_for(node)));
                }
            }
        }
    }
}

/// Render the inline **Option editor** (FR-002): a `Some`/`None` selector plus, when
/// `Some`, an inner-value text field. Switching to `None` sets the value to `None`;
/// switching to `Some` wraps the current (or a placeholder) inner value as `Some(..)`;
/// editing the inner value commits `Some(<inner>)`. Each commit is one undo unit via
/// [`PendingAction::SetValue`] (replacing the whole Option value losslessly).
fn render_option_editor(ui: &mut Ui, node: &TreeNode, ctx: &mut RenderCtx) {
    let Some(shape) = node.option_shape.clone() else {
        return;
    };
    let is_some = matches!(shape, OptionShape::Some(_));
    let inner = match &shape {
        OptionShape::Some(t) => t.clone(),
        OptionShape::None => String::new(),
    };

    // The Some/None selector (a non-blocking dropdown, FR-002).
    let mut selected_some = is_some;
    let label = if is_some { "Some" } else { "None" };
    egui::ComboBox::from_id_salt(("ronin_option", node.node_ref.steps().len(), &node.label))
        .selected_text(label)
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut selected_some, true, "Some");
            ui.selectable_value(&mut selected_some, false, "None");
        });
    if selected_some != is_some {
        // Toggle the arm: None → Some(<placeholder>); Some → None.
        let value = if selected_some {
            let seed = if inner.is_empty() {
                "0"
            } else {
                inner.as_str()
            };
            format!("Some({seed})")
        } else {
            "None".to_string()
        };
        *ctx.pending = Some(PendingAction::SetValue {
            path: node.node_ref.clone(),
            value,
        });
        return;
    }

    // When `Some`, an inner-value editor commits `Some(<inner>)` (FR-002).
    if is_some {
        let editing = ctx.draft.as_ref().is_some_and(|(p, _)| p == &node.node_ref);
        if editing {
            let mut text = ctx
                .draft
                .as_ref()
                .map(|(_, t)| t.clone())
                .unwrap_or_default();
            let resp = ui.text_edit_singleline(&mut text);
            *ctx.draft = Some((node.node_ref.clone(), text.clone()));
            if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                *ctx.pending = Some(PendingAction::SetValue {
                    path: node.node_ref.clone(),
                    value: format!("Some({text})"),
                });
            } else if ui.input(|i| i.key_pressed(Key::Escape)) {
                *ctx.clear_focus = true;
            }
        } else if ui
            .button(&inner)
            .on_hover_text("Edit the Some(..) value")
            .clicked()
        {
            *ctx.new_focus = Some((node.node_ref.clone(), inner));
        }
    }
}

/// The seed draft text for a leaf — its current summary (the literal token).
fn leaf_draft_text_for(node: &TreeNode) -> String {
    node.value_summary.clone()
}

/// Render the per-node structural-op controls (FR-022/FR-023): only the ops that
/// apply to this node's kind, plus reorder affordances for an element/field.
fn render_node_ops(ui: &mut Ui, node: &TreeNode, pending: &mut Option<PendingAction>) {
    if !node.supports_structural_ops() {
        return;
    }
    // Add a child (field for struct/map/variant; element for list/tuple).
    let add_label = match node.kind {
        TreeNodeKind::List | TreeNodeKind::Tuple => "+ element",
        _ => "+ field",
    };
    if ui.small_button(add_label).clicked() {
        *pending = Some(PendingAction::InsertChild {
            parent: node.node_ref.clone(),
            kind: node.kind,
        });
    }
}

/// Render a node's inline diagnostic indicator (FR-018 / SC-008).
///
/// The indicator carries the same severity colour as the text view's squiggle and
/// reveals the detail (code / severity / message) on hover, consistent with the
/// text view — no view downgrades or omits a finding the others show.
fn render_node_diagnostics(ui: &mut Ui, node: &TreeNode) {
    for diag in &node.diagnostics {
        let color = severity_color(ui, diag.severity);
        let glyph = match diag.severity {
            Severity::Error => "\u{2716}",   // heavy multiplication x
            Severity::Warning => "\u{26A0}", // warning sign
        };
        ui.label(RichText::new(glyph).color(color))
            .on_hover_text(format!(
                "{} [{}]: {}",
                severity_word(diag.severity),
                diag.code.code(),
                diag.message
            ));
    }
}

/// The severity word for a diagnostic detail string.
fn severity_word(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
    }
}

/// The theme-aware indicator colour for a severity (matches the text view).
fn severity_color(ui: &Ui, severity: Severity) -> egui::Color32 {
    let dark = ui.visuals().dark_mode;
    match severity {
        Severity::Error => {
            if dark {
                egui::Color32::from_rgb(0xF4, 0x47, 0x47)
            } else {
                egui::Color32::from_rgb(0xCD, 0x31, 0x31)
            }
        }
        Severity::Warning => {
            if dark {
                egui::Color32::from_rgb(0xCC, 0xA7, 0x00)
            } else {
                egui::Color32::from_rgb(0xBF, 0x83, 0x03)
            }
        }
    }
}

/// The inline-error text colour (re-uses the error severity colour for FR-003).
fn error_color(ui: &Ui) -> egui::Color32 {
    severity_color(ui, Severity::Error)
}

/// A user-facing message for a blocked op (FR-003 inline error).
fn blocked_message(reason: BlockedReason) -> String {
    match reason {
        BlockedReason::RenameCollision => {
            "Rename blocked: a field/key with that name already exists here".to_string()
        }
        BlockedReason::TargetNotFound => {
            "Edit could not be applied: the target node no longer exists".to_string()
        }
        BlockedReason::InvalidPayload => {
            "Edit could not be applied: invalid value or operation".to_string()
        }
        // `BlockedReason` is `#[non_exhaustive]`: a future reason is surfaced
        // generically rather than panicking.
        _ => "Edit was blocked".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ron_core::parse;

    fn model_of(src: &str) -> TreeFormModel {
        TreeFormModel::derive(&parse(src), &[])
    }

    #[test]
    fn struct_projects_one_node_per_field() {
        let m = model_of("Point(x: 1, y: 2)");
        assert_eq!(m.roots.len(), 1);
        let root = &m.roots[0];
        assert_eq!(root.kind, TreeNodeKind::Struct);
        let labels: Vec<_> = root.children.iter().map(|c| c.label.clone()).collect();
        assert_eq!(labels, vec!["x", "y"]);
        assert_eq!(root.children[0].editable, TreeEditable::ScalarLeaf);
    }

    #[test]
    fn bool_leaf_uses_toggle_widget() {
        let m = model_of("(flag: true)");
        let flag = &m.roots[0].children[0];
        assert_eq!(flag.leaf_widget, Some(LeafWidget::Bool));
    }

    #[test]
    fn unit_value_is_read_only_leaf() {
        let m = model_of("(u: ())");
        let u = &m.roots[0].children[0];
        assert_eq!(u.kind, TreeNodeKind::Leaf);
        assert_eq!(u.editable, TreeEditable::ReadOnly);
    }

    #[test]
    fn error_region_is_read_only() {
        let m = model_of("@");
        assert_eq!(m.roots[0].kind, TreeNodeKind::Error);
        assert_eq!(m.roots[0].editable, TreeEditable::ReadOnly);
    }

    #[test]
    fn rename_supported_only_for_keys() {
        let m = model_of("Foo(x: 1)");
        let x = &m.roots[0].children[0];
        assert!(x.supports_rename());

        let list = model_of("[1, 2]");
        let elem = &list.roots[0].children[0];
        assert!(!elem.supports_rename(), "a list index is not renameable");
    }

    #[test]
    fn resolve_parent_and_index_finds_field() {
        let cst = parse("Foo(x: 1, y: 2)");
        let path = StructuralPath::from_steps(vec![PathStep::Field("y".to_string())]);
        let (parent, index) = resolve_parent_and_index(&cst, &path).expect("resolves");
        assert!(matches!(parent, ParentRef::Struct(_)));
        assert_eq!(index, 1);
    }

    #[test]
    fn resolve_parent_and_index_finds_list_element() {
        let cst = parse("[10, 20, 30]");
        let path = StructuralPath::from_steps(vec![PathStep::Index(2)]);
        let (parent, index) = resolve_parent_and_index(&cst, &path).expect("resolves");
        assert!(matches!(parent, ParentRef::List(_)));
        assert_eq!(index, 2);
    }

    #[test]
    fn node_at_finds_deep_node() {
        let m = model_of("Outer(items: [1, 2])");
        let path = StructuralPath::from_steps(vec![
            PathStep::Field("items".to_string()),
            PathStep::Index(1),
        ]);
        let node = m.node_at(&path).expect("deep node present");
        assert_eq!(node.label, "1");
    }

    /// The (early-stopping) `summarize` must produce the same preview the original
    /// whitespace-collapse + truncate-to-48 algorithm did, so the only change is the
    /// avoided O(subtree-size)-per-node `node.text()` cost.
    #[test]
    fn summarize_matches_naive_collapse_and_truncate() {
        /// The original algorithm `summarize` replaced (kept here as the oracle).
        fn naive(text: &str) -> String {
            const MAX: usize = 48;
            let compact: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if compact.chars().count() > MAX {
                let truncated: String = compact.chars().take(MAX).collect();
                format!("{truncated}\u{2026}")
            } else {
                compact
            }
        }

        let cases = [
            "true",
            "()",
            "Point(x: 1, y: 2)",
            "[10, 20, 30]",
            // Long, multi-line, deeply-nested → exercises truncation + whitespace runs.
            "Outer(\n    items: [\n        Inner( a: 1, b: 2, c: 3 ),\n        Inner( a: 4, b: 5, c: 6 ),\n    ],\n)",
            // Exactly-at and just-over the 48-char boundary after collapse.
            "\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"", // 48 chars
            "\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"", // 49 chars
        ];
        for src in cases {
            let node = ast::Document::cast(parse(src).root())
                .and_then(|d| d.value())
                .map(|v| v.syntax().clone())
                .unwrap_or_else(|| panic!("parse {src:?}"));
            assert_eq!(
                summarize(&node),
                naive(&node.text()),
                "summarize mismatch for {src:?}"
            );
        }
    }
}
