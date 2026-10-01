#![doc = include_str!("../docs/overview.md")]

mod cli;
mod config;
mod filter;
mod flame;
mod fmt;
mod harness;
mod table;
mod theme;
mod timeline;
mod tuner;
mod views;

use std::{
    collections::HashSet,
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
    flame::Item,
    fmt::Cost,
    harness::Harness,
    table::{Arrow, Click, Columns, RowData},
    theme::Pal,
    timeline::{TimeRange, XDrag},
    tuner::Tuner,
    views::{BuRow, Functions, TdRow, Weighted},
};
use eframe::egui::{
    self, Align2, Color32, FontId, Key, Rect, RichText, Sense, Stroke, StrokeKind, pos2, vec2,
};
use perf_profile::{
    load,
    model::{FrameInfo, Profile, ROOT, Trie},
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

    fn from_name(n: TabName) -> Tab {
        match n {
            TabName::Summary => Tab::Summary,
            TabName::Flame => Tab::FlameGraph,
            TabName::Topdown => Tab::TopDown,
            TabName::Bottomup => Tab::BottomUp,
            TabName::Callercallee => Tab::CallerCallee,
            TabName::Timeline => Tab::Timeline,
        }
    }
}

fn theme_pref(t: Theme) -> egui::ThemePreference {
    match t {
        Theme::System => egui::ThemePreference::System,
        Theme::Light => egui::ThemePreference::Light,
        Theme::Dark => egui::ThemePreference::Dark,
    }
}

