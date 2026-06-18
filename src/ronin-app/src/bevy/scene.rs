//! Scene model — interprets a `.scn.ron` CST into resources + entities +
//! per-entity components keyed by fully-qualified type path (FR-004).
//!
//! # The model is a read projection (FR-004/FR-011/SC-003)
//!
//! [`SceneModel`] is the Bevy-specific, **read-only** interpretation of a
//! `.scn.ron` CST: the scene's top-level `resources` map (type path → value) and
//! its `entities` map (entity id → components map: type path → component value).
//! Each interpreted value is a [`SceneValueRef`] linking the value back to its
//! exact CST [`SyntaxNode`] and byte [`TextRange`] so a downstream diagnostic or
//! elision lands on the right span. Deriving the model changes **zero** document
//! bytes (SC-003): it only reads the CST, never the source of truth.
//!
//! # The `.scn.ron` shape it interprets (FR-004, research)
//!
//! A Bevy DynamicScene is one top-level RON struct-like value:
//!
//! ```text
//! (
//!     resources: { "full::type::path": <value>, ... },
//!     entities: {
//!         <entity-id>: ( components: { "full::type::path": <value>, ... } ),
//!         ...
//!     },
//! )
//! ```
//!
//! `resources` is a map of fully-qualified type-path string → resource value;
//! `entities` is a map of integer entity id → an entity struct carrying a
//! `components` map of fully-qualified type-path string → component value.
//! Component/resource keys are plain RON map-key strings (consumed as data, not
//! crate deps).
//!
//! # Degrade-safe over awkward scenes (FR-004/FR-008, Edge Cases)
//!
//! Interpretation NEVER panics and NEVER errors — a non-scene or malformed
//! top-level value yields an empty-or-partial [`SceneModel`]:
//!
//! * **Omitted / empty `resources`** is valid — the model reads zero resources
//!   and still projects every entity.
//! * **Entity ids are arbitrary integer keys** — non-contiguous, very large, and
//!   even **duplicated** ids each project as a distinct [`SceneEntity`] (no dedup
//!   / merge / contiguity assumption); a duplicate is a structural concern the
//!   existing structural diagnostics surface, never a crash here.
//! * **Unparseable regions are skipped** — an entry whose key or value did not
//!   parse is omitted and the parseable remainder is still modeled.

use ronin_core::{ast, CstDocument, SyntaxNode, TextRange};

/// Whether a [`SceneValueRef`] is a top-level scene resource or an entity's
/// component (data-model `SceneModel`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SceneValueKind {
    /// A top-level `resources` entry (no owning entity).
    Resource,
    /// An entity `components` entry (carries the owning entity id).
    Component,
}

/// A single interpreted scene value — a resource or a component — linked to its
/// exact CST node + byte range (data-model `SceneModel.SceneValueRef`).
///
/// The [`type_path`](Self::type_path) is the fully-qualified Rust type path the
/// registry lookup keys on. The [`value_node`](Self::value_node) /
/// [`range`](Self::range) link the value back to the precise CST span so a
/// diagnostic or elision targets the right bytes (FR-005/FR-007); the range is
/// always the value node's real extent, never fabricated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SceneValueRef {
    /// The fully-qualified type path (the registry lookup key, FR-004/FR-005).
    type_path: String,
    /// The value's CST node (read identity for precise diagnostics / elision).
    value_node: SyntaxNode,
    /// The value node's absolute byte range (precise-range linkage, FR-005/007).
    range: TextRange,
    /// Whether this is a resource or a component.
    kind: SceneValueKind,
    /// For a component, the owning entity id; `None` for a resource.
    entity_id: Option<i128>,
}

impl SceneValueRef {
    /// The fully-qualified type path (the registry lookup key).
    #[must_use]
    pub fn type_path(&self) -> &str {
        &self.type_path
    }

    /// The value's CST node (read-only identity for diagnostics / elision).
    #[must_use]
    pub fn value_node(&self) -> &SyntaxNode {
        &self.value_node
    }

    /// The value node's absolute byte range (precise-range linkage).
    #[must_use]
    pub fn range(&self) -> TextRange {
        self.range
    }

    /// Whether this value is a resource or a component.
    #[must_use]
    pub fn kind(&self) -> SceneValueKind {
        self.kind
    }

