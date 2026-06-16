//! Derive an initial RON document from a `TypeModel` (FR-010) — per-kind typed
//! placeholders; `unknown`→deterministic sentinel + an inline "fill in" diagnostic.
//!
//! This is the **derive-from-type** entry of the interop boundary (US3, AD-003): it
//! walks a named type's [`TypeNode`] shape (consulted strictly as **data**, ADR-0004
//! — never executed, never mutated) and emits a **parseable** RON scaffold populated
//! with **deterministic typed placeholders** per kind, so a developer gets an
//! editable RON skeleton matching their Rust type instead of writing it from scratch
//! (FR-010, SC-005, US3).
//!
//! # Per-kind placeholder policy (data-model §DerivedScaffold)
//!
//! The placeholder for each E004 [`NodeKind`] / [`RonKind`] is **fixed** so the same
//! `(type, model)` always yields a byte-identical scaffold (determinism, FR-010):
//!
//! | kind | placeholder |
//! |------|-------------|
//! | integer | `0` |
//! | number (float) | `0.0` |
//! | string | `""` |
//! | bool | `false` |
//! | null | `None` (a bare unit `()` would also parse; `None` is the safer typed default) |
//! | `char` ([`RonKind::Char`]) | `'\0'` (a single deterministic sentinel char) |
//! | unit `()` ([`RonKind::Unit`]) | `()` |
//! | bytes ([`RonKind::Bytes`]) | `""` |
//! | sequence / list | `[]` (an empty list always parses + conforms) |
//! | tuple | `(p, …)` — an arity-filled tuple of each element's placeholder (`()` for a 0-tuple) |
//! | map | `{}` (an empty map always parses + conforms) |
//! | struct (object) | `(field: <placeholder>, …)` per declared field |
//! | enum | the **first-declared** variant, with its payload placeholders |
//! | `Option` | `None` |
//! | `unknown` (E004) | the [`UNKNOWN_SENTINEL`] string + a "placeholder — fill in" diagnostic |
//!
//! # `unknown` → parseable sentinel + diagnostic, never a hole (FR-010)
//!
//! A field whose type resolves to [`NodeKind::Unknown`] (E004's first-class
//! unresolved node) is filled with the **deterministic, identifiable** parseable
//! sentinel [`UNKNOWN_SENTINEL`] (a distinctive RON string literal) AND records a
//! [`LossKind::UnparseableRegion`]-coded "placeholder — fill in" diagnostic anchored
//! to the sentinel's span in the emitted text. The scaffold therefore *always*
//! parses and conforms to the type's field shape even for partially-`unknown` types
//! (SC-005), while the developer is told exactly which spots need a real value.
//!
//! # Bounded recursion (FR-013, SC-009)
//!
//! Recursive / mutually-recursive types (`struct Node { next: Option<Node> }`) reach
//! their named ref back into the registry; an unbounded walk would stack-overflow.
//! The walk tracks a depth and, past [`MAX_DERIVE_DEPTH`], stops descending and
//! emits the same flagged [`UNKNOWN_SENTINEL`] placeholder + a "fill in" diagnostic
//! instead of recursing — so a cyclic type degrades to a finite, parseable scaffold
//! (analogous to [`json_to_ron`](crate::interop::json_to_ron)'s `MAX_JSON_DEPTH`
//! guard) and never crashes or hangs.

use ron_core::{CstDocument, TextRange};
use ron_types::extension::RonKind;
use ron_types::model::{
    Discriminator, Field, NodeKind, Primitive, TypeModel, TypeNode, TypeRef, Variant, VariantShape,
};

use crate::interop::loss::{LossKind, LossRecovery, LossReport, LossyConstruct};

/// The deterministic, identifiable parseable sentinel emitted for an `unknown`-typed
/// field (FR-010).
///
/// It is a RON **string literal** so it always parses regardless of the surrounding
/// shape, and its distinctive token makes it trivially greppable in the scaffold so
/// the developer can find every spot that needs a real value. Paired with a
/// [`LossKind::UnparseableRegion`]-coded "placeholder — fill in" diagnostic anchored
/// to its span (data-model §DerivedScaffold "never a hole").
pub const UNKNOWN_SENTINEL: &str = "\"<FILL_IN>\"";

