//! Typed accessors over the CST (TR-010, OBJ4).
//!
//! The CST exposed by [`crate::syntax`] is *untyped*: every interior node is a
//! [`SyntaxNode`] tagged with a [`SyntaxKind`]. This module layers a thin,
//! zero-copy *typed* view on top of it so callers can navigate RON constructs by
//! name — `Struct::fields()`, `Map::entries()`, `MapEntry::key()`/`value()`,
//! `List::items()`, `Tuple::items()`, `EnumVariant::name()`, `Document::value()`
//! — without matching on `SyntaxKind` themselves.
//!
//! # Design
//!
//! Each typed wrapper is a newtype around a [`SyntaxNode`] of the matching kind.
//! Construction goes through `cast`, which returns `None` for a node of the
//! wrong kind, so a typed handle always refers to a node of its declared kind
//! (defensive: an `Error`-recovered tree never produces a mis-typed accessor).
//! Accessors return only `ronin-core` types — other typed wrappers, [`Value`],
//! [`SyntaxNode`], [`SyntaxToken`], or `&str` — so **no rowan type ever leaks**
//! (INV-7 / TR-009). The wrappers borrow the tree; they own nothing and copy no
//! source text.
//!
//! Trivia is transparent here: child *nodes* skip trivia tokens automatically
//! (trivia are leaf tokens, never nodes), so navigation is unaffected by
//! whitespace/comments while the underlying tree stays byte-lossless.

use crate::syntax::{SyntaxKind, SyntaxNode, SyntaxToken};

/// A typed RON value: the classified wrapper for any value-position node.
///
/// Returned by the value accessors ([`Document::value`], [`StructField::value`],
/// [`MapEntry::key`]/[`MapEntry::value`], and the `items()` iterators). The
/// `Error` / unknown arm keeps navigation total over error-recovered trees
/// (INV-3): a malformed value is still reachable as [`Value::Error`] rather than
/// silently dropped.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Value {
    /// A named or anonymous struct `Name( field: v, .. )` / `( field: v, .. )`.
    Struct(Struct),
    /// A positional tuple / tuple-struct `( a, b, c )`.
    Tuple(Tuple),
    /// A list / sequence `[ a, b, c ]`.
    List(List),
    /// A map `{ k: v, .. }` (keys may be non-string values).
    Map(Map),
    /// An enum variant: bare `Ident`, or `Ident(..)` / `Ident{..}` payload.
    EnumVariant(EnumVariant),
    /// The unit value `()`.
    Unit(Unit),
    /// A scalar literal (int, float, string, raw string, char, bool).
    Literal(Literal),
    /// An unparseable / recovered value node (`Error` kind). Kept reachable so
    /// navigation is total over error-recovered trees (INV-3).
    Error(SyntaxNode),
}

impl Value {
    /// Classify a value-position [`SyntaxNode`] into a typed [`Value`].
    ///
    /// Returns `None` only for a node whose kind is not a value (e.g. `Root`,
    /// `StructField`, `MapEntry`, `ExtensionAttr`) — callers that already hold a
    /// value-position node always get `Some`.
    #[must_use]
    pub fn cast(node: SyntaxNode) -> Option<Self> {
        Some(match node.kind() {
            SyntaxKind::Struct => Self::Struct(Struct(node)),
            SyntaxKind::Tuple => Self::Tuple(Tuple(node)),
            SyntaxKind::List => Self::List(List(node)),
            SyntaxKind::Map => Self::Map(Map(node)),
            SyntaxKind::EnumVariant => Self::EnumVariant(EnumVariant(node)),
            SyntaxKind::Unit => Self::Unit(Unit(node)),
            SyntaxKind::Literal => Self::Literal(Literal(node)),
            SyntaxKind::Error => Self::Error(node),
            _ => return None,
        })
    }

