//! The normalized [`TypeModel`] and its node/reference types {TR-001, TR-006}.
//!
//! The model is shaped on **JSON Schema 2020-12**: named types live in a
//! `$defs`-style registry ([`TypeModel::named_types`]) and reference one another
//! through [`TypeRef::Named`] (`$ref`-style indirection), so recursive and
//! mutually-recursive types are finite (HINT-005, never inline-expanded).
//!
//! These in-memory types are *not* themselves the serialized interchange — they
//! are serde-serializable for convenience and tests, but the stable
//! JSON-Schema-2020-12-shaped interchange a future WASM core consumes is
//! produced by [`crate::serialize`]. Keeping the in-memory model and the
//! interchange separate lets the model stay ergonomic while the wire form stays
//! validator-consumable.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::diagnostics::AcquisitionDiagnostic;
use crate::extension::{RonKind, RonTypeExtension};

/// The pinned JSON Schema dialect for the serialized interchange (NEW-CONFIG).
///
/// Re-exported from `schemars`' constant so the pin is authoritative and tracks
/// the crate we ingest schemas with in later waves.
pub const SCHEMA_DIALECT_2020_12: &str = schemars::consts::meta_schemas::DRAFT2020_12;

/// A scalar JSON Schema primitive (`type` keyword) — the leaf kinds.
///
/// RON-specific scalars (`char`, bytes, unit) are modeled as a base primitive
/// plus an [`RonTypeExtension`] on the node, not as new primitive variants, so
/// stripping the extension still leaves a valid JSON-Schema node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Primitive {
    /// JSON `"boolean"`.
    Boolean,
    /// JSON `"integer"`.
    Integer,
    /// JSON `"number"` (floating point).
    Number,
    /// JSON `"string"`.
    String,
    /// JSON `"null"`.
    Null,
}

impl Primitive {
    /// The JSON Schema `type` keyword for this primitive.
    #[inline]
    #[must_use]
    pub fn as_type_keyword(self) -> &'static str {
        match self {
            Primitive::Boolean => "boolean",
            Primitive::Integer => "integer",
            Primitive::Number => "number",
            Primitive::String => "string",
            Primitive::Null => "null",
        }
    }
}

/// A reference to a type, either a named entry in [`TypeModel::named_types`] or
/// a small inline node (TR-006 recursion-safety seam).
///
/// Named references are the only way named (recursive) types point at one
/// another — they map to JSON Schema `{"$ref": "#/$defs/<name>"}`. Inline refs
/// embed an anonymous node directly (`array` element types, `map` values, etc.).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeRef {
    /// A `$ref` to a named type in the registry. The target is resolved against
    /// [`TypeModel::named_types`]; an unresolved target is itself a registered
    /// `unknown` node, never a dangling reference (TR-006).
    Named(String),
    /// An inline anonymous node (no registry entry).
    Inline(Box<TypeNode>),
}

impl TypeRef {
    /// Construct a named (`$ref`) reference.
    #[inline]
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        TypeRef::Named(name.into())
    }

    /// Construct an inline reference to `node`.
    #[inline]
    #[must_use]
    pub fn inline(node: TypeNode) -> Self {
        TypeRef::Inline(Box::new(node))
    }

    /// The referenced name, if this is a named reference.
    #[inline]
    #[must_use]
    pub fn as_named(&self) -> Option<&str> {
        match self {
            TypeRef::Named(name) => Some(name),
            TypeRef::Inline(_) => None,
        }
    }
}

/// A field of an `object`/struct node — serde-faithful key + optionality
/// (TR-005 fields; populated by the later `syn` adapter).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    /// The on-the-wire key after serde `rename`/`rename_all` is applied.
    pub serialized_key: String,
    /// The field's value type.
    pub value: TypeRef,
    /// `true` when the field may be absent (serde `default`/`skip`/`Option`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub optional: bool,
    /// `true` when the field is inlined per serde `flatten`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub flatten: bool,
}

