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
    /// Names the table's scroll area, so tables of different tabs keep their own scroll positions.
    pub id: &'a str,
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

/// Horizontal positions of the columns, relative to the left edge of the table.
struct Layout {
    width: f32,
    name_w: f32,
    module_x: f32,
    cost_x: f32,
    self_x: f32,
}

impl Layout {
    fn new(available_width: f32, cols: &Columns<'_>) -> Self {
        let width = available_width.max(200.0);
        let numeric_w = (NUM_W + PCT_W) * if cols.self_cost.is_some() { 2.0 } else { 1.0 };
        let name_w = (width - numeric_w - MODULE_W - 12.0).max(80.0);
        let cost_x = name_w + MODULE_W;
        Self {
            width,
            name_w,
            module_x: name_w,
            cost_x,
            self_x: cost_x + NUM_W + PCT_W,
        }
    }
}

/// A small "sorted, largest first" triangle at the left of the cell (drawn, since the default
/// font lacks the usual arrow glyphs).
fn draw_sorted_marker(ui: &egui::Ui, cell: Rect, color: Color32) {
    let (ax, ay) = (cell.left() + 9.0, cell.center().y);
    ui.painter().add(Shape::convex_polygon(
        vec![
            pos2(ax - 4.0, ay - 3.0),
            pos2(ax + 4.0, ay - 3.0),
            pos2(ax, ay + 3.5),
        ],
        color,
        Stroke::NONE,
    ));
}

fn draw_sort_cells(
    ui: &mut egui::Ui,
    header: Rect,
    layout: &Layout,
    cols: &Columns<'_>,
    current: usize,
    pal: &Pal,
) -> Option<Click> {
    let mut cells = vec![(0usize, layout.cost_x)];
    if cols.self_cost.is_some() {
        cells.push((1, layout.self_x));
    }
    let mut clicked = None;
    for (col, x) in cells {
        let cell = Rect::from_min_size(
            pos2(header.left() + x, header.top()),
            vec2(NUM_W + PCT_W, ROW_H),
        );
        let resp = ui
            .interact(cell, ui.id().with(("sort_header", col)), Sense::click())
            .on_hover_text("Click to sort by this column");
        if resp.hovered() {
            ui.painter().rect_filled(cell, 0.0, pal.tint(16));
        }
        if current == col {
            draw_sorted_marker(ui, cell, pal.text());
        }
        if resp.clicked() {
            clicked = Some(Click::SortBy(col));
        }
    }
    clicked
}

fn draw_header(ui: &mut egui::Ui, layout: &Layout, cols: &Columns<'_>, pal: &Pal) -> Option<Click> {
    let (header, _) = ui.allocate_exact_size(vec2(layout.width, ROW_H), Sense::hover());
    ui.painter().rect_filled(header, 0.0, pal.header_bg());
    let clicked = cols
        .sort
        .and_then(|current| draw_sort_cells(ui, header, layout, cols, current, pal));
    let painter = ui.painter();
    let weak = pal.weak();
    let label = |x: f32, text: &str, align: Align2| {
        painter.text(
            pos2(header.left() + x, header.center().y),
            align,
            text,
            FontId::proportional(12.0),
            weak,
        )
    };
    label(6.0, cols.name, Align2::LEFT_CENTER);
    label(layout.module_x + 4.0, "Module", Align2::LEFT_CENTER);
    label(layout.cost_x + NUM_W - 4.0, cols.cost, Align2::RIGHT_CENTER);
    label(
        layout.cost_x + NUM_W + PCT_W - 4.0,
        "%",
        Align2::RIGHT_CENTER,
    );
    if let Some(self_label) = cols.self_cost {
        label(
            layout.self_x + NUM_W - 4.0,
            self_label,
            Align2::RIGHT_CENTER,
        );
        label(
            layout.self_x + NUM_W + PCT_W - 4.0,
            "%",
            Align2::RIGHT_CENTER,
        );
    }
    clicked
}

