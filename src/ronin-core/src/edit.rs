//! Non-destructive CST edit primitives (TR-011, OBJ4).
//!
//! rowan green trees are **immutable / persistent**: an edit never mutates a
//! shared node in place, it produces a *new* green tree that shares all
//! untouched subtrees with the original (structural sharing). [`apply_edit`]
//! exploits this to satisfy the edit-locality invariant (INV-8 / SC-007): every
//! region the edit does not touch prints byte-identically, because those
//! subtrees are the very same green nodes as before.
//!
//! # Model (AD-004)
//!
//! An [`EditOperation`] names:
//!
//! * an [`EditTarget`] — a whole [`SyntaxNode`] or a token *span*
//!   `[first ..= last]` of adjacent sibling tokens/nodes;
//! * an [`EditKind`] — `Insert` (before the target), `Replace`, or `Remove`;
//! * a `payload` — replacement source text (parsed into a fresh subtree),
//!   absent for `Remove`;
//! * a [`TriviaPolicy`] — whether the adjacent leading / trailing trivia of the
//!   target is kept or discarded (AD-004).
//!
//! # How a new tree is built (green-node splicing)
//!
//! Every edit reduces to *rebuilding one parent node's child list* and then
//! re-rooting the tree with [`rowan::SyntaxNode::replace_with`], which rebuilds
//! only the spine from that parent up to the root (cost ∝ tree depth) and reuses
//! every other subtree verbatim. The payload text is lexed into raw green tokens
//! (not re-parsed structurally) so the spliced bytes are preserved exactly and
//! the surrounding tree keeps printing byte-for-byte.
//!
//! The result is wrapped back into a fresh [`CstDocument`]. Diagnostics from the
//! original parse are **not** carried over — the edited tree is a new document
//! whose diagnostics (if any) would come from re-parsing; for OBJ4 we expose the
//! spliced tree with an empty diagnostics set (the tree is still fully
//! printable, INV-8). Re-validation is a later-epic concern.

use rowan::{GreenNode, GreenToken, NodeOrToken};

use crate::lexer;
use crate::parser::CstDocument;
use crate::syntax::kind::RonLang;
use crate::syntax::{SyntaxKind, SyntaxNode, SyntaxToken};

/// What an [`EditOperation`] acts on (AD-004).
///
/// Either a whole [`SyntaxNode`] subtree, or a contiguous *span* of sibling
/// elements delimited by a first and last token (inclusive). A single-token
/// edit is a span whose first and last token are the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditTarget {
    /// A whole node subtree.
    Node(SyntaxNode),
    /// An inclusive span of adjacent siblings, addressed by its first and last
    /// token. Both tokens MUST share the same parent node.
    TokenSpan {
        /// First token of the span (inclusive).
        first: SyntaxToken,
        /// Last token of the span (inclusive). May equal `first`.
        last: SyntaxToken,
    },
}

/// The kind of edit to perform (AD-004).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EditKind {
    /// Insert the payload immediately *before* the target, keeping the target.
    Insert,
    /// Replace the target with the payload.
    Replace,
    /// Remove the target (payload is ignored / must be absent).
    Remove,
}

/// Caller-chosen trivia handling for an edit (AD-004).
///
/// Controls whether the leading / trailing trivia *adjacent to the target* is
/// kept or discarded when the target is removed or replaced. Leading trivia is
/// the run of trivia tokens immediately preceding the target's first element;
/// trailing trivia is the run immediately following the target's last element,
/// within the same parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TriviaPolicy {
    /// Keep (`true`) or discard (`false`) the leading trivia adjacent to the
    /// target.
    pub keep_leading: bool,
    /// Keep (`true`) or discard (`false`) the trailing trivia adjacent to the
    /// target.
    pub keep_trailing: bool,
}

impl TriviaPolicy {
    /// Keep all adjacent trivia (the conservative, lossless-by-default policy).
    pub const KEEP_ALL: Self = Self {
        keep_leading: true,
        keep_trailing: true,
    };

    /// Discard both adjacent leading and trailing trivia.
    pub const DISCARD_ALL: Self = Self {
        keep_leading: false,
        keep_trailing: false,
    };
}

impl Default for TriviaPolicy {
    /// Defaults to [`TriviaPolicy::KEEP_ALL`] — never drop bytes unless asked.
    #[inline]
    fn default() -> Self {
        Self::KEEP_ALL
    }
}

