//! The whole-document **table-section scanner** for the Table-view navigator
//! (E012 — Table view navigator).
//!
//! # Why scan (and not just classify the root)
//!
//! The original Table view classified only the document's **top-level value** and
//! showed nothing when the root was not a uniform list — which is the common case
//! (a config struct, a struct→map of records, …). This scanner walks the whole
//! CST once and reports **every** table-able section anywhere in the document, so
//! the navigator can list them and let the user pick one to render as the existing
//! virtualized grid.
//!
//! # What counts as table-able (and what does not)
//!
//! A section is one of three [`SectionShape`]s:
//!
//! * [`SectionShape::RecordList`] — a list of same-named records (the existing
//!   [`classifier::classify`] rule). The navigator shows small uniform lists too,
//!   so the ≤2-element `TooSmall` auto-guard is **not** applied here: a list that is
//!   only `TooSmall` is still listed. Genuinely non-uniform lists
//!   (name-mismatch / type-conflict / not-a-record-list / all-nested / empty /
//!   unparseable) are skipped.
//! * [`SectionShape::RecordMap`] — a map whose **every** value is a record (struct
//!   or struct-like enum variant) of the **same** name. It renders with a leading
//!   read-only key column plus the union of the value records' fields.
//! * [`SectionShape::TupleList`] — a list whose **every** element is a tuple, all of
//!   the same arity (≥1 element, arity ≥1). It renders with positional `.0/.1/…`
//!   columns.
//!
//! Scalar lists and scalar maps are intentionally **not** tabular — there is
//! nothing to lay out as a grid.
//!
//! # Depth-guarded single pass (zero bytes — FR-020/FR-026)
//!
//! [`scan_table_sections`] descends from the document's top-level value, recursing
//! into every child regardless of whether the parent itself is a section (so a
//! nested `cells` list inside a map value is still found). The recursion is
//! **depth-guarded** (mirroring the tree/projection surfaces) so a pathologically
//! deep document cannot blow the stack. It is a pure read over the CST — scanning
//! changes **zero** document bytes (FR-020).

use ronin_core::ast;
use ronin_core::{CstDocument, SyntaxNode};

use super::classifier::{self, record_fields, record_shape, FallbackReason};
use super::view_state::{PathStep, StructuralPath};

/// The maximum structural depth the scanner descends before stopping, mirroring
/// the bound the other structural surfaces use so a pathological document cannot
/// blow the stack (a section deeper than this is simply not listed).
const MAX_SCAN_DEPTH: usize = 256;

/// The kind of a table-able section the navigator found.
///
/// `#[non_exhaustive]` so future tabular shapes can be added without a breaking
/// change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SectionShape {
    /// A list of same-named records (the conservative uniform-list rule) — rows are
    /// the elements, columns the union of fields.
    RecordList,
    /// A map whose every value is a same-named record — a leading read-only key
    /// column plus the union of the value records' fields.
    RecordMap,
    /// A list whose every element is a tuple of the same arity — positional
    /// `.0/.1/…` columns.
    TupleList,
}

/// One table-able section the scanner found, with its location, a readable label,
/// and its row/column dimensions for the navigator list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSection {
    /// The cross-reparse identity of the section node (the list / map).
    pub path: StructuralPath,
    /// A readable path label, e.g. `hulls \u{25B8} (1) \u{25B8} cells` or `(root)`.
    pub label: String,
    /// The section's tabular shape.
    pub shape: SectionShape,
    /// The number of rows (list elements / map entries).
    pub rows: usize,
    /// The number of columns (incl. the leading key column for a RecordMap;
    /// positional count for a TupleList).
    pub cols: usize,
}

/// Scan `cst` for every table-able section anywhere in the document (E012).
///
/// Walks the CST once from the top-level value, depth-guarded, recursing into all
/// children so nested sections inside map values / list elements are found (e.g.
/// `hulls \u{25B8} (N) \u{25B8} cells`). A pure read over the CST — zero bytes
/// (FR-020). Returns the sections in document order (the navigator re-sorts for
/// display).
#[must_use]
pub fn scan_table_sections(cst: &CstDocument) -> Vec<TableSection> {
    let root = cst.root();
    let Some(top) = ast::Document::cast(root)
        .and_then(|d| d.value())
        .map(|v| v.syntax().clone())
    else {
        return Vec::new();
    };

    let mut out = Vec::new();
    visit(&top, StructuralPath::root(), &["(root)".to_string()], 0, &mut out);
    out
}