    /// The owning entity id for a component, or `None` for a resource.
    #[must_use]
    pub fn entity_id(&self) -> Option<i128> {
        self.entity_id
    }
}

/// One entity of the scene: its id plus the component values it carries
/// (data-model `EntityModel`).
///
/// The `id` is the integer key verbatim from the scene's `entities` map; it is
/// **not** assumed stable, contiguous, or unique (a duplicate id projects as a
/// separate [`SceneEntity`]). The `components` are in source order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SceneEntity {
    /// The entity's integer id (arbitrary key — not contiguous/unique).
    id: i128,
    /// The entity's components, keyed by fully-qualified type path, in source
    /// order.
    components: Vec<SceneValueRef>,
}

impl SceneEntity {
    /// The entity's integer id (an arbitrary key — see the type docs).
    #[must_use]
    pub fn id(&self) -> i128 {
        self.id
    }

    /// The entity's component value refs, in source order.
    #[must_use]
    pub fn components(&self) -> &[SceneValueRef] {
        &self.components
    }
}

/// The Bevy interpretation of a `.scn.ron` CST: resources + entities → components
/// (data-model `SceneModel`).
///
/// A transient, **read-only** projection derived from the document's CST via
/// [`SceneModel::from_cst`]. It never mutates the CST and changes zero bytes
/// (SC-003). Awkward scenes degrade safely (see the module docs): the model is
/// always the parseable remainder, never an error.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SceneModel {
    /// The scene's top-level `resources` (type path → value), in source order;
    /// empty when `resources` is omitted/empty (FR-004 Edge Cases).
    resources: Vec<SceneValueRef>,
    /// The scene's entities, in source order; an entry per `entities` map key
    /// (duplicates kept distinct).
    entities: Vec<SceneEntity>,
}

impl SceneModel {
    /// Interpret a document's CST into a [`SceneModel`] (FR-004).
    ///
    /// A pure read over the CST — zero bytes (SC-003). Locates the top-level
    /// `resources` and `entities` fields of the scene struct, reads each
    /// resource / component as a [`SceneValueRef`] keyed by its fully-qualified
    /// type path, and links each to its precise CST node + byte range. Degrades
    /// safely (never panics): a non-scene / malformed / partial top-level value
    /// yields an empty-or-partial model, an omitted `resources` reads as empty,
    /// arbitrary/duplicate/large entity ids are tolerated, and an unparseable
    /// entry is skipped (the parseable remainder is still modeled).
    #[must_use]
    pub fn from_cst(doc: &CstDocument) -> Self {
        let Some(top) = ast::Document::cast(doc.root()).and_then(|d| d.value()) else {
            // An empty / trivia-only document is not a scene — empty model.
            return Self::default();
        };
        // The scene is a struct-like top-level value with `resources` /
        // `entities` fields. A non-struct top-level value (a bare list, a
        // scalar, an error region) is not a scene → empty model, never a crash.
        let Some(fields) = scene_fields(&top) else {
            return Self::default();
        };

        let resources = fields
            .iter()
            .find(|(name, _)| name == "resources")
            .and_then(|(_, value)| as_map(value))
            .map(|map| read_resources(&map))
            .unwrap_or_default();

        let entities = fields
            .iter()
            .find(|(name, _)| name == "entities")
            .and_then(|(_, value)| as_map(value))
            .map(|map| read_entities(&map))
            .unwrap_or_default();

        Self {
            resources,
            entities,
        }
    }

    /// The scene's top-level resource value refs, in source order (empty when
    /// `resources` is omitted/empty).
    #[must_use]
    pub fn resources(&self) -> &[SceneValueRef] {
        &self.resources
    }

    /// The scene's entities, in source order.
    #[must_use]
    pub fn entities(&self) -> &[SceneEntity] {
        &self.entities
    }

    /// Every component value ref across all entities, in source order
    /// (entity-major, then component order).
    pub fn components(&self) -> impl Iterator<Item = &SceneValueRef> + '_ {
        self.entities.iter().flat_map(|e| e.components.iter())
    }

    /// Every interpreted value ref — resources then components — in source
    /// order, for a single uniform pass (e.g. scene-aware validation).
    pub fn entries(&self) -> impl Iterator<Item = &SceneValueRef> + '_ {
        self.resources.iter().chain(self.components())
    }
}

