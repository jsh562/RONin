//! The spreadsheet/table structural view: a **virtualized** editable grid of a
//! uniform section (a list of same-shape records) — rows = elements, columns =
//! the union of fields — with scalar cell editing, keyboard cell navigation,
//! discoverable row add/remove, and a nested-cell drill-in to the tree/form
//! surface (E008 Phase 3 / US2 — FR-005..FR-009/FR-018).
//!
//! # The model is a read projection (FR-005/FR-020)
//!
//! [`TableModel`] is a transient projection of one uniform CST list: each
//! [`Cell`] carries its value's [`StructuralPath`] node identity so an edit can be
//! re-resolved against the LIVE CST at commit time (AD-004 / HINT-002). Building
//! and scrolling the grid change **zero** document bytes — only an explicit edit
//! does (FR-020). It is re-derived from the off-frame projection / CST, never the
//! source of truth.
//!
//! # Columns = union of fields, first-seen order (FR-010)
//!
//! The column schema is the union of every record's field names in first-seen
//! order; a field merely *absent* from a record renders as a [`CellClass::Blank`]
//! cell (visually distinct from a present-but-empty scalar so the user can tell
//! "field absent" from "field present, empty" — FR-010). Editing a blank cell adds
//! the previously-absent field rather than altering an empty value. A field whose
//! value is a nested collection in any record makes its column [`ColumnClass::Nested`]:
//! such cells are **not** edited inline — they expose a drill-in into the
//! tree/form surface (FR-006).
//!
//! NOTE (per task scope): for US2 the table is built/tested over a **known**
//! uniform section supplied directly by the caller (the conservative classifier +
//! auto-routing land in US3 / Phase 4). [`TableModel::derive`] computes the column
//! set itself (the same union/first-seen logic the future classifier can share).
//!
//! # Virtualized — `TableBody::rows`, NOT `::row` (AD-001 / HINT-004 / FR-008)
//!
//! The grid renders through `egui_extras` [`TableBuilder`] + [`TableBody::rows`]
//! (uniform row height): only the rows whose extent intersects the viewport (plus
//! a bounded overscan) are realized, so the realized-row count is bounded by the
//! viewport height and does **not** grow with the section's total row count. A
//! 100k-row section scrolls/edits with per-frame work independent of N (SC-004 /
//! SC-010). Using `::row` per element would force every row each frame — that is
//! explicitly NOT what this module does.
//!
//! # How an edit flows (FR-006/FR-007/FR-013/FR-014)
//!
//! The view never mutates the buffer directly. Each op resolves the section's
//! [`StructuralPath`] against the live CST, derives a `ron-core`
//! [`StructuralOp`](ron_core::StructuralOp) (a [`ParentRef`](ron_core::ParentRef)
//! over the list plus a child index / field name) and calls
//! [`EditorDocument::apply_structural_edit`](crate::document::EditorDocument::apply_structural_edit),
//! which records ONE E007 undo unit, prints the new CST byte-losslessly, and
//! requests an off-frame reparse (FR-013/FR-014). A blocked op surfaces inline with
//! no byte change and no undo entry. The path→op resolution lives here in
//! `ronin-app` (ADR-0007); the pure CST→CST transform lives in `ron-core`.
//!
//! # Diagnostics surface consistently with the text view (FR-018 / SC-008)
//!
//! Each cell's value CST byte range is matched against the document's
//! [`DiagnosticView`]s (the same E006 set the text view squiggles); an overlapping
//! finding is attached to the cell and shown as an inline indicator with the same
//! severity + code, its detail revealed on focus/hover (FR-018).

use std::cell::Cell as StdCell;
use std::rc::Rc;
use std::time::Instant;

use egui::{Key, RichText, Ui};
use egui_extras::{Column as TableColumn, TableBuilder};

use ron_core::ast;
use ron_core::transform::{ParentRef, StructuralOp};
use ron_core::{BlockedReason, CstDocument, Severity, SyntaxNode};

use crate::byte_to_char::ByteCharIndex;
use crate::diagnostics_map::DiagnosticView;
use crate::document::EditorDocument;
use crate::reparse::ReparseWorker;
use crate::structural::view_state::{resolve_path, FocusSurface, PathStep, StructuralPath};

/// The per-column cell classification across the whole section (data-model
/// `Column.cell_class`).
///
/// A column is [`ColumnClass::Nested`] when **any** record carries a nested
/// collection for that field (so its cells drill in rather than edit inline,
/// FR-006); otherwise it is [`ColumnClass::Scalar`].
///
/// `#[non_exhaustive]` so future cell classifications can be added without a
/// breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ColumnClass {
    /// Every present value in this column is a scalar/simple value — inline-editable.
    Scalar,
    /// At least one record holds a nested collection for this field — drill-in.
    Nested,
}

/// One column of the table schema (data-model `Column`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// The struct field name / map key this column represents.
    pub field_name: String,
    /// The column's cell classification (scalar inline vs nested drill-in).
    pub class: ColumnClass,
}

/// The classification of one [`Cell`] (data-model `Cell.cell_class`).
///
/// `#[non_exhaustive]` so future cell classifications can be added without a
/// breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CellClass {
    /// A present scalar/simple value — edited inline with a type-appropriate
    /// widget (FR-006).
    Scalar,
    /// A present nested collection — NOT edited inline; drills into the tree/form
    /// surface (FR-006/FR-010).
    Nested,
    /// The field is **absent** from this record — a blank cell, visually distinct
    /// from a present-but-empty scalar; editing it adds the absent field (FR-010).
    Blank,
}