    /// The underlying [`SyntaxNode`], regardless of variant.
    #[must_use]
    pub fn syntax(&self) -> &SyntaxNode {
        match self {
            Self::Struct(n) => n.syntax(),
            Self::Tuple(n) => n.syntax(),
            Self::List(n) => n.syntax(),
            Self::Map(n) => n.syntax(),
            Self::EnumVariant(n) => n.syntax(),
            Self::Unit(n) => n.syntax(),
            Self::Literal(n) => n.syntax(),
            Self::Error(n) => n,
        }
    }
}

/// Cast the first value-position child node of `parent` into a [`Value`].
///
/// Used by accessors that contain exactly one value (struct field, map-entry
/// key/value, document root). Skips trivia and non-value nodes (e.g. a struct
/// field's name is a token, not a node).
fn first_value_child(parent: &SyntaxNode) -> Option<Value> {
    parent.children().find_map(Value::cast)
}

/// The typed root of a parsed document.
///
/// Wraps the [`SyntaxKind::Root`] node and exposes the single top-level value
/// plus any leading extension attributes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Document(SyntaxNode);

impl Document {
    /// Wrap a [`SyntaxKind::Root`] node, or `None` for any other kind.
    #[must_use]
    pub fn cast(node: SyntaxNode) -> Option<Self> {
        (node.kind() == SyntaxKind::Root).then_some(Self(node))
    }

    /// The underlying root [`SyntaxNode`].
    #[must_use]
    pub fn syntax(&self) -> &SyntaxNode {
        &self.0
    }

    /// The single top-level [`Value`], if the document has one (absent for an
    /// empty / trivia-only file).
    #[must_use]
    pub fn value(&self) -> Option<Value> {
        first_value_child(&self.0)
    }

    /// The leading extension attributes `#![enable(..)]`, in source order.
    pub fn extension_attrs(&self) -> impl Iterator<Item = ExtensionAttr> + '_ {
        self.0.children().filter_map(ExtensionAttr::cast)
    }
}

/// A named or anonymous struct: `Name( field: v, .. )` or `( field: v, .. )`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Struct(SyntaxNode);

impl Struct {
    /// Wrap a [`SyntaxKind::Struct`] node, or `None` for any other kind.
    #[must_use]
    pub fn cast(node: SyntaxNode) -> Option<Self> {
        (node.kind() == SyntaxKind::Struct).then_some(Self(node))
    }

    /// The underlying [`SyntaxNode`].
    #[must_use]
    pub fn syntax(&self) -> &SyntaxNode {
        &self.0
    }

    /// The struct's name token (`Ident`) for a named struct, or `None` for an
    /// anonymous `( field: v, .. )` struct.
    #[must_use]
    pub fn name(&self) -> Option<SyntaxToken> {
        self.0.first_token_of(SyntaxKind::Ident)
    }

    /// The struct's name as a string slice, if named.
    #[must_use]
    pub fn name_text(&self) -> Option<String> {
        self.name().map(|t| t.text().to_string())
    }

    /// The `field: value` entries, in source order.
    pub fn fields(&self) -> impl Iterator<Item = StructField> + '_ {
        self.0.children().filter_map(StructField::cast)
    }
}

/// A single `field: value` entry inside a [`Struct`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StructField(SyntaxNode);

impl StructField {
    /// Wrap a [`SyntaxKind::StructField`] node, or `None` for any other kind.
    #[must_use]
    pub fn cast(node: SyntaxNode) -> Option<Self> {
        (node.kind() == SyntaxKind::StructField).then_some(Self(node))
    }

    /// The underlying [`SyntaxNode`].
    #[must_use]
    pub fn syntax(&self) -> &SyntaxNode {
        &self.0
    }

    /// The field-name token (`Ident`), if present.
    #[must_use]
    pub fn name(&self) -> Option<SyntaxToken> {
        self.0.first_token_of(SyntaxKind::Ident)
    }

    /// The field name as a string, if present.
    #[must_use]
    pub fn name_text(&self) -> Option<String> {
        self.name().map(|t| t.text().to_string())
    }

    /// The field's [`Value`], if present (absent in a recovered partial field).
    #[must_use]
    pub fn value(&self) -> Option<Value> {
        first_value_child(&self.0)
    }
}