/// The maximum type-walk depth the scaffold descends before emitting the
/// [`UNKNOWN_SENTINEL`] placeholder instead of recursing (FR-013, SC-009).
///
/// A recursive / cyclic type (`struct Node { next: Option<Node> }`) would otherwise
/// drive unbounded recursion; this bound makes the walk finite. Set well above any
/// realistic nesting yet small enough that a cycle can never stack-overflow. Mirrors
/// [`json_to_ron`](crate::interop::json_to_ron)'s `MAX_JSON_DEPTH` recursion guard.
pub const MAX_DERIVE_DEPTH: usize = 64;

/// The human-readable detail attached to every "placeholder — fill in" diagnostic
/// (FR-010). The diagnostic identity tests pin is the stable
/// [`LossKind::UnparseableRegion`] code, never this wording.
const FILL_IN_DETAIL: &str = "placeholder \u{2014} fill in (the field type is unknown/unresolved)";

/// The outcome of [`derive_scaffold`]: the parseable RON scaffold + the inline "fill
/// in" diagnostics (FR-010, data-model §DerivedScaffold).
///
/// [`text`](Self::text) is the emitted RON source; [`document`](Self::document) is
/// that same text parsed into a lossless `ron_core` CST, ready to open in a new tab.
/// [`fill_in_diagnostics`](Self::fill_in_diagnostics) is the one-list source of the
/// inline "placeholder — fill in" diagnostics (one per `unknown` sentinel /
/// depth-bounded cut), surfaced through the SAME E006 surface as conversion losses
/// via [`map_loss_report`](crate::diagnostics_map::map_loss_report) (FR-006/010).
#[derive(Debug)]
pub struct DeriveScaffold {
    /// The emitted RON scaffold source text (what [`document`](Self::document) was
    /// parsed from).
    pub text: String,
    /// The scaffold parsed into a lossless `ron_core` CST (always parses, SC-005).
    pub document: CstDocument,
    /// The inline "placeholder — fill in" diagnostics — one [`LossyConstruct`] per
    /// `unknown`-resolved sentinel (and per depth-bounded recursion cut), each
    /// anchored to the sentinel's span in [`text`](Self::text) (FR-010).
    pub fill_in_diagnostics: LossReport,
}

/// Derive an initial, parseable RON document from the named `root_type` in `model`
/// (FR-010, SC-005).
///
/// Walks the named type's [`TypeNode`] shape — consulted strictly as **data**
/// (ADR-0004) — emitting a **deterministic typed placeholder** per kind (see the
/// module docs for the per-kind policy). A field whose type resolves to
/// [`NodeKind::Unknown`] gets the [`UNKNOWN_SENTINEL`] parseable sentinel plus a
/// "placeholder — fill in" diagnostic anchored to it, so the scaffold always parses
/// and conforms to the field shape even for partially-`unknown` types (SC-005). The
/// walk is depth-bounded ([`MAX_DERIVE_DEPTH`]) so a recursive / cyclic type degrades
/// to a finite scaffold rather than stack-overflowing (FR-013, SC-009).
///
/// Returns the scaffold text, the parsed CST, and the fill-in diagnostics. When
/// `root_type` is **not registered** in `model` the result is an [`UNKNOWN_SENTINEL`]
/// scaffold with a single fill-in diagnostic — the app layer treats an unregistered
/// type as a "no type model available" message and creates no document (handled in
/// [`crate::app`], US3 AS2); callers can detect this case with
/// [`TypeModel::contains`] before calling.
///
/// Deterministic: the same `(model, root_type)` always yields a byte-identical
/// scaffold (FR-010).
#[must_use]
pub fn derive_scaffold(model: &TypeModel, root_type: &str) -> DeriveScaffold {
    let mut emitter = Emitter {
        model,
        diagnostics: LossReport::new(),
    };

    let mut text = String::new();
    let root = model.lookup(root_type);
    emitter.emit_node(&mut text, root, 0);
    text.push('\n');

    // The scaffold MUST parse cleanly (SC-005). `ron_core::parse` is error-tolerant
    // and never drops bytes, so this always yields a CST; the per-kind policy keeps
    // the emitted text grammar-valid.
    let document = ron_core::parse(&text);

    DeriveScaffold {
        text,
        document,
        fill_in_diagnostics: emitter.diagnostics,
    }
}

