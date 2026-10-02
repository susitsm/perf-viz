//! The caller/callee tab: every function with its costs; the selected one lists its callers and callees.

use eframe::egui::{self};
use perf_callgraph::model::{Profile, ROOT, Trie};

use super::{
    FuncId, Functions, NodeCounts, ViewContext, ViewData, Weighted, func_row, noun,
    table::{self, Click, Columns},
};
use crate::{fmt::Cost, views};

/// State of the caller/callee view.
pub(crate) struct CcState {
    pub(crate) selected: Option<FuncId>,
    sort: views::SortBy,
    list: Vec<FuncId>,
    list_key: Option<ListKey>,
    /// Callers and callees of the selected function.
    lists: Option<CallLists>,
}

/// What the ranked function list was built for.
#[derive(Clone, PartialEq)]
pub(crate) struct ListKey {
    version: u64,
    sort: views::SortBy,
    query: String,
}

/// The callers and callees of one function, for the view version they were built for.
pub(crate) struct CallLists {
    func: FuncId,
    version: u64,
    callers: Vec<Weighted<FuncId>>,
    callees: Vec<Weighted<FuncId>>,
}

impl Default for CcState {
    fn default() -> Self {
        Self {
            selected: None,
            sort: views::SortBy::Inclusive,
            list: Vec::new(),
            list_key: None,
            lists: None,
        }
    }
}

pub(crate) fn refresh_call_lists(
    state: &mut CcState,
    trie: &Trie,
    totals: &NodeCounts,
    funcs: &Functions,
    version: u64,
) {
    if let Some(func) = state.selected
        && !state
            .lists
            .as_ref()
            .is_some_and(|lists| lists.func == func && lists.version == version)
    {
        state.lists = Some(CallLists {
            func,
            version,
            callers: views::callers_of(trie, totals, funcs, func),
            callees: views::callees_of(trie, totals, funcs, func),
        });
    }
}

/// One side of the call lists: a table of related functions; returns the one clicked.
pub(crate) fn draw_related_functions(
    ui: &mut egui::Ui,
    profile: &Profile,
    funcs: &Functions,
    (title, name): (&str, &str),
    related: &[Weighted<FuncId>],
    (cost, inclusive): (Cost, u64),
) -> Option<FuncId> {
    ui.strong(title);
    let click = table::draw(
        ui,
        &Columns {
            unit: cost,
            name,
            cost: "Cost",
            self_cost: None,
            sort: None,
        },
        related.len(),
        inclusive,
        |i| func_row(profile, funcs, related[i].id, related[i].cost, None, false),
    );
    match click {
        Some(Click::Select(i) | Click::Toggle(i)) => Some(related[i].id),
        _ => None,
    }
}

/// The callers and callees of the selected function, in a panel below the function list.
/// Returns the function clicked in either list.
pub(crate) fn draw_call_lists_panel(
    ui: &mut egui::Ui,
    profile: &Profile,
    funcs: &Functions,
    costs: &views::Costs,
    cost: Cost,
    state: &CcState,
) -> Option<FuncId> {
    let mut next = None;
    egui::Panel::bottom("cc_lists")
        .resizable(true)
        .default_size(250.0)
        .min_size(100.0)
        .show(ui, |ui| {
            let (Some(func), Some(lists)) = (&state.selected, &state.lists) else {
                ui.add_space(8.0);
                ui.weak("Select a function above to see who calls it and what it calls.");
                return;
            };
            let inclusive = costs.inclusive[*func];
            ui.horizontal(|ui| {
                ui.strong(funcs.funcs[*func].name.chars().take(90).collect::<String>());
                ui.weak(format!(
                    "inclusive {}, self {} {}; percentages are of the inclusive cost",
                    cost.format(inclusive),
                    cost.format(costs.self_[*func]),
                    noun(profile, cost)
                ));
            });
            ui.columns(2, |columns| {
                let scale = (cost, inclusive);
                let callers = ("Callers (click to select)", "Caller");
                let callees = ("Callees (click to select)", "Callee");
                let clicked_caller = draw_related_functions(
                    &mut columns[0],
                    profile,
                    funcs,
                    callers,
                    &lists.callers,
                    scale,
                );
                let clicked_callee = draw_related_functions(
                    &mut columns[1],
                    profile,
                    funcs,
                    callees,
                    &lists.callees,
                    scale,
                );
                next = clicked_callee.or(clicked_caller);
            });
        });
    next
}

/// Every function with its inclusive and self cost; selecting one lists its callers and callees below.
pub(crate) fn draw_caller_callee(
    ui: &mut egui::Ui,
    context: &ViewContext<'_>,
    data: &ViewData<'_>,
    state: &mut CcState,
) {
    let ViewContext {
        profile,
        trie,
        cost,
        ..
    } = *context;
    let ViewData {
        funcs,
        totals,
        costs,
        query,
        version,
        ..
    } = *data;
    refresh_call_lists(state, trie, totals, funcs, version);
    let mut next = draw_call_lists_panel(ui, profile, funcs, costs, cost, state);

    ui.horizontal(|ui| {
        ui.weak(format!("{} functions; click the Inclusive or Self header to sort; the search box filters by name", state.list.len()));
    });
    let key = ListKey {
        version,
        sort: state.sort,
        query: query.to_string(),
    };
    if state.list_key.as_ref() != Some(&key) {
        state.list = views::ranked_functions(costs, funcs, state.sort, query);
        state.list_key = Some(key);
    }
    let (list, selected) = (&state.list, state.selected);
    let click = table::draw(
        ui,
        &Columns {
            unit: cost,
            name: "Function",
            cost: "Inclusive",
            self_cost: Some("Self"),
            sort: Some(if state.sort == views::SortBy::Inclusive {
                0
            } else {
                1
            }),
        },
        list.len(),
        totals[ROOT],
        |i| {
            let func = list[i];
            func_row(
                profile,
                funcs,
                func,
                costs.inclusive[func],
                Some(costs.self_[func]),
                selected == Some(func),
            )
        },
    );
    match click {
        Some(Click::Select(i) | Click::Toggle(i)) => next = Some(state.list[i]),
        Some(Click::SortBy(col)) => {
            state.sort = if col == 0 {
                views::SortBy::Inclusive
            } else {
                views::SortBy::Self_
            }
        }
        None => {}
    }
    if next.is_some() {
        state.selected = next;
    }
}
