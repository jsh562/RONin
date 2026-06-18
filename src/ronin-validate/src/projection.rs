//! CST→JSON projection and the JSON-Pointer→CST `TextRange` reverse index
//! (E006/FR-003, T008).
//!
//! A [`CstJsonProjection`] re-expresses a RON document's CST value as a
//! `serde_json`-validatable instance and pairs it with a [`PointerRangeIndex`]
//! that maps each `jsonschema` `instance_path` (a JSON Pointer) back to the
//! precise CST byte [`ronin_core::TextRange`] — the **field-key span** for
//! missing/unknown-field findings, the **value span** for type/range/arity
//! findings. This is the bridge that turns an abstract schema-validator instance
//! path into the exact offending span in the document.
//!
//! # Encoding contract (must agree with [`crate::validate`])
//!
//! The CST value is projected into `serde_json` so that `jsonschema` over E004's
//! serialized `TypeModel` interchange (JSON-Schema 2020-12 + `x-ron-*`) asserts
//! the right things:
//!
//! * **Literals**: int → JSON integer, float → JSON number, bool → JSON bool,
//!   string/raw-string → JSON string (decoded best-effort), char → JSON string
//!   (the one char).
//! * **Unit `()`** → JSON `null`.
//! * **Struct** (named or anonymous) → JSON object `{field: value}`.
//! * **Tuple** → JSON array. **List** → JSON array.
//! * **Map** → JSON object; string keys verbatim, non-string keys stringified
//!   from source text (value validated via `additionalProperties`; key-type
//!   checking is out of MVP and never false-positives).
//! * **Enum variant**: `None` → `null`, `Some(x)` → project `x` (Option unwrap);
//!   every other variant → serde external-tagging `{ "<Variant>": <payload> }`
//!   (unit payload = `null`, newtype payload = the inner value, tuple payload =
//!   array, struct payload = object).
//!
//! # Pointer escaping
//!
//! JSON Pointers escape `~` as `~0` and `/` as `~1` inside object keys (RFC 6901),
//! matching how `jsonschema` renders `instance_path`. The root is `""`.
//!
//! # Read-only (FR-020/FR-022)
//!
//! Projection borrows the CST and copies only the text it needs; it never mutates
//! the tree.

use std::collections::BTreeMap;

use ronin_core::syntax::ast::{Document, Value};
use ronin_core::{CstDocument, SyntaxKind, SyntaxNode, TextRange};

/// The spans recorded for one JSON-Pointer location: the value span (the value
/// node's range) and, for an object property, the key span (the field-name
/// token's range).
///
/// A diagnostic picks key-span vs value-span by error kind: missing/unknown-field
/// findings use [`PointerSpans::key`]; type/range/arity findings use
/// [`PointerSpans::value`]. Only real CST spans are ever recorded — a field
/// missing from the source has no node and so contributes no span here
/// (FR-003 / CstJsonProjection round-trip-faithful invariant).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PointerSpans {
    /// The value node's byte range (used for type/range/arity findings).
    pub value: Option<TextRange>,
    /// The key/field-name token's byte range (used for missing/unknown-field
    /// findings). `None` for non-property locations (array items, root).
    pub key: Option<TextRange>,
}

/// A projection of a RON document's CST into a `serde_json`-validatable instance
/// plus the reverse index from JSON Pointer to the precise CST source range
/// (FR-003).
#[derive(Debug, Clone, Default)]
pub struct CstJsonProjection {
    /// The CST RON value re-expressed so the `jsonschema` crate can assert it.
    /// JSON `null` when the document has no value.
    pub instance: serde_json::Value,
    /// The reverse index from a `jsonschema` `instance_path` to the exact CST
    /// source spans (value span + optional key span).
    pub index: PointerRangeIndex,
}

impl CstJsonProjection {
    /// Build a projection from a parsed RON document.
    ///
    /// The walk is read-only (FR-020): it borrows the CST and copies only the
    /// scalar text it needs. A document with no value projects to JSON `null`
    /// with an empty index.
    #[must_use]
    pub fn from_document(doc: &CstDocument) -> Self {
        let root = doc.root();
        let Some(document) = Document::cast(root) else {
            return Self::default();
        };
        let Some(value) = document.value() else {
            return Self::default();
        };

        let mut index = PointerRangeIndex::new();
        let mut builder = PointerBuilder::new();
        let instance = project_value(&value, &mut builder, &mut index);
        Self { instance, index }
    }

    /// Build a projection **guided by the bound schema** so RON's ambiguous
    /// surface forms (a named tuple is a tuple-struct *or* a tuple/newtype enum
    /// variant; a bare ident is a unit struct *or* a unit variant; `Some(x)` is
    /// an Option) project to exactly what each schema location expects.
    ///
    /// `schema` is the schema node the root value is validated against; `defs` is
    /// the `$defs` map for `$ref`/enum resolution. Guidance is best-effort and
    /// degradation-safe: where the schema is absent/unconstrained, the
    /// schema-agnostic encoding is used (no false positives — FR-016). The pass is
    /// read-only over the CST (FR-020).
    #[must_use]
    pub fn from_document_guided(
        doc: &CstDocument,
        schema: &serde_json::Value,
        defs: &serde_json::Value,
    ) -> Self {
        let root = doc.root();
        let Some(document) = Document::cast(root) else {
            return Self::default();
        };
        let Some(value) = document.value() else {
            return Self::default();
        };
        let mut index = PointerRangeIndex::new();
        let mut builder = PointerBuilder::new();
        let g = Guide { defs };
        let instance = project_value_guided(&value, schema, &g, &mut builder, &mut index);
        Self { instance, index }
    }

