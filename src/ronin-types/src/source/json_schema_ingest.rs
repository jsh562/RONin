//! Shared general JSON-Schema-2020-12 → [`TypeModel`] ingestion core
//! {TR-007, TR-008}.
//!
//! Both [`SchemarsSource`](crate::source::SchemarsSource) (schemars-derived
//! schemas) and [`JsonSchemaSource`](crate::source::JsonSchemaSource)
//! (user-supplied schemas) ingest *foreign* JSON Schema 2020-12 documents — i.e.
//! schemas that do **not** necessarily use RONin's own `x-ron-*` interchange
//! shape produced by [`crate::serialize`]. This module is the one place that
//! lowers the standard 2020-12 vocabulary into the normalized [`TypeModel`], so
//! the two adapters share identical mapping behaviour and stay trivially
//! consistent (SC-004).
//!
//! # What it understands
//!
//! | JSON Schema construct | [`NodeKind`] mapping |
//! |-----------------------|----------------------|
//! | `{"$ref": "#/$defs/X"}` | [`TypeRef::Named`] |
//! | `{"type":"object","properties":…,"required":…}` | [`NodeKind::Object`] (`additionalProperties:false` ⇒ `deny_unknown_fields`) |
//! | `{"type":"object","additionalProperties":<schema>}` | [`NodeKind::Map`] (string-keyed) |
//! | `{"type":"array","items":<schema>}` | [`NodeKind::Sequence`] |
//! | `{"type":"array","prefixItems":[…]}` | [`NodeKind::Tuple`] |
//! | `{"type":"string","enum":[…]}` (≥1 string) | [`NodeKind::Enum`] of unit variants |
//! | `{"type":["T","null"]}` / `oneOf`/`anyOf` with a `null` branch | [`NodeKind::Option`] |
//! | `oneOf`/`anyOf` of variant objects | [`NodeKind::Enum`] (externally-tagged best effort) |
//! | `{"type":"boolean"/"integer"/"number"/"string"/"null"}` | [`NodeKind::Primitive`] |
//! | RONin's own `x-ron-*` interchange node | delegated to [`crate::serialize`] |
//! | anything unrecognized | [`NodeKind::Unknown`] + [`DiagnosticCategory::UnsupportedConstruct`] |
//!
//! # Invariants
//!
//! - **Never fails.** Unrecognized / unsupported constructs become `unknown`
//!   nodes plus an [`AcquisitionDiagnostic`]; ingestion never returns an error
//!   and never panics (TR-006, TR-011).
//! - **Recursion-safe.** Named (`$defs`) types and the root each become a single
//!   registry entry; cross-references are [`TypeRef::Named`] `$ref`s, never
//!   inline-expanded.

use serde_json::{Map, Value};

use crate::diagnostics::{AcquisitionDiagnostic, DiagnosticCategory, DiagnosticLocation};
use crate::model::{
    Field, NodeKind, Primitive, TypeModel, TypeNode, TypeRef, Variant, VariantShape,
};
use crate::serialize;

/// `$ref` prefixes this ingester recognizes for a `$defs`-style registry.
const REF_PREFIXES: [&str; 2] = ["#/$defs/", "#/definitions/"];

/// The fallback name used for a root schema with no `title`/`$id`.
const ROOT_FALLBACK_NAME: &str = "Root";

/// A single source of an error/oddity while ingesting a foreign schema.
struct Ingester {
    source_id: String,
    location: String,
    diagnostics: Vec<AcquisitionDiagnostic>,
    model: TypeModel,
}

/// Ingest a parsed general JSON Schema 2020-12 [`Value`] into a [`TypeModel`]
/// plus diagnostics (TR-007, TR-008).
///
/// `source_id` and `location` tag every emitted diagnostic for provenance. The
/// returned model is partial-but-valid: unsupported constructs are `unknown`
/// nodes, never errors.
#[must_use]
pub(crate) fn ingest_value(
    value: &Value,
    source_id: &str,
    location: &str,
) -> (TypeModel, Vec<AcquisitionDiagnostic>) {
    let mut ingester = Ingester {
        source_id: source_id.to_string(),
        location: location.to_string(),
        diagnostics: Vec::new(),
        model: TypeModel::new(),
    };
    ingester.run(value);
    let mut model = ingester.model;
    model.diagnostics = ingester.diagnostics.clone();
    (model, ingester.diagnostics)
}

