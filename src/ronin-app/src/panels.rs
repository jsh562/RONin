//! Panel layout seams for the editor shell.
//!
//! This module is the *single place* later epics extend the shell's side/bottom
//! panels **without** editing shell-core code. It exposes:
//!
//! * an **active** diagnostics-panel region ([`render_diagnostics_seam`]);
//! * the **active** structural **table** host ([`render_table_seam`]) — the E008
//!   virtualized spreadsheet view, wired into the per-document view switcher's
//!   Table arm (US2 / T035); and
//! * one **reserved** seam rendered as a labeled, disabled placeholder:
//!   [`mode_selector_seam_stub`] (reserved for **E009** — the Bevy mode selector).
//!   The legacy [`tree_table_seam_stub`] placeholder remains for layout/host
//!   discoverability; the live table now renders through [`render_table_seam`].
//!
//! The reserved seams render a faint "coming soon" placeholder rather than being
//! empty or a `// TODO`, so the layout is visible and the integration point is
//! discoverable in the running app.
//!
//! # Deferred scope (E008 / E009)
//!
//! The two reserved seams here are deliberate, named hand-offs:
//!
//! * [`tree_table_seam_stub`] reserves the structural **tree / virtualized table**
//!   views — deferred to **E008**.
//! * [`mode_selector_seam_stub`] reserves the **Bevy mode** selector — deferred to
//!   **E009**.
//!
//! Those epics populate these seams without editing shell-core layout.

use crate::diagnostics_map::DiagnosticView;
use crate::document::EditorDocument;
use crate::editor_view::render_binding_indicator;
use crate::reparse::ReparseWorker;
use crate::structural::indicators;
use crate::structural::table::{
    breadcrumb_segments, combinable_child_fields, grouped_view_model, render_table_grid_for,
    render_table_view_any, text_px_width, TableModel,
};
use crate::structural::tree::{TreeNode, TreeNodeKind};
use crate::structural::view_state::{resolve_path, PathStep, StructuralPath};

/// Host the structural **table-view tree-outline navigator** for `doc` (E008 / E012 /
/// E013 — T035, [COMPLETES FR-005]).
///
/// Renders the always-visible active-binding indicator (FR-011) at the top, then a
/// **tree-outline navigator**: a collapsible outline built from
/// [`EditorDocument::cached_tree_model`] that mirrors the document tree. Each
/// **container** node (a struct / map / list / tuple / struct-like enum variant — any
/// node with children) is a selectable [`egui::CollapsingHeader`] row showing its
/// [`TypeIndicator`](indicators::TypeIndicator) icon + label (+ child count); scalar
/// leaf nodes are skipped (never listed). Clicking a row selects that node so the
/// central grid renders it as a table via [`TableModel::derive_any`](crate::structural::table::TableModel::derive_any)
/// — viewing ANY level of the document as a table.
///
/// The view is **never empty** (Part A3): when no node is selected (or the stored
/// selection no longer resolves to a table-able node) it defaults to the document
/// **root**, so e.g. `sample.ron` always shows its root `Config` as a field/value grid.
/// Scalar cells edit inline, RecordList rows add/remove, a nested struct/tuple cell
/// drills into the tree/form surface, and a nested list/map cell opens AS A TABLE in
/// place (re-keying the selection), each routed through the one-undo-unit
/// structural-edit pipeline (FR-013/FR-014).
///
/// This is the [COMPLETES FR-005] host point wired into the per-document view
/// switcher's Table arm (FR-017). The `worker` is the document's off-frame reparse
/// worker, used to re-derive the projection after an edit lands.
/// The fitted width for a navigator side-panel (E023): the widest item label + the icon
/// slot + scrollbar/padding, clamped. `default_size` only applies on first load; the panel
/// stays user-resizable after.
pub fn nav_panel_width(max_label_px: f32) -> f32 {
    (max_label_px + indicators::SLOT_WIDTH + 28.0).clamp(200.0, 460.0)
}

