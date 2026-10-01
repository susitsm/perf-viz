//! The command line of perf-viz.
//!
//! Flags override the saved settings (see [`crate::config`]) for one run and never change them. They
//! are grouped by what they are for: [`InitialView`] (what the window shows when it opens),
//! [`ReaderArgs`] (how the file is read) and, hidden and only with the `scripted` feature,
//! [`ScriptArgs`] (scripted input and screenshots for automated checks of the GUI).

use std::path::PathBuf;

#[cfg(any(test, feature = "scripted"))]
use clap::ArgMatches;
use clap::{Args, CommandFactory, FromArgMatches, Parser, ValueEnum};
use perf_profile::{load, symbolize::DwarfMode};

use crate::{
    config::{TabName, Theme},
    fmt::Cost,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum View {
    /// Every unwound frame; the suspect tail is drawn in red.
    Full,
    /// The same samples with the suspect tails cut off.
    Truncated,
}

/// A window size in points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowSize {
    pub width: f32,
    pub height: f32,
}

/// A time range in milliseconds after the first sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MsRange {
    pub start_ms: f64,
    pub end_ms: f64,
}

/// A position in the window, in points.
#[cfg(any(test, feature = "scripted"))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

/// A press at one point, a move to another, and a release.
#[cfg(any(test, feature = "scripted"))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DragPath {
    pub from: Point,
    pub to: Point,
}

/// A screenshot of the loading screen, taken this long after loading started.
#[cfg(any(test, feature = "scripted"))]
#[derive(Clone, Debug, PartialEq)]
pub struct LoadScreenshot {
    pub path: PathBuf,
    pub after_secs: f32,
}

/// One scripted input, in the order the flags were given.
#[cfg(any(test, feature = "scripted"))]
#[derive(Clone, Debug, PartialEq)]
pub enum ScriptStep {
    Click(Point),
    Hover(Point),
    Drag(DragPath),
    /// Behave as if this file was dropped on the window.
    Open(PathBuf),
    /// Do nothing for this many seconds.
    Pause(f64),
    /// Open the reader tuner window (what Options → Tune reader… does).
    TuneWindow,
}

/// What the window shows when it opens. Anything not given falls back to the saved settings.
#[derive(Args, Debug, Default)]
#[command(next_help_heading = "Initial view")]
pub struct InitialView {
    /// Which call tree to show.
    #[arg(long, value_enum)]
    pub view: Option<View>,

    /// What the views add up.
    #[arg(long, value_enum)]
    pub cost: Option<Cost>,

    /// The tab to open on.
    #[arg(long, value_enum)]
    pub tab: Option<TabName>,

    /// Show only this process.
    #[arg(long, value_name = "PID")]
    pub process: Option<i32>,

    /// Show only this thread.
    #[arg(long, value_name = "TID")]
    pub thread: Option<i32>,

    /// Highlight functions whose name contains this text.
    #[arg(long, value_name = "TEXT")]
    pub search: Option<String>,

    /// Select a time range, in milliseconds after the first sample.
    #[arg(long, value_name = "START,END", value_parser = parse_range)]
    pub range: Option<MsRange>,
}

/// How the file is read. Anything not given falls back to the saved `[reader]` settings.
#[derive(Args, Debug, Default)]
#[command(next_help_heading = "Reader")]
pub struct ReaderArgs {
    /// Threads reading the file in parallel.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u64).range(1..=64))]
    pub readers: Option<u64>,

    /// Threads parsing and unwinding.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u64).range(1..=512))]
    pub threads: Option<u64>,

    /// Size of one file region, in MiB.
    #[arg(long, value_name = "MIB", value_parser = clap::value_parser!(u64).range(1..=8192))]
    pub region_mb: Option<u64>,

    /// Skip DWARF: function names only, but faster to load.
    #[arg(long)]
    pub no_dwarf: bool,

    /// Drop the file from the page cache before loading (to time a cold read).
    #[arg(long)]
    pub cold: bool,
}

impl ReaderArgs {
    /// Put the given flags on top of `opts`.
    pub fn apply(&self, opts: &mut load::Options) {
        if let Some(v) = self.readers {
            opts.readers = v as usize;
        }
        if let Some(v) = self.threads {
            opts.workers = v as usize;
        }
        if let Some(v) = self.region_mb {
            opts.region_mb = v;
        }
        if self.no_dwarf {
            opts.dwarf = DwarfMode::Never;
        }
        if self.cold {
            opts.cold = true;
        }
    }
}

