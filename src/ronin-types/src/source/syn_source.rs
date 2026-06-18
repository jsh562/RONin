//! Static Rust type acquisition via the `syn` AST {TR-003, TR-004, TR-005, TR-006}.
//!
//! [`SynSource`] is the lowest-precedence [`TypeSource`]: it reads Rust source
//! (a single string, one file, or a whole crate/directory tree) with
//! [`syn::parse_file`] and lowers every `struct`/`enum` definition into the
//! normalized [`TypeModel`]. It is **syntax-only** — it sees exactly what the
//! text says and nothing the compiler would resolve (foreign crates, generic
//! instantiations, macro expansions). Those become first-class
//! [`NodeKind::Unknown`] nodes plus a [`DiagnosticCategory::UnresolvedType`]
//! diagnostic (TR-006, ADR-0004 Progressive Intelligence); acquisition NEVER
//! errors or panics.
//!
//! # Resolution model (TR-004)
//!
//! Acquisition runs in two passes:
//!
//! 1. **Collect.** Every `struct`/`enum` item across every parsed file is
//!    registered by its identifier into one name→definition map. Module nesting
//!    is flattened to the bare type name (the granularity RON binding uses); a
//!    duplicate bare name across the crate is flagged and the later one wins.
//! 2. **Lower + resolve.** Each definition's fields/variants are lowered. A field
//!    type that names a collected type becomes a [`TypeRef::Named`] `$ref` into
//!    `$defs`; built-in/std containers (`Vec`, `Option`, `HashMap`, tuples,
//!    primitives, `String`, `char`, …) are lowered structurally; anything else
//!    (foreign, generic parameter, unparsed macro output) becomes `unknown`.
//!
//! Because the collect pass is global, cross-file references resolve
//! transparently: a field in `a.rs` that names a type defined in `b.rs` resolves
//! to the same `$defs` entry (TR-004).
//!
//! # serde fidelity (TR-005)
//!
//! Container/field/variant `#[serde(...)]` attributes are honored so the model
//! matches serde's real (de)serialization. See [`serde_attr`] for the per-family
//! mapping and the documented limits (`with`/`from`/`into`/`try_from` → `unknown`).
//!
//! serde attribute semantics referenced throughout this module are defined by the
//! serde container/field/variant attribute documentation:
//! <https://serde.rs/container-attrs.html>, <https://serde.rs/field-attrs.html>,
//! and <https://serde.rs/variant-attrs.html>.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use walkdir::WalkDir;

use crate::diagnostics::{AcquisitionDiagnostic, DiagnosticCategory, DiagnosticLocation};
use crate::model::{
    Field, NodeKind, Primitive, TypeModel, TypeNode, TypeRef, Variant, VariantShape,
};
use crate::source::{Acquired, SourcePrecedence, TypeSource};

mod serde_attr;

use serde_attr::{ContainerAttrs, FieldAttrs, RenameRule, VariantAttrs};

/// A static Rust type source backed by the `syn` AST (TR-003).
///
/// Construct it from a single source string ([`SynSource::from_source`] /
/// [`SynSource::from_named_source`]), one file ([`SynSource::from_path`]), or a
/// whole crate/directory tree ([`SynSource::from_crate_dir`], TR-004). Files
/// that fail to read or parse are recorded as diagnostics, not errors — `acquire`
/// always returns a (possibly partial) model (TR-011).
#[derive(Debug, Clone)]
pub struct SynSource {
    /// Stable id for provenance/conflict diagnostics (`"syn"` or `"syn:<root>"`).
    id: String,
    /// The parsed input units; one entry per source string / file.
    units: Vec<SourceUnit>,
}

/// One source unit fed into acquisition (a string or a file's contents).
#[derive(Debug, Clone)]
struct SourceUnit {
    /// Human-readable label for diagnostics (file path or `"<source>"`).
    label: String,
    /// The raw Rust source text.
    text: String,
}

impl SynSource {
    /// Build a source from a single in-memory Rust source string. The source id
    /// is the generic `"syn"`.
    #[must_use]
    pub fn from_source(source: impl Into<String>) -> Self {
        Self {
            id: "syn".to_string(),
            units: vec![SourceUnit {
                label: "<source>".to_string(),
                text: source.into(),
            }],
        }
    }

    /// Build a source from a single in-memory Rust source string, tagging it with
    /// an explicit label used both as the source id (`"syn:<label>"`) and the
    /// diagnostic location.
    #[must_use]
    pub fn from_named_source(label: impl Into<String>, source: impl Into<String>) -> Self {
        let label = label.into();
        Self {
            id: format!("syn:{label}"),
            units: vec![SourceUnit {
                label,
                text: source.into(),
            }],
        }
    }

