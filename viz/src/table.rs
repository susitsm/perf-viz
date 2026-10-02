//! A virtualized tree table: only the visible rows are laid out and painted, so a tree with
//! hundreds of thousands of rows scrolls as smoothly as a short one.
//!
//! Columns: the tree (arrow, name), the module, then an inclusive (or plain) cost and,
//! optionally, a self cost, each as a cost (samples or cycles) and a percentage of the view.

use crate::{fmt::Cost, theme::Pal};
use eframe::egui::{self, Align2, Color32, FontId, Rect, Sense, Shape, Stroke, pos2, vec2};

pub const ROW_H: f32 = 20.0;
const INDENT: f32 = 14.0;
const NUM_W: f32 = 84.0;
const PCT_W: f32 = 66.0;
const MODULE_W: f32 = 210.0;
const ARROW_ZONE: f32 = 18.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Arrow {
    None,
    Collapsed,
    Expanded,
}

pub struct RowData<'a> {
    pub depth: u32,
    pub arrow: Arrow,
    pub name: &'a str,
    pub module: &'a str,
    /// The main cost (inclusive for a call tree).
    pub cost: u64,
    /// A second cost column, if the table has one.
    pub self_cost: Option<u64>,
    pub highlight: bool,
    pub suspect: bool,
    pub selected: bool,
}

pub struct Columns<'a> {
    /// How the numbers are shown (exact sample counts, or cycles with an SI suffix).
    pub unit: Cost,
    pub name: &'a str,
    pub cost: &'a str,
    pub self_cost: Option<&'a str>,
    /// `Some(current)` makes the numeric column headers clickable, with `current` (0 = the cost
    /// column, 1 = the second one) marked as the one the rows are sorted by. `None`: not sortable.
    pub sort: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Click {
    /// A numeric column header was clicked (0 = cost, 1 = second cost): the caller should re-sort.
    SortBy(usize),
    /// The expand/collapse arrow of this row index was clicked (or the row was double-clicked).
    Toggle(usize),
    Select(usize),
}

