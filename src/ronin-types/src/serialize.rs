//! JSON-Schema-2020-12-shaped interchange for [`TypeModel`] {TR-001, TR-012}.
//!
//! This module is the boundary between the ergonomic in-memory model
//! ([`crate::model`]) and the stable, validator-consumable wire form. The
//! serialized document is **valid JSON Schema 2020-12 shaped JSON** so a future
//! validator (E006) can hand it to the `jsonschema` crate, and it deserializes
//! back into an equivalent [`TypeModel`]. It uses only plain-serde / JSON types
//! so a WASM-clean consumer can read it (TR-012).
//!
//! # Interchange shape
//!
//! The root is a JSON Schema object:
//!
//! ```jsonc
//! {
//!   "$schema": "https://json-schema.org/draft/2020-12/schema",
//!   "$defs": {                       // named types, in definitions_order
//!     "Name": { /* node schema */ }
//!   },
//!   "x-ron-extensions-active": ["implicit_some"],   // omitted if empty
//!   "x-ron-diagnostics": [ /* AcquisitionDiagnostic[] */ ]  // omitted if empty
//! }
//! ```
//!
//! Each node maps to a 2020-12 construct, annotated with `x-ron-*` keywords when
//! the node carries an [`RonTypeExtension`]:
//!
//! | node | base 2020-12 | `x-ron-*` |
//! |------|--------------|-----------|
//! | object | `type:object` + `properties` + `required` (+ `additionalProperties:false`) | — |
//! | enum | `oneOf` (per-variant) | — |
//! | sequence | `type:array` + `items` | — |
//! | tuple | `type:array` + `prefixItems` + `items:false` | `x-ron-kind:"tuple"`, `x-ron-tuple-arity` |
//! | map | `type:object` + `additionalProperties` (+ `x-ron-key`) | `x-ron-kind:"non-string-key-map"` |
//! | primitive | `type:<scalar>` | (`x-ron-kind` for char/unit/bytes) |
//! | option | `oneOf:[inner, {type:null}]` | `x-ron-kind:"option"` |
//! | unknown | `{}` (true schema) | `x-ron-kind:"unknown"` |
//!
//! Named references serialize to `{"$ref": "#/$defs/<name>"}`. Key ordering is
//! deterministic (a fixed key sequence per node; `$defs` follow
//! [`TypeModel::definitions_order`]) so the output byte-stabilizes for snapshots.
//!
//! # Frozen interchange contract (TR-012)
//!
//! This is the **stable, frozen wire form** of the RONin type model — the only
//! artifact that crosses the native/`wasm32` boundary to a WASM-clean consumer
//! (`ronin-core`, E006). The contract below is fixed; changes to it are
//! breaking and require a versioned migration.
//!
//! 1. **Dialect tag.** The root MUST carry `"$schema"` set to the JSON Schema
//!    2020-12 meta-schema URI ([`crate::model::SCHEMA_DIALECT_2020_12`] =
//!    `"https://json-schema.org/draft/2020-12/schema"`). This pins the validator
//!    dialect E006 hands to `jsonschema`.
//! 2. **Named types live under `"$defs"`**, keyed by type name, in
//!    [`TypeModel::definitions_order`]. Every named type reachable by `$ref`
//!    appears here.
//! 3. **References** use the JSON Schema form `{"$ref": "#/$defs/<name>"}` and
//!    nothing else. Recursive types are expressed only through these refs (never
//!    inline-expanded), so the document is always finite.
//! 4. **Field/key ordering is fixed and deterministic** — each node emits its
//!    keys in the order shown in the table above, `$defs` follow
//!    `definitions_order`, and `properties` follow field declaration order. The
//!    output therefore byte-stabilizes for snapshots and is reproducible.
//! 5. **`x-ron-*` is the closed RON extension vocabulary** layered on top of the
//!    base 2020-12 keywords; the model-level annotations `x-ron-extensions-active`
//!    and `x-ron-diagnostics` are omitted when empty.
//! 6. **Pure-JSON encoding.** The serialized document is a plain
//!    [`serde_json::Value`] tree (object/array/string/number/bool/null only) —
//!    no native-only Rust constructs leak into the wire form, so a WASM-clean
//!    consumer can deserialize it with `serde_json` alone (verified by
//!    `tests/wasm32_deser.rs`).
//!
//! # Finalized entry points
//!
//! [`to_json`] / [`from_json`] are the **finalized, frozen** entry points for the
//! `serde_json::Value` interchange. [`to_json_string`] / [`from_json_str`] are
//! the convenience text wrappers over them. All four are stable; the shape they
//! produce/consume is the frozen contract above.