    /// Build a source from a single `.rs` file on disk (TR-003).
    ///
    /// A read failure is captured as an unparseable unit; `acquire` then emits a
    /// diagnostic rather than failing — the never-fail contract (TR-011).
    #[must_use]
    pub fn from_path(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        let label = path.display().to_string();
        let text = read_unit_text(path);
        Self {
            id: format!("syn:{label}"),
            units: vec![SourceUnit { label, text }],
        }
    }

    /// Build a source from a crate/directory root, parsing every `.rs` file under
    /// it and unioning their type definitions (TR-004).
    ///
    /// The walk is recursive and deterministic (entries sorted by file name).
    /// Files that cannot be read or parsed are recorded as diagnostics at
    /// `acquire` time; the rest still contribute their types.
    #[must_use]
    pub fn from_crate_dir(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        let id = format!("syn:{}", root.display());
        let mut units = Vec::new();
        for entry in WalkDir::new(root)
            .sort_by_file_name()
            .into_iter()
            .filter_map(Result::ok)
        {
            let path = entry.path();
            if !entry.file_type().is_file() {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            units.push(SourceUnit {
                label: path.display().to_string(),
                text: read_unit_text(path),
            });
        }
        Self { id, units }
    }
}

/// Read a file to a string, returning a deliberately unparseable sentinel (that
/// carries the io error) on failure so the parse-diagnostic explains it honestly.
fn read_unit_text(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|err| format!("// ronin-types: failed to read source file: {err}\n)("))
}

impl TypeSource for SynSource {
    fn source_id(&self) -> String {
        self.id.clone()
    }

    fn precedence(&self) -> SourcePrecedence {
        SourcePrecedence::Syn
    }

    fn acquire(&self) -> Acquired {
        let mut builder = ModelBuilder::new(self.id.clone());

        // Pass 1: parse every unit and collect its type definitions globally.
        for unit in &self.units {
            match syn::parse_file(&unit.text) {
                Ok(file) => builder.collect_items(&file.items, &unit.label),
                Err(err) => builder.push_diag(
                    DiagnosticCategory::UnsupportedConstruct,
                    &unit.label,
                    format!("could not parse Rust source: {err}"),
                    &unit.label,
                ),
            }
        }

        // Pass 2: lower every collected definition, resolving references against
        // the now-complete name set.
        builder.lower_all();

        // Pass 3: collapse newtype/transparent aliases now every node exists.
        builder.resolve_aliases();

        builder.finish()
    }
}

/// A collected (not-yet-lowered) type definition, tagged with its origin label.
enum CollectedDef {
    Struct {
        item: Box<syn::ItemStruct>,
        label: String,
    },
    Enum {
        item: Box<syn::ItemEnum>,
        label: String,
    },
}

/// Two-pass model builder: collects definitions, then lowers and resolves them.
struct ModelBuilder {
    source_id: String,
    /// name → collected definition (last write wins; duplicates diagnosed).
    defs: BTreeMap<String, CollectedDef>,
    /// The complete, immutable set of collected type names — used for reference
    /// resolution so it stays correct even while `defs` is drained in pass 2
    /// (e.g. a self-referential `struct Node { next: Vec<Node> }`).
    names: BTreeSet<String>,
    /// Deterministic first-seen order of definitions for stable `$defs` order.
    order: Vec<String>,
    /// name → target name for newtype/transparent aliases to collapse in pass 3.
    aliases: BTreeMap<String, String>,
    model: TypeModel,
    diagnostics: Vec<AcquisitionDiagnostic>,
}

impl ModelBuilder {
    fn new(source_id: String) -> Self {
        Self {
            source_id,
            defs: BTreeMap::new(),
            names: BTreeSet::new(),
            order: Vec::new(),
            aliases: BTreeMap::new(),
            model: TypeModel::new(),
            diagnostics: Vec::new(),
        }
    }

    fn push_diag(
        &mut self,
        category: DiagnosticCategory,
        subject: impl Into<String>,
        detail: impl Into<String>,
        location: impl Into<String>,
    ) {
        let diag = AcquisitionDiagnostic::new(category, subject, detail)
            .with_source_id(self.source_id.clone())
            .with_location(DiagnosticLocation {
                source: Some(location.into()),
                pointer: None,
            });
        self.diagnostics.push(diag);
    }

    // --- Pass 1: collect -----------------------------------------------------

