//! Shared dialog pieces for git operations:
//!
//! - [`confirm_dialog`] — the one modal in the app (§2 "Never block", §5.20, W13): title names
//!   the target, body lists what is lost, a `danger` primary button that states the verb.
//!   `Esc`/**Cancel** cancel; `Enter` does **not** confirm, `Mod+Enter` does.
//! - [`Popover`] + [`Form`] — non-modal W19-style popovers (create branch, create tag, stash):
//!   text fields with live validation, checkboxes, dropdowns, **Cancel** / primary. While focus
//!   is inside the popover, `Esc` cancels and `Enter` submits when every field is valid.
//! - [`progress_pill`] — phase, percent, and **Cancel** for a running git/ssh job (§2, §5.10),
//!   for the git pane header or the status bar.
//!
//! Everything is immediate-mode and draws from state the caller owns; nothing here waits on
//! I/O. Colours come from theme tokens only (§4).

use super::shortcuts;
use super::theme::Colors;
use crate::model::confirm::{self, Confirm, Decision, Press};
use crate::model::keymap::{Chord, Preset};
use crate::model::theme::Token;

// ---- §5.20 destructive confirmation ------------------------------------------------------------

/// Consume this frame's `Enter` / `Mod+Enter` / `Esc` presses so neither a focused button nor the
/// app's shortcuts act on them while the modal is up. Returns the last one seen.
fn take_press(ctx: &egui::Context, preset: Preset) -> Option<Press> {
    let mod_enter = Chord::parse("Mod+Enter").ok();
    ctx.input_mut(|i| {
        let mut press = None;
        i.events.retain(|e| {
            let egui::Event::Key { key, pressed: true, modifiers, .. } = e else { return true };
            match key {
                egui::Key::Escape => press = Some(Press::Escape),
                egui::Key::Enter if shortcuts::chord(*key, *modifiers, preset) == mod_enter => {
                    press = Some(Press::ModEnter)
                }
                egui::Key::Enter => press = Some(Press::Enter),
                _ => return true,
            }
            false
        });
        press
    })
}

/// Draw the §5.20 confirmation for `confirm` (its checkboxes are edited in place). Returns the
/// decision once the user makes one; the caller then drops the `Confirm`, persisting
/// "don't ask again" when [`Confirm::remember`] says so.
pub fn confirm_dialog(
    ctx: &egui::Context,
    colors: &Colors,
    preset: Preset,
    confirm: &mut Confirm,
) -> Option<Decision> {
    let mut decision = take_press(ctx, preset).and_then(confirm::decide);
    let mod_enter = Chord::parse("Mod+Enter").map(|c| preset.label(&c)).unwrap_or_default();

    let frame = egui::Frame::default()
        .fill(colors.get(Token::BgRaised))
        .stroke(egui::Stroke::new(1.0, colors.get(Token::Border)))
        .corner_radius(8)
        .inner_margin(16);
    let modal = egui::Modal::new(egui::Id::new(("amalgum_confirm", confirm.kind.key()))).frame(frame).show(
        ctx,
        |ui| {
            ui.set_max_width(440.0);
            ui.label(egui::RichText::new(&confirm.title).strong().size(15.0));
            if !confirm.detail.is_empty() {
                ui.add_space(8.0);
                ui.add(egui::Label::new(&confirm.detail).wrap());
            }
            let lost = confirm.lost_lines();
            if !lost.is_empty() {
                ui.add_space(4.0);
                for line in lost {
                    let text = egui::RichText::new(format!("  {line}"))
                        .font(egui::FontId::monospace(12.0))
                        .color(colors.get(Token::FgSecondary));
                    ui.add(egui::Label::new(text).truncate());
                }
            }
            if !confirm.options.is_empty() || confirm.kind.allows_dont_ask() {
                ui.add_space(8.0);
            }
            for option in &mut confirm.options {
                ui.checkbox(&mut option.checked, &option.label);
            }
            if confirm.kind.allows_dont_ask() {
                ui.checkbox(&mut confirm.dont_ask, "Don't ask again for this action");
            }
            ui.add_space(12.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().button_padding = egui::vec2(10.0, 4.0);
                let on_accent = colors.get(Token::FgOnAccent);
                let primary = egui::Button::new(egui::RichText::new(&confirm.verb).color(on_accent).strong())
                    .shortcut_text(egui::RichText::new(&mod_enter).color(on_accent))
                    .fill(colors.get(Token::Danger));
                let primary = ui.add(primary);
                // Keyboard focus never rests on the destructive button: Space/Enter on a focused
                // button would otherwise confirm without `Mod+Enter`.
                if primary.has_focus() {
                    primary.surrender_focus();
                }
                if primary.clicked() {
                    decision = Some(Decision::Confirm);
                }
                if ui.add(egui::Button::new("Cancel").shortcut_text("Esc")).clicked() {
                    decision = Some(Decision::Cancel);
                }
            });
        },
    );
    if modal.backdrop_response.clicked() {
        decision = decision.or(Some(Decision::Cancel));
    }
    decision
}

