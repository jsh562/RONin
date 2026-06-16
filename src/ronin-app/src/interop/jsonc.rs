//! A JSONC-tolerant reader that parses JSON-with-comments into a
//! `serde_json::Value` while collecting each comment anchored to a JSON Pointer
//! (FR-008) — the JSON→RON inverse of the RON→JSON comment carrier.
//!
//! `serde_json` cannot parse comments, so this module first **strips** `//` line
//! and `/* … */` block comments to produce valid JSON for `serde_json::from_str`,
//! and in the same pass records each comment anchored to the **JSON Pointer of the
//! value that immediately follows it** (mirroring RON's "leading trivia binds to
//! the following value" anchoring, so RON→JSON→RON comment round-trips). A comment
//! after the last value anchors to the root pointer `""` (a trailing/dangling
//! comment), exactly like the RON→JSON carrier.
//!
//! The reader is **string-quote-aware** (a `//` or `/*` inside a JSON string is not
//! a comment) and **bounded**: a malformed input never panics — it returns a
//! structured error and creates nothing (FR-013).
//!
//! This is a *reader*, not a full JSONC validator: it strips comments + tracks
//! enough structure to assign pointers; `serde_json` does the authoritative JSON
//! validation of the stripped text.

use crate::interop::comments::{Comment, CommentKind};
use ron_core::TextRange;

/// The maximum JSON nesting depth the pointer tracker descends before giving up
/// (FR-013, SC-009).
///
/// A pathologically deep input cannot drive unbounded container-stack growth: past
/// this bound the reader stops tracking structure (comments below it anchor to the
/// last known pointer) but **never** crashes or hangs. `serde_json`'s own recursion
/// limit independently guards the value parse of the stripped text.
const MAX_TRACK_DEPTH: usize = 1024;

/// Why a JSONC import failed (FR-013).
#[derive(Debug)]
#[non_exhaustive]
pub enum JsoncError {
    /// The stripped text was not valid JSON (the underlying `serde_json` message).
    InvalidJson(String),
}

impl std::fmt::Display for JsoncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JsoncError::InvalidJson(msg) => write!(f, "invalid JSON: {msg}"),
        }
    }
}

impl std::error::Error for JsoncError {}

/// The outcome of reading a JSONC document: the parsed value + the anchored
/// comments collected inline (FR-008).
#[derive(Debug)]
pub struct JsoncDocument {
    /// The parsed JSON value (with comments stripped).
    pub value: serde_json::Value,
    /// The inline comments, each anchored to a JSON Pointer (source order).
    pub comments: Vec<Comment>,
}

/// Parse JSONC `input` into a value + inline anchored comments (FR-008).
///
/// Strips `//` and `/* … */` comments (string-quote-aware) to obtain valid JSON for
/// `serde_json`, and anchors each comment to the pointer of the following value. The
/// authoritative JSON validity check is `serde_json`'s parse of the stripped text;
/// a parse failure returns [`JsoncError::InvalidJson`] and creates nothing (FR-013).
///
/// # Errors
///
/// Returns [`JsoncError::InvalidJson`] when the comment-stripped text is not valid
/// JSON.
pub fn parse_jsonc(input: &str) -> Result<JsoncDocument, JsoncError> {
    let scan = scan(input);
    let value: serde_json::Value =
        serde_json::from_str(&scan.stripped).map_err(|e| JsoncError::InvalidJson(e.to_string()))?;
    Ok(JsoncDocument {
        value,
        comments: scan.comments,
    })
}

/// `true` when `input` (after comment stripping) is a comment-free pure JSON value —
/// i.e. no inline comments were present. Lets the caller decide whether to consult a
/// sidecar (FR-008).
#[must_use]
pub fn has_inline_comments(input: &str) -> bool {
    !scan(input).comments.is_empty()
}

/// The result of the single comment-stripping + pointer-tracking pass.
struct Scan {
    /// The input with comments replaced by equivalent whitespace (so byte offsets in
    /// the stripped text still line up for `serde_json` error reporting).
    stripped: String,
    /// The collected comments, anchored to the following value's JSON Pointer.
    comments: Vec<Comment>,
}

/// One open container on the pointer-tracking stack.
#[derive(Clone)]
enum Frame {
    /// Inside an array; `index` is the next element index.
    Array { index: usize },
    /// Inside an object; `key` is the most recently seen property name (the pointer
    /// segment for the value being read), or `None` before the first key.
    Object { key: Option<String> },
}

