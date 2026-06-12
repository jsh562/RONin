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
//! # Deferred scope (E005 / E006)
//!
//! This widget provides structural highlighting and squiggles only. The
//! intelligence layered on top of the editing surface is deferred:
//!
//! * **autocomplete** and **format** (reflow / pretty-print) — deferred to
//!   **E005**;
//! * **type validation** (schema-aware, type-checked diagnostics over the
//!   structural ones) — deferred to **E006**.
//!
//! Both attach here, at the editing surface, without changing the shell.

use std::sync::Arc;

use egui::text::{CCursor, CCursorRange, LayoutJob, TextFormat};
use egui::text_edit::TextEditState;
use egui::{Align, Color32, FontId, Galley, Pos2, Rect, Stroke, TextBuffer, Ui};

use ron_core::{Severity, SyntaxKind};

use crate::diagnostics_map::DiagnosticView;
use crate::document::{EditorDocument, HighlightModel, HighlightSpan};
use crate::reparse::ParseResult;

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
    });

    if changed {
        doc.on_edit();
    }
    changed
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

/// Draw an underline squiggle beneath each diagnostic's character range (FR-008).
///
/// For every [`DiagnosticView`], the start/end char offsets are resolved to galley
/// rectangles via [`Galley::pos_from_cursor`]; the squiggle is drawn just below the
/// row baseline in the severity colour. Multi-row ranges are underlined per row by
/// walking the galley rows the range spans. Degenerate (empty) ranges are skipped.
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
