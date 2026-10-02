#![cfg_attr(doc, doc = ::embed_doc_image::embed_image!("loading", "docs/images/loading.png"))]
#![cfg_attr(doc, doc = ::embed_doc_image::embed_image!("summary", "docs/images/summary.png"))]
#![cfg_attr(doc, doc = ::embed_doc_image::embed_image!("flame", "docs/images/flame.png"))]
#![cfg_attr(doc, doc = ::embed_doc_image::embed_image!("prompt", "docs/images/prompt.png"))]
#![cfg_attr(doc, doc = ::embed_doc_image::embed_image!("tuning", "docs/images/tuning.png"))]
#![doc = include_str!("../../README.md")]

// Only the doc attributes above use it, and only when building the docs.
#[cfg(not(doc))]
use embed_doc_image as _;

mod cli;
mod config;
mod filter;
mod fmt;
mod harness;
mod theme;
mod tuner;
mod views;

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use crate::{
    cli::{Cli, MsRange},
    config::{Config, Dwarf, ReaderConfig, TabName, Theme},
    filter::{Coverage, Filter, ProcRow},
    fmt::Cost,
    harness::Harness,
    theme::Pal,
    tuner::Tuner,
    views::{
        Functions, NodeCounts, ViewContext, ViewData,
        bottom_up::BuState,
        caller_callee::CcState,
        flame::{FlameScroll, ItemsKey},
        flame_layout::Item,
        noun,
        summary::{LoadInfo, SummaryInfo},
        timeline::{self, TimeRange, TimelineCache, XDrag},
        top_down::TdState,
    },
};
use eframe::egui::{self, Align2, Color32, FontId, RichText, Sense, Stroke, StrokeKind, vec2};
use perf_callgraph::{
    index::IndexVec,
    load,
    model::{FrameId, NodeId, Profile, ROOT, Trie},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    Summary,
    FlameGraph,
    TopDown,
    BottomUp,
    CallerCallee,
    Timeline,
}

impl Tab {
    fn name(self) -> TabName {
        match self {
            Tab::Summary => TabName::Summary,
            Tab::FlameGraph => TabName::Flame,
            Tab::TopDown => TabName::Topdown,
            Tab::BottomUp => TabName::Bottomup,
            Tab::CallerCallee => TabName::Callercallee,
            Tab::Timeline => TabName::Timeline,
        }
    }

    fn from_name(name: TabName) -> Tab {
        match name {
            TabName::Summary => Tab::Summary,
            TabName::Flame => Tab::FlameGraph,
            TabName::Topdown => Tab::TopDown,
            TabName::Bottomup => Tab::BottomUp,
            TabName::Callercallee => Tab::CallerCallee,
            TabName::Timeline => Tab::Timeline,
        }
    }
}

fn theme_pref(theme: Theme) -> egui::ThemePreference {
    match theme {
        Theme::System => egui::ThemePreference::System,
        Theme::Light => egui::ThemePreference::Light,
        Theme::Dark => egui::ThemePreference::Dark,
    }
}

fn theme_setting(preference: egui::ThemePreference) -> Theme {
    match preference {
        egui::ThemePreference::System => Theme::System,
        egui::ThemePreference::Light => Theme::Light,
        egui::ThemePreference::Dark => Theme::Dark,
    }
}

const TABS: [(Tab, &str); 6] = [
    (Tab::Summary, "Summary"),
    (Tab::FlameGraph, "Flame graph"),
    (Tab::TopDown, "Top-down"),
    (Tab::BottomUp, "Bottom-up"),
    (Tab::CallerCallee, "Caller/callee"),
    (Tab::Timeline, "Timeline"),
];

const MAX_LIST: usize = 80;

/// The loader's phases, in order, with the names it reports and how to show them.
const PHASES: [(&str, &str); 4] = [
    (
        "reading and unwinding",
        "Reading records and unwinding stacks",
    ),
    ("merging", "Merging the workers' results"),
    ("symbolizing", "Resolving symbols"),
    ("building call trees", "Building call trees"),
];

/// One phase of the load, as the loader reported it.
struct Phase {
    name: String,
    start: Instant,
    /// Unset while the phase is running.
    end: Option<Instant>,
}

#[derive(Default)]
struct LoadLog {
    entries: Vec<Phase>,
}

impl LoadLog {
    fn begin(&mut self, name: &str) {
        self.finish();
        self.entries.push(Phase {
            name: name.to_string(),
            start: Instant::now(),
            end: None,
        });
    }
    fn finish(&mut self) {
        if let Some(last) = self.entries.last_mut() {
            last.end.get_or_insert_with(Instant::now);
        }
    }
}

#[derive(Clone)]
struct Args {
    /// The saved settings (the flags below start from them and override them).
    config: Config,
    /// The file to open at startup; none: start on the welcome screen.
    path: Option<PathBuf>,
    truncated: bool,
    /// `--cost`; by default cycles when the recording has periods.
    cost: Option<Cost>,
    theme: Option<egui::ThemePreference>,
    tab: Tab,
    cold: bool,
    load_options: load::Options,
    process: Option<i32>,
    thread: Option<i32>,
    search: String,
    /// Preset time range, in milliseconds after the first sample.
    range_ms: Option<MsRange>,
    /// Scripted input and screenshots (does nothing without the `scripted` feature).
    harness: Harness,
}

/// What the flame graph's totals were computed for.
#[derive(Clone, Copy, PartialEq)]
struct TotalsKey {
    truncated: bool,
    filter: Filter,
    range: Option<TimeRange>,
    cost: Cost,
}

/// The search text and totals the matches were computed for.
#[derive(Clone, PartialEq, Eq)]
struct SearchKey {
    query: String,
    totals_version: u64,
}