/// Scripted input and screenshots, for checking the GUI without a person. Hidden from `--help`.
#[cfg(any(test, feature = "scripted"))]
#[derive(Args, Debug, Default)]
pub struct ScriptArgs {
    /// Save a screenshot once everything is loaded (and any scripted input has run), then quit.
    #[arg(long, value_name = "PNG", hide = true)]
    pub screenshot: Option<PathBuf>,

    /// Save a screenshot this many seconds into loading, then quit.
    #[arg(long, num_args = 2, value_names = ["PNG", "SECS"], hide = true)]
    load_screenshot: Option<Vec<String>>,

    /// Click at window coordinates in points (screenshot pixels divided by the scale factor).
    #[arg(long, value_name = "X,Y", value_parser = parse_xy, action = clap::ArgAction::Append, hide = true)]
    click: Vec<Point>,

    #[arg(long, value_name = "X,Y", value_parser = parse_xy, action = clap::ArgAction::Append, hide = true)]
    hover: Vec<Point>,

    #[arg(long, value_name = "X0,Y0,X1,Y1", value_parser = parse_drag, action = clap::ArgAction::Append, hide = true)]
    drag: Vec<DragPath>,

    #[arg(long, value_name = "FILE", action = clap::ArgAction::Append, hide = true)]
    open: Vec<PathBuf>,

    #[arg(long, value_name = "SECS", action = clap::ArgAction::Append, hide = true)]
    pause: Vec<f64>,

    /// Open the tuner window without clicking for it.
    #[arg(long, hide = true)]
    tune_window: bool,

    /// The scripted inputs above, merged in command-line order.
    #[arg(skip)]
    order: Vec<ScriptStep>,
}

#[cfg(any(test, feature = "scripted"))]
impl ScriptArgs {
    /// The loading-screen screenshot asked for with `--load-screenshot PNG SECS`, if any.
    pub fn load_screenshot(&self) -> Option<LoadScreenshot> {
        let v = self.load_screenshot.as_ref()?;
        Some(LoadScreenshot {
            path: PathBuf::from(v.first()?),
            after_secs: v.get(1)?.parse().unwrap_or(2.0),
        })
    }

    /// The scripted inputs, in the order they were given on the command line.
    pub fn steps(&self) -> &[ScriptStep] {
        &self.order
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "perf-viz",
    version,
    about = "A viewer for perf.data recordings: flame graph, call trees, timeline.",
    after_help = "Settings (theme, cost, reader, ...) are saved in $XDG_CONFIG_HOME/perf-viz/config.toml (override the path with $PERF_VIZ_CONFIG); flags override them for one run."
)]
pub struct Cli {
    /// The recording to open. Without one, a start screen lets you choose a file or drop one on the window.
    pub file: Option<PathBuf>,

    /// Colour theme.
    #[arg(long, value_enum)]
    pub theme: Option<Theme>,

    /// Window size instead of starting maximized.
    #[arg(long, value_name = "WxH", value_parser = parse_size)]
    pub size: Option<WindowSize>,

    #[command(flatten)]
    pub show: InitialView,

    #[command(flatten)]
    pub reader: ReaderArgs,

    #[cfg(any(test, feature = "scripted"))]
    #[command(flatten)]
    pub script: ScriptArgs,
}

fn parse_size(s: &str) -> Result<WindowSize, String> {
    let (w, h) = s
        .split_once(['x', 'X'])
        .ok_or("expected WIDTHxHEIGHT, e.g. 1500x900")?;
    let (width, height): (f32, f32) = (
        w.trim().parse().map_err(|_| "bad width")?,
        h.trim().parse().map_err(|_| "bad height")?,
    );
    if width < 200.0 || height < 150.0 {
        return Err("the window must be at least 200x150".into());
    }
    Ok(WindowSize { width, height })
}

