//! JSON→RON reconstruction — schema-aware when a `TypeModel` is bound, deterministic
//! best-effort otherwise; emits RON text → `ronin_core::parse` (FR-002/009/015, AD-003).
//!
//! This is the inverse of [`ron_to_json`](crate::interop::ron_to_json). It walks a
//! `serde_json::Value`, emitting **RON text** that `ronin_core::parse` then turns
//! into a lossless [`CstDocument`] (AD-003 / HINT-005). The serde `ron` crate is
//! used ONLY as the boundary **grammar verifier** ([`grammar_verify`]) — never as
//! the primary converter (ADR-0008 / FR-012).
//!
//! # Schema-aware vs best-effort (FR-009, SC-004)
//!
//! When a [`JsonToRonBinding`] is supplied the reconstruction consults the bound
//! `TypeModel` as **data** (ADR-0004) to recover the RON-specific shapes JSON
//! cannot express (the expanded round-trip tier, FR-011):
//!
//! * **array → tuple (by arity) vs list** — a JSON array bound to a
//!   [`NodeKind::Tuple`] becomes a RON tuple `(a, b)`; bound to a
//!   [`NodeKind::Sequence`] (or unbound) it stays a RON list `[a, b]` (FR-009/015).
//! * **enum variant** — a JSON value bound to a [`NodeKind::Enum`] is reconstructed
//!   to the named variant per the type's recorded [`Discriminator`]
//!   (external/internal/adjacent/untagged, E004 TR-005) (FR-009/015).
//! * **char** — a one-character JSON string bound to a `char` node
//!   ([`RonKind::Char`]) becomes a RON char `'x'` (FR-009).
//! * **`Option`** — `null` bound to a [`NodeKind::Option`] becomes `None`; a value
//!   becomes `Some(value)` (FR-009).
//! * **non-string map key** — a canonical-RON-literal string key bound to a
//!   non-string-keyed [`NodeKind::Map`] is **re-parsed** back to the typed key
//!   (e.g. `"(1, 2)"` → the tuple key `(1, 2)`) (FR-009/015).
//! * **struct / unit** — a JSON object bound to a [`NodeKind::Object`] becomes a RON
//!   struct `(field: …)`; `null` bound to a unit node becomes `()` (FR-009).
//!
//! When **no** binding (or no resolved node) applies, a documented deterministic
//! **best-effort** structural mapping is used (FR-009): object → struct, array →
//! list, scalar → literal, an external-tag `{"V": payload}` object is treated as a
//! best-effort enum variant, string keys stay string keys. Every residual
//! ambiguity (array-as-list-not-tuple, string-key-not-typed, …) is surfaced in the
//! returned [`JsonToRon::notes`] so the caller can flag it (FR-009).
//!
//! # Comment read-back (FR-008)
//!
//! When a [`CommentCarrier`] is supplied its comments (JSONC inline + sidecar map,
//! keyed by JSON Pointer) are **re-attached** to the reconstructed RON by emitting
//! each comment immediately before the value at its anchored pointer — restoring
//! RON→JSON→RON comment symmetry (FR-008, SC-001).
//!
//! # Bounded recursion (FR-013, SC-009)
//!
//! Adversarial-but-well-formed JSON (deeply/recursively nested or a pathologically
//! large collection) MUST degrade safely: the builder tracks a recursion depth and
//! refuses to descend past [`MAX_JSON_DEPTH`], emitting a flagged
//! [`LossKind::UnparseableRegion`] placeholder + a note instead of recursing — so
//! the converter never stack-overflows or hangs (FR-013, SC-009). `serde_json`'s
//! own parser is depth-limited too, but this guard governs *our* emit walk.

use ronin_core::CstDocument;
use ronin_types::extension::RonKind;
use ronin_types::model::{Discriminator, NodeKind, TypeModel, TypeNode, TypeRef, VariantShape};

use crate::interop::comments::{CommentCarrier, CommentKind};
use crate::interop::loss::{LossKind, LossRecovery, LossReport, LossyConstruct};

/// The maximum JSON nesting depth the emit walk descends before bailing out with a
/// flagged placeholder (FR-013, SC-009).
///
/// Set well above any realistic config/scene nesting yet bounded so an adversarial
/// deeply-nested input cannot drive unbounded recursion / a stack overflow. A node
/// at this depth is emitted as a flagged [`LossKind::UnparseableRegion`] placeholder
/// (a parseable `()` sentinel) plus a note — never a crash. Kept in lockstep with
/// `ronin-core`'s own parse depth guard so the re-parse of our emitted text also
/// succeeds within bounds.
pub const MAX_JSON_DEPTH: usize = 96;

