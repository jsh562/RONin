//! The editing surface: syntax highlighting derived from the `ron-core` CST and
//! a multiline editor widget with a line-number gutter (FR-004/FR-005/FR-019).
//!
//! Two responsibilities live here:
//!
//! * **Highlight model** ([`build_highlight_model`]) — walk the lossless
//!   `ron-core` CST token stream **once** and project each token's byte range
//!   onto a [`HighlightSpan`] tagged with a [`HighlightClass`]. RONin keeps a
//!   single tokenizer (the engine's); the editor never re-lexes (project-
//!   instructions §II). The model is keyed by reparse `generation` so it is
//!   recomputed only when stale.
//! * **Editor widget** ([`editor_view`]) — a monospace multiline
//!   [`egui::TextEdit`] over the document buffer, with a left line-number gutter
//!   aligned to the text rows, and (unless the file is oversize) a memoized
//!   [`highlight_layouter`] that colours the text per the highlight spans. When
//!   oversize, highlighting/squiggles are suppressed but editing stays fully
//!   functional and a non-blocking degrade label is shown (FR-017).
//!
//! # Large-file degrade reuse (E005 Wave 5, FR-026)
//!
//! The E005 per-frame intelligence layered here degrades on the **same** signal as
//! E003's highlighting/squiggles: the document being `oversize`. The structural
//! completion popup is gated by [`completion_enabled`], which suppresses it past
//! `AppSettings::large_file_threshold` exactly as highlighting is suppressed, and
//! the **existing** E003 degrade indicator ("Large file — highlighting disabled")
//! communicates the state — E005 adds **no** separate message. The explicit Format
//! Document / Format Selection commands are *not* per-frame intelligence; they stay
//! available on an oversize document (a one-shot, verify-before-replace action that
//! never blocks interactive editing), consistent with E003 only degrading the
//! always-on layer, not on-demand commands.
//!
//! # No silent reformat (E005, FR-009)
//!
//! This widget edits the buffer in place; ordinary typing/edits **never** reformat.
//! A buffer mutation only bumps the document's edit generation via
//! [`EditorDocument::on_edit`] (so a coalesced *reparse* — for highlighting and
//! diagnostics — is requested next frame). It does **not** call `ron_core::format`.
//! Reformatting happens exclusively through the explicit Format Document / Format
//! Selection commands or the opt-in format-on-save path, both of which live on the
//! [`crate::app::App`] shell and go through its single safe apply path. The reparse
//! triggered after a command-driven buffer replacement is the same generation-keyed
//! refresh used for typing — it recomputes derived state, never the buffer text.
//!
//! # Deferred seams
//!
//! This widget now carries the full **structural** E005 authoring surface:
//! highlighting, squiggles, the completion popup ([`completion_popup`]), and
//! snippet tab-stop navigation ([`snippet_navigation`]). The deeper intelligence
//! that layers on top of this editing surface is deferred to later epics and
//! attaches here without changing the shell:
//!
//! * **type-aware completion / snippets** (schema-aware ranking + offering only
//!   type-legal candidates) and **type validation** (type-checked diagnostics over
//!   the structural ones) → **E006**;
//! * **semantic / CST-backed undo-redo** of an authoring action → **E007** (the
//!   verified buffer splices here are the seam an undo stack records against);
//! * **tree / table structured editing** as an alternate surface → **E008**;
//! * **Bevy-registry-aware** authoring (registry-resolved component/field names and
//!   snippets) → **E009**;
//! * **RON⇄JSON interop / `derive`-driven** authoring → **E010** (interop lives
//!   outside the editing surface and the `ron-core` engine).

use std::sync::Arc;

use egui::text::{CCursor, CCursorRange, LayoutJob, TextFormat};
use egui::text_edit::{TextEditOutput, TextEditState};
use egui::{Align, Color32, FontId, Galley, Key, Modifiers, Pos2, Rect, Stroke, TextBuffer, Ui};

use ron_core::{CompletionKind, Severity, SyntaxKind};

use crate::completion::Trigger;
use crate::diagnostics_map::DiagnosticView;
use crate::document::{EditorDocument, HighlightModel, HighlightSpan};
use crate::reparse::ParseResult;
use crate::snippets::TabStopKind;

/// The classification a highlight span carries (FR-019).
///
/// Derived 1:1 from the `ron-core` [`SyntaxKind`] of each significant token; a
/// closed, UI-facing palette so the editor never needs to know engine token
/// kinds. Trivia and structure produce [`HighlightClass::Default`] (no special
/// colour).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HighlightClass {
    /// Struct/variant/field identifiers and the `enable` keyword.
    Ident,
    /// Integer and float numeric literals.
    Number,
    /// String, raw-string, and char literals.
    StringLit,
    /// The `true` / `false` boolean keywords.
    Boolean,
    /// Punctuation: brackets, braces, parens, `:`, `,`, `#`, `!`.
    Punctuation,
    /// Line and block comments.
    Comment,
    /// Lex-error / recovery tokens (rendered to stand out).
    Error,
    /// Everything else (whitespace, structure, uncoloured text).
    Default,
}

