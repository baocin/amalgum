//! Theme (§4): semantic tokens with light and dark values, the default terminal ANSI palette,
//! and the graph lane palette. UI code references tokens only — never a literal color
//! (`scripts/check-portability` rejects color literals under `src/ui/` outside `ui/theme.rs`).
//! The token tables here are the spec's tables, value for value.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    Light,
    Dark,
}

/// An sRGB color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// Parse `#RRGGBB` (case-insensitive). Anything else (wrong length, missing `#`,
    /// non-hex digits, shorthand `#RGB`) is rejected.
    pub fn hex(s: &str) -> Option<Self> {
        let digits = s.strip_prefix('#')?;
        if digits.len() != 6 || !digits.is_ascii() {
            return None;
        }
        let r = u8::from_str_radix(&digits[0..2], 16).ok()?;
        let g = u8::from_str_radix(&digits[2..4], 16).ok()?;
        let b = u8::from_str_radix(&digits[4..6], 16).ok()?;
        Some(Rgb(r, g, b))
    }

    /// WCAG 2.x relative luminance (sRGB linearization, 0.04045 threshold).
    pub fn luminance(self) -> f64 {
        fn linearize(channel: u8) -> f64 {
            let c = channel as f64 / 255.0;
            if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        }
        0.2126 * linearize(self.0) + 0.7152 * linearize(self.1) + 0.0722 * linearize(self.2)
    }
}

/// WCAG contrast ratio, 1.0–21.0.
pub fn contrast(a: Rgb, b: Rgb) -> f64 {
    let (la, lb) = (a.luminance(), b.luminance());
    let (lighter, darker) = if la >= lb { (la, lb) } else { (lb, la) };
    (lighter + 0.05) / (darker + 0.05)
}

/// Build an `Rgb` from a `0xRRGGBB` literal, at compile time, without a fallible parse.
const fn rgb(hex: u32) -> Rgb {
    Rgb(((hex >> 16) & 0xFF) as u8, ((hex >> 8) & 0xFF) as u8, (hex & 0xFF) as u8)
}

/// Every semantic token from §4, in table order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Token {
    BgBase,
    BgRaised,
    BgSunken,
    BgHover,
    BgSelected,
    BgSelectedUnfocused,
    FgPrimary,
    FgSecondary,
    FgDisabled,
    FgOnAccent,
    Border,
    BorderFocus,
    Accent,
    Success,
    Warning,
    Danger,
    StatusIdle,
    StatusHibernated,
    DiffAddBg,
    DiffAddFg,
    DiffAddWord,
    DiffDelBg,
    DiffDelFg,
    DiffDelWord,
    DiffHunkBg,
    DiffGutterFg,
    ConflictOurs,
    ConflictTheirs,
    BlameHot,
    BlameCold,
}

