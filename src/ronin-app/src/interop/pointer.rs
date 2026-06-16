//! Shared JSON-Pointer walk over the RON CST value tree (HINT-002).
//!
//! `ron-validate`'s [`CstJsonProjection`] builds a `serde_json` value and a
//! [`PointerRangeIndex`](ron_validate::PointerRangeIndex) keyed by JSON Pointer,
//! but it does **not** expose a public value-node → pointer map. The loss-map
//! builder ([`crate::interop::loss`]) and the comment carrier
//! ([`crate::interop::comments`]) both need to address a CST value node by the
//! **same** JSON Pointer the projection index uses — so they can look up the
//! projection's source [`TextRange`] for a value (HINT-002) and anchor comments in
//! the projection's coordinate space.
//!
//! This module re-walks the value tree producing exactly the same pointers the
//! projection's [`project_value`](ron_validate::projection) walk records, so the
//! pointers align byte-for-byte with the projection index keys. It is the single
//! place that mirrors the projection's pointer rules — keeping the loss map and
//! the comment carrier from drifting from the value mapping they describe.
//!
//! # Pointer rules (must match `ron-validate`'s schema-agnostic projection)
//!
//! * **Struct** → `push_key(field_name)` per field value.
//! * **Tuple**: anonymous `(a, b)` → `push_index(i)`; a named tuple is a
//!   variant — `Some(x)` unwraps to the inner value at the **same** pointer; any
//!   other `Name(..)` external-tags as `push_key(Name)` then the payload (newtype
//!   stays at that key; a 2+-arity tuple pushes `push_index(i)`).
//! * **List** → `push_index(i)`.
//! * **Map** → `push_key(canonical_or_verbatim_key)` per entry (the projection's
//!   verbatim key text — the loss map records the *canonical* literal separately).
//! * **EnumVariant**: `None` → leaf at self; `Some(x)` → unwrap to inner at the
//!   same pointer; any other variant → `push_key(name)` then the payload.
//! * **Unit / Literal / Error** → leaf (no children).

use std::collections::BTreeMap;

use ron_core::syntax::ast::{Document, EnumVariant, List, Map, MapEntry, Struct, Tuple, Value};
use ron_core::{SyntaxKind, SyntaxNode, TextRange};

/// A value node and the JSON Pointer it maps to in the projection's coordinate
/// space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValuePointer {
    /// The value node's source byte range (the real CST span; matches the
    /// projection index's value span for this pointer).
    pub range: TextRange,
    /// The JSON Pointer (RFC 6901) addressing this value in the projected JSON.
    pub pointer: String,
}

/// Build the `(TextRange, pointer)` list for every value node under `root`, in the
/// projection's pointer coordinate space.
///
/// The returned list is in CST walk order (a pre-order traversal of the value
/// tree). Used by the comment carrier to anchor a comment to the nearest value
/// and by the loss-map builder to address the projection index. Returns an empty
/// list when the document has no root value (defensive: never panics).
#[must_use]
pub fn value_pointer_map(root: &SyntaxNode) -> Vec<(TextRange, String)> {
    let mut out = Vec::new();
    let Some(value) = Document::cast(root.clone()).and_then(|d| d.value()) else {
        return out;
    };
    let mut builder = PointerStack::new();
    walk(&value, &mut builder, &mut out);
    out
}

/// Build a fast pointer→range lookup from the same walk (the inverse of the
/// projection index for value spans only).
///
/// Keyed by JSON Pointer so a loss-map entry can fetch the source value span for
/// a pointer without a linear scan. When two value nodes share a pointer (the
/// `Some`/`None` unwrap reuses the parent pointer), the **innermost** (tightest)
/// span wins — matching the projection's "a nested value overwrites at the same
/// pointer" rule.
#[must_use]
pub fn pointer_to_range(root: &SyntaxNode) -> BTreeMap<String, TextRange> {
    let mut map = BTreeMap::new();
    for (range, pointer) in value_pointer_map(root) {
        // Last write wins = the innermost value at a shared pointer (the walk
        // visits an outer value before unwrapping to its inner).
        map.insert(pointer, range);
    }
    map
}

/// An incrementally-built JSON Pointer (RFC 6901), mirroring `ron-validate`'s
/// internal `PointerBuilder` so the segment escaping matches exactly.
struct PointerStack {
    buf: String,
    segment_starts: Vec<usize>,
}

impl PointerStack {
    fn new() -> Self {
        Self {
            buf: String::new(),
            segment_starts: Vec::new(),
        }
    }

    /// Push an object-property segment (escaped per RFC 6901: `~`→`~0`, `/`→`~1`).
    fn push_key(&mut self, key: &str) {
        self.segment_starts.push(self.buf.len());
        self.buf.push('/');
        for ch in key.chars() {
            match ch {
                '~' => self.buf.push_str("~0"),
                '/' => self.buf.push_str("~1"),
                other => self.buf.push(other),
            }
        }
    }

    /// Push an array-index segment.
    fn push_index(&mut self, index: usize) {
        self.segment_starts.push(self.buf.len());
        self.buf.push('/');
        self.buf.push_str(&index.to_string());
    }

