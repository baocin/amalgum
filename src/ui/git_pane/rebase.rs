//! Interactive rebase in the git pane (§5.16, W9): the plan editor (plan on the left, the live
//! preview graph on the right, the pane maximized), the commit-menu shortcuts that build a plan
//! and run it at once (**Edit commit message**, **Squash / Fixup into parent**, **Squash into
//! one**), and a running rebase: "Rebase paused at ab12cd3" with **Continue** / **Abort** in the
//! status bar, conflicts handed to the Changes view (§5.17), and the journal entry once it is
//! done (§5.18: undo resets to the pre-rebase hash).
//!
//! Editing lives in `model::rebase_plan`; running lives in `git::rebase` (on worker threads,
//! through `git::Git`, so a remote workspace rebases on its host with the CLI installed there).

use super::worker::Reply;
use super::{GitEvent, GitPane, Selection};
use crate::git::GitError;
use crate::git::cmd::Location;
use crate::git::ops::{OpError, short_hash};
use crate::git::rebase::{self, Action, Helper, Range, RangeError, Session, Stop};
use crate::git::status::EntryKind;
use crate::model::confirm::{Confirm, Decision};
use crate::model::keymap::{Chord, Key, Mods, Preset};
use crate::model::rebase_plan::{self, PlanEditor};
use crate::model::settings::Settings;
use crate::model::state::GitView;
use crate::model::theme::Token;
use crate::ui::chrome::Toast;
use crate::ui::dialogs::{self, FormOutcome, Popover};
use crate::ui::theme::Colors;
use crate::ui::{jobs, shortcuts};

/// What a [`rebase::read_range`] is for.
#[derive(Debug, Clone)]
pub(super) enum Purpose {
    /// Open the W9 editor.
    Editor,
    /// **Edit commit message**: the popover, prefilled with the commit's message.
    EditMessage { hash: String, at: egui::Pos2 },
    /// **Squash / Fixup into parent**: the range starts at the parent.
    Meld { hash: String, action: Action },
    /// **Squash into one**: the range starts at the oldest commit.
    #[allow(dead_code)] // built by `squash_range`, whose caller lands with §5.14
    SquashRange { newest: String },
}

/// Worker results for this module (carried by `Reply::Rebase`).
pub(super) enum RebaseReply {
    Range { purpose: Purpose, result: Box<Result<Range, RangeError>> },
    Started(Box<Result<(Session, Stop), OpError>>),
    Continued(Result<Stop, OpError>),
    Aborted(Result<(), GitError>),
    Probed(Result<Stop, OpError>),
}

struct EditorUi {
    model: PlanEditor,
    /// The commits' order when the editor opened: a plan that keeps it and picks everything
    /// changes nothing.
    original_order: Vec<String>,
    /// The row being dragged by its handle.
    drag: Option<usize>,
    /// Last frame's row rectangles, for the drop position.
    row_rects: Vec<egui::Rect>,
}

struct MessagePrompt {
    at: egui::Pos2,
    hash: String,
    range: Range,
    text: String,
}

/// A one-shot rewrite (squash / fixup into parent, squash into one) of commits already on a
/// protected branch's remote: the §5.16 warning, confirmed before it runs.
struct PublishedPrompt {
    at: egui::Pos2,
    range: Range,
    plan: rebase::Plan,
    description: String,
}

/// A rebase this pane started that git has stopped (an `edit`, or conflicts).
struct Active {
    session: Session,
    /// The commit it stopped at (full hash).
    at: String,
    conflicts: bool,
    last_probe: f64,
}

enum EditorOutcome {
    Open,
    Cancel,
    Start,
}

/// The pane's interactive-rebase state (one field on `GitPane`).
#[derive(Default)]
pub(super) struct RebaseState {
    editor: Option<EditorUi>,
    message: Option<MessagePrompt>,
    published: Option<PublishedPrompt>,
    active: Option<Active>,
    confirm_abort: Option<Confirm>,
    /// Set by the app for a remote workspace (the host's installed CLI, or why there is none);
    /// `None` locally, where the helper is this very binary.
    helper: Option<Result<Helper, String>>,
    reading: bool,
    running: bool,
    probing: bool,
    /// `Settings::git.protected_branches`, refreshed every frame.
    protected: Vec<String>,
    /// Events from entry points without an `events` list, handed out on the next `show`.
    queued: Vec<GitEvent>,
    runs: u64,
}

/// "Rebase paused at ab12cd3" / "Rebase stopped at ab12cd3 · 2 conflicts" (§5.16 step 4).
pub fn paused_label(at: &str, conflicts: usize) -> String {
    match conflicts {
        0 => format!("Rebase paused at {}", short_hash(at)),
        1 => format!("Rebase stopped at {} · 1 conflict", short_hash(at)),
        n => format!("Rebase stopped at {} · {n} conflicts", short_hash(at)),
    }
}

impl GitPane {
    /// The editor helper for a remote workspace: its host's installed CLI, or why rebasing
    /// there is not possible (shown on the disabled **Start rebase**).
    pub fn set_rebase_helper(&mut self, helper: Result<Helper, String>) {
        self.rebase.helper = Some(helper);
    }