/// The recursive scaffold emitter. Holds the (read-only) `TypeModel` it consults as
/// data and the accumulating fill-in diagnostics.
struct Emitter<'a> {
    /// The bound type model, consulted strictly as data (ADR-0004).
    model: &'a TypeModel,
    /// The "placeholder — fill in" diagnostics (one per `unknown` sentinel / cut).
    diagnostics: LossReport,
}

impl Emitter<'_> {
    /// Resolve a [`TypeRef`] against the model to its target node.
    fn resolve<'r>(&'r self, type_ref: &'r TypeRef) -> Option<&'r TypeNode> {
        self.model.resolve(type_ref)
    }

    /// Emit the placeholder for `node` at `depth`. A `None` node (an unregistered
    /// named ref or an absent root) is treated as `unknown` (FR-010). Past
    /// [`MAX_DERIVE_DEPTH`] the walk emits the sentinel instead of recursing so a
    /// cyclic type stays finite (FR-013, SC-009).
    fn emit_node(&mut self, out: &mut String, node: Option<&TypeNode>, depth: usize) {
        // Bounded recursion: a recursive / cyclic type degrades to a flagged,
        // parseable sentinel rather than stack-overflowing (SC-009).
        if depth >= MAX_DERIVE_DEPTH {
            self.emit_unknown_sentinel(out);
            return;
        }
        let Some(node) = node else {
            // An unregistered named ref / absent root resolves to unknown (FR-010).
            self.emit_unknown_sentinel(out);
            return;
        };

        // RON-only kinds are recorded via the x-ron-* extension, not a NodeKind
        // variant, so check the extension first (char / unit / bytes).
        if let Some(ron_kind) = node.ron_extension.as_ref().and_then(|e| e.ron_kind) {
            match ron_kind {
                RonKind::Char => {
                    // A single deterministic sentinel char (`'\0'`).
                    out.push_str("'\\0'");
                    return;
                }
                RonKind::Unit => {
                    out.push_str("()");
                    return;
                }
                RonKind::Bytes => {
                    out.push_str("\"\"");
                    return;
                }
                // Tuple / Option / NonStringKeyMap are handled by their NodeKind arm
                // below (the kind carries the structural shape).
                RonKind::Tuple | RonKind::Option | RonKind::NonStringKeyMap => {}
            }
        }

        match &node.kind {
            NodeKind::Primitive { primitive } => self.emit_primitive(out, *primitive),
            NodeKind::Sequence { .. } => out.push_str("[]"),
            NodeKind::Map { .. } => out.push_str("{}"),
            NodeKind::Option { .. } => out.push_str("None"),
            NodeKind::Tuple { elements } => self.emit_tuple(out, elements, depth),
            NodeKind::Object { fields, .. } => self.emit_struct(out, fields, depth),
            NodeKind::Enum {
                variants,
                discriminator,
            } => self.emit_enum(out, variants, discriminator, depth),
            // The first-class `unknown` node → sentinel + "fill in" (FR-010).
            NodeKind::Unknown => self.emit_unknown_sentinel(out),
        }
    }

    /// Emit a scalar primitive's deterministic placeholder.
    fn emit_primitive(&mut self, out: &mut String, primitive: Primitive) {
        match primitive {
            Primitive::Boolean => out.push_str("false"),
            Primitive::Integer => out.push('0'),
            Primitive::Number => out.push_str("0.0"),
            Primitive::String => out.push_str("\"\""),
            // A bare `null` is not RON; a unit `()` is the closest typed default and
            // always parses + conforms to a null/unit field.
            Primitive::Null => out.push_str("()"),
        }
    }

    /// Emit an arity-filled tuple `(p, …)` of each element's placeholder (FR-010).
    fn emit_tuple(&mut self, out: &mut String, elements: &[TypeRef], depth: usize) {
        out.push('(');
        for (i, elem) in elements.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let elem_node = self.resolve(elem).map(ToOwned::to_owned);
            self.emit_node(out, elem_node.as_ref(), depth + 1);
        }
        out.push(')');
    }

    /// Emit a struct `(field: <placeholder>, …)` per declared field (FR-010). A field
    /// list is emitted in declaration order so the scaffold is deterministic and
    /// conforms to the type's field shape.
    fn emit_struct(&mut self, out: &mut String, fields: &[Field], depth: usize) {
        out.push('(');
        let mut first = true;
        for field in fields {
            if !first {
                out.push_str(", ");
            }
            first = false;
            out.push_str(&field.serialized_key);
            out.push_str(": ");
            let field_node = self.resolve(&field.value).map(ToOwned::to_owned);
            self.emit_node(out, field_node.as_ref(), depth + 1);
        }
        out.push(')');
    }

    /// Emit the **first-declared** enum variant with its payload placeholders
    /// (FR-010). With no variants the enum degrades to the `unknown` sentinel (an
    /// empty enum has no inhabitant to scaffold).
    fn emit_enum(
        &mut self,
        out: &mut String,
        variants: &[Variant],
        discriminator: &Discriminator,
        depth: usize,
    ) {
        let Some(variant) = variants.first() else {
            self.emit_unknown_sentinel(out);
            return;
        };
        // The scaffold emits RON-native variant syntax (`Variant`, `Variant(p)`,
        // `Variant(field: p)`); the serde tagging governs JSON, not the RON surface,
        // so the discriminator does not change the RON we emit here.
        let _ = discriminator;
        out.push_str(&variant.serialized_name);
        match &variant.shape {
            // A unit variant is a bare ident.
            VariantShape::Unit => {}
            // A newtype variant `V(inner)`.
            VariantShape::Newtype(inner) => {
                let inner_node = self.resolve(inner).map(ToOwned::to_owned);
                out.push('(');
                self.emit_node(out, inner_node.as_ref(), depth + 1);
                out.push(')');
            }
            // A tuple variant `V(a, b)`.
            VariantShape::Tuple(elems) => {
                out.push('(');
                for (i, elem) in elems.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    let elem_node = self.resolve(elem).map(ToOwned::to_owned);
                    self.emit_node(out, elem_node.as_ref(), depth + 1);
                }
                out.push(')');
            }
            // A struct variant `V(field: …)`.
            VariantShape::Struct(struct_fields) => {
                out.push('(');
                let mut first = true;
                for field in struct_fields {
                    if !first {
                        out.push_str(", ");
                    }
                    first = false;
                    out.push_str(&field.serialized_key);
                    out.push_str(": ");
                    let field_node = self.resolve(&field.value).map(ToOwned::to_owned);
                    self.emit_node(out, field_node.as_ref(), depth + 1);
                }
                out.push(')');
            }
        }
    }

    /// Emit the [`UNKNOWN_SENTINEL`] placeholder and record a "placeholder — fill in"
    /// diagnostic anchored to its span in `out` (FR-010).
    ///
    /// The diagnostic carries the stable [`LossKind::UnparseableRegion`] code (the
    /// "flagged placeholder" kind) — the same kind the convert-remainder /
    /// depth-exceeded placeholders use — so it renders through the SAME E006 surface
    /// as conversion losses. The span is the exact byte range of the sentinel token
    /// just emitted, so the inline diagnostic lands on it precisely.
    fn emit_unknown_sentinel(&mut self, out: &mut String) {
        let start = out.len();
        out.push_str(UNKNOWN_SENTINEL);
        let end = out.len();
        self.diagnostics.push(LossyConstruct::with_detail(
            LossKind::UnparseableRegion,
            TextRange::new(start, end),
            LossRecovery::LossyToExternal,
            FILL_IN_DETAIL,
        ));
    }
}