fn theme_setting(p: egui::ThemePreference) -> Theme {
    match p {
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

/// Width of the label column left of the timeline's plot, shared by the strip and the lanes.
const TL_LABEL_W: f32 = 200.0;

/// Facts about the whole recording, computed once.
#[derive(Default)]
struct SummaryInfo {
    processes: usize,
    threads: usize,
    /// Time of the first and last sample, in nanoseconds.
    first_ns: u64,
    last_ns: u64,
    /// How the load went.
    load: Option<LoadInfo>,
}

/// How a load went.
#[derive(Clone, Copy)]
struct LoadInfo {
    file_bytes: u64,
    /// Seconds spent reading (and unwinding) the file.
    read_secs: f32,
    /// Seconds for the whole load, with symbols and call trees.
    total_secs: f32,
}

impl SummaryInfo {
    fn new(p: &Profile) -> Self {
        let pids: HashSet<i32> = p.samples.iter().map(|s| s.pid).collect();
        let tids: HashSet<i32> = p.samples.iter().map(|s| s.tid).collect();
        Self {
            processes: pids.len(),
            threads: tids.len(),
            first_ns: p.samples.first().map_or(0, |s| s.time),
            last_ns: p.samples.last().map_or(0, |s| s.time),
            load: None,
        }
    }
}

/// State of the caller/callee view.
struct CcState {
    selected: Option<u32>,
    sort: views::SortBy,
    list: Vec<u32>,
    list_key: Option<ListKey>,
    /// Callers and callees of the selected function.
    lists: Option<CallLists>,
}

/// What the ranked function list was built for.
#[derive(Clone, PartialEq)]
struct ListKey {
    version: u64,
    sort: views::SortBy,
    query: String,
}

/// The callers and callees of one function, for the view version they were built for.
struct CallLists {
    func: u32,
    version: u64,
    callers: Vec<Weighted>,
    callees: Vec<Weighted>,
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

/// What a tree's visible rows were built for: the view (full or truncated tree), the data version
/// and the expansion version.
#[derive(Clone, Copy, PartialEq, Eq)]
struct RowsKey {
    view: usize,
    data_version: u64,
    expansion_version: u64,
}

/// State of the top-down tree. Node ids differ between the full and truncated tries, so per view.
#[derive(Default)]
struct TdState {
    expanded: [HashSet<u32>; 2],
    /// The user (or the first draw) has set this view's expansion.
    touched: [bool; 2],
    rows: Vec<TdRow>,
    rows_key: Option<RowsKey>,
    /// Bumped whenever `expanded` changes.
    version: u64,
    selected: [Option<u32>; 2],
}

/// State of the bottom-up tree. Rows are identified by the function path from the first level.
#[derive(Default)]
struct BuState {
    expanded: HashSet<Vec<u32>>,
    rows: Vec<BuRow>,
    rows_key: Option<RowsKey>,
    version: u64,
    selected: Option<Vec<u32>>,
}

const ROW_H: f32 = 18.0;
const MIN_BOX_PX: f32 = 0.5;
const LABEL_MIN_PX: f32 = 36.0;
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
    opts: load::Options,
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

/// What the flame graph's boxes were laid out for.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ItemsKey {
    truncated: bool,
    focus: u32,
    width: i32,
    totals_version: u64,
}

/// The view the flame graph's scroll position was last set for.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ScrollKey {
    truncated: bool,
    focus: u32,
    totals_version: u64,
}

/// The search text and totals the matches were computed for.
#[derive(Clone, PartialEq, Eq)]
struct SearchKey {
    query: String,
    totals_version: u64,
}

/// The timeline's data, for the filter, bin count and cost it was built for.
struct TimelineCache {
    filter: Filter,
    bins: usize,
    cost: Cost,
    timeline: timeline::Timeline,
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
    rx: Option<mpsc::Receiver<Result<Profile, String>>>,
    log: Arc<Mutex<LoadLog>>,
    bytes_read: Arc<AtomicU64>,
    load_started: Instant,
    profile: Option<Arc<Profile>>,
    error: Option<String>,

    truncated: bool,
    tab: Tab,
    /// Focused (zoomed) node, per view: node ids differ between the two tries.
    focus: [u32; 2],

    // Views over the loaded profile
    funcs: Option<Functions>,
    /// Samples ending at each node of the current view's trie (see `views::self_counts`).
    selfs: Vec<u64>,
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
    tl: Option<TimelineCache>,

    // Filters
    sel_pid: Option<i32>,
    sel_tid: Option<i32>,
    proc_rows: Vec<ProcRow>,
    proc_query: String,
    totals: Vec<u64>,
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

    // Search
    search: String,
    search_applied: Option<SearchKey>,
    matches: Vec<bool>,
    coverage: Coverage,

    items: Vec<Item>,
    items_key: Option<ItemsKey>,
    /// The view the flame graph's scroll position was last set for.
    flame_scroll_key: Option<ScrollKey>,

    harness: Harness,
    frames_since_load: u32,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, args: Args) -> Self {
        timing("window and renderer ready");
        if let Some(t) = args.theme {
            cc.egui_ctx.set_theme(t);
        }
        args.harness.configure(&cc.egui_ctx);
        Self::from_args(&cc.egui_ctx, args)
    }

    /// Open another file, keeping the settings in effect now (view, cost, tab, saved config).
    fn open(&mut self, ctx: &egui::Context, path: PathBuf) {
        let mut a = self.base.clone();
        a.path = Some(path);
        a.truncated = self.truncated;
        a.cost = Some(self.cost);
        a.tab = self.tab;
        a.config = self.config.clone();
        a.harness = self.harness.after_open();
        let tuner = std::mem::take(&mut self.tuner);
        *self = Self::from_args(ctx, a);
        self.tuner = tuner;
    }

    /// Show a file dialog without blocking the UI; the chosen file arrives through `picker`.
    fn pick_file(&mut self, ctx: &egui::Context) {
        if self.picker.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let picked = rfd::FileDialog::new()
                .set_title("Open a perf.data file")
                .pick_file();
            let _ = tx.send(picked);
            ctx.request_repaint();
        });
        self.picker = Some(rx);
    }

    fn from_args(ctx: &egui::Context, args: Args) -> Self {
        let base = Args {
            path: None,
            process: None,
            thread: None,
            search: String::new(),
            range_ms: None,
            harness: Harness::default(),
            ..args.clone()
        };
        let log = Arc::new(Mutex::new(LoadLog::default()));
        let bytes_read = Arc::new(AtomicU64::new(0));
        let file_size = args
            .path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map_or(0, |m| m.len());
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
            rx: None,
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
            selfs: Vec::new(),
            costs: views::Costs {
                self_: Vec::new(),
                inclusive: Vec::new(),
            },
            summary: SummaryInfo::default(),
            td: TdState::default(),
            bu: BuState::default(),
            cc: CcState::default(),
            range: None,
            pending_range_ms: args.range_ms,
            drag: None,
            tl: None,
            sel_pid: args.process,
            sel_tid: args.thread,
            proc_rows: Vec::new(),
            proc_query: String::new(),
            totals: Vec::new(),
            totals_key: None,
            cost: args.cost.unwrap_or_default(),
            grand_total: 0,
            config: args.config,
            saved_tab: args.tab,
            totals_version: 0,
            search: args.search,
            search_applied: None,
            matches: Vec::new(),
            coverage: Coverage::default(),
            items: Vec::new(),
            items_key: None,
            flame_scroll_key: None,
            harness: args.harness,
            frames_since_load: 0,
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
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        let (log, ctx) = (self.log.clone(), ctx.clone());
        let mut opts = self.base.opts.clone();
        opts.cold = self.base.cold;
        opts.progress = Some(self.bytes_read.clone());
        std::thread::spawn(move || {
            let log2 = log.clone();
            let ctx2 = ctx.clone();
            let r = load::load(&path, opts, &mut move |s| {
                log2.lock().unwrap().begin(s);
                ctx2.request_repaint();
            });
            log.lock().unwrap().finish();
            let _ = tx.send(r.map_err(|e| e.to_string()));
            ctx.request_repaint();
        });
    }

    /// Shown instead of loading, the first time a file is opened: offer to tune the reader first.
    fn tune_prompt_screen(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.frames_since_load += 1;
        self.harness.poll_screen(ctx, self.frames_since_load);
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
        let o = &mut self.base.opts;
        let r = &mut self.config.reader;
        let mut changed = false;
        egui::Grid::new("reader_opts").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
            let row = |ui: &mut egui::Ui, label: &str, tip: &str, v: &mut usize, range: std::ops::RangeInclusive<usize>, store: &mut Option<usize>| -> bool {
                ui.label(label).on_hover_text(tip);
                let c = ui.add(egui::DragValue::new(v).range(range)).changed();
                if c {
                    *store = Some(*v);
                }
                ui.end_row();
                c
            };
            changed |= row(ui, "Readers", "Threads reading the file in parallel", &mut o.readers, 1..=32, &mut r.readers);
            changed |= row(ui, "Parse threads", "Threads parsing and unwinding", &mut o.workers, 1..=128, &mut r.threads);
            let mut region = o.region_mb as usize;
            ui.label("Region (MiB)").on_hover_text("Size of one file region; each reader works on a region at a time. Larger regions read big files faster.");
            if ui.add(egui::DragValue::new(&mut region).range(4..=2048).speed(2.0)).changed() {
                o.region_mb = region as u64;
                r.region_mb = Some(region as u64);
                changed = true;
            }
            ui.end_row();
            changed |= row(ui, "Regions open", "Regions held in memory at once (memory is about Region × this)", &mut o.in_flight, 1..=64, &mut r.in_flight);
            changed |= row(ui, "Lookahead", "Process a region after this many following regions are read", &mut o.lookahead, 0..=8, &mut r.lookahead);
        });
        ui.label(
            RichText::new(format!(
                "about {} MiB of read buffers",
                o.region_mb * o.in_flight as u64
            ))
            .weak(),
        );
        let mut dwarf = o.dwarf == perf_profile::symbolize::DwarfMode::Auto;
        if ui
            .checkbox(&mut dwarf, "Use DWARF (inlined frames, file:line)")
            .on_hover_text("Off: function names only, but faster to load")
            .changed()
        {
            o.dwarf = if dwarf {
                perf_profile::symbolize::DwarfMode::Auto
            } else {
                perf_profile::symbolize::DwarfMode::Never
            };
            r.dwarf = Some(if dwarf { Dwarf::Auto } else { Dwarf::Never });
            changed = true;
        }
        if ui
            .checkbox(&mut o.cold, "Drop the file from the page cache first")
            .on_hover_text("For timing a cold read; adds time when the file is cached")
            .changed()
        {
            self.base.cold = o.cold;
            r.cold = Some(o.cold);
            changed = true;
        }
        if ui
            .small_button("Reset reader settings")
            .on_hover_text("Back to the built-in defaults")
            .clicked()
        {
            *r = ReaderConfig::default();
            let fresh = load::Options::default();
            *o = fresh;
            self.base.cold = false;
            changed = true;
        }
        if changed {
            config::save(&self.config);
        }
    }

    /// The start screen: a button for the file dialog, and a hint that files can be dropped.
    fn welcome_screen(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.frames_since_load += 1;
        self.harness.poll_screen(ctx, self.frames_since_load);
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
            if let Some(p) = config::path() {
                ui.add_space(28.0);
                ui.weak(format!("settings: {}", p.display()));
            }
        });
    }

    fn path_text(&self) -> String {
        self.path
            .as_ref()
            .map_or(String::new(), |p| p.display().to_string())
    }

    fn view(&self) -> usize {
        self.truncated as usize
    }

    fn filter(&self) -> Filter {
        match (self.sel_tid, self.sel_pid) {
            (Some(t), _) => Filter::Thread(t),
            (None, Some(p)) => Filter::Process(p),
            _ => Filter::All,
        }
    }

    fn loading_screen(&self, ui: &mut egui::Ui) {
        // A fixed-width block in the middle of the window.
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
            .filter(|e| e.end.is_none())
            .map(|e| e.name.as_str());
        for (name, label) in PHASES {
            let entry = log.entries.iter().find(|e| e.name == name);
            // Status marks are drawn, not typed: the default font lacks the usual check/cross glyphs.
            let dot = |ui: &mut egui::Ui, fill: Option<Color32>| {
                let (r, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
                match fill {
                    Some(c) => ui.painter().circle_filled(r.center(), 5.0, c),
                    None => ui.painter().circle_stroke(
                        r.center(),
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

fn module_color(p: &Profile, module: u32, name: &str) -> Color32 {
    // Hue from the module, a little brightness variation from the function name.
    let h = |s: &str| {
        s.bytes().fold(0xcbf29ce484222325u64, |a, b| {
            (a ^ b as u64).wrapping_mul(0x100000001b3)
        })
    };
    let hue = (h(p.module_name(module)) % 360) as f32 / 360.0;
    let v = 0.78 + 0.16 * ((h(name) % 100) as f32 / 100.0);
    egui::ecolor::Hsva::new(hue, 0.45, v, 1.0).into()
}

fn frame_color(p: &Profile, f: &FrameInfo) -> Color32 {
    if f.suspect {
        Color32::from_rgb(200, 60, 60)
    } else if f.raw {
        Color32::from_gray(150)
    } else {
        module_color(p, f.module, &f.name)
    }
}

impl eframe::App for App {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // A tuning run in progress deletes its multi-GB test file when it is stopped.
        self.tuner.shutdown();
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        // Scripted pointer events (only with the `scripted` feature).
        let screen_ready = self.profile.is_some() || self.path.is_none() || self.pending_load;
        self.harness
            .raw_input_hook(raw, screen_ready, self.frames_since_load);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let pal = Pal::of(ui);
        if self.frames_total < 3 {
            self.frames_total += 1;
            timing(&format!("frame {}", self.frames_total));
        }

        // A file dropped on the window, or chosen in the file dialog, replaces what is open.
        let dropped = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .next()
                .map(|f| f.path().to_path_buf())
        });
        let picked = self.picker.as_ref().and_then(|rx| match rx.try_recv() {
            Ok(p) => Some(p),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(None),
        });
        if picked.is_some() {
            self.picker = None;
        }
        if let Some(path) = dropped.or(picked.flatten()).or(self.harness.take_open()) {
            if self.tuner.unsaved() {
                // Unsaved tuning results: ask about them first; the file opens once that is settled.
                self.tuner.ask_before_open(path);
            } else {
                self.open(&ctx, path);
                ctx.request_repaint();
                return;
            }
        }
        if ctx.input(|i| i.viewport().close_requested())
            && self.tuner.unsaved()
            && !self.harness.finished()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.tuner.ask_before_quit();
        }
        if self.harness.take_tune_window() {
            self.tuner.open_for(&ctx, self.path.as_deref());
        }
        let tuned = self.tuner.show(&ctx);
        if tuned.quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        let open_after = tuned.open_file;
        if let Some(t) = tuned.apply {
            self.config.tuning_prompted = Some(true);
            self.config.reader.readers = Some(t.readers);
            self.config.reader.region_mb = Some(t.region_mb);
            self.config.reader.in_flight = Some(t.in_flight);
            config::save(&self.config);
            self.base.opts.readers = t.readers;
            self.base.opts.region_mb = t.region_mb;
            self.base.opts.in_flight = t.in_flight;
        }
        if let Some(path) = open_after {
            self.open(&ctx, path);
            ctx.request_repaint();
            return;
        }
        // The first-run tuning finished or was closed: now load the file.
        if self.pending_load && self.tune_first && !self.tuner.open {
            self.start_load(&ctx);
        }
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
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

        if let Some(rx) = &self.rx {
            match rx.try_recv() {
                Ok(Ok(p)) => {
                    if p.stats.samples_with_period == 0 {
                        self.cost = Cost::Samples;
                    }
                    self.proc_rows = filter::processes(&p, self.cost);
                    self.funcs = Some(Functions::new(&p));
                    self.summary = SummaryInfo::new(&p);
                    self.summary.load = {
                        let log = self.log.lock().unwrap();
                        let read = log
                            .entries
                            .iter()
                            .find(|e| e.name == PHASES[0].0)
                            .and_then(|e| e.end.map(|end| (end - e.start).as_secs_f32()));
                        read.map(|read_secs| LoadInfo {
                            file_bytes: self.file_size,
                            read_secs,
                            total_secs: self.load_started.elapsed().as_secs_f32(),
                        })
                    };
                    if let (Some(ms), Some(first)) =
                        (self.pending_range_ms.take(), p.samples.first())
                    {
                        self.range = Some(TimeRange {
                            start: first.time + (ms.start_ms * 1e6) as u64,
                            end: first.time + (ms.end_ms * 1e6) as u64,
                        });
                    }
                    // `--thread` alone: find the thread's process so the selectors agree.
                    if let (Some(t), None) = (self.sel_tid, self.sel_pid) {
                        self.sel_pid = p.samples.iter().find(|s| s.tid == t).map(|s| s.pid);
                    }
                    self.profile = Some(Arc::new(p));
                    self.rx = None;
                }
                Ok(Err(e)) => {
                    self.error = Some(e);
                    self.rx = None;
                }
                Err(_) => {}
            }
        }

        let Some(profile) = self.profile.clone() else {
            egui::CentralPanel::default().show(ui, |ui| {
                if self.path.is_none() {
                    self.welcome_screen(ui, &ctx);
                } else if self.pending_load {
                    self.tune_prompt_screen(ui, &ctx);
                } else if let Some(e) = self.error.clone() {
                    ui.add_space(40.0);
                    ui.colored_label(
                        pal.suspect(),
                        format!("Failed to load {}: {e}", self.path_text()),
                    );
                    ui.add_space(8.0);
                    if ui.button("Open another file…").clicked() {
                        self.pick_file(&ctx);
                    }
                    ui.weak("or drop a file on the window");
                } else {
                    self.loading_screen(ui);
                    ctx.request_repaint_after(Duration::from_millis(100));
                    self.harness
                        .poll_loading(&ctx, self.load_started.elapsed().as_secs_f32());
                }
            });
            return;
        };
        let p: &Profile = &profile;
        self.frames_since_load += 1;

        self.harness.poll_screen(&ctx, self.frames_since_load);

        // ---- controls: view, filters, search ----
        let mut filter_changed = false;
        egui::Panel::top("top").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.strong("perf-viz");
                if ui.button("Open…").on_hover_text("Open another file (or drop one on the window)").clicked() {
                    self.pick_file(&ctx);
                }
                ui.label(self.path_text());
                ui.separator();
                for (t, label) in TABS {
                    ui.selectable_value(&mut self.tab, t, label);
                }
                ui.separator();
                // Stays open while settings are changed; closes on a click outside.
                egui::containers::menu::MenuButton::new("Options")
                    .config(egui::containers::menu::MenuConfig::new().close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside))
                    .ui(ui, |ui| {
                        ui.set_min_width(230.0);
                        if p.stats.samples_with_period > 0 {
                            ui.label(RichText::new("Cost").weak());
                            let before = self.cost;
                            ui.radio_value(&mut self.cost, Cost::Samples, "Samples").on_hover_text("Count each sample once");
                            ui.radio_value(&mut self.cost, Cost::Cycles, cycles_label(p)).on_hover_text("Weigh each sample by its period: the events (cycles) it stands for");
                            if self.cost != before {
                                self.config.cost = Some(self.cost);
                                config::save(&self.config);
                                self.proc_rows = filter::processes(p, self.cost);
                                self.totals_key = None;
                            }
                            ui.separator();
                        }
                        ui.label(RichText::new("Theme").weak());
                        let mut pref = ctx.options(|o| o.theme_preference);
                        let before = pref;
                        ui.radio_value(&mut pref, egui::ThemePreference::System, "System").on_hover_text("Follow the desktop's light/dark setting (dark if it has none)");
                        ui.radio_value(&mut pref, egui::ThemePreference::Light, "Light");
                        ui.radio_value(&mut pref, egui::ThemePreference::Dark, "Dark");
                        if pref != before {
                            ctx.set_theme(pref);
                            self.config.theme = Some(theme_setting(pref));
                            config::save(&self.config);
                        }
                        ui.separator();
                        ui.label(RichText::new("Call trees").weak());
                        if ui.checkbox(&mut self.truncated, "Truncate suspect tails").on_hover_text("Drop the frames from the first invalid return address out to the root").changed() {
                            self.items_key = None;
                            self.totals_key = None;
                            self.config.truncate_suspect_tails = Some(self.truncated);
                            config::save(&self.config);
                        }
                        ui.separator();
                        self.reader_options(ui);
                        ui.separator();
                        if ui.button("Tune reader…").on_hover_text("Measure the disk and find good reader settings").clicked() {
                            self.tuner.open_for(&ctx, self.path.as_deref());
                        }
                    });
                ui.separator();
                let (r, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
                ui.painter().rect_filled(r, 2.0, Color32::from_rgb(200, 60, 60)); // the same red as the boxes
                ui.label("suspect tail: from the first invalid return address out to the root");
            });
            ui.horizontal_wrapped(|ui| {
                // Process selector
                let proc_text = match self.sel_pid {
                    None => "All processes".to_string(),
                    Some(pid) => format!("{} ({pid})", p.comm.get(&pid).map_or("?", String::as_str)),
                };
                ui.label("Process:");
                egui::ComboBox::from_id_salt("proc").width(230.0).selected_text(proc_text).show_ui(ui, |ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.proc_query).hint_text("filter by name or pid"));
                    if ui.selectable_label(self.sel_pid.is_none(), "All processes").clicked() {
                        (self.sel_pid, self.sel_tid) = (None, None);
                        filter_changed = true;
                    }
                    let q = self.proc_query.to_lowercase();
                    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        let mut shown = 0;
                        for r in &self.proc_rows {
                            if shown >= MAX_LIST {
                                ui.weak(format!("…refine the filter to see more of {} processes", self.proc_rows.len()));
                                break;
                            }
                            if q.is_empty() || r.comm.to_lowercase().contains(&q) || r.pid.to_string().contains(&q) {
                                shown += 1;
                                let label = format!("{} ({}) — {} {}", r.comm, r.pid, self.cost.format(r.cost), noun(p, self.cost));
                                if ui.selectable_label(self.sel_pid == Some(r.pid), label).clicked() {
                                    (self.sel_pid, self.sel_tid) = (Some(r.pid), None);
                                    filter_changed = true;
                                }
                            }
                        }
                    });
                });
                // Thread selector, once a process is chosen
                if let Some(pid) = self.sel_pid {
                    ui.label("Thread:");
                    let text = self.sel_tid.map_or("All threads".to_string(), |t| t.to_string());
                    egui::ComboBox::from_id_salt("thread").width(140.0).selected_text(text).show_ui(ui, |ui| {
                        if ui.selectable_label(self.sel_tid.is_none(), "All threads").clicked() {
                            self.sel_tid = None;
                            filter_changed = true;
                        }
                        egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                            for (tid, n) in filter::threads(p, pid, self.cost).into_iter().take(MAX_LIST) {
                                if ui.selectable_label(self.sel_tid == Some(tid), format!("{tid} — {} {}", self.cost.format(n), noun(p, self.cost))).clicked() {
                                    self.sel_tid = Some(tid);
                                    filter_changed = true;
                                }
                            }
                        });
                    });
                }
                ui.separator();
                ui.label("Search:");
                ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("function name").desired_width(200.0));
                if !self.search.is_empty() && ui.small_button("Clear").clicked() {
                    self.search.clear();
                }
            });
        });
        if self.tab != self.saved_tab {
            self.saved_tab = self.tab;
            self.config.tab = Some(self.tab.name());
            config::save(&self.config);
        }
        if filter_changed {
            self.totals_key = None;
        }

        // ---- recompute what depends on the view / filter / search ----
        let filter = self.filter();
        // The trie must be chosen *after* the controls above, which can change `truncated`: totals
        // are per node of the chosen trie, so a stale trie would index them out of range.
        let view = self.view();
        let trie: &Trie = if self.truncated {
            &p.truncated
        } else {
            &p.full
        };
        let cost = self.cost;
        let unit = noun(p, cost);
        let totals_key = TotalsKey {
            truncated: self.truncated,
            filter,
            range: self.range,
            cost,
        };
        if self.totals_key != Some(totals_key) {
            self.totals = filter::totals_in(p, self.truncated, filter, self.range, cost);
            self.grand_total = if filter == Filter::All && self.range.is_none() {
                self.totals[ROOT as usize]
            } else {
                filter::totals_in(p, self.truncated, Filter::All, None, cost)[ROOT as usize]
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
        let search_key = SearchKey {
            query: self.search.clone(),
            totals_version: self.totals_version,
        };
        if self.search_applied.as_ref() != Some(&search_key) {
            self.matches = filter::matching_frames(p, &self.search);
            self.coverage = filter::search_coverage(p, self.truncated, &self.totals, &self.matches);
            self.search_applied = Some(search_key);
        }
        let totals = &self.totals;
        let total = totals[ROOT as usize].max(1);
        let searching = !self.search.is_empty();
        let filtered = filter != Filter::All || self.range.is_some();

        // ---- timeline: build its data, and show the compact strip on every tab but its own ----
        let tl_bins = (((ui.available_width() - TL_LABEL_W - 10.0) / 2.0).max(60.0)) as usize;
        let timeline_current = self
            .tl
            .as_ref()
            .is_some_and(|t| t.filter == filter && t.bins == tl_bins && t.cost == cost);
        if !timeline_current {
            self.tl = Some(TimelineCache {
                filter,
                bins: tl_bins,
                cost,
                timeline: timeline::build(p, filter, tl_bins, 12, cost),
            });
        }
        let mut tl_actions: Vec<timeline::Action> = Vec::new();
        if self.tab != Tab::Timeline {
            tl_actions = egui::Panel::top("timeline_strip")
                .show(ui, |ui| {
                    let o = timeline::Options {
                        tl: &self.tl.as_ref().unwrap().timeline,
                        range: self.range,
                        samples_in_range: self.range.map(|_| totals[ROOT as usize]),
                        cost,
                        noun: unit,
                        strip_h: 44.0,
                        show_lanes: false,
                        comm: &p.comm,
                        selected_tid: self.sel_tid,
                        label_w: TL_LABEL_W,
                    };
                    timeline::draw(ui, &o, &mut self.drag)
                })
                .inner;
        }

        // The breadcrumb and search summary belong to the flame graph's zoom.
        if self.tab == Tab::FlameGraph {
            egui::Panel::top("crumbs").show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    let mut path = Vec::new();
                    let mut n = self.focus[view];
                    while n != ROOT {
                        path.push(n);
                        n = trie.nodes[n as usize].parent;
                    }
                    if ui.small_button("all").clicked() {
                        self.focus[view] = ROOT;
                    }
                    for &n in path.iter().rev() {
                        ui.label(">");
                        let name = &p.frames[trie.nodes[n as usize].frame as usize].name;
                        let short: String = name.chars().take(28).collect();
                        if ui.small_button(short).clicked() {
                            self.focus[view] = n;
                        }
                    }
                    ui.weak("(click a box to zoom, Esc to zoom out)");
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

        // ---- status bar ----
        egui::Panel::bottom("status").show(ui, |ui| {
            let st = &p.stats;
            ui.horizontal_wrapped(|ui| {
                if !filtered {
                    ui.label(format!("{} {unit} in view", cost.format(totals[ROOT as usize])));
                } else {
                    ui.label(format!("{} of {} {unit} (filtered)", cost.format(totals[ROOT as usize]), cost.format(self.grand_total)));
                }
                ui.separator();
                ui.label(format!("{} nodes", trie.nodes.len()));
                ui.separator();
                ui.label(format!("suspect tails: {}% of samples", fmt::pct_of(st.suspect_samples as f64, st.samples as f64)));
                ui.separator();
                if st.lost_events > 0 {
                    let seen = st.samples + st.lost_events;
                    ui.colored_label(pal.warning(), format!("kernel dropped {} events ({}% of {})", st.lost_events, fmt::pct_of(st.lost_events as f64, seen as f64), seen));
                    ui.separator();
                }
                ui.label(format!("symbols: {} DWARF / {} ELF-only / {} raw modules, {} of {} addresses unresolved", st.modules_full, st.modules_symbols_only, st.modules_raw, st.unresolved_addresses, st.unique_addresses));
                if self.tab == Tab::FlameGraph {
                    ui.separator();
                    ui.label(format!("{} boxes", self.items.len()));
                }
            });
        });

        // ---- the selected view ----
        egui::CentralPanel::default().show(ui, |ui| {
            let (query, funcs) = (
                self.search.to_lowercase(),
                self.funcs.as_ref().expect("set when the profile arrived"),
            );
            match self.tab {
                Tab::TopDown => {
                    return draw_top_down(
                        ui,
                        p,
                        trie,
                        totals,
                        &self.selfs,
                        cost,
                        &self.matches,
                        searching,
                        &mut self.td,
                        view,
                        self.totals_version,
                    );
                }
                Tab::BottomUp => {
                    return draw_bottom_up(
                        ui,
                        p,
                        funcs,
                        trie,
                        totals,
                        &self.selfs,
                        cost,
                        &query,
                        &mut self.bu,
                        view,
                        self.totals_version,
                    );
                }
                Tab::CallerCallee => {
                    return draw_caller_callee(
                        ui,
                        p,
                        funcs,
                        trie,
                        totals,
                        &self.costs,
                        cost,
                        &query,
                        &mut self.cc,
                        self.totals_version,
                    );
                }
                Tab::Timeline => {
                    let o = timeline::Options {
                        tl: &self.tl.as_ref().unwrap().timeline,
                        range: self.range,
                        samples_in_range: self.range.map(|_| totals[ROOT as usize]),
                        cost,
                        noun: unit,
                        strip_h: 96.0,
                        show_lanes: true,
                        comm: &p.comm,
                        selected_tid: self.sel_tid,
                        label_w: TL_LABEL_W,
                    };
                    let actions = egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| timeline::draw(ui, &o, &mut self.drag))
                        .inner;
                    tl_actions.extend(actions);
                    return;
                }
                Tab::Summary => {
                    if let Some(f) = draw_summary(
                        ui,
                        p,
                        funcs,
                        &self.costs,
                        cost,
                        totals[ROOT as usize],
                        self.grand_total,
                        &self.summary,
                        filtered,
                    ) {
                        // A hotspot was clicked: show it in the caller/callee view.
                        self.cc.selected = Some(f);
                        self.tab = Tab::CallerCallee;
                    }
                    return;
                }
                Tab::FlameGraph => {}
            }
            if ui.input(|i| i.key_pressed(Key::Escape)) && self.focus[view] != ROOT {
                self.focus[view] = trie.nodes[self.focus[view] as usize].parent;
            }
            let focus = self.focus[view];
            let avail = ui.available_size();
            let width = (avail.x - 14.0).max(50.0);

            let key = ItemsKey {
                truncated: self.truncated,
                focus,
                width: width as i32,
                totals_version: self.totals_version,
            };
            if self.items_key != Some(key) {
                flame::layout(trie, totals, focus, width, MIN_BOX_PX, 400, &mut self.items);
                self.items_key = Some(key);
            }
            let rows = self
                .items
                .iter()
                .map(|i| i.depth)
                .max()
                .map_or(1, |d| d + 1);
            let content_h = (rows as f32 * ROW_H + 8.0).max(avail.y);

            let mut clicked_node = None;
            // The graph grows upward from the root at the bottom, so when it is taller than the window it
            // must start scrolled down to the root. `stick_to_bottom` only keeps an offset that is already at
            // the bottom, so the offset is set whenever the view changes (a new file, zoom, filter, tree).
            let mut area = egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .auto_shrink([false, false]);
            let scroll_key = ScrollKey {
                truncated: self.truncated,
                focus,
                totals_version: self.totals_version,
            };
            if self.flame_scroll_key != Some(scroll_key) {
                area = area.vertical_scroll_offset((content_h - avail.y).max(0.0));
                self.flame_scroll_key = Some(scroll_key);
            }
            area.show(ui, |ui| {
                let (resp, painter) = ui.allocate_painter(vec2(width, content_h), Sense::click());
                let rect = resp.rect;
                let bottom = rect.bottom() - 2.0;

                let hovered = resp.hover_pos().and_then(|pos| {
                    let d = ((bottom - pos.y) / ROW_H).floor();
                    (d >= 0.0)
                        .then(|| flame::hit(&self.items, pos.x - rect.left(), d as u32))
                        .flatten()
                        .copied()
                });

                for it in &self.items {
                    let y1 = bottom - it.depth as f32 * ROW_H;
                    let r = Rect::from_min_max(
                        pos2(rect.left() + it.x, y1 - ROW_H + 1.0),
                        pos2(rect.left() + it.x + it.w, y1),
                    );
                    if !painter.clip_rect().intersects(r) {
                        continue;
                    }
                    let node = trie.nodes[it.node as usize];
                    let (mut fill, label, matched): (Color32, String, bool) =
                        if node.frame == u32::MAX {
                            (pal.root_box(), "all samples".into(), false)
                        } else {
                            let f = &p.frames[node.frame as usize];
                            (
                                frame_color(p, f),
                                f.name.clone(),
                                searching && self.matches[node.frame as usize],
                            )
                        };
                    if searching && !matched && node.frame != u32::MAX {
                        fill = pal.dim(fill);
                    }
                    painter.rect_filled(r, 0.0, fill);
                    painter.rect_stroke(
                        r,
                        0.0,
                        Stroke::new(0.5, Color32::from_black_alpha(90)),
                        StrokeKind::Inside,
                    );
                    if matched {
                        painter.rect_stroke(
                            r,
                            0.0,
                            Stroke::new(2.0, pal.highlight()),
                            StrokeKind::Inside,
                        );
                    }
                    if it.w >= LABEL_MIN_PX {
                        painter.with_clip_rect(r.shrink2(vec2(2.0, 0.0))).text(
                            pos2(r.left() + 3.0, r.center().y),
                            Align2::LEFT_CENTER,
                            label,
                            FontId::proportional(11.0),
                            Color32::BLACK,
                        );
                    }
                }

                if let Some(h) = hovered {
                    let y1 = bottom - h.depth as f32 * ROW_H;
                    let r = Rect::from_min_max(
                        pos2(rect.left() + h.x, y1 - ROW_H + 1.0),
                        pos2(rect.left() + h.x + h.w, y1),
                    );
                    painter.rect_stroke(
                        r,
                        0.0,
                        Stroke::new(1.5, pal.hover_outline()),
                        StrokeKind::Inside,
                    );
                    let node = trie.nodes[h.node as usize];
                    let node_total = totals[h.node as usize];
                    let children_total: u64 = trie
                        .children(h.node)
                        .iter()
                        .map(|&c| totals[c as usize])
                        .sum();
                    let self_count = node_total.saturating_sub(children_total);
                    resp.clone().on_hover_ui_at_pointer(|ui| {
                        if node.frame == u32::MAX {
                            ui.strong("all samples");
                        } else {
                            let f = &p.frames[node.frame as usize];
                            ui.strong(&f.name);
                            ui.label(format!("module: {}", p.module_name(f.module)));
                            if let Some(file) = &f.file {
                                ui.label(format!("{}:{}", file, f.line.unwrap_or(0)));
                            }
                            if f.suspect {
                                ui.colored_label(
                                    pal.suspect(),
                                    "suspect tail: at or beyond the first invalid return address",
                                );
                            }
                            if f.raw {
                                ui.label("no symbol found");
                            }
                        }
                        let parent_total = if node.parent == u32::MAX {
                            node_total
                        } else {
                            totals[node.parent as usize]
                        };
                        ui.label(format!(
                            "{} {unit} inclusive ({}% of the view, {}% of parent)",
                            cost.format(node_total),
                            fmt::pct_of(node_total as f64, total as f64),
                            fmt::pct_of(node_total as f64, parent_total as f64)
                        ));
                        ui.label(format!("{} {unit} self", cost.format(self_count)));
                    });
                    if resp.clicked() {
                        clicked_node = Some(h.node);
                    }
                }
            });
            if let Some(n) = clicked_node {
                self.focus[view] = n;
            }
        });
        for a in tl_actions {
            match a {
                timeline::Action::SetRange(range) => self.range = Some(range),
                timeline::Action::ClearRange => self.range = None,
                timeline::Action::SelectThread { pid, tid } => {
                    self.sel_pid = Some(pid);
                    self.sel_tid = Some(tid);
                }
            }
        }
    }
}