    fn rebase_helper(&self) -> Result<Helper, String> {
        match (&self.rebase.helper, &self.location) {
            (Some(helper), _) => helper.clone(),
            (None, Location::Local { .. }) => {
                // Asked once: `start_blocker` wants it every frame.
                static LOCAL: std::sync::OnceLock<Result<Helper, String>> = std::sync::OnceLock::new();
                LOCAL
                    .get_or_init(|| {
                        std::env::current_exe()
                            .map(|p| Helper { program: p.display().to_string() })
                            .map_err(|e| format!("Can't locate the amalgum binary for git's editor: {e}"))
                    })
                    .clone()
            }
            (None, Location::Remote { .. }) => Err("Waiting for the connection to this host".to_string()),
        }
    }

    /// The W9 editor is open (it replaces the pane's views).
    pub(super) fn rebase_editor_open(&self) -> bool {
        self.rebase.editor.is_some()
    }

    /// The commit a stopped rebase this pane runs is at, for the status bar.
    pub(super) fn rebase_paused_at(&self) -> Option<String> {
        self.rebase.active.as_ref().map(|a| a.at.clone())
    }

    fn rebase_toast(&mut self, t: Toast) {
        self.rebase.queued.push(GitEvent::Toast(t));
    }

    fn is_protected_branch(protected: &[String], branch: &str) -> bool {
        let mut s = Settings::default();
        s.git.protected_branches = protected.to_vec();
        s.is_protected(branch)
    }

    /// Reads the range from `commit` to HEAD on a worker, for `purpose`.
    fn read_rebase_range(&mut self, ctx: &egui::Context, commit: String, purpose: Purpose) {
        if self.rebase.active.is_some() {
            return self
                .rebase_toast(Toast::info("A rebase is already in progress: continue or abort it first"));
        }
        if self.rebase.editor.is_some() {
            return self.rebase_toast(Toast::info("Start or cancel the open rebase plan first"));
        }
        if self.rebase.reading {
            return;
        }
        self.rebase.reading = true;
        let git = self.git.clone();
        let remotes = self.remote_names();
        let protected = self.rebase.protected.clone();
        jobs::spawn(ctx, &self.tx, move || {
            let result =
                rebase::read_range(&git, &commit, &remotes, |b| Self::is_protected_branch(&protected, b));
            Reply::Rebase(RebaseReply::Range { purpose, result: Box::new(result) })
        });
    }

    /// Right-click → **Interactively rebase from here**, `Mod+Shift+R` (§5.16).
    pub(super) fn open_interactive_rebase(&mut self, ctx: &egui::Context, commit: &str) {
        self.read_rebase_range(ctx, commit.to_string(), Purpose::Editor);
    }

    /// **Edit commit message**: a popover with the full message, then a reword plan.
    pub(super) fn open_edit_message(&mut self, ctx: &egui::Context, commit: &str, at: Option<egui::Pos2>) {
        let at = at.unwrap_or_else(|| self.ops_anchor());
        self.read_rebase_range(
            ctx,
            commit.to_string(),
            Purpose::EditMessage { hash: commit.to_string(), at },
        );
    }

    /// **Squash into parent** / **Fixup into parent**: runs at once.
    pub(super) fn meld_into_parent(&mut self, ctx: &egui::Context, commit: &str, action: Action) {
        let parents = self.commits.iter().find(|c| c.id == commit).map(|c| c.parents.clone());
        match parents.as_deref() {
            Some([parent]) => {
                let purpose = Purpose::Meld { hash: commit.to_string(), action };
                self.read_rebase_range(ctx, parent.clone(), purpose);
            }
            Some([]) => self.rebase_toast(Toast::info("The first commit has no parent to meld into")),
            Some(_) => self.rebase_toast(Toast::info("A merge commit can't be squashed into its parent")),
            None => self.rebase_toast(Toast::info("That commit is not loaded in the graph")),
        }
    }

    /// **Squash into one** (§5.14) for a contiguous selection on the current branch, `oldest`
    /// through `newest`: every commit after `oldest` is squashed into it, commits after `newest`
    /// are replayed as they are. Runs at once; conflicts and undo work as for any rebase. For
    /// the multi-select feature to call.
    pub fn squash_range(&mut self, ctx: &egui::Context, oldest: &str, newest: &str) {
        self.read_rebase_range(ctx, oldest.to_string(), Purpose::SquashRange { newest: newest.to_string() });
    }

    /// Starts `plan` on a worker (one git operation at a time).
    fn run_rebase_plan(
        &mut self,
        ctx: &egui::Context,
        base: Option<String>,
        head: String,
        plan: rebase::Plan,
        description: String,
    ) {
        let helper = match self.rebase_helper() {
            Ok(h) => h,
            Err(why) => return self.rebase_toast(Toast::error("Can't start the rebase", why)),
        };
        if let Some(t) = self.busy_refusal() {
            return self.rebase_toast(t);
        }
        self.set_busy(true);
        self.rebase.running = true;
        self.rebase.runs += 1;
        let id = format!("{}-{}-{}", crate::util::unix_now(), std::process::id(), self.rebase.runs);
        let git = self.git.clone();
        let now = crate::util::unix_now();
        jobs::spawn(ctx, &self.tx, move || {
            let result = rebase::start(&git, &helper, base.as_deref(), &head, &plan, &description, &id, now);
            Reply::Rebase(RebaseReply::Started(Box::new(result)))
        });
    }