impl HighlightClass {
    /// Map a `ron-core` [`SyntaxKind`] to its highlight class.
    #[must_use]
    pub fn from_kind(kind: SyntaxKind) -> Self {
        match kind {
            SyntaxKind::Ident | SyntaxKind::EnableKw => HighlightClass::Ident,
            SyntaxKind::Integer | SyntaxKind::Float => HighlightClass::Number,
            SyntaxKind::String | SyntaxKind::RawString | SyntaxKind::Char => {
                HighlightClass::StringLit
            }
            SyntaxKind::TrueKw | SyntaxKind::FalseKw => HighlightClass::Boolean,
            SyntaxKind::LParen
            | SyntaxKind::RParen
            | SyntaxKind::LBracket
            | SyntaxKind::RBracket
            | SyntaxKind::LBrace
            | SyntaxKind::RBrace
            | SyntaxKind::Colon
            | SyntaxKind::Comma
            | SyntaxKind::Hash
            | SyntaxKind::Bang => HighlightClass::Punctuation,
            SyntaxKind::LineComment | SyntaxKind::BlockComment => HighlightClass::Comment,
            SyntaxKind::LexError => HighlightClass::Error,
            _ => HighlightClass::Default,
        }
    }

    /// The stable, human-readable class name stored on a [`HighlightSpan`].
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            HighlightClass::Ident => "ident",
            HighlightClass::Number => "number",
            HighlightClass::StringLit => "string",
            HighlightClass::Boolean => "boolean",
            HighlightClass::Punctuation => "punctuation",
            HighlightClass::Comment => "comment",
            HighlightClass::Error => "error",
            HighlightClass::Default => "default",
        }
    }

    /// Parse a class name back into a [`HighlightClass`] (inverse of [`as_str`]).
    ///
    /// Unknown names resolve to [`HighlightClass::Default`] so a model produced by
    /// a newer build never colours unexpectedly.
    ///
    /// [`as_str`]: Self::as_str
    #[must_use]
    pub fn from_str_name(name: &str) -> Self {
        match name {
            "ident" => HighlightClass::Ident,
            "number" => HighlightClass::Number,
            "string" => HighlightClass::StringLit,
            "boolean" => HighlightClass::Boolean,
            "punctuation" => HighlightClass::Punctuation,
            "comment" => HighlightClass::Comment,
            "error" => HighlightClass::Error,
            _ => HighlightClass::Default,
        }
    }

    /// The colour this class paints with, given egui's current dark/light theme.
    #[must_use]
    pub fn color(self, dark_mode: bool) -> Color32 {
        if dark_mode {
            match self {
                HighlightClass::Ident => Color32::from_rgb(0x9C, 0xDC, 0xFE),
                HighlightClass::Number => Color32::from_rgb(0xB5, 0xCE, 0xA8),
                HighlightClass::StringLit => Color32::from_rgb(0xCE, 0x91, 0x78),
                HighlightClass::Boolean => Color32::from_rgb(0x56, 0x9C, 0xD6),
                HighlightClass::Punctuation => Color32::from_rgb(0xD4, 0xD4, 0xD4),
                HighlightClass::Comment => Color32::from_rgb(0x6A, 0x99, 0x55),
                HighlightClass::Error => Color32::from_rgb(0xF4, 0x47, 0x47),
                HighlightClass::Default => Color32::from_rgb(0xD4, 0xD4, 0xD4),
            }
        } else {
            match self {
                HighlightClass::Ident => Color32::from_rgb(0x00, 0x55, 0x88),
                HighlightClass::Number => Color32::from_rgb(0x09, 0x86, 0x58),
                HighlightClass::StringLit => Color32::from_rgb(0xA3, 0x15, 0x15),
                HighlightClass::Boolean => Color32::from_rgb(0x00, 0x00, 0xFF),
                HighlightClass::Punctuation => Color32::from_rgb(0x20, 0x20, 0x20),
                HighlightClass::Comment => Color32::from_rgb(0x00, 0x80, 0x00),
                HighlightClass::Error => Color32::from_rgb(0xCD, 0x31, 0x31),
                HighlightClass::Default => Color32::from_rgb(0x20, 0x20, 0x20),
            }
        }
    }
}

/// Build a [`HighlightModel`] from a parse result by walking the CST token stream
/// (FR-019).
///
/// Each non-default-class token becomes one [`HighlightSpan`] in **character**
/// offsets (the editor's coordinate space), tagged with its [`HighlightClass`]
/// name. The walk is a single pass over `parse.cst.root().descendant_tokens()`,
/// which yields every leaf token in source order (no second tokenizer). The
/// resulting model records `generation` so callers can skip recompute when their
/// installed generation already matches (see [`HighlightModel::generation`]).
#[must_use]
pub fn build_highlight_model(parse: &ParseResult, generation: u64) -> HighlightModel {
    let root = parse.cst.root();

    // Convert byte offsets to char offsets in one forward scan. CST tokens are in
    // source order, so a single advancing cursor over `char_indices` suffices.
    let source = root.text();
    let mut byte_to_char = ByteToChar::new(&source);

    let mut spans: Vec<HighlightSpan> = Vec::new();
    for token in root.descendant_tokens() {
        let class = HighlightClass::from_kind(token.kind());
        if class == HighlightClass::Default {
            continue;
        }
        let range = token.text_range();
        let start = byte_to_char.char_at(range.start());
        let end = byte_to_char.char_at(range.end());
        if start == end {
            continue;
        }
        spans.push(HighlightSpan {
            start,
            end,
            class: class.as_str().to_string(),
        });
    }

    HighlightModel {
        generation: Some(generation),
        spans,
    }
}

