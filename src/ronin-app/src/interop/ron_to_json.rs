//! RON→JSON value mapping + the FR-015 emit conventions (FR-001/015).
//!
//! The value map is **reused, not reinvented**: `ronin-validate`'s
//! [`CstJsonProjection`] already encodes the RON CST value into `serde_json`
//! (tuple→array, list→array, unit `()`→null, char→one-char string, enum
//! `Some(x)`→x / every other variant → external-tag `{"<Variant>": payload}`,
//! HINT-002). E010 takes that `instance` and applies two deliberate refinements
//! the projection does not (FR-015):
//!
//! 1. **Non-string map keys → CANONICAL RON literals.** The projection stringifies
//!    a non-string key from its *verbatim* source text (e.g. `(1,  2)` keeps the
//!    source spacing); E010 re-keys it to the canonical RON literal `"(1, 2)"` so
//!    JSON→RON re-parses the typed key deterministically (HINT-002, FR-015). The
//!    key's source type is recorded as a loss `encoding_note`.
//! 2. **Enum tagging by the bound type.** When a `TypeModel` is bound, enum
//!    variants are emitted via the type's recorded serde tagging
//!    (external/internal/adjacent/untagged, E004 TR-005); unbound, the external
//!    tagging the projection already produces is the deterministic default
//!    (FR-015).
//!
//! The returned value is paired with the [`LossReport`] (the lossy-construct map,
//! T009) and the [`CommentCarrier`] (T011) so the caller drives the loss dialog,
//! the inline diagnostics, and the comment emission from one conversion.
//!
//! The serde `ron` crate is used here ONLY as the boundary **grammar verifier /
//! round-trip cross-check** ([`grammar_verify`]) — never as the primary converter
//! (ADR-0008 / FR-012).

use ronin_core::syntax::ast::{Document, Map, Value};
use ronin_core::{CstDocument, SyntaxKind};
use ronin_types::model::{Discriminator, NodeKind, TypeModel};
use ronin_validate::CstJsonProjection;

use crate::interop::comments::{CommentCarrier, CommentMode};
use crate::interop::loss::{build_loss_report, LossReport};

/// The type information RON→JSON consults for the FR-015 emit conventions
/// (enum tagging) — the bound `TypeModel` plus the document's root type name.
///
/// `TypeModel` is consulted strictly as **data** (ADR-0004); E010 never owns,
/// mutates, or persists it. When `None` is passed to [`ron_to_json`], the
/// converter uses the deterministic unbound defaults (external enum tagging,
/// canonical-literal keys) (FR-015).
#[derive(Debug, Clone, Copy)]
pub struct RonToJsonBinding<'a> {
    /// The bound type model (consulted as data).
    pub model: &'a TypeModel,
    /// The document's bound root type name (a key into
    /// [`TypeModel::named_types`]).
    pub root_type: &'a str,
}

impl<'a> RonToJsonBinding<'a> {
    /// Build a binding view from a `TypeModel` and a root type name.
    #[must_use]
    pub fn new(model: &'a TypeModel, root_type: &'a str) -> Self {
        Self { model, root_type }
    }
}

/// The complete RON→JSON conversion outcome (FR-001/004/008/015): the JSON value,
/// the lossy-construct map, and the comment carrier.
///
/// All three are built **read-only** over the source CST (data-model
/// §ConversionResult "Built read-only; commit is the only writer"). The caller
/// pretty-prints the value with the configured indent, renders the loss report +
/// inline diagnostics, and emits the carrier's comments (JSONC inline or sidecar).
#[derive(Debug, Clone)]
pub struct RonToJson {
    /// The projected JSON value with the FR-015 emit conventions applied.
    pub value: serde_json::Value,
    /// The lossy-construct map for this conversion (drives the loss dialog AND
    /// inline diagnostics, FR-004/007).
    pub loss_report: LossReport,
    /// The comment carrier (JSONC inline / sidecar / none) for this conversion
    /// (FR-008).
    pub comments: CommentCarrier,
}

