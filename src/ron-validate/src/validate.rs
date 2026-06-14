//! Schema compilation and the validation pass that maps `jsonschema` errors to
//! `RON-V` [`ron_core::Diagnostic`]s with precise CST ranges (E006/FR-001,
//! FR-002, FR-005, FR-007, FR-024, T010–T012).
//!
//! The validator takes E004's serialized `TypeModel` interchange (JSON-Schema
//! 2020-12 + `x-ron-*`) as a [`serde_json::Value`], compiles it with the
//! `jsonschema` crate (`default-features = false`, fully offline), runs it over a
//! [`crate::projection::CstJsonProjection`], and translates each resulting
//! `instance_path` back to a CST span via the projection's
//! [`crate::projection::PointerRangeIndex`]. Each validation-error class maps to
//! one stable `RON-V####` [`ron_core::DiagnosticCode`].
//!
//! # Encoding contract
//!
//! The projection ([`crate::projection`]) and this validator agree on how a RON
//! value becomes `serde_json`: see that module's docs. Enum variants are
//! externally tagged (`{ "<Variant>": payload }`); Option's `Some`/`None` unwrap
//! to the inner value / `null`. Plain `jsonschema` cannot see the `x-ron-variant`
//! discriminator, so enum defs (`oneOf` whose branches carry `x-ron-variant`) are
//! dispatched by a custom pass ([`validate_enum`]) that picks the matching branch
//! and delegates the payload to `jsonschema`.
//!
//! # Fail-soft (FR-024)
//!
//! A `null`/empty model, a model with no selected def, or an uncompilable schema
//! all yield zero type diagnostics — the structural set remains untouched. Schema
//! compilation is size-bounded so a schema-bomb cannot hang; remote `$ref`s are
//! never fetched (the crate carries no HTTP resolver) and degrade to
//! unconstrained.
//!
//! # No-false-positive degradation (Principle III, US3)
//!
//! * **Structural-only / unconstrained unknowns (FR-015/FR-016).** An empty/`null`
//!   model or an unresolved bound type yields zero type diagnostics. An `unknown`
//!   type serializes to `{ "x-ron-kind": "unknown" }` — a schema with no
//!   constraining keyword, which `jsonschema` treats as the *true* schema (accepts
//!   anything). Because `jsonschema` validates each object property / array item
//!   against its own sub-schema independently, an `unknown`-typed field only
//!   relaxes *its own* subtree: sibling and ancestor fields bound to resolved
//!   types are still validated. The unconstrained region is thus scoped to the
//!   `unknown` node's subtree and never disables the surrounding structure.
//! * **Serde-faithful extras (FR-018).** `jsonschema` emits an
//!   `AdditionalProperties` error only where the schema set
//!   `additionalProperties: false` — which E004 emits only for a
//!   `deny_unknown_fields` type. Non-strict types carry no such keyword, so extra
//!   fields are silently allowed (zero diagnostics). See [`map_error`].
//! * **Skip unparseable regions (FR-019).** A parse-error node projects to a
//!   placeholder (`null`); a constrained schema would otherwise emit a *false*
//!   finding there. [`validate_node`] collects the `SyntaxKind::Error` node spans
//!   ([`error_node_spans`]) and drops any type finding intersecting one
//!   ([`drop_in_error_spans`]), so the unparseable span is unconstrained while the
//!   parseable remainder is still validated.
//! * **Dedup vs structural (FR-017).** [`dedup_against_structural`] suppresses, per
//!   finding, any type diagnostic whose byte range intersects a structural
//!   diagnostic's range (structural precedence). The structural set is never
//!   mutated.
//!
//! # Read-only over all inputs and transients (FR-020/FR-022)
//!
//! This module is read-only over every input. The public entries take
//! `&CstDocument` / `&serde_json::Value` (the model) / `&[Diagnostic]` (the
//! structural set) and return owned `Vec`s. No function here mutates the CST, the
//! bound `TypeModel`, or the structural diagnostic set, and there is no interior
//! mutability (`RefCell`/`Cell`/`Mutex`) over any input — only locally-owned
//! buffers (`Vec`/`String`/`serde_json::Map`) are built and returned. The crate
//! is `#![forbid(unsafe_code)]` (see `lib.rs`), so no input can be aliased and
//! mutated unsafely either. The document's bytes are therefore byte-identical
//! before and after a pass (the post-condition test is T035).

use ron_core::{Diagnostic, DiagnosticCode, SyntaxKind, SyntaxNode, TextRange};
use serde_json::Value;

use crate::projection::{CstJsonProjection, PointerRangeIndex};

/// Upper bound on the serialized schema size (bytes) we will hand to the
/// compiler. A pathological/schema-bomb model larger than this degrades to
/// structural-only (FR-024) rather than risking a slow/expensive compile.
const MAX_SCHEMA_BYTES: usize = 4 * 1024 * 1024;

/// Maximum recursion depth for the custom enum-dispatch walk (defense against a
/// cyclic `$ref` reached through enum branches; FR-024).
const MAX_ENUM_DEPTH: usize = 256;

/// The JSON-Schema 2020-12 dialect URI the effective schema declares.
const DIALECT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";

/// Run the type-validation pass for one document, validating its projected value
/// against the whole `model` as a self-contained schema (its root).
///
/// Used by the public [`crate::validate`] entry. `model` is treated as a complete
/// JSON-Schema document (with whatever `$defs` it carries resolvable internally).
/// An empty/`null` model yields no diagnostics (structural-only fallback,
/// FR-015). The pass is read-only over both inputs (FR-020).
///
/// Enum-shaped roots (`oneOf` carrying `x-ron-variant`) are dispatched through
/// the custom enum pass so `x-ron-variant` is honored.
#[must_use]
pub fn validate_root(model: &Value, doc: &ron_core::CstDocument) -> Vec<Diagnostic> {
    if is_empty_model(model) {
        return Vec::new();
    }
    let defs = model.get("$defs").cloned().unwrap_or(Value::Null);
    validate_node(model, &defs, doc)
}