/// Recursively visit `node` (addressed by `path`, labelled by the accumulated
/// `label_steps`), recording it as a section when it is table-able, then recursing
/// into its children. Depth-guarded by `depth` against [`MAX_SCAN_DEPTH`].
fn visit(
    node: &SyntaxNode,
    path: StructuralPath,
    label_steps: &[String],
    depth: usize,
    out: &mut Vec<TableSection>,
) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    let Some(value) = ast::Value::cast(node.clone()) else {
        return;
    };

    // Detect a section AT this node (a list is RecordList xor TupleList; a map may
    // be a RecordMap), then recurse into children regardless so nested sections are
    // still found.
    match &value {
        ast::Value::List(list) => {
            if let Some(section) =
                detect_list_section(node, list, &path, label_steps)
            {
                out.push(section);
            }
            for (i, elem) in list.items().enumerate() {
                let child_path = path.child(PathStep::Index(i));
                let child_label = pushed(label_steps, format!("[{i}]"));
                visit(elem.syntax(), child_path, &child_label, depth + 1, out);
            }
        }
        ast::Value::Map(map) => {
            if let Some(section) = detect_record_map(node, map, &path, label_steps) {
                out.push(section);
            }
            for entry in map.entries() {
                let Some(key) = entry.key() else { continue };
                let Some(val) = entry.value() else { continue };
                let key_text = key.syntax().text();
                let child_path = path.child(PathStep::Key(key_text.clone()));
                let child_label = pushed(label_steps, key_text);
                visit(val.syntax(), child_path, &child_label, depth + 1, out);
            }
        }
        ast::Value::Struct(s) => {
            for field in s.fields() {
                let Some(name) = field.name_text() else {
                    continue;
                };
                let Some(val) = field.value() else { continue };
                let child_path = path.child(PathStep::Field(name.clone()));
                let child_label = pushed(label_steps, name);
                visit(val.syntax(), child_path, &child_label, depth + 1, out);
            }
        }
        ast::Value::EnumVariant(v) => {
            for entry in v.entries() {
                let Some(key) = entry.key() else { continue };
                let Some(val) = entry.value() else { continue };
                let name = key.syntax().text();
                let child_path = path.child(PathStep::VariantField(name.clone()));
                let child_label = pushed(label_steps, name);
                visit(val.syntax(), child_path, &child_label, depth + 1, out);
            }
        }
        ast::Value::Tuple(t) => {
            for (i, elem) in t.items().enumerate() {
                let child_path = path.child(PathStep::Index(i));
                let child_label = pushed(label_steps, format!(".{i}"));
                visit(elem.syntax(), child_path, &child_label, depth + 1, out);
            }
        }
        // Scalars / unit / error nodes have no children and are never sections.
        _ => {}
    }
}

/// Classify a list node as a [`SectionShape::RecordList`] or
/// [`SectionShape::TupleList`] section (xor), or `None` when it is neither.
fn detect_list_section(
    list_node: &SyntaxNode,
    list: &ast::List,
    path: &StructuralPath,
    label_steps: &[String],
) -> Option<TableSection> {
    // RecordList: reuse the conservative classifier, but the navigator shows small
    // uniform lists too — accept a list that is eligible OR whose only fallback is
    // TooSmall (do NOT apply the \u{2264}2 auto-guard here).
    let verdict = classifier::classify(list_node);
    if verdict.table_eligible || verdict.fallback_reason == Some(FallbackReason::TooSmall) {
        let rows = list.items().count();
        let cols = verdict.column_schema.len();
        return Some(TableSection {
            path: path.clone(),
            label: join_label(label_steps),
            shape: SectionShape::RecordList,
            rows,
            cols,
        });
    }

    // TupleList: every element is a tuple, all of the same arity (\u{2265}1 element,
    // arity \u{2265}1).
    if let Some((rows, arity)) = uniform_tuple_arity(list) {
        return Some(TableSection {
            path: path.clone(),
            label: join_label(label_steps),
            shape: SectionShape::TupleList,
            rows,
            cols: arity,
        });
    }

    None
}

/// The `(element_count, arity)` of a list when **every** element is a tuple of the
/// same arity (≥1 element, arity ≥1), or `None` otherwise.
fn uniform_tuple_arity(list: &ast::List) -> Option<(usize, usize)> {
    let mut count = 0usize;
    let mut arity: Option<usize> = None;
    for elem in list.items() {
        let ast::Value::Tuple(t) = elem else {
            return None;
        };
        let this_arity = t.items().count();
        match arity {
            None => arity = Some(this_arity),
            Some(a) if a != this_arity => return None,
            Some(_) => {}
        }
        count += 1;
    }
    let arity = arity?;
    (count >= 1 && arity >= 1).then_some((count, arity))
}

/// Classify a map node as a [`SectionShape::RecordMap`] section when **every**
/// value is a record (struct / struct-like enum variant) of the **same** name
/// (≥1 entry), or `None` otherwise.
fn detect_record_map(
    _map_node: &SyntaxNode,
    map: &ast::Map,
    path: &StructuralPath,
    label_steps: &[String],
) -> Option<TableSection> {
    let mut shape_name: Option<String> = None;
    let mut value_records: Vec<Vec<(String, ast::Value)>> = Vec::new();
    for entry in map.entries() {
        let value = entry.value()?;
        // Every value must be a record of the same name.
        let this_name = record_shape(&value)?;
        match &shape_name {
            None => shape_name = Some(this_name),
            Some(existing) if *existing != this_name => return None,
            Some(_) => {}
        }
        value_records.push(record_fields(&value));
    }
    // Require at least one entry (an empty map is not a section).
    if value_records.is_empty() {
        return None;
    }

    // Columns = the leading key column + the union of the value records' fields.
    let field_cols = super::table::union_field_count(&value_records);
    Some(TableSection {
        path: path.clone(),
        label: join_label(label_steps),
        shape: SectionShape::RecordMap,
        rows: value_records.len(),
        cols: 1 + field_cols,
    })
}