    /// Pop the most recently pushed segment.
    fn pop(&mut self) {
        if let Some(start) = self.segment_starts.pop() {
            self.buf.truncate(start);
        }
    }

    /// The current pointer string (`""` at the root).
    fn as_pointer(&self) -> &str {
        &self.buf
    }
}

/// Recursively record `(range, pointer)` for `value` and its descendants.
fn walk(value: &Value, builder: &mut PointerStack, out: &mut Vec<(TextRange, String)>) {
    out.push((value.syntax().text_range(), builder.as_pointer().to_owned()));
    match value {
        Value::Struct(s) => walk_struct(s, builder, out),
        Value::Tuple(t) => walk_tuple(t, builder, out),
        Value::List(l) => walk_list(l, builder, out),
        Value::Map(m) => walk_map(m, builder, out),
        Value::EnumVariant(v) => walk_enum_variant(v, builder, out),
        // Leaves — no child value nodes.
        Value::Unit(_) | Value::Literal(_) | Value::Error(_) => {}
    }
}

fn walk_struct(s: &Struct, builder: &mut PointerStack, out: &mut Vec<(TextRange, String)>) {
    for field in s.fields() {
        let Some(name_tok) = field.name() else {
            continue;
        };
        builder.push_key(name_tok.text());
        if let Some(v) = field.value() {
            walk(&v, builder, out);
        }
        builder.pop();
    }
}

fn walk_tuple(t: &Tuple, builder: &mut PointerStack, out: &mut Vec<(TextRange, String)>) {
    let name = tuple_name(t);

    // `Some(x)` unwraps to the inner value at the same pointer.
    if name.as_deref() == Some("Some") {
        if let Some(inner) = t.items().next() {
            walk(&inner, builder, out);
        }
        return;
    }

    let items: Vec<Value> = t.items().collect();
    if let Some(variant) = name {
        // Externally-tagged named tuple/newtype variant.
        builder.push_key(&variant);
        match items.len() {
            0 => {}
            1 => walk(&items[0], builder, out),
            _ => {
                for (i, item) in items.iter().enumerate() {
                    builder.push_index(i);
                    walk(item, builder, out);
                    builder.pop();
                }
            }
        }
        builder.pop();
        return;
    }

    // Anonymous tuple → array indices.
    for (i, item) in items.iter().enumerate() {
        builder.push_index(i);
        walk(item, builder, out);
        builder.pop();
    }
}

fn walk_list(l: &List, builder: &mut PointerStack, out: &mut Vec<(TextRange, String)>) {
    for (i, item) in l.items().enumerate() {
        builder.push_index(i);
        walk(&item, builder, out);
        builder.pop();
    }
}

fn walk_map(m: &Map, builder: &mut PointerStack, out: &mut Vec<(TextRange, String)>) {
    for entry in m.entries() {
        let Some(key_value) = entry.key() else {
            continue;
        };
        // Use the projection's verbatim key string so the pointer matches the
        // projection index (the loss map records the *canonical* literal apart).
        let key = projection_key_string(&key_value);
        builder.push_key(&key);
        if let Some(v) = entry.value() {
            walk(&v, builder, out);
        }
        builder.pop();
    }
}

fn walk_enum_variant(
    v: &EnumVariant,
    builder: &mut PointerStack,
    out: &mut Vec<(TextRange, String)>,
) {
    let name = v.name_text().unwrap_or_default();
    let node = v.syntax();

    // `None` (no payload) → leaf; the self-entry was already recorded.
    if name == "None" && payload_values(node).next().is_none() && v.entries().next().is_none() {
        return;
    }
    // `Some(x)` → unwrap to the inner value at the same pointer.
    if name == "Some" {
        if let Some(inner) = payload_values(node).next() {
            walk(&inner, builder, out);
            return;
        }
    }

    // External tagging: push the variant name, then the payload.
    builder.push_key(&name);
    let struct_entries: Vec<MapEntry> = v.entries().collect();
    if !struct_entries.is_empty() || has_brace(node) {
        // Struct-like variant `V { field: v }`.
        for entry in &struct_entries {
            let Some((key, _span)) = entry_key_name(entry) else {
                continue;
            };
            builder.push_key(&key);
            if let Some(val) = entry.value() {
                walk(&val, builder, out);
            }
            builder.pop();
        }
    } else {
        // Positional payload `V(a, b, ..)`.
        let payload: Vec<Value> = payload_values(node).collect();
        match payload.len() {
            0 => {}
            1 => walk(&payload[0], builder, out),
            _ => {
                for (i, item) in payload.iter().enumerate() {
                    builder.push_index(i);
                    walk(item, builder, out);
                    builder.pop();
                }
            }
        }
    }
    builder.pop();
}

/// The leading `Ident` name of a named tuple (`Name(..)`), or `None` for an
/// anonymous tuple `(..)`.
pub(crate) fn tuple_name(t: &Tuple) -> Option<String> {
    t.syntax()
        .first_token_of(SyntaxKind::Ident)
        .map(|tok| tok.text().to_string())
}

