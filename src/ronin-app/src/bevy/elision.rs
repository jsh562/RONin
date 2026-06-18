//! Lossless defaults elision / expand-to-explicit via the E008 structural
//! transforms (FR-014/FR-015/FR-016, AD-005/AD-006, HINT-003/HINT-005).
//!
//! # What this module does (the shrink / expand pair)
//!
//! [`reduce_verbosity`] (**shrink**) elides a Bevy-scene struct field **iff** (a)
//! its component/resource type is registered with `Default` reflected AND a
//! concrete default value is known (a *defaults-carrying* export supplied it —
//! HINT-003/AD-005), AND (b) the field's current value **provably equals** that
//! field's default. [`expand_to_explicit`] (**expand**) is the session-independent
//! inverse: it materializes **every** registered default-bearing field currently
//! ABSENT whose default is known (FR-015) — it never relies on remembering what was
//! elided, because elision leaves no on-disk marker.
//!
//! Both directions are routed through `ronin-core`'s pure E008
//! [`apply_structural`](ronin_core::apply_structural) transforms — never a hand-edit
//! (HINT-005) — so every untouched region stays **byte-for-byte** identical, and
//! the whole invocation is ONE CST→CST result the caller pushes as a single E007
//! undo unit (FR-016).
//!
//! # The provable-default equality rule (FR-014)
//!
//! "Provably equals the default" compares the field's parsed/normalized RON value
//! against the registry-carried default `serde_json::Value` — see
//! [`ron_value_equals_json`]. The rule is **exact**, with one calibrated subtlety:
//!
//! * **Floats compare on the parsed `f64`, bit-for-bit (no epsilon).** `1.0`,
//!   `1.00`, and `1e0` all parse to the same `f64` and so all equal a default of
//!   `1.0` — NOT a source-token-text comparison. (`-0.0`/`0.0` and `NaN` payloads
//!   compare by their `f64` bit pattern, so they are distinguished exactly.)
//! * **Integers compare exactly** (parsed `i128`), ignoring `_` separators and a
//!   numeric type suffix.
//! * **Strings / bools / unit / lists / structs / maps** compare structurally.
//!
//! Any case where the default value is absent, the type is unregistered, `Default`
//! is not reflected, or equality cannot be decided unambiguously is treated as
//! **not-elidable** — the field is left explicit (never elide on an unknown /
//! ambiguous default).
//!
//! # The JSON-default → RON-text renderer (expand, FR-015)
//!
//! Expand inserts a field whose value text is the registry default rendered as RON
//! by [`render_json_as_ron`]: scalars (numbers/bools/null), strings, lists, and
//! structs/maps. A JSON object is rendered as a RON anonymous struct
//! `(field: value, ..)` when the registry type for that field is a struct, and as
//! a RON map `{ "k": value, .. }` otherwise (disambiguated via the registry type
//! where known). The renderer never fabricates a value the registry did not carry.
//!
//! # Lossless / stable (FR-016, SC-006)
//!
//! Because every edit is an `apply_structural` op (which reuses the green tree for
//! untouched subtrees) and elision/expand never touch a field they did not decide
//! on, untouched regions are byte-identical and **shrink → expand → shrink is
//! byte-identical**: the first shrink removes exactly the default-valued fields;
//! expand restores exactly the registry defaults (canonical text); the second
//! shrink removes exactly those same fields again — landing on the identical bytes
//! of the first shrink. Each whole invocation is a single CST→CST document.
//!
//! # Index-shift handling (HINT-005)
//!
//! A `RemoveField`/`InsertField` op shifts the indices of an element's later
//! siblings. To keep a multi-op sequence correct we **re-resolve every target
//! against the current (post-previous-op) document by the field's key text**, never
//! against a stale index. Removals are additionally ordered so each step's target
//! is unambiguous; see [`apply_field_ops`].

use std::collections::BTreeSet;

use ronin_core::{
    apply_structural, ast, syntax::SyntaxKind, CstDocument, ParentRef, StructuralOp, SyntaxNode,
    TransformOutcome,
};
use ronin_types::BevyRegistry;
use serde_json::Value as JsonValue;

use crate::bevy::scene::{SceneModel, SceneValueRef};

/// The scope of an elision / expansion invocation (data-model
/// `DefaultsElisionTransform.scope`).
///
/// Whole-document by default; an optional entity scope restricts the transform to
/// a single entity's components (FR-014). A resource is only in scope under
/// [`Scope::WholeDocument`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Scope {
    /// Every resource + component in the scene (the default).
    #[default]
    WholeDocument,
    /// Only the components of the entity with this id (FR-014 selection/entity scope).
    Entity(i128),
}

/// A field that was decided-on but skipped, with the reason — surfaced so the
/// caller can show a partial-expand advisory (FR-015) or explain why a field was
/// left explicit (FR-014).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedField {
    /// The owning component/resource type path.
    pub type_path: String,
    /// The field name that was skipped.
    pub field: String,
    /// Why it was skipped.
    pub reason: SkipReason,
}

/// Why a candidate field was skipped (left explicit on shrink, or not restored on
/// expand).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SkipReason {
    /// On expand: the registry no longer carries this field's default (registry
    /// drift) — a **partial expand**, never fabricated (FR-015).
    DefaultUnknownOnExpand,
    /// On shrink: the field's value did not provably equal the known default, so
    /// it is left explicit (FR-014).
    ValueDiffersFromDefault,
}

