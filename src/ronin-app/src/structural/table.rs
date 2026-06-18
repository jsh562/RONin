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
//! [`StructuralPath`] against the live CST, derives a `ronin-core`
//! [`StructuralOp`](ronin_core::StructuralOp) (a [`ParentRef`](ronin_core::ParentRef)
//! over the list plus a child index / field name) and calls
//! [`EditorDocument::apply_structural_edit`](crate::document::EditorDocument::apply_structural_edit),
//! which records ONE E007 undo unit, prints the new CST byte-losslessly, and
//! requests an off-frame reparse (FR-013/FR-014). A blocked op surfaces inline with
//! no byte change and no undo entry. The path→op resolution lives here in
//! `ronin-app` (ADR-0007); the pure CST→CST transform lives in `ronin-core`.
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

use ronin_core::ast;
use ronin_core::transform::{ParentRef, StructuralOp};
use ronin_core::{BlockedReason, CstDocument, SyntaxNode};

use crate::byte_to_char::ByteCharIndex;
use crate::diagnostics_map::DiagnosticView;
use crate::document::EditorDocument;
use crate::reparse::ReparseWorker;
use crate::structural::classifier::{scalar_class_of, ScalarClass};
use crate::structural::indicators::{self, TypeIndicator};
use crate::structural::sections::SectionShape;
use crate::structural::tree::TreeNodeKind;
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
    /// A present nested struct / tuple / enum-variant — NOT edited inline; drills
    /// into the tree/form surface (FR-006/FR-010).
    Nested,
    /// A present nested **multi-element collection** (a List or a Map) — NOT edited
    /// inline; opens AS A TABLE in place (the navigator re-keys to its path and
    /// renders the nested collection as a grid), distinct from [`Nested`](Self::Nested)
    /// (a struct/tuple/enum) which drills into the tree/form surface (E013).
    NestedTable,
    /// The field is **absent** from this record — a blank cell, visually distinct
    /// from a present-but-empty scalar; editing it adds the absent field (FR-010).
    Blank,
    /// A read-only value rendered as plain non-editable text: no inline editor, no
    /// drill-in, no `value_ref`. Used for the leading key column of a
    /// [`SectionShape::RecordMap`](super::sections::SectionShape) (the map key is
    /// identity, not an editable field).
    ReadOnly,
}

/// One cell of the table projection (data-model `Cell`).
///
/// A [`CellClass::Scalar`]/[`CellClass::Nested`]/[`CellClass::NestedTable`] cell
/// carries its value's [`StructuralPath`] ([`value_ref`](Self::value_ref)) so an
/// edit / drill-in / open-as-table can re-resolve it against the live CST; a
/// [`CellClass::Blank`] cell has no `value_ref` (the field does not exist yet —
/// editing it inserts the field).
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
    /// The broad scalar type of a [`CellClass::Scalar`] cell (int / float / string /
    /// char / bool / unit), driving the per-cell type indicator glyph + color (E013).
    /// `None` for a nested / blank / read-only cell (those carry no scalar type).
    ///
    /// `pub(crate)` (not `pub`) because [`ScalarClass`] is itself `pub(crate)`; external
    /// callers read the type via the `pub` [`Cell::scalar_type_name`] accessor instead.
    pub(crate) scalar: Option<ScalarClass>,
    /// Inline diagnostics attached to this cell by CST range (FR-018 / SC-008).
    pub diagnostics: Vec<DiagnosticView>,
}