/// A positional tuple / tuple-struct `( a, b, c )`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Tuple(SyntaxNode);

impl Tuple {
    /// Wrap a [`SyntaxKind::Tuple`] node, or `None` for any other kind.
    #[must_use]
    pub fn cast(node: SyntaxNode) -> Option<Self> {
        (node.kind() == SyntaxKind::Tuple).then_some(Self(node))
    }

    /// The underlying [`SyntaxNode`].
    #[must_use]
    pub fn syntax(&self) -> &SyntaxNode {
        &self.0
    }

    /// The positional element [`Value`]s, in source order.
    pub fn items(&self) -> impl Iterator<Item = Value> + '_ {
        self.0.children().filter_map(Value::cast)
    }
}

/// A list / sequence `[ a, b, c ]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct List(SyntaxNode);

impl List {
    /// Wrap a [`SyntaxKind::List`] node, or `None` for any other kind.
    #[must_use]
    pub fn cast(node: SyntaxNode) -> Option<Self> {
        (node.kind() == SyntaxKind::List).then_some(Self(node))
    }

    /// The underlying [`SyntaxNode`].
    #[must_use]
    pub fn syntax(&self) -> &SyntaxNode {
        &self.0
    }

    /// The element [`Value`]s, in source order.
    pub fn items(&self) -> impl Iterator<Item = Value> + '_ {
        self.0.children().filter_map(Value::cast)
    }
}

/// A map `{ k: v, .. }` (keys may be non-string values).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Map(SyntaxNode);

impl Map {
    /// Wrap a [`SyntaxKind::Map`] node, or `None` for any other kind.
    #[must_use]
    pub fn cast(node: SyntaxNode) -> Option<Self> {
        (node.kind() == SyntaxKind::Map).then_some(Self(node))
    }

    /// The underlying [`SyntaxNode`].
    #[must_use]
    pub fn syntax(&self) -> &SyntaxNode {
        &self.0
    }

    /// The `key: value` entries, in source order.
    pub fn entries(&self) -> impl Iterator<Item = MapEntry> + '_ {
        self.0.children().filter_map(MapEntry::cast)
    }
}

/// A single `key: value` entry inside a [`Map`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MapEntry(SyntaxNode);

impl MapEntry {
    /// Wrap a [`SyntaxKind::MapEntry`] node, or `None` for any other kind.
    #[must_use]
    pub fn cast(node: SyntaxNode) -> Option<Self> {
        (node.kind() == SyntaxKind::MapEntry).then_some(Self(node))
    }

    /// The underlying [`SyntaxNode`].
    #[must_use]
    pub fn syntax(&self) -> &SyntaxNode {
        &self.0
    }

    /// The key [`Value`] (the first value child — any RON value, including
    /// non-string keys), if present.
    #[must_use]
    pub fn key(&self) -> Option<Value> {
        self.0.children().filter_map(Value::cast).next()
    }

    /// The value [`Value`] (the second value child), if present.
    #[must_use]
    pub fn value(&self) -> Option<Value> {
        self.0.children().filter_map(Value::cast).nth(1)
    }
}

/// An enum variant: a bare `Ident`, or `Ident(..)` / `Ident{..}` payload.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EnumVariant(SyntaxNode);

impl EnumVariant {
    /// Wrap a [`SyntaxKind::EnumVariant`] node, or `None` for any other kind.
    #[must_use]
    pub fn cast(node: SyntaxNode) -> Option<Self> {
        (node.kind() == SyntaxKind::EnumVariant).then_some(Self(node))
    }

    /// The underlying [`SyntaxNode`].
    #[must_use]
    pub fn syntax(&self) -> &SyntaxNode {
        &self.0
    }

    /// The variant name token (`Ident`), if present.
    #[must_use]
    pub fn name(&self) -> Option<SyntaxToken> {
        self.0.first_token_of(SyntaxKind::Ident)
    }

    /// The variant name as a string, if present.
    #[must_use]
    pub fn name_text(&self) -> Option<String> {
        self.name().map(|t| t.text().to_string())
    }

