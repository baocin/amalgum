//! Grid painting (§5.27 "Rendering"): cell metrics from the monospace font; background runs as
//! batched rectangles; one text galley per row (egui's font cache already keys galleys by the
//! `LayoutJob`'s content, so a row whose text/colors/flags are unchanged from last frame costs
//! nothing extra — we do not keep a second cache on top of it); cursor, selection, and underline
//! drawn on top. Colors resolve through `ui::theme::Colors` (ANSI 0–255 from the theme,
//! truecolor and OSC palette overrides passed through, default fg/bg = fg.primary/bg.base,
//! cursor = accent, selection = bg.selected).
//!
//! Everything in this file that resolves colors or merges cells into paint runs is a pure
//! function, unit-tested below without a painter. [`Snapshot`] is the owned, lock-free copy of
//! one frame's visible grid that [`super::TerminalPane::show`] builds while holding the term
//! lock and paints after releasing it (§6 "never hold the term lock while doing anything slow").

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors as TermColors;
use alacritty_terminal::vte::ansi::{Color as VteColor, CursorShape, NamedColor};
use egui::{Color32, Stroke, TextFormat};

use crate::model::theme::Token;
use crate::ui::theme::Colors;

/// A resolved, paint-ready copy of one grid cell (owned: no lifetime on the `Term`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedCell {
    pub c: char,
    pub fg: Color32,
    pub bg: Color32,
    pub italic: bool,
    pub underline: bool,
    pub strikeout: bool,
    /// `Flags::WIDE_CHAR_SPACER`: the second half of a double-width character. Excluded from
    /// the row's text (its column is still painted by the background pass) — see module docs
    /// on `merge_text_runs` for the known limitation this implies for CJK/emoji rows until
    /// fallback fonts are bundled (§5.27 "Rendering").
    pub skip: bool,
}

/// Where the cursor sits and how to paint it, already resolved from focus + `CursorShape`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CursorInfo {
    pub row: usize,
    pub col: usize,
    pub paint: CursorPaint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorPaint {
    None,
    FilledBlock,
    HollowBlock,
    Beam,
    Underline,
}

/// One frame's visible content, built while the term lock is held and painted after it is
/// released.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// `rows[visible_row][col]`, top of viewport first.
    pub rows: Vec<Vec<ResolvedCell>>,
    pub cursor: Option<CursorInfo>,
}

/// Whether `focused` and `shape` together should paint a cursor, and how (§5.27 "Rendering":
/// "block when focused, hollow box when not; respect `CursorShape`"). Pure; no painter needed.
pub fn cursor_paint(shape: CursorShape, focused: bool) -> CursorPaint {
    if shape == CursorShape::Hidden {
        return CursorPaint::None;
    }
    if !focused {
        return CursorPaint::HollowBlock;
    }
    match shape {
        CursorShape::Beam => CursorPaint::Beam,
        CursorShape::Underline => CursorPaint::Underline,
        CursorShape::Block | CursorShape::HollowBlock => CursorPaint::FilledBlock,
        CursorShape::Hidden => CursorPaint::None,
    }
}

/// Resolve one cell's paint colors from its raw `fg`/`bg`/`flags`, honouring `INVERSE`,
/// `HIDDEN`, `DIM`, and brightening 0–7 for `BOLD`. Pure; no painter needed.
pub fn cell_colors(
    cell_fg: VteColor,
    cell_bg: VteColor,
    flags: Flags,
    palette: &TermColors,
    colors: &Colors,
) -> (Color32, Color32) {
    let bold = flags.contains(Flags::BOLD);
    let resolved_fg = resolve_color(cell_fg, bold, palette, colors);
    let resolved_bg = resolve_color(cell_bg, false, palette, colors);
    let (mut fg, bg) =
        if flags.contains(Flags::INVERSE) { (resolved_bg, resolved_fg) } else { (resolved_fg, resolved_bg) };
    if flags.contains(Flags::HIDDEN) {
        fg = bg;
    } else if flags.contains(Flags::DIM) {
        fg = dim(fg);
    }
    (fg, bg)
}