/// The widest rendered outline-row label width (E023): for each container node, its indent
/// (`depth * spacing.indent`) plus the measured label (+ child-count suffix). Depth-capped
/// so a pathologically deep document can't make this costly (the width is only a first-load
/// default).
fn outline_max_label_px(ui: &egui::Ui, nodes: &[TreeNode], depth: usize) -> f32 {
    if depth > 8 {
        return 0.0;
    }
    let indent = ui.spacing().indent;
    let mut max = 0.0_f32;
    for node in nodes {
        if !is_outline_container(node) {
            continue;
        }
        let count = node
            .children
            .iter()
            .filter(|c| is_outline_container(c))
            .count();
        let label = if count > 0 {
            format!("{}  ({count})", node.label)
        } else {
            node.label.clone()
        };
        let w = depth as f32 * indent + text_px_width(ui, &label, egui::TextStyle::Body);
        max = max.max(w);
        max = max.max(outline_max_label_px(ui, &node.children, depth + 1));
    }
    max
}

pub fn render_table_seam(ui: &mut egui::Ui, doc: &mut EditorDocument, worker: &ReparseWorker) {
    render_binding_indicator(ui, doc);

    // Clone the tree model out so the borrow on `doc` is released before the mutable
    // view-state writes below (the model is cached per parse generation — FR-026). The
    // outline + default-to-root both walk it.
    let Some(model) = doc.cached_tree_model().cloned() else {
        ui.weak("Parsing\u{2026}");
        return;
    };
    if model.roots.is_empty() {
        ui.weak("(empty document)");
        return;
    }

    // Resolve the node the grid renders (Part A3 — never empty): the stored selection
    // when it still resolves to a table-able node against the live CST, else default to
    // the document root so the Table view always shows something.
    let stored = doc.view_state().selected_table_section().cloned();
    let selected = stored
        .filter(|p| selection_is_table_able(doc, p))
        .unwrap_or_else(StructuralPath::root);

    // The collapsible tree-outline navigator side list (Part A1).
    let nav_w = nav_panel_width(outline_max_label_px(ui, &model.roots, 0));
    let mut clicked: Option<StructuralPath> = None;
    egui::Panel::left("ronin_table_navigator")
        .resizable(true)
        .default_size(nav_w)
        .show_inside(ui, |ui| {
            ui.strong("Outline");
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                for (i, root) in model.roots.iter().enumerate() {
                    render_outline_node(ui, root, &selected, 0, i, &mut clicked);
                }
            });
        });

    // Persist a click (byte-free view-state write — FR-020). Route through
    // `navigate_table_section` so the back/forward history records the level change.
    if let Some(path) = clicked {
        doc.view_state_mut().navigate_table_section(path);
        return;
    }

    // The central area: a Back/Forward/Up navigation row + a stateless, path-derived
    // breadcrumb above the grid, then the selected node projected as a table via
    // `derive_any` (Part A1/A3 + E016 navigation).
    let mut breadcrumb_clicked: Option<StructuralPath> = None;
    egui::CentralPanel::default().show_inside(ui, |ui| {
        render_table_nav_controls(ui, doc);
        render_breadcrumb(ui, doc, &selected, &mut breadcrumb_clicked);
        ui.separator();
        render_table_view_any(ui, doc, worker, &selected);
    });

    if let Some(path) = breadcrumb_clicked {
        doc.view_state_mut().navigate_table_section(path);
    }
}

