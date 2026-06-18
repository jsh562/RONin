//! The `x-ron-*` extension layer {TR-002}.
//!
//! JSON Schema 2020-12 cannot express several RON constructs (fixed-arity
//! tuples with heterogeneous element types, `char`, unit, raw bytes, maps with
//! non-string keys, and `Option`/implicit-`Some`), nor the RON parser
//! extensions that let one Rust type accept multiple RON surface forms
//! (`implicit_some`, `unwrap_newtypes`, `unwrap_variant_newtypes`).
//!
//! [`RonTypeExtension`] is the closed annotation set that captures those facts.
//! It is *additive*: it annotates an otherwise-valid base JSON-Schema node, so
//! stripping it leaves a still-valid (if coarser) schema (degradation-safe).
//!
//! # `x-ron-*` keyword mapping (OBJ1, T011)
//!
//! When a node carries a [`RonTypeExtension`] the serializer emits these
//! keywords alongside the standard 2020-12 vocabulary
//! (see [`crate::serialize`]):
//!
//! | RON construct | base 2020-12 shape | `x-ron-*` annotation |
//! |---------------|--------------------|----------------------|
//! | tuple (fixed arity) | `prefixItems` + `items:false` | `x-ron-kind: "tuple"`, `x-ron-tuple-arity: N` |
//! | `char` | `type: "string"` | `x-ron-kind: "char"` |
//! | unit `()` | `type: "null"` | `x-ron-kind: "unit"` |
//! | bytes | `type: "string"` | `x-ron-kind: "bytes"` |
//! | non-string-key map | `type: "object"` | `x-ron-kind: "non-string-key-map"` |
//! | `Option<T>` | `oneOf` / nullable | `x-ron-kind: "option"` |
//!
//! The three boolean RON extension flags are emitted as
//! `x-ron-implicit-some`, `x-ron-unwrap-newtypes`, and
//! `x-ron-unwrap-variant-newtypes` (only when `true`).

use serde::{Deserialize, Serialize};

/// The RON value kind a [`RonTypeExtension`] records — the construct JSON
/// Schema 2020-12 cannot natively express (TR-002).
///
/// The keyword set is **closed** (a pinned internal contract, NEW-CONFIG); each
/// variant maps 1:1 to a stable `x-ron-kind` string via
/// [`RonKind::as_keyword`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RonKind {
    /// A fixed-arity tuple `(A, B, ...)`. Pairs with
    /// [`RonTypeExtension::tuple_arity`] and a `prefixItems` base shape.
    Tuple,
    /// A single Unicode scalar (`char`), serialized as a one-character string.
    Char,
    /// The unit type `()` — base shape `type: "null"`.
    Unit,
    /// Raw bytes (`&[u8]` / byte string) — base shape `type: "string"`.
    Bytes,
    /// A map whose keys are not strings (`HashMap<K, V>` with non-string `K`),
    /// which the JSON object model cannot represent on its own.
    #[serde(rename = "non-string-key-map")]
    NonStringKeyMap,
    /// An `Option<T>` / RON `Some`/`None` (and, with `implicit_some`, a bare
    /// value standing for `Some`).
    Option,
}

impl RonKind {
    /// The stable `x-ron-kind` keyword string for this kind (public contract).
    #[inline]
    #[must_use]
    pub fn as_keyword(self) -> &'static str {
        match self {
            RonKind::Tuple => "tuple",
            RonKind::Char => "char",
            RonKind::Unit => "unit",
            RonKind::Bytes => "bytes",
            RonKind::NonStringKeyMap => "non-string-key-map",
            RonKind::Option => "option",
        }
    }
}

impl std::fmt::Display for RonKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_keyword())
    }
}

/// The `x-ron-*` annotation layer attached to a [`crate::model::TypeNode`]
/// (TR-002).
///
/// Records the [`RonKind`] of a RON-only construct, the fixed `tuple_arity`
/// (for [`RonKind::Tuple`]), and the three active RON parser-extension flags.
/// Round-trip stable: every field survives serialize → deserialize unchanged
/// (SC-001).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RonTypeExtension {
    /// The RON construct this node represents, when it is a RON-only kind.
    /// `None` means the node is a plain JSON-Schema node carrying only
    /// extension *flags* (e.g. a struct under active `unwrap_newtypes`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ron_kind: Option<RonKind>,

    /// Fixed element count for a [`RonKind::Tuple`] node; pairs with the node's
    /// `prefixItems` mapping. `None` for non-tuple kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tuple_arity: Option<usize>,

    /// RON `implicit_some` extension: a bare value may stand for `Some(value)`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub implicit_some: bool,

    /// RON `unwrap_newtypes` extension: newtype wrappers are written without
    /// their wrapper.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unwrap_newtypes: bool,

    /// RON `unwrap_variant_newtypes` extension: newtype enum variants are
    /// written unwrapped.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unwrap_variant_newtypes: bool,
}

#[inline]
fn is_false(b: &bool) -> bool {
    !*b
}

impl RonTypeExtension {
    /// An empty extension (no RON kind, no flags). Equivalent to
    /// [`RonTypeExtension::default`].
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An extension recording a bare RON [`RonKind`] with no extra flags.
    #[inline]
    #[must_use]
    pub fn kind(ron_kind: RonKind) -> Self {
        Self {
            ron_kind: Some(ron_kind),
            ..Self::default()
        }
    }

    /// An extension recording a fixed-arity [`RonKind::Tuple`].
    #[inline]
    #[must_use]
    pub fn tuple(arity: usize) -> Self {
        Self {
            ron_kind: Some(RonKind::Tuple),
            tuple_arity: Some(arity),
            ..Self::default()
        }
    }

    /// `true` if this extension carries no RON kind and no active flags, i.e. it
    /// is semantically empty and may be omitted from a node.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ron_kind.is_none()
            && self.tuple_arity.is_none()
            && !self.implicit_some
            && !self.unwrap_newtypes
            && !self.unwrap_variant_newtypes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T011: every `RonKind` has a stable, kebab-case `x-ron-kind` keyword that
    /// matches its serde representation.
    #[test]
    fn ron_kind_keywords_are_stable_and_match_serde() {
        let cases = [
            (RonKind::Tuple, "tuple"),
            (RonKind::Char, "char"),
            (RonKind::Unit, "unit"),
            (RonKind::Bytes, "bytes"),
            (RonKind::NonStringKeyMap, "non-string-key-map"),
            (RonKind::Option, "option"),
        ];
        for (kind, kw) in cases {
            assert_eq!(kind.as_keyword(), kw);
            assert_eq!(kind.to_string(), kw);
            // serde encodes the same keyword (as a JSON string literal).
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{kw}\""));
        }
    }

    /// An empty extension is recognised as empty; a flag or kind makes it
    /// non-empty.
    #[test]
    fn is_empty_tracks_content() {
        assert!(RonTypeExtension::new().is_empty());
        assert!(!RonTypeExtension::kind(RonKind::Char).is_empty());
        assert!(!RonTypeExtension::tuple(2).is_empty());
        let flagged = RonTypeExtension {
            implicit_some: true,
            ..RonTypeExtension::default()
        };
        assert!(!flagged.is_empty());
    }

    /// Default booleans and `None` fields are omitted from the serialized form
    /// (keeps the `x-ron-*` layer additive/minimal).
    #[test]
    fn empty_extension_serializes_to_empty_object() {
        let json = serde_json::to_value(RonTypeExtension::new()).unwrap();
        assert_eq!(json, serde_json::json!({}));
    }
}
