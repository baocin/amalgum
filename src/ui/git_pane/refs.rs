//! The Refs view (§5.9–5.12, §5.21): Local branches, Remote branches grouped by remote, Tags,
//! Stashes, and Worktrees, each with its W20 empty state.

use super::{GitEvent, GitPane};
use crate::git::refs::{Ref, RefKind};
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

enum RefsAction {
    Checkout(String),
}

impl GitPane {
    pub(super) fn show_refs(
        &mut self,
        ui: &mut egui::Ui,
        colors: &Colors,
        now: u64,
        _events: &mut Vec<GitEvent>,
    ) {
        let locals: Vec<Ref> = self.refs_list.iter().filter(|r| r.kind == RefKind::Local).cloned().collect();
        let tags: Vec<Ref> = self.refs_list.iter().filter(|r| r.kind == RefKind::Tag).cloned().collect();
        let primary_remote = self
            .status
            .branch
            .upstream
            .as_deref()
            .and_then(|u| u.split_once('/'))
            .map(|(a, _)| a.to_string());
        let remote_groups: Vec<(String, Vec<Ref>)> =
            group_remote_branches(&self.refs_list, primary_remote.as_deref())
                .into_iter()
                .map(|(name, refs)| (name, refs.into_iter().cloned().collect()))
                .collect();
        let stashes = self.stashes.clone();
        let worktrees = self.worktrees.clone();

        let mut action: Option<RefsAction> = None;

        ui.label(egui::RichText::new("Local").strong());
        if locals.is_empty() {
            ui.weak("No branches.");
        }
        for r in &locals {
            let mut label = r.short.clone();
            if r.is_head {
                label = format!("✓ {label}");
            }
            let text = egui::RichText::new(label);
            let text = if r.is_head { text.strong() } else { text };
            let resp = ui.horizontal(|ui| {
                ui.label(text);
                if let Some(ab) = ahead_behind_text(r.ahead, r.behind) {
                    ui.weak(ab);
                }
            });
            let hit = ui.interact(
                resp.response.rect,
                ui.id().with(("local_branch", &r.name)),
                egui::Sense::click(),
            );
            if hit.double_clicked() && !r.is_head {
                action = Some(RefsAction::Checkout(r.short.clone()));
            }
        }

        ui.separator();
        ui.label(egui::RichText::new("Remote").strong());
        if remote_groups.is_empty() {
            ui.horizontal(|ui| {
                ui.weak("No remotes.");
                let _ = ui.button("Add remote");
            });
        }
        for (name, refs) in &remote_groups {
            ui.label(format!("▾ {name}"));
            for r in refs {
                ui.horizontal(|ui| {
                    ui.label(&r.short);
                    if let Some(ab) = ahead_behind_text(r.ahead, r.behind) {
                        ui.weak(ab);
                    }
                    if r.gone {
                        ui.colored_label(colors.get(Token::Danger), "gone");
                    }
                });
            }
        }

        ui.separator();
        ui.label(egui::RichText::new("Tags").strong());
        if tags.is_empty() {
            ui.horizontal(|ui| {
                ui.weak("No tags.");
                let _ = ui.button("Tag this commit");
            });
        }
        for t in &tags {
            ui.label(&t.short);
        }

        ui.separator();
        ui.label(egui::RichText::new("Stashes").strong());
        if stashes.is_empty() {
            ui.horizontal(|ui| {
                ui.weak("No stashes.");
                let _ = ui.button("Stash changes");
            });
        }
        for s in &stashes {
            ui.horizontal(|ui| {
                ui.label(format!("stash@{{{}}} {}", s.index, s.message));
                if let Some(branch) = &s.branch {
                    ui.weak(branch);
                }
                ui.weak(crate::util::relative_time(now, s.time));
            });
        }

        ui.separator();
        ui.label(egui::RichText::new("Worktrees").strong());
        if worktrees.len() <= 1 {
            ui.horizontal(|ui| {
                ui.weak("One worktree (this one).");
                let _ = ui.button("Add worktree");
            });
        } else {
            for w in &worktrees {
                ui.horizontal(|ui| {
                    ui.label(&w.path);
                    if let Some(b) = &w.branch {
                        ui.weak(b);
                    } else if w.detached {
                        ui.weak("detached");
                    }
                    if w.locked {
                        ui.weak("locked");
                    }
                });
            }
        }

        if let Some(RefsAction::Checkout(name)) = action {
            let ctx = self.ctx.clone();
            self.start_checkout(&ctx, name);
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
            target: "abc".to_string(),
            peeled: None,
            upstream: None,
            ahead: 0,
            behind: 0,
            gone: false,
            is_head: false,
            time: 0,
            subject: String::new(),
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
}