/// Ingest a JSON Schema *string*, parsing it first (TR-007, TR-008).
///
/// A non-JSON string is itself recorded as an `UnsupportedConstruct` diagnostic
/// on an empty model — ingestion never returns an error.
#[must_use]
pub(crate) fn ingest_str(
    text: &str,
    source_id: &str,
    location: &str,
) -> (TypeModel, Vec<AcquisitionDiagnostic>) {
    match serde_json::from_str::<Value>(text) {
        Ok(value) => ingest_value(&value, source_id, location),
        Err(err) => {
            let mut model = TypeModel::new();
            let diag = AcquisitionDiagnostic::new(
                DiagnosticCategory::UnsupportedConstruct,
                location.to_string(),
                format!("input is not valid JSON: {err}"),
            )
            .with_source_id(source_id.to_string())
            .with_location(DiagnosticLocation {
                source: Some(location.to_string()),
                pointer: None,
            });
            model.diagnostics = vec![diag.clone()];
            (model, vec![diag])
        }
    }
}

impl Ingester {
    fn run(&mut self, value: &Value) {
        let Some(root) = value.as_object() else {
            self.diag(
                DiagnosticCategory::UnsupportedConstruct,
                ROOT_FALLBACK_NAME,
                "schema root is not a JSON object; nothing to ingest",
                None,
            );
            return;
        };

        // RONin's own interchange shape uses a closed `x-ron-*` vocabulary the
        // canonical deserializer already understands losslessly. Detect it and
        // delegate so a schema we produced round-trips exactly (and inherits the
        // serializer's diagnostics).
        if is_ron_interchange(root) {
            match serialize::from_json(value) {
                Ok(model) => {
                    self.model = model;
                    return;
                }
                Err(err) => {
                    // Fall through to general ingestion; record why the fast path
                    // was skipped so the behaviour is auditable.
                    self.diag(
                        DiagnosticCategory::UnsupportedConstruct,
                        ROOT_FALLBACK_NAME,
                        format!("x-ron interchange parse failed; using general ingestion: {err}"),
                        None,
                    );
                }
            }
        }

        // First register every `$defs` entry so the root can `$ref` them.
        for key in ["$defs", "definitions"] {
            if let Some(defs) = root.get(key).and_then(Value::as_object) {
                for (name, schema) in defs {
                    let node = self.ingest_node(schema, name);
                    self.model.insert_named(name.clone(), node);
                }
            }
        }

        // The root schema itself is a named type when it carries body keywords
        // (a bare `{ "$defs": … }` wrapper contributes only its definitions).
        if root_has_body(root) {
            let name = root_name(root);
            let node = self.ingest_node(value, &name);
            self.model.insert_named(name, node);
        }
    }

    /// Lower one schema [`Value`] into a [`TypeNode`].
    fn ingest_node(&mut self, value: &Value, subject: &str) -> TypeNode {
        let Some(obj) = value.as_object() else {
            // `true`/`false`/other JSON literals are not schemas we model.
            self.diag(
                DiagnosticCategory::UnsupportedConstruct,
                subject,
                "schema is not a JSON object; recorded as unknown",
                None,
            );
            return TypeNode::unknown();
        };

        match self.ingest_kind(obj, subject) {
            Some(kind) => TypeNode::new(kind),
            None => {
                self.diag(
                    DiagnosticCategory::UnsupportedConstruct,
                    subject,
                    "schema has no recognized JSON-Schema-2020-12 shape; recorded as unknown",
                    None,
                );
                TypeNode::unknown()
            }
        }
    }

    /// Lower one schema [`TypeRef`] (`$ref` → named, otherwise inline node).
    fn ingest_ref(&mut self, value: &Value, subject: &str) -> TypeRef {
        if let Some(obj) = value.as_object() {
            if let Some(name) = ref_target(obj) {
                return TypeRef::named(name);
            }
        }
        TypeRef::inline(self.ingest_node(value, subject))
    }