    /// **Continue** in the status bar: `git rebase --continue` with the same helpers.
    pub fn rebase_continue(&mut self) {
        let Some(session) = self.rebase.active.as_ref().map(|a| a.session.clone()) else { return };
        if let Some(t) = self.busy_refusal() {
            return self.rebase_toast(t);
        }
        self.set_busy(true);
        self.rebase.running = true;
        let git = self.git.clone();
        let now = crate::util::unix_now();
        jobs::spawn(&self.ctx.clone(), &self.tx, move || {
            Reply::Rebase(RebaseReply::Continued(rebase::resume(&git, &session, now)))
        });
    }

    /// **Abort** in the status bar: the §5.20 confirmation, then `git rebase --abort`.
    pub fn rebase_abort(&mut self) {
        if let Some(active) = &self.rebase.active {
            let branch = active.session.pending.before.branch.clone();
            self.rebase.confirm_abort = Some(rebase_plan::abort_confirm(branch.as_deref(), &active.at));
        }
    }

    fn run_rebase_abort(&mut self, ctx: &egui::Context) {
        let Some(session) = self.rebase.active.as_ref().map(|a| a.session.clone()) else { return };
        if let Some(t) = self.busy_refusal() {
            return self.rebase_toast(t);
        }
        self.set_busy(true);
        self.rebase.running = true;
        let git = self.git.clone();
        jobs::spawn(ctx, &self.tx, move || {
            Reply::Rebase(RebaseReply::Aborted(rebase::abort(&git, &session)))
        });
    }

    /// The name the header and preview give the base: a branch there, else its short hash.
    fn base_label(&self, base: Option<&str>) -> String {
        let Some(base) = base else { return "the root".to_string() };
        let current = self.current_branch();
        self.commits
            .iter()
            .find(|c| c.id == base)
            .and_then(|c| {
                let refs = crate::model::git_menu::RowRefs::from_decorations(&c.refs);
                refs.locals
                    .into_iter()
                    .find(|b| Some(b) != current.as_ref())
                    .or(refs.remotes.into_iter().next())
            })
            .unwrap_or_else(|| short_hash(base).to_string())
    }

    pub(super) fn apply_rebase_reply(
        &mut self,
        ctx: &egui::Context,
        reply: RebaseReply,
        events: &mut Vec<GitEvent>,
    ) {
        match reply {
            RebaseReply::Range { purpose, result } => {
                self.rebase.reading = false;
                let range = match *result {
                    Ok(range) => range,
                    Err(e) => {
                        let details = match &e {
                            RangeError::Git(g) => g.stderr.clone(),
                            _ => String::new(),
                        };
                        return events.push(GitEvent::Toast(Toast::error(e.to_string(), details)));
                    }
                };
                self.on_range(ctx, purpose, range, events);
            }
            RebaseReply::Started(result) => {
                self.set_busy(false);
                self.rebase.running = false;
                match *result {
                    Ok((session, stop)) => self.on_stop(ctx, session, stop, events),
                    Err(e) => events.push(GitEvent::Toast(op_error_toast("Rebase failed", &e))),
                }
            }
            RebaseReply::Continued(result) => {
                self.set_busy(false);
                self.rebase.running = false;
                match (result, self.rebase.active.take()) {
                    (Ok(stop), Some(active)) => self.on_stop(ctx, active.session, stop, events),
                    (Err(e), active) => {
                        self.rebase.active = active;
                        events.push(GitEvent::Toast(op_error_toast("Can't continue the rebase", &e)));
                        self.refresh_now(ctx);
                    }
                    (Ok(_), None) => {}
                }
            }
            RebaseReply::Aborted(result) => {
                self.set_busy(false);
                self.rebase.running = false;
                match result {
                    Ok(()) => {
                        self.rebase.active = None;
                        events.push(GitEvent::Toast(Toast::info("Rebase aborted")));
                    }
                    Err(e) => events.push(GitEvent::Toast(Toast::error(e.summary(), e.stderr.clone()))),
                }
                self.refresh_now(ctx);
            }
            RebaseReply::Probed(result) => {
                self.rebase.probing = false;
                if self.rebase.running {
                    return; // our own continue/abort answers instead
                }
                match result {
                    Ok(Stop::Paused { at, .. }) => {
                        if let Some(a) = self.rebase.active.as_mut() {
                            (a.at, a.conflicts) = (at, false);
                        }
                    }
                    Ok(Stop::Conflicts { at, .. }) => {
                        if let Some(a) = self.rebase.active.as_mut() {
                            (a.at, a.conflicts) = (at, true);
                        }
                    }
                    Ok(Stop::Done(entry)) => {
                        // Continued or aborted outside the app (a terminal, an agent).
                        if let Some(active) = self.rebase.active.take() {
                            self.on_stop(ctx, active.session, Stop::Done(entry), events);
                        }
                    }
                    Err(_) => {} // try again on the next probe
                }
            }
        }
    }

