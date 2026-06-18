//! Shared CST→projection derivation feeding the tree/form + table surfaces
//! (E008 Phase 1b — AD-003, FR-015/FR-019/FR-026).
//!
//! # One derivation, two surfaces (FR-015)
//!
//! The tree/form view (US1) and the table view (US2) both project the same CST.
//! This module is the single, shared derivation seam they grow from: it walks the
//! landed [`ParseResult`](crate::reparse::ParseResult)'s CST **once** and produces
//! a [`DerivedProjection`] describing the document's top-level value and its
//! immediate structure. The per-view models (`TreeFormModel`, `TableModel`) are
//! realized lazily on top of this in US1/US2 — only what a frame actually shows is
//! built (FR-026 lazy realization).
//!
//! # Off-frame + once per landed reparse (AD-003 / FR-026)
//!
//! Derivation runs **off the per-frame `update()` path**: the caller
//! ([`EditorDocument::poll_parse`](crate::document::EditorDocument::poll_parse))
//! invokes [`derive_projection`] exactly once when a current reparse result lands,
//! not per keystroke and not per frame. The cost is a single pass over the landed
//! CST per reparse; there is **no** new persistence path (it reuses the existing
//! off-frame reparse worker — NEW-WORKER).
//!
//! # Read-only — zero bytes (FR-020)
//!
//! Deriving a projection is a pure read over the CST. It changes **no** document
//! bytes; only an explicit structural edit does (FR-020).
//!
//! # Degrades safely over parse errors (FR-019)
//!
//! An unparseable / partially-invalid value classifies as
//! [`NodeKind::Error`] (a read-only error region) rather than a typed editable
//! node — well-formed nodes stay reachable, nothing is coerced, nothing panics
//! (FR-019).

use ronin_core::ast;
use ronin_core::{CstDocument, SyntaxKind, SyntaxNode};

use crate::structural::view_state::{path_of, PathStep, StructuralPath};

/// The structural classification of a value node — the shape both surfaces branch
/// on (a struct/map → form rows, a list → potential table, a leaf → inline editor).
///
/// `#[non_exhaustive]` so future RON shapes can be added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NodeKind {
    /// A named or anonymous struct `Name( field: v, .. )`.
    Struct,
    /// A map `{ k: v, .. }`.
    Map,
    /// A list / sequence `[ a, b, c ]` — the table-eligibility candidate (US2/US3).
    List,
    /// A positional tuple `( a, b, c )`.
    Tuple,
    /// An enum variant: bare `Ident` or `Ident(..)` / `Ident{..}` payload.
    EnumVariant,
    /// The unit value `()`.
    Unit,
    /// A scalar literal (int / float / string / char / bool) — an inline-editable
    /// leaf.
    Leaf,
    /// An unparseable / recovered region — read-only; surfaced, never coerced
    /// (FR-019).
    Error,
}

impl NodeKind {
    /// Classify a value-position [`SyntaxNode`] into a [`NodeKind`].
    ///
    /// Total over error-recovered trees: an `Error`-kind node maps to
    /// [`NodeKind::Error`] so derivation degrades safely (FR-019).
    #[must_use]
    pub fn of(node: &SyntaxNode) -> Self {
        match node.kind() {
            SyntaxKind::Struct => Self::Struct,
            SyntaxKind::Map => Self::Map,
            SyntaxKind::List => Self::List,
            SyntaxKind::Tuple => Self::Tuple,
            SyntaxKind::EnumVariant => Self::EnumVariant,
            SyntaxKind::Unit => Self::Unit,
            SyntaxKind::Literal => Self::Leaf,
            // Anything else (incl. SyntaxKind::Error and non-value nodes reached
            // defensively) degrades to a read-only error region (FR-019).
            _ => Self::Error,
        }
    }

    /// `true` for a collection node whose immediate children form rows/elements
    /// (struct / map / list / tuple / enum-variant payload).
    #[must_use]
    pub fn is_collection(self) -> bool {
        matches!(
            self,
            Self::Struct | Self::Map | Self::List | Self::Tuple | Self::EnumVariant
        )
    }
}

