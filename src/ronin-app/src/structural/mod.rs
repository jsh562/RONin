//! Structural editing surfaces (E008) — the tree/form + table views and their
//! shared scaffolding.
//!
//! This module hosts the structure-aware editing surfaces RONin layers over the
//! lossless CST: a **tree/form** view for any nested RON (US1) and a virtualized
//! **spreadsheet/table** view for uniform sections (US2), with automatic uniform
//! detection + a visible boundary + a user override (US3). Every structural edit
//! routes through `ronin-core`'s pure CST→CST transforms so it round-trips
//! byte-for-byte and lands as one E007 undo unit (FR-013/FR-014).
//!
//! # Phase 1b scaffolding (this commit)
//!
//! The foundational, view-agnostic pieces both delivery phases depend on:
//!
//! * [`view_state`] — the cross-reparse **node identity** ([`StructuralPath`] +
//!   [`resolve_path`]/[`path_of`], AD-004/FR-016/FR-027) and the per-document
//!   [`ViewSelectionAndFocus`] (active view, edit focus, section overrides, stale
//!   marker — FR-016/FR-017).
//! * [`projection`] — the shared, off-frame CST→projection derivation
//!   ([`derive_projection`] → [`DerivedProjection`], AD-003/FR-015/FR-019/FR-026)
//!   the per-view models realize lazily on top of.
//!
//! # US3 — automatic uniform detection + boundary/fallback (this phase)
//!
//! [`render_structural_view`] is the **auto-routing structural surface**: within one
//! document view it classifies each section and renders a table-eligible uniform list
//! as an embedded table (US2 [`render_table_view`]) and everything else as tree/form
//! (US1 [`render_tree_view`]), with a **persistent per-section boundary indicator**
//! (table-vs-tree, auto-vs-forced + the fallback reason on hover) and a
//! **discoverable, reversible per-section override** (FR-010/FR-011/FR-012/FR-024/
//! FR-025). The conservative [`classifier`] never coerces non-uniform data into a
//! grid (FR-011); viewing/classifying changes zero bytes (FR-020).
//!
//! ## How the document-level switcher reconciles with per-section auto-routing
//!
//! The Phase-1b per-document switcher chooses **Text vs Structural** (FR-017). The
//! structural arm is [`ActiveView::TreeForm`] — the **default** — and it now hosts
//! [`render_structural_view`], which auto-routes the section (table vs tree/form) and
//! honors a per-section override. [`ActiveView::Text`] stays the whole-document text
//! view; selecting it shows the document as text regardless of any per-section
//! overrides (which are retained for the return — FR-024). [`ActiveView::Table`]
//! remains the US2 standalone whole-document table arm. A per-section override applies
//! only while in a structural view and only to its one section, and never changes the
//! document-level active view (FR-024).

pub mod classifier;
pub mod indicators;
pub mod projection;
pub mod sections;
pub mod table;
pub mod tree;
pub mod view_state;

pub use classifier::{classify, FallbackReason, Verdict};
pub use indicators::TypeIndicator;
pub use projection::{capture_path, derive_projection, ChildOutline, DerivedProjection, NodeKind};
pub use sections::{scan_table_sections, SectionShape, TableSection};
pub use table::{
    breadcrumb_segments, render_table_view, render_table_view_any, render_table_view_any_counting,
    render_table_view_counting, BreadcrumbSegment, Cell, CellClass, Column, ColumnClass, Row,
    TableModel,
};
pub use tree::{
    render_tree_view, LeafWidget, OptionShape, TreeEditable, TreeFormModel, TreeNode, TreeNodeKind,
};
pub use view_state::{
    path_of, resolve_path, resolve_path_visiting, ActiveView, DrillInReturn, EditFocus,
    FocusSurface, PathStep, SectionOverride, SectionRendering, StructuralPath,
    ViewSelectionAndFocus,
};

use egui::{RichText, Ui};

use ronin_core::ast;

use crate::document::EditorDocument;
use crate::reparse::ReparseWorker;

