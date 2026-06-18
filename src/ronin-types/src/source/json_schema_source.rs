//! User-supplied general JSON Schema 2020-12 acquisition {TR-008}.
//!
//! [`JsonSchemaSource`] is the highest-precedence [`TypeSource`]: it ingests a
//! *general* user-authored JSON Schema 2020-12 document — one not necessarily
//! produced by `schemars` and not necessarily using RONin's own `x-ron-*`
//! interchange shape. A user schema is the most authoritative description of a
//! type a host can provide, so it wins over both `schemars` and `syn` at merge
//! time (TR-010).
//!
//! Ingestion shares the exact mapping core
//! ([`json_schema_ingest`](crate::source::json_schema_ingest)) with
//! [`SchemarsSource`](crate::source::SchemarsSource), so the two adapters lower
//! identical standard-2020-12 constructs identically (SC-004) and differ only in
//! their declared [`precedence`](TypeSource::precedence). Unsupported or
//! unrecognized constructs become `unknown` nodes plus an
//! [`DiagnosticCategory::UnsupportedConstruct`](crate::diagnostics::DiagnosticCategory::UnsupportedConstruct)
//! diagnostic — acquisition never errors (TR-006, TR-011).

use serde_json::Value;

use crate::source::json_schema_ingest;
use crate::source::{Acquired, SourcePrecedence, TypeSource};

/// A [`TypeSource`] over a general user-supplied JSON Schema 2020-12 document
/// (TR-008).
///
/// Build it from a parsed [`serde_json::Value`] ([`JsonSchemaSource::from_value`]),
/// a JSON string ([`JsonSchemaSource::from_json_str`]), or a `.json` file on disk
/// ([`JsonSchemaSource::from_path`]). Its merge precedence is
/// [`SourcePrecedence::UserSchema`] — the highest in the total order.
#[derive(Debug, Clone)]
pub struct JsonSchemaSource {
    /// Stable id for provenance/conflict diagnostics (`"user-schema[:<label>]"`).
    id: String,
    /// A human-readable origin label for diagnostic locations.
    label: String,
    /// The schema payload, either a parsed value or raw JSON text to parse.
    payload: Payload,
}

/// The ingestible payload of a [`JsonSchemaSource`].
#[derive(Debug, Clone)]
enum Payload {
    /// An already-parsed schema document.
    Value(Box<Value>),
    /// Raw JSON text (parsed at `acquire` time; invalid JSON → a diagnostic).
    Text(String),
}

impl JsonSchemaSource {
    /// Build a source from an already-parsed user schema [`Value`].
    #[must_use]
    pub fn from_value(schema: Value) -> Self {
        Self {
            id: "user-schema".to_string(),
            label: "<user-schema-value>".to_string(),
            payload: Payload::Value(Box::new(schema)),
        }
    }

    /// Build a source from an already-parsed schema [`Value`], tagging it with an
    /// explicit label used in the source id (`"user-schema:<label>"`) and as the
    /// diagnostic location.
    #[must_use]
    pub fn from_named_value(label: impl Into<String>, schema: Value) -> Self {
        let label = label.into();
        Self {
            id: format!("user-schema:{label}"),
            label,
            payload: Payload::Value(Box::new(schema)),
        }
    }

    /// Build a source from a JSON string of a user schema.
    #[must_use]
    pub fn from_json_str(json: impl Into<String>) -> Self {
        Self {
            id: "user-schema".to_string(),
            label: "<user-schema-json>".to_string(),
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
            id: format!("user-schema:{label}"),
            label,
            payload: Payload::Text(text),
        }
    }
}

impl TypeSource for JsonSchemaSource {
    fn source_id(&self) -> String {
        self.id.clone()
    }

    fn precedence(&self) -> SourcePrecedence {
        SourcePrecedence::UserSchema
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
    use crate::diagnostics::DiagnosticCategory;
    use crate::model::NodeKind;
    use serde_json::json;

    #[test]
    fn precedence_is_user_schema() {
        let src = JsonSchemaSource::from_value(json!({}));
        assert_eq!(src.precedence(), SourcePrecedence::UserSchema);
        assert_eq!(src.source_id(), "user-schema");
    }

    #[test]
    fn ingests_general_object_schema() {
        let schema = json!({
            "title": "Config",
            "type": "object",
            "properties": {
                "host": { "type": "string" },
                "port": { "type": "integer" },
                "tags": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["host", "port"]
        });
        let acq = JsonSchemaSource::from_value(schema).acquire();
        let cfg = acq.model.lookup("Config").unwrap();
        let NodeKind::Object { fields, .. } = &cfg.kind else {
            panic!("object");
        };
        assert_eq!(fields.len(), 3);
        let port = fields.iter().find(|f| f.serialized_key == "port").unwrap();
        assert!(!port.optional, "port is required");
        let tags = fields.iter().find(|f| f.serialized_key == "tags").unwrap();
        assert!(tags.optional, "tags absent from required");
        let tags_node = acq.model.resolve(&tags.value).unwrap();
        assert!(matches!(tags_node.kind, NodeKind::Sequence { .. }));
    }

    #[test]
    fn uses_definitions_keyword_too() {
        // Draft pre-2020 used `definitions`; the ingester accepts both.
        let schema = json!({
            "title": "Outer",
            "type": "object",
            "properties": { "inner": { "$ref": "#/definitions/Inner" } },
            "definitions": { "Inner": { "type": "object", "properties": { "v": { "type": "integer" } } } }
        });
        let acq = JsonSchemaSource::from_value(schema).acquire();
        assert!(acq.model.contains("Inner"));
        let outer = acq.model.lookup("Outer").unwrap();
        let NodeKind::Object { fields, .. } = &outer.kind else {
            panic!("object");
        };
        assert_eq!(fields[0].value.as_named(), Some("Inner"));
    }

    #[test]
    fn unsupported_construct_becomes_unknown_with_diagnostic() {
        let schema = json!({
            "title": "Multi",
            "type": ["integer", "string", "boolean"]
        });
        let acq = JsonSchemaSource::from_value(schema).acquire();
        assert!(acq.model.lookup("Multi").unwrap().is_unknown());
        assert!(acq
            .diagnostics
            .iter()
            .any(|d| d.category == DiagnosticCategory::UnsupportedConstruct));
    }

    #[test]
    fn invalid_json_never_fails() {
        let acq = JsonSchemaSource::from_json_str("not json at all {{{").acquire();
        assert!(acq.model.is_empty());
        assert!(!acq.diagnostics.is_empty());
    }
}