/// The top-down call tree: every frame with inclusive and self cost, heaviest first.
#[allow(clippy::too_many_arguments)]
fn draw_top_down(
    ui: &mut egui::Ui,
    p: &Profile,
    trie: &Trie,
    totals: &[u64],
    selfs: &[u64],
    cost: Cost,
    matches: &[bool],
    searching: bool,
    st: &mut TdState,
    view: usize,
    version: u64,
) {
    // Until the user has chosen, show the hottest path expanded, as a useful starting point.
    if !st.touched[view] {
        st.expanded[view] = views::hottest_path(trie, totals, 0.3).into_iter().collect();
        st.touched[view] = true;
        st.version += 1;
    }
    ui.horizontal(|ui| {
        if ui.button("Collapse all").clicked() {
            st.expanded[view].clear();
            st.version += 1;
        }
        if ui.button("Expand hottest path").clicked() {
            st.expanded[view] = views::hottest_path(trie, totals, 0.3).into_iter().collect();
            st.version += 1;
        }
        if ui.button("Expand 6 levels").clicked() {
            st.expanded[view] = views::expand_to_depth(trie, totals, 6);
            st.version += 1;
        }
        ui.weak(format!(
            "{} rows shown; click the arrow (or double-click) to expand",
            st.rows.len()
        ));
    });
    let key = RowsKey {
        view,
        data_version: version,
        expansion_version: st.version,
    };
    if st.rows_key != Some(key) {
        st.rows = views::top_down_rows(trie, totals, &st.expanded[view]);
        st.rows_key = Some(key);
    }
    let selected = st.selected[view];
    let rows = &st.rows;
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
            let r = rows[i];
            let node = trie.nodes[r.node as usize];
            let f = &p.frames[node.frame as usize];
            RowData {
                depth: r.depth,
                arrow: if !r.expandable {
                    Arrow::None
                } else if r.expanded {
                    Arrow::Expanded
                } else {
                    Arrow::Collapsed
                },
                name: &f.name,
                module: p.module_name(f.module),
                cost: totals[r.node as usize],
                self_cost: Some(selfs[r.node as usize]),
                highlight: searching && matches[node.frame as usize],
                suspect: f.suspect,
                selected: selected == Some(r.node),
            }
        },
    );
    match action {
        Some(Click::Toggle(i)) => {
            let n = st.rows[i].node;
            if !st.expanded[view].remove(&n) {
                st.expanded[view].insert(n);
            }
            st.version += 1;
        }
        Some(Click::Select(i)) => st.selected[view] = Some(st.rows[i].node),
        Some(Click::SortBy(_)) | None => {}
    }
}

