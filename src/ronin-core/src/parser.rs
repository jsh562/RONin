//! The RON parser: tokens → a lossless rowan green tree, wrapped in [`CstDocument`].
//!
//! Hand-written recursive-descent (AD-006) feeding [`rowan::GreenNodeBuilder`].
//! It covers the full RON 0.12 value surface (T014): structs (named + anonymous),
//! tuples, lists/sequences, maps (incl. non-string keys), enum variants, unit
//! `()`, `Option`/`implicit_some`, and extension attributes.
//!
//! # Trivia (AD-001, T015)
//!
//! Trivia is attached per the rule documented in [`crate::syntax`]: leading
//! trivia (whitespace / comments / BOM) binds to the **following** significant
//! token; trailing trivia at EOF binds to the [`SyntaxKind::Root`] node. The
//! parser realizes this by, before consuming any significant token, flushing all
//! pending trivia tokens into the current open node, then any trailing trivia at
//! EOF into the still-open `Root` node.
//!
//! # Losslessness (INV-1/INV-2)
//!
//! Every lexer token — significant or trivia — is emitted into the green tree
//! exactly once, in source order, so concatenating all token texts reproduces
//! the input byte-for-byte. This holds for valid input **and** for malformed
//! input that triggers error recovery (INV-3).
//!
//! # Error recovery + diagnostics (OBJ2, T021–T024)
//!
//! Malformed or incomplete input never panics and never drops bytes. The parser:
//!
//! * wraps unexpected tokens in [`SyntaxKind::Error`] nodes and represents absent
//!   constructs as missing/empty nodes, using recovery sets on `,` `)` `]` `}`
//!   and field identifiers (T021), so the tree always covers all input (INV-3);
//! * emits exactly one [`Diagnostic`] per recovery point with a precise byte
//!   range (T022, TR-006/TR-013) and enforces the must-consume-a-token invariant
//!   — every loop iteration consumes ≥ 1 token — so parsing always terminates
//!   (HINT-004);
//! * enforces a configurable nesting-depth guard (default 128, [`ParseOptions`])
//!   that stops descent at the limit, emits a [`DiagnosticCode::NestingDepthExceeded`]
//!   diagnostic, and still tokenizes the remaining bytes into `Error` nodes so no
//!   stack overflow occurs and byte coverage holds (T023, INV-5);
//! * is deterministic — identical input yields an identical tree **and** an
//!   identical diagnostics set (same codes, order, and ranges), since parsing is
//!   a pure function of the token stream with no nondeterministic inputs (T024,
//!   INV-6/TR-012).

use rowan::GreenNodeBuilder;

use crate::diagnostics::{Diagnostic, DiagnosticCode};
use crate::lexer::{self, LexError, Token};
use crate::syntax::{SyntaxKind, SyntaxNode, TextRange};

/// The default nesting-depth guard (AD-005 / TR-014). Descent past this many
/// nested composite values stops and emits an over-limit diagnostic instead of
/// risking a stack overflow.
pub const DEFAULT_MAX_DEPTH: usize = 128;

/// Configuration for [`parse_with_options`].
///
/// Currently carries only the nesting-depth guard (AD-005). It is
/// `#[non_exhaustive]` so future knobs can be added without a breaking change;
/// construct it via [`ParseOptions::default`] and adjust fields, or use the
/// builder-style [`ParseOptions::with_max_depth`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ParseOptions {
    /// Maximum nesting depth of composite values before the depth guard trips
    /// (default [`DEFAULT_MAX_DEPTH`]). A value of `0` means even the top-level
    /// composite trips the guard.
    pub max_depth: usize,
}

impl Default for ParseOptions {
    #[inline]
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_MAX_DEPTH,
        }
    }
}

impl ParseOptions {
    /// Builder-style override of [`ParseOptions::max_depth`].
    #[inline]
    #[must_use]
    pub fn with_max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = max_depth;
        self
    }
}

/// The parsed, lossless concrete syntax tree of a RON document.
///
/// Holds the green root, any diagnostics produced during parsing (empty for
/// well-formed input; populated by error recovery), and the byte length of the
/// accepted source. Round-trip identity (INV-2/INV-3): concatenating all token
/// texts under the root equals the original source bytes, for valid **and**
/// error-recovered trees.
#[derive(Clone)]
pub struct CstDocument {
    green: rowan::GreenNode,
    diagnostics: Vec<Diagnostic>,
    source_len: usize,
}

impl CstDocument {
    /// The root [`SyntaxNode`] of the tree.
    #[must_use]
    pub fn root(&self) -> SyntaxNode {
        SyntaxNode::new_root(self.green.clone())
    }

