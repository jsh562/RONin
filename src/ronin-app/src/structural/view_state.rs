//! Structural-path node identity + per-document view selection / edit focus
//! (E008 Phase 1b — AD-004, FR-016/FR-017/FR-027).
//!
//! # Why a structural *path*, not a `SyntaxNode` handle (AD-004 / HINT-002)
//!
//! A reparse builds a **new** green tree, so a raw [`SyntaxNode`] handle held
//! across an off-frame reparse points at the *old* tree and is useless for
//! re-binding focus or expansion. `ronin-core`'s [`ParentRef`](ronin_core::ParentRef)
//! addresses by `(kind + start-offset)` and is stable only *within* a single
//! composed transform (where intermediate trees keep their start offsets) — it is
//! **not** a cross-reparse identity either, because edits shift offsets.
//!
//! This module's [`StructuralPath`] is the cross-reparse identity: an ordered list
//! of [`PathStep`]s from the document root to a node, each step naming a child by
//! its **structural role** (a struct field / map key by name, a list/tuple element
//! by index, an enum-variant payload field by name) rather than by a byte offset
//! or a tree handle. The path re-resolves against *any* CST of the same shape, so
//! after a reparse the same logical node is found again ([`resolve_path`]); if the
//! node vanished (a conflicting text edit deleted it) resolution returns `None`
//! and the caller drops edit mode gracefully (FR-016).
//!
//! # Cost is proportional to path depth (FR-027 / SC-011)
//!
//! [`resolve_path`] walks **one step per path element**, descending only into the
//! child the step names. It performs **no** full-tree scan and never re-walks the
//! whole document: the work at each step is bounded by that one parent's direct
//! children, and the number of steps is the node's structural-path depth. So the
//! rebind cost is proportional to path depth, independent of the section's row
//! count or the document's total node count (FR-027). [`path_of`] is the inverse:
//! it walks **up** via [`SyntaxNode::parent`], one hop per level, also depth-bounded.

use ronin_core::ast;
use ronin_core::{SyntaxKind, SyntaxNode};

/// One step along a [`StructuralPath`]: how to descend from a parent collection to
/// one of its children by the child's **structural role** (not by offset or handle).
///
/// `#[non_exhaustive]` so future container kinds (e.g. a richer enum addressing)
/// can be added without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PathStep {
    /// Descend into a struct field by its field **name** (bare identifier).
    Field(String),
    /// Descend into a map entry by its key's verbatim RON **text** (covers
    /// non-string keys, e.g. `1` or `'c'`).
    Key(String),
    /// Descend into a list/tuple element by its 0-based **index** in source order.
    Index(usize),
    /// Descend into an enum-variant's struct-like payload field by its field
    /// **name**. The variant selector itself is identified by the field's enclosing
    /// variant; this step addresses one payload entry of that variant.
    VariantField(String),
    /// A **synthetic** trailing step used ONLY by the Table view's "combined" /
    /// flattened projection: on a parent collection (map/list of records), it selects
    /// the union of the named child collection across **every** entry (e.g.
    /// `hulls ▸ CombinedChild("cells")` = all hulls' `cells` rows in one table). It
    /// does **not** resolve to a single live node ([`resolve_path`] returns `None`
    /// for it); the combined table is built by `TableModel::derive_combined` from the
    /// parent prefix + this field name. Never produced by [`path_of`].
    CombinedChild(String),
}

/// A stable, cross-reparse identity for a CST node: the ordered steps from the
/// document root to that node (AD-004).
///
/// Two CSTs of the same structural shape resolve a given path to the
/// corresponding node in each tree — so a path captured before a reparse
/// re-binds the same logical node after it ([`resolve_path`]). An empty path
/// addresses the document's top-level value.
///
/// The identity is **value-based** (names + indices), so it is `Clone` + `Eq` +
/// `Hash` and may be stored on the per-document [`ViewSelectionAndFocus`] across
/// frames without holding any tree reference alive.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct StructuralPath {
    /// The steps from the root to the node, in descend order.
    steps: Vec<PathStep>,
}

impl StructuralPath {
    /// The root path (addresses the document's top-level value).
    #[must_use]
    pub fn root() -> Self {
        Self { steps: Vec::new() }
    }

    /// Build a path from an explicit ordered step list.
    #[must_use]
    pub fn from_steps(steps: Vec<PathStep>) -> Self {
        Self { steps }
    }

    /// The ordered steps, root-to-node.
    #[must_use]
    pub fn steps(&self) -> &[PathStep] {
        &self.steps
    }

    /// The path's depth (number of steps); the cost bound of a re-resolution
    /// (FR-027).
    #[must_use]
    pub fn depth(&self) -> usize {
        self.steps.len()
    }

    /// `true` for the root path (no steps).
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.steps.is_empty()
    }

    /// Return a new path with `step` appended (child of `self`).
    #[must_use]
    pub fn child(&self, step: PathStep) -> Self {
        let mut steps = self.steps.clone();
        steps.push(step);
        Self { steps }
    }
}

/// Resolve a [`StructuralPath`] against `cst_root` to the live [`SyntaxNode`] it
/// names, or `None` if it no longer resolves (the node vanished — FR-016).
///
/// `cst_root` is the document's [`SyntaxKind::Root`] node (e.g.
/// `parse_result.cst.root()`). Resolution descends **one step at a time**,
/// looking only at the current node's direct children for the child the step
/// names; it performs no full-tree scan, so its cost is proportional to the
/// path's depth, not the document's size (FR-027 / SC-011).
///
/// The root path resolves to the document's top-level value. A step that does not
/// match the current node's shape (wrong kind, a missing field/key, an
/// out-of-range index) returns `None` rather than guessing — never resolve to the
/// *wrong* node (FR-016).
#[must_use]
pub fn resolve_path(cst_root: &SyntaxNode, path: &StructuralPath) -> Option<SyntaxNode> {
    // Start at the document's single top-level value (skip the Root wrapper).
    let mut current = top_level_value(cst_root)?;
    for step in &path.steps {
        current = descend(&current, step)?;
    }
    Some(current)
}

/// Resolve `path` against `cst_root` like [`resolve_path`], additionally returning a
/// **node-visit count**: the number of direct child entries/elements
/// [`resolve_path`] examined while descending (the test seam behind SC-011 / T051).
///
/// The visit count is the structural cost of a focus/expansion rebind. Because each
/// step looks only at the named child's enclosing parent's direct children, and stops
/// at the matched child, the count is bounded by the path's depth + the addressed
/// indices/positions — it is **independent of the section's total row count** (FR-027
/// / SC-011): resolving the same fixed-depth, fixed-position path in a 1k-sibling and
/// a 100k-sibling document visits the **same** number of nodes.
///
/// Returns `(resolved_node, visit_count)`; the node is `None` when the path no longer
/// resolves (the visit count still reflects the work done up to the failed step).
#[must_use]
pub fn resolve_path_visiting(
    cst_root: &SyntaxNode,
    path: &StructuralPath,
) -> (Option<SyntaxNode>, usize) {
    let mut visits = 0usize;
    let Some(mut current) = top_level_value(cst_root) else {
        return (None, visits);
    };
    for step in &path.steps {
        match descend_counting(&current, step, &mut visits) {
            Some(next) => current = next,
            None => return (None, visits),
        }
    }
    (Some(current), visits)
}

/// The document's single top-level value node, skipping the `Root` wrapper and any
/// leading extension attributes / trivia.
fn top_level_value(cst_root: &SyntaxNode) -> Option<SyntaxNode> {
    if cst_root.kind() == SyntaxKind::Root {
        ast::Document::cast(cst_root.clone())
            .and_then(|d| d.value())
            .map(|v| v.syntax().clone())
    } else {
        // Already a value node (defensive: callers should pass the Root).
        Some(cst_root.clone())
    }
}

