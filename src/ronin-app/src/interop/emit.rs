//! Serialize a RON→JSON [`RonToJson`](crate::interop::RonToJson) result to output
//! text — strict standard JSON, or JSONC with the carrier's comments woven in at
//! their anchored values (FR-001/008).
//!
//! This is the single, deterministic renderer the in-place convert (T014), the
//! file export (T015), and the loss-report dialog preview (T016) all share, so a
//! conversion's *bytes* are computed in exactly one place. Two output forms
//! (data-model §ConversionResult `produced`):
//!
//! * **Strict JSON** ([`render_json`] with [`JsoncStyle::Strict`]) — standard
//!   `serde_json` pretty output at the configured indent. Comments are NOT inline;
//!   they survive (when the carrier is [`CommentMode::Sidecar`]) via the sibling
//!   sidecar map written separately (T015), or are dropped + reported (T009) when
//!   the carrier is [`CommentMode::None`].
//! * **JSONC** ([`JsoncStyle::Jsonc`]) — the same pretty output with each anchored
//!   comment emitted on its own line immediately **before** the value it anchors to
//!   (the projection-coordinate JSON Pointer, FR-008). A comment anchored to the
//!   root pointer `""` is emitted as a leading header line.
//!
//! The output is deterministic (stable key order, stable comment placement) so the
//! JSONC snapshots (T007) are reproducible. The renderer walks the
//! [`serde_json::Value`] tracking the current JSON Pointer in lockstep with the
//! [`CommentCarrier`]'s anchors — it never re-parses or re-walks the source CST.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::interop::comments::{Comment, CommentCarrier, CommentMode};

/// Whether [`render_json`] emits strict standard JSON or JSONC (comments inline).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsoncStyle {
    /// Strict standard JSON — no inline comments (FR-008).
    Strict,
    /// JSONC — anchored comments emitted inline before their value (FR-008).
    Jsonc,
}

impl JsoncStyle {
    /// Resolve the emit style from a [`CommentMode`]: only
    /// [`CommentMode::JsoncInline`] emits inline comments; sidecar / none are
    /// strict standard JSON (comments travel via the sidecar, or are dropped).
    #[inline]
    #[must_use]
    pub fn from_comment_mode(mode: CommentMode) -> Self {
        match mode {
            CommentMode::JsoncInline => JsoncStyle::Jsonc,
            CommentMode::Sidecar | CommentMode::None => JsoncStyle::Strict,
        }
    }
}

/// Render a RON→JSON value to output text at `indent` spaces, weaving in the
/// carrier's comments when `style` is [`JsoncStyle::Jsonc`] (FR-001/008).
///
/// * `value` — the projected JSON value (with the FR-015 emit conventions applied).
/// * `comments` — the comment carrier; its [`inline_comments`](CommentCarrier::inline_comments)
///   are emitted inline in JSONC mode (the carrier's mode is consulted, so a
///   non-JSONC carrier emits no inline comments even when `style` is `Jsonc`).
/// * `indent` — the pretty-print indent width in spaces (`0` = a compact-ish single
///   space per level; the caller resolves this from
///   [`ConversionSettings`](crate::settings::ConversionSettings)).
/// * `style` — strict vs JSONC.
///
/// The output is deterministic for a given `(value, comments, indent, style)`.
#[must_use]
pub fn render_json(
    value: &serde_json::Value,
    comments: &CommentCarrier,
    indent: usize,
    style: JsoncStyle,
) -> String {
    // In JSONC mode index the carrier's inline comments by anchor pointer so the
    // walk can emit them in source order at the right value (FR-008). A non-JSONC
    // carrier / strict style yields an empty index → identical to strict output.
    let anchored: BTreeMap<String, Vec<&Comment>> = if style == JsoncStyle::Jsonc {
        let mut map: BTreeMap<String, Vec<&Comment>> = BTreeMap::new();
        for comment in comments.inline_comments() {
            map.entry(comment.anchor_pointer.clone())
                .or_default()
                .push(comment);
        }
        map
    } else {
        BTreeMap::new()
    };

    let mut out = String::new();
    // A comment anchored to the root pointer "" is a leading header (FR-008).
    if let Some(header) = anchored.get("") {
        for comment in header {
            push_comment_line(&mut out, comment, 0, indent);
        }
    }
    let mut writer = JsonWriter {
        out: &mut out,
        anchored: &anchored,
        indent,
        pointer: String::new(),
        segment_starts: Vec::new(),
    };
    writer.write_value(value, 0);
    out.push('\n');
    out
}