    /// Diagnostics produced during parsing (empty for well-formed input).
    ///
    /// Deterministic (INV-6): identical input yields an identical diagnostics
    /// set — same codes, order, and byte ranges — including recovery and
    /// over-limit diagnostics.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Byte length of the accepted source.
    #[must_use]
    pub fn source_len(&self) -> usize {
        self.source_len
    }

    /// Build a [`CstDocument`] from a green tree produced by an edit splice
    /// (crate-internal; see [`crate::edit`]).
    ///
    /// The spliced tree carries no diagnostics (the edit produces a fully
    /// printable tree, INV-8; re-validation is a later-epic concern). `source_len`
    /// is recomputed from the new tree's total text length so it stays consistent.
    #[inline]
    pub(crate) fn from_green_for_edit(green: rowan::GreenNode) -> Self {
        let source_len = usize::from(green.text_len());
        Self {
            green,
            diagnostics: Vec::new(),
            source_len,
        }
    }
}

impl std::fmt::Debug for CstDocument {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CstDocument")
            .field("root", &self.root())
            .field("diagnostics", &self.diagnostics)
            .field("source_len", &self.source_len)
            .finish()
    }
}

/// Parse a UTF-8 `&str` into a lossless [`CstDocument`] with the default
/// [`ParseOptions`] (depth guard [`DEFAULT_MAX_DEPTH`]). Never panics.
#[must_use]
pub fn parse(src: &str) -> CstDocument {
    parse_with_options(src, ParseOptions::default())
}

/// Parse a UTF-8 `&str` into a lossless [`CstDocument`] with explicit
/// [`ParseOptions`] (e.g. a custom nesting-depth guard). Never panics.
#[must_use]
pub fn parse_with_options(src: &str, options: ParseOptions) -> CstDocument {
    let tokens = lexer::tokenize(src);
    Parser::new(src.len(), tokens, options).parse_document()
}

/// Parse raw bytes into a [`CstDocument`], rejecting non-UTF-8 cleanly.
///
/// Uses the default [`ParseOptions`].
///
/// # Errors
///
/// Returns [`LexError`] if `bytes` is not valid UTF-8 (TR-001/AD-008/INV-4).
pub fn parse_bytes(bytes: &[u8]) -> Result<CstDocument, LexError> {
    let src = lexer::validate_utf8(bytes)?;
    Ok(parse(src))
}

/// A token paired with its absolute byte offset into the source, so the parser
/// can attach precise byte ranges to diagnostics (TR-006) without re-scanning.
struct Spanned<'a> {
    tok: Token<'a>,
    /// Absolute start offset of `tok.text` in the source.
    start: usize,
}

struct Parser<'a> {
    tokens: Vec<Spanned<'a>>,
    /// Index of the next unconsumed token.
    cursor: usize,
    builder: GreenNodeBuilder<'static>,
    source_len: usize,
    diagnostics: Vec<Diagnostic>,
    options: ParseOptions,
}

impl<'a> Parser<'a> {
    fn new(source_len: usize, tokens: Vec<Token<'a>>, options: ParseOptions) -> Self {
        // Precompute absolute offsets once; tokens cover every byte in order
        // (INV-1), so the running sum is exact.
        let mut offset = 0usize;
        let spanned = tokens
            .into_iter()
            .map(|tok| {
                let start = offset;
                offset += tok.text.len();
                Spanned { tok, start }
            })
            .collect();
        Self {
            tokens: spanned,
            cursor: 0,
            builder: GreenNodeBuilder::new(),
            source_len,
            diagnostics: Vec::new(),
            options,
        }
    }

    fn parse_document(mut self) -> CstDocument {
        self.builder.start_node(rowan_kind(SyntaxKind::Root));

        // Leading trivia binds to the following token (here: the top-level value
        // or any extension attributes), so flush it inside Root first.
        self.eat_trivia();

        // Zero or more extension attributes `#![enable(...)]` at the top.
        while self.at(SyntaxKind::Hash) {
            self.parse_extension_attr();
            self.eat_trivia();
        }

        // The single top-level value (absent for empty / trivia-only files).
        if !self.at_eof() {
            self.parse_value(0);
        }

        // Trailing trivia at EOF binds to Root (AD-001). Any remaining
        // significant tokens are stray top-level content: wrap them in Error
        // nodes (one diagnostic at the first stray token) so the tree always
        // covers all input (INV-3).
        self.eat_trivia();
        if !self.at_eof() {
            let start = self.current_offset();
            let end = self.source_len;
            self.push_diagnostic(
                DiagnosticCode::UnexpectedToken,
                TextRange::new(start, end),
                "unexpected trailing tokens after the top-level value",
            );
            while !self.at_eof() {
                self.bump_into_error();
                self.eat_trivia();
            }
        }

        self.builder.finish_node(); // Root
        let green = self.builder.finish();
        CstDocument {
            green,
            diagnostics: self.diagnostics,
            source_len: self.source_len,
        }
    }

