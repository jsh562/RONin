//! The native Bevy type-registry [`TypeSource`] adapter {E009, FR-001/003/008/017}.
//!
//! [`BevySource`] ingests a Bevy **registry-schema-format JSON** export — the
//! shape the BRP `bevy/registry/schema` method returns: a map of fully-qualified
//! Rust type path → reflect schema — and maps each registered type into the
//! normalized E004 [`TypeModel`] (JSON-Schema-2020-12 + `x-ron-*`). It plugs into
//! the existing [`TypeSource`] adapter contract (`source_id` / `precedence` /
//! `acquire`) and adds **only** the already-present
//! [`SourcePrecedence::Bevy`](crate::source::SourcePrecedence::Bevy) rank — no
//! change to [`TypeModel`], [`TypeNode`](crate::model::TypeNode), or any existing
//! adapter (AD-002, TR-009).
//!
//! # Consumed strictly as data (no `bevy` crate)
//!
//! The registry is read as ordinary JSON (ADR-0002): the fully-qualified type
//! paths are JSON string keys, never a compile-time dependency on `bevy`. The
//! BRP registry schema is close to JSON Schema 2020-12, so the type-path `$ref`s,
//! `properties`/`required`, `prefixItems`/`items`, and `oneOf` enum arms reuse
//! the same lowering vocabulary as the [`json_schema_ingest`](crate::source::json_schema_ingest)
//! core. The registry-specific surface (the reflect `kind` keyword, `reflectTypes`,
//! and the per-type concrete defaults a *defaults-carrying* export adds) is parsed
//! here into a tolerant intermediate, [`BevyRegistry`].
//!
//! # Invariants
//!
//! - **Never-fail acquire (FR-008, TR-006/011).** Malformed / partial / empty
//!   input yields an empty-or-partial registry + [`AcquisitionDiagnostic`]s, never
//!   a panic and never a fatal error. An unrecognized reflect `kind` degrades to a
//!   first-class [`unknown`](crate::model::TypeNode::unknown) node plus a
//!   diagnostic — never a false error (FR-006).
//! - **Version-tolerant (FR-003, ADR-0004).** Unknown / extra schema fields are
//!   ignored, never fatal — the reader keeps only the fields it understands.
//! - **Native-only / WASM-clean preserved (FR-017).** Lives in native `ronin-types`;
//!   contributes nothing to the WASM-clean core.
//!
//! # Future BRP read (FR-002)
//!
//! Live registry acquisition via the Bevy Remote Protocol is deferred. Because
//! BRP returns the *same* registry-schema format this adapter already ingests, a
//! future BRP read slots in as **another constructor** on [`BevySource`] (e.g. a
//! `from_brp_endpoint`) that produces the same parsed [`BevyRegistry`] — with no
//! change to the WASM-clean core, the [`TypeModel`], or this mapping.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use crate::diagnostics::{AcquisitionDiagnostic, DiagnosticCategory, DiagnosticLocation};
use crate::model::{
    Discriminator, Field, NodeKind, Primitive, TypeModel, TypeNode, TypeRef, Variant, VariantShape,
};
use crate::source::{Acquired, SourcePrecedence, TypeSource};

/// The stable `reflectTypes` marker whose presence is a precondition for defaults
/// elision (a type without a reflected `Default` can never be elided — FR-014).
const REFLECT_DEFAULT: &str = "Default";

// ===========================================================================
// BevyRegistry — the parsed, tolerant intermediate {Data Model §2}
// ===========================================================================

/// The reflect `kind` keyword of a registered type, mapped tolerantly from the
/// registry export.
///
/// The variants mirror Bevy's reflect kinds the BRP schema reports. An
/// **unrecognized** keyword is preserved verbatim as [`ReflectKind::Unknown`] so
/// the adapter can degrade that one type to an `unknown` node (plus a diagnostic)
/// without affecting the rest of the registry (FR-003/006/008).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReflectKind {
    /// A `Struct` — named fields (`properties` + `required`).
    Struct,
    /// A `TupleStruct` — ordered, positional fields (`prefixItems`).
    TupleStruct,
    /// A `Tuple` — ordered, positional elements (`prefixItems`).
    Tuple,
    /// An `Enum` — variants (`oneOf`).
    Enum,
    /// A `List` (homogeneous sequence; `items`).
    List,
    /// An `Array` (fixed-length homogeneous sequence; `items`).
    Array,
    /// A `Set` (collection; modeled as a sequence of its element type).
    Set,
    /// A `Map` (associative; `additionalProperties` / `valueType`).
    Map,
    /// A leaf / scalar `Value` (the reflect opaque-scalar kind).
    Value,
    /// An unrecognized reflect kind, preserved verbatim (version-tolerant).
    Unknown(String),
}

