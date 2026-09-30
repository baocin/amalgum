//! Compare mode (§5.13, W11) and the multi-commit selection (§5.14) of the Graph view.
//!
//! - **Selection**: a plain click selects one commit, `Shift+click` / `Shift+↓/↑` the rows from
//!   the anchor, `Mod+click` toggles one (`model::selection`). With two or more selected the
//!   details show "5 commits selected · 18 files": the combined diff of a contiguous first-parent
//!   range (`git diff <oldest>~1 <newest>`, root-safe), else each commit's files grouped under
//!   it. Right-click offers the §5.14 menu (`model::git_menu::selection_menu`).
//! - **Compare**: the details area shows "Comparing `A`…`B` · N commits · M files" with two
//!   fuzzy ref pickers (any ref, or a hash typed in), a swap button, a three-dot / two-dot
//!   toggle, and **Files** / **Commits** tabs. Clicking a file shows its diff with the details'
//!   diff renderer; clicking a commit shows its details. `Esc` leaves and restores the selection
//!   from before.
//!
//! All git runs through `git::compare` on workers (`jobs::spawn`), replies come back as
//! [`CompareReply`] and are dropped when a newer request superseded them.

use super::details::{self, DIFF_LINE_PAGE};
use super::{GitEvent, GitPane, Selection};
use crate::git::cmd::GitError;
use crate::git::compare::{self as cmp, Comparison, DiffMode, SelectionFiles};
use crate::git::diff::{FileChange, FileDiff};
use crate::git::log::Commit;
use crate::git::ops::Op;
use crate::git::refs::RefKind;
use crate::model::git_menu::{self, MenuAction, SelectionContext};
use crate::model::selection::MultiSelect;
use crate::model::theme::Token;
use crate::ui::chrome::Toast;
use crate::ui::jobs;
use crate::ui::theme::Colors;
use std::collections::BTreeSet;

/// The pane's selection beyond one commit: the multi-selection, the compare view, and what the
/// details show for either.
#[derive(Default)]
pub(super) struct SelectUi {
    pub multi: MultiSelect,
    compare: Option<CompareState>,
    multi_view: Option<MultiView>,
    menu_cache: Option<(MenuKey, Vec<git_menu::MenuEntry>)>,
}

/// What the selection menu's content depends on.
#[derive(PartialEq, Eq)]
struct MenuKey {
    ids: Vec<String>,
    head: Option<String>,
    current: Option<String>,
    loaded: usize,
    locals: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Tab {
    #[default]
    Files,
    Commits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    A,
    B,
}

/// A file diff the details area shows, and the request it came from.
#[derive(Default)]
struct DiffPane {
    /// `(commit, path)`: the commit a per-commit group's file belongs to, `None` otherwise.
    file: Option<(Option<String>, String)>,
    diff: Vec<FileDiff>,
    loading: bool,
    req: u64,
    limit: usize,
}

struct Picker {
    side: Side,
    query: String,
    selected: usize,
    focus: bool,
}

struct CompareState {
    a: String,
    b: String,
    mode: DiffMode,
    tab: Tab,
    /// `None` while loading.
    loaded: Option<Result<Comparison, GitError>>,
    req: u64,
    diff: DiffPane,
    picker: Option<Picker>,
    collapsed: BTreeSet<String>,
    /// A commit opened from the Commits tab: its details show under a "Back" bar.
    viewing: Option<String>,
    /// What `Esc` restores.
    restore: (Selection, MultiSelect),
    /// The graph selection when compare opened: anything that selects something else (search,
    /// a Refs row, a new commit) ends compare mode — the new selection wins.
    entered_on: Selection,
}

struct MultiView {
    /// The selection and loaded row count this view was checked against.
    from: (MultiSelect, usize),
    /// The selected ids, newest first: a different selection reloads.
    key: Vec<String>,
    subjects: Vec<(String, String)>,
    files: Option<Result<SelectionFiles, GitError>>,
    collapsed: BTreeSet<String>,
    req: u64,
    diff: DiffPane,
}

/// Replies of this module's worker jobs, carried in `Reply::Compare`.
pub(super) enum CompareReply {
    Loaded { req: u64, result: Box<Result<Comparison, GitError>> },
    MultiFiles { req: u64, result: Result<SelectionFiles, GitError> },
    Diff { req: u64, result: Result<Vec<FileDiff>, GitError> },
}

/// Files grouped by directory, in path order, the W11 tree: `(dir, indices into files)`; files at
/// the top level come first under `""`.
pub(super) fn group_by_dir(files: &[FileChange]) -> Vec<(String, Vec<usize>)> {
    let mut groups: std::collections::BTreeMap<String, Vec<usize>> = Default::default();
    for (i, f) in files.iter().enumerate() {
        let dir = f.path.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
        groups.entry(dir).or_default().push(i);
    }
    for idx in groups.values_mut() {
        idx.sort_by(|a, b| files[*a].path.cmp(&files[*b].path));
    }
    groups.into_iter().collect()
}

/// The W11 header: "Comparing `main`…`feat` · 12 commits · 34 files".
pub(super) fn compare_title(a: &str, b: &str, commits: Option<usize>, files: Option<usize>) -> String {
    let mut out = format!("Comparing `{a}`…`{b}`");
    if let Some(n) = commits {
        out.push_str(&format!(" · {}", plural(n, "commit")));
    }
    if let Some(n) = files {
        out.push_str(&format!(" · {}", plural(n, "file")));
    }
    out
}

/// The §5.14 details header: "5 commits selected · 18 files".
pub(super) fn selection_title(count: usize, files: Option<usize>) -> String {
    match files {
        Some(n) => format!("{count} commits selected · {}", plural(n, "file")),
        None => format!("{count} commits selected"),
    }
}

/// A rename's source path, for the diff's pathspec (`git::compare::file_diff_args`).
fn old_path_of(files: &[FileChange], path: &str) -> Option<String> {
    files.iter().find(|f| f.path == path).and_then(|f| f.old_path.clone())
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") }
}

/// A short, readable name for a side: a hash becomes its short form.
fn side_label(rev: &str) -> String {
    let hex = rev.len() >= 12 && rev.bytes().all(|b| b.is_ascii_hexdigit());
    if hex { super::worker::short_hash(rev).to_string() } else { rev.to_string() }
}

// ---- graph hooks ----------------------------------------------------------------------------------

impl GitPane {
    fn row_ids(&self) -> Vec<&str> {
        self.commits.iter().map(|c| c.id.as_str()).collect()
    }