struct App {
    /// The file being loaded or shown; none on the welcome screen.
    path: Option<PathBuf>,
    /// The startup settings, reused when another file is opened.
    base: Args,
    /// A file dialog in progress (it runs on its own thread).
    picker: Option<mpsc::Receiver<Option<PathBuf>>>,
    /// The reader tuner window (kept when another file is opened).
    tuner: Tuner,
    /// The file is chosen but its load waits for the answer to the "tune first?" question.
    pending_load: bool,
    /// Frames drawn so far (only to report startup timing).
    frames_total: u32,
    /// "Tune now" was chosen for the pending load.
    tune_first: bool,
    file_size: u64,
    receiver: Option<mpsc::Receiver<Result<Profile, String>>>,
    log: Arc<Mutex<LoadLog>>,
    bytes_read: Arc<AtomicU64>,
    load_started: Instant,
    profile: Option<Arc<Profile>>,
    error: Option<String>,

    truncated: bool,
    tab: Tab,
    /// Focused (zoomed) node, per view: node ids differ between the two tries.
    focus: [NodeId; 2],

    funcs: Option<Functions>,
    /// Samples ending at each node of the current view's trie (see `views::self_counts`).
    selfs: NodeCounts,
    /// Self and inclusive cost per function, for the current view and filter.
    costs: views::Costs,
    summary: SummaryInfo,
    td: TdState,
    bu: BuState,
    cc: CcState,

    // Time range selection (absolute sample timestamps, `[start, end)`) and the timeline data
    range: Option<TimeRange>,
    pending_range_ms: Option<MsRange>,
    /// A selection being dragged, as x coordinates.
    drag: Option<XDrag>,
    timeline_data: Option<TimelineCache>,

    sel_pid: Option<i32>,
    sel_tid: Option<i32>,
    proc_rows: Vec<ProcRow>,
    proc_query: String,
    totals: NodeCounts,
    totals_key: Option<TotalsKey>,
    /// What the views add up, and the total of the whole recording in that cost.
    cost: Cost,
    grand_total: u64,
    totals_version: u64,
    /// The user (or `--cost`) chose the cost, so the loader's default must not override it.
    /// The saved settings; written back when the user changes one.
    config: Config,
    /// The tab last written to the config, so a tab given on the command line isn't saved.
    saved_tab: Tab,

    search: String,
    search_applied: Option<SearchKey>,
    matches: IndexVec<FrameId, bool>,
    coverage: Coverage,

    items: Vec<Item>,
    items_key: Option<ItemsKey>,
    /// Where the flame graph is scrolled to.
    flame_scroll: FlameScroll,
    /// When Escape was last pressed in the flame graph (egui time, seconds).
    last_escape: Option<f64>,