impl ReflectKind {
    /// Parse a reflect-`kind` keyword tolerantly. Recognition is case-sensitive
    /// against Bevy's reflect kinds; anything else is preserved as
    /// [`ReflectKind::Unknown`] so degradation is per-type, not fatal.
    #[must_use]
    fn parse(raw: &str) -> Self {
        match raw {
            "Struct" => ReflectKind::Struct,
            "TupleStruct" => ReflectKind::TupleStruct,
            "Tuple" => ReflectKind::Tuple,
            "Enum" => ReflectKind::Enum,
            "List" => ReflectKind::List,
            "Array" => ReflectKind::Array,
            "Set" => ReflectKind::Set,
            "Map" => ReflectKind::Map,
            "Value" => ReflectKind::Value,
            other => ReflectKind::Unknown(other.to_string()),
        }
    }

    /// The stable keyword for this kind (the original string for an unknown).
    #[inline]
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            ReflectKind::Struct => "Struct",
            ReflectKind::TupleStruct => "TupleStruct",
            ReflectKind::Tuple => "Tuple",
            ReflectKind::Enum => "Enum",
            ReflectKind::List => "List",
            ReflectKind::Array => "Array",
            ReflectKind::Set => "Set",
            ReflectKind::Map => "Map",
            ReflectKind::Value => "Value",
            ReflectKind::Unknown(raw) => raw,
        }
    }
}

/// One registered type's parsed reflect schema (Data Model §2 `ReflectSchema`).
///
/// Holds only the fields the elision / validation code downstream needs; the raw
/// schema [`Value`] is retained so the [`TypeNode`] mapping in
/// [`BevySource::acquire`] can read the structural keywords (`properties`,
/// `prefixItems`, `oneOf`, `items`, `valueType`, …) the same way the JSON-Schema
/// core does — keeping that lowering close to the proven adapter.
#[derive(Debug, Clone)]
pub struct ReflectSchema {
    /// The reflect kind (`Struct`/`Enum`/… or [`ReflectKind::Unknown`]).
    kind: ReflectKind,
    /// The set of fields the reflect schema marks `required` (excludes
    /// `Option<T>`). NOTE: "required by reflect", **not** "must appear in scene"
    /// (FR-006 pitfall) — the adapter uses this only to mark a field optional.
    required: BTreeSet<String>,
    /// The `reflectTypes` set (e.g. `Default`/`Component`/`Resource`/`Serialize`).
    /// `Default` presence gates elidability (FR-014).
    reflect_types: BTreeSet<String>,
    /// A concrete default value, present **only** when a defaults-carrying export
    /// supplied it (the BRP schema alone omits concrete defaults). Absent ⇒ the
    /// type's default is unknown ⇒ its fields are non-elidable (FR-014).
    default_value: Option<Value>,
    /// The raw reflect schema object — the source of the structural keywords the
    /// [`TypeNode`] mapping reads. Retained verbatim for tolerant lowering.
    raw: Map<String, Value>,
}

impl ReflectSchema {
    /// The reflect kind of this type.
    #[inline]
    #[must_use]
    pub fn kind(&self) -> &ReflectKind {
        &self.kind
    }

    /// `true` if `field` is in this type's reflect-`required` set.
    #[inline]
    #[must_use]
    pub fn is_required(&self, field: &str) -> bool {
        self.required.contains(field)
    }

    /// `true` if `Default` is reflected — a precondition for elidability (FR-014).
    #[inline]
    #[must_use]
    pub fn is_default_reflected(&self) -> bool {
        self.reflect_types.contains(REFLECT_DEFAULT)
    }

    /// The concrete default value, if a defaults-carrying export supplied one.
    #[inline]
    #[must_use]
    pub fn default_value(&self) -> Option<&Value> {
        self.default_value.as_ref()
    }
}

/// The parsed, in-memory Bevy registry (Data Model §2 `BevyRegistry (ingested)`).
///
/// A map of fully-qualified type path → [`ReflectSchema`], plus an optional
/// apparent Bevy-version marker driving the staleness advisory (FR-008). It is
/// the intermediate [`BevySource`] maps into the [`TypeModel`] and the lookup the
/// later scene validator / elision code consults by type path. Transient — held
/// for the session, never persisted.
///
/// Built tolerantly by [`BevyRegistry::from_schema_json`] /
/// [`BevyRegistry::from_schema_value`]: unknown fields are ignored and malformed
/// input degrades to an empty-or-partial registry plus diagnostics, never a panic
/// (FR-003/008).
#[derive(Debug, Clone, Default)]
pub struct BevyRegistry {
    /// type path → reflect schema (deterministic, sorted iteration).
    types: BTreeMap<String, ReflectSchema>,
    /// The export's apparent Bevy version, if it carried one (staleness marker).
    apparent_version: Option<String>,
}

