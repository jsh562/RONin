//! The deterministic, lossless-by-semantics RON formatter (E005 Wave 1).
//!
//! The formatter turns a [`CstDocument`] (or a single [`SyntaxNode`] subtree) into
//! a **canonically laid-out** RON string: one element per line for multi-line
//! collections, canonical indentation per nesting depth, normalized spacing, and a
//! deterministic trailing-comma rule. It is the formatter half of RONin's "smart
//! authoring" epic and lives in `ronin-core` so every surface (desktop, future LSP)
//! shares one engine (project-instructions §II, "One Core, Many Surfaces").
//!
//! # Hard invariants (project-instructions §I, "Never Corrupt User Data")
//!
//! The formatter ONLY changes whitespace / layout. It MUST:
//!
//! * **preserve every comment** — line `//` and block `/* */`, including comments
//!   at construct boundaries (before `)`, `]`, `}`) and dangling comments inside
//!   otherwise-empty collections — re-emitted attached to the same node (T007);
//! * **preserve order** — fields, variants, list elements, map entries, tuple
//!   elements stay in source order (never sorted / reordered);
//! * **preserve every name and value** — struct/variant names and all scalar
//!   tokens are emitted verbatim (never normalized / re-escaped);
//! * **be idempotent** — `format(format(x)) == format(x)` (T019);
//! * **be a no-op on failure** — unparseable / in-progress input, or any internal
//!   inconsistency, returns [`FormatResult::NoOp`] with the document byte-unchanged
//!   (T011), and the candidate output is verified to be semantically identical to
//!   the input before it is ever returned (T012, AD-008 verify-before-replace).
//!
//! # WASM-clean (project-instructions §II, INV-9)
//!
//! This module adds **no** filesystem / UI / async / native dependency — it uses
//! only `std` and `ronin-core`'s own CST types, so the `wasm32` build of `ronin-core`
//! stays green.
//!
//! # Canonical style
//!
//! * **Indent** — `indent_width` spaces per nesting depth (clamped to `1..=16`,
//!   default `4`).
//! * **Element-per-line** — a collection that spans more than one line in the
//!   source (or that contains a comment) is laid out one element per line; a
//!   collection that fits on a single source line and has no comments stays on one
//!   line.
//! * **Trailing comma** — a multi-line collection gets a trailing comma after
//!   every element including the last; a single-line collection gets none (T008).
//! * **Spacing** — `name: value` (one space after `:`), `key: value` in maps, one
//!   space after a struct/variant name has no gap before its `(`/`{`, etc.
//! * **Blank lines** — [`BlankLinePolicy::Collapse`] (default) collapses any run of
//!   blank lines to at most one; [`BlankLinePolicy::Preserve`] keeps the original
//!   blank-line count between elements.
//!
//! # Deferred seams
//!
//! The formatter is **structural only** — it lays out the CST it is given and never
//! consults type information. The intelligence that layers on top is deferred to
//! later epics and attaches around (never inside) this pure-CST engine:
//!
//! * **type-aware formatting / completion** (e.g. canonical layout choices keyed to
//!   an expected type) → **E006** (schema-optional type model);
//! * **semantic / CST-backed undo-redo** of a format apply → **E007** (the format
//!   command's buffer replacement is the seam an undo stack records against);
//! * **tree / table structured editing** that would re-emit through this formatter
//!   → **E008**;
//! * **Bevy-registry-aware** formatting (component-aware layout) → **E009**;
//! * **RON⇄JSON interop / `derive`-driven canonicalization** → **E010** (interop
//!   lives outside this CST formatter; the `ron` crate is used there, never here).

use crate::parser::{parse, CstDocument};
use crate::syntax::{SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken};

/// How the formatter treats runs of blank lines between elements / fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlankLinePolicy {
    /// Collapse any run of blank lines to at most one (the default). A blank line
    /// the user inserted between two fields is kept (a single blank), but a run of
    /// several is reduced to one.
    Collapse,
    /// Preserve the original number of blank lines between elements verbatim.
    Preserve,
}

impl Default for BlankLinePolicy {
    #[inline]
    fn default() -> Self {
        Self::Collapse
    }
}

/// Formatter configuration (the formatter-side mirror of `ronin-app`'s
/// `FormattingConfig`). A pure value type with no I/O — WASM-clean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FormatConfig {
    /// Spaces of indent per nesting depth. Constructed values are clamped to
    /// [`FormatConfig::MIN_INDENT`]`..=`[`FormatConfig::MAX_INDENT`] via
    /// [`FormatConfig::new`] / [`FormatConfig::with_indent_width`]; read it back
    /// through [`FormatConfig::indent_width`].
    indent_width: u32,
    /// How runs of blank lines are treated.
    blank_line_policy: BlankLinePolicy,
}

impl FormatConfig {
    /// The smallest permitted indent width (1 space).
    pub const MIN_INDENT: u32 = 1;
    /// The largest permitted indent width (16 spaces).
    pub const MAX_INDENT: u32 = 16;
    /// The default indent width (4 spaces).
    pub const DEFAULT_INDENT: u32 = 4;

    /// Build a config, clamping `indent_width` to the sane range
    /// [`MIN_INDENT`](Self::MIN_INDENT)`..=`[`MAX_INDENT`](Self::MAX_INDENT).
    #[must_use]
    pub fn new(indent_width: u32, blank_line_policy: BlankLinePolicy) -> Self {
        Self {
            indent_width: indent_width.clamp(Self::MIN_INDENT, Self::MAX_INDENT),
            blank_line_policy,
        }
    }

    /// Builder-style override of the indent width (clamped to the sane range).
    #[must_use]
    pub fn with_indent_width(mut self, indent_width: u32) -> Self {
        self.indent_width = indent_width.clamp(Self::MIN_INDENT, Self::MAX_INDENT);
        self
    }

    /// Builder-style override of the blank-line policy.
    #[must_use]
    pub fn with_blank_line_policy(mut self, policy: BlankLinePolicy) -> Self {
        self.blank_line_policy = policy;
        self
    }

    /// The (already-clamped) indent width in spaces.
    #[must_use]
    pub fn indent_width(self) -> u32 {
        self.indent_width
    }

    /// The blank-line policy.
    #[must_use]
    pub fn blank_line_policy(self) -> BlankLinePolicy {
        self.blank_line_policy
    }
}

impl Default for FormatConfig {
    #[inline]
    fn default() -> Self {
        Self {
            indent_width: Self::DEFAULT_INDENT,
            blank_line_policy: BlankLinePolicy::default(),
        }
    }
}

