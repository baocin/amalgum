//! The Refs view (§5.9–5.12, §5.21): Local branches, Remote branches grouped by remote, Tags,
//! Stashes, and Worktrees, each with its W20 empty state.
//!
//! Rows are selectable (a click also selects the ref's commit for the details area), carry a
//! context menu, and answer §7.6 keys while the view has focus: `Enter` checkout, `F2` rename,
//! `Backspace` delete / remove (confirming when unsafe). The operations themselves are
//! `ops_ui`'s; network items become `SyncRequest`s.

use super::requests::SyncRequest;
use super::{GitEvent, GitPane, Selection};
use crate::git::refs::{Ref, RefKind, Worktree};
use crate::git::worktree_ops::WorktreeOp;
use crate::git::{Location, remote as gitremote};
use crate::model::git_menu::{self, CheckoutTarget};
use crate::model::theme::Token;
use crate::ui::theme::Colors;
use std::collections::BTreeMap;

/// `↑N ↓M` vs upstream, omitting a side that is zero and the whole thing when both are.
pub(super) fn ahead_behind_text(ahead: u32, behind: u32) -> Option<String> {
    match (ahead, behind) {
        (0, 0) => None,
        (a, 0) => Some(format!("↑{a}")),
        (0, b) => Some(format!("↓{b}")),
        (a, b) => Some(format!("↑{a} ↓{b}")),
    }
}

/// Groups `Remote`-kind refs by the remote name (the segment before the first `/` of `short`,
/// e.g. `origin/main` → `origin`), the bound remote's group first if `primary` names one.
pub(super) fn group_remote_branches<'a>(
    refs: &'a [Ref],
    primary: Option<&str>,
) -> Vec<(String, Vec<&'a Ref>)> {
    let mut groups: BTreeMap<String, Vec<&Ref>> = BTreeMap::new();
    for r in refs.iter().filter(|r| r.kind == RefKind::Remote) {
        let remote = r.short.split_once('/').map(|(a, _)| a.to_string()).unwrap_or_else(|| r.short.clone());
        groups.entry(remote).or_default().push(r);
    }
    let mut out: Vec<(String, Vec<&Ref>)> = groups.into_iter().collect();
    if let Some(p) = primary {
        out.sort_by_key(|(name, _)| (name != p, name.clone()));
    }
    out
}

/// Adds an empty group for every configured remote that has no remote-tracking branches yet
/// (just added, never fetched), so it still gets a row with its menu (§5.10).
pub(super) fn with_empty_remotes<'a>(
    mut groups: Vec<(String, Vec<&'a Ref>)>,
    remotes: &[String],
) -> Vec<(String, Vec<&'a Ref>)> {
    for r in remotes {
        if !groups.iter().any(|(name, _)| name == r) {
            groups.push((r.clone(), Vec::new()));
        }
    }
    groups
}

/// §5.11 "Tags section, newest first".
pub(super) fn tags_newest_first(refs: &[Ref]) -> Vec<Ref> {
    let mut tags: Vec<Ref> = refs.iter().filter(|r| r.kind == RefKind::Tag).cloned().collect();
    tags.sort_by(|a, b| b.time.cmp(&a.time).then_with(|| a.short.cmp(&b.short)));
    tags
}

/// The main worktree (listed first) and the one this pane shows can't be removed from here.
pub(super) fn worktree_removable(index: usize, w: &Worktree, current: Option<&str>) -> bool {
    index > 0 && !w.bare && current != Some(w.path.as_str())
}

/// §5.21 "path (abbreviated)": the repo itself by its folder name, a sibling as `../name`,
/// anything else as `…/parent/name`. The full path is the row's hover text.
pub(super) fn worktree_label(path: &str, repo: Option<&str>) -> String {
    let p = std::path::Path::new(path.trim_end_matches('/'));
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string());
    let repo = repo.map(|r| std::path::Path::new(r.trim_end_matches('/')));
    if repo == Some(p) {
        return name;
    }
    if repo.and_then(|r| r.parent()).is_some_and(|rp| p.parent() == Some(rp)) {
        return format!("../{name}");
    }
    match p.parent().and_then(|pp| pp.file_name()) {
        Some(parent) => format!("…/{}/{name}", parent.to_string_lossy()),
        None => path.to_string(),
    }
}