/// The bottom-up tree: functions ranked by self cost; expanding one shows who called it.
#[allow(clippy::too_many_arguments)]
fn draw_bottom_up(
    ui: &mut egui::Ui,
    p: &Profile,
    funcs: &Functions,
    trie: &Trie,
    totals: &[u64],
    selfs: &[u64],
    cost: Cost,
    query: &str,
    st: &mut BuState,
    view: usize,
    version: u64,
) {
    ui.horizontal(|ui| {
        if ui.button("Collapse all").clicked() {
            st.expanded.clear();
            st.version += 1;
        }
        ui.weak(format!("{} rows shown; each function's cost is the {} that end in it; expand to see its callers", st.rows.len(), noun(p, cost)));
    });
    let key = RowsKey {
        view,
        data_version: version,
        expansion_version: st.version,
    };
    if st.rows_key != Some(key) {
        st.rows = views::bu_rows(trie, funcs, selfs, &st.expanded);
        st.rows_key = Some(key);
    }
    let rows = &st.rows;
    let selected = st.selected.clone();
    let action = table::draw(
        ui,
        &Columns {
            unit: cost,
            name: "Function",
            cost: "Cost",
            self_cost: None,
            sort: None,
        },
        rows.len(),
        totals[ROOT as usize],
        |i| {
            let r = &rows[i];
            let f = &funcs.funcs[r.func as usize];
            RowData {
                depth: r.path.len() as u32 - 1,
                arrow: if !r.has_callers {
                    Arrow::None
                } else if r.expanded {
                    Arrow::Expanded
                } else {
                    Arrow::Collapsed
                },
                name: &f.name,
                module: p.module_name(f.module),
                cost: r.cost,
                self_cost: None,
                highlight: !query.is_empty() && f.name.to_lowercase().contains(query),
                suspect: false,
                selected: selected.as_ref() == Some(&r.path),
            }
        },
    );
    match action {
        Some(Click::Toggle(i)) => {
            let path = st.rows[i].path.clone();
            if !st.expanded.remove(&path) {
                st.expanded.insert(path);
            }
            st.version += 1;
        }
        Some(Click::Select(i)) => st.selected = Some(st.rows[i].path.clone()),
        Some(Click::SortBy(_)) | None => {}
    }
}