// ---- W16 unknown host key ------------------------------------------------------------------------

/// What the user did in the W16 host-key dialog this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyChoice {
    CopyFingerprint,
    Cancel,
    Trust,
}

/// The W16 dialog for an unknown host key: the only way a key is ever accepted (§5.28). A
/// §5.20 confirmation too: `Esc` cancels, `Enter` does nothing, `Mod+Enter` trusts.
pub fn host_key_dialog(
    ctx: &egui::Context,
    colors: &Colors,
    preset: Preset,
    prompt: &crate::ssh::HostKeyPrompt,
    ssh_said: &str,
) -> Option<HostKeyChoice> {
    let mut choice = take_press(ctx, preset).and_then(confirm::decide).map(|d| match d {
        Decision::Confirm => HostKeyChoice::Trust,
        Decision::Cancel => HostKeyChoice::Cancel,
    });
    let mod_enter = Chord::parse("Mod+Enter").map(|c| preset.label(&c)).unwrap_or_default();
    let frame = egui::Frame::default()
        .fill(colors.get(Token::BgRaised))
        .stroke(egui::Stroke::new(1.0, colors.get(Token::Border)))
        .corner_radius(8)
        .inner_margin(16);
    let modal =
        egui::Modal::new(egui::Id::new(("amalgum_host_key", &prompt.host))).frame(frame).show(ctx, |ui| {
            ui.set_max_width(480.0);
            ui.label(
                egui::RichText::new(format!("Unknown host key for {}", prompt.host)).strong().size(15.0),
            );
            ui.add_space(8.0);
            let key = format!("{} {}", prompt.key_type, prompt.fingerprint);
            ui.label(egui::RichText::new(key).font(egui::FontId::monospace(13.0)));
            ui.label("Verify this fingerprint with the host owner.");
            if !ssh_said.trim().is_empty() {
                ui.add_space(6.0);
                ui.colored_label(colors.get(Token::FgSecondary), "ssh said:");
                egui::ScrollArea::vertical().max_height(120.0).show(ui, |ui| {
                    let text = egui::RichText::new(ssh_said.trim_end())
                        .font(egui::FontId::monospace(11.0))
                        .color(colors.get(Token::FgSecondary));
                    ui.add(egui::Label::new(text).wrap());
                });
            }
            ui.add_space(12.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().button_padding = egui::vec2(10.0, 4.0);
                let on_accent = colors.get(Token::FgOnAccent);
                let trust = egui::Button::new(egui::RichText::new("Trust").color(on_accent).strong())
                    .shortcut_text(egui::RichText::new(&mod_enter).color(on_accent))
                    .fill(colors.get(Token::Danger));
                let trust = ui.add(trust);
                if trust.has_focus() {
                    trust.surrender_focus();
                }
                if trust.clicked() {
                    choice = Some(HostKeyChoice::Trust);
                }
                if ui.add(egui::Button::new("Cancel").shortcut_text("Esc")).clicked() {
                    choice = Some(HostKeyChoice::Cancel);
                }
                if ui.button("Copy fingerprint").clicked() {
                    choice = Some(HostKeyChoice::CopyFingerprint);
                }
            });
        });
    if modal.backdrop_response.clicked() {
        choice = choice.or(Some(HostKeyChoice::Cancel));
    }
    choice
}

// ---- W19 popover forms ---------------------------------------------------------------------------

