//! The W9 interactive-rebase editor, headless (§5.16 steps 1–3): one row per commit from the
//! selected one to HEAD, oldest at top; reorder (drag, `Mod+↑/↓`), set an action (dropdown or
//! `p r e s f x` on the focused row), edit the subject inline for reword / squash; the problems
//! that keep **Start rebase** disabled; and the preview graph, laid out by `git::graph` over
//! synthetic commits and recomputed on every change. `ui::git_pane::rebase` only draws it.
//!
//! Also the one-shot plans the commit menu runs directly: **Edit commit message** (a reword of
//! one commit), **Squash / Fixup into parent**, and **Squash into one** for a contiguous range.

use crate::git::graph::{Layout, Row as GraphRow};
use crate::git::ops::short_hash;
use crate::git::rebase::{Action, Plan, Range, RangeCommit, Step};
use crate::model::confirm::{Confirm, ConfirmKind};

/// One commit of the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub action: Action,
    pub hash: String,
    pub subject: String,
    /// The commit's full message as it is now.
    pub original: String,
    /// The full message the user typed, for reword / squash (`None`: not edited, git's own).
    pub message: Option<String>,
}

/// Why **Start rebase** is disabled, and the row to point at (`None`: the whole plan).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub row: Option<usize>,
    pub message: String,
}

/// One commit of the preview, newest first; the last row is the base the plan applies onto.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewRow {
    pub graph: GraphRow,
    pub subject: String,
    /// The short hash of the commit it starts from (the base's, for the base row).
    pub hash: String,
    /// How many commits were squashed / fixed up into it ("Add OAuth (2 sq.)").
    pub melded: usize,
    /// The plan stops here to amend (`edit`).
    pub pauses: bool,
    /// Its message changes (reword, or a squash with typed text).
    pub reworded: bool,
    /// The branch chip on the newest row.
    pub branch: Option<String>,
    pub is_base: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEditor {
    pub branch: String,
    /// `git rebase -i <base>`; `None` for `--root`.
    pub base: Option<String>,
    /// HEAD when the range was read: `git::rebase::start` refuses a branch that moved since.
    pub head: String,
    /// How the base is named in the header and preview: a branch there, else its short hash.
    pub base_label: String,
    pub rows: Vec<Row>,
    pub focused: usize,
    /// Protected remote branches that already have commits of the plan (§5.16 warning).
    pub published: Vec<String>,
}

fn row_of(c: &RangeCommit) -> Row {
    Row {
        action: Action::Pick,
        hash: c.hash.clone(),
        subject: c.subject.clone(),
        original: c.message.clone(),
        message: None,
    }
}

/// `new` subject line in front of `message`'s body.
fn with_subject(message: &str, subject: &str) -> String {
    match message.split_once('\n') {
        Some((_, body)) => format!("{subject}\n{body}"),
        None => subject.to_string(),
    }
}

fn first_line(message: &str) -> &str {
    message.lines().next().unwrap_or_default()
}

impl PlanEditor {
    /// Every commit of `range` picked, oldest first, the first row focused.
    pub fn new(range: &Range, base_label: impl Into<String>) -> Self {
        PlanEditor {
            branch: range.branch.clone(),
            base: range.base.clone(),
            head: range.head.clone(),
            base_label: base_label.into(),
            rows: range.commits.iter().map(row_of).collect(),
            focused: 0,
            published: range.published.clone(),
        }
    }

    /// W9's header: "Rebase feat onto main · 4 commits".
    pub fn header(&self) -> String {
        let n = self.rows.len();
        format!(
            "Rebase {} onto {} · {n} commit{}",
            self.branch,
            self.base_label,
            if n == 1 { "" } else { "s" }
        )
    }

    pub fn focus(&mut self, index: usize) {
        if index < self.rows.len() {
            self.focused = index;
        }
    }

    /// `↑` / `↓` between rows.
    pub fn focus_by(&mut self, delta: isize) {
        if !self.rows.is_empty() {
            self.focused = self.focused.saturating_add_signed(delta).min(self.rows.len() - 1);
        }
    }