    /// Walk a module item list (recursing into inline `mod` blocks) and register
    /// every struct/enum by its bare identifier.
    fn collect_items(&mut self, items: &[syn::Item], label: &str) {
        for item in items {
            match item {
                syn::Item::Struct(s) => self.collect(
                    s.ident.to_string(),
                    CollectedDef::Struct {
                        item: Box::new(s.clone()),
                        label: label.to_string(),
                    },
                    label,
                ),
                syn::Item::Enum(e) => self.collect(
                    e.ident.to_string(),
                    CollectedDef::Enum {
                        item: Box::new(e.clone()),
                        label: label.to_string(),
                    },
                    label,
                ),
                // Inline modules contribute their items at the flattened name
                // granularity RON binding uses.
                syn::Item::Mod(m) => {
                    if let Some((_, nested)) = &m.content {
                        self.collect_items(nested, label);
                    }
                }
                _ => {}
            }
        }
    }

    fn collect(&mut self, name: String, def: CollectedDef, label: &str) {
        if self.defs.contains_key(&name) {
            // serde (de)serializes one definition per type path; a duplicate bare
            // name across the crate is a genuine conflict worth flagging.
            self.push_diag(
                DiagnosticCategory::SourceConflict,
                name.clone(),
                "duplicate type name across the crate; later definition wins",
                label,
            );
        } else {
            self.order.push(name.clone());
        }
        self.names.insert(name.clone());
        self.defs.insert(name, def);
    }

    // --- Pass 2: lower -------------------------------------------------------

    /// Lower each collected definition into the model in collection order.
    fn lower_all(&mut self) {
        let order = self.order.clone();
        for name in order {
            // `defs` is keyed by name and not mutated during lowering, so this
            // remove-and-restore avoids cloning the whole definition.
            let Some(def) = self.defs.remove(&name) else {
                continue;
            };
            let node = match &def {
                CollectedDef::Struct { item, label } => self.lower_struct(&name, item, label),
                CollectedDef::Enum { item, label } => self.lower_enum(item, label),
            };
            self.model.insert_named(name.clone(), node);
            self.defs.insert(name, def);
        }
    }

    fn lower_struct(&mut self, name: &str, item: &syn::ItemStruct, label: &str) -> TypeNode {
        let container = ContainerAttrs::parse(&item.attrs, &mut |detail| {
            self.diagnostics.push(diag(
                &self.source_id,
                DiagnosticCategory::UnsupportedConstruct,
                item.ident.to_string(),
                detail,
                label,
            ));
        });

        // serde `transparent`: a one-field struct (de)serializes exactly as its
        // inner field. Collapse to that inner type's shape.
        // <https://serde.rs/container-attrs.html#transparent>
        if container.transparent {
            return self.lower_single_field_alias(
                name,
                single_field_type(&item.fields),
                "#[serde(transparent)] struct",
                label,
            );
        }

        match &item.fields {
            syn::Fields::Named(named) => {
                let mut fields = Vec::new();
                for f in &named.named {
                    if let Some(field) = self.lower_named_field(f, &container.rename_all, label) {
                        if field.flatten {
                            self.note_flatten(name, label);
                        }
                        fields.push(field);
                    }
                }
                TypeNode::new(NodeKind::Object {
                    fields,
                    deny_unknown_fields: container.deny_unknown_fields,
                })
            }
            syn::Fields::Unnamed(unnamed) => {
                if unnamed.unnamed.len() == 1 {
                    // Newtype struct: serde represents it transparently as the
                    // single inner value.
                    self.lower_single_field_alias(
                        name,
                        Some(&unnamed.unnamed.first().expect("len == 1").ty),
                        "newtype struct",
                        label,
                    )
                } else {
                    // Tuple struct: fixed-arity ordered payload.
                    let elems: Vec<TypeRef> = unnamed
                        .unnamed
                        .iter()
                        .map(|f| self.lower_type(&f.ty, label))
                        .collect();
                    TypeNode::tuple(elems)
                }
            }
            // Unit struct: serializes as unit `()`.
            syn::Fields::Unit => TypeNode::unit(),
        }
    }

    /// Lower a newtype / transparent struct that collapses onto a single inner
    /// type. Inline inner types become that node directly; a *named* inner type
    /// is recorded as an alias to collapse in pass 3 (the target may not be
    /// lowered yet), with the [`RonTypeExtension::unwrap_newtypes`] flag noted.
    fn lower_single_field_alias(
        &mut self,
        name: &str,
        inner_ty: Option<&syn::Type>,
        kind_desc: &str,
        label: &str,
    ) -> TypeNode {
        let Some(ty) = inner_ty else {
            self.push_diag(
                DiagnosticCategory::UnsupportedConstruct,
                name.to_string(),
                format!("{kind_desc} does not have exactly one field; treating as unknown"),
                label,
            );
            return TypeNode::unknown();
        };
        match self.lower_type(ty, label) {
            TypeRef::Inline(node) => *node,
            TypeRef::Named(target) => {
                // Defer: collapse onto the target node in pass 3.
                self.aliases.insert(name.to_string(), target);
                // Placeholder; pass 3 overwrites it with the resolved shape.
                TypeNode::unknown()
            }
        }
    }