/// Run the type-validation pass against a named def selected from `model.$defs`.
///
/// This is the testable core the binding/type-name wiring (Phase 3c/4) will call
/// with the bound type name. It builds an effective schema
/// `{ "$schema": <dialect>, "$ref": "#/$defs/<type>", "$defs": model.$defs }` so
/// `jsonschema` can resolve internal `$ref`s, then validates the document's
/// projected value against it.
///
/// Returns an empty set (no panic) when the model is empty, the def is absent, or
/// the schema cannot be compiled (FR-015/FR-024).
#[must_use]
pub fn validate_against(
    model: &Value,
    type_name: &str,
    doc: &ron_core::CstDocument,
) -> Vec<Diagnostic> {
    if is_empty_model(model) {
        return Vec::new();
    }
    let Some(defs) = model.get("$defs") else {
        return Vec::new();
    };
    let Some(target) = defs.get(type_name) else {
        // Unresolved bound type -> unconstrained (no false positives, FR-016).
        return Vec::new();
    };
    // Note (FR-016 scoping): when `target` (or any field within it) is an
    // `unknown` type it serializes to `{ "x-ron-kind": "unknown" }`, a schema with
    // no constraining keyword. `jsonschema` treats that as the *true* schema and
    // validates each sibling property/item against its own sub-schema, so an
    // `unknown`-typed field relaxes only its own subtree while siblings/ancestors
    // bound to resolved types are still validated. No special handling is needed
    // here — the unconstrained-subtree behavior falls out of per-location schema
    // application; this comment records that it is intentional and verified.
    validate_node(target, defs, doc)
}

/// Validate a document against one schema node, given the `$defs` map for `$ref`
/// resolution. Builds a schema-guided projection (so RON's ambiguous surface
/// forms project to exactly what each location expects), runs the main
/// `jsonschema` pass for the standard keywords, and a recursive enum-dispatch
/// walk for `x-ron-variant` (which plain jsonschema ignores).
fn validate_node(target: &Value, defs: &Value, doc: &ron_core::CstDocument) -> Vec<Diagnostic> {
    let resolved = resolve_ref(target, defs);
    let projection = CstJsonProjection::from_document_guided(doc, resolved, defs);

    let mut out = Vec::new();

    // Main pass: standard keywords (type, required, range, additionalProperties,
    // tuple arity, enum/const, length). oneOf wrappers at enum locations are
    // ignored in `map_error`; the enum walk below handles those.
    let effective = build_effective_schema(target, defs);
    out.extend(validate_with_schema(&effective, &projection));

    // Enum walk: custom x-ron-variant dispatch at every enum location.
    walk_enums(
        resolved,
        defs,
        &projection.instance,
        "",
        &projection.index,
        0,
        &mut out,
    );

    // Skip-unparseable (FR-019): a parse-error region projects to a placeholder
    // (`null`), against which a constrained schema would otherwise emit a FALSE
    // finding (e.g. a TypeMismatch where an integer was expected). Drop any type
    // finding whose span intersects a `ron-core` parse-error node span, so the
    // unparseable region is unconstrained while the parseable remainder is still
    // validated — no false-positive cascade (Principle III). The structural
    // diagnostics already cover those spans; type validation defers to them.
    let error_spans = error_node_spans(doc);
    let out = drop_in_error_spans(out, &error_spans);

    dedup_by_pointer(out)
}

/// Recursively descend the schema + instance together, invoking the custom enum
/// dispatch at each enum-typed location (so nested enums are validated, not only
/// a root enum). Non-enum nodes are descended structurally to reach nested enums;
/// standard-keyword findings are handled by the main `jsonschema` pass.
#[allow(clippy::too_many_arguments)]
fn walk_enums(
    schema: &Value,
    defs: &Value,
    instance: &Value,
    pointer: &str,
    index: &PointerRangeIndex,
    depth: usize,
    out: &mut Vec<Diagnostic>,
) {
    if depth > MAX_ENUM_DEPTH {
        return;
    }
    let schema = resolve_ref_in(schema, Some(defs));

    // Option: descend into the non-null branch with the inner value.
    if schema.get("x-ron-kind").and_then(Value::as_str) == Some("option") {
        if instance.is_null() {
            return;
        }
        if let Some(inner) = schema
            .get("oneOf")
            .and_then(Value::as_array)
            .and_then(|bs| {
                bs.iter()
                    .find(|b| b.get("type").and_then(Value::as_str) != Some("null"))
            })
        {
            walk_enums(inner, defs, instance, pointer, index, depth + 1, out);
        }
        return;
    }

    if is_enum_def(schema) {
        validate_enum(schema, defs, instance, pointer, index, depth, out);
        return;
    }

    // Descend object properties.
    if let Some(props) = schema.get("properties").and_then(Value::as_object) {
        if let Some(obj) = instance.as_object() {
            for (key, child_schema) in props {
                if let Some(child) = obj.get(key) {
                    let child_ptr = join_pointer(pointer, key);
                    walk_enums(child_schema, defs, child, &child_ptr, index, depth + 1, out);
                }
            }
        }
    }
    // Descend additionalProperties (maps).
    if let Some(ap) = schema.get("additionalProperties") {
        if ap.is_object() {
            let declared: std::collections::BTreeSet<&str> = schema
                .get("properties")
                .and_then(Value::as_object)
                .map(|p| p.keys().map(String::as_str).collect())
                .unwrap_or_default();
            if let Some(obj) = instance.as_object() {
                for (key, child) in obj {
                    if declared.contains(key.as_str()) {
                        continue;
                    }
                    let child_ptr = join_pointer(pointer, key);
                    walk_enums(ap, defs, child, &child_ptr, index, depth + 1, out);
                }
            }
        }
    }
    // Descend array items (prefixItems + items).
    if let Some(arr) = instance.as_array() {
        let prefix = schema.get("prefixItems").and_then(Value::as_array);
        let items = schema.get("items").filter(|i| i.is_object());
        for (i, child) in arr.iter().enumerate() {
            let child_schema = prefix.and_then(|p| p.get(i)).or(items);
            if let Some(cs) = child_schema {
                let child_ptr = format!("{pointer}/{i}");
                walk_enums(cs, defs, child, &child_ptr, index, depth + 1, out);
            }
        }
    }
}