/// Descend one [`PathStep`] from `node`, returning the named child or `None`.
///
/// Looks only at `node`'s direct entry/element children (one parent's children) —
/// the per-step cost bound behind FR-027.
fn descend(node: &SyntaxNode, step: &PathStep) -> Option<SyntaxNode> {
    match step {
        PathStep::Field(name) => {
            let s = ast::Struct::cast(node.clone())?;
            let found = s
                .fields()
                .find(|f| f.name_text().as_deref() == Some(name.as_str()))
                .and_then(|f| f.value())
                .map(|v| v.syntax().clone());
            found
        }
        PathStep::Key(text) => {
            let m = ast::Map::cast(node.clone())?;
            let found = m
                .entries()
                .find(|e| e.key().map(|k| k.syntax().text()).as_deref() == Some(text.as_str()))
                .and_then(|e| e.value())
                .map(|v| v.syntax().clone());
            found
        }
        PathStep::Index(idx) => match ast::Value::cast(node.clone())? {
            ast::Value::List(l) => l.items().nth(*idx).map(|v| v.syntax().clone()),
            ast::Value::Tuple(t) => t.items().nth(*idx).map(|v| v.syntax().clone()),
            _ => None,
        },
        PathStep::VariantField(name) => {
            let variant = ast::EnumVariant::cast(node.clone())?;
            let found = variant
                .entries()
                .find(|e| e.key().map(|k| k.syntax().text()).as_deref() == Some(name.as_str()))
                .and_then(|e| e.value())
                .map(|val| val.syntax().clone());
            found
        }
        // A synthetic combined-table step never names a single live node.
        PathStep::CombinedChild(_) => None,
    }
}

/// Like [`descend`] but increments `visits` once per direct child entry/element it
/// examines, stopping at the matched child (the SC-011 node-visit probe). The count
/// mirrors `descend`'s real iteration: a `find`/`nth` over the parent's direct
/// children examines entries up to (and including) the match, never the full tree.
fn descend_counting(node: &SyntaxNode, step: &PathStep, visits: &mut usize) -> Option<SyntaxNode> {
    match step {
        PathStep::Field(name) => {
            let s = ast::Struct::cast(node.clone())?;
            for f in s.fields() {
                *visits += 1;
                if f.name_text().as_deref() == Some(name.as_str()) {
                    return f.value().map(|v| v.syntax().clone());
                }
            }
            None
        }
        PathStep::Key(text) => {
            let m = ast::Map::cast(node.clone())?;
            for e in m.entries() {
                *visits += 1;
                if e.key().map(|k| k.syntax().text()).as_deref() == Some(text.as_str()) {
                    return e.value().map(|v| v.syntax().clone());
                }
            }
            None
        }
        PathStep::Index(idx) => {
            // Iterate LAZILY (like `descend`'s `items().nth(idx)`), stopping at the
            // addressed index — examining `idx + 1` elements, NOT the full list. This
            // is the load-bearing N-independence: with a fixed index the visit count
            // is the same for a 1k- and a 100k-element list (FR-027 / SC-011).
            match ast::Value::cast(node.clone())? {
                ast::Value::List(l) => index_visiting(l.items(), *idx, visits),
                ast::Value::Tuple(t) => index_visiting(t.items(), *idx, visits),
                _ => None,
            }
        }
        PathStep::VariantField(name) => {
            let variant = ast::EnumVariant::cast(node.clone())?;
            for e in variant.entries() {
                *visits += 1;
                if e.key().map(|k| k.syntax().text()).as_deref() == Some(name.as_str()) {
                    return e.value().map(|v| v.syntax().clone());
                }
            }
            None
        }
        // A synthetic combined-table step never names a single live node.
        PathStep::CombinedChild(_) => None,
    }
}

/// Take the `idx`-th item from a lazy element iterator, counting each examined
/// element into `visits` and stopping at the match — examining `idx + 1` elements at
/// most, never the whole sequence (the SC-011 N-independence property).
fn index_visiting(
    items: impl Iterator<Item = ast::Value>,
    idx: usize,
    visits: &mut usize,
) -> Option<SyntaxNode> {
    for (i, v) in items.enumerate() {
        *visits += 1;
        if i == idx {
            return Some(v.syntax().clone());
        }
    }
    None
}

/// Compute the [`StructuralPath`] of `node` within `cst_root` (the inverse of
/// [`resolve_path`]) — for **capturing** focus/expansion identity (FR-016).
///
/// Walks **up** the tree one parent hop per level (depth-bounded, FR-027),
/// deriving the step that addresses each value node from its enclosing
/// struct-field / map-entry / list-or-tuple / enum-variant-payload context, then
/// reverses to produce a root-to-node path. Returns `None` if `node` is not a
/// value-position node reachable from the document's top-level value (e.g. it is
/// the `Root` itself or a stray token-only node).
#[must_use]
pub fn path_of(cst_root: &SyntaxNode, node: &SyntaxNode) -> Option<StructuralPath> {
    // Only value-position nodes have a structural path.
    ast::Value::cast(node.clone())?;
    let top = top_level_value(cst_root)?;

    let mut steps: Vec<PathStep> = Vec::new();
    let mut current = node.clone();
    // Climb to the top-level value, deriving one step per level.
    while current != top {
        let step = step_for(&current)?;
        steps.push(step);
        // Move to the enclosing *value* node (the grandparent of `current`: its
        // parent is the StructField/MapEntry wrapper or the collection itself).
        current = enclosing_value(&current)?;
    }
    steps.reverse();
    Some(StructuralPath { steps })
}

/// Derive the [`PathStep`] that addresses the value node `node` from within its
/// immediate structural parent (struct field / map entry / list-or-tuple element /
/// enum-variant payload field).
fn step_for(node: &SyntaxNode) -> Option<PathStep> {
    let parent = node.parent()?;
    match parent.kind() {
        // A struct field wraps `name: value`; the value's step is the field name.
        SyntaxKind::StructField => {
            let name = ast::StructField::cast(parent.clone()).and_then(|f| f.name_text())?;
            Some(PathStep::Field(name))
        }
        // A map entry wraps `key: value`. Distinguish a true map from an
        // enum-variant struct-like payload (both parse entries as `MapEntry`).
        SyntaxKind::MapEntry => {
            let entry = ast::MapEntry::cast(parent.clone())?;
            let key_text = entry.key().map(|k| k.syntax().text())?;
            // The grandparent decides Key vs VariantField.
            match parent.parent().map(|gp| gp.kind()) {
                Some(SyntaxKind::EnumVariant) => Some(PathStep::VariantField(key_text)),
                _ => Some(PathStep::Key(key_text)),
            }
        }
        // A list/tuple element's step is its 0-based index among siblings.
        SyntaxKind::List | SyntaxKind::Tuple => {
            let idx = index_in_collection(&parent, node)?;
            Some(PathStep::Index(idx))
        }
        _ => None,
    }
}

/// The enclosing *value* node of a value node `node` — i.e. the parent collection
/// value (struct / map / list / tuple / enum variant) that contains it.
fn enclosing_value(node: &SyntaxNode) -> Option<SyntaxNode> {
    let parent = node.parent()?;
    match parent.kind() {
        // Skip the entry wrapper to reach the enclosing collection value.
        SyntaxKind::StructField => parent.parent(),
        SyntaxKind::MapEntry => parent.parent(),
        // For a list/tuple the parent IS the enclosing value.
        SyntaxKind::List | SyntaxKind::Tuple => Some(parent),
        _ => None,
    }
}

/// The 0-based index of value node `child` among the value-position children of a
/// list/tuple `collection`, in source order.
fn index_in_collection(collection: &SyntaxNode, child: &SyntaxNode) -> Option<usize> {
    collection
        .children()
        .filter(|c| ast::Value::cast(c.clone()).is_some())
        .position(|c| &c == child)
}

/// Which structural surface is active for a document (FR-017).
///
/// `#[non_exhaustive]` so future surfaces can be added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ActiveView {
    /// The raw-text editor view (E003) — available on demand.
    Text,
    /// The structural tree/form view — the **default on open** (FR-017).
    #[default]
    TreeForm,
    /// The virtualized spreadsheet/table view — the tree-traversal **outline**
    /// navigator (the default Table surface).
    Table,
    /// An alternate Table surface using the scanner-driven **grouped-sections**
    /// navigator (a comparison variant alongside [`Table`]). Same central grid +
    /// breadcrumb + back/forward; only the left navigator differs. Treated as a
    /// structural view exactly like [`Table`].
    TableSections,
    /// A **pivot-style** Table surface (E021): the same section navigator, but the
    /// selected collection's rows are grouped by the value(s) of 1–2 chosen fields
    /// ([`group_by`](ViewSelectionAndFocus::group_by)) and shown as collapsible groups.
    /// A comparison variant alongside [`TableSections`]; treated as a structural view.
    TableGrouped,
}

/// Which structural surface an [`EditFocus`] lives on (FR-004/FR-009).
///
/// `#[non_exhaustive]` so future surfaces can be added without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FocusSurface {
    /// Focus is on a tree-node value editor.
    TreeNode,
    /// Focus is on a table cell at `(row, column)` indices.
    TableCell {
        /// 0-based row index within the section.
        row: usize,
        /// 0-based column index within the section's column schema.
        column: usize,
    },
}