    /// The action dropdown, or `p r e s f x` on the focused row. Text typed as a reword is this
    /// commit's message, text typed on a squash is the whole chain's: switching between the two
    /// drops it rather than let one stand in for the other.
    pub fn set_action(&mut self, index: usize, action: Action) {
        if let Some(row) = self.rows.get_mut(index) {
            let was = row.action;
            let squash = |a: Action| a == Action::Squash;
            if was != action && (squash(was) || squash(action)) {
                row.message = None;
            }
            row.action = action;
        }
    }

    pub fn set_focused_action(&mut self, action: Action) {
        self.set_action(self.focused, action);
    }

    /// Drag row `from` to position `to` (clamped); focus follows the dragged row.
    pub fn move_row(&mut self, from: usize, to: usize) {
        if from >= self.rows.len() {
            return;
        }
        let row = self.rows.remove(from);
        let to = to.min(self.rows.len());
        self.rows.insert(to, row);
        self.focused = to;
    }

    /// `Mod+↑` / `Mod+↓`: move the focused row one place.
    pub fn move_focused(&mut self, delta: isize) {
        let to = self.focused.saturating_add_signed(delta);
        if to < self.rows.len() && to != self.focused {
            self.move_row(self.focused, to);
        }
    }

    /// The rows of the squash chain `index` is in: its head (the commit squashes meld into)
    /// through the last squash / fixup / drop after it. `None` for a row not in a chain.
    fn chain(&self, index: usize) -> Option<std::ops::Range<usize>> {
        let melds = |a: Action| matches!(a, Action::Squash | Action::Fixup | Action::Drop);
        if !matches!(self.rows.get(index)?.action, Action::Squash | Action::Fixup) {
            return None;
        }
        let head = (0..index).rev().find(|&i| !melds(self.rows[i].action))?;
        let end = (index..self.rows.len()).find(|&i| !melds(self.rows[i].action)).unwrap_or(self.rows.len());
        Some(head..end)
    }

    /// The full message a reword or squash row will get: typed text, else the commit's own (a
    /// reword) or git's combined message of the chain's head and its squashes (a squash). A
    /// reworded head contributes its new message, as git rewords it before melding.
    pub fn effective_message(&self, index: usize) -> Option<String> {
        let row = self.rows.get(index)?;
        let own = |r: &Row| match (r.action, &r.message) {
            (Action::Reword, Some(m)) => m.clone(),
            _ => r.original.clone(),
        };
        match row.action {
            Action::Reword => Some(own(row)),
            Action::Squash => {
                let chain = self.chain(index)?;
                let rows = &self.rows[chain.clone()];
                let typed = rows.iter().rev().find(|r| r.action == Action::Squash && r.message.is_some());
                Some(match typed {
                    Some(r) => r.message.clone().unwrap_or_default(),
                    None => rows
                        .iter()
                        .enumerate()
                        .filter(|(i, r)| *i == 0 || r.action == Action::Squash)
                        .map(|(i, r)| if i == 0 { own(r) } else { r.original.clone() })
                        .map(|m| m.trim_end().to_string())
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                })
            }
            _ => None,
        }
    }

    /// Whether row `index` has an inline subject field (reword / squash).
    pub fn editable(&self, index: usize) -> bool {
        self.rows.get(index).is_some_and(|r| matches!(r.action, Action::Reword | Action::Squash))
    }

    /// The inline field's text: the subject line of [`Self::effective_message`].
    pub fn inline_text(&self, index: usize) -> Option<String> {
        self.editable(index).then(|| self.effective_message(index).map(|m| first_line(&m).to_string()))?
    }

    /// Typing in the inline field: the subject line changes, the body stays. A squash chain has
    /// one message, so every squash row of the chain gets it.
    pub fn set_inline_text(&mut self, index: usize, subject: &str) {
        let Some(current) = self.effective_message(index) else { return };
        let message = with_subject(&current, subject);
        match self.rows[index].action {
            Action::Reword => self.rows[index].message = Some(message),
            Action::Squash => {
                let chain = self.chain(index).unwrap_or(index..index + 1);
                for row in &mut self.rows[chain] {
                    if row.action == Action::Squash {
                        row.message = Some(message.clone());
                    }
                }
            }
            _ => {}
        }
    }