    fn on_range(&mut self, ctx: &egui::Context, purpose: Purpose, range: Range, events: &mut Vec<GitEvent>) {
        match purpose {
            Purpose::Editor => {
                let label = self.base_label(range.base.as_deref());
                let model = PlanEditor::new(&range, label);
                let original_order = model.rows.iter().map(|r| r.hash.clone()).collect();
                self.rebase.editor =
                    Some(EditorUi { model, original_order, drag: None, row_rects: Vec::new() });
                events.push(GitEvent::MaximizePane(true));
            }
            Purpose::EditMessage { hash, at } => {
                let text = range.commits.first().map(|c| c.message.clone()).unwrap_or_default();
                self.rebase.message = Some(MessagePrompt { at, hash, range, text });
            }
            Purpose::Meld { hash, action } => {
                // The range is read from the parent; the commit itself must be on it too.
                if !range.commits.iter().any(|c| c.hash == hash) {
                    return events.push(GitEvent::Toast(Toast::info(format!(
                        "{} is not in the history of `{}`",
                        short_hash(&hash),
                        range.branch
                    ))));
                }
                let plan = rebase_plan::meld_into_parent_plan(&range, &hash, action);
                let verb = if action == Action::Fixup { "fixup" } else { "squash" };
                let description = format!("{verb} {} into parent", short_hash(&hash));
                self.run_or_warn(ctx, range, plan, description);
            }
            Purpose::SquashRange { newest } => match rebase_plan::squash_range_plan(&range, &newest) {
                Some(plan) => {
                    let n = plan.steps.iter().filter(|s| s.action == Action::Squash).count() + 1;
                    self.run_or_warn(ctx, range, plan, format!("squash {n} commits into one"));
                }
                None => events.push(GitEvent::Toast(Toast::info(format!(
                    "{} is not after the oldest selected commit on `{}`",
                    short_hash(&newest),
                    range.branch
                )))),
            },
        }
    }

    /// Runs a one-shot plan at once, or first asks when it rewrites commits a protected remote
    /// branch already has (§5.16 precondition warning).
    fn run_or_warn(&mut self, ctx: &egui::Context, range: Range, plan: rebase::Plan, description: String) {
        if range.published.is_empty() {
            let (base, head) = (range.base.clone(), range.head.clone());
            return self.run_rebase_plan(ctx, base, head, plan, description);
        }
        let at = self.ops_anchor();
        self.rebase.published = Some(PublishedPrompt { at, range, plan, description });
    }

    /// Where git left the rebase: journal a finished one, or hold a stopped one for the status
    /// bar and hand the Changes view over (to amend at an `edit`, to resolve conflicts, §5.17).
    fn on_stop(&mut self, ctx: &egui::Context, session: Session, stop: Stop, events: &mut Vec<GitEvent>) {
        let toast = |t: Toast| GitEvent::Toast(t);
        match stop {
            Stop::Done(Some(entry)) => {
                self.rebase.active = None;
                let description = entry.description.clone();
                self.journal.record(*entry);
                let _ = self.journal.save(&self.journal_path);
                events.push(toast(Toast::success(format!("Rebased: {description} (undo is available)"))));
            }
            Stop::Done(None) => {
                self.rebase.active = None;
                events.push(toast(Toast::info("Rebase finished: nothing changed")));
            }
            Stop::Paused { at, error } => {
                let short = short_hash(&at).to_string();
                self.rebase.active = Some(Active { session, at, conflicts: false, last_probe: 0.0 });
                self.view = GitView::Changes;
                events.push(toast(match error {
                    // git stopped because a step failed, not as planned: say why (§5.24).
                    // Its message can span lines ("You must edit all merge conflicts and then /
                    // mark them as resolved…"), so all of it goes in the details.
                    Some(e) => {
                        Toast::error(format!("Rebase is still stopped at {short}: git refused"), e.stderr)
                    }
                    None => Toast::info(format!("Rebase paused at {short}: amend, then Continue")),
                }));
            }
            Stop::Conflicts { at, error } => {
                self.rebase.active = Some(Active { session, at, conflicts: true, last_probe: 0.0 });
                self.view = GitView::Changes;
                events.push(toast(Toast::warning("Rebase stopped on conflicts", error.stderr)));
            }
        }
        self.refresh_now(ctx);
    }