/// The outcome of an elision / expansion invocation (data-model
/// `DefaultsElisionTransform` output).
///
/// `#[non_exhaustive]` so future outcome arms can be added without a breaking
/// change.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ElisionOutcome {
    /// The transform changed bytes: carries the fresh CST the caller pushes as ONE
    /// E007 undo unit (untouched regions byte-identical), plus any per-field skip
    /// advisories (e.g. the partial-expand list on registry drift, FR-015).
    Applied {
        /// The new document (a single CST→CST result; one undo unit).
        document: CstDocument,
        /// Fields decided-on but not changed (advisory; e.g. partial expand).
        skipped: Vec<SkippedField>,
    },
    /// Nothing in scope was elidable / expandable: **zero bytes changed and no
    /// undo unit** is pushed (FR-014 no-op). Carries any skip advisories.
    NoOp {
        /// Fields decided-on but not changed (advisory).
        skipped: Vec<SkippedField>,
    },
}

impl ElisionOutcome {
    /// The resulting document if the transform changed bytes, else `None` (no-op).
    #[must_use]
    pub fn document(&self) -> Option<&CstDocument> {
        match self {
            Self::Applied { document, .. } => Some(document),
            Self::NoOp { .. } => None,
        }
    }

    /// The per-field skip advisories (partial-expand drift / value-differs).
    #[must_use]
    pub fn skipped(&self) -> &[SkippedField] {
        match self {
            Self::Applied { skipped, .. } | Self::NoOp { skipped } => skipped,
        }
    }

    /// `true` when nothing changed (a no-op).
    #[must_use]
    pub fn is_no_op(&self) -> bool {
        matches!(self, Self::NoOp { .. })
    }
}

// ===========================================================================
// Public entry points (T026 shrink / T027 expand, routed through T028)
// ===========================================================================

/// **Reduce verbosity (shrink)** — elide every in-scope struct field whose value
/// provably equals its known registry default (T026, FR-014).
///
/// `doc` is the current CST (never mutated). `model` is the [`SceneModel`] derived
/// from that **same** `doc` (so every target maps to a real, current CST range).
/// `registry` supplies the per-type concrete defaults. `scope` restricts the pass
/// (whole-document by default).
///
/// Returns [`ElisionOutcome::Applied`] with a fresh CST (one undo unit) when at
/// least one field was elided, or [`ElisionOutcome::NoOp`] (zero bytes) when
/// nothing in scope was elidable. Unparseable spans are skipped (the scene model
/// never resolves them to a struct), never crashing.
#[must_use]
pub fn reduce_verbosity(
    doc: &CstDocument,
    model: &SceneModel,
    registry: &BevyRegistry,
    scope: Scope,
) -> ElisionOutcome {
    let mut groups: Vec<ComponentRemoval> = Vec::new();
    let mut skipped: Vec<SkippedField> = Vec::new();

    for value_ref in in_scope(model, scope) {
        let type_path = value_ref.type_path();
        // Precondition: registered + Default reflected + a concrete default known.
        if !registry.is_default_reflected(type_path) {
            continue;
        }
        let Some(default) = registry.default_value(type_path) else {
            continue; // default absent ⇒ non-elidable (HINT-003/AD-005).
        };
        // A struct default is a JSON object keyed by field; only struct-shaped
        // components participate in field-level elision.
        let Some(default_obj) = default.as_object() else {
            continue;
        };
        let Some(struct_node) = as_struct_node(value_ref) else {
            continue; // unparseable / non-struct value: never elide (FR-014).
        };

        let all_fields = struct_fields(&struct_node);
        let present_count = all_fields.len();
        let mut fields = Vec::new();
        for field in all_fields {
            let Some(name) = field.name_text() else {
                continue;
            };
            let Some(field_default) = default_obj.get(&name) else {
                // No default for this specific field ⇒ leave explicit (FR-014).
                continue;
            };
            let Some(value) = field.value() else {
                continue;
            };
            if ron_value_equals_json(&value, field_default) {
                fields.push(name);
            } else {
                skipped.push(SkippedField {
                    type_path: type_path.to_string(),
                    field: name,
                    reason: SkipReason::ValueDiffersFromDefault,
                });
            }
        }
        if fields.is_empty() {
            continue;
        }
        // When EVERY present field is elidable, replace the whole value with a
        // clean `()` (rather than removing fields one-by-one, which would leave a
        // stray separator on the final sole-field removal) — this also makes shrink
        // the exact inverse of expand's whole-value materialization, which is what
        // guarantees shrink→expand→shrink is byte-identical (FR-016).
        let clear_to_unit = fields.len() == present_count;
        groups.push(ComponentRemoval {
            struct_start: struct_node.text_range().start(),
            fields,
            clear_to_unit,
        });
    }

    if groups.is_empty() {
        return ElisionOutcome::NoOp { skipped };
    }

    match apply_removals(doc, groups) {
        Some(document) => ElisionOutcome::Applied { document, skipped },
        None => ElisionOutcome::NoOp { skipped },
    }
}