impl BevyRegistry {
    /// Parse a registry-schema JSON **string** tolerantly into a [`BevyRegistry`]
    /// plus diagnostics (FR-003/008).
    ///
    /// Invalid JSON is **not** fatal: it yields an empty registry plus a single
    /// `UnsupportedConstruct` diagnostic, never a panic. The caller threads
    /// `source_id`/`location` onto every emitted diagnostic for provenance.
    #[must_use]
    pub fn from_schema_json(
        json: &str,
        source_id: &str,
        location: &str,
    ) -> (Self, Vec<AcquisitionDiagnostic>) {
        match serde_json::from_str::<Value>(json) {
            Ok(value) => Self::from_schema_value(&value, source_id, location),
            Err(err) => {
                let diag = diag(
                    DiagnosticCategory::UnsupportedConstruct,
                    "<registry>",
                    format!("registry export is not valid JSON: {err}"),
                    source_id,
                    location,
                    None,
                );
                (Self::default(), vec![diag])
            }
        }
    }

    /// Parse an already-parsed registry-schema [`Value`] tolerantly into a
    /// [`BevyRegistry`] plus diagnostics (FR-003/008).
    ///
    /// Accepts either the bare type-path → schema map, or a wrapper object
    /// carrying the map under a conventional key (`$defs` / `types` / `schemas`)
    /// alongside extra/unknown metadata fields (ignored, version-tolerant). A root
    /// that is not a JSON object yields an empty registry plus a diagnostic.
    #[must_use]
    pub fn from_schema_value(
        value: &Value,
        source_id: &str,
        location: &str,
    ) -> (Self, Vec<AcquisitionDiagnostic>) {
        let mut diagnostics = Vec::new();
        let Some(root) = value.as_object() else {
            diagnostics.push(diag(
                DiagnosticCategory::UnsupportedConstruct,
                "<registry>",
                "registry export root is not a JSON object; nothing to ingest",
                source_id,
                location,
                None,
            ));
            return (Self::default(), diagnostics);
        };

        // Version-tolerant: read an apparent-version marker if present under any
        // of the conventional keys, ignore it otherwise.
        let apparent_version = root
            .iter()
            .find(|(k, _)| is_version_key(k))
            .and_then(|(_, v)| v.as_str())
            .map(ToString::to_string);

        // The type map is either the root itself, or nested under a conventional
        // wrapper key. Extra sibling fields on a wrapper are ignored (FR-003).
        let type_map = locate_type_map(root);

        let mut types = BTreeMap::new();
        for (type_path, schema_value) in type_map {
            // A non-object schema entry is tolerated: skip it with a diagnostic
            // rather than aborting the whole registry (FR-008).
            let Some(schema_obj) = schema_value.as_object() else {
                diagnostics.push(diag(
                    DiagnosticCategory::UnsupportedConstruct,
                    type_path,
                    "reflect schema entry is not a JSON object; skipped",
                    source_id,
                    location,
                    Some(type_path.clone()),
                ));
                continue;
            };
            types.insert(type_path.clone(), parse_reflect_schema(schema_obj));
        }

        (
            Self {
                types,
                apparent_version,
            },
            diagnostics,
        )
    }

    /// `true` if a type with this fully-qualified path is registered.
    #[inline]
    #[must_use]
    pub fn contains(&self, type_path: &str) -> bool {
        self.types.contains_key(type_path)
    }

    /// The reflect kind of a registered type path, if present.
    #[inline]
    #[must_use]
    pub fn reflect_kind(&self, type_path: &str) -> Option<&ReflectKind> {
        self.types.get(type_path).map(ReflectSchema::kind)
    }

    /// `true` if the type path is registered AND reflects `Default` — the
    /// precondition for any field of that type being elidable (FR-014).
    #[inline]
    #[must_use]
    pub fn is_default_reflected(&self, type_path: &str) -> bool {
        self.types
            .get(type_path)
            .is_some_and(ReflectSchema::is_default_reflected)
    }

    /// The concrete default value for a type path, present **only** when a
    /// defaults-carrying export supplied it (absent ⇒ non-elidable, FR-014).
    #[inline]
    #[must_use]
    pub fn default_value(&self, type_path: &str) -> Option<&Value> {
        self.types
            .get(type_path)
            .and_then(ReflectSchema::default_value)
    }

    /// The full reflect schema for a type path, if registered.
    #[inline]
    #[must_use]
    pub fn schema(&self, type_path: &str) -> Option<&ReflectSchema> {
        self.types.get(type_path)
    }

    /// The export's apparent Bevy version, if it carried one (staleness marker).
    #[inline]
    #[must_use]
    pub fn apparent_version(&self) -> Option<&str> {
        self.apparent_version.as_deref()
    }

    /// `true` if no type is registered (a structural-only / degraded registry).
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }

    /// The number of registered type paths.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.types.len()
    }

    /// Iterate the registered type paths in deterministic (sorted) order.
    pub fn type_paths(&self) -> impl Iterator<Item = &str> {
        self.types.keys().map(String::as_str)
    }

    /// Iterate `(type path, reflect schema)` pairs in deterministic order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &ReflectSchema)> {
        self.types.iter().map(|(p, s)| (p.as_str(), s))
    }
}