    /// Determine the [`NodeKind`] of an object schema, or `None` if unrecognized.
    fn ingest_kind(&mut self, obj: &Map<String, Value>, subject: &str) -> Option<NodeKind> {
        // A node that is purely a `$ref` is handled by `ingest_ref`; if we reach
        // here with a `$ref` (e.g. a `$defs` entry that is itself an alias),
        // resolve it to a named-alias object by inlining the reference shape is
        // not possible without the target, so model it as an unknown alias.
        if let Some(name) = ref_target(obj) {
            // A `$defs` entry that is a bare alias to another def. Represent it
            // as a single-element option-less indirection is not expressible, so
            // record an unsupported-construct and fall back to unknown.
            self.diag(
                DiagnosticCategory::UnsupportedConstruct,
                subject,
                format!("schema is a bare `$ref` to `{name}`; alias indirection is not modeled, recorded as unknown"),
                None,
            );
            return Some(NodeKind::Unknown);
        }

        // Option / nullable union: `oneOf`/`anyOf` with exactly one non-null
        // branch plus a null branch, or a `type` array containing "null".
        if let Some(kind) = self.try_nullable(obj, subject) {
            return Some(kind);
        }

        // Enum-of-variants via oneOf/anyOf.
        for key in ["oneOf", "anyOf"] {
            if let Some(arms) = obj.get(key).and_then(Value::as_array) {
                return Some(self.ingest_enum_oneof(arms, subject));
            }
        }

        // String enum: `{"type":"string","enum":[...]}` (all-unit Rust enum).
        if let Some(values) = obj.get("enum").and_then(Value::as_array) {
            if let Some(kind) = self.ingest_string_enum(values, subject) {
                return Some(kind);
            }
        }

        // `const` scalar — model as the underlying primitive (drops the literal
        // constraint, which the type model does not carry).
        if obj.contains_key("const") {
            if let Some(prim) = primitive_from_type(obj) {
                return Some(NodeKind::Primitive { primitive: prim });
            }
        }

        match type_keyword(obj) {
            Some(TypeTag::Single("object")) => Some(self.ingest_object(obj, subject)),
            Some(TypeTag::Single("array")) => Some(self.ingest_array(obj, subject)),
            Some(TypeTag::Single(prim)) => {
                primitive_keyword(prim).map(|primitive| NodeKind::Primitive { primitive })
            }
            // A multi-type array without a clean nullable shape (handled above)
            // is an unmodeled union.
            Some(TypeTag::Multiple) => None,
            None => {
                // No `type`/`oneOf`/`enum`: could be a structural object given by
                // `properties` alone, or a permissive `{}` schema.
                if obj.contains_key("properties")
                    || obj.contains_key("required")
                    || obj.contains_key("additionalProperties")
                {
                    Some(self.ingest_object(obj, subject))
                } else if obj.is_empty() || obj.keys().all(|k| is_annotation_keyword(k.as_str())) {
                    // The permissive "true" schema accepts anything → unknown.
                    Some(NodeKind::Unknown)
                } else {
                    None
                }
            }
        }
    }

    /// `{"type":"object", ...}` → object/struct or string-keyed map.
    fn ingest_object(&mut self, obj: &Map<String, Value>, subject: &str) -> NodeKind {
        let has_properties = obj
            .get("properties")
            .and_then(Value::as_object)
            .is_some_and(|p| !p.is_empty());

        // A map: `additionalProperties` is a *schema* and there are no declared
        // properties. (`additionalProperties: false`/`true` is a struct flag.)
        if !has_properties {
            if let Some(addl) = obj.get("additionalProperties") {
                if !matches!(addl, Value::Bool(_)) {
                    let value = self.ingest_ref(addl, &format!("{subject}.<value>"));
                    return NodeKind::Map {
                        // Foreign JSON-object maps are string-keyed by definition.
                        key: TypeRef::inline(TypeNode::primitive(Primitive::String)),
                        value,
                    };
                }
            }
        }

        let fields = self.ingest_fields(obj, subject);
        let deny_unknown_fields = obj.get("additionalProperties") == Some(&Value::Bool(false));
        NodeKind::Object {
            fields,
            deny_unknown_fields,
        }
    }

    /// `{"type":"array", ...}` → sequence (`items`) or tuple (`prefixItems`).
    fn ingest_array(&mut self, obj: &Map<String, Value>, subject: &str) -> NodeKind {
        if let Some(prefix) = obj.get("prefixItems").and_then(Value::as_array) {
            let elements = prefix
                .iter()
                .enumerate()
                .map(|(i, v)| self.ingest_ref(v, &format!("{subject}[{i}]")))
                .collect();
            // A tuple node auto-attaches the RonKind::Tuple extension.
            return TypeNode::tuple(elements).kind;
        }
        match obj.get("items") {
            Some(items) if !matches!(items, Value::Bool(_)) => {
                let element = self.ingest_ref(items, &format!("{subject}[]"));
                NodeKind::Sequence { element }
            }
            // `items: false`/absent with no prefixItems: an array of unknowns.
            _ => NodeKind::Sequence {
                element: TypeRef::inline(TypeNode::unknown()),
            },
        }
    }