/// A single non-destructive edit over a [`CstDocument`] (AD-004).
///
/// Construct via [`EditOperation::insert`], [`EditOperation::replace`], or
/// [`EditOperation::remove`], then apply with [`apply_edit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditOperation {
    /// What the edit acts on.
    pub target: EditTarget,
    /// The kind of edit.
    pub kind: EditKind,
    /// Replacement / inserted source text (`None` for [`EditKind::Remove`]).
    pub payload: Option<String>,
    /// Trivia handling for the affected region.
    pub trivia_policy: TriviaPolicy,
}

impl EditOperation {
    /// Insert `text` immediately before `target`, keeping the target.
    #[must_use]
    pub fn insert(
        target: EditTarget,
        text: impl Into<String>,
        trivia_policy: TriviaPolicy,
    ) -> Self {
        Self {
            target,
            kind: EditKind::Insert,
            payload: Some(text.into()),
            trivia_policy,
        }
    }

    /// Replace `target` with `text`.
    #[must_use]
    pub fn replace(
        target: EditTarget,
        text: impl Into<String>,
        trivia_policy: TriviaPolicy,
    ) -> Self {
        Self {
            target,
            kind: EditKind::Replace,
            payload: Some(text.into()),
            trivia_policy,
        }
    }

    /// Remove `target`.
    #[must_use]
    pub fn remove(target: EditTarget, trivia_policy: TriviaPolicy) -> Self {
        Self {
            target,
            kind: EditKind::Remove,
            payload: None,
            trivia_policy,
        }
    }
}

/// Why an [`apply_edit`] call could not produce a tree.
///
/// All variants are non-panicking: a malformed request returns an error rather
/// than corrupting the tree (Principle I — never corrupt user data).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    /// The target node is the document root, which has no parent to splice into.
    RootNotEditable,
    /// A token-span's two endpoints do not share the same parent node.
    SpanParentMismatch,
    /// A token-span's `last` token precedes its `first` token.
    SpanOutOfOrder,
    /// The target element could not be located within its parent (e.g. it came
    /// from a different tree).
    TargetNotFound,
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::RootNotEditable => "the document root cannot be edited",
            Self::SpanParentMismatch => "token-span endpoints have different parents",
            Self::SpanOutOfOrder => "token-span `last` precedes `first`",
            Self::TargetNotFound => "edit target not found in its parent",
        };
        f.write_str(s)
    }
}

impl std::error::Error for EditError {}

/// Apply `edit` to `doc`, returning a **new** [`CstDocument`] (non-destructive).
///
/// The original `doc` is untouched. Unaffected regions print byte-identically
/// (INV-8) because their green subtrees are reused unchanged; the new tree is
/// always fully printable.
///
/// # Errors
///
/// Returns [`EditError`] if the target is the root, if a token-span is
/// ill-formed (different parents or reversed), or if the target cannot be found
/// in its parent. Never panics.
pub fn apply_edit(doc: &CstDocument, edit: EditOperation) -> Result<CstDocument, EditError> {
    // Resolve the target to (parent, child-index range [start..=end]).
    let (parent, start, end) = resolve_target(&edit.target)?;

    // Defensive: the target must belong to `doc` — splicing a node from a
    // foreign tree would silently re-root that other tree (Principle I).
    if !belongs_to(&parent, doc) {
        return Err(EditError::TargetNotFound);
    }

    let parent_green = parent.raw().green().into_owned();
    let child_count = parent_green.children().count();

    // Expand the affected index range to absorb adjacent trivia per policy.
    let (mut splice_start, mut splice_end) = (start, end);
    if !edit.trivia_policy.keep_leading {
        splice_start = absorb_leading_trivia(&parent_green, splice_start);
    }
    if !edit.trivia_policy.keep_trailing {
        splice_end = absorb_trailing_trivia(&parent_green, splice_end, child_count);
    }

    // Build the replacement / inserted elements from the payload text.
    let payload_elems: Vec<GreenElem> = edit
        .payload
        .as_deref()
        .map(payload_to_green)
        .unwrap_or_default();

    // Rebuild the parent's child list.
    let mut children: Vec<GreenElem> = parent_green
        .children()
        .map(|c| match c {
            NodeOrToken::Node(n) => NodeOrToken::Node(n.to_owned()),
            NodeOrToken::Token(t) => NodeOrToken::Token(t.to_owned()),
        })
        .collect();

    match edit.kind {
        EditKind::Insert => {
            // Insert payload immediately before the (unexpanded) target start.
            // Leading-trivia policy still applies to where "before" begins.
            let at = if edit.trivia_policy.keep_leading {
                start
            } else {
                splice_start
            };
            splice(&mut children, at..at, payload_elems);
        }
        EditKind::Replace => {
            splice(&mut children, splice_start..splice_end + 1, payload_elems);
        }
        EditKind::Remove => {
            splice(&mut children, splice_start..splice_end + 1, Vec::new());
        }
    }

    let new_parent = GreenNode::new(rowan_kind(parent.kind()), children);

    // Re-root: replace the parent subtree, rebuilding only the spine to the root.
    let new_root_green = parent.raw().replace_with(new_parent);

    Ok(CstDocument::from_green_for_edit(new_root_green))
}