/// The active edit focus: which node is being edited, keyed to a stable
/// [`StructuralPath`] identity (FR-016).
///
/// Focus is keyed to the path — **not** a screen/row position or a tree handle —
/// so an off-frame reparse and a table-virtualization scroll never steal or
/// mis-target it; if the path no longer resolves the focus is dropped gracefully
/// (FR-016).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditFocus {
    /// The cross-reparse identity of the focused node.
    pub path: StructuralPath,
    /// Which surface the focus lives on.
    pub surface: FocusSurface,
    /// The in-progress, uncommitted edit value (committed on confirm, discarded on
    /// cancel) — carried across a view switch so it is never silently lost
    /// (FR-017).
    pub draft: String,
}

/// The per-section override forcing a (uniform) section's rendering (FR-012/FR-024).
///
/// `#[non_exhaustive]` so the override set can grow (e.g. a forced-table override
/// for a small uniform list) without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SectionOverride {
    /// Force this section to render as tree/form even when table-eligible.
    ForceTreeForm,
    /// Force this section to render as a table (e.g. a small uniform list that
    /// defaults to tree/form, FR-010/FR-012).
    ForceTable,
}

/// How a single section is rendered inside the structural view (FR-010/FR-012/
/// FR-024) — the outcome of the switcher↔override↔classifier precedence.
///
/// Produced by [`ViewSelectionAndFocus::section_rendering`] from the active view, a
/// per-section override, and the classifier's eligibility for that one section. The
/// `forced` flag distinguishes an **automatic** rendering (the classifier's verdict)
/// from a **manual override** (FR-012) so the boundary indicator can show whether a
/// section is on auto or a manual override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SectionRendering {
    /// Render this section as an embedded table (FR-010).
    Table {
        /// `true` when a per-section [`SectionOverride::ForceTable`] forced this (a
        /// small uniform list overridden to a table); `false` for the automatic
        /// table rendering of a uniform-eligible section.
        forced: bool,
    },
    /// Render this section as tree/form (FR-011).
    TreeForm {
        /// `true` when a per-section [`SectionOverride::ForceTreeForm`] forced this
        /// (a uniform section the user pushed back to tree/form); `false` for the
        /// automatic tree/form fallback of a non-uniform / small / empty section.
        forced: bool,
    },
}

impl SectionRendering {
    /// `true` when this section is rendered as a table (forced or automatic).
    #[must_use]
    pub fn is_table(self) -> bool {
        matches!(self, Self::Table { .. })
    }

    /// `true` when the rendering is a manual per-section override (FR-012), vs the
    /// classifier's automatic verdict.
    #[must_use]
    pub fn is_forced(self) -> bool {
        match self {
            Self::Table { forced } | Self::TreeForm { forced } => forced,
        }
    }
}

/// Per-section column **presentation** state for the Table view (E012 / US3 /
/// FR-007): which columns are hidden, what order they display in, and which single
/// column is pinned (frozen at the left).
///
/// # View-only — NEVER a CST/file edit (Principle I / HINT-002 / AD-004)
///
/// Hiding, reordering, and pinning a column are pure presentation changes: every
/// mutator here is byte-free and the document buffer stays byte-identical. The grid
/// applies this state as a **view transform** over [`TableModel::columns`] at render
/// time; it never reshapes the CST. Keep this distinction strict — a column op must
/// not flow into any [`StructuralOp`](ronin_core::StructuralOp).
///
/// # Indices are MODEL-column indices (the stable key)
///
/// `order`, `hidden`, and `pinned` all carry **model**-column indices (indices into
/// `TableModel::columns`), not visible/screen positions. This is the load-bearing
/// invariant for correctness: selection, editing, and the US1/US2 cell gestures all
/// index into the model's columns, so keying view-state by the same model index lets
/// the renderer compute a clean VISIBLE→MODEL mapping without disturbing any of them.
///
/// Out-of-range indices are ignored everywhere (mirroring [`grid_selection`] /
/// [`group_show_cols`] robustness), so a section switch or a reparse that changes the
/// column set can never corrupt the view — stale indices simply drop out.
///
/// [`TableModel::columns`]: crate::structural::table::TableModel
/// [`grid_selection`]: ViewSelectionAndFocus
/// [`group_show_cols`]: ViewSelectionAndFocus::group_show_cols
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ColumnViewState {
    /// The display order as a permutation of MODEL-column indices. A column index
    /// absent from `order` (e.g. a column added by a later reparse) is appended in
    /// its natural model order by the renderer, so a partial/stale `order` still
    /// shows every column. Pinning floats `pinned` to the front at render time; it
    /// is NOT reordered within this vector (so unpin restores the prior position).
    pub order: Vec<usize>,
    /// The MODEL-column indices that are hidden (not rendered). A
    /// [`std::collections::BTreeSet`] for deterministic iteration + cheap membership.
    pub hidden: std::collections::BTreeSet<usize>,
    /// The single pinned/frozen key column (a MODEL-column index), floated to the
    /// left and kept sticky during horizontal scroll. `None` when no column is
    /// pinned. A pinned column is always shown even if it is also in `hidden`
    /// (pinning a column reveals it).
    pub pinned: Option<usize>,
}

impl ColumnViewState {
    /// A fresh, empty column view-state: no hidden columns, default (natural model)
    /// order, nothing pinned.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` when this is the default layout (nothing hidden, no explicit order, no
    /// pin) — the renderer shows the natural model order, and the reset action treats
    /// such a section as already-default.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.order.is_empty() && self.hidden.is_empty() && self.pinned.is_none()
    }

    /// Hide the MODEL column `col` (byte-free). A no-op when `col` is already hidden.
    /// Hiding the pinned column also unpins it (a hidden column cannot stay frozen
    /// visible — one gesture, one consistent result).
    pub fn hide(&mut self, col: usize) {
        self.hidden.insert(col);
        if self.pinned == Some(col) {
            self.pinned = None;
        }
    }

    /// Show (un-hide) the MODEL column `col` (byte-free). A no-op when `col` is not
    /// hidden.
    pub fn show(&mut self, col: usize) {
        self.hidden.remove(&col);
    }

    /// Toggle the hidden state of the MODEL column `col` (byte-free).
    pub fn toggle_hidden(&mut self, col: usize) {
        if self.hidden.contains(&col) {
            self.show(col);
        } else {
            self.hide(col);
        }
    }

    /// `true` when the MODEL column `col` is hidden. The pinned column is never
    /// reported hidden (pinning reveals it).
    #[must_use]
    pub fn is_hidden(&self, col: usize) -> bool {
        self.pinned != Some(col) && self.hidden.contains(&col)
    }

    /// Pin the MODEL column `col` as the frozen key column (byte-free), replacing any
    /// prior pin. Pinning a hidden column also reveals it.
    pub fn pin(&mut self, col: usize) {
        self.hidden.remove(&col);
        self.pinned = Some(col);
    }

    /// Clear the pin (byte-free); a no-op when nothing is pinned.
    pub fn unpin(&mut self) {
        self.pinned = None;
    }

    /// Toggle the pin on the MODEL column `col` (byte-free): pin it if it is not the
    /// current pin, otherwise unpin.
    pub fn toggle_pinned(&mut self, col: usize) {
        if self.pinned == Some(col) {
            self.unpin();
        } else {
            self.pin(col);
        }
    }

    /// Move the MODEL column `col` to display position `to` within the order
    /// (byte-free). The order is first normalized to the full model column set
    /// (`model_cols` columns) so a never-reordered or partial `order` reorders
    /// correctly; `col`/`to` out of range are ignored (no panic, no corruption).
    pub fn move_column(&mut self, col: usize, to: usize, model_cols: usize) {
        if col >= model_cols {
            return;
        }
        let mut order = self.effective_order(model_cols);
        let Some(from) = order.iter().position(|&c| c == col) else {
            return;
        };
        let to = to.min(order.len().saturating_sub(1));
        let moved = order.remove(from);
        order.insert(to, moved);
        self.order = order;
    }

    /// The full display order over a model of `model_cols` columns: the stored
    /// `order` filtered to valid in-range indices, with any model column missing from
    /// it appended in natural model order. This is the canonical permutation the
    /// renderer and [`move_column`](Self::move_column) build on, so a stale/partial
    /// `order` (after a reparse changed the column set) still names every column
    /// exactly once.
    #[must_use]
    pub fn effective_order(&self, model_cols: usize) -> Vec<usize> {
        let mut seen = vec![false; model_cols];
        let mut order: Vec<usize> = Vec::with_capacity(model_cols);
        for &c in &self.order {
            if c < model_cols && !seen[c] {
                seen[c] = true;
                order.push(c);
            }
        }
        for (c, was_seen) in seen.iter().enumerate() {
            if !was_seen {
                order.push(c);
            }
        }
        order
    }

    /// The VISIBLE→MODEL column mapping for a model of `model_cols` columns: the
    /// model-column index shown at each visible position, left to right. The pinned
    /// column (when set + in range) leads; the remaining columns follow in
    /// [`effective_order`](Self::effective_order), skipping hidden ones.
    ///
    /// This is the single source of truth the grid uses to remap a visible click /
    /// cell render back to its MODEL column, so selection / editing / fill / paste /
    /// increment / enum-picker (all of which index `TableModel::columns`) keep
    /// targeting the correct model column after any hide / reorder / pin.
    #[must_use]
    pub fn visible_columns(&self, model_cols: usize) -> Vec<usize> {
        let mut visible: Vec<usize> = Vec::with_capacity(model_cols);
        let pinned = self.pinned.filter(|&p| p < model_cols);
        if let Some(p) = pinned {
            visible.push(p);
        }
        for c in self.effective_order(model_cols) {
            if Some(c) == pinned {
                continue; // already floated to the front
            }
            if self.hidden.contains(&c) {
                continue; // hidden (the pin path above already revealed any pin)
            }
            visible.push(c);
        }
        visible
    }
}