    fn note_flatten(&mut self, subject: &str, label: &str) {
        // serde `flatten` inlines the target's fields. syn resolves the target by
        // reference only, so we mark the field and flag that flattened keys are
        // not expanded inline. <https://serde.rs/field-attrs.html#flatten>
        self.push_diag(
            DiagnosticCategory::UnsupportedConstruct,
            subject.to_string(),
            "serde `flatten` inlines the target's fields; syn resolves the target \
             by reference only, so flattened keys are not expanded inline here",
            label,
        );
    }

    fn lower_named_field(
        &mut self,
        field: &syn::Field,
        rename_all: &RenameRule,
        label: &str,
    ) -> Option<Field> {
        let ident = field
            .ident
            .as_ref()
            .expect("named field always has an ident")
            .to_string();
        let attrs = FieldAttrs::parse(&field.attrs, &mut |detail| {
            self.diagnostics.push(diag(
                &self.source_id,
                DiagnosticCategory::UnsupportedConstruct,
                ident.clone(),
                detail,
                label,
            ));
        });

        // serde `skip` removes the field from the data model entirely.
        // <https://serde.rs/field-attrs.html#skip>
        if attrs.skip {
            return None;
        }

        let serialized_key = attrs
            .rename
            .clone()
            .unwrap_or_else(|| rename_all.apply_to_field(&ident));

        // serde `with`/`from`/`into`/`try_from` swap in a converter whose wire
        // shape syn cannot see; the effective field type is unknown-from-source.
        // <https://serde.rs/field-attrs.html#with>
        let value = if attrs.opaque_conversion {
            self.push_diag(
                DiagnosticCategory::UnresolvedType,
                serialized_key.clone(),
                "field uses serde `with`/`from`/`into`/`try_from`; the converted \
                 wire shape is not visible to static syn analysis",
                label,
            );
            TypeRef::inline(TypeNode::unknown())
        } else {
            self.lower_type(&field.ty, label)
        };

        // A field is optional if it is an `Option`, or carries serde
        // `default` / `skip_serializing_if`.
        // <https://serde.rs/field-attrs.html#default>
        let is_option =
            matches!(&value, TypeRef::Inline(n) if matches!(n.kind, NodeKind::Option { .. }));
        let optional = is_option || attrs.default || attrs.skip_serializing_if;

        Some(Field {
            serialized_key,
            value,
            optional,
            flatten: attrs.flatten,
        })
    }

    fn lower_enum(&mut self, item: &syn::ItemEnum, label: &str) -> TypeNode {
        let container = ContainerAttrs::parse(&item.attrs, &mut |detail| {
            self.diagnostics.push(diag(
                &self.source_id,
                DiagnosticCategory::UnsupportedConstruct,
                item.ident.to_string(),
                detail,
                label,
            ));
        });

        let mut variants = Vec::new();
        for v in &item.variants {
            let vattrs = VariantAttrs::parse(&v.attrs, &mut |detail| {
                self.diagnostics.push(diag(
                    &self.source_id,
                    DiagnosticCategory::UnsupportedConstruct,
                    v.ident.to_string(),
                    detail,
                    label,
                ));
            });
            // serde `skip` drops the variant from the data model.
            // <https://serde.rs/variant-attrs.html#skip>
            if vattrs.skip {
                continue;
            }
            let serialized_name = vattrs
                .rename
                .clone()
                .unwrap_or_else(|| container.rename_all.apply_to_variant(&v.ident.to_string()));
            let shape = self.lower_variant_shape(v, &container.rename_all_fields, label);
            variants.push(Variant {
                serialized_name,
                shape,
            });
        }

        TypeNode::new(NodeKind::Enum {
            variants,
            discriminator: container.discriminator(),
        })
    }

    fn lower_variant_shape(
        &mut self,
        v: &syn::Variant,
        rename_all_fields: &RenameRule,
        label: &str,
    ) -> VariantShape {
        match &v.fields {
            syn::Fields::Unit => VariantShape::Unit,
            syn::Fields::Unnamed(unnamed) => {
                let elems: Vec<TypeRef> = unnamed
                    .unnamed
                    .iter()
                    .map(|f| self.lower_type(&f.ty, label))
                    .collect();
                if elems.len() == 1 {
                    VariantShape::Newtype(elems.into_iter().next().expect("len == 1"))
                } else {
                    VariantShape::Tuple(elems)
                }
            }
            syn::Fields::Named(named) => {
                let mut fields = Vec::new();
                for f in &named.named {
                    if let Some(field) = self.lower_named_field(f, rename_all_fields, label) {
                        fields.push(field);
                    }
                }
                VariantShape::Struct(fields)
            }
        }
    }