/// The outcome of a format request.
///
/// On success it carries the canonically-formatted text. On any failure path —
/// unparseable input, error-recovered tree, no clean subtree boundary, or a
/// verify-before-replace mismatch — it carries a human-readable reason and the
/// **caller's original bytes are left unchanged** (the formatter never performs a
/// partial rewrite, project-instructions §I).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatResult {
    /// Formatting succeeded; the canonical text is enclosed.
    Formatted(String),
    /// Formatting was declined; the document is unchanged. `reason` explains why
    /// (suitable for a non-blocking notice).
    NoOp {
        /// Why the formatter declined (e.g. "input has parse errors").
        reason: String,
    },
}

impl FormatResult {
    /// A [`FormatResult::NoOp`] carrying `reason`.
    fn no_op(reason: impl Into<String>) -> Self {
        Self::NoOp {
            reason: reason.into(),
        }
    }

    /// The formatted text, if formatting succeeded.
    #[must_use]
    pub fn formatted(&self) -> Option<&str> {
        match self {
            Self::Formatted(s) => Some(s),
            Self::NoOp { .. } => None,
        }
    }

    /// `true` if formatting was declined (a no-op).
    #[must_use]
    pub fn is_no_op(&self) -> bool {
        matches!(self, Self::NoOp { .. })
    }
}

/// Format the whole document into canonical RON (T009).
///
/// Returns [`FormatResult::Formatted`] with the canonical text, or
/// [`FormatResult::NoOp`] (input unchanged) when the input does not parse cleanly
/// (error-recovered tree, T011) or the candidate output fails the
/// verify-before-replace semantic check (T012).
#[must_use]
pub fn format(doc: &CstDocument, config: &FormatConfig) -> FormatResult {
    // T011 — no-op on unparseable / in-progress input: a tree that triggered error
    // recovery is never reflowed (a partial / wrong reflow would corrupt data).
    if !doc.diagnostics().is_empty() {
        return FormatResult::no_op("input has parse errors; formatting skipped");
    }
    let root = doc.root();
    format_subtree(&root, config, FormatScope::WholeDocument)
}

/// Format the smallest enclosing CST subtree for "Format Selection" (T010).
///
/// `node` is expected to be a value-position node (struct / tuple / list / map /
/// enum-variant / unit / literal) or the document root. For any other node — i.e.
/// no clean subtree boundary — this returns [`FormatResult::NoOp`]. The rest of the
/// document is the caller's responsibility (the caller splices the returned text in
/// place); this function only produces the canonical text for `node`'s span.
#[must_use]
pub fn format_node(node: &SyntaxNode, config: &FormatConfig) -> FormatResult {
    // T011 — refuse to format any subtree that contains an error-recovery node:
    // reflowing partially-parsed input risks corruption.
    if subtree_has_errors(node) {
        return FormatResult::no_op("selection contains parse errors; formatting skipped");
    }
    // A clean subtree boundary is a value node or the root. Anything else (a bare
    // StructField, MapEntry, a token, the ExtensionAttr, an Error node) has no
    // standalone canonical form here → no-op (T010).
    let scope = match node.kind() {
        SyntaxKind::Root => FormatScope::WholeDocument,
        SyntaxKind::Struct
        | SyntaxKind::Tuple
        | SyntaxKind::List
        | SyntaxKind::Map
        | SyntaxKind::EnumVariant
        | SyntaxKind::Unit
        | SyntaxKind::Literal => FormatScope::Subtree,
        _ => {
            return FormatResult::no_op(
                "no clean subtree boundary at the selection; formatting skipped",
            )
        }
    };
    format_subtree(node, config, scope)
}

/// Whether a format pass covers the whole document (so it owns leading extension
/// attributes + a final trailing newline) or a single embedded subtree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormatScope {
    WholeDocument,
    Subtree,
}

/// Shared implementation behind [`format`] and [`format_node`].
///
/// Emits the canonical text for `node`, then runs verify-before-replace (T012)
/// before returning it. Any internal inconsistency degrades to a no-op (T011), so
/// the formatter is never the source of a partial / corrupting rewrite.
fn format_subtree(node: &SyntaxNode, config: &FormatConfig, scope: FormatScope) -> FormatResult {
    let original = node.text();

    let mut writer = Writer::new(config);
    match scope {
        FormatScope::WholeDocument => emit_root(node, &mut writer, config),
        FormatScope::Subtree => emit_value_like(node, &mut writer, config),
    }
    let candidate = writer.finish(scope, &original);

    // T012 — verify-before-replace (AD-008): re-parse the candidate and confirm it
    // is SEMANTICALLY identical to the input (same names / order / values /
    // comments, ignoring only whitespace layout). On ANY mismatch, no-op.
    match scope {
        FormatScope::WholeDocument => {
            let reparsed = parse(&candidate);
            if !reparsed.diagnostics().is_empty() {
                return FormatResult::no_op(
                    "internal: formatted output did not re-parse cleanly; left unchanged",
                );
            }
            if !semantically_equal(&original, &candidate) {
                return FormatResult::no_op(
                    "internal: semantic verification failed; document left unchanged",
                );
            }
        }
        FormatScope::Subtree => {
            // A subtree's candidate is verified against the original subtree text by
            // comparing significant + comment token streams directly (the candidate
            // may not be a stand-alone whole document, e.g. a bare literal).
            if !semantic_tokens_equal(&original, &candidate) {
                return FormatResult::no_op(
                    "internal: semantic verification failed; selection left unchanged",
                );
            }
        }
    }

    // Idempotence guard: if the input was already canonical, the candidate equals
    // the original — still a successful Formatted (the caller can compare to detect
    // a no-change result if it cares).
    FormatResult::Formatted(candidate)
}

// =============================================================================
// Trivia model (T007): classify and re-emit comments attached to the right node.
// =============================================================================

/// A comment captured from the CST, with its leading-blank context.
#[derive(Debug, Clone)]
struct Comment {
    /// Verbatim comment text (e.g. `// foo` or `/* bar */`), never normalized.
    text: String,
    /// Number of blank lines that preceded this comment in the source (already
    /// resolved against the blank-line policy by the caller).
    blanks_before: usize,
    /// `true` if this comment began on the same source line as the preceding
    /// significant token (an inline trailing comment, e.g. `1, // note`).
    same_line_as_prev: bool,
}

/// The trivia found between two significant tokens (or around a construct), split
/// into the comments it carries and the blank-line run that trails it.
#[derive(Debug, Clone, Default)]
struct TriviaRun {
    /// Comments in source order.
    comments: Vec<Comment>,
    /// Blank lines after the last comment (or in an all-whitespace run), already
    /// resolved against the policy.
    trailing_blanks: usize,
    /// `true` if the run contained at least one newline (used to decide whether a
    /// following inline comment really is "same line").
    has_newline: bool,
}

impl TriviaRun {
    fn is_empty(&self) -> bool {
        self.comments.is_empty()
    }
}

