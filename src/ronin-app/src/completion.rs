//! The custom completion popup state machine + CST-verified insertion (E005
//! Wave 3, US2, AD-007/HINT-005).
//!
//! egui ships **no** built-in completion popup, so RONin builds its own over the
//! editor's [`egui::TextEdit`]. This module holds the *pure, UI-agnostic* half of
//! that popup — the part the headless tests drive directly (T031) — while
//! `editor_view::completion_popup` does the egui rendering and keystroke wiring on
//! top of it.
//!
//! The design splits cleanly so the decision logic never needs a live UI:
//!
//! * [`CompletionState`] holds the popup's cross-frame state: whether it is open,
//!   the candidate items (already ranked below the literal by `ronin-core`), the
//!   explicitly-highlighted index (`None` = nothing preselected), and the byte
//!   offset the popup was triggered at.
//! * [`recompute`] re-derives candidates from the buffer + caret each frame via
//!   `ronin_core::completion_context`. An empty/ambiguous context (no items) closes
//!   the popup (FR-014); a context with items (auto-trigger on a non-empty prefix,
//!   or any value slot on manual invoke) opens it.
//! * [`accept`] turns a highlighted item into a CST-verified buffer splice
//!   (FR-013): it replaces the in-progress prefix with the item's `insert_text`,
//!   then **re-parses the candidate buffer** and only commits if the splice does
//!   not introduce a *new* parse error versus the original buffer. The caret lands
//!   at the end of the inserted text (FR-022).
//!
//! # Never auto-accept (FR-012)
//!
//! Nothing is ever preselected: [`CompletionState::highlighted`] starts `None`, so
//! a bare `Enter`/`Tab` with the popup open inserts the user's *literal* (the host
//! lets the `TextEdit` handle it) — only an explicit arrow-key highlight makes
//! `Enter`/`Tab` accept a suggestion. Typing through the popup never auto-accepts.

use ronin_core::{completion_context, CompletionItem};

/// The cross-frame state of the completion popup for one document.
///
/// Default is the closed/empty state. `editor_view` recomputes it each frame from
/// the live buffer + caret, then renders it if [`is_open`](Self::is_open).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompletionState {
    /// Whether the popup is currently shown.
    open: bool,
    /// The ranked candidate items (kind-then-alpha, all below the literal).
    items: Vec<CompletionItem>,
    /// The explicitly-highlighted item index (`None` = nothing preselected).
    ///
    /// Only an arrow-key navigation sets this; it is the single gate that lets
    /// `Enter`/`Tab` accept a suggestion instead of the literal (FR-012).
    highlighted: Option<usize>,
    /// The in-progress identifier prefix the candidates were filtered by.
    prefix: String,
    /// The caret byte offset the popup was last recomputed at; a caret move away
    /// from this offset dismisses the popup (FR-022).
    caret_byte: usize,
}

/// Why the popup is being recomputed this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// Ordinary typing: only auto-open when the user is mid-identifier (a
    /// non-empty prefix) and there are candidates.
    Auto,
    /// An explicit manual-invoke keystroke (e.g. Ctrl+Space): open whenever there
    /// are candidates, even at a fresh (empty-prefix) value slot.
    Manual,
}

/// The outcome of an [`accept`] attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    /// The new full buffer text after the verified splice.
    pub new_buffer: String,
    /// The caret byte offset after insertion (end of the inserted text) (FR-022).
    pub new_caret_byte: usize,
}

impl CompletionState {
    /// A fresh, closed popup state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` when the popup is shown.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The current candidate items (empty when closed).
    #[must_use]
    pub fn items(&self) -> &[CompletionItem] {
        &self.items
    }

    /// The explicitly-highlighted index, or `None` if nothing is selected.
    #[must_use]
    pub fn highlighted(&self) -> Option<usize> {
        self.highlighted
    }

    /// The highlighted item, if one is explicitly selected.
    #[must_use]
    pub fn highlighted_item(&self) -> Option<&CompletionItem> {
        self.highlighted.and_then(|i| self.items.get(i))
    }