    harness: Harness,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, args: Args) -> Self {
        timing("window and renderer ready");
        if let Some(theme) = args.theme {
            cc.egui_ctx.set_theme(theme);
        }
        args.harness.configure(&cc.egui_ctx);
        Self::from_args(&cc.egui_ctx, args)
    }

    /// Open another file, keeping the settings in effect now (view, cost, tab, saved config).
    fn open(&mut self, ctx: &egui::Context, path: PathBuf) {
        let mut args = self.base.clone();
        args.path = Some(path);
        args.truncated = self.truncated;
        args.cost = Some(self.cost);
        args.tab = self.tab;
        args.config = self.config.clone();
        args.harness = self.harness.after_open();
        let tuner = std::mem::take(&mut self.tuner);
        *self = Self::from_args(ctx, args);
        self.tuner = tuner;
    }

    /// Show a file dialog without blocking the UI; the chosen file arrives through `picker`.
    fn pick_file(&mut self, ctx: &egui::Context) {
        if self.picker.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let picked = rfd::FileDialog::new()
                .set_title("Open a perf.data file")
                .pick_file();
            let _ = sender.send(picked);
            ctx.request_repaint();
        });
        self.picker = Some(receiver);
    }

    fn from_args(ctx: &egui::Context, args: Args) -> Self {
        let base = Args {
            path: None,
            process: None,
            thread: None,
            search: String::new(),
            range_ms: None,
            // A unit struct unless the `scripted` feature is on.
            #[allow(clippy::default_constructed_unit_structs)]
            harness: Harness::default(),
            ..args.clone()
        };
        let log = Arc::new(Mutex::new(LoadLog::default()));
        let bytes_read = Arc::new(AtomicU64::new(0));
        let file_size = args
            .path
            .as_ref()
            .and_then(|path| std::fs::metadata(path).ok())
            .map_or(0, |metadata| metadata.len());
        // The first time a file is opened, offer to tune the reader before the load starts.
        let ask_to_tune = args.path.is_some()
            && args.config.tuning_prompted != Some(true)
            && args.config.reader.is_empty_settings();
        let mut app = Self {
            file_size,
            path: args.path.clone(),
            base,
            picker: None,
            tuner: Tuner::default(),
            receiver: None,
            pending_load: ask_to_tune,
            frames_total: 0,
            tune_first: false,
            log,
            bytes_read,
            load_started: Instant::now(),
            profile: None,
            error: None,
            truncated: args.truncated,
            tab: args.tab,
            focus: [ROOT; 2],
            funcs: None,
            selfs: NodeCounts::new(),
            costs: views::Costs {
                self_: IndexVec::new(),
                inclusive: IndexVec::new(),
            },
            summary: SummaryInfo::default(),
            td: TdState::default(),
            bu: BuState::default(),
            cc: CcState::default(),
            range: None,
            pending_range_ms: args.range_ms,
            drag: None,
            timeline_data: None,
            sel_pid: args.process,
            sel_tid: args.thread,
            proc_rows: Vec::new(),
            proc_query: String::new(),
            totals: NodeCounts::new(),
            totals_key: None,
            cost: args.cost.unwrap_or_default(),
            grand_total: 0,
            config: args.config,
            saved_tab: args.tab,
            totals_version: 0,
            search: args.search,
            search_applied: None,
            matches: IndexVec::new(),
            coverage: Coverage::default(),
            items: Vec::new(),
            items_key: None,
            flame_scroll: FlameScroll::default(),
            last_escape: None,
            harness: args.harness,
        };
        if app.path.is_some() && !app.pending_load {
            app.start_load(ctx);
        }
        app
    }

    /// Start loading `self.path` on a background thread.
    fn start_load(&mut self, ctx: &egui::Context) {
        let Some(path) = self.path.clone() else {
            return;
        };
        self.pending_load = false;
        self.load_started = Instant::now();
        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        let (log, ctx) = (self.log.clone(), ctx.clone());
        let mut load_options = self.base.load_options.clone();
        load_options.cold = self.base.cold;
        load_options.progress = Some(self.bytes_read.clone());
        std::thread::spawn(move || {
            let log2 = log.clone();
            let ctx2 = ctx.clone();
            let result = load::load(&path, load_options, &mut move |phase| {
                log2.lock().unwrap().begin(phase);
                ctx2.request_repaint();
            });
            log.lock().unwrap().finish();
            let _ = sender.send(result.map_err(|error| error.to_string()));
            ctx.request_repaint();
        });
    }

    /// Shown instead of loading, the first time a file is opened: offer to tune the reader first.
    fn tune_prompt_screen(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.harness.poll_screen(ctx);
        ui.add_space(((ui.available_height() - 220.0) / 2.0).max(30.0));
        ui.vertical_centered(|ui| {
            ui.heading("Tune the reader for this machine?");
            ui.add_space(8.0);
            ui.label(format!("About to open {}.", self.path_text()));
            ui.add_space(6.0);
            ui.label("perf-viz can measure how fast your disk reads and pick the reader settings that suit it (about half a minute).");
            ui.label("You are only asked this once; it is always available in the Options menu, under “Tune reader…”.");
            ui.add_space(18.0);
            if self.tuner.open {
                ui.spinner();
                ui.label("Tuning… the file opens when you close the tuner.");
                return;
            }
            // Both buttons have exact sizes, so the pair can be centered by arithmetic.
            const TUNE_W: f32 = 170.0;
            const SKIP_W: f32 = 200.0;
            const BTN_H: f32 = 38.0;
            let gap = ui.spacing().item_spacing.x;
            ui.horizontal(|ui| {
                ui.add_space(((ui.available_width() - (TUNE_W + gap + SKIP_W)) / 2.0).max(0.0));
                if ui.add_sized([TUNE_W, BTN_H], egui::Button::new(RichText::new("Tune now").size(16.0))).clicked() {
                    self.config.tuning_prompted = Some(true);
                    config::save(&self.config);
                    self.tune_first = true;
                    let path = self.path.clone();
                    self.tuner.open_for(ctx, path.as_deref());
                }
                if ui.add_sized([SKIP_W, BTN_H], egui::Button::new(RichText::new("Skip, open the file").size(16.0))).clicked() {
                    self.config.tuning_prompted = Some(true);
                    config::save(&self.config);
                    self.start_load(ctx);
                }
            });
        });
    }

    /// The reader's settings, editable in the Options menu. They are saved, and used the next time a
    /// file is opened (the one on screen is already loaded).
    fn reader_options(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Reader (applies to the next file opened)").weak());
        let options = &mut self.base.load_options;
        let reader = &mut self.config.reader;
        let mut changed = false;
        egui::Grid::new("reader_opts").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
            let row = |ui: &mut egui::Ui, label: &str, tip: &str, value: &mut usize, range: std::ops::RangeInclusive<usize>, store: &mut Option<usize>| -> bool {
                ui.label(label).on_hover_text(tip);
                let changed = ui.add(egui::DragValue::new(value).range(range)).changed();
                if changed {
                    *store = Some(*value);
                }
                ui.end_row();
                changed
            };
            changed |= row(ui, "Readers", "Threads reading the file in parallel", &mut options.readers, 1..=32, &mut reader.readers);
            changed |= row(ui, "Parse threads", "Threads parsing and unwinding", &mut options.workers, 1..=128, &mut reader.threads);
            let mut region = options.region_mb as usize;
            ui.label("Region (MiB)").on_hover_text("Size of one file region; each reader works on a region at a time. Larger regions read big files faster.");
            if ui.add(egui::DragValue::new(&mut region).range(4..=2048).speed(2.0)).changed() {
                options.region_mb = region as u64;
                reader.region_mb = Some(region as u64);
                changed = true;
            }
            ui.end_row();
            changed |= row(ui, "Regions open", "Regions held in memory at once (memory is about Region × this)", &mut options.in_flight, 1..=64, &mut reader.in_flight);
            changed |= row(ui, "Lookahead", "Process a region after this many following regions are read", &mut options.lookahead, 0..=8, &mut reader.lookahead);
        });
        ui.label(
            RichText::new(format!(
                "about {} MiB of read buffers",
                options.region_mb * options.in_flight as u64
            ))
            .weak(),
        );
        let mut dwarf = options.dwarf == perf_callgraph::DwarfMode::Auto;
        if ui
            .checkbox(&mut dwarf, "Use DWARF (inlined frames, file:line)")
            .on_hover_text("Off: function names only, but faster to load")
            .changed()
        {
            options.dwarf = if dwarf {
                perf_callgraph::DwarfMode::Auto
            } else {
                perf_callgraph::DwarfMode::Never
            };
            reader.dwarf = Some(if dwarf { Dwarf::Auto } else { Dwarf::Never });
            changed = true;
        }
        if ui
            .checkbox(&mut options.cold, "Drop the file from the page cache first")
            .on_hover_text("For timing a cold read; adds time when the file is cached")
            .changed()
        {
            self.base.cold = options.cold;
            reader.cold = Some(options.cold);
            changed = true;
        }
        if ui
            .small_button("Reset reader settings")
            .on_hover_text("Back to the built-in defaults")
            .clicked()
        {
            *reader = ReaderConfig::default();
            let fresh = load::Options::default();
            *options = fresh;
            self.base.cold = false;
            changed = true;
        }
        if changed {
            config::save(&self.config);
        }
    }

    /// The start screen: a button for the file dialog, and a hint that files can be dropped.
    fn welcome_screen(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.harness.poll_screen(ctx);
        ui.add_space(((ui.available_height() - 200.0) / 2.0).max(30.0));
        ui.vertical_centered(|ui| {
            ui.heading("perf-viz");
            ui.add_space(4.0);
            ui.label("A viewer for perf.data recordings");
            ui.add_space(24.0);
            let busy = self.picker.is_some();
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new(RichText::new("Open file…").size(18.0))
                        .min_size(vec2(180.0, 44.0)),
                )
                .clicked()
            {
                self.pick_file(ctx);
            }
            ui.add_space(14.0);
            ui.weak("or drag a perf.data file into this window");
            ui.add_space(14.0);
            if ui
                .small_button("Tune reader…")
                .on_hover_text("Measure the disk and find good reader settings")
                .clicked()
            {
                self.tuner.open_for(ctx, None);
            }
            if let Some(path) = config::path() {
                ui.add_space(28.0);
                ui.weak(format!("settings: {}", path.display()));
            }
        });
    }

    fn path_text(&self) -> String {
        self.path
            .as_ref()
            .map_or(String::new(), |path| path.display().to_string())
    }

    fn view(&self) -> usize {
        self.truncated as usize
    }

    fn filter(&self) -> Filter {
        match (self.sel_tid, self.sel_pid) {
            (Some(thread_id), _) => Filter::Thread(thread_id),
            (None, Some(process_id)) => Filter::Process(process_id),
            _ => Filter::All,
        }
    }

    fn loading_screen(&self, ui: &mut egui::Ui) {
        const BLOCK_W: f32 = 460.0;
        const BLOCK_H: f32 = 230.0;
        ui.add_space(((ui.available_height() - BLOCK_H) / 2.0).max(30.0));
        ui.vertical_centered(|ui| {
            ui.heading(format!("Loading {}", self.path_text()));
            ui.label(format!(
                "{:.1}s elapsed",
                self.load_started.elapsed().as_secs_f32()
            ));
        });
        ui.add_space(12.0);
        let margin = ((ui.available_width() - BLOCK_W) / 2.0).max(0.0);
        ui.horizontal_top(|ui| {
            ui.add_space(margin);
            ui.vertical(|ui| {
                ui.set_width(BLOCK_W);
                self.loading_phases(ui);
            });
        });
    }

    fn loading_phases(&self, ui: &mut egui::Ui) {
        let log = self.log.lock().unwrap();
        let active = log
            .entries
            .last()
            .filter(|phase| phase.end.is_none())
            .map(|phase| phase.name.as_str());
        for (name, label) in PHASES {
            let entry = log.entries.iter().find(|phase| phase.name == name);
            // Status marks are drawn, not typed: the default font lacks the usual check/cross glyphs.
            let dot = |ui: &mut egui::Ui, fill: Option<Color32>| {
                let (rect, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
                match fill {
                    Some(color) => ui.painter().circle_filled(rect.center(), 5.0, color),
                    None => ui.painter().circle_stroke(
                        rect.center(),
                        5.0,
                        Stroke::new(1.0, ui.visuals().weak_text_color()),
                    ),
                };
            };
            ui.horizontal(|ui| match entry {
                Some(Phase {
                    start,
                    end: Some(end),
                    ..
                }) => {
                    dot(ui, Some(Color32::from_rgb(90, 200, 120)));
                    ui.label(format!("{label}  ({:.2}s)", (*end - *start).as_secs_f32()));
                }
                Some(Phase {
                    start, end: None, ..
                }) => {
                    ui.spinner();
                    ui.label(
                        RichText::new(format!("{label}  ({:.1}s)", start.elapsed().as_secs_f32()))
                            .strong(),
                    );
                }
                None => {
                    dot(ui, None);
                    ui.label(RichText::new(label).weak());
                }
            });
            if active == Some(name) && name == PHASES[0].0 && self.file_size > 0 {
                let done = self.bytes_read.load(Ordering::Relaxed);
                let frac = (done as f32 / self.file_size as f32).min(1.0);
                ui.add(
                    egui::ProgressBar::new(frac)
                        .desired_width(ui.available_width())
                        .text(format!(
                            "{:.1} of {:.1} GB",
                            done as f64 / 1e9,
                            self.file_size as f64 / 1e9
                        )),
                );
            }
        }
    }
}