/// Draw the table filling the available space. `total` is the number of samples that 100% means.
pub fn draw<'a>(
    ui: &mut egui::Ui,
    cols: &Columns<'_>,
    n_rows: usize,
    total: u64,
    mut row: impl FnMut(usize) -> RowData<'a>,
) -> Option<Click> {
    let width = ui.available_width().max(200.0);
    let numeric_w = (NUM_W + PCT_W) * if cols.self_cost.is_some() { 2.0 } else { 1.0 };
    let name_w = (width - numeric_w - MODULE_W - 12.0).max(80.0);
    // x offsets from the left of the table
    let module_x = name_w;
    let cost_x = module_x + MODULE_W;
    let self_x = cost_x + NUM_W + PCT_W;
    let total_f = total.max(1) as f32;

    let pal = Pal::of(ui);
    let (text, weak) = (pal.text(), pal.weak());
    let mut clicked = None;

    // Header. The numeric columns are clickable when the table is sortable.
    let (hrect, _) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::hover());
    ui.painter().rect_filled(hrect, 0.0, pal.header_bg());
    if let Some(current) = cols.sort {
        let mut cells = vec![(0usize, cost_x)];
        if cols.self_cost.is_some() {
            cells.push((1, self_x));
        }
        for (col, x) in cells {
            let cell = Rect::from_min_size(
                pos2(hrect.left() + x, hrect.top()),
                vec2(NUM_W + PCT_W, ROW_H),
            );
            let resp = ui
                .interact(cell, ui.id().with(("sort_header", col)), Sense::click())
                .on_hover_text("Click to sort by this column");
            if resp.hovered() {
                ui.painter().rect_filled(cell, 0.0, pal.tint(16));
            }
            if current == col {
                // A small "sorted, largest first" triangle at the left of the cell (drawn, since the
                // default font lacks the usual arrow glyphs).
                let (ax, ay) = (cell.left() + 9.0, cell.center().y);
                ui.painter().add(Shape::convex_polygon(
                    vec![
                        pos2(ax - 4.0, ay - 3.0),
                        pos2(ax + 4.0, ay - 3.0),
                        pos2(ax, ay + 3.5),
                    ],
                    text,
                    Stroke::NONE,
                ));
            }
            if resp.clicked() {
                clicked = Some(Click::SortBy(col));
            }
        }
    }
    let hp = ui.painter();
    let head = |x: f32, text: &str, align: Align2| {
        hp.text(
            pos2(hrect.left() + x, hrect.center().y),
            align,
            text,
            FontId::proportional(12.0),
            weak,
        )
    };
    head(6.0, cols.name, Align2::LEFT_CENTER);
    head(module_x + 4.0, "Module", Align2::LEFT_CENTER);
    head(cost_x + NUM_W - 4.0, cols.cost, Align2::RIGHT_CENTER);
    head(cost_x + NUM_W + PCT_W - 4.0, "%", Align2::RIGHT_CENTER);
    if let Some(self_label) = cols.self_cost {
        head(self_x + NUM_W - 4.0, self_label, Align2::RIGHT_CENTER);
        head(self_x + NUM_W + PCT_W - 4.0, "%", Align2::RIGHT_CENTER);
    }

    ui.spacing_mut().item_spacing.y = 0.0;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, ROW_H, n_rows, |ui, range| {
            for i in range {
                let row_data = row(i);
                let (rect, resp) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::click());
                let painter = ui.painter();
                let (left, cy) = (rect.left(), rect.center().y);
                if row_data.selected {
                    painter.rect_filled(rect, 0.0, pal.selected_row());
                } else if i % 2 == 1 {
                    painter.rect_filled(rect, 0.0, pal.tint(8));
                }
                if resp.hovered() && !row_data.selected {
                    painter.rect_filled(rect, 0.0, pal.tint(14));
                }

                // Tree column: indent, arrow, name
                let indent_x = 6.0 + row_data.depth as f32 * INDENT;
                if row_data.arrow != Arrow::None {
                    let (ax, ay) = (left + indent_x + 4.0, cy);
                    let pts = if row_data.arrow == Arrow::Collapsed {
                        vec![
                            pos2(ax - 2.0, ay - 4.5),
                            pos2(ax - 2.0, ay + 4.5),
                            pos2(ax + 4.0, ay),
                        ]
                    } else {
                        vec![
                            pos2(ax - 4.0, ay - 3.0),
                            pos2(ax + 4.0, ay - 3.0),
                            pos2(ax, ay + 3.5),
                        ]
                    };
                    painter.add(Shape::convex_polygon(pts, weak, Stroke::NONE));
                }
                let name_rect = Rect::from_min_max(
                    pos2(left + indent_x + ARROW_ZONE, rect.top()),
                    pos2(left + name_w, rect.bottom()),
                );
                let color = if row_data.highlight {
                    pal.highlight()
                } else if row_data.suspect {
                    pal.suspect()
                } else {
                    text
                };
                painter.with_clip_rect(name_rect).text(
                    pos2(name_rect.left(), cy),
                    Align2::LEFT_CENTER,
                    row_data.name,
                    FontId::proportional(12.0),
                    color,
                );
                let module_rect = Rect::from_min_max(
                    pos2(left + module_x, rect.top()),
                    pos2(left + cost_x - 6.0, rect.bottom()),
                );
                painter.with_clip_rect(module_rect).text(
                    pos2(module_rect.left() + 4.0, cy),
                    Align2::LEFT_CENTER,
                    row_data.module,
                    FontId::proportional(12.0),
                    weak,
                );

                // Numeric columns, each a count and a percentage (with a bar behind the percentage)
                let cell = |x: f32, value: u64, bar: Color32| {
                    let frac = (value as f32 / total_f).clamp(0.0, 1.0);
                    let bar_rect = Rect::from_min_size(
                        pos2(left + x + NUM_W, rect.top() + 2.0),
                        vec2((PCT_W - 4.0) * frac, ROW_H - 4.0),
                    );
                    painter.rect_filled(bar_rect, 1.0, bar);
                    painter.text(
                        pos2(left + x + NUM_W - 4.0, cy),
                        Align2::RIGHT_CENTER,
                        cols.unit.format(value),
                        FontId::monospace(11.5),
                        text,
                    );
                    painter.text(
                        pos2(left + x + NUM_W + PCT_W - 4.0, cy),
                        Align2::RIGHT_CENTER,
                        crate::fmt::pct_of(value as f64, total as f64),
                        FontId::monospace(11.5),
                        text,
                    );
                };
                cell(
                    cost_x,
                    row_data.cost,
                    Color32::from_rgba_unmultiplied(70, 130, 200, 90),
                );
                if let Some(self_cost) = row_data.self_cost {
                    cell(
                        self_x,
                        self_cost,
                        Color32::from_rgba_unmultiplied(200, 130, 70, 90),
                    );
                }

                if resp.clicked() {
                    let on_arrow = resp.interact_pointer_pos().is_some_and(|pointer| {
                        pointer.x >= left + indent_x - 2.0
                            && pointer.x < left + indent_x + ARROW_ZONE
                    });
                    clicked = Some(if row_data.arrow != Arrow::None && on_arrow {
                        Click::Toggle(i)
                    } else {
                        Click::Select(i)
                    });
                }
                if resp.double_clicked() && row_data.arrow != Arrow::None {
                    clicked = Some(Click::Toggle(i));
                }
            }
        });
    clicked
}