    /// Build a schema-guided projection from a single value-position CST
    /// **sub-tree** rather than a whole document (E009/IP-002, AD-001).
    ///
    /// This is the generic, Bevy-agnostic entry the scene interpreter
    /// ([`ronin-app`]) drives per component: it projects ONE value node (e.g. a
    /// component value inside a scene) against ONE schema node, with the same
    /// guidance and read-only guarantees as [`Self::from_document_guided`].
    ///
    /// `subtree` must be a value-position node (`Struct`/`Tuple`/`List`/`Map`/
    /// `EnumVariant`/`Unit`/`Literal`/`Error`). A non-value node (e.g. `Root`, a
    /// struct field, a map entry) yields an empty projection (JSON `null`, empty
    /// index) — the same fail-soft as a document with no value (no panic).
    ///
    /// # Full-document coordinates
    ///
    /// rowan [`SyntaxNode`]s carry **absolute** [`TextRange`] offsets within their
    /// owning tree, and the projection records every span via
    /// `value.syntax().text_range()`. So when `subtree` is a node of the original
    /// parsed document, the spans recorded here are already in full-document byte
    /// coordinates — a diagnostic produced from this projection points at the exact
    /// offending construct in the whole document, with no offset translation
    /// needed (FR-005). The walk is read-only over the CST (FR-020).
    #[must_use]
    pub fn from_subtree_guided(
        subtree: &SyntaxNode,
        schema: &serde_json::Value,
        defs: &serde_json::Value,
    ) -> Self {
        let Some(value) = Value::cast(subtree.clone()) else {
            return Self::default();
        };
        let mut index = PointerRangeIndex::new();
        let mut builder = PointerBuilder::new();
        let g = Guide { defs };
        let instance = project_value_guided(&value, schema, &g, &mut builder, &mut index);
        Self { instance, index }
    }
}

/// Schema-guidance context for the guided projection: the `$defs` map used to
/// resolve `$ref`s while walking.
struct Guide<'a> {
    defs: &'a serde_json::Value,
}

impl Guide<'_> {
    /// Resolve a one-level local `$ref` (`#/$defs/X`) against `defs`.
    fn resolve<'b>(&'b self, node: &'b serde_json::Value) -> &'b serde_json::Value {
        let Some(reference) = node.get("$ref").and_then(serde_json::Value::as_str) else {
            return node;
        };
        let Some(name) = reference.strip_prefix("#/$defs/") else {
            return node;
        };
        self.defs.get(name).unwrap_or(node)
    }
}

/// Whether a schema node is a RON enum def (`oneOf` whose branches carry
/// `x-ron-variant`).
fn schema_is_enum(node: &serde_json::Value) -> bool {
    node.get("oneOf")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|bs| bs.iter().any(|b| b.get("x-ron-variant").is_some()))
}

/// The `x-ron-kind` keyword of a schema node, if any.
fn schema_ron_kind(node: &serde_json::Value) -> Option<&str> {
    node.get("x-ron-kind").and_then(serde_json::Value::as_str)
}

/// Whether the schema expects a JSON array (tuple/list/`type:array`).
fn schema_expects_array(node: &serde_json::Value) -> bool {
    node.get("type").and_then(serde_json::Value::as_str) == Some("array")
        || node.get("prefixItems").is_some()
}

/// Project a typed value guided by `schema`. Records the value span and recurses,
/// consulting the resolved schema to disambiguate RON-only forms.
fn project_value_guided(
    value: &Value,
    schema: &serde_json::Value,
    g: &Guide<'_>,
    builder: &mut PointerBuilder,
    index: &mut PointerRangeIndex,
) -> serde_json::Value {
    index.set_value(builder.as_pointer().to_owned(), value.syntax().text_range());
    let schema = g.resolve(schema);

    // Option: `Some(x)`/bare-value -> inner; `None` -> null (unwrap the oneOf).
    if schema_ron_kind(schema) == Some("option") {
        return project_option_guided(value, schema, g, builder, index);
    }

    // Enum def: dispatch by variant name into the matching branch's payload shape.
    if schema_is_enum(schema) {
        return project_enum_guided(value, schema, g, builder, index);
    }

    match value {
        Value::Literal(lit) => project_literal(lit),
        Value::Unit(_) => serde_json::Value::Null,
        Value::Struct(s) => project_struct_guided(s, schema, g, builder, index),
        Value::Tuple(t) => project_tuple_guided(t, schema, g, builder, index),
        Value::List(l) => project_list_guided(l, schema, g, builder, index),
        Value::Map(m) => project_map_guided(m, schema, g, builder, index),
        Value::EnumVariant(v) => project_enum_variant(v, builder, index),
        Value::Error(_) => serde_json::Value::Null,
    }
}