/// A forward-only byte-offset → char-offset resolver over a source string.
///
/// Tokens are visited in source order with non-decreasing offsets, so resolving
/// is amortised O(n) across the whole walk rather than O(n) per token.
struct ByteToChar<'a> {
    iter: std::str::CharIndices<'a>,
    source_len: usize,
    cur_byte: usize,
    cur_char: usize,
    done: bool,
}

impl<'a> ByteToChar<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            iter: source.char_indices(),
            source_len: source.len(),
            cur_byte: 0,
            cur_char: 0,
            done: false,
        }
    }

    /// The char offset at `byte_offset`. Offsets must be non-decreasing across
    /// calls; an offset past end-of-source clamps to the final char count.
    fn char_at(&mut self, byte_offset: usize) -> usize {
        let target = byte_offset.min(self.source_len);
        while self.cur_byte < target && !self.done {
            match self.iter.next() {
                Some((idx, _)) => {
                    // `idx` is the byte index of the char we are about to pass.
                    if idx >= target {
                        self.cur_byte = idx;
                        return self.cur_char;
                    }
                    self.cur_byte = idx;
                    self.cur_char += 1;
                }
                None => {
                    self.done = true;
                    self.cur_byte = self.source_len;
                    return self.cur_char;
                }
            }
        }
        self.cur_char
    }
}

/// Build a memoized layouter that colours `TextEdit` text per a highlight model
/// (FR-019).
///
/// Returns a closure matching egui 0.34's `TextEdit::layouter` signature
/// (`FnMut(&Ui, &dyn TextBuffer, f32) -> Arc<Galley>`). It lays out the buffer
/// into a [`LayoutJob`] whose segments are coloured from `spans`, caching the
/// produced [`Galley`] so repeated frames over unchanged text/wrap-width do not
/// re-shape. Text outside any span uses the default text colour.
///
/// The closure borrows `spans` and `dark_mode`; build it fresh per frame from the
/// document's installed model so it always reflects the latest highlight.
pub fn highlight_layouter(
    spans: &[HighlightSpan],
    font_size: f32,
    dark_mode: bool,
) -> impl FnMut(&Ui, &dyn TextBuffer, f32) -> Arc<Galley> + '_ {
    // Cache the last (text, wrap_width) → Galley so unchanged frames are cheap.
    let mut cached: Option<(String, f32, Arc<Galley>)> = None;
    move |ui: &Ui, buf: &dyn TextBuffer, wrap_width: f32| {
        let text = buf.as_str();
        if let Some((cached_text, cached_w, galley)) = &cached {
            if cached_text == text && (*cached_w - wrap_width).abs() < f32::EPSILON {
                return Arc::clone(galley);
            }
        }
        let mut job = build_layout_job(text, spans, font_size, dark_mode);
        job.wrap.max_width = wrap_width;
        let galley = ui.fonts_mut(|f| f.layout_job(job));
        cached = Some((text.to_string(), wrap_width, Arc::clone(&galley)));
        galley
    }
}

/// Assemble a coloured [`LayoutJob`] for `text` from highlight `spans`.
///
/// Spans are in character offsets; segments are appended in order, filling any
/// gaps with default-coloured runs so every byte of `text` is covered. Spans that
/// are out of order or overlapping are handled defensively (a span starting
/// before the cursor is skipped) so a malformed model can never panic or drop
/// text.
fn build_layout_job(
    text: &str,
    spans: &[HighlightSpan],
    font_size: f32,
    dark_mode: bool,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    let font = FontId::monospace(font_size);
    let default_color = HighlightClass::Default.color(dark_mode);

    // Map char offsets to byte offsets once: build a lookup of char→byte.
    // `char_indices` yields (byte, char) in order; collect the byte boundary for
    // each char index, plus the final length sentinel.
    let mut char_byte: Vec<usize> = Vec::with_capacity(text.len() + 1);
    for (b, _) in text.char_indices() {
        char_byte.push(b);
    }
    char_byte.push(text.len());

    let char_count = char_byte.len() - 1;
    let mut cursor_char = 0usize;

    let to_byte = |c: usize| -> usize { char_byte[c.min(char_count)] };

    for span in spans {
        let start = span.start.min(char_count);
        let end = span.end.min(char_count);
        // Defensive: ignore degenerate or backward spans.
        if end <= start || start < cursor_char {
            continue;
        }
        // Default-coloured gap before this span.
        if start > cursor_char {
            let gap = &text[to_byte(cursor_char)..to_byte(start)];
            append_run(&mut job, gap, font.clone(), default_color);
        }
        let class = HighlightClass::from_str_name(&span.class);
        let piece = &text[to_byte(start)..to_byte(end)];
        append_run(&mut job, piece, font.clone(), class.color(dark_mode));
        cursor_char = end;
    }

    // Trailing default-coloured remainder.
    if cursor_char < char_count {
        let tail = &text[to_byte(cursor_char)..];
        append_run(&mut job, tail, font.clone(), default_color);
    }

    job
}

