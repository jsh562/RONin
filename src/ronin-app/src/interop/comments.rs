//! The comment carrier (FR-008) — how RON comments cross the RON→JSON boundary
//! and are read back.
//!
//! RON comments are CST **trivia** (`ron-core` INV-7 keeps trivia internal). This
//! module reads them via the public token stream
//! ([`ron_core::SyntaxNode::descendant_tokens`]) — it does **NOT** add a new
//! `ron-core` comment API (HINT-003, data-model §CommentCarrier "Comments read via
//! the CST token stream"). Each comment is anchored to the **nearest following
//! value** (its JSON Pointer in the [`CstJsonProjection`] coordinate space), so
//! JSON→RON can re-attach it on read-back (round-trip symmetry, FR-008).
//!
//! Two carriers (data-model §CommentCarrier):
//! * [`CommentMode::JsoncInline`] — the default/primary carrier: comments are
//!   emitted inline as JSONC at the anchored value.
//! * [`CommentMode::Sidecar`] — the strict-JSON fallback: comments are written to
//!   a sibling sidecar map (JSON-Pointer → comment) so they survive strict mode.
//! * [`CommentMode::None`] — pure standard JSON: comments are dropped (each drop
//!   is then reported as a [`crate::interop::LossKind::DroppedComment`] loss).
//!
//! The carrier is built **read-only** over the source CST and the projection
//! coordinate space; it never mutates the tree (data-model §CommentCarrier).

use std::collections::BTreeMap;

use ron_core::syntax::ast::{Document, Value};
use ron_core::{CstDocument, SyntaxKind, SyntaxNode, TextRange};

use crate::interop::pointer::value_pointer_map;

/// Which carrier moves comments across the boundary for one conversion (FR-008).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CommentMode {
    /// JSONC inline comments — the default/primary carrier (FR-008).
    #[default]
    JsoncInline,
    /// A sibling sidecar comment map — the strict-JSON fallback so comments
    /// survive strict mode (FR-008).
    Sidecar,
    /// Pure standard JSON — comments dropped; each drop reported as a loss
    /// (FR-008, FR-007).
    None,
}

impl CommentMode {
    /// The stable lowercase label for this mode (tests key on this, never on a
    /// human-readable string).
    #[inline]
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CommentMode::JsoncInline => "jsonc-inline",
            CommentMode::Sidecar => "sidecar",
            CommentMode::None => "none",
        }
    }

    /// `true` when this mode carries comments at all (JSONC inline or sidecar) —
    /// i.e. comments are NOT dropped. [`CommentMode::None`] returns `false`.
    #[inline]
    #[must_use]
    pub fn preserves_comments(self) -> bool {
        !matches!(self, CommentMode::None)
    }
}

/// The kind of a RON comment token, preserved so JSON→RON can re-emit it in the
/// same style on read-back (FR-008).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentKind {
    /// A line comment `// ...` (`ron_core` [`SyntaxKind::LineComment`]).
    Line,
    /// A block comment `/* ... */` (`ron_core` [`SyntaxKind::BlockComment`]).
    Block,
}

impl CommentKind {
    /// Classify a comment trivia [`SyntaxKind`]; `None` for a non-comment kind.
    #[inline]
    #[must_use]
    fn from_kind(kind: SyntaxKind) -> Option<Self> {
        match kind {
            SyntaxKind::LineComment => Some(CommentKind::Line),
            SyntaxKind::BlockComment => Some(CommentKind::Block),
            _ => None,
        }
    }

    /// The stable lowercase label for this kind.
    #[inline]
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CommentKind::Line => "line",
            CommentKind::Block => "block",
        }
    }
}

/// One comment collected from the CST, anchored to the nearest following value
/// (FR-008).
///
/// Carries the verbatim comment text (exactly as in source — never normalized,
/// data-model §CommentCarrier "Anchored to real positions"), its [`CommentKind`]
/// (line/block), its source byte [`TextRange`], and the JSON Pointer of the value
/// it anchors to (the projection's coordinate space). A comment that follows the
/// last value (a trailing/dangling comment) anchors to the **root** pointer `""`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    /// The verbatim source text of the comment token (incl. the `//` / `/* */`
    /// delimiters), never normalized.
    pub text: String,
    /// Whether this is a line or block comment.
    pub kind: CommentKind,
    /// The comment token's source byte range (real CST span, never fabricated).
    pub source_range: TextRange,
    /// The JSON Pointer (RFC 6901) of the nearest following value the comment
    /// anchors to — `""` for a trailing/dangling comment with no following value.
    pub anchor_pointer: String,
}