/// Count the blank lines represented by a run of whitespace text.
///
/// A "blank line" is a newline beyond the first: `"\n"` (end of one line) is zero
/// blanks; `"\n\n"` is one blank line; `"\n\n\n"` is two. CRLF is handled by
/// counting `\n` only.
fn count_blank_lines(ws: &str) -> usize {
    let newlines = ws.bytes().filter(|&b| b == b'\n').count();
    newlines.saturating_sub(1)
}

/// Resolve a raw blank-line count against the policy.
fn resolve_blanks(raw: usize, policy: BlankLinePolicy) -> usize {
    match policy {
        BlankLinePolicy::Collapse => raw.min(1),
        BlankLinePolicy::Preserve => raw,
    }
}

// =============================================================================
// Emit: the canonical layout walk.
// =============================================================================

/// A small indent-tracking string writer.
struct Writer {
    out: String,
    indent_level: usize,
    indent_width: usize,
    /// `true` when the current line has no content yet (so indentation is pending).
    at_line_start: bool,
}

impl Writer {
    fn new(config: &FormatConfig) -> Self {
        Self {
            out: String::new(),
            indent_level: 0,
            indent_width: config.indent_width() as usize,
            at_line_start: true,
        }
    }

    fn indent(&mut self) {
        self.indent_level += 1;
    }

    fn dedent(&mut self) {
        self.indent_level = self.indent_level.saturating_sub(1);
    }

    /// Write `s` (no newlines expected inside) at the current position, emitting
    /// pending indentation first if at line start.
    fn write(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        if self.at_line_start {
            for _ in 0..(self.indent_level * self.indent_width) {
                self.out.push(' ');
            }
            self.at_line_start = false;
        }
        self.out.push_str(s);
    }

    /// End the current line.
    fn newline(&mut self) {
        // Trim trailing spaces on the line we are closing (no trailing whitespace).
        while self.out.ends_with(' ') {
            self.out.pop();
        }
        self.out.push('\n');
        self.at_line_start = true;
    }

    /// Emit `count` blank lines (each an empty line).
    fn blank_lines(&mut self, count: usize) {
        for _ in 0..count {
            // A blank line is just a newline with no content.
            while self.out.ends_with(' ') {
                self.out.pop();
            }
            self.out.push('\n');
            self.at_line_start = true;
        }
    }

    /// Finish the formatted text, normalizing the document-final newline.
    fn finish(mut self, scope: FormatScope, original: &str) -> String {
        // Trim trailing spaces on the final line.
        while self.out.ends_with(' ') {
            self.out.pop();
        }
        match scope {
            FormatScope::WholeDocument => {
                // Canonical whole-document output ends in exactly one newline when
                // there is content; an empty/trivia-only doc keeps its emptiness.
                while self.out.ends_with('\n') {
                    self.out.pop();
                }
                // Preserve a leading BOM if the original carried one.
                if original.starts_with('\u{FEFF}') && !self.out.starts_with('\u{FEFF}') {
                    self.out.insert(0, '\u{FEFF}');
                }
                let body_is_empty =
                    self.out.is_empty() || self.out == "\u{FEFF}" || self.out.trim().is_empty();
                if !body_is_empty {
                    self.out.push('\n');
                }
                self.out
            }
            FormatScope::Subtree => {
                // A subtree carries no document-final newline; strip any trailing
                // newline we may have emitted so the splice site stays exact.
                while self.out.ends_with('\n') {
                    self.out.pop();
                }
                self.out
            }
        }
    }
}

/// Emit the canonical layout for the document root.
///
/// Walks the root's children in source order, emitting each significant child
/// (extension attributes, the single top-level value) on its own line(s) and
/// threading every comment between them so nothing is lost (T007). A leading BOM is
/// re-inserted by [`Writer::finish`], so it is skipped here.
fn emit_root(root: &SyntaxNode, w: &mut Writer, config: &FormatConfig) {
    let policy = config.blank_line_policy();
    let children: Vec<SyntaxElement> = root.children_with_tokens().collect();

    // Pending trivia buffer between significant children.
    let mut pending: Vec<SyntaxToken> = Vec::new();
    // Whether we have emitted any significant content yet (so we know when to start
    // a fresh line before the next item).
    let mut emitted_any = false;
    // Whether the last thing emitted was a significant item on the current line
    // (so an inline trailing comment can attach to it).
    let mut last_was_item = false;

    for el in &children {
        match el {
            SyntaxElement::Token(t) if t.is_trivia() => {
                // BOM is layout metadata re-emitted by `finish`; skip it here.
                if t.kind() != SyntaxKind::Bom {
                    pending.push(t.clone());
                }
            }
            SyntaxElement::Node(n) if n.kind() == SyntaxKind::ExtensionAttr => {
                emit_root_pending(&pending, w, policy, last_was_item, &mut emitted_any);
                pending.clear();
                if emitted_any {
                    w.newline();
                }
                emit_extension_attr(n, w);
                emitted_any = true;
                last_was_item = true;
            }
            SyntaxElement::Node(n) if is_value_kind(n.kind()) => {
                emit_root_pending(&pending, w, policy, last_was_item, &mut emitted_any);
                pending.clear();
                if emitted_any {
                    w.newline();
                }
                emit_value_like(n, w, config);
                emitted_any = true;
                last_was_item = true;
            }
            // Any other node (Error etc.) — emit its verbatim trimmed text so bytes
            // are never dropped; verification will catch a real problem.
            SyntaxElement::Node(n) => {
                emit_root_pending(&pending, w, policy, last_was_item, &mut emitted_any);
                pending.clear();
                if emitted_any {
                    w.newline();
                }
                w.write(n.text().trim());
                emitted_any = true;
                last_was_item = true;
            }
            // A stray significant token at root level (recovery): keep it verbatim.
            SyntaxElement::Token(t) => {
                emit_root_pending(&pending, w, policy, last_was_item, &mut emitted_any);
                pending.clear();
                if emitted_any {
                    w.newline();
                }
                w.write(t.text());
                emitted_any = true;
                last_was_item = true;
            }
        }
    }

    // Trailing comments bound to the root (after the last significant child).
    emit_root_pending(&pending, w, policy, last_was_item, &mut emitted_any);
}

/// Emit the buffered root-level trivia (comments) between significant children.
///
/// Comments on their own line are emitted at column 0; a comment that shared the
/// line with the previous item is emitted inline (one space after it). Updates
/// `emitted_any` when it emits content.
fn emit_root_pending(
    pending: &[SyntaxToken],
    w: &mut Writer,
    policy: BlankLinePolicy,
    last_was_item: bool,
    emitted_any: &mut bool,
) {
    if pending.is_empty() {
        return;
    }
    let (inline, leading) = split_pending_trivia(pending);

    // Inline comment(s) on the same line as the previous item.
    let inline_run = build_trivia_run(&inline, policy, last_was_item);
    for c in &inline_run.comments {
        if c.same_line_as_prev && *emitted_any {
            w.write(" ");
            w.write(&c.text);
        } else {
            if *emitted_any {
                w.newline();
                w.blank_lines(c.blanks_before);
            }
            w.write(&c.text);
            *emitted_any = true;
        }
    }

    // Own-line leading comments.
    let leading_run = build_trivia_run(&leading, policy, false);
    for c in &leading_run.comments {
        if *emitted_any {
            w.newline();
            w.blank_lines(c.blanks_before);
        }
        w.write(&c.text);
        *emitted_any = true;
    }
}