/// The shape of an enum variant (TR-003), matching serde's variant forms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VariantShape {
    /// A unit variant (`Foo`) — no payload.
    Unit,
    /// A newtype variant (`Foo(T)`) — a single inner type.
    Newtype(TypeRef),
    /// A tuple variant (`Foo(A, B, ...)`) — fixed-arity ordered payload.
    Tuple(Vec<TypeRef>),
    /// A struct variant (`Foo { .. }`) — named fields.
    Struct(Vec<Field>),
}

/// A variant of an `enum` node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Variant {
    /// The on-the-wire variant name after serde `rename`/`rename_all`.
    pub serialized_name: String,
    /// The variant payload shape.
    pub shape: VariantShape,
}

/// serde enum tagging strategy (TR-005; populated by the later `syn` adapter).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Discriminator {
    /// Default serde representation (externally tagged).
    External,
    /// `#[serde(tag = "...")]` — internally tagged.
    Internal {
        /// The tag field name.
        tag: String,
    },
    /// `#[serde(tag = "...", content = "...")]` — adjacently tagged.
    Adjacent {
        /// The tag field name.
        tag: String,
        /// The content field name.
        content: String,
    },
    /// `#[serde(untagged)]`.
    Untagged,
}

/// The kind-specific payload of a [`TypeNode`].
///
/// Each variant maps to a defined JSON-Schema-2020-12 construct (object →
/// `properties`, enum → `oneOf`, sequence → `items`, tuple → `prefixItems`, map
/// → `additionalProperties`, primitive → `type`, option → nullable `oneOf`),
/// plus `x-ron-*` where the node carries an [`RonTypeExtension`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum NodeKind {
    /// A struct / JSON `object` with named fields.
    Object {
        /// The ordered fields.
        fields: Vec<Field>,
        /// `true` ⇒ serde `deny_unknown_fields` (JSON Schema
        /// `additionalProperties: false`).
        #[serde(default, skip_serializing_if = "is_false")]
        deny_unknown_fields: bool,
    },
    /// A sum type / Rust `enum` (JSON Schema `oneOf`).
    Enum {
        /// The variants.
        variants: Vec<Variant>,
        /// serde tagging strategy.
        #[serde(default = "default_discriminator")]
        discriminator: Discriminator,
    },
    /// A homogeneous sequence (`Vec<T>` / JSON `array` with `items`).
    Sequence {
        /// The element type.
        element: TypeRef,
    },
    /// A fixed-arity tuple (`(A, B, ...)` / JSON `array` with `prefixItems`).
    /// Always carries a [`RonKind::Tuple`] extension on its node.
    Tuple {
        /// The ordered element types; length is the tuple arity.
        elements: Vec<TypeRef>,
    },
    /// A map (`HashMap<K, V>`). String-keyed maps are plain JSON objects;
    /// non-string-keyed maps additionally carry a [`RonKind::NonStringKeyMap`]
    /// extension and record the key type.
    Map {
        /// The key type (`String` for ordinary JSON-object maps).
        key: TypeRef,
        /// The value type.
        value: TypeRef,
    },
    /// A scalar primitive (`type` keyword).
    Primitive {
        /// The scalar kind.
        primitive: Primitive,
    },
    /// An `Option<T>` (nullable). Carries a [`RonKind::Option`] extension.
    Option {
        /// The inner `Some` type.
        inner: TypeRef,
    },
    /// A type that could not be resolved — a **first-class** node, never an
    /// error or a dangling reference (TR-006, ADR-0004).
    Unknown,
}

fn default_discriminator() -> Discriminator {
    Discriminator::External
}

/// A single type descriptor: a [`NodeKind`] payload plus an optional `x-ron-*`
/// [`RonTypeExtension`] (0..1 per node, TR-002).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypeNode {
    /// The structural kind + payload.
    #[serde(flatten)]
    pub kind: NodeKind,
    /// Optional RON extension annotation (omitted when empty).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ron_extension: Option<RonTypeExtension>,
}