    /// Two or more commits are selected, and the graph's single selection still is the
    /// multi-selection's cursor (anything else that selects a commit ends it).
    pub(super) fn multi_active(&self) -> bool {
        self.sel.multi.len() > 1
            && matches!(&self.selected, Selection::Commit(id) if Some(id.as_str()) == self.sel.multi.cursor())
    }

    /// Compare mode or a multi-selection owns the details area.
    pub(super) fn selection_mode_active(&self) -> bool {
        self.sel.compare.is_some() || self.multi_active()
    }

    /// Whether the graph highlights row `id`.
    pub(super) fn row_selected(&self, id: &str) -> bool {
        match &self.selected {
            Selection::Commit(s) if s == id => true,
            _ => self.multi_active() && self.sel.multi.contains(id),
        }
    }

    /// A single commit was selected (click, keys, chip): the multi-selection restarts there and
    /// compare mode ends — the new selection wins over the one `Esc` would restore.
    pub(super) fn on_single_select(&mut self, id: &str) {
        self.sel.multi.select(id);
        self.sel.compare = None;
    }

    /// A click on graph row `i` with `mods` (§5.14): `Shift` ranges, `Mod` toggles.
    pub(super) fn graph_click(&mut self, i: usize, mods: egui::Modifiers) {
        let Some(id) = self.commits.get(i).map(|c| c.id.clone()) else { return };
        if !mods.shift && !mods.command {
            self.select_row(i);
            return;
        }
        let mut multi = std::mem::take(&mut self.sel.multi);
        if multi.is_empty()
            && let Selection::Commit(cur) = &self.selected
        {
            multi.select(cur); // the single selection is where a range starts
        }
        let order = self.row_ids();
        if mods.shift {
            multi.extend_to(&order, &id)
        } else {
            multi.toggle(&order, &id)
        }
        self.sel.multi = multi;
        self.sel.compare = None;
        self.follow_cursor();
    }

    /// `Shift+↓` / `Shift+↑` (§5.14).
    pub(super) fn extend_by_key(&mut self, delta: isize) {
        let mut multi = std::mem::take(&mut self.sel.multi);
        if let Selection::Commit(cur) = &self.selected
            && multi.cursor() != Some(cur.as_str())
        {
            multi.select(cur);
        }
        multi.step(&self.row_ids(), delta);
        self.sel.multi = multi;
        self.sel.compare = None;
        self.follow_cursor();
    }

    /// Point the graph's single selection at the multi-selection's cursor; with one commit left
    /// its ordinary details load.
    fn follow_cursor(&mut self) {
        let ctx = self.ctx.clone();
        match self.sel.multi.cursor().map(str::to_string) {
            Some(id) => {
                self.selected = Selection::Commit(id);
                if self.sel.multi.len() == 1 {
                    self.load_details_for_selected(&ctx);
                }
            }
            None => {
                self.selected = Selection::None;
                self.details = None;
            }
        }
    }

    /// A right-click on row `i` keeps a multi-selection it belongs to, else selects the row.
    pub(super) fn graph_secondary_click(&mut self, i: usize) {
        let in_multi =
            self.commits.get(i).is_some_and(|c| self.multi_active() && self.sel.multi.contains(&c.id));
        if !in_multi {
            self.select_row(i);
        }
    }

    fn select_row(&mut self, i: usize) {
        let Some(id) = self.commits.get(i).map(|c| c.id.clone()) else { return };
        self.on_single_select(&id);
        self.selected = Selection::Commit(id);
        let ctx = self.ctx.clone();
        self.load_details_for_selected(&ctx);
    }

    /// The context menu of a graph row: the §5.14 selection menu on a selected row of a
    /// multi-selection, the commit menu otherwise.
    pub(super) fn attach_row_menu(
        &mut self,
        ctx: &egui::Context,
        resp: &egui::Response,
        commit: &Commit,
        events: &mut Vec<GitEvent>,
    ) {
        if !(self.multi_active() && self.sel.multi.contains(&commit.id)) {
            self.attach_commit_menu(ctx, resp, commit, events);
            return;
        }
        let mut chosen = None;
        resp.context_menu(|ui| {
            let entries = self.selection_menu_entries();
            if let Some(a) = super::menus::show_entries(ui, &entries) {
                chosen = Some(a);
            }
        });
        if let Some(action) = chosen {
            self.on_selection_action(ctx, action, events);
        }
    }

    /// The selected commits, newest first.
    fn selected_commits(&self) -> Vec<Commit> {
        self.sel
            .multi
            .ordered(&self.row_ids())
            .into_iter()
            .filter_map(|(i, _)| self.commits.get(i).cloned())
            .collect()
    }

    /// Contiguous rows forming one first-parent line (the combined diff applies).
    fn selection_is_range(&self, commits: &[Commit]) -> bool {
        let chain: Vec<(&str, &[String])> =
            commits.iter().map(|c| (c.id.as_str(), c.parents.as_slice())).collect();
        self.sel.multi.is_contiguous(&self.row_ids()) && cmp::is_linear_chain(&chain)
    }

    /// The §5.14 menu for the current selection, built once per selection and repo state: the
    /// menu redraws every frame while open, and the ancestry checks walk the loaded graph.
    fn selection_menu_entries(&mut self) -> Vec<git_menu::MenuEntry> {
        let commits = self.selected_commits();
        let current = self.current_branch();
        let head = self.head_id();
        let key = MenuKey {
            ids: commits.iter().map(|c| c.id.clone()).collect(),
            head: head.clone(),
            current: current.clone(),
            loaded: self.commits.len(),
            locals: self.local_branches(),
        };
        if let Some((k, entries)) = &self.sel.menu_cache
            && *k == key
        {
            return entries.clone();
        }
        let in_head: Vec<Option<bool>> = match head.as_deref() {
            Some(h) => commits.iter().map(|c| self.in_head(h, &c.id)).collect(),
            None => vec![None; commits.len()],
        };
        let chain: Vec<(&str, &[String])> =
            commits.iter().map(|c| (c.id.as_str(), c.parents.as_slice())).collect();
        let squashable = current.is_some()
            && self.sel.multi.is_contiguous(&self.row_ids())
            && cmp::can_squash(&chain, head.as_deref());
        let cx = SelectionContext {
            count: commits.len(),
            current_branch: current.as_deref(),
            local_branches: &key.locals,
            squashable,
            has_merge: commits.iter().any(|c| c.parents.len() > 1),
            in_current: in_head.iter().all(|i| *i == Some(true)),
        };
        let entries = git_menu::selection_menu(&cx);
        self.sel.menu_cache = Some((key, entries.clone()));
        entries
    }