/// The selected Refs row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RefsSel {
    Local(String),
    RemoteBranch(String),
    Remote(String),
    Tag(String),
    Worktree(String),
}

enum RefsAction {
    Select(RefsSel, Option<String>),
    Checkout(CheckoutTarget),
    CreateBranch {
        base: String,
        at: egui::Pos2,
    },
    CreateTag {
        at: egui::Pos2,
    },
    Rename(String, egui::Pos2),
    DeleteBranch(String),
    SetUpstream(String, egui::Pos2),
    UnsetUpstream(String),
    Merge(String),
    Rebase(String),
    Sync(SyncRequest),
    Copy(String),
    Send(String),
    DeleteTag(String),
    AddRemote(egui::Pos2),
    RenameRemote(String, egui::Pos2),
    RemoteUrl(String, egui::Pos2),
    RemoveRemote(String),
    OpenUrl(String),
    Bind(String),
    RemoveWorktree(String),
    Worktree(WorktreeOp),
    Reveal(String),
    /// §5.12: show the stash in the details.
    SelectStash(crate::git::refs::Stash),
    Stash,
}

impl GitPane {
    pub(super) fn show_refs(
        &mut self,
        ui: &mut egui::Ui,
        colors: &Colors,
        now: u64,
        events: &mut Vec<GitEvent>,
    ) {
        let locals: Vec<Ref> = self.refs_list.iter().filter(|r| r.kind == RefKind::Local).cloned().collect();
        let tags = tags_newest_first(&self.refs_list);
        let primary_remote = self.default_remote();
        let remote_names = self.remote_names();
        let remote_groups: Vec<(String, Vec<Ref>)> = with_empty_remotes(
            group_remote_branches(&self.refs_list, primary_remote.as_deref()),
            &remote_names,
        )
        .into_iter()
        .map(|(name, refs)| (name, refs.into_iter().cloned().collect()))
        .collect();
        let stashes = self.stashes.clone();
        let worktrees = self.worktrees.clone();
        let current_path = match &self.location {
            Location::Local { path } => Some(path.display().to_string()),
            Location::Remote { path, .. } => Some(path.clone()),
        };
        let is_local = matches!(self.location, Location::Local { .. });
        let current = self.current_branch();
        let sel = self.ops.refs_sel.clone();

        // Keyboard focus for §7.6 keys: a click anywhere in the view (or on a row) takes it.
        let focus_id = ui.id().with("refs_focus_bg");
        let bg = ui.interact(ui.available_rect_before_wrap(), focus_id, egui::Sense::click());
        if bg.clicked() {
            bg.request_focus();
        }

        let mut action: Option<RefsAction> = None;
        let selected_stash = self.selected_stash().map(|s| s.oid.clone());
        let mut set = |a: RefsAction| action = Some(a);

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            // ---- Local ----
            let header = ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Local").strong());
                ui.small_button("+").on_hover_text("Create branch…").clicked()
            });
            if header.inner {
                let at = header.response.rect.left_bottom();
                set(RefsAction::CreateBranch { base: "HEAD".into(), at });
            }
            if locals.is_empty() {
                ui.weak("No branches.");
            }
            for r in &locals {
                let selected = sel == Some(RefsSel::Local(r.short.clone()));
                let label = if r.is_head { format!("✓ {}", r.short) } else { r.short.clone() };
                let text = egui::RichText::new(label);
                let text = if r.is_head { text.strong() } else { text };
                let row = ui.horizontal(|ui| {
                    let resp = ui.selectable_label(selected, text);
                    if let Some(ab) = ahead_behind_text(r.ahead, r.behind) {
                        ui.weak(ab);
                    }
                    if r.gone {
                        ui.colored_label(colors.get(Token::Danger), "gone");
                    }
                    resp
                });
                let resp = row.inner;
                let at = resp.rect.left_bottom();
                if resp.clicked() || resp.secondary_clicked() {
                    set(RefsAction::Select(RefsSel::Local(r.short.clone()), Some(r.target.clone())));
                }
                if resp.double_clicked() && !r.is_head {
                    set(RefsAction::Checkout(CheckoutTarget::Branch(r.short.clone())));
                }
                resp.context_menu(|ui| {
                    let b = r.short.clone();
                    let is_current = current.as_deref() == Some(b.as_str());
                    if !is_current && ui.button(format!("Checkout `{b}`")).clicked() {
                        set(RefsAction::Checkout(CheckoutTarget::Branch(b.clone())));
                    }
                    if ui.button("Create branch from here…").clicked() {
                        set(RefsAction::CreateBranch { base: b.clone(), at });
                    }
                    ui.separator();
                    if let Some(remote) = &primary_remote {
                        if ui.button(format!("Push `{b}` to {remote}")).clicked() {
                            set(RefsAction::Sync(SyncRequest::PushBranch {
                                branch: b.clone(),
                                remote: None,
                            }));
                        }
                        if is_current && ui.button("Pull").clicked() {
                            set(RefsAction::Sync(SyncRequest::Pull));
                        }
                        if ui.button("Set upstream…").clicked() {
                            set(RefsAction::SetUpstream(b.clone(), at));
                        }
                    }
                    if r.upstream.is_some() && ui.button("Unset upstream").clicked() {
                        set(RefsAction::UnsetUpstream(b.clone()));
                    }
                    if !is_current {
                        ui.separator();
                        let cur = current.as_deref().unwrap_or("HEAD");
                        if ui.button(format!("Merge `{b}` into `{cur}`")).clicked() {
                            set(RefsAction::Merge(b.clone()));
                        }
                        if ui.button(format!("Rebase `{cur}` onto `{b}`")).clicked() {
                            set(RefsAction::Rebase(b.clone()));
                        }
                    }
                    ui.separator();
                    if ui.button("Rename…").on_hover_text("F2").clicked() {
                        set(RefsAction::Rename(b.clone(), at));
                    }
                    if !is_current && ui.button("Delete").on_hover_text("Backspace").clicked() {
                        set(RefsAction::DeleteBranch(b.clone()));
                    }
                    ui.separator();
                    if ui.button("Copy branch name").clicked() {
                        set(RefsAction::Copy(b.clone()));
                    }
                    if ui.button("Send branch name to terminal").clicked() {
                        set(RefsAction::Send(b.clone()));
                    }
                });
            }

            // ---- Remote ----
            ui.separator();
            let header = ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Remote").strong());
                ui.small_button("+").on_hover_text("Add remote…").clicked()
            });
            if header.inner {
                set(RefsAction::AddRemote(header.response.rect.left_bottom()));
            }
            if remote_groups.is_empty() {
                let empty = ui.horizontal(|ui| {
                    ui.weak("No remotes.");
                    ui.button("Add remote").clicked()
                });
                if empty.inner {
                    set(RefsAction::AddRemote(empty.response.rect.left_bottom()));
                }
            }
            for (name, refs) in &remote_groups {
                let selected = sel == Some(RefsSel::Remote(name.clone()));
                let resp = ui.selectable_label(selected, format!("▾ {name}"));
                let at = resp.rect.left_bottom();
                if resp.clicked() || resp.secondary_clicked() {
                    set(RefsAction::Select(RefsSel::Remote(name.clone()), None));
                }
                let url = self.remote_url(Some(name));
                resp.context_menu(|ui| {
                    if ui.button("Fetch").clicked() {
                        set(RefsAction::Sync(SyncRequest::Fetch { remote: Some(name.clone()) }));
                    }
                    if ui.button("Prune").clicked() {
                        set(RefsAction::Sync(SyncRequest::Prune { remote: name.clone() }));
                    }
                    if ui.button("Push all tags").clicked() {
                        set(RefsAction::Sync(SyncRequest::PushAllTags { remote: name.clone() }));
                    }
                    ui.separator();
                    if ui.button("Edit URL…").clicked() {
                        set(RefsAction::RemoteUrl(name.clone(), at));
                    }
                    if ui.button("Rename…").on_hover_text("F2").clicked() {
                        set(RefsAction::RenameRemote(name.clone(), at));
                    }
                    if ui.button("Remove…").on_hover_text("Backspace").clicked() {
                        set(RefsAction::RemoveRemote(name.clone()));
                    }
                    ui.separator();
                    if let Some(web) = url.as_deref().and_then(gitremote::web_url)
                        && ui.button("Open in browser").clicked()
                    {
                        set(RefsAction::OpenUrl(web));
                    }
                    if ui.button("Bind active workspace to this remote").clicked() {
                        set(RefsAction::Bind(name.clone()));
                    }
                });
                for r in refs {
                    let selected = sel == Some(RefsSel::RemoteBranch(r.short.clone()));
                    let row = ui.horizontal(|ui| {
                        ui.add_space(12.0);
                        let resp = ui.selectable_label(selected, &r.short);
                        if let Some(ab) = ahead_behind_text(r.ahead, r.behind) {
                            ui.weak(ab);
                        }
                        if r.gone {
                            ui.colored_label(colors.get(Token::Danger), "gone");
                        }
                        resp
                    });
                    let resp = row.inner;
                    let at = resp.rect.left_bottom();
                    let target = git_menu::split_remote_branch(&r.short, &remote_names)
                        .map(|(remote, branch)| CheckoutTarget::Remote { remote, branch });
                    if resp.clicked() || resp.secondary_clicked() {
                        set(RefsAction::Select(
                            RefsSel::RemoteBranch(r.short.clone()),
                            Some(r.target.clone()),
                        ));
                    }
                    if resp.double_clicked()
                        && let Some(t) = &target
                    {
                        set(RefsAction::Checkout(t.clone()));
                    }
                    resp.context_menu(|ui| {
                        if let Some(t) = &target
                            && ui.button(git_menu::checkout_label(t)).clicked()
                        {
                            set(RefsAction::Checkout(t.clone()));
                        }
                        if ui.button("Create branch from here…").clicked() {
                            set(RefsAction::CreateBranch { base: r.short.clone(), at });
                        }
                        ui.separator();
                        let cur = current.as_deref().unwrap_or("HEAD");
                        if ui.button(format!("Merge `{}` into `{cur}`", r.short)).clicked() {
                            set(RefsAction::Merge(r.short.clone()));
                        }
                        if ui.button(format!("Rebase `{cur}` onto `{}`", r.short)).clicked() {
                            set(RefsAction::Rebase(r.short.clone()));
                        }
                        ui.separator();
                        if ui.button("Copy branch name").clicked() {
                            set(RefsAction::Copy(r.short.clone()));
                        }
                        if ui.button("Send branch name to terminal").clicked() {
                            set(RefsAction::Send(r.short.clone()));
                        }
                    });
                }
            }

            // ---- Tags ----
            ui.separator();
            ui.label(egui::RichText::new("Tags").strong());
            if tags.is_empty() {
                let empty = ui.horizontal(|ui| {
                    ui.weak("No tags.");
                    ui.button("Tag this commit").clicked()
                });
                if empty.inner {
                    set(RefsAction::CreateTag { at: empty.response.rect.left_bottom() });
                }
            }
            for t in &tags {
                let selected = sel == Some(RefsSel::Tag(t.short.clone()));
                let row = ui.horizontal(|ui| {
                    let resp = ui.selectable_label(selected, &t.short);
                    if !t.subject.is_empty() {
                        ui.add(egui::Label::new(egui::RichText::new(&t.subject).weak()).truncate());
                    }
                    resp
                });
                let resp = row.inner;
                let at = resp.rect.left_bottom();
                let commit = t.peeled.clone().unwrap_or_else(|| t.target.clone());
                if resp.clicked() || resp.secondary_clicked() {
                    set(RefsAction::Select(RefsSel::Tag(t.short.clone()), Some(commit.clone())));
                }
                resp.context_menu(|ui| {
                    if ui.button("Checkout tag (detached)").clicked() {
                        set(RefsAction::Checkout(CheckoutTarget::Detached(t.short.clone())));
                    }
                    if ui.button("Create branch from tag…").clicked() {
                        set(RefsAction::CreateBranch { base: t.short.clone(), at });
                    }
                    if !remote_names.is_empty() {
                        ui.menu_button("Push to…", |ui| {
                            for r in &remote_names {
                                if ui.button(r).clicked() {
                                    set(RefsAction::Sync(SyncRequest::PushTag {
                                        tag: t.short.clone(),
                                        remote: Some(r.clone()),
                                    }));
                                }
                            }
                        });
                    }
                    ui.separator();
                    if ui.button("Delete tag…").on_hover_text("Backspace").clicked() {
                        set(RefsAction::DeleteTag(t.short.clone()));
                    }
                    if ui.button("Copy tag name").clicked() {
                        set(RefsAction::Copy(t.short.clone()));
                    }
                });
            }

            // ---- Stashes ----
            ui.separator();
            ui.label(egui::RichText::new("Stashes").strong());
            if stashes.is_empty() {
                ui.horizontal(|ui| {
                    ui.weak("No stashes.");
                    if ui.button("Stash changes").clicked() {
                        set(RefsAction::Stash);
                    }
                });
            }
            for s in &stashes {
                let selected = selected_stash.as_deref() == Some(s.oid.as_str());
                let resp = ui.horizontal(|ui| {
                    ui.add(
                        egui::Label::new(format!("stash@{{{}}} {}", s.index, s.message)).selectable(false),
                    );
                    if let Some(branch) = &s.branch {
                        ui.weak(branch);
                    }
                    ui.weak(crate::util::relative_time(now, s.time));
                });
                if selected {
                    ui.painter().rect_stroke(
                        resp.response.rect.expand(1.0),
                        2.0,
                        egui::Stroke::new(1.0, colors.get(Token::Accent)),
                        egui::StrokeKind::Outside,
                    );
                }
                if ui
                    .interact(resp.response.rect, ui.id().with(("stash", &s.oid)), egui::Sense::click())
                    .clicked()
                {
                    set(RefsAction::SelectStash(s.clone()));
                }
            }

            // ---- Worktrees ----
            ui.separator();
            let header = ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Worktrees").strong());
                ui.small_button("Prune").on_hover_text("git worktree prune").clicked()
            });
            if header.inner {
                set(RefsAction::Worktree(WorktreeOp::Prune));
            }
            if worktrees.len() <= 1 {
                ui.horizontal(|ui| {
                    ui.weak("One worktree (this one).");
                    let _ = ui.button("Add worktree");
                });
            } else {
                for (i, w) in worktrees.iter().enumerate() {
                    let selected = sel == Some(RefsSel::Worktree(w.path.clone()));
                    let is_current = current_path.as_deref() == Some(w.path.as_str());
                    let row = ui.horizontal(|ui| {
                        let short = worktree_label(&w.path, current_path.as_deref());
                        let label = if is_current { format!("✓ {short}") } else { short };
                        let resp = ui.selectable_label(selected, label).on_hover_text(&w.path);
                        if let Some(b) = &w.branch {
                            ui.weak(b);
                        } else if w.detached {
                            ui.weak("detached");
                        }
                        if w.locked {
                            ui.weak("locked");
                        }
                        if w.prunable {
                            ui.colored_label(colors.get(Token::Warning), "missing");
                        }
                        resp
                    });
                    let resp = row.inner;
                    if resp.clicked() || resp.secondary_clicked() {
                        set(RefsAction::Select(RefsSel::Worktree(w.path.clone()), w.head.clone()));
                    }
                    let removable = worktree_removable(i, w, current_path.as_deref());
                    resp.context_menu(|ui| {
                        if removable && ui.button("Remove…").on_hover_text("Backspace").clicked() {
                            set(RefsAction::RemoveWorktree(w.path.clone()));
                        }
                        if i > 0 {
                            let (label, op) = if w.locked {
                                ("Unlock", WorktreeOp::Unlock { path: w.path.clone() })
                            } else {
                                ("Lock", WorktreeOp::Lock { path: w.path.clone() })
                            };
                            if ui.button(label).clicked() {
                                set(RefsAction::Worktree(op));
                            }
                        }
                        if is_local && ui.button("Reveal in file manager").clicked() {
                            set(RefsAction::Reveal(w.path.clone()));
                        }
                        if ui.button("Copy path").clicked() {
                            set(RefsAction::Copy(w.path.clone()));
                        }
                    });
                }
            }
        });

        if bg.has_focus() || matches!(action, Some(RefsAction::Select(..))) {
            if action.is_none() {
                action = self.refs_key_action(ui);
            }
            if matches!(action, Some(RefsAction::Select(..))) {
                bg.request_focus();
            }
        }
        if let Some(a) = action {
            self.apply_refs_action(ui.ctx(), a, events);
        }
    }

    /// §7.6 `Enter` / `F2` / `Backspace` on the selected row.
    fn refs_key_action(&mut self, ui: &egui::Ui) -> Option<RefsAction> {
        if self.ops_prompt_open() {
            return None;
        }
        let sel = self.ops.refs_sel.clone()?;
        let none = egui::Modifiers::NONE;
        let (enter, f2, backspace) = ui.input_mut(|i| {
            (
                i.consume_key(none, egui::Key::Enter),
                i.consume_key(none, egui::Key::F2),
                i.consume_key(none, egui::Key::Backspace),
            )
        });
        let at = self.ops_anchor();
        let current = self.current_branch();
        match sel {
            RefsSel::Local(b) if enter && current.as_ref() != Some(&b) => {
                Some(RefsAction::Checkout(CheckoutTarget::Branch(b)))
            }
            RefsSel::Local(b) if f2 => Some(RefsAction::Rename(b, at)),
            RefsSel::Local(b) if backspace && current.as_ref() != Some(&b) => {
                Some(RefsAction::DeleteBranch(b))
            }
            RefsSel::RemoteBranch(r) if enter => git_menu::split_remote_branch(&r, &self.remote_names())
                .map(|(remote, branch)| RefsAction::Checkout(CheckoutTarget::Remote { remote, branch })),
            RefsSel::Remote(r) if f2 => Some(RefsAction::RenameRemote(r, at)),
            RefsSel::Remote(r) if backspace => Some(RefsAction::RemoveRemote(r)),
            RefsSel::Tag(t) if enter => Some(RefsAction::Checkout(CheckoutTarget::Detached(t))),
            RefsSel::Tag(t) if backspace => Some(RefsAction::DeleteTag(t)),
            RefsSel::Worktree(path) if backspace => {
                let current_path = match &self.location {
                    Location::Local { path } => path.display().to_string(),
                    Location::Remote { path, .. } => path.clone(),
                };
                let i = self.worktrees.iter().position(|w| w.path == path)?;
                worktree_removable(i, &self.worktrees[i], Some(&current_path))
                    .then_some(RefsAction::RemoveWorktree(path))
            }
            _ => None,
        }
    }

    fn apply_refs_action(&mut self, ctx: &egui::Context, action: RefsAction, events: &mut Vec<GitEvent>) {
        match action {
            RefsAction::Select(sel, commit) => {
                self.ops.refs_sel = Some(sel);
                if let Some(id) = commit {
                    self.selected = Selection::Commit(id);
                    self.load_details_for_selected(ctx);
                }
            }
            RefsAction::Checkout(t) => self.run_checkout(ctx, t, events),
            RefsAction::CreateBranch { base, at } => self.open_create_branch(Some(base), Some(at)),
            RefsAction::CreateTag { at } => {
                let head = self.head_id();
                self.open_create_tag(ctx, head, Some(at));
            }
            RefsAction::Rename(b, at) => self.open_rename_branch(b, Some(at)),
            RefsAction::DeleteBranch(b) => self.start_delete_branch(ctx, b, events),
            RefsAction::SetUpstream(b, at) => self.open_upstream_picker(b, Some(at)),
            RefsAction::UnsetUpstream(branch) => {
                self.run_op(ctx, crate::git::ops::Op::SetUpstream { branch, upstream: None }, events)
            }
            RefsAction::Merge(rev) => {
                let op = self.merge_op(rev);
                self.run_op(ctx, op, events);
            }
            RefsAction::Rebase(onto) => self.run_op(ctx, crate::git::ops::Op::Rebase { onto }, events),
            RefsAction::Sync(r) => events.push(GitEvent::Sync(r)),
            RefsAction::Copy(text) => ctx.copy_text(text),
            RefsAction::Send(text) => events.push(GitEvent::SendToTerminal(text)),
            RefsAction::DeleteTag(t) => self.start_delete_tag(t),
            RefsAction::AddRemote(at) => self.open_add_remote(Some(at)),
            RefsAction::RenameRemote(r, at) => self.open_rename_remote(r, Some(at)),
            RefsAction::RemoteUrl(r, at) => self.open_remote_url(r, Some(at)),
            RefsAction::RemoveRemote(r) => self.start_remove_remote(r),
            RefsAction::OpenUrl(url) => self.open_url(ctx, url),
            RefsAction::Bind(r) => events.push(GitEvent::BindRemote(r)),
            RefsAction::RemoveWorktree(path) => self.start_remove_worktree(ctx, path),
            RefsAction::Worktree(op) => self.run_worktree_op(ctx, op),
            RefsAction::Reveal(path) => self.reveal(ctx, path),
            RefsAction::SelectStash(stash) => self.select_stash(ctx, &stash),
            RefsAction::Stash => events.extend(self.open_stash().map(GitEvent::Toast)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn branch(name: &str, ahead: u32, behind: u32) -> Ref {
        Ref {
            name: format!("refs/heads/{name}"),
            short: name.to_string(),
            kind: RefKind::Local,
            target: "abc".to_string(),
            peeled: None,
            upstream: None,
            ahead,
            behind,
            gone: false,
            is_head: false,
            time: 0,
            subject: String::new(),
        }
    }

    fn remote_ref(short: &str) -> Ref {
        Ref {
            name: format!("refs/remotes/{short}"),
            short: short.to_string(),
            kind: RefKind::Remote,
            ..branch("x", 0, 0)
        }
    }

    fn tag(short: &str, time: u64) -> Ref {
        Ref {
            name: format!("refs/tags/{short}"),
            short: short.to_string(),
            kind: RefKind::Tag,
            time,
            ..branch("x", 0, 0)
        }
    }

    // ---- ahead_behind_text ------------------------------------------------------------------

    #[test]
    fn ahead_behind_text_variants() {
        assert_eq!(ahead_behind_text(0, 0), None);
        assert_eq!(ahead_behind_text(2, 0).as_deref(), Some("↑2"));
        assert_eq!(ahead_behind_text(0, 3).as_deref(), Some("↓3"));
        assert_eq!(ahead_behind_text(2, 3).as_deref(), Some("↑2 ↓3"));
    }

    // ---- group_remote_branches --------------------------------------------------------------

    #[test]
    fn group_remote_branches_groups_by_remote_name() {
        let refs = vec![
            remote_ref("origin/main"),
            remote_ref("origin/feat"),
            remote_ref("upstream/main"),
            branch("local", 0, 0),
        ];
        let groups = group_remote_branches(&refs, None);
        assert_eq!(groups.len(), 2, "only remote-kind refs, grouped");
        let origin = groups.iter().find(|(n, _)| n == "origin").expect("origin group");
        assert_eq!(origin.1.len(), 2);
        let upstream = groups.iter().find(|(n, _)| n == "upstream").expect("upstream group");
        assert_eq!(upstream.1.len(), 1);
    }

    #[test]
    fn group_remote_branches_bound_remote_sorts_first() {
        let refs = vec![remote_ref("upstream/main"), remote_ref("origin/main")];
        let groups = group_remote_branches(&refs, Some("upstream"));
        assert_eq!(groups[0].0, "upstream");
        assert_eq!(groups[1].0, "origin");
    }

    #[test]
    fn group_remote_branches_empty_when_no_remote_refs() {
        let refs = vec![branch("main", 0, 0)];
        assert!(group_remote_branches(&refs, None).is_empty());
    }

    #[test]
    fn remotes_without_branches_still_get_a_group() {
        let refs = vec![remote_ref("origin/main")];
        let groups =
            with_empty_remotes(group_remote_branches(&refs, None), &["origin".into(), "fork".into()]);
        let names: Vec<&str> = groups.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["origin", "fork"]);
        assert!(groups[1].1.is_empty());
    }

    // ---- tags / worktrees -------------------------------------------------------------------

    #[test]
    fn tags_sort_newest_first() {
        let refs = vec![tag("v1", 10), branch("main", 0, 0), tag("v3", 30), tag("v2", 20)];
        let names: Vec<String> = tags_newest_first(&refs).into_iter().map(|t| t.short).collect();
        assert_eq!(names, ["v3", "v2", "v1"]);
    }

    #[test]
    fn worktree_paths_are_abbreviated() {
        let repo = Some("/w/conduit"); // portability: allow
        assert_eq!(worktree_label("/w/conduit", repo), "conduit"); // portability: allow
        assert_eq!(worktree_label("/w/conduit-fix", repo), "../conduit-fix"); // portability: allow
        assert_eq!(worktree_label("/tmp/wt/fix", repo), "…/wt/fix"); // portability: allow
        assert_eq!(worktree_label("~/g/x", Some("~/g/y")), "../x");
    }

    #[test]
    fn neither_the_main_nor_the_current_worktree_is_removable() {
        let wt = |path: &str| Worktree {
            path: path.into(),
            head: None,
            branch: None,
            bare: false,
            detached: false,
            locked: false,
            prunable: false,
        };
        assert!(!worktree_removable(0, &wt("/r"), None)); // portability: allow
        assert!(worktree_removable(1, &wt("/r-wt"), Some("/r"))); // portability: allow
        assert!(!worktree_removable(1, &wt("/r-wt"), Some("/r-wt"))); // portability: allow
    }
}