/// Project an Option-typed value: unwrap `Some(x)`/`None`, else treat a bare
/// value as the `Some` payload (implicit_some). The inner schema is the non-null
/// branch of the `oneOf`.
fn project_option_guided(
    value: &Value,
    schema: &serde_json::Value,
    g: &Guide<'_>,
    builder: &mut PointerBuilder,
    index: &mut PointerRangeIndex,
) -> serde_json::Value {
    // `None` -> null.
    if let Value::EnumVariant(v) = value {
        if v.name_text().as_deref() == Some("None") {
            return serde_json::Value::Null;
        }
    }
    let inner_schema = option_inner_schema(schema).unwrap_or(&serde_json::Value::Null);
    // `Some(x)` -> project x against the inner schema.
    if let Value::Tuple(t) = value {
        if tuple_name(t).as_deref() == Some("Some") {
            if let Some(inner) = t.items().next() {
                return project_value_guided(&inner, inner_schema, g, builder, index);
            }
        }
    }
    // Bare value standing for Some.
    project_value_guided(value, inner_schema, g, builder, index)
}

/// The non-null branch schema of an Option's `oneOf`.
fn option_inner_schema(schema: &serde_json::Value) -> Option<&serde_json::Value> {
    schema
        .get("oneOf")
        .and_then(serde_json::Value::as_array)?
        .iter()
        .find(|b| b.get("type").and_then(serde_json::Value::as_str) != Some("null"))
}

/// Project an enum-typed value into the external-tag form the validator expects.
/// Resolves the variant name from the CST regardless of surface syntax (bare
/// ident, named tuple, named struct, brace variant).
fn project_enum_guided(
    value: &Value,
    schema: &serde_json::Value,
    g: &Guide<'_>,
    builder: &mut PointerBuilder,
    index: &mut PointerRangeIndex,
) -> serde_json::Value {
    let Some((name, payload_kind)) = enum_variant_name_and_payload(value) else {
        // Not a recognizable variant surface form -> project structurally so the
        // validator's enum dispatch can decide (it will, conservatively, skip).
        return project_structural(value, builder, index);
    };

    // Find the matching branch to guide payload projection.
    let branch = schema
        .get("oneOf")
        .and_then(serde_json::Value::as_array)
        .and_then(|bs| {
            bs.iter().find(|b| {
                b.get("x-ron-variant").and_then(serde_json::Value::as_str) == Some(name.as_str())
            })
        });

    let payload = match payload_kind {
        VariantPayload::Unit => serde_json::Value::Null,
        VariantPayload::Tuple(items) => {
            builder.push_key(&name);
            index.set_value(builder.as_pointer().to_owned(), value.syntax().text_range());
            let json = project_variant_tuple_payload(&items, branch, g, builder, index);
            builder.pop();
            json
        }
        VariantPayload::Struct(entries) => {
            builder.push_key(&name);
            index.set_value(builder.as_pointer().to_owned(), value.syntax().text_range());
            let json = project_variant_struct_payload(&entries, branch, g, builder, index);
            builder.pop();
            json
        }
    };
    let mut map = serde_json::Map::new();
    map.insert(name, payload);
    serde_json::Value::Object(map)
}

/// One field of a struct-shaped variant payload: name, key span, optional value.
struct VariantField {
    name: String,
    key_span: TextRange,
    value: Option<Value>,
}

/// The payload shape of a variant value.
enum VariantPayload {
    Unit,
    Tuple(Vec<Value>),
    Struct(Vec<VariantField>),
}

/// Extract `(variant_name, payload)` from an enum value's surface form.
fn enum_variant_name_and_payload(value: &Value) -> Option<(String, VariantPayload)> {
    match value {
        // Bare ident `Active` / brace `V { .. }`.
        Value::EnumVariant(ev) => {
            let name = ev.name_text()?;
            let entries: Vec<_> = ev.entries().collect();
            if entries.is_empty() {
                Some((name, VariantPayload::Unit))
            } else {
                let fields = entries
                    .iter()
                    .filter_map(|e| {
                        let (n, span) = entry_key_name_and_span(e)?;
                        Some(VariantField {
                            name: n,
                            key_span: span,
                            value: e.value(),
                        })
                    })
                    .collect();
                Some((name, VariantPayload::Struct(fields)))
            }
        }
        // Named tuple `V(a, b, ..)`.
        Value::Tuple(t) => {
            let name = tuple_name(t)?;
            let items: Vec<Value> = t.items().collect();
            if items.is_empty() {
                Some((name, VariantPayload::Unit))
            } else {
                Some((name, VariantPayload::Tuple(items)))
            }
        }
        // Named struct `V(field: v)`.
        Value::Struct(s) => {
            let name = s.name_text()?;
            let fields: Vec<VariantField> = s
                .fields()
                .filter_map(|f| {
                    let tok = f.name()?;
                    Some(VariantField {
                        name: tok.text().to_string(),
                        key_span: tok.text_range(),
                        value: f.value(),
                    })
                })
                .collect();
            if fields.is_empty() {
                Some((name, VariantPayload::Unit))
            } else {
                Some((name, VariantPayload::Struct(fields)))
            }
        }
        _ => None,
    }
}