/// One cell of the table projection (data-model `Cell`).
///
/// A [`CellClass::Scalar`]/[`CellClass::Nested`] cell carries its value's
/// [`StructuralPath`] ([`value_ref`](Self::value_ref)) so an edit / drill-in can
/// re-resolve it against the live CST; a [`CellClass::Blank`] cell has no
/// `value_ref` (the field does not exist yet — editing it inserts the field).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// The cross-reparse identity of the cell's value node, or `None` for a Blank
    /// (field-absent) cell.
    pub value_ref: Option<StructuralPath>,
    /// The cell's classification.
    pub class: CellClass,
    /// The verbatim value text for a Scalar cell (its literal RON token), a compact
    /// summary for a Nested cell, or `None` for a Blank cell.
    pub text: Option<String>,
    /// Inline diagnostics attached to this cell by CST range (FR-018 / SC-008).
    pub diagnostics: Vec<DiagnosticView>,
}

/// One row of the table projection (data-model `Row`): a section element addressed
/// by its index, with its per-column cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The cross-reparse identity of this row's element (the record node).
    pub element_ref: StructuralPath,
    /// The cells of this row, one per column in [`TableModel::columns`] order.
    pub cells: Vec<Cell>,
}

/// The table view model: a grid projection of ONE uniform section (data-model
/// `TableModel`).
///
/// Built only for a list whose elements share the same record shape (the caller
/// supplies a known-uniform section for US2; the classifier routes in US3). Rows
/// are realized eagerly within a derivation but **rendered** virtualized — only
/// viewport-visible rows are painted (FR-008).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TableModel {
    /// The uniform list/section this table projects (its [`StructuralPath`]); the
    /// parent for row add/remove (FR-005/FR-007).
    pub section_ref: StructuralPath,
    /// The column schema — the union of fields in first-seen order (FR-010).
    pub columns: Vec<Column>,
    /// One [`Row`] per list element, in source order.
    pub rows: Vec<Row>,
}

impl TableModel {
    /// Derive the table model for the list addressed by `section` within `cst`,
    /// with its diagnostics (FR-005/FR-010), or `None` when `section` does not
    /// resolve to a list.
    ///
    /// A pure read over the CST (zero bytes, FR-020). The column set is the union
    /// of every record's field names in first-seen order; an absent field becomes a
    /// [`CellClass::Blank`] cell. A column is [`ColumnClass::Nested`] when any
    /// record holds a nested collection for that field. Each cell's CST range is
    /// matched against `diagnostics` so a cell with a finding carries an inline
    /// indicator consistent with the text view (FR-018 / SC-008).
    ///
    /// Records that are not structs (e.g. a non-record element in a list the caller
    /// presumed uniform) contribute no columns and project all-blank rows — the
    /// derivation degrades safely rather than panicking (FR-019).
    #[must_use]
    pub fn derive(
        cst: &CstDocument,
        section: &StructuralPath,
        diagnostics: &[DiagnosticView],
    ) -> Option<Self> {
        let root = cst.root();
        // Map cell value byte ranges → char ranges for diagnostic attachment in a
        // single amortised-O(n) forward pass (see [`build_byte_char_index`]) rather
        // than an O(file-size) `chars().count()` per cell — the O(cells × file_size)
        // cost that froze the view. Empty (and never queried) when there are no
        // diagnostics, since [`diagnostics_for`] short-circuits on an empty set.
        let index = build_byte_char_index(&root, diagnostics);
        let list_node = resolve_path(&root, section)?;
        let list = ast::List::cast(list_node)?;

        // Pass 1: collect each record's fields in source order + classify each
        // field's value, building the union column schema (first-seen order). A
        // field is Nested for the column if ANY record holds a nested collection.
        let records: Vec<Vec<(String, ast::Value)>> = list
            .items()
            .map(|elem| record_fields(&elem))
            .collect::<Vec<_>>();

        let columns = union_columns(&records);

        // Pass 2: project each record into a row of cells over the column schema.
        let rows = records
            .iter()
            .enumerate()
            .map(|(row_idx, fields)| {
                let element_ref = section.child(PathStep::Index(row_idx));
                let cells = columns
                    .iter()
                    .map(|col| build_cell(&element_ref, col, fields, diagnostics, &index))
                    .collect();
                Row { element_ref, cells }
            })
            .collect();

        Some(Self {
            section_ref: section.clone(),
            columns,
            rows,
        })
    }

    /// The number of rows (records) in the section.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// The cell at `(row, column)`, if both indices are in range.
    #[must_use]
    pub fn cell(&self, row: usize, column: usize) -> Option<&Cell> {
        self.rows.get(row).and_then(|r| r.cells.get(column))
    }
}