/// A green child element. `GreenNode`/`GreenToken` both convert into the (crate-
/// private to rowan) element type via public `From` impls, so we work with this
/// `NodeOrToken` alias and rely on `.into()` at the splice boundary.
type GreenElem = NodeOrToken<GreenNode, GreenToken>;

/// Does `node` live in `doc`'s tree? Compares the green root reached by walking
/// parents against `doc`'s root green node.
fn belongs_to(node: &SyntaxNode, doc: &CstDocument) -> bool {
    let mut top = node.clone();
    while let Some(p) = top.parent() {
        top = p;
    }
    top == doc.root()
}

/// Resolve an [`EditTarget`] to its parent node and the inclusive child-index
/// range `[start, end]` it spans within that parent.
fn resolve_target(target: &EditTarget) -> Result<(SyntaxNode, usize, usize), EditError> {
    match target {
        EditTarget::Node(node) => {
            let parent = node.parent().ok_or(EditError::RootNotEditable)?;
            let idx = child_index_of_node(&parent, node).ok_or(EditError::TargetNotFound)?;
            Ok((parent, idx, idx))
        }
        EditTarget::TokenSpan { first, last } => {
            let parent = first.parent().ok_or(EditError::RootNotEditable)?;
            let last_parent = last.parent().ok_or(EditError::RootNotEditable)?;
            if parent != last_parent {
                return Err(EditError::SpanParentMismatch);
            }
            let start = child_index_of_token(&parent, first).ok_or(EditError::TargetNotFound)?;
            let end = child_index_of_token(&parent, last).ok_or(EditError::TargetNotFound)?;
            if end < start {
                return Err(EditError::SpanOutOfOrder);
            }
            Ok((parent, start, end))
        }
    }
}

/// Index of `node` among its parent's children (nodes + tokens), if present.
fn child_index_of_node(parent: &SyntaxNode, node: &SyntaxNode) -> Option<usize> {
    parent
        .children_with_tokens()
        .position(|el| el.as_node() == Some(node))
}

/// Index of `token` among its parent's children (nodes + tokens), if present.
fn child_index_of_token(parent: &SyntaxNode, token: &SyntaxToken) -> Option<usize> {
    parent
        .children_with_tokens()
        .position(|el| el.as_token() == Some(token))
}

/// Move `start` left past any immediately-preceding trivia tokens.
fn absorb_leading_trivia(parent: &rowan::GreenNodeData, start: usize) -> usize {
    let kinds = child_kinds(parent);
    let mut i = start;
    while i > 0 && kinds[i - 1].is_trivia() {
        i -= 1;
    }
    i
}

/// Move `end` right past any immediately-following trivia tokens.
fn absorb_trailing_trivia(parent: &rowan::GreenNodeData, end: usize, count: usize) -> usize {
    let kinds = child_kinds(parent);
    let mut i = end;
    while i + 1 < count && kinds[i + 1].is_trivia() {
        i += 1;
    }
    i
}

/// The [`SyntaxKind`] of each direct child of a green node, in order.
fn child_kinds(parent: &rowan::GreenNodeData) -> Vec<SyntaxKind> {
    parent
        .children()
        .map(|c| {
            let raw = match c {
                NodeOrToken::Node(n) => n.kind(),
                NodeOrToken::Token(t) => t.kind(),
            };
            SyntaxKind::from_raw(raw.0).unwrap_or(SyntaxKind::Error)
        })
        .collect()
}

