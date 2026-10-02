//! The timeline: sample density over time, overall and per thread, with a draggable time range.
//!
//! The first half is pure (binning, ranges, axis ticks) and tested; the second half draws it.

use std::{collections::HashMap, ops::Range};

use eframe::egui::{self, Align2, Color32, FontId, Rect, Sense, Stroke, pos2, vec2};
use perf_profile::model::Profile;

use crate::{
    filter::{Filter, weight},
    fmt::{self, Cost},
    theme::Pal,
};

/// A stretch of time `[start, end)`, in absolute sample timestamps (nanoseconds).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeRange {
    pub start: u64,
    pub end: u64,
}

impl TimeRange {
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }
}

/// A selection being dragged across the plot: where the press was and where the pointer is, as x
/// coordinates (the pointer may be left of the start).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct XDrag {
    pub from_x: f32,
    pub to_x: f32,
}

/// An axis tick: where it sits (a fraction of the plot width) and its label.
#[derive(Clone, Debug, PartialEq)]
pub struct Tick {
    pub fraction: f32,
    pub label: String,
}

/// What one thread contributed, while the busiest threads are being picked.
struct ThreadCost {
    pid: i32,
    cost: u64,
}

/// One thread's activity over time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lane {
    pub pid: i32,
    pub tid: i32,
    pub samples: u64,
    pub bins: Vec<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct Timeline {
    /// First sample time, and one past the last (so every sample falls in `[t0, t1)`).
    pub t0: u64,
    pub t1: u64,
    /// Cost (samples or cycles) per time bin, over the whole recording, for the samples the filter keeps.
    pub bins: Vec<u64>,
    /// The busiest threads (among the samples the filter keeps), busiest first.
    pub lanes: Vec<Lane>,
    pub total: u64,
}

impl Timeline {
    pub fn span(&self) -> u64 {
        (self.t1 - self.t0).max(1)
    }

    #[cfg(test)]
    pub fn bin_of(&self, time: u64) -> usize {
        bin_of(self.t0, self.span(), self.bins.len(), time)
    }

    /// The time at horizontal fraction `f` (0..=1) of the recording.
    pub fn time_at(&self, fraction: f32) -> u64 {
        self.t0 + (fraction.clamp(0.0, 1.0) as f64 * self.span() as f64) as u64
    }
}

fn bin_of(t0: u64, span: u64, n_bins: usize, time: u64) -> usize {
    (((time.saturating_sub(t0)) as u128 * n_bins as u128 / span as u128) as usize)
        .min(n_bins.saturating_sub(1))
}

/// Bin the samples the filter keeps into `n_bins` time bins, and into per-thread lanes for the
/// `max_lanes` busiest threads. Scans the samples twice (choose lanes, then bin).
pub fn build(
    profile: &Profile,
    filter: Filter,
    n_bins: usize,
    max_lanes: usize,
    cost: Cost,
) -> Timeline {
    let n_bins = n_bins.max(1);
    let (Some(first), Some(last)) = (profile.samples.first(), profile.samples.last()) else {
        return Timeline {
            bins: vec![0; n_bins],
            ..Default::default()
        };
    };
    let (t0, t1) = (first.time, last.time + 1);
    let span = (t1 - t0).max(1);
    let keeps = |pid: i32, tid: i32| match filter {
        Filter::All => true,
        Filter::Process(process_id) => pid == process_id,
        Filter::Thread(thread_id) => tid == thread_id,
    };

    let mut per_thread: HashMap<i32, ThreadCost> = HashMap::new();
    for sample in profile
        .samples
        .iter()
        .filter(|sample| keeps(sample.pid, sample.tid))
    {
        per_thread
            .entry(sample.tid)
            .or_insert(ThreadCost {
                pid: sample.pid,
                cost: 0,
            })
            .cost += weight(sample, cost);
    }
    let mut ranked: Vec<(i32, ThreadCost)> = per_thread.into_iter().collect();
    ranked.sort_by_key(|(tid, thread)| (std::cmp::Reverse(thread.cost), *tid));
    ranked.truncate(max_lanes);
    let mut lanes: Vec<Lane> = ranked
        .iter()
        .map(|(tid, thread)| Lane {
            pid: thread.pid,
            tid: *tid,
            samples: thread.cost,
            bins: vec![0; n_bins],
        })
        .collect();
    let lane_of: HashMap<i32, usize> = ranked
        .iter()
        .enumerate()
        .map(|(i, (tid, _))| (*tid, i))
        .collect();

    let mut bins = vec![0u64; n_bins];
    let mut total = 0u64;
    for sample in profile
        .samples
        .iter()
        .filter(|sample| keeps(sample.pid, sample.tid))
    {
        let bin = bin_of(t0, span, n_bins, sample.time);
        let sample_weight = weight(sample, cost);
        bins[bin] += sample_weight;
        total += sample_weight;
        if let Some(&lane_index) = lane_of.get(&sample.tid) {
            lanes[lane_index].bins[bin] += sample_weight;
        }
    }
    Timeline {
        t0,
        t1,
        bins,
        lanes,
        total,
    }
}