/// The per-document active-view selection + edit focus + section overrides + stale
/// marker (data-model `ViewSelectionAndFocus`).
///
/// Held per [`EditorDocument`](crate::document::EditorDocument), alongside its
/// cursor/parse state. It is **transient/session-only** — never persisted. Focus
/// and overrides are keyed to [`StructuralPath`] identity so they survive an
/// off-frame reparse and a virtualization scroll, or drop gracefully when the
/// node vanishes (FR-016).
#[derive(Debug, Clone, Default)]
pub struct ViewSelectionAndFocus {
    /// The active surface; **default on open is [`ActiveView::TreeForm`]** (FR-017).
    active_view: ActiveView,
    /// The active edit focus, or `None` when nothing is being edited (FR-016).
    edit_focus: Option<EditFocus>,
    /// Per-section overrides, keyed to the section's [`StructuralPath`] (FR-012).
    section_overrides: Vec<(StructuralPath, SectionOverride)>,
    /// An in-progress rename draft for a struct field / map key, keyed to the
    /// renamed node's [`StructuralPath`] (FR-003/FR-022). Distinct from
    /// [`edit_focus`](Self::edit_focus) (which edits a node's *value*) so a rename
    /// and a value edit never clobber one another. `None` when no rename is open.
    rename_draft: Option<(StructuralPath, String)>,
    /// A drill-in return target: the table cell a tree/form drill-in came from
    /// (FR-006). When set, the tree/form surface renders a discoverable "back"
    /// control that restores the [`ActiveView::Table`] view and re-focuses this
    /// cell. `None` when the current tree/form view was not reached by a drill-in.
    drill_in_return: Option<DrillInReturn>,
    /// `true` while an off-frame reparse is pending — the projection is shown
    /// stale-marked rather than inconsistent (FR-015). A user-perceivable marker,
    /// not a silent flag.
    stale: bool,
    /// The table-view navigator's selected section, keyed to its [`StructuralPath`]
    /// (E012). Transient/byte-free: it is re-resolved across reparse by path
    /// identity (the navigator defaults to the largest section when it no longer
    /// matches any scanned section). `None` until the user picks one (the navigator
    /// then defaults to the largest).
    selected_table_section: Option<StructuralPath>,
    /// The Table view's **back** history: previously-selected sections, most-recent
    /// last (E016). [`navigate_table_section`](Self::navigate_table_section) pushes the
    /// outgoing selection here; [`table_go_back`](Self::table_go_back) pops it. Per-doc
    /// + transient/byte-free, like [`selected_table_section`](Self::selected_table_section).
    table_back: Vec<StructuralPath>,
    /// The Table view's **forward** history: sections navigated *away from* via Back,
    /// most-recent last (E016). [`table_go_back`](Self::table_go_back) pushes the
    /// section it left here so [`table_go_forward`](Self::table_go_forward) can
    /// re-advance; a NEW navigation clears it (the standard back/forward semantics).
    table_forward: Vec<StructuralPath>,
    /// The Table grid's **rectangular cell selection** for bulk copy/paste/fill
    /// (E019): `(anchor, cursor)` grid cells `(row, col)`. The highlighted range is
    /// the normalized rectangle between them. Byte-free / transient — by `(row,col)`
    /// coordinates (not paths), cleared on Esc or when it falls outside the model.
    /// `None` when no range is selected.
    grid_selection: Option<((usize, usize), (usize, usize))>,
    /// The column indices the **Table (grouped)** view groups rows by (E021): 0, 1, or 2
    /// entries, each an index into the selected section's columns. Byte-free / transient.
    group_by: Vec<usize>,
    /// The column indices the **Table (grouped)** view displays (E022); empty = all. Each is
    /// an index into the selected section's columns. Byte-free / transient.
    group_show_cols: Vec<usize>,
    /// One-shot: a table-cell editor just began and should grab keyboard focus on its next
    /// render (E021 — Excel "the cell I'm editing is ready to type into"). Set by
    /// [`set_focus`](Self::set_focus) for a `TableCell`, consumed by the grid renderer.
    editor_focus_pending: bool,
    /// Per-section column **presentation** state (E012 / US3 / FR-007): hide / order /
    /// pin, keyed by the section's [`StructuralPath`]. View-only / byte-free — a column
    /// op NEVER touches the CST (Principle I / HINT-002). Kept as a path-keyed
    /// association list (the same shape as [`section_overrides`](Self::section_overrides))
    /// since [`StructuralPath`] is `Hash`/`Eq` but not `Ord`; out-of-range column indices
    /// are ignored, so a section switch / reparse can never corrupt the view.
    column_view_states: Vec<(StructuralPath, ColumnViewState)>,
    /// The **type-rejected** table cells from the most recent fill-down / paste
    /// (E012 / US4 / FR-016), keyed by each rejected cell's value [`StructuralPath`].
    /// A bulk fill / paste DROPS a write whose value is incompatible with the target
    /// cell's declared type (never silently corrupting it) and records the target path
    /// here so the grid can flag exactly which cells were refused. Byte-free / transient:
    /// it carries no value (only "this cell was a rejected target"), is replaced by the
    /// next fill / paste, and is cleared by any other grid action (selection move, edit,
    /// escape) so a stale rejection never lingers.
    rejected_cells: Vec<StructuralPath>,
}

/// The originating table cell a tree/form drill-in returns to (FR-006).
///
/// Recorded when the user drills into a nested table cell so the drilled-in
/// tree/form view can offer a discoverable **back** control that restores the table
/// view with the originating row/cell re-focused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrillInReturn {
    /// The originating cell's value [`StructuralPath`] (the cell that was drilled
    /// into), used to re-focus the originating row/cell on return.
    pub cell_path: StructuralPath,
    /// The originating cell's grid `(row, column)` for re-focusing it (FR-006).
    pub row: usize,
    /// The originating cell's column index.
    pub column: usize,
}

impl ViewSelectionAndFocus {
    /// A fresh per-document state: default to the structural view, no focus, no
    /// overrides, not stale (FR-017).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The active surface (FR-017).
    #[must_use]
    pub fn active_view(&self) -> ActiveView {
        self.active_view
    }

    /// Switch the active surface (FR-017).
    ///
    /// Lossless: switching changes **zero** document bytes (the views all project
    /// the one CST source of truth — FR-020). Any in-progress [`edit_focus`] (and
    /// its draft) is **kept** so an uncommitted edit is never silently lost across
    /// a switch (FR-017); the caller commits or re-resolves it as appropriate.
    pub fn set_active_view(&mut self, view: ActiveView) {
        self.active_view = view;
    }

    /// The active edit focus, if any (FR-016).
    #[must_use]
    pub fn edit_focus(&self) -> Option<&EditFocus> {
        self.edit_focus.as_ref()
    }

    /// The active edit focus mutably, if any (e.g. to update the live draft).
    #[must_use]
    pub fn edit_focus_mut(&mut self) -> Option<&mut EditFocus> {
        self.edit_focus.as_mut()
    }

    /// Begin (or replace) edit focus on the node identified by `path`.
    pub fn set_focus(&mut self, path: StructuralPath, surface: FocusSurface, draft: String) {
        // A table-cell editor should auto-focus when it next renders so it's ready to type
        // into (Excel — E021); the grid renderer consumes this one-shot request.
        if matches!(surface, FocusSurface::TableCell { .. }) {
            self.editor_focus_pending = true;
        }
        self.edit_focus = Some(EditFocus {
            path,
            surface,
            draft,
        });
    }

