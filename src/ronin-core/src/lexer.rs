//! The RON lexer: source bytes / `&str` → a flat stream of [`Token`]s.
//!
//! Two responsibilities:
//!
//! 1. **UTF-8 boundary (T012, TR-001/AD-008/INV-4).** [`validate_utf8`] accepts
//!    `&[u8]` and returns a borrowed `&str` for valid UTF-8 or a clean
//!    [`LexError`] for invalid UTF-8 — it never panics. A leading UTF-8 BOM is
//!    *not* stripped here; it is preserved and later emitted as a [`SyntaxKind::Bom`]
//!    trivia token so it round-trips (AD-008).
//!
//! 2. **Tokenization (T013, TR-002/TR-004/INV-1).** [`tokenize`] splits a `&str`
//!    into tokens covering the **full** RON 0.12 surface verbatim, such that the
//!    concatenation of every token's text equals the input exactly — every byte
//!    lands in exactly one token (INV-1). Malformed bytes never panic; an
//!    unrecognized run becomes a [`SyntaxKind::LexError`] token so coverage holds.

use crate::syntax::SyntaxKind;

/// A clean lexer error returned at the UTF-8 boundary (never a panic).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    /// Human-readable description of the failure.
    pub message: String,
    /// Byte offset at which the failure was detected, when known.
    pub offset: Option<usize>,
}

impl std::fmt::Display for LexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.offset {
            Some(o) => write!(f, "{} (at byte {o})", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for LexError {}

/// A single lexed token: a kind plus the verbatim source slice it covers.
///
/// `text` is always an exact slice of the input; the sum of all `text` lengths
/// equals the input length (INV-1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token<'a> {
    /// Token classification.
    pub kind: SyntaxKind,
    /// Verbatim source text for this token (never normalized).
    pub text: &'a str,
}

/// The UTF-8 BOM as a `&str` (`\u{FEFF}`, 3 bytes: `EF BB BF`).
const BOM_STR: &str = "\u{FEFF}";

/// Validate that `bytes` is UTF-8 and return it as `&str`, or a clean error.
///
/// Never panics. A leading BOM is left intact for the tokenizer to preserve as
/// trivia (AD-008/INV-4).
///
/// # Errors
///
/// Returns [`LexError`] if `bytes` is not valid UTF-8, with the byte offset of
/// the first invalid sequence.
pub fn validate_utf8(bytes: &[u8]) -> Result<&str, LexError> {
    match std::str::from_utf8(bytes) {
        Ok(s) => Ok(s),
        Err(e) => Err(LexError {
            message: "input is not valid UTF-8".to_string(),
            offset: Some(e.valid_up_to()),
        }),
    }
}

/// Tokenize a `&str` into the full RON surface. Total over all input.
///
/// Guarantees (INV-1): the returned tokens, concatenated in order, reproduce
/// `src` byte-for-byte. Never panics.
#[must_use]
pub fn tokenize(src: &str) -> Vec<Token<'_>> {
    let mut lexer = Lexer::new(src);
    let mut tokens = Vec::new();
    while let Some(tok) = lexer.next_token() {
        tokens.push(tok);
    }
    debug_assert_eq!(
        tokens.iter().map(|t| t.text.len()).sum::<usize>(),
        src.len(),
        "lexer must cover every source byte exactly once (INV-1)"
    );
    tokens
}