/// Build `{ "$schema": <dialect>, "$ref": <target-or-#/$defs>, "$defs": defs }`.
fn build_effective_schema(target: &Value, defs: &Value) -> Value {
    let mut schema = serde_json::Map::new();
    schema.insert(
        "$schema".to_owned(),
        Value::String(DIALECT_2020_12.to_owned()),
    );
    if let Some(obj) = target.as_object() {
        for (k, v) in obj {
            schema.insert(k.clone(), v.clone());
        }
    }
    // Ensure $defs is present for internal $ref resolution.
    schema
        .entry("$defs".to_owned())
        .or_insert_with(|| defs.clone());
    Value::Object(schema)
}

/// Compile `schema` and run it over the projection, mapping each error to a
/// `RON-V` diagnostic.
fn validate_with_schema(schema: &Value, projection: &CstJsonProjection) -> Vec<Diagnostic> {
    let Some(validator) = compile_schema(schema) else {
        // Fail-soft: malformed/uncompilable schema or a remote $ref -> no type
        // diagnostics (FR-024). The structural set is left untouched.
        return Vec::new();
    };

    let mut diagnostics = Vec::new();
    for error in validator.iter_errors(&projection.instance) {
        if let Some(diag) = map_error(&error, schema, projection) {
            diagnostics.push(diag);
        }
    }
    dedup_by_pointer(diagnostics)
}

/// Compile a JSON-Schema value into a `jsonschema` validator with the 2020-12
/// dialect, fully offline. Returns `None` on any compile failure (malformed
/// schema, unresolvable/remote `$ref`, oversize bomb) so the caller fails soft
/// (FR-024).
#[must_use]
pub fn compile_schema(schema: &Value) -> Option<jsonschema::Validator> {
    // Size guard: a schema-bomb (huge serialized form) degrades to no validation
    // rather than being handed to the compiler.
    let approx_size = approx_value_size(schema, 0);
    if approx_size > MAX_SCHEMA_BYTES {
        return None;
    }

    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        // Do not opt into format validation (kept conservative; no false
        // positives from format checks). No retriever is configured, so a remote
        // $ref can never be fetched (FR-023) — it surfaces as a compile error and
        // we fail soft.
        .build(schema)
        .ok()
}

/// Run a compiled validator over an instance and return its errors mapped to
/// `RON-V` diagnostics. Exposed for direct callers (T010/T011).
#[must_use]
pub fn run_validation(
    validator: &jsonschema::Validator,
    schema: &Value,
    projection: &CstJsonProjection,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    for error in validator.iter_errors(&projection.instance) {
        if let Some(diag) = map_error(&error, schema, projection) {
            diagnostics.push(diag);
        }
    }
    dedup_by_pointer(diagnostics)
}