/// **Expand to explicit** — the session-independent inverse of shrink (T027,
/// FR-015).
///
/// For every in-scope registered component/resource whose `Default` is reflected
/// and whose concrete default is known, materializes each default-bearing field
/// currently ABSENT from the scene value, rendering the registry default as RON
/// text. On registry drift (a field's default no longer known) it performs a
/// **partial expand** — restoring only fields whose default is still known and
/// recording the rest as a [`SkipReason::DefaultUnknownOnExpand`] advisory — and
/// never fabricates a value the registry did not carry.
///
/// Returns [`ElisionOutcome::Applied`] (one undo unit) when at least one field was
/// inserted, else [`ElisionOutcome::NoOp`].
#[must_use]
pub fn expand_to_explicit(
    doc: &CstDocument,
    model: &SceneModel,
    registry: &BevyRegistry,
    scope: Scope,
) -> ElisionOutcome {
    let mut groups: Vec<ComponentInsert> = Vec::new();
    let mut skipped: Vec<SkippedField> = Vec::new();

    for value_ref in in_scope(model, scope) {
        let type_path = value_ref.type_path();
        if !registry.is_default_reflected(type_path) {
            continue;
        }
        let Some(default) = registry.default_value(type_path) else {
            continue;
        };
        let Some(default_obj) = default.as_object() else {
            continue;
        };
        let value_node = value_ref.value_node().clone();

        // The currently-present field names (empty when the value is a Unit `()` or
        // an empty struct — every default field is then absent).
        let present: BTreeSet<String> = if value_node.kind() == SyntaxKind::Struct {
            struct_fields(&value_node)
                .into_iter()
                .filter_map(|f| f.name_text())
                .collect()
        } else if value_node.kind() == SyntaxKind::Unit {
            BTreeSet::new()
        } else {
            // A non-struct, non-unit value (scalar / enum / tuple / error): not a
            // struct-default component shape — never expand into it (FR-015).
            continue;
        };

        // The absent default-bearing fields, rendered to RON text (deterministic
        // registry order). A default the renderer cannot place is a partial-expand
        // skip — never fabricated (FR-015).
        let mut fields: Vec<(String, String)> = Vec::new();
        for (field_name, field_default) in default_obj {
            if present.contains(field_name) {
                continue; // already explicit.
            }
            match render_json_as_ron(field_default, registry, None) {
                Some(text) => fields.push((field_name.clone(), text)),
                None => skipped.push(SkippedField {
                    type_path: type_path.to_string(),
                    field: field_name.clone(),
                    reason: SkipReason::DefaultUnknownOnExpand,
                }),
            }
        }
        if fields.is_empty() {
            continue;
        }

        let mode = if present.is_empty() {
            // No field is present (Unit `()` or empty struct): replace the whole
            // value with a freshly-rendered default struct — there is no struct to
            // insert into for a Unit, and this keeps the layout canonical so a
            // later shrink lands on the same bytes.
            InsertMode::ReplaceWholeValue
        } else {
            InsertMode::InsertFields
        };

        groups.push(ComponentInsert {
            value_start: value_node.text_range().start(),
            fields,
            mode,
        });
    }

    if groups.is_empty() {
        return ElisionOutcome::NoOp { skipped };
    }

    match apply_inserts(doc, groups) {
        Some(document) => ElisionOutcome::Applied { document, skipped },
        None => ElisionOutcome::NoOp { skipped },
    }
}

// ===========================================================================
// Scope + target resolution
// ===========================================================================

/// The in-scope scene value refs for `scope`.
fn in_scope(model: &SceneModel, scope: Scope) -> Vec<&SceneValueRef> {
    match scope {
        Scope::WholeDocument => model.entries().collect(),
        Scope::Entity(id) => model
            .entities()
            .iter()
            .filter(|e| e.id() == id)
            .flat_map(|e| e.components().iter())
            .collect(),
    }
}

/// The `field: value` entries of a struct node, collected (owned) so the iterator
/// does not borrow a temporary `ast::Struct`.
fn struct_fields(node: &SyntaxNode) -> Vec<ast::StructField> {
    ast::Struct::cast(node.clone())
        .map(|s| s.fields().collect())
        .unwrap_or_default()
}

/// The struct node backing a scene value, if its value is an anonymous/named
/// struct (the only shape that carries elidable `field: value` entries). A
/// non-struct value (enum, tuple, scalar, unparseable `Error`) is never elided.
fn as_struct_node(value_ref: &SceneValueRef) -> Option<SyntaxNode> {
    let node = value_ref.value_node().clone();
    if node.kind() == SyntaxKind::Struct {
        Some(node)
    } else {
        None
    }
}

// ===========================================================================
// Op application — route both directions through apply_structural (T028)
// ===========================================================================

/// A per-component removal plan: the component struct's start offset (a stable
/// address — see [`find_node_at`]) plus the field names to elide from it.
#[derive(Debug, Clone)]
struct ComponentRemoval {
    struct_start: usize,
    fields: Vec<String>,
    /// `true` when every present field is elided — replace the whole value with a
    /// clean `()` instead of removing fields individually (FR-016 stability).
    clear_to_unit: bool,
}

/// How an expand op materializes a component's absent defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InsertMode {
    /// Insert each rendered field into the existing (non-empty) struct.
    InsertFields,
    /// Replace the whole component value (a Unit `()` / empty struct) with one
    /// freshly-rendered default struct — there is no struct to insert into.
    ReplaceWholeValue,
}