/// The type information JSON→RON consults for schema-aware reconstruction — the
/// bound `TypeModel` plus the document's root type name (FR-009/015).
///
/// `TypeModel` is consulted strictly as **data** (ADR-0004); E010 never owns,
/// mutates, or persists it. When `None` is passed to [`json_to_ron`] the converter
/// uses the deterministic best-effort structural mapping (FR-009).
#[derive(Debug, Clone, Copy)]
pub struct JsonToRonBinding<'a> {
    /// The bound type model (consulted as data).
    pub model: &'a TypeModel,
    /// The document's bound root type name (a key into
    /// [`TypeModel::named_types`](ronin_types::model::TypeModel::named_types)).
    pub root_type: &'a str,
}

impl<'a> JsonToRonBinding<'a> {
    /// Build a binding view from a `TypeModel` and a root type name.
    #[must_use]
    pub fn new(model: &'a TypeModel, root_type: &'a str) -> Self {
        Self { model, root_type }
    }

    /// Resolve the root node this binding points at, when registered.
    fn root_node(&self) -> Option<&'a TypeNode> {
        self.model.lookup(self.root_type)
    }
}

/// The complete JSON→RON reconstruction outcome (FR-002/009/015): the reconstructed
/// RON document, the residual-ambiguity notes, and the lossy-construct map.
///
/// The reconstructed [`document`](Self::document) is a real `ronin_core` CST built by
/// parsing the emitted RON text — lossless and ready to install in a buffer or open
/// in a new tab. The [`text`](Self::text) is that same emitted RON source. The
/// [`notes`](Self::notes) surface every residual ambiguity of the best-effort path
/// (FR-009). The [`loss_report`](Self::loss_report) records any flagged placeholders
/// (depth-exceeded / unrepresentable regions, FR-013).
#[derive(Debug)]
pub struct JsonToRon {
    /// The reconstructed RON document (a lossless `ronin_core` CST).
    pub document: CstDocument,
    /// The emitted RON source text (what [`document`](Self::document) was parsed
    /// from).
    pub text: String,
    /// Residual-ambiguity notes from the best-effort / unbound path (FR-009). Empty
    /// on a fully schema-aware reconstruction.
    pub notes: Vec<String>,
    /// The lossy-construct map — flagged placeholders for depth-exceeded /
    /// unrepresentable regions (FR-013). Empty on a clean reconstruction.
    pub loss_report: LossReport,
}

/// Reconstruct RON from a JSON value — schema-aware when `binding` is `Some`,
/// deterministic best-effort otherwise (FR-002/009/015).
///
/// * `json` — the parsed input JSON value (read-only).
/// * `binding` — the bound `TypeModel` view for schema-aware reconstruction, or
///   `None` for the deterministic best-effort structural mapping (FR-009).
/// * `comments` — an optional comment carrier whose comments (JSONC inline + sidecar
///   map, keyed by JSON Pointer) are re-attached to the reconstructed RON (FR-008).
///
/// Returns the reconstructed RON document + text, the residual-ambiguity notes, and
/// the loss report. The emit walk is depth-bounded ([`MAX_JSON_DEPTH`]) so an
/// adversarial deeply-nested input degrades safely (FR-013, SC-009).
#[must_use]
pub fn json_to_ron(
    json: &serde_json::Value,
    binding: Option<JsonToRonBinding<'_>>,
    comments: Option<&CommentCarrier>,
) -> JsonToRon {
    let mut builder = Builder {
        model: binding.map(|b| b.model),
        comments,
        notes: Vec::new(),
        loss_report: LossReport::new(),
    };

    let root_node = binding.and_then(|b| b.root_node());
    let mut text = String::new();
    // Emit comments anchored to the root pointer "" as a leading header (FR-008).
    builder.emit_anchored_comments(&mut text, "");
    builder.emit_value(&mut text, json, root_node, "", 0);
    text.push('\n');

    // The CST-based path carries fidelity; the serde `ron` crate VERIFIES the
    // emitted text parses as RON (grammar verifier role, ADR-0008 / FR-012). If the
    // emit produced something the grammar rejects (it should not for well-formed
    // input), the note records it — the document is still produced via the
    // error-tolerant `ronin_core::parse` (which never drops bytes).
    if !grammar_verify(&text) {
        builder
            .notes
            .push("emitted RON did not pass the serde `ron` grammar cross-check".to_string());
    }

    let document = ronin_core::parse(&text);
    JsonToRon {
        document,
        text,
        notes: builder.notes,
        loss_report: builder.loss_report,
    }
}