/// Map one `jsonschema` validation error to a `RON-V` [`Diagnostic`] (T011/T012).
///
/// The error kind selects the [`DiagnosticCode`]; the `instance_path` is
/// translated to a CST span via the [`PointerRangeIndex`] (value span for
/// type/range/arity, key/object span for required/unknown-field). Returns `None`
/// when the error has no actionable span or should be ignored (e.g. an
/// `oneOf`/`anyOf` wrapper whose meaningful causes surface elsewhere).
#[must_use]
pub fn map_error(
    error: &jsonschema::ValidationError<'_>,
    schema: &Value,
    projection: &CstJsonProjection,
) -> Option<Diagnostic> {
    use jsonschema::error::ValidationErrorKind as K;

    let pointer = error.instance_path().as_str();
    let index = &projection.index;

    match error.kind() {
        K::Type { .. } => {
            // Tuple arity surfaces as Type when items:false rejects the array
            // shape? No — arity surfaces via AdditionalItems / MinItems on a
            // tuple-kind schema. A Type error is a genuine type mismatch.
            let range = index
                .value_range(pointer)
                .or_else(|| index.range_for(pointer))?;
            Some(diag(DiagnosticCode::TypeMismatch, range, error))
        }
        K::Required { property } => {
            // Missing-required: attach to the field key if it somehow exists,
            // else to the containing object/struct span (deterministic: the
            // object's own value span).
            let prop = property.as_str().unwrap_or_default();
            let child_ptr = join_pointer(pointer, prop);
            let range = index
                .key_range(&child_ptr)
                .or_else(|| index.value_range(pointer))
                .or_else(|| index.range_for(pointer))?;
            Some(diag(DiagnosticCode::MissingRequiredField, range, error))
        }
        K::AdditionalProperties { unexpected } => {
            // Serde-faithful: only flagged when the schema set
            // additionalProperties:false at this location (FR-018). jsonschema
            // only emits this error when that is the case, so emitting here is
            // already correct. Attach to the first unexpected field's key span.
            let range = unexpected
                .iter()
                .find_map(|name| index.key_range(&join_pointer(pointer, name)))
                .or_else(|| index.value_range(pointer))
                .or_else(|| index.range_for(pointer))?;
            Some(diag(DiagnosticCode::UnknownField, range, error))
        }
        K::AdditionalItems { .. } | K::MaxItems { .. } | K::MinItems { .. } => {
            // On an x-ron-kind:tuple schema these mean wrong arity; otherwise a
            // collection length constraint.
            let code = if is_tuple_at(schema, pointer, projection) {
                DiagnosticCode::WrongTupleArity
            } else {
                DiagnosticCode::ValueConstraintViolation
            };
            let range = index
                .value_range(pointer)
                .or_else(|| index.range_for(pointer))?;
            Some(diag(code, range, error))
        }
        K::FalseSchema => {
            // `items: false` on a tuple def rejects an overflow element at
            // `<tuple>/<n>`; the arity finding attaches to the parent tuple span.
            let parent = parent_pointer(pointer);
            if is_tuple_at(schema, parent, projection) {
                let range = index
                    .value_range(parent)
                    .or_else(|| index.range_for(parent))?;
                return Some(diag(DiagnosticCode::WrongTupleArity, range, error));
            }
            // A false schema elsewhere is a value-constraint violation at the
            // offending element.
            let range = index
                .value_range(pointer)
                .or_else(|| index.range_for(pointer))?;
            Some(diag(DiagnosticCode::ValueConstraintViolation, range, error))
        }
        K::Enum { .. } | K::Constant { .. } => {
            let range = index
                .value_range(pointer)
                .or_else(|| index.range_for(pointer))?;
            Some(diag(DiagnosticCode::ValueConstraintViolation, range, error))
        }
        K::Minimum { .. }
        | K::Maximum { .. }
        | K::ExclusiveMinimum { .. }
        | K::ExclusiveMaximum { .. }
        | K::MultipleOf { .. }
        | K::MaxLength { .. }
        | K::MinLength { .. }
        | K::Pattern { .. }
        | K::MaxProperties { .. }
        | K::MinProperties { .. }
        | K::UniqueItems => {
            let range = index
                .value_range(pointer)
                .or_else(|| index.range_for(pointer))?;
            Some(diag(DiagnosticCode::ValueConstraintViolation, range, error))
        }
        // oneOf/anyOf wrappers, $ref/referencing errors, and other meta-kinds do
        // not map to a single precise finding — skip them (no false positives,
        // Principle III). The meaningful sub-cause, if any, surfaces via the
        // custom enum dispatch.
        _ => None,
    }
}

/// Build a [`Diagnostic`] with the code's default severity (T012) and the error's
/// message.
fn diag(
    code: DiagnosticCode,
    range: TextRange,
    error: &jsonschema::ValidationError<'_>,
) -> Diagnostic {
    Diagnostic::new(code, range, error.to_string())
}

/// Whether the schema location addressed by `pointer` is a RON tuple
/// (`x-ron-kind: "tuple"`). Best-effort: walks the schema following the same
/// instance pointer through `properties`/`prefixItems`/`items`. When unknown,
/// returns `false` (so the constraint is reported as a value-constraint, not a
/// false tuple-arity claim).
fn is_tuple_at(schema: &Value, pointer: &str, _projection: &CstJsonProjection) -> bool {
    // Resolve the schema sub-node for this instance pointer, then read x-ron-kind.
    let node = resolve_schema_for_pointer(schema, pointer);
    node.and_then(|n| n.get("x-ron-kind"))
        .and_then(Value::as_str)
        == Some("tuple")
}

/// Walk a schema toward the sub-schema that governs the instance location named
/// by `pointer`. Follows `$ref` (within the schema's own `$defs`),
/// `properties`/`additionalProperties` for object keys, and
/// `prefixItems`/`items` for array indices. Returns `None` if the path cannot be
/// resolved (then the caller treats the kind conservatively).
fn resolve_schema_for_pointer<'a>(schema: &'a Value, pointer: &str) -> Option<&'a Value> {
    let defs = schema.get("$defs");
    let mut current = resolve_ref_in(schema, defs);
    for raw_seg in pointer.split('/').filter(|s| !s.is_empty()) {
        let seg = unescape_pointer_segment(raw_seg);
        current = resolve_ref_in(current, defs);
        if let Ok(idx) = seg.parse::<usize>() {
            // Array index: prefer prefixItems[idx], else items.
            let next = current
                .get("prefixItems")
                .and_then(|p| p.get(idx))
                .or_else(|| current.get("items").filter(|i| i.is_object()));
            current = next?;
        } else {
            let next = current
                .get("properties")
                .and_then(|p| p.get(&seg))
                .or_else(|| {
                    current
                        .get("additionalProperties")
                        .filter(|a| a.is_object())
                });
            current = next?;
        }
    }
    Some(resolve_ref_in(current, defs))
}

/// Resolve a `{ "$ref": "#/$defs/X" }` against a `$defs` map (one level; returns
/// the node unchanged if it is not a local def `$ref`).
fn resolve_ref_in<'a>(node: &'a Value, defs: Option<&'a Value>) -> &'a Value {
    let Some(reference) = node.get("$ref").and_then(Value::as_str) else {
        return node;
    };
    let Some(name) = reference.strip_prefix("#/$defs/") else {
        return node;
    };
    defs.and_then(|d| d.get(name)).unwrap_or(node)
}

/// Resolve a top-level local `$ref` against `defs` (used at the entry point).
fn resolve_ref<'a>(node: &'a Value, defs: &'a Value) -> &'a Value {
    resolve_ref_in(node, Some(defs))
}

