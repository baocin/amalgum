//! Pure translation of egui key events into the bytes a terminal expects (xterm conventions):
//! printable text as UTF-8, Enter `\r`, Backspace `\x7f`, Tab `\t`, Shift+Tab `\x1b[Z`, Esc
//! `\x1b`, Ctrl+letter as C0 control codes, Alt as an ESC prefix, arrows/Home/End as `\x1b[A…`
//! (or `\x1bOA…` in application cursor mode), PageUp/Down, Insert/Delete, F1–F12, and modifier
//! parameters (`\x1b[1;5C` for Ctrl+Right). Pastes are wrapped in `\x1b[200~ … \x1b[201~` when
//! bracketed paste mode is on. Unit-tested without a PTY.
//!
//! egui delivers printable text as `Event::Text`, separately from `Event::Key` — so [`key_bytes`]
//! returns `None` for a plain letter/digit/punctuation press (avoiding double input) and only
//! acts when Ctrl is held (a C0 control code). **Alt+letter is deliberately not ESC-prefixed
//! here**: on macOS, Option+letter composes accented text delivered as `Event::Text` (e.g.
//! Option+A → "å"), and sending an ESC-prefixed byte too would duplicate/corrupt that input.
//! Since `src/ui/` code cannot branch on which OS it runs on (§1 portability rules), the same
//! rule applies on Linux/Windows: Alt+letter/digit/punctuation relies
//! entirely on whatever `Event::Text` the platform sends (which may be nothing, a known,
//! documented gap versus classic xterm's Alt-as-Meta for those keys). Alt combined with a
//! navigation key (arrows, Home/End, Page Up/Down, Insert/Delete, F-keys) has no such ambiguity
//! and folds into the xterm modifier-parameter scheme below like Ctrl and Shift do.
//!
//! egui-winit also turns `Ctrl`/`Cmd` + `C`/`X`/`V` into synthesized `Event::Copy` / `Event::Cut`
//! / `Event::Paste(text)` instead of an `Event::Key` at all. [`copy_event_action`],
//! [`cut_event_action`], and [`paste_event_action`] decide what those should do: forward to the
//! app's copy/cut/paste of the terminal selection when the modifiers resolve to the keymap's
//! `Mod` chord (Ctrl+Shift+C on Linux, any Cmd+C on macOS — see `ui::shortcuts::chord`), or send
//! the bare Ctrl+C/X/V control byte a shell expects otherwise.

use egui::{Key, Modifiers};

/// The C0 control code `Ctrl+<key>` sends for the keys xterm maps this way. `None` for keys
/// with no such mapping (letters outside `Ctrl` fall through to `Event::Text`; see module docs).
fn ctrl_c0(key: Key) -> Option<u8> {
    match key {
        Key::A => Some(1),
        Key::B => Some(2),
        Key::C => Some(3),
        Key::D => Some(4),
        Key::E => Some(5),
        Key::F => Some(6),
        Key::G => Some(7),
        Key::H => Some(8),
        Key::I => Some(9),
        Key::J => Some(10),
        Key::K => Some(11),
        Key::L => Some(12),
        Key::M => Some(13),
        Key::N => Some(14),
        Key::O => Some(15),
        Key::P => Some(16),
        Key::Q => Some(17),
        Key::R => Some(18),
        Key::S => Some(19),
        Key::T => Some(20),
        Key::U => Some(21),
        Key::V => Some(22),
        Key::W => Some(23),
        Key::X => Some(24),
        Key::Y => Some(25),
        Key::Z => Some(26),
        // Ctrl+[ → Esc.
        Key::OpenBracket => Some(0x1b),
        Key::Backslash => Some(0x1c),
        Key::CloseBracket => Some(0x1d),
        _ => None,
    }
}

fn alt_prefixed(bytes: &[u8], alt: bool) -> Vec<u8> {
    if alt {
        let mut out = Vec::with_capacity(bytes.len() + 1);
        out.push(0x1b);
        out.extend_from_slice(bytes);
        out
    } else {
        bytes.to_vec()
    }
}