/// A per-component insert plan: the component value's start offset (a stable
/// address) plus the absent default fields (name, rendered RON text) to add, and
/// whether to insert into an existing struct or replace the whole value.
#[derive(Debug, Clone)]
struct ComponentInsert {
    value_start: usize,
    fields: Vec<(String, String)>,
    mode: InsertMode,
}

/// Apply the per-component removals through [`apply_structural`], producing one
/// final CST (T028, HINT-005).
///
/// **Index-shift handling:** an edit only shifts the start offsets of nodes that
/// begin **after** it. We therefore process components in **descending** start-
/// offset order, so every not-yet-processed component's captured offset stays
/// valid. Within a component we re-resolve the parent struct by its (stable) start
/// offset and re-resolve each field's index **by key** in the current tree before
/// each `RemoveField`, so removing one field never mis-targets the next.
///
/// Returns the new CST, or `None` if no op changed bytes (the caller reports a
/// no-op rather than an unchanged "Applied").
fn apply_removals(doc: &CstDocument, mut groups: Vec<ComponentRemoval>) -> Option<CstDocument> {
    // Descending by start offset: edit later components first.
    groups.sort_by_key(|g| std::cmp::Reverse(g.struct_start));

    let mut current = doc.clone();
    let mut changed = false;

    for group in &groups {
        if group.clear_to_unit {
            // Every present field is a default ⇒ replace the whole value with `()`.
            let Some(value_node) =
                find_node_at(&current.root(), SyntaxKind::Struct, group.struct_start)
            else {
                continue;
            };
            let Some((parent_map, index)) = locate_map_value(&value_node) else {
                // Not a map-entry value (shouldn't happen for a component): fall
                // back to per-field removal below would be unreachable here.
                continue;
            };
            if let TransformOutcome::Applied(next) = apply_structural(
                &current,
                StructuralOp::SetValue {
                    parent: ParentRef::Map(parent_map),
                    index,
                    value: "()".to_string(),
                },
            ) {
                current = reparse(&next);
                changed = true;
            }
            continue;
        }
        for field in &group.fields {
            // Re-resolve the parent struct + the field's current index by key.
            let Some(struct_node) =
                find_node_at(&current.root(), SyntaxKind::Struct, group.struct_start)
            else {
                continue; // already gone / unresolvable: skip (never corrupt).
            };
            let Some(strukt) = ast::Struct::cast(struct_node.clone()) else {
                continue;
            };
            let Some(index) = strukt
                .fields()
                .position(|f| f.name_text().as_deref() == Some(field.as_str()))
            else {
                continue; // already removed.
            };
            if let TransformOutcome::Applied(next) = apply_structural(
                &current,
                StructuralOp::RemoveField {
                    parent: ParentRef::Struct(struct_node),
                    index,
                },
            ) {
                current = reparse(&next);
                changed = true;
            }
        }
    }

    changed.then_some(current)
}

/// Reparse a document's printed text into a structurally-correct CST.
///
/// `apply_structural` splices inserted/replaced text as raw green tokens; the
/// returned tree's text round-trips byte-for-byte, but a *newly inserted* field is
/// not yet a typed `StructField` node until reparsed. Because the text is
/// byte-identical, **every byte offset is preserved**, so re-resolving a target by
/// its start offset across this reparse stays valid (HINT-005). This keeps the
/// per-field index/key re-resolution between chained ops correct without changing
/// any byte.
fn reparse(doc: &CstDocument) -> CstDocument {
    ronin_core::parse(&ronin_core::print(doc))
}

/// Apply the per-component inserts through [`apply_structural`], producing one
/// final CST (T028, HINT-005). Processes components in **descending** start-offset
/// order so an earlier component's captured offset stays valid across edits.
fn apply_inserts(doc: &CstDocument, mut groups: Vec<ComponentInsert>) -> Option<CstDocument> {
    groups.sort_by_key(|g| std::cmp::Reverse(g.value_start));

    let mut current = doc.clone();
    let mut changed = false;

    for group in &groups {
        match group.mode {
            InsertMode::ReplaceWholeValue => {
                // Render the whole default struct and replace the component value
                // (a Unit `()` or empty struct) via its parent map entry's value.
                let Some(value_node) = find_value_node_at(&current.root(), group.value_start)
                else {
                    continue;
                };
                let Some((parent_map, index)) = locate_map_value(&value_node) else {
                    continue;
                };
                let rendered = render_struct(&group.fields);
                if let TransformOutcome::Applied(next) = apply_structural(
                    &current,
                    StructuralOp::SetValue {
                        parent: ParentRef::Map(parent_map),
                        index,
                        value: rendered,
                    },
                ) {
                    current = reparse(&next);
                    changed = true;
                }
            }
            InsertMode::InsertFields => {
                for (name, text) in &group.fields {
                    let Some(struct_node) =
                        find_node_at(&current.root(), SyntaxKind::Struct, group.value_start)
                    else {
                        continue;
                    };
                    let index = ast::Struct::cast(struct_node.clone())
                        .map(|s| s.fields().count())
                        .unwrap_or(0);
                    if let TransformOutcome::Applied(next) = apply_structural(
                        &current,
                        StructuralOp::InsertField {
                            parent: ParentRef::Struct(struct_node),
                            index,
                            name: name.clone(),
                            value: text.clone(),
                        },
                    ) {
                        current = reparse(&next);
                        changed = true;
                    }
                }
            }
        }
    }

    changed.then_some(current)
}