    /// The prefix the candidates were filtered by.
    #[must_use]
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Close the popup and clear its state (e.g. on `Esc` or a cursor move)
    /// (FR-022). The literal the user typed is left untouched.
    pub fn dismiss(&mut self) {
        self.open = false;
        self.items.clear();
        self.highlighted = None;
        self.prefix.clear();
    }

    /// Re-derive the popup from `buffer` at `caret_byte` for the given `trigger`
    /// (FR-014/FR-022).
    ///
    /// Asks `ronin_core::completion_context` for the structural candidates, then:
    /// * an empty/ambiguous context (no items) **closes** the popup (FR-014);
    /// * otherwise, under [`Trigger::Auto`] the popup only opens when the user is
    ///   mid-identifier (a non-empty prefix); under [`Trigger::Manual`] it opens
    ///   whenever there are candidates;
    /// * a caret move to a different offset than the previously-open popup clears
    ///   the highlight (a new position is a new, never-preselected list).
    ///
    /// Returns `true` if the popup is open after recompute.
    pub fn recompute(&mut self, buffer: &str, caret_byte: usize, trigger: Trigger) -> bool {
        let ctx = completion_context(&ronin_core::parse(buffer), caret_byte);

        // Empty / ambiguous context → no list, no popup (FR-014).
        if ctx.items.is_empty() {
            self.dismiss();
            return false;
        }

        // Auto-trigger only mid-identifier; manual invoke can open at a fresh slot.
        let should_open = match trigger {
            Trigger::Auto => !ctx.prefix.is_empty(),
            Trigger::Manual => true,
        };
        if !should_open {
            self.dismiss();
            return false;
        }

        // A caret move resets any prior highlight (nothing preselected, FR-012).
        let caret_moved = self.open && self.caret_byte != caret_byte;
        let prefix_changed = self.prefix != ctx.prefix;
        if caret_moved || prefix_changed || !self.open {
            self.highlighted = None;
        } else if let Some(h) = self.highlighted {
            // Keep the highlight in range if the (smaller) list shrank.
            if h >= ctx.items.len() {
                self.highlighted = None;
            }
        }

        self.open = true;
        self.items = ctx.items;
        self.prefix = ctx.prefix;
        self.caret_byte = caret_byte;
        true
    }

    /// Move the highlight to the next item, wrapping; opens a selection from the
    /// "nothing preselected" state to the first item (arrow-down).
    pub fn highlight_next(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.highlighted = Some(match self.highlighted {
            None => 0,
            Some(i) => (i + 1) % self.items.len(),
        });
    }

    /// Move the highlight to the previous item, wrapping; from "nothing
    /// preselected" this selects the last item (arrow-up).
    pub fn highlight_prev(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.highlighted = Some(match self.highlighted {
            None => self.items.len() - 1,
            Some(0) => self.items.len() - 1,
            Some(i) => i - 1,
        });
    }

    /// Accept the highlighted suggestion against `buffer` at `caret_byte`,
    /// returning the CST-verified splice (FR-013/FR-022), or `None` when nothing
    /// is highlighted or the splice would corrupt the document.
    ///
    /// The in-progress prefix `[caret_byte - prefix.len(), caret_byte)` is replaced
    /// by the item's `insert_text`. The candidate buffer is re-parsed and the
    /// splice is committed only if it introduces **no new** parse error versus the
    /// original (so an accepted suggestion round-trips through the CST). The caret
    /// lands at the end of the inserted text.
    ///
    /// On success the popup is left to be re-derived (the caller dismisses or
    /// recomputes); this method only computes the splice, it does not mutate the
    /// popup's own state.
    #[must_use]
    pub fn accept(&self, buffer: &str, caret_byte: usize) -> Option<Accepted> {
        let item = self.highlighted_item()?;
        accept_item(buffer, caret_byte, &self.prefix, item)
    }
}