    // --- type-reference resolution (TR-004, TR-006) --------------------------

    /// Lower a `syn::Type` into a [`TypeRef`], resolving against collected defs.
    ///
    /// Built-in containers/primitives are lowered structurally (inline); a name
    /// matching a collected definition becomes a `$ref`; anything else (foreign
    /// crate, generic parameter, macro output, fn pointer, etc.) becomes an
    /// inline `unknown` node plus an `UnresolvedType` diagnostic (TR-006).
    fn lower_type(&mut self, ty: &syn::Type, label: &str) -> TypeRef {
        match ty {
            syn::Type::Path(tp) => self.lower_type_path(tp, label),
            syn::Type::Tuple(t) => {
                if t.elems.is_empty() {
                    return TypeRef::inline(TypeNode::unit()); // `()` unit type
                }
                let elems: Vec<TypeRef> =
                    t.elems.iter().map(|e| self.lower_type(e, label)).collect();
                TypeRef::inline(TypeNode::tuple(elems))
            }
            // `[T; N]` / `[T]` — a homogeneous sequence of the element type.
            syn::Type::Array(arr) => {
                let element = self.lower_type(&arr.elem, label);
                TypeRef::inline(TypeNode::new(NodeKind::Sequence { element }))
            }
            syn::Type::Slice(s) => {
                let element = self.lower_type(&s.elem, label);
                TypeRef::inline(TypeNode::new(NodeKind::Sequence { element }))
            }
            // `&T` / `&mut T` — serde delegates to the referent's shape.
            syn::Type::Reference(r) => self.lower_type(&r.elem, label),
            // Transparent grouping wrappers — unwrap.
            syn::Type::Paren(p) => self.lower_type(&p.elem, label),
            syn::Type::Group(g) => self.lower_type(&g.elem, label),
            // Fn pointers, trait objects, impl Trait, macros in type position,
            // etc. have no statically-known data shape.
            other => {
                let rendered = render_type(other);
                self.push_diag(
                    DiagnosticCategory::UnresolvedType,
                    rendered.clone(),
                    format!("type `{rendered}` is not statically resolvable to a data shape"),
                    label,
                );
                TypeRef::inline(TypeNode::unknown())
            }
        }
    }

    fn lower_type_path(&mut self, tp: &syn::TypePath, label: &str) -> TypeRef {
        // A qualified `<T as Trait>::Assoc` path has no statically-known shape.
        if tp.qself.is_some() {
            let rendered = render_type(&syn::Type::Path(tp.clone()));
            self.push_diag(
                DiagnosticCategory::UnresolvedType,
                rendered,
                "qualified associated type path is not statically resolvable",
                label,
            );
            return TypeRef::inline(TypeNode::unknown());
        }

        let Some(segment) = tp.path.segments.last() else {
            return TypeRef::inline(TypeNode::unknown());
        };
        let ident = segment.ident.to_string();
        let args = generic_args(&segment.arguments);

        // 1. std/builtin scalars and string types (only when un-parameterized).
        if args.is_empty() {
            if let Some(prim) = primitive_for(&ident) {
                return match prim {
                    PrimitiveKind::Scalar(p) => TypeRef::inline(TypeNode::primitive(p)),
                    PrimitiveKind::Char => TypeRef::inline(TypeNode::char_()),
                };
            }
        }

        // 2. std containers we model structurally.
        match ident.as_str() {
            "Option" => {
                let inner = self.single_arg(&args, &ident, label);
                return TypeRef::inline(TypeNode::option(inner));
            }
            // Transparent smart-pointer / wrapper newtypes: serde delegates to
            // the inner type's shape.
            "Box" | "Rc" | "Arc" | "Cell" | "RefCell" | "Mutex" | "RwLock" | "Cow" | "Reverse"
            | "Wrapping" => {
                return self.single_arg(&args, &ident, label);
            }
            "Vec" | "VecDeque" | "LinkedList" | "HashSet" | "BTreeSet" | "BinaryHeap" => {
                let element = self.single_arg(&args, &ident, label);
                return TypeRef::inline(TypeNode::new(NodeKind::Sequence { element }));
            }
            "HashMap" | "BTreeMap" => return self.lower_map(&args, &ident, label),
            _ => {}
        }

        // 3. A collected user type → $ref (cross-file resolution, TR-004). Uses
        //    the immutable name set so self-references resolve while `defs` is
        //    being drained in pass 2.
        if self.names.contains(&ident) {
            if !args.is_empty() {
                // A generic *instantiation* of a user type (`Wrapper<T>`): the
                // model holds only the un-instantiated definition, so the
                // instantiation's concrete shape is not statically known.
                self.push_diag(
                    DiagnosticCategory::UnresolvedType,
                    ident.clone(),
                    format!(
                        "generic instantiation `{}` is not statically resolvable; \
                         referencing the base definition's shape",
                        render_type(&syn::Type::Path(tp.clone()))
                    ),
                    label,
                );
            }
            return TypeRef::named(ident);
        }

        // 4. A name that is neither std nor a collected type: a generic type
        //    parameter (`T`), a foreign-crate type, or a macro-generated type.
        let rendered = render_type(&syn::Type::Path(tp.clone()));
        self.push_diag(
            DiagnosticCategory::UnresolvedType,
            rendered.clone(),
            format!(
                "type `{rendered}` is not defined in the analyzed source (foreign \
                 crate, generic parameter, or macro-generated); recorded as unknown"
            ),
            label,
        );
        TypeRef::inline(TypeNode::unknown())
    }