    /// Lower the `properties`/`required` of an object into ordered [`Field`]s.
    fn ingest_fields(&mut self, obj: &Map<String, Value>, subject: &str) -> Vec<Field> {
        let required: std::collections::BTreeSet<&str> = obj
            .get("required")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        let mut fields = Vec::new();
        if let Some(props) = obj.get("properties").and_then(Value::as_object) {
            for (key, schema) in props {
                let value = self.ingest_ref(schema, &format!("{subject}.{key}"));
                // A field is optional unless listed in `required`, OR if its own
                // schema is a nullable/Option shape.
                let is_option = matches!(
                    &value,
                    TypeRef::Inline(n) if matches!(n.kind, NodeKind::Option { .. })
                );
                fields.push(Field {
                    serialized_key: key.clone(),
                    value,
                    optional: is_option || !required.contains(key.as_str()),
                    flatten: false,
                });
            }
        }
        fields
    }

    /// A `oneOf`/`anyOf` whose arms are variant schemas → an externally-tagged
    /// enum (the schemars / serde default representation).
    fn ingest_enum_oneof(&mut self, arms: &[Value], subject: &str) -> NodeKind {
        let mut variants = Vec::new();
        for (i, arm) in arms.iter().enumerate() {
            match self.ingest_variant(arm, &format!("{subject}::variant{i}")) {
                Some(variant) => variants.push(variant),
                None => {
                    self.diag(
                        DiagnosticCategory::UnsupportedConstruct,
                        subject,
                        format!("enum arm #{i} has no recognizable variant shape; skipped"),
                        None,
                    );
                }
            }
        }
        NodeKind::Enum {
            variants,
            discriminator: crate::model::Discriminator::External,
        }
    }

    /// Lower one `oneOf` arm into a [`Variant`].
    fn ingest_variant(&mut self, arm: &Value, subject: &str) -> Option<Variant> {
        let obj = arm.as_object()?;

        // Unit variant carried as a string enum literal: `{"enum":["Name"]}`.
        if let Some(values) = obj.get("enum").and_then(Value::as_array) {
            if let [Value::String(name)] = values.as_slice() {
                return Some(Variant {
                    serialized_name: name.clone(),
                    shape: VariantShape::Unit,
                });
            }
        }
        // Unit variant carried as `{"const":"Name"}`.
        if let Some(Value::String(name)) = obj.get("const") {
            return Some(Variant {
                serialized_name: name.clone(),
                shape: VariantShape::Unit,
            });
        }

        // Externally-tagged payload variant: a single-property object whose key
        // is the variant name and value is the payload schema.
        if type_keyword(obj) == Some(TypeTag::Single("object")) || obj.contains_key("properties") {
            if let Some(props) = obj.get("properties").and_then(Value::as_object) {
                if props.len() == 1 {
                    let (name, payload) = props.iter().next().expect("len == 1");
                    let shape = self.ingest_variant_payload(payload, &format!("{subject}.{name}"));
                    return Some(Variant {
                        serialized_name: name.clone(),
                        shape,
                    });
                }
                // Multiple properties with no single tag key: an internally- or
                // adjacently-tagged / struct-like variant. Model the whole arm as
                // a struct variant keyed by an inferred tag `const`, if present.
                if let Some(name) = inferred_tag_const(props) {
                    let fields = self.ingest_fields(obj, subject);
                    return Some(Variant {
                        serialized_name: name,
                        shape: VariantShape::Struct(fields),
                    });
                }
            }
        }

        None
    }

    /// Lower the payload schema of an externally-tagged variant into its shape.
    fn ingest_variant_payload(&mut self, payload: &Value, subject: &str) -> VariantShape {
        if let Some(obj) = payload.as_object() {
            // Struct variant: payload is an object with properties.
            if type_keyword(obj) == Some(TypeTag::Single("object"))
                && obj
                    .get("properties")
                    .and_then(Value::as_object)
                    .is_some_and(|p| !p.is_empty())
            {
                return VariantShape::Struct(self.ingest_fields(obj, subject));
            }
            // Tuple variant: payload is an array with prefixItems.
            if let Some(prefix) = obj.get("prefixItems").and_then(Value::as_array) {
                let elements = prefix
                    .iter()
                    .enumerate()
                    .map(|(i, v)| self.ingest_ref(v, &format!("{subject}[{i}]")))
                    .collect();
                return VariantShape::Tuple(elements);
            }
        }
        // Otherwise a newtype variant carrying a single inner type.
        VariantShape::Newtype(self.ingest_ref(payload, subject))
    }