/// Compute the CST-verified splice for accepting `item` at `caret_byte`, replacing
/// the in-progress `prefix` (FR-013/FR-022).
///
/// Pulled out as a free function so the headless tests can verify the
/// insertion-and-reparse contract without a [`CompletionState`].
#[must_use]
pub fn accept_item(
    buffer: &str,
    caret_byte: usize,
    prefix: &str,
    item: &CompletionItem,
) -> Option<Accepted> {
    // The prefix occupies `[start, caret_byte)`. Guard every index against the
    // live buffer (a stale caret must never panic or splice off a char boundary).
    if caret_byte > buffer.len() || !buffer.is_char_boundary(caret_byte) {
        return None;
    }
    let start = caret_byte.checked_sub(prefix.len())?;
    if !buffer.is_char_boundary(start) {
        return None;
    }
    // The slice we are replacing must actually be the prefix we filtered by; if it
    // is not (the buffer shifted under us), refuse rather than corrupt.
    if &buffer[start..caret_byte] != prefix {
        return None;
    }

    let mut new_buffer =
        String::with_capacity(buffer.len() - prefix.len() + item.insert_text.len());
    new_buffer.push_str(&buffer[..start]);
    new_buffer.push_str(&item.insert_text);
    new_buffer.push_str(&buffer[caret_byte..]);

    // Verify-before-commit (Principle I): the splice must not add a NEW parse
    // error. We allow it to *reduce* errors (completing an in-progress construct)
    // but never to introduce one the original buffer did not have.
    let before = parse_error_count(buffer);
    let after = parse_error_count(&new_buffer);
    if after > before {
        return None;
    }

    // The caret lands at the end of the inserted text (FR-022). Many `insert_text`
    // values carry a closing delimiter (`Some()`, `[]`); placing the caret at the
    // very end keeps the result valid and predictable for the structural layer.
    let new_caret_byte = start + item.insert_text.len();

    Some(Accepted {
        new_buffer,
        new_caret_byte,
    })
}

