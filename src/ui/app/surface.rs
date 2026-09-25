//! The workspace surface (§5.27, W4): a tab strip, then the active tab's split tree of
//! terminals. The focused pane gets a 1 px `border.focus` outline; a pane whose agent needs
//! input gets a 2 px `accent` ring until it is focused (§2 "Agents first").

use super::{App, Focus, workspace::pane_key};
use crate::agent::status::{Status, Tracker, rollup};
use crate::model::layout::{PaneId, Rect};
use crate::model::theme::Token;

impl App {
    pub(super) fn surface(&mut self, ui: &mut egui::Ui) {
        let Some(id) = self.state.active.clone() else { return };
        let Some(ws) = self.state.workspace(&id).cloned() else { return };
        let (mut select, mut new_tab) = (None, false);

        egui::Frame::new()
            .fill(self.colors.get(Token::BgRaised))
            .inner_margin(egui::Margin::symmetric(6, 3))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (i, tab) in ws.tabs.iter().enumerate() {
                        let status = self.live.get(&id).map_or(Status::None, |l| {
                            rollup(
                                tab.panes
                                    .iter()
                                    .filter_map(|p| l.trackers.get(&pane_key(&tab.id, p.id)))
                                    .map(Tracker::status),
                            )
                        });
                        let title = tab.name.clone().or_else(|| {
                            let focused = tab.panes.iter().find(|p| p.id == tab.focused);
                            let live = self.live.get(&id).and_then(|l| l.panes.get(&tab.focused));
                            live.and_then(|t| t.title().map(str::to_string))
                                .or_else(|| focused.and_then(|p| p.title.clone()))
                        });
                        let label = format!("{} {}", status.glyph(), title.as_deref().unwrap_or("shell"));
                        if ui.selectable_label(i == ws.active_tab, label.trim()).clicked() {
                            select = Some(i);
                        }
                    }
                    if ui.small_button("+").on_hover_text("New terminal").clicked() {
                        new_tab = true;
                    }
                });
            });
        if let Some(i) = select {
            if let Some(w) = self.state.workspace_mut(&id) {
                w.active_tab = i;
            }
            self.focus = Focus::Terminal;
            self.dirty = true;
        }
        if new_tab {
            let ctx = ui.ctx().clone();
            self.new_tab(&ctx);
            return;
        }

        let Some(tab) = ws.tabs.get(ws.active_tab) else {
            ui.centered_and_justified(|ui| {
                ui.weak("No terminals. New terminal with the + button or the palette.")
            });
            return;
        };
        let area = ui.available_rect_before_wrap();
        ui.allocate_rect(area, egui::Sense::hover());
        let font_size = self.settings.terminal.font_size;
        let mut focus_to: Option<PaneId> = None;
        for (pane_id, r) in tab.layout.layout(to_model(area)) {
            let rect = to_egui(r).shrink(1.0);
            let key = pane_key(&tab.id, pane_id);
            let focused = self.focus == Focus::Terminal && tab.focused == pane_id;
            let (colors, keymap) = (self.colors, &self.keymap);
            let Some(live) = self.live.get_mut(&id) else { continue };
            let needs_input = live.trackers.get(&key).is_some_and(|t| t.status() == Status::NeedsInput);
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
            match live.panes.get_mut(&pane_id) {
                Some(term) => {
                    if term.show(&mut child, focused, keymap, &colors, font_size).focus_requested {
                        focus_to = Some(pane_id);
                    }
                }
                None => {
                    child.weak("This terminal could not be started. Close it with the palette (Close pane).");
                }
            }
            let stroke = if needs_input && !focused {
                egui::Stroke::new(2.0, colors.get(Token::Accent))
            } else if focused {
                egui::Stroke::new(1.0, colors.get(Token::BorderFocus))
            } else {
                egui::Stroke::new(1.0, colors.get(Token::Border))
            };
            ui.painter().rect_stroke(rect, 0.0, stroke, egui::StrokeKind::Inside);
        }
        if let Some(pane) = focus_to {
            self.focus_pane(&id, pane);
        }
    }

    /// Focus a pane of the active tab; focusing clears its unread notifications (§5.29).
    pub(super) fn focus_pane(&mut self, workspace: &str, pane: PaneId) {
        let Some(ws) = self.state.workspace_mut(workspace) else { return };
        let Some(tab) = ws.tabs.get_mut(ws.active_tab) else { return };
        tab.focused = pane;
        let key = pane_key(&tab.id, pane);
        self.notifications.mark_tab_read(&key);
        self.focus = Focus::Terminal;
        self.dirty = true;
    }
}

fn to_model(r: egui::Rect) -> Rect {
    Rect { x: r.min.x, y: r.min.y, w: r.width(), h: r.height() }
}

fn to_egui(r: Rect) -> egui::Rect {
    egui::Rect::from_min_size(egui::pos2(r.x, r.y), egui::vec2(r.w, r.h))
}