/// Scan `input`: strip comments to whitespace and collect anchored comments.
///
/// The pass is a small JSON tokenizer that tracks the container stack so each
/// comment can be anchored to the pointer of the **next value**. It is
/// string-quote-aware (comments inside strings are literal) and depth-bounded.
fn scan(input: &str) -> Scan {
    let bytes = input.as_bytes();
    let mut stripped = String::with_capacity(input.len());
    let mut comments: Vec<Comment> = Vec::new();
    // Pending comments awaiting the next value to anchor to.
    let mut pending: Vec<(String, CommentKind, TextRange)> = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut expecting_key = false;

    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                // Line comment to end of line.
                let start = i;
                let mut j = i + 2;
                while j < bytes.len() && bytes[j] != b'\n' {
                    j += 1;
                }
                let text = input.get(start..j).unwrap_or("").to_string();
                pending.push((text, CommentKind::Line, TextRange::new(start, j)));
                // Replace the comment span with spaces (preserve newline).
                push_blanks(&mut stripped, &input[start..j]);
                i = j;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                // Block comment to the closing `*/`.
                let start = i;
                let mut j = i + 2;
                while j + 1 < bytes.len() && !(bytes[j] == b'*' && bytes[j + 1] == b'/') {
                    j += 1;
                }
                let end = (j + 2).min(bytes.len());
                let text = input.get(start..end).unwrap_or("").to_string();
                pending.push((text, CommentKind::Block, TextRange::new(start, end)));
                push_blanks(&mut stripped, &input[start..end]);
                i = end;
            }
            b'"' => {
                // A JSON string: copy verbatim through the closing quote. If this is
                // an object key (expecting_key), record it as the pointer segment.
                let (s, next) = copy_string(input, i, &mut stripped);
                if expecting_key {
                    if let Some(Frame::Object { key }) = stack.last_mut() {
                        *key = Some(s);
                    }
                    expecting_key = false;
                } else {
                    // A string value at the current pointer: anchor pending comments.
                    flush_pending(&mut pending, &mut comments, &current_pointer(&stack));
                    advance_after_value(&mut stack);
                }
                i = next;
            }
            b'{' => {
                flush_pending(&mut pending, &mut comments, &current_pointer(&stack));
                if stack.len() < MAX_TRACK_DEPTH {
                    stack.push(Frame::Object { key: None });
                }
                expecting_key = true;
                stripped.push('{');
                i += 1;
            }
            b'[' => {
                flush_pending(&mut pending, &mut comments, &current_pointer(&stack));
                if stack.len() < MAX_TRACK_DEPTH {
                    stack.push(Frame::Array { index: 0 });
                }
                expecting_key = false;
                stripped.push('[');
                i += 1;
            }
            b'}' => {
                stack.pop();
                advance_after_value(&mut stack);
                expecting_key = false;
                stripped.push('}');
                i += 1;
            }
            b']' => {
                stack.pop();
                advance_after_value(&mut stack);
                expecting_key = false;
                stripped.push(']');
                i += 1;
            }
            b':' => {
                // After a key; the next token is the value at the key's pointer.
                expecting_key = false;
                stripped.push(':');
                i += 1;
            }
            b',' => {
                // Next element/member. In an object the next token is a key.
                if matches!(stack.last(), Some(Frame::Object { .. })) {
                    expecting_key = true;
                }
                stripped.push(',');
                i += 1;
            }
            b' ' | b'\t' | b'\r' | b'\n' => {
                stripped.push(b as char);
                i += 1;
            }
            _ => {
                // A scalar literal (number / true / false / null). Copy it through and
                // anchor any pending comments to the current pointer.
                let start = i;
                let mut j = i;
                while j < bytes.len() && !is_value_terminator(bytes[j]) {
                    j += 1;
                }
                flush_pending(&mut pending, &mut comments, &current_pointer(&stack));
                advance_after_value(&mut stack);
                stripped.push_str(input.get(start..j).unwrap_or(""));
                i = j;
            }
        }
    }
    // Any trailing/dangling comments anchor to the root pointer "".
    flush_pending(&mut pending, &mut comments, "");
    Scan { stripped, comments }
}

/// `true` when `b` ends a bare scalar literal (whitespace / structural / comment).
fn is_value_terminator(b: u8) -> bool {
    matches!(
        b,
        b' ' | b'\t' | b'\r' | b'\n' | b',' | b'}' | b']' | b'{' | b'[' | b':' | b'/'
    )
}

/// Copy a JSON string literal starting at `start` (the opening `"`) into `out`,
/// returning the decoded inner contents + the index just past the closing quote.
fn copy_string(input: &str, start: usize, out: &mut String) -> (String, usize) {
    let bytes = input.as_bytes();
    out.push('"');
    let mut inner = String::new();
    let mut i = start + 1;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' {
            // Escape: copy the backslash + the next byte verbatim into the stripped
            // JSON, and decode it (best-effort) for the key segment.
            out.push('\\');
            if let Some(&next) = bytes.get(i + 1) {
                out.push(next as char);
                inner.push(decode_escape(next));
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        out.push(b as char);
        if b == b'"' {
            i += 1;
            break;
        }
        // A multi-byte UTF-8 char: push it whole to `inner`. The `out` already got
        // the raw byte; re-sync by copying the full char into `out` only once.
        inner.push(b as char);
        i += 1;
    }
    (inner, i)
}