    /// What keeps **Start rebase** disabled, first one first.
    pub fn problem(&self) -> Option<Problem> {
        if let Err(message) = self.plan().validate() {
            let row = self.rows.iter().position(|r| r.action != Action::Drop);
            return Some(Problem { row, message });
        }
        for (i, row) in self.rows.iter().enumerate() {
            let empty = self.inline_text(i).is_some_and(|t| t.trim().is_empty());
            if empty {
                let what = if row.action == Action::Reword { "Reword" } else { "Squash" };
                return Some(Problem { row: Some(i), message: format!("{what} needs a commit message") });
            }
        }
        None
    }

    /// Every commit picked, in its original order: running it would change nothing.
    pub fn unchanged(&self, original_order: &[String]) -> bool {
        self.rows.iter().all(|r| r.action == Action::Pick)
            && self.rows.iter().map(|r| &r.hash).eq(original_order.iter())
    }

    /// The plan to run. A squash chain's text sits on every squash row, the helper takes it
    /// from the last ([`Plan::messages`]); untyped messages stay `None` (git's own).
    pub fn plan(&self) -> Plan {
        let steps = self
            .rows
            .iter()
            .map(|r| Step {
                action: r.action,
                hash: r.hash.clone(),
                subject: r.subject.clone(),
                message: match r.action {
                    Action::Reword | Action::Squash => r.message.clone(),
                    _ => None,
                },
            })
            .collect();
        Plan { steps }
    }

    /// The branch as the plan would leave it, newest first, then the base (§5.16 step 3). Each
    /// kept commit is a synthetic node laid out by `git::graph`, so it looks like the graph.
    pub fn preview(&self) -> Vec<PreviewRow> {
        struct Out {
            hash: String,
            subject: String,
            melded: usize,
            pauses: bool,
            reworded: bool,
        }
        let mut commits: Vec<Out> = Vec::new();
        for (i, row) in self.rows.iter().enumerate() {
            match row.action {
                Action::Drop => {}
                Action::Squash | Action::Fixup if !commits.is_empty() => {
                    let message = if row.action == Action::Squash { self.effective_message(i) } else { None };
                    let last = commits.last_mut().expect("not empty");
                    last.melded += 1;
                    if let Some(m) = message {
                        last.reworded |= self.rows[i].message.is_some();
                        last.subject = first_line(&m).to_string();
                    }
                }
                action => {
                    let reworded =
                        action == Action::Reword && row.message.as_ref().is_some_and(|m| m != &row.original);
                    let subject = match action {
                        Action::Reword => {
                            first_line(&self.effective_message(i).unwrap_or_default()).to_string()
                        }
                        _ => row.subject.clone(),
                    };
                    commits.push(Out {
                        hash: short_hash(&row.hash).to_string(),
                        subject,
                        melded: 0,
                        pauses: action == Action::Edit,
                        reworded,
                    });
                }
            }
        }
        let mut layout = Layout::default();
        let n = commits.len();
        let mut out = Vec::with_capacity(n + 1);
        for (k, c) in commits.into_iter().rev().enumerate() {
            let id = format!("preview-{k}");
            let parent = if k + 1 < n { format!("preview-{}", k + 1) } else { "preview-base".to_string() };
            let branch = (k == 0).then(|| self.branch.clone());
            let graph = layout.push(&id, &[parent], Some(&self.branch));
            out.push(PreviewRow {
                graph,
                subject: c.subject,
                hash: c.hash,
                melded: c.melded,
                pauses: c.pauses,
                reworded: c.reworded,
                branch,
                is_base: false,
            });
        }
        let graph = layout.push("preview-base", &[], Some(&self.base_label));
        out.push(PreviewRow {
            graph,
            subject: self.base_label.clone(),
            hash: self.base.as_deref().map(short_hash).unwrap_or_default().to_string(),
            melded: 0,
            pauses: false,
            reworded: false,
            branch: None,
            is_base: true,
        });
        out
    }
}

/// §5.20 **Abort** of a stopped rebase: the branch goes back to where it started, and what was
/// amended or resolved since is lost.
pub fn abort_confirm(branch: Option<&str>, at: &str) -> Confirm {
    let title = match branch {
        Some(b) => format!("Abort the rebase of `{b}`?"),
        None => "Abort the rebase?".to_string(),
    };
    Confirm::new(ConfirmKind::AbortRebase, title).detail(format!(
        "The branch returns to where it was before the rebase. Changes amended or resolved since it \
         stopped at {} are discarded.",
        short_hash(at)
    ))
}