/// The comment carrier for one RON→JSON conversion (FR-008, data-model
/// §CommentCarrier).
///
/// Built read-only from the source CST: a CST trivia walk collects every line /
/// block comment ([`Comment`]) and anchors each to the nearest following value's
/// JSON Pointer. The same collected list backs **both** the JSONC inline form
/// ([`Self::inline_comments`]) **and** the strict-mode sidecar map
/// ([`Self::sidecar_map`]) — they are two projections of one comment list, so the
/// two carriers can never disagree. JSON→RON reads comments back from whichever
/// carrier is present (round-trip symmetry, FR-008).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommentCarrier {
    /// The carrier in use for this conversion (FR-008).
    carrier: CommentMode,
    /// Every comment collected from the CST, in source order, each anchored to a
    /// value pointer (FR-008).
    comments: Vec<Comment>,
}

impl CommentCarrier {
    /// Build a carrier from a parsed RON document for the chosen [`CommentMode`]
    /// (FR-008).
    ///
    /// Walks the CST token stream for line / block comments and the value tree
    /// for pointers, anchoring each comment to the nearest following value. The
    /// collected comment list is the same regardless of `mode` — `mode` only
    /// decides how the comments are *emitted* ([`Self::inline_comments`] vs
    /// [`Self::sidecar_map`]); even [`CommentMode::None`] keeps the list so the
    /// dropped comments can be reported as losses (FR-007). The walk is read-only
    /// over the CST (data-model §CommentCarrier).
    #[must_use]
    pub fn from_document(doc: &CstDocument, mode: CommentMode) -> Self {
        let root = doc.root();
        let comments = collect_comments(&root);
        Self {
            carrier: mode,
            comments,
        }
    }

    /// An empty carrier for the given mode (no comments collected). Equivalent to
    /// a document with no comments.
    #[inline]
    #[must_use]
    pub fn empty(mode: CommentMode) -> Self {
        Self {
            carrier: mode,
            comments: Vec::new(),
        }
    }

    /// Build a carrier directly from a pre-collected, anchored comment list — used by
    /// JSON→RON read-back to assemble comments from the JSONC inline stream and/or a
    /// sibling sidecar (FR-008).
    ///
    /// The comments are taken verbatim (each already carries its `anchor_pointer`);
    /// the carrier mode is recorded for symmetry with the RON→JSON side. The list is
    /// the single source both [`inline_comments`](Self::inline_comments) and
    /// [`sidecar_map`](Self::sidecar_map) project from, exactly as
    /// [`from_document`](Self::from_document).
    #[inline]
    #[must_use]
    pub fn from_comments(mode: CommentMode, comments: Vec<Comment>) -> Self {
        Self {
            carrier: mode,
            comments,
        }
    }

    /// The carrier mode in use (FR-008).
    #[inline]
    #[must_use]
    pub fn carrier(&self) -> CommentMode {
        self.carrier
    }

    /// Every collected comment, in source order — the one list both the JSONC
    /// inline form and the sidecar map are projected from (FR-008).
    #[inline]
    #[must_use]
    pub fn comments(&self) -> &[Comment] {
        &self.comments
    }

    /// `true` when no comments were collected (a comment-free document).
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.comments.is_empty()
    }

    /// The number of collected comments.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.comments.len()
    }

    /// The comments to emit **inline** as JSONC, in source order (FR-008).
    ///
    /// Non-empty only when the carrier is [`CommentMode::JsoncInline`]; the strict
    /// and pure-JSON carriers emit no inline comments (they use the sidecar, or
    /// drop). Returns the comment list so the emitter can place each at its
    /// anchored value (US1 emit step).
    #[must_use]
    pub fn inline_comments(&self) -> &[Comment] {
        if matches!(self.carrier, CommentMode::JsoncInline) {
            &self.comments
        } else {
            &[]
        }
    }

    /// The sidecar comment map (JSON-Pointer → ordered comment texts) for strict
    /// mode (FR-008).
    ///
    /// Non-empty only when the carrier is [`CommentMode::Sidecar`]; JSONC and
    /// pure-JSON carriers produce an empty map (JSONC carries comments inline,
    /// pure-JSON drops them). Multiple comments anchored to the same value pointer
    /// are kept in source order under that pointer so the sidecar is faithful and
    /// JSON→RON can re-attach them all. The map is ordered by JSON Pointer for a
    /// deterministic sidecar file (data-model §CommentCarrier "Anchored to real
    /// positions").
    #[must_use]
    pub fn sidecar_map(&self) -> BTreeMap<String, Vec<String>> {
        let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        if !matches!(self.carrier, CommentMode::Sidecar) {
            return map;
        }
        for comment in &self.comments {
            map.entry(comment.anchor_pointer.clone())
                .or_default()
                .push(comment.text.clone());
        }
        map
    }

    /// The number of comments that are **dropped** by the current carrier — i.e.
    /// the count of [`crate::interop::LossKind::DroppedComment`] losses to report
    /// (FR-007).
    ///
    /// Zero for JSONC inline and sidecar (both preserve comments); equal to
    /// [`Self::len`] for [`CommentMode::None`] (pure standard JSON drops every
    /// comment). The loss-map builder (T009) uses this to decide whether to emit a
    /// dropped-comment loss per comment.
    #[must_use]
    pub fn dropped_count(&self) -> usize {
        if self.carrier.preserves_comments() {
            0
        } else {
            self.comments.len()
        }
    }

    /// The dropped comments (their spans/anchors) the loss-map builder reports as
    /// [`crate::interop::LossKind::DroppedComment`] losses (FR-007).
    ///
    /// Empty unless the carrier is [`CommentMode::None`]; then it is every
    /// collected comment so each drop is surfaced (never silently dropped).
    #[must_use]
    pub fn dropped_comments(&self) -> &[Comment] {
        if self.carrier.preserves_comments() {
            &[]
        } else {
            &self.comments
        }
    }
}