/// The xterm modifier parameter (`CSI 1;<n><final>`): 2=Shift, 3=Alt, 4=Shift+Alt, 5=Ctrl, …
fn modifier_param(mods: Modifiers) -> u8 {
    1 + mods.shift as u8 + (mods.alt as u8) * 2 + (mods.ctrl as u8) * 4
}

fn has_modifier(mods: Modifiers) -> bool {
    mods.shift || mods.alt || mods.ctrl
}

/// `CSI <final>` with no modifiers, `CSI 1;<n><final>` with any.
fn csi(final_byte: char, mods: Modifiers) -> Vec<u8> {
    if has_modifier(mods) {
        format!("\x1b[1;{}{}", modifier_param(mods), final_byte).into_bytes()
    } else {
        format!("\x1b[{final_byte}").into_bytes()
    }
}

/// Arrows/Home/End: `SS3 <final>` (`ESC O <final>`) in application-cursor mode with no
/// modifiers, else the [`csi`] form (xterm always uses CSI once modifiers are involved).
fn ss3_or_csi(final_byte: char, mods: Modifiers, app_cursor: bool) -> Vec<u8> {
    if !has_modifier(mods) && app_cursor {
        format!("\x1bO{final_byte}").into_bytes()
    } else {
        csi(final_byte, mods)
    }
}

/// `CSI <n>~` with no modifiers, `CSI <n>;<m>~` with any (Insert/Delete/PageUp/PageDown/F5-F12).
fn tilde(n: u32, mods: Modifiers) -> Vec<u8> {
    if has_modifier(mods) {
        format!("\x1b[{};{}~", n, modifier_param(mods)).into_bytes()
    } else {
        format!("\x1b[{n}~").into_bytes()
    }
}

/// F1–F4: `SS3 <final>` with no modifiers, `CSI 1;<n><final>` with any (xterm has no tilde form
/// for these four).
fn fkey_low(final_byte: char, mods: Modifiers) -> Vec<u8> {
    if has_modifier(mods) {
        format!("\x1b[1;{}{}", modifier_param(mods), final_byte).into_bytes()
    } else {
        format!("\x1bO{final_byte}").into_bytes()
    }
}

/// Translate one non-text key press into the bytes to send the PTY, or `None` if this key is
/// handled by `Event::Text` instead (see module docs).
pub fn key_bytes(key: Key, mods: Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
    if let Some(c0) = ctrl_c0(key) {
        // Without Ctrl, none of these keys are handled here — a letter's plain/Alt-only press
        // is `Event::Text` (see module docs), and `[`/`\`/`]` fall through to the same.
        return mods.ctrl.then(|| alt_prefixed(&[c0], mods.alt));
    }

    match key {
        Key::Enter => Some(alt_prefixed(b"\r", mods.alt)),
        Key::Backspace => Some(alt_prefixed(b"\x7f", mods.alt)),
        Key::Tab => Some(alt_prefixed(if mods.shift { b"\x1b[Z" } else { b"\t" }, mods.alt)),
        Key::Escape => Some(alt_prefixed(b"\x1b", mods.alt)),
        Key::Space if mods.ctrl => Some(alt_prefixed(&[0], mods.alt)),
        Key::Space if mods.alt => Some(vec![0x1b, b' ']),

        Key::ArrowUp => Some(ss3_or_csi('A', mods, app_cursor)),
        Key::ArrowDown => Some(ss3_or_csi('B', mods, app_cursor)),
        Key::ArrowRight => Some(ss3_or_csi('C', mods, app_cursor)),
        Key::ArrowLeft => Some(ss3_or_csi('D', mods, app_cursor)),
        Key::Home => Some(ss3_or_csi('H', mods, app_cursor)),
        Key::End => Some(ss3_or_csi('F', mods, app_cursor)),

        Key::Insert => Some(tilde(2, mods)),
        Key::Delete => Some(tilde(3, mods)),
        Key::PageUp => Some(tilde(5, mods)),
        Key::PageDown => Some(tilde(6, mods)),

        Key::F1 => Some(fkey_low('P', mods)),
        Key::F2 => Some(fkey_low('Q', mods)),
        Key::F3 => Some(fkey_low('R', mods)),
        Key::F4 => Some(fkey_low('S', mods)),
        Key::F5 => Some(tilde(15, mods)),
        Key::F6 => Some(tilde(17, mods)),
        Key::F7 => Some(tilde(18, mods)),
        Key::F8 => Some(tilde(19, mods)),
        Key::F9 => Some(tilde(20, mods)),
        Key::F10 => Some(tilde(21, mods)),
        Key::F11 => Some(tilde(23, mods)),
        Key::F12 => Some(tilde(24, mods)),

        _ => None,
    }
}

