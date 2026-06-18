//! The conservative uniform-section classifier (E008 Phase 4 / US3 — AD-002,
//! FR-010/FR-011/FR-025).
//!
//! # What this decides (and why it is conservative)
//!
//! Given a CST list node, [`classify`] returns a [`Verdict`]: either **table-eligible**
//! (the list is a uniform section the table view can safely render — with its
//! [column schema](Verdict::column_schema)) or a **fallback** to tree/form carrying
//! the exhaustive [`FallbackReason`] that explains *why* (FR-025).
//!
//! The rule is deliberately conservative (AD-002): it never coerces heterogeneous,
//! variant-mixed, or arbitrarily nested data into a grid (FR-011). A list is
//! table-eligible only when **every** element is a record of the **same**
//! struct/variant **name** and the same-named fields agree on a scalar/simple value
//! **type** across elements:
//!
//! * **Name** — all elements share the same struct name (anonymous structs share
//!   the empty name) or the same enum-variant name. A mismatch → [`FallbackReason::NameMismatch`].
//! * **Field set** — the column set is the **union of every record's field names in
//!   first-seen order** (reusing the same union logic [`TableModel::derive`](super::table::TableModel::derive)
//!   already has). A field merely *absent* from some records keeps the list uniform
//!   (it renders as a blank cell), so optional/missing fields do not disqualify a
//!   list (FR-010).
//! * **No type conflict** — a same-named field that appears with **conflicting
//!   scalar value types** across elements (e.g. `1` in one record, `"x"` in another)
//!   makes the list non-uniform → [`FallbackReason::TypeConflict`] (FR-010).
//! * **Cells are scalar/simple** — a nested-collection cell is allowed: it renders
//!   as a drill-in, not a coercion blocker (FR-006/FR-010). But a column that is
//!   nested in *every* record where it appears, with **no** scalar cell anywhere in
//!   the list, means the list is "all-nested" → [`FallbackReason::NestedOnly`]: there
//!   is nothing to edit as a grid, so tree/form is the right surface.
//! * **Record list** — every element must be a struct or struct-like enum-variant
//!   record. A non-record element (a bare scalar, a tuple, a bare list) → the list
//!   is not a record-list → [`FallbackReason::NotARecordList`].
//! * **Size / shape guards** — an empty list → [`FallbackReason::Empty`]; a uniform
//!   list of **≤2 elements** defaults to tree/form (the user can still override it to
//!   a table, FR-012) → [`FallbackReason::TooSmall`]; a list node that does not
//!   resolve / parse → [`FallbackReason::Unparseable`].
//!
//! # Bounded, off-frame cost (FR-026)
//!
//! Classification is a single pass over the list's elements and **short-circuits on
//! the first shape mismatch** — it never re-scans after reaching a verdict. It runs
//! off the per-frame path (on the landed reparse projection); a single list's cost is
//! linear in its element count (FR-026). It is a pure read over the CST: classifying
//! changes **zero** document bytes (FR-020).

use ronin_core::ast;
use ronin_core::{SyntaxKind, SyntaxNode};

use super::table::{Column, ColumnClass};

/// Why a list fell back to the tree/form view instead of rendering as a table
/// (FR-025 / data-model `UniformSectionClassifier.fallback_reason`).
///
/// This set is **exhaustive and closed**: every not-eligible list maps to exactly
/// one reason, so no non-uniform shape is ever left unclassified. It is surfaced to
/// the user on the section boundary indicator (FR-025), not used only internally.
///
/// Even though the producer ([`classify`]) covers every case, this is marked
/// `#[non_exhaustive]` so downstream `match`es keep a wildcard arm and the set can
/// grow in a future revision without a breaking change (per plan naming conventions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FallbackReason {
    /// The elements have differing struct/variant names (a name mismatch) — the
    /// list mixes record shapes, so it is not a uniform section (FR-010/FR-011).
    NameMismatch,
    /// A same-named field appears with conflicting scalar value types across
    /// elements (e.g. an integer in one record, a string in another) (FR-010).
    TypeConflict,
    /// Every cell of the list is a nested collection (no scalar cell anywhere) —
    /// there is nothing to edit as a grid, so tree/form drill-in is the surface.
    NestedOnly,
    /// The list contains a non-record element (a bare scalar, a tuple, a nested
    /// list) — it is not a list of records (FR-011).
    NotARecordList,
    /// A uniform list of **≤2 elements** defaults to tree/form (FR-010); the user
    /// can still override it to a table (FR-012).
    TooSmall,
    /// The list is empty — no rows to project (Edge Cases / FR-019).
    Empty,
    /// The list node does not resolve / parse to a list (e.g. an error-recovered
    /// region) — it degrades safely to tree/form rather than crashing (FR-019).
    Unparseable,
}