/// Convert a parsed RON document to a JSON value with the FR-015 emit conventions,
/// the lossy-construct map, and the comment carrier (FR-001/004/008/015).
///
/// * `doc` — the source RON document (read-only).
/// * `binding` — the bound `TypeModel` view for schema-aware enum tagging, or
///   `None` for the deterministic unbound defaults (external tagging) (FR-015).
/// * `comment_mode` — how comments are carried: JSONC inline (default), sidecar
///   (strict fallback), or none (pure standard JSON, comments reported as losses)
///   (FR-008). The caller resolves this from
///   [`ConversionSettings`](crate::settings::ConversionSettings) + any
///   per-conversion override.
///
/// Returns the JSON value plus the loss report and comment carrier. The JSON value
/// reuses the projection's `instance` with non-string keys re-keyed to canonical
/// RON literals and enum tagging applied per the bound type (FR-015). The walk is
/// read-only over the CST; no source byte is changed (data-model §ConversionResult).
#[must_use]
pub fn ron_to_json(
    doc: &CstDocument,
    binding: Option<RonToJsonBinding<'_>>,
    comment_mode: CommentMode,
) -> RonToJson {
    // 1. Reuse the projection's value map + its Pointer→TextRange index (HINT-002).
    let projection = CstJsonProjection::from_document(doc);
    let mut value = projection.instance;

    // 2. Apply the FR-015 emit conventions the projection does not:
    //    - non-string keys → canonical RON literals;
    //    - enum tagging per the bound type's recorded serde tagging.
    if let Some(root) = Document::cast(doc.root()).and_then(|d| d.value()) {
        let bound_root = binding.and_then(|b| b.model.lookup(b.root_type).map(|n| (b.model, n)));
        apply_emit_conventions(&mut value, &root, bound_root);
    }

    // 3. The comment carrier (T011) and the loss-construct map (T009).
    let comments = CommentCarrier::from_document(doc, comment_mode);
    let loss_report = build_loss_report(doc, &projection.index, &comments, binding.is_some());

    RonToJson {
        value,
        loss_report,
        comments,
    }
}

/// Apply the FR-015 emit conventions in place over the projected JSON value,
/// re-walking the CST in lockstep so each JSON node is matched to its source value.
///
/// `bound` is the resolved `(model, node)` for the current value position, or
/// `None` when unbound / unresolved (best-effort defaults apply).
fn apply_emit_conventions(
    json: &mut serde_json::Value,
    value: &Value,
    bound: Option<(&TypeModel, &ronin_types::model::TypeNode)>,
) {
    // Enum tagging is driven by the BOUND TYPE, not the RON surface form: a serde
    // enum variant can be authored as a bare ident, a named tuple, or a named
    // struct, all of which the projection external-tags `{"<Variant>": payload}`.
    // When the bound node is an enum, rewrite the tagging here (FR-015) before the
    // surface-form match below.
    if let Some((_model, node)) = bound {
        if matches!(node.kind, NodeKind::Enum { .. }) {
            apply_enum_tagging(json, bound);
            return;
        }
    }
    match value {
        Value::Map(m) => apply_map_conventions(json, m, bound),
        Value::Struct(s) => {
            if let serde_json::Value::Object(obj) = json {
                for field in s.fields() {
                    let Some(name_tok) = field.name() else {
                        continue;
                    };
                    let key = name_tok.text();
                    if let (Some(child_json), Some(child_val)) = (obj.get_mut(key), field.value()) {
                        // Field-level type binding is consulted in US2; for the
                        // RON→JSON value the external-tag default already matches
                        // the projection, so recurse without a child node lookup.
                        apply_emit_conventions(child_json, &child_val, None);
                    }
                }
            }
        }
        Value::List(l) => {
            if let serde_json::Value::Array(arr) = json {
                for (child_json, child_val) in arr.iter_mut().zip(l.items()) {
                    apply_emit_conventions(child_json, &child_val, None);
                }
            }
        }
        Value::Tuple(t) => {
            // `Some(x)` unwraps to the inner value at the same JSON node.
            if tuple_name(t).as_deref() == Some("Some") {
                if let Some(inner) = t.items().next() {
                    apply_emit_conventions(json, &inner, None);
                }
                return;
            }
            if let serde_json::Value::Array(arr) = json {
                for (child_json, child_val) in arr.iter_mut().zip(t.items()) {
                    apply_emit_conventions(child_json, &child_val, None);
                }
            }
        }
        // An enum variant with no bound enum type keeps the projection's external
        // tag (the deterministic unbound default, FR-015); the bound-enum case is
        // handled above. Nested-variant tagging by a child binding is US2.
        Value::EnumVariant(_) => {}
        // Literals / unit / error carry no nested convention.
        Value::Literal(_) | Value::Unit(_) | Value::Error(_) => {}
    }
}