/// What a popover did this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormOutcome {
    Open,
    Submitted,
    Cancelled,
}

/// Cancel wins over submit; a submit with an invalid field keeps the popover open (the field's
/// message already says why).
fn form_outcome(valid: bool, submit: bool, cancel: bool) -> FormOutcome {
    if cancel {
        FormOutcome::Cancelled
    } else if submit && valid {
        FormOutcome::Submitted
    } else {
        FormOutcome::Open
    }
}

/// The body of a popover: add fields in order. Every field edits caller-owned state.
pub struct Form<'u> {
    ui: &'u mut egui::Ui,
    colors: Colors,
    valid: bool,
    /// A multi-line field has focus, so `Enter` types a newline instead of submitting.
    typing_newlines: bool,
    /// Some field has (or, a single-line one pressed `Enter`, just gave up) keyboard focus.
    focused: bool,
    focus_first: bool,
}

impl Form<'_> {
    /// A single-line field. `error` is the live validation message (`None` = valid), shown in
    /// `danger` under the field; any error disables the primary button.
    pub fn text(&mut self, value: &mut String, hint: &str, error: Option<&str>) -> &mut Self {
        let edit = egui::TextEdit::singleline(value).hint_text(hint).desired_width(f32::INFINITY);
        let resp = self.ui.add(edit);
        if std::mem::take(&mut self.focus_first) {
            resp.request_focus();
        }
        self.track(&resp);
        self.validation(error)
    }

    /// A multi-line field (tag annotation, stash message); `Enter` types a newline here.
    pub fn text_area(&mut self, value: &mut String, hint: &str, error: Option<&str>) -> &mut Self {
        let edit =
            egui::TextEdit::multiline(value).hint_text(hint).desired_rows(3).desired_width(f32::INFINITY);
        let resp = self.ui.add(edit);
        self.typing_newlines |= resp.has_focus();
        self.track(&resp);
        self.validation(error)
    }

    fn track(&mut self, resp: &egui::Response) {
        // A single-line `TextEdit` surrenders focus on the `Enter` that should submit.
        self.focused |= resp.has_focus() || resp.lost_focus();
    }

    fn validation(&mut self, error: Option<&str>) -> &mut Self {
        if let Some(message) = error {
            self.valid = false;
            self.ui.colored_label(self.colors.get(Token::Danger), message);
        }
        self
    }

    pub fn checkbox(&mut self, value: &mut bool, label: &str) -> &mut Self {
        let resp = self.ui.checkbox(value, label);
        self.track(&resp);
        self
    }

    /// A labelled dropdown over `options` (value, label).
    pub fn dropdown<T: PartialEq + Copy>(
        &mut self,
        label: &str,
        value: &mut T,
        options: &[(T, &str)],
    ) -> &mut Self {
        let current = options.iter().find(|(v, _)| v == value).map_or("", |(_, l)| *l);
        let resp = self.ui.horizontal(|ui| {
            ui.label(label);
            egui::ComboBox::from_id_salt(("amalgum_form_dropdown", label))
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for (v, l) in options {
                        ui.selectable_value(value, *v, *l);
                    }
                })
                .response
        });
        self.track(&resp.inner);
        self
    }

    /// Secondary text: a hint or a consequence ("Also pushes the tag to origin").
    pub fn note(&mut self, text: &str) -> &mut Self {
        self.ui.colored_label(self.colors.get(Token::FgSecondary), text);
        self
    }
}

/// A small non-modal form anchored at a point (W19). Declare one per frame while open:
///
/// ```ignore
/// match Popover::new("new-branch", "New branch at 5e6f").primary("Create").show(ctx, colors, at, |f| {
///     f.text(&mut s.name, "branch name", ref_format_error(&s.name).as_deref())
///         .checkbox(&mut s.checkout, "Checkout after create")
///         .checkbox(&mut s.worktree, "Open in new worktree workspace");
/// }) { FormOutcome::Submitted => …, FormOutcome::Cancelled => …, FormOutcome::Open => {} }
/// ```
pub struct Popover {
    id: egui::Id,
    title: String,
    primary: String,
}