impl Cell {
    /// The cell's scalar type as a short, stable, user-facing word — `Some("integer"
    /// | "float" | "string" | "char" | "bool" | "unit" | "scalar")` for a Scalar cell
    /// carrying a [`ScalarClass`], `None` otherwise (nested / blank / read-only). The
    /// word matches the per-cell indicator's hover text. `pub` so an external test can
    /// assert a cell's type indicator without naming the `pub(crate)` `ScalarClass`.
    #[must_use]
    pub fn scalar_type_name(&self) -> Option<&'static str> {
        self.scalar
            .map(|c| indicators::from_scalar_class(c).word())
    }
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

    /// Derive the table model for `section` of `shape` within `cst`, dispatching to
    /// the per-shape builder (E012 — Table view navigator), or `None` when `section`
    /// does not resolve to the expected kind.
    ///
    /// The produced [`TableModel`] is the **same** `{ section_ref, columns, rows }`
    /// the renderer already consumes, so rendering is shape-agnostic: a RecordList
    /// reuses [`TableModel::derive`]; a RecordMap gets a leading read-only key column
    /// plus the union of its value records' fields; a TupleList gets positional
    /// `.0/.1/…` columns. A pure read over the CST — zero bytes (FR-020).
    #[must_use]
    pub fn derive_section(
        cst: &CstDocument,
        section: &StructuralPath,
        shape: super::sections::SectionShape,
        diagnostics: &[DiagnosticView],
    ) -> Option<Self> {
        use super::sections::SectionShape;
        match shape {
            SectionShape::RecordList => Self::derive(cst, section, diagnostics),
            SectionShape::RecordMap => Self::derive_record_map(cst, section, diagnostics),
            SectionShape::TupleList => Self::derive_tuple_list(cst, section, diagnostics),
        }
    }

    /// Derive a table for the LIVE node at `path`, projecting the best grid for **any**
    /// node shape (E012/E013 — the Table view as a tree-outline navigator that can view
    /// ANY level of the document as a table). Returns `None` only for a **scalar leaf**
    /// (which the outline never selects); every container node — map, list, single
    /// struct, single tuple, single struct-like enum variant — projects a sensible
    /// [`TableModel`].
    ///
    /// Unlike [`derive_section`](Self::derive_section) (which renders the scanner's
    /// strict labeled shapes), this is **permissive for reach**: it never requires
    /// matching record names, and projects whatever grid best fits the live node so the
    /// navigator can render any node the user selects:
    ///
    /// * **Map** → a leading read-only `(key)` column (the map key is identity, not
    ///   data), then: if **every** value is a record, the union of their fields (mixed
    ///   record names allowed); else a single `value` column showing each value's
    ///   summary. Rows are keyed by `path / Key(key_text)`.
    /// * **List** → if **every** element is a record, the union of their fields (mixed
    ///   names allowed); elif **every** element is a tuple, positional `.0 / .1 / …`
    ///   columns; else a single `value` column showing each element's summary. Rows are
    ///   keyed by `path / Index(i)`.
    /// * **Single tuple** → a 1-row positional table, columns `.0 / .1 / …`, the single
    ///   row's `element_ref = path`, each cell at `path / Index(i)`
    ///   ([`project_tuple`](Self::project_tuple)).
    /// * **Single struct / struct-like enum variant** → a field/value table: a leading
    ///   read-only `(field)` column + a `value` column, one row per field, each value
    ///   cell at `path / Field(name)` (a nested value keeps its drill marker so it opens
    ///   as its own table) ([`project_struct`](Self::project_struct)).
    /// * **Scalar leaf** → `None`.
    ///
    /// The result is the **same** `{ section_ref, columns, rows }` the grid renderer
    /// already consumes, so rendering is shape-agnostic. A pure read over the CST —
    /// zero bytes (FR-020).
    #[must_use]
    pub fn derive_any(
        cst: &CstDocument,
        path: &StructuralPath,
        diagnostics: &[DiagnosticView],
    ) -> Option<Self> {
        let root = cst.root();
        let node = resolve_path(&root, path)?;
        match ast::Value::cast(node.clone())? {
            ast::Value::Map(map) => Some(Self::project_map(path, &map, diagnostics, &root)),
            ast::Value::List(list) => Some(Self::project_list(path, &list, diagnostics, &root)),
            // A single tuple → a positional 1-row table.
            ast::Value::Tuple(tuple) => {
                Some(Self::project_tuple(path, &tuple, diagnostics, &root))
            }
            // A single struct / struct-like enum variant → a field/value table.
            ast::Value::Struct(_) | ast::Value::EnumVariant(_) => Some(Self::project_struct(
                path,
                &ast::Value::cast(node)?,
                diagnostics,
                &root,
            )),
            // A scalar leaf (unit / literal / error) is NOT a table — the outline never
            // selects one.
            _ => None,
        }
    }

    /// Project a **single tuple** at `path` into a 1-row positional table (E012): columns
    /// `.0 / .1 / …` (one per member), one row whose `element_ref = path` and whose cells
    /// are each member at `path / Index(i)` via the shared [`tuple_member_cell`] (a nested
    /// member keeps its drill marker; a scalar member is inline-editable).
    fn project_tuple(
        path: &StructuralPath,
        tuple: &ast::Tuple,
        diagnostics: &[DiagnosticView],
        root: &SyntaxNode,
    ) -> Self {
        let index = build_byte_char_index(root, diagnostics);
        let members: Vec<ast::Value> = tuple.items().collect();
        let columns: Vec<Column> = (0..members.len())
            .map(|pos| Column {
                field_name: format!(".{pos}"),
                class: if is_nested(&members[pos]) {
                    ColumnClass::Nested
                } else {
                    ColumnClass::Scalar
                },
            })
            .collect();
        let cells = members
            .iter()
            .enumerate()
            .map(|(pos, value)| {
                let value_ref = path.child(PathStep::Index(pos));
                tuple_member_cell(value_ref, value, diagnostics, &index)
            })
            .collect();
        // The single row IS the tuple itself (element_ref = path).
        let rows = vec![Row {
            element_ref: path.clone(),
            cells,
        }];
        Self {
            section_ref: path.clone(),
            columns,
            rows,
        }
    }

    /// Project a **single struct / struct-like enum variant** at `path` into a field/value
    /// table (E012): a leading read-only `(field)` column (the field name as text) plus a
    /// `value` column, one row per field. Each row's `element_ref` is the field's value
    /// (`path / Field(name)`); the value cell is built via the shared [`value_cell`] at the
    /// same path (a nested value keeps the `▦`/`▸` drill marker so it opens as its own
    /// table). `value` must be a [`Struct`](ast::Value::Struct) or
    /// [`EnumVariant`](ast::Value::EnumVariant); other kinds project an empty table.
    fn project_struct(
        path: &StructuralPath,
        value: &ast::Value,
        diagnostics: &[DiagnosticView],
        root: &SyntaxNode,
    ) -> Self {
        let index = build_byte_char_index(root, diagnostics);
        // The struct/variant fields in source order — reuse the shared record-fields
        // extractor so the field/value projection matches the row-based shapes exactly.
        let fields = record_fields(value);

        let columns = vec![
            Column {
                field_name: "(field)".to_string(),
                class: ColumnClass::Scalar,
            },
            value_column(&fields.iter().map(|(_, v)| v.clone()).collect::<Vec<_>>()),
        ];

        let rows = fields
            .iter()
            .map(|(name, field_value)| {
                let element_ref = path.child(PathStep::Field(name.clone()));
                // The leading read-only field-name cell (the field name is identity).
                let field_cell = Cell {
                    value_ref: None,
                    class: CellClass::ReadOnly,
                    text: Some(name.clone()),
                    scalar: None,
                    diagnostics: Vec::new(),
                };
                // The value cell IS the field value itself (element_ref): a nested value
                // becomes a drill-in / open-as-table cell, a scalar an editable cell.
                let value_cell =
                    value_cell(element_ref.clone(), field_value, diagnostics, &index);
                Row {
                    element_ref,
                    cells: vec![field_cell, value_cell],
                }
            })
            .collect();

        Self {
            section_ref: path.clone(),
            columns,
            rows,
        }
    }

    /// Project a Map at `path` into a table: a leading read-only `(key)` column plus a
    /// value projection (union of record fields when every value is a record, else a
    /// single `value` column). Permissive — mixed record names are allowed (E013).
    fn project_map(
        path: &StructuralPath,
        map: &ast::Map,
        diagnostics: &[DiagnosticView],
        root: &SyntaxNode,
    ) -> Self {
        let index = build_byte_char_index(root, diagnostics);

        // Collect each entry's key text + value, in source order.
        let mut keys: Vec<String> = Vec::new();
        let mut values: Vec<ast::Value> = Vec::new();
        for entry in map.entries() {
            let Some(value) = entry.value() else { continue };
            let key_text = entry.key().map(|k| k.syntax().text()).unwrap_or_default();
            keys.push(key_text);
            values.push(value);
        }

        // Value projection: union of record fields when EVERY value is a record;
        // otherwise a single `value` column showing each value's summary.
        let all_records = !values.is_empty() && values.iter().all(is_record);
        let records: Vec<Vec<(String, ast::Value)>> =
            values.iter().map(record_fields).collect();

        // The leading read-only key column.
        let mut columns = vec![Column {
            field_name: "(key)".to_string(),
            class: ColumnClass::Scalar,
        }];
        if all_records {
            columns.extend(union_columns(&records));
        } else {
            columns.push(value_column(&values));
        }

        let rows = keys
            .iter()
            .zip(values.iter())
            .zip(records.iter())
            .map(|((key_text, value), fields)| {
                let element_ref = path.child(PathStep::Key(key_text.clone()));
                let mut cells = vec![Cell {
                    value_ref: None,
                    class: CellClass::ReadOnly,
                    text: Some(key_text.clone()),
                    scalar: None,
                    diagnostics: Vec::new(),
                }];
                if all_records {
                    for col in columns.iter().skip(1) {
                        cells.push(build_cell(&element_ref, col, fields, diagnostics, &index));
                    }
                } else {
                    // The single `value` cell IS the entry value itself (element_ref).
                    cells.push(value_cell(
                        element_ref.clone(),
                        value,
                        diagnostics,
                        &index,
                    ));
                }
                Row { element_ref, cells }
            })
            .collect();

        Self {
            section_ref: path.clone(),
            columns,
            rows,
        }
    }

    /// Project a List at `path` into a table: union of record fields when every element
    /// is a record (mixed names allowed); positional `.0/.1/…` columns when every
    /// element is a tuple; else a single `value` column of element summaries (E013).
    fn project_list(
        path: &StructuralPath,
        list: &ast::List,
        diagnostics: &[DiagnosticView],
        root: &SyntaxNode,
    ) -> Self {
        let index = build_byte_char_index(root, diagnostics);
        let elements: Vec<ast::Value> = list.items().collect();

        let all_records = !elements.is_empty() && elements.iter().all(is_record);
        let all_tuples = !elements.is_empty()
            && elements
                .iter()
                .all(|e| matches!(e, ast::Value::Tuple(_)));

        if all_records {
            // Union of fields, first-seen — permissive (mixed record names allowed).
            let records: Vec<Vec<(String, ast::Value)>> =
                elements.iter().map(record_fields).collect();
            let columns = union_columns(&records);
            let rows = records
                .iter()
                .enumerate()
                .map(|(row_idx, fields)| {
                    let element_ref = path.child(PathStep::Index(row_idx));
                    let cells = columns
                        .iter()
                        .map(|col| build_cell(&element_ref, col, fields, diagnostics, &index))
                        .collect();
                    Row { element_ref, cells }
                })
                .collect();
            return Self {
                section_ref: path.clone(),
                columns,
                rows,
            };
        }

        if all_tuples {
            // Positional `.0/.1/…` columns; reuse the tuple-list projection inline.
            let tuples: Vec<ast::Tuple> = elements
                .iter()
                .filter_map(|e| match e {
                    ast::Value::Tuple(t) => Some(t.clone()),
                    _ => None,
                })
                .collect();
            let arity = tuples
                .iter()
                .map(|t| t.items().count())
                .max()
                .unwrap_or(0);
            let columns: Vec<Column> = (0..arity)
                .map(|pos| Column {
                    field_name: format!(".{pos}"),
                    class: ColumnClass::Scalar,
                })
                .collect();
            let rows = tuples
                .iter()
                .enumerate()
                .map(|(row_idx, tuple)| {
                    let element_ref = path.child(PathStep::Index(row_idx));
                    let members: Vec<ast::Value> = tuple.items().collect();
                    let cells = (0..arity)
                        .map(|pos| match members.get(pos) {
                            Some(value) => {
                                let value_ref = element_ref.child(PathStep::Index(pos));
                                tuple_member_cell(value_ref, value, diagnostics, &index)
                            }
                            None => Cell {
                                value_ref: None,
                                class: CellClass::Blank,
                                text: None,
                                scalar: None,
                                diagnostics: Vec::new(),
                            },
                        })
                        .collect();
                    Row { element_ref, cells }
                })
                .collect();
            return Self {
                section_ref: path.clone(),
                columns,
                rows,
            };
        }

        // A mixed / scalar list: a single `value` column showing each element's
        // summary (each cell IS the element itself).
        let columns = vec![value_column(&elements)];
        let rows = elements
            .iter()
            .enumerate()
            .map(|(row_idx, value)| {
                let element_ref = path.child(PathStep::Index(row_idx));
                let cell = value_cell(element_ref.clone(), value, diagnostics, &index);
                Row {
                    element_ref,
                    cells: vec![cell],
                }
            })
            .collect();
        Self {
            section_ref: path.clone(),
            columns,
            rows,
        }
    }

    /// Derive a [`SectionShape::RecordMap`](super::sections::SectionShape::RecordMap)
    /// table: a leading read-only `(key)` column whose cells carry the entry key text,
    /// then the union of the value records' fields. Each row's `element_ref` is the
    /// map entry's value (`section / Key(text)`); each value-field cell is
    /// `element_ref / Field(name)`. Returns `None` when `section` is not a map.
    #[must_use]
    fn derive_record_map(
        cst: &CstDocument,
        section: &StructuralPath,
        diagnostics: &[DiagnosticView],
    ) -> Option<Self> {
        let root = cst.root();
        let index = build_byte_char_index(&root, diagnostics);
        let map_node = resolve_path(&root, section)?;
        let map = ast::Map::cast(map_node)?;

        // Collect each entry's key text + the value record's fields, in source order.
        let mut keys: Vec<String> = Vec::new();
        let mut records: Vec<Vec<(String, ast::Value)>> = Vec::new();
        for entry in map.entries() {
            let Some(value) = entry.value() else { continue };
            let key_text = entry
                .key()
                .map(|k| k.syntax().text())
                .unwrap_or_default();
            keys.push(key_text);
            records.push(record_fields(&value));
        }

        // Columns: the leading read-only key column, then the union of value fields.
        let mut columns = vec![Column {
            field_name: "(key)".to_string(),
            class: ColumnClass::Scalar,
        }];
        columns.extend(union_columns(&records));

        let rows = keys
            .iter()
            .zip(records.iter())
            .map(|(key_text, fields)| {
                let element_ref = section.child(PathStep::Key(key_text.clone()));
                // The leading key cell is read-only (the key is identity, not data).
                let mut cells = vec![Cell {
                    value_ref: None,
                    class: CellClass::ReadOnly,
                    text: Some(key_text.clone()),
                    scalar: None,
                    diagnostics: Vec::new(),
                }];
                // Value-field cells reuse the standard record-cell builder.
                for col in columns.iter().skip(1) {
                    cells.push(build_cell(&element_ref, col, fields, diagnostics, &index));
                }
                Row { element_ref, cells }
            })
            .collect();

        Some(Self {
            section_ref: section.clone(),
            columns,
            rows,
        })
    }

    /// Derive a [`SectionShape::TupleList`](super::sections::SectionShape::TupleList)
    /// table: positional `.0/.1/…` columns, one row per tuple element. A scalar tuple
    /// member is an editable [`CellClass::Scalar`] cell; a nested member is a
    /// [`CellClass::Nested`] drill-in. Each row's `element_ref` is `section /
    /// Index(i)`; each cell is `element_ref / Index(pos)`. Returns `None` when
    /// `section` is not a list.
    #[must_use]
    fn derive_tuple_list(
        cst: &CstDocument,
        section: &StructuralPath,
        diagnostics: &[DiagnosticView],
    ) -> Option<Self> {
        let root = cst.root();
        let index = build_byte_char_index(&root, diagnostics);
        let list_node = resolve_path(&root, section)?;
        let list = ast::List::cast(list_node)?;

        let tuples: Vec<ast::Tuple> = list
            .items()
            .filter_map(|elem| match elem {
                ast::Value::Tuple(t) => Some(t),
                _ => None,
            })
            .collect();

        // Column count = the max arity across tuples (uniform by construction, but
        // computed defensively so a degraded shape still projects safely — FR-019).
        let arity = tuples
            .iter()
            .map(|t| t.items().count())
            .max()
            .unwrap_or(0);
        let columns: Vec<Column> = (0..arity)
            .map(|pos| Column {
                field_name: format!(".{pos}"),
                class: ColumnClass::Scalar,
            })
            .collect();

        let rows = tuples
            .iter()
            .enumerate()
            .map(|(row_idx, tuple)| {
                let element_ref = section.child(PathStep::Index(row_idx));
                let members: Vec<ast::Value> = tuple.items().collect();
                let cells = (0..arity)
                    .map(|pos| match members.get(pos) {
                        Some(value) => {
                            let value_ref = element_ref.child(PathStep::Index(pos));
                            let diags = diagnostics_for(value.syntax(), diagnostics, &index);
                            if is_nested(value) {
                                Cell {
                                    value_ref: Some(value_ref),
                                    class: nested_cell_class(value),
                                    text: Some(summarize(value.syntax())),
                                    scalar: None,
                                    diagnostics: diags,
                                }
                            } else {
                                Cell {
                                    value_ref: Some(value_ref),
                                    class: CellClass::Scalar,
                                    text: Some(value.syntax().text()),
                                    scalar: scalar_class_of(value),
                                    diagnostics: diags,
                                }
                            }
                        }
                        // A short tuple (defensive — uniform by construction) → blank.
                        None => Cell {
                            value_ref: None,
                            class: CellClass::Blank,
                            text: None,
                            scalar: None,
                            diagnostics: Vec::new(),
                        },
                    })
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

/// The number of distinct field names across `records` (the union, first-seen) —
/// the value-field column count of a [`SectionShape::RecordMap`](super::sections::SectionShape::RecordMap)
/// (excluding its leading key column). `pub(crate)` for the section scanner's
/// dimension reporting.
pub(crate) fn union_field_count(records: &[Vec<(String, ast::Value)>]) -> usize {
    union_columns(records).len()
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

/// `true` when `value` is a **multi-element collection** (a List or a Map) — the
/// only nested kinds that open AS A TABLE in place ([`CellClass::NestedTable`],
/// E013). A struct / tuple / enum-variant payload is nested but NOT a list/map, so
/// it stays a tree/form drill-in ([`CellClass::Nested`]).
fn is_collection_value(value: &ast::Value) -> bool {
    matches!(value, ast::Value::List(_) | ast::Value::Map(_))
}

/// The drill-in [`CellClass`] for a nested `value`: [`CellClass::NestedTable`] for a
/// List/Map (open-as-table), else [`CellClass::Nested`] for a struct/tuple/enum
/// (tree/form drill-in). Callers must only invoke this when [`is_nested`] is true.
fn nested_cell_class(value: &ast::Value) -> CellClass {
    if is_collection_value(value) {
        CellClass::NestedTable
    } else {
        CellClass::Nested
    }
}

/// `true` when `value` is a record (a struct or a struct-like enum variant) — the
/// permissive (name-agnostic) record test [`TableModel::derive_any`] uses to decide
/// whether a list/map projects union-of-field columns (E013).
fn is_record(value: &ast::Value) -> bool {
    matches!(value, ast::Value::Struct(_) | ast::Value::EnumVariant(_))
}

/// The single fallback `value` column for a heterogeneous/scalar list or map (E013):
/// it is [`ColumnClass::Nested`] when ANY value is a nested collection (so its cells
/// drill in / open as a table), else [`ColumnClass::Scalar`] (inline-editable).
fn value_column(values: &[ast::Value]) -> Column {
    let any_nested = values.iter().any(is_nested);
    Column {
        field_name: "value".to_string(),
        class: if any_nested {
            ColumnClass::Nested
        } else {
            ColumnClass::Scalar
        },
    }
}

/// Build the single `value` cell for an element/entry value addressed by `value_ref`
/// (E013): a nested collection becomes a NestedTable/Nested drill-in cell, a scalar
/// becomes an editable Scalar cell carrying its verbatim text.
fn value_cell(
    value_ref: StructuralPath,
    value: &ast::Value,
    diagnostics: &[DiagnosticView],
    index: &ByteCharIndex,
) -> Cell {
    let diags = diagnostics_for(value.syntax(), diagnostics, index);
    if is_nested(value) {
        Cell {
            value_ref: Some(value_ref),
            class: nested_cell_class(value),
            text: Some(summarize(value.syntax())),
            scalar: None,
            diagnostics: diags,
        }
    } else {
        Cell {
            value_ref: Some(value_ref),
            class: CellClass::Scalar,
            text: Some(value.syntax().text()),
            scalar: scalar_class_of(value),
            diagnostics: diags,
        }
    }
}

/// Build one positional tuple-member cell addressed by `value_ref` (E013 / TupleList):
/// a nested member drills in (NestedTable/Nested), a scalar member is inline-editable.
fn tuple_member_cell(
    value_ref: StructuralPath,
    value: &ast::Value,
    diagnostics: &[DiagnosticView],
    index: &ByteCharIndex,
) -> Cell {
    value_cell(value_ref, value, diagnostics, index)
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
                    class: nested_cell_class(value),
                    text: Some(summarize(value.syntax())),
                    scalar: None,
                    diagnostics: diags,
                }
            } else {
                Cell {
                    value_ref: Some(value_ref),
                    class: CellClass::Scalar,
                    text: Some(value.syntax().text()),
                    scalar: scalar_class_of(value),
                    diagnostics: diags,
                }
            }
        }
        // The field is absent from this record → a blank cell (FR-010).
        None => Cell {
            value_ref: None,
            class: CellClass::Blank,
            text: None,
            scalar: None,
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
/// `ronin-core` ranges are byte ranges; the [`DiagnosticView`] carries char ranges,
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
// Breadcrumb — stateless, path-derived (E013 / Part A3)
// =============================================================================

/// One segment of the table-view breadcrumb (E013): a prefix of the selected
/// section's [`StructuralPath`], its display label, and whether it is a clickable
/// navigation target (it resolves to a List or Map — the only openable kinds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreadcrumbSegment {
    /// The prefix path this segment addresses (clicking navigates here).
    pub path: StructuralPath,
    /// The segment's display label (`root`, a field name, `(key)`, or `[i]`).
    pub label: String,
    /// `true` when the prefix resolves to a List or Map (an openable table target):
    /// the segment is clickable; otherwise it is shown weak / non-clickable.
    pub clickable: bool,
}

/// Compute the breadcrumb segments for the selected section `path` against `cst`
/// (E013 / Part A3) — stateless, derived each frame from the path (no new state).
///
/// Produces one segment per prefix of `path` (from the root down to `path` itself):
/// the root prefix is labeled `root`; each subsequent segment is labeled from its
/// trailing [`PathStep`] (a field name, `(key)` for a map key, `[i]` for an index).
/// A segment is **clickable** iff its prefix resolves to a List or Map (the only
/// openable kinds) — so the breadcrumb only offers navigation to table-able ancestors.
#[must_use]
pub fn breadcrumb_segments(
    cst: &CstDocument,
    path: &StructuralPath,
) -> Vec<BreadcrumbSegment> {
    let root = cst.root();
    let steps = path.steps();
    let mut out = Vec::with_capacity(steps.len() + 1);

    // The root prefix.
    out.push(BreadcrumbSegment {
        path: StructuralPath::root(),
        label: "root".to_string(),
        clickable: prefix_is_openable(&root, &StructuralPath::root()),
    });

    // Each deeper prefix, labeled from its trailing step.
    let mut acc: Vec<PathStep> = Vec::new();
    for step in steps {
        acc.push(step.clone());
        let prefix = StructuralPath::from_steps(acc.clone());
        out.push(BreadcrumbSegment {
            label: step_label(step),
            clickable: prefix_is_openable(&root, &prefix),
            path: prefix,
        });
    }
    out
}

/// The display label for one [`PathStep`] in a breadcrumb (E013): a field/variant
/// field by its name, a map key as `(text)`-normalized text, an index as `[i]`.
fn step_label(step: &PathStep) -> String {
    match step {
        PathStep::Field(name) | PathStep::VariantField(name) => name.clone(),
        PathStep::Key(text) => text.clone(),
        PathStep::Index(i) => format!("[{i}]"),
    }
}

/// `true` when `prefix` resolves to a List or Map within `root` (an openable table
/// target — the breadcrumb-clickability test, E013).
fn prefix_is_openable(root: &SyntaxNode, prefix: &StructuralPath) -> bool {
    matches!(
        resolve_path(root, prefix).and_then(ast::Value::cast),
        Some(ast::Value::List(_) | ast::Value::Map(_))
    )
}

// =============================================================================
// Document-side op entry points (the path→op→apply_structural_edit pipeline)
// =============================================================================

impl EditorDocument {
    /// Re-resolve the list addressed by `section` against the live buffer to a
    /// [`ParentRef::List`], or [`BlockedReason::TargetNotFound`] (FR-016).
    fn table_resolve_list(&self, section: &StructuralPath) -> Result<ParentRef, BlockedReason> {
        let cst = ronin_core::parse(&self.buffer);
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
        let cst = ronin_core::parse(&self.buffer);
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

    /// Set the value of the cell addressed by `value_ref` (its value node's
    /// structural path) as one undo unit (E012 — RecordMap / TupleList cell edits).
    ///
    /// Unlike [`apply_table_set_cell`](Self::apply_table_set_cell) (which is keyed by
    /// `(row, field)` and supports the RecordList blank-cell "add absent field"), this
    /// is a shape-agnostic in-place replace of an **existing** cell value: it
    /// re-resolves the value node against the live buffer, derives its parent
    /// collection + child index, and issues a [`StructuralOp::SetValue`]. This covers
    /// a RecordMap value-field cell (parent = the value struct/variant) and a
    /// TupleList positional cell (parent = the tuple). Returns
    /// [`BlockedReason::TargetNotFound`] when the node vanished and
    /// [`BlockedReason::InvalidPayload`] when its parent is not an editable collection.
    pub fn apply_table_set_cell_at(
        &mut self,
        value_ref: &StructuralPath,
        value: String,
        worker: &ReparseWorker,
        now: Instant,
    ) -> Result<(), BlockedReason> {
        let cst = ronin_core::parse(&self.buffer);
        let node = resolve_path(&cst.root(), value_ref).ok_or(BlockedReason::TargetNotFound)?;
        let (parent, index) = Self::parent_and_index_of(&node).ok_or(BlockedReason::InvalidPayload)?;
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

    /// Derive the enclosing collection [`ParentRef`] + the 0-based child index of the
    /// value node `node` within it (the address [`StructuralOp::SetValue`] needs), or
    /// `None` when `node` is not a value-position child of an editable collection.
    fn parent_and_index_of(node: &SyntaxNode) -> Option<(ParentRef, usize)> {
        use ronin_core::SyntaxKind;
        let immediate = node.parent()?;
        match immediate.kind() {
            // A struct field / map entry / enum-variant payload entry wraps the value.
            SyntaxKind::StructField | SyntaxKind::MapEntry => {
                let collection = immediate.parent()?;
                let index = collection
                    .children()
                    .filter(|c| {
                        matches!(c.kind(), SyntaxKind::StructField | SyntaxKind::MapEntry)
                    })
                    .position(|c| c == immediate)?;
                let parent = match collection.kind() {
                    SyntaxKind::Struct => ParentRef::Struct(collection),
                    SyntaxKind::Map => ParentRef::Map(collection),
                    SyntaxKind::EnumVariant => ParentRef::EnumVariant(collection),
                    _ => return None,
                };
                Some((parent, index))
            }
            // A list / tuple element: the parent IS the collection; index by position.
            SyntaxKind::List | SyntaxKind::Tuple => {
                let index = immediate
                    .children()
                    .filter(|c| ast::Value::cast(c.clone()).is_some())
                    .position(|c| &c == node)?;
                let parent = if immediate.kind() == SyntaxKind::List {
                    ParentRef::List(immediate)
                } else {
                    ParentRef::Tuple(immediate)
                };
                Some((parent, index))
            }
            _ => None,
        }
    }

    /// Append a row (record element) to the table section, adopting the section's
    /// sibling style, as one undo unit (FR-007 / SC-003).
    ///
    /// `value` is the new element's literal RON text (e.g. `(name: "c", hp: 3)`).
    /// An appended row inherits the collection's layout; appending into an empty
    /// collection uses the document default (AD-005, handled in `ronin-core` T004).
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
    /// and focus lands in its first cell. `value_ref` is the cell's value path (when
    /// known) used to commit a RecordMap / TupleList cell in place by path; for a
    /// RecordList blank cell it is `None` (the commit adds the absent field by name).
    SetCell {
        row: usize,
        field: String,
        value: String,
        value_ref: Option<StructuralPath>,
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
    /// Open a nested List/Map cell **as a table in place** (E013): re-key the
    /// navigator's selected section to the cell's path and STAY in the Table view (no
    /// switch to tree/form). Byte-free — it only writes view state.
    OpenAsTable { path: StructuralPath },
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

/// Render the virtualized table view for the `section` of `shape` within `doc`,
/// driving cell edits + row ops through the one-undo-unit pipeline (E008/E012 —
/// FR-005..FR-009/FR-018).
///
/// Renders the section as a grid: one column per field (or positional `.N` / leading
/// `(key)`), one row per record/entry/tuple, with [`TableBody::rows`] virtualization
/// (only visible rows realized — AD-001/HINT-004/FR-008). Scalar cells edit inline; a
/// nested cell shows a summary + a drill-in button that opens the subtree in the
/// tree/form surface (FR-006). For a [`SectionShape::RecordList`] section, row
/// add/remove and a blank-cell "add field" route through the transforms
/// (FR-007/FR-010); for RecordMap / TupleList those row-op controls are suppressed
/// (no well-defined uniform append yet) but scalar cells stay editable. A blocked op
/// surfaces inline (FR-003).
pub fn render_table_view(
    ui: &mut Ui,
    doc: &mut EditorDocument,
    worker: &ReparseWorker,
    section: &StructuralPath,
    shape: SectionShape,
) {
    let counter = Rc::new(StdCell::new(0usize));
    render_table_view_counting(ui, doc, worker, section, shape, &counter);
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
    section: &StructuralPath,
    shape: SectionShape,
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

    // Row add/remove + blank-cell "add absent field" are RecordList-only: the other
    // shapes have no well-defined uniform append yet, so their row-op controls are
    // suppressed (cell edits still commit in place).
    let row_ops = shape == SectionShape::RecordList;
    let section = section.clone();
    // Reuse the per-parse cached model (derived once per parse generation), instead of
    // re-deriving from the CST every render frame (zero bytes, FR-020). The clone is a
    // cheap structural copy taken so the borrow on `doc` is released before the mutable
    // view-state writes later in this function; the table virtualization still paints
    // only viewport-visible rows below (the cache holds the model, not row widgets).
    let Some(model) = doc.cached_table_model(&section, shape).cloned() else {
        ui.weak("(this section is no longer table-able)");
        return;
    };
    if model.columns.is_empty() {
        ui.weak("(empty table)");
        return;
    }

    render_table_grid(ui, doc, worker, &section, &model, row_ops, realized_rows);
}

/// Render `model`'s virtualized grid for the section at `section`, driving cell edits,
/// keyboard navigation, drill-in / open-as-table, and (when `row_ops`) row add/remove
/// through the one-undo-unit pipeline. The shared rendering core both the shape-based
/// ([`render_table_view_counting`]) and the path-based navigator
/// ([`render_table_view_any_counting`]) entry points call once they have resolved a
/// [`TableModel`].
fn render_table_grid(
    ui: &mut Ui,
    doc: &mut EditorDocument,
    worker: &ReparseWorker,
    section: &StructuralPath,
    model: &TableModel,
    row_ops: bool,
    realized_rows: &Rc<StdCell<usize>>,
) {
    let section = section.clone();
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
        .and_then(|f| cell_coords_of(model, &f.path));
    let mut pending: Option<PendingAction> = None;
    let mut new_focus: Option<(StructuralPath, FocusSurface, String)> = None;
    let mut clear_focus = false;
    // A keyboard cell-navigation intent captured this frame (FR-009).
    let mut nav: Option<CellNav> = None;

    // Discoverable add-row affordance (FR-009): a visible control above the grid —
    // RecordList only (the other shapes have no well-defined uniform append yet).
    ui.horizontal(|ui| {
        if row_ops && ui.button("+ row").on_hover_text("Append a row").clicked() {
            pending = Some(PendingAction::AppendRow);
        }
        ui.weak(format!("{} rows", model.row_count()));
    });

    let row_height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
    let columns = &model.columns;
    let rows = &model.rows;
    let realized = Rc::clone(realized_rows);

    // HORIZONTAL SCROLL (E013): a wide table (e.g. the `hulls` RecordMap = key column
    // + 12 fields ≈ 13 columns) exceeds the viewport width. egui_extras 0.34's
    // `TableBuilder` only scrolls VERTICALLY (its body's internal `ScrollArea` is
    // hard-coded `[false, vscroll]`), so to make every column reachable we (a) give
    // each column an INTRINSIC width via `TableColumn::initial(..)` — an *absolute*
    // size that `Sizing::to_lengths` keeps verbatim regardless of available width, so
    // the columns' total can exceed the viewport — and (b) wrap the whole table in a
    // HORIZONTAL-ONLY outer `ScrollArea`. The outer area scrolls X only (vertical is
    // disabled), so it does NOT take over the body's VERTICAL virtualization: the
    // Table body keeps its own vertical `ScrollArea` + `TableBody::rows`, so the
    // realized-row count stays bounded by the viewport (SC-010 unchanged).
    egui::ScrollArea::horizontal()
        .id_salt(("ronin_table_hscroll", section.depth()))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let mut builder = TableBuilder::new(ui)
                .id_salt(("ronin_table", section.depth()))
                .striped(true)
                .resizable(true)
                .auto_shrink([false, false])
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
            for _ in columns {
                // An intrinsic (absolute, resizable) width per column: it keeps this
                // width regardless of available width, so wide tables overflow the
                // viewport and the outer horizontal scrollbar appears. NOT clipped — a
                // clipped/auto column would shrink to fit and never overflow.
                builder = builder.column(TableColumn::initial(140.0).at_least(80.0));
            }
            // A trailing column for per-row controls (delete) — only when row-ops apply.
            if row_ops {
                builder = builder.column(TableColumn::initial(40.0).at_least(40.0));
            }

            builder
                .header(row_height, |mut header| {
                    for (col_idx, col) in columns.iter().enumerate() {
                        header.col(|ui| {
                            render_column_header(ui, col, rows, col_idx);
                        });
                    }
                    if row_ops {
                        header.col(|ui| {
                            ui.strong("");
                        });
                    }
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
                                    &section,
                                    CellPos {
                                        row: row_idx,
                                        column: col_idx,
                                    },
                                    col,
                                    cell,
                                    row_ops,
                                    &mut draft,
                                    &mut pending,
                                    &mut new_focus,
                                    &mut clear_focus,
                                    &mut nav,
                                );
                            });
                        }
                        // The per-row delete control (discoverable row removal, FR-007)
                        // — RecordList only.
                        if row_ops {
                            table_row.col(|ui| {
                                if ui
                                    .small_button("\u{2716}")
                                    .on_hover_text("Delete row")
                                    .clicked()
                                {
                                    pending = Some(PendingAction::DeleteRow { row: row_idx });
                                }
                            });
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
            if let Some((nr, nc)) = neighbour_cell(model, row, col, dir) {
                if let Some(target) = focus_target_for(model, nr, nc) {
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
                value_ref,
                advance,
            } => {
                doc.view_state_mut().clear_focus();
                // RecordList commits by `(row, field)` (and adds an absent field for a
                // blank cell); RecordMap / TupleList commit the existing cell value in
                // place by its structural path (FR-006).
                let res = match (row_ops, &value_ref) {
                    (false, Some(path)) => {
                        doc.apply_table_set_cell_at(path, value, worker, now)
                    }
                    _ => doc.apply_table_set_cell(&section, row, &field, value, worker, now),
                };
                // On a successful commit, move focus per the keyboard model (FR-009):
                // Next → next cell; last column → next row; last cell of last row →
                // append a row + land in its first cell. Prev → previous cell.
                if res.is_ok() {
                    if let (Some(dir), Some(col)) = (
                        advance,
                        model.columns.iter().position(|c| c.field_name == field),
                    ) {
                        advance_focus(doc, model, row_ops, row, col, dir, worker, now);
                    }
                }
                res
            }
            PendingAction::AppendRow => {
                let value = default_row_text(model);
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
            PendingAction::OpenAsTable { path } => {
                // Open the nested List/Map as a table in place: re-key the navigator's
                // selected section and STAY in the Table view (E013). Routed through
                // `navigate_table_section` so back/forward history records the drill-in
                // (E016). Byte-free.
                doc.view_state_mut().clear_focus();
                doc.view_state_mut().navigate_table_section(path);
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

/// Render the **path-projected** table for the LIVE node at `path` (E013 — "open any
/// nested collection as a table"), driving the same cell edits / drill-in /
/// open-as-table / (RecordList-only) row ops as [`render_table_view`].
///
/// Unlike [`render_table_view`] (keyed by `(section, shape)` for the scanner's strict
/// labeled shapes), this projects the node permissively through
/// [`TableModel::derive_any`] so the navigator can render any drilled-into List/Map.
/// Row add/remove + blank-cell "add field" stay RecordList-only — they are enabled
/// only when the node is a List of records (the [`apply_table_append_row`] /
/// blank-cell pipeline is well-defined only there); other projected shapes (a map, a
/// tuple list, a scalar/mixed list) render with editable scalar cells but no row ops.
pub fn render_table_view_any(
    ui: &mut Ui,
    doc: &mut EditorDocument,
    worker: &ReparseWorker,
    path: &StructuralPath,
) {
    let counter = Rc::new(StdCell::new(0usize));
    render_table_view_any_counting(ui, doc, worker, path, &counter);
}

/// Identical to [`render_table_view_any`] but increments `realized_rows` once per row
/// the virtualization realizes (the headless test seam, mirroring
/// [`render_table_view_counting`]).
pub fn render_table_view_any_counting(
    ui: &mut Ui,
    doc: &mut EditorDocument,
    worker: &ReparseWorker,
    path: &StructuralPath,
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

    // Row ops are RecordList-only — enable them only when the live node is a List of
    // records (where the append / blank-cell pipeline is well-defined). Computed off
    // the live CST before the model borrow / mutable view-state writes below.
    let row_ops = path_is_record_list(doc, path);

    let path = path.clone();
    let Some(model) = doc.cached_table_model_any(&path).cloned() else {
        ui.weak("(this is not a table-able collection)");
        return;
    };
    if model.columns.is_empty() {
        ui.weak("(empty table)");
        return;
    }

    render_table_grid(ui, doc, worker, &path, &model, row_ops, realized_rows);
}

/// `true` when the live node at `path` is a List whose **every** element is a record
/// (struct / struct-like enum variant) — the only [`derive_any`](TableModel::derive_any)
/// projection where RecordList row ops (append / delete / blank-cell add) are
/// well-defined (E013 / Part A5). A pure read over the CST.
fn path_is_record_list(doc: &EditorDocument, path: &StructuralPath) -> bool {
    let Some(parse) = doc.parse.as_ref() else {
        return false;
    };
    let root = parse.cst.root();
    let Some(node) = resolve_path(&root, path) else {
        return false;
    };
    match ast::Value::cast(node) {
        Some(ast::Value::List(list)) => {
            let mut any = false;
            for elem in list.items() {
                any = true;
                if !is_record(&elem) {
                    return false;
                }
            }
            any
        }
        _ => false,
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
/// Matches `path` against each cell's authoritative [`Cell::value_ref`] (and the
/// RecordList blank-cell path `section / Index(row) / Field(name)` for a not-yet-
/// present field). This is model-driven so it works across all section shapes
/// (RecordList `Field`, RecordMap `Key`/`Field`, TupleList `Index`/`Index`). The
/// active cell survives a virtualization scroll (FR-016): focus is keyed to the
/// path, not a screen row.
fn cell_coords_of(model: &TableModel, path: &StructuralPath) -> Option<(usize, usize)> {
    for (r, row) in model.rows.iter().enumerate() {
        for (c, cell) in row.cells.iter().enumerate() {
            // A present cell matches by its value_ref.
            if cell.value_ref.as_ref() == Some(path) {
                return Some((r, c));
            }
            // A RecordList blank cell has no value_ref yet; match the prospective
            // field path the editor uses (section / Index(row) / Field(name)).
            if cell.class == CellClass::Blank {
                if let Some(col) = model.columns.get(c) {
                    let blank_path = model
                        .section_ref
                        .child(PathStep::Index(r))
                        .child(PathStep::Field(col.field_name.clone()));
                    if &blank_path == path {
                        return Some((r, c));
                    }
                }
            }
        }
    }
    None
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
/// `None` when out of range or not editable (a read-only key cell). The seed draft
/// is the cell's current text (so editing continues from its value) or empty for a
/// blank cell.
///
/// The path is the cell's authoritative [`Cell::value_ref`] when present (works for
/// RecordMap `Field` cells and TupleList `Index` cells); for a RecordList blank cell
/// (no value_ref yet) it is the prospective `section / Index(row) / Field(name)`
/// path the editor adds the field at.
fn focus_target_for(
    model: &TableModel,
    row: usize,
    col: usize,
) -> Option<(StructuralPath, String)> {
    let cell = model.cell(row, col)?;
    // A read-only key cell and a nested drill-in / open-as-table cell are not
    // inline-editable — never a keyboard-nav focus target (E013).
    if matches!(
        cell.class,
        CellClass::ReadOnly | CellClass::Nested | CellClass::NestedTable
    ) {
        return None;
    }
    let path = match &cell.value_ref {
        Some(p) => p.clone(),
        // A blank cell: the prospective add-field path (RecordList only).
        None => {
            let column = model.columns.get(col)?;
            model
                .section_ref
                .child(PathStep::Index(row))
                .child(PathStep::Field(column.field_name.clone()))
        }
    };
    let seed = cell.text.clone().unwrap_or_default();
    Some((path, seed))
}

/// Move edit focus after a committed cell edit (FR-009): forward (`Next`) advances
/// to the next cell — last column → next row, and the last cell of the last row
/// **appends a new row** and lands focus in its first cell; backward (`Prev`) moves
/// to the previous cell. Byte-free except the explicit append.
#[allow(clippy::too_many_arguments)]
fn advance_focus(
    doc: &mut EditorDocument,
    model: &TableModel,
    row_ops: bool,
    row: usize,
    col: usize,
    dir: CellNav,
    worker: &ReparseWorker,
    now: Instant,
) {
    let section = &model.section_ref;
    if let Some((nr, nc)) = neighbour_cell(model, row, col, dir) {
        // A neighbour exists: re-key focus to it (its path survives the reparse the
        // commit triggered — FR-016). A read-only key cell yields no focus target.
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
    } else if row_ops && matches!(dir, CellNav::Next) {
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
/// cell, an "add field" affordance for a Blank cell (RecordList only — FR-006/
/// FR-010), or plain read-only text for a ReadOnly key cell (E012).
///
/// `section` addresses the table section (for building a blank cell's prospective
/// add-field path); `row_ops` is true only for a RecordList section (the only shape
/// where a blank cell becomes an editable add-field affordance).
#[allow(clippy::too_many_arguments)]
fn render_cell(
    ui: &mut Ui,
    section: &StructuralPath,
    pos: CellPos,
    col: &Column,
    cell: &Cell,
    row_ops: bool,
    draft: &mut Option<(StructuralPath, String)>,
    pending: &mut Option<PendingAction>,
    new_focus: &mut Option<(StructuralPath, FocusSurface, String)>,
    clear_focus: &mut bool,
    nav: &mut Option<CellNav>,
) {
    match cell.class {
        CellClass::ReadOnly => {
            // A read-only value (e.g. a RecordMap key) — plain non-editable text,
            // no inline editor, no drill-in (E012).
            ui.label(cell.text.clone().unwrap_or_default());
        }
        CellClass::Nested => {
            // A nested struct / tuple / enum cell is NOT edited inline — it drills
            // into the tree/form surface (FR-006). Show the value's KIND icon via the
            // shared [`TypeIndicator`] (▢ struct / ◇ tuple / ◈ enum — E014, the same
            // glyph the tree paints) + the summary + a drill-in button.
            ui.horizontal(|ui| {
                let indicator = indicators::from_tree_kind(nested_cell_kind(cell));
                indicator.show(ui);
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
        CellClass::NestedTable => {
            // A nested List/Map cell opens AS A TABLE in place (E013): the list/map
            // icon itself (▤ / ▦ via the shared [`TypeIndicator`] — E014) prefixes the
            // clickable "open as table" button; there is no separate drill marker.
            // Clicking re-keys the navigator's selected section to this cell's path and
            // STAYS in the Table view (no switch to tree/form).
            ui.horizontal(|ui| {
                let indicator = indicators::from_tree_kind(nested_cell_kind(cell));
                // The list/map icon goes through the shared fixed-width slot (E014),
                // BEFORE the button; the button text is the summary only (no embedded
                // glyph), so the open-as-table cells align with the other cell icons.
                indicator.show(ui).on_hover_text(indicator.word());
                if let Some(path) = &cell.value_ref {
                    let summary = cell.text.clone().unwrap_or_default();
                    if ui
                        .button(summary)
                        .on_hover_text("Open as table")
                        .clicked()
                    {
                        *pending = Some(PendingAction::OpenAsTable { path: path.clone() });
                    }
                }
                render_cell_diagnostics(ui, cell);
            });
        }
        CellClass::Blank => {
            // A blank (absent-field) cell is visually distinct (an empty/dim
            // affordance) from a present-but-empty scalar (FR-010). Clicking it
            // begins editing, which ADDS the previously-absent field. This add-field
            // affordance is RecordList-only; for other shapes a blank cell renders
            // as inert text (no well-defined add yet).
            let value_path = section
                .child(PathStep::Index(pos.row))
                .child(PathStep::Field(col.field_name.clone()));
            if !row_ops {
                ui.weak("\u{2014}");
                return;
            }
            let editing = draft.as_ref().is_some_and(|(p, _)| p == &value_path);
            if editing {
                // A blank cell has no existing value_ref — commit adds the field.
                edit_inline(ui, &value_path, None, pos, col, draft, pending, clear_focus, nav);
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
                // A small, weak, theme-aware type glyph immediately left of the value
                // (E013) — a prefix label inside the cell's existing horizontal layout,
                // so it never breaks the inline editor / focus / keyboard navigation.
                render_scalar_type_indicator(ui, cell);
                if editing {
                    edit_inline(
                        ui,
                        path,
                        Some(path.clone()),
                        pos,
                        col,
                        draft,
                        pending,
                        clear_focus,
                        nav,
                    );
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
///
/// `value_ref` is the cell's existing value path (`Some` for a present scalar,
/// `None` for a RecordList blank cell whose commit adds the absent field). It is
/// carried into the [`PendingAction::SetCell`] so a RecordMap / TupleList cell
/// commits in place by path.
#[allow(clippy::too_many_arguments)]
fn edit_inline(
    ui: &mut Ui,
    path: &StructuralPath,
    value_ref: Option<StructuralPath>,
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
            value_ref,
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
        // Route the glyph + color + word through the shared [`TypeIndicator`] (E014)
        // so the cell diagnostic indicator matches the tree's and the text view's.
        let indicator = indicators::from_severity(diag.severity);
        // Draw the cell diagnostic glyph through the shared fixed-width slot (E014) so
        // it aligns with the other indicators in the cell row.
        indicator.show(ui).on_hover_text(format!(
            "{} [{}]: {}",
            indicator.word(),
            diag.code.code(),
            diag.message
        ));
    }
}

/// The inline-error text colour (re-uses the shared error indicator's colour for FR-003).
fn error_color(ui: &Ui) -> egui::Color32 {
    TypeIndicator::Error.color(ui)
}

// =============================================================================
// Type indicators (E013/E014) — per-cell / per-column type glyphs + colors via the
// single shared [`TypeIndicator`] system, so the table and the tree draw the SAME
// glyph + color for the same concept (`structural::indicators`).
// =============================================================================

/// The [`TreeNodeKind`] a [`CellClass::Nested`] / [`CellClass::NestedTable`] cell's
/// nested value belongs to, so the table reuses the shared [`TypeIndicator`] for
/// nested type indicators (E014).
///
/// `NestedTable` is only ever produced for a List or a Map (open-as-table); `Nested`
/// for a struct / tuple / enum-variant (tree/form drill-in). A column's
/// representative nested cell is consulted, defaulting safely to a generic kind.
#[must_use]
fn nested_cell_kind(cell: &Cell) -> TreeNodeKind {
    match cell.class {
        // A NestedTable cell is a List or a Map; resolve which from the live value if
        // available, else default to List (the more common open-as-table case).
        CellClass::NestedTable => nested_value_kind(cell).unwrap_or(TreeNodeKind::List),
        CellClass::Nested => nested_value_kind(cell).unwrap_or(TreeNodeKind::Struct),
        _ => TreeNodeKind::Leaf,
    }
}

/// Best-effort: the [`TreeNodeKind`] of a nested cell's value, parsed from its summary
/// text's leading delimiter (the cell carries a compact summary, not the live node).
/// `[` → list, `{` → map, `(` → struct/tuple, an identifier → enum/struct.
#[must_use]
fn nested_value_kind(cell: &Cell) -> Option<TreeNodeKind> {
    let text = cell.text.as_deref()?.trim_start();
    let first = text.chars().next()?;
    Some(match first {
        '[' => TreeNodeKind::List,
        '{' => TreeNodeKind::Map,
        // A bare `( .. )` is an anonymous struct/tuple; with a leading Ident it is a
        // named struct or enum variant. The exact split is not load-bearing for the
        // indicator color — both map onto the struct/tuple/enum palette.
        '(' => TreeNodeKind::Tuple,
        _ => TreeNodeKind::Struct,
    })
}

/// Render the per-cell type indicator immediately left of a [`CellClass::Scalar`]
/// cell's value (E014). A no-op when the cell carries no scalar class. The glyph is
/// drawn via the shared [`TypeIndicator`] (same glyph/size/color as the tree — NEVER
/// `.small()`), as a prefix label inside the cell's existing horizontal layout, so it
/// never interferes with the inline editor / focus / keyboard navigation.
fn render_scalar_type_indicator(ui: &mut Ui, cell: &Cell) {
    if let Some(class) = cell.scalar {
        let indicator = indicators::from_scalar_class(class);
        // The fixed-width slot keeps the value start-X aligned across scalar cells.
        indicator.show(ui).on_hover_text(indicator.word());
    }
}

/// The representative [`ScalarClass`] of a Scalar `column` across `rows` (E013): the
/// dominant present scalar type (most frequent), tie-broken by first appearance. Used
/// to color/glyph the column header. `None` when no scalar cell is present.
#[must_use]
fn column_scalar_class(rows: &[Row], col_idx: usize) -> Option<ScalarClass> {
    // first-seen order of classes + their counts, so we can pick the dominant one and
    // tie-break by first appearance deterministically.
    let mut seen: Vec<(ScalarClass, usize)> = Vec::new();
    for row in rows {
        if let Some(cell) = row.cells.get(col_idx) {
            if cell.class == CellClass::Scalar {
                if let Some(class) = cell.scalar {
                    if let Some(entry) = seen.iter_mut().find(|(c, _)| *c == class) {
                        entry.1 += 1;
                    } else {
                        seen.push((class, 1));
                    }
                }
            }
        }
    }
    // Max by count; `max_by_key` keeps the FIRST max on ties (stable), which is the
    // first-present class — exactly the documented tie-break.
    seen.iter().max_by_key(|(_, n)| *n).map(|(c, _)| *c)
}

/// The representative nested [`TreeNodeKind`] of a [`ColumnClass::Nested`] column, for
/// its header [`TypeIndicator`] (E014): the kind of the column's first nested cell,
/// defaulting safely to List.
#[must_use]
fn column_nested_kind(rows: &[Row], col_idx: usize) -> TreeNodeKind {
    rows.iter()
        .filter_map(|row| row.cells.get(col_idx))
        .find(|c| matches!(c.class, CellClass::Nested | CellClass::NestedTable))
        .map(nested_cell_kind)
        .unwrap_or(TreeNodeKind::List)
}

/// The header [`TypeIndicator`] for one `column` over `rows` (E014): a Scalar column
/// uses its dominant cell type's indicator ([`from_scalar_class`](indicators::from_scalar_class));
/// a Nested column uses its representative nested kind's indicator
/// ([`from_tree_kind`](indicators::from_tree_kind) — ▤/▦ for a list/map column, ▢/◇/◈
/// for a struct/tuple/enum drill-in column). `None` when no representative type is
/// derivable (an all-blank scalar column).
#[must_use]
fn column_type_indicator(
    column: &Column,
    rows: &[Row],
    col_idx: usize,
) -> Option<TypeIndicator> {
    match column.class {
        ColumnClass::Scalar => column_scalar_class(rows, col_idx).map(indicators::from_scalar_class),
        ColumnClass::Nested => Some(indicators::from_tree_kind(column_nested_kind(rows, col_idx))),
    }
}

/// Render one column header (E014): the column's shared [`TypeIndicator`] glyph (same
/// glyph/size/color as the tree paints) + the field name (strong).
fn render_column_header(ui: &mut Ui, column: &Column, rows: &[Row], col_idx: usize) {
    let indicator = column_type_indicator(column, rows, col_idx);
    ui.horizontal(|ui| {
        // Draw the header icon through the shared fixed-width slot (E014) so column
        // headers align with one another.
        if let Some(indicator) = indicator {
            indicator.show(ui).on_hover_text(indicator.word());
        }
        ui.strong(&column.field_name);
    });
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
    use ronin_core::parse;

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
        // A List value cell opens AS A TABLE in place (E013), distinct from a
        // struct/tuple/enum cell which stays a tree/form drill-in (`Nested`).
        assert_eq!(m.cell(0, tags).unwrap().class, CellClass::NestedTable);
    }

    #[test]
    fn struct_and_tuple_cells_stay_nested_while_list_map_cells_open_as_table() {
        // E013: only List/Map value cells become NestedTable; struct/tuple/enum stay
        // Nested (tree/form drill-in).
        let m = model_of("[(l: [1], m: {1: 2}, s: (k: 1), t: (1, 2))]");
        let col = |name: &str| m.columns.iter().position(|c| c.field_name == name).unwrap();
        assert_eq!(m.cell(0, col("l")).unwrap().class, CellClass::NestedTable);
        assert_eq!(m.cell(0, col("m")).unwrap().class, CellClass::NestedTable);
        assert_eq!(m.cell(0, col("s")).unwrap().class, CellClass::Nested);
        assert_eq!(m.cell(0, col("t")).unwrap().class, CellClass::Nested);
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

    // =========================================================================
    // E013 — per-cell / per-column type indicators
    // =========================================================================

    #[test]
    fn scalar_cells_carry_their_scalar_class() {
        // Each present scalar cell carries the classifier's broad type so the per-cell
        // indicator can glyph/color it; a nested/blank cell carries none.
        let m = model_of("[(i: 1, f: 1.5, s: \"x\", c: 'q', b: true, n: [1])]");
        let col = |name: &str| m.columns.iter().position(|c| c.field_name == name).unwrap();

        assert_eq!(m.cell(0, col("i")).unwrap().scalar, Some(ScalarClass::Integer));
        assert_eq!(m.cell(0, col("f")).unwrap().scalar, Some(ScalarClass::Float));
        assert_eq!(m.cell(0, col("s")).unwrap().scalar, Some(ScalarClass::Str));
        assert_eq!(m.cell(0, col("c")).unwrap().scalar, Some(ScalarClass::Char));
        assert_eq!(m.cell(0, col("b")).unwrap().scalar, Some(ScalarClass::Bool));
        // A nested-list cell (NestedTable) carries NO scalar class.
        assert_eq!(m.cell(0, col("n")).unwrap().scalar, None);
    }

    #[test]
    fn blank_and_readonly_cells_carry_no_scalar_class() {
        let m = model_of("[(a: 1, b: 2), (a: 3)]");
        let b_col = m.columns.iter().position(|c| c.field_name == "b").unwrap();
        // Row 1 lacks `b` → Blank, no scalar class.
        assert_eq!(m.cell(1, b_col).unwrap().class, CellClass::Blank);
        assert_eq!(m.cell(1, b_col).unwrap().scalar, None);
    }

    #[test]
    fn scalar_type_name_exposes_the_word() {
        let m = model_of("[(i: 1, s: \"x\")]");
        assert_eq!(m.cell(0, 0).unwrap().scalar_type_name(), Some("integer"));
        assert_eq!(m.cell(0, 1).unwrap().scalar_type_name(), Some("string"));
    }

    #[test]
    fn each_scalar_type_has_a_distinct_font_covered_glyph() {
        // The scalar classes map (via the shared `TypeIndicator`) to distinct glyphs.
        // `Other` collapses onto the generic `Scalar` indicator (•), so the six
        // typed classes (integer/float/str/char/bool/unit) must be mutually distinct.
        let classes = [
            ScalarClass::Integer,
            ScalarClass::Float,
            ScalarClass::Str,
            ScalarClass::Char,
            ScalarClass::Bool,
            ScalarClass::Unit,
        ];
        let mut glyphs: Vec<&str> = classes
            .iter()
            .map(|c| indicators::from_scalar_class(*c).glyph())
            .collect();
        glyphs.sort_unstable();
        glyphs.dedup();
        assert_eq!(glyphs.len(), classes.len(), "every typed scalar glyph is distinct");
    }

    #[test]
    fn column_representative_scalar_class_is_the_dominant_type() {
        // Column `a` is integer in 2 of 3 rows, string in 1 → dominant = integer.
        let m = model_of("[(a: 1), (a: 2), (a: \"x\")]");
        // (this list is non-uniform for the classifier, but `derive` is permissive)
        let a = m.columns.iter().position(|c| c.field_name == "a").unwrap();
        assert_eq!(column_scalar_class(&m.rows, a), Some(ScalarClass::Integer));
    }

    #[test]
    fn nested_column_indicator_uses_the_shared_kind_glyph() {
        // A column whose cells are Lists carries the List indicator (▤, the list icon
        // is itself the open-as-table affordance); a column whose cells are structs
        // carries the Struct indicator (▢ tree/form drill-in) — the SAME glyphs the
        // tree paints (E014).
        let m = model_of("[(items: [1], meta: Meta(k: 1)), (items: [2], meta: Meta(k: 2)), (items: [3], meta: Meta(k: 3))]");
        let items = m.columns.iter().position(|c| c.field_name == "items").unwrap();
        let meta = m.columns.iter().position(|c| c.field_name == "meta").unwrap();
        assert_eq!(column_nested_kind(&m.rows, items), TreeNodeKind::List);
        assert_eq!(
            column_type_indicator(&m.columns[items], &m.rows, items),
            Some(TypeIndicator::List),
            "List column → list indicator (▤)"
        );
        assert_eq!(
            column_type_indicator(&m.columns[meta], &m.rows, meta),
            Some(TypeIndicator::Struct),
            "struct column → struct indicator (▢)"
        );
    }
}
