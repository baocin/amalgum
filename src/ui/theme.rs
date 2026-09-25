//! Bridges `model::theme` tokens to egui. The only file under `src/ui/` allowed to build a
//! `Color32` from raw channels (`scripts/check-portability` enforces this).

use crate::model::theme::{self, Mode, Rgb, Token};
use egui::Color32;

/// Resolved colors for one mode. Cheap to copy; rebuilt when the theme flips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colors {
    pub mode: Mode,
}

impl Colors {
    pub fn new(mode: Mode) -> Self {
        Self { mode }
    }
    pub fn get(&self, token: Token) -> Color32 {
        rgb(theme::color(token, self.mode))
    }
    pub fn ansi(&self, index: u8) -> Color32 {
        rgb(theme::ansi(index, self.mode))
    }
    pub fn lane(&self, lane: usize) -> Color32 {
        rgb(theme::lane(lane, self.mode))
    }
    /// A token at partial opacity (hibernated overlay 50 %, dimmed graph rows 60 %).
    pub fn faded(&self, token: Token, opacity: f32) -> Color32 {
        self.get(token).gamma_multiply(opacity)
    }

    /// egui's widget visuals derived from the semantic tokens.
    pub fn visuals(&self) -> egui::Visuals {
        let mut v = match self.mode {
            Mode::Light => egui::Visuals::light(),
            Mode::Dark => egui::Visuals::dark(),
        };
        v.panel_fill = self.get(Token::BgRaised);
        v.window_fill = self.get(Token::BgRaised);
        v.extreme_bg_color = self.get(Token::BgSunken);
        v.faint_bg_color = self.get(Token::BgHover);
        v.override_text_color = Some(self.get(Token::FgPrimary));
        v.hyperlink_color = self.get(Token::Accent);
        v.warn_fg_color = self.get(Token::Warning);
        v.error_fg_color = self.get(Token::Danger);
        v.selection.bg_fill = self.get(Token::BgSelected);
        v.selection.stroke.color = self.get(Token::BorderFocus);
        v.window_stroke.color = self.get(Token::Border);
        v.widgets.noninteractive.bg_stroke.color = self.get(Token::Border);
        v.widgets.noninteractive.fg_stroke.color = self.get(Token::FgPrimary);
        v.widgets.inactive.bg_fill = self.get(Token::BgSunken);
        v.widgets.inactive.weak_bg_fill = self.get(Token::BgSunken);
        v.widgets.hovered.bg_fill = self.get(Token::BgHover);
        v.widgets.hovered.weak_bg_fill = self.get(Token::BgHover);
        v.widgets.active.bg_fill = self.get(Token::BgSelected);
        v
    }
}

fn rgb(c: Rgb) -> Color32 {
    Color32::from_rgb(c.0, c.1, c.2)
}