/// Whether a schema node is a RON enum def: a `oneOf` whose branches carry
/// `x-ron-variant`.
fn is_enum_def(node: &Value) -> bool {
    node.get("oneOf")
        .and_then(Value::as_array)
        .is_some_and(|branches| branches.iter().any(|b| b.get("x-ron-variant").is_some()))
}

/// Custom enum-variant dispatch (T011 — plain jsonschema cannot, since
/// `x-ron-variant` is an ignored annotation).
///
/// `instance` is the projected value at `pointer`. For an externally-tagged
/// variant `{ "<Variant>": payload }` (or `null` for a unit/None), the matching
/// `oneOf` branch is found by `x-ron-variant`. No match -> `InvalidEnumVariant`
/// at the value span. A match validates the payload against the branch's payload
/// shape (newtype -> `x-ron-payload`, tuple -> `prefixItems` with arity ->
/// `WrongTupleArity`, struct -> `type:object`+`properties`+`required`). When in
/// doubt, emit nothing (no false positives, FR-016).
#[allow(clippy::too_many_arguments)]
fn validate_enum(
    def: &Value,
    defs: &Value,
    instance: &Value,
    pointer: &str,
    index: &PointerRangeIndex,
    depth: usize,
    out: &mut Vec<Diagnostic>,
) {
    if depth > MAX_ENUM_DEPTH {
        return; // cyclic-$ref guard (FR-024)
    }
    let Some(branches) = def.get("oneOf").and_then(Value::as_array) else {
        return;
    };

    // Determine the variant name + payload from the externally-tagged instance.
    let (variant_name, payload): (Option<String>, Option<&Value>) = match instance {
        // Unit variant / None -> projected as null. Match a unit branch.
        Value::Null => (None, None),
        Value::Object(map) if map.len() == 1 => {
            let (k, v) = map.iter().next().expect("len == 1");
            (Some(k.clone()), Some(v))
        }
        // An Option that unwrapped Some(x) to a bare x means the value is NOT an
        // external-tag object; for a non-Option enum this is a malformed shape,
        // but we conservatively skip (no false positive) unless it is clearly a
        // bare unit-ident which the projection encodes as the tag object already.
        _ => {
            // The value is some bare scalar/array/object that is not an external
            // tag. We cannot confidently say which variant it is -> skip.
            return;
        }
    };

    // Find the matching branch.
    let matched = branches.iter().find(|b| {
        let v = b.get("x-ron-variant").and_then(Value::as_str);
        match (&variant_name, v) {
            (Some(name), Some(branch_name)) => name == branch_name,
            // A null instance (unit/None) matches a unit-shape branch.
            (None, Some(_)) => b.get("x-ron-variant-shape").and_then(Value::as_str) == Some("unit"),
            _ => false,
        }
    });

    let Some(branch) = matched else {
        // Invalid/unknown variant -> RON-V0003 at the value span.
        if let Some(range) = index
            .value_range(pointer)
            .or_else(|| index.range_for(pointer))
        {
            out.push(Diagnostic::new(
                DiagnosticCode::InvalidEnumVariant,
                range,
                invalid_variant_message(&variant_name, branches),
            ));
        }
        return;
    };

    // Validate the payload against the matched branch's shape.
    let shape = branch.get("x-ron-variant-shape").and_then(Value::as_str);
    let payload_ptr = match &variant_name {
        Some(name) => join_pointer(pointer, name),
        None => pointer.to_owned(),
    };

    match (shape, payload) {
        (Some("unit"), _) | (_, None) => {
            // Nothing to validate for a unit payload.
        }
        (Some("newtype"), Some(p)) => {
            if let Some(inner) = branch.get("x-ron-payload") {
                validate_subschema(inner, defs, p, &payload_ptr, index, depth, out);
            }
        }
        (Some("tuple"), Some(p)) => {
            validate_tuple_branch(branch, defs, p, &payload_ptr, index, depth, out);
        }
        (Some("struct"), Some(p)) => {
            validate_subschema(branch, defs, p, &payload_ptr, index, depth, out);
        }
        // Unknown shape -> conservatively validate against the branch as-is if it
        // looks like a real schema, else skip.
        (_, Some(p)) => {
            if branch.get("x-ron-payload").is_some()
                || branch.get("prefixItems").is_some()
                || branch.get("properties").is_some()
                || branch.get("type").is_some()
            {
                validate_subschema(branch, defs, p, &payload_ptr, index, depth, out);
            }
        }
    }
}

/// Validate a tuple-variant payload: arity against `prefixItems` length
/// (`WrongTupleArity`) and each element against its `prefixItems[i]`.
#[allow(clippy::too_many_arguments)]
fn validate_tuple_branch(
    branch: &Value,
    defs: &Value,
    payload: &Value,
    payload_ptr: &str,
    index: &PointerRangeIndex,
    depth: usize,
    out: &mut Vec<Diagnostic>,
) {
    let Some(prefix) = branch.get("prefixItems").and_then(Value::as_array) else {
        return;
    };
    let Some(items) = payload.as_array() else {
        // A non-array payload where a tuple is expected -> type mismatch at value.
        if let Some(range) = index
            .value_range(payload_ptr)
            .or_else(|| index.range_for(payload_ptr))
        {
            out.push(Diagnostic::new(
                DiagnosticCode::TypeMismatch,
                range,
                "tuple variant payload is not a tuple",
            ));
        }
        return;
    };
    if items.len() != prefix.len() {
        if let Some(range) = index
            .value_range(payload_ptr)
            .or_else(|| index.range_for(payload_ptr))
        {
            out.push(Diagnostic::new(
                DiagnosticCode::WrongTupleArity,
                range,
                format!(
                    "tuple variant expects {} element(s), found {}",
                    prefix.len(),
                    items.len()
                ),
            ));
        }
        // Still validate the overlapping prefix elements.
    }
    for (i, (elem, elem_schema)) in items.iter().zip(prefix.iter()).enumerate() {
        let elem_ptr = format!("{payload_ptr}/{i}");
        validate_subschema(elem_schema, defs, elem, &elem_ptr, index, depth, out);
    }
}