/// Resolve one `vte::ansi::Color` to a paint color: named colors go through the ANSI palette
/// (with the theme's fg/bg/accent for the pseudo-colors), `Indexed` goes through the same
/// palette by number, and `Spec` (24-bit truecolor from the PTY stream) passes through as-is.
fn resolve_color(color: VteColor, bold: bool, palette: &TermColors, colors: &Colors) -> Color32 {
    match color {
        VteColor::Named(named) => resolve_named(named, bold, palette, colors),
        VteColor::Indexed(idx) => resolve_indexed(idx, bold, palette, colors),
        // Truecolor sent by the program over the wire (SGR 38;2;r;g;b) — a dynamic runtime
        // value from the PTY stream, not a UI literal, and there is no theme token for an
        // arbitrary RGB triple.
        VteColor::Spec(rgb) => Color32::from_rgb(rgb.r, rgb.g, rgb.b), // portability: allow
    }
}

fn resolve_named(named: NamedColor, bold: bool, palette: &TermColors, colors: &Colors) -> Color32 {
    use NamedColor::*;
    match named {
        Foreground => named_or(named, palette, colors.get(Token::FgPrimary)),
        Background => named_or(named, palette, colors.get(Token::BgBase)),
        Cursor => named_or(named, palette, colors.get(Token::Accent)),
        BrightForeground => named_or(named, palette, colors.get(Token::FgPrimary)),
        DimForeground => dim(named_or(named, palette, colors.get(Token::FgPrimary))),
        DimBlack => dim(resolve_indexed(0, false, palette, colors)),
        DimRed => dim(resolve_indexed(1, false, palette, colors)),
        DimGreen => dim(resolve_indexed(2, false, palette, colors)),
        DimYellow => dim(resolve_indexed(3, false, palette, colors)),
        DimBlue => dim(resolve_indexed(4, false, palette, colors)),
        DimMagenta => dim(resolve_indexed(5, false, palette, colors)),
        DimCyan => dim(resolve_indexed(6, false, palette, colors)),
        DimWhite => dim(resolve_indexed(7, false, palette, colors)),
        // Black..White (0..=7) and BrightBlack..BrightWhite (8..=15): the discriminant is the
        // ANSI index directly.
        _ => resolve_indexed(named as u8, bold, palette, colors),
    }
}

/// A palette override for one of the three named pseudo-colors (set by OSC 10/11/12), or the
/// theme default when none was set.
fn named_or(named: NamedColor, palette: &TermColors, default: Color32) -> Color32 {
    match palette[named] {
        Some(rgb) => rgb_color(rgb),
        None => default,
    }
}

/// ANSI index 0–255: 0–15 through the theme (with the bold-brightens-0..7 rule), 16–255 through
/// `Colors::ansi`'s xterm cube/grayscale, honouring an OSC 4 palette override at that index.
fn resolve_indexed(idx: u8, bold: bool, palette: &TermColors, colors: &Colors) -> Color32 {
    let idx = if bold && idx < 8 { idx + 8 } else { idx };
    match palette[idx as usize] {
        Some(rgb) => rgb_color(rgb),
        None => colors.ansi(idx),
    }
}

fn rgb_color(rgb: alacritty_terminal::vte::ansi::Rgb) -> Color32 {
    Color32::from_rgb(rgb.r, rgb.g, rgb.b) // portability: allow — dynamic OSC palette override.
}

/// `Flags::DIM` and the `DimX` named colors: a fixed 30% reduction, since the theme defines no
/// separate dim table (§4 lists only the 16 base ANSI colors).
fn dim(color: Color32) -> Color32 {
    color.gamma_multiply(0.7)
}

/// Merge a row's per-column background colors into `(start_col, end_col_exclusive, color)`
/// rectangles. Pure; no painter needed.
pub fn merge_bg_runs(bgs: &[Color32]) -> Vec<(usize, usize, Color32)> {
    let mut runs = Vec::new();
    let mut start = 0;
    for i in 1..=bgs.len() {
        if i == bgs.len() || bgs[i] != bgs[start] {
            runs.push((start, i, bgs[start]));
            start = i;
        }
    }
    runs
}