fn parse_range(s: &str) -> Result<MsRange, String> {
    let (a, b) = s
        .split_once(',')
        .ok_or("expected START,END in milliseconds")?;
    let (start_ms, end_ms): (f64, f64) = (
        a.trim().parse().map_err(|_| "bad start")?,
        b.trim().parse().map_err(|_| "bad end")?,
    );
    if end_ms <= start_ms {
        return Err("the end must be after the start".into());
    }
    Ok(MsRange { start_ms, end_ms })
}

#[cfg(any(test, feature = "scripted"))]
fn floats(s: &str, n: usize) -> Result<Vec<f32>, String> {
    let v: Vec<f32> = s
        .split(',')
        .map(|x| {
            x.trim()
                .parse::<f32>()
                .map_err(|_| format!("`{x}` is not a number"))
        })
        .collect::<Result<_, _>>()?;
    if v.len() == n {
        Ok(v)
    } else {
        Err(format!(
            "expected {n} comma-separated numbers, got {}",
            v.len()
        ))
    }
}

#[cfg(any(test, feature = "scripted"))]
fn parse_xy(s: &str) -> Result<Point, String> {
    let v = floats(s, 2)?;
    Ok(Point { x: v[0], y: v[1] })
}

#[cfg(any(test, feature = "scripted"))]
fn parse_drag(s: &str) -> Result<DragPath, String> {
    let v = floats(s, 4)?;
    Ok(DragPath {
        from: Point { x: v[0], y: v[1] },
        to: Point { x: v[2], y: v[3] },
    })
}

#[cfg(any(test, feature = "scripted"))]
/// Merge the scripted flags into one list in the order they appeared (each flag's values keep their
/// own order, and `indices_of` says where each one sat on the command line).
fn steps_in_order(m: &ArgMatches) -> Vec<ScriptStep> {
    fn collect<T: Clone + Send + Sync + 'static>(
        m: &ArgMatches,
        id: &str,
        f: impl Fn(T) -> ScriptStep,
        out: &mut Vec<(usize, ScriptStep)>,
    ) {
        if let (Some(vals), Some(idx)) = (m.get_many::<T>(id), m.indices_of(id)) {
            out.extend(idx.zip(vals).map(|(i, v)| (i, f(v.clone()))));
        }
    }
    let mut v = Vec::new();
    collect::<Point>(m, "click", ScriptStep::Click, &mut v);
    collect::<Point>(m, "hover", ScriptStep::Hover, &mut v);
    collect::<DragPath>(m, "drag", ScriptStep::Drag, &mut v);
    collect::<PathBuf>(m, "open", ScriptStep::Open, &mut v);
    collect::<f64>(m, "pause", ScriptStep::Pause, &mut v);
    if m.get_flag("tune_window") {
        v.extend(
            m.indices_of("tune_window")
                .into_iter()
                .flatten()
                .map(|i| (i, ScriptStep::TuneWindow)),
        );
    }
    v.sort_by_key(|s| s.0);
    v.into_iter().map(|s| s.1).collect()
}

impl Cli {
    /// Parse the process arguments (exits with a message on error, like any clap program).
    pub fn parse_args() -> Cli {
        Self::from_matches(Cli::command().get_matches())
    }