/// One lazily-derivable child of a projected collection node — enough for a tree
/// row or a table cell header, with the cross-reparse identity needed to edit it.
///
/// The child's *own* subtree is **not** realized here (lazy — FR-026): a tree
/// expands it on demand and a table realizes only visible rows. This carries the
/// label, the child's [`NodeKind`], and the [`PathStep`] that addresses it from
/// its parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildOutline {
    /// The display label: a struct field name, a map key's text, an element index
    /// as a string, or an enum-variant payload field name.
    pub label: String,
    /// The step addressing this child from its parent (the leaf of the child's
    /// [`StructuralPath`]).
    pub step: PathStep,
    /// The child value's structural classification.
    pub kind: NodeKind,
}

/// The shared, off-frame-derived structural projection of a document's top-level
/// value (data-model: the seam `TreeFormModel`/`TableModel` realize on top of).
///
/// It describes the **root** value's kind + path and the **immediate** outline of
/// its children (one level), realized lazily deeper by the per-view models. It is
/// a read projection (zero byte mutation, FR-020) re-derived once per landed
/// reparse (AD-003, FR-015/FR-026).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DerivedProjection {
    /// The top-level value's classification, or `None` for an empty / trivia-only
    /// document (no top-level value to project).
    pub root_kind: Option<NodeKind>,
    /// The structural path of the top-level value (the root path) — present
    /// whenever `root_kind` is `Some`.
    pub root_path: StructuralPath,
    /// The immediate children of the top-level value, in source order, as lazy
    /// outlines (empty for a leaf / unit / error root).
    pub root_children: Vec<ChildOutline>,
}

impl DerivedProjection {
    /// `true` when the document has a projectable top-level value.
    #[must_use]
    pub fn has_root(&self) -> bool {
        self.root_kind.is_some()
    }
}

/// Derive the shared structural projection of `cst`'s document (AD-003 / FR-015).
///
/// Runs **once per landed reparse** off the per-frame path: a single read pass over
/// the landed CST that classifies the top-level value and outlines its immediate
/// children (deeper levels are realized lazily by the per-view models — FR-026).
/// Read-only — changes zero bytes (FR-020); degrades safely over parse-error nodes
/// (FR-019). Returns an empty projection (no `root_kind`) for an empty /
/// trivia-only document.
#[must_use]
pub fn derive_projection(cst: &CstDocument) -> DerivedProjection {
    let root = cst.root();
    let Some(top) = ast::Document::cast(root)
        .and_then(|d| d.value())
        .map(|v| v.syntax().clone())
    else {
        return DerivedProjection::default();
    };

    let kind = NodeKind::of(&top);
    let children = outline_children(&top, kind);
    DerivedProjection {
        root_kind: Some(kind),
        root_path: StructuralPath::root(),
        root_children: children,
    }
}

