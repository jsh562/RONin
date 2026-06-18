//! `schemars`-derived JSON Schema acquisition {TR-007}.
//!
//! [`SchemarsSource`] is the middle-precedence [`TypeSource`]: it **ingests** a
//! JSON Schema document that `schemars` *already produced* (e.g. via
//! `schemars::schema_for!(T)`) and normalizes it into the [`TypeModel`]. It does
//! **not** run the `schemars` derive itself (HINT-002) — derives need the Rust
//! type in scope at compile time; this adapter consumes the schema artifact, so
//! a host can derive once and hand RONin the resulting schema.
//!
//! # schemars 1.x conventions recognized
//!
//! - **Root + `$defs`.** The root schema *is* a named type (named by its
//!   `title`, which schemars sets to the Rust type name); referenced types live
//!   under `$defs` and are reached via `{"$ref":"#/$defs/Name"}`.
//! - **Enums.** All-unit enums become a top-level `{"type":"string","enum":[…]}`;
//!   data-carrying enums become a `oneOf` of externally-tagged variant objects
//!   (`{"type":"object","properties":{"Variant":<payload>},"required":["Variant"]}`).
//! - **`Option<T>`.** Emitted as a `{"type":["T","null"]}` union (or a `oneOf`
//!   with a `null` arm).
//! - **Tuples.** Emitted as `{"type":"array","prefixItems":[…],"minItems":N,"maxItems":N}`.
//! - **Maps.** Emitted as `{"type":"object","additionalProperties":<value>}`.
//!
//! All of the above are lowered by the shared
//! [`json_schema_ingest`](crate::source::json_schema_ingest) core, so this
//! adapter is a thin precedence/identity wrapper. Unsupported constructs degrade
//! to `unknown` nodes plus diagnostics — acquisition never fails (TR-006,
//! TR-011).

use serde_json::Value;

use crate::source::json_schema_ingest;
use crate::source::{Acquired, SourcePrecedence, TypeSource};

/// A [`TypeSource`] over a `schemars`-derived JSON Schema document (TR-007).
///
/// Build it from a parsed [`serde_json::Value`] ([`SchemarsSource::from_value`]),
/// a JSON string ([`SchemarsSource::from_json_str`]), or a `.json` file on disk
/// ([`SchemarsSource::from_path`]). Its merge [`precedence`](TypeSource::precedence)
/// is [`SourcePrecedence::Schemars`] — above `syn`, below user-supplied schemas.
#[derive(Debug, Clone)]
pub struct SchemarsSource {
    /// Stable id for provenance/conflict diagnostics (`"schemars[:<label>]"`).
    id: String,
    /// A human-readable origin label for diagnostic locations.
    label: String,
    /// The schema payload, either a parsed value or raw JSON text to parse.
    payload: Payload,
}

/// The ingestible payload of a [`SchemarsSource`].
#[derive(Debug, Clone)]
enum Payload {
    /// An already-parsed schema document.
    Value(Box<Value>),
    /// Raw JSON text (parsed at `acquire` time; invalid JSON → a diagnostic).
    Text(String),
}

impl SchemarsSource {
    /// Build a source from an already-parsed schemars schema [`Value`].
    #[must_use]
    pub fn from_value(schema: Value) -> Self {
        Self {
            id: "schemars".to_string(),
            label: "<schemars-value>".to_string(),
            payload: Payload::Value(Box::new(schema)),
        }
    }

    /// Build a source from an already-parsed schema [`Value`], tagging it with an
    /// explicit label used in the source id (`"schemars:<label>"`) and as the
    /// diagnostic location.
    #[must_use]
    pub fn from_named_value(label: impl Into<String>, schema: Value) -> Self {
        let label = label.into();
        Self {
            id: format!("schemars:{label}"),
            label,
            payload: Payload::Value(Box::new(schema)),
        }
    }

    /// Build a source from a JSON string of a schemars-derived schema.
    #[must_use]
    pub fn from_json_str(json: impl Into<String>) -> Self {
        Self {
            id: "schemars".to_string(),
            label: "<schemars-json>".to_string(),
            payload: Payload::Text(json.into()),
        }
    }

    /// Build a source from a `.json` schema file on disk.
    ///
    /// A read failure is captured as JSON text that fails to parse, so `acquire`
    /// emits a diagnostic rather than failing — the never-fail contract (TR-011).
    #[must_use]
    pub fn from_path(path: impl AsRef<std::path::Path>) -> Self {
        let path = path.as_ref();
        let label = path.display().to_string();
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|err| format!("// ronin-types: failed to read schema file: {err}"));
        Self {
            id: format!("schemars:{label}"),
            label,
            payload: Payload::Text(text),
        }
    }
}