use serde_json::{json, Map, Value};

use crate::diagnostics::AcquisitionDiagnostic;
use crate::extension::{RonKind, RonTypeExtension};
use crate::model::{
    Discriminator, Field, NodeKind, Primitive, TypeModel, TypeNode, TypeRef, Variant, VariantShape,
};

/// `$ref` prefix for named types in the `$defs` registry.
const DEFS_REF_PREFIX: &str = "#/$defs/";

/// An error from deserializing the JSON interchange back into a [`TypeModel`].
///
/// Distinct from acquisition diagnostics: this signals a *malformed interchange
/// document* (a programming/transport error), not an unresolved user type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterchangeError {
    /// Human-readable description of what was malformed.
    pub message: String,
}

impl InterchangeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for InterchangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "malformed type-model interchange: {}", self.message)
    }
}

impl std::error::Error for InterchangeError {}

// ---------------------------------------------------------------------------
// Serialize: TypeModel -> JSON-Schema-2020-12-shaped serde_json::Value
// ---------------------------------------------------------------------------

/// Serialize a [`TypeModel`] to its JSON-Schema-2020-12-shaped
/// [`serde_json::Value`] interchange (TR-001).
///
/// Deterministic: `$defs` follow [`TypeModel::definitions_order`] and every node
/// emits its keys in a fixed sequence, so the output is byte-stable for
/// snapshots and reproducible across runs.
#[must_use]
pub fn to_json(model: &TypeModel) -> Value {
    let mut root = Map::new();
    root.insert("$schema".into(), json!(model.schema_dialect));

    let mut defs = Map::new();
    for name in &model.definitions_order {
        if let Some(node) = model.named_types.get(name) {
            defs.insert(name.clone(), node_to_json(node));
        }
    }
    // Include any named type not captured by definitions_order (defensive:
    // keeps the interchange complete even if order drifts).
    for (name, node) in &model.named_types {
        defs.entry(name.clone())
            .or_insert_with(|| node_to_json(node));
    }
    root.insert("$defs".into(), Value::Object(defs));

    if !model.ron_extensions_active.is_empty() {
        root.insert(
            "x-ron-extensions-active".into(),
            json!(model.ron_extensions_active),
        );
    }
    if !model.diagnostics.is_empty() {
        let diags: Vec<Value> = model
            .diagnostics
            .iter()
            .map(|d| serde_json::to_value(d).expect("AcquisitionDiagnostic is serializable"))
            .collect();
        root.insert("x-ron-diagnostics".into(), Value::Array(diags));
    }

    Value::Object(root)
}

/// Serialize a [`TypeModel`] to a pretty JSON interchange string (TR-001).
#[must_use]
pub fn to_json_string(model: &TypeModel) -> String {
    serde_json::to_string_pretty(&to_json(model)).expect("interchange Value is always serializable")
}

fn ref_to_json(type_ref: &TypeRef) -> Value {
    match type_ref {
        TypeRef::Named(name) => json!({ "$ref": format!("{DEFS_REF_PREFIX}{name}") }),
        TypeRef::Inline(node) => node_to_json(node),
    }
}