    pub(super) fn on_selection_action(
        &mut self,
        ctx: &egui::Context,
        action: MenuAction,
        events: &mut Vec<GitEvent>,
    ) {
        let commits = self.selected_commits();
        let (Some(newest), Some(oldest)) = (commits.first().cloned(), commits.last().cloned()) else {
            return;
        };
        let oldest_first: Vec<String> = commits.iter().rev().map(|c| c.id.clone()).collect();
        match action {
            MenuAction::CherryPickSelection { onto } => {
                // The single-commit op carries the `-x` preference; pick the whole selection.
                let mut pick = self.cherry_pick_op(String::new());
                if let Op::CherryPick { commits, .. } = &mut pick {
                    *commits = oldest_first;
                }
                let mut list = Vec::new();
                if self.current_branch().as_deref() != Some(onto.as_str()) {
                    list.push(Op::Checkout { branch: onto, force: false });
                }
                list.push(pick);
                if let Some(t) = self.start_ops(ctx, list, Vec::new()) {
                    events.push(GitEvent::Toast(t));
                }
            }
            MenuAction::RevertSelection => {
                let newest_first = commits.iter().map(|c| c.id.clone()).collect();
                self.run_op(ctx, Op::Revert { commits: newest_first, mainline: None }, events);
            }
            MenuAction::SquashSelection => {
                let chain: Vec<(&str, &[String])> =
                    commits.iter().map(|c| (c.id.as_str(), c.parents.as_slice())).collect();
                if !cmp::can_squash(&chain, self.head_id().as_deref()) {
                    return; // HEAD moved since the menu opened: the plan would drop commits
                }
                let newest = commits.first().map(|c| c.id.clone()).unwrap_or_default();
                self.squash_range(ctx, &oldest.id.clone(), &newest);
            }
            MenuAction::CopyHashes => {
                ctx.copy_text(commits.iter().map(|c| c.id.as_str()).collect::<Vec<_>>().join("\n"));
                events.push(GitEvent::Toast(Toast::success(format!("Copied {} hashes", commits.len()))));
            }
            MenuAction::CompareSelection => self.open_compare(oldest.id, newest.id, None),
            _ => {}
        }
    }
}

// ---- entering and leaving compare mode ---------------------------------------------------------

impl GitPane {
    /// Enter compare mode for `a`…`b`, remembering the selection for `Esc`; `pick` opens that
    /// side's ref picker at once (**Compare with…**, palette **Compare**).
    fn open_compare(&mut self, a: String, b: String, pick: Option<Side>) {
        let restore = match self.sel.compare.take() {
            Some(old) => old.restore,
            None => (self.selected.clone(), self.sel.multi.clone()),
        };
        self.view = crate::model::state::GitView::Graph;
        self.sel.compare = Some(CompareState {
            a,
            b,
            mode: DiffMode::default(),
            tab: Tab::default(),
            loaded: None,
            req: 0,
            diff: DiffPane { limit: DIFF_LINE_PAGE, ..DiffPane::default() },
            picker: pick.map(|side| Picker { side, query: String::new(), selected: 0, focus: true }),
            collapsed: BTreeSet::new(),
            viewing: None,
            restore,
            entered_on: self.selected.clone(),
        });
        self.dispatch_compare();
    }

    /// **Compare with current** (§5.13): the current branch (or HEAD) against `rev`.
    pub(super) fn compare_with_current(&mut self, rev: String) {
        let current = self.current_branch().unwrap_or_else(|| "HEAD".to_string());
        self.open_compare(current, rev, None);
    }

    /// **Compare with…**: `rev` on the right, the left side's picker open on the current branch.
    pub(super) fn compare_with(&mut self, rev: String) {
        let current = self.current_branch().unwrap_or_else(|| "HEAD".to_string());
        self.open_compare(current, rev, Some(Side::A));
    }

    /// Palette **Compare** (§5.13): the current branch against the selected commit's ref (or
    /// HEAD), with the right side's picker open.
    pub fn open_compare_palette(&mut self) {
        let current = self.current_branch().unwrap_or_else(|| "HEAD".to_string());
        let b = match &self.selected {
            Selection::Commit(id) => {
                let refs =
                    self.commits.iter().find(|c| &c.id == id).map(|c| c.refs.clone()).unwrap_or_default();
                let cx =
                    git_menu::MenuContext { current_branch: Some(current.as_str()), ..Default::default() };
                git_menu::compare_rev(id, &git_menu::RowRefs::from_decorations(&refs), &cx)
            }
            Selection::None => current.clone(),
        };
        self.open_compare(current, b, Some(Side::B));
    }

    /// The details were replaced by something other than compare mode (a commit or stash
    /// selected anywhere): compare mode ends without restoring, the new selection wins.
    pub(super) fn details_replaced(&mut self) {
        self.sel.compare = None;
    }

    /// The graph selection moved since compare opened (a new commit, search, a Refs row):
    /// compare mode ends.
    fn drop_stale_compare(&mut self) {
        if self.sel.compare.as_ref().is_some_and(|c| c.entered_on != self.selected) {
            self.sel.compare = None;
        }
    }

    /// Load details on compare mode's own behalf (a commit opened from its Commits tab, "Back"),
    /// which must not end it the way [`Self::details_replaced`] does.
    fn keeping_compare(&mut self, load: impl FnOnce(&mut Self)) {
        let keep = self.sel.compare.take();
        load(self);
        self.sel.compare = keep;
    }

    /// `Esc`: leave compare mode, restoring the selection from before it.
    fn exit_compare(&mut self) {
        let Some(state) = self.sel.compare.take() else { return };
        let (selected, multi) = state.restore;
        self.selected = selected;
        self.sel.multi = multi;
        let ctx = self.ctx.clone();
        self.load_details_for_selected(&ctx);
    }