    /// Take (read + clear) the one-shot "the table-cell editor should grab focus" request
    /// (E021). The grid renderer calls this each frame after laying out the cells.
    pub fn take_editor_focus_pending(&mut self) -> bool {
        std::mem::take(&mut self.editor_focus_pending)
    }

    /// Clear edit focus (commit/cancel done, or graceful drop on vanish — FR-016).
    pub fn clear_focus(&mut self) {
        self.edit_focus = None;
    }

    /// The in-progress rename draft `(path, text)`, if a rename is open (FR-003).
    #[must_use]
    pub fn rename_draft(&self) -> Option<&(StructuralPath, String)> {
        self.rename_draft.as_ref()
    }

    /// Begin (or replace) a rename draft on the node identified by `path`,
    /// seeding it with `text` (the field/key's current name) — FR-003/FR-022.
    pub fn set_rename_draft(&mut self, path: StructuralPath, text: String) {
        self.rename_draft = Some((path, text));
    }

    /// Update the text of the active rename draft when it targets `path` (FR-003).
    pub fn update_rename_draft(&mut self, path: &StructuralPath, text: String) {
        if let Some(slot) = self.rename_draft.as_mut() {
            if &slot.0 == path {
                slot.1 = text;
            }
        }
    }

    /// Clear the rename draft (commit/cancel done) — FR-003.
    pub fn clear_rename_draft(&mut self) {
        self.rename_draft = None;
    }

    /// The drill-in return target, if the current tree/form view was reached by a
    /// table-cell drill-in (FR-006).
    #[must_use]
    pub fn drill_in_return(&self) -> Option<&DrillInReturn> {
        self.drill_in_return.as_ref()
    }

    /// Record the originating table cell of a drill-in so the drilled-in tree/form
    /// view can offer a discoverable back control (FR-006).
    pub fn set_drill_in_return(&mut self, ret: DrillInReturn) {
        self.drill_in_return = Some(ret);
    }

    /// Clear the drill-in return target (the user returned to the table, or the
    /// drilled-in node vanished) — FR-006.
    pub fn clear_drill_in_return(&mut self) {
        self.drill_in_return = None;
    }

    /// The table-view navigator's selected section path, if one is set (E012).
    #[must_use]
    pub fn selected_table_section(&self) -> Option<&StructuralPath> {
        self.selected_table_section.as_ref()
    }

    /// Set (or clear) the table-view navigator's selected section (E012). Byte-free
    /// (FR-020): selecting a section in the navigator changes zero document bytes.
    ///
    /// This is the **raw** setter — it does NOT record back/forward history. Use it
    /// only for non-navigational writes (e.g. the seam's default-to-root fallback,
    /// which must never pollute history). User-initiated level changes route through
    /// [`navigate_table_section`](Self::navigate_table_section) instead (E016).
    pub fn set_selected_table_section(&mut self, path: Option<StructuralPath>) {
        self.selected_table_section = path;
    }

    /// Navigate the Table view to the section at `path`, recording back/forward history
    /// (E016) — the user-initiated level-change entry point (an outline click, a
    /// breadcrumb click, or opening a nested cell as a table).
    ///
    /// If `path` is already the current selection this is a **no-op** (no duplicate
    /// history entry). Otherwise the current selection (when set) is pushed onto the
    /// **back** stack, the **forward** stack is cleared (a new navigation invalidates
    /// the redo path), and `path` becomes the new selection. Byte-free (FR-020).
    pub fn navigate_table_section(&mut self, path: StructuralPath) {
        if Some(&path) == self.selected_table_section.as_ref() {
            return;
        }
        if let Some(current) = self.selected_table_section.take() {
            self.table_back.push(current);
        }
        self.table_forward.clear();
        self.selected_table_section = Some(path);
    }

    /// Go **back** to the previously-selected Table section (E016): pop the back stack,
    /// push the current selection onto the forward stack, and select the popped path.
    /// A no-op when the back stack is empty ([`can_go_back`](Self::can_go_back) is
    /// `false`). Byte-free (FR-020).
    pub fn table_go_back(&mut self) {
        let Some(prev) = self.table_back.pop() else {
            return;
        };
        if let Some(current) = self.selected_table_section.take() {
            self.table_forward.push(current);
        }
        self.selected_table_section = Some(prev);
    }

    /// Go **forward** to a Table section previously left via Back (E016): pop the
    /// forward stack, push the current selection onto the back stack, and select the
    /// popped path. A no-op when the forward stack is empty
    /// ([`can_go_forward`](Self::can_go_forward) is `false`). Byte-free (FR-020).
    pub fn table_go_forward(&mut self) {
        let Some(next) = self.table_forward.pop() else {
            return;
        };
        if let Some(current) = self.selected_table_section.take() {
            self.table_back.push(current);
        }
        self.selected_table_section = Some(next);
    }

    /// Go **up a level** in the Table view (E016): navigate to the parent of the
    /// current selection (the document root when nothing is selected). A no-op when the
    /// current selection is already the root (there is no parent). Records history via
    /// [`navigate_table_section`](Self::navigate_table_section). Byte-free (FR-020).
    pub fn table_go_up(&mut self) {
        let current = self
            .selected_table_section
            .clone()
            .unwrap_or_else(StructuralPath::root);
        if current.is_root() {
            return;
        }
        let steps = current.steps();
        let parent = StructuralPath::from_steps(steps[..steps.len() - 1].to_vec());
        self.navigate_table_section(parent);
    }

    /// `true` when [`table_go_back`](Self::table_go_back) would move (the back stack is
    /// non-empty) — for enabling a Back button (E016).
    #[must_use]
    pub fn can_go_back(&self) -> bool {
        !self.table_back.is_empty()
    }

    /// `true` when [`table_go_forward`](Self::table_go_forward) would move (the forward
    /// stack is non-empty) — for enabling a Forward button (E016).
    #[must_use]
    pub fn can_go_forward(&self) -> bool {
        !self.table_forward.is_empty()
    }

    /// `true` when [`table_go_up`](Self::table_go_up) would move (the current selection,
    /// or the root default, is not already the root) — for enabling an Up button (E016).
    #[must_use]
    pub fn can_go_up(&self) -> bool {
        self.selected_table_section
            .as_ref()
            .is_some_and(|p| !p.is_root())
    }

    // --- E021: Table (grouped) view — group-by field selection ------------------

    /// The column indices the Table (grouped) view groups rows by (0–2 entries). Byte-free.
    #[must_use]
    pub fn group_by(&self) -> &[usize] {
        &self.group_by
    }

    /// Set the group-by column indices (capped at 2; the grouped view supports up to two
    /// levels). Byte-free (FR-020).
    pub fn set_group_by(&mut self, cols: Vec<usize>) {
        let mut cols = cols;
        cols.truncate(2);
        self.group_by = cols;
    }

    /// The column indices the Table (grouped) view **displays** (E022). Empty = show all
    /// columns. Out-of-range indices are ignored by the renderer, so a section switch can
    /// never corrupt the view.
    #[must_use]
    pub fn group_show_cols(&self) -> &[usize] {
        &self.group_show_cols
    }

    /// Set the displayed-column indices for the Table (grouped) view (E022). Byte-free.
    pub fn set_group_show_cols(&mut self, cols: Vec<usize>) {
        self.group_show_cols = cols;
    }

    // --- E019: Table grid rectangular selection (bulk copy/paste/fill) ----------

    /// Start a single-cell grid selection at `(row, col)` — sets both the anchor and
    /// cursor (a 1×1 selection). Byte-free (FR-020).
    pub fn set_grid_anchor(&mut self, row: usize, col: usize) {
        self.grid_selection = Some(((row, col), (row, col)));
    }

    /// Extend the grid selection's cursor to `(row, col)`, keeping the anchor (the
    /// shift-click / shift-arrow gesture). Starts a fresh selection at `(row, col)` if
    /// none is active. Byte-free (FR-020).
    pub fn extend_grid_to(&mut self, row: usize, col: usize) {
        match &mut self.grid_selection {
            Some((_, cursor)) => *cursor = (row, col),
            None => self.grid_selection = Some(((row, col), (row, col))),
        }
    }

    /// Select the whole grid `rows × cols` (Ctrl+A): anchor top-left, cursor
    /// bottom-right. A no-op for an empty grid. Byte-free (FR-020).
    pub fn select_grid_all(&mut self, rows: usize, cols: usize) {
        if rows == 0 || cols == 0 {
            self.grid_selection = None;
            return;
        }
        self.grid_selection = Some(((0, 0), (rows - 1, cols - 1)));
    }