/// The number of parse diagnostics for `text` (the verify-before-commit metric).
fn parse_error_count(text: &str) -> usize {
    ronin_core::parse(text).diagnostics().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(label: &str, insert: &str, kind: ronin_core::CompletionKind) -> CompletionItem {
        CompletionItem {
            label: label.to_string(),
            insert_text: insert.to_string(),
            kind,
            rank: 1,
        }
    }

    #[test]
    fn auto_trigger_opens_only_with_a_prefix() {
        let mut state = CompletionState::new();
        // Empty buffer: no context → closed.
        assert!(!state.recompute("", 0, Trigger::Auto));
        assert!(!state.is_open());

        // Mid-identifier at top level: `So` has a non-empty prefix and candidates.
        assert!(state.recompute("So", 2, Trigger::Auto));
        assert!(state.is_open());
        assert!(state.items().iter().any(|i| i.label == "Some"));
    }

    #[test]
    fn auto_trigger_stays_closed_at_empty_prefix() {
        let mut state = CompletionState::new();
        // A fresh value slot inside `[]`: candidates exist but the prefix is empty,
        // so auto-trigger does NOT open the popup.
        assert!(!state.recompute("[]", 1, Trigger::Auto));
        assert!(!state.is_open());
    }

    #[test]
    fn manual_invoke_opens_at_empty_prefix() {
        let mut state = CompletionState::new();
        // Manual invoke opens even with an empty prefix when candidates exist.
        assert!(state.recompute("[]", 1, Trigger::Manual));
        assert!(state.is_open());
        assert!(!state.items().is_empty());
    }

    #[test]
    fn empty_or_ambiguous_context_shows_no_list() {
        let mut state = CompletionState::new();
        // Whitespace-only buffer → ambiguous/empty → no popup.
        assert!(!state.recompute("   ", 2, Trigger::Manual));
        assert!(!state.is_open());
        assert!(state.items().is_empty());
    }

    #[test]
    fn nothing_is_preselected_on_open() {
        let mut state = CompletionState::new();
        state.recompute("So", 2, Trigger::Auto);
        assert_eq!(
            state.highlighted(),
            None,
            "the popup must never preselect an item (FR-012)"
        );
        assert!(state.highlighted_item().is_none());
    }

    #[test]
    fn arrow_navigation_selects_and_wraps() {
        let mut state = CompletionState::new();
        state.recompute("So", 2, Trigger::Manual);
        let n = state.items().len();
        assert!(n >= 1);
        state.highlight_next();
        assert_eq!(state.highlighted(), Some(0));
        state.highlight_prev();
        // From index 0, prev wraps to the last.
        assert_eq!(state.highlighted(), Some(n - 1));
        // From "nothing", prev selects the last directly.
        state.dismiss();
        state.recompute("So", 2, Trigger::Manual);
        state.highlight_prev();
        assert_eq!(state.highlighted(), Some(n - 1));
    }

    #[test]
    fn accept_without_highlight_is_none() {
        let state = {
            let mut s = CompletionState::new();
            s.recompute("So", 2, Trigger::Manual);
            s
        };
        // Nothing highlighted → Enter/Tab inserts the literal, not a suggestion.
        assert!(state.accept("So", 2).is_none());
    }

    #[test]
    fn accept_highlighted_splices_and_places_caret_at_end() {
        let mut state = CompletionState::new();
        // Buffer `[So]` at the end of the in-progress `So` prefix (offset 3).
        let buffer = "[So]";
        state.recompute(buffer, 3, Trigger::Auto);
        // Highlight the `Some` suggestion explicitly.
        let some_idx = state
            .items()
            .iter()
            .position(|i| i.label == "Some")
            .expect("Some offered");
        state.highlighted = Some(some_idx);
        let accepted = state.accept(buffer, 3).expect("accept splices");
        // `So` (offset 1..3) is replaced by `Some()` → `[Some()]`.
        assert_eq!(accepted.new_buffer, "[Some()]");
        // Caret lands at the end of the inserted `Some()` (offset 1 + 6 = 7).
        assert_eq!(accepted.new_caret_byte, 7);
    }

    #[test]
    fn accepted_text_round_trips_through_the_cst() {
        let mut state = CompletionState::new();
        let buffer = "[So]";
        state.recompute(buffer, 3, Trigger::Auto);
        let some_idx = state
            .items()
            .iter()
            .position(|i| i.label == "Some")
            .unwrap();
        state.highlighted = Some(some_idx);
        let accepted = state.accept(buffer, 3).unwrap();
        // The spliced buffer parses without diagnostics (lossless, Principle I).
        let parsed = ronin_core::parse(&accepted.new_buffer);
        assert!(
            parsed.diagnostics().is_empty(),
            "accepted suggestion must round-trip cleanly: {:?}",
            parsed.diagnostics()
        );
    }

    #[test]
    fn accept_refuses_a_splice_that_introduces_a_new_error() {
        // A hand-crafted item whose insert_text would corrupt the buffer must be
        // refused by the verify-before-commit guard.
        let bad = item("Some", "Some(", ronin_core::CompletionKind::Option);
        // Replacing the empty prefix at the end of a clean buffer with `Some(`
        // introduces an unclosed delimiter → must be refused.
        let accepted = accept_item("[1]", 3, "", &bad);
        assert!(
            accepted.is_none(),
            "a splice that adds a new parse error must be refused"
        );
    }

    #[test]
    fn accept_with_stale_caret_does_not_panic() {
        // Caret past the buffer end: refused, never panics.
        assert!(accept_item(
            "So",
            999,
            "So",
            &item("Some", "Some()", ronin_core::CompletionKind::Option)
        )
        .is_none());
    }

    #[test]
    fn dismiss_clears_state_but_keeps_literal() {
        let mut state = CompletionState::new();
        state.recompute("So", 2, Trigger::Auto);
        assert!(state.is_open());
        state.dismiss();
        assert!(!state.is_open());
        assert!(state.items().is_empty());
        assert_eq!(state.highlighted(), None);
        // `dismiss` does not touch any buffer — the literal is the caller's.
    }
}
