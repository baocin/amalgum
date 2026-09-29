//! The commit context menu (§5.4) and the Graph view's single-key operations (§7.3 `c`, `b`,
//! `t`, `m`, `r`, `p`). The menu's content and order come from `model::git_menu::commit_menu`;
//! this file renders it and turns a chosen [`MenuAction`] into an operation, a popover, a
//! confirmation, or a [`SyncRequest`] for the sync feature.

use super::requests::SyncRequest;
use super::{GitEvent, GitPane, Selection};
use crate::git::log::Commit;
use crate::git::remote;
use crate::model::git_menu::{self, MenuAction, MenuContext, MenuEntry, RowRefs};

/// Draws `entries` into an open menu; returns the chosen action. Disabled items (Undo / Redo
/// with an empty stack) show their label but can't be clicked.
pub(super) fn show_entries(ui: &mut egui::Ui, entries: &[MenuEntry]) -> Option<MenuAction> {
    let mut chosen = None;
    for entry in entries {
        match entry {
            MenuEntry::Separator => {
                ui.separator();
            }
            MenuEntry::Item { label, action, enabled } => {
                if ui.add_enabled(*enabled, egui::Button::new(label)).clicked() {
                    chosen = Some(action.clone());
                    ui.close();
                }
            }
            MenuEntry::Submenu { label, entries } => {
                ui.menu_button(label, |ui| {
                    if let Some(a) = show_entries(ui, entries) {
                        chosen = Some(a);
                        ui.close();
                    }
                });
            }
        }
    }
    chosen
}

impl GitPane {
    /// The ordered menu for `commit` in the current repo state.
    pub(super) fn commit_menu_entries(&mut self, commit: &Commit) -> Vec<MenuEntry> {
        let head = self.head_id();
        let current = self.current_branch();
        let remotes = self.remote_names();
        let locals = self.local_branches();
        let default_remote = self.default_remote();
        let (undo, redo) = self.undo_redo_descriptions();
        let web = self.remote_url(None).and_then(|u| remote::commit_url(&u, &commit.id)).is_some();
        let in_head = head.as_deref().and_then(|h| self.in_head(h, &commit.id));
        let cx = MenuContext {
            head: head.as_deref(),
            current_branch: current.as_deref(),
            default_remote: default_remote.as_deref(),
            remotes: &remotes,
            local_branches: &locals,
            undo: undo.as_deref(),
            redo: redo.as_deref(),
            web,
            in_head,
        };
        git_menu::commit_menu(&commit.id, &RowRefs::from_decorations(&commit.refs), &cx)
    }

    /// Runs what the commit menu's `action` asks for. `at` is where a follow-up popover opens.
    pub(super) fn on_menu_action(
        &mut self,
        ctx: &egui::Context,
        commit: &Commit,
        action: MenuAction,
        at: Option<egui::Pos2>,
        events: &mut Vec<GitEvent>,
    ) {
        let id = commit.id.clone();
        match action {
            MenuAction::Checkout(target) => self.run_checkout(ctx, target, events),
            MenuAction::CreateBranch => self.open_create_branch(Some(id), at),
            MenuAction::CreateTag => self.open_create_tag(ctx, Some(id), at),
            MenuAction::Push { branch, remote } => {
                events.push(GitEvent::Sync(SyncRequest::PushBranch { branch, remote }))
            }
            MenuAction::ForcePushWithLease { branch } => {
                events.push(GitEvent::Sync(SyncRequest::ForcePushWithLease { branch, remote: None }))
            }
            MenuAction::PushTag { tag, remote } => {
                events.push(GitEvent::Sync(SyncRequest::PushTag { tag, remote }))
            }
            MenuAction::SetUpstream { branch } => self.open_upstream_picker(branch, at),
            MenuAction::Pull => events.push(GitEvent::Sync(SyncRequest::Pull)),
            MenuAction::Merge { rev } => {
                let op = self.merge_op(rev);
                self.run_op(ctx, op, events);
            }
            MenuAction::Rebase { onto } => self.run_op(ctx, crate::git::ops::Op::Rebase { onto }, events),
            MenuAction::CherryPick => self.start_cherry_pick(ctx, &id, events),
            MenuAction::Revert => self.start_revert(ctx, &id, at, events),
            MenuAction::Reset(mode) => self.start_reset(ctx, id, mode),
            MenuAction::Undo => self.undo(),
            MenuAction::Redo => self.redo(),
            MenuAction::RenameBranch(b) => self.open_rename_branch(b, at),
            MenuAction::DeleteBranch(b) => self.start_delete_branch(ctx, b, events),
            MenuAction::DeleteTag(t) => self.start_delete_tag(t),
            MenuAction::CopyHash => ctx.copy_text(id),
            MenuAction::CopySubject => ctx.copy_text(commit.subject.clone()),
            MenuAction::CopyPermalink => {
                if let Some(url) = self.remote_url(None).and_then(|u| remote::commit_url(&u, &id)) {
                    ctx.copy_text(url);
                }
            }
            MenuAction::OpenInBrowser => {
                if let Some(url) = self.remote_url(None).and_then(|u| remote::commit_url(&u, &id)) {
                    self.open_url(ctx, url);
                }
            }
        }
    }

