//! The summary tab: facts about the recording and its hottest functions and modules.

use std::collections::HashSet;

use eframe::egui::{self, RichText, vec2};
use perf_callgraph::model::{ModuleId, Profile, ROOT};

use super::{
    FuncId, Functions, ViewContext, Weighted, func_row, noun,
    table::{self, Arrow, Click, Columns, RowData},
};
use crate::{
    App, Tab,
    fmt::{self, Cost},
    views,
};

/// Facts about the whole recording, computed once.
#[derive(Default)]
pub(crate) struct SummaryInfo {
    processes: usize,
    threads: usize,
    /// Time of the first and last sample, in nanoseconds.
    first_ns: u64,
    last_ns: u64,
    /// How the load went.
    pub(crate) load: Option<LoadInfo>,
}

/// How a load went.
#[derive(Clone, Copy)]
pub(crate) struct LoadInfo {
    pub(crate) file_bytes: u64,
    /// Seconds spent reading (and unwinding) the file.
    pub(crate) read_secs: f32,
    /// Seconds for the whole load, with symbols and call trees.
    pub(crate) total_secs: f32,
}

impl SummaryInfo {
    pub(crate) fn new(profile: &Profile) -> Self {
        let pids: HashSet<i32> = profile.samples.iter().map(|sample| sample.pid).collect();
        let tids: HashSet<i32> = profile.samples.iter().map(|sample| sample.tid).collect();
        Self {
            processes: pids.len(),
            threads: tids.len(),
            first_ns: profile.samples.first().map_or(0, |sample| sample.time),
            last_ns: profile.samples.last().map_or(0, |sample| sample.time),
            load: None,
        }
    }
}

/// The cost the summary covers, and whether a filter narrowed it from the whole recording.
pub(crate) struct SummaryScope {
    total: u64,
    grand_total: u64,
    filtered: bool,
}

/// An overview: what the recording is, the hottest functions and modules. Returns a function to
/// show in the caller/callee view if one of the hotspots was clicked.
pub(crate) fn draw_summary(
    ui: &mut egui::Ui,
    profile: &Profile,
    funcs: &Functions,
    costs: &views::Costs,
    cost: Cost,
    scope: SummaryScope,
    info: &SummaryInfo,
) -> Option<FuncId> {
    let SummaryScope {
        total,
        grand_total,
        filtered,
    } = scope;
    let mut nav = None;
    let state = &profile.stats;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(6.0);
        ui.heading("Summary");
        ui.add_space(4.0);
        egui::Grid::new("summary_grid").num_columns(2).spacing([28.0, 4.0]).show(ui, |ui| {
            let mut row = |k: &str, value: String| {
                ui.label(RichText::new(k).weak());
                ui.label(value);
                ui.end_row();
            };
            let with_stacks = state.samples - state.samples_without_stack;
            let unit = noun(profile, cost);
            let _ = with_stacks;
            row(if cost == Cost::Samples { "Samples" } else { "Cost" }, if !filtered { format!("{} {unit}", cost.format(total)) } else { format!("{} {unit} in the current filter (process, thread or time range), of {} in the recording", cost.format(total), cost.format(grand_total)) });
            if let Some(load) = info.load {
                let (bytes, read_s, total_s) = (load.file_bytes, load.read_secs, load.total_secs);
                let gb = bytes as f64 / 1e9;
                let size = if gb >= 1.0 { format!("{} GB", fmt::sig3(gb)) } else { format!("{} MB", fmt::sig3(bytes as f64 / 1e6)) };
                row("Read speed", format!("{size} read and unwound in {} s: {} GB/s (whole load, with symbols and call trees: {} s)", fmt::sig3(read_s as f64), fmt::sig3(gb / read_s.max(1e-6) as f64), fmt::sig3(total_s as f64)));
            }
            row("Recording", format!("{:.2} s, {} processes, {} threads", info.last_ns.saturating_sub(info.first_ns) as f64 / 1e9, info.processes, info.threads));
            if state.lost_events > 0 {
                row("Kernel drops", format!("{} events in {} LOST records ({}% of everything sampled): the profile under-counts", state.lost_events, state.lost_records, fmt::pct_of(state.lost_events as f64, (state.samples + state.lost_events) as f64)));
            }
            row("Suspect stack tails", format!("{} samples ({}%) have frames past the first invalid return address", state.suspect_samples, fmt::pct_of(state.suspect_samples as f64, state.samples as f64)));
            row("Symbols", format!("{} modules with DWARF, {} with ELF symbols only, {} without; {} of {} addresses unresolved ({}%)", state.modules_full, state.modules_symbols_only, state.modules_raw, state.unresolved_addresses, state.unique_addresses, fmt::pct_of(state.unresolved_addresses as f64, state.unique_addresses as f64)));
            row("Call tree", format!("{} functions, {} nodes (full), {} (truncated)", funcs.funcs.len(), profile.full.nodes.len(), profile.truncated.nodes.len()));
        });
        ui.add_space(12.0);
        ui.strong("Top hotspots by self cost (click one to see its callers and callees)");
        let top: Vec<FuncId> = views::ranked_functions(costs, funcs, views::SortBy::Self_, "").into_iter().take(12).collect();
        let height = (top.len() + 1) as f32 * table::ROW_H + 6.0;
        let click = ui
            .allocate_ui(vec2(ui.available_width(), height), |ui| table::draw(ui, &Columns { unit: cost, name: "Function", cost: "Self", self_cost: Some("Inclusive"), sort: None }, top.len(), total, |i| func_row(profile, funcs, top[i], costs.self_[top[i]], Some(costs.inclusive[top[i]]), false)))
            .inner;
        if let Some(Click::Select(i) | Click::Toggle(i)) = click {
            nav = Some(top[i]);
        }
        ui.add_space(12.0);
        ui.strong("Self cost by module");
        let mods: Vec<Weighted<Option<ModuleId>>> = views::module_self_costs(funcs, costs).into_iter().take(10).collect();
        let height = (mods.len() + 1) as f32 * table::ROW_H + 6.0;
        ui.allocate_ui(vec2(ui.available_width(), height), |ui| {
            table::draw(ui, &Columns { unit: cost, name: "Module", cost: "Self", self_cost: None, sort: None }, mods.len(), total, |i| RowData {
                depth: 0,
                arrow: Arrow::None,
                name: profile.module_name(mods[i].id),
                module: "",
                cost: mods[i].cost,
                self_cost: None,
                highlight: false,
                suspect: false,
                selected: false,
            })
        });
    });
    nav
}

impl App {
    pub(crate) fn draw_summary_tab(&mut self, ui: &mut egui::Ui, view_context: &ViewContext<'_>) {
        let scope = SummaryScope {
            total: self.totals[ROOT],
            grand_total: self.grand_total,
            filtered: view_context.filtered,
        };
        let funcs = self.funcs.as_ref().expect("set when the profile arrived");
        if let Some(function) = draw_summary(
            ui,
            view_context.profile,
            funcs,
            &self.costs,
            view_context.cost,
            scope,
            &self.summary,
        ) {
            self.cc.selected = Some(function);
            self.tab = Tab::CallerCallee;
        }
    }
}