    /// Resolve the sole generic argument of a single-parameter container.
    fn single_arg(&mut self, args: &[syn::Type], container: &str, label: &str) -> TypeRef {
        match args.first() {
            Some(ty) => self.lower_type(ty, label),
            None => {
                self.push_diag(
                    DiagnosticCategory::UnresolvedType,
                    container.to_string(),
                    format!("`{container}` is missing its type argument; element unknown"),
                    label,
                );
                TypeRef::inline(TypeNode::unknown())
            }
        }
    }

    /// Lower a `HashMap<K, V>` / `BTreeMap<K, V>`, distinguishing string-keyed
    /// (plain JSON object) from non-string-keyed maps (carries the RON ext).
    fn lower_map(&mut self, args: &[syn::Type], container: &str, label: &str) -> TypeRef {
        let key = match args.first() {
            Some(k) => self.lower_type(k, label),
            None => {
                self.push_diag(
                    DiagnosticCategory::UnresolvedType,
                    container.to_string(),
                    format!("`{container}` is missing its key type; key unknown"),
                    label,
                );
                TypeRef::inline(TypeNode::unknown())
            }
        };
        let value = match args.get(1) {
            Some(v) => self.lower_type(v, label),
            None => {
                self.push_diag(
                    DiagnosticCategory::UnresolvedType,
                    container.to_string(),
                    format!("`{container}` is missing its value type; value unknown"),
                    label,
                );
                TypeRef::inline(TypeNode::unknown())
            }
        };

        if is_string_keyed(&key) {
            TypeRef::inline(TypeNode::new(NodeKind::Map { key, value }))
        } else {
            TypeRef::inline(TypeNode::non_string_key_map(key, value))
        }
    }

    // --- Pass 3: alias collapse ---------------------------------------------

    /// Collapse newtype/transparent aliases over *named* inner types: replace the
    /// alias' placeholder node with a clone of the resolved target, tagged with
    /// `unwrap_newtypes` so consumers know the wrapper is written unwrapped.
    fn resolve_aliases(&mut self) {
        let aliases: Vec<(String, String)> = self
            .aliases
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (alias, target) in aliases {
            // Follow chained aliases to a concrete (non-alias) target.
            let resolved = self.resolve_alias_target(&target);
            let node = match resolved {
                Some(name) => match self.model.lookup(&name).cloned() {
                    Some(mut node) => {
                        let mut ext = node.ron_extension.take().unwrap_or_default();
                        ext.unwrap_newtypes = true;
                        node.ron_extension = Some(ext);
                        node
                    }
                    None => TypeNode::unknown(),
                },
                None => TypeNode::unknown(),
            };
            self.model.insert_named(alias, node);
        }
    }

    /// Follow an alias chain to the first non-alias name (cycle-safe).
    fn resolve_alias_target(&self, start: &str) -> Option<String> {
        let mut seen = std::collections::BTreeSet::new();
        let mut current = start.to_string();
        loop {
            if !seen.insert(current.clone()) {
                return None; // cycle of aliases; bail to unknown
            }
            match self.aliases.get(&current) {
                Some(next) => current = next.clone(),
                None => return Some(current),
            }
        }
    }

    fn finish(mut self) -> Acquired {
        // Diagnostics travel both with the model and on the Acquired wrapper (the
        // contract's explicit findings channel).
        self.model.diagnostics = self.diagnostics.clone();
        Acquired {
            model: self.model,
            diagnostics: self.diagnostics,
        }
    }
}