/// Emit an extension attribute node verbatim (significant tokens with single
/// spaces, comments preserved). Extension attrs are rare and structurally fixed
/// (`#![enable(a, b)]`), so we emit a conservative canonical form.
fn emit_extension_attr(attr: &SyntaxNode, w: &mut Writer) {
    // Re-emit the significant tokens with no internal reflow other than trimming —
    // the canonical form of `#![enable(implicit_some)]` is itself. We rebuild it
    // from significant tokens to drop any odd internal whitespace, preserving any
    // comments inline.
    let mut first = true;
    let mut prev: Option<SyntaxKind> = None;
    for el in attr.children_with_tokens() {
        if let SyntaxElement::Token(t) = el {
            if t.is_trivia() {
                if matches!(t.kind(), SyntaxKind::LineComment | SyntaxKind::BlockComment) {
                    w.write(" ");
                    w.write(t.text());
                }
                continue;
            }
            let k = t.kind();
            if !first {
                // Space rule inside an extension attr: a space before `enable`-ident
                // after `[`, and `, ` between idents; none around `#`, `!`, `(`, `)`,
                // `[`, `]`.
                if needs_space_in_ext_attr(prev, k) {
                    w.write(" ");
                }
            }
            w.write(t.text());
            first = false;
            prev = Some(k);
        }
    }
}

/// Spacing rule for the two adjacent significant tokens inside an extension attr.
fn needs_space_in_ext_attr(prev: Option<SyntaxKind>, cur: SyntaxKind) -> bool {
    match (prev, cur) {
        // `, ident` → space after comma.
        (Some(SyntaxKind::Comma), _) => true,
        _ => false,
    }
}

/// Whether `kind` is a value-position node kind.
fn is_value_kind(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::Struct
            | SyntaxKind::Tuple
            | SyntaxKind::List
            | SyntaxKind::Map
            | SyntaxKind::EnumVariant
            | SyntaxKind::Unit
            | SyntaxKind::Literal
    )
}

/// Emit any value-like node (dispatch by kind).
fn emit_value_like(node: &SyntaxNode, w: &mut Writer, config: &FormatConfig) {
    match node.kind() {
        SyntaxKind::Literal => emit_literal(node, w),
        SyntaxKind::Unit => emit_unit(node, w, config),
        SyntaxKind::Struct => emit_struct(node, w, config),
        SyntaxKind::Tuple => emit_tuple(node, w, config),
        SyntaxKind::List => emit_list(node, w, config),
        SyntaxKind::Map => emit_map(node, w, config),
        SyntaxKind::EnumVariant => emit_enum_variant(node, w, config),
        SyntaxKind::Root => emit_root(node, w, config),
        // Any other node (Error, etc.) should have been rejected earlier; emit its
        // verbatim text as a last-resort so we never drop bytes (verification will
        // then decide).
        _ => w.write(node.text().trim()),
    }
}

/// Emit a scalar literal verbatim (its single token text).
fn emit_literal(node: &SyntaxNode, w: &mut Writer) {
    if let Some(tok) = node
        .children_with_tokens()
        .filter_map(|el| el.as_token().cloned())
        .find(|t| !t.is_trivia())
    {
        w.write(tok.text());
    }
}

/// Emit the unit value `()` (or a named empty `Foo()`), preserving any dangling
/// comment inside the parens (T007).
///
/// The parser classifies `Foo()` and `()` — and `Foo(/* c */)` / `(/* c */)` —
/// as [`SyntaxKind::Unit`]; this emitter therefore handles a possible leading
/// name and an interior dangling comment so neither the name nor the comment is
/// ever dropped.
fn emit_unit(node: &SyntaxNode, w: &mut Writer, config: &FormatConfig) {
    if let Some(name) = leading_name_token(node) {
        w.write(name.text());
    }
    // No elements; reuse the collection emitter so a dangling comment is threaded.
    emit_paren_collection(node, &[], w, config, EntryKind::Value);
}

/// Emit a bare enum variant (`Ident`) or struct-like variant (`Ident { .. }`).
fn emit_enum_variant(node: &SyntaxNode, w: &mut Writer, config: &FormatConfig) {
    // The variant name token.
    if let Some(name) = node.first_token_of(SyntaxKind::Ident) {
        w.write(name.text());
    }
    // A struct-like payload `{ .. }` if present (an LBrace child token).
    if node
        .children_with_tokens()
        .any(|el| el.kind() == SyntaxKind::LBrace)
    {
        let entries: Vec<SyntaxNode> = node
            .children()
            .filter(|n| n.kind() == SyntaxKind::MapEntry)
            .collect();
        emit_brace_collection(node, &entries, w, config, Delim::Brace, EntryKind::MapEntry);
    }
}

/// Emit a named or anonymous struct.
fn emit_struct(node: &SyntaxNode, w: &mut Writer, config: &FormatConfig) {
    if let Some(name) = node.first_token_of(SyntaxKind::Ident) {
        w.write(name.text());
    }
    let fields: Vec<SyntaxNode> = node
        .children()
        .filter(|n| n.kind() == SyntaxKind::StructField)
        .collect();
    emit_paren_collection(node, &fields, w, config, EntryKind::StructField);
}

/// Emit a positional tuple, including a leading name for a tuple-struct /
/// newtype-variant payload such as `Some(5)` or `Foo(1, 2)`.
fn emit_tuple(node: &SyntaxNode, w: &mut Writer, config: &FormatConfig) {
    // A named tuple (tuple struct / variant payload) carries a leading `Ident`
    // token before its `(`; emit it verbatim so the name is never dropped.
    if let Some(name) = leading_name_token(node) {
        w.write(name.text());
    }
    let items: Vec<SyntaxNode> = node
        .children()
        .filter(|n| is_value_kind(n.kind()))
        .collect();
    emit_paren_collection(node, &items, w, config, EntryKind::Value);
}

/// The leading name `Ident` token of a node (before its first delimiter), if any.
///
/// Distinguishes a tuple-struct / named struct (`Foo(..)`) from an anonymous one.
/// Returns the `Ident` only when it appears before the first `(`/`{`/`[` delimiter
/// (a name), never a stray ident inside the body.
fn leading_name_token(node: &SyntaxNode) -> Option<SyntaxToken> {
    for el in node.children_with_tokens() {
        match el {
            SyntaxElement::Token(t) if t.is_trivia() => continue,
            SyntaxElement::Token(t) if t.kind() == SyntaxKind::Ident => return Some(t),
            // First non-trivia, non-ident element (a delimiter or a value node):
            // there is no leading name.
            _ => return None,
        }
    }
    None
}