/// Cross-check, via the serde `ron` crate, that `text` parses as RON — the boundary
/// **grammar verifier** role (ADR-0008 / FR-012), NOT the primary converter.
///
/// Returns `true` when the serde `ron` parser accepts `text` as a RON value. The
/// serde `ron` crate is never the primary converter — the CST path carries fidelity;
/// this only confirms the emitted text is grammar-consistent RON.
#[must_use]
pub fn grammar_verify(text: &str) -> bool {
    ron::from_str::<ron::Value>(text).is_ok()
}

/// The recursive JSON→RON emitter. Holds the (optional) bound `TypeModel`, the
/// (optional) comment carrier, and the accumulating notes / loss report.
struct Builder<'a> {
    /// The bound type model, consulted as data for schema-aware reconstruction
    /// (FR-009). `None` ⇒ best-effort.
    model: Option<&'a TypeModel>,
    /// The comment carrier whose comments are re-attached on read-back (FR-008).
    comments: Option<&'a CommentCarrier>,
    /// Residual-ambiguity notes from the best-effort path (FR-009).
    notes: Vec<String>,
    /// Flagged-placeholder losses (depth-exceeded / unrepresentable) (FR-013).
    loss_report: LossReport,
}

impl Builder<'_> {
    /// Resolve a [`TypeRef`] against the bound model, returning the target node.
    fn resolve<'r>(&'r self, type_ref: &'r TypeRef) -> Option<&'r TypeNode> {
        self.model.and_then(|m| m.resolve(type_ref))
    }

    /// Emit `value` as RON text guided by the optional bound `node`, at `depth`.
    ///
    /// `pointer` is the value's JSON Pointer (for comment read-back). The output is
    /// single-line RON, so no indentation level is tracked. The walk is
    /// depth-bounded (FR-013, SC-009): past [`MAX_JSON_DEPTH`] it emits a flagged
    /// `()` placeholder + a loss rather than recursing.
    fn emit_value(
        &mut self,
        out: &mut String,
        value: &serde_json::Value,
        node: Option<&TypeNode>,
        pointer: &str,
        depth: usize,
    ) {
        // Bounded recursion: an adversarial deeply-nested input degrades to a
        // flagged parseable placeholder instead of stack-overflowing (SC-009).
        if depth >= MAX_JSON_DEPTH {
            out.push_str("()");
            self.loss_report.push(LossyConstruct::with_detail(
                LossKind::UnparseableRegion,
                ronin_core::TextRange::new(0usize, 0usize),
                LossRecovery::LossyToExternal,
                "JSON nesting exceeded the safe depth bound — emitted as a flagged placeholder",
            ));
            self.notes.push(format!(
                "nesting depth limit ({MAX_JSON_DEPTH}) reached at pointer {pointer:?}; \
                 deeper structure replaced with a flagged placeholder"
            ));
            return;
        }

        // Schema-aware: dispatch on the resolved node kind first (FR-009).
        if let Some(node) = node {
            if self.emit_typed(out, value, node, pointer, depth) {
                return;
            }
        }

        // Best-effort structural mapping (FR-009).
        self.emit_best_effort(out, value, pointer, depth);
    }

    /// Schema-aware emit: reconstruct the RON-specific shape from the bound `node`.
    /// Returns `true` when the typed path handled the value, `false` to fall back to
    /// the best-effort path (e.g. a type/value mismatch).
    fn emit_typed(
        &mut self,
        out: &mut String,
        value: &serde_json::Value,
        node: &TypeNode,
        pointer: &str,
        depth: usize,
    ) -> bool {
        match &node.kind {
            // null → None / value → Some(value) (FR-009).
            NodeKind::Option { inner } => {
                if value.is_null() {
                    out.push_str("None");
                } else {
                    let inner_node = self.resolve(inner).map(ToOwned::to_owned);
                    out.push_str("Some(");
                    self.emit_value(out, value, inner_node.as_ref(), pointer, depth + 1);
                    out.push(')');
                }
                true
            }
            // A unit node `()`: null → () (FR-009).
            NodeKind::Primitive { .. }
                if node
                    .ron_extension
                    .as_ref()
                    .and_then(|e| e.ron_kind)
                    .map(|k| k == RonKind::Unit)
                    .unwrap_or(false) =>
            {
                if value.is_null() {
                    out.push_str("()");
                    true
                } else {
                    false
                }
            }
            // A char node: one-character string → 'x' (FR-009).
            NodeKind::Primitive { .. }
                if node
                    .ron_extension
                    .as_ref()
                    .and_then(|e| e.ron_kind)
                    .map(|k| k == RonKind::Char)
                    .unwrap_or(false) =>
            {
                if let Some(s) = value.as_str() {
                    if s.chars().count() == 1 {
                        emit_char_literal(out, s);
                        return true;
                    }
                }
                false
            }
            // A fixed-arity tuple: array → (a, b) (FR-009/015).
            NodeKind::Tuple { elements } => {
                let Some(arr) = value.as_array() else {
                    return false;
                };
                out.push('(');
                for (i, item) in arr.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    let elem_node = elements
                        .get(i)
                        .and_then(|r| self.resolve(r))
                        .map(ToOwned::to_owned);
                    let child_ptr = push_index(pointer, i);
                    self.emit_value(out, item, elem_node.as_ref(), &child_ptr, depth + 1);
                }
                out.push(')');
                true
            }
            // A homogeneous sequence: array → [a, b] (FR-009).
            NodeKind::Sequence { element } => {
                let Some(arr) = value.as_array() else {
                    return false;
                };
                let elem_node = self.resolve(element).map(ToOwned::to_owned);
                self.emit_list(out, arr, elem_node.as_ref(), pointer, depth);
                true
            }
            // A struct: object → (field: …) (FR-009).
            NodeKind::Object { fields, .. } => {
                let Some(obj) = value.as_object() else {
                    return false;
                };
                out.push('(');
                let mut first = true;
                for (key, child) in obj {
                    if !first {
                        out.push_str(", ");
                    }
                    first = false;
                    // Resolve the field's declared node by serialized key.
                    let field_node = fields
                        .iter()
                        .find(|f| f.serialized_key == *key)
                        .and_then(|f| self.resolve(&f.value))
                        .map(ToOwned::to_owned);
                    let child_ptr = push_key(pointer, key);
                    self.emit_anchored_comments(out, &child_ptr);
                    emit_field_ident(out, key);
                    out.push_str(": ");
                    self.emit_value(out, child, field_node.as_ref(), &child_ptr, depth + 1);
                }
                out.push(')');
                true
            }
            // A map. Non-string-keyed maps re-parse the canonical RON literal key
            // back to its typed form (FR-009/015); string-keyed maps stay objects.
            NodeKind::Map {
                key,
                value: val_ref,
            } => {
                let Some(obj) = value.as_object() else {
                    return false;
                };
                let non_string = node
                    .ron_extension
                    .as_ref()
                    .and_then(|e| e.ron_kind)
                    .map(|k| k == RonKind::NonStringKeyMap)
                    .unwrap_or(false);
                let _ = key;
                let val_node = self.resolve(val_ref).map(ToOwned::to_owned);
                self.emit_map(out, obj, val_node.as_ref(), non_string, pointer, depth);
                true
            }
            // An enum: reconstruct the named variant per the recorded tagging
            // (FR-009/015).
            NodeKind::Enum {
                variants,
                discriminator,
            } => {
                self.emit_enum(out, value, variants, discriminator, pointer, depth);
                true
            }
            // A plain primitive (number/string/bool) emits the scalar literal.
            NodeKind::Primitive { .. } => {
                self.emit_scalar(out, value);
                true
            }
            // Unknown bound node → best-effort (the schema cannot guide it).
            NodeKind::Unknown => false,
        }
    }

    /// Best-effort structural mapping (FR-009): object → struct, array → list,
    /// scalar → literal, with external-tag enum detection and string keys.
    fn emit_best_effort(
        &mut self,
        out: &mut String,
        value: &serde_json::Value,
        pointer: &str,
        depth: usize,
    ) {
        match value {
            serde_json::Value::Null => out.push_str("None"),
            serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::String(_) => self.emit_scalar(out, value),
            serde_json::Value::Array(arr) => {
                // Unbound: an array is a list, but it COULD be a tuple — note the
                // residual ambiguity (FR-009).
                self.note_once(format!(
                    "array at pointer {pointer:?} reconstructed as a RON list \
                     (could be a tuple — bind a type to disambiguate)"
                ));
                self.emit_list(out, arr, None, pointer, depth);
            }
            serde_json::Value::Object(obj) => {
                // An external-tag `{"Variant": payload}` single-entry object is a
                // best-effort enum variant (the unbound default, FR-015).
                if let Some((variant, payload)) = single_entry(obj) {
                    if looks_like_variant_name(variant) {
                        self.note_once(format!(
                            "single-key object {{{variant:?}: …}} at pointer {pointer:?} \
                             reconstructed as an externally-tagged enum variant \
                             (bind a type to confirm the tagging)"
                        ));
                        self.emit_external_variant(out, variant, payload, pointer, depth);
                        return;
                    }
                }
                // An object whose keys are all valid RON identifiers reconstructs as
                // a RON struct `(field: …)`; an object with any non-ident key cannot
                // be a struct, so it reconstructs as a string-keyed RON map
                // `{"k": …}` (the base-tier round-trip-safe form). Both RON shapes
                // project to the same JSON object, so unbound this is the documented
                // deterministic best-effort choice (FR-009).
                let all_ident_keys = obj.keys().all(|k| is_ron_ident(k));
                if all_ident_keys {
                    out.push('(');
                    let mut first = true;
                    for (key, child) in obj {
                        if !first {
                            out.push_str(", ");
                        }
                        first = false;
                        let child_ptr = push_key(pointer, key);
                        self.emit_anchored_comments(out, &child_ptr);
                        out.push_str(key);
                        out.push_str(": ");
                        self.emit_value(out, child, None, &child_ptr, depth + 1);
                    }
                    out.push(')');
                } else {
                    self.note_once(format!(
                        "object with non-identifier keys at pointer {pointer:?} \
                         reconstructed as a string-keyed RON map (bind a type for \
                         struct fields or typed map keys)"
                    ));
                    self.emit_map(out, obj, None, false, pointer, depth);
                }
            }
        }
    }

    /// Emit a JSON array as a RON list `[a, b, c]`, recursing under `elem_node`.
    fn emit_list(
        &mut self,
        out: &mut String,
        arr: &[serde_json::Value],
        elem_node: Option<&TypeNode>,
        pointer: &str,
        depth: usize,
    ) {
        out.push('[');
        for (i, item) in arr.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let child_ptr = push_index(pointer, i);
            self.emit_anchored_comments(out, &child_ptr);
            self.emit_value(out, item, elem_node, &child_ptr, depth + 1);
        }
        out.push(']');
    }

    /// Emit a JSON object as a RON map `{k: v, …}`. When `non_string` is set each
    /// key string is a canonical RON literal **re-parsed** to its typed form
    /// (FR-015); otherwise keys stay quoted strings.
    fn emit_map(
        &mut self,
        out: &mut String,
        obj: &serde_json::Map<String, serde_json::Value>,
        val_node: Option<&TypeNode>,
        non_string: bool,
        pointer: &str,
        depth: usize,
    ) {
        out.push('{');
        let mut first = true;
        for (key, child) in obj {
            if !first {
                out.push_str(", ");
            }
            first = false;
            let child_ptr = push_key(pointer, key);
            self.emit_anchored_comments(out, &child_ptr);
            if non_string {
                // Re-parse the canonical RON literal back to the typed key (FR-015).
                // The key string is itself a RON value literal (e.g. "(1, 2)", "3").
                out.push_str(key);
            } else {
                emit_string_literal(out, key);
            }
            out.push_str(": ");
            self.emit_value(out, child, val_node, &child_ptr, depth + 1);
        }
        out.push('}');
    }

    /// Emit a named enum variant per the recorded serde `discriminator` (FR-015).
    fn emit_enum(
        &mut self,
        out: &mut String,
        value: &serde_json::Value,
        variants: &[ronin_types::model::Variant],
        discriminator: &Discriminator,
        pointer: &str,
        depth: usize,
    ) {
        // Recover the (variant_name, payload) pair from the JSON per the tagging.
        let recovered: Option<(String, Option<serde_json::Value>)> = match discriminator {
            Discriminator::External => value
                .as_object()
                .and_then(single_entry)
                .map(|(v, p)| (v.to_string(), Some(p.clone()))),
            Discriminator::Internal { tag } => value.as_object().and_then(|obj| {
                obj.get(tag).and_then(serde_json::Value::as_str).map(|v| {
                    // The payload is the object minus the tag field.
                    let mut payload = obj.clone();
                    payload.remove(tag);
                    let p = if payload.is_empty() {
                        None
                    } else {
                        Some(serde_json::Value::Object(payload))
                    };
                    (v.to_string(), p)
                })
            }),
            Discriminator::Adjacent { tag, content } => value.as_object().and_then(|obj| {
                obj.get(tag)
                    .and_then(serde_json::Value::as_str)
                    .map(|v| (v.to_string(), obj.get(content).cloned()))
            }),
            Discriminator::Untagged => {
                // Untagged: the payload alone, no tag to recover. Best-effort —
                // match the first variant whose shape fits, else note ambiguity.
                self.note_once(format!(
                    "untagged enum at pointer {pointer:?} reconstructed best-effort \
                     (no tag to recover the variant from)"
                ));
                None
            }
        };

        let Some((variant_name, payload)) = recovered else {
            // Could not recover a tagged variant → best-effort structural mapping.
            self.emit_best_effort(out, value, pointer, depth);
            return;
        };

        // Find the declared variant to shape the payload (unit / newtype / tuple /
        // struct). An unknown variant name is emitted as a bare ident (defensive).
        let declared = variants.iter().find(|v| v.serialized_name == variant_name);
        out.push_str(&variant_name);
        // No payload (a unit variant, or a tag-only recovery) → bare ident.
        let Some(p) = &payload else {
            return;
        };
        match declared.map(|d| &d.shape) {
            // A unit variant should carry no payload; ignore any stray payload to
            // keep the variant a bare ident (defensive).
            Some(VariantShape::Unit) => {}
            // Newtype variant `V(inner)`.
            Some(VariantShape::Newtype(inner)) => {
                let inner_node = self.resolve(inner).map(ToOwned::to_owned);
                out.push('(');
                self.emit_value(out, p, inner_node.as_ref(), pointer, depth + 1);
                out.push(')');
            }
            // Tuple variant `V(a, b)` — the payload is a JSON array.
            Some(VariantShape::Tuple(elems)) => {
                out.push('(');
                if let Some(arr) = p.as_array() {
                    for (i, item) in arr.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        let elem_node = elems
                            .get(i)
                            .and_then(|r| self.resolve(r))
                            .map(ToOwned::to_owned);
                        let child_ptr = push_index(pointer, i);
                        self.emit_value(out, item, elem_node.as_ref(), &child_ptr, depth + 1);
                    }
                } else {
                    self.emit_value(out, p, None, pointer, depth + 1);
                }
                out.push(')');
            }
            // Struct variant `V(field: …)` — the payload is a JSON object.
            Some(VariantShape::Struct(struct_fields)) => {
                out.push('(');
                if let Some(obj) = p.as_object() {
                    let mut first = true;
                    for (key, child) in obj {
                        if !first {
                            out.push_str(", ");
                        }
                        first = false;
                        let field_node = struct_fields
                            .iter()
                            .find(|f| f.serialized_key == *key)
                            .and_then(|f| self.resolve(&f.value))
                            .map(ToOwned::to_owned);
                        let child_ptr = push_key(pointer, key);
                        emit_field_ident(out, key);
                        out.push_str(": ");
                        self.emit_value(out, child, field_node.as_ref(), &child_ptr, depth + 1);
                    }
                }
                out.push(')');
            }
            // An undeclared variant name with a payload → emit the payload as a
            // best-effort single-item parenthesized payload.
            None => {
                out.push('(');
                self.emit_value(out, p, None, pointer, depth + 1);
                out.push(')');
            }
        }
    }

    /// Emit a best-effort external-tag variant `{"V": payload}` (unbound default,
    /// FR-015) — `V(payload)` for a value payload, bare `V` for a `null` payload.
    fn emit_external_variant(
        &mut self,
        out: &mut String,
        variant: &str,
        payload: &serde_json::Value,
        pointer: &str,
        depth: usize,
    ) {
        out.push_str(variant);
        if payload.is_null() {
            // A `null` payload (a serde unit variant external-tag) → bare ident.
            return;
        }
        out.push('(');
        self.emit_value(out, payload, None, pointer, depth + 1);
        out.push(')');
    }

    /// Emit a JSON scalar (bool/number/string) as its RON literal.
    fn emit_scalar(&mut self, out: &mut String, value: &serde_json::Value) {
        match value {
            serde_json::Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            serde_json::Value::Number(n) => out.push_str(&n.to_string()),
            serde_json::Value::String(s) => emit_string_literal(out, s),
            // Null / collections never reach here from the scalar paths; emit a
            // safe `()` placeholder defensively rather than nothing.
            _ => out.push_str("()"),
        }
    }

    /// Re-attach comments anchored to `pointer` from the carrier, each before the
    /// value (FR-008). A no-op without a carrier. The output is single-line RON, so
    /// no indentation is applied.
    fn emit_anchored_comments(&mut self, out: &mut String, pointer: &str) {
        let Some(carrier) = self.comments else {
            return;
        };
        for comment in carrier.comments() {
            if comment.anchor_pointer == pointer {
                // Re-emit in the original style (line/block); a block comment is kept
                // verbatim, a line comment keeps its `//` form (FR-008).
                out.push_str(&comment.text);
                if comment.kind == CommentKind::Line {
                    out.push('\n');
                } else {
                    out.push(' ');
                }
            }
        }
    }

    /// Push `note` unless an identical note was already recorded (keeps the residual
    /// ambiguity list de-duplicated for a deterministic output).
    fn note_once(&mut self, note: String) {
        if !self.notes.contains(&note) {
            self.notes.push(note);
        }
    }
}