    // ---- value parsing ---------------------------------------------------

    /// Parse one value at nesting `depth`. Composite values recurse with
    /// `depth + 1`; the depth guard (T023) trips before descending past
    /// `options.max_depth`.
    fn parse_value(&mut self, depth: usize) {
        self.eat_trivia();
        let Some(kind) = self.peek_kind() else {
            return;
        };
        match kind {
            SyntaxKind::LParen | SyntaxKind::LBracket | SyntaxKind::LBrace
                if depth >= self.options.max_depth =>
            {
                // Depth guard (INV-5): stop recursive descent. Still consume the
                // remaining bytes into Error nodes so coverage/round-trip hold.
                self.recover_depth_limit();
            }
            SyntaxKind::LParen => self.parse_tuple_or_struct(None, depth),
            SyntaxKind::LBracket => self.parse_list(depth),
            SyntaxKind::LBrace => self.parse_map(depth),
            SyntaxKind::Ident => self.parse_ident_led(depth),
            SyntaxKind::TrueKw | SyntaxKind::FalseKw => self.parse_literal(),
            SyntaxKind::Integer
            | SyntaxKind::Float
            | SyntaxKind::String
            | SyntaxKind::RawString
            | SyntaxKind::Char => self.parse_literal(),
            // Anything else at value position: an unexpected token. Wrap it in an
            // Error node (keeping the byte) and emit one diagnostic (T022).
            _ => {
                let range = self.current_token_range();
                self.push_diagnostic(DiagnosticCode::UnexpectedToken, range, "expected a value");
                self.bump_into_error();
            }
        }
    }

    /// A value starting with an identifier: either an enum variant
    /// (`Ident`, `Ident(...)`, `Ident{...}`) or a named struct (`Name(...)`),
    /// or a bare ident value. We classify by the following significant token.
    fn parse_ident_led(&mut self, depth: usize) {
        // Look past trivia at the token after the ident.
        let next_sig = self.peek_kind_after_first_significant();
        match next_sig {
            Some(SyntaxKind::LParen) => {
                // `Name( ... )` — named struct/tuple-struct or variant payload.
                // We model it as a Struct if it contains `field:` entries, else
                // a Tuple; decided inside parse_tuple_or_struct by lookahead.
                let name_checkpoint = self.builder.checkpoint();
                self.bump(); // ident (the name)
                self.parse_tuple_or_struct(Some(name_checkpoint), depth);
            }
            Some(SyntaxKind::LBrace) => {
                // `Variant { ... }` — struct-like enum variant.
                self.builder.start_node(rowan_kind(SyntaxKind::EnumVariant));
                self.bump(); // variant ident
                self.eat_trivia();
                self.parse_map_like_braces(depth);
                self.builder.finish_node();
            }
            _ => {
                // Bare identifier: a unit enum variant / unit struct name / bool
                // already handled. Wrap as EnumVariant for a single ident value.
                self.builder.start_node(rowan_kind(SyntaxKind::EnumVariant));
                self.bump(); // ident
                self.builder.finish_node();
            }
        }
    }

    /// Parse `( ... )`. If a leading name checkpoint is given, the open node
    /// wraps the name + parens. The body is classified as a `Struct` when it
    /// contains `field:` entries, otherwise a positional `Tuple`. Empty `()` is
    /// a `Unit`.
    fn parse_tuple_or_struct(&mut self, name_checkpoint: Option<rowan::Checkpoint>, depth: usize) {
        // Decide the node kind by scanning the body for a `field :` pattern.
        let is_struct = self.parens_contain_struct_fields();

        let kind = if is_struct {
            SyntaxKind::Struct
        } else {
            // Distinguish `()` unit from a 1+ element tuple.
            if self.parens_are_empty() {
                SyntaxKind::Unit
            } else {
                SyntaxKind::Tuple
            }
        };

        match name_checkpoint {
            Some(cp) => self.builder.start_node_at(cp, rowan_kind(kind)),
            None => self.builder.start_node(rowan_kind(kind)),
        }

        self.eat_trivia();
        let open = self.current_offset();
        self.expect_bump(SyntaxKind::LParen);
        self.eat_trivia();

        while !self.at_eof() && !self.at(SyntaxKind::RParen) {
            let before = self.cursor;
            if is_struct {
                self.parse_struct_field(depth);
            } else {
                self.parse_value(depth + 1);
            }
            self.eat_trivia();
            if self.at(SyntaxKind::Comma) {
                self.bump();
                self.eat_trivia();
            } else if self.at(SyntaxKind::RParen) || self.at_eof() {
                break;
            } else {
                // Neither a separator nor a closer: recover by wrapping the stray
                // token in an Error node so the loop makes progress (HINT-004).
                self.recover_unexpected_in_group();
            }
            // Must-consume-a-token invariant (HINT-004): if an iteration parsed
            // nothing, force progress to guarantee termination.
            if self.cursor == before {
                self.recover_unexpected_in_group();
            }
        }

        self.eat_trivia();
        self.expect_close(SyntaxKind::RParen, open, "(");
        self.builder.finish_node();
    }