/// Re-key a JSON object built from a RON map: non-string keys become canonical
/// RON literals (FR-015, HINT-002), then recurse into the values.
fn apply_map_conventions(
    json: &mut serde_json::Value,
    m: &Map,
    _bound: Option<(&TypeModel, &ronin_types::model::TypeNode)>,
) {
    let serde_json::Value::Object(obj) = json else {
        return;
    };
    // Rebuild the object so non-string keys are re-keyed to canonical literals
    // while string keys keep their value. Iteration order follows the source map.
    let mut rebuilt = serde_json::Map::with_capacity(obj.len());
    for entry in m.entries() {
        let Some(key_value) = entry.key() else {
            continue;
        };
        let verbatim_key = crate::interop::pointer::projection_key_string(&key_value);
        let Some(mut child) = obj.remove(&verbatim_key) else {
            continue;
        };
        if let Some(v) = entry.value() {
            apply_emit_conventions(&mut child, &v, None);
        }
        let out_key = if is_string_key(&key_value) {
            verbatim_key
        } else {
            // Non-string key → CANONICAL RON literal (e.g. `(1,  2)` → `(1, 2)`).
            canonical_ron_literal(&key_value)
        };
        rebuilt.insert(out_key, child);
    }
    *obj = rebuilt;
}

/// Rewrite an external-tag enum JSON object `{"<Variant>": payload}` to the bound
/// type's recorded serde tagging (FR-015). Unbound / external → unchanged.
fn apply_enum_tagging(
    json: &mut serde_json::Value,
    bound: Option<(&TypeModel, &ronin_types::model::TypeNode)>,
) {
    // The projection already emits external tagging; only a bound non-external
    // discriminator changes the shape.
    let Some((_model, node)) = bound else {
        return;
    };
    let NodeKind::Enum { discriminator, .. } = &node.kind else {
        return;
    };
    // Extract the single `{"<Variant>": payload}` entry the projection produced.
    let serde_json::Value::Object(obj) = json else {
        return;
    };
    if obj.len() != 1 {
        return;
    }
    let (variant, payload) = {
        let (k, v) = obj.iter().next().expect("len == 1");
        (k.clone(), v.clone())
    };
    match discriminator {
        Discriminator::External => {}
        Discriminator::Internal { tag } => {
            // Internally tagged (serde): a struct/unit variant merges `{tag:
            // variant}` into the payload object — a unit variant (`null` payload)
            // becomes the lone `{tag: variant}` object. A non-object, non-null
            // payload cannot be internally tagged in serde, so it is left external.
            match payload {
                serde_json::Value::Object(mut payload_obj) => {
                    payload_obj.insert(tag.clone(), serde_json::Value::String(variant));
                    *json = serde_json::Value::Object(payload_obj);
                }
                serde_json::Value::Null => {
                    let mut obj = serde_json::Map::new();
                    obj.insert(tag.clone(), serde_json::Value::String(variant));
                    *json = serde_json::Value::Object(obj);
                }
                _ => {}
            }
        }
        Discriminator::Adjacent { tag, content } => {
            let mut adj = serde_json::Map::new();
            adj.insert(tag.clone(), serde_json::Value::String(variant));
            adj.insert(content.clone(), payload);
            *json = serde_json::Value::Object(adj);
        }
        Discriminator::Untagged => {
            // Untagged: the payload alone, no tag.
            *json = payload;
        }
    }
}

/// Whether a map key is JSON-string-native (string / raw-string keys keep their
/// string value; everything else is re-keyed to a canonical literal).
fn is_string_key(key: &Value) -> bool {
    if let Value::Literal(lit) = key {
        matches!(
            lit.token_kind(),
            Some(SyntaxKind::String | SyntaxKind::RawString)
        )
    } else {
        false
    }
}

/// The leading `Ident` name of a named tuple, or `None` for an anonymous tuple.
fn tuple_name(t: &ronin_core::syntax::ast::Tuple) -> Option<String> {
    t.syntax()
        .first_token_of(SyntaxKind::Ident)
        .map(|tok| tok.text().to_string())
}

/// Render a RON value into its **canonical** single-line RON literal — the
/// deterministic form a non-string map key is emitted as (FR-015, HINT-002).
///
/// Canonicalization normalizes whitespace (collapsing the source spacing) so two
/// keys with the same value but different source layout produce the same literal:
/// `(1,  2)` and `(1,2)` both canonicalize to `(1, 2)`. JSON→RON re-parses this
/// exact literal back to the typed key when a type is bound.
#[must_use]
pub fn canonical_ron_literal(value: &Value) -> String {
    let mut out = String::new();
    render_canonical(value, &mut out);
    out
}