    /// Per frame: hand out queued events, poll a stopped rebase (it may be continued or aborted
    /// in a terminal), and draw the abort confirmation and the edit-message popover.
    pub(super) fn show_rebase_overlays(
        &mut self,
        ui: &egui::Ui,
        colors: &Colors,
        settings: &Settings,
        preset: Preset,
        events: &mut Vec<GitEvent>,
    ) {
        let ctx = ui.ctx().clone();
        self.rebase.protected.clone_from(&settings.git.protected_branches);
        events.append(&mut self.rebase.queued);

        let now = ctx.input(|i| i.time);
        let every = match self.location {
            Location::Local { .. } => 2.0,
            Location::Remote { .. } => f64::from(settings.git.remote_refresh_secs.max(2)),
        };
        if let Some(active) = self.rebase.active.as_mut()
            && !self.rebase.running
            && !self.rebase.probing
            && now - active.last_probe >= every
        {
            active.last_probe = now;
            self.rebase.probing = true;
            let (git, session) = (self.git.clone(), active.session.clone());
            let unix = crate::util::unix_now();
            jobs::spawn(&ctx, &self.tx, move || {
                Reply::Rebase(RebaseReply::Probed(rebase::probe(&git, &session, unix)))
            });
        }
        if self.rebase.active.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(every));
        }

        if let Some(confirm) = &mut self.rebase.confirm_abort {
            match dialogs::confirm_dialog(&ctx, colors, preset, confirm) {
                Some(Decision::Confirm) => {
                    self.rebase.confirm_abort = None;
                    self.run_rebase_abort(&ctx);
                }
                Some(Decision::Cancel) => self.rebase.confirm_abort = None,
                None => {}
            }
        }

        if let Some(mut prompt) = self.rebase.message.take() {
            let title = format!("Edit message of {}", short_hash(&prompt.hash));
            let error = prompt.text.trim().is_empty().then_some("A commit message can't be empty");
            let warning = published_warning(&prompt.range.published);
            let outcome = Popover::new("rebase-edit-message", title).primary("Reword").show(
                &ctx,
                colors,
                prompt.at,
                |f| {
                    f.text_area(&mut prompt.text, "Commit message", error)
                        .note("Rewrites this commit and every commit after it on the branch.");
                    if let Some(w) = &warning {
                        f.note(w);
                    }
                },
            );
            match outcome {
                FormOutcome::Open => self.rebase.message = Some(prompt),
                FormOutcome::Cancelled => {}
                FormOutcome::Submitted => {
                    let plan = rebase_plan::reword_plan(&prompt.range, &prompt.hash, &prompt.text);
                    let description = format!("reword {}", short_hash(&prompt.hash));
                    let (base, head) = (prompt.range.base.clone(), prompt.range.head.clone());
                    self.run_rebase_plan(&ctx, base, head, plan, description);
                }
            }
        }

        if let Some(prompt) = self.rebase.published.take() {
            let title = format!("Rewrite published commits: {}?", prompt.description);
            let warning = published_warning(&prompt.range.published).unwrap_or_default();
            let outcome = Popover::new("rebase-published", title).primary("Rewrite").show(
                &ctx,
                colors,
                prompt.at,
                |f| {
                    f.note(&warning);
                },
            );
            match outcome {
                FormOutcome::Open => self.rebase.published = Some(prompt),
                FormOutcome::Cancelled => {}
                FormOutcome::Submitted => {
                    let (base, head) = (prompt.range.base.clone(), prompt.range.head.clone());
                    self.run_rebase_plan(&ctx, base, head, prompt.plan, prompt.description);
                }
            }
        }
    }

    /// Tracked files with changes: `git rebase` refuses to start over them.
    fn tracked_changes(&self) -> usize {
        self.status
            .entries
            .iter()
            .filter(|e| !matches!(e.kind, EntryKind::Untracked | EntryKind::Ignored))
            .count()
    }

    /// Why **Start rebase** is disabled, if it is.
    fn start_blocker(&self, ed: &EditorUi) -> Option<String> {
        if self.rebase.running || self.busy_refusal().is_some() {
            return Some("Another git operation is still running".to_string());
        }
        if let Err(why) = self.rebase_helper() {
            return Some(why);
        }
        match self.tracked_changes() {
            0 => {}
            1 => return Some("Commit or stash the changed file first".to_string()),
            n => return Some(format!("Commit or stash the {n} changed files first")),
        }
        if let Some(problem) = ed.model.problem() {
            return Some(problem.message);
        }
        ed.model.unchanged(&ed.original_order).then(|| "The plan doesn't change anything yet".to_string())
    }

    /// W9 in place of the pane's views while the editor is open; `false` when it isn't.
    pub(super) fn show_rebase_editor(
        &mut self,
        ui: &mut egui::Ui,
        colors: &Colors,
        preset: Preset,
        events: &mut Vec<GitEvent>,
    ) -> bool {
        let Some(mut ed) = self.rebase.editor.take() else { return false };
        let blocker = self.start_blocker(&ed);
        let focused = self.app_focused();
        let outcome = draw_editor(ui, colors, &mut ed, blocker.as_deref(), focused, preset);
        match outcome {
            EditorOutcome::Open => self.rebase.editor = Some(ed),
            EditorOutcome::Cancel => events.push(GitEvent::MaximizePane(false)),
            EditorOutcome::Start => {
                events.push(GitEvent::MaximizePane(false));
                let description = format!("interactive rebase of {}", ed.model.branch);
                let ctx = ui.ctx().clone();
                let (base, head) = (ed.model.base.clone(), ed.model.head.clone());
                self.run_rebase_plan(&ctx, base, head, ed.model.plan(), description);
            }
        }
        true
    }

    /// `Mod+Shift+R` and the palette's commit-menu shortcuts, on the selected graph commit.
    pub(super) fn rebase_action(&mut self, ctx: &egui::Context, action: crate::model::keymap::Action) {
        use crate::model::keymap::Action as A;
        let Selection::Commit(id) = self.selected.clone() else {
            return self.rebase_toast(Toast::info("Select a commit in the graph first"));
        };
        match action {
            A::InteractiveRebaseFromSelected => self.open_interactive_rebase(ctx, &id),
            A::SquashSelectedIntoParent => self.meld_into_parent(ctx, &id, Action::Squash),
            A::FixupSelectedIntoParent => self.meld_into_parent(ctx, &id, Action::Fixup),
            A::EditSelectedCommitMessage => self.open_edit_message(ctx, &id, None),
            _ => {}
        }
    }
}

/// The §5.16 warning for commits a protected remote branch already has; `None` when none do.
fn published_warning(published: &[String]) -> Option<String> {
    (!published.is_empty()).then(|| {
        format!(
            "⚠ Some of these commits are already on {}: rewriting them needs a force push",
            published.join(", ")
        )
    })
}