impl eframe::App for App {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // A tuning run in progress deletes its multi-GB test file when it is stopped.
        self.tuner.shutdown();
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        let screen_ready = self.profile.is_some() || self.path.is_none() || self.pending_load;
        self.harness.raw_input_hook(raw, screen_ready);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let pal = Pal::of(ui);
        if self.frames_total < 3 {
            self.frames_total += 1;
            timing(&format!("frame {}", self.frames_total));
        }
        if self.handle_file_requests(&ctx) {
            return;
        }
        self.handle_close_request(&ctx);
        if self.update_tuner(&ctx) {
            return;
        }
        if self.pending_load && self.tune_first && !self.tuner.open {
            self.start_load(&ctx);
        }
        draw_drop_overlay(&ctx, &pal);
        self.receive_profile();

        let Some(shared_profile) = self.profile.clone() else {
            self.draw_without_profile(ui, &ctx, &pal);
            return;
        };
        let profile: &Profile = &shared_profile;
        self.harness.poll_screen(&ctx);

        if self.draw_top_bar(ui, &ctx, profile) {
            self.totals_key = None;
        }
        self.save_tab_choice();

        let filter = self.filter();
        // The trie must be chosen *after* the controls above, which can change `truncated`: totals
        // are per node of the chosen trie, so a stale trie would index them out of range.
        let trie: &Trie = if self.truncated {
            &profile.truncated
        } else {
            &profile.full
        };
        let view_context = ViewContext {
            profile,
            trie,
            view: self.view(),
            cost: self.cost,
            unit: noun(profile, self.cost),
            filtered: filter != Filter::All || self.range.is_some(),
            searching: !self.search.is_empty(),
            pal,
        };
        self.refresh_totals(&view_context, filter);
        self.refresh_search(profile);
        self.refresh_timeline(ui, &view_context, filter);