/// The recursive JSON writer that tracks the current JSON Pointer so it can emit
/// each anchored comment immediately before the value it anchors to (FR-008).
struct JsonWriter<'a> {
    out: &'a mut String,
    anchored: &'a BTreeMap<String, Vec<&'a Comment>>,
    indent: usize,
    pointer: String,
    segment_starts: Vec<usize>,
}

impl JsonWriter<'_> {
    /// Push the indent for `depth` levels.
    fn push_indent(&mut self, depth: usize) {
        for _ in 0..depth * self.indent {
            self.out.push(' ');
        }
    }

    fn push_key_segment(&mut self, key: &str) {
        self.segment_starts.push(self.pointer.len());
        self.pointer.push('/');
        for ch in key.chars() {
            match ch {
                '~' => self.pointer.push_str("~0"),
                '/' => self.pointer.push_str("~1"),
                other => self.pointer.push(other),
            }
        }
    }

    fn push_index_segment(&mut self, index: usize) {
        self.segment_starts.push(self.pointer.len());
        self.pointer.push('/');
        let _ = write!(self.pointer, "{index}");
    }

    fn pop_segment(&mut self) {
        if let Some(start) = self.segment_starts.pop() {
            self.pointer.truncate(start);
        }
    }

    /// Emit any comments anchored to the current pointer, each on its own indented
    /// line, before the value is written (FR-008). Root-anchored comments are
    /// handled separately by [`render_json`] (the leading header), so they are not
    /// re-emitted here.
    fn emit_anchored_comments(&mut self, depth: usize) {
        if self.pointer.is_empty() {
            return;
        }
        if let Some(comments) = self.anchored.get(&self.pointer) {
            for comment in comments {
                push_comment_line(self.out, comment, depth, self.indent);
            }
        }
    }

    /// Write `value` at `depth`, recursing into objects/arrays and tracking the
    /// pointer so anchored comments land at the right child (FR-008).
    fn write_value(&mut self, value: &serde_json::Value, depth: usize) {
        match value {
            serde_json::Value::Object(map) => self.write_object(map, depth),
            serde_json::Value::Array(arr) => self.write_array(arr, depth),
            scalar => {
                // A scalar is serialized via serde_json so escaping is exact.
                let _ = write!(self.out, "{scalar}");
            }
        }
    }

    fn write_object(&mut self, map: &serde_json::Map<String, serde_json::Value>, depth: usize) {
        if map.is_empty() {
            self.out.push_str("{}");
            return;
        }
        self.out.push('{');
        self.out.push('\n');
        let last = map.len() - 1;
        for (i, (key, val)) in map.iter().enumerate() {
            self.push_key_segment(key);
            // Any comments anchored to this child are emitted before its line.
            self.emit_anchored_comments(depth + 1);
            self.push_indent(depth + 1);
            // The key is a JSON string; serialize it for exact escaping.
            let key_json = serde_json::Value::String(key.clone());
            let _ = write!(self.out, "{key_json}: ");
            self.write_value(val, depth + 1);
            if i != last {
                self.out.push(',');
            }
            self.out.push('\n');
            self.pop_segment();
        }
        self.push_indent(depth);
        self.out.push('}');
    }

    fn write_array(&mut self, arr: &[serde_json::Value], depth: usize) {
        if arr.is_empty() {
            self.out.push_str("[]");
            return;
        }
        self.out.push('[');
        self.out.push('\n');
        let last = arr.len() - 1;
        for (i, val) in arr.iter().enumerate() {
            self.push_index_segment(i);
            self.emit_anchored_comments(depth + 1);
            self.push_indent(depth + 1);
            self.write_value(val, depth + 1);
            if i != last {
                self.out.push(',');
            }
            self.out.push('\n');
            self.pop_segment();
        }
        self.push_indent(depth);
        self.out.push(']');
    }
}