/// Render the **auto-routing structural view** for `doc` (E008 / US3 — T039/T040/
/// T041, [COMPLETES FR-010]).
///
/// This is the structural surface the per-document switcher's structural arm hosts.
/// It classifies the document's top-level section with the conservative [`classify`]
/// rule and routes it (FR-010/FR-011):
///
/// * a **table-eligible** uniform list (>2 elements) → an embedded virtualized table
///   ([`render_table_view`]) — unless a per-section override forces tree/form;
/// * **everything else** (non-uniform / variant / nested / small / empty, or a
///   non-list root) → tree/form ([`render_tree_view`]) — unless a per-section
///   override forces a (uniform) list to a table.
///
/// Above the chosen surface it draws a **persistent boundary indicator** (T040): a
/// header/badge labeling the section "table" vs "tree/form" and "auto" vs "forced",
/// the fallback reason revealed on focus/hover when it fell back (T041 / FR-025), and
/// a **discoverable, reversible per-section override** control whose current state is
/// visible (FR-012). The classifier and routing change **zero** document bytes
/// (FR-020) and never coerce non-uniform data into a grid (FR-011).
pub fn render_structural_view(ui: &mut Ui, doc: &mut EditorDocument, worker: &ReparseWorker) {
    // The inner surface (tree/table) renders its own stale marker (FR-015); the
    // boundary indicator is drawn here regardless.
    if doc.parse.is_none() {
        ui.weak("Parsing\u{2026}");
        return;
    }

    // Classify the document's top-level section (zero bytes, FR-020). The section the
    // structural view auto-routes is the document's top-level value addressed by the
    // root path; only a list can be table-eligible.
    let section = StructuralPath::root();
    let verdict = classify_section(doc, &section);

    // Resolve the switcher↔override↔classifier precedence for this one section
    // (FR-024). In a structural view this is always `Some`.
    let rendering = doc
        .view_state()
        .section_rendering(&section, verdict.table_eligible)
        .unwrap_or(SectionRendering::TreeForm { forced: false });

    // A pending drill-in return (FR-006): the user drilled into a nested table cell
    // and is now in the tree/form surface. Force the tree/form rendering so the
    // drilled-in subtree shows with its discoverable "back to table" control — even
    // when the top-level section would otherwise auto-route to a table.
    let drilled_in = doc.view_state().drill_in_return().is_some();

    // The persistent per-section boundary indicator + reversible override control
    // (FR-012/FR-025). It reads/writes only view state (byte-free, FR-020).
    render_section_boundary(ui, doc, &section, &verdict, rendering);
    ui.separator();

    // Route to the chosen surface (FR-010/FR-011). Never coerces non-uniform data:
    // a table only renders when the rendering resolved to a table (and we are not in
    // a drill-in return, which forces tree/form so the back control is reachable).
    if rendering.is_table() && !drilled_in {
        // The auto-routed embedded table is the document's top-level uniform record
        // list (US3 behavior unchanged): root section, RecordList shape.
        render_table_view(ui, doc, worker, &section, SectionShape::RecordList);
    } else {
        render_tree_view(ui, doc, worker);
    }
}

/// Classify the section addressed by `section` against the document's landed CST.
///
/// A node that does not resolve / is not a list yields an
/// [`FallbackReason::Unparseable`](classifier::FallbackReason::Unparseable) verdict
/// so routing degrades safely to tree/form (FR-019). A pure read — zero bytes
/// (FR-020).
fn classify_section(doc: &EditorDocument, section: &StructuralPath) -> Verdict {
    let Some(parse) = doc.parse.as_ref() else {
        return Verdict {
            table_eligible: false,
            column_schema: Vec::new(),
            fallback_reason: Some(FallbackReason::Unparseable),
        };
    };
    let root = parse.cst.root();
    match resolve_path(&root, section) {
        // Only a list is ever table-eligible; a non-list section routes to tree/form
        // (the classifier reports Unparseable for a non-list node).
        Some(node) if ast::List::cast(node.clone()).is_some() => classify(&node),
        _ => Verdict {
            table_eligible: false,
            column_schema: Vec::new(),
            // A non-list top-level value is simply rendered as tree/form; flag it as
            // not-a-record-list so the boundary reason is meaningful when shown.
            fallback_reason: Some(FallbackReason::NotARecordList),
        },
    }
}