#[cfg(test)]
mod tests {
    //! T026 — derive-from-type scaffold generator unit coverage (FR-010, SC-005).

    use super::*;

    /// Build a model with a single registered named type for terse tests.
    fn model_with(name: &str, node: TypeNode) -> TypeModel {
        let mut model = TypeModel::new();
        model.insert_named(name, node);
        model
    }

    /// Parse-conformance helper: a scaffold MUST parse with zero diagnostics (SC-005).
    fn assert_parses(scaffold: &DeriveScaffold) {
        assert!(
            scaffold.document.diagnostics().is_empty(),
            "scaffold must parse cleanly, got diagnostics for: {:?}",
            scaffold.text
        );
    }

    #[test]
    fn struct_with_scalar_fields_uses_typed_placeholders() {
        let model = model_with(
            "Config",
            TypeNode::new(NodeKind::Object {
                fields: vec![
                    Field {
                        serialized_key: "count".into(),
                        value: TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
                        optional: false,
                        flatten: false,
                    },
                    Field {
                        serialized_key: "ratio".into(),
                        value: TypeRef::inline(TypeNode::primitive(Primitive::Number)),
                        optional: false,
                        flatten: false,
                    },
                    Field {
                        serialized_key: "name".into(),
                        value: TypeRef::inline(TypeNode::primitive(Primitive::String)),
                        optional: false,
                        flatten: false,
                    },
                    Field {
                        serialized_key: "enabled".into(),
                        value: TypeRef::inline(TypeNode::primitive(Primitive::Boolean)),
                        optional: false,
                        flatten: false,
                    },
                ],
                deny_unknown_fields: false,
            }),
        );
        let s = derive_scaffold(&model, "Config");
        assert_parses(&s);
        assert!(s.text.contains("count: 0"), "int → 0: {}", s.text);
        assert!(s.text.contains("ratio: 0.0"), "float → 0.0: {}", s.text);
        assert!(s.text.contains("name: \"\""), "string → \"\": {}", s.text);
        assert!(
            s.text.contains("enabled: false"),
            "bool → false: {}",
            s.text
        );
        assert!(s.fill_in_diagnostics.is_empty(), "no unknowns ⇒ no fill-in");
    }