impl FallbackReason {
    /// A short, user-facing phrase for the section boundary indicator (FR-025).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::NameMismatch => "mixed record names",
            Self::TypeConflict => "conflicting field types",
            Self::NestedOnly => "only nested values",
            Self::NotARecordList => "not a list of records",
            Self::TooSmall => "too few elements (\u{2264}2)",
            Self::Empty => "empty list",
            Self::Unparseable => "unparseable region",
        }
    }
}

/// The classifier's decision for one list (data-model `UniformSectionClassifier`
/// verdict).
///
/// Exactly one of the two states holds for any list:
///
/// * **table-eligible** — [`table_eligible`](Self::table_eligible) is `true`,
///   [`column_schema`](Self::column_schema) carries the union-of-fields column set,
///   and [`fallback_reason`](Self::fallback_reason) is `None`;
/// * **fallback** — `table_eligible` is `false`, `fallback_reason` is `Some(reason)`
///   from the exhaustive [`FallbackReason`] set, and `column_schema` is empty.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Verdict {
    /// `true` when the list is a uniform section the table view may render (FR-010).
    pub table_eligible: bool,
    /// The union-of-fields column schema (first-seen order), populated only when
    /// [`table_eligible`](Self::table_eligible) (FR-010); empty on a fallback.
    pub column_schema: Vec<Column>,
    /// The closed-set reason the list fell back to tree/form, or `None` when it is
    /// table-eligible (FR-025).
    pub fallback_reason: Option<FallbackReason>,
}

impl Verdict {
    /// An eligible verdict carrying its `columns` schema.
    #[must_use]
    fn eligible(columns: Vec<Column>) -> Self {
        Self {
            table_eligible: true,
            column_schema: columns,
            fallback_reason: None,
        }
    }

    /// A fallback verdict carrying its `reason`.
    #[must_use]
    fn fallback(reason: FallbackReason) -> Self {
        Self {
            table_eligible: false,
            column_schema: Vec::new(),
            fallback_reason: Some(reason),
        }
    }
}

/// A scalar value's broad type class, used to detect a same-named field appearing
/// with conflicting value types across elements (FR-010 type-conflict rule) and to
/// drive the table view's per-cell / per-column type indicators (E013 — table type
/// glyphs/colors mirroring the tree view's per-kind icons).
///
/// Nested collections are not assigned a scalar class — they are tracked separately
/// (a nested cell is a drill-in, never a type-conflict blocker, FR-006/FR-010).
///
/// `pub(crate)` so the table view can carry the classifier's verdict per scalar cell
/// (`Cell::scalar`) and render a consistent type indicator; `Copy` so it threads
/// through projection + render cheaply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScalarClass {
    /// An integer literal.
    Integer,
    /// A floating-point literal.
    Float,
    /// A string / raw-string literal.
    Str,
    /// A character literal.
    Char,
    /// A boolean literal.
    Bool,
    /// The unit value `()`.
    Unit,
    /// Any other scalar/simple token not otherwise classified (kept distinct so it
    /// never silently unifies with a known class).
    Other,
}