// ===========================================================================
// BevySource — the TypeSource adapter {Data Model §1, T008}
// ===========================================================================

/// A [`TypeSource`] over a Bevy registry-schema-format JSON export (E009).
///
/// Build it from a registry-schema JSON string ([`BevySource::from_schema_json`]),
/// an already-parsed [`Value`] ([`BevySource::from_schema_value`]), a `.json` file
/// on disk ([`BevySource::from_path`]), or an already-parsed [`BevyRegistry`]
/// ([`BevySource::from_registry`]). Its merge precedence is
/// [`SourcePrecedence::Bevy`] — the highest in the total order, since Bevy mode
/// *replaces* the active source for a `.scn.ron` document (FR-013).
///
/// [`acquire`](TypeSource::acquire) maps every registered type path to a
/// [`TypeNode`] and inserts it into a partial [`TypeModel`]; it never panics and
/// never returns a fatal error (FR-008, TR-006/011).
#[derive(Debug, Clone)]
pub struct BevySource {
    /// Stable id for provenance/conflict diagnostics (`"bevy-registry[:<label>]"`).
    id: String,
    /// A human-readable origin label for diagnostic locations.
    label: String,
    /// The ingestible payload.
    payload: Payload,
}

/// The ingestible payload of a [`BevySource`].
#[derive(Debug, Clone)]
enum Payload {
    /// Raw registry-schema JSON text (parsed at `acquire` time).
    Text(String),
    /// An already-parsed registry-schema document.
    Value(Box<Value>),
    /// An already-parsed registry (e.g. produced by a future BRP read).
    Registry(Box<BevyRegistry>),
}

impl BevySource {
    /// Build a source from a registry-schema JSON **string**.
    #[must_use]
    pub fn from_schema_json(json: impl Into<String>) -> Self {
        Self {
            id: "bevy-registry".to_string(),
            label: "<bevy-registry-json>".to_string(),
            payload: Payload::Text(json.into()),
        }
    }

    /// Build a source from an already-parsed registry-schema [`Value`].
    #[must_use]
    pub fn from_schema_value(value: Value) -> Self {
        Self {
            id: "bevy-registry".to_string(),
            label: "<bevy-registry-value>".to_string(),
            payload: Payload::Value(Box::new(value)),
        }
    }

    /// Build a source from an already-parsed [`BevyRegistry`].
    ///
    /// This is the constructor a future BRP read (FR-002) reuses: it acquires the
    /// same registry-schema format over the network into a [`BevyRegistry`] and
    /// hands it here — no change to this mapping or the WASM-clean core.
    #[must_use]
    pub fn from_registry(registry: BevyRegistry) -> Self {
        Self {
            id: "bevy-registry".to_string(),
            label: "<bevy-registry>".to_string(),
            payload: Payload::Registry(Box::new(registry)),
        }
    }

    /// Build a source from a `.json` registry-schema file on disk.
    ///
    /// A read failure is captured as JSON text that fails to parse, so `acquire`
    /// emits a diagnostic rather than failing — the never-fail contract (FR-008).
    /// The file path is folded into the source id (`"bevy-registry:<path>"`) for
    /// provenance.
    #[must_use]
    pub fn from_path(path: impl AsRef<std::path::Path>) -> Self {
        let path = path.as_ref();
        let label = path.display().to_string();
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|err| format!("// ronin-types: failed to read registry export: {err}"));
        Self {
            id: format!("bevy-registry:{label}"),
            label,
            payload: Payload::Text(text),
        }
    }
}

impl TypeSource for BevySource {
    fn source_id(&self) -> String {
        self.id.clone()
    }

    fn precedence(&self) -> SourcePrecedence {
        SourcePrecedence::Bevy
    }

    fn acquire(&self) -> Acquired {
        // Parse (tolerantly) into the registry intermediate, collecting any
        // parse-level diagnostics.
        let (registry, mut diagnostics) = match &self.payload {
            Payload::Text(text) => BevyRegistry::from_schema_json(text, &self.id, &self.label),
            Payload::Value(value) => BevyRegistry::from_schema_value(value, &self.id, &self.label),
            Payload::Registry(registry) => ((**registry).clone(), Vec::new()),
        };

        // Map each registered type path → a TypeNode in a partial TypeModel.
        let mut model = TypeModel::new();
        for (type_path, schema) in registry.iter() {
            let node = self.map_type(type_path, schema, &mut diagnostics);
            model.insert_named(type_path.to_string(), node);
        }

        // Diagnostics travel with the model (mirrors the JSON-Schema adapters).
        model.diagnostics = diagnostics.clone();
        Acquired { model, diagnostics }
    }
}

