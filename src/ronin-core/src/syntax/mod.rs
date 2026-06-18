//! The rowan-free CST facade.
//!
//! `ronin-core` builds its concrete syntax tree on [`rowan`], but the public API
//! MUST NOT expose any rowan type (TR-009 / INV-7) so the underlying library
//! stays swappable. This module wraps rowan's `SyntaxNode`/`SyntaxToken`/
//! `SyntaxElement` behind `ronin-core` newtypes whose accessors return only
//! `ronin-core` types ([`SyntaxKind`], [`TextRange`], and these newtypes).
//!
//! # AD-001 — Trivia attachment rule (module invariant, T011)
//!
//! Exactly one trivia-attachment rule is applied consistently across the whole
//! parser (risk mitigation: "inconsistent trivia model"):
//!
//! * **Leading trivia binds to the following significant token.** All
//!   whitespace / comments / BOM that precede a significant token are emitted
//!   into the green tree immediately before that token, inside the same node
//!   the token belongs to.
//! * **Trailing trivia at end-of-input binds to the last token** — i.e. trivia
//!   after the final significant token (including a missing trailing newline,
//!   trailing whitespace, or trailing comments) is attached to the nearest
//!   preceding structure (the [`SyntaxKind::Root`] node), since there is no
//!   following token to bind it to.
//! * A leading UTF-8 **BOM** is the first leading-trivia token of the document
//!   (AD-008). CRLF vs LF is preserved verbatim inside [`SyntaxKind::Whitespace`]
//!   tokens.
//!
//! This rule is load-bearing for the round-trip invariant (INV-2): because every
//! trivia byte is emitted into exactly one token in source order, concatenating
//! all token texts reproduces the source exactly, regardless of where trivia
//! sits relative to structure.

pub mod ast;
pub mod kind;

pub use kind::SyntaxKind;

use kind::RonLang;

/// A half-open byte range `[start, end)` into the original source.
///
/// This is `ronin-core`'s own range type so no rowan type leaks across the API
/// boundary (INV-7). Offsets are absolute byte offsets into the accepted UTF-8
/// source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TextRange {
    start: usize,
    end: usize,
}

impl TextRange {
    /// Construct a range from absolute byte offsets. `start <= end` is required.
    #[inline]
    #[must_use]
    pub fn new(start: usize, end: usize) -> Self {
        debug_assert!(start <= end, "TextRange start must not exceed end");
        Self { start, end }
    }

    /// Inclusive start offset (bytes).
    #[inline]
    #[must_use]
    pub fn start(self) -> usize {
        self.start
    }

    /// Exclusive end offset (bytes).
    #[inline]
    #[must_use]
    pub fn end(self) -> usize {
        self.end
    }

    /// Byte length of the range.
    #[inline]
    #[must_use]
    pub fn len(self) -> usize {
        self.end - self.start
    }

    /// `true` if the range covers zero bytes.
    #[inline]
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.start == self.end
    }

    /// `true` if `offset` falls within `[start, end)`.
    #[inline]
    #[must_use]
    pub fn contains(self, offset: usize) -> bool {
        self.start <= offset && offset < self.end
    }
}

impl From<rowan::TextRange> for TextRange {
    #[inline]
    fn from(r: rowan::TextRange) -> Self {
        Self {
            start: usize::from(r.start()),
            end: usize::from(r.end()),
        }
    }
}

/// An opaque, navigable interior node of the CST.
///
/// Wraps `rowan::SyntaxNode<RonLang>`; the inner rowan type is never exposed.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct SyntaxNode(rowan::SyntaxNode<RonLang>);

/// An opaque leaf token of the CST, carrying verbatim source text (incl. trivia).
///
/// Wraps `rowan::SyntaxToken<RonLang>`; the inner rowan type is never exposed.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct SyntaxToken(rowan::SyntaxToken<RonLang>);

/// Either a [`SyntaxNode`] or a [`SyntaxToken`] — a child element in source order.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum SyntaxElement {
    /// An interior node child.
    Node(SyntaxNode),
    /// A leaf token child.
    Token(SyntaxToken),
}

impl SyntaxNode {
    /// Wrap a rowan node. Crate-internal: rowan types never cross the API edge.
    #[inline]
    #[allow(dead_code)] // used by later stages (OBJ4 edit primitives)
    pub(crate) fn from_rowan(node: rowan::SyntaxNode<RonLang>) -> Self {
        Self(node)
    }

    /// Build a red-tree root from a green node (used by the parser/printer).
    #[inline]
    pub(crate) fn new_root(green: rowan::GreenNode) -> Self {
        Self(rowan::SyntaxNode::new_root(green))
    }