fn op_error_toast(title: &str, e: &OpError) -> Toast {
    match e {
        OpError::Git(g) => Toast::error(format!("{title}: {}", g.summary()), g.stderr.clone()),
        other => Toast::error(format!("{title}: {other}"), String::new()),
    }
}

/// What a key press does in the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorKey {
    SetAction(Action),
    Focus(isize),
    Move(isize),
    Cancel,
}

/// The editor's keys (W9, §5.16 step 2), as the keymap preset spells them: `p r e s f x` set the
/// focused row's action, `↑/↓` move the focus, `Mod+↑/↓` move the row, `Esc` cancels.
fn editor_key(chord: &Chord) -> Option<EditorKey> {
    let is = |spec: &str| Chord::parse(spec).ok().as_ref() == Some(chord);
    if is("Mod+↑") {
        return Some(EditorKey::Move(-1));
    }
    if is("Mod+↓") {
        return Some(EditorKey::Move(1));
    }
    if is("↑") {
        return Some(EditorKey::Focus(-1));
    }
    if is("↓") {
        return Some(EditorKey::Focus(1));
    }
    if is("Esc") {
        return Some(EditorKey::Cancel);
    }
    match chord.key {
        Key::Char(c) if chord.mods == Mods::default() => Action::from_key(c).map(EditorKey::SetAction),
        _ => None,
    }
}

/// Applies (and consumes) this frame's editor keys; `true` when `Esc` cancelled.
fn editor_keys(ui: &egui::Ui, ed: &mut EditorUi, preset: Preset) -> bool {
    let mut cancel = false;
    ui.input_mut(|i| {
        i.events.retain(|e| {
            let egui::Event::Key { key, pressed: true, modifiers, .. } = e else { return true };
            let Some(k) = shortcuts::chord(*key, *modifiers, preset).as_ref().and_then(editor_key) else {
                return true;
            };
            match k {
                EditorKey::SetAction(a) => ed.model.set_focused_action(a),
                EditorKey::Focus(d) => ed.model.focus_by(d),
                EditorKey::Move(d) => ed.model.move_focused(d),
                EditorKey::Cancel => cancel = true,
            }
            false
        });
    });
    cancel
}

fn draw_editor(
    ui: &mut egui::Ui,
    colors: &Colors,
    ed: &mut EditorUi,
    blocker: Option<&str>,
    focused: bool,
    preset: Preset,
) -> EditorOutcome {
    let mut outcome = EditorOutcome::Open;
    // An open action dropdown owns the keys (Esc closes it, not the whole plan).
    let popup_open = egui::Popup::is_any_open(ui.ctx());
    if focused && !popup_open && !ui.ctx().egui_wants_keyboard_input() && editor_keys(ui, ed, preset) {
        outcome = EditorOutcome::Cancel;
    }
    let secondary = colors.get(Token::FgSecondary);
    let available = ui.available_size();
    let plan_width = (available.x * 0.58).max(320.0).min(available.x);
    let height = available.y.max(200.0);
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(plan_width, height),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.label(egui::RichText::new(ed.model.header()).strong().size(14.0));
                ui.add_space(4.0);
                if let Some(warning) = published_warning(&ed.model.published) {
                    ui.colored_label(colors.get(Token::Warning), warning);
                }
                let problem_row = ed.model.problem().and_then(|p| p.row);
                egui::Frame::default()
                    .stroke(egui::Stroke::new(1.0, colors.get(Token::Border)))
                    .corner_radius(6)
                    .inner_margin(6)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let max_height = (height - 150.0).max(120.0);
                        egui::ScrollArea::vertical()
                            .max_height(max_height)
                            .auto_shrink([false, true])
                            .show(ui, |ui| plan_rows(ui, colors, ed, problem_row));
                    });
                ui.add_space(4.0);
                ui.colored_label(secondary, "p pick  r reword  e edit  s squash  f fixup  x drop");
                let label = |spec: &str| Chord::parse(spec).map(|c| preset.label(&c)).unwrap_or_default();
                let (up, down) = (label("Mod+↑"), label("Mod+↓"));
                ui.colored_label(secondary, format!("drag ⋮ or {up} / {down} to reorder · Esc cancels"));
                let problem = ed.model.problem().map(|p| p.message);
                if let Some(problem) = &problem {
                    ui.colored_label(colors.get(Token::Danger), problem);
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let on_accent = colors.get(Token::FgOnAccent);
                    let start =
                        egui::Button::new(egui::RichText::new("Start rebase").color(on_accent).strong())
                            .fill(colors.get(Token::Accent));
                    let resp = ui.add_enabled(blocker.is_none(), start);
                    let resp = match blocker {
                        Some(why) => resp.on_disabled_hover_text(why),
                        None => resp,
                    };
                    if resp.clicked() {
                        outcome = EditorOutcome::Start;
                    }
                    if ui.button("Cancel").clicked() {
                        outcome = EditorOutcome::Cancel;
                    }
                    if let Some(why) = blocker.filter(|w| Some(*w) != problem.as_deref()) {
                        ui.colored_label(secondary, why);
                    }
                });
            },
        );
        ui.separator();
        ui.vertical(|ui| {
            ui.label(egui::RichText::new("Preview").strong().size(14.0));
            ui.add_space(4.0);
            preview(ui, colors, &ed.model);
        });
    });
    outcome
}