    fn parse_struct_field(&mut self, depth: usize) {
        self.builder.start_node(rowan_kind(SyntaxKind::StructField));
        self.eat_trivia();
        // field name
        if self.at(SyntaxKind::Ident) {
            self.bump();
        }
        self.eat_trivia();
        if self.at(SyntaxKind::Colon) {
            self.bump();
        }
        // Note: a missing `:` here is tolerated silently — RON struct fields
        // always carry a `:`, but the recovery contract favors covering bytes
        // over over-reporting; the enclosing loop's progress guard handles
        // pathological cases.
        self.eat_trivia();
        if !self.at(SyntaxKind::Comma) && !self.at(SyntaxKind::RParen) && !self.at_eof() {
            self.parse_value(depth + 1);
        }
        self.builder.finish_node();
    }

    fn parse_list(&mut self, depth: usize) {
        self.builder.start_node(rowan_kind(SyntaxKind::List));
        self.eat_trivia();
        let open = self.current_offset();
        self.expect_bump(SyntaxKind::LBracket);
        self.eat_trivia();
        while !self.at_eof() && !self.at(SyntaxKind::RBracket) {
            let before = self.cursor;
            self.parse_value(depth + 1);
            self.eat_trivia();
            if self.at(SyntaxKind::Comma) {
                self.bump();
                self.eat_trivia();
            } else if self.at(SyntaxKind::RBracket) || self.at_eof() {
                break;
            } else {
                self.recover_unexpected_in_group();
            }
            if self.cursor == before {
                self.recover_unexpected_in_group();
            }
        }
        self.eat_trivia();
        self.expect_close(SyntaxKind::RBracket, open, "[");
        self.builder.finish_node();
    }

    fn parse_map(&mut self, depth: usize) {
        self.builder.start_node(rowan_kind(SyntaxKind::Map));
        self.parse_map_like_braces(depth);
        self.builder.finish_node();
    }

    /// Parse `{ entry, entry, ... }` assuming the `Map`/`EnumVariant` node is
    /// already open. Consumes the braces and the entries.
    fn parse_map_like_braces(&mut self, depth: usize) {
        self.eat_trivia();
        let open = self.current_offset();
        self.expect_bump(SyntaxKind::LBrace);
        self.eat_trivia();
        while !self.at_eof() && !self.at(SyntaxKind::RBrace) {
            let before = self.cursor;
            self.parse_map_entry(depth);
            self.eat_trivia();
            if self.at(SyntaxKind::Comma) {
                self.bump();
                self.eat_trivia();
            } else if self.at(SyntaxKind::RBrace) || self.at_eof() {
                break;
            } else {
                self.recover_unexpected_in_group();
            }
            if self.cursor == before {
                self.recover_unexpected_in_group();
            }
        }
        self.eat_trivia();
        self.expect_close(SyntaxKind::RBrace, open, "{");
    }

    fn parse_map_entry(&mut self, depth: usize) {
        self.builder.start_node(rowan_kind(SyntaxKind::MapEntry));
        self.eat_trivia();
        // key — any value (incl. non-string keys: numbers, chars, idents, tuples)
        if !self.at(SyntaxKind::Colon) && !self.at(SyntaxKind::RBrace) && !self.at_eof() {
            self.parse_value(depth + 1);
        }
        self.eat_trivia();
        if self.at(SyntaxKind::Colon) {
            self.bump();
        }
        self.eat_trivia();
        if !self.at(SyntaxKind::Comma) && !self.at(SyntaxKind::RBrace) && !self.at_eof() {
            self.parse_value(depth + 1);
        }
        self.builder.finish_node();
    }

    fn parse_literal(&mut self) {
        self.builder.start_node(rowan_kind(SyntaxKind::Literal));
        self.bump(); // the scalar token
        self.builder.finish_node();
    }