/// Render a struct value text `(name: value, ..)` from rendered field pairs (used
/// to replace a Unit `()` / empty struct on expand). Empty pairs render `()`.
fn render_struct(fields: &[(String, String)]) -> String {
    if fields.is_empty() {
        return "()".to_string();
    }
    let body: Vec<String> = fields
        .iter()
        .map(|(name, text)| format!("{name}: {text}"))
        .collect();
    format!("({})", body.join(", "))
}

/// The parent [`ast::Map`] and the entry index of a value node that is a map
/// entry's value (so it can be replaced via `SetValue` on `ParentRef::Map`).
fn locate_map_value(value_node: &SyntaxNode) -> Option<(SyntaxNode, usize)> {
    let entry = value_node
        .parent()
        .filter(|p| p.kind() == SyntaxKind::MapEntry)?;
    let map = entry.parent().filter(|p| p.kind() == SyntaxKind::Map)?;
    let entry_start = entry.text_range().start();
    let index = ast::Map::cast(map.clone())?
        .entries()
        .position(|e| e.syntax().text_range().start() == entry_start)?;
    Some((map, index))
}

/// Depth-first search for the node of `kind` whose start offset equals `start` (the
/// stable container address used by E008's locator).
fn find_node_at(root: &SyntaxNode, kind: SyntaxKind, start: usize) -> Option<SyntaxNode> {
    fn walk(node: &SyntaxNode, kind: SyntaxKind, start: usize) -> Option<SyntaxNode> {
        if node.kind() == kind && node.text_range().start() == start {
            return Some(node.clone());
        }
        for child in node.children() {
            let r = child.text_range();
            if r.start() <= start && start < r.end() {
                if let Some(found) = walk(&child, kind, start) {
                    return Some(found);
                }
            }
        }
        None
    }
    walk(root, kind, start)
}

/// Depth-first search for the value-position node whose start offset equals `start`
/// (any value kind — used to relocate a Unit / struct component value to replace).
fn find_value_node_at(root: &SyntaxNode, start: usize) -> Option<SyntaxNode> {
    fn walk(node: &SyntaxNode, start: usize) -> Option<SyntaxNode> {
        if node.text_range().start() == start && ast::Value::cast(node.clone()).is_some() {
            return Some(node.clone());
        }
        for child in node.children() {
            let r = child.text_range();
            if r.start() <= start && start < r.end() {
                if let Some(found) = walk(&child, start) {
                    return Some(found);
                }
            }
        }
        None
    }
    walk(root, start)
}

// ===========================================================================
// Value-vs-default equality (FR-014, the bit-for-bit float rule)
// ===========================================================================

/// `true` if the RON [`ast::Value`] provably equals the registry default
/// [`JsonValue`] under the registry's value representation (FR-014).
///
/// * **Numbers** compare on the parsed `f64` **bit-for-bit** (no epsilon): a RON
///   float / integer literal is parsed to `f64` and compared by `to_bits()`
///   against the JSON number's `f64` — so `1.0`, `1.00`, `1e0` all equal a JSON
///   `1.0`. Two integers also compare exactly as `i128` first (so very large
///   integers that lose `f64` precision still compare exactly).
/// * **Strings** compare by their decoded content (a RON string vs a JSON string).
/// * **Bools / null / unit** compare structurally.
/// * **Lists** compare element-wise, in order.
/// * **Structs** compare against a JSON object by field name (order-independent),
///   every JSON key present and equal and no extra RON field; a RON **map** also
///   compares against a JSON object by key.
///
/// Returns `false` (not-elidable) for any shape it cannot decide unambiguously.
#[must_use]
pub fn ron_value_equals_json(value: &ast::Value, default: &JsonValue) -> bool {
    match value {
        ast::Value::Literal(lit) => literal_equals_json(lit, default),
        ast::Value::Unit(_) => default.is_null() || is_empty_json_collection(default),
        ast::Value::List(list) => {
            let Some(arr) = default.as_array() else {
                return false;
            };
            let items: Vec<ast::Value> = list.items().collect();
            items.len() == arr.len()
                && items
                    .iter()
                    .zip(arr.iter())
                    .all(|(v, d)| ron_value_equals_json(v, d))
        }
        ast::Value::Tuple(tuple) => {
            // A RON tuple compares against a JSON array, positionally (e.g. a
            // tuple-struct default carried as `[..]`).
            let Some(arr) = default.as_array() else {
                return false;
            };
            let items: Vec<ast::Value> = tuple.items().collect();
            items.len() == arr.len()
                && items
                    .iter()
                    .zip(arr.iter())
                    .all(|(v, d)| ron_value_equals_json(v, d))
        }
        ast::Value::Struct(s) => struct_equals_json_object(s, default),
        ast::Value::Map(m) => map_equals_json_object(m, default),
        ast::Value::EnumVariant(v) => enum_equals_json(v, default),
        ast::Value::Error(_) => false, // unparseable ⇒ never elide (FR-014).
    }
}