impl Token {
    pub const ALL: &'static [Token] = &[
        Token::BgBase,
        Token::BgRaised,
        Token::BgSunken,
        Token::BgHover,
        Token::BgSelected,
        Token::BgSelectedUnfocused,
        Token::FgPrimary,
        Token::FgSecondary,
        Token::FgDisabled,
        Token::FgOnAccent,
        Token::Border,
        Token::BorderFocus,
        Token::Accent,
        Token::Success,
        Token::Warning,
        Token::Danger,
        Token::StatusIdle,
        Token::StatusHibernated,
        Token::DiffAddBg,
        Token::DiffAddFg,
        Token::DiffAddWord,
        Token::DiffDelBg,
        Token::DiffDelFg,
        Token::DiffDelWord,
        Token::DiffHunkBg,
        Token::DiffGutterFg,
        Token::ConflictOurs,
        Token::ConflictTheirs,
        Token::BlameHot,
        Token::BlameCold,
    ];

    /// The spec name, e.g. `bg.selected.unfocused`.
    pub fn name(self) -> &'static str {
        use Token::*;
        match self {
            BgBase => "bg.base",
            BgRaised => "bg.raised",
            BgSunken => "bg.sunken",
            BgHover => "bg.hover",
            BgSelected => "bg.selected",
            BgSelectedUnfocused => "bg.selected.unfocused",
            FgPrimary => "fg.primary",
            FgSecondary => "fg.secondary",
            FgDisabled => "fg.disabled",
            FgOnAccent => "fg.on-accent",
            Border => "border",
            BorderFocus => "border.focus",
            Accent => "accent",
            Success => "success",
            Warning => "warning",
            Danger => "danger",
            StatusIdle => "status.idle",
            StatusHibernated => "status.hibernated",
            DiffAddBg => "diff.add.bg",
            DiffAddFg => "diff.add.fg",
            DiffAddWord => "diff.add.word",
            DiffDelBg => "diff.del.bg",
            DiffDelFg => "diff.del.fg",
            DiffDelWord => "diff.del.word",
            DiffHunkBg => "diff.hunk.bg",
            DiffGutterFg => "diff.gutter.fg",
            ConflictOurs => "conflict.ours",
            ConflictTheirs => "conflict.theirs",
            BlameHot => "blame.hot",
            BlameCold => "blame.cold",
        }
    }

    /// (light, dark) as `0xRRGGBB`, transcribed from the §4 "Semantic tokens" table.
    fn hex_pair(self) -> (u32, u32) {
        use Token::*;
        match self {
            BgBase => (0xFFFFFF, 0x1E1F22),
            BgRaised => (0xF5F5F7, 0x2B2D31),
            BgSunken => (0xEBEBEF, 0x17181A),
            BgHover => (0xEDEDF2, 0x33353A),
            BgSelected => (0xD9E7FF, 0x2C4370),
            BgSelectedUnfocused => (0xE8E8EC, 0x3A3C42),
            FgPrimary => (0x1D1D1F, 0xE6E6E8),
            FgSecondary => (0x5C5C61, 0xA0A0A6),
            FgDisabled => (0xA5A5AA, 0x5E5E64),
            FgOnAccent => (0xFFFFFF, 0xFFFFFF),
            Border => (0xD6D6DB, 0x3D3F45),
            BorderFocus => (0x0A66FF, 0x5B9BFF),
            Accent => (0x0A66FF, 0x5B9BFF),
            Success => (0x1A7F37, 0x3FB950),
            Warning => (0x9A6700, 0xD29922),
            Danger => (0xCF222E, 0xF85149),
            StatusIdle => (0x8E8E93, 0x6E6E76),
            StatusHibernated => (0xA5A5AA, 0x5E5E64),
            DiffAddBg => (0xDDF4E4, 0x1B3A26),
            DiffAddFg => (0x116329, 0x7EE787),
            DiffAddWord => (0xABE9B9, 0x2B6B3E),
            DiffDelBg => (0xFFE1E1, 0x3E1C1F),
            DiffDelFg => (0xA40E26, 0xFF7B72),
            DiffDelWord => (0xFFB3B3, 0x7A2D33),
            DiffHunkBg => (0xEEF3FF, 0x1F2A44),
            DiffGutterFg => (0x8E8E93, 0x6E6E76),
            ConflictOurs => (0xDDF4E4, 0x1B3A26),
            ConflictTheirs => (0xE5E1FF, 0x2A2545),
            BlameHot => (0xFFB3B3, 0x7A2D33),
            BlameCold => (0xF5F5F7, 0x2B2D31),
        }
    }
}

pub fn color(token: Token, mode: Mode) -> Rgb {
    let (light, dark) = token.hex_pair();
    rgb(if mode == Mode::Dark { dark } else { light })
}

/// (light, dark) as `0xRRGGBB` for ANSI indices 0–15, in table order (§4 "Terminal ANSI palette").
const ANSI_BASE: [(u32, u32); 16] = [
    (0x1D1D1F, 0x1E1F22), // 0 black
    (0xCF222E, 0xFF7B72), // 1 red
    (0x1A7F37, 0x3FB950), // 2 green
    (0x9A6700, 0xD29922), // 3 yellow
    (0x0A66FF, 0x5B9BFF), // 4 blue
    (0x8250DF, 0xA371F7), // 5 magenta
    (0x1B7C83, 0x39C5CF), // 6 cyan
    (0xD6D6DB, 0xE6E6E8), // 7 white
    (0x5C5C61, 0x6E6E76), // 8 bright black
    (0xA40E26, 0xFFA198), // 9 bright red
    (0x116329, 0x56D364), // 10 bright green
    (0x7A5200, 0xE3B341), // 11 bright yellow
    (0x0969DA, 0x79C0FF), // 12 bright blue
    (0x6639BA, 0xD2A8FF), // 13 bright magenta
    (0x15646B, 0x56D4DD), // 14 bright cyan
    (0xFFFFFF, 0xFFFFFF), // 15 bright white
];