/// One run of a row's text sharing the same paint attributes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlyphStyle {
    pub fg: Color32,
    pub italic: bool,
    pub underline: bool,
    pub strikeout: bool,
}

/// Merge a row's cells into `(text, style)` runs, skipping wide-char spacers. Pure; no painter
/// needed — turning the result into a `LayoutJob` needs a `FontId` and is done by
/// [`row_layout_job`].
pub fn merge_text_runs(cells: &[ResolvedCell]) -> Vec<(String, GlyphStyle)> {
    let mut runs: Vec<(String, GlyphStyle)> = Vec::new();
    for cell in cells {
        if cell.skip {
            continue;
        }
        let style = GlyphStyle {
            fg: cell.fg,
            italic: cell.italic,
            underline: cell.underline,
            strikeout: cell.strikeout,
        };
        match runs.last_mut() {
            Some((text, last)) if *last == style => text.push(cell.c),
            _ => runs.push((cell.c.to_string(), style)),
        }
    }
    runs
}

/// Build one row's `LayoutJob` from its merged text runs. Not unit-tested here (it only
/// assembles a value from already-tested [`merge_text_runs`] output; laying it out into a
/// galley is egui's job and is cached by `LayoutJob` content, per the module docs).
pub fn row_layout_job(runs: &[(String, GlyphStyle)], font_id: &egui::FontId) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob { break_on_newline: false, ..Default::default() };
    for (text, style) in runs {
        let underline = if style.underline { Stroke::new(1.0, style.fg) } else { Stroke::NONE };
        let strikethrough = if style.strikeout { Stroke::new(1.0, style.fg) } else { Stroke::NONE };
        let format = TextFormat {
            font_id: font_id.clone(),
            color: style.fg,
            italics: style.italic,
            underline,
            strikethrough,
            ..Default::default()
        };
        job.append(text, 0.0, format);
    }
    job
}

/// Paint one built [`Snapshot`] into `rect` (already unlocked — see module docs). `cell` is
/// `(width, height)` in points; `font_id` is the same one `show()` used to measure it.
pub fn paint(
    ui: &egui::Ui,
    rect: egui::Rect,
    snapshot: &Snapshot,
    cell: (f32, f32),
    font_id: &egui::FontId,
    colors: &Colors,
) {
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, colors.get(Token::BgBase));
    let (cw, ch) = cell;

    for (row_index, row) in snapshot.rows.iter().enumerate() {
        let row_top = rect.min.y + row_index as f32 * ch;
        if row_top > rect.max.y {
            break;
        }

        let bgs: Vec<Color32> = row.iter().map(|c| c.bg).collect();
        for (start, end, color) in merge_bg_runs(&bgs) {
            if color == colors.get(Token::BgBase) {
                continue; // already painted by the single full-pane fill above.
            }
            let run_rect = egui::Rect::from_min_size(
                egui::pos2(rect.min.x + start as f32 * cw, row_top),
                egui::vec2((end - start) as f32 * cw, ch),
            );
            painter.rect_filled(run_rect, 0.0, color);
        }

        let runs = merge_text_runs(row);
        if !runs.is_empty() {
            let job = row_layout_job(&runs, font_id);
            let galley = ui.ctx().fonts_mut(|f| f.layout_job(job));
            painter.galley(egui::pos2(rect.min.x, row_top), galley, colors.get(Token::FgPrimary));
        }
    }

    if let Some(cursor) = snapshot.cursor {
        paint_cursor(painter, rect, cursor, cell, colors);
    }
}