    fn dispatch_compare(&mut self) {
        let req = self.next_req_id();
        let Some(state) = &mut self.sel.compare else { return };
        state.req = req;
        state.loaded = None;
        state.diff = DiffPane { limit: DIFF_LINE_PAGE, ..DiffPane::default() };
        state.viewing = None;
        let (git, a, b, mode) = (self.git.clone(), state.a.clone(), state.b.clone(), state.mode);
        jobs::spawn(&self.ctx, &self.tx, move || {
            let result = cmp::load(&git, &a, &b, mode);
            super::worker::Reply::Compare(CompareReply::Loaded { req, result: Box::new(result) })
        });
    }

    fn dispatch_compare_diff(&mut self, path: String) {
        let req = self.next_req_id();
        let Some(state) = &mut self.sel.compare else { return };
        let Some(Ok(c)) = &state.loaded else { return };
        let (a, b, mode) = (c.a_id.clone(), c.b_id.clone(), state.mode);
        let old_path = c.files.as_deref().ok().and_then(|f| old_path_of(f, &path));
        state.diff = DiffPane {
            file: Some((None, path.clone())),
            loading: true,
            req,
            limit: DIFF_LINE_PAGE,
            ..Default::default()
        };
        let git = self.git.clone();
        jobs::spawn(&self.ctx, &self.tx, move || {
            let result = cmp::load_file_diff(&git, &a, &b, mode, &path, old_path.as_deref());
            super::worker::Reply::Compare(CompareReply::Diff { req, result })
        });
    }

    pub(super) fn apply_compare_reply(&mut self, reply: CompareReply, events: &mut Vec<GitEvent>) {
        match reply {
            CompareReply::Loaded { req, result } => {
                if let Some(state) = &mut self.sel.compare
                    && state.req == req
                {
                    state.loaded = Some(*result);
                }
            }
            CompareReply::MultiFiles { req, result } => {
                if let Some(view) = &mut self.sel.multi_view
                    && view.req == req
                {
                    view.files = Some(result);
                }
            }
            CompareReply::Diff { req, result } => {
                let pane = match (&mut self.sel.compare, &mut self.sel.multi_view) {
                    (Some(s), _) if s.diff.req == req => &mut s.diff,
                    (_, Some(v)) if v.diff.req == req => &mut v.diff,
                    _ => return,
                };
                pane.loading = false;
                match result {
                    Ok(diff) => pane.diff = diff,
                    Err(e) => {
                        events.push(GitEvent::Toast(Toast::error("Could not read diff", e.to_string())))
                    }
                }
            }
        }
    }
}

// ---- the details area ----------------------------------------------------------------------------

/// What the user did in the compare view this frame, applied once rendering is done.
enum CompareAction {
    Exit,
    Swap,
    SetMode(DiffMode),
    Tab(Tab),
    OpenPicker(Side),
    ClosePicker,
    Pick(Side, String),
    File(String),
    ToggleDir(String),
    ViewCommit(String),
    BackFromCommit,
    MoreLines,
}

impl GitPane {
    /// The details area's §5.13 / §5.14 modes, drawn instead of the commit details; `false`
    /// when neither applies (or a commit opened from compare shows its ordinary details below
    /// the bar drawn here).
    pub(super) fn show_selection_details(&mut self, ui: &mut egui::Ui, colors: &Colors, now: u64) -> bool {
        self.drop_stale_compare();
        if self.sel.compare.is_some() {
            return self.show_compare(ui, colors, now);
        }
        if self.multi_active() {
            self.show_multi(ui, colors);
            return true;
        }
        self.sel.multi_view = None;
        false
    }

    fn picker_candidates(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for kind in [RefKind::Local, RefKind::Remote, RefKind::Tag] {
            out.extend(self.refs_list.iter().filter(|r| r.kind == kind).map(|r| r.short.clone()));
        }
        out.retain(|r| !r.ends_with("/HEAD"));
        out
    }

    fn show_compare(&mut self, ui: &mut egui::Ui, colors: &Colors, now: u64) -> bool {
        let candidates = self.picker_candidates();
        let esc_free = self.app_focused() && !ui.ctx().egui_wants_keyboard_input();
        let Some(state) = &mut self.sel.compare else { return false };
        let mut action: Option<CompareAction> = None;
        let mut set = |a: CompareAction| action = Some(a);

        if let Some(id) = &state.viewing {
            ui.horizontal(|ui| {
                if ui.button("← Back to comparison").clicked() {
                    set(CompareAction::BackFromCommit);
                }
                ui.weak(format!(
                    "{} · {}",
                    compare_title(&side_label(&state.a), &side_label(&state.b), None, None),
                    super::worker::short_hash(id)
                ));
            });
            ui.separator();
            let back = action.is_some();
            if back || (esc_free && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)))
            {
                state.viewing = None;
                let ctx = self.ctx.clone();
                self.keeping_compare(|pane| pane.load_details_for_selected(&ctx));
                return true;
            }
            return false;
        }