impl BevySource {
    /// Map one registered reflect schema into its [`TypeNode`], appending any
    /// degradation diagnostics. An unrecognized kind degrades to `unknown` plus a
    /// diagnostic (FR-006/008) — never a panic, never a false error.
    fn map_type(
        &self,
        type_path: &str,
        schema: &ReflectSchema,
        diagnostics: &mut Vec<AcquisitionDiagnostic>,
    ) -> TypeNode {
        match schema.kind() {
            ReflectKind::Struct => self.map_struct(type_path, schema),
            ReflectKind::TupleStruct | ReflectKind::Tuple => self.map_tuple(type_path, schema),
            ReflectKind::Enum => self.map_enum(type_path, schema, diagnostics),
            ReflectKind::List | ReflectKind::Array | ReflectKind::Set => {
                self.map_sequence(type_path, schema)
            }
            ReflectKind::Map => self.map_map(type_path, schema),
            ReflectKind::Value => map_value_scalar(schema),
            ReflectKind::Unknown(raw) => {
                diagnostics.push(diag(
                    DiagnosticCategory::UnsupportedConstruct,
                    type_path,
                    format!(
                        "unrecognized reflect kind `{raw}`; recorded as an unconstrained `unknown` node"
                    ),
                    &self.id,
                    &self.label,
                    Some(type_path.to_string()),
                ));
                TypeNode::unknown()
            }
        }
    }

    /// `Struct` → object with named [`Field`]s (`deny_unknown_fields: true`, since
    /// Bevy's reflect schema sets `additionalProperties: false`).
    fn map_struct(&self, type_path: &str, schema: &ReflectSchema) -> TypeNode {
        let mut fields = Vec::new();
        if let Some(props) = schema.raw.get("properties").and_then(Value::as_object) {
            for (key, prop) in props {
                let value = property_ref(prop, &format!("{type_path}.{key}"));
                fields.push(Field {
                    serialized_key: key.clone(),
                    value,
                    // Optional unless reflect-`required` (FR-006: required ≠ scene
                    // presence, but it is the optionality signal we have).
                    optional: !schema.is_required(key),
                    flatten: false,
                });
            }
        }
        TypeNode::new(NodeKind::Object {
            fields,
            deny_unknown_fields: true,
        })
    }

    /// `TupleStruct`/`Tuple` → a fixed-arity [`NodeKind::Tuple`] (auto-attaches
    /// the `RonKind::Tuple` extension). Elements come from `prefixItems`.
    fn map_tuple(&self, type_path: &str, schema: &ReflectSchema) -> TypeNode {
        let elements = schema
            .raw
            .get("prefixItems")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .enumerate()
                    .map(|(i, v)| element_ref(v, &format!("{type_path}[{i}]")))
                    .collect()
            })
            .unwrap_or_default();
        TypeNode::tuple(elements)
    }

    /// `Enum` → [`NodeKind::Enum`] of externally-tagged variants (the serde /
    /// Bevy default). Variants come from `oneOf`; an arm with no recognizable
    /// shape is skipped with a diagnostic rather than aborting the type.
    fn map_enum(
        &self,
        type_path: &str,
        schema: &ReflectSchema,
        diagnostics: &mut Vec<AcquisitionDiagnostic>,
    ) -> TypeNode {
        let mut variants = Vec::new();
        if let Some(arms) = schema.raw.get("oneOf").and_then(Value::as_array) {
            for (i, arm) in arms.iter().enumerate() {
                match parse_variant(arm, &format!("{type_path}::variant{i}")) {
                    Some(variant) => variants.push(variant),
                    None => diagnostics.push(diag(
                        DiagnosticCategory::UnsupportedConstruct,
                        type_path,
                        format!("enum arm #{i} has no recognizable variant shape; skipped"),
                        &self.id,
                        &self.label,
                        Some(type_path.to_string()),
                    )),
                }
            }
        }
        TypeNode::new(NodeKind::Enum {
            variants,
            discriminator: Discriminator::External,
        })
    }

    /// `List`/`Array`/`Set` → [`NodeKind::Sequence`]. The element type is read
    /// from `items` (then `valueType` / `type` as fallbacks); absent ⇒ a sequence
    /// of `unknown`.
    fn map_sequence(&self, type_path: &str, schema: &ReflectSchema) -> TypeNode {
        let element = element_schema(&schema.raw)
            .map(|v| element_ref(v, &format!("{type_path}[]")))
            .unwrap_or_else(|| TypeRef::inline(TypeNode::unknown()));
        TypeNode::new(NodeKind::Sequence { element })
    }

    /// `Map` → [`NodeKind::Map`]. The value type is read from `additionalProperties`
    /// / `valueType`; the key type from `keyType` (defaulting to string for an
    /// ordinary JSON-object map). A non-string key attaches the
    /// `RonKind::NonStringKeyMap` extension.
    fn map_map(&self, type_path: &str, schema: &ReflectSchema) -> TypeNode {
        let value = map_value_schema(&schema.raw)
            .map(|v| element_ref(v, &format!("{type_path}.<value>")))
            .unwrap_or_else(|| TypeRef::inline(TypeNode::unknown()));

        match schema.raw.get("keyType") {
            Some(key_schema) => {
                let key = element_ref(key_schema, &format!("{type_path}.<key>"));
                // A non-trivially-string key is a non-string-key map.
                if is_string_key(key_schema) {
                    TypeNode::new(NodeKind::Map { key, value })
                } else {
                    TypeNode::non_string_key_map(key, value)
                }
            }
            None => TypeNode::new(NodeKind::Map {
                key: TypeRef::inline(TypeNode::primitive(Primitive::String)),
                value,
            }),
        }
    }
}

