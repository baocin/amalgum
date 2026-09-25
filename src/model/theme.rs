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
    /// Parse `#RRGGBB`.
    pub fn hex(s: &str) -> Option<Self> {
        todo!()
    }
    /// WCAG 2.x relative luminance.
    pub fn luminance(self) -> f64 {
        todo!()
    }
}

/// WCAG contrast ratio, 1.0–21.0.
pub fn contrast(a: Rgb, b: Rgb) -> f64 {
    todo!()
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
        todo!()
    }
}

pub fn color(token: Token, mode: Mode) -> Rgb {
    todo!()
}

/// Default terminal palette, indices 0–15 (§4 "Terminal ANSI palette").
pub fn ansi(index: u8, mode: Mode) -> Rgb {
    todo!()
}

/// Graph lane palette, 8 lanes (§4 "Graph lane palette"); `lane` wraps modulo 8.
pub fn lane(lane: usize, mode: Mode) -> Rgb {
    todo!()
}

/// Blame gutter heat: linear blend from `blame.cold` (t = 0, oldest) to `blame.hot` (t = 1).
pub fn blame_heat(t: f32, mode: Mode) -> Rgb {
    todo!()
}
