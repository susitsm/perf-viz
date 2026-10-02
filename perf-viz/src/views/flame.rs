//! The flame graph tab: boxes laid out by [`flame_layout`], with hover, zoom and search highlighting.

use eframe::egui::{
    self, Align2, Color32, FontId, Key, Rect, Sense, Stroke, StrokeKind, pos2, vec2,
};
use perf_callgraph::model::{FrameInfo, Profile, ROOT};

use super::{ViewContext, flame_layout, flame_layout::Item};
use crate::{
    App,
    fmt::{self},
};

/// What the flame graph's boxes were laid out for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct ItemsKey {
    truncated: bool,
    focus: u32,
    width: i32,
    totals_version: u64,
}

/// The view the flame graph's scroll position was last set for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScrollKey {
    truncated: bool,
    focus: u32,
    totals_version: u64,
}

pub(crate) const ROW_H: f32 = 18.0;

pub(crate) const MIN_BOX_PX: f32 = 0.5;

pub(crate) const LABEL_MIN_PX: f32 = 36.0;

pub(crate) fn module_color(profile: &Profile, module: u32, name: &str) -> Color32 {
    // Hue from the module, a little brightness variation from the function name.
    let hash = |text: &str| {
        text.bytes()
            .fold(0xcbf29ce484222325u64, |hash_so_far, byte| {
                (hash_so_far ^ byte as u64).wrapping_mul(0x100000001b3)
            })
    };
    let hue = (hash(profile.module_name(module)) % 360) as f32 / 360.0;
    let brightness = 0.78 + 0.16 * ((hash(name) % 100) as f32 / 100.0);
    egui::ecolor::Hsva::new(hue, 0.45, brightness, 1.0).into()
}

pub(crate) fn frame_color(profile: &Profile, frame: &FrameInfo) -> Color32 {
    if frame.suspect {
        Color32::from_rgb(200, 60, 60)
    } else if frame.raw {
        Color32::from_gray(150)
    } else {
        module_color(profile, frame.module, &frame.name)
    }
}

pub(crate) fn flame_box_rect(rect: Rect, bottom: f32, item: &Item) -> Rect {
    let box_bottom = bottom - item.depth as f32 * ROW_H;
    Rect::from_min_max(
        pos2(rect.left() + item.x, box_bottom - ROW_H + 1.0),
        pos2(rect.left() + item.x + item.width, box_bottom),
    )
}