        // -- W11 header: Compare [main ▾] ⇄ [feat ▾] ----------------------------------------------
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Compare").strong());
            let open = state.picker.as_ref().map(|p| p.side);
            if ui.selectable_label(open == Some(Side::A), format!("{} ▾", side_label(&state.a))).clicked() {
                set(if open == Some(Side::A) {
                    CompareAction::ClosePicker
                } else {
                    CompareAction::OpenPicker(Side::A)
                });
            }
            if ui.button("⇄").on_hover_text("Swap sides").clicked() {
                set(CompareAction::Swap);
            }
            if ui.selectable_label(open == Some(Side::B), format!("{} ▾", side_label(&state.b))).clicked() {
                set(if open == Some(Side::B) {
                    CompareAction::ClosePicker
                } else {
                    CompareAction::OpenPicker(Side::B)
                });
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("✕").on_hover_text("Exit compare (Esc)").clicked() {
                    set(CompareAction::Exit);
                }
            });
        });

        if let Some(picker) = &mut state.picker {
            show_picker(ui, colors, picker, &candidates, &mut set);
        }

        let (commits, files) = match &state.loaded {
            Some(Ok(c)) => (Some(c.commit_count), c.files.as_ref().ok().map(Vec::len)),
            _ => (None, None),
        };
        ui.label(compare_title(&side_label(&state.a), &side_label(&state.b), commits, files));
        ui.horizontal(|ui| {
            for (label, tab) in [("Files", Tab::Files), ("Commits", Tab::Commits)] {
                if ui.selectable_label(state.tab == tab, label).clicked() {
                    set(CompareAction::Tab(tab));
                }
            }
            // §5.13 "three-dot by default, toggle to two-dot".
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let two = state.mode == DiffMode::TwoDot;
                let hint =
                    "Two-dot: A's tree against B's (off: three-dot, what B changed since it forked from A)";
                if ui.selectable_label(two, "Two-dot").on_hover_text(hint).clicked() {
                    set(CompareAction::SetMode(if two { DiffMode::ThreeDot } else { DiffMode::TwoDot }));
                }
            });
        });
        ui.separator();

        egui::ScrollArea::vertical().id_salt("compare_body").auto_shrink([false, false]).show(ui, |ui| {
            match &state.loaded {
                None => {
                    ui.weak("Comparing…");
                }
                Some(Err(e)) => {
                    ui.colored_label(colors.get(Token::Danger), e.summary());
                    ui.weak(e.stderr.trim());
                }
                Some(Ok(c)) => match state.tab {
                    Tab::Files => {
                        let files = match &c.files {
                            Ok(files) => files.as_slice(),
                            Err(e) => {
                                ui.colored_label(colors.get(Token::Danger), e.summary());
                                ui.weak(e.stderr.trim());
                                if state.mode == DiffMode::ThreeDot {
                                    ui.weak("Three-dot needs a merge base; Two-dot compares the trees.");
                                }
                                &[]
                            }
                        };
                        if files.is_empty() && c.files.is_ok() {
                            ui.weak("No differences.");
                        }
                        let selected = state.diff.file.as_ref().map(|(_, p)| p.as_str());
                        show_file_tree(
                            ui,
                            colors,
                            files,
                            selected,
                            &state.collapsed,
                            "cmp",
                            &mut set,
                            CompareAction::File,
                            CompareAction::ToggleDir,
                        );
                        ui.separator();
                        let mut more = false;
                        show_diff(ui, colors, &state.diff, &mut more);
                        if more {
                            set(CompareAction::MoreLines);
                        }
                    }
                    Tab::Commits => {
                        if c.commits.is_empty() {
                            ui.weak(format!(
                                "No commits in `{}` that are not in `{}`.",
                                side_label(&state.b),
                                side_label(&state.a)
                            ));
                        }
                        for commit in &c.commits {
                            if commit_row(ui, colors, now, commit).clicked() {
                                set(CompareAction::ViewCommit(commit.id.clone()));
                            }
                        }
                        if c.commit_count > c.commits.len() {
                            ui.weak(format!("Showing the newest {} of {}.", c.commits.len(), c.commit_count));
                        }
                    }
                },
            }
        });

        let picker_open = state.picker.is_some();
        if action.is_none()
            && esc_free
            && !picker_open
            && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            action = Some(CompareAction::Exit);
        }
        if let Some(action) = action {
            self.apply_compare_action(action);
        }
        true
    }

    fn apply_compare_action(&mut self, action: CompareAction) {
        let Some(state) = &mut self.sel.compare else { return };
        match action {
            CompareAction::Exit => self.exit_compare(),
            CompareAction::Swap => {
                std::mem::swap(&mut state.a, &mut state.b);
                self.dispatch_compare();
            }
            CompareAction::SetMode(mode) => {
                state.mode = mode;
                self.dispatch_compare();
            }
            CompareAction::Tab(tab) => state.tab = tab,
            CompareAction::OpenPicker(side) => {
                state.picker = Some(Picker { side, query: String::new(), selected: 0, focus: true })
            }
            CompareAction::ClosePicker => state.picker = None,
            CompareAction::Pick(side, rev) => {
                state.picker = None;
                match side {
                    Side::A => state.a = rev,
                    Side::B => state.b = rev,
                }
                self.dispatch_compare();
            }
            CompareAction::File(path) => self.dispatch_compare_diff(path),
            CompareAction::ToggleDir(dir) => {
                if !state.collapsed.remove(&dir) {
                    state.collapsed.insert(dir);
                }
            }
            CompareAction::ViewCommit(id) => {
                let commit = match &state.loaded {
                    Some(Ok(c)) => c.commits.iter().find(|c| c.id == id).cloned(),
                    _ => None,
                };
                if let Some(commit) = commit {
                    state.viewing = Some(id);
                    let ctx = self.ctx.clone();
                    self.keeping_compare(|pane| pane.show_commit_details(&ctx, commit));
                }
            }
            CompareAction::BackFromCommit => state.viewing = None,
            CompareAction::MoreLines => state.diff.limit += DIFF_LINE_PAGE,
        }
    }
}

/// The picker's rows for `query`: up to 8 refs ranked by fuzzy match, plus the typed text
/// itself as a revision (a hash or an expression like `main~3`) unless it names a listed ref
/// exactly — first when it looks like a hash, last otherwise. `Err` explains a typed text git
/// can't take.
pub(super) fn picker_options(candidates: &[String], query: &str) -> (Vec<String>, Option<String>) {
    let typed = query.trim();
    let mut rows: Vec<String> =
        git_menu::rank_candidates(candidates, None, typed).into_iter().take(8).collect();
    if typed.is_empty() || candidates.iter().any(|c| c == typed) {
        return (rows, None);
    }
    if let Some(e) = cmp::rev_error(typed) {
        return (rows, Some(e));
    }
    if crate::model::fuzzy::looks_like_hash(typed) {
        rows.insert(0, typed.to_string());
    } else {
        rows.push(typed.to_string());
    }
    (rows, None)
}