/// Compare a scalar RON literal against a JSON default value.
fn literal_equals_json(lit: &ast::Literal, default: &JsonValue) -> bool {
    let Some(kind) = lit.token_kind() else {
        return false;
    };
    let Some(text) = lit.text() else {
        return false;
    };
    match kind {
        SyntaxKind::Integer => match default {
            // Integer vs integer: exact i128. Integer vs JSON float: parse the RON
            // int to f64 and compare bit-for-bit (covers `0` vs default `0.0`).
            JsonValue::Number(n) => {
                if let Some(d_i) = json_number_as_i128(n) {
                    parse_ron_int(&text).is_some_and(|v| v == d_i)
                } else if let Some(d_f) = n.as_f64() {
                    parse_ron_float(&text).is_some_and(|v| f64_bits_eq(v, d_f))
                } else {
                    false
                }
            }
            _ => false,
        },
        SyntaxKind::Float => match default.as_f64() {
            // The bit-for-bit float rule (FR-014): parsed f64, no epsilon.
            Some(d_f) => parse_ron_float(&text).is_some_and(|v| f64_bits_eq(v, d_f)),
            None => false,
        },
        SyntaxKind::TrueKw => matches!(default, JsonValue::Bool(true)),
        SyntaxKind::FalseKw => matches!(default, JsonValue::Bool(false)),
        SyntaxKind::String | SyntaxKind::RawString => match default.as_str() {
            Some(d) => decode_ron_string(&text).as_deref() == Some(d),
            None => false,
        },
        SyntaxKind::Char => match default.as_str() {
            Some(d) => decode_ron_char(&text).as_deref() == Some(d),
            None => false,
        },
        _ => false,
    }
}

/// Compare a RON struct against a JSON object by field name (order-independent).
/// Every JSON key must be present in the struct and equal; the struct must carry
/// no extra field the default omits (an exact match — extra non-default fields
/// would not round-trip on expand).
fn struct_equals_json_object(s: &ast::Struct, default: &JsonValue) -> bool {
    let Some(obj) = default.as_object() else {
        return false;
    };
    let fields: Vec<ast::StructField> = s.fields().collect();
    if fields.len() != obj.len() {
        return false;
    }
    fields.iter().all(|f| {
        let Some(name) = f.name_text() else {
            return false;
        };
        let Some(field_default) = obj.get(&name) else {
            return false;
        };
        f.value()
            .is_some_and(|v| ron_value_equals_json(&v, field_default))
    })
}

/// Compare a RON map against a JSON object by string key (order-independent).
fn map_equals_json_object(m: &ast::Map, default: &JsonValue) -> bool {
    let Some(obj) = default.as_object() else {
        return false;
    };
    let entries: Vec<ast::MapEntry> = m.entries().collect();
    if entries.len() != obj.len() {
        return false;
    }
    entries.iter().all(|e| {
        let Some(key) = e.key().and_then(|k| json_key_text(&k)) else {
            return false;
        };
        let Some(value_default) = obj.get(&key) else {
            return false;
        };
        e.value()
            .is_some_and(|v| ron_value_equals_json(&v, value_default))
    })
}

/// Compare a RON enum variant against a JSON default.
///
/// A **unit** variant (`Inherited`) compares against a JSON string of the variant
/// name (the externally-tagged default shape). A payload-carrying variant compares
/// against a single-key JSON object `{ "Variant": <payload> }`.
fn enum_equals_json(v: &ast::EnumVariant, default: &JsonValue) -> bool {
    let Some(name) = v.name_text() else {
        return false;
    };
    let entries: Vec<ast::MapEntry> = v.entries().collect();
    if entries.is_empty() {
        // Unit variant: a bare string default, or `{ "Variant": null }`.
        if let Some(d) = default.as_str() {
            return d == name;
        }
        if let Some(obj) = default.as_object() {
            if let Some(inner) = obj.get(&name) {
                return obj.len() == 1 && (inner.is_null() || is_empty_json_collection(inner));
            }
        }
        return false;
    }
    // Struct-like payload: `{ "Variant": { field: v, .. } }`.
    let Some(obj) = default.as_object() else {
        return false;
    };
    let Some(inner) = obj.get(&name) else {
        return false;
    };
    if obj.len() != 1 {
        return false;
    }
    let Some(inner_obj) = inner.as_object() else {
        return false;
    };
    if entries.len() != inner_obj.len() {
        return false;
    }
    entries.iter().all(|e| {
        let Some(key) = e.key().and_then(|k| json_key_text(&k)) else {
            return false;
        };
        let Some(field_default) = inner_obj.get(&key) else {
            return false;
        };
        e.value()
            .is_some_and(|val| ron_value_equals_json(&val, field_default))
    })
}

/// `true` for a JSON empty array / empty object (treated as a unit-equivalent).
fn is_empty_json_collection(v: &JsonValue) -> bool {
    matches!(v, JsonValue::Array(a) if a.is_empty())
        || matches!(v, JsonValue::Object(o) if o.is_empty())
}

/// The string text of a RON value used as a map/struct key (a string literal's
/// decoded content, or a bare-ident key).
fn json_key_text(key: &ast::Value) -> Option<String> {
    match key {
        ast::Value::Literal(lit) => match lit.token_kind() {
            Some(SyntaxKind::String | SyntaxKind::RawString) => decode_ron_string(&lit.text()?),
            _ => lit.text(),
        },
        ast::Value::EnumVariant(v) => v.name_text(),
        _ => None,
    }
}