/// Append `step` to a borrowed label-step slice, returning the extended owned list.
fn pushed(steps: &[String], step: String) -> Vec<String> {
    let mut next = steps.to_vec();
    next.push(step);
    next
}

/// Join accumulated label steps into a readable `a \u{25B8} b \u{25B8} c` string.
fn join_label(steps: &[String]) -> String {
    steps.join(" \u{25B8} ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ronin_core::parse;

    fn scan(src: &str) -> Vec<TableSection> {
        scan_table_sections(&parse(src))
    }

    #[test]
    fn scalar_only_struct_has_no_sections() {
        // A sample.ron-shaped doc: a scalar struct with scalar/tuple/enum fields —
        // no uniform record lists, record maps, or tuple lists.
        let s = scan("Config(name: \"x\", retries: 3, mode: Fast)");
        assert!(s.is_empty(), "expected no sections, got {s:?}");
    }

    #[test]
    fn record_list_section_is_found_small_or_large() {
        // A small (\u{2264}2) uniform list is still listed by the navigator.
        let small = scan("(items: [(a: 1), (a: 2)])");
        assert_eq!(small.len(), 1);
        assert_eq!(small[0].shape, SectionShape::RecordList);
        assert_eq!(small[0].rows, 2);

        // A larger uniform list too.
        let big = scan("(items: [(a: 1), (a: 2), (a: 3)])");
        assert_eq!(big.len(), 1);
        assert_eq!(big[0].shape, SectionShape::RecordList);
        assert_eq!(big[0].rows, 3);
    }

    #[test]
    fn non_uniform_list_is_not_a_section() {
        let s = scan("(items: [A(x: 1), B(y: 2)])");
        assert!(s.is_empty(), "name-mismatch list is not a section: {s:?}");
    }

    #[test]
    fn record_map_section_is_found() {
        let s = scan("(hulls: { (1): (hp: 1), (2): (hp: 2) })");
        let map = s
            .iter()
            .find(|x| x.shape == SectionShape::RecordMap)
            .expect("a record map section");
        assert_eq!(map.rows, 2);
        // 1 key column + 1 field column (`hp`).
        assert_eq!(map.cols, 2);
    }

    #[test]
    fn tuple_list_section_is_found() {
        let s = scan("(coords: [(1, 2), (3, 4), (5, 6)])");
        let t = s
            .iter()
            .find(|x| x.shape == SectionShape::TupleList)
            .expect("a tuple list section");
        assert_eq!(t.rows, 3);
        assert_eq!(t.cols, 2);
    }

    #[test]
    fn ships_shaped_doc_finds_nested_sections() {
        // A ships.ron-shaped doc: root struct \u{2192} `hulls` map of same-shape
        // hull structs, each with a `cells` list of same-shape records.
        let src = concat!(
            "(hulls: {\n",
            "  (1): (name: \"a\", cells: [(coord: (0,0), s: true), (coord: (1,0), s: false), (coord: (2,0), s: true)]),\n",
            "  (2): (name: \"b\", cells: [(coord: (0,0), s: true), (coord: (1,0), s: false), (coord: (2,0), s: true)]),\n",
            "})"
        );
        let s = scan(src);
        // The `hulls` map is a RecordMap.
        let hulls = s
            .iter()
            .find(|x| x.shape == SectionShape::RecordMap)
            .expect("hulls record map");
        assert_eq!(hulls.path, StructuralPath::from_steps(vec![PathStep::Field("hulls".to_string())]));
        assert_eq!(hulls.rows, 2);

        // Each hull's `cells` is a RecordList (two of them).
        let cells: Vec<_> = s
            .iter()
            .filter(|x| x.shape == SectionShape::RecordList)
            .collect();
        assert_eq!(cells.len(), 2, "one cells list per hull");
        assert!(cells.iter().all(|c| c.rows == 3));
        // The `coord` tuple list inside each cell record is too small (arity ok but
        // it is a tuple element, not a list-of-tuples) — verify a cells path.
        assert!(cells.iter().any(|c| c.path
            == StructuralPath::from_steps(vec![
                PathStep::Field("hulls".to_string()),
                PathStep::Key("(1)".to_string()),
                PathStep::Field("cells".to_string()),
            ])));
    }

    #[test]
    fn root_list_labels_as_root() {
        let s = scan("[(a: 1), (a: 2), (a: 3)]");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].label, "(root)");
        assert!(s[0].path.is_root());
    }
}