/// The fuzzy ref picker under the header: a field, the rows of [`picker_options`] (↑/↓ move,
/// Enter or a click takes one), Esc closes it.
fn show_picker(
    ui: &mut egui::Ui,
    colors: &Colors,
    picker: &mut Picker,
    candidates: &[String],
    set: &mut impl FnMut(CompareAction),
) {
    let id = ui.id().with(("compare_picker", picker.side == Side::A));
    let (mut up, mut down, mut enter, mut esc) = (false, false, false, false);
    if ui.memory(|m| m.has_focus(id)) {
        ui.input_mut(|i| {
            up = i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp);
            down = i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown);
            enter = i.consume_key(egui::Modifiers::NONE, egui::Key::Enter);
            esc = i.consume_key(egui::Modifiers::NONE, egui::Key::Escape);
        });
    }
    let side = if picker.side == Side::A { "left" } else { "right" };
    let resp = ui.add(
        egui::TextEdit::singleline(&mut picker.query)
            .id(id)
            .hint_text(format!("Branch, tag, or commit for the {side} side"))
            .desired_width(ui.available_width()),
    );
    if std::mem::take(&mut picker.focus) {
        resp.request_focus();
    }
    if resp.changed() {
        picker.selected = 0;
    }
    let (rows, error) = picker_options(candidates, &picker.query);
    let last = rows.len().saturating_sub(1);
    if down {
        picker.selected = (picker.selected + 1).min(last);
    }
    if up {
        picker.selected = picker.selected.saturating_sub(1);
    }
    picker.selected = picker.selected.min(last);
    let typed = picker.query.trim();
    for (i, row) in rows.iter().enumerate() {
        let label = if row == typed && !candidates.contains(row) {
            format!("Compare with `{row}`")
        } else {
            row.clone()
        };
        if ui.selectable_label(i == picker.selected, label).clicked() {
            set(CompareAction::Pick(picker.side, row.clone()));
        }
    }
    if let Some(e) = error {
        ui.colored_label(colors.get(Token::Danger), e);
    }
    if enter && let Some(row) = rows.get(picker.selected) {
        set(CompareAction::Pick(picker.side, row.clone()));
    }
    if esc {
        set(CompareAction::ClosePicker);
    }
    ui.separator();
}

/// The W11 file tree: directories as collapsible headers, files with status and `+a −d`.
#[allow(clippy::too_many_arguments)]
fn show_file_tree<A>(
    ui: &mut egui::Ui,
    colors: &Colors,
    files: &[FileChange],
    selected: Option<&str>,
    collapsed: &BTreeSet<String>,
    salt: &str,
    set: &mut impl FnMut(A),
    file: impl Fn(String) -> A,
    dir: impl Fn(String) -> A,
) {
    for (d, idx) in group_by_dir(files) {
        let indent = if d.is_empty() { 0.0 } else { 14.0 };
        if !d.is_empty() {
            let arrow = if collapsed.contains(&d) { "▸" } else { "▾" };
            let resp = ui.add(egui::Label::new(format!("{arrow} {d}")).sense(egui::Sense::click()));
            if resp.clicked() {
                set(dir(d.clone()));
            }
            if collapsed.contains(&d) {
                continue;
            }
        }
        for i in idx {
            let f = &files[i];
            let on = selected == Some(f.path.as_str());
            if file_row(ui, colors, f, indent, false, on, (salt, &f.path)).clicked() {
                set(file(f.path.clone()));
            }
        }
    }
}

/// One changed file: `M mod.rs  +12 −3` (renames `R old → new`), highlighted when selected;
/// `full_path` names it by its whole path instead of the name under its directory header.
#[allow(clippy::too_many_arguments)]
fn file_row(
    ui: &mut egui::Ui,
    colors: &Colors,
    f: &FileChange,
    indent: f32,
    full_path: bool,
    selected: bool,
    salt: impl std::hash::Hash + std::fmt::Debug,
) -> egui::Response {
    let name =
        if full_path { f.path.as_str() } else { f.path.rsplit_once('/').map_or(f.path.as_str(), |(_, n)| n) };
    let label = match &f.old_path {
        Some(old) => format!("{} {old} → {name}", f.status),
        None => format!("{} {name}", f.status),
    };
    let counts = match (f.added, f.deleted) {
        (Some(a), Some(0)) => format!("+{a}"),
        (Some(a), Some(d)) => format!("+{a} −{d}"),
        _ => "binary".to_string(),
    };
    let resp = ui.horizontal(|ui| {
        if selected {
            ui.painter().rect_filled(ui.available_rect_before_wrap(), 0.0, colors.get(Token::BgSelected));
        }
        ui.add_space(indent);
        ui.colored_label(details::status_color(colors, f.status), label);
        ui.weak(counts);
    });
    ui.interact(resp.response.rect, ui.id().with(("sel_file", salt)), egui::Sense::click())
}

/// A commit in a list: short hash, subject, author, age.
fn commit_row(ui: &mut egui::Ui, colors: &Colors, now: u64, c: &Commit) -> egui::Response {
    let resp = ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(super::worker::short_hash(&c.id))
                .monospace()
                .color(colors.get(Token::FgSecondary)),
        );
        ui.label(&c.subject);
        ui.weak(format!("{} · {}", c.author, crate::util::relative_time(now, c.time)));
    });
    ui.interact(resp.response.rect, ui.id().with(("sel_commit", &c.id)), egui::Sense::click())
}

fn show_diff(ui: &mut egui::Ui, colors: &Colors, pane: &DiffPane, more: &mut bool) {
    if pane.loading {
        ui.weak("Loading diff…");
        return;
    }
    if pane.file.is_some() && pane.diff.is_empty() {
        ui.weak("No changes to this file here.");
    }
    for file in &pane.diff {
        details::render_diff_file(ui, colors, file, pane.limit, more);
    }
}

// ---- §5.14 multi-selection details --------------------------------------------------------------

impl GitPane {
    fn ensure_multi_view(&mut self) {
        // Cheap check first: this runs every frame, and placing the ids walks every loaded row.
        let from = (self.sel.multi.clone(), self.commits.len());
        if self.sel.multi_view.as_ref().is_some_and(|v| v.from == from) {
            return;
        }
        let commits = self.selected_commits();
        let key: Vec<String> = commits.iter().map(|c| c.id.clone()).collect();
        if let Some(v) = &mut self.sel.multi_view
            && v.key == key
        {
            v.from = from;
            return;
        }
        let req = self.next_req_id();
        let range = self.selection_is_range(&commits);
        let git = self.git.clone();
        let (newest, oldest) = (commits.first().cloned(), commits.last().cloned());
        let ids = key.clone();
        jobs::spawn(&self.ctx, &self.tx, move || {
            let result = match (range, newest, oldest) {
                (true, Some(n), Some(o)) => cmp::load_range_files(&git, &o.id, o.parents.is_empty(), &n.id),
                _ => cmp::load_per_commit_files(&git, &ids),
            };
            super::worker::Reply::Compare(CompareReply::MultiFiles { req, result })
        });
        self.sel.multi_view = Some(MultiView {
            from,
            key,
            subjects: commits.iter().map(|c| (c.id.clone(), c.subject.clone())).collect(),
            files: None,
            collapsed: BTreeSet::new(),
            req,
            diff: DiffPane { limit: DIFF_LINE_PAGE, ..DiffPane::default() },
        });
    }