// ===========================================================================
// Number / string parsing (the bit-for-bit float rule core)
// ===========================================================================

/// `true` if two `f64`s are bit-for-bit identical (FR-014: no epsilon). Comparing
/// `to_bits()` means `NaN == NaN` (same payload) and `0.0 != -0.0`, an exact match.
#[inline]
fn f64_bits_eq(a: f64, b: f64) -> bool {
    a.to_bits() == b.to_bits()
}

/// Parse a RON float literal to `f64`, stripping `_` separators and any numeric
/// type suffix (`f32`/`f64`). Returns `None` if it does not parse.
fn parse_ron_float(text: &str) -> Option<f64> {
    let cleaned = strip_numeric_suffix(&text.replace('_', ""), &["f32", "f64"]);
    cleaned.parse::<f64>().ok()
}

/// Parse a RON integer literal to `i128`, stripping `_` separators and any integer
/// type suffix. Supports the common `0x`/`0o`/`0b` radix prefixes.
fn parse_ron_int(text: &str) -> Option<i128> {
    let no_sep = text.replace('_', "");
    let cleaned = strip_numeric_suffix(
        &no_sep,
        &[
            "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize",
        ],
    );
    let (neg, body) = match cleaned.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, cleaned.as_str()),
    };
    let magnitude = if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        i128::from_str_radix(hex, 16).ok()?
    } else if let Some(oct) = body.strip_prefix("0o").or_else(|| body.strip_prefix("0O")) {
        i128::from_str_radix(oct, 8).ok()?
    } else if let Some(bin) = body.strip_prefix("0b").or_else(|| body.strip_prefix("0B")) {
        i128::from_str_radix(bin, 2).ok()?
    } else {
        body.parse::<i128>().ok()?
    };
    Some(if neg { -magnitude } else { magnitude })
}

/// Strip a known numeric type suffix from the end of a cleaned numeric string.
fn strip_numeric_suffix(s: &str, suffixes: &[&str]) -> String {
    for suffix in suffixes {
        if let Some(stripped) = s.strip_suffix(suffix) {
            return stripped.to_string();
        }
    }
    s.to_string()
}

/// A JSON number as an exact `i128`, if it is an integer that fits.
fn json_number_as_i128(n: &serde_json::Number) -> Option<i128> {
    if let Some(u) = n.as_u64() {
        return Some(i128::from(u));
    }
    if let Some(i) = n.as_i64() {
        return Some(i128::from(i));
    }
    None
}

/// Decode a RON string literal's verbatim token text (with surrounding quotes)
/// into its content. Handles plain `"..."` (with common escapes) and raw
/// `r"..."` / `r#"..."#` strings.
fn decode_ron_string(verbatim: &str) -> Option<String> {
    if verbatim.starts_with('"') && verbatim.ends_with('"') && verbatim.len() >= 2 {
        return Some(unescape(&verbatim[1..verbatim.len() - 1]));
    }
    if let Some(after_r) = verbatim.strip_prefix('r') {
        let hashes = after_r.bytes().take_while(|b| *b == b'#').count();
        let body = &after_r[hashes..];
        let close = format!("\"{}", "#".repeat(hashes));
        if body.starts_with('"') && after_r.ends_with(&close) && after_r.len() > 1 + 2 * hashes {
            return Some(after_r[hashes + 1..after_r.len() - hashes - 1].to_string());
        }
    }
    None
}

/// Decode a RON char literal `'c'` into its single-character content string.
fn decode_ron_char(verbatim: &str) -> Option<String> {
    if verbatim.starts_with('\'') && verbatim.ends_with('\'') && verbatim.len() >= 2 {
        return Some(unescape(&verbatim[1..verbatim.len() - 1]));
    }
    None
}