/// Host the **grouped-sections** Table navigator — the comparison variant of the
/// tree-outline [`render_table_seam`] (selectable as `ActiveView::TableSections`).
///
/// Same central grid + breadcrumb + Back/Forward/Up as the outline view; the left
/// panel instead lists the scanner-detected table sections
/// ([`EditorDocument::cached_table_sections`]) — only the genuinely table-able shapes
/// (uniform record lists, record maps, equal-arity tuple lists) — **grouped by their
/// top-level ancestor** (the first [`PathStep`] of each section's path), each group a
/// collapsible header whose sections are sorted by row count (largest first) and
/// labeled `name (rows×cols)`. Clicking a section selects it (shared
/// `selected_table_section`, so switching between the two Table tabs keeps the same
/// viewed level). Byte-free (FR-020).
pub fn render_table_sections_seam(
    ui: &mut egui::Ui,
    doc: &mut EditorDocument,
    worker: &ReparseWorker,
) {
    render_binding_indicator(ui, doc);

    // Clone the scanned sections out so the borrow on `doc` is released before the
    // mutable view-state writes below (the scan is cached per parse generation).
    let sections = doc.cached_table_sections().to_vec();
    if sections.is_empty() {
        ui.weak(
            "No table-able sections in this document \u{2014} it has no uniform record lists, \
             record maps, or tuple lists. Switch to Tree/form.",
        );
        return;
    }

    // The grid renders the stored selection when it still resolves to a table-able node,
    // else the document root (shared with the outline seam — never empty).
    let stored = doc.view_state().selected_table_section().cloned();
    let selected = stored
        .filter(|p| selection_is_table_able(doc, p))
        .unwrap_or_else(StructuralPath::root);

    // Group sections by top-level ancestor (first path step), preserving first-seen
    // group order; within a group, largest (most rows) first.
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for (idx, section) in sections.iter().enumerate() {
        let key = section
            .path
            .steps()
            .first()
            .map(step_label)
            .unwrap_or_else(|| "(root)".to_string());
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, members)) => members.push(idx),
            None => groups.push((key, vec![idx])),
        }
    }
    for (_, members) in &mut groups {
        members.sort_by(|&a, &b| sections[b].rows.cmp(&sections[a].rows));
    }

    let nav_w = nav_panel_width(
        sections
            .iter()
            .map(|s| {
                text_px_width(
                    ui,
                    &format!("{}  ({}\u{00D7}{})", s.label, s.rows, s.cols),
                    egui::TextStyle::Body,
                )
            })
            .fold(0.0_f32, f32::max)
            + ui.spacing().indent, // section rows sit one level inside the ancestor header
    );
    let mut clicked: Option<StructuralPath> = None;
    egui::Panel::left("ronin_table_sections_nav")
        .resizable(true)
        .default_size(nav_w)
        .show_inside(ui, |ui| {
            ui.strong("Tables");
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                for (group_index, (group_label, members)) in groups.iter().enumerate() {
                    egui::CollapsingHeader::new(group_label)
                        .id_salt(("ronin_tbl_sections_group", group_index, group_label))
                        .default_open(true)
                        .show(ui, |ui| {
                            for &idx in members {
                                let section = &sections[idx];
                                let is_selected = section.path == selected;
                                let label = format!(
                                    "{}  ({}\u{00D7}{})",
                                    section.label, section.rows, section.cols
                                );
                                if ui.selectable_label(is_selected, label).clicked() {
                                    clicked = Some(section.path.clone());
                                }
                            }
                        });
                }
            });
        });

    // Persist a click through `navigate_table_section` so back/forward records it.
    if let Some(path) = clicked {
        doc.view_state_mut().navigate_table_section(path);
        return;
    }

    // On-demand "Combine child" control (E018): when the selected node is a parent
    // map/list of records with repeated child collections, offer to flatten each into
    // one table (the parent key becomes a column). Computed read-only before the panel.
    let combinable = doc
        .parse
        .as_ref()
        .and_then(|p| resolve_path(&p.cst.root(), &selected))
        .map(|node| crate::structural::table::combinable_child_fields(&node))
        .unwrap_or_default();

    // The central area: Back/Forward/Up + breadcrumb, the Combine-child control (when
    // offered), then the selected node projected as a table via `derive_any`.
    let mut breadcrumb_clicked: Option<StructuralPath> = None;
    let mut combine_clicked: Option<StructuralPath> = None;
    egui::CentralPanel::default().show_inside(ui, |ui| {
        render_table_nav_controls(ui, doc);
        render_breadcrumb(ui, doc, &selected, &mut breadcrumb_clicked);
        if !combinable.is_empty() {
            ui.horizontal(|ui| {
                ui.weak("Combine child:");
                for child in &combinable {
                    if ui
                        .button(child.field.as_str())
                        .on_hover_text(format!(
                            "Flatten {} across all entries into one table ({} rows)",
                            child.field, child.rows
                        ))
                        .clicked()
                    {
                        combine_clicked =
                            Some(selected.child(PathStep::CombinedChild(child.field.clone())));
                    }
                }
            });
        }
        ui.separator();
        render_table_view_any(ui, doc, worker, &selected);
    });

    // A Combine click takes priority; either routes through `navigate_table_section`.
    if let Some(path) = combine_clicked.or(breadcrumb_clicked) {
        doc.view_state_mut().navigate_table_section(path);
    }
}