struct Lexer<'a> {
    src: &'a str,
    /// Absolute byte offset of the next unconsumed character.
    pos: usize,
    /// `true` until the first token is produced (for leading-BOM detection).
    at_start: bool,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            pos: 0,
            at_start: true,
        }
    }

    /// Remaining unconsumed source.
    #[inline]
    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    /// Peek the next `char` without consuming.
    #[inline]
    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    /// Peek the char `n` chars ahead (0 == next), without consuming.
    #[inline]
    fn peek_nth(&self, n: usize) -> Option<char> {
        self.rest().chars().nth(n)
    }

    /// Emit a token covering `[start, self.pos)` with `kind`.
    #[inline]
    fn emit(&self, kind: SyntaxKind, start: usize) -> Token<'a> {
        Token {
            kind,
            text: &self.src[start..self.pos],
        }
    }

    fn next_token(&mut self) -> Option<Token<'a>> {
        if self.pos >= self.src.len() {
            return None;
        }
        let start = self.pos;

        // Leading BOM is its own trivia token (AD-008), only at the very start.
        if self.at_start && self.rest().starts_with(BOM_STR) {
            self.pos += BOM_STR.len();
            self.at_start = false;
            return Some(self.emit(SyntaxKind::Bom, start));
        }
        self.at_start = false;

        let c = self.peek().expect("non-empty rest has a char");

        let token = match c {
            c if is_whitespace(c) => self.lex_whitespace(start),
            '/' if self.peek_nth(1) == Some('/') => self.lex_line_comment(start),
            '/' if self.peek_nth(1) == Some('*') => self.lex_block_comment(start),
            '"' => self.lex_string(start),
            'r' if matches!(self.peek_nth(1), Some('"') | Some('#')) => self.lex_raw_string(start),
            '\'' => self.lex_char(start),
            '0'..='9' => self.lex_number(start, false),
            '+' | '-' if matches!(self.peek_nth(1), Some('0'..='9') | Some('.')) => {
                self.lex_number(start, true)
            }
            '.' if matches!(self.peek_nth(1), Some('0'..='9')) => self.lex_number(start, false),
            c if is_ident_start(c) => self.lex_ident_or_keyword(start),
            '(' => self.bump_punct(SyntaxKind::LParen, start),
            ')' => self.bump_punct(SyntaxKind::RParen, start),
            '[' => self.bump_punct(SyntaxKind::LBracket, start),
            ']' => self.bump_punct(SyntaxKind::RBracket, start),
            '{' => self.bump_punct(SyntaxKind::LBrace, start),
            '}' => self.bump_punct(SyntaxKind::RBrace, start),
            ':' => self.bump_punct(SyntaxKind::Colon, start),
            ',' => self.bump_punct(SyntaxKind::Comma, start),
            '#' => self.bump_punct(SyntaxKind::Hash, start),
            '!' => self.bump_punct(SyntaxKind::Bang, start),
            // Unknown byte run: consume one char, classify as a lex error.
            _ => {
                self.pos += c.len_utf8();
                self.emit(SyntaxKind::LexError, start)
            }
        };
        Some(token)
    }

    /// Consume the single char `c` already peeked, emit `kind`.
    #[inline]
    fn bump_punct(&mut self, kind: SyntaxKind, start: usize) -> Token<'a> {
        // All punctuation handled here is single-byte ASCII.
        self.pos += 1;
        self.emit(kind, start)
    }

    fn lex_whitespace(&mut self, start: usize) -> Token<'a> {
        while let Some(c) = self.peek() {
            if is_whitespace(c) {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
        self.emit(SyntaxKind::Whitespace, start)
    }

    fn lex_line_comment(&mut self, start: usize) -> Token<'a> {
        // Consume `//` then everything up to (not including) the line break.
        self.pos += 2;
        while let Some(c) = self.peek() {
            if c == '\n' {
                break;
            }
            self.pos += c.len_utf8();
        }
        self.emit(SyntaxKind::LineComment, start)
    }

    fn lex_block_comment(&mut self, start: usize) -> Token<'a> {
        // Consume `/*`, then balance nested `/* ... */`. Unterminated comments
        // run to EOF (still a single token — losslessness holds).
        self.pos += 2;
        let mut depth = 1usize;
        while depth > 0 {
            let Some(c) = self.peek() else { break };
            if c == '/' && self.peek_nth(1) == Some('*') {
                self.pos += 2;
                depth += 1;
            } else if c == '*' && self.peek_nth(1) == Some('/') {
                self.pos += 2;
                depth -= 1;
            } else {
                self.pos += c.len_utf8();
            }
        }
        self.emit(SyntaxKind::BlockComment, start)
    }

    fn lex_string(&mut self, start: usize) -> Token<'a> {
        // Opening quote.
        self.pos += 1;
        while let Some(c) = self.peek() {
            match c {
                '\\' => {
                    // Escape: consume the backslash and the escaped char (if any)
                    // verbatim. Validation is the parser's concern; the lexer only
                    // needs to keep the bytes and not terminate on an escaped quote.
                    self.pos += 1;
                    if let Some(esc) = self.peek() {
                        self.pos += esc.len_utf8();
                    }
                }
                '"' => {
                    self.pos += 1;
                    break;
                }
                _ => self.pos += c.len_utf8(),
            }
        }
        self.emit(SyntaxKind::String, start)
    }

    fn lex_raw_string(&mut self, start: usize) -> Token<'a> {
        // Form: r#*"..."#*  — `r`, then N hashes, then `"`, then body, then `"`
        // followed by the same N hashes. Consume `r`.
        self.pos += 1;
        // Count opening hashes.
        let mut hashes = 0usize;
        while self.peek() == Some('#') {
            self.pos += 1;
            hashes += 1;
        }
        // Expect an opening quote; if absent this is a malformed raw string —
        // keep what we consumed as a single token (round-trip still holds).
        if self.peek() != Some('"') {
            return self.emit(SyntaxKind::RawString, start);
        }
        self.pos += 1; // opening quote
                       // Scan body until a `"` followed by exactly `hashes` hashes.
                       // unterminated → run to EOF
        while let Some(c) = self.peek() {
            if c == '"' {
                // Tentatively consume the quote and check the closing hashes.
                let after_quote = self.pos + 1;
                let mut matched = 0usize;
                let mut probe = after_quote;
                while matched < hashes && self.src[probe..].starts_with('#') {
                    probe += 1;
                    matched += 1;
                }
                if matched == hashes {
                    self.pos = probe;
                    break;
                }
                // Not the real terminator: consume the quote and continue.
                self.pos += 1;
            } else {
                self.pos += c.len_utf8();
            }
        }
        self.emit(SyntaxKind::RawString, start)
    }

    fn lex_char(&mut self, start: usize) -> Token<'a> {
        // Opening `'`.
        self.pos += 1;
        while let Some(c) = self.peek() {
            match c {
                '\\' => {
                    self.pos += 1;
                    if let Some(esc) = self.peek() {
                        self.pos += esc.len_utf8();
                    }
                }
                '\'' => {
                    self.pos += 1;
                    break;
                }
                _ => self.pos += c.len_utf8(),
            }
        }
        self.emit(SyntaxKind::Char, start)
    }

    /// Lex an integer or float. `signed` indicates a leading `+`/`-` was seen.
    fn lex_number(&mut self, start: usize, signed: bool) -> Token<'a> {
        if signed {
            self.pos += 1; // sign char (ASCII)
        }

        // Hex / binary / octal integer prefixes.
        if self.peek() == Some('0') {
            if let Some(radix) = self.peek_nth(1) {
                let base = match radix {
                    'x' | 'X' => Some(16u32),
                    'b' | 'B' => Some(2),
                    'o' | 'O' => Some(8),
                    _ => None,
                };
                if let Some(base) = base {
                    self.pos += 2; // `0x` / `0b` / `0o`
                    self.consume_digits(base);
                    self.consume_type_suffix();
                    return self.emit(SyntaxKind::Integer, start);
                }
            }
        }

        // Decimal integer part.
        self.consume_digits(10);

        let mut is_float = false;

        // Fractional part: a `.` followed by a digit, or `.` not followed by `.`
        // (RON allows `1.` and `.5`). We only treat `.` as a decimal point when
        // it is not the start of a range/field access — RON has no such tokens at
        // value position, so a `.` here is always the float point.
        if self.peek() == Some('.') && self.peek_nth(1) != Some('.') {
            is_float = true;
            self.pos += 1;
            self.consume_digits(10);
        }

        // Exponent.
        if matches!(self.peek(), Some('e') | Some('E')) {
            // Only an exponent if followed by digits or a sign+digits.
            let next = self.peek_nth(1);
            let exp = matches!(next, Some('0'..='9'))
                || (matches!(next, Some('+') | Some('-'))
                    && matches!(self.peek_nth(2), Some('0'..='9')));
            if exp {
                is_float = true;
                self.pos += 1; // e/E
                if matches!(self.peek(), Some('+') | Some('-')) {
                    self.pos += 1;
                }
                self.consume_digits(10);
            }
        }

        self.consume_type_suffix();

        self.emit(
            if is_float {
                SyntaxKind::Float
            } else {
                SyntaxKind::Integer
            },
            start,
        )
    }

    /// Consume a run of digits valid for `base`, allowing `_` separators.
    fn consume_digits(&mut self, base: u32) {
        while let Some(c) = self.peek() {
            if c == '_' || c.is_digit(base) {
                self.pos += 1; // digits and `_` are ASCII (single byte)
            } else {
                break;
            }
        }
    }

    /// Consume an optional numeric type suffix (e.g. `i32`, `u8`, `f64`, `usize`).
    ///
    /// A suffix is an ident-continue run immediately following the numeric body.
    /// Floats keep `f32`/`f64`; ints keep `i*`/`u*`. We accept any ident run as
    /// the suffix verbatim — the parser/validator (later epics) judge validity;
    /// the lexer only preserves bytes.
    fn consume_type_suffix(&mut self) {
        // Only consume if the next char could begin a suffix (a letter).
        if let Some(c) = self.peek() {
            if c.is_ascii_alphabetic() {
                while let Some(c) = self.peek() {
                    if is_ident_continue(c) {
                        self.pos += c.len_utf8();
                    } else {
                        break;
                    }
                }
            }
        }
    }

    fn lex_ident_or_keyword(&mut self, start: usize) -> Token<'a> {
        while let Some(c) = self.peek() {
            if is_ident_continue(c) {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
        let text = &self.src[start..self.pos];
        let kind = match text {
            "true" => SyntaxKind::TrueKw,
            "false" => SyntaxKind::FalseKw,
            "enable" => SyntaxKind::EnableKw,
            _ => SyntaxKind::Ident,
        };
        Token { kind, text }
    }
}