// ---- one-shot plans for the commit menu (§5.16 last two bullets) ---------------------------------

/// **Edit commit message**: every commit from `hash` picked, `hash` reworded to `message`.
pub fn reword_plan(range: &Range, hash: &str, message: &str) -> Plan {
    let mut plan = Plan::from_commits(&range.picks());
    for step in plan.steps.iter_mut().filter(|s| s.hash == hash) {
        step.action = Action::Reword;
        step.message = Some(message.trim_end().to_string());
    }
    plan
}

/// **Squash / Fixup into parent**: `range` starts at the parent; the commit after it melds in.
/// `action` is [`Action::Squash`] (git's combined message) or [`Action::Fixup`] (the parent's).
pub fn meld_into_parent_plan(range: &Range, hash: &str, action: Action) -> Plan {
    let mut plan = Plan::from_commits(&range.picks());
    for step in plan.steps.iter_mut().filter(|s| s.hash == hash) {
        step.action = action;
    }
    plan
}

/// **Squash into one** (§5.14): `range` starts at `oldest`; every commit after it up to
/// `newest` is squashed into it, the rest stay picked. `None` when `newest` is not in the range.
pub fn squash_range_plan(range: &Range, newest: &str) -> Option<Plan> {
    let mut plan = Plan::from_commits(&range.picks());
    let end = plan.steps.iter().position(|s| s.hash == newest)?;
    for step in &mut plan.steps[1..=end] {
        step.action = Action::Squash;
    }
    Some(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(hash: &str, message: &str) -> RangeCommit {
        RangeCommit {
            hash: hash.to_string(),
            parents: Vec::new(),
            subject: first_line(message).to_string(),
            message: message.to_string(),
        }
    }

    fn range(messages: &[&str]) -> Range {
        Range {
            branch: "feat".into(),
            head: "h".into(),
            base: Some("base0000".into()),
            commits: messages
                .iter()
                .enumerate()
                .map(|(i, m)| commit(&format!("{i}{i}{i}{i}aaaa"), m))
                .collect(),
            published: Vec::new(),
        }
    }

    fn editor(messages: &[&str]) -> PlanEditor {
        PlanEditor::new(&range(messages), "main")
    }

    fn hashes(e: &PlanEditor) -> Vec<&str> {
        e.rows.iter().map(|r| &r.hash[..1]).collect()
    }

    #[test]
    fn new_picks_everything_oldest_first_with_a_w9_header() {
        let e = editor(&["Add OAuth", "Refresh tokens"]);
        assert!(e.rows.iter().all(|r| r.action == Action::Pick));
        assert_eq!(hashes(&e), ["0", "1"]);
        assert_eq!(e.header(), "Rebase feat onto main · 2 commits");
        assert_eq!(editor(&["One"]).header(), "Rebase feat onto main · 1 commit");
        assert_eq!(e.problem(), None);
        assert!(e.unchanged(&e.rows.iter().map(|r| r.hash.clone()).collect::<Vec<_>>()));
    }

    #[test]
    fn keys_set_the_focused_rows_action() {
        let mut e = editor(&["a", "b", "c"]);
        e.focus_by(1);
        e.set_focused_action(Action::from_key('x').expect("x is drop"));
        e.focus_by(5); // clamps to the last row
        e.set_focused_action(Action::from_key('f').expect("f is fixup"));
        e.focus_by(-9);
        assert_eq!(e.focused, 0);
        let actions: Vec<Action> = e.rows.iter().map(|r| r.action).collect();
        assert_eq!(actions, [Action::Pick, Action::Drop, Action::Fixup]);
    }

    #[test]
    fn move_focused_moves_the_row_and_focus_follows() {
        let mut e = editor(&["a", "b", "c"]);
        e.move_focused(1); // Mod+↓ on the first row
        assert_eq!(hashes(&e), ["1", "0", "2"]);
        assert_eq!(e.focused, 1);
        e.move_focused(-1);
        e.move_focused(-1); // already at the top: no-op
        assert_eq!(hashes(&e), ["0", "1", "2"]);
        assert_eq!(e.focused, 0);
        e.move_row(0, 99); // drag past the end
        assert_eq!(hashes(&e), ["1", "2", "0"]);
        assert_eq!(e.focused, 2);
    }

    #[test]
    fn first_row_cannot_be_squash_or_fixup() {
        let mut e = editor(&["a", "b"]);
        e.set_action(0, Action::Squash);
        let p = e.problem().expect("problem");
        assert_eq!(p.row, Some(0));
        assert!(p.message.contains("first commit"), "{}", p.message);
        e.set_action(0, Action::Drop);
        e.set_action(1, Action::Fixup);
        assert_eq!(e.problem().and_then(|p| p.row), Some(1), "the first kept row counts");
        e.set_action(1, Action::Drop);
        assert_eq!(e.problem().map(|p| p.message), Some("Every commit is dropped".to_string()));
    }

    #[test]
    fn reword_needs_a_message() {
        let mut e = editor(&["Handle 401\n\nBody text"]);
        e.set_action(0, Action::Reword);
        assert_eq!(e.inline_text(0).as_deref(), Some("Handle 401"), "prefilled with the subject");
        assert_eq!(e.problem(), None, "keeping the message is fine");
        e.set_inline_text(0, "  ");
        assert_eq!(
            e.problem(),
            Some(Problem { row: Some(0), message: "Reword needs a commit message".into() })
        );
        e.set_inline_text(0, "Handle 401 on token refresh");
        assert_eq!(e.problem(), None);
        assert_eq!(
            e.plan().steps[0].message.as_deref(),
            Some("Handle 401 on token refresh\n\nBody text"),
            "the body is kept"
        );
    }

    #[test]
    fn squash_text_is_one_message_per_chain() {
        let mut e = editor(&["Refresh tokens", "Add OAuth", "Tweak", "Other"]);
        e.set_action(1, Action::Squash);
        e.set_action(2, Action::Squash);
        assert_eq!(e.effective_message(1).as_deref(), Some("Refresh tokens\n\nAdd OAuth\n\nTweak"));
        e.set_inline_text(2, "Add OAuth");
        assert_eq!(e.inline_text(1).as_deref(), Some("Add OAuth"), "every squash row shows the chain's text");
        let plan = e.plan();
        assert_eq!(plan.messages(), vec![Some("Add OAuth\n\nAdd OAuth\n\nTweak".to_string())]);
        assert_eq!(plan.steps[3].message, None, "picks carry no message");
    }

    #[test]
    fn a_squash_chain_starts_from_its_heads_reworded_message() {
        let mut e = editor(&["Old head\n\nhead body", "Squashed"]);
        e.set_action(0, Action::Reword);
        e.set_inline_text(0, "New head");
        e.set_action(1, Action::Squash);
        assert_eq!(e.effective_message(1).as_deref(), Some("New head\n\nhead body\n\nSquashed"));
        assert_eq!(e.preview()[0].subject, "New head");
        e.set_inline_text(1, "Combined");
        assert_eq!(
            e.plan().messages(),
            vec![
                Some("New head\n\nhead body".to_string()),
                Some("Combined\n\nhead body\n\nSquashed".to_string())
            ]
        );
    }

    #[test]
    fn switching_between_reword_and_squash_drops_the_typed_text() {
        let mut e = editor(&["Head", "Mine"]);
        e.set_action(1, Action::Reword);
        e.set_inline_text(1, "Mine, reworded");
        e.set_action(1, Action::Squash);
        assert_eq!(e.effective_message(1).as_deref(), Some("Head\n\nMine"), "the chain's default");
        e.set_inline_text(1, "Both");
        e.set_action(1, Action::Reword);
        assert_eq!(e.effective_message(1).as_deref(), Some("Mine"), "its own message again");
    }

    #[test]
    fn plan_only_carries_messages_for_reword_and_squash() {
        let mut e = editor(&["a", "b"]);
        e.set_action(1, Action::Reword);
        e.set_inline_text(1, "B!");
        e.set_action(1, Action::Edit); // typed text is kept on the row but not used
        assert_eq!(e.plan().steps[1].message, None);
        e.set_action(1, Action::Reword);
        assert_eq!(e.plan().steps[1].message.as_deref(), Some("B!"));
    }

    #[test]
    fn preview_melds_drops_and_marks_the_plan_newest_first() {
        // W9: ef56 pick, cd34 squash "Add OAuth", 5e6f reword, 7a8b drop.
        let mut e = editor(&["Refresh tokens", "Add OAuth", "Handle 401", "Add retry"]);
        e.set_action(1, Action::Squash);
        e.set_inline_text(1, "Add OAuth");
        e.set_action(2, Action::Reword);
        e.set_inline_text(2, "Handle 401 on token refresh");
        e.set_action(3, Action::Drop);
        let p = e.preview();
        let subjects: Vec<&str> = p.iter().map(|r| r.subject.as_str()).collect();
        assert_eq!(subjects, ["Handle 401 on token refresh", "Add OAuth", "main"]);
        assert_eq!(p[0].branch.as_deref(), Some("feat"));
        assert!(p[0].reworded);
        assert_eq!(p[1].melded, 1);
        assert_eq!(p[1].hash, "0000aaa", "a squashed commit keeps its head's hash label");
        assert!(p[2].is_base);
        assert_eq!(p[2].hash, "base000");
        // One lane: every node on column 0, linked top to bottom.
        assert!(p.iter().all(|r| r.graph.node == 0), "{p:?}");
    }

    #[test]
    fn preview_marks_edit_pauses_and_follows_reorders() {
        let mut e = editor(&["a", "b"]);
        e.move_row(1, 0);
        e.set_action(1, Action::Edit);
        let p = e.preview();
        let subjects: Vec<&str> = p.iter().map(|r| r.subject.as_str()).collect();
        assert_eq!(subjects, ["a", "b", "main"]);
        assert!(p[0].pauses);
        assert!(!p[1].pauses);
    }

    #[test]
    fn preview_of_everything_dropped_is_just_the_base() {
        let mut e = editor(&["a"]);
        e.set_action(0, Action::Drop);
        let p = e.preview();
        assert_eq!(p.len(), 1);
        assert!(p[0].is_base);
    }

    #[test]
    fn unchanged_detects_a_plan_that_would_do_nothing() {
        let mut e = editor(&["a", "b"]);
        let order: Vec<String> = e.rows.iter().map(|r| r.hash.clone()).collect();
        assert!(e.unchanged(&order));
        e.move_row(1, 0);
        assert!(!e.unchanged(&order));
        e.move_row(1, 0);
        e.set_action(0, Action::Edit);
        assert!(!e.unchanged(&order));
    }

    #[test]
    fn abort_confirms_as_a_destructive_abort_rebase() {
        let c = abort_confirm(Some("feat"), "ab12cd34567");
        assert_eq!(c.kind, ConfirmKind::AbortRebase);
        assert_eq!(c.title, "Abort the rebase of `feat`?");
        assert_eq!(c.verb, "Abort");
        assert!(c.detail.contains("ab12cd3"), "{}", c.detail);
        assert!(!c.kind.allows_dont_ask());
    }

    #[test]
    fn one_shot_plans_for_the_commit_menu() {
        let r = range(&["Parent", "Child", "Later"]);
        let child = r.commits[1].hash.clone();

        let reword = reword_plan(&r, &child, "Better child\n\n");
        assert_eq!(reword.steps[1].action, Action::Reword);
        assert_eq!(reword.steps[1].message.as_deref(), Some("Better child"));
        assert!(reword.steps.iter().filter(|s| s.hash != child).all(|s| s.action == Action::Pick));

        let fixup = meld_into_parent_plan(&r, &child, Action::Fixup);
        let actions: Vec<Action> = fixup.steps.iter().map(|s| s.action).collect();
        assert_eq!(actions, [Action::Pick, Action::Fixup, Action::Pick]);
        assert_eq!(fixup.validate(), Ok(()));

        let one = squash_range_plan(&r, &child).expect("in range");
        let actions: Vec<Action> = one.steps.iter().map(|s| s.action).collect();
        assert_eq!(actions, [Action::Pick, Action::Squash, Action::Pick]);
        assert_eq!(squash_range_plan(&r, "nope"), None);
    }
}