// ===========================================================================
// Reflect-schema parsing helpers
// ===========================================================================

/// Parse one reflect schema object into a [`ReflectSchema`] (version-tolerant:
/// unknown fields ignored).
fn parse_reflect_schema(obj: &Map<String, Value>) -> ReflectSchema {
    // `kind` is the reflect kind; missing/non-string degrades to `Value` (a leaf
    // scalar best-effort) rather than aborting — but record the raw so structural
    // keywords can still inform the mapping.
    let kind = obj
        .get("kind")
        .and_then(Value::as_str)
        .map_or_else(|| ReflectKind::infer_from_keywords(obj), ReflectKind::parse);

    let required = obj
        .get("required")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default();

    let reflect_types = obj
        .get("reflectTypes")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default();

    // The concrete default is carried only by a defaults-carrying export, under a
    // conventional key. Either is accepted; absence ⇒ non-elidable (FR-014).
    let default_value = obj
        .get("default")
        .or_else(|| obj.get("defaultValue"))
        .cloned();

    ReflectSchema {
        kind,
        required,
        reflect_types,
        default_value,
        raw: obj.clone(),
    }
}

impl ReflectKind {
    /// Infer a reflect kind from structural keywords when `kind` is absent — a
    /// version-tolerant best effort, never fatal.
    #[must_use]
    fn infer_from_keywords(obj: &Map<String, Value>) -> Self {
        if obj.contains_key("properties") {
            ReflectKind::Struct
        } else if obj.contains_key("oneOf") {
            ReflectKind::Enum
        } else if obj.contains_key("prefixItems") {
            ReflectKind::Tuple
        } else if obj.contains_key("items") {
            ReflectKind::List
        } else if obj.contains_key("additionalProperties") || obj.contains_key("valueType") {
            ReflectKind::Map
        } else {
            ReflectKind::Value
        }
    }
}

/// Lower an enum `oneOf` arm into a [`Variant`], or `None` if it has no
/// recognizable shape. Mirrors the JSON-Schema core's variant lowering.
fn parse_variant(arm: &Value, subject: &str) -> Option<Variant> {
    let obj = arm.as_object()?;

    // Unit variant as a short-path / name string: `{"shortPath":"Name"}` or
    // `{"const":"Name"}` or `{"enum":["Name"]}`.
    if let Some(Value::String(name)) = obj.get("const") {
        return Some(unit_variant(name));
    }
    if let Some([Value::String(name)]) =
        obj.get("enum").and_then(Value::as_array).map(Vec::as_slice)
    {
        return Some(unit_variant(name));
    }

    // Bevy reflect enum arms carry a `kind` + the variant name (`shortPath`).
    if let Some(name) = obj.get("shortPath").and_then(Value::as_str) {
        let shape = match obj.get("kind").and_then(Value::as_str) {
            Some("Struct") => VariantShape::Struct(fields_from(obj, subject)),
            Some("Tuple") => VariantShape::Tuple(prefix_refs(obj, subject)),
            _ => VariantShape::Unit,
        };
        return Some(Variant {
            serialized_name: name.to_string(),
            shape,
        });
    }

    // Externally-tagged single-property object: `{"properties":{"Name":<payload>}}`.
    if let Some(props) = obj.get("properties").and_then(Value::as_object) {
        if props.len() == 1 {
            let (name, payload) = props.iter().next().expect("len == 1");
            let shape = variant_payload_shape(payload, &format!("{subject}.{name}"));
            return Some(Variant {
                serialized_name: name.clone(),
                shape,
            });
        }
    }

    None
}

/// A unit variant with the given serialized name.
#[inline]
fn unit_variant(name: &str) -> Variant {
    Variant {
        serialized_name: name.to_string(),
        shape: VariantShape::Unit,
    }
}

/// Lower the payload schema of an externally-tagged variant into its shape.
fn variant_payload_shape(payload: &Value, subject: &str) -> VariantShape {
    if let Some(obj) = payload.as_object() {
        if obj.contains_key("properties") {
            return VariantShape::Struct(fields_from(obj, subject));
        }
        if obj.contains_key("prefixItems") {
            return VariantShape::Tuple(prefix_refs(obj, subject));
        }
    }
    VariantShape::Newtype(element_ref(payload, subject))
}