/// One group-by field picker (E021): a `(none)` + column-name combo bound to `sel`.
fn group_field_combo(ui: &mut egui::Ui, id: &str, sel: &mut Option<usize>, col_names: &[String]) {
    let current = sel
        .and_then(|i| col_names.get(i))
        .map(String::as_str)
        .unwrap_or("(none)");
    egui::ComboBox::from_id_salt(id)
        .selected_text(current)
        .show_ui(ui, |ui| {
            ui.selectable_value(sel, None, "(none)");
            for (i, name) in col_names.iter().enumerate() {
                ui.selectable_value(sel, Some(i), name);
            }
        });
}

/// Host the **Table (grouped)** view (E022) — the *superset* table surface. Same tree
/// navigator as Table (outline) (reach any node) + a **Combine child** union dropdown
/// (reach any combined view, like Table (sections)), plus **Group by** and **Show columns**
/// selections that reorganize the chosen collection. The grid is the normal editable one
/// (a transformed model rendered with `row_ops=false`, so edits/selection/virtualization
/// all work). Three composable operations: Combine (union, +rows) → Group by (partition
/// rows) → Show columns (project) — see [`grouped_view_model`](crate::structural::table::grouped_view_model).
pub fn render_table_grouped_seam(
    ui: &mut egui::Ui,
    doc: &mut EditorDocument,
    worker: &ReparseWorker,
) {
    render_binding_indicator(ui, doc);

    // Tree-outline navigator (reaches any node), exactly like Table (outline).
    let Some(tree) = doc.cached_tree_model().cloned() else {
        ui.weak("Parsing\u{2026}");
        return;
    };
    if tree.roots.is_empty() {
        ui.weak("(empty document)");
        return;
    }
    let stored = doc.view_state().selected_table_section().cloned();
    let selected = stored
        .filter(|p| selection_is_table_able(doc, p))
        .unwrap_or_else(StructuralPath::root);

    let nav_w = nav_panel_width(outline_max_label_px(ui, &tree.roots, 0));
    let mut clicked: Option<StructuralPath> = None;
    egui::Panel::left("ronin_table_grouped_nav")
        .resizable(true)
        .default_size(nav_w)
        .show_inside(ui, |ui| {
            ui.strong("Outline");
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                for (i, root) in tree.roots.iter().enumerate() {
                    render_outline_node(ui, root, &selected, 0, i, &mut clicked);
                }
            });
        });
    if let Some(path) = clicked {
        doc.view_state_mut().navigate_table_section(path);
        return;
    }

    // Combine state: split `selected` into its (combine parent, current child) so the
    // Combine dropdown can build/undo a union. The combinable children come from the parent.
    let (combine_parent, current_combined): (StructuralPath, Option<String>) =
        match selected.steps().last() {
            Some(PathStep::CombinedChild(field)) => {
                let steps = selected.steps();
                (
                    StructuralPath::from_steps(steps[..steps.len() - 1].to_vec()),
                    Some(field.clone()),
                )
            }
            _ => (selected.clone(), None),
        };
    let combinable: Vec<String> = doc
        .parse
        .as_ref()
        .and_then(|p| resolve_path(&p.cst.root(), &combine_parent))
        .map(|node| {
            combinable_child_fields(&node)
                .into_iter()
                .map(|c| c.field)
                .collect()
        })
        .unwrap_or_default();

    // Base model (owned, so the doc borrow is released) for the column pickers + transform.
    let base: Option<TableModel> = doc.cached_table_model_any(&selected).cloned();
    let col_names: Vec<String> = base
        .as_ref()
        .map(|m| m.columns.iter().map(|c| c.field_name.clone()).collect())
        .unwrap_or_default();

    let mut breadcrumb_clicked: Option<StructuralPath> = None;
    let mut combine_nav: Option<StructuralPath> = None;
    egui::CentralPanel::default().show_inside(ui, |ui| {
        render_table_nav_controls(ui, doc);
        render_breadcrumb(ui, doc, &selected, &mut breadcrumb_clicked);
        ui.separator();

        // 1) Combine child (union) — only when there's something to combine / un-combine.
        if !combinable.is_empty() || current_combined.is_some() {
            ui.horizontal(|ui| {
                ui.label("Combine child:")
                    .on_hover_text("Union a repeated child collection across all entries (adds rows + a key column)");
                let mut combine_sel = current_combined.clone();
                egui::ComboBox::from_id_salt("ronin_grp_combine")
                    .selected_text(combine_sel.clone().unwrap_or_else(|| "(none)".to_string()))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut combine_sel, None, "(none)");
                        for f in &combinable {
                            ui.selectable_value(&mut combine_sel, Some(f.clone()), f);
                        }
                    });
                if combine_sel != current_combined {
                    combine_nav = Some(match combine_sel {
                        Some(f) => combine_parent.child(PathStep::CombinedChild(f)),
                        None => combine_parent.clone(),
                    });
                }
            });
        }

        if col_names.is_empty() {
            ui.weak("This selection has no columns.");
            return;
        }

        // 2) Group by (partition rows) — up to two levels, bound to view-state `group_by`.
        let mut sel0 = doc.view_state().group_by().first().copied();
        let mut sel1 = doc.view_state().group_by().get(1).copied();
        ui.horizontal(|ui| {
            ui.label("Group by:");
            group_field_combo(ui, "ronin_grp_field_0", &mut sel0, &col_names);
            ui.label("then");
            group_field_combo(ui, "ronin_grp_field_1", &mut sel1, &col_names);
        });
        if sel1 == sel0 {
            sel1 = None; // grouping by the same field twice is redundant
        }
        let group_by: Vec<usize> = [sel0, sel1].into_iter().flatten().collect();
        doc.view_state_mut().set_group_by(group_by.clone());

        // 3) Show columns (project) — toggles; empty = all. Group-by fields always show.
        let mut show: Vec<usize> = doc.view_state().group_show_cols().to_vec();
        ui.horizontal_wrapped(|ui| {
            ui.label("Show:");
            for (c, name) in col_names.iter().enumerate() {
                let is_shown = show.is_empty() || show.contains(&c);
                if ui.selectable_label(is_shown, name).clicked() {
                    if show.is_empty() {
                        show = (0..col_names.len()).collect();
                    }
                    if let Some(pos) = show.iter().position(|&x| x == c) {
                        show.remove(pos);
                    } else {
                        show.push(c);
                    }
                }
            }
        });
        doc.view_state_mut().set_group_show_cols(show.clone());

        ui.separator();

        // The editable grid: a grouped + column-projected transform of the base model,
        // rendered through the normal grid (row_ops=false → edits commit by path).
        if let Some(base) = base.as_ref() {
            let transformed = grouped_view_model(base, &group_by, &show);
            render_table_grid_for(ui, doc, worker, &selected, &transformed, false);
        } else {
            ui.weak("This selection does not project a table.");
        }
    });

    if let Some(path) = combine_nav.or(breadcrumb_clicked) {
        doc.view_state_mut().navigate_table_section(path);
    }
}