/// Append one coloured run to a layout job (skips empty runs).
fn append_run(job: &mut LayoutJob, text: &str, font: FontId, color: Color32) {
    if text.is_empty() {
        return;
    }
    job.append(text, 0.0, TextFormat::simple(font, color));
}

/// Default monospace font size (points) for the editor surface.
const EDITOR_FONT_SIZE: f32 = 13.0;

/// Render the active-binding status strip for `doc` (E006 US2 — FR-011).
///
/// Always visible above the editor so the author can tell whether type-awareness is
/// on and against which type. When [`BindingState::Bound`](crate::binding::BindingState::Bound)
/// it shows `Type: <name> (<origin>)` (via [`EditorDocument::binding_label`]) plus
/// the source locator (`schema: …` / `rust: …`, via
/// [`EditorDocument::binding_source_label`]); when
/// [`BindingState::NoBinding`](crate::binding::BindingState::NoBinding) it shows an
/// explicit `no type bound` indicator. The label text is exactly what
/// [`EditorDocument::binding_label`] returns so tests can assert the shown state by
/// querying the rendered label rather than scraping pixels.
fn render_binding_indicator(ui: &mut Ui, doc: &EditorDocument) {
    ui.horizontal(|ui| {
        let label = doc.binding_label();
        if doc.binding.is_bound() {
            // Bound: emphasize the type label; show the source alongside (weak).
            ui.strong(label);
            if let Some(source) = doc.binding_source_label() {
                ui.weak(source);
            }
        } else {
            // Unbound: explicit "no type bound" indicator (structural-only).
            ui.weak(label);
        }
    });
}

/// Render the editor surface for `doc`: a monospace multiline editor with a
/// line-number gutter and (unless `oversize`) syntax highlighting (FR-004/FR-005).
///
/// * Empty and whitespace-only buffers are valid, editable states — neither is an
///   error and neither crashes (FR-021).
/// * When `oversize` (the file exceeds the configured large-file threshold), the
///   highlight layouter is **not** installed and a small non-blocking degrade
///   label is shown; the buffer stays fully editable (FR-017).
/// * Buffer mutations are detected via the [`egui::Response::changed`] flag and
///   reported to the document with [`EditorDocument::on_edit`] so a coalesced
///   reparse is requested next frame.
///
/// Returns `true` when the buffer changed this frame.
pub fn editor_view(ui: &mut Ui, doc: &mut EditorDocument, oversize: bool) -> bool {
    if oversize {
        // Non-blocking degrade indicator; editing remains available below.
        ui.horizontal(|ui| {
            ui.weak("Large file — highlighting disabled");
        });
    }

    // Active-binding indicator (E006 US2 — FR-011): always-visible status strip
    // just above the editor showing the bound type + origin (or "no type bound"),
    // with the source locator alongside. The resolved (most-specific) binding is
    // what `doc.binding` holds, so this reflects the single chosen binding.
    render_binding_indicator(ui, doc);

    let dark_mode = ui.visuals().dark_mode;
    let line_count = doc.buffer.lines().count().max(1)
        + usize::from(doc.buffer.ends_with('\n') || doc.buffer.is_empty());
    // `lines()` drops a trailing empty line; the editor shows one, so account for
    // a trailing newline. An empty buffer still shows a single (line 1) row.
    let gutter_digits = line_count.max(1).to_string().len();

    // Snapshot the highlight spans so the layouter can borrow them without
    // borrowing the whole document (the TextEdit needs `&mut doc.buffer`).
    let spans = doc
        .highlight
        .as_ref()
        .map(|m| m.spans.clone())
        .unwrap_or_default();

    // Snapshot the diagnostics so the squiggle pass can borrow them without
    // borrowing the whole document while the TextEdit holds `&mut doc.buffer`.
    let diagnostics: Vec<DiagnosticView> = if oversize {
        // Squiggles are suppressed entirely on oversize files (FR-008/FR-017).
        Vec::new()
    } else {
        doc.diagnostics.clone()
    };

    // Take any pending Problems-panel cursor jump (clamped to the live buffer)
    // before rendering, so we can install it into the TextEdit state this frame.
    let pending_jump = doc.take_cursor_jump();

    let mut changed = false;
    ui.horizontal_top(|ui| {
        render_gutter(ui, line_count, gutter_digits);

        let output = if oversize {
            // Plain monospace editor, no layouter (FR-017).
            egui::TextEdit::multiline(&mut doc.buffer)
                .font(egui::TextStyle::Monospace)
                .code_editor()
                .desired_width(f32::INFINITY)
                .desired_rows(20)
                .show(ui)
        } else {
            let mut layouter = highlight_layouter(&spans, EDITOR_FONT_SIZE, dark_mode);
            egui::TextEdit::multiline(&mut doc.buffer)
                .font(egui::TextStyle::Monospace)
                .code_editor()
                .desired_width(f32::INFINITY)
                .desired_rows(20)
                .layouter(&mut layouter)
                .show(ui)
        };

        if output.response.response.changed() {
            changed = true;
        }

        // Apply a queued caret jump from the Problems panel (FR-009): set the
        // TextEdit cursor to the diagnostic's start and scroll it into view.
        if let Some(offset) = pending_jump {
            apply_cursor_jump(ui, &output, offset);
        }

        // Inline diagnostic squiggles under each diagnostic char-range (FR-008).
        // Suppressed already above when oversize (empty `diagnostics`).
        if !diagnostics.is_empty() {
            draw_squiggles(ui, &output.galley, output.galley_pos, &diagnostics);
        }

        // Snippet tab-stop navigation (E005 Wave 4, FR-016). Runs BEFORE the
        // completion popup so an active snippet's `Tab`/`Shift+Tab` drive its stops
        // rather than the completion accept. A buffer edit drops the session.
        snippet_navigation(ui, doc, &output);

        // Structural autocomplete popup over the editor (E005 Wave 3, FR-022).
        // Suppressed on oversize files (the highlight/intelligence layer is off).
        // Suppressed while a snippet session is active so `Tab` is unambiguous.
        if completion_enabled(oversize, doc.snippet_session.is_some())
            && completion_popup(ui, doc, &output)
        {
            changed = true;
        }
    });

    if changed {
        doc.on_edit();
    }
    changed
}