/// Validate `instance` (at `pointer`) against a payload sub-schema, delegating to
/// `jsonschema` (with the model's `$defs` wired in for `$ref` resolution) and
/// re-pointing the resulting errors to the payload's pointer namespace.
#[allow(clippy::too_many_arguments)]
fn validate_subschema(
    subschema: &Value,
    defs: &Value,
    instance: &Value,
    pointer: &str,
    index: &PointerRangeIndex,
    depth: usize,
    out: &mut Vec<Diagnostic>,
) {
    // A nested enum payload needs custom dispatch too.
    let resolved = resolve_ref_in(subschema, Some(defs));
    if is_enum_def(resolved) {
        validate_enum(resolved, defs, instance, pointer, index, depth + 1, out);
        return;
    }

    let effective = build_effective_schema(subschema, defs);
    let Some(validator) = compile_schema(&effective) else {
        return; // fail-soft
    };
    for error in validator.iter_errors(instance) {
        // Re-base the error's instance pointer under `pointer` so spans resolve.
        let sub_ptr = error.instance_path().as_str();
        let full_ptr = concat_pointer(pointer, sub_ptr);
        if let Some(diag) = map_error_at(&error, &effective, index, &full_ptr) {
            out.push(diag);
        }
    }
}

/// Like [`map_error`] but using an explicit (already-rebased) pointer for span
/// lookup.
fn map_error_at(
    error: &jsonschema::ValidationError<'_>,
    schema: &Value,
    index: &PointerRangeIndex,
    full_ptr: &str,
) -> Option<Diagnostic> {
    use jsonschema::error::ValidationErrorKind as K;
    match error.kind() {
        K::Type { .. } => {
            let range = index
                .value_range(full_ptr)
                .or_else(|| index.range_for(full_ptr))?;
            Some(diag(DiagnosticCode::TypeMismatch, range, error))
        }
        K::Required { property } => {
            let prop = property.as_str().unwrap_or_default();
            let child = join_pointer(full_ptr, prop);
            let range = index
                .key_range(&child)
                .or_else(|| index.value_range(full_ptr))
                .or_else(|| index.range_for(full_ptr))?;
            Some(diag(DiagnosticCode::MissingRequiredField, range, error))
        }
        K::AdditionalProperties { unexpected } => {
            let range = unexpected
                .iter()
                .find_map(|name| index.key_range(&join_pointer(full_ptr, name)))
                .or_else(|| index.value_range(full_ptr))
                .or_else(|| index.range_for(full_ptr))?;
            Some(diag(DiagnosticCode::UnknownField, range, error))
        }
        K::AdditionalItems { .. } | K::MaxItems { .. } | K::MinItems { .. } => {
            let code = if is_tuple_schema(schema, full_ptr) {
                DiagnosticCode::WrongTupleArity
            } else {
                DiagnosticCode::ValueConstraintViolation
            };
            let range = index
                .value_range(full_ptr)
                .or_else(|| index.range_for(full_ptr))?;
            Some(diag(code, range, error))
        }
        K::FalseSchema => {
            let parent = parent_pointer(full_ptr);
            if is_tuple_schema(schema, parent) {
                let range = index
                    .value_range(parent)
                    .or_else(|| index.range_for(parent))?;
                return Some(diag(DiagnosticCode::WrongTupleArity, range, error));
            }
            let range = index
                .value_range(full_ptr)
                .or_else(|| index.range_for(full_ptr))?;
            Some(diag(DiagnosticCode::ValueConstraintViolation, range, error))
        }
        K::Enum { .. }
        | K::Constant { .. }
        | K::Minimum { .. }
        | K::Maximum { .. }
        | K::ExclusiveMinimum { .. }
        | K::ExclusiveMaximum { .. }
        | K::MultipleOf { .. }
        | K::MaxLength { .. }
        | K::MinLength { .. }
        | K::Pattern { .. }
        | K::MaxProperties { .. }
        | K::MinProperties { .. }
        | K::UniqueItems => {
            let range = index
                .value_range(full_ptr)
                .or_else(|| index.range_for(full_ptr))?;
            Some(diag(DiagnosticCode::ValueConstraintViolation, range, error))
        }
        _ => None,
    }
}

/// Whether the sub-schema for `pointer` in `schema` is an `x-ron-kind:tuple`.
fn is_tuple_schema(schema: &Value, pointer: &str) -> bool {
    resolve_schema_for_pointer(schema, pointer)
        .and_then(|n| n.get("x-ron-kind"))
        .and_then(Value::as_str)
        == Some("tuple")
}

/// Build the `InvalidEnumVariant` message listing the allowed variant names.
fn invalid_variant_message(found: &Option<String>, branches: &[Value]) -> String {
    let allowed: Vec<&str> = branches
        .iter()
        .filter_map(|b| b.get("x-ron-variant").and_then(Value::as_str))
        .collect();
    match found {
        Some(name) => format!(
            "unknown enum variant `{name}` (expected one of: {})",
            allowed.join(", ")
        ),
        None => format!(
            "value is not a valid enum variant (expected one of: {})",
            allowed.join(", ")
        ),
    }
}