fn draw_arrow(painter: &egui::Painter, arrow: Arrow, center: egui::Pos2, color: Color32) {
    let (ax, ay) = (center.x, center.y);
    let points = if arrow == Arrow::Collapsed {
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
    painter.add(Shape::convex_polygon(points, color, Stroke::NONE));
}

/// A cost with its share of the view, over a bar as wide as that share.
fn draw_cost_cell(
    painter: &egui::Painter,
    row_rect: Rect,
    x: f32,
    value: u64,
    total: u64,
    unit: Cost,
    colors: (Color32, Color32),
) {
    let (bar_color, text_color) = colors;
    let left = row_rect.left();
    let center_y = row_rect.center().y;
    let share = (value as f32 / total.max(1) as f32).clamp(0.0, 1.0);
    let bar_rect = Rect::from_min_size(
        pos2(left + x + NUM_W, row_rect.top() + 2.0),
        vec2((PCT_W - 4.0) * share, ROW_H - 4.0),
    );
    painter.rect_filled(bar_rect, 1.0, bar_color);
    painter.text(
        pos2(left + x + NUM_W - 4.0, center_y),
        Align2::RIGHT_CENTER,
        unit.format(value),
        FontId::monospace(11.5),
        text_color,
    );
    painter.text(
        pos2(left + x + NUM_W + PCT_W - 4.0, center_y),
        Align2::RIGHT_CENTER,
        crate::fmt::pct_of(value as f64, total as f64),
        FontId::monospace(11.5),
        text_color,
    );
}

fn row_click(
    resp: &egui::Response,
    index: usize,
    row: &RowData<'_>,
    arrow_x: f32,
) -> Option<Click> {
    let mut clicked = None;
    if resp.clicked() {
        let on_arrow = resp
            .interact_pointer_pos()
            .is_some_and(|pointer| pointer.x >= arrow_x - 2.0 && pointer.x < arrow_x + ARROW_ZONE);
        clicked = Some(if row.arrow != Arrow::None && on_arrow {
            Click::Toggle(index)
        } else {
            Click::Select(index)
        });
    }
    if resp.double_clicked() && row.arrow != Arrow::None {
        clicked = Some(Click::Toggle(index));
    }
    clicked
}

fn draw_row_background(
    painter: &egui::Painter,
    rect: Rect,
    index: usize,
    row: &RowData<'_>,
    hovered: bool,
    pal: &Pal,
) {
    if row.selected {
        painter.rect_filled(rect, 0.0, pal.selected_row());
    } else if index % 2 == 1 {
        painter.rect_filled(rect, 0.0, pal.tint(8));
    }
    if hovered && !row.selected {
        painter.rect_filled(rect, 0.0, pal.tint(14));
    }
}

fn draw_clipped_text(painter: &egui::Painter, clip: Rect, text_x: f32, text: &str, color: Color32) {
    painter.with_clip_rect(clip).text(
        pos2(text_x, clip.center().y),
        Align2::LEFT_CENTER,
        text,
        FontId::proportional(12.0),
        color,
    );
}

fn draw_row(
    ui: &mut egui::Ui,
    index: usize,
    row: &RowData<'_>,
    layout: &Layout,
    cols: &Columns<'_>,
    total: u64,
    pal: &Pal,
) -> Option<Click> {
    let (rect, resp) = ui.allocate_exact_size(vec2(layout.width, ROW_H), Sense::click());
    let painter = ui.painter();
    let left = rect.left();
    let (text, weak) = (pal.text(), pal.weak());
    draw_row_background(painter, rect, index, row, resp.hovered(), pal);

    let indent_x = 6.0 + row.depth as f32 * INDENT;
    if row.arrow != Arrow::None {
        draw_arrow(
            painter,
            row.arrow,
            pos2(left + indent_x + 4.0, rect.center().y),
            weak,
        );
    }
    let name_rect = Rect::from_min_max(
        pos2(left + indent_x + ARROW_ZONE, rect.top()),
        pos2(left + layout.name_w, rect.bottom()),
    );
    let name_color = if row.highlight {
        pal.highlight()
    } else if row.suspect {
        pal.suspect()
    } else {
        text
    };
    draw_clipped_text(painter, name_rect, name_rect.left(), row.name, name_color);
    let module_rect = Rect::from_min_max(
        pos2(left + layout.module_x, rect.top()),
        pos2(left + layout.cost_x - 6.0, rect.bottom()),
    );
    draw_clipped_text(
        painter,
        module_rect,
        module_rect.left() + 4.0,
        row.module,
        weak,
    );

    let inclusive_bar = Color32::from_rgba_unmultiplied(70, 130, 200, 90);
    let self_bar = Color32::from_rgba_unmultiplied(200, 130, 70, 90);
    draw_cost_cell(
        painter,
        rect,
        layout.cost_x,
        row.cost,
        total,
        cols.unit,
        (inclusive_bar, text),
    );
    if let Some(self_cost) = row.self_cost {
        draw_cost_cell(
            painter,
            rect,
            layout.self_x,
            self_cost,
            total,
            cols.unit,
            (self_bar, text),
        );
    }
    row_click(&resp, index, row, left + indent_x)
}

/// Draw the table filling the available space. `total` is the number of samples that 100% means.
pub fn draw<'a>(
    ui: &mut egui::Ui,
    cols: &Columns<'_>,
    n_rows: usize,
    total: u64,
    mut row: impl FnMut(usize) -> RowData<'a>,
) -> Option<Click> {
    let layout = Layout::new(ui.available_width(), cols);
    let pal = Pal::of(ui);
    let mut clicked = draw_header(ui, &layout, cols, &pal);

    ui.spacing_mut().item_spacing.y = 0.0;
    egui::ScrollArea::vertical()
        .id_salt(cols.id)
        .auto_shrink([false, false])
        .show_rows(ui, ROW_H, n_rows, |ui, range| {
            for index in range {
                let row_data = row(index);
                if let Some(click) = draw_row(ui, index, &row_data, &layout, cols, total, &pal) {
                    clicked = Some(click);
                }
            }
        });
    clicked
}
