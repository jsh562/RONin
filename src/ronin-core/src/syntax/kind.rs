//! [`SyntaxKind`] — the closed classification of every CST node and token.
//!
//! The set is fixed by the pinned RON grammar (ron 0.12.x, per AD-002/TR-004).
//! Every [`crate::syntax::SyntaxNode`] and [`crate::syntax::SyntaxToken`] carries
//! exactly one `SyntaxKind`. The enum is `#[repr(u16)]` so it maps directly onto
//! rowan's raw `u16` kind, but rowan's raw integers are **never** exposed in the
//! public API (TR-009/INV-7): conversion lives in [`RonLang`] below.
//!
//! Kinds are split into two families:
//!
//! * **Token kinds** — leaf classifications produced by the lexer. Each source
//!   byte lands in exactly one token (INV-1). Includes trivia (whitespace,
//!   comments, BOM) and the error/sentinel kinds.
//! * **Node kinds** — interior classifications produced by the parser while
//!   building the green tree, including the `Error` recovery kind and `Root`.

/// Closed classification of every CST node and token.
///
/// `#[repr(u16)]` with explicit, stable discriminants so the mapping to/from
/// rowan's raw kind is total and order-independent. New variants MUST be added
/// before [`SyntaxKind::__Last`] and existing discriminants MUST NOT be
/// renumbered (the value is part of the on-tree representation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
#[non_exhaustive]
pub enum SyntaxKind {
    // ---- Trivia tokens (semantically inert, preserved for losslessness) ----
    /// Run of ASCII/Unicode whitespace (spaces, tabs, newlines — CR and LF
    /// preserved verbatim).
    Whitespace = 0,
    /// A leading UTF-8 byte-order mark (`\u{FEFF}`), kept as trivia (AD-008).
    Bom,
    /// Line comment: `// ...` up to (but not including) the line break.
    LineComment,
    /// Block comment: `/* ... */`, nestable.
    BlockComment,

    // ---- Literal / atom tokens ----
    /// Bare identifier or struct/variant/field name (e.g. `Foo`, `x`).
    Ident,
    /// Integer literal (any base, with optional `_` separators / type suffix).
    Integer,
    /// Floating-point literal (with optional `_` separators / type suffix).
    Float,
    /// Normal string literal `"..."` (with escapes), verbatim including quotes.
    String,
    /// Raw string literal `r#"..."#`, verbatim including delimiters.
    RawString,
    /// Character literal `'c'` (with escapes), verbatim including quotes.
    Char,
    /// The `true` keyword token.
    TrueKw,
    /// The `false` keyword token.
    FalseKw,

    // ---- Punctuation tokens ----
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
    /// `{`
    LBrace,
    /// `}`
    RBrace,
    /// `:`
    Colon,
    /// `,`
    Comma,

    // ---- Extension-attribute tokens (`#![enable(...)]`) ----
    /// `#`
    Hash,
    /// `!`
    Bang,
    /// `enable` keyword inside an extension attribute.
    EnableKw,

    /// Any byte run the lexer could not classify (recovery sentinel token).
    /// Always wrapped in an [`SyntaxKind::Error`] node by the parser.
    LexError,

    // =====================================================================
    // Node kinds (interior tree classifications, produced by the parser).
    // =====================================================================
    /// The document root node. Holds the single top-level value plus any
    /// leading extension attributes and surrounding trivia.
    Root,
    /// Named struct `Name( field: value, ... )` or anonymous struct
    /// `( field: value, ... )`.
    Struct,
    /// A single `field: value` entry inside a [`SyntaxKind::Struct`].
    StructField,
    /// Tuple / tuple-struct `( a, b, c )` (positional, no field names).
    Tuple,
    /// List / sequence `[ a, b, c ]`.
    List,
    /// Map `{ k: v, ... }` (keys may be non-string values).
    Map,
    /// A single `key: value` entry inside a [`SyntaxKind::Map`].
    MapEntry,
    /// Enum variant: a bare `Ident`, or `Ident(...)` / `Ident{...}` payload.
    EnumVariant,
    /// Unit value `()`.
    Unit,
    /// Wrapper around a scalar literal token so scalar values are uniform nodes.
    Literal,
    /// Extension attribute `#![enable(ext, ...)]`. Unknown extensions are still
    /// preserved verbatim as text within this node.
    ExtensionAttr,
    /// Recovery node wrapping unexpected/unparseable tokens so the tree still
    /// covers all input (TR-005/INV-3).
    Error,

    /// Sentinel marking the exclusive upper bound of the enum. NOT a real kind;
    /// never assigned to a node or token. Used only for range checks.
    #[doc(hidden)]
    __Last,
}

impl SyntaxKind {
    /// Reconstruct a `SyntaxKind` from its raw `u16` discriminant.
    ///
    /// Returns `None` for any value outside the closed set (including the
    /// `__Last` sentinel), so a corrupt/foreign raw kind can never silently
    /// masquerade as a valid kind.
    ///
    /// Implemented with a total `match` (no `unsafe`/`transmute`) so the crate
    /// can keep `#![forbid(unsafe_code)]`. The `ALL` table keeps this in sync
    /// with the variant set; a missing variant would be caught by
    /// `raw_roundtrip_is_total`.
    #[inline]
    #[must_use]
    pub(crate) fn from_raw(raw: u16) -> Option<Self> {
        Self::ALL.get(raw as usize).copied()
    }