/// A readable label for one [`PathStep`] (the grouped-sections navigator's group key):
/// a field / variant-field name verbatim, a map key as `(key)`, an index as `[i]`.
fn step_label(step: &PathStep) -> String {
    match step {
        PathStep::Field(name) | PathStep::VariantField(name) => name.clone(),
        PathStep::Key(text) => format!("({text})"),
        PathStep::Index(i) => format!("[{i}]"),
        PathStep::CombinedChild(field) => format!("\u{2217} {field}"),
    }
}

/// Render the Table view's **Back / Forward / Up** navigation controls (E016).
///
/// Three small buttons — ◀ (back), ▶ (forward), ▲ (up a level) — each enabled only
/// when the move is possible (`can_go_back` / `can_go_forward` / `can_go_up`) and
/// carrying a hover label. Wired to
/// [`table_go_back`](crate::structural::view_state::ViewSelectionAndFocus::table_go_back)
/// / `table_go_forward` / `table_go_up`. After a Back/Forward move, if the resulting
/// selection no longer resolves to a table-able container in the live CST, the move is
/// re-applied (skipping the stale entry) until a resolvable section is reached or the
/// stack empties — so navigation never lands on a blank grid (the seam's
/// default-to-root then shows the root). Byte-free (FR-020).
fn render_table_nav_controls(ui: &mut egui::Ui, doc: &mut EditorDocument) {
    let (can_back, can_forward, can_up) = {
        let vs = doc.view_state();
        (vs.can_go_back(), vs.can_go_forward(), vs.can_go_up())
    };
    ui.horizontal(|ui| {
        if ui
            .add_enabled(can_back, egui::Button::new("\u{25C0}")) // ◀
            .on_hover_text("Back")
            .clicked()
        {
            table_go_back_resolvable(doc);
        }
        if ui
            .add_enabled(can_forward, egui::Button::new("\u{25B6}")) // ▶
            .on_hover_text("Forward")
            .clicked()
        {
            table_go_forward_resolvable(doc);
        }
        if ui
            .add_enabled(can_up, egui::Button::new("\u{25B2}")) // ▲
            .on_hover_text("Up a level")
            .clicked()
        {
            doc.view_state_mut().table_go_up();
        }
    });
}