/// Lower an object's `properties`/`required` into ordered [`Field`]s.
fn fields_from(obj: &Map<String, Value>, subject: &str) -> Vec<Field> {
    let required: BTreeSet<&str> = obj
        .get("required")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let mut fields = Vec::new();
    if let Some(props) = obj.get("properties").and_then(Value::as_object) {
        for (key, prop) in props {
            fields.push(Field {
                serialized_key: key.clone(),
                value: property_ref(prop, &format!("{subject}.{key}")),
                optional: !required.contains(key.as_str()),
                flatten: false,
            });
        }
    }
    fields
}

/// Lower an object's `prefixItems` into ordered element [`TypeRef`]s.
fn prefix_refs(obj: &Map<String, Value>, subject: &str) -> Vec<TypeRef> {
    obj.get("prefixItems")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .enumerate()
                .map(|(i, v)| element_ref(v, &format!("{subject}[{i}]")))
                .collect()
        })
        .unwrap_or_default()
}

// ===========================================================================
// Reference / scalar lowering
// ===========================================================================

/// Lower a struct/tuple **property** schema into a [`TypeRef`]. A `$ref` /
/// `typePath` to another registered type becomes a [`TypeRef::Named`]; otherwise
/// the property is lowered as an inline scalar/sequence/unknown node.
fn property_ref(value: &Value, subject: &str) -> TypeRef {
    element_ref(value, subject)
}

/// Lower an element schema into a [`TypeRef`]: a `$ref` / `type`-path reference to
/// a registered type → [`TypeRef::Named`] (the registry type path); a recognized
/// scalar → an inline primitive; anything else → an inline `unknown`.
fn element_ref(value: &Value, subject: &str) -> TypeRef {
    if let Some(obj) = value.as_object() {
        if let Some(name) = ref_target(obj) {
            return TypeRef::named(name);
        }
        if let Some(prim) = scalar_primitive(obj) {
            return TypeRef::inline(TypeNode::primitive(prim));
        }
    }
    // A bare string element is treated as a type-path reference (some exports
    // inline a `$ref` value as a plain string).
    if let Some(s) = value.as_str() {
        if !s.is_empty() {
            return TypeRef::named(last_path_segment(s));
        }
    }
    let _ = subject;
    TypeRef::inline(TypeNode::unknown())
}

/// Map a leaf `Value` reflect schema to a primitive node (best-effort), else an
/// `unknown` node. Bevy reflect `Value` kinds are opaque scalars; the `type`
/// keyword (when the export carries it) refines the primitive.
fn map_value_scalar(schema: &ReflectSchema) -> TypeNode {
    scalar_primitive(&schema.raw)
        .map(TypeNode::primitive)
        .unwrap_or_else(TypeNode::unknown)
}

/// The `$defs`/`$ref` target name of a reference schema, if any. Recognizes the
/// JSON-Schema `$ref` form and Bevy's `typePath`/`type` reference fields.
fn ref_target(obj: &Map<String, Value>) -> Option<String> {
    if let Some(reference) = obj.get("$ref").and_then(Value::as_str) {
        return Some(last_path_segment(reference));
    }
    // Bevy reflect property refs can carry the target type path directly.
    for key in ["typePath", "type"] {
        if let Some(path) = obj.get(key).and_then(Value::as_str) {
            // A scalar JSON `type` keyword (`"string"`, …) is NOT a type-path ref.
            if key == "type" && scalar_keyword(path).is_some() {
                return None;
            }
            if !path.is_empty() {
                return Some(last_path_segment(path));
            }
        }
    }
    None
}

/// The element schema of a sequence: `items`, then `valueType`/`type` fallbacks.
fn element_schema(obj: &Map<String, Value>) -> Option<&Value> {
    obj.get("items")
        .filter(|v| !matches!(v, Value::Bool(_)))
        .or_else(|| obj.get("valueType"))
}

/// The value schema of a map: `additionalProperties` (a schema, not a bool),
/// then `valueType`.
fn map_value_schema(obj: &Map<String, Value>) -> Option<&Value> {
    obj.get("additionalProperties")
        .filter(|v| !matches!(v, Value::Bool(_)))
        .or_else(|| obj.get("valueType"))
}

/// `true` if a map key schema denotes a plain string key (an ordinary JSON-object
/// map). A `$ref`/non-string key is a non-string-key map.
fn is_string_key(value: &Value) -> bool {
    value
        .as_object()
        .and_then(|o| o.get("type"))
        .and_then(Value::as_str)
        == Some("string")
}

/// The primitive a scalar object schema denotes, if any (`type` keyword).
fn scalar_primitive(obj: &Map<String, Value>) -> Option<Primitive> {
    obj.get("type")
        .and_then(Value::as_str)
        .and_then(scalar_keyword)
}

/// Map a single JSON Schema scalar `type` keyword to a [`Primitive`].
fn scalar_keyword(name: &str) -> Option<Primitive> {
    match name {
        "boolean" => Some(Primitive::Boolean),
        "integer" | "uint" | "int" => Some(Primitive::Integer),
        "number" | "float" => Some(Primitive::Number),
        "string" => Some(Primitive::String),
        "null" => Some(Primitive::Null),
        _ => None,
    }
}