/// Recursively render `value` into `out` in canonical RON form.
fn render_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Literal(lit) => {
            // Scalar literals keep their verbatim token text — it is already a
            // canonical RON literal (numbers, bools, quoted strings, chars).
            out.push_str(&lit.text().unwrap_or_default());
        }
        Value::Unit(_) => out.push_str("()"),
        Value::Tuple(t) => {
            if let Some(name) = tuple_name(t) {
                out.push_str(&name);
            }
            out.push('(');
            render_comma_list(t.items(), out);
            out.push(')');
        }
        Value::List(l) => {
            out.push('[');
            render_comma_list(l.items(), out);
            out.push(']');
        }
        Value::Struct(s) => {
            if let Some(name) = s.name_text() {
                out.push_str(&name);
            }
            out.push('(');
            let mut first = true;
            for field in s.fields() {
                let Some(name_tok) = field.name() else {
                    continue;
                };
                if !first {
                    out.push_str(", ");
                }
                first = false;
                out.push_str(name_tok.text());
                out.push_str(": ");
                if let Some(v) = field.value() {
                    render_canonical(&v, out);
                }
            }
            out.push(')');
        }
        Value::Map(m) => {
            out.push('{');
            let mut first = true;
            for entry in m.entries() {
                let Some(k) = entry.key() else {
                    continue;
                };
                if !first {
                    out.push_str(", ");
                }
                first = false;
                render_canonical(&k, out);
                out.push_str(": ");
                if let Some(v) = entry.value() {
                    render_canonical(&v, out);
                }
            }
            out.push('}');
        }
        Value::EnumVariant(v) => {
            // A bare ident / named payload variant — render verbatim-ish but
            // canonicalize the payload spacing via the typed accessors.
            out.push_str(&v.name_text().unwrap_or_default());
            let items: Vec<Value> = v.syntax().children().filter_map(Value::cast).collect();
            if !items.is_empty() {
                out.push('(');
                render_comma_list(items.into_iter(), out);
                out.push(')');
            }
        }
        // An unparseable node has no canonical literal; fall back to verbatim text
        // so the key is never silently emptied (defensive).
        Value::Error(node) => out.push_str(node.text().trim()),
    }
}

/// Render an iterator of values into `out` as a canonical `a, b, c` list.
fn render_comma_list(items: impl Iterator<Item = Value>, out: &mut String) {
    let mut first = true;
    for item in items {
        if !first {
            out.push_str(", ");
        }
        first = false;
        render_canonical(&item, out);
    }
}

/// Cross-check, via the serde `ron` crate, that `text` parses as RON — the
/// boundary **grammar verifier** role (ADR-0008 / FR-012), NOT the primary
/// converter.
///
/// Returns `true` when the serde `ron` parser accepts `text` as a RON value. Used
/// by tests / an optional convert-time sanity check to confirm the CST-based path
/// emitted grammar-consistent RON (e.g. a canonical map-key literal). The serde
/// `ron` crate is never the primary converter — the CST path carries fidelity.
#[must_use]
pub fn grammar_verify(text: &str) -> bool {
    ron::from_str::<ron::Value>(text).is_ok()
}

#[cfg(test)]
mod tests {
    //! T010 — RON→JSON value mapping + FR-015 emit conventions (canonical
    //! non-string keys, enum tagging) over the reused `CstJsonProjection`.

    use super::*;
    use ronin_types::model::{TypeNode, Variant, VariantShape};

    fn convert(src: &str) -> RonToJson {
        let doc = ronin_core::parse(src);
        ron_to_json(&doc, None, CommentMode::JsoncInline)
    }

    #[test]
    fn tuple_maps_to_array_unit_to_null_char_to_string() {
        // The reused projection encoding: tuple→array, unit→null, char→string.
        let r = convert("(t: (1, 2), u: (), c: 'x')");
        let obj = r.value.as_object().expect("root object");
        assert_eq!(obj.get("t"), Some(&serde_json::json!([1, 2])));
        assert_eq!(obj.get("u"), Some(&serde_json::Value::Null));
        assert_eq!(obj.get("c"), Some(&serde_json::json!("x")));
    }