/// One step of the xterm 256-color cube axis (0, 95, 135, 175, 215, 255).
fn cube_level(step: u8) -> u8 {
    if step == 0 { 0 } else { 55 + 40 * step }
}

/// Default terminal palette, indices 0–15 (§4 "Terminal ANSI palette"); indices 16–255 are the
/// standard xterm 256-color cube (16–231) and grayscale ramp (232–255), which are mode-independent.
pub fn ansi(index: u8, mode: Mode) -> Rgb {
    if let Some(&(light, dark)) = ANSI_BASE.get(index as usize) {
        return rgb(if mode == Mode::Dark { dark } else { light });
    }
    if index <= 231 {
        let i = index - 16;
        let r = cube_level(i / 36);
        let g = cube_level((i / 6) % 6);
        let b = cube_level(i % 6);
        Rgb(r, g, b)
    } else {
        let level = 8 + 10 * (index - 232);
        Rgb(level, level, level)
    }
}

/// (light, dark) as `0xRRGGBB` for the 8 graph lanes, in table order (§4 "Graph lane palette").
const LANE_COLORS: [(u32, u32); 8] = [
    (0x0A66FF, 0x5B9BFF),
    (0x1A7F37, 0x3FB950),
    (0xBF3989, 0xF778BA),
    (0x9A6700, 0xD29922),
    (0x8250DF, 0xA371F7),
    (0x0969DA, 0x79C0FF),
    (0xCF222E, 0xF85149),
    (0x1B7C83, 0x39C5CF),
];

/// Graph lane palette, 8 lanes (§4 "Graph lane palette"); `lane` wraps modulo 8.
pub fn lane(lane: usize, mode: Mode) -> Rgb {
    let (light, dark) = LANE_COLORS[lane % 8];
    rgb(if mode == Mode::Dark { dark } else { light })
}