/// Build a diagnostic outside `&mut self` (used by attribute-parser callbacks to
/// sidestep a double mutable borrow of the builder).
fn diag(
    source_id: &str,
    category: DiagnosticCategory,
    subject: impl Into<String>,
    detail: impl Into<String>,
    location: impl Into<String>,
) -> AcquisitionDiagnostic {
    AcquisitionDiagnostic::new(category, subject, detail)
        .with_source_id(source_id.to_string())
        .with_location(DiagnosticLocation {
            source: Some(location.into()),
            pointer: None,
        })
}

/// A primitive classification used while lowering a type path.
enum PrimitiveKind {
    Scalar(Primitive),
    Char,
}

/// Map a std scalar/string type name to its model primitive, if it is one.
fn primitive_for(ident: &str) -> Option<PrimitiveKind> {
    Some(match ident {
        "bool" => PrimitiveKind::Scalar(Primitive::Boolean),
        "i8" | "i16" | "i32" | "i64" | "i128" | "isize" | "u8" | "u16" | "u32" | "u64" | "u128"
        | "usize" => PrimitiveKind::Scalar(Primitive::Integer),
        "f32" | "f64" => PrimitiveKind::Scalar(Primitive::Number),
        "String" | "str" => PrimitiveKind::Scalar(Primitive::String),
        "char" => PrimitiveKind::Char,
        _ => return None,
    })
}

/// `true` when a map key lowers to a JSON-string-compatible key (so the map is a
/// plain JSON object rather than a non-string-key map).
fn is_string_keyed(key: &TypeRef) -> bool {
    match key {
        TypeRef::Inline(node) => {
            matches!(
                &node.kind,
                NodeKind::Primitive {
                    primitive: Primitive::String,
                }
            ) && node.ron_extension.is_none()
        } // plain `String`, not `char`/bytes
        // A named key's shape is unknown here; treat as non-string-keyed (the
        // serde-honest default: most named keys are enums/integers).
        TypeRef::Named(_) => false,
    }
}