/// Emit a list `[ .. ]`.
fn emit_list(node: &SyntaxNode, w: &mut Writer, config: &FormatConfig) {
    let items: Vec<SyntaxNode> = node
        .children()
        .filter(|n| is_value_kind(n.kind()))
        .collect();
    emit_bracket_collection(node, &items, w, config, EntryKind::Value);
}

/// Emit a map `{ .. }`.
fn emit_map(node: &SyntaxNode, w: &mut Writer, config: &FormatConfig) {
    let entries: Vec<SyntaxNode> = node
        .children()
        .filter(|n| n.kind() == SyntaxKind::MapEntry)
        .collect();
    emit_brace_collection(node, &entries, w, config, Delim::Brace, EntryKind::MapEntry);
}

/// The delimiter family of a collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delim {
    Paren,
    Bracket,
    Brace,
}

impl Delim {
    fn open(self) -> &'static str {
        match self {
            Delim::Paren => "(",
            Delim::Bracket => "[",
            Delim::Brace => "{",
        }
    }
    fn close(self) -> &'static str {
        match self {
            Delim::Paren => ")",
            Delim::Bracket => "]",
            Delim::Brace => "}",
        }
    }
    fn close_kind(self) -> SyntaxKind {
        match self {
            Delim::Paren => SyntaxKind::RParen,
            Delim::Bracket => SyntaxKind::RBracket,
            Delim::Brace => SyntaxKind::RBrace,
        }
    }
}

/// The kind of element inside a collection (controls how it is emitted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    /// A `field: value` struct field.
    StructField,
    /// A `key: value` map entry.
    MapEntry,
    /// A bare positional value (list/tuple element).
    Value,
}

fn emit_paren_collection(
    node: &SyntaxNode,
    elements: &[SyntaxNode],
    w: &mut Writer,
    config: &FormatConfig,
    entry: EntryKind,
) {
    emit_collection(node, elements, w, config, Delim::Paren, entry);
}

fn emit_bracket_collection(
    node: &SyntaxNode,
    elements: &[SyntaxNode],
    w: &mut Writer,
    config: &FormatConfig,
    entry: EntryKind,
) {
    emit_collection(node, elements, w, config, Delim::Bracket, entry);
}

fn emit_brace_collection(
    node: &SyntaxNode,
    elements: &[SyntaxNode],
    w: &mut Writer,
    config: &FormatConfig,
    delim: Delim,
    entry: EntryKind,
) {
    emit_collection(node, elements, w, config, delim, entry);
}

/// The core collection emitter: decide single- vs multi-line, then lay out the
/// elements, threading comments (T007) and applying the trailing-comma oracle
/// (T008).
fn emit_collection(
    node: &SyntaxNode,
    elements: &[SyntaxNode],
    w: &mut Writer,
    config: &FormatConfig,
    delim: Delim,
    entry: EntryKind,
) {
    // Gather the interior trivia segments around / between elements.
    let layout = collect_collection_layout(node, elements, config, delim);

    let multiline = decide_multiline(node, &layout, elements.len());

    w.write(delim.open());

    if elements.is_empty() {
        // Possibly a dangling comment inside an empty collection (T007).
        emit_empty_collection_body(&layout, w, multiline);
        w.write(delim.close());
        return;
    }

    if multiline {
        w.indent();
        for (i, el) in elements.iter().enumerate() {
            let seg = &layout.before[i];
            // Newline to start this element's line, plus any leading comments.
            w.newline();
            w.blank_lines(if i == 0 { 0 } else { seg_blank(seg) });
            emit_leading_comments_block(seg, w);
            emit_entry(el, w, config, entry);
            // Trailing comma (oracle: multi-line ⇒ every element, incl. last).
            w.write(",");
            // An inline trailing comment after this element (e.g. `1, // note`).
            emit_inline_trailing_comment(&layout.after[i], w);
        }
        // Comments that sit just before the closing delimiter (boundary comments).
        emit_pre_close_comments(&layout.pre_close, w);
        w.dedent();
        w.newline();
        w.write(delim.close());
    } else {
        // Single line: `( a, b, c )`-style with no trailing comma.
        for (i, el) in elements.iter().enumerate() {
            if i > 0 {
                w.write(", ");
            }
            emit_entry(el, w, config, entry);
        }
        w.write(delim.close());
    }
}

/// Blank-lines to emit before an element, derived from its leading trivia segment.
fn seg_blank(seg: &TriviaRun) -> usize {
    seg.trailing_blanks
}

/// Emit an element's leading comments, each on its own line (multi-line layout).
fn emit_leading_comments_block(seg: &TriviaRun, w: &mut Writer) {
    for c in &seg.comments {
        w.write(&c.text);
        w.newline();
        w.blank_lines(c.blanks_before_or(0));
    }
}

impl Comment {
    /// Blank lines before this comment, or `default` when none recorded.
    fn blanks_before_or(&self, _default: usize) -> usize {
        self.blanks_before
    }
}

/// Emit an inline trailing comment (same source line as the element) after the
/// element + comma, e.g. `1, // note`. If the comment was on its own line it is
/// emitted as a leading comment of the *next* element instead, so here we only
/// handle the genuinely-inline case.
fn emit_inline_trailing_comment(seg: &TriviaRun, w: &mut Writer) {
    for c in &seg.comments {
        if c.same_line_as_prev {
            w.write(" ");
            w.write(&c.text);
        } else {
            // Own-line comment trailing the element: put it on its own line.
            w.newline();
            w.blank_lines(c.blanks_before);
            w.write(&c.text);
        }
    }
}

/// Emit comments that sit between the last element and the closing delimiter.
fn emit_pre_close_comments(seg: &TriviaRun, w: &mut Writer) {
    for c in &seg.comments {
        w.newline();
        w.blank_lines(c.blanks_before);
        w.write(&c.text);
    }
}

/// Emit the body of an empty collection — possibly a dangling comment (T007).
fn emit_empty_collection_body(layout: &CollectionLayout, w: &mut Writer, multiline: bool) {
    if layout.pre_close.is_empty() {
        return;
    }
    if multiline {
        w.indent();
        for c in &layout.pre_close.comments {
            w.newline();
            w.blank_lines(c.blanks_before);
            w.write(&c.text);
        }
        w.dedent();
        w.newline();
    } else {
        // Single-line dangling comment: keep it inline with spaces.
        for c in &layout.pre_close.comments {
            w.write(" ");
            w.write(&c.text);
            w.write(" ");
        }
    }
}