/// `Event::Text` → UTF-8 bytes, verbatim.
pub fn text_bytes(text: &str) -> Vec<u8> {
    text.as_bytes().to_vec()
}

/// Wrap a paste in bracketed-paste markers when the terminal enabled that mode.
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return text.as_bytes().to_vec();
    }
    let mut out = Vec::with_capacity(text.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

/// What a synthesized `Event::Copy`/`Event::Cut`/`Event::Paste` should do (see module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardAction {
    /// Copy the terminal's current selection to the OS clipboard.
    CopySelection,
    /// Send these bytes straight to the PTY.
    Bytes(Vec<u8>),
}

/// `Event::Copy`: `Mod+C` copies the selection; a bare Ctrl+C (Linux, no Shift) is SIGINT.
pub fn copy_event_action(mod_chord: bool) -> ClipboardAction {
    if mod_chord { ClipboardAction::CopySelection } else { ClipboardAction::Bytes(vec![0x03]) }
}

/// `Event::Cut`: the spec defines no terminal "cut" (§7.2 lists only `Mod+C`/`Mod+V`), so
/// `Mod+X` copies the selection like `Mod+C`; a bare Ctrl+X sends `^X` (0x18).
pub fn cut_event_action(mod_chord: bool) -> ClipboardAction {
    if mod_chord { ClipboardAction::CopySelection } else { ClipboardAction::Bytes(vec![0x18]) }
}