    #[test]
    fn collection_and_ron_kinds_use_fixed_placeholders() {
        let model = model_with(
            "Bag",
            TypeNode::new(NodeKind::Object {
                fields: vec![
                    Field {
                        serialized_key: "list".into(),
                        value: TypeRef::inline(TypeNode::new(NodeKind::Sequence {
                            element: TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
                        })),
                        optional: false,
                        flatten: false,
                    },
                    Field {
                        serialized_key: "pair".into(),
                        value: TypeRef::inline(TypeNode::tuple(vec![
                            TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
                            TypeRef::inline(TypeNode::primitive(Primitive::String)),
                        ])),
                        optional: false,
                        flatten: false,
                    },
                    Field {
                        serialized_key: "ch".into(),
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
        let s = derive_scaffold(&model, "Bag");
        assert_parses(&s);
        assert!(s.text.contains("list: []"), "seq → []: {}", s.text);
        assert!(
            s.text.contains("pair: (0, \"\")"),
            "tuple arity-filled: {}",
            s.text
        );
        assert!(s.text.contains("ch: '\\0'"), "char → '\\0': {}", s.text);
        assert!(s.text.contains("maybe: None"), "Option → None: {}", s.text);
        assert!(s.text.contains("u: ()"), "unit → (): {}", s.text);
    }

    #[test]
    fn enum_uses_first_declared_variant_with_payload() {
        let model = model_with(
            "State",
            TypeNode::new(NodeKind::Enum {
                variants: vec![
                    Variant {
                        serialized_name: "Idle".into(),
                        shape: VariantShape::Unit,
                    },
                    Variant {
                        serialized_name: "Running".into(),
                        shape: VariantShape::Newtype(TypeRef::inline(TypeNode::primitive(
                            Primitive::Integer,
                        ))),
                    },
                ],
                discriminator: Discriminator::External,
            }),
        );
        let s = derive_scaffold(&model, "State");
        assert_parses(&s);
        // The FIRST-declared variant (Idle, a unit variant) is chosen deterministically.
        assert_eq!(
            s.text.trim(),
            "Idle",
            "first-declared unit variant: {}",
            s.text
        );
    }

    #[test]
    fn enum_first_variant_with_tuple_payload() {
        let model = model_with(
            "Shape",
            TypeNode::new(NodeKind::Enum {
                variants: vec![Variant {
                    serialized_name: "Rect".into(),
                    shape: VariantShape::Tuple(vec![
                        TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
                        TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
                    ]),
                }],
                discriminator: Discriminator::External,
            }),
        );
        let s = derive_scaffold(&model, "Shape");
        assert_parses(&s);
        assert_eq!(
            s.text.trim(),
            "Rect(0, 0)",
            "tuple-variant payload: {}",
            s.text
        );
    }

    #[test]
    fn unknown_field_gets_sentinel_and_fill_in_diagnostic() {
        let model = model_with(
            "Partial",
            TypeNode::new(NodeKind::Object {
                fields: vec![
                    Field {
                        serialized_key: "known".into(),
                        value: TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
                        optional: false,
                        flatten: false,
                    },
                    Field {
                        serialized_key: "mystery".into(),
                        value: TypeRef::inline(TypeNode::unknown()),
                        optional: false,
                        flatten: false,
                    },
                ],
                deny_unknown_fields: false,
            }),
        );
        let s = derive_scaffold(&model, "Partial");
        // Always parses + conforms even with a partially-unknown type (SC-005).
        assert_parses(&s);
        assert!(s.text.contains("known: 0"));
        assert!(
            s.text.contains(&format!("mystery: {UNKNOWN_SENTINEL}")),
            "unknown field → sentinel: {}",
            s.text
        );
        // Exactly one "fill in" diagnostic, anchored on the sentinel, RON-I0010 code.
        assert_eq!(s.fill_in_diagnostics.len(), 1);
        let c = &s.fill_in_diagnostics.constructs()[0];
        assert_eq!(c.kind(), LossKind::UnparseableRegion);
        assert_eq!(c.code(), "RON-I0010");
        // The diagnostic span covers the sentinel token in the emitted text.
        let span = c.source_range();
        assert_eq!(
            &s.text[span.start()..span.end()],
            UNKNOWN_SENTINEL,
            "diagnostic anchors exactly on the sentinel"
        );
    }

    #[test]
    fn unregistered_root_type_yields_a_fill_in_sentinel_scaffold() {
        let model = TypeModel::new();
        let s = derive_scaffold(&model, "NotRegistered");
        // An absent root → the sentinel scaffold + one fill-in diagnostic (the app
        // layer treats this as a "no type model available" message, US3 AS2).
        assert_parses(&s);
        assert_eq!(s.text.trim(), UNKNOWN_SENTINEL);
        assert_eq!(s.fill_in_diagnostics.len(), 1);
    }

    #[test]
    fn recursive_type_is_bounded_and_parses() {
        // struct Node { next: Option<Node> } — a cyclic type via a named ref.
        let mut model = TypeModel::new();
        model.insert_named(
            "Node",
            TypeNode::new(NodeKind::Object {
                fields: vec![Field {
                    serialized_key: "next".into(),
                    value: TypeRef::inline(TypeNode::option(TypeRef::named("Node"))),
                    optional: true,
                    flatten: false,
                }],
                deny_unknown_fields: false,
            }),
        );
        // The Option placeholder is `None`, so this particular cycle terminates at the
        // Option without recursing — but the depth guard still protects pathological
        // shapes. Assert it parses and does not blow the stack.
        let s = derive_scaffold(&model, "Node");
        assert_parses(&s);
        assert!(
            s.text.contains("next: None"),
            "Option short-circuits: {}",
            s.text
        );
    }

    #[test]
    fn deeply_recursive_newtype_is_depth_bounded() {
        // struct Wrap(Wrap) — a newtype cycle that would recurse forever without the
        // depth guard (no Option to short-circuit).
        let mut model = TypeModel::new();
        model.insert_named(
            "Wrap",
            TypeNode::new(NodeKind::Object {
                fields: vec![Field {
                    serialized_key: "inner".into(),
                    value: TypeRef::named("Wrap"),
                    optional: false,
                    flatten: false,
                }],
                deny_unknown_fields: false,
            }),
        );
        // Must terminate (depth-bounded) and parse, with a fill-in at the cut point.
        let s = derive_scaffold(&model, "Wrap");
        assert_parses(&s);
        assert!(
            !s.fill_in_diagnostics.is_empty(),
            "the depth-bounded cut records a fill-in placeholder"
        );
    }

    #[test]
    fn derive_is_deterministic() {
        let model = model_with(
            "Pair",
            TypeNode::tuple(vec![
                TypeRef::inline(TypeNode::primitive(Primitive::Integer)),
                TypeRef::inline(TypeNode::unknown()),
            ]),
        );
        let a = derive_scaffold(&model, "Pair");
        let b = derive_scaffold(&model, "Pair");
        assert_eq!(a.text, b.text, "same (type, model) ⇒ identical scaffold");
        assert_eq!(
            a.fill_in_diagnostics.len(),
            b.fill_in_diagnostics.len(),
            "deterministic fill-in count"
        );
    }
}