impl TypeNode {
    /// Construct a node from a [`NodeKind`] with no RON extension.
    #[inline]
    #[must_use]
    pub fn new(kind: NodeKind) -> Self {
        Self {
            kind,
            ron_extension: None,
        }
    }

    /// The first-class `unknown` node (TR-006).
    #[inline]
    #[must_use]
    pub fn unknown() -> Self {
        Self::new(NodeKind::Unknown)
    }

    /// A scalar primitive node.
    #[inline]
    #[must_use]
    pub fn primitive(primitive: Primitive) -> Self {
        Self::new(NodeKind::Primitive { primitive })
    }

    /// A fixed-arity tuple node, with the matching [`RonKind::Tuple`] extension
    /// auto-attached (arity = `elements.len()`).
    #[must_use]
    pub fn tuple(elements: Vec<TypeRef>) -> Self {
        let arity = elements.len();
        Self {
            kind: NodeKind::Tuple { elements },
            ron_extension: Some(RonTypeExtension::tuple(arity)),
        }
    }

    /// A `char` node: base `string` primitive + [`RonKind::Char`] extension.
    #[must_use]
    pub fn char_() -> Self {
        Self {
            kind: NodeKind::Primitive {
                primitive: Primitive::String,
            },
            ron_extension: Some(RonTypeExtension::kind(RonKind::Char)),
        }
    }

    /// A unit `()` node: base `null` primitive + [`RonKind::Unit`] extension.
    #[must_use]
    pub fn unit() -> Self {
        Self {
            kind: NodeKind::Primitive {
                primitive: Primitive::Null,
            },
            ron_extension: Some(RonTypeExtension::kind(RonKind::Unit)),
        }
    }

    /// A bytes node: base `string` primitive + [`RonKind::Bytes`] extension.
    #[must_use]
    pub fn bytes() -> Self {
        Self {
            kind: NodeKind::Primitive {
                primitive: Primitive::String,
            },
            ron_extension: Some(RonTypeExtension::kind(RonKind::Bytes)),
        }
    }

    /// An `Option<inner>` node with the [`RonKind::Option`] extension attached.
    #[must_use]
    pub fn option(inner: TypeRef) -> Self {
        Self {
            kind: NodeKind::Option { inner },
            ron_extension: Some(RonTypeExtension::kind(RonKind::Option)),
        }
    }

    /// A non-string-key map node with the [`RonKind::NonStringKeyMap`]
    /// extension attached.
    #[must_use]
    pub fn non_string_key_map(key: TypeRef, value: TypeRef) -> Self {
        Self {
            kind: NodeKind::Map { key, value },
            ron_extension: Some(RonTypeExtension::kind(RonKind::NonStringKeyMap)),
        }
    }

    /// Builder: attach (or replace) the RON extension.
    #[must_use]
    pub fn with_ron_extension(mut self, extension: RonTypeExtension) -> Self {
        self.ron_extension = Some(extension);
        self
    }

    /// `true` if this is the first-class `unknown` node.
    #[inline]
    #[must_use]
    pub fn is_unknown(&self) -> bool {
        matches!(self.kind, NodeKind::Unknown)
    }
}

/// The normalized registry of named types — RONin's single internal type model
/// (TR-001, TR-014, TR-015).
///
/// `named_types` is the `$defs`-style table; `definitions_order` fixes a
/// deterministic iteration/serialization order so the serialized interchange is
/// stable (TR-012). An **empty** model is a legal structural-only fallback, not
/// an error (TR-015).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TypeModel {
    /// The pinned dialect identifier ([`SCHEMA_DIALECT_2020_12`]).
    #[serde(default = "default_dialect")]
    pub schema_dialect: String,
    /// The addressable named-type registry (`$defs`).
    #[serde(default)]
    pub named_types: BTreeMap<String, TypeNode>,
    /// Deterministic insertion order for the named types (drives serialization
    /// order; `named_types` itself is a sorted map for lookup).
    #[serde(default)]
    pub definitions_order: Vec<String>,
    /// Accumulated non-fatal acquisition findings (TR-011).
    #[serde(default)]
    pub diagnostics: Vec<AcquisitionDiagnostic>,
    /// Model-level summary of active RON extension flags influencing surface
    /// forms (TR-002): `implicit_some`, `unwrap_newtypes`,
    /// `unwrap_variant_newtypes` (sorted, deduped).
    #[serde(default)]
    pub ron_extensions_active: Vec<String>,
}