    /// Clear the grid selection (Esc / a structural edit invalidated it).
    pub fn clear_grid_selection(&mut self) {
        self.grid_selection = None;
    }

    /// The grid selection's **cursor** cell `(row, col)`, the moving end shift-arrows
    /// extend from. `None` when nothing is selected.
    #[must_use]
    pub fn grid_cursor(&self) -> Option<(usize, usize)> {
        self.grid_selection.map(|(_, cursor)| cursor)
    }

    /// The grid selection's **anchor** cell `(row, col)`. `None` when nothing is
    /// selected.
    #[must_use]
    pub fn grid_anchor(&self) -> Option<(usize, usize)> {
        self.grid_selection.map(|(anchor, _)| anchor)
    }

    /// The normalized selection rectangle `(min_row, min_col, max_row, max_col)`
    /// (inclusive), or `None` when nothing is selected.
    #[must_use]
    pub fn grid_selection_rect(&self) -> Option<(usize, usize, usize, usize)> {
        self.grid_selection
            .map(|((ar, ac), (cr, cc))| (ar.min(cr), ac.min(cc), ar.max(cr), ac.max(cc)))
    }

    // --- E012 / US4 / FR-016 — type-rejected fill/paste cell flags ---------------

    /// Record the cells a bulk fill / paste **refused to write** because their value was
    /// incompatible with the target cell's declared type (E012 / FR-016). REPLACES any
    /// prior set (each fill / paste reports its own rejections), so the grid flags only
    /// the cells the latest operation refused. Byte-free / transient (FR-020): a refused
    /// write changes zero document bytes; this only marks which targets were skipped.
    pub fn set_rejected_cells(&mut self, paths: Vec<StructuralPath>) {
        self.rejected_cells = paths;
    }

    /// Clear the type-rejected fill/paste cell flags (E012 / FR-016) — called when a
    /// different grid action runs (selection move, edit, escape) so a stale rejection
    /// marker never lingers past the operation it described. A no-op when none are set.
    pub fn clear_rejected_cells(&mut self) {
        if !self.rejected_cells.is_empty() {
            self.rejected_cells.clear();
        }
    }

    /// `true` when the cell at `path` was a type-rejected target of the most recent
    /// fill / paste (E012 / FR-016) — the grid paints the rejected marker on it. By
    /// value [`StructuralPath`], so it survives a virtualization scroll like the other
    /// cell state. Read-only / byte-free.
    #[must_use]
    pub fn is_rejected_cell(&self, path: &StructuralPath) -> bool {
        self.rejected_cells.iter().any(|p| p == path)
    }

    /// `true` when at least one cell is flagged type-rejected (E012 / FR-016). A cheap
    /// guard the renderer uses to skip the per-cell lookup when nothing was rejected.
    #[must_use]
    pub fn has_rejected_cells(&self) -> bool {
        !self.rejected_cells.is_empty()
    }

    /// The type-rejected fill/paste cell paths (E012 / FR-016), for snapshotting once per
    /// frame so the immutable-borrow render closures can flag each target without
    /// re-borrowing the document. Empty when the last fill / paste refused nothing.
    #[must_use]
    pub fn rejected_cells(&self) -> &[StructuralPath] {
        &self.rejected_cells
    }

    // --- E012 / US3 — per-section column view-state (hide / order / pin) ---------