/// Indices into `p.samples` (sorted by time) of the samples inside `[start, end)`.
pub fn sample_indices(profile: &Profile, range: Option<TimeRange>) -> Range<usize> {
    match range {
        None => 0..profile.samples.len(),
        Some(range) => {
            let lo = profile
                .samples
                .partition_point(|sample| sample.time < range.start);
            let hi = profile
                .samples
                .partition_point(|sample| sample.time < range.end);
            lo..hi.max(lo)
        }
    }
}

/// `1, 2, 5 x 10^k` at least as large as `raw` (0 gives 1).
pub fn nice_step(raw: f64) -> f64 {
    if raw <= 0.0 || !raw.is_finite() {
        return 1.0;
    }
    let mag = 10f64.powf(raw.log10().floor());
    [1.0, 2.0, 5.0, 10.0]
        .iter()
        .map(|multiplier| multiplier * mag)
        .find(|&step| step >= raw)
        .unwrap_or(10.0 * mag)
}

/// A duration in a readable unit, three significant digits: `812 ns`, `12.3 ms`, `4.99 s`.
pub fn duration(ns: u64) -> String {
    let x = ns as f64;
    if ns < 1_000 {
        format!("{ns} ns")
    } else if ns < 1_000_000 {
        format!("{} µs", fmt::sig3(x / 1e3))
    } else if ns < 1_000_000_000 {
        format!("{} ms", fmt::sig3(x / 1e6))
    } else {
        format!("{} s", fmt::sig3(x / 1e9))
    }
}

/// Axis ticks over `span_ns` for a plot `width_px` wide, at least `min_gap_px` apart. The first tick
/// is at 0.
pub fn ticks(span_ns: u64, width_px: f32, min_gap_px: f32) -> Vec<Tick> {
    if span_ns == 0 || width_px <= 0.0 {
        return vec![];
    }
    let step = nice_step(span_ns as f64 * min_gap_px as f64 / width_px as f64) as u64;
    let step = step.max(1);
    (0..)
        .map(|i| i * step)
        .take_while(|&time_ns| time_ns <= span_ns)
        .map(|time_ns| Tick {
            fraction: time_ns as f32 / span_ns as f32,
            label: duration(time_ns),
        })
        .collect()
}

/// What the user did in the timeline this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    SetRange(TimeRange),
    ClearRange,
    /// A lane's label was clicked: show only this thread.
    SelectThread {
        pid: i32,
        tid: i32,
    },
}

pub struct Options<'a> {
    pub timeline_data: &'a Timeline,
    pub range: Option<TimeRange>,
    /// Samples inside the range (for the caption).
    pub samples_in_range: Option<u64>,
    pub cost: Cost,
    /// What the cost counts, for captions: "samples", "cycles", ...
    pub noun: &'a str,
    pub strip_h: f32,
    pub show_lanes: bool,
    pub comm: &'a HashMap<i32, String>,
    pub selected_tid: Option<i32>,
    /// Width of the label column left of the plot.
    pub label_w: f32,
}

const RIGHT_PAD: f32 = 10.0;
const LANE_H: f32 = 26.0;