fn paint_cursor(
    painter: &egui::Painter,
    rect: egui::Rect,
    cursor: CursorInfo,
    cell: (f32, f32),
    colors: &Colors,
) {
    if cursor.paint == CursorPaint::None {
        return;
    }
    let (cw, ch) = cell;
    let origin = egui::pos2(rect.min.x + cursor.col as f32 * cw, rect.min.y + cursor.row as f32 * ch);
    let cell_rect = egui::Rect::from_min_size(origin, egui::vec2(cw, ch));
    let accent = colors.get(Token::Accent);
    match cursor.paint {
        CursorPaint::None => {}
        CursorPaint::FilledBlock => {
            painter.rect_filled(cell_rect, 0.0, accent);
        }
        CursorPaint::HollowBlock => {
            painter.rect_stroke(cell_rect, 0.0, Stroke::new(1.0, accent), egui::StrokeKind::Inside);
        }
        CursorPaint::Beam => {
            painter.vline(cell_rect.min.x, cell_rect.y_range(), Stroke::new(1.5, accent));
        }
        CursorPaint::Underline => {
            painter.hline(cell_rect.x_range(), cell_rect.max.y - 1.0, Stroke::new(1.5, accent));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::theme::Mode;
    use alacritty_terminal::term::color::Colors as TermColors;
    use alacritty_terminal::vte::ansi::Rgb as VteRgb;

    fn colors() -> Colors {
        Colors::new(Mode::Dark)
    }

    fn palette() -> TermColors {
        TermColors::default()
    }

    // -- cursor_paint ---------------------------------------------------------------------

    #[test]
    fn hidden_shape_never_paints() {
        assert_eq!(cursor_paint(CursorShape::Hidden, true), CursorPaint::None);
        assert_eq!(cursor_paint(CursorShape::Hidden, false), CursorPaint::None);
    }

    #[test]
    fn unfocused_is_always_hollow_unless_hidden() {
        assert_eq!(cursor_paint(CursorShape::Block, false), CursorPaint::HollowBlock);
        assert_eq!(cursor_paint(CursorShape::Beam, false), CursorPaint::HollowBlock);
        assert_eq!(cursor_paint(CursorShape::Underline, false), CursorPaint::HollowBlock);
        assert_eq!(cursor_paint(CursorShape::HollowBlock, false), CursorPaint::HollowBlock);
    }

    #[test]
    fn focused_respects_shape() {
        assert_eq!(cursor_paint(CursorShape::Block, true), CursorPaint::FilledBlock);
        assert_eq!(cursor_paint(CursorShape::HollowBlock, true), CursorPaint::FilledBlock);
        assert_eq!(cursor_paint(CursorShape::Beam, true), CursorPaint::Beam);
        assert_eq!(cursor_paint(CursorShape::Underline, true), CursorPaint::Underline);
    }

    // -- color resolution -------------------------------------------------------------------

    #[test]
    fn named_ansi_0_to_15_match_theme_ansi_table() {
        let c = colors();
        let p = palette();
        for i in 0u8..16 {
            let named = named_color_for_index(i);
            assert_eq!(resolve_named(named, false, &p, &c), c.ansi(i), "index {i}");
        }
    }

    #[test]
    fn bold_brightens_0_to_7_but_not_8_to_15() {
        let c = colors();
        let p = palette();
        for i in 0u8..8 {
            let named = named_color_for_index(i);
            assert_eq!(resolve_named(named, true, &p, &c), c.ansi(i + 8), "index {i}");
        }
        for i in 8u8..16 {
            let named = named_color_for_index(i);
            assert_eq!(resolve_named(named, true, &p, &c), c.ansi(i), "index {i}");
        }
    }

    #[test]
    fn indexed_256_cube_passes_through_ansi() {
        let c = colors();
        let p = palette();
        for i in [16u8, 100, 231, 232, 255] {
            assert_eq!(resolve_indexed(i, false, &p, &c), c.ansi(i));
        }
    }

    #[test]
    fn indexed_bold_brightens_low_eight_only() {
        let c = colors();
        let p = palette();
        assert_eq!(resolve_indexed(3, true, &p, &c), c.ansi(11));
        assert_eq!(resolve_indexed(20, true, &p, &c), c.ansi(20));
    }

    #[test]
    fn spec_truecolor_passes_through_exactly() {
        let c = colors();
        let p = palette();
        let rgb = VteRgb { r: 12, g: 200, b: 77 };
        assert_eq!(resolve_color(VteColor::Spec(rgb), false, &p, &c), Color32::from_rgb(12, 200, 77)); // portability: allow
    }

    #[test]
    fn named_foreground_background_cursor_use_theme_defaults() {
        let c = colors();
        let p = palette();
        assert_eq!(resolve_named(NamedColor::Foreground, false, &p, &c), c.get(Token::FgPrimary));
        assert_eq!(resolve_named(NamedColor::Background, false, &p, &c), c.get(Token::BgBase));
        assert_eq!(resolve_named(NamedColor::Cursor, false, &p, &c), c.get(Token::Accent));
    }

    #[test]
    fn palette_override_wins_over_theme_default() {
        let c = colors();
        let mut p = palette();
        p[NamedColor::Background] = Some(VteRgb { r: 10, g: 20, b: 30 });
        assert_eq!(resolve_named(NamedColor::Background, false, &p, &c), Color32::from_rgb(10, 20, 30)); // portability: allow
    }

    #[test]
    fn palette_override_wins_for_indexed_slot() {
        let c = colors();
        let mut p = palette();
        p[3usize] = Some(VteRgb { r: 1, g: 2, b: 3 });
        assert_eq!(resolve_indexed(3, false, &p, &c), Color32::from_rgb(1, 2, 3)); // portability: allow
    }

    #[test]
    fn dim_named_colors_are_dimmer_than_their_base() {
        let c = colors();
        let p = palette();
        let base = resolve_indexed(1, false, &p, &c);
        let dimmed = resolve_named(NamedColor::DimRed, false, &p, &c);
        assert_eq!(dimmed, base.gamma_multiply(0.7));
        assert_ne!(dimmed, base);
    }

    // -- cell_colors: flags ------------------------------------------------------------------

    #[test]
    fn plain_cell_uses_fg_and_bg_unchanged() {
        let c = colors();
        let p = palette();
        let (fg, bg) = cell_colors(
            VteColor::Named(NamedColor::Foreground),
            VteColor::Named(NamedColor::Background),
            Flags::empty(),
            &p,
            &c,
        );
        assert_eq!(fg, c.get(Token::FgPrimary));
        assert_eq!(bg, c.get(Token::BgBase));
    }

    #[test]
    fn inverse_swaps_fg_and_bg() {
        let c = colors();
        let p = palette();
        let (fg, bg) = cell_colors(
            VteColor::Named(NamedColor::Foreground),
            VteColor::Named(NamedColor::Background),
            Flags::INVERSE,
            &p,
            &c,
        );
        assert_eq!(fg, c.get(Token::BgBase));
        assert_eq!(bg, c.get(Token::FgPrimary));
    }

    #[test]
    fn hidden_makes_fg_match_bg() {
        let c = colors();
        let p = palette();
        let (fg, bg) = cell_colors(
            VteColor::Named(NamedColor::Red),
            VteColor::Named(NamedColor::Background),
            Flags::HIDDEN,
            &p,
            &c,
        );
        assert_eq!(fg, bg);
    }

    #[test]
    fn dim_flag_dims_the_resolved_fg_only() {
        let c = colors();
        let p = palette();
        let (fg, bg) = cell_colors(
            VteColor::Named(NamedColor::Red),
            VteColor::Named(NamedColor::Background),
            Flags::DIM,
            &p,
            &c,
        );
        assert_eq!(fg, c.ansi(1).gamma_multiply(0.7));
        assert_eq!(bg, c.get(Token::BgBase));
    }

    #[test]
    fn bold_brightens_fg_not_bg() {
        let c = colors();
        let p = palette();
        let (fg, bg) = cell_colors(
            VteColor::Named(NamedColor::Red),
            VteColor::Named(NamedColor::Red),
            Flags::BOLD,
            &p,
            &c,
        );
        assert_eq!(fg, c.ansi(9));
        assert_eq!(bg, c.ansi(1));
    }

    // -- merge_bg_runs -------------------------------------------------------------------

    #[test]
    fn empty_row_merges_to_no_runs() {
        assert_eq!(merge_bg_runs(&[]), vec![]);
    }

    #[test]
    fn uniform_row_merges_to_one_run() {
        let c = colors();
        let bg = c.get(Token::BgBase);
        let row = vec![bg; 5];
        assert_eq!(merge_bg_runs(&row), vec![(0, 5, bg)]);
    }

    #[test]
    fn alternating_colors_never_merge() {
        let c = colors();
        let a = c.get(Token::BgBase);
        let b = c.get(Token::BgSelected);
        let row = vec![a, b, a, b];
        assert_eq!(merge_bg_runs(&row), vec![(0, 1, a), (1, 2, b), (2, 3, a), (3, 4, b)]);
    }

    #[test]
    fn a_run_in_the_middle_is_a_single_rectangle() {
        let c = colors();
        let a = c.get(Token::BgBase);
        let b = c.get(Token::BgSelected);
        let row = vec![a, a, b, b, b, a];
        assert_eq!(merge_bg_runs(&row), vec![(0, 2, a), (2, 5, b), (5, 6, a)]);
    }

    // -- merge_text_runs -------------------------------------------------------------------

    fn cell(c: char, fg: Color32, skip: bool) -> ResolvedCell {
        ResolvedCell {
            c,
            fg,
            bg: Color32::TRANSPARENT,
            italic: false,
            underline: false,
            strikeout: false,
            skip,
        }
    }

    #[test]
    fn empty_row_has_no_runs() {
        assert_eq!(merge_text_runs(&[]), vec![]);
    }

    #[test]
    fn same_style_run_concatenates_text() {
        let cs = colors();
        let fg = cs.get(Token::FgPrimary);
        let row = [cell('h', fg, false), cell('i', fg, false)];
        let runs = merge_text_runs(&row);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].0, "hi");
        assert_eq!(runs[0].1.fg, fg);
    }

    #[test]
    fn color_change_starts_a_new_run() {
        let cs = colors();
        let a = cs.get(Token::FgPrimary);
        let b = cs.get(Token::Accent);
        let row = [cell('a', a, false), cell('b', b, false)];
        let runs = merge_text_runs(&row);
        assert_eq!(
            runs,
            vec![
                ("a".to_string(), GlyphStyle { fg: a, italic: false, underline: false, strikeout: false }),
                ("b".to_string(), GlyphStyle { fg: b, italic: false, underline: false, strikeout: false })
            ]
        );
    }

    #[test]
    fn wide_char_spacer_is_skipped_but_does_not_break_the_run() {
        let cs = colors();
        let fg = cs.get(Token::FgPrimary);
        // "话" occupies two grid columns; the second is a spacer with the same style.
        let row = [cell('话', fg, false), cell(' ', fg, true), cell('!', fg, false)];
        let runs = merge_text_runs(&row);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].0, "话!");
    }

    #[test]
    fn style_change_across_a_skipped_cell_still_splits() {
        let cs = colors();
        let a = cs.get(Token::FgPrimary);
        let b = cs.get(Token::Accent);
        let row = [cell('话', a, false), cell(' ', a, true), cell('!', b, false)];
        let runs = merge_text_runs(&row);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].0, "话");
        assert_eq!(runs[1].0, "!");
    }

    #[test]
    fn italic_underline_strikeout_each_split_a_run() {
        let cs = colors();
        let fg = cs.get(Token::FgPrimary);
        let plain = cell('a', fg, false);
        let mut italic = cell('b', fg, false);
        italic.italic = true;
        let row = [plain, italic];
        let runs = merge_text_runs(&row);
        assert_eq!(runs.len(), 2);
    }

    /// Test-only inverse of the ANSI index → `NamedColor` mapping used by production code
    /// (`named as u8`), so the color-table tests above can drive every index 0..16.
    fn named_color_for_index(i: u8) -> NamedColor {
        use NamedColor::*;
        match i {
            0 => Black,
            1 => Red,
            2 => Green,
            3 => Yellow,
            4 => Blue,
            5 => Magenta,
            6 => Cyan,
            7 => White,
            8 => BrightBlack,
            9 => BrightRed,
            10 => BrightGreen,
            11 => BrightYellow,
            12 => BrightBlue,
            13 => BrightMagenta,
            14 => BrightCyan,
            15 => BrightWhite,
            _ => unreachable!(),
        }
    }
}