/// Collect every line / block comment under `root`, anchoring each to the nearest
/// following value's JSON Pointer.
///
/// Reads comments via the public token stream (INV-7 / HINT-003 — no new
/// `ron-core` API). The value-pointer map ([`value_pointer_map`]) gives the same
/// pointers the [`CstJsonProjection`] index uses, so an anchored comment lands at
/// the projection coordinate JSON→RON read-back understands.
fn collect_comments(root: &SyntaxNode) -> Vec<Comment> {
    // The projection's value pointers, sorted by start offset, so we can find the
    // nearest value at or after each comment with a binary search.
    let pointers = value_pointer_map(root);
    let mut starts: Vec<(usize, &str)> = pointers
        .iter()
        .map(|(range, ptr)| (range.start(), ptr.as_str()))
        .collect();
    starts.sort_by_key(|(start, _)| *start);

    let mut comments = Vec::new();
    for token in root.descendant_tokens() {
        let Some(kind) = CommentKind::from_kind(token.kind()) else {
            continue;
        };
        let range = token.text_range();
        // Anchor to the nearest value that begins at or after the comment's end —
        // "leading trivia binds to the following significant token" (ron-core
        // AD-001). A comment after the last value (trailing/dangling) has no
        // following value and anchors to the root pointer `""`.
        let anchor = nearest_following_pointer(&starts, range.end());
        comments.push(Comment {
            text: token.text().to_string(),
            kind,
            source_range: range,
            anchor_pointer: anchor.to_string(),
        });
    }
    comments
}

/// The JSON Pointer of the value whose start offset is the smallest one `>=
/// offset`; `""` (root) when no value follows.
fn nearest_following_pointer<'a>(starts: &[(usize, &'a str)], offset: usize) -> &'a str {
    // `starts` is sorted by start offset; find the first value at/after `offset`.
    match starts.binary_search_by(|(start, _)| start.cmp(&offset)) {
        Ok(i) => starts[i].1,
        Err(i) => starts.get(i).map_or("", |(_, ptr)| *ptr),
    }
}

/// `true` when a document parses to a root value at all (used by callers to skip
/// comment collection on an empty document). Kept here so callers do not duplicate
/// the cast.
#[must_use]
pub fn has_root_value(doc: &CstDocument) -> bool {
    Document::cast(doc.root()).and_then(|d| d.value()).is_some()
}

/// `true` when `value` is a value-position node a comment can anchor to. Exposed
/// for callers that pre-filter (e.g. JSON→RON read-back).
#[must_use]
pub fn is_anchorable(value: &Value) -> bool {
    !matches!(value, Value::Error(_))
}

#[cfg(test)]
mod tests {
    //! T011 — comment collection + JSONC vs sidecar carriers (FR-008).

    use super::*;

    fn carrier(src: &str, mode: CommentMode) -> CommentCarrier {
        let doc = ron_core::parse(src);
        CommentCarrier::from_document(&doc, mode)
    }

    #[test]
    fn collects_line_and_block_comments_via_token_stream() {
        let src = "// leading\n(x: 1 /* inline */)";
        let c = carrier(src, CommentMode::JsoncInline);
        assert_eq!(c.len(), 2, "both the line and block comment are collected");
        assert_eq!(c.comments()[0].kind, CommentKind::Line);
        assert_eq!(c.comments()[0].text, "// leading");
        assert_eq!(c.comments()[1].kind, CommentKind::Block);
        assert_eq!(c.comments()[1].text, "/* inline */");
        // Real spans, never fabricated.
        assert!(!c.comments()[0].source_range.is_empty());
    }