/// Go back, skipping history entries that no longer resolve to a table-able container
/// against the live CST (E016 robustness): repeatedly pop while the landed selection is
/// stale and more history remains, so Back never lands on a blank grid.
fn table_go_back_resolvable(doc: &mut EditorDocument) {
    loop {
        doc.view_state_mut().table_go_back();
        let landed_ok = match doc.view_state().selected_table_section() {
            Some(path) => selection_is_table_able(doc, path),
            None => true, // back to "no selection" → seam defaults to root (fine).
        };
        if landed_ok || !doc.view_state().can_go_back() {
            break;
        }
    }
}

/// Forward counterpart of [`table_go_back_resolvable`] (E016 robustness).
fn table_go_forward_resolvable(doc: &mut EditorDocument) {
    loop {
        doc.view_state_mut().table_go_forward();
        let landed_ok = match doc.view_state().selected_table_section() {
            Some(path) => selection_is_table_able(doc, path),
            None => true,
        };
        if landed_ok || !doc.view_state().can_go_forward() {
            break;
        }
    }
}

/// `true` when the navigator selection at `path` still resolves to a **table-able**
/// node against the live CST (Part A3): any non-scalar node — a list, map, struct,
/// tuple, or struct-like enum variant — projects a table via `derive_any`; only a
/// scalar leaf (unit / literal) does not. The stored selection is kept iff it is still
/// table-able, else the navigator defaults to the root.
fn selection_is_table_able(doc: &EditorDocument, path: &StructuralPath) -> bool {
    let Some(parse) = doc.parse.as_ref() else {
        return false;
    };
    let root = parse.cst.root();
    // A combined selection (trailing `CombinedChild(field)`) does not resolve to a
    // single node; it is table-able iff its parent prefix resolves to a map/list whose
    // entries still share that child field (E018).
    if let Some(PathStep::CombinedChild(field)) = path.steps().last() {
        let n = path.steps().len();
        let parent = StructuralPath::from_steps(path.steps()[..n - 1].to_vec());
        return resolve_path(&root, &parent).is_some_and(|node| {
            crate::structural::table::combinable_child_fields(&node)
                .iter()
                .any(|c| &c.field == field)
        });
    }
    matches!(
        resolve_path(&root, path).and_then(ronin_core::ast::Value::cast),
        Some(
            ronin_core::ast::Value::List(_)
                | ronin_core::ast::Value::Map(_)
                | ronin_core::ast::Value::Struct(_)
                | ronin_core::ast::Value::Tuple(_)
                | ronin_core::ast::Value::EnumVariant(_)
        )
    )
}

/// `true` when `node` is an **outline container** the navigator lists (Part A1): a
/// collection-kind node (struct / map / list / tuple / struct-like enum variant) that
/// has children. A scalar leaf — and a childless container such as a bare enum variant
/// (`Fast`), empty struct/list/map, or `()` — is skipped (treated as a leaf): the
/// outline lists only nodes worth opening as a table.
fn is_outline_container(node: &TreeNode) -> bool {
    matches!(
        node.kind,
        TreeNodeKind::Struct
            | TreeNodeKind::Map
            | TreeNodeKind::List
            | TreeNodeKind::Tuple
            | TreeNodeKind::EnumVariant
    ) && !node.children.is_empty()
}

