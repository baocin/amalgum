//! egui key events → `model::keymap` chords. The keymap owns meaning; this only translates.

use crate::model::keymap::{Chord, Key, Physical, Preset};

/// The chord for a key press, or `None` for keys the action table never binds.
pub fn chord(key: egui::Key, m: egui::Modifiers, preset: Preset) -> Option<Chord> {
    use egui::Key as K;
    let key_ = match key {
        K::Enter => Key::Enter,
        K::Escape => Key::Escape,
        K::Tab => Key::Tab,
        K::Space => Key::Space,
        K::Backspace => Key::Backspace,
        K::ArrowUp => Key::Up,
        K::ArrowDown => Key::Down,
        K::ArrowLeft => Key::Left,
        K::ArrowRight => Key::Right,
        K::Home => Key::Home,
        K::End => Key::End,
        K::PageUp => Key::PageUp,
        K::PageDown => Key::PageDown,
        K::F2 => Key::F2,
        // Letters, digits, and punctuation are single characters.
        other => {
            let mut chars = other.symbol_or_name().chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Key::Char(c.to_ascii_lowercase()),
                _ => return None,
            }
        }
    };
    // `?` and `+` are typed with Shift; the spec writes them without it.
    let shift =
        m.shift && !matches!(key, K::Questionmark | K::Plus | K::Colon | K::Exclamationmark | K::Pipe);
    let physical = Physical { cmd: m.mac_cmd, ctrl: m.ctrl, alt: m.alt, shift, sup: false };
    let mods = preset.interpret(physical)?;
    Some(Chord { mods, key: key_ })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(ctrl: bool, shift: bool, alt: bool, cmd: bool) -> egui::Modifiers {
        egui::Modifiers { alt, ctrl, shift, mac_cmd: cmd, command: cmd || ctrl }
    }

    #[test]
    fn mod_is_cmd_on_macos_and_ctrl_shift_on_linux() {
        let mac = chord(egui::Key::K, m(false, false, false, true), Preset::MacOs);
        let linux = chord(egui::Key::K, m(true, true, false, false), Preset::LinuxCtrlShift);
        assert_eq!(mac, Chord::parse("Mod+K").ok());
        assert_eq!(linux, Chord::parse("Mod+K").ok());
    }

    #[test]
    fn linux_mod_shift_is_ctrl_alt() {
        let c = chord(egui::Key::D, m(true, false, true, false), Preset::LinuxCtrlShift);
        assert_eq!(c, Chord::parse("Mod+Shift+D").ok());
    }

    #[test]
    fn plain_keys_and_shifted_symbols() {
        assert_eq!(chord(egui::Key::J, m(false, false, false, false), Preset::MacOs), Chord::parse("j").ok());
        assert_eq!(
            chord(egui::Key::Questionmark, m(false, true, false, false), Preset::MacOs),
            Chord::parse("?").ok()
        );
        assert_eq!(
            chord(egui::Key::ArrowLeft, m(false, false, true, true), Preset::MacOs),
            Chord::parse("Mod+Alt+←").ok()
        );
    }

    #[test]
    fn unbindable_keys_are_ignored() {
        assert_eq!(chord(egui::Key::F20, m(false, false, false, false), Preset::MacOs), None);
    }
}