/// A table row for a function with a cost, with no tree structure.
fn func_row<'a>(
    p: &'a Profile,
    funcs: &'a Functions,
    f: u32,
    cost: u64,
    self_cost: Option<u64>,
    selected: bool,
) -> RowData<'a> {
    let func = &funcs.funcs[f as usize];
    RowData {
        depth: 0,
        arrow: Arrow::None,
        name: &func.name,
        module: p.module_name(func.module),
        cost,
        self_cost,
        highlight: false,
        suspect: false,
        selected,
    }
}

/// Every function with its inclusive and self cost; selecting one lists its callers and callees below.
#[allow(clippy::too_many_arguments)]
fn draw_caller_callee(
    ui: &mut egui::Ui,
    p: &Profile,
    funcs: &Functions,
    trie: &Trie,
    totals: &[u64],
    costs: &views::Costs,
    cost: Cost,
    query: &str,
    st: &mut CcState,
    version: u64,
) {
    if let Some(f) = st.selected {
        if !st
            .lists
            .as_ref()
            .is_some_and(|l| l.func == f && l.version == version)
        {
            st.lists = Some(CallLists {
                func: f,
                version,
                callers: views::callers_of(trie, totals, funcs, f),
                callees: views::callees_of(trie, totals, funcs, f),
            });
        }
    }
    let mut next: Option<u32> = None;

    egui::Panel::bottom("cc_lists")
        .resizable(true)
        .default_size(250.0)
        .min_size(100.0)
        .show(ui, |ui| match (&st.selected, &st.lists) {
            (
                Some(f),
                Some(CallLists {
                    callers, callees, ..
                }),
            ) => {
                let (name, incl) = (&funcs.funcs[*f as usize].name, costs.inclusive[*f as usize]);
                ui.horizontal(|ui| {
                    ui.strong(name.chars().take(90).collect::<String>());
                    ui.weak(format!(
                        "inclusive {}, self {} {}; percentages are of the inclusive cost",
                        cost.format(incl),
                        cost.format(costs.self_[*f as usize]),
                        noun(p, cost)
                    ));
                });
                ui.columns(2, |cols| {
                    cols[0].strong("Callers (click to select)");
                    let click = table::draw(
                        &mut cols[0],
                        &Columns {
                            unit: cost,
                            name: "Caller",
                            cost: "Cost",
                            self_cost: None,
                            sort: None,
                        },
                        callers.len(),
                        incl,
                        |i| func_row(p, funcs, callers[i].id, callers[i].cost, None, false),
                    );
                    if let Some(Click::Select(i) | Click::Toggle(i)) = click {
                        next = Some(callers[i].id);
                    }
                    cols[1].strong("Callees (click to select)");
                    let click = table::draw(
                        &mut cols[1],
                        &Columns {
                            unit: cost,
                            name: "Callee",
                            cost: "Cost",
                            self_cost: None,
                            sort: None,
                        },
                        callees.len(),
                        incl,
                        |i| func_row(p, funcs, callees[i].id, callees[i].cost, None, false),
                    );
                    if let Some(Click::Select(i) | Click::Toggle(i)) = click {
                        next = Some(callees[i].id);
                    }
                });
            }
            _ => {
                ui.add_space(8.0);
                ui.weak("Select a function above to see who calls it and what it calls.");
            }
        });

    ui.horizontal(|ui| {
        ui.weak(format!("{} functions; click the Inclusive or Self header to sort; the search box filters by name", st.list.len()));
    });
    let key = ListKey {
        version,
        sort: st.sort,
        query: query.to_string(),
    };
    if st.list_key.as_ref() != Some(&key) {
        st.list = views::ranked_functions(costs, funcs, st.sort, query);
        st.list_key = Some(key);
    }
    let (list, selected) = (&st.list, st.selected);
    let click = table::draw(
        ui,
        &Columns {
            unit: cost,
            name: "Function",
            cost: "Inclusive",
            self_cost: Some("Self"),
            sort: Some(if st.sort == views::SortBy::Inclusive {
                0
            } else {
                1
            }),
        },
        list.len(),
        totals[ROOT as usize],
        |i| {
            let f = list[i];
            func_row(
                p,
                funcs,
                f,
                costs.inclusive[f as usize],
                Some(costs.self_[f as usize]),
                selected == Some(f),
            )
        },
    );
    match click {
        Some(Click::Select(i) | Click::Toggle(i)) => next = Some(st.list[i]),
        Some(Click::SortBy(col)) => {
            st.sort = if col == 0 {
                views::SortBy::Inclusive
            } else {
                views::SortBy::Self_
            }
        }
        None => {}
    }
    if next.is_some() {
        st.selected = next;
    }
}