/// Recursively render one outline node + (collapsibly) its container children (Part A1).
///
/// A container node renders as an [`egui::CollapsingHeader`] whose header is a
/// **selectable** row: the node's [`TypeIndicator`](indicators::TypeIndicator) icon +
/// its tree label + its child count. Clicking the row selects the node (byte-free —
/// `selected_table_section = node.node_ref`). Children that are themselves containers
/// are nested inside the header to mirror the hierarchy; scalar leaf children are
/// skipped ([`is_outline_container`]). The root + first level default open; deeper
/// levels default collapsed so the outline is reasonably collapsed by default.
fn render_outline_node(
    ui: &mut egui::Ui,
    node: &TreeNode,
    selected: &StructuralPath,
    depth: usize,
    sibling_index: usize,
    clicked: &mut Option<StructuralPath>,
) {
    // Skip scalar leaf nodes — the outline lists only container nodes (Part A1).
    if !is_outline_container(node) {
        return;
    }

    let indicator = indicators::from_tree_kind(node.kind);
    let is_selected = node.node_ref == *selected;
    let child_count = node
        .children
        .iter()
        .filter(|c| is_outline_container(c))
        .count();

    // The collapsing-header id is keyed by the node's full path + sibling index so it
    // is stable across reparse and never collides between unrelated subtrees.
    let id = egui::Id::new(("ronin_outline", node.node_ref.steps().len(), sibling_index))
        .with(&node.node_ref);
    egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, depth < 2)
        .show_header(ui, |ui| {
            // The header is a selectable row: the type icon in the shared fixed-width
            // slot (E014 — aligns vertically across outline rows) then a selectable
            // label carrying the text only (+ a child-container count), so the glyph is
            // never embedded in the label string.
            ui.horizontal(|ui| {
                indicator.show(ui).on_hover_text(indicator.word());
                let label = if child_count > 0 {
                    format!("{}  ({child_count})", node.label)
                } else {
                    node.label.clone()
                };
                if ui.selectable_label(is_selected, label).clicked() {
                    *clicked = Some(node.node_ref.clone());
                }
            });
        })
        .body(|ui| {
            for (i, child) in node.children.iter().enumerate() {
                render_outline_node(ui, child, selected, depth + 1, i, clicked);
            }
        });
}

/// Render the stateless, path-derived breadcrumb above the grid (Part A3): one segment
/// per prefix of `selected`, each a clickable button iff its prefix resolves to a
/// List/Map (clicking re-selects that prefix), otherwise shown weak / non-clickable.
fn render_breadcrumb(
    ui: &mut egui::Ui,
    doc: &EditorDocument,
    selected: &StructuralPath,
    clicked: &mut Option<StructuralPath>,
) {
    let Some(parse) = doc.parse.as_ref() else {
        return;
    };
    let segments = breadcrumb_segments(&parse.cst, selected);
    ui.horizontal_wrapped(|ui| {
        for (i, seg) in segments.iter().enumerate() {
            if i > 0 {
                ui.weak("\u{25B8}"); // U+25B8 separator
            }
            if seg.clickable && seg.path != *selected {
                if ui.button(&seg.label).clicked() {
                    *clicked = Some(seg.path.clone());
                }
            } else {
                // The current segment (or a non-openable ancestor) is non-clickable.
                ui.weak(&seg.label);
            }
        }
    });
}

/// Render the active diagnostics-panel region.
///
/// Lists the supplied [`DiagnosticView`]s (already projected into editor
/// coordinates) one per row: severity, code, `line:column`, and message. An
/// empty list shows a faint "No problems" state. This is the live seam — later
/// waves replace the row rendering with a richer, navigable problems panel.
pub fn render_diagnostics_seam(ui: &mut egui::Ui, diagnostics: &[DiagnosticView]) {
    if diagnostics.is_empty() {
        ui.weak("No problems");
        return;
    }
    for d in diagnostics {
        let (line, col) = d.line_col.0;
        // Lines/columns are zero-based internally; present them one-based.
        ui.label(format!(
            "{} {} [{}:{}] {}",
            d.severity,
            d.code,
            line + 1,
            col + 1,
            d.message
        ));
    }
}

/// Reserved seam for the **E008** structural tree/table views.
///
/// Renders a faint, disabled placeholder. Replace the body in E008 to mount the
/// tree/table widgets here without touching shell-core layout.
pub fn tree_table_seam_stub(ui: &mut egui::Ui) {
    ui.add_enabled_ui(false, |ui| {
        ui.weak("Structure (coming soon)");
    });
}

/// Reserved seam for the **E009** Bevy mode selector.
///
/// Renders a faint, disabled placeholder. Replace the body in E009 to mount the
/// mode selector here without touching shell-core layout.
pub fn mode_selector_seam_stub(ui: &mut egui::Ui) {
    ui.add_enabled_ui(false, |ui| {
        ui.weak("Mode (coming soon)");
    });
}