/// Emit one collection element by kind.
fn emit_entry(el: &SyntaxNode, w: &mut Writer, config: &FormatConfig, kind: EntryKind) {
    match kind {
        EntryKind::StructField => emit_struct_field(el, w, config),
        EntryKind::MapEntry => emit_map_entry(el, w, config),
        EntryKind::Value => emit_value_like(el, w, config),
    }
}

/// Emit `name: value` for a struct field.
fn emit_struct_field(field: &SyntaxNode, w: &mut Writer, config: &FormatConfig) {
    if let Some(name) = field.first_token_of(SyntaxKind::Ident) {
        w.write(name.text());
    }
    w.write(":");
    if let Some(value) = field.children().find(|n| is_value_kind(n.kind())) {
        w.write(" ");
        emit_value_like(&value, w, config);
    }
}

/// Emit `key: value` for a map entry (the key can be any value).
fn emit_map_entry(entry: &SyntaxNode, w: &mut Writer, config: &FormatConfig) {
    let values: Vec<SyntaxNode> = entry
        .children()
        .filter(|n| is_value_kind(n.kind()))
        .collect();
    if let Some(key) = values.first() {
        emit_value_like(key, w, config);
    }
    w.write(":");
    if let Some(value) = values.get(1) {
        w.write(" ");
        emit_value_like(value, w, config);
    }
}

// =============================================================================
// Collection-layout extraction: split a collection's interior trivia into the
// segments the emitter consumes (before each element, after each element, and
// before the close delimiter).
// =============================================================================

/// The trivia layout of a collection, indexed alongside its elements.
#[derive(Debug, Default)]
struct CollectionLayout {
    /// Trivia immediately before element `i` (leading comments + blank context).
    before: Vec<TriviaRun>,
    /// Trivia immediately after element `i` up to the next separator/element
    /// (an inline trailing comment lives here).
    after: Vec<TriviaRun>,
    /// Trivia between the last element (or the open delim, if empty) and the close
    /// delimiter — boundary / dangling comments (T007).
    pre_close: TriviaRun,
    /// `true` if the open delimiter and close delimiter were on different source
    /// lines in the input (a primary multi-line signal).
    spans_multiple_lines: bool,
}

/// Walk a collection node's `children_with_tokens` and bucket trivia into
/// per-element segments. Elements are identified by their node identity (kind +
/// range) in `elements`.
fn collect_collection_layout(
    node: &SyntaxNode,
    elements: &[SyntaxNode],
    config: &FormatConfig,
    delim: Delim,
) -> CollectionLayout {
    let policy = config.blank_line_policy();
    let mut layout = CollectionLayout {
        before: vec![TriviaRun::default(); elements.len()],
        after: vec![TriviaRun::default(); elements.len()],
        pre_close: TriviaRun::default(),
        spans_multiple_lines: false,
    };

    // Build a flat, ordered list of this node's *direct* children (tokens + nodes),
    // so we can scan from the open delimiter to the close delimiter.
    let children: Vec<SyntaxElement> = node.children_with_tokens().collect();

    // Locate the open delimiter index and the close delimiter index (last matching
    // close token at this level).
    let open_kind = match delim {
        Delim::Paren => SyntaxKind::LParen,
        Delim::Bracket => SyntaxKind::LBracket,
        Delim::Brace => SyntaxKind::LBrace,
    };
    let close_kind = delim.close_kind();

    let open_idx = children
        .iter()
        .position(|el| el.kind() == open_kind && el.as_token().is_some());
    let close_idx = children
        .iter()
        .rposition(|el| el.kind() == close_kind && el.as_token().is_some());

    let (open_idx, close_idx) = match (open_idx, close_idx) {
        (Some(o), Some(c)) if c > o => (o, c),
        // Degenerate (recovery) shape: no clean delimiters; treat as single-line
        // with no captured trivia. Verification will then decide.
        _ => return layout,
    };

    // Determine element node order: a slice over the interior children that are
    // element nodes (in source order).
    let element_ranges: Vec<(usize, usize)> = elements
        .iter()
        .map(|n| {
            let r = n.text_range();
            (r.start(), r.end())
        })
        .collect();

    // Track newline span between open and close for the multi-line signal.
    let mut saw_newline_between = false;

    // Pending trivia buffer (whitespace text + comments) accumulating between
    // significant items.
    let mut pending: Vec<SyntaxToken> = Vec::new();

    // The index of the element we last emitted (so a trailing-comment after it can
    // be bucketed into `after[that]`). `None` before the first element.
    let mut last_element: Option<usize> = None;

    // Helper to find which element (if any) a node child corresponds to.
    let element_index_of = |start: usize, end: usize| -> Option<usize> {
        element_ranges
            .iter()
            .position(|&(s, e)| s == start && e == end)
    };

    for el in &children[(open_idx + 1)..close_idx] {
        match el {
            SyntaxElement::Token(t) if t.is_trivia() => {
                if t.text().contains('\n') {
                    saw_newline_between = true;
                }
                pending.push(t.clone());
            }
            SyntaxElement::Token(t) if t.kind() == SyntaxKind::Comma => {
                // A separator: flush pending trivia. Comments before a comma that
                // are on the same line as the previous element are inline-trailing
                // for that element; otherwise they lead the next element. We attach
                // pending here as `after[last_element]`.
                let run = build_trivia_run(&pending, policy, /*after_element=*/ true);
                if let Some(idx) = last_element {
                    merge_run(&mut layout.after[idx], run);
                } else {
                    // Comma with no preceding element (recovery) — drop into
                    // pre_close as a fallback so comments survive.
                    merge_run(&mut layout.pre_close, run);
                }
                pending.clear();
            }
            SyntaxElement::Node(n) => {
                let r = n.text_range();
                if let Some(idx) = element_index_of(r.start(), r.end()) {
                    // Pending trivia precedes this element. Split it: comments on the
                    // same line as the previous element/comma (before the first
                    // newline) are inline-trailing for `last_element`; the rest lead
                    // this element (T007).
                    let (inline, leading) = split_pending_trivia(&pending);
                    if let Some(prev) = last_element {
                        let inline_run = build_trivia_run(&inline, policy, true);
                        merge_run(&mut layout.after[prev], inline_run);
                    } else if !inline.is_empty() {
                        // No previous element: fold it into this element's leading.
                        let inline_run = build_trivia_run(&inline, policy, false);
                        merge_run(&mut layout.before[idx], inline_run);
                    }
                    let leading_run = build_trivia_run(&leading, policy, false);
                    merge_run(&mut layout.before[idx], leading_run);
                    pending.clear();
                    last_element = Some(idx);
                } else {
                    // A nested non-element node (shouldn't happen for clean trees);
                    // keep its preceding trivia bucketed conservatively.
                    let run = build_trivia_run(&pending, policy, false);
                    merge_run(&mut layout.pre_close, run);
                    pending.clear();
                }
            }
            // Any other significant token inside the group (recovery): flush.
            SyntaxElement::Token(_) => {
                let run = build_trivia_run(&pending, policy, false);
                merge_run(&mut layout.pre_close, run);
                pending.clear();
            }
        }
    }

    // Whatever trivia remains before the close delimiter. Split it: a comment on
    // the same line as the last element/comma is inline-trailing for that element;
    // the rest are pre-close (boundary / dangling) comments (T007).
    let (inline, boundary) = split_pending_trivia(&pending);
    if let Some(prev) = last_element {
        let inline_run = build_trivia_run(&inline, policy, true);
        merge_run(&mut layout.after[prev], inline_run);
    } else {
        let inline_run = build_trivia_run(&inline, policy, false);
        merge_run(&mut layout.pre_close, inline_run);
    }
    let boundary_run = build_trivia_run(&boundary, policy, false);
    merge_run(&mut layout.pre_close, boundary_run);

    layout.spans_multiple_lines = saw_newline_between;
    layout
}