// ===========================================================================
// Root / wrapper / version helpers
// ===========================================================================

/// Conventional wrapper keys under which a registry export may nest its type map.
const TYPE_MAP_KEYS: [&str; 3] = ["$defs", "types", "schemas"];

/// Locate the type-path → schema map within a registry root.
///
/// If the root carries one of the conventional wrapper keys ([`TYPE_MAP_KEYS`])
/// whose value is an object, that nested object is the type map (extra sibling
/// metadata is ignored). Otherwise the root itself is treated as the type map.
fn locate_type_map(root: &Map<String, Value>) -> &Map<String, Value> {
    for key in TYPE_MAP_KEYS {
        if let Some(nested) = root.get(key).and_then(Value::as_object) {
            return nested;
        }
    }
    root
}

/// `true` for keys that carry the export's apparent Bevy version (version-tolerant).
fn is_version_key(key: &str) -> bool {
    matches!(
        key,
        "bevyVersion" | "bevy_version" | "version" | "apparentVersion" | "apparent_version"
    )
}

/// The trailing path segment of a `/`- or `::`-delimited reference, falling back
/// to the whole string. Used to map a `$ref`/type path to its registered name.
fn last_path_segment(reference: &str) -> String {
    let after_slash = reference.rsplit('/').next().unwrap_or(reference);
    // A registered type is keyed by its full path; only strip the JSON-pointer
    // `#/$defs/` style prefix (the `/` split), keeping `::`-qualified paths whole.
    after_slash.to_string()
}

/// Build a provenance-tagged [`AcquisitionDiagnostic`].
fn diag(
    category: DiagnosticCategory,
    subject: impl Into<String>,
    detail: impl Into<String>,
    source_id: &str,
    location: &str,
    pointer: Option<String>,
) -> AcquisitionDiagnostic {
    AcquisitionDiagnostic::new(category, subject, detail)
        .with_source_id(source_id.to_string())
        .with_location(DiagnosticLocation {
            source: Some(location.to_string()),
            pointer,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn precedence_and_id_are_bevy() {
        let src = BevySource::from_schema_json("{}");
        assert_eq!(src.precedence(), SourcePrecedence::Bevy);
        assert_eq!(src.source_id(), "bevy-registry");
    }

    #[test]
    fn struct_maps_to_object_with_optional_fields() {
        let json = json!({
            "my_crate::Foo": {
                "kind": "Struct",
                "properties": {
                    "x": { "type": "number" },
                    "label": { "type": "string" }
                },
                "required": ["x"],
                "additionalProperties": false,
                "reflectTypes": ["Default", "Component"]
            }
        });
        let acq = BevySource::from_schema_value(json).acquire();
        let foo = acq.model.lookup("my_crate::Foo").unwrap();
        let NodeKind::Object {
            fields,
            deny_unknown_fields,
        } = &foo.kind
        else {
            panic!("struct → object");
        };
        assert!(deny_unknown_fields, "reflect structs deny unknown fields");
        let x = fields.iter().find(|f| f.serialized_key == "x").unwrap();
        assert!(!x.optional, "x is reflect-required");
        let label = fields.iter().find(|f| f.serialized_key == "label").unwrap();
        assert!(label.optional, "label is not required");
        assert!(acq.diagnostics.is_empty());
    }

    #[test]
    fn unknown_kind_degrades_to_unknown_node_with_diagnostic() {
        let json = json!({
            "my_crate::Weird": { "kind": "SomethingNew" }
        });
        let acq = BevySource::from_schema_value(json).acquire();
        assert!(acq.model.lookup("my_crate::Weird").unwrap().is_unknown());
        assert!(acq
            .diagnostics
            .iter()
            .any(|d| d.category == DiagnosticCategory::UnsupportedConstruct));
    }

    #[test]
    fn malformed_json_never_panics() {
        let acq = BevySource::from_schema_json("{ not json").acquire();
        assert!(acq.model.is_empty());
        assert!(!acq.diagnostics.is_empty());
    }

    #[test]
    fn registry_accessors_report_defaults() {
        let json = json!({
            "my_crate::Bar": {
                "kind": "Struct",
                "properties": {},
                "reflectTypes": ["Default"],
                "default": { "x": 0.0 }
            },
            "my_crate::Baz": {
                "kind": "Struct",
                "properties": {},
                "reflectTypes": []
            }
        });
        let (registry, diags) = BevyRegistry::from_schema_value(&json, "t", "<t>");
        assert!(diags.is_empty());
        assert!(registry.contains("my_crate::Bar"));
        assert!(registry.is_default_reflected("my_crate::Bar"));
        assert!(registry.default_value("my_crate::Bar").is_some());
        assert!(!registry.is_default_reflected("my_crate::Baz"));
        assert!(registry.default_value("my_crate::Baz").is_none());
        assert!(!registry.contains("my_crate::Nope"));
    }
}