fn default_dialect() -> String {
    SCHEMA_DIALECT_2020_12.to_string()
}

impl TypeModel {
    /// An empty structural-only model with the pinned dialect (TR-015).
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema_dialect: SCHEMA_DIALECT_2020_12.to_string(),
            named_types: BTreeMap::new(),
            definitions_order: Vec::new(),
            diagnostics: Vec::new(),
            ron_extensions_active: Vec::new(),
        }
    }

    /// `true` when no named type has been registered — the structural-only
    /// fallback state (TR-015). Diagnostics may still be present.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.named_types.is_empty()
    }

    /// Insert (or replace) a named type, preserving deterministic order. If the
    /// name is new it is appended to [`TypeModel::definitions_order`].
    pub fn insert_named(&mut self, name: impl Into<String>, node: TypeNode) {
        let name = name.into();
        if !self.named_types.contains_key(&name) {
            self.definitions_order.push(name.clone());
        }
        self.named_types.insert(name, node);
    }

    /// Addressable named-type lookup by name/path — the entry point a consumer
    /// (E006) uses to bind a RON document root to a type (TR-014).
    ///
    /// Returns `None` if no type with that name is registered. (A registered but
    /// unresolved type is an explicit `unknown` node, which IS returned.)
    #[inline]
    #[must_use]
    pub fn lookup(&self, name: &str) -> Option<&TypeNode> {
        self.named_types.get(name)
    }

    /// `true` if a named type with this name exists in the registry.
    #[inline]
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.named_types.contains_key(name)
    }

    /// Resolve a [`TypeRef`] to its [`TypeNode`].
    ///
    /// For [`TypeRef::Named`] this looks the name up in the registry; for
    /// [`TypeRef::Inline`] it returns the embedded node. Returns `None` only for
    /// a named ref whose target is not registered — by invariant such a target
    /// should instead be a registered `unknown` node, so `None` signals a
    /// malformed model rather than a normal unresolved type (TR-006).
    #[must_use]
    pub fn resolve<'a>(&'a self, type_ref: &'a TypeRef) -> Option<&'a TypeNode> {
        match type_ref {
            TypeRef::Named(name) => self.lookup(name),
            TypeRef::Inline(node) => Some(node),
        }
    }

    /// Iterate named types in deterministic [`TypeModel::definitions_order`].
    pub fn iter_ordered(&self) -> impl Iterator<Item = (&str, &TypeNode)> {
        self.definitions_order
            .iter()
            .filter_map(|name| self.named_types.get(name).map(|n| (name.as_str(), n)))
    }

    /// Record an active RON extension flag (deduped, kept sorted).
    pub fn add_active_extension(&mut self, flag: impl Into<String>) {
        let flag = flag.into();
        if !self.ron_extensions_active.contains(&flag) {
            self.ron_extensions_active.push(flag);
            self.ron_extensions_active.sort();
        }
    }
}