    /// Attach the commit context menu to `resp` (a row or one of its chips).
    pub(super) fn attach_commit_menu(
        &mut self,
        ctx: &egui::Context,
        resp: &egui::Response,
        commit: &Commit,
        events: &mut Vec<GitEvent>,
    ) {
        let mut chosen = None;
        let mut at = None;
        resp.context_menu(|ui| {
            let entries = self.commit_menu_entries(commit);
            if let Some(a) = show_entries(ui, &entries) {
                chosen = Some(a);
                at = Some(ui.min_rect().left_top());
            }
        });
        if let Some(action) = chosen {
            self.on_menu_action(ctx, commit, action, at, events);
        }
    }

    /// §5.4 "Double-click a branch chip: checkout (5.9)": a local branch, or a remote branch as
    /// a tracking branch of the same name.
    pub(super) fn chip_double_clicked(
        &mut self,
        dec: &crate::git::log::Decoration,
        events: &mut Vec<GitEvent>,
    ) {
        let target = match dec {
            crate::git::log::Decoration::Remote(short) => {
                git_menu::split_remote_branch(short, &self.remote_names())
                    .map(|(remote, branch)| git_menu::CheckoutTarget::Remote { remote, branch })
            }
            _ => super::graph::checkout_target(dec).map(|b| git_menu::CheckoutTarget::Branch(b.to_string())),
        };
        if let Some(target) = target {
            let ctx = self.ctx.clone();
            self.run_checkout(&ctx, target, events);
        }
    }

    fn selected_commit(&self) -> Option<Commit> {
        match &self.selected {
            Selection::Commit(id) => self.commits.iter().find(|c| &c.id == id).cloned(),
            Selection::None => None,
        }
    }

    /// The branch `m` / `r` act on: the selected row's first branch that isn't current, else
    /// its commit.
    fn selected_other_rev(&self, commit: &Commit) -> String {
        let refs = RowRefs::from_decorations(&commit.refs);
        let current = self.current_branch();
        refs.locals
            .iter()
            .find(|b| Some(*b) != current.as_ref())
            .or(refs.remotes.first())
            .cloned()
            .unwrap_or_else(|| commit.id.clone())
    }

    /// §7.3: `c` checkout, `b` / `t` create branch / tag (direct: cheap and undoable), `m` / `r`
    /// / `p` open the anchored popover naming the exact operation (§5.4).
    pub(super) fn graph_op_keys(&mut self, ui: &egui::Ui, has_focus: bool, events: &mut Vec<GitEvent>) {
        use crate::model::keymap::Action as A;
        if !has_focus || self.ops_prompt_open() {
            return;
        }
        let Some(commit) = self.selected_commit() else { return };
        let keys = [
            (egui::Key::C, A::CheckoutSelected),
            (egui::Key::B, A::CreateBranchAtSelectedCommit),
            (egui::Key::T, A::CreateTagAtSelectedCommit),
            (egui::Key::M, A::MergeSelectedIntoCurrent),
            (egui::Key::R, A::RebaseCurrentOntoSelected),
            (egui::Key::P, A::CherryPickSelectedOntoCurrent),
        ];
        let action = ui.input_mut(|i| {
            keys.iter().find(|(k, _)| i.consume_key(egui::Modifiers::NONE, *k)).map(|(_, a)| *a)
        });
        if let Some(action) = action {
            let ctx = ui.ctx().clone();
            self.selected_commit_action(&ctx, &commit, action, events);
        }
    }

    /// One §7.3 graph action on `commit` (its key, or the palette).
    pub(super) fn selected_commit_action(
        &mut self,
        ctx: &egui::Context,
        commit: &Commit,
        action: crate::model::keymap::Action,
        events: &mut Vec<GitEvent>,
    ) {
        use crate::model::keymap::Action as A;
        let current = self.current_branch();
        match action {
            A::CheckoutSelected => {
                let head = self.head_id();
                let remotes = self.remote_names();
                let locals = self.local_branches();
                let cx = MenuContext {
                    head: head.as_deref(),
                    current_branch: current.as_deref(),
                    remotes: &remotes,
                    local_branches: &locals,
                    ..MenuContext::default()
                };
                match git_menu::checkout_choice(&commit.id, &RowRefs::from_decorations(&commit.refs), &cx) {
                    Some(target) => self.run_checkout(ctx, target, events),
                    None => {
                        events.push(GitEvent::Toast(crate::ui::chrome::Toast::info("Already checked out")))
                    }
                }
            }
            A::CreateBranchAtSelectedCommit => self.open_create_branch(Some(commit.id.clone()), None),
            A::CreateTagAtSelectedCommit => self.open_create_tag(ctx, Some(commit.id.clone()), None),
            A::MergeSelectedIntoCurrent => {
                let rev = self.selected_other_rev(commit);
                let sentence = git_menu::merge_sentence(&rev, current.as_deref());
                let op = self.merge_op(rev);
                self.open_run_prompt(sentence, "Merge", op);
            }
            A::RebaseCurrentOntoSelected => {
                let onto = self.selected_other_rev(commit);
                let sentence = git_menu::rebase_sentence(&onto, current.as_deref());
                self.open_run_prompt(sentence, "Rebase", crate::git::ops::Op::Rebase { onto });
            }
            A::CherryPickSelectedOntoCurrent => {
                let sentence = git_menu::cherry_pick_sentence(&commit.id, current.as_deref());
                let op = self.cherry_pick_op(commit.id.clone());
                self.open_run_prompt(sentence, "Cherry-pick", op);
            }
            _ => {}
        }
    }
}