/// The plan rows: handle, action dropdown, short hash, subject (a text field for reword /
/// squash). Dragging a handle moves the row to where it is dropped.
fn plan_rows(ui: &mut egui::Ui, colors: &Colors, ed: &mut EditorUi, problem_row: Option<usize>) {
    let row_height = 26.0;
    // Dragging a handle across the rows must not select their text.
    ui.style_mut().interaction.selectable_labels = false;
    let mut rects = Vec::with_capacity(ed.model.rows.len());
    let mut dropped: Option<(usize, usize)> = None;
    let pointer = ui.ctx().pointer_interact_pos();
    for i in 0..ed.model.rows.len() {
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), row_height), egui::Sense::hover());
        rects.push(rect);
        let focused = ed.model.focused == i;
        if focused {
            ui.painter().rect_filled(rect, 4.0, colors.get(Token::BgSelected));
        }
        if problem_row == Some(i) {
            ui.painter().rect_stroke(
                rect,
                4.0,
                egui::Stroke::new(1.0, colors.get(Token::Danger)),
                egui::StrokeKind::Inside,
            );
        }
        let mut row_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect.shrink2(egui::vec2(4.0, 2.0)))
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        let handle = row_ui
            .add(
                egui::Label::new(egui::RichText::new("⋮").strong().color(colors.get(Token::FgSecondary)))
                    .sense(egui::Sense::drag()),
            )
            .on_hover_cursor(egui::CursorIcon::Grab);
        if handle.drag_started() {
            ed.drag = Some(i);
            ed.model.focus(i);
        }
        if handle.drag_stopped()
            && let Some(from) = ed.drag.take()
            && let Some(pos) = pointer
        {
            dropped = Some((from, drop_index(&ed.row_rects, pos.y)));
        }
        let mut action = ed.model.rows[i].action;
        egui::ComboBox::from_id_salt(("rebase-action", &ed.model.rows[i].hash))
            .width(78.0)
            .selected_text(action.keyword())
            .show_ui(&mut row_ui, |ui| {
                for a in Action::ALL {
                    ui.selectable_value(&mut action, a, a.keyword());
                }
            });
        if action != ed.model.rows[i].action {
            ed.model.set_action(i, action);
            ed.model.focus(i);
        }
        let hash = short_hash(&ed.model.rows[i].hash).to_string();
        row_ui.label(egui::RichText::new(hash).monospace().color(colors.get(Token::FgSecondary)));
        match ed.model.inline_text(i) {
            Some(mut text) => {
                let edit = egui::TextEdit::singleline(&mut text)
                    .hint_text("Commit message")
                    .desired_width(row_ui.available_width());
                let resp = row_ui.add(edit);
                if resp.changed() {
                    ed.model.set_inline_text(i, &text);
                }
                if resp.gained_focus() {
                    ed.model.focus(i);
                }
            }
            None => {
                let dropped_row = ed.model.rows[i].action == Action::Drop;
                let mut subject = egui::RichText::new(&ed.model.rows[i].subject);
                if dropped_row {
                    subject = subject.strikethrough().color(colors.get(Token::FgDisabled));
                }
                let resp = row_ui.add(egui::Label::new(subject).truncate().sense(egui::Sense::click()));
                if resp.clicked() {
                    ed.model.focus(i);
                }
            }
        }
    }
    // The insertion line while dragging.
    if let (Some(from), Some(pos)) = (ed.drag, pointer) {
        let to = drop_index(&rects, pos.y);
        if to != from
            && let Some(y) = insertion_y(&rects, to)
        {
            let x = rects.first().map_or(egui::Rangef::new(0.0, 0.0), |r| r.x_range());
            ui.painter().hline(x, y, egui::Stroke::new(2.0, colors.get(Token::Accent)));
        }
    }
    ed.row_rects = rects;
    if let Some((from, to)) = dropped {
        ed.model.move_row(from, to);
    }
}

/// The row index a drop at height `y` lands on.
fn drop_index(rects: &[egui::Rect], y: f32) -> usize {
    rects.iter().position(|r| y < r.bottom()).unwrap_or(rects.len().saturating_sub(1))
}

fn insertion_y(rects: &[egui::Rect], index: usize) -> Option<f32> {
    rects.get(index).map(|r| r.center().y)
}