/// Join a base pointer with a single (unescaped) child key, escaping per RFC 6901.
fn join_pointer(base: &str, key: &str) -> String {
    let mut s = String::with_capacity(base.len() + key.len() + 1);
    s.push_str(base);
    s.push('/');
    for ch in key.chars() {
        match ch {
            '~' => s.push_str("~0"),
            '/' => s.push_str("~1"),
            other => s.push(other),
        }
    }
    s
}

/// Concatenate a base pointer with a sub-pointer (the sub-pointer is already a
/// full RFC-6901 pointer, possibly empty).
fn concat_pointer(base: &str, sub: &str) -> String {
    if sub.is_empty() {
        base.to_owned()
    } else {
        format!("{base}{sub}")
    }
}

/// Unescape a single JSON-Pointer segment (`~1` -> `/`, `~0` -> `~`).
fn unescape_pointer_segment(seg: &str) -> String {
    seg.replace("~1", "/").replace("~0", "~")
}

/// The parent pointer of `pointer` (drop the last `/segment`); `""` for the root.
fn parent_pointer(pointer: &str) -> &str {
    match pointer.rfind('/') {
        Some(idx) => &pointer[..idx],
        None => "",
    }
}

/// A model is "empty" (structural-only) when it is `null`, not an object, or an
/// object with no usable schema content (FR-015).
fn is_empty_model(model: &Value) -> bool {
    match model {
        Value::Null => true,
        Value::Object(map) => map.is_empty(),
        _ => false,
    }
}

/// Whether two byte ranges intersect — share one or more bytes (FR-017/FR-019).
///
/// Containment is the inclusive case of intersection. Two zero-length (empty)
/// ranges intersect only when they sit at the same offset; an empty range
/// intersects a non-empty one when the offset falls inside `[start, end)`. This is
/// the half-open `[start, end)` overlap test: `a.start < b.end && b.start < a.end`,
/// with an equal-offset special case so a zero-length error span at the exact
/// start/edge of a finding still suppresses it (conservative — never a false
/// positive).
fn ranges_intersect(a: TextRange, b: TextRange) -> bool {
    if a.is_empty() || b.is_empty() {
        // A zero-length range shares a byte with the other range when its offset
        // lies within (or at the inclusive edge of) the other.
        let (point, span) = if a.is_empty() { (a, b) } else { (b, a) };
        let p = point.start();
        return p >= span.start() && p <= span.end();
    }
    a.start() < b.end() && b.start() < a.end()
}

/// Collect the byte spans of every `ron-core` parse-error node (`SyntaxKind::Error`)
/// in the document's CST (FR-019).
///
/// These are the spans of the error/`ERROR`-kind subtree(s) the parser produced
/// for unparseable regions. Type validation skips exactly these spans (see
/// [`drop_in_error_spans`]) so a malformed region never yields a cascaded type
/// error while the parseable remainder is still validated. Read-only: walks the
/// (borrowed) CST and copies only ranges (FR-020/FR-022).
fn error_node_spans(doc: &ron_core::CstDocument) -> Vec<TextRange> {
    let mut spans = Vec::new();
    collect_error_spans(&doc.root(), &mut spans);
    spans
}

/// Recursively collect `SyntaxKind::Error` node spans under `node`.
///
/// An error node's whole subtree is the unparseable region; once one is found its
/// children are not descended further (the span already covers them). The walk is
/// iterative-friendly but written recursively over the (bounded-depth) CST.
fn collect_error_spans(node: &SyntaxNode, spans: &mut Vec<TextRange>) {
    if node.kind() == SyntaxKind::Error {
        spans.push(node.text_range());
        // The whole subtree is covered by this span; no need to descend.
        return;
    }
    for child in node.children() {
        collect_error_spans(&child, spans);
    }
}

/// Drop any type finding whose span intersects a parse-error span (FR-019).
///
/// Findings outside every error span are retained unchanged; findings that share
/// one or more bytes with any error span are suppressed (the unparseable region is
/// unconstrained). Order of the retained findings is preserved.
fn drop_in_error_spans(diags: Vec<Diagnostic>, error_spans: &[TextRange]) -> Vec<Diagnostic> {
    if error_spans.is_empty() {
        return diags;
    }
    diags
        .into_iter()
        .filter(|d| {
            !error_spans
                .iter()
                .any(|span| ranges_intersect(d.range(), *span))
        })
        .collect()
}

/// Suppress, per finding, any type diagnostic whose byte range intersects the
/// range of ANY structural diagnostic — structural always wins on overlap
/// (FR-017). Non-overlapping type findings are retained.
///
/// This is the dedup contract from the public [`crate::validate`] entry and the
/// shell's merge point (`ronin-app`'s `merge_type_diagnostics`). Structural
/// diagnostics are NEVER passed through or mutated here — only the (already
/// owned) type set is filtered — so the caller keeps the structural set intact
/// and merges the two however it renders them. Read-only over `structural`
/// (FR-020/FR-022): it is borrowed, never modified.
///
/// "Intersect" means any non-empty byte-range overlap (containment is the
/// inclusive case); see [`ranges_intersect`].
#[must_use]
pub fn dedup_against_structural(
    type_diags: Vec<Diagnostic>,
    structural: &[Diagnostic],
) -> Vec<Diagnostic> {
    if structural.is_empty() {
        return type_diags;
    }
    type_diags
        .into_iter()
        .filter(|t| {
            !structural
                .iter()
                .any(|s| ranges_intersect(t.range(), s.range()))
        })
        .collect()
}

/// Deduplicate diagnostics that share the same (code, range): the same logical
/// finding can surface more than once via oneOf branch exploration.
fn dedup_by_pointer(mut diags: Vec<Diagnostic>) -> Vec<Diagnostic> {
    let mut seen = std::collections::BTreeSet::new();
    diags.retain(|d| {
        let key = (
            d.code().code(),
            d.range().start(),
            d.range().end(),
            d.message().to_owned(),
        );
        seen.insert(key)
    });
    diags
}