/// The positional payload values inside a variant `Variant(a, b, ..)`.
fn payload_values(node: &SyntaxNode) -> impl Iterator<Item = Value> {
    node.children().filter_map(Value::cast)
}

/// Whether a variant node uses brace-style payload `{ .. }` (struct-like).
fn has_brace(node: &SyntaxNode) -> bool {
    node.children_with_tokens()
        .any(|el| el.kind() == SyntaxKind::LBrace)
}

/// The field-name string + span of a struct-like variant entry.
fn entry_key_name(entry: &MapEntry) -> Option<(String, TextRange)> {
    let key = entry.key()?;
    let name = match &key {
        Value::EnumVariant(ev) => ev.name_text()?,
        Value::Literal(lit) => lit.text()?,
        other => other.syntax().text(),
    };
    Some((name, key.syntax().text_range()))
}

/// The object-key string the projection records for a map key value (verbatim for
/// non-string keys) — mirrors `ron-validate`'s `map_key_string` so the pointer
/// segment matches the projection index key (HINT-002).
pub(crate) fn projection_key_string(key: &Value) -> String {
    if let Value::Literal(lit) = key {
        match lit.token_kind() {
            Some(SyntaxKind::String) => return decode_string(&lit.text().unwrap_or_default()),
            Some(SyntaxKind::RawString) => {
                return decode_raw_string(&lit.text().unwrap_or_default())
            }
            Some(SyntaxKind::Char) => return decode_char(&lit.text().unwrap_or_default()),
            _ => {}
        }
    }
    key.syntax().text()
}

/// Decode a `"..."` string literal's contents (best-effort; mirrors the
/// projection's decoder so map-key pointers match).
fn decode_string(text: &str) -> String {
    let inner = text
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(text);
    unescape(inner)
}

/// Decode a raw string literal `r#"..."#` — verbatim contents.
fn decode_raw_string(text: &str) -> String {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'r') {
        return text.to_owned();
    }
    let mut i = 1;
    let mut hashes = 0usize;
    while bytes.get(i) == Some(&b'#') {
        hashes += 1;
        i += 1;
    }
    if bytes.get(i) != Some(&b'"') {
        return text.to_owned();
    }
    let content_start = i + 1;
    let closing_len = 1 + hashes;
    if text.len() < content_start + closing_len {
        return text.to_owned();
    }
    let content_end = text.len() - closing_len;
    text.get(content_start..content_end)
        .unwrap_or("")
        .to_owned()
}

/// Decode a char literal `'c'` into its single-character string.
fn decode_char(text: &str) -> String {
    let inner = text
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .unwrap_or(text);
    unescape(inner)
}

/// Minimal escape decoder (mirrors the projection's `unescape`).
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some('0') => out.push('\0'),
            Some('u') => {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    let mut hex = String::new();
                    for h in chars.by_ref() {
                        if h == '}' {
                            break;
                        }
                        hex.push(h);
                    }
                    if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        out.push(ch);
                    }
                } else {
                    out.push('\\');
                    out.push('u');
                }
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pointers(src: &str) -> Vec<String> {
        let doc = ron_core::parse(src);
        value_pointer_map(&doc.root())
            .into_iter()
            .map(|(_, p)| p)
            .collect()
    }

    #[test]
    fn struct_field_pointers_match_projection() {
        let ps = pointers("(x: 1, y: 2)");
        assert!(ps.contains(&"".to_string()), "root pointer present");
        assert!(ps.contains(&"/x".to_string()));
        assert!(ps.contains(&"/y".to_string()));
    }

    #[test]
    fn list_uses_array_indices() {
        let ps = pointers("[10, 20, 30]");
        assert!(ps.contains(&"/0".to_string()));
        assert!(ps.contains(&"/1".to_string()));
        assert!(ps.contains(&"/2".to_string()));
    }

    #[test]
    fn anonymous_tuple_uses_array_indices() {
        let ps = pointers("(1, 2)");
        assert!(ps.contains(&"/0".to_string()));
        assert!(ps.contains(&"/1".to_string()));
    }

    #[test]
    fn some_unwraps_to_same_pointer() {
        // `Some(x)` does not add a `/Some` segment — it unwraps in place.
        let ps = pointers("(opt: Some(5))");
        assert!(ps.contains(&"/opt".to_string()));
        assert!(!ps.iter().any(|p| p.contains("Some")));
    }

    #[test]
    fn named_enum_variant_external_tags() {
        let ps = pointers("(state: Active)");
        assert!(ps.contains(&"/state".to_string()));
    }

    #[test]
    fn pointer_to_range_keeps_innermost_for_shared_pointer() {
        let doc = ron_core::parse("(opt: Some(5))");
        let map = pointer_to_range(&doc.root());
        // `/opt` resolves to the inner literal `5`'s span (the innermost value).
        let opt = map.get("/opt").copied().expect("/opt mapped");
        let text = &doc.root().text()[opt.start()..opt.end()];
        assert_eq!(text, "5", "the unwrapped Some inner value wins at /opt");
    }
}