fn node_to_json(node: &TypeNode) -> Value {
    let mut obj = Map::new();
    match &node.kind {
        NodeKind::Object {
            fields,
            deny_unknown_fields,
        } => {
            obj.insert("type".into(), json!("object"));
            let mut properties = Map::new();
            let mut required = Vec::new();
            for field in fields {
                properties.insert(field.serialized_key.clone(), field_to_json(field));
                if !field.optional {
                    required.push(Value::String(field.serialized_key.clone()));
                }
            }
            obj.insert("properties".into(), Value::Object(properties));
            if !required.is_empty() {
                obj.insert("required".into(), Value::Array(required));
            }
            if *deny_unknown_fields {
                obj.insert("additionalProperties".into(), json!(false));
            }
        }
        NodeKind::Enum {
            variants,
            discriminator,
        } => {
            let one_of: Vec<Value> = variants.iter().map(variant_to_json).collect();
            obj.insert("oneOf".into(), Value::Array(one_of));
            insert_discriminator(&mut obj, discriminator);
        }
        NodeKind::Sequence { element } => {
            obj.insert("type".into(), json!("array"));
            obj.insert("items".into(), ref_to_json(element));
        }
        NodeKind::Tuple { elements } => {
            obj.insert("type".into(), json!("array"));
            let prefix: Vec<Value> = elements.iter().map(ref_to_json).collect();
            obj.insert("prefixItems".into(), Value::Array(prefix));
            // Fixed arity: no items beyond the prefix.
            obj.insert("items".into(), json!(false));
        }
        NodeKind::Map { key, value } => {
            obj.insert("type".into(), json!("object"));
            obj.insert("additionalProperties".into(), ref_to_json(value));
            // Record the key type for non-string-key maps (round-trips the K).
            obj.insert("x-ron-key".into(), ref_to_json(key));
        }
        NodeKind::Primitive { primitive } => {
            obj.insert("type".into(), json!(primitive.as_type_keyword()));
        }
        NodeKind::Option { inner } => {
            obj.insert(
                "oneOf".into(),
                json!([ref_to_json(inner), { "type": "null" }]),
            );
        }
        NodeKind::Unknown => {
            // The JSON Schema "true" schema (accepts anything): an empty object.
            // The x-ron-kind annotation below marks it as a deliberate unknown.
            obj.insert("x-ron-kind".into(), json!("unknown"));
        }
    }

    if let Some(ext) = &node.ron_extension {
        insert_ron_extension(&mut obj, ext);
    }

    Value::Object(obj)
}

fn field_to_json(field: &Field) -> Value {
    let mut v = ref_to_json(&field.value);
    if field.flatten {
        if let Value::Object(map) = &mut v {
            map.insert("x-ron-flatten".into(), json!(true));
        }
    }
    v
}

fn variant_to_json(variant: &Variant) -> Value {
    let mut obj = Map::new();
    obj.insert("x-ron-variant".into(), json!(variant.serialized_name));
    match &variant.shape {
        VariantShape::Unit => {
            obj.insert("x-ron-variant-shape".into(), json!("unit"));
        }
        VariantShape::Newtype(inner) => {
            obj.insert("x-ron-variant-shape".into(), json!("newtype"));
            obj.insert("x-ron-payload".into(), ref_to_json(inner));
        }
        VariantShape::Tuple(elements) => {
            obj.insert("x-ron-variant-shape".into(), json!("tuple"));
            let prefix: Vec<Value> = elements.iter().map(ref_to_json).collect();
            obj.insert("prefixItems".into(), Value::Array(prefix));
        }
        VariantShape::Struct(fields) => {
            obj.insert("x-ron-variant-shape".into(), json!("struct"));
            let mut properties = Map::new();
            let mut required = Vec::new();
            for field in fields {
                properties.insert(field.serialized_key.clone(), field_to_json(field));
                if !field.optional {
                    required.push(Value::String(field.serialized_key.clone()));
                }
            }
            obj.insert("type".into(), json!("object"));
            obj.insert("properties".into(), Value::Object(properties));
            if !required.is_empty() {
                obj.insert("required".into(), Value::Array(required));
            }
        }
    }
    Value::Object(obj)
}