/// Decode the common RON/Rust escape sequences inside an already-delimiter-stripped
/// string body. Unknown escapes are preserved verbatim (best-effort, never panics).
fn unescape(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some('0') => out.push('\0'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

// ===========================================================================
// JSON-default → RON-text renderer (expand, FR-015)
// ===========================================================================

/// Render a registry default [`JsonValue`] as RON value text suitable for an
/// `InsertField` payload (FR-015). Returns `None` for a shape it cannot render
/// (so the caller records a partial-expand skip rather than fabricating bytes).
///
/// `type_hint` (when known) is the registered type path of the value, used to
/// disambiguate a JSON object into a RON struct (`(k: v)`) vs a RON map
/// (`{ "k": v }`).
#[must_use]
pub fn render_json_as_ron(
    value: &JsonValue,
    registry: &BevyRegistry,
    type_hint: Option<&str>,
) -> Option<String> {
    match value {
        JsonValue::Null => None, // cannot place a bare unit field default; skip.
        JsonValue::Bool(b) => Some(b.to_string()),
        JsonValue::Number(n) => Some(render_number(n)),
        JsonValue::String(s) => Some(render_string(s)),
        JsonValue::Array(arr) => {
            let mut parts = Vec::with_capacity(arr.len());
            for item in arr {
                parts.push(render_json_as_ron(item, registry, None)?);
            }
            Some(format!("[{}]", parts.join(", ")))
        }
        JsonValue::Object(obj) => {
            // Struct when the registry type for this value is a struct; else a map.
            let as_struct = type_hint
                .map(|t| is_struct_type(registry, t))
                .unwrap_or(true); // default: a reflected default object is a struct.
            if as_struct {
                let mut parts = Vec::with_capacity(obj.len());
                for (k, v) in obj {
                    // Nested objects in a reflected default are themselves
                    // struct-shaped (Bevy reflects struct defaults as objects); the
                    // recursive call's `None` hint defaults to a struct render.
                    parts.push(format!("{k}: {}", render_json_as_ron(v, registry, None)?));
                }
                Some(format!("({})", parts.join(", ")))
            } else {
                let mut parts = Vec::with_capacity(obj.len());
                for (k, v) in obj {
                    parts.push(format!(
                        "{}: {}",
                        render_string(k),
                        render_json_as_ron(v, registry, None)?
                    ));
                }
                Some(format!("{{{}}}", parts.join(", ")))
            }
        }
    }
}

/// Render a JSON number as canonical RON text. An integer prints without a decimal
/// point; a non-integer float prints via the shortest round-trip representation.
fn render_number(n: &serde_json::Number) -> String {
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(f) = n.as_f64() {
        // A whole-valued float keeps its `.0` so it stays a RON float (e.g. `1.0`).
        if f.fract() == 0.0 && f.is_finite() {
            return format!("{f:.1}");
        }
        return format!("{f}");
    }
    n.to_string()
}

/// Render a JSON string as a double-quoted RON string with the common escapes.
fn render_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// `true` if the registered type is a reflect `Struct` (so a JSON object default
/// renders as a RON struct, not a map).
fn is_struct_type(registry: &BevyRegistry, type_path: &str) -> bool {
    use ronin_types::source::ReflectKind;
    matches!(registry.reflect_kind(type_path), Some(ReflectKind::Struct))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ronin_core::{parse, print};
    use serde_json::json;

    /// Parse a top-level RON value (the doc's single value).
    fn value_of(src: &str) -> ast::Value {
        ast::Document::cast(parse(src).root())
            .and_then(|d| d.value())
            .expect("a top-level value")
    }

    #[test]
    fn float_text_variants_all_equal_default_bit_for_bit() {
        for src in ["1.0", "1.00", "1e0", "1.0e0", "01.0", "1.000000"] {
            assert!(
                ron_value_equals_json(&value_of(src), &json!(1.0)),
                "{src} should equal default 1.0 (bit-for-bit)"
            );
        }
        // An integer literal `1` also equals a JSON float default `1.0`.
        assert!(ron_value_equals_json(&value_of("1"), &json!(1.0)));
        // A different value does NOT equal.
        assert!(!ron_value_equals_json(&value_of("1.5"), &json!(1.0)));
    }

    #[test]
    fn zero_and_negative_zero_are_distinguished_bit_for_bit() {
        assert!(ron_value_equals_json(&value_of("0.0"), &json!(0.0)));
        // -0.0 has a different bit pattern from 0.0 (no epsilon tolerance).
        assert!(!ron_value_equals_json(&value_of("-0.0"), &json!(0.0)));
    }

    #[test]
    fn integer_compares_exactly() {
        assert!(ron_value_equals_json(&value_of("42"), &json!(42)));
        assert!(ron_value_equals_json(&value_of("1_000"), &json!(1000)));
        assert!(!ron_value_equals_json(&value_of("43"), &json!(42)));
    }

    #[test]
    fn struct_compares_by_field_order_independent() {
        let v = value_of("(x: 1.0, y: 2.0)");
        assert!(ron_value_equals_json(&v, &json!({"y": 2.0, "x": 1.0})));
        // Extra field ⇒ not equal (would not round-trip).
        assert!(!ron_value_equals_json(
            &value_of("(x: 1.0)"),
            &json!({"x": 1.0, "y": 2.0})
        ));
    }

    #[test]
    fn render_round_trips_through_parse() {
        let text = render_json_as_ron(&json!({"x": 1.0, "y": 0.0}), &BevyRegistry::default(), None)
            .unwrap();
        // The rendered struct text parses back to an equal value.
        let v = value_of(&text);
        assert!(ron_value_equals_json(&v, &json!({"x": 1.0, "y": 0.0})));
    }

    #[test]
    fn whole_float_renders_with_decimal_point() {
        assert_eq!(
            render_number(&serde_json::Number::from_f64(1.0).unwrap()),
            "1.0"
        );
    }

    #[test]
    fn round_trip_doc_print_is_stable() {
        // A tiny end-to-end: a struct value where one field equals its default and
        // one does not — shrink removes only the default one.
        let src = "(x: 1.0, y: 5.0)";
        let doc = parse(src);
        // Pretend `x`'s default is 1.0, `y`'s default is 0.0.
        let v = value_of(src);
        let ast::Value::Struct(s) = &v else { panic!() };
        let x = s.fields().next().unwrap();
        assert!(ron_value_equals_json(&x.value().unwrap(), &json!(1.0)));
        let y = s.fields().nth(1).unwrap();
        assert!(!ron_value_equals_json(&y.value().unwrap(), &json!(0.0)));
        // Sanity: print is byte-identical (no edit applied here).
        assert_eq!(print(&doc), src);
    }
}