        let mut timeline_actions = Vec::new();
        if self.tab != Tab::Timeline {
            timeline_actions = self.draw_timeline_strip(ui, &view_context);
        }
        if self.tab == Tab::FlameGraph {
            self.draw_breadcrumbs(ui, &view_context);
        }
        self.draw_status_bar(ui, &view_context);
        egui::CentralPanel::default().show(ui, |ui| {
            self.draw_tab(ui, &view_context, &mut timeline_actions);
        });
        self.apply_timeline_actions(timeline_actions);
    }
}

fn draw_drop_overlay(ctx: &egui::Context, pal: &Pal) {
    if ctx.input(|i| i.raw.hovered_files.is_empty()) {
        return;
    }
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("drop_overlay"),
    ));
    let rect = ctx.content_rect();
    painter.rect_filled(rect, 0.0, Color32::from_black_alpha(150));
    painter.rect_stroke(
        rect.shrink(12.0),
        8.0,
        Stroke::new(3.0, pal.highlight()),
        StrokeKind::Inside,
    );
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        "Drop a perf.data file to open it",
        FontId::proportional(26.0),
        Color32::WHITE,
    );
}

impl App {
    /// Opens a file that was dropped, chosen in the dialog or requested by the script. Returns
    /// whether it was opened (this frame is then over).
    fn handle_file_requests(&mut self, ctx: &egui::Context) -> bool {
        let dropped = ctx.input(|i| {
            i.raw
                .dropped_files
                .first()
                .map(|file| file.path().to_path_buf())
        });
        let picked = self
            .picker
            .as_ref()
            .and_then(|receiver| match receiver.try_recv() {
                Ok(picked) => Some(picked),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => Some(None),
            });
        if picked.is_some() {
            self.picker = None;
        }
        let Some(path) = dropped.or(picked.flatten()).or(self.harness.take_open()) else {
            return false;
        };
        if self.tuner.unsaved() {
            // Unsaved tuning results: ask about them first; the file opens once that is settled.
            self.tuner.ask_before_open(path);
            return false;
        }
        self.open(ctx, path);
        ctx.request_repaint();
        true
    }