impl Popover {
    pub fn new(id: impl std::hash::Hash + std::fmt::Debug, title: impl Into<String>) -> Self {
        Self { id: egui::Id::new(("amalgum_popover", id)), title: title.into(), primary: "OK".into() }
    }

    /// The primary button's verb ("Create", "Stash").
    pub fn primary(mut self, verb: impl Into<String>) -> Self {
        self.primary = verb.into();
        self
    }

    /// Draw at `at` (top-left, kept on screen). On the frame it opens, the first single-line
    /// field takes keyboard focus so the popover is typeable at once. `Enter`/`Esc` act only
    /// while focus is inside it (and are consumed), so typing in a terminal never submits or
    /// cancels it. A caller whose widget re-requests focus every frame (the terminal) must
    /// yield it while the popover is open, as it does for the palette.
    pub fn show(
        self,
        ctx: &egui::Context,
        colors: &Colors,
        at: egui::Pos2,
        body: impl FnOnce(&mut Form),
    ) -> FormOutcome {
        let (mut submit, mut cancel) = (false, false);
        let mut valid = true;
        // Shown last pass too → still open; otherwise this is the frame it opens.
        let pass = ctx.cumulative_pass_nr();
        let last = ctx.data_mut(|d| {
            let last = d.get_temp::<u64>(self.id);
            d.insert_temp(self.id, pass);
            last
        });
        let focus_first = !last.is_some_and(|l| l + 1 >= pass);
        egui::Area::new(self.id).order(egui::Order::Foreground).fixed_pos(at).constrain(true).show(
            ctx,
            |ui| {
                egui::Frame::default()
                    .fill(colors.get(Token::BgRaised))
                    .stroke(egui::Stroke::new(1.0, colors.get(Token::Border)))
                    .corner_radius(8)
                    .inner_margin(12)
                    .show(ui, |ui| {
                        ui.set_width(300.0);
                        ui.label(egui::RichText::new(&self.title).strong());
                        ui.add_space(4.0);
                        let mut form = Form {
                            ui,
                            colors: *colors,
                            valid: true,
                            typing_newlines: false,
                            focused: false,
                            focus_first,
                        };
                        body(&mut form);
                        let (typing_newlines, mut focused) = (form.typing_newlines, form.focused);
                        valid = form.valid;
                        ui.add_space(8.0);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let primary = egui::Button::new(
                                egui::RichText::new(format!("{} ↵", self.primary))
                                    .color(colors.get(Token::FgOnAccent)),
                            )
                            .fill(colors.get(Token::Accent));
                            let primary = ui.add_enabled(valid, primary);
                            let secondary = ui.button("Cancel");
                            submit |= primary.clicked();
                            cancel |= secondary.clicked();
                            focused |= primary.has_focus() || secondary.has_focus();
                        });
                        let enter = focused && !typing_newlines;
                        let esc = focused || ctx.memory(|m| m.focused().is_none());
                        let (enter, esc) = ui.input_mut(|i| {
                            (
                                enter && i.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
                                esc && i.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
                            )
                        });
                        submit |= enter;
                        cancel |= esc;
                    });
            },
        );
        form_outcome(valid, submit, cancel)
    }
}

// ---- progress pill -------------------------------------------------------------------------------

/// A running job's progress, e.g. from `git fetch --progress` stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    /// "Fetching origin", "Receiving objects".
    pub phase: String,
    /// `None` until git reports a percentage.
    pub percent: Option<u8>,
}

/// "Receiving objects 42%", or "Fetching origin…" before a percentage is known.
fn pill_text(p: &Progress) -> String {
    match p.percent {
        Some(pct) => format!("{} {}%", p.phase, pct.min(100)),
        None => format!("{}…", p.phase),
    }
}