impl TypeSource for SchemarsSource {
    fn source_id(&self) -> String {
        self.id.clone()
    }

    fn precedence(&self) -> SourcePrecedence {
        SourcePrecedence::Schemars
    }

    fn acquire(&self) -> Acquired {
        let (model, diagnostics) = match &self.payload {
            Payload::Value(value) => json_schema_ingest::ingest_value(value, &self.id, &self.label),
            Payload::Text(text) => json_schema_ingest::ingest_str(text, &self.id, &self.label),
        };
        Acquired { model, diagnostics }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NodeKind, VariantShape};
    use serde_json::json;

    /// A representative schemars 1.x struct schema (root + `$defs` + Option +
    /// tuple + map), exactly as `schemars::schema_for!` emits it.
    fn schemars_demo() -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "Demo",
            "type": "object",
            "properties": {
                "count": { "type": "integer", "format": "uint32", "minimum": 0 },
                "inner": { "$ref": "#/$defs/Inner" },
                "items": { "type": "array", "items": { "type": "integer", "format": "int32" } },
                "map": { "type": "object", "additionalProperties": { "type": "integer" } },
                "maybe": { "type": ["string", "null"] },
                "name": { "type": "string" },
                "pair": {
                    "type": "array", "maxItems": 2, "minItems": 2,
                    "prefixItems": [ { "type": "integer" }, { "type": "string" } ]
                }
            },
            "required": ["name", "count", "items", "pair", "inner", "map"],
            "$defs": {
                "Inner": { "type": "object", "properties": { "v": { "type": "integer" } }, "required": ["v"] }
            }
        })
    }

    #[test]
    fn precedence_is_schemars() {
        let src = SchemarsSource::from_value(json!({}));
        assert_eq!(src.precedence(), SourcePrecedence::Schemars);
        assert_eq!(src.source_id(), "schemars");
    }

    #[test]
    fn ingests_root_and_defs() {
        let acq = SchemarsSource::from_value(schemars_demo()).acquire();
        assert!(acq.model.contains("Demo"));
        assert!(acq.model.contains("Inner"));

        let demo = acq.model.lookup("Demo").unwrap();
        let NodeKind::Object { fields, .. } = &demo.kind else {
            panic!("Demo is an object");
        };
        // `inner` resolves to the named $defs entry.
        let inner_field = fields.iter().find(|f| f.serialized_key == "inner").unwrap();
        assert_eq!(inner_field.value.as_named(), Some("Inner"));
        // `maybe` is an Option and therefore optional.
        let maybe = fields.iter().find(|f| f.serialized_key == "maybe").unwrap();
        assert!(maybe.optional);
        let maybe_node = acq.model.resolve(&maybe.value).unwrap();
        assert!(matches!(maybe_node.kind, NodeKind::Option { .. }));
    }

    #[test]
    fn data_enum_oneof_becomes_variants() {
        let schema = json!({
            "title": "Shape",
            "oneOf": [
                { "type": "string", "enum": ["Empty"] },
                { "type": "object", "properties": { "Circle": { "type": "number" } }, "required": ["Circle"], "additionalProperties": false },
                { "type": "object", "properties": { "Rect": { "type": "array", "prefixItems": [ { "type": "number" }, { "type": "number" } ] } }, "required": ["Rect"], "additionalProperties": false },
                { "type": "object", "properties": { "Named": { "type": "object", "properties": { "w": { "type": "integer" }, "h": { "type": "integer" } }, "required": ["w", "h"] } }, "required": ["Named"], "additionalProperties": false }
            ]
        });
        let acq = SchemarsSource::from_value(schema).acquire();
        let shape = acq.model.lookup("Shape").unwrap();
        let NodeKind::Enum { variants, .. } = &shape.kind else {
            panic!("enum");
        };
        assert_eq!(variants.len(), 4);
        assert!(matches!(variants[0].shape, VariantShape::Unit));
        assert!(matches!(variants[1].shape, VariantShape::Newtype(_)));
        assert!(matches!(variants[2].shape, VariantShape::Tuple(_)));
        assert!(matches!(variants[3].shape, VariantShape::Struct(_)));
    }

    #[test]
    fn from_json_str_parses() {
        let acq = SchemarsSource::from_json_str(schemars_demo().to_string()).acquire();
        assert!(acq.model.contains("Demo"));
    }

    #[test]
    fn invalid_json_never_fails() {
        let acq = SchemarsSource::from_json_str("{ broken").acquire();
        assert!(acq.model.is_empty());
        assert!(!acq.diagnostics.is_empty());
    }
}