/// Split a buffer of trivia tokens at the first line break.
///
/// Returns `(inline, rest)` where `inline` is the prefix up to and including the
/// first whitespace token that contains a newline (so any comment on the same line
/// as the preceding significant token stays in `inline`, an inline-trailing
/// comment), and `rest` is everything after that newline (leading the next
/// element). If there is no newline at all, everything is `inline`.
fn split_pending_trivia(tokens: &[SyntaxToken]) -> (Vec<SyntaxToken>, Vec<SyntaxToken>) {
    for (i, t) in tokens.iter().enumerate() {
        if t.kind() == SyntaxKind::Whitespace && t.text().contains('\n') {
            // `inline` = tokens[0..i] (the comments/ws before the first newline);
            // `rest` = tokens[i..] (the newline-bearing ws and everything after).
            return (tokens[..i].to_vec(), tokens[i..].to_vec());
        }
    }
    (tokens.to_vec(), Vec::new())
}

/// Merge `src` into `dst` (append comments, take the max blank context).
fn merge_run(dst: &mut TriviaRun, src: TriviaRun) {
    if src.has_newline {
        dst.has_newline = true;
    }
    dst.trailing_blanks = dst.trailing_blanks.max(src.trailing_blanks);
    dst.comments.extend(src.comments);
}

/// Build a [`TriviaRun`] from a buffer of trivia tokens.
///
/// `after_element` hints whether the first comment, if it shares the line with the
/// preceding significant token (no newline before it), is an inline trailing
/// comment.
fn build_trivia_run(
    tokens: &[SyntaxToken],
    policy: BlankLinePolicy,
    after_element: bool,
) -> TriviaRun {
    let mut run = TriviaRun::default();
    let mut blanks_acc = 0usize; // raw blank lines accumulated before the next comment
    let mut seen_newline = false;
    let mut first_comment = true;

    for t in tokens {
        match t.kind() {
            SyntaxKind::Whitespace => {
                if t.text().contains('\n') {
                    seen_newline = true;
                    run.has_newline = true;
                }
                blanks_acc += count_blank_lines(t.text());
            }
            SyntaxKind::Bom => {
                // BOM only appears at document start; ignore inside collections.
            }
            SyntaxKind::LineComment | SyntaxKind::BlockComment => {
                let same_line = first_comment && after_element && !seen_newline;
                run.comments.push(Comment {
                    text: t.text().to_string(),
                    blanks_before: resolve_blanks(blanks_acc, policy),
                    same_line_as_prev: same_line,
                });
                blanks_acc = 0;
                first_comment = false;
                // After a line comment the line necessarily ends; after a block
                // comment it may not, but treat subsequent comments as own-line.
                seen_newline = true;
            }
            _ => {}
        }
    }

    run.trailing_blanks = resolve_blanks(blanks_acc, policy);
    run
}

/// Decide whether a collection lays out multi-line.
///
/// Rules (deterministic, T009):
/// * empty collection → single-line (unless it carries a dangling comment that
///   itself forces multi-line, handled by the comment presence test);
/// * a collection whose open/close delimiters were on different source lines →
///   multi-line;
/// * a collection that contains ANY comment → multi-line (so comments get their
///   own clean lines and are never lost);
/// * otherwise → single-line.
fn decide_multiline(_node: &SyntaxNode, layout: &CollectionLayout, element_count: usize) -> bool {
    if layout.spans_multiple_lines {
        return true;
    }
    // Any comment anywhere in the collection forces multi-line so we never have to
    // jam a comment onto a crowded single line (and never drop one).
    let has_comments = layout.before.iter().any(|r| !r.is_empty())
        || layout.after.iter().any(|r| !r.is_empty())
        || !layout.pre_close.is_empty();
    if has_comments {
        return true;
    }
    let _ = element_count;
    false
}

// =============================================================================
// Error detection + semantic verification (T011, T012).
// =============================================================================

/// `true` if `node` (or any descendant) is an `Error` recovery node.
fn subtree_has_errors(node: &SyntaxNode) -> bool {
    if node.kind() == SyntaxKind::Error {
        return true;
    }
    node.children().any(|c| subtree_has_errors(&c))
}

/// Verify two whole-document strings are semantically identical: same significant
/// tokens (verbatim) in the same order, and same comments (verbatim) in the same
/// order — ignoring only whitespace layout (T012).
fn semantically_equal(a: &str, b: &str) -> bool {
    semantic_tokens_equal(a, b)
}

/// Compare the semantic token streams of two RON fragments: significant tokens and
/// comments, both verbatim and in order; whitespace and BOM are ignored.
///
/// This is the verify-before-replace oracle (AD-008): names, ordering, values, and
/// comments must be byte-identical (modulo layout). Re-lexing via [`parse`] reuses
/// the single engine tokenizer, so no second lexer is introduced.
fn semantic_tokens_equal(a: &str, b: &str) -> bool {
    let ta = semantic_token_stream(a);
    let tb = semantic_token_stream(b);
    ta == tb
}

