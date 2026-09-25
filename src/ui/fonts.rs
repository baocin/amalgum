//! Bundled fonts (§1, §4 "Typography"): Inter for the UI, JetBrains Mono for terminals, code,
//! and hashes — identical rendering on every OS. egui's default fonts stay behind them as
//! fallbacks for symbols neither covers (status dots, emoji). Both fonts are SIL OFL 1.1; the
//! license texts ship beside the binary (assets/fonts/*-OFL.txt).

use egui::{FontData, FontDefinitions, FontFamily, FontId, TextStyle};
use std::sync::Arc;

/// Powerline's branch symbol (present in JetBrains Mono), used where the spec draws `⑂`.
pub const BRANCH: &str = "\u{E0A0}";

pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    for (name, bytes) in [
        ("Inter", &include_bytes!("../../assets/fonts/Inter-Regular.ttf")[..]),
        ("JetBrainsMono", &include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf")[..]),
    ] {
        fonts.font_data.insert(name.into(), Arc::new(FontData::from_static(bytes)));
    }
    for (family, order) in [
        (FontFamily::Proportional, ["Inter", "JetBrainsMono"]),
        (FontFamily::Monospace, ["JetBrainsMono", "Inter"]),
    ] {
        let list = fonts.families.entry(family).or_default();
        for (i, name) in order.into_iter().enumerate() {
            list.insert(i, name.into());
        }
    }
    ctx.set_fonts(fonts);
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, FontId::proportional(11.0)),
            (TextStyle::Body, FontId::proportional(13.0)),
            (TextStyle::Button, FontId::proportional(13.0)),
            (TextStyle::Heading, FontId::proportional(15.0)),
            (TextStyle::Monospace, FontId::monospace(12.0)),
        ]
        .into();
    });
}