/// Extract the ordered type arguments of a path segment (`<A, B>`), ignoring
/// lifetime/const/binding args (they carry no data shape).
fn generic_args(arguments: &syn::PathArguments) -> Vec<syn::Type> {
    match arguments {
        syn::PathArguments::AngleBracketed(ab) => ab
            .args
            .iter()
            .filter_map(|a| match a {
                syn::GenericArgument::Type(t) => Some(t.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The single field's type of a struct that has exactly one field, else `None`.
fn single_field_type(fields: &syn::Fields) -> Option<&syn::Type> {
    match fields {
        syn::Fields::Named(named) if named.named.len() == 1 => {
            Some(&named.named.first().expect("len == 1").ty)
        }
        syn::Fields::Unnamed(unnamed) if unnamed.unnamed.len() == 1 => {
            Some(&unnamed.unnamed.first().expect("len == 1").ty)
        }
        _ => None,
    }
}

/// Render a `syn::Type` to a compact source-ish string for diagnostics, without
/// pulling in `quote`/`proc-macro2` as direct deps (path segments + a coarse
/// fallback are enough for human-readable diagnostic subjects).
fn render_type(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(tp) => {
            let mut parts: Vec<String> = tp
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect();
            // Annotate generic args on the last segment for readability.
            if let Some(last) = tp.path.segments.last() {
                let args = generic_args(&last.arguments);
                if !args.is_empty() {
                    let rendered: Vec<String> = args.iter().map(render_type).collect();
                    let joined = format!(
                        "{}<{}>",
                        parts.pop().unwrap_or_default(),
                        rendered.join(", ")
                    );
                    parts.push(joined);
                }
            }
            parts.join("::")
        }
        syn::Type::Reference(r) => format!("&{}", render_type(&r.elem)),
        syn::Type::Slice(s) => format!("[{}]", render_type(&s.elem)),
        syn::Type::Array(a) => format!("[{}; _]", render_type(&a.elem)),
        syn::Type::Tuple(t) => {
            let parts: Vec<String> = t.elems.iter().map(render_type).collect();
            format!("({})", parts.join(", "))
        }
        syn::Type::Paren(p) => render_type(&p.elem),
        syn::Type::Group(g) => render_type(&g.elem),
        syn::Type::Ptr(_) => "<raw pointer>".to_string(),
        syn::Type::BareFn(_) => "<fn pointer>".to_string(),
        syn::Type::TraitObject(_) => "<dyn trait>".to_string(),
        syn::Type::ImplTrait(_) => "<impl trait>".to_string(),
        syn::Type::Macro(_) => "<macro type>".to_string(),
        syn::Type::Infer(_) => "_".to_string(),
        syn::Type::Never(_) => "!".to_string(),
        _ => "<unknown type>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::RonKind;

    fn acquire(src: &str) -> Acquired {
        SynSource::from_source(src).acquire()
    }

    #[test]
    fn named_struct_maps_to_object_with_fields() {
        let acq = acquire("struct Point { x: i32, y: f64, name: String }");
        let node = acq.model.lookup("Point").expect("Point registered");
        let NodeKind::Object { fields, .. } = &node.kind else {
            panic!("expected object");
        };
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0].serialized_key, "x");
        let x = acq.model.resolve(&fields[0].value).unwrap();
        assert!(matches!(
            x.kind,
            NodeKind::Primitive {
                primitive: Primitive::Integer
            }
        ));
        assert!(acq.diagnostics.is_empty(), "no spurious diagnostics");
    }

    #[test]
    fn tuple_struct_maps_to_tuple() {
        let acq = acquire("struct Pair(i32, String);");
        let node = acq.model.lookup("Pair").unwrap();
        let NodeKind::Tuple { elements } = &node.kind else {
            panic!("expected tuple, got {:?}", node.kind);
        };
        assert_eq!(elements.len(), 2);
    }

    #[test]
    fn unit_struct_maps_to_unit() {
        let acq = acquire("struct Marker;");
        let node = acq.model.lookup("Marker").unwrap();
        assert_eq!(
            node.ron_extension.as_ref().unwrap().ron_kind,
            Some(RonKind::Unit)
        );
    }

    #[test]
    fn newtype_struct_collapses_to_inner_inline() {
        let acq = acquire("struct Meters(f64);");
        let node = acq.model.lookup("Meters").unwrap();
        assert!(matches!(
            node.kind,
            NodeKind::Primitive {
                primitive: Primitive::Number
            }
        ));
    }

    #[test]
    fn newtype_struct_over_named_collapses_via_alias() {
        let acq = acquire(
            r#"
            struct Inner { v: i32 }
            struct Wrapper(Inner);
        "#,
        );
        let node = acq.model.lookup("Wrapper").unwrap();
        // Collapsed onto Inner's object shape, tagged unwrap_newtypes.
        assert!(matches!(node.kind, NodeKind::Object { .. }));
        assert!(node.ron_extension.as_ref().unwrap().unwrap_newtypes);
    }

    #[test]
    fn enum_variants_lower_to_shapes() {
        let acq = acquire(
            r#"
            enum Shape {
                Empty,
                Circle(f64),
                Rect(f64, f64),
                Named { w: u32, h: u32 },
            }
        "#,
        );
        let node = acq.model.lookup("Shape").unwrap();
        let NodeKind::Enum { variants, .. } = &node.kind else {
            panic!("expected enum");
        };
        assert_eq!(variants.len(), 4);
        assert!(matches!(variants[0].shape, VariantShape::Unit));
        assert!(matches!(variants[1].shape, VariantShape::Newtype(_)));
        assert!(matches!(variants[2].shape, VariantShape::Tuple(_)));
        assert!(matches!(variants[3].shape, VariantShape::Struct(_)));
    }

    #[test]
    fn cross_type_reference_resolves_to_named() {
        let acq = acquire(
            r#"
            struct Inner { v: i32 }
            struct Outer { inner: Inner }
        "#,
        );
        let outer = acq.model.lookup("Outer").unwrap();
        let NodeKind::Object { fields, .. } = &outer.kind else {
            panic!("object");
        };
        assert_eq!(fields[0].value.as_named(), Some("Inner"));
    }

    #[test]
    fn foreign_type_becomes_unknown_with_diagnostic() {
        let acq = acquire("struct Holder { f: some_crate::Foreign }");
        let node = acq.model.lookup("Holder").unwrap();
        let NodeKind::Object { fields, .. } = &node.kind else {
            panic!("object");
        };
        let v = acq.model.resolve(&fields[0].value).unwrap();
        assert!(v.is_unknown());
        assert!(acq
            .diagnostics
            .iter()
            .any(|d| d.category == DiagnosticCategory::UnresolvedType));
    }

    #[test]
    fn vec_and_option_lower_structurally() {
        let acq = acquire("struct S { items: Vec<i32>, maybe: Option<String> }");
        let node = acq.model.lookup("S").unwrap();
        let NodeKind::Object { fields, .. } = &node.kind else {
            panic!("object");
        };
        let items = acq.model.resolve(&fields[0].value).unwrap();
        assert!(matches!(items.kind, NodeKind::Sequence { .. }));
        let maybe = acq.model.resolve(&fields[1].value).unwrap();
        assert!(matches!(maybe.kind, NodeKind::Option { .. }));
        assert!(fields[1].optional, "Option field is optional");
    }

    #[test]
    fn never_panics_on_garbage() {
        let acq = acquire("this is not valid rust @@@@");
        assert!(acq.model.is_empty());
        assert!(!acq.diagnostics.is_empty());
    }
}