/// Render one section's persistent boundary indicator + reversible per-section
/// override control (T040/T041 — FR-012/FR-025).
///
/// The header always shows: a kind badge ("table" / "tree/form"), an auto-vs-forced
/// marker, the fallback reason (when the section fell back, revealed on focus/hover —
/// FR-025), and a toggle that forces the section away from / back to its automatic
/// rendering (FR-012). The control's current state is visible so the user can tell
/// auto from a manual override. Reading/writing the override changes zero bytes
/// (FR-020) and never changes the document-level active view (FR-024).
fn render_section_boundary(
    ui: &mut Ui,
    doc: &mut EditorDocument,
    section: &StructuralPath,
    verdict: &Verdict,
    rendering: SectionRendering,
) {
    let mut toggle = false;
    ui.horizontal(|ui| {
        // The persistent kind badge — perceivable without interaction (FR-012). The
        // badge glyph reuses the shared [`TypeIndicator`] so the table/tree boundary
        // markers match the icons the inner views paint (E014): the table badge is
        // the Map/table glyph (▦), the tree/form badge the Struct glyph (▢).
        let (indicator, label) = match rendering {
            SectionRendering::Table { .. } => (TypeIndicator::Map, "table"),
            SectionRendering::TreeForm { .. } => (TypeIndicator::Struct, "tree/form"),
        };
        // The badge glyph goes through the shared fixed-width slot (E014) so the badge
        // icon aligns with the icons the inner views paint.
        indicator.show(ui).on_hover_text(indicator.word());
        ui.label(
            RichText::new(label)
                .color(indicator.color(ui))
                .strong(),
        );

        // The auto-vs-forced state marker (FR-012): the user can tell whether the
        // section is showing its automatic rendering or a manual override.
        if rendering.is_forced() {
            ui.label(RichText::new("[forced]").italics().weak())
                .on_hover_text("A manual per-section override is active for this section");
        } else {
            ui.label(RichText::new("[auto]").weak())
                .on_hover_text("Automatic rendering from the uniform-section classifier");
        }

        // The fallback reason (FR-025): surfaced on the boundary indicator on
        // focus/hover whenever the section is rendered as tree/form because the
        // classifier did not deem it table-eligible.
        if !verdict.table_eligible {
            if let Some(reason) = verdict.fallback_reason {
                ui.label(RichText::new("\u{24D8}").weak())
                    .on_hover_text(format!(
                        "Shown as tree/form: {} ({reason:?})",
                        reason.label()
                    ));
            }
        }

        // The discoverable, reversible per-section override toggle (FR-012). Its
        // label states the action AND reflects the current state.
        let toggle_label = match rendering {
            // Auto table → offer to push it to tree/form.
            SectionRendering::Table { forced: false } => "show as tree/form",
            // Auto tree/form (a non-uniform or small list) → offer to force a table.
            SectionRendering::TreeForm { forced: false } => "show as table",
            // Forced renderings → offer to return to the automatic rendering.
            SectionRendering::Table { forced: true } => "back to auto (tree/form)",
            SectionRendering::TreeForm { forced: true } => "back to auto (table)",
        };
        if ui
            .small_button(toggle_label)
            .on_hover_text("Override how this section renders (reversible)")
            .clicked()
        {
            toggle = true;
        }
    });

    if toggle {
        // Toggle the override against the classifier's verdict, byte-free (FR-012/
        // FR-020). This never touches the document-level active view (FR-024).
        doc.view_state_mut()
            .toggle_section_override(section, verdict.table_eligible);
    }
}