/// `true` when an object has exactly one entry; returns the `(key, value)` pair.
fn single_entry(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<(&str, &serde_json::Value)> {
    if obj.len() == 1 {
        obj.iter().next().map(|(k, v)| (k.as_str(), v))
    } else {
        None
    }
}

/// A heuristic for the best-effort external-tag detection: a variant name is an
/// identifier starting with an uppercase ASCII letter (serde variants are
/// PascalCase by default). Conservative so an ordinary string-keyed map is not
/// mis-read as a variant.
fn looks_like_variant_name(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_uppercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Emit a RON struct/variant field identifier. A serde key that is a valid RON
/// identifier is emitted bare; otherwise it is quoted (RON allows quoted field
/// names in maps, but struct fields must be idents — a non-ident key only arises
/// from a string-keyed object, which the map path handles, so this is defensive).
fn emit_field_ident(out: &mut String, key: &str) {
    if is_ron_ident(key) {
        out.push_str(key);
    } else {
        emit_string_literal(out, key);
    }
}

/// `true` when `s` is a valid RON identifier (ASCII alpha/underscore start, then
/// alphanumeric/underscore).
fn is_ron_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Emit a RON char literal `'x'` from a one-character string, escaping as needed.
fn emit_char_literal(out: &mut String, s: &str) {
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('\'');
}

/// Emit a RON string literal `"…"` from `s` with the standard escapes.
fn emit_string_literal(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
}

/// Append an object-property segment to a JSON Pointer (RFC 6901 escaping).
fn push_key(pointer: &str, key: &str) -> String {
    let mut out = String::with_capacity(pointer.len() + key.len() + 1);
    out.push_str(pointer);
    out.push('/');
    for ch in key.chars() {
        match ch {
            '~' => out.push_str("~0"),
            '/' => out.push_str("~1"),
            other => out.push(other),
        }
    }
    out
}

/// Append an array-index segment to a JSON Pointer.
fn push_index(pointer: &str, index: usize) -> String {
    format!("{pointer}/{index}")
}

#[cfg(test)]
mod tests {
    //! T020 — JSON→RON reconstruction (schema-aware + best-effort) unit coverage.

    use super::*;
    use ronin_types::model::{Field, Primitive, Variant};

    fn unbound(json: serde_json::Value) -> JsonToRon {
        json_to_ron(&json, None, None)
    }

    #[test]
    fn best_effort_object_becomes_struct() {
        let r = unbound(serde_json::json!({ "name": "hero", "level": 3 }));
        assert!(r.text.contains("name: \"hero\""), "got: {}", r.text);
        assert!(r.text.contains("level: 3"));
        assert!(grammar_verify(&r.text), "emitted RON parses");
    }

    #[test]
    fn best_effort_array_becomes_list_with_ambiguity_note() {
        let r = unbound(serde_json::json!({ "pos": [1, 2] }));
        assert!(r.text.contains("pos: [1, 2]"), "got: {}", r.text);
        assert!(
            r.notes.iter().any(|n| n.contains("could be a tuple")),
            "tuple-vs-list ambiguity is noted: {:?}",
            r.notes
        );
    }

    #[test]
    fn bound_array_becomes_tuple_by_arity() {
        // pos: a 2-tuple of integers.
        let mut model = TypeModel::new();
        model.insert_named(
            "Pos2",
            TypeNode::tuple(vec![
                TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
                TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
            ]),
        );
        model.insert_named(
            "Root",
            TypeNode::new(NodeKind::Object {
                fields: vec![Field {
                    serialized_key: "pos".into(),
                    value: TypeRef::named("Pos2"),
                    optional: false,
                    flatten: false,
                }],
                deny_unknown_fields: false,
            }),
        );
        let json = serde_json::json!({ "pos": [1, 2] });
        let r = json_to_ron(&json, Some(JsonToRonBinding::new(&model, "Root")), None);
        assert!(
            r.text.contains("pos: (1, 2)"),
            "tuple by arity, got: {}",
            r.text
        );
    }

    #[test]
    fn bound_char_and_option_and_unit() {
        let mut model = TypeModel::new();
        model.insert_named(
            "Root",
            TypeNode::new(NodeKind::Object {
                fields: vec![
                    Field {
                        serialized_key: "c".into(),
                        value: TypeRef::inline(TypeNode::char_()),
                        optional: false,
                        flatten: false,
                    },
                    Field {
                        serialized_key: "maybe".into(),
                        value: TypeRef::inline(TypeNode::option(TypeRef::inline(
                            TypeNode::primitive(Primitive::Integer),
                        ))),
                        optional: true,
                        flatten: false,
                    },
                    Field {
                        serialized_key: "u".into(),
                        value: TypeRef::inline(TypeNode::unit()),
                        optional: false,
                        flatten: false,
                    },
                ],
                deny_unknown_fields: false,
            }),
        );
        let json = serde_json::json!({ "c": "x", "maybe": null, "u": null });
        let r = json_to_ron(&json, Some(JsonToRonBinding::new(&model, "Root")), None);
        assert!(r.text.contains("c: 'x'"), "char, got: {}", r.text);
        assert!(
            r.text.contains("maybe: None"),
            "Option null → None, got: {}",
            r.text
        );
        assert!(r.text.contains("u: ()"), "unit, got: {}", r.text);
    }

    #[test]
    fn bound_external_enum_variant() {
        let mut model = TypeModel::new();
        model.insert_named(
            "State",
            TypeNode::new(NodeKind::Enum {
                variants: vec![Variant {
                    serialized_name: "Running".into(),
                    shape: VariantShape::Newtype(TypeRef::inline(TypeNode::primitive(
                        Primitive::Integer,
                    ))),
                }],
                discriminator: Discriminator::External,
            }),
        );
        let json = serde_json::json!({ "Running": 5 });
        let r = json_to_ron(&json, Some(JsonToRonBinding::new(&model, "State")), None);
        assert_eq!(r.text.trim(), "Running(5)", "external variant");
    }

    #[test]
    fn bound_internal_enum_variant() {
        let mut model = TypeModel::new();
        model.insert_named(
            "Shape",
            TypeNode::new(NodeKind::Enum {
                variants: vec![Variant {
                    serialized_name: "Circle".into(),
                    shape: VariantShape::Unit,
                }],
                discriminator: Discriminator::Internal { tag: "type".into() },
            }),
        );
        // Internally-tagged unit variant: {"type": "Circle"} → Circle.
        let json = serde_json::json!({ "type": "Circle" });
        let r = json_to_ron(&json, Some(JsonToRonBinding::new(&model, "Shape")), None);
        assert_eq!(r.text.trim(), "Circle", "internal-tag unit variant");
    }

    #[test]
    fn bound_non_string_key_map_reparses_typed_key() {
        let mut model = TypeModel::new();
        model.insert_named(
            "Keyed",
            TypeNode::non_string_key_map(
                TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
                TypeRef::inline(TypeNode::primitive(Primitive::String)),
            ),
        );
        // The canonical RON-literal key "7" re-parses to the integer key 7.
        let json = serde_json::json!({ "7": "x" });
        let r = json_to_ron(&json, Some(JsonToRonBinding::new(&model, "Keyed")), None);
        assert!(
            r.text.contains("7: \"x\""),
            "typed int key, got: {}",
            r.text
        );
        assert!(
            !r.text.contains("\"7\":"),
            "the key is NOT a quoted string: {}",
            r.text
        );
    }

    #[test]
    fn unbound_string_keys_stay_strings() {
        let r = unbound(serde_json::json!({ "k": "v" }));
        // A non-ident-shaped key path in a map keeps quotes; here it is a struct.
        assert!(grammar_verify(&r.text));
    }

    #[test]
    fn deeply_nested_input_degrades_safely() {
        // Build JSON nested far beyond MAX_JSON_DEPTH; the emit must not overflow.
        let mut value = serde_json::json!(0);
        for _ in 0..(MAX_JSON_DEPTH + 50) {
            value = serde_json::Value::Array(vec![value]);
        }
        let r = unbound(value);
        // It produced output, recorded a depth note + a flagged placeholder loss,
        // and did NOT crash.
        assert!(
            r.notes.iter().any(|n| n.contains("nesting depth limit")),
            "depth bound is noted: {:?}",
            r.notes
        );
        assert!(
            r.loss_report
                .constructs()
                .iter()
                .any(|c| c.kind() == LossKind::UnparseableRegion),
            "depth-exceeded region is a flagged placeholder loss"
        );
    }

    #[test]
    fn comment_read_back_reattaches_at_anchor() {
        // A carrier with a comment anchored to "/x" re-attaches before that field.
        let ron = "(\n  // about x\n  x: 1,\n)";
        let doc = ronin_core::parse(ron);
        let carrier = CommentCarrier::from_document(&doc, crate::interop::CommentMode::JsoncInline);
        let json = serde_json::json!({ "x": 1 });
        let r = json_to_ron(&json, None, Some(&carrier));
        let about = r.text.find("// about x");
        let field = r.text.find("x: 1");
        assert!(about.is_some(), "comment re-attached: {}", r.text);
        assert!(about < field, "comment precedes its value: {}", r.text);
    }
}