/// Whether the structural-completion popup runs this frame (E005 Wave 5, T041,
/// FR-026).
///
/// Completion is a per-frame, always-on intelligence layer, so it degrades on the
/// **same** signal E003 already uses to suppress highlighting + squiggles: the
/// document being `oversize` (past `AppSettings::large_file_threshold`). When the
/// file is oversize, completion is suppressed exactly like highlighting/squiggles
/// and the existing E003 non-blocking degrade label ("Large file — highlighting
/// disabled") communicates the state — E005 invents **no** separate message. It is
/// also suppressed while a snippet tab-stop session is active so `Tab` unambiguously
/// drives snippet navigation rather than a completion accept.
///
/// Pure and side-effect-free so the gating decision is unit-testable without a live
/// egui frame.
#[must_use]
pub fn completion_enabled(oversize: bool, snippet_session_active: bool) -> bool {
    !oversize && !snippet_session_active
}

/// Move the editor caret to `char_offset` and scroll it into view (FR-009).
///
/// Installs a collapsed [`CCursorRange`] at `char_offset` into the live
/// [`TextEditState`] (loaded by the widget id), stores it back so it takes effect
/// on the next layout, then scrolls the caret's galley rectangle into view. The
/// offset is assumed already clamped to the buffer's character length by the
/// caller ([`EditorDocument::take_cursor_jump`]).
fn apply_cursor_jump(ui: &Ui, output: &egui::text_edit::TextEditOutput, char_offset: usize) {
    let id = output.response.response.id;
    let ctx = ui.ctx();

    let mut state = TextEditState::load(ctx, id).unwrap_or_default();
    let cursor = CCursor::new(char_offset);
    state.cursor.set_char_range(Some(CCursorRange::one(cursor)));
    state.store(ctx, id);

    // Scroll the caret rectangle (galley-local) into view, centred vertically.
    let local = output.galley.pos_from_cursor(cursor);
    let screen = local.translate(output.galley_pos.to_vec2());
    ui.scroll_to_rect(screen, Some(Align::Center));
}

/// Drive + render the structural autocomplete popup over the editor (E005 Wave 3,
/// US2, AD-007/HINT-005).
///
/// This is the egui half of the custom popup; the cross-frame decision logic lives
/// in [`crate::completion::CompletionState`] (headlessly tested, T031). Per frame
/// it:
///
/// 1. reads the live caret (a collapsed cursor, no active selection) as a byte
///    offset into the buffer;
/// 2. handles popup keystrokes *before* they reach text editing when the popup is
///    open — `Esc` dismisses (keeping the literal), `Up`/`Down` highlight, and
///    `Enter`/`Tab` accept **only** when an item is explicitly highlighted
///    (otherwise the literal stands; FR-012);
/// 3. recomputes candidates from the buffer + caret — auto-trigger mid-identifier,
///    or manual-invoke (`Ctrl`+`Space`) at any value slot; an empty/ambiguous
///    context shows no list (FR-014);
/// 4. renders the candidate list in analysis order, visually secondary to the
///    literal, anchored just below the caret.
///
/// Returns `true` when an accepted suggestion mutated the buffer (so the caller
/// requests a reparse).
///
/// # Headless-rendering boundary (E003)
///
/// The popup's *rendering* and live keystroke routing are exercised manually / in
/// QC. The buffer-mutating decisions (trigger, dismiss, accept + CST-verified
/// splice) are covered headlessly via the `completion` module API (T031).
pub fn completion_popup(ui: &mut Ui, doc: &mut EditorDocument, output: &TextEditOutput) -> bool {
    // Only operate with a single collapsed caret (no active selection): completion
    // is meaningless across a selection.
    let Some(range) = output.cursor_range else {
        doc.completion.dismiss();
        return false;
    };
    if !range.is_empty() {
        doc.completion.dismiss();
        return false;
    }
    let caret_char = range.primary.index;
    let caret_byte = char_to_byte(&doc.buffer, caret_char);

    // ---- keystroke handling (only while the popup is open) ----
    let mut accept_now = false;
    if doc.completion.is_open() {
        if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            // Esc dismisses; the user's literal is untouched (FR-022).
            doc.completion.dismiss();
            return false;
        }
        if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
            doc.completion.highlight_next();
        }
        if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
            doc.completion.highlight_prev();
        }
        // Enter/Tab accept ONLY when an item is explicitly highlighted; otherwise
        // we leave the key for the TextEdit so the literal stands (FR-012). We
        // therefore only *consume* the key when we will actually accept.
        if doc.completion.highlighted_item().is_some() {
            let enter = ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
            let tab = ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab));
            if enter || tab {
                accept_now = true;
            }
        }
    }

    // ---- accept a highlighted suggestion (CST-verified splice, FR-013) ----
    if accept_now {
        if let Some(accepted) = doc.completion.accept(&doc.buffer, caret_byte) {
            doc.buffer = accepted.new_buffer;
            doc.completion.dismiss();
            // Move the caret to the end of the inserted text (FR-022).
            let new_char = byte_to_char(&doc.buffer, accepted.new_caret_byte);
            set_caret(ui, output, new_char);
            doc.on_edit();
            return true;
        }
        // A refused splice (would corrupt) keeps the literal; just dismiss.
        doc.completion.dismiss();
        return false;
    }

    // ---- recompute candidates (trigger / dismiss) ----
    let manual = ui.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Space));
    let trigger = if manual {
        Trigger::Manual
    } else {
        Trigger::Auto
    };
    let open = doc.completion.recompute(&doc.buffer, caret_byte, trigger);

    // ---- render ----
    if open {
        render_completion_list(ui, output, doc, caret_char);
    }
    false
}