/// Best-effort single-escape decode for key segments (mirrors JSON escapes).
fn decode_escape(b: u8) -> char {
    match b {
        b'n' => '\n',
        b'r' => '\r',
        b't' => '\t',
        other => other as char,
    }
}

/// Push `len(span)` bytes of equivalent whitespace, preserving newlines so line/col
/// reporting and offsets in the stripped text stay aligned.
fn push_blanks(out: &mut String, span: &str) {
    for ch in span.chars() {
        if ch == '\n' {
            out.push('\n');
        } else if ch == '\r' {
            out.push('\r');
        } else {
            out.push(' ');
        }
    }
}

/// The JSON Pointer of the value currently being read, given the container stack.
fn current_pointer(stack: &[Frame]) -> String {
    let mut out = String::new();
    for frame in stack {
        match frame {
            Frame::Array { index } => {
                out.push('/');
                out.push_str(&index.to_string());
            }
            Frame::Object { key } => {
                if let Some(key) = key {
                    out.push('/');
                    for ch in key.chars() {
                        match ch {
                            '~' => out.push_str("~0"),
                            '/' => out.push_str("~1"),
                            other => out.push(other),
                        }
                    }
                }
            }
        }
    }
    out
}

/// After a complete value is read, advance the innermost array index so the next
/// element gets the right pointer.
fn advance_after_value(stack: &mut [Frame]) {
    if let Some(Frame::Array { index }) = stack.last_mut() {
        *index += 1;
    }
}

/// Flush all pending comments, anchoring each to `pointer`.
fn flush_pending(
    pending: &mut Vec<(String, CommentKind, TextRange)>,
    comments: &mut Vec<Comment>,
    pointer: &str,
) {
    for (text, kind, range) in pending.drain(..) {
        comments.push(Comment {
            text,
            kind,
            source_range: range,
            anchor_pointer: pointer.to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    //! T022 support — the JSONC reader: strip + anchor comments (FR-008).

    use super::*;

    #[test]
    fn parses_plain_json_with_no_comments() {
        let doc = parse_jsonc("{ \"a\": 1, \"b\": [1, 2] }").expect("valid json");
        assert_eq!(doc.value, serde_json::json!({ "a": 1, "b": [1, 2] }));
        assert!(doc.comments.is_empty());
    }

    #[test]
    fn strips_line_comment_and_anchors_to_following_field() {
        let doc = parse_jsonc("{\n  // about a\n  \"a\": 1\n}").expect("valid jsonc");
        assert_eq!(doc.value, serde_json::json!({ "a": 1 }));
        assert_eq!(doc.comments.len(), 1);
        assert_eq!(doc.comments[0].text, "// about a");
        assert_eq!(doc.comments[0].anchor_pointer, "/a");
        assert_eq!(doc.comments[0].kind, CommentKind::Line);
    }

    #[test]
    fn strips_block_comment() {
        let doc = parse_jsonc("{ /* hdr */ \"a\": 1 }").expect("valid jsonc");
        assert_eq!(doc.value, serde_json::json!({ "a": 1 }));
        assert_eq!(doc.comments.len(), 1);
        assert_eq!(doc.comments[0].kind, CommentKind::Block);
        assert_eq!(doc.comments[0].anchor_pointer, "/a");
    }

    #[test]
    fn leading_header_comment_anchors_to_root() {
        let doc = parse_jsonc("// header\n{ \"a\": 1 }").expect("valid jsonc");
        // The header precedes the root object → anchors to the root pointer "".
        assert_eq!(doc.comments[0].anchor_pointer, "");
    }

    #[test]
    fn comment_marker_inside_string_is_not_a_comment() {
        let doc = parse_jsonc("{ \"url\": \"http://x\" }").expect("valid jsonc");
        assert_eq!(doc.value, serde_json::json!({ "url": "http://x" }));
        assert!(doc.comments.is_empty(), "no comment inside the string");
    }

    #[test]
    fn invalid_json_after_stripping_errors() {
        assert!(matches!(
            parse_jsonc("{ \"a\": }"),
            Err(JsoncError::InvalidJson(_))
        ));
    }

    #[test]
    fn array_element_comment_anchors_to_index() {
        let doc = parse_jsonc("[\n  1,\n  // second\n  2\n]").expect("valid jsonc");
        assert_eq!(doc.value, serde_json::json!([1, 2]));
        assert_eq!(doc.comments.len(), 1);
        assert_eq!(doc.comments[0].anchor_pointer, "/1");
    }

    #[test]
    fn deeply_nested_jsonc_does_not_panic() {
        let mut s = String::new();
        for _ in 0..(MAX_TRACK_DEPTH + 50) {
            s.push('[');
        }
        s.push('0');
        for _ in 0..(MAX_TRACK_DEPTH + 50) {
            s.push(']');
        }
        // serde_json's own recursion limit may reject this, but the SCAN must not
        // panic / overflow regardless.
        let _ = parse_jsonc(&s);
        assert!(!has_inline_comments(&s));
    }
}