/// Project a tuple-variant payload guided by the branch's `prefixItems`
/// (newtype = single inner; tuple = array).
fn project_variant_tuple_payload(
    items: &[Value],
    branch: Option<&serde_json::Value>,
    g: &Guide<'_>,
    builder: &mut PointerBuilder,
    index: &mut PointerRangeIndex,
) -> serde_json::Value {
    let prefix = branch
        .and_then(|b| b.get("prefixItems"))
        .and_then(serde_json::Value::as_array);
    let newtype_inner = branch.and_then(|b| b.get("x-ron-payload"));

    if items.len() == 1 && prefix.is_none() {
        // Newtype payload -> the single inner value.
        let inner_schema = newtype_inner.unwrap_or(&serde_json::Value::Null);
        return project_value_guided(&items[0], inner_schema, g, builder, index);
    }
    let mut arr = Vec::new();
    for (i, item) in items.iter().enumerate() {
        builder.push_index(i);
        let elem_schema = prefix
            .and_then(|p| p.get(i))
            .unwrap_or(&serde_json::Value::Null);
        arr.push(project_value_guided(item, elem_schema, g, builder, index));
        builder.pop();
    }
    serde_json::Value::Array(arr)
}

/// Project a struct-variant payload guided by the branch's `properties`.
fn project_variant_struct_payload(
    fields: &[VariantField],
    branch: Option<&serde_json::Value>,
    g: &Guide<'_>,
    builder: &mut PointerBuilder,
    index: &mut PointerRangeIndex,
) -> serde_json::Value {
    let props = branch.and_then(|b| b.get("properties"));
    let mut map = serde_json::Map::new();
    for field in fields {
        builder.push_key(&field.name);
        index.set_key(builder.as_pointer(), field.key_span);
        let field_schema = props
            .and_then(|p| p.get(&field.name))
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let json = match &field.value {
            Some(val) => project_value_guided(val, &field_schema, g, builder, index),
            None => serde_json::Value::Null,
        };
        builder.pop();
        map.insert(field.name.clone(), json);
    }
    serde_json::Value::Object(map)
}

/// Project a struct guided by `schema.properties`.
fn project_struct_guided(
    s: &ronin_core::syntax::ast::Struct,
    schema: &serde_json::Value,
    g: &Guide<'_>,
    builder: &mut PointerBuilder,
    index: &mut PointerRangeIndex,
) -> serde_json::Value {
    let props = schema.get("properties");
    let mut map = serde_json::Map::new();
    for field in s.fields() {
        let Some(name_tok) = field.name() else {
            continue;
        };
        let key = name_tok.text().to_string();
        builder.push_key(&key);
        index.set_key(builder.as_pointer(), name_tok.text_range());
        let field_schema = props
            .and_then(|p| p.get(&key))
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let json = match field.value() {
            Some(v) => project_value_guided(&v, &field_schema, g, builder, index),
            None => serde_json::Value::Null,
        };
        builder.pop();
        map.insert(key, json);
    }
    serde_json::Value::Object(map)
}

/// Project a tuple guided by `schema`. A named tuple validated against an array
/// schema (tuple-struct) drops its name → array; otherwise it falls back to the
/// schema-agnostic encoding (which external-tags named tuples).
fn project_tuple_guided(
    t: &ronin_core::syntax::ast::Tuple,
    schema: &serde_json::Value,
    g: &Guide<'_>,
    builder: &mut PointerBuilder,
    index: &mut PointerRangeIndex,
) -> serde_json::Value {
    let prefix = schema
        .get("prefixItems")
        .and_then(serde_json::Value::as_array);
    let items_schema = schema.get("items");
    // Always project a (named or anonymous) tuple to an array when the schema
    // expects one — the tuple-struct name is not part of serde's JSON.
    if schema_expects_array(schema) || schema_ron_kind(schema) == Some("tuple") {
        let mut arr = Vec::new();
        for (i, item) in t.items().enumerate() {
            builder.push_index(i);
            let elem_schema = prefix
                .and_then(|p| p.get(i))
                .or_else(|| items_schema.filter(|s| s.is_object()))
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            arr.push(project_value_guided(&item, &elem_schema, g, builder, index));
            builder.pop();
        }
        return serde_json::Value::Array(arr);
    }
    // Schema does not expect an array -> use the schema-agnostic tuple encoding
    // (handles `Some`/named-tuple-variant), then validation proceeds.
    project_tuple(t, builder, index)
}

/// Project a list guided by `schema.items`.
fn project_list_guided(
    l: &ronin_core::syntax::ast::List,
    schema: &serde_json::Value,
    g: &Guide<'_>,
    builder: &mut PointerBuilder,
    index: &mut PointerRangeIndex,
) -> serde_json::Value {
    let items_schema = schema.get("items");
    let mut arr = Vec::new();
    for (i, item) in l.items().enumerate() {
        builder.push_index(i);
        let elem_schema = items_schema
            .filter(|s| s.is_object())
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        arr.push(project_value_guided(&item, &elem_schema, g, builder, index));
        builder.pop();
    }
    serde_json::Value::Array(arr)
}

/// Project a map guided by `schema.additionalProperties`.
fn project_map_guided(
    m: &ronin_core::syntax::ast::Map,
    schema: &serde_json::Value,
    g: &Guide<'_>,
    builder: &mut PointerBuilder,
    index: &mut PointerRangeIndex,
) -> serde_json::Value {
    let value_schema = schema.get("additionalProperties");
    let mut map = serde_json::Map::new();
    for entry in m.entries() {
        let Some(key_value) = entry.key() else {
            continue;
        };
        let key = map_key_string(&key_value);
        builder.push_key(&key);
        index.set_key(builder.as_pointer(), key_value.syntax().text_range());
        let vs = value_schema
            .filter(|s| s.is_object())
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let json = match entry.value() {
            Some(v) => project_value_guided(&v, &vs, g, builder, index),
            None => serde_json::Value::Null,
        };
        builder.pop();
        map.insert(key, json);
    }
    serde_json::Value::Object(map)
}