#[inline]
fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TR-006: `unknown` is a first-class node — registered, looked up, and
    /// recognised, never a missing/dangling entry.
    #[test]
    fn unknown_is_first_class() {
        let mut model = TypeModel::new();
        model.insert_named("Foreign", TypeNode::unknown());

        // Looked up like any other type.
        let node = model.lookup("Foreign").expect("unknown must be registered");
        assert!(node.is_unknown());
        assert!(model.contains("Foreign"));

        // A reference to it resolves to the explicit unknown node (not None).
        let r = TypeRef::named("Foreign");
        let resolved = model
            .resolve(&r)
            .expect("named ref resolves to unknown node");
        assert!(resolved.is_unknown());
    }

    /// TR-014 / HINT-005: a recursive named type resolves finitely via named-ref
    /// indirection (no inline expansion, no infinite recursion).
    #[test]
    fn recursive_named_type_resolves_finitely() {
        // struct Node { next: Option<Node> }
        let mut model = TypeModel::new();
        let next_field = Field {
            serialized_key: "next".into(),
            value: TypeRef::inline(TypeNode::option(TypeRef::named("Node"))),
            optional: true,
            flatten: false,
        };
        model.insert_named(
            "Node",
            TypeNode::new(NodeKind::Object {
                fields: vec![next_field],
                deny_unknown_fields: false,
            }),
        );

        // Walk one full cycle: Node -> field "next" -> Option -> $ref Node.
        let node = model.lookup("Node").expect("Node registered");
        let NodeKind::Object { fields, .. } = &node.kind else {
            panic!("Node must be an object");
        };
        let opt_ref = &fields[0].value;
        let opt_node = model.resolve(opt_ref).expect("inline option resolves");
        let NodeKind::Option { inner } = &opt_node.kind else {
            panic!("field must be Option");
        };
        // The recursive edge is a NAMED ref back to "Node" — finite, not expanded.
        assert_eq!(inner.as_named(), Some("Node"));
        assert!(model.resolve(inner).expect("cycle resolves").kind == node.kind);
    }

    /// TR-002 wiring: a tuple node carries a `RonKind::Tuple` extension whose
    /// arity matches its element count; char/unit/bytes/option/non-string-key
    /// map all attach their extension.
    #[test]
    fn ron_kinds_attach_extensions() {
        let t = TypeNode::tuple(vec![
            TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
            TypeRef::inline(TypeNode::char_()),
        ]);
        let ext = t.ron_extension.as_ref().unwrap();
        assert_eq!(ext.ron_kind, Some(RonKind::Tuple));
        assert_eq!(ext.tuple_arity, Some(2));

        assert_eq!(
            TypeNode::char_().ron_extension.unwrap().ron_kind,
            Some(RonKind::Char)
        );
        assert_eq!(
            TypeNode::unit().ron_extension.unwrap().ron_kind,
            Some(RonKind::Unit)
        );
        assert_eq!(
            TypeNode::bytes().ron_extension.unwrap().ron_kind,
            Some(RonKind::Bytes)
        );
        assert_eq!(
            TypeNode::option(TypeRef::named("X"))
                .ron_extension
                .unwrap()
                .ron_kind,
            Some(RonKind::Option)
        );
        assert_eq!(
            TypeNode::non_string_key_map(
                TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
                TypeRef::inline(TypeNode::primitive(Primitive::String)),
            )
            .ron_extension
            .unwrap()
            .ron_kind,
            Some(RonKind::NonStringKeyMap)
        );
    }

    /// TR-015: an empty model is valid (structural-only), not an error.
    #[test]
    fn empty_model_is_structural_only() {
        let model = TypeModel::new();
        assert!(model.is_empty());
        assert_eq!(model.schema_dialect, SCHEMA_DIALECT_2020_12);
        assert!(model.lookup("Anything").is_none());
    }

    /// Insertion order is preserved deterministically even though the registry
    /// is a sorted map.
    #[test]
    fn definitions_order_is_insertion_order() {
        let mut model = TypeModel::new();
        model.insert_named("Zebra", TypeNode::unknown());
        model.insert_named("Apple", TypeNode::unknown());
        let order: Vec<&str> = model.iter_ordered().map(|(n, _)| n).collect();
        assert_eq!(order, vec!["Zebra", "Apple"]);
    }
}