    fn handle_close_request(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested())
            && self.tuner.unsaved()
            && !self.harness.finished()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.tuner.ask_before_quit();
        }
    }

    /// Shows the tuner window and applies what it decided. Returns whether it opened a file
    /// (this frame is then over).
    fn update_tuner(&mut self, ctx: &egui::Context) -> bool {
        if self.harness.take_tune_window() {
            self.tuner.open_for(ctx, self.path.as_deref());
        }
        let tuned = self.tuner.show(ctx);
        if tuned.quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if let Some(settings) = tuned.apply {
            self.config.tuning_prompted = Some(true);
            self.config.reader.readers = Some(settings.readers);
            self.config.reader.region_mb = Some(settings.region_mb);
            self.config.reader.in_flight = Some(settings.in_flight);
            config::save(&self.config);
            self.base.load_options.readers = settings.readers;
            self.base.load_options.region_mb = settings.region_mb;
            self.base.load_options.in_flight = settings.in_flight;
        }
        let Some(path) = tuned.open_file else {
            return false;
        };
        self.open(ctx, path);
        ctx.request_repaint();
        true
    }

    /// Takes the finished load, if any, and sets up everything derived from the profile.
    fn receive_profile(&mut self) {
        let Some(receiver) = &self.receiver else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(loaded_profile)) => {
                if loaded_profile.stats.samples_with_period == 0 {
                    self.cost = Cost::Samples;
                }
                self.proc_rows = filter::processes(&loaded_profile, self.cost);
                self.funcs = Some(Functions::new(&loaded_profile));
                self.summary = SummaryInfo::new(&loaded_profile);
                self.summary.load = self.load_info();
                if let (Some(ms), Some(first)) =
                    (self.pending_range_ms.take(), loaded_profile.samples.first())
                {
                    self.range = Some(TimeRange {
                        start: first.time + (ms.start_ms * 1e6) as u64,
                        end: first.time + (ms.end_ms * 1e6) as u64,
                    });
                }
                // `--thread` alone: find the thread's process so the selectors agree.
                if let (Some(thread_id), None) = (self.sel_tid, self.sel_pid) {
                    self.sel_pid = loaded_profile
                        .samples
                        .iter()
                        .find(|sample| sample.tid == thread_id)
                        .map(|sample| sample.pid);
                }
                self.profile = Some(Arc::new(loaded_profile));
                self.receiver = None;
            }
            Ok(Err(message)) => {
                self.error = Some(message);
                self.receiver = None;
            }
            Err(_) => {}
        }
    }

    /// How long the file read took and the whole load, for the summary.
    fn load_info(&self) -> Option<LoadInfo> {
        let log = self.log.lock().unwrap();
        let read_secs = log
            .entries
            .iter()
            .find(|phase| phase.name == PHASES[0].0)
            .and_then(|phase| phase.end.map(|end| (end - phase.start).as_secs_f32()))?;
        Some(LoadInfo {
            file_bytes: self.file_size,
            read_secs,
            total_secs: self.load_started.elapsed().as_secs_f32(),
        })
    }

    /// The screens shown while there is no profile: welcome, tune prompt, error or progress.
    fn draw_without_profile(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, pal: &Pal) {
        egui::CentralPanel::default().show(ui, |ui| {
            if self.path.is_none() {
                self.welcome_screen(ui, ctx);
            } else if self.pending_load {
                self.tune_prompt_screen(ui, ctx);
            } else if let Some(message) = self.error.clone() {
                ui.add_space(40.0);
                ui.colored_label(
                    pal.suspect(),
                    format!("Failed to load {}: {message}", self.path_text()),
                );
                ui.add_space(8.0);
                if ui.button("Open another file…").clicked() {
                    self.pick_file(ctx);
                }
                ui.weak("or drop a file on the window");
            } else {
                self.loading_screen(ui);
                ctx.request_repaint_after(Duration::from_millis(100));
                self.harness
                    .poll_loading(ctx, self.load_started.elapsed().as_secs_f32());
            }
        });
    }

    /// Draws the toolbar and the filters; returns whether a filter changed.
    fn draw_top_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, profile: &Profile) -> bool {
        let mut filter_changed = false;
        egui::Panel::top("top").show(ui, |ui| {
            self.draw_toolbar(ui, ctx, profile);
            filter_changed = self.draw_filters(ui, profile);
        });
        filter_changed
    }

    fn draw_toolbar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, profile: &Profile) {
        ui.horizontal_wrapped(|ui| {
            ui.strong("perf-viz");
            if ui
                .button("Open…")
                .on_hover_text("Open another file (or drop one on the window)")
                .clicked()
            {
                self.pick_file(ctx);
            }
            ui.label(self.path_text());
            ui.separator();
            for (tab, label) in TABS {
                ui.selectable_value(&mut self.tab, tab, label);
            }
            ui.separator();
            // Stays open while settings are changed; closes on a click outside.
            egui::containers::menu::MenuButton::new("Options")
                .config(
                    egui::containers::menu::MenuConfig::new()
                        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
                )
                .ui(ui, |ui| self.draw_options_menu(ui, ctx, profile));
            ui.separator();
            let (rect, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
            ui.painter()
                .rect_filled(rect, 2.0, Color32::from_rgb(200, 60, 60)); // the same red as the boxes
            ui.label("suspect tail: from the first invalid return address out to the root");
        });
    }

    fn draw_options_menu(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, profile: &Profile) {
        ui.set_min_width(230.0);
        if profile.stats.samples_with_period > 0 {
            self.draw_cost_options(ui, profile);
            ui.separator();
        }
        self.draw_theme_options(ui, ctx);
        ui.separator();
        ui.label(RichText::new("Call trees").weak());
        if ui
            .checkbox(&mut self.truncated, "Truncate suspect tails")
            .on_hover_text("Drop the frames from the first invalid return address out to the root")
            .changed()
        {
            self.items_key = None;
            self.totals_key = None;
            self.config.truncate_suspect_tails = Some(self.truncated);
            config::save(&self.config);
        }
        ui.separator();
        self.reader_options(ui);
        ui.separator();
        if ui
            .button("Tune reader…")
            .on_hover_text("Measure the disk and find good reader settings")
            .clicked()
        {
            self.tuner.open_for(ctx, self.path.as_deref());
        }
    }

    fn draw_cost_options(&mut self, ui: &mut egui::Ui, profile: &Profile) {
        ui.label(RichText::new("Cost").weak());
        let before = self.cost;
        ui.radio_value(&mut self.cost, Cost::Samples, "Samples")
            .on_hover_text("Count each sample once");
        ui.radio_value(&mut self.cost, Cost::Cycles, cycles_label(profile))
            .on_hover_text("Weigh each sample by its period: the events (cycles) it stands for");
        if self.cost != before {
            self.config.cost = Some(self.cost);
            config::save(&self.config);
            self.proc_rows = filter::processes(profile, self.cost);
            self.totals_key = None;
        }
    }

    fn draw_theme_options(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.label(RichText::new("Theme").weak());
        let mut preference = ctx.options(|options| options.theme_preference);
        let before = preference;
        ui.radio_value(&mut preference, egui::ThemePreference::System, "System")
            .on_hover_text("Follow the desktop's light/dark setting (dark if it has none)");
        ui.radio_value(&mut preference, egui::ThemePreference::Light, "Light");
        ui.radio_value(&mut preference, egui::ThemePreference::Dark, "Dark");
        if preference != before {
            ctx.set_theme(preference);
            self.config.theme = Some(theme_setting(preference));
            config::save(&self.config);
        }
    }

    /// The process and thread selectors and the search box; returns whether a selection changed.
    fn draw_filters(&mut self, ui: &mut egui::Ui, profile: &Profile) -> bool {
        let mut changed = false;
        ui.horizontal_wrapped(|ui| {
            changed |= self.draw_process_selector(ui, profile);
            if let Some(pid) = self.sel_pid {
                changed |= self.draw_thread_selector(ui, profile, pid);
            }
            ui.separator();
            ui.label("Search:");
            ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("function name")
                    .desired_width(200.0),
            );
            if !self.search.is_empty() && ui.small_button("Clear").clicked() {
                self.search.clear();
            }
        });
        changed
    }

    fn draw_process_selector(&mut self, ui: &mut egui::Ui, profile: &Profile) -> bool {
        let mut changed = false;
        let proc_text = match self.sel_pid {
            None => "All processes".to_string(),
            Some(pid) => format!(
                "{} ({pid})",
                profile.comm.get(&pid).map_or("?", String::as_str)
            ),
        };
        ui.label("Process:");
        egui::ComboBox::from_id_salt("proc")
            .width(230.0)
            .selected_text(proc_text)
            .show_ui(ui, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.proc_query)
                        .hint_text("filter by name or pid"),
                );
                if ui
                    .selectable_label(self.sel_pid.is_none(), "All processes")
                    .clicked()
                {
                    (self.sel_pid, self.sel_tid) = (None, None);
                    changed = true;
                }
                let query_lowercase = self.proc_query.to_lowercase();
                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .show(ui, |ui| {
                        let mut shown = 0;
                        for row in &self.proc_rows {
                            if shown >= MAX_LIST {
                                ui.weak(format!(
                                    "…refine the filter to see more of {} processes",
                                    self.proc_rows.len()
                                ));
                                break;
                            }
                            if query_lowercase.is_empty()
                                || row.comm.to_lowercase().contains(&query_lowercase)
                                || row.pid.to_string().contains(&query_lowercase)
                            {
                                shown += 1;
                                let label = format!(
                                    "{} ({}) — {} {}",
                                    row.comm,
                                    row.pid,
                                    self.cost.format(row.cost),
                                    noun(profile, self.cost)
                                );
                                if ui
                                    .selectable_label(self.sel_pid == Some(row.pid), label)
                                    .clicked()
                                {
                                    (self.sel_pid, self.sel_tid) = (Some(row.pid), None);
                                    changed = true;
                                }
                            }
                        }
                    });
            });
        changed
    }

    fn draw_thread_selector(&mut self, ui: &mut egui::Ui, profile: &Profile, pid: i32) -> bool {
        let mut changed = false;
        ui.label("Thread:");
        let text = self
            .sel_tid
            .map_or("All threads".to_string(), |thread_id| thread_id.to_string());
        egui::ComboBox::from_id_salt("thread")
            .width(140.0)
            .selected_text(text)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(self.sel_tid.is_none(), "All threads")
                    .clicked()
                {
                    self.sel_tid = None;
                    changed = true;
                }
                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .show(ui, |ui| {
                        for (tid, cost) in filter::threads(profile, pid, self.cost)
                            .into_iter()
                            .take(MAX_LIST)
                        {
                            if ui
                                .selectable_label(
                                    self.sel_tid == Some(tid),
                                    format!(
                                        "{tid} — {} {}",
                                        self.cost.format(cost),
                                        noun(profile, self.cost)
                                    ),
                                )
                                .clicked()
                            {
                                self.sel_tid = Some(tid);
                                changed = true;
                            }
                        }
                    });
            });
        changed
    }

    fn save_tab_choice(&mut self) {
        if self.tab != self.saved_tab {
            self.saved_tab = self.tab;
            self.config.tab = Some(self.tab.name());
            config::save(&self.config);
        }
    }

    /// Recomputes the per-node totals and what depends on them when the view, filter, range or
    /// cost changed.
    fn refresh_totals(&mut self, view_context: &ViewContext<'_>, filter: Filter) {
        let ViewContext {
            profile,
            trie,
            view,
            cost,
            ..
        } = *view_context;
        let totals_key = TotalsKey {
            truncated: self.truncated,
            filter,
            range: self.range,
            cost,
        };
        if self.totals_key == Some(totals_key) {
            return;
        }
        self.totals = filter::totals_in(profile, self.truncated, filter, self.range, cost);
        self.grand_total = if filter == Filter::All && self.range.is_none() {
            self.totals[ROOT]
        } else {
            filter::totals_in(profile, self.truncated, Filter::All, None, cost)[ROOT]
        };
        self.selfs = views::self_counts(trie, &self.totals);
        if let Some(funcs) = &self.funcs {
            self.costs = views::costs(trie, &self.totals, &self.selfs, funcs);
        }
        self.totals_key = Some(totals_key);
        self.totals_version += 1;
        self.items_key = None;
        self.search_applied = None;
        if !filter::has_samples(&self.totals, self.focus[view]) {
            self.focus[view] = ROOT;
        }
    }

    fn refresh_search(&mut self, profile: &Profile) {
        let search_key = SearchKey {
            query: self.search.clone(),
            totals_version: self.totals_version,
        };
        if self.search_applied.as_ref() != Some(&search_key) {
            self.matches = filter::matching_frames(profile, &self.search);
            self.coverage =
                filter::search_coverage(profile, self.truncated, &self.totals, &self.matches);
            self.search_applied = Some(search_key);
        }
    }

    fn draw_breadcrumbs(&mut self, ui: &mut egui::Ui, view_context: &ViewContext<'_>) {
        let ViewContext {
            profile,
            trie,
            view,
            cost,
            unit,
            searching,
            pal,
            ..
        } = *view_context;
        let total = self.totals[ROOT].max(1);
        egui::Panel::top("crumbs").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                let mut path = Vec::new();
                let mut current = self.focus[view];
                while current != ROOT {
                    path.push(current);
                    current = trie.nodes[current]
                        .parent
                        .expect("a node other than the root has a parent");
                }
                if ui.small_button("all").clicked() {
                    self.focus[view] = ROOT;
                }
                for &node in path.iter().rev() {
                    ui.label(">");
                    let frame = trie.nodes[node]
                        .frame
                        .expect("a node on the path has a frame");
                    let name = &profile.frames[frame].name;
                    let short: String = name.chars().take(28).collect();
                    if ui.small_button(short).clicked() {
                        self.focus[view] = node;
                    }
                }
                ui.weak(
                    "(click a box to zoom, Esc to zoom out, Esc twice to zoom all the way out)",
                );
                if searching {
                    ui.separator();
                    let Coverage {
                        cost: samples,
                        boxes,
                    } = self.coverage;
                    if boxes == 0 {
                        ui.colored_label(
                            pal.warning(),
                            format!("no frame matches “{}”", self.search),
                        );
                    } else {
                        ui.colored_label(
                            pal.highlight(),
                            format!(
                                "“{}”: {} {unit} ({}% of the view) in {boxes} boxes",
                                self.search,
                                cost.format(samples),
                                fmt::pct_of(samples as f64, total as f64)
                            ),
                        );
                    }
                }
            });
        });
    }

    fn draw_status_bar(&self, ui: &mut egui::Ui, view_context: &ViewContext<'_>) {
        let ViewContext {
            profile,
            trie,
            cost,
            unit,
            filtered,
            pal,
            ..
        } = *view_context;
        let shown_total = self.totals[ROOT];
        egui::Panel::bottom("status").show(ui, |ui| {
            let state = &profile.stats;
            ui.horizontal_wrapped(|ui| {
                if !filtered {
                    ui.label(format!("{} {unit} in view", cost.format(shown_total)));
                } else {
                    ui.label(format!("{} of {} {unit} (filtered)", cost.format(shown_total), cost.format(self.grand_total)));
                }
                ui.separator();
                ui.label(format!("{} nodes", trie.nodes.len()));
                ui.separator();
                ui.label(format!("suspect tails: {}% of samples", fmt::pct_of(state.suspect_samples as f64, state.samples as f64)));
                ui.separator();
                if state.lost_events > 0 {
                    let seen = state.samples + state.lost_events;
                    ui.colored_label(pal.warning(), format!("kernel dropped {} events ({}% of {})", state.lost_events, fmt::pct_of(state.lost_events as f64, seen as f64), seen));
                    ui.separator();
                }
                ui.label(format!("symbols: {} DWARF / {} ELF-only / {} raw modules, {} of {} addresses unresolved", state.modules_full, state.modules_symbols_only, state.modules_raw, state.unresolved_addresses, state.unique_addresses));
                if self.tab == Tab::FlameGraph {
                    ui.separator();
                    ui.label(format!("{} boxes", self.items.len()));
                }
            });
        });
    }

    /// The main area: the view of the selected tab.
    fn draw_tab(
        &mut self,
        ui: &mut egui::Ui,
        view_context: &ViewContext<'_>,
        timeline_actions: &mut Vec<timeline::Action>,
    ) {
        if self.tab != Tab::FlameGraph {
            self.flame_scroll.restore = true;
        }
        let query = self.search.to_lowercase();
        let data = ViewData {
            funcs: self.funcs.as_ref().expect("set when the profile arrived"),
            totals: &self.totals,
            selfs: &self.selfs,
            costs: &self.costs,
            matches: &self.matches,
            query: &query,
            version: self.totals_version,
        };
        match self.tab {
            Tab::TopDown => views::top_down::draw_top_down(ui, view_context, &data, &mut self.td),
            Tab::BottomUp => {
                views::bottom_up::draw_bottom_up(ui, view_context, &data, &mut self.bu)
            }
            Tab::CallerCallee => {
                views::caller_callee::draw_caller_callee(ui, view_context, &data, &mut self.cc)
            }
            Tab::Timeline => self.draw_timeline_tab(ui, view_context, timeline_actions),
            Tab::Summary => self.draw_summary_tab(ui, view_context),
            Tab::FlameGraph => self.draw_flame_graph(ui, view_context),
        }
    }
}