/// Draw the timeline and return what the user did. `drag` holds a selection in progress as x
/// coordinates and is owned by the caller so it survives between frames.
pub fn draw(ui: &mut egui::Ui, options: &Options<'_>, drag: &mut Option<XDrag>) -> Vec<Action> {
    let mut actions = Vec::new();
    let timeline_data = options.timeline_data;
    let pal = Pal::of(ui);
    let span = timeline_data.span();

    ui.horizontal(|ui| {
        match options.range {
            Some(range) => {
                ui.colored_label(pal.highlight(), format!("Selected: +{} to +{} ({}{})", duration(range.start.saturating_sub(timeline_data.t0)), duration(range.end.saturating_sub(timeline_data.t0)), duration(range.len()), options.samples_in_range.map_or(String::new(), |amount| format!(", {} {}", options.cost.format(amount), options.noun))));
                if ui.small_button("Clear selection").clicked() {
                    actions.push(Action::ClearRange);
                }
            }
            None => {
                ui.weak(format!("Recording {}: drag across the timeline to select a time range; every view follows the selection", duration(span)));
            }
        }
    });

    let plot_w = (ui.available_width() - options.label_w - RIGHT_PAD).max(50.0);
    let to_time = |x: f32, left: f32| timeline_data.time_at((x - left) / plot_w);
    let to_x = |time: u64, left: f32| {
        left + (time.saturating_sub(timeline_data.t0) as f64 / span as f64 * plot_w as f64) as f32
    };

    let mut row = |ui: &mut egui::Ui,
                   id: usize,
                   row_height: f32,
                   bins: &[u64],
                   color: Color32,
                   label: &str,
                   label_action: Option<Action>,
                   tip: &dyn Fn(usize) -> String| {
        let (rect, resp) = ui.allocate_at_least(
            vec2(options.label_w + plot_w + RIGHT_PAD, row_height),
            Sense::click_and_drag(),
        );
        let (label_rect, plot) = (
            Rect::from_min_size(rect.min, vec2(options.label_w, row_height)),
            Rect::from_min_size(
                pos2(rect.left() + options.label_w, rect.top()),
                vec2(plot_w, row_height),
            ),
        );
        let painter = ui.painter();
        painter.rect_filled(plot, 2.0, pal.plot_bg());
        let max = bins.iter().copied().max().unwrap_or(0).max(1) as f32;
        let bw = plot_w / bins.len().max(1) as f32;
        for (i, &count) in bins.iter().enumerate() {
            if count > 0 {
                let bh = (count as f32 / max * (row_height - 4.0)).max(1.0);
                let x = plot.left() + i as f32 * bw;
                painter.rect_filled(
                    Rect::from_min_max(
                        pos2(x, plot.bottom() - 2.0 - bh),
                        pos2(x + bw.max(1.0), plot.bottom() - 2.0),
                    ),
                    0.0,
                    color,
                );
            }
        }
        let band = match *drag {
            Some(drag) => Some((drag.from_x.min(drag.to_x), drag.from_x.max(drag.to_x))),
            None => options
                .range
                .map(|range| (to_x(range.start, plot.left()), to_x(range.end, plot.left()))),
        };
        if let Some((xa, xb)) = band {
            let band = Rect::from_min_max(
                pos2(xa.clamp(plot.left(), plot.right()), plot.top()),
                pos2(xb.clamp(plot.left(), plot.right()), plot.bottom()),
            );
            painter.rect_filled(band, 0.0, pal.band());
            painter.rect_stroke(
                band,
                0.0,
                Stroke::new(1.0, pal.highlight()),
                egui::StrokeKind::Inside,
            );
        }
        let selected = label_action.is_some_and(
            |action| matches!(action, Action::SelectThread { tid, .. } if Some(tid) == options.selected_tid),
        );
        painter
            .with_clip_rect(label_rect.shrink2(vec2(2.0, 0.0)))
            .text(
                pos2(label_rect.left() + 4.0, label_rect.center().y),
                Align2::LEFT_CENTER,
                label,
                FontId::proportional(11.5),
                if selected {
                    pal.highlight()
                } else {
                    pal.text()
                },
            );

        if resp.drag_started() {
            if let Some(pointer) = resp
                .interact_pointer_pos()
                .filter(|point| plot.contains(*point))
            {
                *drag = Some(XDrag {
                    from_x: pointer.x,
                    to_x: pointer.x,
                });
            }
        }
        if resp.dragged() {
            if let (Some(active_drag), Some(pointer)) = (drag.as_mut(), resp.interact_pointer_pos())
            {
                active_drag.to_x = pointer.x;
            }
        }
        if resp.drag_stopped() {
            if let Some(finished_drag) = drag.take() {
                if (finished_drag.from_x - finished_drag.to_x).abs() >= 4.0 {
                    let (ta, tb) = (
                        to_time(finished_drag.from_x.min(finished_drag.to_x), plot.left()),
                        to_time(finished_drag.from_x.max(finished_drag.to_x), plot.left()),
                    );
                    actions.push(Action::SetRange(TimeRange {
                        start: ta,
                        end: tb.max(ta + 1),
                    }));
                } else {
                    actions.push(Action::ClearRange);
                }
            }
        }
        if resp.clicked() {
            if let Some(pointer) = resp.interact_pointer_pos() {
                if label_rect.contains(pointer) {
                    actions.extend(label_action);
                } else if plot.contains(pointer) {
                    actions.push(Action::ClearRange);
                }
            }
        }
        if drag.is_none() {
            if let Some(pointer) = resp.hover_pos().filter(|point| plot.contains(*point)) {
                let bin = (((pointer.x - plot.left()) / plot_w) * bins.len() as f32) as usize;
                let bin = bin.min(bins.len().saturating_sub(1));
                let time = to_time(pointer.x, plot.left());
                resp.on_hover_ui_at_pointer(|ui| {
                    ui.label(format!(
                        "+{}",
                        duration(time.saturating_sub(timeline_data.t0))
                    ));
                    ui.label(tip(bin));
                });
            }
        }
        id
    };

    let (axis, _) = ui.allocate_exact_size(
        vec2(options.label_w + plot_w + RIGHT_PAD, 16.0),
        Sense::hover(),
    );
    let plot_left = axis.left() + options.label_w;
    for tick in ticks(span, plot_w, 90.0) {
        let x = plot_left + tick.fraction * plot_w;
        ui.painter().line_segment(
            [pos2(x, axis.bottom() - 4.0), pos2(x, axis.bottom())],
            Stroke::new(1.0, pal.axis()),
        );
        ui.painter().text(
            pos2(x + 2.0, axis.center().y - 1.0),
            Align2::LEFT_CENTER,
            tick.label,
            FontId::proportional(10.5),
            pal.weak(),
        );
    }

    let bin_ns = span / timeline_data.bins.len().max(1) as u64;
    let all_tip = |bin: usize| {
        format!(
            "{} {} in this {} slice",
            options
                .cost
                .format(timeline_data.bins.get(bin).copied().unwrap_or(0)),
            options.noun,
            duration(bin_ns)
        )
    };
    row(
        ui,
        usize::MAX,
        options.strip_h,
        &timeline_data.bins,
        pal.timeline_bar(),
        &format!("All ({})", options.cost.format(timeline_data.total)),
        None,
        &all_tip,
    );

    if options.show_lanes {
        ui.add_space(4.0);
        for (i, lane) in timeline_data.lanes.iter().enumerate() {
            let comm = options.comm.get(&lane.pid).map_or("?", String::as_str);
            let hue = ((lane.tid as u32).wrapping_mul(2654435761) >> 8) % 360;
            let color: Color32 = egui::ecolor::Hsva::new(
                hue as f32 / 360.0,
                if pal.dark { 0.5 } else { 0.65 },
                if pal.dark { 0.85 } else { 0.72 },
                1.0,
            )
            .into();
            let tip = |bin: usize| {
                format!(
                    "thread {}: {} {} in this {} slice",
                    lane.tid,
                    options
                        .cost
                        .format(lane.bins.get(bin).copied().unwrap_or(0)),
                    options.noun,
                    duration(bin_ns)
                )
            };
            row(
                ui,
                i,
                LANE_H,
                &lane.bins,
                color,
                &format!(
                    "{} ({comm}) — {}",
                    lane.tid,
                    options.cost.format(lane.samples)
                ),
                Some(Action::SelectThread {
                    pid: lane.pid,
                    tid: lane.tid,
                }),
                &tip,
            );
        }
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use perf_profile::model::{LoadStats, SampleRef, Trie};

    fn profile(samples: &[(u64, i32, i32)]) -> Profile {
        Profile {
            modules: vec![],
            frames: vec![],
            full: Trie::default(),
            truncated: Trie::default(),
            samples: samples
                .iter()
                .map(|&(time, pid, tid)| SampleRef {
                    time,
                    period: 1,
                    pid,
                    tid,
                    full: 0,
                    truncated: 0,
                })
                .collect(),
            comm: Default::default(),
            stats: LoadStats::default(),
        }
    }

    #[test]
    fn every_sample_lands_in_a_bin_and_bins_sum_to_the_total() {
        let sample_profile = profile(&[(1000, 1, 10), (1500, 1, 10), (2000, 1, 11), (3000, 2, 20)]);
        let timeline_data = build(&sample_profile, Filter::All, 4, 8, Cost::Samples);
        assert_eq!((timeline_data.t0, timeline_data.t1), (1000, 3001));
        assert_eq!(timeline_data.bins.iter().sum::<u64>(), 4);
        assert_eq!(timeline_data.total, 4);
        assert_eq!(
            timeline_data.bins[0], 2,
            "the first two samples are in the first slice"
        );
        assert_eq!(
            timeline_data.bins[3], 1,
            "the last sample is in the last slice, not one past it"
        );
    }

    #[test]
    fn lanes_are_the_busiest_threads_and_respect_the_filter() {
        let sample_profile = profile(&[
            (0, 1, 10),
            (1, 1, 10),
            (2, 1, 10),
            (3, 1, 11),
            (4, 2, 20),
            (5, 2, 20),
        ]);
        let timeline_data = build(&sample_profile, Filter::All, 6, 2, Cost::Samples);
        assert_eq!(
            timeline_data
                .lanes
                .iter()
                .map(|lane| (lane.tid, lane.samples))
                .collect::<Vec<_>>(),
            vec![(10, 3), (20, 2)],
            "top 2, busiest first"
        );
        assert_eq!(timeline_data.lanes[0].bins.iter().sum::<u64>(), 3);
        let only1 = build(&sample_profile, Filter::Process(1), 6, 8, Cost::Samples);
        assert_eq!(only1.total, 4);
        assert_eq!(
            only1.lanes.iter().map(|lane| lane.tid).collect::<Vec<_>>(),
            vec![10, 11]
        );
        let one = build(&sample_profile, Filter::Thread(20), 6, 8, Cost::Samples);
        assert_eq!((one.total, one.lanes.len()), (2, 1));
        assert_eq!(
            (one.t0, one.t1),
            (0, 6),
            "the axis always spans the whole recording"
        );
    }

    fn r(start: u64, end: u64) -> TimeRange {
        TimeRange { start, end }
    }

    #[test]
    fn empty_profile_has_an_empty_timeline() {
        let timeline_data = build(&profile(&[]), Filter::All, 5, 3, Cost::Samples);
        assert_eq!(
            (
                timeline_data.total,
                timeline_data.bins.len(),
                timeline_data.lanes.len()
            ),
            (0, 5, 0)
        );
    }

    #[test]
    fn sample_indices_use_a_half_open_range() {
        let sample_profile = profile(&[(10, 1, 1), (20, 1, 1), (20, 1, 1), (30, 1, 1)]);
        assert_eq!(sample_indices(&sample_profile, None), 0..4);
        assert_eq!(
            sample_indices(&sample_profile, Some(r(20, 30))),
            1..3,
            "includes 20, excludes 30"
        );
        assert_eq!(sample_indices(&sample_profile, Some(r(0, 10))), 0..0);
        assert_eq!(sample_indices(&sample_profile, Some(r(31, 99))), 4..4);
        assert_eq!(
            sample_indices(&sample_profile, Some(r(25, 5))),
            3..3,
            "an inverted range is empty"
        );
    }

    #[test]
    fn nice_steps_and_ticks() {
        assert_eq!(nice_step(0.7), 1.0);
        assert_eq!(nice_step(1.1), 2.0);
        assert_eq!(nice_step(3.0), 5.0);
        assert_eq!(nice_step(7.0), 10.0);
        assert_eq!(nice_step(140.0), 200.0);
        // A 4.99 s recording over 900 px with ticks at least 90 px apart: 10 ticks max -> 0.5 s steps.
        let ticks = ticks(4_990_000_000, 900.0, 90.0);
        assert_eq!(
            ticks[0],
            Tick {
                fraction: 0.0,
                label: "0 ns".to_string()
            }
        );
        assert_eq!(ticks[1].label, "500 ms");
        assert!(
            ticks
                .windows(2)
                .all(|pair| (pair[1].fraction - pair[0].fraction) * 900.0 >= 89.0),
            "ticks are at least the minimum gap apart"
        );
        assert!(ticks.last().unwrap().fraction <= 1.0);
    }

    #[test]
    fn durations_use_readable_units() {
        assert_eq!(duration(812), "812 ns");
        assert_eq!(duration(12_300), "12.3 µs");
        assert_eq!(duration(46_600_000), "46.6 ms");
        assert_eq!(duration(4_990_000_000), "4.99 s");
    }

    #[test]
    fn time_at_maps_fractions_to_times_and_clamps() {
        let sample_profile = profile(&[(1000, 1, 1), (2999, 1, 1)]);
        let timeline_data = build(&sample_profile, Filter::All, 10, 1, Cost::Samples);
        assert_eq!(timeline_data.time_at(0.0), 1000);
        assert_eq!(timeline_data.time_at(1.0), 3000);
        assert_eq!(timeline_data.time_at(-3.0), 1000);
        assert_eq!(timeline_data.time_at(9.0), 3000);
        assert_eq!(timeline_data.bin_of(1000), 0);
        assert_eq!(timeline_data.bin_of(2999), 9);
    }
}