    /// The column view-state for the section at `path`, if one has been set (E012 /
    /// FR-007). `None` when the section uses the default layout (the renderer then
    /// shows every column in natural model order). Read-only; byte-free.
    #[must_use]
    pub fn column_view_state(&self, path: &StructuralPath) -> Option<&ColumnViewState> {
        self.column_view_states
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, s)| s)
    }

    /// The column view-state for the section at `path`, creating an empty default one
    /// if absent (E012 / FR-007) — the mutable entry point for a hide / reorder / pin
    /// gesture. Every mutation through the returned reference is view-only / byte-free
    /// (Principle I): it changes presentation state only and NEVER the document buffer.
    pub fn column_view_state_mut(&mut self, path: &StructuralPath) -> &mut ColumnViewState {
        if let Some(idx) = self.column_view_states.iter().position(|(p, _)| p == path) {
            return &mut self.column_view_states[idx].1;
        }
        self.column_view_states
            .push((path.clone(), ColumnViewState::new()));
        &mut self
            .column_view_states
            .last_mut()
            .expect("just pushed an entry")
            .1
    }

    /// Reset the section at `path` to the default column layout (E012 / FR-015):
    /// drop its column view-state so the renderer shows every column in natural model
    /// order, nothing hidden, nothing pinned. A no-op when the section is already
    /// default. Byte-free.
    pub fn reset_column_view_state(&mut self, path: &StructuralPath) {
        self.column_view_states.retain(|(p, _)| p != path);
    }

    /// The section paths that currently have a live column view-state (E012 / FR-007).
    ///
    /// Used by the settings-sync seam to write each live layout back to its persisted
    /// entry; the order mirrors first-seen insertion order. Byte-free.
    #[must_use]
    pub fn column_view_state_paths(&self) -> Vec<StructuralPath> {
        self.column_view_states
            .iter()
            .map(|(p, _)| p.clone())
            .collect()
    }

    /// `true` when the projection is stale (a reparse is in flight) (FR-015).
    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.stale
    }

    /// Mark the projection stale (an edit was requested; the reparse has not yet
    /// landed) (FR-015).
    pub fn mark_stale(&mut self) {
        self.stale = true;
    }

    /// Clear the stale marker (a current reparse landed and the projection was
    /// re-derived) (FR-015).
    pub fn clear_stale(&mut self) {
        self.stale = false;
    }

    /// The override for the section identified by `path`, if any (FR-012).
    #[must_use]
    pub fn section_override(&self, path: &StructuralPath) -> Option<SectionOverride> {
        self.section_overrides
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, o)| *o)
    }

    /// Set (or replace) the override for the section identified by `path` (FR-012).
    pub fn set_section_override(&mut self, path: StructuralPath, ovr: SectionOverride) {
        if let Some(slot) = self.section_overrides.iter_mut().find(|(p, _)| *p == path) {
            slot.1 = ovr;
        } else {
            self.section_overrides.push((path, ovr));
        }
    }

    /// Clear the override for the section identified by `path` (FR-012); a no-op if
    /// none was set.
    pub fn clear_section_override(&mut self, path: &StructuralPath) {
        self.section_overrides.retain(|(p, _)| p != path);
    }

    /// Toggle the per-section override for `path` between forced-tree/form and the
    /// section's automatic rendering, or between forced-table and automatic — the
    /// reversible per-section control behind FR-012.
    ///
    /// `table_eligible` is the classifier's verdict for this section (whether its
    /// automatic rendering is a table). The toggle is symmetric and reversible:
    ///
    /// * an **eligible** section (auto = table): no override → force tree/form;
    ///   already forced-tree/form → clear (back to the automatic table);
    /// * an **ineligible** section (auto = tree/form, e.g. a ≤2-element uniform
    ///   list): no override → force table; already forced-table → clear (back to the
    ///   automatic tree/form).
    ///
    /// A pre-existing *opposite* override is replaced by the natural toggle for this
    /// section, so the same control always returns the section to its automatic
    /// rendering on the second click (FR-012). Changing an override changes **zero**
    /// document bytes (FR-020) and never changes the document-level
    /// [`active_view`](Self::active_view) (FR-024).
    pub fn toggle_section_override(&mut self, path: &StructuralPath, table_eligible: bool) {
        let current = self.section_override(path);
        // The override that forces a section AWAY from its automatic rendering.
        let force_away = if table_eligible {
            SectionOverride::ForceTreeForm
        } else {
            SectionOverride::ForceTable
        };
        match current {
            // Already forced away from auto → clear (reverse to automatic).
            Some(o) if o == force_away => self.clear_section_override(path),
            // No override (or the opposite one) → force away from automatic.
            _ => self.set_section_override(path.clone(), force_away),
        }
    }

    /// Resolve how the section at `path` renders, given the active view, any
    /// per-section override, and the classifier's `table_eligible` verdict — the
    /// switcher↔override precedence of FR-024 ([COMPLETES FR-024]).
    ///
    /// Precedence (predictable for any section):
    ///
    /// 1. A per-section override applies **only while the document is in a structural
    ///    view** ([`ActiveView::TreeForm`]/[`ActiveView::Table`]) and only to *that*
    ///    one section. When the active view is [`ActiveView::Text`] this returns
    ///    `None` — the whole document shows as text regardless of any overrides, and
    ///    the overrides are **retained** (not cleared) for the return to a structural
    ///    view (FR-024).
    /// 2. Within a structural view, a [`SectionOverride::ForceTreeForm`] →
    ///    `TreeForm { forced: true }`; a [`SectionOverride::ForceTable`] →
    ///    `Table { forced: true }` (FR-012).
    /// 3. With no override, the classifier's verdict decides: `table_eligible` →
    ///    `Table { forced: false }`; otherwise `TreeForm { forced: false }`
    ///    (FR-010/FR-011).
    ///
    /// An override never changes the document-level active view (the caller reads
    /// [`active_view`](Self::active_view) separately); this is a *per-section*
    /// decision only (FR-024).
    #[must_use]
    pub fn section_rendering(
        &self,
        path: &StructuralPath,
        table_eligible: bool,
    ) -> Option<SectionRendering> {
        // A per-section override never applies in the text view (FR-024).
        if self.active_view == ActiveView::Text {
            return None;
        }
        let rendering = match self.section_override(path) {
            Some(SectionOverride::ForceTreeForm) => SectionRendering::TreeForm { forced: true },
            Some(SectionOverride::ForceTable) => SectionRendering::Table { forced: true },
            None => {
                if table_eligible {
                    SectionRendering::Table { forced: false }
                } else {
                    SectionRendering::TreeForm { forced: false }
                }
            }
        };
        Some(rendering)
    }

    /// Re-resolve edit focus + section overrides against a freshly-landed CST,
    /// dropping focus gracefully when its node vanished (T013, FR-016/FR-027).
    ///
    /// Called once after an off-frame reparse lands (and also usable after a
    /// virtualization scroll re-realizes rows). For the active focus, [`resolve_path`]
    /// is run against `cst_root`:
    ///
    /// * the path **still resolves** → focus is kept (the same logical node is now
    ///   bound in the fresh tree — no mis-target);
    /// * the path **no longer resolves** (a conflicting text edit deleted the node)
    ///   → edit mode is **dropped gracefully** (`edit_focus` cleared) so the wrong
    ///   node is never edited (FR-016).
    ///
    /// Stale section overrides (whose section path no longer resolves) are pruned
    /// the same way. Each lookup costs time proportional to the path's depth, not
    /// the section's row count or the document's node count (FR-027 / SC-011).
    ///
    /// Returns `true` when focus was dropped because its node vanished (so the
    /// caller can react, e.g. surface a notice); `false` when focus was kept or was
    /// already absent.
    pub fn reresolve(&mut self, cst_root: &SyntaxNode) -> bool {
        // Prune section overrides whose section no longer exists (depth-bounded
        // lookups, FR-027).
        self.section_overrides
            .retain(|(path, _)| resolve_path(cst_root, path).is_some());

        // Drop a drill-in return target whose originating cell vanished (FR-006/
        // FR-016): the back control can no longer re-focus a node that no longer
        // exists, so it degrades gracefully (depth-bounded lookup, FR-027).
        if let Some(ret) = &self.drill_in_return {
            if resolve_path(cst_root, &ret.cell_path).is_none() {
                self.drill_in_return = None;
            }
        }

        let Some(focus) = &self.edit_focus else {
            return false;
        };
        if resolve_path(cst_root, &focus.path).is_some() {
            // Path still resolves: keep focus bound to the same logical node.
            false
        } else {
            // Node vanished: drop edit mode gracefully — never edit the wrong node.
            self.edit_focus = None;
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ronin_core::parse;

    fn root_of(src: &str) -> SyntaxNode {
        parse(src).root()
    }

    #[test]
    fn root_path_resolves_top_level_value() {
        let root = root_of("Point(x: 1, y: 2)");
        let node = resolve_path(&root, &StructuralPath::root()).expect("root resolves");
        assert_eq!(node.kind(), SyntaxKind::Struct);
    }

    #[test]
    fn field_step_resolves_struct_field_value() {
        let root = root_of("Point(x: 1, y: 2)");
        let path = StructuralPath::from_steps(vec![PathStep::Field("y".to_string())]);
        let node = resolve_path(&root, &path).expect("y resolves");
        assert_eq!(node.text(), "2");
    }

    #[test]
    fn index_step_resolves_list_element() {
        let root = root_of("[10, 20, 30]");
        let path = StructuralPath::from_steps(vec![PathStep::Index(2)]);
        let node = resolve_path(&root, &path).expect("index 2 resolves");
        assert_eq!(node.text(), "30");
    }

    #[test]
    fn nested_path_resolves_deep_node() {
        let root = root_of("Outer(items: [A(v: 1), A(v: 2)])");
        let path = StructuralPath::from_steps(vec![
            PathStep::Field("items".to_string()),
            PathStep::Index(1),
            PathStep::Field("v".to_string()),
        ]);
        let node = resolve_path(&root, &path).expect("deep node resolves");
        assert_eq!(node.text(), "2");
    }

    #[test]
    fn vanished_node_does_not_resolve() {
        let root = root_of("Point(x: 1)");
        let path = StructuralPath::from_steps(vec![PathStep::Field("missing".to_string())]);
        assert!(resolve_path(&root, &path).is_none());
    }

    #[test]
    fn path_of_round_trips_through_resolve() {
        let src = "Outer(items: [A(v: 1), A(v: 2)])";
        let root = root_of(src);
        // Find the inner `2` literal by resolving a known path, then derive its
        // path and confirm it resolves back to the same node.
        let known = StructuralPath::from_steps(vec![
            PathStep::Field("items".to_string()),
            PathStep::Index(1),
            PathStep::Field("v".to_string()),
        ]);
        let node = resolve_path(&root, &known).expect("known resolves");
        let derived = path_of(&root, &node).expect("path derivable");
        assert_eq!(derived, known);
        let re = resolve_path(&root, &derived).expect("derived resolves");
        assert_eq!(re.text(), node.text());
    }

    #[test]
    fn focus_survives_equivalent_reparse_and_drops_on_vanish() {
        let mut vsf = ViewSelectionAndFocus::new();
        assert_eq!(vsf.active_view(), ActiveView::TreeForm); // default structural

        let path = StructuralPath::from_steps(vec![PathStep::Field("x".to_string())]);
        vsf.set_focus(path, FocusSurface::TreeNode, "1".to_string());

        // A reparse of an equivalent doc keeps focus.
        let root_keep = root_of("Point(x: 1, y: 2)");
        assert!(!vsf.reresolve(&root_keep));
        assert!(vsf.edit_focus().is_some());

        // A reparse where `x` vanished drops focus gracefully.
        let root_gone = root_of("Point(y: 2)");
        assert!(vsf.reresolve(&root_gone));
        assert!(vsf.edit_focus().is_none());
    }

    #[test]
    fn map_key_step_resolves_non_string_key() {
        let root = root_of("{ 1: \"one\", 2: \"two\" }");
        let path = StructuralPath::from_steps(vec![PathStep::Key("2".to_string())]);
        let node = resolve_path(&root, &path).expect("key 2 resolves");
        assert_eq!(node.text(), "\"two\"");
    }

    // =========================================================================
    // E016 — Table view Back / Forward / Up navigation history
    // =========================================================================

    /// A `StructuralPath` of one field step (a convenient distinct navigation target).
    fn field(name: &str) -> StructuralPath {
        StructuralPath::from_steps(vec![PathStep::Field(name.to_string())])
    }

    #[test]
    fn navigate_table_section_pushes_back_and_clears_forward() {
        let mut vsf = ViewSelectionAndFocus::new();
        assert!(vsf.selected_table_section().is_none());
        assert!(!vsf.can_go_back() && !vsf.can_go_forward());

        // First navigation: nothing was selected, so nothing pushed onto back.
        vsf.navigate_table_section(field("a"));
        assert_eq!(vsf.selected_table_section(), Some(&field("a")));
        assert!(!vsf.can_go_back(), "first navigation pushes no back entry");

        // Second navigation: the outgoing selection (`a`) is pushed onto back.
        vsf.navigate_table_section(field("b"));
        assert_eq!(vsf.selected_table_section(), Some(&field("b")));
        assert!(
            vsf.can_go_back(),
            "navigating away pushes the prior onto back"
        );

        // Going back populates forward; a NEW navigation must clear forward.
        vsf.table_go_back();
        assert_eq!(vsf.selected_table_section(), Some(&field("a")));
        assert!(vsf.can_go_forward(), "back populates forward");
        vsf.navigate_table_section(field("c"));
        assert!(
            !vsf.can_go_forward(),
            "a new navigation clears the forward stack"
        );
    }

    #[test]
    fn navigate_table_section_is_a_noop_when_path_equals_current() {
        let mut vsf = ViewSelectionAndFocus::new();
        vsf.navigate_table_section(field("a"));
        vsf.navigate_table_section(field("b"));
        assert!(vsf.can_go_back());
        // Re-navigating to the CURRENT selection records no duplicate history entry.
        vsf.navigate_table_section(field("b"));
        assert_eq!(vsf.selected_table_section(), Some(&field("b")));
        // Only one back entry (`a`) — the no-op did not push `b` again.
        vsf.table_go_back();
        assert_eq!(vsf.selected_table_section(), Some(&field("a")));
        assert!(!vsf.can_go_back(), "the no-op recorded no extra back entry");
    }

    #[test]
    fn table_back_and_forward_round_trip_a_sequence() {
        // A → B → C, then Back → B, Back → A, Forward → B (E016).
        let mut vsf = ViewSelectionAndFocus::new();
        vsf.navigate_table_section(field("a"));
        vsf.navigate_table_section(field("b"));
        vsf.navigate_table_section(field("c"));
        assert_eq!(vsf.selected_table_section(), Some(&field("c")));

        vsf.table_go_back();
        assert_eq!(vsf.selected_table_section(), Some(&field("b")));
        vsf.table_go_back();
        assert_eq!(vsf.selected_table_section(), Some(&field("a")));
        assert!(!vsf.can_go_back(), "back stack is exhausted at A");

        vsf.table_go_forward();
        assert_eq!(vsf.selected_table_section(), Some(&field("b")));
        assert!(vsf.can_go_back() && vsf.can_go_forward());
    }

    #[test]
    fn table_go_back_and_forward_are_noops_on_empty_stacks() {
        let mut vsf = ViewSelectionAndFocus::new();
        vsf.navigate_table_section(field("a"));
        // No history yet: both are no-ops (selection unchanged).
        vsf.table_go_back();
        assert_eq!(vsf.selected_table_section(), Some(&field("a")));
        vsf.table_go_forward();
        assert_eq!(vsf.selected_table_section(), Some(&field("a")));
    }

    #[test]
    fn table_go_up_yields_parent_and_is_noop_at_root() {
        let mut vsf = ViewSelectionAndFocus::new();
        let deep = StructuralPath::from_steps(vec![
            PathStep::Field("data".to_string()),
            PathStep::Field("rows".to_string()),
            PathStep::Index(0),
        ]);
        vsf.set_selected_table_section(Some(deep));

        vsf.table_go_up();
        assert_eq!(
            vsf.selected_table_section(),
            Some(&StructuralPath::from_steps(vec![
                PathStep::Field("data".to_string()),
                PathStep::Field("rows".to_string()),
            ])),
            "up a level drops the trailing step"
        );

        vsf.table_go_up();
        assert_eq!(
            vsf.selected_table_section(),
            Some(&field("data")),
            "up again drops another step"
        );

        vsf.table_go_up();
        assert_eq!(
            vsf.selected_table_section(),
            Some(&StructuralPath::root()),
            "up from a depth-1 path reaches the root"
        );

        // At the root there is no parent: up is a no-op (and `can_go_up` is false).
        assert!(!vsf.can_go_up(), "the root has no parent to go up to");
        vsf.table_go_up();
        assert_eq!(
            vsf.selected_table_section(),
            Some(&StructuralPath::root()),
            "up at the root is a no-op"
        );
    }

    #[test]
    fn can_go_back_forward_up_reflect_state() {
        let mut vsf = ViewSelectionAndFocus::new();
        // Nothing selected: no history, and no parent (None defaults to root).
        assert!(!vsf.can_go_back());
        assert!(!vsf.can_go_forward());
        assert!(!vsf.can_go_up());

        vsf.navigate_table_section(field("a"));
        assert!(!vsf.can_go_back(), "first nav records no back");
        assert!(!vsf.can_go_forward());
        assert!(vsf.can_go_up(), "a depth-1 selection can go up to the root");

        vsf.navigate_table_section(field("b"));
        assert!(vsf.can_go_back());
        assert!(!vsf.can_go_forward());

        vsf.table_go_back();
        assert!(!vsf.can_go_back());
        assert!(vsf.can_go_forward(), "back populated the forward stack");

        // At the root selection, can_go_up is false.
        vsf.set_selected_table_section(Some(StructuralPath::root()));
        assert!(!vsf.can_go_up());
    }

    // =========================================================================
    // E012 / US3 — ColumnViewState (hide / reorder / pin), view-only / byte-free
    // =========================================================================

    #[test]
    fn column_view_state_default_is_natural_order_all_visible() {
        let cvs = ColumnViewState::new();
        assert!(cvs.is_default());
        // 4 model columns → visible = [0,1,2,3] (natural order, nothing hidden).
        assert_eq!(cvs.visible_columns(4), vec![0, 1, 2, 3]);
        assert_eq!(cvs.effective_order(4), vec![0, 1, 2, 3]);
    }

    #[test]
    fn column_view_state_hide_removes_from_visible_only() {
        let mut cvs = ColumnViewState::new();
        cvs.hide(1);
        assert!(cvs.is_hidden(1));
        assert_eq!(cvs.visible_columns(4), vec![0, 2, 3], "column 1 hidden");
        cvs.show(1);
        assert!(!cvs.is_hidden(1));
        assert_eq!(cvs.visible_columns(4), vec![0, 1, 2, 3]);
    }

    #[test]
    fn column_view_state_move_reorders_visible() {
        let mut cvs = ColumnViewState::new();
        // Move model column 3 to display position 0.
        cvs.move_column(3, 0, 4);
        assert_eq!(cvs.visible_columns(4), vec![3, 0, 1, 2]);
        // Move model column 0 to position 2 within the new order [3,0,1,2].
        cvs.move_column(0, 2, 4);
        assert_eq!(cvs.visible_columns(4), vec![3, 1, 0, 2]);
    }

    #[test]
    fn column_view_state_pin_floats_to_front_and_reveals() {
        let mut cvs = ColumnViewState::new();
        cvs.hide(2);
        cvs.pin(2); // pinning a hidden column reveals it
        assert!(!cvs.is_hidden(2), "pin reveals a hidden column");
        // Pinned column leads; the rest follow in natural order (2 not repeated).
        assert_eq!(cvs.visible_columns(4), vec![2, 0, 1, 3]);
        cvs.unpin();
        assert_eq!(cvs.visible_columns(4), vec![0, 1, 2, 3]);
    }

    #[test]
    fn column_view_state_hide_pinned_unpins() {
        let mut cvs = ColumnViewState::new();
        cvs.pin(1);
        assert_eq!(cvs.pinned, Some(1));
        cvs.hide(1);
        assert_eq!(cvs.pinned, None, "hiding the pinned column unpins it");
        assert_eq!(cvs.visible_columns(4), vec![0, 2, 3]);
    }

    #[test]
    fn column_view_state_ignores_out_of_range_indices() {
        let mut cvs = ColumnViewState::new();
        // Stale indices from a wider prior model must not corrupt a now-narrower one.
        cvs.hide(9);
        cvs.pin(9);
        cvs.move_column(9, 0, 3); // col out of range → no-op
        cvs.order = vec![5, 1, 0]; // a stale order entry (5) is dropped
        assert_eq!(
            cvs.visible_columns(3),
            vec![1, 0, 2],
            "out-of-range pin/hide/order entries are ignored; col 2 appended"
        );
    }

    #[test]
    fn reset_column_view_state_restores_default() {
        let mut vsf = ViewSelectionAndFocus::new();
        let sec = field("rows");
        vsf.column_view_state_mut(&sec).hide(0);
        vsf.column_view_state_mut(&sec).pin(2);
        assert!(vsf.column_view_state(&sec).is_some());
        vsf.reset_column_view_state(&sec);
        assert!(
            vsf.column_view_state(&sec).is_none(),
            "reset drops the section's column view-state (default layout)"
        );
    }

    #[test]
    fn set_selected_table_section_does_not_record_history() {
        // The raw setter (used by the default-to-root fallback) must NOT pollute the
        // back/forward history (E016).
        let mut vsf = ViewSelectionAndFocus::new();
        vsf.navigate_table_section(field("a"));
        vsf.navigate_table_section(field("b"));
        let back_before = vsf.can_go_back();
        // A raw set (e.g. the seam's default-to-root resolve) records no history.
        vsf.set_selected_table_section(Some(StructuralPath::root()));
        assert_eq!(
            vsf.can_go_back(),
            back_before,
            "the raw setter must not change the back stack"
        );
        assert!(
            !vsf.can_go_forward(),
            "the raw setter must not touch forward"
        );
    }
}