    /// The struct-like payload entries for `Variant { .. }`, in source order
    /// (empty for a bare or tuple-style variant).
    pub fn entries(&self) -> impl Iterator<Item = MapEntry> + '_ {
        self.0.children().filter_map(MapEntry::cast)
    }
}

/// The unit value `()`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Unit(SyntaxNode);

impl Unit {
    /// Wrap a [`SyntaxKind::Unit`] node, or `None` for any other kind.
    #[must_use]
    pub fn cast(node: SyntaxNode) -> Option<Self> {
        (node.kind() == SyntaxKind::Unit).then_some(Self(node))
    }

    /// The underlying [`SyntaxNode`].
    #[must_use]
    pub fn syntax(&self) -> &SyntaxNode {
        &self.0
    }
}

/// A scalar literal node (int, float, string, raw string, char, bool keyword).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Literal(SyntaxNode);

impl Literal {
    /// Wrap a [`SyntaxKind::Literal`] node, or `None` for any other kind.
    #[must_use]
    pub fn cast(node: SyntaxNode) -> Option<Self> {
        (node.kind() == SyntaxKind::Literal).then_some(Self(node))
    }

    /// The underlying [`SyntaxNode`].
    #[must_use]
    pub fn syntax(&self) -> &SyntaxNode {
        &self.0
    }

    /// The single scalar token this literal wraps (the first non-trivia token).
    #[must_use]
    pub fn token(&self) -> Option<SyntaxToken> {
        self.0
            .children_with_tokens()
            .filter_map(|el| el.as_token().cloned())
            .find(|t| !t.is_trivia())
    }

    /// The [`SyntaxKind`] of the underlying scalar token (e.g. `Integer`,
    /// `String`, `Char`, `TrueKw`), if present.
    #[must_use]
    pub fn token_kind(&self) -> Option<SyntaxKind> {
        self.token().map(|t| t.kind())
    }

    /// The verbatim source text of the scalar token (never normalized), if
    /// present.
    #[must_use]
    pub fn text(&self) -> Option<String> {
        self.token().map(|t| t.text().to_string())
    }
}

/// An extension attribute `#![enable(ext, ..)]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExtensionAttr(SyntaxNode);

impl ExtensionAttr {
    /// Wrap a [`SyntaxKind::ExtensionAttr`] node, or `None` for any other kind.
    #[must_use]
    pub fn cast(node: SyntaxNode) -> Option<Self> {
        (node.kind() == SyntaxKind::ExtensionAttr).then_some(Self(node))
    }

    /// The underlying [`SyntaxNode`].
    #[must_use]
    pub fn syntax(&self) -> &SyntaxNode {
        &self.0
    }

    /// The enabled-extension identifier tokens (e.g. `implicit_some`), in source
    /// order. Unknown extensions are still preserved verbatim as `Ident` tokens.
    pub fn extensions(&self) -> impl Iterator<Item = SyntaxToken> + '_ {
        self.0
            .children_with_tokens()
            .filter_map(|el| el.as_token().cloned())
            .filter(|t| t.kind() == SyntaxKind::Ident)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    /// Build the typed [`Document`] for `src`.
    fn doc_of(src: &str) -> Document {
        Document::cast(parse(src).root()).expect("root is always a Document")
    }