/// Label of the weighted cost button, e.g. `Cycles`.
fn cycles_label(profile: &Profile) -> String {
    let mut chars = profile.stats.period_unit.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// Startup timing, printed with `PERF_VIZ_TIMING=1`: milliseconds since the process began.
static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

fn timing(what: &str) {
    if std::env::var_os("PERF_VIZ_TIMING").is_some() {
        eprintln!(
            "[{:7.1} ms] {what}",
            START.get_or_init(Instant::now).elapsed().as_secs_f64() * 1e3
        );
    }
}

fn main() -> eframe::Result {
    timing("main");
    let cli = Cli::parse_args();
    let config = config::load();
    let mut load_options = load::Options::default();
    config.reader.apply(&mut load_options);
    cli.reader.apply(&mut load_options);
    let harness = Harness::from_cli(&cli);
    let show = cli.show;
    let args = Args {
        path: cli.file,
        truncated: show
            .view
            .map_or(config.truncate_suspect_tails.unwrap_or(false), |view| {
                view == cli::View::Truncated
            }),
        cost: show.cost.or(config.cost),
        theme: cli.theme.or(config.theme).map(theme_pref),
        tab: show
            .tab
            .or(config.tab)
            .map_or(Tab::FlameGraph, Tab::from_name),
        cold: load_options.cold,
        load_options,
        process: show.process,
        thread: show.thread,
        search: show.search.unwrap_or_default(),
        range_ms: show.range,
        harness,
        config,
    };
    // Maximized unless `--size` asks for a fixed window (which the screenshot checks rely on).
    let (size, maximized) = cli.size.map_or(([1500.0, 900.0], true), |size| {
        ([size.width, size.height], false)
    });
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(size)
            .with_maximized(maximized)
            .with_title("perf-viz"),
        ..Default::default()
    };
    timing("options ready, creating the window");
    eframe::run_native(
        "perf-viz",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, args)))),
    )
}