/// Whitespace per RON: ASCII whitespace plus the BOM is handled separately.
#[inline]
fn is_whitespace(c: char) -> bool {
    // Note: a non-leading BOM is *not* whitespace; it falls through to LexError,
    // which still preserves the byte. A leading BOM is handled before this.
    c.is_whitespace()
}

/// Identifier start: Unicode XID-start-ish plus `_`. RON idents follow Rust's.
#[inline]
fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

/// Identifier continue: alphanumeric or `_`.
#[inline]
fn is_ident_continue(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn concat(tokens: &[Token<'_>]) -> String {
        tokens.iter().map(|t| t.text).collect()
    }

    fn kinds(tokens: &[Token<'_>]) -> Vec<SyntaxKind> {
        tokens.iter().map(|t| t.kind).collect()
    }

    #[test]
    fn validate_utf8_accepts_valid() {
        assert_eq!(validate_utf8(b"hello").unwrap(), "hello");
    }

    #[test]
    fn validate_utf8_rejects_invalid_without_panic() {
        let bad = [0xFF, 0xFE, 0x00];
        let err = validate_utf8(&bad).unwrap_err();
        assert_eq!(err.offset, Some(0));
    }

    #[test]
    fn covers_every_byte() {
        let inputs = [
            "",
            "   ",
            "// only a comment",
            "/* nested /* block */ comment */",
            "Foo(x: 1, y: 2.5)",
            "[1, 2, 3,]",
            "{ \"k\": 'c', 4: true }",
            "r#\"raw \"quote\" string\"#",
            "Some(())",
            "#![enable(implicit_some)]\n42",
            "0xFF_u8 0b1010 0o17 1_000.5e-3f64 -1 +2.0",
        ];
        for input in inputs {
            let toks = tokenize(input);
            assert_eq!(concat(&toks), input, "round-trip for {input:?}");
        }
    }

    #[test]
    fn leading_bom_is_trivia() {
        let src = "\u{FEFF}1";
        let toks = tokenize(src);
        assert_eq!(toks[0].kind, SyntaxKind::Bom);
        assert_eq!(toks[0].text, "\u{FEFF}");
        assert_eq!(concat(&toks), src);
    }

    #[test]
    fn raw_string_with_hashes() {
        let src = "r##\"has \"# inside\"##";
        let toks = tokenize(src);
        assert_eq!(kinds(&toks), vec![SyntaxKind::RawString]);
        assert_eq!(toks[0].text, src);
    }

    #[test]
    fn numbers_classified() {
        assert_eq!(tokenize("42")[0].kind, SyntaxKind::Integer);
        assert_eq!(tokenize("0xFF")[0].kind, SyntaxKind::Integer);
        assert_eq!(tokenize("3.14")[0].kind, SyntaxKind::Float);
        assert_eq!(tokenize("1e10")[0].kind, SyntaxKind::Float);
        assert_eq!(tokenize("1_000i64")[0].kind, SyntaxKind::Integer);
    }

    #[test]
    fn keywords_and_idents() {
        assert_eq!(tokenize("true")[0].kind, SyntaxKind::TrueKw);
        assert_eq!(tokenize("false")[0].kind, SyntaxKind::FalseKw);
        assert_eq!(tokenize("enable")[0].kind, SyntaxKind::EnableKw);
        assert_eq!(tokenize("Foo")[0].kind, SyntaxKind::Ident);
    }

    #[test]
    fn unterminated_string_runs_to_eof_without_panic() {
        let src = "\"no end";
        let toks = tokenize(src);
        assert_eq!(toks[0].kind, SyntaxKind::String);
        assert_eq!(concat(&toks), src);
    }

    #[test]
    fn crlf_preserved() {
        let src = "1\r\n2";
        let toks = tokenize(src);
        assert_eq!(concat(&toks), src);
    }
}