fn insert_discriminator(obj: &mut Map<String, Value>, discriminator: &Discriminator) {
    match discriminator {
        Discriminator::External => {} // default; omit for compactness
        Discriminator::Internal { tag } => {
            obj.insert(
                "x-ron-discriminator".into(),
                json!({ "strategy": "internal", "tag": tag }),
            );
        }
        Discriminator::Adjacent { tag, content } => {
            obj.insert(
                "x-ron-discriminator".into(),
                json!({ "strategy": "adjacent", "tag": tag, "content": content }),
            );
        }
        Discriminator::Untagged => {
            obj.insert(
                "x-ron-discriminator".into(),
                json!({ "strategy": "untagged" }),
            );
        }
    }
}

fn insert_ron_extension(obj: &mut Map<String, Value>, ext: &RonTypeExtension) {
    if let Some(kind) = ext.ron_kind {
        // For Unknown nodes the kind is already written; otherwise set it now.
        obj.entry("x-ron-kind".to_string())
            .or_insert_with(|| json!(kind.as_keyword()));
    }
    if let Some(arity) = ext.tuple_arity {
        obj.insert("x-ron-tuple-arity".into(), json!(arity));
    }
    if ext.implicit_some {
        obj.insert("x-ron-implicit-some".into(), json!(true));
    }
    if ext.unwrap_newtypes {
        obj.insert("x-ron-unwrap-newtypes".into(), json!(true));
    }
    if ext.unwrap_variant_newtypes {
        obj.insert("x-ron-unwrap-variant-newtypes".into(), json!(true));
    }
}

// ---------------------------------------------------------------------------
// Deserialize: JSON interchange -> TypeModel
// ---------------------------------------------------------------------------

/// Deserialize a JSON-Schema-2020-12-shaped interchange [`serde_json::Value`]
/// back into a [`TypeModel`] (TR-012).
///
/// # Errors
///
/// Returns [`InterchangeError`] only when the document is structurally
/// malformed (not an object, `$defs` not an object, an unrecognized node shape).
/// Unresolved *user* types are valid `unknown` nodes, not errors.
pub fn from_json(value: &Value) -> Result<TypeModel, InterchangeError> {
    let root = value
        .as_object()
        .ok_or_else(|| InterchangeError::new("root must be a JSON object"))?;

    let mut model = TypeModel::new();
    if let Some(dialect) = root.get("$schema").and_then(Value::as_str) {
        model.schema_dialect = dialect.to_string();
    }

    if let Some(defs) = root.get("$defs") {
        let defs = defs
            .as_object()
            .ok_or_else(|| InterchangeError::new("$defs must be a JSON object"))?;
        // `definitions_order` must reflect the serialized `$defs` order. The
        // `serde_json` `preserve_order` feature is enabled (see Cargo.toml), so
        // this object iterates in its original serialized order; reconstruct
        // `definitions_order` by inserting in that iteration order — deterministic
        // and stable for round-trips.
        for (name, node_value) in defs {
            let node = json_to_node(node_value)?;
            model.insert_named(name.clone(), node);
        }
    }

    if let Some(active) = root.get("x-ron-extensions-active") {
        let arr = active
            .as_array()
            .ok_or_else(|| InterchangeError::new("x-ron-extensions-active must be an array"))?;
        for v in arr {
            let flag = v
                .as_str()
                .ok_or_else(|| InterchangeError::new("extension flag must be a string"))?;
            model.add_active_extension(flag.to_string());
        }
    }

    if let Some(diags) = root.get("x-ron-diagnostics") {
        let arr = diags
            .as_array()
            .ok_or_else(|| InterchangeError::new("x-ron-diagnostics must be an array"))?;
        for v in arr {
            let d: AcquisitionDiagnostic = serde_json::from_value(v.clone())
                .map_err(|e| InterchangeError::new(format!("malformed diagnostic: {e}")))?;
            model.diagnostics.push(d);
        }
    }

    Ok(model)
}

/// Deserialize a JSON interchange *string* into a [`TypeModel`] (TR-012).
///
/// # Errors
///
/// Returns [`InterchangeError`] if the string is not valid JSON or the document
/// is structurally malformed.
pub fn from_json_str(s: &str) -> Result<TypeModel, InterchangeError> {
    let value: Value =
        serde_json::from_str(s).map_err(|e| InterchangeError::new(format!("invalid JSON: {e}")))?;
    from_json(&value)
}