/// Schema-agnostic projection used as a guided-path fallback (records spans).
fn project_structural(
    value: &Value,
    builder: &mut PointerBuilder,
    index: &mut PointerRangeIndex,
) -> serde_json::Value {
    project_value(value, builder, index)
}

/// Maps a JSON Pointer (a `jsonschema` `instance_path`, rendered as a string) to
/// the precise CST byte spans it refers to (FR-003).
///
/// Every mapped pointer carries at least a value span (the projected value's
/// node range); object-property pointers additionally carry the key span (the
/// field-name token range). No pointer is ever mapped to a fabricated or empty
/// range — only real CST node/token spans are recorded.
#[derive(Debug, Clone, Default)]
pub struct PointerRangeIndex {
    pointer_to_spans: BTreeMap<String, PointerSpans>,
}

impl PointerRangeIndex {
    /// Create an empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the value span for a pointer (merging with any existing key span).
    fn set_value(&mut self, pointer: String, range: TextRange) {
        self.pointer_to_spans.entry(pointer).or_default().value = Some(range);
    }

    /// Record the key span for a pointer (merging with any existing value span).
    fn set_key(&mut self, pointer: &str, range: TextRange) {
        self.pointer_to_spans
            .entry(pointer.to_owned())
            .or_default()
            .key = Some(range);
    }

    /// Both spans recorded for a pointer, if any.
    #[must_use]
    pub fn spans_for(&self, pointer: &str) -> Option<PointerSpans> {
        self.pointer_to_spans.get(pointer).copied()
    }

    /// The value span for a pointer (type/range/arity findings), if mapped.
    #[must_use]
    pub fn value_range(&self, pointer: &str) -> Option<TextRange> {
        self.pointer_to_spans.get(pointer).and_then(|s| s.value)
    }

    /// The key span for a pointer (missing/unknown-field findings), if mapped.
    #[must_use]
    pub fn key_range(&self, pointer: &str) -> Option<TextRange> {
        self.pointer_to_spans.get(pointer).and_then(|s| s.key)
    }

    /// The value span for a pointer, with the key span as a fallback. Used by the
    /// validator when the preferred span for a kind is absent (defensive — never
    /// fabricates a range).
    #[must_use]
    pub fn range_for(&self, pointer: &str) -> Option<TextRange> {
        self.pointer_to_spans
            .get(pointer)
            .and_then(|s| s.value.or(s.key))
    }

    /// Whether the index holds no pointer→span entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pointer_to_spans.is_empty()
    }

    /// Number of mapped pointers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pointer_to_spans.len()
    }
}

/// An incrementally-built JSON Pointer (RFC 6901). Segments are pushed/popped as
/// the CST walk descends/ascends; [`PointerBuilder::as_pointer`] renders the
/// current location.
struct PointerBuilder {
    /// The current pointer string, kept escaped and ready to read.
    buf: String,
    /// Byte offsets into `buf` marking where each pushed segment started (so a
    /// pop truncates back exactly).
    segment_starts: Vec<usize>,
}

impl PointerBuilder {
    fn new() -> Self {
        Self {
            buf: String::new(),
            segment_starts: Vec::new(),
        }
    }

    /// Push an object-property segment (escaped per RFC 6901).
    fn push_key(&mut self, key: &str) {
        self.segment_starts.push(self.buf.len());
        self.buf.push('/');
        for ch in key.chars() {
            match ch {
                '~' => self.buf.push_str("~0"),
                '/' => self.buf.push_str("~1"),
                other => self.buf.push(other),
            }
        }
    }

    /// Push an array-index segment.
    fn push_index(&mut self, index: usize) {
        self.segment_starts.push(self.buf.len());
        self.buf.push('/');
        // `usize` decimal is already pointer-safe (no `~`/`/`).
        self.buf.push_str(itoa_usize(index).as_str());
    }

    /// Pop the most recently pushed segment.
    fn pop(&mut self) {
        if let Some(start) = self.segment_starts.pop() {
            self.buf.truncate(start);
        }
    }

    /// The current pointer string (`""` at the root).
    fn as_pointer(&self) -> &str {
        &self.buf
    }
}

/// Minimal allocation-light usize→string (avoids a dep on `itoa`).
fn itoa_usize(n: usize) -> String {
    n.to_string()
}

/// Project a typed [`Value`], recording its value span at the current pointer and
/// recursing into children. Returns the projected `serde_json` value.
fn project_value(
    value: &Value,
    builder: &mut PointerBuilder,
    index: &mut PointerRangeIndex,
) -> serde_json::Value {
    // Record the value span for this location (the innermost value node's range —
    // a nested value overwrites a coarser ancestor only at its own deeper
    // pointer, never the ancestor's, because pointers differ).
    let span = value.syntax().text_range();
    index.set_value(builder.as_pointer().to_owned(), span);

    match value {
        Value::Literal(lit) => project_literal(lit),
        Value::Unit(_) => serde_json::Value::Null,
        Value::Struct(s) => project_struct(s, builder, index),
        Value::Tuple(t) => project_tuple(t, builder, index),
        Value::List(l) => project_list(l, builder, index),
        Value::Map(m) => project_map(m, builder, index),
        Value::EnumVariant(v) => project_enum_variant(v, builder, index),
        // An unparseable/recovered node has no meaningful JSON value; project it
        // as `null` so siblings still validate and no false positive is forced.
        // The validator skips spans that overlap structural errors (FR-019).
        Value::Error(_) => serde_json::Value::Null,
    }
}