    #[test]
    fn struct_fields_and_name() {
        let d = doc_of("Point(x: 1, y: -2.0)");
        let Some(Value::Struct(s)) = d.value() else {
            panic!("expected a struct");
        };
        assert_eq!(s.name_text().as_deref(), Some("Point"));
        let fields: Vec<_> = s.fields().collect();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].name_text().as_deref(), Some("x"));
        assert_eq!(fields[1].name_text().as_deref(), Some("y"));
        // Field values are reachable and typed.
        let Some(Value::Literal(lit)) = fields[0].value() else {
            panic!("x should be a literal");
        };
        assert_eq!(lit.text().as_deref(), Some("1"));
        assert_eq!(lit.token_kind(), Some(SyntaxKind::Integer));
    }

    #[test]
    fn anonymous_struct_has_no_name() {
        let d = doc_of("(a: 1, b: 2)");
        let Some(Value::Struct(s)) = d.value() else {
            panic!("expected a struct");
        };
        assert_eq!(s.name_text(), None);
        assert_eq!(s.fields().count(), 2);
    }

    #[test]
    fn list_items() {
        let d = doc_of("[1, 2, 3,]");
        let Some(Value::List(list)) = d.value() else {
            panic!("expected a list");
        };
        let items: Vec<_> = list.items().collect();
        assert_eq!(items.len(), 3);
        for it in &items {
            assert!(matches!(it, Value::Literal(_)));
        }
    }

    #[test]
    fn tuple_items() {
        let d = doc_of("(1, \"two\", 'c')");
        let Some(Value::Tuple(t)) = d.value() else {
            panic!("expected a tuple");
        };
        assert_eq!(t.items().count(), 3);
    }

    #[test]
    fn map_entries_with_non_string_keys() {
        let d = doc_of("{ 1: \"one\", 'c': true }");
        let Some(Value::Map(m)) = d.value() else {
            panic!("expected a map");
        };
        let entries: Vec<_> = m.entries().collect();
        assert_eq!(entries.len(), 2);
        // First key is an integer literal (a non-string key).
        let Some(Value::Literal(k0)) = entries[0].key() else {
            panic!("key 0 should be a literal");
        };
        assert_eq!(k0.token_kind(), Some(SyntaxKind::Integer));
        let Some(Value::Literal(v0)) = entries[0].value() else {
            panic!("value 0 should be a literal");
        };
        assert_eq!(v0.token_kind(), Some(SyntaxKind::String));
    }

    #[test]
    fn enum_variant_struct_like() {
        let d = doc_of("Variant { field: 1 }");
        let Some(Value::EnumVariant(v)) = d.value() else {
            panic!("expected an enum variant");
        };
        assert_eq!(v.name_text().as_deref(), Some("Variant"));
        assert_eq!(v.entries().count(), 1);
    }

    #[test]
    fn bare_enum_variant() {
        let d = doc_of("Unit");
        let Some(Value::EnumVariant(v)) = d.value() else {
            panic!("expected a bare variant");
        };
        assert_eq!(v.name_text().as_deref(), Some("Unit"));
        assert_eq!(v.entries().count(), 0);
    }

    #[test]
    fn unit_value() {
        let d = doc_of("()");
        assert!(matches!(d.value(), Some(Value::Unit(_))));
    }

    #[test]
    fn literal_text_is_verbatim() {
        let d = doc_of("r#\"raw \"q\" str\"#");
        let Some(Value::Literal(lit)) = d.value() else {
            panic!("expected a literal");
        };
        assert_eq!(lit.token_kind(), Some(SyntaxKind::RawString));
        assert_eq!(lit.text().as_deref(), Some("r#\"raw \"q\" str\"#"));
    }

    #[test]
    fn extension_attrs_and_value() {
        let d = doc_of("#![enable(implicit_some)]\nSome(5)");
        let attrs: Vec<_> = d.extension_attrs().collect();
        assert_eq!(attrs.len(), 1);
        let exts: Vec<_> = attrs[0]
            .extensions()
            .map(|t| t.text().to_string())
            .collect();
        // `enable` is a keyword token, so only the extension idents surface here.
        assert!(exts.contains(&"implicit_some".to_string()));
        // The top-level value is still reachable past the attributes.
        assert!(d.value().is_some());
    }

    #[test]
    fn error_value_is_reachable() {
        // A stray top-level token recovers into an Error node; navigation stays
        // total — the value is reachable as Value::Error rather than dropped.
        let d = doc_of("@");
        assert!(matches!(d.value(), Some(Value::Error(_))));
    }

    #[test]
    fn value_syntax_round_trips_text() {
        let src = "Foo(x: [1, 2], y: { 'a': 'b' })";
        let d = doc_of(src);
        let v = d.value().expect("has a value");
        // The typed value's underlying node text equals the source span.
        assert_eq!(v.syntax().text(), src);
    }
}