/// Lex `text` into a flat run of raw green tokens (no structural parse).
///
/// The payload bytes are preserved exactly: each lexer token becomes a green
/// token of the same kind and verbatim text, so a replace/insert splices the
/// payload in byte-for-byte. Structural re-classification is intentionally
/// avoided here so the edit never reflows surrounding bytes.
fn payload_to_green(text: &str) -> Vec<GreenElem> {
    lexer::tokenize(text)
        .into_iter()
        .map(|t| NodeOrToken::Token(GreenToken::new(rowan_kind(t.kind), t.text)))
        .collect()
}

/// Splice `replacement` into `children` over the half-open index `range`.
///
/// Converts each element to rowan's green element type at the boundary via the
/// public `From` impls.
fn splice(
    children: &mut Vec<GreenElem>,
    range: std::ops::Range<usize>,
    replacement: Vec<GreenElem>,
) {
    children.splice(range, replacement);
}

#[inline]
fn rowan_kind(kind: SyntaxKind) -> rowan::SyntaxKind {
    <RonLang as rowan::Language>::kind_to_raw(kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;
    use crate::printer::print;

    /// Find the first descendant node of `kind` in the document.
    fn first_node(doc: &CstDocument, kind: SyntaxKind) -> SyntaxNode {
        fn walk(n: SyntaxNode, kind: SyntaxKind, out: &mut Option<SyntaxNode>) {
            if out.is_some() {
                return;
            }
            if n.kind() == kind {
                *out = Some(n.clone());
                return;
            }
            for c in n.children() {
                walk(c, kind, out);
            }
        }
        let mut out = None;
        walk(doc.root(), kind, &mut out);
        out.unwrap_or_else(|| panic!("no {kind:?} node found"))
    }

    #[test]
    fn replace_node_keeps_unaffected_regions() {
        let src = "Foo(x: 1, y: 2)";
        let doc = parse(src);
        // Replace the first struct field value `1` (its Literal node).
        let field = first_node(&doc, SyntaxKind::StructField);
        let lit = field
            .children()
            .find(|c| c.kind() == SyntaxKind::Literal)
            .unwrap();
        let edited = apply_edit(
            &doc,
            EditOperation::replace(EditTarget::Node(lit), "99", TriviaPolicy::KEEP_ALL),
        )
        .unwrap();
        assert_eq!(print(&edited), "Foo(x: 99, y: 2)");
        // Original is untouched (non-destructive).
        assert_eq!(print(&doc), src);
    }

    #[test]
    fn remove_node_keep_trivia() {
        let src = "[1, 2, 3]";
        let doc = parse(src);
        let list = first_node(&doc, SyntaxKind::List);
        // Remove the first Literal node (the `1`), keeping adjacent trivia.
        let first_lit = list
            .children()
            .find(|c| c.kind() == SyntaxKind::Literal)
            .unwrap();
        let edited = apply_edit(
            &doc,
            EditOperation::remove(EditTarget::Node(first_lit), TriviaPolicy::KEEP_ALL),
        )
        .unwrap();
        // `1` removed; the comma+space that followed remain (keep policy).
        assert_eq!(print(&edited), "[, 2, 3]");
    }

    #[test]
    fn insert_before_node() {
        let src = "[1]";
        let doc = parse(src);
        let list = first_node(&doc, SyntaxKind::List);
        let lit = list
            .children()
            .find(|c| c.kind() == SyntaxKind::Literal)
            .unwrap();
        let edited = apply_edit(
            &doc,
            EditOperation::insert(EditTarget::Node(lit), "0, ", TriviaPolicy::KEEP_ALL),
        )
        .unwrap();
        assert_eq!(print(&edited), "[0, 1]");
    }

    #[test]
    fn token_span_replace() {
        let src = "Foo(x: 1)";
        let doc = parse(src);
        // Replace the name token `Foo` (a single-token span) with `Bar`.
        let strukt = first_node(&doc, SyntaxKind::Struct);
        let name = strukt.first_token_of(SyntaxKind::Ident).unwrap();
        let edited = apply_edit(
            &doc,
            EditOperation::replace(
                EditTarget::TokenSpan {
                    first: name.clone(),
                    last: name,
                },
                "Bar",
                TriviaPolicy::KEEP_ALL,
            ),
        )
        .unwrap();
        assert_eq!(print(&edited), "Bar(x: 1)");
    }

    #[test]
    fn root_is_not_editable() {
        let doc = parse("1");
        let err = apply_edit(
            &doc,
            EditOperation::remove(EditTarget::Node(doc.root()), TriviaPolicy::KEEP_ALL),
        )
        .unwrap_err();
        assert_eq!(err, EditError::RootNotEditable);
    }
}