/// Project a scalar literal into its `serde_json` value.
fn project_literal(lit: &ronin_core::syntax::ast::Literal) -> serde_json::Value {
    let Some(kind) = lit.token_kind() else {
        return serde_json::Value::Null;
    };
    let text = lit.text().unwrap_or_default();
    match kind {
        SyntaxKind::Integer => project_integer(&text),
        SyntaxKind::Float => project_float(&text),
        SyntaxKind::TrueKw => serde_json::Value::Bool(true),
        SyntaxKind::FalseKw => serde_json::Value::Bool(false),
        SyntaxKind::String => serde_json::Value::String(decode_string(&text)),
        SyntaxKind::RawString => serde_json::Value::String(decode_raw_string(&text)),
        SyntaxKind::Char => serde_json::Value::String(decode_char(&text)),
        // Any other token wrapped in a Literal node — treat as its raw text.
        _ => serde_json::Value::String(text),
    }
}

/// Parse a RON integer literal into a JSON integer (best-effort; falls back to a
/// JSON number/string so projection never panics and never drops the value).
fn project_integer(text: &str) -> serde_json::Value {
    let cleaned = strip_int_suffix_and_separators(text);
    let (radix, digits, neg) = split_radix(&cleaned);
    if let Ok(v) = i64::from_str_radix(digits, radix) {
        let v = if neg { v.checked_neg().unwrap_or(v) } else { v };
        return serde_json::Value::from(v);
    }
    if !neg {
        if let Ok(v) = u64::from_str_radix(digits, radix) {
            return serde_json::Value::from(v);
        }
    }
    // Unrepresentable as i64/u64 — keep it as a number string so a numeric schema
    // still sees "a number-ish thing"; jsonschema treats a string as not-integer,
    // which is acceptable (we never fabricate a passing value).
    serde_json::Value::String(text.to_owned())
}

/// Parse a RON float literal into a JSON number.
fn project_float(text: &str) -> serde_json::Value {
    let cleaned = strip_float_suffix_and_separators(text);
    cleaned
        .parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map_or_else(
            || serde_json::Value::String(text.to_owned()),
            serde_json::Value::Number,
        )
}

/// Remove a trailing integer type suffix (`i32`, `u64`, …) and `_` separators.
fn strip_int_suffix_and_separators(text: &str) -> String {
    let no_sep: String = text.chars().filter(|&c| c != '_').collect();
    // Strip a Rust-style integer suffix after the digits.
    for suffix in [
        "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize",
    ] {
        if let Some(stripped) = no_sep.strip_suffix(suffix) {
            return stripped.to_owned();
        }
    }
    no_sep
}

/// Remove a trailing float type suffix (`f32`/`f64`) and `_` separators.
fn strip_float_suffix_and_separators(text: &str) -> String {
    let no_sep: String = text.chars().filter(|&c| c != '_').collect();
    for suffix in ["f32", "f64"] {
        if let Some(stripped) = no_sep.strip_suffix(suffix) {
            return stripped.to_owned();
        }
    }
    no_sep
}

/// Split an integer literal into `(radix, digits, negative)`.
fn split_radix(text: &str) -> (u32, &str, bool) {
    let (neg, rest) = match text.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    if let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        (16, hex, neg)
    } else if let Some(oct) = rest.strip_prefix("0o").or_else(|| rest.strip_prefix("0O")) {
        (8, oct, neg)
    } else if let Some(bin) = rest.strip_prefix("0b").or_else(|| rest.strip_prefix("0B")) {
        (2, bin, neg)
    } else {
        (10, rest, neg)
    }
}

/// Decode a `"..."` string literal's contents (handles common escapes;
/// best-effort — unknown escapes are kept literally rather than dropping bytes).
fn decode_string(text: &str) -> String {
    let inner = text
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(text);
    unescape(inner)
}

/// Decode a raw string literal `r#"..."#` — its contents are verbatim (no
/// escapes).
fn decode_raw_string(text: &str) -> String {
    // r, then zero or more '#', then '"' ... '"', then matching '#'s.
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'r') {
        return text.to_owned();
    }
    let mut i = 1;
    let mut hashes = 0usize;
    while bytes.get(i) == Some(&b'#') {
        hashes += 1;
        i += 1;
    }
    if bytes.get(i) != Some(&b'"') {
        return text.to_owned();
    }
    let content_start = i + 1;
    // closing is '"' followed by `hashes` '#'.
    let closing_len = 1 + hashes;
    if text.len() < content_start + closing_len {
        return text.to_owned();
    }
    let content_end = text.len() - closing_len;
    text.get(content_start..content_end)
        .unwrap_or("")
        .to_owned()
}

/// Decode a char literal `'c'` into its single-character string.
fn decode_char(text: &str) -> String {
    let inner = text
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .unwrap_or(text);
    unescape(inner)
}