    #[test]
    fn comment_anchors_to_nearest_following_value() {
        // `// before x` precedes the value at pointer `/x`.
        let src = "(\n  // before x\n  x: 42,\n)";
        let c = carrier(src, CommentMode::JsoncInline);
        assert_eq!(c.len(), 1);
        assert_eq!(
            c.comments()[0].anchor_pointer,
            "/x",
            "the comment binds to the following value pointer /x"
        );
    }

    #[test]
    fn trailing_comment_anchors_to_root() {
        let src = "(x: 1)\n// trailing dangling comment";
        let c = carrier(src, CommentMode::JsoncInline);
        assert_eq!(c.len(), 1);
        assert_eq!(
            c.comments()[0].anchor_pointer,
            "",
            "a comment after the last value anchors to the root pointer"
        );
    }

    #[test]
    fn jsonc_emits_inline_but_not_sidecar() {
        let src = "// c\n(x: 1)";
        let c = carrier(src, CommentMode::JsoncInline);
        assert_eq!(c.carrier(), CommentMode::JsoncInline);
        assert_eq!(c.inline_comments().len(), 1, "JSONC emits inline comments");
        assert!(
            c.sidecar_map().is_empty(),
            "JSONC produces no sidecar (comments are inline)"
        );
        assert_eq!(c.dropped_count(), 0, "JSONC drops nothing");
    }

    #[test]
    fn sidecar_emits_map_but_not_inline() {
        let src = "(\n  // about x\n  x: 1,\n  // about y\n  y: 2,\n)";
        let c = carrier(src, CommentMode::Sidecar);
        assert_eq!(c.carrier(), CommentMode::Sidecar);
        assert!(
            c.inline_comments().is_empty(),
            "the sidecar carrier emits no inline comments"
        );
        let map = c.sidecar_map();
        assert_eq!(map.len(), 2, "two anchored value pointers in the sidecar");
        assert_eq!(
            map.get("/x").map(Vec::as_slice),
            Some(["// about x".to_string()].as_slice())
        );
        assert_eq!(
            map.get("/y").map(Vec::as_slice),
            Some(["// about y".to_string()].as_slice())
        );
        assert_eq!(c.dropped_count(), 0, "the sidecar drops nothing");
    }

    #[test]
    fn sidecar_groups_multiple_comments_under_one_pointer_in_order() {
        // Two comments before the same value `/x` keep source order under `/x`.
        let src = "(\n  // first\n  // second\n  x: 1,\n)";
        let c = carrier(src, CommentMode::Sidecar);
        let map = c.sidecar_map();
        assert_eq!(
            map.get("/x").map(Vec::as_slice),
            Some(["// first".to_string(), "// second".to_string()].as_slice())
        );
    }

    #[test]
    fn pure_json_drops_every_comment_and_reports_each() {
        let src = "// a\n(x: 1 /* b */)";
        let c = carrier(src, CommentMode::None);
        assert_eq!(c.carrier(), CommentMode::None);
        assert!(c.inline_comments().is_empty());
        assert!(c.sidecar_map().is_empty());
        // Every comment is a reportable drop — never silently dropped (FR-007).
        assert_eq!(c.dropped_count(), 2);
        assert_eq!(c.dropped_comments().len(), 2);
    }

    #[test]
    fn comment_free_document_has_empty_carrier() {
        let c = carrier("(x: 1)", CommentMode::JsoncInline);
        assert!(c.is_empty());
        assert_eq!(c.len(), 0);
        assert_eq!(c.dropped_count(), 0);
        assert!(c.dropped_comments().is_empty());
    }

    #[test]
    fn mode_does_not_change_the_collected_list() {
        // The collected comment list is mode-independent; only emission differs.
        let src = "// c\n(x: 1)";
        let jsonc = carrier(src, CommentMode::JsoncInline);
        let sidecar = carrier(src, CommentMode::Sidecar);
        let none = carrier(src, CommentMode::None);
        assert_eq!(jsonc.comments(), sidecar.comments());
        assert_eq!(sidecar.comments(), none.comments());
    }

    #[test]
    fn comment_mode_labels_and_predicates() {
        assert_eq!(CommentMode::JsoncInline.as_str(), "jsonc-inline");
        assert_eq!(CommentMode::Sidecar.as_str(), "sidecar");
        assert_eq!(CommentMode::None.as_str(), "none");
        assert!(CommentMode::JsoncInline.preserves_comments());
        assert!(CommentMode::Sidecar.preserves_comments());
        assert!(!CommentMode::None.preserves_comments());
        assert_eq!(CommentKind::Line.as_str(), "line");
        assert_eq!(CommentKind::Block.as_str(), "block");
    }
}