/// The `(field_name, value)` pairs of a scene's top-level struct-like value, or
/// `None` when the top-level value is not struct-like (not a scene).
///
/// A Bevy scene's top-level value parses as an anonymous struct
/// `(resources: .., entities: ..)`; a named tuple-struct form
/// (`Scene(resources: .., entities: ..)`) parses the same way. A bare tuple,
/// list, scalar, or error region is not a scene.
fn scene_fields(top: &ast::Value) -> Option<Vec<(String, ast::Value)>> {
    let ast::Value::Struct(s) = top else {
        return None;
    };
    Some(
        s.fields()
            .filter_map(|f| Some((f.name_text()?, f.value()?)))
            .collect(),
    )
}

/// Read the `resources` map into resource value refs, in source order.
///
/// Each entry's key is a fully-qualified type-path string; an entry whose key is
/// not a string literal (an unparseable / malformed key) is skipped.
fn read_resources(map: &ast::Map) -> Vec<SceneValueRef> {
    map.entries()
        .filter_map(|entry| {
            let type_path = entry_type_path(&entry)?;
            let value = entry.value()?;
            Some(make_ref(type_path, &value, SceneValueKind::Resource, None))
        })
        .collect()
}

/// Read the `entities` map into entity models, in source order.
///
/// Each entry's key is an integer entity id; an entry whose key is not an integer
/// literal (an unparseable / malformed key) is skipped. Duplicate / non-contiguous
/// / very large ids are tolerated — each entry projects as a distinct entity.
fn read_entities(map: &ast::Map) -> Vec<SceneEntity> {
    map.entries()
        .filter_map(|entry| {
            let id = entry_entity_id(&entry)?;
            let value = entry.value()?;
            // The entity value is a struct `( components: { .. } )`; read its
            // `components` map. A missing/empty `components` yields no components
            // (still a valid entity), never a crash.
            let components = entity_components(&value, id);
            Some(SceneEntity { id, components })
        })
        .collect()
}