    fn parse_extension_attr(&mut self) {
        self.builder
            .start_node(rowan_kind(SyntaxKind::ExtensionAttr));
        // `#` `!` `[` enable ( idents... ) `]` — consume verbatim up to the
        // matching `]`, preserving unknown extensions as text (TR-004).
        self.expect_bump(SyntaxKind::Hash);
        self.eat_trivia();
        if self.at(SyntaxKind::Bang) {
            self.bump();
        }
        self.eat_trivia();
        if self.at(SyntaxKind::LBracket) {
            self.bump();
            self.eat_trivia();
            // Consume everything up to and including the matching `]`.
            let mut depth = 1usize;
            while !self.at_eof() && depth > 0 {
                match self.peek_kind() {
                    Some(SyntaxKind::LBracket) => {
                        depth += 1;
                        self.bump();
                    }
                    Some(SyntaxKind::RBracket) => {
                        depth -= 1;
                        self.bump();
                    }
                    Some(_) => self.bump(),
                    None => break,
                }
                if depth > 0 {
                    self.eat_trivia();
                }
            }
        }
        self.builder.finish_node();
    }

    // ---- recovery --------------------------------------------------------

    /// Recover from an unexpected token inside a composite group: wrap it in an
    /// Error node (one diagnostic) and consume it, guaranteeing loop progress
    /// (HINT-004). Skips over trivia harmlessly first.
    fn recover_unexpected_in_group(&mut self) {
        self.eat_trivia();
        if self.at_eof() {
            return;
        }
        let range = self.current_token_range();
        self.push_diagnostic(
            DiagnosticCode::UnexpectedToken,
            range,
            "unexpected token in delimited group",
        );
        self.bump_into_error();
    }

    /// Depth-guard recovery (T023/INV-5): emit one over-limit diagnostic spanning
    /// the remaining input from the offending open delimiter to EOF, then consume
    /// every remaining token into Error nodes so the tree still covers all bytes
    /// and round-trips. Tokenization is unaffected — only recursive descent stops.
    fn recover_depth_limit(&mut self) {
        self.eat_trivia();
        let start = self.current_offset();
        self.push_diagnostic(
            DiagnosticCode::NestingDepthExceeded,
            TextRange::new(start, self.source_len),
            "nesting depth exceeds the configured limit",
        );
        // Consume all remaining significant tokens (and interleaved trivia) into
        // Error nodes. No recursion → no stack growth (INV-5).
        while !self.at_eof() {
            self.bump_into_error();
            self.eat_trivia();
        }
    }

    // ---- lookahead helpers ----------------------------------------------

    /// Kind of the token at `cursor` (trivia included), or `None` at EOF.
    fn peek_kind(&self) -> Option<SyntaxKind> {
        self.tokens.get(self.cursor).map(|t| t.tok.kind)
    }

    /// Is the next significant (non-trivia) token of `kind`?
    fn at(&self, kind: SyntaxKind) -> bool {
        self.peek_significant() == Some(kind)
    }

    /// Kind of the next significant token from `cursor`, skipping trivia.
    fn peek_significant(&self) -> Option<SyntaxKind> {
        self.tokens[self.cursor..]
            .iter()
            .map(|t| t.tok.kind)
            .find(|k| !k.is_trivia())
    }

    /// Kind of the second significant token from `cursor` (skipping the first
    /// significant token and all trivia). Used to classify ident-led values.
    fn peek_kind_after_first_significant(&self) -> Option<SyntaxKind> {
        let mut sig_seen = 0;
        for t in &self.tokens[self.cursor..] {
            if t.tok.kind.is_trivia() {
                continue;
            }
            sig_seen += 1;
            if sig_seen == 2 {
                return Some(t.tok.kind);
            }
        }
        None
    }

    /// `true` once all significant tokens are consumed.
    fn at_eof(&self) -> bool {
        self.peek_significant().is_none()
    }

    /// Absolute byte offset of the token at `cursor` (or `source_len` at EOF).
    fn current_offset(&self) -> usize {
        self.tokens
            .get(self.cursor)
            .map_or(self.source_len, |t| t.start)
    }

    /// Byte range of the next significant token (skipping trivia), or an empty
    /// range at `source_len` if none remains.
    fn current_token_range(&self) -> TextRange {
        for t in &self.tokens[self.cursor..] {
            if !t.tok.kind.is_trivia() {
                return TextRange::new(t.start, t.start + t.tok.text.len());
            }
        }
        TextRange::new(self.source_len, self.source_len)
    }