    /// `{"type":"string","enum":[...]}` with one-or-more string values →
    /// an enum of unit variants. Returns `None` if the values are not all
    /// strings (e.g. an integer/mixed enum, which is not a Rust unit enum).
    fn ingest_string_enum(&mut self, values: &[Value], subject: &str) -> Option<NodeKind> {
        if values.is_empty() {
            return None;
        }
        let mut variants = Vec::with_capacity(values.len());
        for v in values {
            let name = v.as_str()?;
            variants.push(Variant {
                serialized_name: name.to_string(),
                shape: VariantShape::Unit,
            });
        }
        let _ = subject;
        Some(NodeKind::Enum {
            variants,
            discriminator: crate::model::Discriminator::External,
        })
    }

    /// Detect a nullable/Option shape and lower it to [`NodeKind::Option`].
    fn try_nullable(&mut self, obj: &Map<String, Value>, subject: &str) -> Option<NodeKind> {
        // Form 1: `type` array containing "null" plus exactly one other type.
        if let Some(types) = obj.get("type").and_then(Value::as_array) {
            let names: Vec<&str> = types.iter().filter_map(Value::as_str).collect();
            if names.contains(&"null") && names.len() == 2 {
                let other = names
                    .iter()
                    .find(|t| **t != "null")
                    .copied()
                    .expect("len == 2 with a null");
                if let Some(primitive) = primitive_keyword(other) {
                    let inner = TypeRef::inline(TypeNode::primitive(primitive));
                    return Some(NodeKind::Option { inner });
                }
            }
        }
        // Form 2: oneOf/anyOf of exactly two arms, one a bare `{"type":"null"}`.
        for key in ["oneOf", "anyOf"] {
            if let Some(arms) = obj.get(key).and_then(Value::as_array) {
                if arms.len() == 2 {
                    let null_idx = arms.iter().position(is_null_schema);
                    if let Some(idx) = null_idx {
                        let other = &arms[1 - idx];
                        let inner = self.ingest_ref(other, &format!("{subject}.<some>"));
                        return Some(NodeKind::Option { inner });
                    }
                }
            }
        }
        None
    }

