//! Grid painting (§5.27 "Rendering"): cell metrics from the monospace font; background runs as
//! batched rectangles; one text galley per row (cached by row content hash so unchanged rows
//! cost nothing); cursor, selection, and underline drawn on top. Colors resolve through
//! `ui::theme::Colors` (ANSI 0–255 from the theme, truecolor passed through, default fg/bg =
//! fg.primary/bg.base, cursor = accent, selection = bg.selected).