/// Drive snippet tab-stop navigation over the editor while a session is active
/// (E005 Wave 4, US3, FR-016).
///
/// Per frame, when [`EditorDocument::snippet_session`] is `Some`:
///
/// 1. drops the session if the buffer was edited out from under it (a stop offset no
///    longer maps), so an edit during navigation never lands the caret out of bounds;
/// 2. on `Esc`, ends navigation (the inserted text stays — only navigation stops);
/// 3. on `Tab` / `Shift+Tab`, moves to the next / previous stop and installs the new
///    caret + selection into the live `TextEdit` (so the user can overtype a
///    placeholder default); reaching past the final `$0` ends navigation (FR-016);
/// 4. renders the inline choice picker for a [`TabStopKind::Choice`] active stop.
///
/// The keys are consumed only while a session is active, so ordinary `Tab` typing is
/// unaffected once navigation ends.
///
/// # Headless-rendering boundary (E003)
///
/// The live keystroke routing + the choice-picker rendering are exercised
/// manually / in QC; the session's pure navigation logic (next/prev/end, caret +
/// selection derivation) is covered headlessly via the `snippets` module API (T038).
fn snippet_navigation(ui: &mut Ui, doc: &mut EditorDocument, output: &TextEditOutput) {
    let Some(session) = doc.snippet_session.as_mut() else {
        return;
    };
    if !session.is_active() {
        doc.snippet_session = None;
        return;
    }

    // If the active stop's range no longer fits the buffer (the user edited the text
    // during navigation), end the session rather than risk an out-of-bounds caret.
    let char_len = doc.buffer.chars().count();
    if session
        .active_stop()
        .is_some_and(|s| s.char_end > char_len || s.char_start > char_len)
    {
        doc.snippet_session = None;
        return;
    }

    // Esc ends navigation (the inserted snippet text is kept; only nav stops).
    if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        session.end();
        doc.snippet_session = None;
        return;
    }

    // Tab → next stop; Shift+Tab → previous. Consume so the TextEdit does not also
    // insert a tab / move focus while a session is active.
    let mut moved = false;
    if ui.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::Tab)) {
        session.prev_stop();
        moved = true;
    } else if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab)) {
        session.next_stop();
        moved = true;
    }

    if moved {
        match session.active_stop() {
            Some(stop) => {
                // Install the new caret + (placeholder/choice) selection so the user
                // can immediately overtype the default value.
                let (start, end) = (stop.char_start, stop.char_end);
                set_caret_range(ui, output, start, end);
            }
            None => {
                // Past the final stop: navigation has ended (FR-016).
                doc.snippet_session = None;
                return;
            }
        }
    }

    // Render the inline choice picker for a choice-kind active stop.
    if let Some(stop) = doc.snippet_session.as_ref().and_then(|s| s.active_stop()) {
        if let TabStopKind::Choice { options } = &stop.kind {
            let options = options.clone();
            let (start, end) = (stop.char_start, stop.char_end);
            render_choice_picker(ui, output, doc, start, end, &options);
        }
    }
}