    /// Borrow the inner rowan node (crate-internal only).
    #[inline]
    #[allow(dead_code)] // used by later stages (OBJ4 edit primitives)
    pub(crate) fn raw(&self) -> &rowan::SyntaxNode<RonLang> {
        &self.0
    }

    /// This node's classification.
    #[inline]
    #[must_use]
    pub fn kind(&self) -> SyntaxKind {
        self.0.kind()
    }

    /// Absolute byte range spanned by this node (union of descendant tokens).
    #[inline]
    #[must_use]
    pub fn text_range(&self) -> TextRange {
        self.0.text_range().into()
    }

    /// The full source text spanned by this node, including trivia, as a string.
    #[must_use]
    pub fn text(&self) -> String {
        self.0.text().to_string()
    }

    /// Parent node, or `None` for the root.
    #[inline]
    #[must_use]
    pub fn parent(&self) -> Option<SyntaxNode> {
        self.0.parent().map(SyntaxNode)
    }

    /// Direct child nodes (excluding tokens), in source order.
    pub fn children(&self) -> impl Iterator<Item = SyntaxNode> {
        self.0.children().map(SyntaxNode)
    }

    /// Direct children (both nodes and tokens), in source order.
    pub fn children_with_tokens(&self) -> impl Iterator<Item = SyntaxElement> {
        self.0.children_with_tokens().map(SyntaxElement::from_rowan)
    }

    /// Every descendant token (leaves), in source order — the basis for printing.
    pub fn descendant_tokens(&self) -> impl Iterator<Item = SyntaxToken> {
        self.0.descendants_with_tokens().filter_map(|el| match el {
            rowan::NodeOrToken::Token(t) => Some(SyntaxToken(t)),
            rowan::NodeOrToken::Node(_) => None,
        })
    }

    /// First child token whose kind matches `kind`, if any.
    #[must_use]
    pub fn first_token_of(&self, kind: SyntaxKind) -> Option<SyntaxToken> {
        self.0
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .find(|t| t.kind() == kind)
            .map(SyntaxToken)
    }
}

impl SyntaxToken {
    /// This token's classification.
    #[inline]
    #[must_use]
    pub fn kind(&self) -> SyntaxKind {
        self.0.kind()
    }

    /// Absolute byte range spanned by this token.
    #[inline]
    #[must_use]
    pub fn text_range(&self) -> TextRange {
        self.0.text_range().into()
    }

    /// The verbatim source slice for this token (never normalized or re-escaped).
    #[inline]
    #[must_use]
    pub fn text(&self) -> &str {
        self.0.text()
    }

    /// Parent node.
    #[inline]
    #[must_use]
    pub fn parent(&self) -> Option<SyntaxNode> {
        self.0.parent().map(SyntaxNode)
    }

    /// `true` if this token is trivia (whitespace / comment / BOM) per AD-001.
    #[inline]
    #[must_use]
    pub fn is_trivia(&self) -> bool {
        self.kind().is_trivia()
    }
}

impl SyntaxElement {
    #[inline]
    fn from_rowan(el: rowan::SyntaxElement<RonLang>) -> Self {
        match el {
            rowan::NodeOrToken::Node(n) => Self::Node(SyntaxNode(n)),
            rowan::NodeOrToken::Token(t) => Self::Token(SyntaxToken(t)),
        }
    }

    /// This element's classification (node or token kind).
    #[inline]
    #[must_use]
    pub fn kind(&self) -> SyntaxKind {
        match self {
            Self::Node(n) => n.kind(),
            Self::Token(t) => t.kind(),
        }
    }

    /// Absolute byte range spanned by this element.
    #[inline]
    #[must_use]
    pub fn text_range(&self) -> TextRange {
        match self {
            Self::Node(n) => n.text_range(),
            Self::Token(t) => t.text_range(),
        }
    }

    /// Borrow as a node, if this element is a node.
    #[inline]
    #[must_use]
    pub fn as_node(&self) -> Option<&SyntaxNode> {
        match self {
            Self::Node(n) => Some(n),
            Self::Token(_) => None,
        }
    }

    /// Borrow as a token, if this element is a token.
    #[inline]
    #[must_use]
    pub fn as_token(&self) -> Option<&SyntaxToken> {
        match self {
            Self::Token(t) => Some(t),
            Self::Node(_) => None,
        }
    }
}

impl std::fmt::Debug for SyntaxNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}@{:?}", self.kind(), self.text_range())
    }
}

impl std::fmt::Debug for SyntaxToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?}@{:?} {:?}",
            self.kind(),
            self.text_range(),
            self.text()
        )
    }
}

impl std::fmt::Debug for SyntaxElement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Node(n) => std::fmt::Debug::fmt(n, f),
            Self::Token(t) => std::fmt::Debug::fmt(t, f),
        }
    }
}