    /// Every real `SyntaxKind` variant in discriminant order (excludes the
    /// `__Last` sentinel). The slice index equals the variant's `u16` value.
    const ALL: &'static [SyntaxKind] = &[
        SyntaxKind::Whitespace,
        SyntaxKind::Bom,
        SyntaxKind::LineComment,
        SyntaxKind::BlockComment,
        SyntaxKind::Ident,
        SyntaxKind::Integer,
        SyntaxKind::Float,
        SyntaxKind::String,
        SyntaxKind::RawString,
        SyntaxKind::Char,
        SyntaxKind::TrueKw,
        SyntaxKind::FalseKw,
        SyntaxKind::LParen,
        SyntaxKind::RParen,
        SyntaxKind::LBracket,
        SyntaxKind::RBracket,
        SyntaxKind::LBrace,
        SyntaxKind::RBrace,
        SyntaxKind::Colon,
        SyntaxKind::Comma,
        SyntaxKind::Hash,
        SyntaxKind::Bang,
        SyntaxKind::EnableKw,
        SyntaxKind::LexError,
        SyntaxKind::Root,
        SyntaxKind::Struct,
        SyntaxKind::StructField,
        SyntaxKind::Tuple,
        SyntaxKind::List,
        SyntaxKind::Map,
        SyntaxKind::MapEntry,
        SyntaxKind::EnumVariant,
        SyntaxKind::Unit,
        SyntaxKind::Literal,
        SyntaxKind::ExtensionAttr,
        SyntaxKind::Error,
    ];

    /// The raw `u16` discriminant for this kind (rowan-facing only).
    #[inline]
    #[must_use]
    pub(crate) fn to_raw(self) -> u16 {
        self as u16
    }

    /// `true` for trivia token kinds (whitespace, comments, BOM).
    ///
    /// Trivia is semantically inert but preserved verbatim for losslessness
    /// (AD-001). Used by the printer/accessors, never alters byte coverage.
    #[inline]
    #[must_use]
    pub fn is_trivia(self) -> bool {
        matches!(
            self,
            Self::Whitespace | Self::Bom | Self::LineComment | Self::BlockComment
        )
    }

    /// `true` if this kind classifies a token (leaf) rather than a node.
    #[inline]
    #[must_use]
    pub fn is_token(self) -> bool {
        (self as u16) <= (Self::LexError as u16)
    }
}

/// The rowan [`Language`](rowan::Language) implementation for RON.
///
/// This is the single bridge between [`SyntaxKind`] and rowan's raw `u16`
/// kind. It is `pub(crate)` — it MUST NOT appear in the public API so that the
/// underlying CST library stays swappable (HINT-005/INV-7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum RonLang {}

impl rowan::Language for RonLang {
    type Kind = SyntaxKind;

    #[inline]
    fn kind_from_raw(raw: rowan::SyntaxKind) -> Self::Kind {
        // A raw kind always originates from a `SyntaxKind` we ourselves wrote
        // into the green tree, so the range check should always succeed. If it
        // somehow does not, fall back to `Error` rather than panicking — this
        // keeps the never-panic contract (TR-001) even under internal misuse.
        SyntaxKind::from_raw(raw.0).unwrap_or(SyntaxKind::Error)
    }

    #[inline]
    fn kind_to_raw(kind: Self::Kind) -> rowan::SyntaxKind {
        rowan::SyntaxKind(kind.to_raw())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_roundtrip_is_total() {
        // The ALL table must hold exactly the variants `0..__Last`.
        assert_eq!(
            SyntaxKind::ALL.len(),
            SyntaxKind::__Last as usize,
            "ALL table must list every variant before __Last"
        );
        // Every discriminant below `__Last` must round-trip exactly.
        let mut raw = 0u16;
        while raw < SyntaxKind::__Last as u16 {
            let kind = SyntaxKind::from_raw(raw).expect("in-range raw must decode");
            assert_eq!(kind.to_raw(), raw, "raw discriminant must round-trip");
            raw += 1;
        }
    }

    #[test]
    fn out_of_range_raw_is_rejected() {
        assert_eq!(SyntaxKind::from_raw(SyntaxKind::__Last as u16), None);
        assert_eq!(SyntaxKind::from_raw(u16::MAX), None);
    }

    #[test]
    fn token_node_partition_is_consistent() {
        assert!(SyntaxKind::Whitespace.is_token());
        assert!(SyntaxKind::LexError.is_token());
        assert!(!SyntaxKind::Root.is_token());
        assert!(!SyntaxKind::Error.is_token());
    }

    #[test]
    fn trivia_classification() {
        assert!(SyntaxKind::Whitespace.is_trivia());
        assert!(SyntaxKind::Bom.is_trivia());
        assert!(SyntaxKind::LineComment.is_trivia());
        assert!(SyntaxKind::BlockComment.is_trivia());
        assert!(!SyntaxKind::Ident.is_trivia());
        assert!(!SyntaxKind::Comma.is_trivia());
    }
}