/// Render the inline choice picker for a snippet choice tab-stop (FR-016).
///
/// Shows the options as a small floating list anchored at the stop; picking one
/// replaces the stop's current text `[start, end)` with the chosen option (a plain
/// buffer splice — the option came from the snippet body, so the result still
/// round-trips) and advances the session's stop offsets are left for the next nav.
fn render_choice_picker(
    ui: &mut Ui,
    output: &TextEditOutput,
    doc: &mut EditorDocument,
    start: usize,
    end: usize,
    options: &[String],
) {
    let anchor_rect = output
        .galley
        .pos_from_cursor(CCursor::new(start))
        .translate(output.galley_pos.to_vec2());
    let anchor = Pos2::new(anchor_rect.left(), anchor_rect.bottom() + 2.0);

    let mut picked: Option<String> = None;
    egui::Area::new(ui.id().with("ronin_snippet_choice"))
        .order(egui::Order::Foreground)
        .fixed_pos(anchor)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_max_width(200.0);
                for opt in options {
                    if ui.selectable_label(false, opt).clicked() {
                        picked = Some(opt.clone());
                    }
                }
            });
        });

    if let Some(choice) = picked {
        // Splice the chosen option over the stop's current text.
        let start_byte = char_to_byte(&doc.buffer, start);
        let end_byte = char_to_byte(&doc.buffer, end);
        if start_byte <= end_byte && end_byte <= doc.buffer.len() {
            let mut next = String::with_capacity(doc.buffer.len());
            next.push_str(&doc.buffer[..start_byte]);
            next.push_str(&choice);
            next.push_str(&doc.buffer[end_byte..]);
            // Only commit if the choice keeps the buffer parseable (it should, since
            // the option came from the snippet body) — never corrupt (§I).
            if ron_core::parse(&next).diagnostics().len()
                <= ron_core::parse(&doc.buffer).diagnostics().len()
            {
                doc.buffer = next;
                let new_end = start + choice.chars().count();
                set_caret_range(ui, output, start, new_end);
                doc.on_edit();
                // The chosen text length may differ; end the picker's stop selection.
                // The next Tab continues navigation from here.
            }
        }
    }
}

/// Install a (possibly non-collapsed) caret selection `[start, end)` into the live
/// `TextEdit` state, so the user can overtype a placeholder default (FR-016).
fn set_caret_range(ui: &Ui, output: &TextEditOutput, start: usize, end: usize) {
    let id = output.response.response.id;
    let ctx = ui.ctx();
    let mut state = TextEditState::load(ctx, id).unwrap_or_default();
    let range = CCursorRange::two(CCursor::new(start), CCursor::new(end));
    state.cursor.set_char_range(Some(range));
    state.store(ctx, id);
    // Scroll the stop into view.
    let local = output.galley.pos_from_cursor(CCursor::new(start));
    let screen = local.translate(output.galley_pos.to_vec2());
    ui.scroll_to_rect(screen, Some(Align::Center));
}

/// Render the open completion list as an `egui::Area` just below the caret.
///
/// Items are shown in analysis order (kind-then-alpha, already ranked below the
/// literal). The explicitly-highlighted row (if any) is visually selected; the
/// list as a whole is rendered weakly/secondary so it never visually competes with
/// the literal the user typed (FR-022).
fn render_completion_list(
    ui: &Ui,
    output: &TextEditOutput,
    doc: &EditorDocument,
    caret_char: usize,
) {
    let items = doc.completion.items();
    if items.is_empty() {
        return;
    }
    // Anchor just below the caret's galley rectangle.
    let caret_rect = output
        .galley
        .pos_from_cursor(CCursor::new(caret_char))
        .translate(output.galley_pos.to_vec2());
    let anchor = Pos2::new(caret_rect.left(), caret_rect.bottom() + 2.0);

    let highlighted = doc.completion.highlighted();
    egui::Area::new(ui.id().with("ronin_completion_popup"))
        .order(egui::Order::Foreground)
        .fixed_pos(anchor)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_max_width(280.0);
                for (i, item) in items.iter().enumerate() {
                    let selected = highlighted == Some(i);
                    let text = format!("{}  {}", kind_glyph(item.kind), item.label);
                    if selected {
                        ui.label(egui::RichText::new(text).strong());
                    } else {
                        // Secondary to the literal: weak, low-emphasis rows.
                        ui.weak(text);
                    }
                }
            });
        });
}

/// A short glyph hint for a completion kind (display-only).
fn kind_glyph(kind: CompletionKind) -> &'static str {
    match kind {
        CompletionKind::Field => "f",
        CompletionKind::Variant => "v",
        CompletionKind::MapKey => "k",
        CompletionKind::Option => "o",
        CompletionKind::Delimiter => ".",
    }
}

/// Convert a character offset into a byte offset within `buffer` (clamped).
fn char_to_byte(buffer: &str, char_offset: usize) -> usize {
    buffer
        .char_indices()
        .nth(char_offset)
        .map_or(buffer.len(), |(b, _)| b)
}

/// Convert a byte offset into a character offset within `buffer` (clamped).
fn byte_to_char(buffer: &str, byte_offset: usize) -> usize {
    let clamped = byte_offset.min(buffer.len());
    buffer[..clamped].chars().count()
}

/// Install a collapsed caret at `char_offset` into the live `TextEdit` state.
fn set_caret(ui: &Ui, output: &TextEditOutput, char_offset: usize) {
    let id = output.response.response.id;
    let ctx = ui.ctx();
    let mut state = TextEditState::load(ctx, id).unwrap_or_default();
    let cursor = CCursor::new(char_offset);
    state.cursor.set_char_range(Some(CCursorRange::one(cursor)));
    state.store(ctx, id);
}