/// Classify the `list_node` into a [`Verdict`] (AD-002 / FR-010/FR-011/FR-025).
///
/// `list_node` is the candidate section's CST [`SyntaxNode`] (a `SyntaxKind::List`).
/// A pure read over the CST — zero bytes (FR-020). The scan is linear in the list's
/// element count and short-circuits on the first shape mismatch (FR-026). Returns a
/// fallback [`Verdict`] (never panics) for any awkward shape — an empty list, a
/// ≤2-element list, a non-record element, a name mismatch, a type conflict, an
/// all-nested list, or an unparseable region (FR-019).
#[must_use]
pub fn classify(list_node: &SyntaxNode) -> Verdict {
    // A node that does not resolve to a list degrades safely (FR-019).
    let Some(list) = ast::List::cast(list_node.clone()) else {
        return Verdict::fallback(FallbackReason::Unparseable);
    };

    let elements: Vec<ast::Value> = list.items().collect();
    if elements.is_empty() {
        return Verdict::fallback(FallbackReason::Empty);
    }

    // Pass 1 (short-circuiting): every element must be a record of the SAME name.
    let mut shape: Option<String> = None;
    let mut records: Vec<Vec<(String, ast::Value)>> = Vec::with_capacity(elements.len());
    for elem in &elements {
        let Some(this_shape) = record_shape(elem) else {
            // A non-record element (scalar / tuple / bare list) — never a grid.
            return Verdict::fallback(FallbackReason::NotARecordList);
        };
        match &shape {
            None => shape = Some(this_shape),
            Some(existing) if *existing != this_shape => {
                // First name mismatch → short-circuit (FR-026).
                return Verdict::fallback(FallbackReason::NameMismatch);
            }
            Some(_) => {}
        }
        records.push(record_fields(elem));
    }

    // Pass 2: reconcile the field set (union, first-seen) + detect type conflicts.
    //
    // For each column we track the first scalar class seen (if any) and whether any
    // record nested that field — reusing the union/first-seen + nested-promotion
    // logic the table model already encodes (here over an additional ScalarClass
    // check for the conflict rule).
    let mut columns: Vec<Column> = Vec::new();
    // Parallel to `columns`: the first scalar class observed for each column, used
    // only to detect conflicts (None until a scalar cell is seen).
    let mut scalar_class: Vec<Option<ScalarClass>> = Vec::new();
    // Whether ANY cell anywhere in the list is a scalar (vs all-nested → NestedOnly).
    let mut any_scalar_cell = false;

    for fields in &records {
        for (name, value) in fields {
            let nested = is_nested(value);
            if !nested {
                any_scalar_cell = true;
            }
            match columns.iter().position(|c| &c.field_name == name) {
                Some(idx) => {
                    // Existing column: promote to Nested if this record nests it.
                    if nested {
                        columns[idx].class = ColumnClass::Nested;
                    } else if let Some(class) = scalar_class_of(value) {
                        match scalar_class[idx] {
                            // First scalar value for this column sets its class.
                            None => scalar_class[idx] = Some(class),
                            // A differing scalar class on the same field → conflict.
                            Some(existing) if existing != class => {
                                return Verdict::fallback(FallbackReason::TypeConflict);
                            }
                            Some(_) => {}
                        }
                    }
                }
                None => {
                    // New column in first-seen order.
                    columns.push(Column {
                        field_name: name.clone(),
                        class: if nested {
                            ColumnClass::Nested
                        } else {
                            ColumnClass::Scalar
                        },
                    });
                    scalar_class.push(if nested { None } else { scalar_class_of(value) });
                }
            }
        }
    }

    // An all-nested list (no scalar cell anywhere) has nothing to edit as a grid —
    // tree/form drill-in is the right surface (FR-006/FR-010).
    if !any_scalar_cell {
        return Verdict::fallback(FallbackReason::NestedOnly);
    }

    // A uniform list of ≤2 elements defaults to tree/form (override available,
    // FR-010/FR-012). This guard runs AFTER the uniformity checks so a small list
    // that is ALSO non-uniform reports the more specific reason — but a small,
    // genuinely-uniform list reports TooSmall.
    if elements.len() <= 2 {
        return Verdict::fallback(FallbackReason::TooSmall);
    }

    Verdict::eligible(columns)
}

/// The record **name** of an element (empty string for an anonymous struct), or
/// `None` when it is not a record (struct / struct-like enum variant) — FR-011.
///
/// `pub(crate)` so the section scanner can reuse the exact same record-detection
/// rule when classifying a [`SectionShape::RecordMap`](super::sections::SectionShape::RecordMap).
pub(crate) fn record_shape(elem: &ast::Value) -> Option<String> {
    match elem {
        // A struct: its name (empty for an anonymous `( .. )` struct).
        ast::Value::Struct(s) => Some(s.name_text().unwrap_or_default()),
        // A struct-like enum variant: its variant name.
        ast::Value::EnumVariant(v) => Some(v.name_text().unwrap_or_default()),
        // Anything else (scalar, tuple, list, map, unit, error) is not a record.
        _ => None,
    }
}