    fn show_multi(&mut self, ui: &mut egui::Ui, colors: &Colors) {
        self.ensure_multi_view();
        let Some(view) = &self.sel.multi_view else { return };
        let mut clicked: Option<(Option<String>, String)> = None;
        let mut toggled: Option<String> = None;
        let mut more = false;
        let files = view.files.as_ref().and_then(|r| r.as_ref().ok()).map(SelectionFiles::file_count);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(selection_title(view.key.len(), files)).strong());
            if ui.button("Copy hashes").clicked() {
                ui.ctx().copy_text(view.key.join("\n"));
            }
        });
        egui::ScrollArea::vertical().id_salt("multi_body").auto_shrink([false, false]).show(ui, |ui| {
            for (id, subject) in &view.subjects {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(super::worker::short_hash(id))
                            .monospace()
                            .color(colors.get(Token::FgSecondary)),
                    );
                    ui.label(subject);
                });
            }
            ui.separator();
            let selected = view.diff.file.as_ref();
            match &view.files {
                None => {
                    ui.weak("Loading changed files…");
                }
                Some(Err(e)) => {
                    ui.colored_label(colors.get(Token::Danger), e.summary());
                }
                Some(Ok(SelectionFiles::Combined { files, .. })) => {
                    ui.weak("Combined diff of the range");
                    let on = selected.filter(|(c, _)| c.is_none()).map(|(_, p)| p.as_str());
                    let mut tree = None;
                    let (file, dir) = (CompareAction::File, CompareAction::ToggleDir);
                    show_file_tree(
                        ui,
                        colors,
                        files,
                        on,
                        &view.collapsed,
                        "multi",
                        &mut |a| tree = Some(a),
                        file,
                        dir,
                    );
                    match tree {
                        Some(CompareAction::File(path)) => clicked = Some((None, path)),
                        Some(CompareAction::ToggleDir(d)) => toggled = Some(d),
                        _ => {}
                    }
                }
                Some(Ok(SelectionFiles::PerCommit(groups))) => {
                    for (id, files) in groups {
                        let subject =
                            view.subjects.iter().find(|(i, _)| i == id).map_or("", |(_, s)| s.as_str());
                        ui.label(
                            egui::RichText::new(format!("{} {subject}", super::worker::short_hash(id)))
                                .strong(),
                        );
                        if files.is_empty() {
                            ui.weak("  No file changes.");
                        }
                        for f in files {
                            let on = selected
                                .is_some_and(|(c, p)| c.as_deref() == Some(id.as_str()) && p == &f.path);
                            if file_row(ui, colors, f, 14.0, true, on, ("multi", id, &f.path)).clicked() {
                                clicked = Some((Some(id.clone()), f.path.clone()));
                            }
                        }
                    }
                }
            }
            ui.separator();
            show_diff(ui, colors, &view.diff, &mut more);
        });
        if let Some(v) = &mut self.sel.multi_view {
            if more {
                v.diff.limit += DIFF_LINE_PAGE;
            }
            if let Some(d) = toggled
                && !v.collapsed.remove(&d)
            {
                v.collapsed.insert(d);
            }
        }
        if let Some((commit, path)) = clicked {
            self.dispatch_multi_diff(commit, path);
        }
    }

    fn dispatch_multi_diff(&mut self, commit: Option<String>, path: String) {
        let req = self.next_req_id();
        let Some(view) = &mut self.sel.multi_view else { return };
        let (range, old_path) = match &view.files {
            Some(Ok(SelectionFiles::Combined { base, newest, files })) => {
                (Some((base.clone(), newest.clone())), old_path_of(files, &path))
            }
            Some(Ok(SelectionFiles::PerCommit(groups))) => {
                let files =
                    groups.iter().find(|(id, _)| Some(id) == commit.as_ref()).map(|(_, f)| f.as_slice());
                (None, files.and_then(|f| old_path_of(f, &path)))
            }
            _ => (None, None),
        };
        view.diff = DiffPane {
            file: Some((commit.clone(), path.clone())),
            loading: true,
            req,
            limit: DIFF_LINE_PAGE,
            ..Default::default()
        };
        let git = self.git.clone();
        jobs::spawn(&self.ctx, &self.tx, move || {
            let result = match (range, commit) {
                (Some((base, newest)), _) => {
                    cmp::load_range_file_diff(&git, &base, &newest, &path, old_path.as_deref())
                }
                (None, Some(id)) => cmp::load_commit_file_diff(&git, &id, &path, old_path.as_deref()),
                (None, None) => Ok(Vec::new()),
            };
            super::worker::Reply::Compare(CompareReply::Diff { req, result })
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fc(path: &str) -> FileChange {
        FileChange { path: path.into(), old_path: None, status: 'M', added: Some(1), deleted: Some(0) }
    }

    #[test]
    fn files_group_by_directory_with_top_level_first() {
        let files =
            vec![fc("tests/oauth.rs"), fc("src/auth/oauth.rs"), fc("README.md"), fc("src/auth/mod.rs")];
        let groups = group_by_dir(&files);
        let named: Vec<(&str, Vec<&str>)> = groups
            .iter()
            .map(|(d, idx)| (d.as_str(), idx.iter().map(|i| files[*i].path.as_str()).collect()))
            .collect();
        assert_eq!(
            named,
            vec![
                ("", vec!["README.md"]),
                ("src/auth", vec!["src/auth/mod.rs", "src/auth/oauth.rs"]),
                ("tests", vec!["tests/oauth.rs"]),
            ]
        );
    }

    #[test]
    fn picker_offers_refs_and_the_typed_revision() {
        let refs: Vec<String> = ["feat", "main", "origin/feat", "v1.0"].map(String::from).to_vec();
        let (rows, err) = picker_options(&refs, "");
        assert_eq!((rows.len(), err), (4, None), "every ref, nothing typed");
        let (rows, _) = picker_options(&refs, "feat");
        assert_eq!(rows[0], "feat");
        assert_eq!(rows.iter().filter(|r| *r == "feat").count(), 1, "an exact ref is not repeated");
        let (rows, _) = picker_options(&refs, "fea1234");
        assert_eq!(rows[0], "fea1234", "a hash-looking text comes first, ahead of fuzzy matches");
        let (rows, _) = picker_options(&refs, "main~2");
        assert_eq!(rows.last().map(String::as_str), Some("main~2"), "an expression comes last");
        let (rows, err) = picker_options(&refs, "-x");
        assert!(err.is_some() && !rows.contains(&"-x".to_string()));
    }

    #[test]
    fn headers_follow_the_spec_wording() {
        assert_eq!(
            compare_title("main", "feat", Some(12), Some(34)),
            "Comparing `main`…`feat` · 12 commits · 34 files"
        );
        assert_eq!(
            compare_title("main", "feat", Some(1), Some(1)),
            "Comparing `main`…`feat` · 1 commit · 1 file"
        );
        assert_eq!(compare_title("a", "b", None, None), "Comparing `a`…`b`");
        assert_eq!(selection_title(5, Some(18)), "5 commits selected · 18 files");
        assert_eq!(selection_title(2, None), "2 commits selected");
    }

    #[test]
    fn full_hashes_show_short_and_refs_as_typed() {
        assert_eq!(side_label(&"a".repeat(40)), "aaaaaaa");
        assert_eq!(side_label("feat"), "feat");
        assert_eq!(side_label("deadbeef"), "deadbeef", "a short hash is left as typed");
    }

    fn commit(id: &str, parents: &[&str]) -> Commit {
        Commit {
            id: id.into(),
            parents: parents.iter().map(|p| p.to_string()).collect(),
            author: "A".into(),
            email: "a@x".into(),
            time: 0,
            committer: "A".into(),
            committer_email: "a@x".into(),
            commit_time: 0,
            refs: Vec::new(),
            subject: id.into(),
        }
    }

    fn pane_with_rows() -> GitPane {
        let mut pane = super::super::tests::new_test_pane();
        pane.apply_log_page(
            vec![commit("d", &["c"]), commit("c", &["b"]), commit("b", &["a"]), commit("a", &[])],
            500,
        );
        pane
    }

    #[test]
    fn modifier_clicks_build_the_selection_and_the_details_follow_the_cursor() {
        let mut pane = pane_with_rows();
        pane.graph_click(0, egui::Modifiers::NONE);
        assert!(!pane.multi_active());
        pane.graph_click(2, egui::Modifiers::SHIFT);
        assert!(pane.multi_active());
        assert_eq!(pane.selected, Selection::Commit("b".into()));
        assert!(pane.row_selected("c") && !pane.row_selected("a"));
        let rows: Vec<String> = pane.selected_commits().into_iter().map(|c| c.id).collect();
        assert_eq!(rows, ["d", "c", "b"]);
        assert!(pane.selection_is_range(&pane.selected_commits()));
        pane.graph_click(1, egui::Modifiers::COMMAND);
        assert!(!pane.selection_is_range(&pane.selected_commits()), "d and b are not contiguous");
        // A plain click ends it.
        pane.graph_click(3, egui::Modifiers::NONE);
        assert!(!pane.multi_active());
        assert_eq!(pane.selected, Selection::Commit("a".into()));
    }

    #[test]
    fn shift_arrows_extend_from_the_single_selection() {
        let mut pane = pane_with_rows();
        pane.selected = Selection::Commit("c".into()); // selected some other way (search, keys)
        pane.extend_by_key(1);
        pane.extend_by_key(1);
        let rows: Vec<String> = pane.selected_commits().into_iter().map(|c| c.id).collect();
        assert_eq!(rows, ["c", "b", "a"]);
        assert_eq!(pane.selected, Selection::Commit("a".into()));
    }

    #[test]
    fn a_right_click_inside_the_selection_keeps_it() {
        let mut pane = pane_with_rows();
        pane.graph_click(0, egui::Modifiers::NONE);
        pane.graph_click(1, egui::Modifiers::SHIFT);
        pane.graph_secondary_click(0);
        assert!(pane.multi_active());
        pane.graph_secondary_click(3);
        assert!(!pane.multi_active());
    }

    #[test]
    fn esc_restores_the_selection_from_before_compare() {
        let mut pane = pane_with_rows();
        pane.graph_click(0, egui::Modifiers::NONE);
        pane.graph_click(1, egui::Modifiers::SHIFT);
        pane.open_compare("a".into(), "d".into(), None);
        pane.graph_click(3, egui::Modifiers::COMMAND); // a Mod-click ends compare…
        assert!(pane.sel.compare.is_none());
        pane.graph_click(0, egui::Modifiers::NONE);
        pane.graph_click(1, egui::Modifiers::SHIFT);
        pane.open_compare("a".into(), "d".into(), Some(Side::A));
        pane.open_compare("b".into(), "d".into(), None); // re-targeting keeps the first restore point
        pane.selected = Selection::None;
        pane.exit_compare();
        assert!(pane.sel.compare.is_none());
        assert_eq!(pane.selected, Selection::Commit("c".into()));
        assert!(pane.multi_active(), "the multi-selection comes back too");
    }

    /// Selecting a stash in the Refs view (or anything else, anywhere) shows it, not compare.
    #[test]
    fn another_selection_ends_compare_mode() {
        let mut pane = pane_with_rows();
        pane.graph_click(1, egui::Modifiers::NONE);
        pane.open_compare("a".into(), "d".into(), None);
        let stash = crate::git::refs::Stash {
            index: 0,
            oid: "e".repeat(40),
            time: 0,
            message: "wip".into(),
            branch: None,
        };
        let ctx = pane.ctx.clone();
        pane.select_stash(&ctx, &stash);
        assert!(pane.sel.compare.is_none(), "the stash's details and buttons show");
        // A commit opened from the Commits tab keeps compare; a search hit or a new commit
        // moving the graph selection ends it.
        pane.open_compare("a".into(), "d".into(), None);
        pane.keeping_compare(|p| p.load_details_for_selected(&ctx));
        pane.drop_stale_compare();
        assert!(pane.sel.compare.is_some());
        pane.selected = Selection::Commit("a".into());
        pane.drop_stale_compare();
        assert!(pane.sel.compare.is_none());
    }
}