/// The `(field_name, value)` pairs of a record element, in source order.
///
/// A struct record yields its fields; a non-struct element yields none (it
/// contributes no columns and projects an all-blank row — FR-019).
fn record_fields(elem: &ast::Value) -> Vec<(String, ast::Value)> {
    match elem {
        ast::Value::Struct(s) => s
            .fields()
            .filter_map(|f| Some((f.name_text()?, f.value()?)))
            .collect(),
        // An enum-variant struct payload also presents named fields; project them
        // so a uniform list of struct-like variants still tables (FR-010).
        ast::Value::EnumVariant(v) => v
            .entries()
            .filter_map(|e| {
                let key = e.key()?.syntax().text();
                let value = e.value()?;
                Some((key, value))
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Build the union column schema across all records: every field name in
/// first-seen order, each classified Nested if any record holds a nested
/// collection for it (FR-010).
fn union_columns(records: &[Vec<(String, ast::Value)>]) -> Vec<Column> {
    let mut columns: Vec<Column> = Vec::new();
    for fields in records {
        for (name, value) in fields {
            let value_nested = is_nested(value);
            if let Some(col) = columns.iter_mut().find(|c| &c.field_name == name) {
                // Promote the column to Nested if any record nests this field.
                if value_nested {
                    col.class = ColumnClass::Nested;
                }
            } else {
                columns.push(Column {
                    field_name: name.clone(),
                    class: if value_nested {
                        ColumnClass::Nested
                    } else {
                        ColumnClass::Scalar
                    },
                });
            }
        }
    }
    columns
}

/// `true` when `value` is a nested collection (struct / map / list / tuple /
/// enum-variant payload) — a drill-in cell, not an inline scalar (FR-006/FR-010).
fn is_nested(value: &ast::Value) -> bool {
    matches!(
        value,
        ast::Value::Struct(_)
            | ast::Value::Map(_)
            | ast::Value::List(_)
            | ast::Value::Tuple(_)
            | ast::Value::EnumVariant(_)
    )
}

/// Build one [`Cell`] for `col` within `element_ref`'s record `fields`.
fn build_cell(
    element_ref: &StructuralPath,
    col: &Column,
    fields: &[(String, ast::Value)],
    diagnostics: &[DiagnosticView],
    index: &ByteCharIndex,
) -> Cell {
    match fields.iter().find(|(name, _)| name == &col.field_name) {
        Some((_, value)) => {
            let value_ref = element_ref.child(PathStep::Field(col.field_name.clone()));
            let diags = diagnostics_for(value.syntax(), diagnostics, index);
            if is_nested(value) {
                Cell {
                    value_ref: Some(value_ref),
                    class: CellClass::Nested,
                    text: Some(summarize(value.syntax())),
                    diagnostics: diags,
                }
            } else {
                Cell {
                    value_ref: Some(value_ref),
                    class: CellClass::Scalar,
                    text: Some(value.syntax().text()),
                    diagnostics: diags,
                }
            }
        }
        // The field is absent from this record → a blank cell (FR-010).
        None => Cell {
            value_ref: None,
            class: CellClass::Blank,
            text: None,
            diagnostics: Vec::new(),
        },
    }
}

/// A compact one-line preview of a nested value node (display-only for a drill-in
/// cell; never normalized for editing).
fn summarize(node: &SyntaxNode) -> String {
    /// Maximum preview length before eliding.
    const MAX: usize = 32;
    let text = node.text().to_string();
    let compact: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() > MAX {
        let truncated: String = compact.chars().take(MAX).collect();
        format!("{truncated}\u{2026}")
    } else {
        compact
    }
}

/// Collect the diagnostics whose char range overlaps `node`'s char range (FR-018).
///
/// `ron-core` ranges are byte ranges; the [`DiagnosticView`] carries char ranges,
/// so we compare against the node's char extent (computed from its byte range over
/// the document text). Mirrors the tree view's attachment so a cell + a tree node
/// surface the same finding consistently (FR-018).
fn diagnostics_for(
    node: &SyntaxNode,
    diagnostics: &[DiagnosticView],
    index: &ByteCharIndex,
) -> Vec<DiagnosticView> {
    if diagnostics.is_empty() {
        return Vec::new();
    }
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

/// Build a byte→char index covering every node boundary in `root`, for the cell
/// diagnostic-attachment char mapping (FR-018), in a single forward pass.
///
/// Mirrors the tree view's index (see `tree::build_byte_char_index`): token
/// boundaries are a superset of node boundaries, so registering every token's
/// start/end resolves any cell value node's `(start, end)` exactly. Returns an empty
/// index when there are no diagnostics, since the char mapping is then never queried.
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
// Document-side op entry points (the path→op→apply_structural_edit pipeline)
// =============================================================================

impl EditorDocument {
    /// Re-resolve the list addressed by `section` against the live buffer to a
    /// [`ParentRef::List`], or [`BlockedReason::TargetNotFound`] (FR-016).
    fn table_resolve_list(&self, section: &StructuralPath) -> Result<ParentRef, BlockedReason> {
        let cst = ron_core::parse(&self.buffer);
        let node = resolve_path(&cst.root(), section).ok_or(BlockedReason::TargetNotFound)?;
        if ast::List::cast(node.clone()).is_some() {
            Ok(ParentRef::List(node))
        } else {
            Err(BlockedReason::InvalidPayload)
        }
    }

    /// Re-resolve the record at `row` within `section` to a [`ParentRef::Struct`]
    /// (or enum-variant payload), against the live buffer (FR-016).
    fn table_resolve_record(
        &self,
        section: &StructuralPath,
        row: usize,
    ) -> Result<(ParentRef, SyntaxNode), BlockedReason> {
        let cst = ron_core::parse(&self.buffer);
        let row_path = section.child(PathStep::Index(row));
        let node = resolve_path(&cst.root(), &row_path).ok_or(BlockedReason::TargetNotFound)?;
        match ast::Value::cast(node.clone()) {
            Some(ast::Value::Struct(_)) => Ok((ParentRef::Struct(node.clone()), node)),
            Some(ast::Value::EnumVariant(_)) => Ok((ParentRef::EnumVariant(node.clone()), node)),
            _ => Err(BlockedReason::InvalidPayload),
        }
    }

    /// The 0-based index of `field` among the record `record`'s entries, if present.
    fn field_index(parent: &ParentRef, record: &SyntaxNode, field: &str) -> Option<usize> {
        match parent {
            ParentRef::Struct(_) => ast::Struct::cast(record.clone())?
                .fields()
                .position(|f| f.name_text().as_deref() == Some(field)),
            ParentRef::EnumVariant(_) => ast::EnumVariant::cast(record.clone())?
                .entries()
                .position(|e| e.key().map(|k| k.syntax().text()).as_deref() == Some(field)),
            _ => None,
        }
    }

    /// Set (or add) the value of the `field` cell of the record at `row` within the
    /// table section, as one undo unit (FR-006 / SC-003).
    ///
    /// When the field is **present** the op is a [`StructuralOp::SetValue`] over its
    /// index; when the field is **absent** (a blank cell) the op is a
    /// [`StructuralOp::InsertField`] appending it — editing a blank cell adds the
    /// previously-absent field (FR-010). Either way it round-trips losslessly and is
    /// a single undo unit.
    pub fn apply_table_set_cell(
        &mut self,
        section: &StructuralPath,
        row: usize,
        field: &str,
        value: String,
        worker: &ReparseWorker,
        now: Instant,
    ) -> Result<(), BlockedReason> {
        let (parent, record) = self.table_resolve_record(section, row)?;
        match Self::field_index(&parent, &record, field) {
            Some(index) => self.apply_structural_edit(
                StructuralOp::SetValue {
                    parent,
                    index,
                    value,
                },
                worker,
                now,
            ),
            // Blank cell: append the absent field (FR-010).
            None => self.apply_structural_edit(
                StructuralOp::InsertField {
                    parent,
                    index: usize::MAX,
                    name: field.to_string(),
                    value,
                },
                worker,
                now,
            ),
        }
    }

    /// Append a row (record element) to the table section, adopting the section's
    /// sibling style, as one undo unit (FR-007 / SC-003).
    ///
    /// `value` is the new element's literal RON text (e.g. `(name: "c", hp: 3)`).
    /// An appended row inherits the collection's layout; appending into an empty
    /// collection uses the document default (AD-005, handled in `ron-core` T004).
    pub fn apply_table_append_row(
        &mut self,
        section: &StructuralPath,
        value: String,
        worker: &ReparseWorker,
        now: Instant,
    ) -> Result<(), BlockedReason> {
        let parent = self.table_resolve_list(section)?;
        self.apply_structural_edit(
            StructuralOp::InsertElement {
                parent,
                index: usize::MAX,
                value,
            },
            worker,
            now,
        )
    }

    /// Delete the row (element) at `row` within the table section, as one undo unit
    /// (FR-007 / SC-003); surviving rows stay byte-identical.
    pub fn apply_table_delete_row(
        &mut self,
        section: &StructuralPath,
        row: usize,
        worker: &ReparseWorker,
        now: Instant,
    ) -> Result<(), BlockedReason> {
        let parent = self.table_resolve_list(section)?;
        self.apply_structural_edit(
            StructuralOp::RemoveElement { parent, index: row },
            worker,
            now,
        )
    }
}

// =============================================================================
// Rendering (egui_extras) — virtualized grid, inline cell editors, keyboard nav
// =============================================================================

/// A deferred, one-per-frame table action the render pass collected from the UI;
/// applied after the immutable model walk so the borrow of `doc` is clean.
enum PendingAction {
    /// Commit a cell edit: set (or add) the `field` cell of `row` to `value`. When
    /// `advance` is set, focus moves in that direction on commit (FR-009) — and when
    /// a forward advance leaves the LAST cell of the LAST row, a new row is appended
    /// and focus lands in its first cell.
    SetCell {
        row: usize,
        field: String,
        value: String,
        advance: Option<CellNav>,
    },
    /// Append a new placeholder row to the section.
    AppendRow,
    /// Delete the row at `row`.
    DeleteRow { row: usize },
    /// Drill into the nested cell at `(row, column)` — open it in the tree/form
    /// surface (FR-006), keyed by its structural path. The originating cell's path +
    /// grid `(row, column)` are recorded as the return target so the drilled-in view
    /// can offer a discoverable back control re-focusing this cell.
    DrillIn {
        path: StructuralPath,
        row: usize,
        column: usize,
    },
}

/// A keyboard cell-navigation intent collected during the render walk (FR-009).
///
/// Moving the active cell is byte-free: it only re-keys edit focus to a different
/// cell's [`StructuralPath`] (which survives a virtualization scroll — FR-016).
#[derive(Debug, Clone, Copy)]
enum CellNav {
    /// Next cell in row order (Tab / Right): last column → first column of next row.
    Next,
    /// Previous cell in row order (Shift-Tab / Left): first column → last column of
    /// the previous row.
    Prev,
    /// The cell directly above (Up).
    Up,
    /// The cell directly below (Down).
    Down,
}

/// Render the virtualized table view for `doc`, driving cell edits + row ops
/// through the one-undo-unit pipeline (E008 / US2 — FR-005..FR-009/FR-018).
///
/// Renders the document's top-level uniform list as a grid: one column per field,
/// one row per record, with [`TableBody::rows`] virtualization (only visible rows
/// realized — AD-001/HINT-004/FR-008). Scalar cells edit inline; a nested cell
/// shows a summary + a drill-in button that opens the subtree in the tree/form
/// surface (FR-006). Row add/remove and a blank-cell "add field" route through the
/// transforms (FR-007/FR-010). A blocked op surfaces inline (FR-003).
///
/// For US2 the table projects the document's top-level value when it is a list (the
/// classifier + per-section routing land in US3); a non-list document shows a
/// fallback notice.
pub fn render_table_view(ui: &mut Ui, doc: &mut EditorDocument, worker: &ReparseWorker) {
    let counter = Rc::new(StdCell::new(0usize));
    render_table_view_counting(ui, doc, worker, &counter);
}

/// Identical to [`render_table_view`] but increments `realized_rows` once per row
/// the virtualization actually realizes (invokes the row closure for).
///
/// This is the test seam behind T027/T034: it lets the egui_kittest harness observe
/// that the realized-row count is bounded by the viewport and independent of the
/// section's total row count (FR-008 / SC-004 / SC-010) without exposing any
/// internal egui detail.
pub fn render_table_view_counting(
    ui: &mut Ui,
    doc: &mut EditorDocument,
    worker: &ReparseWorker,
    realized_rows: &Rc<StdCell<usize>>,
) {
    // Stale marker (FR-015): a user-perceivable notice while a reparse is pending.
    if doc.view_state().is_stale() {
        ui.weak("(updating\u{2026})");
    }

    if doc.parse.is_none() {
        ui.weak("Parsing\u{2026}");
        return;
    }

    // For US2 the section is the document's top-level value (must be a list).
    let section = StructuralPath::root();
    // Reuse the per-parse cached model (derived once per parse generation), instead of
    // re-deriving from the CST every render frame (zero bytes, FR-020). The clone is a
    // cheap structural copy taken so the borrow on `doc` is released before the mutable
    // view-state writes later in this function; the table virtualization still paints
    // only viewport-visible rows below (the cache holds the model, not row widgets).
    let Some(model) = doc.cached_table_model(&section).cloned() else {
        ui.weak("(not a uniform list section)");
        return;
    };
    if model.columns.is_empty() {
        ui.weak("(empty table)");
        return;
    }

    // The current draft for an in-progress cell edit (carried on the view-state).
    let mut draft: Option<(StructuralPath, String)> = doc
        .view_state()
        .edit_focus()
        .map(|f| (f.path.clone(), f.draft.clone()));
    // The active cell's (row, column), recovered from focus by CST identity so it
    // survives a virtualization scroll (FR-016/FR-027): the focus path's last two
    // steps are the row index + field name, which we map back to grid coordinates.
    let active_cell: Option<(usize, usize)> = doc
        .view_state()
        .edit_focus()
        .and_then(|f| cell_coords_of(&model, &f.path));
    let mut pending: Option<PendingAction> = None;
    let mut new_focus: Option<(StructuralPath, FocusSurface, String)> = None;
    let mut clear_focus = false;
    // A keyboard cell-navigation intent captured this frame (FR-009).
    let mut nav: Option<CellNav> = None;

    // Discoverable add-row affordance (FR-009): a visible control above the grid.
    ui.horizontal(|ui| {
        if ui.button("+ row").on_hover_text("Append a row").clicked() {
            pending = Some(PendingAction::AppendRow);
        }
        ui.weak(format!("{} rows", model.row_count()));
    });

    let row_height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
    let columns = &model.columns;
    let rows = &model.rows;
    let realized = Rc::clone(realized_rows);

    let mut builder = TableBuilder::new(ui)
        .id_salt(("ronin_table", section.depth()))
        .striped(true)
        .resizable(true)
        .auto_shrink([false, false])
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
    for _ in columns {
        builder = builder.column(TableColumn::auto().at_least(80.0).clip(true));
    }
    // A trailing column for per-row controls (delete).
    builder = builder.column(TableColumn::auto().at_least(40.0));

    builder
        .header(row_height, |mut header| {
            for col in columns {
                header.col(|ui| {
                    let glyph = match col.class {
                        ColumnClass::Nested => "\u{25B8} ", // drill-in marker
                        ColumnClass::Scalar => "",
                    };
                    ui.strong(format!("{glyph}{}", col.field_name));
                });
            }
            header.col(|ui| {
                ui.strong("");
            });
        })
        .body(|body| {
            // VIRTUALIZED: `TableBody::rows` realizes only viewport-visible rows
            // (NOT `::row` per element — AD-001/HINT-004/FR-008).
            body.rows(row_height, rows.len(), |mut table_row| {
                let row_idx = table_row.index();
                realized.set(realized.get() + 1);
                let row = &rows[row_idx];
                for (col_idx, col) in columns.iter().enumerate() {
                    let cell = &row.cells[col_idx];
                    table_row.col(|ui| {
                        render_cell(
                            ui,
                            CellPos {
                                row: row_idx,
                                column: col_idx,
                            },
                            col,
                            cell,
                            &mut draft,
                            &mut pending,
                            &mut new_focus,
                            &mut clear_focus,
                            &mut nav,
                        );
                    });
                }
                // The per-row delete control (discoverable row removal, FR-007).
                table_row.col(|ui| {
                    if ui
                        .small_button("\u{2716}")
                        .on_hover_text("Delete row")
                        .clicked()
                    {
                        pending = Some(PendingAction::DeleteRow { row: row_idx });
                    }
                });
            });
        });

    // Apply view-state focus changes (byte-free — FR-020).
    if clear_focus {
        doc.view_state_mut().clear_focus();
    } else if let Some((path, surface, text)) = new_focus {
        doc.view_state_mut().set_focus(path, surface, text);
    } else if let Some((path, text)) = draft.clone() {
        if let Some(focus) = doc.view_state_mut().edit_focus_mut() {
            if focus.path == path {
                focus.draft = text;
            }
        }
    }

    // Keyboard cell navigation (FR-009): a Tab/Shift-Tab/arrow with NO pending op
    // re-keys focus to the neighbouring cell. Byte-free — it only moves the active
    // cell (its focus path survives a virtualization scroll, FR-016/FR-027).
    if pending.is_none() {
        if let (Some(dir), Some((row, col))) = (nav, active_cell) {
            if let Some((nr, nc)) = neighbour_cell(&model, row, col, dir) {
                if let Some(target) = focus_target_for(&model, nr, nc) {
                    doc.view_state_mut().set_focus(
                        target.0,
                        FocusSurface::TableCell {
                            row: nr,
                            column: nc,
                        },
                        target.1,
                    );
                }
            }
        }
    }

    // Apply at most one structural op this frame, as one undo unit. A blocked op is
    // surfaced as an inline error notice (FR-003) without changing bytes.
    if let Some(action) = pending {
        let now = Instant::now();
        let result = match action {
            PendingAction::SetCell {
                row,
                field,
                value,
                advance,
            } => {
                doc.view_state_mut().clear_focus();
                let res = doc.apply_table_set_cell(&section, row, &field, value, worker, now);
                // On a successful commit, move focus per the keyboard model (FR-009):
                // Next → next cell; last column → next row; last cell of last row →
                // append a row + land in its first cell. Prev → previous cell.
                if res.is_ok() {
                    if let (Some(dir), Some(col)) = (
                        advance,
                        model.columns.iter().position(|c| c.field_name == field),
                    ) {
                        advance_focus(doc, &model, row, col, dir, worker, now);
                    }
                }
                res
            }
            PendingAction::AppendRow => {
                let value = default_row_text(&model);
                doc.apply_table_append_row(&section, value, worker, now)
            }
            PendingAction::DeleteRow { row } => {
                doc.apply_table_delete_row(&section, row, worker, now)
            }
            PendingAction::DrillIn { path, row, column } => {
                // Drill-in is a focus change onto the nested subtree in the
                // tree/form surface, with a discoverable return path back to this
                // cell (FR-006). It changes zero bytes; record the originating cell
                // as the return target, focus the nested node, and switch to the
                // tree/form surface.
                doc.view_state_mut().set_drill_in_return(
                    crate::structural::view_state::DrillInReturn {
                        cell_path: path.clone(),
                        row,
                        column,
                    },
                );
                doc.view_state_mut()
                    .set_focus(path, FocusSurface::TreeNode, String::new());
                doc.view_state_mut()
                    .set_active_view(crate::structural::view_state::ActiveView::TreeForm);
                Ok(())
            }
        };
        if let Err(reason) = result {
            doc.set_tree_error(blocked_message(reason));
        } else {
            doc.clear_tree_error();
        }
    }

    // Surface the last inline error consistently with the diagnostics model (FR-003).
    if let Some(msg) = doc.tree_error() {
        ui.colored_label(error_color(ui), msg);
    }
}

/// A placeholder element text for an appended row: one field per column with a `0`
/// scalar placeholder for each scalar column (the user then edits each cell). A
/// nested column gets an empty list placeholder so the row stays well-formed.
fn default_row_text(model: &TableModel) -> String {
    let fields: Vec<String> = model
        .columns
        .iter()
        .map(|c| {
            let placeholder = match c.class {
                ColumnClass::Nested => "[]",
                ColumnClass::Scalar => "0",
            };
            format!("{}: {placeholder}", c.field_name)
        })
        .collect();
    format!("({})", fields.join(", "))
}

// =============================================================================
// Keyboard cell navigation (FR-009 / FR-016 / FR-027)
// =============================================================================

/// Recover a focus path's grid `(row, column)` within `model`, or `None` when the
/// path is not a cell of this section.
///
/// The cell path is `section / Index(row) / Field(name)`; we read the row index
/// from the penultimate step and map the field name to its column index. This is a
/// path-keyed lookup (cost proportional to path depth + a column-schema scan, not
/// the section's row count — FR-027), so the active cell survives a virtualization
/// scroll (FR-016).
fn cell_coords_of(model: &TableModel, path: &StructuralPath) -> Option<(usize, usize)> {
    let steps = path.steps();
    let field = match steps.last()? {
        PathStep::Field(name) => name.clone(),
        _ => return None,
    };
    let row = match steps.get(steps.len().checked_sub(2)?)? {
        PathStep::Index(i) => *i,
        _ => return None,
    };
    let column = model.columns.iter().position(|c| c.field_name == field)?;
    Some((row, column))
}

/// The grid neighbour of `(row, col)` in direction `dir`, or `None` at a boundary.
///
/// `Next`/`Prev` wrap across rows in row order (last column → first column of next
/// row, and the inverse); `Up`/`Down` move within the same column. The result is
/// always an in-range cell of `model` (boundaries return `None` rather than
/// wrapping off the grid — FR-009).
fn neighbour_cell(
    model: &TableModel,
    row: usize,
    col: usize,
    dir: CellNav,
) -> Option<(usize, usize)> {
    let cols = model.columns.len();
    let rows = model.row_count();
    if cols == 0 || rows == 0 {
        return None;
    }
    match dir {
        CellNav::Next => {
            if col + 1 < cols {
                Some((row, col + 1))
            } else if row + 1 < rows {
                Some((row + 1, 0))
            } else {
                None
            }
        }
        CellNav::Prev => {
            if col > 0 {
                Some((row, col - 1))
            } else if row > 0 {
                Some((row - 1, cols - 1))
            } else {
                None
            }
        }
        CellNav::Up => (row > 0).then(|| (row - 1, col)),
        CellNav::Down => (row + 1 < rows).then(|| (row + 1, col)),
    }
}

/// The `(focus_path, seed_draft)` for the cell at `(row, col)` of `model`, or
/// `None` when out of range. The seed draft is the cell's current text (so editing
/// continues from its value) or empty for a blank cell.
fn focus_target_for(
    model: &TableModel,
    row: usize,
    col: usize,
) -> Option<(StructuralPath, String)> {
    let column = model.columns.get(col)?;
    let path = model
        .section_ref
        .child(PathStep::Index(row))
        .child(PathStep::Field(column.field_name.clone()));
    let seed = model
        .cell(row, col)
        .and_then(|c| c.text.clone())
        .unwrap_or_default();
    Some((path, seed))
}

/// Move edit focus after a committed cell edit (FR-009): forward (`Next`) advances
/// to the next cell — last column → next row, and the last cell of the last row
/// **appends a new row** and lands focus in its first cell; backward (`Prev`) moves
/// to the previous cell. Byte-free except the explicit append.
fn advance_focus(
    doc: &mut EditorDocument,
    model: &TableModel,
    row: usize,
    col: usize,
    dir: CellNav,
    worker: &ReparseWorker,
    now: Instant,
) {
    let section = &model.section_ref;
    if let Some((nr, nc)) = neighbour_cell(model, row, col, dir) {
        // A neighbour exists: re-key focus to it (its path survives the reparse the
        // commit triggered — FR-016).
        if let Some((path, seed)) = focus_target_for(model, nr, nc) {
            doc.view_state_mut().set_focus(
                path,
                FocusSurface::TableCell {
                    row: nr,
                    column: nc,
                },
                seed,
            );
        }
    } else if matches!(dir, CellNav::Next) {
        // Past the last cell of the last row → append a new row (one undo unit) and
        // land focus in its first cell (FR-009). The appended row is a separate undo
        // unit from the cell commit, matching the "append a row" action.
        let value = default_row_text(model);
        if doc
            .apply_table_append_row(section, value, worker, now)
            .is_ok()
        {
            if let Some(first) = model.columns.first() {
                let new_row = model.row_count();
                let path = section
                    .child(PathStep::Index(new_row))
                    .child(PathStep::Field(first.field_name.clone()));
                doc.view_state_mut().set_focus(
                    path,
                    FocusSurface::TableCell {
                        row: new_row,
                        column: 0,
                    },
                    String::new(),
                );
            }
        }
    }
}

/// The grid position of a cell being rendered.
#[derive(Debug, Clone, Copy)]
struct CellPos {
    /// 0-based row index.
    row: usize,
    /// 0-based column index within the column schema.
    column: usize,
}

/// Render one cell: an inline editor for a Scalar cell, a drill-in for a Nested
/// cell, or an "add field" affordance for a Blank cell (FR-006/FR-010).
#[allow(clippy::too_many_arguments)]
fn render_cell(
    ui: &mut Ui,
    pos: CellPos,
    col: &Column,
    cell: &Cell,
    draft: &mut Option<(StructuralPath, String)>,
    pending: &mut Option<PendingAction>,
    new_focus: &mut Option<(StructuralPath, FocusSurface, String)>,
    clear_focus: &mut bool,
    nav: &mut Option<CellNav>,
) {
    match cell.class {
        CellClass::Nested => {
            // A nested cell is NOT edited inline — it drills into the tree/form
            // surface (FR-006). Show the summary + a drill-in button.
            ui.horizontal(|ui| {
                if let Some(path) = &cell.value_ref {
                    if ui
                        .button(cell.text.clone().unwrap_or_default())
                        .on_hover_text("Open in tree/form")
                        .clicked()
                    {
                        *pending = Some(PendingAction::DrillIn {
                            path: path.clone(),
                            row: pos.row,
                            column: pos.column,
                        });
                    }
                }
                render_cell_diagnostics(ui, cell);
            });
        }
        CellClass::Blank => {
            // A blank (absent-field) cell is visually distinct (an empty/dim
            // affordance) from a present-but-empty scalar (FR-010). Clicking it
            // begins editing, which ADDS the previously-absent field.
            let value_path = StructuralPath::root()
                .child(PathStep::Index(pos.row))
                .child(PathStep::Field(col.field_name.clone()));
            let editing = draft.as_ref().is_some_and(|(p, _)| p == &value_path);
            if editing {
                edit_inline(ui, &value_path, pos, col, draft, pending, clear_focus, nav);
            } else if ui
                .add(egui::Button::new(RichText::new("\u{2014}").weak()))
                .on_hover_text("Add this field")
                .clicked()
            {
                *new_focus = Some((
                    value_path,
                    FocusSurface::TableCell {
                        row: pos.row,
                        column: pos.column,
                    },
                    String::new(),
                ));
            }
        }
        CellClass::Scalar => {
            let Some(path) = &cell.value_ref else {
                ui.weak(cell.text.clone().unwrap_or_default());
                return;
            };
            let editing = draft.as_ref().is_some_and(|(p, _)| p == path);
            ui.horizontal(|ui| {
                if editing {
                    edit_inline(ui, path, pos, col, draft, pending, clear_focus, nav);
                } else if ui.button(cell.text.clone().unwrap_or_default()).clicked() {
                    *new_focus = Some((
                        path.clone(),
                        FocusSurface::TableCell {
                            row: pos.row,
                            column: pos.column,
                        },
                        cell.text.clone().unwrap_or_default(),
                    ));
                }
                render_cell_diagnostics(ui, cell);
            });
        }
    }
}

/// Render the inline text editor for a scalar/blank cell (FR-006/FR-009): commit on
/// Enter / Tab (advancing focus to the next cell), cancel on Esc; Shift-Tab and the
/// arrow keys move the active cell without committing.
#[allow(clippy::too_many_arguments)]
fn edit_inline(
    ui: &mut Ui,
    path: &StructuralPath,
    pos: CellPos,
    col: &Column,
    draft: &mut Option<(StructuralPath, String)>,
    pending: &mut Option<PendingAction>,
    clear_focus: &mut bool,
    nav: &mut Option<CellNav>,
) {
    let mut text = draft.as_ref().map(|(_, t)| t.clone()).unwrap_or_default();
    let resp = ui.text_edit_singleline(&mut text);
    *draft = Some((path.clone(), text.clone()));

    let (enter, tab, shift, esc, up, down) = ui.input(|i| {
        (
            i.key_pressed(Key::Enter),
            i.key_pressed(Key::Tab),
            i.modifiers.shift,
            i.key_pressed(Key::Escape),
            i.key_pressed(Key::ArrowUp),
            i.key_pressed(Key::ArrowDown),
        )
    });

    if esc {
        // Cancel the edit (FR-009): discard the draft, no byte change.
        *clear_focus = true;
    } else if (resp.lost_focus() && enter) || tab {
        // Commit + advance (FR-009): commit the cell value, then move focus forward
        // (Enter / Tab) — next cell; last column → next row; last cell of the last
        // row → append a new row and land focus in its first cell. Shift-Tab commits
        // and moves backward instead.
        *pending = Some(PendingAction::SetCell {
            row: pos.row,
            field: col.field_name.clone(),
            value: text,
            advance: Some(if shift { CellNav::Prev } else { CellNav::Next }),
        });
    } else if up {
        *nav = Some(CellNav::Up);
    } else if down {
        *nav = Some(CellNav::Down);
    }
}

/// Render a cell's inline diagnostic indicator (FR-018 / SC-008).
///
/// The indicator carries the same severity colour as the text view's squiggle and
/// reveals the detail (code / severity / message) on hover, consistent with the
/// text view — no view downgrades or omits a finding the others show.
fn render_cell_diagnostics(ui: &mut Ui, cell: &Cell) {
    for diag in &cell.diagnostics {
        let color = severity_color(ui, diag.severity);
        let glyph = match diag.severity {
            Severity::Error => "\u{2716}",
            Severity::Warning => "\u{26A0}",
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

/// The theme-aware indicator colour for a severity (matches the text + tree views).
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
            "Edit blocked: a field/key with that name already exists here".to_string()
        }
        BlockedReason::TargetNotFound => {
            "Edit could not be applied: the target cell no longer exists".to_string()
        }
        BlockedReason::InvalidPayload => {
            "Edit could not be applied: invalid value or operation".to_string()
        }
        _ => "Edit was blocked".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ron_core::parse;

    fn model_of(src: &str) -> TableModel {
        TableModel::derive(&parse(src), &StructuralPath::root(), &[])
            .expect("top-level list projects a table")
    }

    #[test]
    fn uniform_struct_list_columns_are_union_first_seen() {
        let m = model_of("[(a: 1, b: 2), (a: 3, c: 4)]");
        let cols: Vec<_> = m.columns.iter().map(|c| c.field_name.clone()).collect();
        assert_eq!(cols, vec!["a", "b", "c"]);
        assert_eq!(m.row_count(), 2);
    }

    #[test]
    fn absent_field_is_a_blank_cell() {
        let m = model_of("[(a: 1, b: 2), (a: 3)]");
        // Row 1 has no `b` field → Blank.
        let b_col = m.columns.iter().position(|c| c.field_name == "b").unwrap();
        assert_eq!(m.cell(1, b_col).unwrap().class, CellClass::Blank);
        assert!(m.cell(1, b_col).unwrap().value_ref.is_none());
    }

    #[test]
    fn nested_field_makes_a_nested_column_and_cell() {
        let m = model_of("[(id: 1, tags: [\"x\"]), (id: 2, tags: [])]");
        let tags = m
            .columns
            .iter()
            .position(|c| c.field_name == "tags")
            .unwrap();
        assert_eq!(m.columns[tags].class, ColumnClass::Nested);
        assert_eq!(m.cell(0, tags).unwrap().class, CellClass::Nested);
    }

    #[test]
    fn scalar_cell_carries_verbatim_text_and_value_ref() {
        let m = model_of("[(name: \"a\", hp: 1)]");
        let name = m.cell(0, 0).unwrap();
        assert_eq!(name.class, CellClass::Scalar);
        assert_eq!(name.text.as_deref(), Some("\"a\""));
        assert_eq!(
            name.value_ref,
            Some(StructuralPath::from_steps(vec![
                PathStep::Index(0),
                PathStep::Field("name".to_string())
            ]))
        );
    }

    #[test]
    fn non_list_section_has_no_table() {
        assert!(TableModel::derive(&parse("Point(x: 1)"), &StructuralPath::root(), &[]).is_none());
    }
}