/// The `(field_name, value)` pairs of a record element, in source order.
///
/// Mirrors [`TableModel::derive`](super::table::TableModel::derive)'s record reader
/// so the classifier's column union matches the table's exactly. `pub(crate)` so the
/// section scanner can reuse it for [`SectionShape::RecordMap`](super::sections::SectionShape::RecordMap).
pub(crate) fn record_fields(elem: &ast::Value) -> Vec<(String, ast::Value)> {
    match elem {
        ast::Value::Struct(s) => s
            .fields()
            .filter_map(|f| Some((f.name_text()?, f.value()?)))
            .collect(),
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

/// `true` when `value` is a nested collection (drill-in cell, not an inline scalar).
///
/// Matches the table model's `is_nested` classification (FR-006/FR-010).
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

/// The broad scalar class of a non-nested value, for the type-conflict rule
/// (FR-010) and the table view's per-cell type indicator, or `None` when the value
/// is nested (which is tracked separately).
///
/// `pub(crate)` so [`Cell`](super::table::Cell) can carry it per scalar cell.
pub(crate) fn scalar_class_of(value: &ast::Value) -> Option<ScalarClass> {
    match value {
        ast::Value::Unit(_) => Some(ScalarClass::Unit),
        ast::Value::Literal(lit) => Some(match lit.token_kind() {
            Some(SyntaxKind::Integer) => ScalarClass::Integer,
            Some(SyntaxKind::Float) => ScalarClass::Float,
            Some(SyntaxKind::String | SyntaxKind::RawString) => ScalarClass::Str,
            Some(SyntaxKind::Char) => ScalarClass::Char,
            Some(SyntaxKind::TrueKw | SyntaxKind::FalseKw) => ScalarClass::Bool,
            _ => ScalarClass::Other,
        }),
        // Nested values carry no scalar class.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ronin_core::ast;
    use ronin_core::parse;

    /// Parse `src` (whose top-level value must be a list) and classify it.
    fn classify_src(src: &str) -> Verdict {
        let cst = parse(src);
        let root = cst.root();
        let top = ast::Document::cast(root)
            .and_then(|d| d.value())
            .expect("a top-level value")
            .syntax()
            .clone();
        classify(&top)
    }

    #[test]
    fn same_shape_list_is_eligible_with_union_columns() {
        let v = classify_src("[(a: 1, b: 2), (a: 3, c: 4), (a: 5, b: 6)]");
        assert!(v.table_eligible);
        assert!(v.fallback_reason.is_none());
        let cols: Vec<_> = v
            .column_schema
            .iter()
            .map(|c| c.field_name.clone())
            .collect();
        assert_eq!(cols, vec!["a", "b", "c"], "union of fields, first-seen");
    }

    #[test]
    fn absent_field_stays_uniform() {
        // The third record lacks `b`; absent != non-uniform (FR-010).
        let v = classify_src("[(a: 1, b: 2), (a: 3, b: 4), (a: 5)]");
        assert!(
            v.table_eligible,
            "an absent field stays uniform (blank cell)"
        );
    }

    #[test]
    fn name_mismatch_falls_back() {
        let v = classify_src("[A(x: 1), B(x: 2), A(x: 3)]");
        assert!(!v.table_eligible);
        assert_eq!(v.fallback_reason, Some(FallbackReason::NameMismatch));
    }

    #[test]
    fn conflicting_field_type_falls_back() {
        let v = classify_src("[(a: 1), (a: \"x\"), (a: 3)]");
        assert!(!v.table_eligible);
        assert_eq!(v.fallback_reason, Some(FallbackReason::TypeConflict));
    }

    #[test]
    fn all_nested_cells_fall_back() {
        let v = classify_src("[(a: [1]), (a: [2]), (a: [3])]");
        assert!(!v.table_eligible);
        assert_eq!(v.fallback_reason, Some(FallbackReason::NestedOnly));
    }

    #[test]
    fn non_record_element_falls_back() {
        let v = classify_src("[(a: 1), 2, (a: 3)]");
        assert!(!v.table_eligible);
        assert_eq!(v.fallback_reason, Some(FallbackReason::NotARecordList));
    }

    #[test]
    fn too_small_uniform_list_falls_back() {
        let v = classify_src("[(a: 1), (a: 2)]");
        assert!(!v.table_eligible);
        assert_eq!(v.fallback_reason, Some(FallbackReason::TooSmall));
    }

    #[test]
    fn empty_list_falls_back() {
        let v = classify_src("[]");
        assert!(!v.table_eligible);
        assert_eq!(v.fallback_reason, Some(FallbackReason::Empty));
    }

    #[test]
    fn non_list_node_is_unparseable() {
        // A non-list value classified directly degrades safely (FR-019).
        let cst = parse("Point(x: 1)");
        let top = ast::Document::cast(cst.root())
            .and_then(|d| d.value())
            .expect("value")
            .syntax()
            .clone();
        let v = classify(&top);
        assert!(!v.table_eligible);
        assert_eq!(v.fallback_reason, Some(FallbackReason::Unparseable));
    }

    #[test]
    fn nested_column_with_some_scalar_stays_eligible() {
        // `tags` is nested in every record, but the list has scalar cells (`id`), so
        // it is still a uniform section with a nested drill-in column (FR-006/FR-010).
        let v = classify_src("[(id: 1, tags: [\"x\"]), (id: 2, tags: []), (id: 3, tags: [\"y\"])]");
        assert!(v.table_eligible);
        let tags = v
            .column_schema
            .iter()
            .find(|c| c.field_name == "tags")
            .expect("tags column");
        assert_eq!(tags.class, ColumnClass::Nested);
    }

    #[test]
    fn struct_like_enum_variants_table_when_uniform() {
        // A list of same-named struct-like variants is a uniform section (FR-010).
        let v = classify_src("[E(x: 1), E(x: 2), E(x: 3)]");
        assert!(v.table_eligible);
    }
}