    /// Does the upcoming `( ... )` group contain a top-level `ident :` pair
    /// (i.e. is it a struct rather than a positional tuple)? Scans with bracket
    /// depth tracking; does not consume.
    fn parens_contain_struct_fields(&self) -> bool {
        let mut i = self.cursor;
        // Skip to the opening paren.
        while i < self.tokens.len() && self.tokens[i].tok.kind != SyntaxKind::LParen {
            if !self.tokens[i].tok.kind.is_trivia() {
                // A non-trivia, non-`(` token before the paren means we are not
                // at a paren group (shouldn't happen given callers).
                return false;
            }
            i += 1;
        }
        if i >= self.tokens.len() {
            return false;
        }
        i += 1; // past `(`
        let mut depth = 1usize;
        let mut last_significant: Option<SyntaxKind> = None;
        while i < self.tokens.len() && depth > 0 {
            let k = self.tokens[i].tok.kind;
            match k {
                SyntaxKind::LParen | SyntaxKind::LBracket | SyntaxKind::LBrace => depth += 1,
                SyntaxKind::RParen | SyntaxKind::RBracket | SyntaxKind::RBrace => depth -= 1,
                SyntaxKind::Colon
                    if depth == 1
                    // A `:` at the top level of this paren group, immediately
                    // preceded (ignoring trivia) by an ident → struct field.
                    && last_significant == Some(SyntaxKind::Ident) =>
                {
                    return true;
                }
                _ => {}
            }
            if !k.is_trivia() && depth >= 1 {
                last_significant = Some(k);
            }
            i += 1;
        }
        false
    }

    /// Is the upcoming paren group empty (`()` with only trivia inside)?
    fn parens_are_empty(&self) -> bool {
        let mut i = self.cursor;
        while i < self.tokens.len() && self.tokens[i].tok.kind != SyntaxKind::LParen {
            i += 1;
        }
        if i >= self.tokens.len() {
            return false;
        }
        i += 1; // past `(`
        while i < self.tokens.len() {
            let k = self.tokens[i].tok.kind;
            if k.is_trivia() {
                i += 1;
                continue;
            }
            return k == SyntaxKind::RParen;
        }
        false
    }

    // ---- token consumption ----------------------------------------------

    /// Emit all leading trivia tokens at `cursor` into the current open node
    /// (AD-001: leading trivia binds to the following token).
    fn eat_trivia(&mut self) {
        while let Some(spanned) = self.tokens.get(self.cursor) {
            if spanned.tok.kind.is_trivia() {
                self.builder
                    .token(rowan_kind(spanned.tok.kind), spanned.tok.text);
                self.cursor += 1;
            } else {
                break;
            }
        }
    }

    /// Consume one significant token, emitting any preceding trivia first.
    fn bump(&mut self) {
        self.eat_trivia();
        if let Some(spanned) = self.tokens.get(self.cursor) {
            self.builder
                .token(rowan_kind(spanned.tok.kind), spanned.tok.text);
            self.cursor += 1;
        }
    }

    /// Consume one token (significant or not) wrapped in an `Error` node, so
    /// unexpected input still lands in the tree (INV-1/INV-3). Trivia is flushed
    /// outside the error node to keep error spans tight.
    fn bump_into_error(&mut self) {
        self.eat_trivia();
        if let Some(spanned) = self.tokens.get(self.cursor) {
            self.builder.start_node(rowan_kind(SyntaxKind::Error));
            self.builder
                .token(rowan_kind(spanned.tok.kind), spanned.tok.text);
            self.builder.finish_node();
            self.cursor += 1;
        }
    }

    /// Consume the expected significant token if present (no-op + lossless if
    /// absent). The missing-delimiter diagnostic is handled by callers that know
    /// the open-delimiter span (see [`Parser::expect_close`]).
    fn expect_bump(&mut self, kind: SyntaxKind) {
        if self.at(kind) {
            self.bump();
        }
    }

    /// Consume the expected closing delimiter if present; otherwise emit a single
    /// [`DiagnosticCode::UnclosedDelimiter`] diagnostic spanning from the opening
    /// delimiter to the current position (T022/TR-006). The tree's byte coverage
    /// is unaffected — the close is simply synthesized as missing.
    fn expect_close(&mut self, kind: SyntaxKind, open_offset: usize, open: &str) {
        if self.at(kind) {
            self.bump();
        } else {
            let end = self.current_offset();
            self.push_diagnostic(
                DiagnosticCode::UnclosedDelimiter,
                TextRange::new(open_offset, end),
                format!("unclosed delimiter `{open}`"),
            );
        }
    }

    /// Record one diagnostic. Centralized so every recovery point goes through a
    /// single path (one-per-recovery-point, deterministic order; T022/T024).
    fn push_diagnostic(
        &mut self,
        code: DiagnosticCode,
        range: TextRange,
        message: impl Into<String>,
    ) {
        debug_assert!(
            range.start() <= range.end() && range.end() <= self.source_len,
            "diagnostic range must lie within [0, source_len)"
        );
        self.diagnostics.push(Diagnostic::new(code, range, message));
    }
}