/// Blame gutter heat: linear blend from `blame.cold` (t = 0, oldest) to `blame.hot` (t = 1).
pub fn blame_heat(t: f32, mode: Mode) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let cold = color(Token::BlameCold, mode);
    let hot = color(Token::BlameHot, mode);
    let mix = |c0: u8, c1: u8| -> u8 { (c0 as f32 + (c1 as f32 - c0 as f32) * t).round() as u8 };
    Rgb(mix(cold.0, hot.0), mix(cold.1, hot.1), mix(cold.2, hot.2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // -- Rgb::hex -----------------------------------------------------------------------------

    #[test]
    fn hex_parses_rrggbb_case_insensitive() {
        assert_eq!(Rgb::hex("#ffffff"), Some(Rgb(0xFF, 0xFF, 0xFF)));
        assert_eq!(Rgb::hex("#FFFFFF"), Some(Rgb(0xFF, 0xFF, 0xFF)));
        assert_eq!(Rgb::hex("#5B9BFF"), Some(Rgb(0x5B, 0x9B, 0xFF)));
        assert_eq!(Rgb::hex("#5b9bff"), Some(Rgb(0x5B, 0x9B, 0xFF)));
        assert_eq!(Rgb::hex("#000000"), Some(Rgb(0, 0, 0)));
    }

    #[test]
    fn hex_rejects_non_rrggbb() {
        assert_eq!(Rgb::hex("FFFFFF"), None, "missing #");
        assert_eq!(Rgb::hex("#FFF"), None, "shorthand not accepted");
        assert_eq!(Rgb::hex("#FFFFFFF"), None, "too long");
        assert_eq!(Rgb::hex("#FFFFF"), None, "too short");
        assert_eq!(Rgb::hex("#GGGGGG"), None, "non-hex digits");
        assert_eq!(Rgb::hex(""), None, "empty");
        assert_eq!(Rgb::hex("#"), None, "just a hash");
    }

    // -- luminance / contrast -------------------------------------------------------------------

    #[test]
    fn contrast_known_values() {
        assert_eq!(contrast(Rgb(0, 0, 0), Rgb(255, 255, 255)), 21.0);
        assert_eq!(contrast(Rgb(0x77, 0x77, 0x77), Rgb(0x77, 0x77, 0x77)), 1.0);
        let c = contrast(Rgb(0x77, 0x77, 0x77), Rgb(255, 255, 255));
        assert!((c - 4.48).abs() < 0.01, "#777777 on white ≈ 4.48, got {c}");
    }

    #[test]
    fn contrast_symmetric() {
        let a = Rgb(0x0A, 0x66, 0xFF);
        let b = Rgb(0xFF, 0xFF, 0xFF);
        assert_eq!(contrast(a, b), contrast(b, a));
    }

    // -- ansi 256-color cube / grayscale ramp ----------------------------------------------------

    #[test]
    fn ansi_256_cube_and_grayscale_known_values() {
        // Mode-independent: cube and grayscale sit above the 16-color base.
        assert_eq!(ansi(16, Mode::Dark), Rgb(0x00, 0x00, 0x00));
        assert_eq!(ansi(21, Mode::Dark), Rgb(0x00, 0x00, 0xFF));
        assert_eq!(ansi(231, Mode::Dark), Rgb(0xFF, 0xFF, 0xFF));
        assert_eq!(ansi(232, Mode::Dark), Rgb(0x08, 0x08, 0x08));
        assert_eq!(ansi(255, Mode::Dark), Rgb(0xEE, 0xEE, 0xEE));
        assert_eq!(ansi(16, Mode::Light), Rgb(0x00, 0x00, 0x00));
        assert_eq!(ansi(255, Mode::Light), Rgb(0xEE, 0xEE, 0xEE));
    }

    #[test]
    fn ansi_base_16_differs_by_mode() {
        assert_eq!(ansi(0, Mode::Light), Rgb(0x1D, 0x1D, 0x1F));
        assert_eq!(ansi(0, Mode::Dark), Rgb(0x1E, 0x1F, 0x22));
        assert_eq!(ansi(15, Mode::Light), Rgb(0xFF, 0xFF, 0xFF));
        assert_eq!(ansi(15, Mode::Dark), Rgb(0xFF, 0xFF, 0xFF));
    }

    // -- lane / blame_heat ------------------------------------------------------------------------

    #[test]
    fn lane_wraps_modulo_8() {
        assert_eq!(lane(0, Mode::Light), lane(8, Mode::Light));
        assert_eq!(lane(3, Mode::Dark), lane(11, Mode::Dark));
        assert_eq!(lane(0, Mode::Light), Rgb(0x0A, 0x66, 0xFF));
    }

    #[test]
    fn blame_heat_endpoints_and_clamp() {
        assert_eq!(blame_heat(0.0, Mode::Light), color(Token::BlameCold, Mode::Light));
        assert_eq!(blame_heat(1.0, Mode::Light), color(Token::BlameHot, Mode::Light));
        assert_eq!(blame_heat(-1.0, Mode::Dark), color(Token::BlameCold, Mode::Dark));
        assert_eq!(blame_heat(2.0, Mode::Dark), color(Token::BlameHot, Mode::Dark));
    }

    #[test]
    fn blame_heat_interpolates_linearly() {
        let cold = color(Token::BlameCold, Mode::Dark);
        let hot = color(Token::BlameHot, Mode::Dark);
        let mid = blame_heat(0.5, Mode::Dark);
        let expect = |c0: u8, c1: u8| ((c0 as f32 + c1 as f32) / 2.0).round() as u8;
        assert_eq!(mid, Rgb(expect(cold.0, hot.0), expect(cold.1, hot.1), expect(cold.2, hot.2)));
    }

    // -- docs/SPEC.md as source of truth -----------------------------------------------------

    fn spec_text() -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/SPEC.md");
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"))
    }

    /// The text strictly between `start` (a heading, excluded) and the next `end` heading.
    fn section<'a>(text: &'a str, start: &str, end: &str) -> &'a str {
        let s = text.find(start).unwrap_or_else(|| panic!("heading {start:?} not found")) + start.len();
        let rest = &text[s..];
        let e = rest.find(end).unwrap_or(rest.len());
        &rest[..e]
    }

    /// Parses a GitHub-flavoured Markdown table's data rows (skips the header and `---` rows),
    /// stripping backticks from each cell.
    fn table_data_rows(section: &str) -> Vec<Vec<String>> {
        section
            .lines()
            .filter(|l| l.trim_start().starts_with('|'))
            .map(|l| {
                l.trim()
                    .trim_matches('|')
                    .split('|')
                    .map(|c| c.trim().trim_matches('`').to_string())
                    .collect::<Vec<_>>()
            })
            .skip(2) // header row, then the |---|---|...| separator row
            .collect()
    }

    #[test]
    fn spec_semantic_tokens_table_matches() {
        let text = spec_text();
        let rows = table_data_rows(section(&text, "### Semantic tokens", "### Terminal ANSI palette"));
        assert_eq!(rows.len(), Token::ALL.len(), "table row count vs Token::ALL");
        for row in rows {
            let name = &row[0];
            let token = Token::ALL
                .iter()
                .copied()
                .find(|t| t.name() == name)
                .unwrap_or_else(|| panic!("no Token for spec name {name:?}"));
            let light = Rgb::hex(&row[1]).unwrap_or_else(|| panic!("bad light hex for {name}: {:?}", row[1]));
            let dark = Rgb::hex(&row[2]).unwrap_or_else(|| panic!("bad dark hex for {name}: {:?}", row[2]));
            assert_eq!(color(token, Mode::Light), light, "{name} light");
            assert_eq!(color(token, Mode::Dark), dark, "{name} dark");
        }
    }

    #[test]
    fn spec_ansi_table_matches() {
        let text = spec_text();
        let rows = table_data_rows(section(&text, "### Terminal ANSI palette", "### Graph lane palette"));
        assert_eq!(rows.len(), 16);
        for row in rows {
            let index: u8 = row[0].parse().unwrap_or_else(|_| panic!("bad ANSI index {:?}", row[0]));
            let light = Rgb::hex(&row[2]).unwrap_or_else(|| panic!("bad light hex for ansi {index}"));
            let dark = Rgb::hex(&row[3]).unwrap_or_else(|| panic!("bad dark hex for ansi {index}"));
            assert_eq!(ansi(index, Mode::Light), light, "ansi {index} light");
            assert_eq!(ansi(index, Mode::Dark), dark, "ansi {index} dark");
        }
    }

    #[test]
    fn spec_lane_table_matches() {
        let text = spec_text();
        let rows = table_data_rows(section(&text, "### Graph lane palette", "### Syntax theme"));
        assert_eq!(rows.len(), 8);
        for row in rows {
            let index: usize = row[0].parse().unwrap_or_else(|_| panic!("bad lane index {:?}", row[0]));
            let light = Rgb::hex(&row[1]).unwrap_or_else(|| panic!("bad light hex for lane {index}"));
            let dark = Rgb::hex(&row[2]).unwrap_or_else(|| panic!("bad dark hex for lane {index}"));
            assert_eq!(lane(index, Mode::Light), light, "lane {index} light");
            assert_eq!(lane(index, Mode::Dark), dark, "lane {index} dark");
        }
    }

    // -- WCAG AA (§4: "All foreground/background pairs meet WCAG AA") -------------------------
    //
    // The spec's own color values are transcribed verbatim (never adjusted to pass here). A
    // handful of pairs the spec itself calls out (text 4.5:1, UI/borders/graph lines 3:1) fall
    // short; they are listed explicitly below so any further drift is caught, and are reported
    // to a human as product decisions rather than silently "fixed".

    /// (fg token name, bg token name, mode, measured ratio) for every spec pair that fails its
    /// WCAG AA threshold. Measured against the transcribed spec colors above.
    const KNOWN_SPEC_CONTRAST_GAPS: &[(&str, &str, Mode, f64)] = &[
        ("diff.gutter.fg", "bg.base", Mode::Light, 3.260528410471992),
        ("diff.gutter.fg", "bg.base", Mode::Dark, 3.2605996191055557),
        ("border", "bg.base", Mode::Light, 1.4482009704361507),
        ("border", "bg.base", Mode::Dark, 1.5658490468764596),
        ("fg.secondary", "bg.selected", Mode::Dark, 3.7658344602388443),
        ("fg.on-accent", "accent", Mode::Dark, 2.771411155350167),
    ];

    #[test]
    fn wcag_aa_pairs_match_known_gaps() {
        let mut gaps: Vec<(String, String, Mode, f64)> = Vec::new();
        let mut check = |fg_name: &str, fg: Rgb, bg_name: &str, bg: Rgb, mode: Mode, min: f64| {
            let ratio = contrast(fg, bg);
            if ratio < min {
                gaps.push((fg_name.to_string(), bg_name.to_string(), mode, ratio));
            }
        };

        for &mode in &[Mode::Light, Mode::Dark] {
            let bg_tokens =
                [Token::BgBase, Token::BgRaised, Token::BgSunken, Token::BgHover, Token::BgSelected];
            for &fg_token in &[Token::FgPrimary, Token::FgSecondary] {
                for &bg_token in &bg_tokens {
                    check(
                        fg_token.name(),
                        color(fg_token, mode),
                        bg_token.name(),
                        color(bg_token, mode),
                        mode,
                        4.5,
                    );
                }
            }
            check(
                Token::FgOnAccent.name(),
                color(Token::FgOnAccent, mode),
                Token::Accent.name(),
                color(Token::Accent, mode),
                mode,
                4.5,
            );
            check(
                Token::DiffAddFg.name(),
                color(Token::DiffAddFg, mode),
                Token::DiffAddBg.name(),
                color(Token::DiffAddBg, mode),
                mode,
                4.5,
            );
            check(
                Token::DiffDelFg.name(),
                color(Token::DiffDelFg, mode),
                Token::DiffDelBg.name(),
                color(Token::DiffDelBg, mode),
                mode,
                4.5,
            );
            check(
                Token::DiffGutterFg.name(),
                color(Token::DiffGutterFg, mode),
                Token::BgBase.name(),
                color(Token::BgBase, mode),
                mode,
                4.5,
            );
            for &ui_token in &[Token::Success, Token::Warning, Token::Danger] {
                check(
                    ui_token.name(),
                    color(ui_token, mode),
                    Token::BgBase.name(),
                    color(Token::BgBase, mode),
                    mode,
                    3.0,
                );
            }
            check(
                Token::Border.name(),
                color(Token::Border, mode),
                Token::BgBase.name(),
                color(Token::BgBase, mode),
                mode,
                3.0,
            );
            for i in 0..8 {
                check(
                    &format!("lane.{i}"),
                    lane(i, mode),
                    Token::BgBase.name(),
                    color(Token::BgBase, mode),
                    mode,
                    3.0,
                );
            }
        }

        let mut expected: Vec<(String, String, Mode, f64)> = KNOWN_SPEC_CONTRAST_GAPS
            .iter()
            .map(|(fg, bg, mode, ratio)| (fg.to_string(), bg.to_string(), *mode, *ratio))
            .collect();
        gaps.sort_by(|a, b| (a.0.as_str(), a.1.as_str()).cmp(&(b.0.as_str(), b.1.as_str())));
        expected.sort_by(|a, b| (a.0.as_str(), a.1.as_str()).cmp(&(b.0.as_str(), b.1.as_str())));

        assert_eq!(
            gaps.len(),
            expected.len(),
            "failing WCAG pairs changed: {gaps:#?} vs known {expected:#?}"
        );
        for (got, want) in gaps.iter().zip(expected.iter()) {
            assert_eq!((&got.0, &got.1, got.2), (&want.0, &want.1, want.2), "gap identity");
            assert!(
                (got.3 - want.3).abs() < 1e-6,
                "gap ratio drifted for {}/{}: got {} want {}",
                got.0,
                got.1,
                got.3,
                want.3
            );
        }
    }
}
