//! The top-down tab: the call tree from the roots, heaviest first.

use std::collections::HashSet;

use eframe::egui::{self};
use perf_callgraph::model::{Profile, ROOT, Trie};

use super::{
    RowsKey, TdRow,
    table::{self, Arrow, Click, Columns, RowData},
};
use crate::{fmt::Cost, views};

/// State of the top-down tree. Node ids differ between the full and truncated tries, so per view.
#[derive(Default)]
pub(crate) struct TdState {
    expanded: [HashSet<u32>; 2],
    /// The user (or the first draw) has set this view's expansion.
    touched: [bool; 2],
    rows: Vec<TdRow>,
    rows_key: Option<RowsKey>,
    /// Bumped whenever `expanded` changes.
    version: u64,
    selected: [Option<u32>; 2],
}

/// The top-down call tree: every frame with inclusive and self cost, heaviest first.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_top_down(
    ui: &mut egui::Ui,
    profile: &Profile,
    trie: &Trie,
    totals: &[u64],
    selfs: &[u64],
    cost: Cost,
    matches: &[bool],
    searching: bool,
    state: &mut TdState,
    view: usize,
    version: u64,
) {
    // Until the user has chosen, show the hottest path expanded, as a useful starting point.
    if !state.touched[view] {
        state.expanded[view] = views::hottest_path(trie, totals, 0.3).into_iter().collect();
        state.touched[view] = true;
        state.version += 1;
    }
    ui.horizontal(|ui| {
        if ui.button("Collapse all").clicked() {
            state.expanded[view].clear();
            state.version += 1;
        }
        if ui.button("Expand hottest path").clicked() {
            state.expanded[view] = views::hottest_path(trie, totals, 0.3).into_iter().collect();
            state.version += 1;
        }
        if ui.button("Expand 6 levels").clicked() {
            state.expanded[view] = views::expand_to_depth(trie, totals, 6);
            state.version += 1;
        }
        ui.weak(format!(
            "{} rows shown; click the arrow (or double-click) to expand",
            state.rows.len()
        ));
    });
    let key = RowsKey {
        view,
        data_version: version,
        expansion_version: state.version,
    };
    if state.rows_key != Some(key) {
        state.rows = views::top_down_rows(trie, totals, &state.expanded[view]);
        state.rows_key = Some(key);
    }
    let selected = state.selected[view];
    let rows = &state.rows;
    let action = table::draw(
        ui,
        &Columns {
            unit: cost,
            name: "Function",
            cost: "Inclusive",
            self_cost: Some("Self"),
            sort: None,
        },
        rows.len(),
        totals[ROOT as usize],
        |i| {
            let row = rows[i];
            let node = trie.nodes[row.node as usize];
            let frame = &profile.frames[node.frame as usize];
            RowData {
                depth: row.depth,
                arrow: if !row.expandable {
                    Arrow::None
                } else if row.expanded {
                    Arrow::Expanded
                } else {
                    Arrow::Collapsed
                },
                name: &frame.name,
                module: profile.module_name(frame.module),
                cost: totals[row.node as usize],
                self_cost: Some(selfs[row.node as usize]),
                highlight: searching && matches[node.frame as usize],
                suspect: frame.suspect,
                selected: selected == Some(row.node),
            }
        },
    );
    match action {
        Some(Click::Toggle(i)) => {
            let node = state.rows[i].node;
            if !state.expanded[view].remove(&node) {
                state.expanded[view].insert(node);
            }
            state.version += 1;
        }
        Some(Click::Select(i)) => state.selected[view] = Some(state.rows[i].node),
        Some(Click::SortBy(_)) | None => {}
    }
}