/// Minimal escape decoder for string/char contents. Unknown escapes are kept as
/// the literal backslash + char so no bytes are silently dropped.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some('0') => out.push('\0'),
            Some('u') => {
                // \u{XXXX}
                if chars.peek() == Some(&'{') {
                    chars.next();
                    let mut hex = String::new();
                    for h in chars.by_ref() {
                        if h == '}' {
                            break;
                        }
                        hex.push(h);
                    }
                    if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        out.push(ch);
                    }
                } else {
                    out.push('\\');
                    out.push('u');
                }
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Project a struct (named or anonymous) into a JSON object.
fn project_struct(
    s: &ronin_core::syntax::ast::Struct,
    builder: &mut PointerBuilder,
    index: &mut PointerRangeIndex,
) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for field in s.fields() {
        let Some(name_tok) = field.name() else {
            continue;
        };
        let key = name_tok.text().to_string();
        builder.push_key(&key);
        // Record the key span at this property pointer (used for unknown-field
        // findings; missing-field findings are attached to the parent object).
        index.set_key(builder.as_pointer(), name_tok.text_range());
        let json = match field.value() {
            Some(v) => project_value(&v, builder, index),
            None => serde_json::Value::Null,
        };
        builder.pop();
        map.insert(key, json);
    }
    serde_json::Value::Object(map)
}

/// Project a tuple into a JSON array.
///
/// A **named** tuple `Name(a, b, ..)` is ambiguous in RON between a tuple-struct
/// (serde JSON = a bare array, name dropped) and a tuple/newtype enum variant
/// (serde JSON = externally-tagged `{ "Name": payload }`). The schema-agnostic
/// projection cannot resolve this without type info, so it uses the conventional
/// variant encoding for named tuples (external tag), special-casing `Some` as the
/// Option `Some(x)` unwrap. The schema-guided path in [`crate::validate`] resolves
/// the ambiguity precisely against the bound type and never relies on this
/// heuristic for a finding (no false positives, FR-016). An **anonymous** tuple
/// `(a, b, ..)` always projects to a bare array.
fn project_tuple(
    t: &ronin_core::syntax::ast::Tuple,
    builder: &mut PointerBuilder,
    index: &mut PointerRangeIndex,
) -> serde_json::Value {
    let name = tuple_name(t);

    // Option `Some(x)` unwrap (implicit_some-friendly): project the single inner
    // value at the same pointer (its value span overwrites, which is the tighter
    // span — the "value" the diagnostic should point at).
    if name.as_deref() == Some("Some") {
        if let Some(inner) = t.items().next() {
            return project_value(&inner, builder, index);
        }
    }

    let items: Vec<Value> = t.items().collect();

    if let Some(variant) = name {
        // Externally-tagged named tuple/newtype variant.
        builder.push_key(&variant);
        index.set_value(builder.as_pointer().to_owned(), t.syntax().text_range());
        let payload = match items.len() {
            0 => serde_json::Value::Null,
            1 => project_value(&items[0], builder, index),
            _ => {
                let mut arr = Vec::new();
                for (i, item) in items.iter().enumerate() {
                    builder.push_index(i);
                    arr.push(project_value(item, builder, index));
                    builder.pop();
                }
                serde_json::Value::Array(arr)
            }
        };
        builder.pop();
        let mut map = serde_json::Map::new();
        map.insert(variant, payload);
        return serde_json::Value::Object(map);
    }

    // Anonymous tuple -> bare array.
    let mut arr = Vec::new();
    for (i, item) in items.iter().enumerate() {
        builder.push_index(i);
        arr.push(project_value(item, builder, index));
        builder.pop();
    }
    serde_json::Value::Array(arr)
}

/// The leading `Ident` name of a named tuple (`Name(..)`), or `None` for an
/// anonymous tuple `(..)`.
fn tuple_name(t: &ronin_core::syntax::ast::Tuple) -> Option<String> {
    t.syntax()
        .first_token_of(SyntaxKind::Ident)
        .map(|tok| tok.text().to_string())
}

/// Project a list into a JSON array.
fn project_list(
    l: &ronin_core::syntax::ast::List,
    builder: &mut PointerBuilder,
    index: &mut PointerRangeIndex,
) -> serde_json::Value {
    let mut arr = Vec::new();
    for (i, item) in l.items().enumerate() {
        builder.push_index(i);
        arr.push(project_value(&item, builder, index));
        builder.pop();
    }
    serde_json::Value::Array(arr)
}

/// Project a map into a JSON object. String keys are kept verbatim; non-string
/// keys are stringified from their source text (value validated via
/// `additionalProperties`).
fn project_map(
    m: &ronin_core::syntax::ast::Map,
    builder: &mut PointerBuilder,
    index: &mut PointerRangeIndex,
) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for entry in m.entries() {
        let Some(key_value) = entry.key() else {
            continue;
        };
        let key = map_key_string(&key_value);
        builder.push_key(&key);
        // Record the key span (the key value node's range) for this property.
        index.set_key(builder.as_pointer(), key_value.syntax().text_range());
        let json = match entry.value() {
            Some(v) => project_value(&v, builder, index),
            None => serde_json::Value::Null,
        };
        builder.pop();
        map.insert(key, json);
    }
    serde_json::Value::Object(map)
}