/// The progress pill (§2 "Never block"). No animation, so an idle-looking long phase costs no
/// frames. Returns `true` when **Cancel** was clicked.
pub fn progress_pill(ui: &mut egui::Ui, colors: &Colors, progress: &Progress, cancellable: bool) -> bool {
    let mut cancel = false;
    egui::Frame::default()
        .fill(colors.get(Token::BgSunken))
        .stroke(egui::Stroke::new(1.0, colors.get(Token::Border)))
        .corner_radius(10)
        .inner_margin(egui::Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(pill_text(progress));
                if let Some(pct) = progress.percent {
                    let bar = egui::ProgressBar::new(f32::from(pct.min(100)) / 100.0)
                        .desired_width(48.0)
                        .desired_height(6.0)
                        .fill(colors.get(Token::Accent));
                    ui.add(bar);
                }
                if cancellable {
                    cancel = ui.small_button("Cancel").clicked();
                }
            });
        });
    cancel
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_submits_only_when_valid_and_cancel_wins() {
        assert_eq!(form_outcome(true, true, false), FormOutcome::Submitted);
        assert_eq!(form_outcome(false, true, false), FormOutcome::Open, "invalid field blocks submit");
        assert_eq!(form_outcome(true, true, true), FormOutcome::Cancelled);
        assert_eq!(form_outcome(false, false, true), FormOutcome::Cancelled);
        assert_eq!(form_outcome(true, false, false), FormOutcome::Open);
    }

    #[test]
    fn pill_text_shows_phase_and_percent() {
        let p = Progress { phase: "Receiving objects".into(), percent: Some(42) };
        assert_eq!(pill_text(&p), "Receiving objects 42%");
        let p = Progress { phase: "Fetching origin".into(), percent: None };
        assert_eq!(pill_text(&p), "Fetching origin…");
        let p = Progress { phase: "Writing".into(), percent: Some(250) };
        assert_eq!(pill_text(&p), "Writing 100%");
    }

    fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
    }

    /// Run one frame per entry of `frames` on one context; report each frame's decision.
    fn frames(frames: Vec<Vec<egui::Event>>, preset: Preset) -> Vec<Option<Decision>> {
        let ctx = egui::Context::default();
        let colors = Colors::new(crate::model::theme::Mode::Light);
        let mut confirm = Confirm::new(confirm::ConfirmKind::DiscardHunk, "Discard hunk?");
        frames
            .into_iter()
            .map(|events| {
                let mut out = None;
                let mut output = ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| {
                    out = confirm_dialog(ui.ctx(), &colors, preset, &mut confirm);
                });
                output.textures_delta.clear(); // no renderer to apply the font atlas upload
                out
            })
            .collect()
    }

    fn press(events: Vec<egui::Event>, preset: Preset) -> Option<Decision> {
        frames(vec![events], preset).remove(0)
    }

    #[test]
    fn plain_enter_does_not_confirm_whatever_has_focus() {
        // Tab focus onto each control in turn (the danger button among them), then press what a
        // focused button treats as a click: neither may confirm.
        let none = egui::Modifiers::NONE;
        for tabs in 0..6 {
            for activate in [egui::Key::Enter, egui::Key::Space] {
                let mut input = vec![vec![]];
                input.extend((0..tabs).map(|_| vec![key(egui::Key::Tab, none)]));
                input.push(vec![key(activate, none)]);
                input.push(vec![]); // a click registers on release / next pass
                let decided = frames(input, Preset::MacOs);
                assert!(
                    !decided.contains(&Some(Decision::Confirm)),
                    "{tabs} tab(s) then {activate:?} confirmed: {decided:?}"
                );
            }
        }
    }

    #[test]
    fn mod_enter_confirms_per_preset() {
        let cmd = egui::Modifiers { mac_cmd: true, command: true, ..Default::default() };
        assert_eq!(press(vec![key(egui::Key::Enter, cmd)], Preset::MacOs), Some(Decision::Confirm));
        let ctrl_shift = egui::Modifiers { ctrl: true, shift: true, command: true, ..Default::default() };
        assert_eq!(
            press(vec![key(egui::Key::Enter, ctrl_shift)], Preset::LinuxCtrlShift),
            Some(Decision::Confirm)
        );
        // Bare Ctrl+Enter is not `Mod+Enter` under Linux Ctrl+Shift.
        let ctrl = egui::Modifiers { ctrl: true, command: true, ..Default::default() };
        assert_eq!(press(vec![key(egui::Key::Enter, ctrl)], Preset::LinuxCtrlShift), None);
    }

    #[test]
    fn escape_cancels() {
        assert_eq!(
            press(vec![key(egui::Key::Escape, egui::Modifiers::NONE)], Preset::MacOs),
            Some(Decision::Cancel)
        );
    }
}