/// Outline the immediate children of a collection node `node` of `kind`, in source
/// order (lazy: the children's own subtrees are not realized — FR-026).
///
/// A non-collection node (leaf / unit / error) has no children outline.
fn outline_children(node: &SyntaxNode, kind: NodeKind) -> Vec<ChildOutline> {
    match kind {
        NodeKind::Struct => ast::Struct::cast(node.clone())
            .map(|s| {
                s.fields()
                    .filter_map(|f| {
                        let name = f.name_text()?;
                        let value = f.value()?;
                        Some(ChildOutline {
                            label: name.clone(),
                            step: PathStep::Field(name),
                            kind: NodeKind::of(value.syntax()),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        NodeKind::Map => ast::Map::cast(node.clone())
            .map(|m| {
                m.entries()
                    .filter_map(|e| {
                        let key = e.key()?.syntax().text();
                        let value = e.value()?;
                        Some(ChildOutline {
                            label: key.clone(),
                            step: PathStep::Key(key),
                            kind: NodeKind::of(value.syntax()),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        NodeKind::EnumVariant => ast::EnumVariant::cast(node.clone())
            .map(|v| {
                v.entries()
                    .filter_map(|e| {
                        let key = e.key()?.syntax().text();
                        let value = e.value()?;
                        Some(ChildOutline {
                            label: key.clone(),
                            step: PathStep::VariantField(key),
                            kind: NodeKind::of(value.syntax()),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        NodeKind::List => ast::List::cast(node.clone())
            .map(|l| {
                l.items()
                    .enumerate()
                    .map(|(i, v)| ChildOutline {
                        label: i.to_string(),
                        step: PathStep::Index(i),
                        kind: NodeKind::of(v.syntax()),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        NodeKind::Tuple => ast::Tuple::cast(node.clone())
            .map(|t| {
                t.items()
                    .enumerate()
                    .map(|(i, v)| ChildOutline {
                        label: i.to_string(),
                        step: PathStep::Index(i),
                        kind: NodeKind::of(v.syntax()),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        // Leaf / unit / error: no children outline.
        NodeKind::Unit | NodeKind::Leaf | NodeKind::Error => Vec::new(),
    }
}

/// Capture the [`StructuralPath`] of `node` within `cst` (re-export of the
/// view-state inverse, scoped to a `CstDocument` for the projection caller).
///
/// Convenience for the document wiring (T011/T013): given a CST node currently
/// being edited/focused, derive its cross-reparse identity to store on the
/// view-state. Returns `None` when `node` is not a value-position node reachable
/// from the document's top-level value.
#[must_use]
pub fn capture_path(cst: &CstDocument, node: &SyntaxNode) -> Option<StructuralPath> {
    path_of(&cst.root(), node)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ronin_core::parse;

    #[test]
    fn empty_document_has_no_root() {
        let cst = parse("");
        let proj = derive_projection(&cst);
        assert!(!proj.has_root());
        assert!(proj.root_children.is_empty());
    }

    #[test]
    fn struct_root_outlines_fields_in_order() {
        let cst = parse("Point(x: 1, y: 2)");
        let proj = derive_projection(&cst);
        assert_eq!(proj.root_kind, Some(NodeKind::Struct));
        let labels: Vec<_> = proj.root_children.iter().map(|c| c.label.clone()).collect();
        assert_eq!(labels, vec!["x", "y"]);
        assert!(proj.root_children.iter().all(|c| c.kind == NodeKind::Leaf));
    }

    #[test]
    fn list_root_outlines_indices() {
        let cst = parse("[10, 20, 30]");
        let proj = derive_projection(&cst);
        assert_eq!(proj.root_kind, Some(NodeKind::List));
        let steps: Vec<_> = proj.root_children.iter().map(|c| c.step.clone()).collect();
        assert_eq!(
            steps,
            vec![PathStep::Index(0), PathStep::Index(1), PathStep::Index(2)]
        );
    }

    #[test]
    fn nested_child_kind_is_classified() {
        let cst = parse("Outer(items: [1, 2], name: \"n\")");
        let proj = derive_projection(&cst);
        let items = proj
            .root_children
            .iter()
            .find(|c| c.label == "items")
            .expect("items present");
        assert_eq!(items.kind, NodeKind::List);
    }

    #[test]
    fn error_region_degrades_safely() {
        // A stray top-level token recovers into an Error node; derivation does not
        // panic and classifies it as a read-only error region (FR-019).
        let cst = parse("@");
        let proj = derive_projection(&cst);
        assert_eq!(proj.root_kind, Some(NodeKind::Error));
        assert!(proj.root_children.is_empty());
    }

    #[test]
    fn capture_path_round_trips() {
        let cst = parse("Point(x: 1, y: 2)");
        let root = cst.root();
        // Resolve a known field value, capture its path, and confirm it resolves
        // back through view_state::resolve_path.
        let path = StructuralPath::from_steps(vec![PathStep::Field("y".to_string())]);
        let node = crate::structural::view_state::resolve_path(&root, &path).expect("y resolves");
        let captured = capture_path(&cst, &node).expect("path captured");
        assert_eq!(captured, path);
    }
}