/// A bounded, cheap estimate of a JSON value's serialized size, used to reject a
/// schema-bomb before compiling (FR-024). Recursion is depth-bounded so the
/// estimate itself cannot blow the stack.
fn approx_value_size(value: &Value, depth: usize) -> usize {
    if depth > 1024 {
        return MAX_SCHEMA_BYTES + 1; // treat extreme depth as oversize
    }
    match value {
        Value::Null => 4,
        Value::Bool(_) => 5,
        Value::Number(_) => 8,
        Value::String(s) => s.len() + 2,
        Value::Array(a) => {
            2 + a
                .iter()
                .map(|v| approx_value_size(v, depth + 1) + 1)
                .sum::<usize>()
        }
        Value::Object(o) => {
            2 + o
                .iter()
                .map(|(k, v)| k.len() + 3 + approx_value_size(v, depth + 1))
                .sum::<usize>()
        }
    }
}

#[cfg(test)]
mod tests {
    //! Sanity-level inline checks for the Phase-5a degradation behaviors. The
    //! comprehensive corpus/oracle tests are the next phase (T033–T035); these are
    //! minimal guards so the new T030/T031/T032 code paths cannot silently regress.

    use super::*;
    use serde_json::json;

    fn range(start: usize, end: usize) -> TextRange {
        TextRange::new(start, end)
    }

    fn structural(start: usize, end: usize) -> Diagnostic {
        Diagnostic::new(
            DiagnosticCode::UnexpectedToken,
            range(start, end),
            "structural",
        )
    }

    fn type_diag(start: usize, end: usize) -> Diagnostic {
        Diagnostic::new(DiagnosticCode::TypeMismatch, range(start, end), "type")
    }

    #[test]
    fn ranges_intersect_covers_overlap_containment_and_edges() {
        // Disjoint -> no intersection.
        assert!(!ranges_intersect(range(0, 5), range(5, 10)));
        // Partial overlap -> intersection.
        assert!(ranges_intersect(range(0, 6), range(5, 10)));
        // Containment -> intersection.
        assert!(ranges_intersect(range(2, 4), range(0, 10)));
        // Zero-length point inside -> intersection.
        assert!(ranges_intersect(range(3, 3), range(0, 10)));
        // Zero-length point at the inclusive end edge -> intersection (conservative).
        assert!(ranges_intersect(range(10, 10), range(0, 10)));
        // Zero-length point outside -> no intersection.
        assert!(!ranges_intersect(range(11, 11), range(0, 10)));
    }

    #[test]
    fn dedup_against_structural_suppresses_only_overlapping_type_findings() {
        let structural = [structural(10, 20)];
        let type_diags = vec![
            type_diag(0, 5),   // disjoint -> kept
            type_diag(15, 18), // contained in structural -> suppressed
            type_diag(18, 25), // partial overlap -> suppressed
            type_diag(30, 35), // disjoint -> kept
        ];
        let kept = dedup_against_structural(type_diags, &structural);
        assert_eq!(kept.len(), 2);
        assert_eq!((kept[0].range().start(), kept[0].range().end()), (0, 5));
        assert_eq!((kept[1].range().start(), kept[1].range().end()), (30, 35));
    }

    #[test]
    fn dedup_against_structural_is_identity_without_structural() {
        let type_diags = vec![type_diag(0, 5), type_diag(10, 20)];
        let kept = dedup_against_structural(type_diags.clone(), &[]);
        assert_eq!(kept, type_diags);
    }

    #[test]
    fn skip_unparseable_drops_findings_in_error_span_but_keeps_remainder() {
        // `id` is malformed (a stray `@`) so it recovers into an Error node; `name`
        // is a valid-but-type-violating remainder (string expected, integer given).
        // FR-019 oracle: zero findings inside the malformed span AND the expected
        // type diagnostic on the parseable remainder.
        let model = json!({
            "$defs": {
                "Entity": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "integer" },
                        "name": { "type": "string" }
                    }
                }
            }
        });
        let src = "Entity(id: @bad, name: 7)";
        let doc = ron_core::parse(src);

        // There IS at least one parse-error span (the malformed `id` value).
        let spans = error_node_spans(&doc);
        assert!(
            !spans.is_empty(),
            "expected a parse-error node span for the malformed region"
        );

        let diags = validate_against(&model, "Entity", &doc);
        // No finding may land inside any error span.
        for d in &diags {
            assert!(
                !spans.iter().any(|s| ranges_intersect(d.range(), *s)),
                "a type finding leaked into the unparseable span: {:?}",
                (d.code().code(), d.range().start(), d.range().end())
            );
        }
        // The remainder's violation (name: 7 is not a string) is still reported.
        assert!(
            diags
                .iter()
                .any(|d| d.code() == DiagnosticCode::TypeMismatch),
            "expected the remainder type mismatch to survive, got: {:?}",
            diags
                .iter()
                .map(|d| (d.code().code(), d.range().start(), d.range().end()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn read_only_document_bytes_unchanged_after_pass() {
        // FR-020/FR-022 spot check: a pass over a document leaves its bytes
        // byte-identical (the comprehensive post-condition test is T035).
        let model = json!({
            "$defs": { "Entity": { "type": "object",
                "properties": { "id": { "type": "integer" } } } }
        });
        let src = "Entity(id: \"oops\")";
        let doc = ron_core::parse(src);
        let before = doc.root().text();
        let _ = validate_against(&model, "Entity", &doc);
        let after = doc.root().text();
        assert_eq!(before, after, "validation must not mutate the CST bytes");
    }
}