#[inline]
fn rowan_kind(kind: SyntaxKind) -> rowan::SyntaxKind {
    <crate::syntax::kind::RonLang as rowan::Language>::kind_to_raw(kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::Severity;

    /// Round-trip helper: concatenate all token texts of the parsed tree.
    fn roundtrip(src: &str) -> String {
        let doc = parse(src);
        doc.root()
            .descendant_tokens()
            .map(|t| t.text().to_string())
            .collect()
    }

    #[test]
    fn roundtrip_covers_all_constructs() {
        let inputs = [
            "",
            "   \n\t",
            "// comment only\n",
            "/* block */",
            "42",
            "-3.14",
            "true",
            "false",
            "'c'",
            "\"hello\\nworld\"",
            "r#\"raw \"q\" str\"#",
            "()",
            "Unit",
            "Some(42)",
            "Foo(x: 1, y: 2.0)",
            "(1, 2, 3)",
            "[1, 2, 3,]",
            "{ \"a\": 1, \"b\": 2, }",
            "{ 1: \"one\", 'c': true }",
            "Point(x: 1.0, y: -2.0)",
            "Enum::A", // `::` is unknown to RON; still must round-trip
            "Variant { field: 1 }",
            "#![enable(implicit_some)]\nSome(5)",
            "#![enable(unwrap_newtypes)]\n#![enable(implicit_some)]\n[1, 2]",
            "\u{FEFF}42",
            "1\r\n2\r\n",
            "  Foo(  a : [ 1 , 2 ] , b : { 'x' : 'y' } )  // trailing\n",
        ];
        for src in inputs {
            assert_eq!(roundtrip(src), src, "round-trip failed for {src:?}");
        }
    }

    /// Well-formed input produces zero diagnostics.
    #[test]
    fn valid_input_has_no_diagnostics() {
        for src in [
            "Foo(x: 1, y: 2.0)",
            "[1, 2, 3,]",
            "{ \"a\": 1 }",
            "Some(())",
            "#![enable(implicit_some)]\nSome(5)",
        ] {
            assert!(
                parse(src).diagnostics().is_empty(),
                "unexpected diagnostics for {src:?}: {:?}",
                parse(src).diagnostics()
            );
        }
    }

    #[test]
    fn parse_bytes_rejects_non_utf8() {
        let bad = [0xFFu8, 0x00];
        assert!(parse_bytes(&bad).is_err());
    }

    #[test]
    fn parse_bytes_accepts_bom() {
        let doc = parse_bytes("\u{FEFF}1".as_bytes()).unwrap();
        let printed: String = doc
            .root()
            .descendant_tokens()
            .map(|t| t.text().to_string())
            .collect();
        assert_eq!(printed, "\u{FEFF}1");
    }

    #[test]
    fn source_len_matches() {
        let src = "Foo(x: 1)";
        let doc = parse(src);
        assert_eq!(doc.source_len(), src.len());
    }

    #[test]
    fn struct_vs_tuple_classification() {
        let s = parse("Foo(x: 1)");
        let has_struct = s.root().descendant_tokens().count() > 0
            && s.root().children().any(|n| n.kind() == SyntaxKind::Struct);
        assert!(has_struct, "named struct should produce a Struct node");

        let t = parse("(1, 2)");
        let has_tuple = t.root().children().any(|n| n.kind() == SyntaxKind::Tuple);
        assert!(has_tuple, "positional parens should produce a Tuple node");

        let u = parse("()");
        let has_unit = u.root().children().any(|n| n.kind() == SyntaxKind::Unit);
        assert!(has_unit, "empty parens should produce a Unit node");
    }

    // ---- OBJ2: diagnostic contract (T025) -------------------------------

    /// Every diagnostic's byte range must lie within `[0, source_len]` and be
    /// well-ordered (TR-006/SC-004). Checks across a malformed-sample set.
    #[test]
    fn diagnostic_ranges_are_within_source() {
        for src in [
            "[1, 2",           // unclosed list
            "Foo(x: 1",        // unclosed struct
            "{ \"a\": 1",      // unclosed map
            "@",               // stray top-level token (lex error)
            "[1 @ 2]",         // stray token inside a list
            "1 2 3",           // stray trailing tokens
            "Foo(x: 1) extra", // trailing content after value
        ] {
            let doc = parse(src);
            for d in doc.diagnostics() {
                assert!(
                    d.range().start() <= d.range().end(),
                    "range ordered for {src:?}"
                );
                assert!(
                    d.range().end() <= doc.source_len(),
                    "range within source for {src:?}: {:?} (len {})",
                    d.range(),
                    doc.source_len()
                );
            }
        }
    }

    /// Recovery diagnostics carry the expected severity and registry code.
    #[test]
    fn recovery_diagnostic_codes_and_severity() {
        let unclosed = parse("[1, 2");
        assert!(unclosed
            .diagnostics()
            .iter()
            .any(|d| d.code() == DiagnosticCode::UnclosedDelimiter
                && d.severity() == Severity::Error));

        let stray = parse("@");
        assert!(stray.diagnostics().iter().any(
            |d| d.code() == DiagnosticCode::UnexpectedToken && d.severity() == Severity::Error
        ));
    }

    /// One diagnostic per recovery point: a single unclosed delimiter yields
    /// exactly one diagnostic.
    #[test]
    fn one_diagnostic_per_recovery_point() {
        let doc = parse("[1, 2");
        let unclosed: Vec<_> = doc
            .diagnostics()
            .iter()
            .filter(|d| d.code() == DiagnosticCode::UnclosedDelimiter)
            .collect();
        assert_eq!(
            unclosed.len(),
            1,
            "exactly one unclosed-delimiter diagnostic"
        );

        // A single stray token yields a single unexpected-token diagnostic.
        let stray = parse("@");
        assert_eq!(stray.diagnostics().len(), 1);
        assert_eq!(
            stray.diagnostics()[0].code(),
            DiagnosticCode::UnexpectedToken
        );
    }

    // ---- OBJ2: error-node coverage (T026) -------------------------------

    /// Malformed input still round-trips byte-for-byte (INV-3) and never panics.
    #[test]
    fn malformed_input_roundtrips() {
        for src in [
            "[1, 2",
            "Foo(x: 1",
            "{ \"a\": 1",
            "Some(",
            "(((",
            "}]) ",
            "@#$%",
            "Foo(x: 1) trailing garbage",
            "[1 2 3]",    // missing commas
            "{a 1, b 2}", // missing colons
            "[1, [2, [3", // nested unclosed
        ] {
            assert_eq!(
                roundtrip(src),
                src,
                "malformed round-trip failed for {src:?}"
            );
        }
    }

    /// Malformed input produces at least one `Error` node somewhere in the tree
    /// (recovery completeness).
    #[test]
    fn malformed_input_has_error_nodes() {
        let doc = parse("@ stray");
        let has_error = doc
            .root()
            .descendant_tokens()
            .any(|t| t.parent().map(|p| p.kind()) == Some(SyntaxKind::Error));
        assert!(
            has_error,
            "expected an Error node for stray top-level tokens"
        );
    }

    // ---- OBJ2: determinism (T024) ---------------------------------------

    /// Identical input yields identical trees and identical diagnostics.
    #[test]
    fn parsing_is_deterministic() {
        for src in ["Foo(x: 1, y: [2, 3", "@ junk ] ) }", "{a: 1, b: 2,"] {
            let a = parse(src);
            let b = parse(src);
            // Same printed tree.
            let ta: String = a
                .root()
                .descendant_tokens()
                .map(|t| t.text().to_string())
                .collect();
            let tb: String = b
                .root()
                .descendant_tokens()
                .map(|t| t.text().to_string())
                .collect();
            assert_eq!(ta, tb);
            // Same diagnostics (codes, order, ranges).
            assert_eq!(
                a.diagnostics(),
                b.diagnostics(),
                "diagnostics differ for {src:?}"
            );
        }
    }

    // ---- OBJ2: depth guard (T027 companion; full test in tests/) --------

    /// At depth bound+1 the guard trips: no overflow, an over-limit diagnostic is
    /// emitted, and the tree still round-trips.
    #[test]
    fn depth_guard_trips_at_bound_plus_one() {
        let depth = 5usize;
        let opts = ParseOptions::default().with_max_depth(depth);
        // depth+1 nested lists.
        let src = format!("{}{}", "[".repeat(depth + 1), "]".repeat(depth + 1));
        let doc = parse_with_options(&src, opts);
        let printed: String = doc
            .root()
            .descendant_tokens()
            .map(|t| t.text().to_string())
            .collect();
        assert_eq!(printed, src, "depth-limited tree must round-trip");
        assert!(
            doc.diagnostics()
                .iter()
                .any(|d| d.code() == DiagnosticCode::NestingDepthExceeded),
            "expected an over-limit diagnostic"
        );
    }

    /// Below the bound, no over-limit diagnostic is emitted.
    #[test]
    fn depth_guard_silent_below_bound() {
        let opts = ParseOptions::default().with_max_depth(10);
        let src = "[[[[1]]]]";
        let doc = parse_with_options(src, opts);
        assert!(!doc
            .diagnostics()
            .iter()
            .any(|d| d.code() == DiagnosticCode::NestingDepthExceeded));
    }
}