impl App {
    pub(crate) fn draw_flame_graph(&mut self, ui: &mut egui::Ui, view_context: &ViewContext<'_>) {
        let (trie, view) = (view_context.trie, view_context.view);
        if ui.input(|i| i.key_pressed(Key::Escape)) && self.focus[view] != ROOT {
            self.focus[view] = trie.nodes[self.focus[view] as usize].parent;
        }
        let focus = self.focus[view];
        let available = ui.available_size();
        let width = (available.x - 14.0).max(50.0);
        let key = ItemsKey {
            truncated: self.truncated,
            focus,
            width: width as i32,
            totals_version: self.totals_version,
        };
        if self.items_key != Some(key) {
            flame_layout::layout(
                trie,
                &self.totals,
                focus,
                width,
                MIN_BOX_PX,
                400,
                &mut self.items,
            );
            self.items_key = Some(key);
        }
        let rows = self
            .items
            .iter()
            .map(|item| item.depth)
            .max()
            .map_or(1, |depth| depth + 1);
        let content_height = (rows as f32 * ROW_H + 8.0).max(available.y);

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
            area = area.vertical_scroll_offset((content_height - available.y).max(0.0));
            self.flame_scroll_key = Some(scroll_key);
        }
        let clicked_node = area
            .show(ui, |ui| {
                self.draw_flame_boxes(ui, view_context, width, content_height)
            })
            .inner;
        if let Some(node) = clicked_node {
            self.focus[view] = node;
        }
    }

    /// Paints the boxes and the hover tooltip; returns the node whose box was clicked.
    pub(crate) fn draw_flame_boxes(
        &self,
        ui: &mut egui::Ui,
        view_context: &ViewContext<'_>,
        width: f32,
        content_height: f32,
    ) -> Option<u32> {
        let (response, painter) = ui.allocate_painter(vec2(width, content_height), Sense::click());
        let rect = response.rect;
        let bottom = rect.bottom() - 2.0;
        let hovered = response.hover_pos().and_then(|pos| {
            let depth_index = ((bottom - pos.y) / ROW_H).floor();
            (depth_index >= 0.0)
                .then(|| flame_layout::hit(&self.items, pos.x - rect.left(), depth_index as u32))
                .flatten()
                .copied()
        });
        for item in &self.items {
            let box_rect = flame_box_rect(rect, bottom, item);
            if painter.clip_rect().intersects(box_rect) {
                self.paint_flame_box(&painter, view_context, item, box_rect);
            }
        }
        let hovered_item = hovered?;
        painter.rect_stroke(
            flame_box_rect(rect, bottom, &hovered_item),
            0.0,
            Stroke::new(1.5, view_context.pal.hover_outline()),
            StrokeKind::Inside,
        );
        self.show_flame_tooltip(&response, view_context, hovered_item.node);
        response.clicked().then_some(hovered_item.node)
    }

    pub(crate) fn paint_flame_box(
        &self,
        painter: &egui::Painter,
        view_context: &ViewContext<'_>,
        item: &Item,
        box_rect: Rect,
    ) {
        let ViewContext {
            profile,
            trie,
            searching,
            pal,
            ..
        } = *view_context;
        let node = trie.nodes[item.node as usize];
        let (mut fill, label, matched): (Color32, String, bool) = if node.frame == u32::MAX {
            (pal.root_box(), "all samples".into(), false)
        } else {
            let frame = &profile.frames[node.frame as usize];
            (
                frame_color(profile, frame),
                frame.name.clone(),
                searching && self.matches[node.frame as usize],
            )
        };
        if searching && !matched && node.frame != u32::MAX {
            fill = pal.dim(fill);
        }
        painter.rect_filled(box_rect, 0.0, fill);
        painter.rect_stroke(
            box_rect,
            0.0,
            Stroke::new(0.5, Color32::from_black_alpha(90)),
            StrokeKind::Inside,
        );
        if matched {
            painter.rect_stroke(
                box_rect,
                0.0,
                Stroke::new(2.0, pal.highlight()),
                StrokeKind::Inside,
            );
        }
        if item.width >= LABEL_MIN_PX {
            painter
                .with_clip_rect(box_rect.shrink2(vec2(2.0, 0.0)))
                .text(
                    pos2(box_rect.left() + 3.0, box_rect.center().y),
                    Align2::LEFT_CENTER,
                    label,
                    FontId::proportional(11.0),
                    Color32::BLACK,
                );
        }
    }

    pub(crate) fn show_flame_tooltip(
        &self,
        response: &egui::Response,
        view_context: &ViewContext<'_>,
        hovered_node: u32,
    ) {
        let ViewContext {
            profile,
            trie,
            cost,
            unit,
            pal,
            ..
        } = *view_context;
        let total = self.totals[ROOT as usize].max(1);
        let node = trie.nodes[hovered_node as usize];
        let node_total = self.totals[hovered_node as usize];
        let children_total: u64 = trie
            .children(hovered_node)
            .iter()
            .map(|&child| self.totals[child as usize])
            .sum();
        let self_count = node_total.saturating_sub(children_total);
        response.clone().on_hover_ui_at_pointer(|ui| {
            if node.frame == u32::MAX {
                ui.strong("all samples");
            } else {
                let frame = &profile.frames[node.frame as usize];
                ui.strong(&frame.name);
                ui.label(format!("module: {}", profile.module_name(frame.module)));
                if let Some(file) = &frame.file {
                    ui.label(format!("{}:{}", file, frame.line.unwrap_or(0)));
                }
                if frame.suspect {
                    ui.colored_label(
                        pal.suspect(),
                        "suspect tail: at or beyond the first invalid return address",
                    );
                }
                if frame.raw {
                    ui.label("no symbol found");
                }
            }
            let parent_total = if node.parent == u32::MAX {
                node_total
            } else {
                self.totals[node.parent as usize]
            };
            ui.label(format!(
                "{} {unit} inclusive ({}% of the view, {}% of parent)",
                cost.format(node_total),
                fmt::pct_of(node_total as f64, total as f64),
                fmt::pct_of(node_total as f64, parent_total as f64)
            ));
            ui.label(format!("{} {unit} self", cost.format(self_count)));
        });
    }
}