    fn diag(
        &mut self,
        category: DiagnosticCategory,
        subject: impl Into<String>,
        detail: impl Into<String>,
        pointer: Option<String>,
    ) {
        let diag = AcquisitionDiagnostic::new(category, subject, detail)
            .with_source_id(self.source_id.clone())
            .with_location(DiagnosticLocation {
                source: Some(self.location.clone()),
                pointer,
            });
        self.diagnostics.push(diag);
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// `true` when the root object looks like RONin's own `x-ron-*` interchange
/// (carries the closed extension markers the canonical deserializer reads).
fn is_ron_interchange(root: &Map<String, Value>) -> bool {
    if root.contains_key("x-ron-extensions-active") || root.contains_key("x-ron-diagnostics") {
        return true;
    }
    // Any `$defs` entry using an x-ron marker means it is our shape, not foreign.
    root.get("$defs")
        .and_then(Value::as_object)
        .is_some_and(|defs| defs.values().any(node_has_ron_marker))
}

/// `true` when a node uses one of RONin's closed `x-ron-*` shape markers (as
/// opposed to a plain foreign schema that merely happens to carry annotations).
fn node_has_ron_marker(value: &Value) -> bool {
    value.as_object().is_some_and(|obj| {
        obj.contains_key("x-ron-kind")
            || obj.contains_key("x-ron-variant")
            || obj.contains_key("x-ron-key")
            || obj.contains_key("x-ron-tuple-arity")
    })
}

/// The `$defs`/`definitions` target name of a bare `$ref`, if any.
fn ref_target(obj: &Map<String, Value>) -> Option<String> {
    let reference = obj.get("$ref").and_then(Value::as_str)?;
    for prefix in REF_PREFIXES {
        if let Some(name) = reference.strip_prefix(prefix) {
            return Some(name.to_string());
        }
    }
    // A `$ref` we cannot resolve to a local def name — fall back to the raw
    // pointer's last path segment so it still maps to a named ref.
    reference
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

/// Does the root object carry body keywords (i.e. is the root itself a type)?
fn root_has_body(root: &Map<String, Value>) -> bool {
    const BODY_KEYS: [&str; 8] = [
        "type",
        "properties",
        "oneOf",
        "anyOf",
        "enum",
        "const",
        "items",
        "prefixItems",
    ];
    BODY_KEYS.iter().any(|k| root.contains_key(*k)) || root.contains_key("additionalProperties")
}

/// The name for the root type: `title`, else the trailing segment of `$id`,
/// else [`ROOT_FALLBACK_NAME`].
fn root_name(root: &Map<String, Value>) -> String {
    if let Some(title) = root.get("title").and_then(Value::as_str) {
        if !title.is_empty() {
            return title.to_string();
        }
    }
    if let Some(id) = root.get("$id").and_then(Value::as_str) {
        if let Some(seg) = id.rsplit('/').next().filter(|s| !s.is_empty()) {
            return seg.trim_end_matches(".json").to_string();
        }
    }
    ROOT_FALLBACK_NAME.to_string()
}

/// A single recognized `type` keyword, a multi-type union, or absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypeTag<'a> {
    Single(&'a str),
    Multiple,
}

fn type_keyword(obj: &Map<String, Value>) -> Option<TypeTag<'_>> {
    match obj.get("type") {
        Some(Value::String(s)) => Some(TypeTag::Single(s.as_str())),
        Some(Value::Array(_)) => Some(TypeTag::Multiple),
        _ => None,
    }
}

/// Map a single JSON Schema scalar `type` keyword to a [`Primitive`].
fn primitive_keyword(name: &str) -> Option<Primitive> {
    match name {
        "boolean" => Some(Primitive::Boolean),
        "integer" => Some(Primitive::Integer),
        "number" => Some(Primitive::Number),
        "string" => Some(Primitive::String),
        "null" => Some(Primitive::Null),
        _ => None,
    }
}

/// The primitive of an object whose `type` is a single scalar keyword.
fn primitive_from_type(obj: &Map<String, Value>) -> Option<Primitive> {
    match type_keyword(obj)? {
        TypeTag::Single(name) => primitive_keyword(name),
        TypeTag::Multiple => None,
    }
}

/// `true` for the `{"type":"null"}` schema (the null branch of an Option).
fn is_null_schema(value: &Value) -> bool {
    value
        .as_object()
        .and_then(|o| o.get("type"))
        .and_then(Value::as_str)
        == Some("null")
}

/// For an internally-tagged variant, find the property whose schema is a
/// `{"const": "<Name>"}` and return that name (the serde tag value).
fn inferred_tag_const(props: &Map<String, Value>) -> Option<String> {
    props.values().find_map(|schema| {
        schema
            .as_object()
            .and_then(|o| o.get("const"))
            .and_then(Value::as_str)
            .map(ToString::to_string)
    })
}

/// Annotation-only keywords that do not, by themselves, give a node a shape.
fn is_annotation_keyword(key: &str) -> bool {
    matches!(
        key,
        "title"
            | "description"
            | "$schema"
            | "$id"
            | "$comment"
            | "default"
            | "examples"
            | "readOnly"
            | "writeOnly"
            | "deprecated"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::DiagnosticSeverity;
    use serde_json::json;

    fn ingest(value: Value) -> (TypeModel, Vec<AcquisitionDiagnostic>) {
        ingest_value(&value, "test", "<test>")
    }

    #[test]
    fn root_object_with_defs_resolves_named_ref() {
        let schema = json!({
            "title": "Demo",
            "type": "object",
            "properties": { "inner": { "$ref": "#/$defs/Inner" } },
            "required": ["inner"],
            "$defs": { "Inner": { "type": "object", "properties": { "v": { "type": "integer" } }, "required": ["v"] } }
        });
        let (model, diags) = ingest(schema);
        assert!(model.contains("Demo"));
        assert!(model.contains("Inner"));
        let demo = model.lookup("Demo").unwrap();
        let NodeKind::Object { fields, .. } = &demo.kind else {
            panic!("object");
        };
        assert_eq!(fields[0].value.as_named(), Some("Inner"));
        assert!(diags.is_empty(), "no diagnostics: {diags:?}");
    }

    #[test]
    fn type_array_with_null_is_option() {
        let (model, _) = ingest(json!({
            "title": "Holder",
            "type": "object",
            "properties": { "maybe": { "type": ["string", "null"] } }
        }));
        let holder = model.lookup("Holder").unwrap();
        let NodeKind::Object { fields, .. } = &holder.kind else {
            panic!("object");
        };
        let maybe = model.resolve(&fields[0].value).unwrap();
        assert!(matches!(maybe.kind, NodeKind::Option { .. }));
        assert!(fields[0].optional);
    }

    #[test]
    fn prefix_items_become_tuple() {
        let (model, _) = ingest(json!({
            "title": "Pair",
            "type": "array",
            "prefixItems": [ { "type": "integer" }, { "type": "string" } ]
        }));
        let pair = model.lookup("Pair").unwrap();
        let NodeKind::Tuple { elements } = &pair.kind else {
            panic!("tuple");
        };
        assert_eq!(elements.len(), 2);
    }

    #[test]
    fn additional_properties_schema_is_map() {
        let (model, _) = ingest(json!({
            "title": "Bag",
            "type": "object",
            "additionalProperties": { "type": "integer" }
        }));
        let bag = model.lookup("Bag").unwrap();
        assert!(matches!(bag.kind, NodeKind::Map { .. }));
    }

    #[test]
    fn string_enum_becomes_unit_variants() {
        let (model, _) = ingest(json!({
            "title": "Color",
            "type": "string",
            "enum": ["Red", "Green", "Blue"]
        }));
        let color = model.lookup("Color").unwrap();
        let NodeKind::Enum { variants, .. } = &color.kind else {
            panic!("enum");
        };
        assert_eq!(variants.len(), 3);
        assert!(variants
            .iter()
            .all(|v| matches!(v.shape, VariantShape::Unit)));
    }

    #[test]
    fn one_of_variant_objects_become_enum() {
        let (model, _) = ingest(json!({
            "title": "Shape",
            "oneOf": [
                { "type": "string", "enum": ["Empty"] },
                { "type": "object", "properties": { "Circle": { "type": "number" } }, "required": ["Circle"], "additionalProperties": false }
            ]
        }));
        let shape = model.lookup("Shape").unwrap();
        let NodeKind::Enum { variants, .. } = &shape.kind else {
            panic!("enum");
        };
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[0].serialized_name, "Empty");
        assert!(matches!(variants[0].shape, VariantShape::Unit));
        assert_eq!(variants[1].serialized_name, "Circle");
        assert!(matches!(variants[1].shape, VariantShape::Newtype(_)));
    }

    #[test]
    fn unsupported_construct_is_unknown_not_error() {
        // A node with no recognizable shape becomes unknown + a diagnostic.
        let (model, diags) = ingest(json!({
            "title": "Weird",
            "type": ["integer", "string", "boolean"]
        }));
        let weird = model.lookup("Weird").unwrap();
        assert!(weird.is_unknown());
        assert!(diags
            .iter()
            .any(|d| d.category == DiagnosticCategory::UnsupportedConstruct));
    }

    #[test]
    fn non_object_root_is_diagnostic_not_panic() {
        let (model, diags) = ingest(json!("not a schema"));
        assert!(model.is_empty());
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, DiagnosticSeverity::Warning);
    }

    #[test]
    fn invalid_json_string_is_diagnostic() {
        let (model, diags) = ingest_str("{ not json", "test", "<test>");
        assert!(model.is_empty());
        assert!(diags
            .iter()
            .any(|d| d.category == DiagnosticCategory::UnsupportedConstruct));
    }

    #[test]
    fn ron_interchange_round_trips_via_delegation() {
        // A document carrying our own x-ron markers is delegated to the canonical
        // deserializer rather than re-ingested generically.
        let interchange = json!({
            "$schema": SCHEMA_DIALECT,
            "$defs": {
                "Pair": {
                    "type": "array",
                    "x-ron-kind": "tuple",
                    "x-ron-tuple-arity": 2,
                    "prefixItems": [ { "type": "integer" }, { "type": "string" } ],
                    "items": false
                }
            }
        });
        let (model, diags) = ingest(interchange);
        let pair = model.lookup("Pair").unwrap();
        assert_eq!(
            pair.ron_extension.as_ref().unwrap().tuple_arity,
            Some(2),
            "x-ron extension preserved via delegation"
        );
        assert!(diags.is_empty());
    }

    const SCHEMA_DIALECT: &str = crate::model::SCHEMA_DIALECT_2020_12;
}