/// The object-key string for a map key value. A string/char key uses its decoded
/// contents; any other key uses its verbatim source text.
fn map_key_string(key: &Value) -> String {
    if let Value::Literal(lit) = key {
        match lit.token_kind() {
            Some(SyntaxKind::String) => return decode_string(&lit.text().unwrap_or_default()),
            Some(SyntaxKind::RawString) => {
                return decode_raw_string(&lit.text().unwrap_or_default())
            }
            Some(SyntaxKind::Char) => return decode_char(&lit.text().unwrap_or_default()),
            _ => {}
        }
    }
    // Non-string key: stringify the verbatim source text.
    key.syntax().text()
}

/// Project an enum variant. Option's `None`/`Some(x)` are special-cased; every
/// other variant uses serde external tagging `{ "<Variant>": <payload> }`.
fn project_enum_variant(
    v: &ronin_core::syntax::ast::EnumVariant,
    builder: &mut PointerBuilder,
    index: &mut PointerRangeIndex,
) -> serde_json::Value {
    let name = v.name_text().unwrap_or_default();
    let node = v.syntax();

    // Option unwrap: `None` -> null, `Some(x)` -> project x at the SAME pointer
    // (the value span stays the variant's span, recorded by the caller).
    if name == "None" && payload_values(node).next().is_none() && v.entries().next().is_none() {
        return serde_json::Value::Null;
    }
    if name == "Some" {
        // `Some(x)` has exactly one positional payload value.
        if let Some(inner) = payload_values(node).next() {
            // Re-record this pointer's value span as the inner value? No — keep
            // the variant span for the Option location (the diagnostic should
            // point at the whole `Some(..)`); project the inner without pushing a
            // segment so its pointer equals this one. The inner's own value span
            // overwrites at the same pointer, which is the tighter span — but for
            // Option we prefer the inner value span (matches "the value").
            return project_value(&inner, builder, index);
        }
    }

    // External tagging: { "<Variant>": <payload> }.
    let payload = project_variant_payload(v, builder, index);
    let mut map = serde_json::Map::new();
    map.insert(name, payload);
    serde_json::Value::Object(map)
}

/// Project an enum variant's payload (under the external tag) by shape.
///
/// The payload is recorded under the pointer `<current>/<Variant>` so the
/// validator's custom enum dispatch can find spans for payload sub-findings.
fn project_variant_payload(
    v: &ronin_core::syntax::ast::EnumVariant,
    builder: &mut PointerBuilder,
    index: &mut PointerRangeIndex,
) -> serde_json::Value {
    let name = v.name_text().unwrap_or_default();
    let node = v.syntax();

    // Struct-like variant: `Variant { field: v, .. }`.
    let struct_entries: Vec<_> = v.entries().collect();
    if !struct_entries.is_empty() || has_brace(node) {
        builder.push_key(&name);
        // Record the variant payload's value span (the whole variant node).
        index.set_value(builder.as_pointer().to_owned(), node.text_range());
        let mut map = serde_json::Map::new();
        for entry in &struct_entries {
            let Some((key, key_span)) = entry_key_name_and_span(entry) else {
                continue;
            };
            builder.push_key(&key);
            index.set_key(builder.as_pointer(), key_span);
            let json = match entry.value() {
                Some(val) => project_value(&val, builder, index),
                None => serde_json::Value::Null,
            };
            builder.pop();
            map.insert(key, json);
        }
        builder.pop();
        return serde_json::Value::Object(map);
    }

    // Positional payload: `Variant(a, b, ..)`.
    let payload: Vec<_> = payload_values(node).collect();
    builder.push_key(&name);
    index.set_value(builder.as_pointer().to_owned(), node.text_range());
    let result = match payload.len() {
        // Unit variant `Variant` -> null payload.
        0 => serde_json::Value::Null,
        // Newtype variant `Variant(x)` -> the inner value directly.
        1 => project_value(&payload[0], builder, index),
        // Tuple variant `Variant(a, b, ..)` -> array.
        _ => {
            let mut arr = Vec::new();
            for (i, item) in payload.iter().enumerate() {
                builder.push_index(i);
                arr.push(project_value(item, builder, index));
                builder.pop();
            }
            serde_json::Value::Array(arr)
        }
    };
    builder.pop();
    result
}

/// The positional payload values inside an enum variant `Variant(a, b, ..)`.
fn payload_values(node: &SyntaxNode) -> impl Iterator<Item = Value> {
    node.children().filter_map(Value::cast)
}

/// Whether an enum-variant node uses brace-style payload `{ .. }` (struct-like),
/// even with zero fields.
fn has_brace(node: &SyntaxNode) -> bool {
    node.children_with_tokens()
        .any(|el| el.kind() == SyntaxKind::LBrace)
}

/// The field-name string and span of a struct-like variant entry. In a brace
/// variant `V { field: v }` the key `field` is parsed as a bare-ident value node
/// (an `EnumVariant`), so we read it as a [`Value`] and take its name/span rather
/// than a direct token.
fn entry_key_name_and_span(entry: &ronin_core::syntax::ast::MapEntry) -> Option<(String, TextRange)> {
    let key = entry.key()?;
    // The key text is the bare identifier; its span is the key node's range.
    let name = match &key {
        Value::EnumVariant(ev) => ev.name_text()?,
        Value::Literal(lit) => lit.text()?,
        other => other.syntax().text(),
    };
    Some((name, key.syntax().text_range()))
}