fn json_to_ref(value: &Value) -> Result<TypeRef, InterchangeError> {
    if let Some(obj) = value.as_object() {
        if let Some(reference) = obj.get("$ref").and_then(Value::as_str) {
            let name = reference.strip_prefix(DEFS_REF_PREFIX).ok_or_else(|| {
                InterchangeError::new(format!("unsupported $ref form: {reference:?}"))
            })?;
            return Ok(TypeRef::named(name.to_string()));
        }
    }
    Ok(TypeRef::inline(json_to_node(value)?))
}

fn json_to_node(value: &Value) -> Result<TypeNode, InterchangeError> {
    let obj = value
        .as_object()
        .ok_or_else(|| InterchangeError::new("a type node must be a JSON object"))?;

    let ron_extension = parse_ron_extension(obj);
    let kind = parse_node_kind(obj)?;

    Ok(TypeNode {
        kind,
        ron_extension,
    })
}

fn parse_node_kind(obj: &Map<String, Value>) -> Result<NodeKind, InterchangeError> {
    // Explicit unknown marker takes priority.
    if obj.get("x-ron-kind").and_then(Value::as_str) == Some("unknown") {
        return Ok(NodeKind::Unknown);
    }
    // Option: oneOf with a trailing null branch + x-ron-kind option.
    if obj.get("x-ron-kind").and_then(Value::as_str) == Some(RonKind::Option.as_keyword()) {
        let arms = obj
            .get("oneOf")
            .and_then(Value::as_array)
            .ok_or_else(|| InterchangeError::new("option node must have oneOf"))?;
        let inner = arms
            .first()
            .ok_or_else(|| InterchangeError::new("option oneOf must have an inner arm"))?;
        return Ok(NodeKind::Option {
            inner: json_to_ref(inner)?,
        });
    }
    // Enum: oneOf of variant schemas.
    if let Some(one_of) = obj.get("oneOf").and_then(Value::as_array) {
        let variants = one_of
            .iter()
            .map(json_to_variant)
            .collect::<Result<Vec<_>, _>>()?;
        let discriminator = parse_discriminator(obj)?;
        return Ok(NodeKind::Enum {
            variants,
            discriminator,
        });
    }

    match obj.get("type").and_then(Value::as_str) {
        Some("array") => {
            if let Some(prefix) = obj.get("prefixItems").and_then(Value::as_array) {
                let elements = prefix
                    .iter()
                    .map(json_to_ref)
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(NodeKind::Tuple { elements })
            } else {
                let items = obj
                    .get("items")
                    .ok_or_else(|| InterchangeError::new("array node must have items"))?;
                Ok(NodeKind::Sequence {
                    element: json_to_ref(items)?,
                })
            }
        }
        Some("object") => {
            // Map: additionalProperties is a schema (not false) + x-ron-key.
            if let Some(addl) = obj.get("additionalProperties") {
                if !matches!(addl, Value::Bool(_)) {
                    let key = obj
                        .get("x-ron-key")
                        .ok_or_else(|| InterchangeError::new("map node must record x-ron-key"))?;
                    return Ok(NodeKind::Map {
                        key: json_to_ref(key)?,
                        value: json_to_ref(addl)?,
                    });
                }
            }
            // Otherwise an object/struct with properties.
            let fields = parse_fields(obj)?;
            let deny_unknown_fields = obj.get("additionalProperties") == Some(&Value::Bool(false));
            Ok(NodeKind::Object {
                fields,
                deny_unknown_fields,
            })
        }
        Some(prim) => {
            let primitive = parse_primitive(prim)?;
            Ok(NodeKind::Primitive { primitive })
        }
        None => Err(InterchangeError::new(
            "node has no recognized kind (missing type/oneOf/x-ron-kind)",
        )),
    }
}

fn parse_primitive(prim: &str) -> Result<Primitive, InterchangeError> {
    match prim {
        "boolean" => Ok(Primitive::Boolean),
        "integer" => Ok(Primitive::Integer),
        "number" => Ok(Primitive::Number),
        "string" => Ok(Primitive::String),
        "null" => Ok(Primitive::Null),
        other => Err(InterchangeError::new(format!(
            "unsupported primitive type: {other:?}"
        ))),
    }
}