    /// Parse the given arguments (the first is the program name).
    #[cfg(test)]
    pub fn try_parse_args<I, T>(args: I) -> Result<Cli, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        Cli::command()
            .try_get_matches_from(args)
            .map(Self::from_matches)
    }

    fn from_matches(m: clap::ArgMatches) -> Cli {
        #[allow(unused_mut)]
        let mut cli = Cli::from_arg_matches(&m).expect("clap parsed the arguments");
        #[cfg(any(test, feature = "scripted"))]
        {
            cli.script.order = steps_in_order(&m);
        }
        cli
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_args(std::iter::once("perf-viz").chain(args.iter().copied())).unwrap()
    }

    fn pt(x: f32, y: f32) -> Point {
        Point { x, y }
    }

    fn fails(args: &[&str]) -> String {
        Cli::try_parse_args(std::iter::once("perf-viz").chain(args.iter().copied()))
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn no_arguments_means_the_start_screen() {
        let c = parse(&[]);
        assert!(
            c.file.is_none()
                && c.show.view.is_none()
                && c.show.cost.is_none()
                && !c.reader.cold
                && c.script.steps().is_empty()
        );
    }

    #[test]
    fn a_file_and_the_initial_view_flags() {
        let c = parse(&[
            "rec.data",
            "--view",
            "truncated",
            "--cost",
            "samples",
            "--theme",
            "light",
            "--tab",
            "callercallee",
            "--size",
            "1280x720",
            "--process",
            "42",
            "--thread",
            "43",
            "--search",
            "intern",
            "--range",
            "10,250.5",
        ]);
        assert_eq!(c.file, Some(PathBuf::from("rec.data")));
        assert_eq!(
            (c.show.view, c.show.cost, c.theme, c.show.tab),
            (
                Some(View::Truncated),
                Some(Cost::Samples),
                Some(Theme::Light),
                Some(TabName::Callercallee)
            )
        );
        assert_eq!(
            c.size,
            Some(WindowSize {
                width: 1280.0,
                height: 720.0
            })
        );
        assert_eq!(
            (
                c.show.process,
                c.show.thread,
                c.show.search.as_deref(),
                c.show.range
            ),
            (
                Some(42),
                Some(43),
                Some("intern"),
                Some(MsRange {
                    start_ms: 10.0,
                    end_ms: 250.5
                })
            )
        );
    }

    #[test]
    fn reader_flags() {
        let c = parse(&[
            "--readers",
            "4",
            "--threads",
            "8",
            "--region-mb",
            "128",
            "--no-dwarf",
            "--cold",
        ]);
        let r = &c.reader;
        assert_eq!(
            (r.readers, r.threads, r.region_mb, r.no_dwarf, r.cold),
            (Some(4), Some(8), Some(128), true, true)
        );
    }

    #[test]
    fn reader_flags_go_on_top_of_the_loader_defaults() {
        let defaults = load::Options::default();
        let mut opts = load::Options::default();
        parse(&[]).reader.apply(&mut opts);
        assert_eq!(
            (opts.readers, opts.workers, opts.region_mb, opts.cold),
            (
                defaults.readers,
                defaults.workers,
                defaults.region_mb,
                false
            )
        );
        parse(&[
            "--readers",
            "5",
            "--threads",
            "2",
            "--region-mb",
            "64",
            "--no-dwarf",
            "--cold",
        ])
        .reader
        .apply(&mut opts);
        assert_eq!(
            (
                opts.readers,
                opts.workers,
                opts.region_mb,
                opts.dwarf,
                opts.cold
            ),
            (5, 2, 64, DwarfMode::Never, true)
        );
    }

    #[test]
    fn bad_values_are_errors_with_a_reason() {
        assert!(fails(&["--cost", "joules"]).contains("possible values"));
        assert!(fails(&["--readers", "0"]).contains("not in 1..=64"));
        assert!(fails(&["--size", "wide"]).contains("WIDTHxHEIGHT"));
        assert!(fails(&["--range", "9,3"]).contains("after the start"));
        assert!(fails(&["--nonsense"]).contains("unexpected argument"));
    }

    #[test]
    fn bad_scripted_values_are_errors() {
        assert!(fails(&["--click", "5"]).contains("2 comma-separated"));
    }

    #[test]
    fn scripted_steps_keep_their_command_line_order() {
        let c = parse(&[
            "--click", "1,2", "--pause", "3", "--open", "a.data", "--drag", "1,2,3,4", "--hover",
            "5,6", "--click", "7,8",
        ]);
        assert_eq!(
            c.script.steps(),
            [
                ScriptStep::Click(pt(1.0, 2.0)),
                ScriptStep::Pause(3.0),
                ScriptStep::Open("a.data".into()),
                ScriptStep::Drag(DragPath {
                    from: pt(1.0, 2.0),
                    to: pt(3.0, 4.0)
                }),
                ScriptStep::Hover(pt(5.0, 6.0)),
                ScriptStep::Click(pt(7.0, 8.0)),
            ]
        );
    }

    #[test]
    fn the_command_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn help_groups_the_flags_and_hides_the_scripted_ones() {
        let help = Cli::command().render_help().to_string();
        assert!(
            help.contains("Initial view:") && help.contains("Reader:"),
            "{help}"
        );
        assert!(help.contains("--view") && help.contains("--readers"));
        assert!(!help.contains("--click") && !help.contains("--screenshot"));
    }
}