/// Draw an underline squiggle beneath each diagnostic's character range (FR-008,
/// E006/FR-004).
///
/// For every [`DiagnosticView`], the start/end char offsets are resolved to galley
/// rectangles via [`Galley::pos_from_cursor`]; the squiggle is drawn just below the
/// row baseline in the severity colour. Multi-row ranges are underlined per row by
/// walking the galley rows the range spans. Degenerate (empty) ranges are skipped.
///
/// The `diagnostics` slice carries both `ron-core` structural findings and
/// `ron-validate` type findings (merged by
/// [`merge_type_diagnostics`](crate::document::merge_type_diagnostics)); both render
/// here by severity (type Errors like structural Errors, type Warnings in the
/// warning colour). The two are distinguishable by each view's
/// [`code`](DiagnosticView::code)
/// [`source`](ron_core::DiagnosticCode::source) tag, but render uniformly by
/// severity so existing structural rendering is unchanged.
fn draw_squiggles(ui: &Ui, galley: &Galley, galley_pos: Pos2, diagnostics: &[DiagnosticView]) {
    let dark_mode = ui.visuals().dark_mode;
    let painter = ui.painter();
    let offset = galley_pos.to_vec2();

    for diag in diagnostics {
        let (start, end) = diag.char_range;
        if end <= start {
            continue;
        }
        let color = severity_squiggle_color(diag.severity, dark_mode);

        // Resolve the start and end rectangles in galley-local space, then shift
        // to screen space. `pos_from_cursor` returns a zero-width caret rect.
        let start_rect = galley
            .pos_from_cursor(CCursor::new(start))
            .translate(offset);
        let end_rect = galley.pos_from_cursor(CCursor::new(end)).translate(offset);

        if (start_rect.top() - end_rect.top()).abs() < f32::EPSILON {
            // Single-row range: underline from start.x to end.x at that row.
            draw_underline(
                painter,
                start_rect.left(),
                end_rect.left(),
                start_rect,
                color,
            );
        } else {
            // Multi-row range: underline each spanned galley row across its width.
            let top = start_rect.top().min(end_rect.top());
            let bottom = start_rect.bottom().max(end_rect.bottom());
            for row in &galley.rows {
                let row_rect = row.rect().translate(offset);
                let row_mid = (row_rect.top() + row_rect.bottom()) * 0.5;
                if row_mid < top || row_mid > bottom {
                    continue;
                }
                let x0 = if row_rect.top() <= start_rect.top() + f32::EPSILON
                    && start_rect.top() <= row_rect.bottom()
                {
                    start_rect.left()
                } else {
                    row_rect.left()
                };
                let x1 = if row_rect.top() <= end_rect.top() + f32::EPSILON
                    && end_rect.top() <= row_rect.bottom()
                {
                    end_rect.left()
                } else {
                    row_rect.right()
                };
                draw_underline(painter, x0, x1, row_rect, color);
            }
        }
    }
}

/// Paint a single wavy underline run from `x0` to `x1` just below `row_rect`.
///
/// The wave is a small zig-zag polyline (a few logical pixels tall) drawn at the
/// row's bottom; a zero/near-zero width run still draws a short tick so a caret-
/// width diagnostic remains visible.
fn draw_underline(painter: &egui::Painter, x0: f32, x1: f32, row_rect: Rect, color: Color32) {
    /// Vertical amplitude of the squiggle wave (logical pixels).
    const AMPLITUDE: f32 = 1.5;
    /// Horizontal period of one wave segment (logical pixels).
    const PERIOD: f32 = 4.0;

    let left = x0.min(x1);
    // Guarantee a minimum visible width so caret-width ranges still show.
    let right = (x0.max(x1)).max(left + PERIOD);
    let baseline = row_rect.bottom() - 1.0;

    let mut points: Vec<Pos2> = Vec::new();
    let mut x = left;
    let mut up = true;
    while x < right {
        let y = if up { baseline - AMPLITUDE } else { baseline };
        points.push(Pos2::new(x, y));
        up = !up;
        x += PERIOD * 0.5;
    }
    // Ensure the wave reaches the right edge.
    points.push(Pos2::new(
        right,
        if up { baseline - AMPLITUDE } else { baseline },
    ));

    if points.len() >= 2 {
        painter.add(egui::Shape::line(points, Stroke::new(1.0, color)));
    }
}

/// The squiggle colour for a diagnostic severity, theme-aware.
fn severity_squiggle_color(severity: Severity, dark_mode: bool) -> Color32 {
    match severity {
        Severity::Error => {
            if dark_mode {
                Color32::from_rgb(0xF4, 0x47, 0x47)
            } else {
                Color32::from_rgb(0xCD, 0x31, 0x31)
            }
        }
        Severity::Warning => {
            if dark_mode {
                Color32::from_rgb(0xCC, 0xA7, 0x00)
            } else {
                Color32::from_rgb(0xBF, 0x83, 0x03)
            }
        }
    }
}

/// Render the left line-number gutter, one right-aligned number per row.
fn render_gutter(ui: &mut Ui, line_count: usize, digits: usize) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        for n in 1..=line_count {
            ui.monospace(format!("{n:>digits$} "));
        }
    });
}