fn parse_fields(obj: &Map<String, Value>) -> Result<Vec<Field>, InterchangeError> {
    let mut fields = Vec::new();
    let required: std::collections::BTreeSet<&str> = obj
        .get("required")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    if let Some(props) = obj.get("properties").and_then(Value::as_object) {
        for (key, schema) in props {
            let flatten = schema
                .as_object()
                .and_then(|m| m.get("x-ron-flatten"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            fields.push(Field {
                serialized_key: key.clone(),
                value: json_to_ref(schema)?,
                optional: !required.contains(key.as_str()),
                flatten,
            });
        }
    }
    Ok(fields)
}

fn json_to_variant(value: &Value) -> Result<Variant, InterchangeError> {
    let obj = value
        .as_object()
        .ok_or_else(|| InterchangeError::new("a variant must be a JSON object"))?;
    let serialized_name = obj
        .get("x-ron-variant")
        .and_then(Value::as_str)
        .ok_or_else(|| InterchangeError::new("variant must have x-ron-variant name"))?
        .to_string();
    let shape = match obj.get("x-ron-variant-shape").and_then(Value::as_str) {
        Some("unit") => VariantShape::Unit,
        Some("newtype") => {
            let payload = obj
                .get("x-ron-payload")
                .ok_or_else(|| InterchangeError::new("newtype variant must have x-ron-payload"))?;
            VariantShape::Newtype(json_to_ref(payload)?)
        }
        Some("tuple") => {
            let prefix = obj
                .get("prefixItems")
                .and_then(Value::as_array)
                .ok_or_else(|| InterchangeError::new("tuple variant must have prefixItems"))?;
            VariantShape::Tuple(
                prefix
                    .iter()
                    .map(json_to_ref)
                    .collect::<Result<Vec<_>, _>>()?,
            )
        }
        Some("struct") => VariantShape::Struct(parse_fields(obj)?),
        other => {
            return Err(InterchangeError::new(format!(
                "unknown variant shape: {other:?}"
            )))
        }
    };
    Ok(Variant {
        serialized_name,
        shape,
    })
}

fn parse_discriminator(obj: &Map<String, Value>) -> Result<Discriminator, InterchangeError> {
    let Some(disc) = obj.get("x-ron-discriminator").and_then(Value::as_object) else {
        return Ok(Discriminator::External);
    };
    match disc.get("strategy").and_then(Value::as_str) {
        Some("internal") => {
            let tag = disc
                .get("tag")
                .and_then(Value::as_str)
                .ok_or_else(|| InterchangeError::new("internal discriminator needs tag"))?
                .to_string();
            Ok(Discriminator::Internal { tag })
        }
        Some("adjacent") => {
            let tag = disc
                .get("tag")
                .and_then(Value::as_str)
                .ok_or_else(|| InterchangeError::new("adjacent discriminator needs tag"))?
                .to_string();
            let content = disc
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| InterchangeError::new("adjacent discriminator needs content"))?
                .to_string();
            Ok(Discriminator::Adjacent { tag, content })
        }
        Some("untagged") => Ok(Discriminator::Untagged),
        other => Err(InterchangeError::new(format!(
            "unknown discriminator strategy: {other:?}"
        ))),
    }
}

fn parse_ron_extension(obj: &Map<String, Value>) -> Option<RonTypeExtension> {
    let ron_kind = obj
        .get("x-ron-kind")
        .and_then(Value::as_str)
        .and_then(parse_ron_kind);
    let tuple_arity = obj
        .get("x-ron-tuple-arity")
        .and_then(Value::as_u64)
        .map(|n| n as usize);
    let implicit_some = obj
        .get("x-ron-implicit-some")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let unwrap_newtypes = obj
        .get("x-ron-unwrap-newtypes")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let unwrap_variant_newtypes = obj
        .get("x-ron-unwrap-variant-newtypes")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let ext = RonTypeExtension {
        ron_kind,
        tuple_arity,
        implicit_some,
        unwrap_newtypes,
        unwrap_variant_newtypes,
    };
    if ext.is_empty() {
        None
    } else {
        Some(ext)
    }
}

fn parse_ron_kind(s: &str) -> Option<RonKind> {
    match s {
        "tuple" => Some(RonKind::Tuple),
        "char" => Some(RonKind::Char),
        "unit" => Some(RonKind::Unit),
        "bytes" => Some(RonKind::Bytes),
        "non-string-key-map" => Some(RonKind::NonStringKeyMap),
        "option" => Some(RonKind::Option),
        // "unknown" is the NodeKind::Unknown marker, not a RonKind.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NodeKind, SCHEMA_DIALECT_2020_12};

    fn sample_model() -> TypeModel {
        let mut model = TypeModel::new();
        model.add_active_extension("implicit_some");
        model.insert_named(
            "Point",
            TypeNode::new(NodeKind::Object {
                fields: vec![
                    Field {
                        serialized_key: "x".into(),
                        value: TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
                        optional: false,
                        flatten: false,
                    },
                    Field {
                        serialized_key: "label".into(),
                        value: TypeRef::inline(TypeNode::char_()),
                        optional: true,
                        flatten: false,
                    },
                ],
                deny_unknown_fields: true,
            }),
        );
        model.insert_named(
            "Pair",
            TypeNode::tuple(vec![
                TypeRef::named("Point"),
                TypeRef::inline(TypeNode::primitive(Primitive::String)),
            ]),
        );
        model
    }

    #[test]
    fn root_has_dialect_and_defs() {
        let json = to_json(&sample_model());
        assert_eq!(json["$schema"], json!(SCHEMA_DIALECT_2020_12));
        assert!(json["$defs"]["Point"].is_object());
        assert!(json["$defs"]["Pair"].is_object());
        assert_eq!(json["x-ron-extensions-active"], json!(["implicit_some"]));
    }

    #[test]
    fn tuple_uses_prefix_items_with_x_ron_keywords() {
        let json = to_json(&sample_model());
        let pair = &json["$defs"]["Pair"];
        assert_eq!(pair["type"], json!("array"));
        assert_eq!(pair["x-ron-kind"], json!("tuple"));
        assert_eq!(pair["x-ron-tuple-arity"], json!(2));
        assert_eq!(pair["prefixItems"][0]["$ref"], json!("#/$defs/Point"));
        assert_eq!(pair["items"], json!(false));
    }

    #[test]
    fn char_field_carries_x_ron_kind() {
        let json = to_json(&sample_model());
        let label = &json["$defs"]["Point"]["properties"]["label"];
        assert_eq!(label["type"], json!("string"));
        assert_eq!(label["x-ron-kind"], json!("char"));
        // `x` is required, `label` is optional.
        assert_eq!(json["$defs"]["Point"]["required"], json!(["x"]));
        assert_eq!(json["$defs"]["Point"]["additionalProperties"], json!(false));
    }

    #[test]
    fn round_trip_value_is_equal() {
        let model = sample_model();
        let json = to_json(&model);
        let back = from_json(&json).expect("round-trips");
        assert_eq!(model, back);
    }

    #[test]
    fn round_trip_string_is_equal() {
        let model = sample_model();
        let s = to_json_string(&model);
        let back = from_json_str(&s).expect("round-trips from string");
        assert_eq!(model, back);
    }

    #[test]
    fn unknown_node_round_trips() {
        let mut model = TypeModel::new();
        model.insert_named("Foreign", TypeNode::unknown());
        let json = to_json(&model);
        assert_eq!(json["$defs"]["Foreign"]["x-ron-kind"], json!("unknown"));
        let back = from_json(&json).unwrap();
        assert!(back.lookup("Foreign").unwrap().is_unknown());
    }

    #[test]
    fn malformed_root_errors() {
        let err = from_json(&json!("not an object")).unwrap_err();
        assert!(err.to_string().contains("root must be a JSON object"));
    }
}
