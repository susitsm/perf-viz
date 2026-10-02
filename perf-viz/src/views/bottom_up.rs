//! The bottom-up tab: functions by self cost, each expanding into its callers.

use std::collections::HashSet;

use eframe::egui::{self};
use perf_callgraph::model::ROOT;

use super::{
    BuRow, FuncId, RowsKey, ViewContext, ViewData, noun,
    table::{self, Arrow, Click, Columns, RowData},
};
use crate::views;

/// State of the bottom-up tree. Rows are identified by the function path from the first level.
#[derive(Default)]
pub(crate) struct BuState {
    expanded: HashSet<Vec<FuncId>>,
    rows: Vec<BuRow>,
    rows_key: Option<RowsKey>,
    version: u64,
    selected: Option<Vec<FuncId>>,
}

/// The bottom-up tree: functions ranked by self cost; expanding one shows who called it.
pub(crate) fn draw_bottom_up(
    ui: &mut egui::Ui,
    context: &ViewContext<'_>,
    data: &ViewData<'_>,
    state: &mut BuState,
) {
    let ViewContext {
        profile,
        trie,
        view,
        cost,
        ..
    } = *context;
    let ViewData {
        funcs,
        totals,
        selfs,
        query,
        version,
        ..
    } = *data;
    ui.horizontal(|ui| {
        if ui.button("Collapse all").clicked() {
            state.expanded.clear();
            state.version += 1;
        }
        ui.weak(format!("{} rows shown; each function's cost is the {} that end in it; expand to see its callers", state.rows.len(), noun(profile, cost)));
    });
    let key = RowsKey {
        view,
        data_version: version,
        expansion_version: state.version,
    };
    if state.rows_key != Some(key) {
        state.rows = views::bu_rows(trie, funcs, selfs, &state.expanded);
        state.rows_key = Some(key);
    }
    let rows = &state.rows;
    let selected = state.selected.clone();
    let action = table::draw(
        ui,
        &Columns {
            id: "bottom_up_table",
            unit: cost,
            name: "Function",
            cost: "Cost",
            self_cost: None,
            sort: None,
        },
        rows.len(),
        totals[ROOT],
        |i| {
            let row = &rows[i];
            let func = &funcs.funcs[row.func];
            RowData {
                depth: row.path.len() as u32 - 1,
                arrow: if !row.has_callers {
                    Arrow::None
                } else if row.expanded {
                    Arrow::Expanded
                } else {
                    Arrow::Collapsed
                },
                name: &func.name,
                module: profile.module_name(func.module),
                cost: row.cost,
                self_cost: None,
                highlight: !query.is_empty() && func.name.to_lowercase().contains(query),
                suspect: false,
                selected: selected.as_ref() == Some(&row.path),
            }
        },
    );
    match action {
        Some(Click::Toggle(i)) => {
            let path = state.rows[i].path.clone();
            if !state.expanded.remove(&path) {
                state.expanded.insert(path);
            }
            state.version += 1;
        }
        Some(Click::Select(i)) => state.selected = Some(state.rows[i].path.clone()),
        Some(Click::SortBy(_)) | None => {}
    }
}
