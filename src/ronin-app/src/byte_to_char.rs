//! Shared byte-offset → char-offset resolution over a source string.
//!
//! `ron-core` syntax ranges are **byte** offsets; the editor surfaces (highlight
//! spans, diagnostic char ranges) work in **char** offsets. Converting naively
//! with `source[..off].chars().count()` is O(off) per query, so resolving every
//! token/node is O(n²) over a large file — the exact cost that froze the
//! structural views on `samples/ships.ron`.
//!
//! Two amortized-O(n) resolvers live here, sharing one forward scan strategy:
//!
//! * [`ByteToChar`] — a forward-only cursor for callers that query offsets in
//!   **non-decreasing** order (e.g. the highlight walk over disjoint, source-ordered
//!   leaf tokens). Amortized O(n) across the whole walk.
//! * [`ByteCharIndex`] — a precomputed index for callers that query offsets in an
//!   **arbitrary** order (e.g. the structural derivations, whose pre-order node
//!   walk yields non-monotone `(start, end)` pairs because a parent's end follows
//!   its children's starts). It runs a single [`ByteToChar`] pass over the sorted
//!   set of needed offsets, then answers each query by binary search — O(n + k log k)
//!   total instead of O(nodes × file_size).

/// A forward-only byte-offset → char-offset resolver over a source string.
///
/// Offsets are visited with non-decreasing values, so resolving is amortised O(n)
/// across the whole walk rather than O(n) per query.
pub(crate) struct ByteToChar<'a> {
    iter: std::iter::Peekable<std::str::CharIndices<'a>>,
    source_len: usize,
    /// The char offset of the next char the iterator will yield (i.e. the char count
    /// of everything before the iterator's current peek position).
    cur_char: usize,
}

impl<'a> ByteToChar<'a> {
    pub(crate) fn new(source: &'a str) -> Self {
        Self {
            iter: source.char_indices().peekable(),
            source_len: source.len(),
            cur_char: 0,
        }
    }

    /// The char offset at `byte_offset` (the number of chars strictly before it).
    /// Offsets must be non-decreasing across calls; an offset past end-of-source
    /// clamps to the final char count.
    ///
    /// Consumes chars whose **start** byte is `< target`, counting each, then stops
    /// with the iterator peeked at the first char starting at or after `target`. The
    /// char-count accumulated is exactly the number of chars before `byte_offset` —
    /// peeking (not consuming) the boundary char keeps consecutive queries correct.
    pub(crate) fn char_at(&mut self, byte_offset: usize) -> usize {
        let target = byte_offset.min(self.source_len);
        while let Some(&(idx, _)) = self.iter.peek() {
            if idx >= target {
                break;
            }
            self.iter.next();
            self.cur_char += 1;
        }
        self.cur_char
    }
}

/// A precomputed byte-offset → char-offset index for **arbitrary-order** queries.
///
/// Built from the full set of byte offsets a caller will query (e.g. every
/// structural node's `start`/`end`). The constructor sorts + dedups the offsets and
/// runs a single forward [`ByteToChar`] pass to record each offset's char position,
/// so per-query cost drops to a binary search. Total cost is O(n + k log k) where
/// `n = source.len()` and `k = offsets.len()`, replacing the prior O(nodes × file_size).
///
/// A query for an offset that was not registered at build time clamps to the
/// nearest registered offset at or below it (used only for offsets the index was
/// built from in practice, so this is a defensive fallback, never a hot path).
pub(crate) struct ByteCharIndex {
    /// `(byte_offset, char_offset)` pairs, sorted ascending by byte offset and
    /// deduplicated. Always carries an entry for byte offset `0`.
    entries: Vec<(usize, usize)>,
}

impl ByteCharIndex {
    /// Build an index resolving each byte offset in `byte_offsets` to its char
    /// offset over `source`, in a single forward pass.
    ///
    /// Offsets are sorted + deduped first so the single [`ByteToChar`] scan visits
    /// them in the non-decreasing order it requires. Offsets past end-of-source
    /// clamp to the final char count.
    pub(crate) fn build(source: &str, byte_offsets: impl IntoIterator<Item = usize>) -> Self {
        let mut wanted: Vec<usize> = byte_offsets.into_iter().collect();
        // Always anchor offset 0 so a below-range query has a base to clamp to.
        wanted.push(0);
        wanted.sort_unstable();
        wanted.dedup();

        let mut cursor = ByteToChar::new(source);
        let entries = wanted
            .into_iter()
            .map(|byte| (byte, cursor.char_at(byte)))
            .collect();

        Self { entries }
    }

    /// The char offset registered for `byte_offset`.
    ///
    /// Returns the char offset for the largest registered offset `<= byte_offset`
    /// (an exact match in normal use, since callers build the index from exactly the
    /// offsets they query). Empty/below-range queries clamp to char offset `0`.
    pub(crate) fn char_at(&self, byte_offset: usize) -> usize {
        match self
            .entries
            .binary_search_by_key(&byte_offset, |&(byte, _)| byte)
        {
            Ok(idx) => self.entries[idx].1,
            // Not registered exactly: clamp to the nearest registered offset below.
            Err(0) => 0,
            Err(idx) => self.entries[idx - 1].1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_resolver_matches_naive_counts() {
        let source = "abc\u{00e9}de\u{1f600}fg"; // mixes 1/2/4-byte chars
        let mut cursor = ByteToChar::new(source);
        let mut byte = 0;
        while byte <= source.len() {
            if source.is_char_boundary(byte) {
                let expected = source[..byte].chars().count();
                assert_eq!(cursor.char_at(byte), expected, "byte {byte}");
            }
            byte += 1;
        }
    }

    #[test]
    fn index_resolves_arbitrary_order() {
        let source = "abc\u{00e9}de\u{1f600}fg";
        // Every char-boundary offset, queried out of order against the index.
        let boundaries: Vec<usize> = (0..=source.len())
            .filter(|&b| source.is_char_boundary(b))
            .collect();
        let index = ByteCharIndex::build(source, boundaries.iter().copied());
        // Query in reverse + interleaved order; the index must not depend on order.
        for &byte in boundaries.iter().rev() {
            let expected = source[..byte].chars().count();
            assert_eq!(index.char_at(byte), expected, "byte {byte}");
        }
        for &byte in &boundaries {
            let expected = source[..byte].chars().count();
            assert_eq!(index.char_at(byte), expected, "byte {byte}");
        }
    }

    #[test]
    fn index_clamps_below_and_above_range() {
        let source = "abcdef";
        let index = ByteCharIndex::build(source, [3usize]);
        assert_eq!(index.char_at(3), 3);
        // Below the smallest registered offset (other than the 0 anchor) clamps to 0.
        assert_eq!(index.char_at(0), 0);
        // Above the largest registered offset clamps to that offset's char value.
        assert_eq!(index.char_at(100), 3);
    }
}