/// An overview: what the recording is, the hottest functions and modules. Returns a function to
/// show in the caller/callee view if one of the hotspots was clicked.
fn draw_summary(
    ui: &mut egui::Ui,
    p: &Profile,
    funcs: &Functions,
    costs: &views::Costs,
    cost: Cost,
    total: u64,
    grand_total: u64,
    info: &SummaryInfo,
    filtered: bool,
) -> Option<u32> {
    let mut nav = None;
    let st = &p.stats;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(6.0);
        ui.heading("Summary");
        ui.add_space(4.0);
        egui::Grid::new("summary_grid").num_columns(2).spacing([28.0, 4.0]).show(ui, |ui| {
            let mut row = |k: &str, v: String| {
                ui.label(RichText::new(k).weak());
                ui.label(v);
                ui.end_row();
            };
            let with_stacks = st.samples - st.samples_without_stack;
            let unit = noun(p, cost);
            let _ = with_stacks;
            row(if cost == Cost::Samples { "Samples" } else { "Cost" }, if !filtered { format!("{} {unit}", cost.format(total)) } else { format!("{} {unit} in the current filter (process, thread or time range), of {} in the recording", cost.format(total), cost.format(grand_total)) });
            if let Some(load) = info.load {
                let (bytes, read_s, total_s) = (load.file_bytes, load.read_secs, load.total_secs);
                let gb = bytes as f64 / 1e9;
                let size = if gb >= 1.0 { format!("{} GB", fmt::sig3(gb)) } else { format!("{} MB", fmt::sig3(bytes as f64 / 1e6)) };
                row("Read speed", format!("{size} read and unwound in {} s: {} GB/s (whole load, with symbols and call trees: {} s)", fmt::sig3(read_s as f64), fmt::sig3(gb / read_s.max(1e-6) as f64), fmt::sig3(total_s as f64)));
            }
            row("Recording", format!("{:.2} s, {} processes, {} threads", info.last_ns.saturating_sub(info.first_ns) as f64 / 1e9, info.processes, info.threads));
            if st.lost_events > 0 {
                row("Kernel drops", format!("{} events in {} LOST records ({}% of everything sampled): the profile under-counts", st.lost_events, st.lost_records, fmt::pct_of(st.lost_events as f64, (st.samples + st.lost_events) as f64)));
            }
            row("Suspect stack tails", format!("{} samples ({}%) have frames past the first invalid return address", st.suspect_samples, fmt::pct_of(st.suspect_samples as f64, st.samples as f64)));
            row("Symbols", format!("{} modules with DWARF, {} with ELF symbols only, {} without; {} of {} addresses unresolved ({}%)", st.modules_full, st.modules_symbols_only, st.modules_raw, st.unresolved_addresses, st.unique_addresses, fmt::pct_of(st.unresolved_addresses as f64, st.unique_addresses as f64)));
            row("Call tree", format!("{} functions, {} nodes (full), {} (truncated)", funcs.funcs.len(), p.full.nodes.len(), p.truncated.nodes.len()));
        });
        ui.add_space(12.0);
        ui.strong("Top hotspots by self cost (click one to see its callers and callees)");
        let top: Vec<u32> = views::ranked_functions(costs, funcs, views::SortBy::Self_, "").into_iter().take(12).collect();
        let h = (top.len() + 1) as f32 * table::ROW_H + 6.0;
        let click = ui
            .allocate_ui(vec2(ui.available_width(), h), |ui| table::draw(ui, &Columns { unit: cost, name: "Function", cost: "Self", self_cost: Some("Inclusive"), sort: None }, top.len(), total, |i| func_row(p, funcs, top[i], costs.self_[top[i] as usize], Some(costs.inclusive[top[i] as usize]), false)))
            .inner;
        if let Some(Click::Select(i) | Click::Toggle(i)) = click {
            nav = Some(top[i]);
        }
        ui.add_space(12.0);
        ui.strong("Self cost by module");
        let mods: Vec<Weighted> = views::module_self_costs(funcs, costs).into_iter().take(10).collect();
        let h = (mods.len() + 1) as f32 * table::ROW_H + 6.0;
        ui.allocate_ui(vec2(ui.available_width(), h), |ui| {
            table::draw(ui, &Columns { unit: cost, name: "Module", cost: "Self", self_cost: None, sort: None }, mods.len(), total, |i| RowData {
                depth: 0,
                arrow: Arrow::None,
                name: p.module_name(mods[i].id),
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

/// The name of what a cost counts: samples, or the profile's event unit (cycles, ns, ...).
fn noun(p: &Profile, cost: Cost) -> &str {
    match cost {
        Cost::Samples => "samples",
        Cost::Cycles => &p.stats.period_unit,
    }
}

/// Label of the weighted cost button, e.g. `Cycles`.
fn cycles_label(p: &Profile) -> String {
    let mut c = p.stats.period_unit.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
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
    let mut opts = load::Options::default();
    config.reader.apply(&mut opts);
    // Flags override the saved settings for this run.
    cli.reader.apply(&mut opts);
    let harness = Harness::from_cli(&cli);
    let show = cli.show;
    let args = Args {
        path: cli.file,
        truncated: show
            .view
            .map_or(config.truncate_suspect_tails.unwrap_or(false), |v| {
                v == cli::View::Truncated
            }),
        cost: show.cost.or(config.cost),
        theme: cli.theme.or(config.theme).map(theme_pref),
        tab: show
            .tab
            .or(config.tab)
            .map_or(Tab::FlameGraph, Tab::from_name),
        cold: opts.cold,
        opts,
        process: show.process,
        thread: show.thread,
        search: show.search.unwrap_or_default(),
        range_ms: show.range,
        harness,
        config,
    };
    // Maximized unless `--size` asks for a fixed window (which the screenshot checks rely on).
    let (size, maximized) = cli
        .size
        .map_or(([1500.0, 900.0], true), |s| ([s.width, s.height], false));
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