/// Push one comment as an indented line (its verbatim text + a trailing newline).
///
/// A block comment (`/* … */`) is emitted verbatim; a line comment (`// …`) is
/// emitted verbatim too — both are valid JSONC. The text is the comment's exact
/// source bytes (never normalized, FR-008).
fn push_comment_line(out: &mut String, comment: &Comment, depth: usize, indent: usize) {
    for _ in 0..depth * indent {
        out.push(' ');
    }
    out.push_str(&comment.text);
    out.push('\n');
}

#[cfg(test)]
mod tests {
    //! T014/T015 support — the deterministic JSON / JSONC renderer (FR-001/008).

    use super::*;
    use crate::interop::{ron_to_json, RonToJson};
    use ron_core::parse;

    fn convert(src: &str, mode: CommentMode) -> RonToJson {
        let doc = parse(src);
        ron_to_json(&doc, None, mode)
    }

    #[test]
    fn strict_json_has_no_comments_and_stable_indent() {
        let r = convert("// header\n(a: 1, b: [1, 2])", CommentMode::None);
        let text = render_json(&r.value, &r.comments, 2, JsoncStyle::Strict);
        assert!(
            !text.contains("//"),
            "strict JSON carries no inline comments"
        );
        assert!(text.contains("\"a\": 1"));
        assert!(text.contains("\"b\": [\n"), "arrays pretty-print");
        // Deterministic: same inputs → same bytes.
        let again = render_json(&r.value, &r.comments, 2, JsoncStyle::Strict);
        assert_eq!(text, again);
    }

    #[test]
    fn jsonc_emits_a_root_header_comment() {
        let r = convert("// header\n(a: 1)", CommentMode::JsoncInline);
        let text = render_json(&r.value, &r.comments, 2, JsoncStyle::Jsonc);
        // The leading header comment precedes the opening brace.
        assert!(
            text.starts_with("// header\n{"),
            "root-anchored comment is a leading header line, got:\n{text}"
        );
    }

    #[test]
    fn jsonc_emits_an_anchored_field_comment_before_its_value() {
        let r = convert("(\n  // about a\n  a: 1,\n)", CommentMode::JsoncInline);
        let text = render_json(&r.value, &r.comments, 2, JsoncStyle::Jsonc);
        // The comment anchored to `/a` is emitted on its own line before `"a": 1`.
        let about = text.find("// about a").expect("anchored comment present");
        let field = text.find("\"a\": 1").expect("the field is present");
        assert!(about < field, "the comment precedes its value");
    }

    #[test]
    fn empty_collections_render_compactly() {
        let r = convert("(l: [], m: {})", CommentMode::None);
        let text = render_json(&r.value, &r.comments, 2, JsoncStyle::Strict);
        assert!(text.contains("\"l\": []"));
        assert!(text.contains("\"m\": {}"));
    }

    #[test]
    fn style_from_comment_mode() {
        assert_eq!(
            JsoncStyle::from_comment_mode(CommentMode::JsoncInline),
            JsoncStyle::Jsonc
        );
        assert_eq!(
            JsoncStyle::from_comment_mode(CommentMode::Sidecar),
            JsoncStyle::Strict
        );
        assert_eq!(
            JsoncStyle::from_comment_mode(CommentMode::None),
            JsoncStyle::Strict
        );
    }
}