/// Read an entity value's `components` map into component value refs.
///
/// The entity value is a struct-like value `( components: { .. } )`. A value that
/// is not struct-like, or that lacks a `components` map, yields no components.
fn entity_components(entity_value: &ast::Value, entity_id: i128) -> Vec<SceneValueRef> {
    let Some(fields) = scene_fields(entity_value) else {
        return Vec::new();
    };
    fields
        .iter()
        .find(|(name, _)| name == "components")
        .and_then(|(_, value)| as_map(value))
        .map(|map| {
            map.entries()
                .filter_map(|entry| {
                    let type_path = entry_type_path(&entry)?;
                    let value = entry.value()?;
                    Some(make_ref(
                        type_path,
                        &value,
                        SceneValueKind::Component,
                        Some(entity_id),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Cast a value to a [`ast::Map`], or `None` when it is not a map.
///
/// `resources`, `entities`, and `components` are all RON maps `{ k: v, .. }`.
fn as_map(value: &ast::Value) -> Option<ast::Map> {
    match value {
        ast::Value::Map(m) => Some(m.clone()),
        _ => None,
    }
}

/// The fully-qualified type path of a map entry whose key is a quoted string
/// literal (the unescaped inner text), or `None` for a non-string / malformed
/// key.
fn entry_type_path(entry: &ast::MapEntry) -> Option<String> {
    let ast::Value::Literal(lit) = entry.key()? else {
        return None;
    };
    match lit.token_kind() {
        Some(ronin_core::SyntaxKind::String | ronin_core::SyntaxKind::RawString) => {
            unquote_string(&lit.text()?)
        }
        _ => None,
    }
}

/// The integer entity id of a map entry whose key is an integer literal, or
/// `None` for a non-integer / malformed key.
///
/// Parsed as `i128` so very large / negative ids are tolerated (FR-004); a value
/// that does not fit / does not parse skips the entry rather than crashing.
fn entry_entity_id(entry: &ast::MapEntry) -> Option<i128> {
    let ast::Value::Literal(lit) = entry.key()? else {
        return None;
    };
    if lit.token_kind() != Some(ronin_core::SyntaxKind::Integer) {
        return None;
    }
    // RON integers may carry `_` separators; strip them before parsing.
    let text = lit.text()?;
    let cleaned: String = text.chars().filter(|c| *c != '_').collect();
    cleaned.parse::<i128>().ok()
}

/// Build a [`SceneValueRef`] from a type path + value node (capturing its range).
fn make_ref(
    type_path: String,
    value: &ast::Value,
    kind: SceneValueKind,
    entity_id: Option<i128>,
) -> SceneValueRef {
    let node = value.syntax().clone();
    let range = node.text_range();
    SceneValueRef {
        type_path,
        value_node: node,
        range,
        kind,
        entity_id,
    }
}

/// Unquote a RON string literal's verbatim token text into its inner type path.
///
/// Handles a plain double-quoted string (`"a::b"`) and a raw string
/// (`r"a::b"` / `r#"a::b"#`). Escape sequences inside a plain string are NOT
/// generally interpreted — a type path is a plain identifier path with no escapes,
/// so stripping the surrounding delimiters is sufficient and lossless for the key.
/// Returns `None` when the delimiters are malformed.
fn unquote_string(verbatim: &str) -> Option<String> {
    let bytes = verbatim.as_bytes();
    if verbatim.starts_with('"') && verbatim.ends_with('"') && verbatim.len() >= 2 {
        return Some(verbatim[1..verbatim.len() - 1].to_string());
    }
    // Raw string: r"..." or r#"..."# / r##"..."## etc.
    if bytes.first() == Some(&b'r') {
        let after_r = &verbatim[1..];
        let hashes = after_r.bytes().take_while(|b| *b == b'#').count();
        let open = format!("{}\"", "#".repeat(hashes));
        let close = format!("\"{}", "#".repeat(hashes));
        let body = &after_r[hashes..];
        if body.starts_with('"') && after_r.ends_with(&close) && after_r.len() >= open.len() {
            let inner = &after_r[hashes + 1..after_r.len() - hashes - 1];
            return Some(inner.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use ronin_core::parse;

    /// The type paths of a value-ref slice, in order.
    fn paths(refs: &[SceneValueRef]) -> Vec<&str> {
        refs.iter().map(SceneValueRef::type_path).collect()
    }

    #[test]
    fn valid_scene_reads_resources_and_components() {
        let src = r#"(
    resources: {
        "my::Res": (level: 1),
    },
    entities: {
        0: (components: {
            "my::A": (x: 1),
            "my::B": (y: 2),
        }),
        1: (components: {
            "my::A": (x: 3),
        }),
    },
)"#;
        let model = SceneModel::from_cst(&parse(src));

        // Resources: one ref, keyed by its unquoted type path.
        assert_eq!(paths(model.resources()), vec!["my::Res"]);
        assert_eq!(model.resources()[0].kind(), SceneValueKind::Resource);
        assert_eq!(model.resources()[0].entity_id(), None);

        // Entities: two, in source order, with their components.
        let entities = model.entities();
        assert_eq!(entities.len(), 2);
        assert_eq!(entities[0].id(), 0);
        assert_eq!(paths(entities[0].components()), vec!["my::A", "my::B"]);
        assert_eq!(entities[1].id(), 1);
        assert_eq!(paths(entities[1].components()), vec!["my::A"]);

        // Each component records its owning entity id + kind.
        let a0 = &entities[0].components()[0];
        assert_eq!(a0.kind(), SceneValueKind::Component);
        assert_eq!(a0.entity_id(), Some(0));
    }

    #[test]
    fn value_refs_carry_precise_cst_ranges() {
        // The resource value `(level: 1)` must map to its exact byte span.
        let src = r#"(resources: {"my::Res": (level: 1)}, entities: {})"#;
        let model = SceneModel::from_cst(&parse(src));
        let res = &model.resources()[0];
        let range = res.range();
        // The recorded range is the value node's real extent (never fabricated).
        assert_eq!(res.value_node().text_range(), range);
        assert!(!range.is_empty());
        // It addresses the `(level: 1)` substring exactly.
        assert_eq!(&src[range.start()..range.end()], "(level: 1)");
    }

    #[test]
    fn components_and_entries_iterate_all_refs() {
        let src = r#"(
    resources: {"my::Res": (n: 0)},
    entities: {
        0: (components: {"my::A": (x: 1)}),
        1: (components: {"my::B": (y: 2)}),
    },
)"#;
        let model = SceneModel::from_cst(&parse(src));
        // `components()` yields component refs across entities, in source order.
        let comps: Vec<&str> = model.components().map(SceneValueRef::type_path).collect();
        assert_eq!(comps, vec!["my::A", "my::B"]);
        // `entries()` yields resources first, then components.
        let entries: Vec<&str> = model.entries().map(SceneValueRef::type_path).collect();
        assert_eq!(entries, vec!["my::Res", "my::A", "my::B"]);
    }

    #[test]
    fn omitted_resources_reads_empty_but_entities_still_read() {
        // `resources` entirely omitted: zero resources, entities intact.
        let src = r#"(entities: {7: (components: {"my::A": (x: 1)})})"#;
        let model = SceneModel::from_cst(&parse(src));
        assert!(model.resources().is_empty());
        assert_eq!(model.entities().len(), 1);
        assert_eq!(model.entities()[0].id(), 7);
        assert_eq!(paths(model.entities()[0].components()), vec!["my::A"]);
    }

    #[test]
    fn empty_resources_map_reads_empty() {
        let src = r#"(resources: {}, entities: {0: (components: {})})"#;
        let model = SceneModel::from_cst(&parse(src));
        assert!(model.resources().is_empty());
        assert_eq!(model.entities().len(), 1);
        assert!(model.entities()[0].components().is_empty());
    }

    #[test]
    fn duplicate_entity_ids_project_as_distinct_entries() {
        // A duplicate id is NOT merged/deduped — two distinct entries (FR-004).
        let src = r#"(entities: {
            0: (components: {"my::A": (x: 1)}),
            0: (components: {"my::B": (y: 2)}),
        })"#;
        let model = SceneModel::from_cst(&parse(src));
        let entities = model.entities();
        assert_eq!(entities.len(), 2, "duplicate ids kept distinct");
        assert_eq!(entities[0].id(), 0);
        assert_eq!(entities[1].id(), 0);
        assert_eq!(paths(entities[0].components()), vec!["my::A"]);
        assert_eq!(paths(entities[1].components()), vec!["my::B"]);
    }

    #[test]
    fn non_contiguous_and_large_ids_are_tolerated() {
        let src = r#"(entities: {
            999999999999: (components: {"my::A": (x: 1)}),
            5: (components: {"my::B": (y: 2)}),
        })"#;
        let model = SceneModel::from_cst(&parse(src));
        let ids: Vec<i128> = model.entities().iter().map(SceneEntity::id).collect();
        assert_eq!(ids, vec![999_999_999_999, 5]);
    }

    #[test]
    fn unparseable_region_skips_remainder_still_modeled() {
        // A garbled entity-map entry (an unparseable value) must not crash the
        // interpretation; the well-formed entries still project (FR-008).
        let src = r#"(entities: {
            0: (components: {"my::A": (x: 1)}),
            1: @@@,
            2: (components: {"my::B": (y: 2)}),
        })"#;
        let model = SceneModel::from_cst(&parse(src));
        // The parseable entities (0 and 2) remain; the garbled one degrades.
        let ids: Vec<i128> = model.entities().iter().map(SceneEntity::id).collect();
        assert!(ids.contains(&0), "entity 0 modeled");
        assert!(ids.contains(&2), "entity 2 modeled");
        // No panic reaching here is the core invariant.
    }

    #[test]
    fn non_scene_top_level_value_yields_empty_model() {
        // A bare list / scalar is not a scene — empty model, never an error.
        assert_eq!(
            SceneModel::from_cst(&parse("[1, 2, 3]")),
            SceneModel::default()
        );
        assert_eq!(SceneModel::from_cst(&parse("42")), SceneModel::default());
        assert_eq!(SceneModel::from_cst(&parse("")), SceneModel::default());
    }

    #[test]
    fn non_string_component_key_is_skipped() {
        // A component map key that is not a string literal is malformed for a
        // scene; it is skipped, the valid sibling still reads.
        let src = r#"(entities: {0: (components: {
            42: (x: 1),
            "my::A": (y: 2),
        })})"#;
        let model = SceneModel::from_cst(&parse(src));
        assert_eq!(paths(model.entities()[0].components()), vec!["my::A"]);
    }

    #[test]
    fn raw_string_type_path_is_unquoted() {
        let src = r##"(entities: {0: (components: {r#"my::Raw"#: (x: 1)})})"##;
        let model = SceneModel::from_cst(&parse(src));
        assert_eq!(paths(model.entities()[0].components()), vec!["my::Raw"]);
    }
}
