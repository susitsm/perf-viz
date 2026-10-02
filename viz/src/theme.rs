//! Colours that differ between the dark and the light theme. Everything painted by hand goes
//! through a [`Pal`], so it stays readable in both; widgets use egui's own visuals.

use eframe::egui::{Color32, Ui};

#[derive(Clone, Copy, Debug)]
pub struct Pal {
    pub dark: bool,
}

impl Pal {
    pub fn of(ui: &Ui) -> Self {
        Pal {
            dark: ui.visuals().dark_mode,
        }
    }

    fn pick(&self, dark: Color32, light: Color32) -> Color32 {
        if self.dark { dark } else { light }
    }

    /// Regular and secondary text.
    pub fn text(&self) -> Color32 {
        self.pick(Color32::from_gray(215), Color32::from_gray(35))
    }
    pub fn weak(&self) -> Color32 {
        self.pick(Color32::from_gray(140), Color32::from_gray(110))
    }
    /// Text on a search match, and other emphasis (gold on dark, amber on light).
    pub fn highlight(&self) -> Color32 {
        self.pick(
            Color32::from_rgb(255, 215, 0),
            Color32::from_rgb(176, 120, 0),
        )
    }
    /// Suspect (untrustworthy) frames.
    pub fn suspect(&self) -> Color32 {
        self.pick(
            Color32::from_rgb(230, 100, 100),
            Color32::from_rgb(190, 40, 40),
        )
    }
    pub fn warning(&self) -> Color32 {
        self.pick(
            Color32::from_rgb(230, 170, 60),
            Color32::from_rgb(170, 100, 0),
        )
    }
    pub fn header_bg(&self) -> Color32 {
        self.pick(Color32::from_gray(38), Color32::from_gray(225))
    }
    pub fn selected_row(&self) -> Color32 {
        self.pick(
            Color32::from_rgb(30, 70, 110),
            Color32::from_rgb(180, 210, 245),
        )
    }
    /// A translucent tint over a row, `alpha` out of 255 (white on dark, black on light).
    pub fn tint(&self, alpha: u8) -> Color32 {
        if self.dark {
            Color32::from_white_alpha(alpha)
        } else {
            Color32::from_black_alpha(alpha)
        }
    }
    /// The outline drawn around the hovered flame box.
    pub fn hover_outline(&self) -> Color32 {
        self.pick(Color32::WHITE, Color32::BLACK)
    }
    /// Background of the timeline plot.
    pub fn plot_bg(&self) -> Color32 {
        self.pick(Color32::from_gray(28), Color32::from_gray(232))
    }
    pub fn axis(&self) -> Color32 {
        self.pick(Color32::from_gray(120), Color32::from_gray(130))
    }
    pub fn timeline_bar(&self) -> Color32 {
        self.pick(
            Color32::from_rgb(90, 150, 220),
            Color32::from_rgb(50, 110, 190),
        )
    }
    /// A translucent fill for the selected time range.
    pub fn band(&self) -> Color32 {
        self.pick(
            Color32::from_rgba_premultiplied(40, 34, 0, 40),
            Color32::from_rgba_premultiplied(45, 30, 0, 45),
        )
    }
    /// Fade a colour toward the background, for boxes that do not match the search.
    pub fn dim(&self, color: Color32) -> Color32 {
        let bg = if self.dark { 40.0 } else { 240.0 };
        let mix = |channel: u8| (channel as f32 * 0.3 + bg * 0.7) as u8;
        Color32::from_rgb(mix(color.r()), mix(color.g()), mix(color.b()))
    }
    /// The box for the flame graph's root ("all samples").
    pub fn root_box(&self) -> Color32 {
        self.pick(Color32::from_gray(90), Color32::from_gray(170))
    }
}
