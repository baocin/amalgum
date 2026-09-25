//! Pure translation of egui key events into the bytes a terminal expects (xterm conventions):
//! printable text as UTF-8, Enter `\r`, Backspace `\x7f`, Tab `\t`, Shift+Tab `\x1b[Z`, Esc
//! `\x1b`, Ctrl+letter as C0 control codes, Alt as an ESC prefix, arrows/Home/End as `\x1b[A…`
//! (or `\x1bOA…` in application cursor mode), PageUp/Down, Insert/Delete, F1–F12, and modifier
//! parameters (`\x1b[1;5C` for Ctrl+Right). Pastes are wrapped in `\x1b[200~ … \x1b[201~` when
//! bracketed paste mode is on. Unit-tested without a PTY.