    #[test]
    fn non_string_key_emitted_as_canonical_ron_literal() {
        // The projection would key this verbatim `(1,  2)`; E010 re-keys it to the
        // CANONICAL literal `(1, 2)` (FR-015, HINT-002).
        let r = convert("{ (1,  2): \"a\", 3: \"b\" }");
        let obj = r.value.as_object().expect("root object");
        assert!(
            obj.contains_key("(1, 2)"),
            "tuple key canonicalized: keys = {:?}",
            obj.keys().collect::<Vec<_>>()
        );
        assert!(obj.contains_key("3"), "integer key kept as its literal");
        assert!(
            !obj.contains_key("(1,  2)"),
            "verbatim double-space key must NOT survive"
        );
    }

    #[test]
    fn string_keys_are_unchanged() {
        let r = convert("{ \"name\": 1 }");
        let obj = r.value.as_object().expect("root object");
        assert_eq!(obj.get("name"), Some(&serde_json::json!(1)));
    }

    #[test]
    fn enum_variant_external_tag_is_the_unbound_default() {
        // Unbound: a named variant keeps the projection's external tag.
        let r = convert("(state: Running(5))");
        let obj = r.value.as_object().expect("root object");
        assert_eq!(obj.get("state"), Some(&serde_json::json!({ "Running": 5 })));
    }

    #[test]
    fn bound_internal_tagging_rewrites_external_tag() {
        // A bound enum with internal tagging rewrites `{"V": {..}}` to a tagged
        // payload object `{tag: "V", ..}` (FR-015, E004 TR-005).
        let mut model = TypeModel::new();
        model.insert_named(
            "Shape",
            TypeNode::new(NodeKind::Enum {
                variants: vec![Variant {
                    serialized_name: "Circle".to_string(),
                    shape: VariantShape::Unit,
                }],
                discriminator: Discriminator::Internal {
                    tag: "type".to_string(),
                },
            }),
        );
        // `Circle` is a bare-ident unit variant — the projection external-tags it
        // `{"Circle": null}`; internal tagging makes it `{"type": "Circle"}`.
        let doc = ronin_core::parse("Circle");
        let r = ron_to_json(
            &doc,
            Some(RonToJsonBinding::new(&model, "Shape")),
            CommentMode::JsoncInline,
        );
        assert_eq!(r.value, serde_json::json!({ "type": "Circle" }));
    }

    #[test]
    fn bound_adjacent_tagging_rewrites_external_tag() {
        let mut model = TypeModel::new();
        model.insert_named(
            "Msg",
            TypeNode::new(NodeKind::Enum {
                variants: vec![Variant {
                    serialized_name: "Ping".to_string(),
                    shape: VariantShape::Newtype(ronin_types::model::TypeRef::Inline(Box::new(
                        TypeNode::primitive(ronin_types::model::Primitive::Integer),
                    ))),
                }],
                discriminator: Discriminator::Adjacent {
                    tag: "t".to_string(),
                    content: "c".to_string(),
                },
            }),
        );
        let doc = ronin_core::parse("Ping(7)");
        let r = ron_to_json(
            &doc,
            Some(RonToJsonBinding::new(&model, "Msg")),
            CommentMode::JsoncInline,
        );
        assert_eq!(r.value, serde_json::json!({ "t": "Ping", "c": 7 }));
    }

    #[test]
    fn loss_report_and_comments_are_attached() {
        // The conversion bundles the loss map (T009) and comment carrier (T011).
        let doc = ronin_core::parse("// header\n(t: (1, 2), c: 'x')");
        let r = ron_to_json(&doc, None, CommentMode::JsoncInline);
        // The tuple + char are reported as losses.
        assert!(r.loss_report.requires_confirmation());
        assert!(
            r.loss_report
                .count_of(crate::interop::LossKind::TupleVsList)
                >= 1
        );
        assert!(r.loss_report.count_of(crate::interop::LossKind::Char) >= 1);
        // The comment is carried inline.
        assert_eq!(r.comments.len(), 1);
        assert_eq!(r.comments.inline_comments().len(), 1);
    }

    #[test]
    fn canonical_literal_normalizes_spacing() {
        let doc = ronin_core::parse("{ (1,  2): 1 }");
        let value = Document::cast(doc.root()).unwrap().value().unwrap();
        // Reach the map key.
        if let Value::Map(m) = value {
            let key = m.entries().next().unwrap().key().unwrap();
            assert_eq!(canonical_ron_literal(&key), "(1, 2)");
        } else {
            panic!("expected a map");
        }
    }

    #[test]
    fn grammar_verify_accepts_canonical_key_literal() {
        // The serde `ron` crate (grammar verifier) accepts the canonical literal.
        assert!(grammar_verify("(1, 2)"));
        assert!(grammar_verify("'x'"));
        assert!(!grammar_verify("(1, 2"));
    }
}