/// The ordered list of semantically-significant token texts (significant tokens +
/// comments, verbatim), with layout-only tokens dropped.
///
/// Dropped tokens (layout, not data):
/// * `Whitespace` / `Bom` — pure layout;
/// * `Comma` — a separator whose presence/absence is the formatter's
///   trailing-comma canonicalization (T008), never a data change. Element ORDER is
///   still verified because the elements' own tokens stay in order between the
///   commas.
///
/// Kept tokens carry the data the formatter MUST preserve: struct/variant names
/// (`Ident`), all scalar values, delimiters (so structure cannot silently change),
/// and every comment (verbatim).
fn semantic_token_stream(src: &str) -> Vec<(SyntaxKind, String)> {
    let doc = parse(src);
    doc.root()
        .descendant_tokens()
        .filter(|t| {
            !matches!(
                t.kind(),
                SyntaxKind::Whitespace | SyntaxKind::Bom | SyntaxKind::Comma
            )
        })
        .map(|t| (t.kind(), t.text().to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(src: &str) -> String {
        let doc = parse(src);
        match format(&doc, &FormatConfig::default()) {
            FormatResult::Formatted(s) => s,
            FormatResult::NoOp { reason } => panic!("unexpected no-op for {src:?}: {reason}"),
        }
    }

    #[test]
    fn config_clamps_indent() {
        assert_eq!(
            FormatConfig::new(0, BlankLinePolicy::Collapse).indent_width(),
            1
        );
        assert_eq!(
            FormatConfig::new(99, BlankLinePolicy::Collapse).indent_width(),
            16
        );
        assert_eq!(FormatConfig::default().indent_width(), 4);
    }

    #[test]
    fn single_line_collection_stays_single_line() {
        assert_eq!(fmt("[1, 2, 3]"), "[1, 2, 3]\n");
        assert_eq!(fmt("[1,2,3]"), "[1, 2, 3]\n");
        assert_eq!(fmt("(1, 2)"), "(1, 2)\n");
        assert_eq!(fmt("Foo(x: 1, y: 2)"), "Foo(x: 1, y: 2)\n");
    }

    #[test]
    fn single_line_drops_trailing_comma() {
        assert_eq!(fmt("[1, 2, 3,]"), "[1, 2, 3]\n");
    }

    #[test]
    fn multiline_gets_trailing_comma_on_every_element() {
        let out = fmt("[\n1,\n2,\n3\n]");
        assert_eq!(out, "[\n    1,\n    2,\n    3,\n]\n");
    }

    #[test]
    fn multiline_struct_canonical_indent() {
        let out = fmt("Foo(\nx: 1,\ny: 2\n)");
        assert_eq!(out, "Foo(\n    x: 1,\n    y: 2,\n)\n");
    }

    #[test]
    fn nested_indentation() {
        let out = fmt("Foo(\na: [\n1,\n2\n]\n)");
        assert_eq!(out, "Foo(\n    a: [\n        1,\n        2,\n    ],\n)\n");
    }

    #[test]
    fn literal_passthrough() {
        assert_eq!(fmt("42"), "42\n");
        assert_eq!(fmt("  42  "), "42\n");
        assert_eq!(fmt("\"hi\""), "\"hi\"\n");
        assert_eq!(fmt("true"), "true\n");
    }

    #[test]
    fn unit_value() {
        assert_eq!(fmt("()"), "()\n");
    }

    #[test]
    fn comment_forces_multiline_and_is_preserved() {
        let out = fmt("[1, 2] // trailing");
        // The comment trails the value (root-level), preserved.
        assert!(out.contains("// trailing"), "comment lost: {out:?}");
    }

    #[test]
    fn leading_comment_preserved() {
        let out = fmt("// header\n42");
        assert_eq!(out, "// header\n42\n");
    }

    #[test]
    fn inline_field_comment_preserved() {
        let out = fmt("Foo(\nx: 1, // note\ny: 2\n)");
        assert!(out.contains("// note"), "inline comment lost: {out:?}");
        assert!(
            out.contains("x: 1, // note"),
            "inline comment misplaced: {out:?}"
        );
    }

    #[test]
    fn dangling_comment_in_empty_collection_preserved() {
        let out = fmt("[\n// empty\n]");
        assert!(out.contains("// empty"), "dangling comment lost: {out:?}");
    }

    #[test]
    fn boundary_comment_before_close_preserved() {
        let out = fmt("[\n1,\n// last\n]");
        assert!(out.contains("// last"), "boundary comment lost: {out:?}");
    }

    #[test]
    fn idempotent_on_corpus_samples() {
        for src in [
            "[1, 2, 3]",
            "Foo(\nx: 1,\ny: 2\n)",
            "// header\n42\n",
            "{ \"a\": 1, \"b\": 2 }",
            "Foo(\na: [\n1,\n2\n]\n)",
        ] {
            let once = fmt(src);
            let twice = fmt(&once);
            assert_eq!(once, twice, "not idempotent for {src:?}");
        }
    }

    #[test]
    fn no_op_on_parse_errors() {
        let doc = parse("[1, 2");
        assert!(format(&doc, &FormatConfig::default()).is_no_op());
    }

    #[test]
    fn extension_attr_preserved() {
        let out = fmt("#![enable(implicit_some)]\nSome(5)");
        assert!(
            out.contains("#![enable(implicit_some)]"),
            "ext attr lost: {out:?}"
        );
        assert!(out.contains("Some(5)"));
    }

    #[test]
    fn map_canonical() {
        assert_eq!(fmt("{\"a\":1,\"b\":2}"), "{\"a\": 1, \"b\": 2}\n");
    }

    #[test]
    fn format_node_subtree() {
        let doc = parse("Foo(\nx: 1,\ny: 2\n)");
        let value = doc
            .root()
            .children()
            .find(|n| n.kind() == SyntaxKind::Struct)
            .unwrap();
        let res = format_node(&value, &FormatConfig::default());
        match res {
            FormatResult::Formatted(s) => assert_eq!(s, "Foo(\n    x: 1,\n    y: 2,\n)"),
            FormatResult::NoOp { reason } => panic!("subtree no-op: {reason}"),
        }
    }

    #[test]
    fn format_node_rejects_non_value_node() {
        let doc = parse("Foo(x: 1)");
        // A bare StructField is not a clean value-position subtree boundary, so
        // Format Selection on it must no-op (T010).
        let field = find_kind(&doc.root(), SyntaxKind::StructField).expect("has a field");
        assert!(format_node(&field, &FormatConfig::default()).is_no_op());
    }

    /// Find the first descendant node of `kind` (depth-first), if any.
    fn find_kind(node: &SyntaxNode, kind: SyntaxKind) -> Option<SyntaxNode> {
        if node.kind() == kind {
            return Some(node.clone());
        }
        for c in node.children() {
            if let Some(found) = find_kind(&c, kind) {
                return Some(found);
            }
        }
        None
    }

    #[test]
    fn empty_document_stays_empty() {
        assert_eq!(fmt(""), "");
        assert_eq!(fmt("   "), "");
    }

    #[test]
    fn blank_line_collapse_default() {
        let out = fmt("Foo(\nx: 1,\n\n\n\ny: 2\n)");
        // Collapse: at most one blank between fields.
        assert_eq!(out, "Foo(\n    x: 1,\n\n    y: 2,\n)\n");
    }

    #[test]
    fn blank_line_preserve() {
        let doc = parse("Foo(\nx: 1,\n\n\ny: 2\n)");
        let cfg = FormatConfig::new(4, BlankLinePolicy::Preserve);
        let FormatResult::Formatted(out) = format(&doc, &cfg) else {
            panic!("no-op");
        };
        assert_eq!(out, "Foo(\n    x: 1,\n\n\n    y: 2,\n)\n");
    }
}