/// `Event::Paste(text)`: `Mod+V` pastes `text` (bracketed if the terminal enabled that mode); a
/// bare Ctrl+V sends `^V` (0x16) and the carried `text` is discarded.
pub fn paste_event_action(mod_chord: bool, text: &str, bracketed: bool) -> ClipboardAction {
    if mod_chord {
        ClipboardAction::Bytes(paste_bytes(text, bracketed))
    } else {
        ClipboardAction::Bytes(vec![0x16])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(ctrl: bool, shift: bool, alt: bool) -> Modifiers {
        Modifiers { alt, ctrl, shift, mac_cmd: false, command: ctrl }
    }
    const NONE: Modifiers = Modifiers::NONE;

    /// (name, key, mods, app_cursor, expected)
    #[allow(clippy::type_complexity)]
    fn cases() -> Vec<(&'static str, Key, Modifiers, bool, Option<&'static [u8]>)> {
        vec![
            // -- simple keys, plain and Alt-prefixed --------------------------------------
            ("enter", Key::Enter, NONE, false, Some(b"\r")),
            ("alt+enter", Key::Enter, m(false, false, true), false, Some(b"\x1b\r")),
            ("backspace", Key::Backspace, NONE, false, Some(b"\x7f")),
            ("alt+backspace", Key::Backspace, m(false, false, true), false, Some(b"\x1b\x7f")),
            ("tab", Key::Tab, NONE, false, Some(b"\t")),
            ("shift+tab", Key::Tab, m(false, true, false), false, Some(b"\x1b[Z")),
            ("alt+tab", Key::Tab, m(false, false, true), false, Some(b"\x1b\t")),
            ("escape", Key::Escape, NONE, false, Some(b"\x1b")),
            ("alt+escape", Key::Escape, m(false, false, true), false, Some(b"\x1b\x1b")),
            ("plain space is text, not key", Key::Space, NONE, false, None),
            ("ctrl+space is nul", Key::Space, m(true, false, false), false, Some(&[0])),
            ("ctrl+alt+space", Key::Space, m(true, false, true), false, Some(&[0x1b, 0])),
            ("alt+space", Key::Space, m(false, false, true), false, Some(&[0x1b, b' '])),
            // -- Ctrl+letter C0 codes ------------------------------------------------------
            ("ctrl+a", Key::A, m(true, false, false), false, Some(&[1])),
            ("ctrl+c", Key::C, m(true, false, false), false, Some(&[3])),
            ("ctrl+d", Key::D, m(true, false, false), false, Some(&[4])),
            ("ctrl+h", Key::H, m(true, false, false), false, Some(&[8])),
            ("ctrl+i_is_tab", Key::I, m(true, false, false), false, Some(&[9])),
            ("ctrl+m_is_cr", Key::M, m(true, false, false), false, Some(&[13])),
            ("ctrl+z", Key::Z, m(true, false, false), false, Some(&[26])),
            ("ctrl+alt+c", Key::C, m(true, false, true), false, Some(&[0x1b, 3])),
            ("ctrl+openbracket_is_esc", Key::OpenBracket, m(true, false, false), false, Some(&[0x1b])),
            ("ctrl+backslash", Key::Backslash, m(true, false, false), false, Some(&[0x1c])),
            ("ctrl+closebracket", Key::CloseBracket, m(true, false, false), false, Some(&[0x1d])),
            // -- plain / Alt-only letters send nothing (Event::Text handles them) ----------
            ("plain letter is text", Key::A, NONE, false, None),
            ("shift+letter is text", Key::A, m(false, true, false), false, None),
            ("alt+letter is text (see module docs)", Key::A, m(false, false, true), false, None),
            ("alt+openbracket still ctrl-gated", Key::OpenBracket, m(false, false, true), false, None),
            // -- arrows: plain, app-cursor, and modifier params -----------------------------
            ("up", Key::ArrowUp, NONE, false, Some(b"\x1b[A")),
            ("up app-cursor", Key::ArrowUp, NONE, true, Some(b"\x1bOA")),
            ("down", Key::ArrowDown, NONE, false, Some(b"\x1b[B")),
            ("down app-cursor", Key::ArrowDown, NONE, true, Some(b"\x1bOB")),
            ("right", Key::ArrowRight, NONE, false, Some(b"\x1b[C")),
            ("left", Key::ArrowLeft, NONE, false, Some(b"\x1b[D")),
            ("shift+right", Key::ArrowRight, m(false, true, false), false, Some(b"\x1b[1;2C")),
            ("alt+right", Key::ArrowRight, m(false, false, true), false, Some(b"\x1b[1;3C")),
            ("shift+alt+right", Key::ArrowRight, m(false, true, true), false, Some(b"\x1b[1;4C")),
            ("ctrl+right", Key::ArrowRight, m(true, false, false), false, Some(b"\x1b[1;5C")),
            ("ctrl+shift+right", Key::ArrowRight, m(true, true, false), false, Some(b"\x1b[1;6C")),
            ("ctrl+alt+right", Key::ArrowRight, m(true, false, true), false, Some(b"\x1b[1;7C")),
            ("ctrl+shift+alt+right", Key::ArrowRight, m(true, true, true), false, Some(b"\x1b[1;8C")),
            // modifiers override app-cursor mode back to CSI.
            ("ctrl+right app-cursor", Key::ArrowRight, m(true, false, false), true, Some(b"\x1b[1;5C")),
            // -- Home/End --------------------------------------------------------------------
            ("home", Key::Home, NONE, false, Some(b"\x1b[H")),
            ("home app-cursor", Key::Home, NONE, true, Some(b"\x1bOH")),
            ("end", Key::End, NONE, false, Some(b"\x1b[F")),
            ("shift+home", Key::Home, m(false, true, false), false, Some(b"\x1b[1;2H")),
            // -- PageUp/PageDown/Insert/Delete ------------------------------------------------
            ("pageup", Key::PageUp, NONE, false, Some(b"\x1b[5~")),
            ("pagedown", Key::PageDown, NONE, false, Some(b"\x1b[6~")),
            ("shift+pageup", Key::PageUp, m(false, true, false), false, Some(b"\x1b[5;2~")),
            ("insert", Key::Insert, NONE, false, Some(b"\x1b[2~")),
            ("delete", Key::Delete, NONE, false, Some(b"\x1b[3~")),
            ("ctrl+delete", Key::Delete, m(true, false, false), false, Some(b"\x1b[3;5~")),
            // -- F1-F12 ------------------------------------------------------------------------
            ("f1", Key::F1, NONE, false, Some(b"\x1bOP")),
            ("f2", Key::F2, NONE, false, Some(b"\x1bOQ")),
            ("f3", Key::F3, NONE, false, Some(b"\x1bOR")),
            ("f4", Key::F4, NONE, false, Some(b"\x1bOS")),
            ("shift+f1", Key::F1, m(false, true, false), false, Some(b"\x1b[1;2P")),
            ("f5", Key::F5, NONE, false, Some(b"\x1b[15~")),
            ("f6", Key::F6, NONE, false, Some(b"\x1b[17~")),
            ("f7", Key::F7, NONE, false, Some(b"\x1b[18~")),
            ("f8", Key::F8, NONE, false, Some(b"\x1b[19~")),
            ("f9", Key::F9, NONE, false, Some(b"\x1b[20~")),
            ("f10", Key::F10, NONE, false, Some(b"\x1b[21~")),
            ("f11", Key::F11, NONE, false, Some(b"\x1b[23~")),
            ("f12", Key::F12, NONE, false, Some(b"\x1b[24~")),
            ("ctrl+f5", Key::F5, m(true, false, false), false, Some(b"\x1b[15;5~")),
            // -- unmapped ------------------------------------------------------------------
            ("unmapped key", Key::F20, NONE, false, None),
        ]
    }

    #[test]
    fn key_bytes_table() {
        for (name, key, mods, app_cursor, expected) in cases() {
            let got = key_bytes(key, mods, app_cursor);
            assert_eq!(got.as_deref(), expected, "case: {name}");
        }
    }

    #[test]
    fn at_least_forty_cases() {
        assert!(cases().len() >= 40, "only {} cases", cases().len());
    }

    #[test]
    fn text_bytes_is_utf8_passthrough() {
        assert_eq!(text_bytes("hé llo"), "hé llo".as_bytes().to_vec());
    }

    #[test]
    fn paste_bytes_plain_is_passthrough() {
        assert_eq!(paste_bytes("hello", false), b"hello".to_vec());
    }

    #[test]
    fn paste_bytes_bracketed_wraps_markers() {
        let mut expected = b"\x1b[200~".to_vec();
        expected.extend_from_slice(b"hello\nworld");
        expected.extend_from_slice(b"\x1b[201~");
        assert_eq!(paste_bytes("hello\nworld", true), expected);
    }

    #[test]
    fn copy_action_mod_chord_copies_selection() {
        assert_eq!(copy_event_action(true), ClipboardAction::CopySelection);
    }

    #[test]
    fn copy_action_bare_ctrl_is_sigint() {
        assert_eq!(copy_event_action(false), ClipboardAction::Bytes(vec![0x03]));
    }

    #[test]
    fn cut_action_mod_chord_copies_selection() {
        assert_eq!(cut_event_action(true), ClipboardAction::CopySelection);
    }

    #[test]
    fn cut_action_bare_ctrl_sends_control_x() {
        assert_eq!(cut_event_action(false), ClipboardAction::Bytes(vec![0x18]));
    }

    #[test]
    fn paste_action_mod_chord_sends_text() {
        assert_eq!(paste_event_action(true, "hi", false), ClipboardAction::Bytes(b"hi".to_vec()));
    }

    #[test]
    fn paste_action_mod_chord_bracketed() {
        let mut expected = b"\x1b[200~".to_vec();
        expected.extend_from_slice(b"hi");
        expected.extend_from_slice(b"\x1b[201~");
        assert_eq!(paste_event_action(true, "hi", true), ClipboardAction::Bytes(expected));
    }

    #[test]
    fn paste_action_bare_ctrl_ignores_text_sends_control_v() {
        assert_eq!(paste_event_action(false, "discarded", true), ClipboardAction::Bytes(vec![0x16]));
    }
}