/// The preview graph: the branch as the plan leaves it, newest first, down to the base.
fn preview(ui: &mut egui::Ui, colors: &Colors, model: &PlanEditor) {
    let row_height = 24.0;
    let lane_width = 16.0;
    for row in model.preview() {
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), row_height), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        let mid_y = rect.center().y;
        for seg in &row.graph.segments {
            let x0 = super::graph::lane_x(rect.left(), seg.from, lane_width);
            let x1 = super::graph::lane_x(rect.left(), seg.to, lane_width);
            let (y0, y1) = match seg.half {
                crate::git::graph::Half::Top => (rect.top(), mid_y),
                crate::git::graph::Half::Bottom => (mid_y, rect.bottom()),
            };
            painter.line_segment(
                [egui::pos2(x0, y0), egui::pos2(x1, y1)],
                egui::Stroke::new(2.0, colors.lane(seg.color)),
            );
        }
        let node = egui::pos2(super::graph::lane_x(rect.left(), row.graph.node, lane_width), mid_y);
        let lane = colors.lane(row.graph.color);
        if row.is_base {
            painter.circle_stroke(node, 4.5, egui::Stroke::new(2.0, lane));
        } else {
            painter.circle_filled(node, 4.5, lane);
        }
        let mut x = rect.left() + lane_width * (row.graph.width.max(1) as f32 + 1.0);
        let font = egui::FontId::proportional(13.0);
        if let Some(branch) = &row.branch {
            let galley = painter.layout_no_wrap(
                branch.clone(),
                egui::FontId::proportional(11.0),
                colors.get(Token::FgPrimary),
            );
            let chip = egui::Rect::from_min_size(
                egui::pos2(x, rect.top() + 3.0),
                egui::vec2(galley.size().x + 10.0, row_height - 6.0),
            );
            painter.rect_filled(chip, 3.0, colors.get(Token::BgHover));
            painter.galley(chip.center() - galley.size() / 2.0, galley, colors.get(Token::FgPrimary));
            x = chip.right() + 6.0;
        }
        let (text, color) = if row.is_base {
            (row.subject.clone(), colors.get(Token::FgSecondary))
        } else {
            let mut t = row.subject.clone();
            if row.melded > 0 {
                t.push_str(&format!("  ({} sq.)", row.melded + 1));
            }
            (t, colors.get(Token::FgPrimary))
        };
        let galley = painter.layout_no_wrap(text, font, color);
        let width = galley.size().x;
        painter.galley(egui::pos2(x, mid_y - galley.size().y / 2.0), galley, color);
        x += width + 8.0;
        let mut marks = Vec::new();
        if row.reworded {
            marks.push("reworded");
        }
        if row.pauses {
            marks.push("pauses to amend");
        }
        if !marks.is_empty() {
            painter.text(
                egui::pos2(x, mid_y),
                egui::Align2::LEFT_CENTER,
                marks.join(" · "),
                egui::FontId::proportional(11.0),
                colors.get(Token::Accent),
            );
        }
        if !row.hash.is_empty() {
            painter.text(
                egui::pos2(rect.right() - 4.0, mid_y),
                egui::Align2::RIGHT_CENTER,
                &row.hash,
                egui::FontId::monospace(11.0),
                colors.get(Token::FgSecondary),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paused_label_names_the_commit_and_conflicts() {
        let at = "ab12cd34567890";
        assert_eq!(paused_label(at, 0), "Rebase paused at ab12cd3");
        assert_eq!(paused_label(at, 1), "Rebase stopped at ab12cd3 · 1 conflict");
        assert_eq!(paused_label(at, 3), "Rebase stopped at ab12cd3 · 3 conflicts");
    }

    #[test]
    fn editor_keys_follow_the_keymap_preset() {
        let m = |ctrl, shift, cmd| egui::Modifiers {
            alt: false,
            ctrl,
            shift,
            mac_cmd: cmd,
            command: cmd || ctrl,
        };
        let key = |k, mods, preset| shortcuts::chord(k, mods, preset).as_ref().and_then(editor_key);
        let (up, none) = (egui::Key::ArrowUp, m(false, false, false));
        // `Mod+↑` is Ctrl+Shift+↑ on Linux and Cmd+↑ on macOS; plain Ctrl+↑ is neither.
        assert_eq!(key(up, m(true, true, false), Preset::LinuxCtrlShift), Some(EditorKey::Move(-1)));
        assert_eq!(key(up, m(false, false, true), Preset::MacOs), Some(EditorKey::Move(-1)));
        assert_eq!(key(up, m(true, false, false), Preset::LinuxCtrlShift), None);
        assert_eq!(key(egui::Key::ArrowDown, none, Preset::LinuxCtrlShift), Some(EditorKey::Focus(1)));
        assert_eq!(key(egui::Key::Escape, none, Preset::MacOs), Some(EditorKey::Cancel));
        assert_eq!(key(egui::Key::X, none, Preset::MacOs), Some(EditorKey::SetAction(Action::Drop)));
        assert_eq!(key(egui::Key::S, m(false, true, false), Preset::MacOs), None, "Shift+S is not `s`");
        assert_eq!(key(egui::Key::Q, none, Preset::MacOs), None);
    }

    #[test]
    fn drop_index_is_the_row_under_the_pointer_clamped_to_the_last() {
        let rects: Vec<egui::Rect> = (0..3)
            .map(|i| egui::Rect::from_min_size(egui::pos2(0.0, i as f32 * 10.0), egui::vec2(100.0, 10.0)))
            .collect();
        assert_eq!(drop_index(&rects, -5.0), 0);
        assert_eq!(drop_index(&rects, 15.0), 1);
        assert_eq!(drop_index(&rects, 29.0), 2);
        assert_eq!(drop_index(&rects, 500.0), 2);
        assert_eq!(drop_index(&[], 5.0), 0);
    }

    #[test]
    fn a_remote_pane_without_a_helper_says_why_start_is_disabled() {
        let mut pane = super::super::tests::new_test_pane();
        pane.location = Location::Remote { host: "gpu-box".into(), path: "~/g".into() };
        assert!(pane.rebase_helper().is_err());
        pane.set_rebase_helper(Err("no CLI for this host".into()));
        assert_eq!(pane.rebase_helper(), Err("no CLI for this host".to_string()));
        let helper = Helper { program: "/h/.amalgum/bin/1/amalgum".into() }; // portability: allow
        pane.set_rebase_helper(Ok(helper.clone()));
        assert_eq!(pane.rebase_helper(), Ok(helper));
    }
}
