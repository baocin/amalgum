//! Interactive rebase without a terminal editor (§5.16). The UI builds a [`Plan`]; [`start`]
//! runs `git rebase -i <base>` with `GIT_SEQUENCE_EDITOR="<exe> --sequence-editor"` and
//! `GIT_EDITOR="<exe> --editor"`, and env `AMALGUM_REBASE_PLAN` / `AMALGUM_REBASE_MSGS` naming
//! JSON files in a per-run directory inside the repository's git dir ([`RunFiles`]). The
//! helpers run in the CLI, on the machine git runs on (a remote host too, §5.28):
//! `--sequence-editor <todo>` overwrites git's todo file with [`todo_text`]; `--editor <msgfile>`
//! looks up which todo step git is committing (the last line of `rebase-merge/done` beside the
//! message file) and, when the plan has text for that step ([`Plan::keyed_messages`]),
//! overwrites the message file with it. Every other prompt (a picked commit after a conflict, an
//! amend at an `edit` stop, a reword or squash chain without text) keeps git's own message.
//! Keying by step instead of counting prompts keeps messages on their commits when git opens
//! the editor for extra commits, as it does when continuing after a conflict.
//!
//! Running: [`read_range`] checks §5.16's preconditions and lists the commits from the selected
//! one to HEAD; [`start`] writes the run files (through git itself, so it works over ssh),
//! journals nothing yet, and runs git; [`resume`] (`rebase --continue`) and [`abort`] follow a
//! stop. Each returns a [`Stop`]: finished (with the journal entry: undo resets to the
//! pre-rebase hash), paused at an `edit`, or stopped on conflicts (§5.17).

use super::journal::Entry;
use super::ops::{self, OpError, Pending, PlanError};
use super::status::{self, RepoOp, STATUS_ARGS};
use super::{Git, GitError};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Pick,
    Reword,
    Edit,
    Squash,
    Fixup,
    Drop,
}

impl Action {
    /// Every action, in W9's dropdown order.
    pub const ALL: [Action; 6] =
        [Action::Pick, Action::Reword, Action::Edit, Action::Squash, Action::Fixup, Action::Drop];

    /// Single-key shortcuts from W9: p r e s f x.
    pub fn from_key(c: char) -> Option<Self> {
        match c.to_ascii_lowercase() {
            'p' => Some(Action::Pick),
            'r' => Some(Action::Reword),
            'e' => Some(Action::Edit),
            's' => Some(Action::Squash),
            'f' => Some(Action::Fixup),
            'x' => Some(Action::Drop),
            _ => None,
        }
    }
    /// git's todo-list keyword for this action, also its label in W9's dropdown.
    pub fn keyword(self) -> &'static str {
        match self {
            Action::Pick => "pick",
            Action::Reword => "reword",
            Action::Edit => "edit",
            Action::Squash => "squash",
            Action::Fixup => "fixup",
            Action::Drop => "drop",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub action: Action,
    pub hash: String,
    pub subject: String,
    /// New message for `reword` / combined message for `squash`.
    pub message: Option<String>,
}

/// Oldest commit first, as git's todo list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub steps: Vec<Step>,
}

impl Plan {
    /// `pick` for every commit, oldest first. `commits` is `(hash, subject)`, oldest first.
    pub fn from_commits(commits: &[(String, String)]) -> Self {
        let steps = commits
            .iter()
            .map(|(hash, subject)| Step {
                action: Action::Pick,
                hash: hash.clone(),
                subject: subject.clone(),
                message: None,
            })
            .collect();
        Plan { steps }
    }
    /// Move step `from` to index `to` (drag or `Mod+↑/↓`). `to` is clamped to the last valid
    /// position; out-of-range `from` is a no-op.
    pub fn move_step(&mut self, from: usize, to: usize) {
        if from >= self.steps.len() {
            return;
        }
        let step = self.steps.remove(from);
        let to = to.min(self.steps.len());
        self.steps.insert(to, step);
    }
    /// Plan problems shown before Start: first non-dropped step is squash/fixup (nothing to
    /// meld into), or every step dropped.
    pub fn validate(&self) -> Result<(), String> {
        match self.steps.iter().find(|s| s.action != Action::Drop) {
            None => Err("Every commit is dropped".to_string()),
            Some(first) if matches!(first.action, Action::Squash | Action::Fixup) => {
                Err("The first commit can't be squash or fixup: there is nothing before it to meld into"
                    .to_string())
            }
            Some(_) => Ok(()),
        }
    }
    /// One slot per editor prompt git opens for the plan itself, in the order it opens them:
    /// the text the plan has for it, or `None` to keep git's pre-filled message (the commit's
    /// own message for a `reword`, git's combined message for a squash chain).
    ///
    /// `reword` always prompts once, for its own commit. `squash`/`fixup` steps meld into
    /// whatever commit precedes them; git prompts once per maximal run of squash/fixup steps
    /// (a `drop` inside the run doesn't end it: git skips it as a no-op), but only when that run
    /// contains at least one `squash` (a run of pure `fixup`s is silent, keeping the preceding
    /// message unedited). The prompt's slot takes the message of the *last* `squash` step in the
    /// run that has one — which is where this plan's UI stores the user's final, already-combined
    /// text.
    pub fn messages(&self) -> Vec<Option<String>> {
        self.prompts().into_iter().map(|(_, message)| message).collect()
    }

    /// [`messages`](Self::messages) keyed by the step git is committing when it opens the
    /// editor — the reword's own hash, or a squash chain's *final* squash/fixup step (git opens
    /// the editor for the combined message there) — for prompts that have text. What the
    /// `--editor` helper looks up.
    pub fn keyed_messages(&self) -> Vec<(String, String)> {
        self.prompts().into_iter().filter_map(|(hash, message)| Some((hash.to_string(), message?))).collect()
    }

    /// `(step hash, text)` per prompt, in order (see [`messages`](Self::messages)).
    fn prompts(&self) -> Vec<(&str, Option<String>)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.steps.len() {
            match self.steps[i].action {
                Action::Reword => {
                    out.push((self.steps[i].hash.as_str(), self.steps[i].message.clone()));
                    i += 1;
                }
                Action::Squash | Action::Fixup => {
                    let mut has_squash = false;
                    let mut last_squash_message = None;
                    let mut final_step = i;
                    while i < self.steps.len()
                        && matches!(self.steps[i].action, Action::Squash | Action::Fixup | Action::Drop)
                    {
                        let step = &self.steps[i];
                        if step.action == Action::Squash {
                            has_squash = true;
                            last_squash_message = step.message.clone().or(last_squash_message);
                        }
                        if step.action != Action::Drop {
                            final_step = i;
                        }
                        i += 1;
                    }
                    if has_squash {
                        out.push((self.steps[final_step].hash.as_str(), last_squash_message));
                    }
                }
                Action::Pick | Action::Edit | Action::Drop => i += 1,
            }
        }
        out
    }
}

/// git's todo format: `<action> <hash> <subject>` per line.
pub fn todo_text(plan: &Plan) -> String {
    let mut text = String::new();
    for step in &plan.steps {
        text.push_str(step.action.keyword());
        text.push(' ');
        text.push_str(&step.hash);
        text.push(' ');
        text.push_str(&step.subject);
        text.push('\n');
    }
    text
}

/// The contents of `plan.json` and `messages.json` ([`Plan::keyed_messages`]).
pub fn plan_file_contents(plan: &Plan) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let plan_json = serde_json::to_vec_pretty(plan).map_err(io::Error::other)?;
    let messages_json = serde_json::to_vec_pretty(&plan.keyed_messages()).map_err(io::Error::other)?;
    Ok((plan_json, messages_json))
}

/// Write `plan.json` and `messages.json` into `dir` on this machine; returns their paths.
pub fn write_plan_files(plan: &Plan, dir: &Path) -> io::Result<(std::path::PathBuf, std::path::PathBuf)> {
    std::fs::create_dir_all(dir)?;
    let plan_path = dir.join("plan.json");
    let messages_path = dir.join("messages.json");
    let (plan_json, messages_json) = plan_file_contents(plan)?;
    std::fs::write(&plan_path, plan_json)?;
    std::fs::write(&messages_path, messages_json)?;
    Ok((plan_path, messages_path))
}

/// `--sequence-editor` helper body: replace git's todo file with this plan's [`todo_text`].
///
/// Refuses (an error, so git aborts the rebase before it changes anything) when git's todo has
/// a commit the plan doesn't: the branch moved since the plan was read (an agent committed), and
/// git would silently drop every commit the todo leaves out (`rebase.missingCommitsCheck`
/// defaults to `ignore`).
pub fn apply_sequence_editor(plan_file: &Path, todo_file: &Path) -> io::Result<()> {
    let plan: Plan = serde_json::from_slice(&std::fs::read(plan_file)?).map_err(io::Error::other)?;
    let generated = match std::fs::read_to_string(todo_file) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let missing = missing_from_plan(&plan, &generated);
    if !missing.is_empty() {
        return Err(io::Error::other(format!(
            "the branch has commits the rebase plan doesn't list ({}): it moved since the plan was \
             made, so nothing was rewritten. Reopen the rebase to include them.",
            missing.join(", ")
        )));
    }
    std::fs::write(todo_file, todo_text(&plan))
}

/// The commits of git's generated `todo` (its `pick`-style lines) that `plan` has no step for.
/// Comments, `exec`, `label`, `update-ref` and the like name no commit and are ignored.
pub fn missing_from_plan<'a>(plan: &Plan, todo: &'a str) -> Vec<&'a str> {
    const COMMIT_COMMANDS: &[&str] =
        &["pick", "p", "reword", "r", "edit", "e", "squash", "s", "fixup", "f", "drop", "d"];
    todo.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            if !COMMIT_COMMANDS.contains(&words.next()?) {
                return None;
            }
            let hash = words.find(|w| !w.starts_with('-'))?;
            hash.bytes().all(|b| b.is_ascii_hexdigit()).then_some(hash)
        })
        .filter(|hash| {
            !plan.steps.iter().any(|s| s.hash.starts_with(hash) || hash.starts_with(s.hash.as_str()))
        })
        .collect()
}

/// A `core.commentChar` for a run of `plan`, when git's default `#` would strip lines of the
/// plan's own messages (a Markdown heading in a body): git cleans every message the editor
/// hands back by deleting lines that start with the comment character. `None`: `#` is safe.
pub fn comment_char(plan: &Plan) -> Option<char> {
    let starts = |c: char| {
        plan.steps.iter().filter_map(|s| s.message.as_deref()).flat_map(str::lines).any(|l| l.starts_with(c))
    };
    if !starts('#') {
        return None;
    }
    [';', '%', '!', '@', '|', '~', '&', '^', '='].into_iter().find(|&c| !starts(c))
}

/// `--editor` helper body: when git is committing a todo step that `messages_file`
/// ([`Plan::keyed_messages`]) has text for, write that text over git's message file. The step
/// is the last line of `rebase-merge/done` in the git dir holding `message_file` (git's
/// `COMMIT_EDITMSG`). Outside a rebase, or for a step without text, the file is left untouched
/// and git keeps its own message.
pub fn apply_editor(messages_file: &Path, message_file: &Path) -> io::Result<()> {
    let messages: Vec<(String, String)> =
        serde_json::from_slice(&std::fs::read(messages_file)?).map_err(io::Error::other)?;
    let done = match message_file.parent() {
        Some(git_dir) => git_dir.join("rebase-merge").join("done"),
        None => return Ok(()),
    };
    let done = match std::fs::read_to_string(done) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if let Some(message) = current_step(&done).and_then(|hash| message_for(&messages, hash)) {
        std::fs::write(message_file, format!("{message}\n"))?;
    }
    Ok(())
}

/// The commit of the step git is on: the hash on the last command line of `rebase-merge/done`
/// (`fixup -C <hash>` style options skipped). `None` for a command without one (`exec`, `break`).
pub fn current_step(done: &str) -> Option<&str> {
    let line = done.lines().map(str::trim).rfind(|l| !l.is_empty() && !l.starts_with('#'))?;
    let mut words = line.split_whitespace().skip(1).skip_while(|w| w.starts_with('-'));
    words.next().filter(|w| w.len() >= 4 && w.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The text for `hash` (either may be abbreviated: git rewrites the todo list with short ids).
fn message_for<'a>(messages: &'a [(String, String)], hash: &str) -> Option<&'a str> {
    messages
        .iter()
        .find(|(key, _)| key.starts_with(hash) || hash.starts_with(key.as_str()))
        .map(|(_, message)| message.as_str())
}

// ---- running a plan (§5.16 step 4) ---------------------------------------------------------------

/// The `amalgum` binary on the machine git runs on, which git starts as its editors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Helper {
    pub program: String,
}

impl Helper {
    /// The remote CLI the ssh session installed: `<home>/.amalgum/bin/<version>/amalgum`
    /// (absolute: git starts it through a shell whose `~` would stay quoted). A host whose CLI
    /// could not be installed has none, with the reason as the error.
    pub fn remote(cli: &crate::ssh::steps::CliStatus, home: &str, version: &str) -> Result<Helper, String> {
        use crate::ssh::steps::CliStatus;
        match cli {
            CliStatus::Present | CliStatus::Installed => Ok(Helper {
                program: format!(
                    "{}/{}/bin/{version}/amalgum",
                    home.trim_end_matches('/'),
                    crate::paths::REMOTE_ROOT
                ),
            }),
            CliStatus::Unavailable { reason } => {
                Err(format!("Interactive rebase needs the amalgum CLI on this host: {reason}"))
            }
        }
    }

    /// `GIT_SEQUENCE_EDITOR`, `GIT_EDITOR`, and the helper's own two variables for `files`. The
    /// program path is shell-quoted: git runs editors through `sh -c`.
    pub fn env(&self, files: &RunFiles) -> Vec<(&'static str, String)> {
        let program = crate::ssh::quote_literal(&self.program);
        vec![
            ("GIT_SEQUENCE_EDITOR", format!("{program} --sequence-editor")),
            ("GIT_EDITOR", format!("{program} --editor")),
            ("AMALGUM_REBASE_PLAN", files.plan.clone()),
            ("AMALGUM_REBASE_MSGS", files.messages.clone()),
        ]
    }
}

/// `git rev-parse --absolute-git-dir`: where a run's files go.
pub const GIT_DIR_ARGS: &[&str] = &["rev-parse", "--absolute-git-dir"];

/// Writes stdin to the path given as the next argument (creating its directory), on whichever
/// machine git runs: a shell alias, so the path and the content stay arguments and data.
pub const PUT_FILE_ARGS: &[&str] =
    &["-c", "alias.amalgum-put=!f() { mkdir -p \"${1%/*}\" && cat >\"$1\"; }; f", "amalgum-put"];

/// Removes the directory given as the next argument, wherever git runs (a run's files), and
/// its parent ([`RUNS_DIR`]) once no other run's files are left in it.
pub const REMOVE_DIR_ARGS: &[&str] = &[
    "-c",
    "alias.amalgum-rm=!f() { rm -rf -- \"$1\" && { rmdir -- \"${1%/*}\" 2>/dev/null; true; }; }; f",
    "amalgum-rm",
];

/// The directory under the git dir that holds every run's files.
pub const RUNS_DIR: &str = "amalgum-rebase";

/// One run's files, as paths on the machine git runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunFiles {
    pub dir: String,
    pub plan: String,
    pub messages: String,
}

impl RunFiles {
    /// `<git_dir>/amalgum-rebase/<id>/{plan,messages}.json`.
    pub fn new(git_dir: &str, id: &str) -> Self {
        let dir = format!("{}/{RUNS_DIR}/{id}", git_dir.trim_end_matches('/'));
        RunFiles { plan: format!("{dir}/plan.json"), messages: format!("{dir}/messages.json"), dir }
    }
}

/// A commit an interactive rebase rewrites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeCommit {
    pub hash: String,
    pub parents: Vec<String>,
    pub subject: String,
    /// The full message, as `%B` prints it (trailing newline trimmed).
    pub message: String,
}

/// What an interactive rebase from a commit would rewrite (§5.16 "Precondition").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Range {
    pub branch: String,
    pub head: String,
    /// The parent of the oldest commit (`git rebase -i <base>`); `None` when the oldest is a
    /// root commit (`--root`).
    pub base: Option<String>,
    /// Oldest first.
    pub commits: Vec<RangeCommit>,
    /// Protected remote branches (`origin/main`) that already contain commits of the range:
    /// the §5.16 warning.
    pub published: Vec<String>,
}

impl Range {
    /// `(hash, subject)` of every commit, oldest first, for [`Plan::from_commits`].
    pub fn picks(&self) -> Vec<(String, String)> {
        self.commits.iter().map(|c| (c.hash.clone(), c.subject.clone())).collect()
    }
}

/// Why an interactive rebase can't start from a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeError {
    Detached,
    NoCommits,
    /// The commit is not in HEAD's history.
    NotAncestor {
        commit: String,
        branch: String,
    },
    /// The range has merge commits, which `rebase -i` would flatten.
    Merges {
        count: usize,
    },
    Git(GitError),
}

impl std::fmt::Display for RangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RangeError::Detached => f.write_str("HEAD is detached: check out the branch to rewrite first"),
            RangeError::NoCommits => f.write_str("There are no commits to rebase"),
            RangeError::NotAncestor { commit, branch } => {
                write!(f, "{} is not in the history of `{branch}`", ops::short_hash(commit))
            }
            RangeError::Merges { count: 1 } => {
                f.write_str("The range contains a merge commit, which an interactive rebase would flatten")
            }
            RangeError::Merges { count } => write!(
                f,
                "The range contains {count} merge commits, which an interactive rebase would flatten"
            ),
            RangeError::Git(e) => e.fmt(f),
        }
    }
}

impl From<GitError> for RangeError {
    fn from(e: GitError) -> Self {
        RangeError::Git(e)
    }
}

/// `git log` of the commits after `base` up to HEAD (all of HEAD's history for `None`), oldest
/// first: `%H %P %B`, `%x1f`-separated, NUL-terminated.
pub fn range_log_args(base: Option<&str>) -> Vec<String> {
    let rev = base.map_or_else(|| "HEAD".to_string(), |b| format!("{b}..HEAD"));
    ["log", "--reverse", "--topo-order", "-z", "--no-show-signature", "--no-color", "--format=%H%x1f%P%x1f%B"]
        .iter()
        .map(|s| s.to_string())
        .chain([rev, "--".to_string()])
        .collect()
}

/// Parses [`range_log_args`] output.
pub fn parse_range_log(out: &[u8]) -> Vec<RangeCommit> {
    String::from_utf8_lossy(out)
        .split('\0')
        .filter(|r| !r.trim().is_empty())
        .filter_map(|record| {
            let mut fields = record.trim_start_matches('\n').splitn(3, '\u{1f}');
            let hash = fields.next()?.to_string();
            let parents = fields.next()?.split_whitespace().map(str::to_string).collect();
            let message = fields.next().unwrap_or_default().trim_end().to_string();
            let subject = message.lines().next().unwrap_or_default().to_string();
            Some(RangeCommit { hash, parents, subject, message })
        })
        .collect()
}

/// Remote-tracking branches containing a commit: `refs/remotes/origin/main` per line.
pub fn published_args(commit: &str) -> Vec<String> {
    ["for-each-ref", "--format=%(refname)", "--contains", commit, "refs/remotes/"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// The [`published_args`] branches whose name (without the remote) `is_protected` accepts, as
/// `origin/main`. `remotes` are the remote names, longest match first (a name may contain `/`).
pub fn parse_published(out: &[u8], remotes: &[String], is_protected: impl Fn(&str) -> bool) -> Vec<String> {
    String::from_utf8_lossy(out)
        .lines()
        .filter_map(|l| l.trim().strip_prefix("refs/remotes/"))
        .filter(|short| !short.ends_with("/HEAD"))
        .filter(|short| {
            let remote = remotes
                .iter()
                .filter(|r| short.len() > r.len() + 1 && short.starts_with(&format!("{r}/")))
                .max_by_key(|r| r.len());
            let branch = match remote {
                Some(r) => Some(&short[r.len() + 1..]),
                None => short.split_once('/').map(|(_, b)| b),
            };
            branch.is_some_and(&is_protected)
        })
        .map(str::to_string)
        .collect()
}

/// §5.16 precondition checks for an interactive rebase from `commit`, and the commits it
/// rewrites: HEAD must be on a branch whose history contains `commit`, without merges from there
/// on. `remotes` and `is_protected` find the protected remote branches that already have
/// commits of the range ([`Range::published`], a warning, not a refusal).
pub fn read_range(
    git: &Git,
    commit: &str,
    remotes: &[String],
    is_protected: impl Fn(&str) -> bool,
) -> Result<Range, RangeError> {
    let (head, branch) = ops::head_and_branch(git);
    let head = head.ok_or(RangeError::NoCommits)?;
    let branch = branch.ok_or(RangeError::Detached)?;
    match git.run(&["merge-base", "--is-ancestor", commit, "HEAD"]) {
        Ok(_) => {}
        Err(e) if e.code == Some(1) => {
            return Err(RangeError::NotAncestor { commit: commit.to_string(), branch });
        }
        Err(e) => return Err(e.into()),
    }
    let base = git.run(&["rev-parse", "--verify", "-q", &format!("{commit}^")]).ok().map(text);
    let args = range_log_args(base.as_deref());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let commits = parse_range_log(&git.run(&args)?);
    let merges = commits.iter().filter(|c| c.parents.len() > 1).count();
    if merges > 0 {
        return Err(RangeError::Merges { count: merges });
    }
    let oldest = commits.first().ok_or(RangeError::NoCommits)?.hash.clone();
    let args = published_args(&oldest);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let published = git.run(&args).map(|o| parse_published(&o, remotes, is_protected)).unwrap_or_default();
    Ok(Range { branch, head, base, commits, published })
}

/// A started rebase: what [`resume`] and [`abort`] need, and the journal entry in waiting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub helper: Helper,
    pub files: RunFiles,
    /// `-c` options every git run of this rebase gets ([`comment_char`]).
    pub config: Vec<String>,
    /// Journaled by [`Pending::finish`] once git is done (undo: `reset --hard` to the
    /// pre-rebase hash).
    pub pending: Pending,
}

/// Where a run of git left the rebase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// Finished. `None`: nothing changed (a plan that keeps every commit as it was).
    Done(Option<Box<Entry>>),
    /// Stopped at an `edit` step (or on another stop without conflicts): `at` is the commit
    /// (`REBASE_HEAD`, else HEAD). `error` is git's, when it stopped because a step failed
    /// rather than as planned: `--continue` over unstaged changes, a failing editor, an
    /// untracked file in the way (§5.24: shown, never swallowed).
    Paused { at: String, error: Option<GitError> },
    /// Stopped on conflicts (§5.17).
    Conflicts { at: String, error: GitError },
}

fn text(out: Vec<u8>) -> String {
    String::from_utf8_lossy(&out).trim().to_string()
}

/// Writes `plan`'s files for a run `id` (inside the git dir, wherever git runs).
pub fn write_run_files(git: &Git, id: &str, plan: &Plan) -> Result<RunFiles, OpError> {
    let git_dir = text(git.run(GIT_DIR_ARGS)?);
    let files = RunFiles::new(&git_dir, id);
    let (plan_json, messages_json) =
        plan_file_contents(plan).map_err(|e| OpError::Plan(PlanError(e.to_string())))?;
    for (path, content) in [(&files.plan, plan_json), (&files.messages, messages_json)] {
        let mut args = PUT_FILE_ARGS.to_vec();
        args.push(path);
        git.run_with_stdin(&args, &content)?;
    }
    Ok(files)
}

/// Deletes a run's files (best effort: a leftover directory in the git dir is harmless).
pub fn remove_run_files(git: &Git, files: &RunFiles) {
    if files.dir.contains(&format!("/{RUNS_DIR}/")) {
        let mut args = REMOVE_DIR_ARGS.to_vec();
        args.push(&files.dir);
        let _ = git.run(&args);
    }
}

/// §5.16 step 4: runs `git rebase -i` (`--root` when `base` is `None`) with `plan` through the
/// helpers. `expected_head` is the HEAD the plan was read at ([`Range::head`]): a branch that
/// moved since (an agent committed) is refused, because the plan doesn't list the new commits and
/// git would drop them. `description` names the journal entry ("reword ab12cd3"); `id` names the
/// run's directory. Validation failures, another operation in progress, and git refusing to
/// start (a dirty tree) are errors; stops are [`Stop`]s, with the [`Session`] to continue or
/// abort them.
#[allow(clippy::too_many_arguments)]
pub fn start(
    git: &Git,
    helper: &Helper,
    base: Option<&str>,
    expected_head: &str,
    plan: &Plan,
    description: &str,
    id: &str,
    now: u64,
) -> Result<(Session, Stop), OpError> {
    plan.validate().map_err(|e| OpError::Plan(PlanError(e)))?;
    if let Some(base) = base
        && base.starts_with('-')
    {
        return Err(OpError::Plan(PlanError(format!("Invalid revision `{base}`"))));
    }
    if let Some(op) = ops::in_progress(git) {
        return Err(OpError::Plan(PlanError(format!(
            "A {} is already in progress: continue or abort it first",
            op.name().to_lowercase()
        ))));
    }
    let (_, branch) = ops::head_and_branch(git);
    let refs: Vec<String> = branch.iter().map(|b| format!("refs/heads/{b}")).collect();
    let before = ops::read_snapshot(git, &refs, false);
    let head = before.head.clone().ok_or_else(|| OpError::Plan(PlanError("No commits yet".into())))?;
    if head != expected_head {
        return Err(OpError::Plan(PlanError(format!(
            "The branch moved to {} since the plan was made (at {}): reopen it to include the new commits",
            ops::short_hash(&head),
            ops::short_hash(expected_head)
        ))));
    }
    let files = write_run_files(git, id, plan)?;
    let pending = Pending {
        description: description.to_string(),
        before,
        inverse: Some(vec![vec!["reset".to_string(), "--hard".to_string(), head]]),
        refs,
    };
    let config = match comment_char(plan) {
        Some(c) => vec!["-c".to_string(), format!("core.commentChar={c}")],
        None => Vec::new(),
    };
    let session = Session { helper: helper.clone(), files, config, pending };
    let mut args = vec!["rebase", "-i"];
    args.push(base.unwrap_or("--root"));
    let result = run_with_helpers(git, &session, &args);
    let stop = settle(git, &session, result, now)?;
    Ok((session, stop))
}

/// **Continue** after a stop: `git rebase --continue` with the same helpers, so later reword
/// and squash steps still get their messages.
pub fn resume(git: &Git, session: &Session, now: u64) -> Result<Stop, OpError> {
    let result = run_with_helpers(git, session, &["rebase", "--continue"]);
    settle(git, session, result, now)
}

/// **Abort** (after the §5.20 confirmation): `git rebase --abort`, then the run's files go.
pub fn abort(git: &Git, session: &Session) -> Result<(), GitError> {
    git.run(&["rebase", "--abort"])?;
    remove_run_files(git, &session.files);
    Ok(())
}

/// What the rebase is now — finished (journaled, files removed; `Done(None)` when it was
/// aborted), paused, or conflicted — without running git's rebase: for polling a stopped
/// rebase that may have been continued or aborted elsewhere (a terminal, an agent).
pub fn probe(git: &Git, session: &Session, now: u64) -> Result<Stop, OpError> {
    settle(git, session, Ok(Vec::new()), now)
}

fn run_with_helpers(git: &Git, session: &Session, args: &[&str]) -> Result<Vec<u8>, GitError> {
    let env = session.helper.env(&session.files);
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let args: Vec<&str> = session.config.iter().map(String::as_str).chain(args.iter().copied()).collect();
    git.run_with_env(&args, &env)
}

/// Classifies the repository after git returned `result`.
fn settle(
    git: &Git,
    session: &Session,
    result: Result<Vec<u8>, GitError>,
    now: u64,
) -> Result<Stop, OpError> {
    match ops::in_progress(git) {
        Some(RepoOp::Rebase) => {
            let at = git
                .run(&["rev-parse", "--verify", "-q", "REBASE_HEAD"])
                .or_else(|_| git.run(&["rev-parse", "HEAD"]))
                .map(text)
                .unwrap_or_default();
            let conflicted = git
                .run(STATUS_ARGS)
                .ok()
                .and_then(|o| status::parse(&o).ok())
                .is_some_and(|s| s.entries.iter().any(|e| e.is_conflicted()));
            match (conflicted, result) {
                (true, Err(error)) => Ok(Stop::Conflicts { at, error }),
                (true, Ok(_)) => Ok(Stop::Conflicts {
                    at,
                    error: GitError { command: "git rebase".into(), code: None, stderr: String::new() },
                }),
                (false, result) => Ok(Stop::Paused { at, error: result.err() }),
            }
        }
        Some(other) => Err(OpError::Conflict {
            op: Some(other),
            error: result.err().unwrap_or(GitError {
                command: "git rebase".into(),
                code: None,
                stderr: format!("{} in progress", other.name()),
            }),
            pending: None,
        }),
        None => {
            remove_run_files(git, &session.files);
            result?;
            Ok(Stop::Done(session.pending.clone().finish(git, now).map(Box::new)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(action: Action, hash: &str, subject: &str, message: Option<&str>) -> Step {
        Step {
            action,
            hash: hash.to_string(),
            subject: subject.to_string(),
            message: message.map(str::to_string),
        }
    }

    #[test]
    fn from_key_matches_w9_shortcuts() {
        let cases = [
            ('p', Some(Action::Pick)),
            ('r', Some(Action::Reword)),
            ('e', Some(Action::Edit)),
            ('s', Some(Action::Squash)),
            ('f', Some(Action::Fixup)),
            ('x', Some(Action::Drop)),
            ('P', Some(Action::Pick)), // case-insensitive: keys come from key events, not text
            ('q', None),
            (' ', None),
        ];
        for (key, expected) in cases {
            assert_eq!(Action::from_key(key), expected, "key {key:?}");
        }
    }

    #[test]
    fn action_serializes_lowercase() {
        let cases = [
            (Action::Pick, "\"pick\""),
            (Action::Reword, "\"reword\""),
            (Action::Edit, "\"edit\""),
            (Action::Squash, "\"squash\""),
            (Action::Fixup, "\"fixup\""),
            (Action::Drop, "\"drop\""),
        ];
        for (action, json) in cases {
            assert_eq!(serde_json::to_string(&action).expect("serialize"), json);
        }
    }

    #[test]
    fn from_commits_picks_everything_in_input_order() {
        let commits = [("h1".to_string(), "first".to_string()), ("h2".to_string(), "second".to_string())];
        let plan = Plan::from_commits(&commits);
        assert_eq!(
            plan.steps,
            vec![step(Action::Pick, "h1", "first", None), step(Action::Pick, "h2", "second", None)]
        );
    }

    #[test]
    fn move_step_reorders() {
        let mut plan = Plan::from_commits(&[
            ("h1".to_string(), "a".to_string()),
            ("h2".to_string(), "b".to_string()),
            ("h3".to_string(), "c".to_string()),
        ]);
        plan.move_step(2, 0); // drag the last row to the top
        let hashes: Vec<&str> = plan.steps.iter().map(|s| s.hash.as_str()).collect();
        assert_eq!(hashes, vec!["h3", "h1", "h2"]);
    }

    #[test]
    fn move_step_clamps_to_the_end() {
        let mut plan =
            Plan::from_commits(&[("h1".to_string(), "a".to_string()), ("h2".to_string(), "b".to_string())]);
        plan.move_step(0, 999);
        let hashes: Vec<&str> = plan.steps.iter().map(|s| s.hash.as_str()).collect();
        assert_eq!(hashes, vec!["h2", "h1"]);
    }

    #[test]
    fn move_step_ignores_out_of_range_from() {
        let mut plan =
            Plan::from_commits(&[("h1".to_string(), "a".to_string()), ("h2".to_string(), "b".to_string())]);
        plan.move_step(5, 0);
        let hashes: Vec<&str> = plan.steps.iter().map(|s| s.hash.as_str()).collect();
        assert_eq!(hashes, vec!["h1", "h2"]);
    }

    #[test]
    fn validate_rejects_all_dropped() {
        let plan =
            Plan { steps: vec![step(Action::Drop, "h1", "a", None), step(Action::Drop, "h2", "b", None)] };
        assert!(plan.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_plan() {
        assert!(Plan::default().validate().is_err());
    }

    #[test]
    fn validate_rejects_leading_squash_or_fixup() {
        let squash_first = Plan { steps: vec![step(Action::Squash, "h1", "a", None)] };
        assert!(squash_first.validate().is_err());
        let fixup_first = Plan { steps: vec![step(Action::Fixup, "h1", "a", None)] };
        assert!(fixup_first.validate().is_err());
    }

    #[test]
    fn validate_accepts_leading_drop_then_pick() {
        let plan =
            Plan { steps: vec![step(Action::Drop, "h1", "a", None), step(Action::Pick, "h2", "b", None)] };
        assert_eq!(plan.validate(), Ok(()));
    }

    #[test]
    fn validate_accepts_a_normal_plan() {
        let plan = Plan { steps: vec![step(Action::Pick, "h1", "a", None)] };
        assert_eq!(plan.validate(), Ok(()));
    }

    #[test]
    fn todo_text_formats_action_hash_subject_lines() {
        let plan = Plan {
            steps: vec![
                step(Action::Pick, "abc123", "Add feature", None),
                step(Action::Drop, "def456", "WIP", None),
            ],
        };
        assert_eq!(todo_text(&plan), "pick abc123 Add feature\ndrop def456 WIP\n");
    }

    #[test]
    fn messages_empty_for_picks_edits_and_drops() {
        let plan = Plan {
            steps: vec![
                step(Action::Pick, "h1", "a", None),
                step(Action::Edit, "h2", "b", None),
                step(Action::Drop, "h3", "c", None),
            ],
        };
        assert!(plan.messages().is_empty());
    }

    #[test]
    fn messages_unset_reword_still_holds_its_slot() {
        // git opens the editor for every reword: without its slot, the next reword's text would
        // be written into this commit.
        let plan = Plan {
            steps: vec![
                step(Action::Pick, "h1", "a", None),
                step(Action::Reword, "h2", "b", None),
                step(Action::Reword, "h3", "c", Some("new c")),
            ],
        };
        assert_eq!(plan.messages(), vec![None, Some("new c".to_string())]);
    }

    #[test]
    fn messages_includes_reword_message() {
        let plan = Plan { steps: vec![step(Action::Reword, "h1", "a", Some("New subject"))] };
        assert_eq!(plan.messages(), vec![Some("New subject".to_string())]);
    }

    #[test]
    fn messages_one_per_squash_chain_using_the_last_squash_message() {
        // pick, fixup, squash, fixup: one chain, the *last squash* step supplies the message,
        // even though a fixup with no message of its own follows it in the chain.
        let plan = Plan {
            steps: vec![
                step(Action::Pick, "h1", "a", None),
                step(Action::Fixup, "h2", "b", None),
                step(Action::Squash, "h3", "c", Some("combined")),
                step(Action::Fixup, "h4", "d", None),
            ],
        };
        assert_eq!(plan.messages(), vec![Some("combined".to_string())]);
    }

    #[test]
    fn messages_silent_for_fixup_only_chain() {
        let plan =
            Plan { steps: vec![step(Action::Pick, "h1", "a", None), step(Action::Fixup, "h2", "b", None)] };
        assert!(plan.messages().is_empty());
    }

    #[test]
    fn messages_squash_chain_without_text_holds_an_empty_slot() {
        let plan =
            Plan { steps: vec![step(Action::Pick, "h1", "a", None), step(Action::Squash, "h2", "b", None)] };
        assert_eq!(plan.messages(), vec![None]);
    }

    #[test]
    fn messages_drop_inside_a_squash_chain_does_not_split_it() {
        // git skips no-op commands (drop) when looking for a chain's final fixup/squash, so this
        // is one chain and one prompt, answered with the last squash's text.
        let plan = Plan {
            steps: vec![
                step(Action::Pick, "h1", "a", None),
                step(Action::Squash, "h2", "b", Some("msg b")),
                step(Action::Drop, "h3", "c", None),
                step(Action::Squash, "h4", "d", Some("final")),
            ],
        };
        assert_eq!(plan.messages(), vec![Some("final".to_string())]);
    }

    #[test]
    fn messages_reword_then_squash_chain_are_two_separate_prompts() {
        let plan = Plan {
            steps: vec![
                step(Action::Reword, "h1", "a", Some("reworded")),
                step(Action::Squash, "h2", "b", Some("squashed")),
            ],
        };
        assert_eq!(plan.messages(), vec![Some("reworded".to_string()), Some("squashed".to_string())]);
    }

    #[test]
    fn messages_multiple_independent_chains_stay_in_plan_order() {
        let plan = Plan {
            steps: vec![
                step(Action::Pick, "h1", "a", None),
                step(Action::Squash, "h2", "b", Some("first combo")),
                step(Action::Pick, "h3", "c", None),
                step(Action::Squash, "h4", "d", Some("second combo")),
            ],
        };
        assert_eq!(plan.messages(), vec![Some("first combo".to_string()), Some("second combo".to_string())]);
    }

    #[test]
    fn write_plan_files_round_trips_plan_and_messages() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plan = Plan { steps: vec![step(Action::Reword, "h1", "a", Some("new message"))] };

        let (plan_path, messages_path) = write_plan_files(&plan, dir.path()).expect("write");
        assert_eq!(plan_path, dir.path().join("plan.json"));
        assert_eq!(messages_path, dir.path().join("messages.json"));

        let loaded_plan: Plan =
            serde_json::from_slice(&std::fs::read(&plan_path).expect("read")).expect("parse");
        assert_eq!(loaded_plan, plan);

        let loaded_messages: Vec<(String, String)> =
            serde_json::from_slice(&std::fs::read(&messages_path).expect("read")).expect("parse");
        assert_eq!(loaded_messages, vec![("h1".to_string(), "new message".to_string())]);
    }

    #[test]
    fn write_plan_files_creates_missing_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("a").join("b");
        write_plan_files(&Plan::default(), &nested).expect("write");
        assert!(nested.join("plan.json").exists());
    }

    #[test]
    fn apply_sequence_editor_overwrites_the_todo_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plan = Plan {
            steps: vec![step(Action::Pick, "abc", "First", None), step(Action::Drop, "def", "Second", None)],
        };
        let plan_file = dir.path().join("plan.json");
        std::fs::write(&plan_file, serde_json::to_vec(&plan).expect("serialize")).expect("write");
        let todo_file = dir.path().join("git-rebase-todo");
        std::fs::write(&todo_file, "stale content").expect("seed");

        apply_sequence_editor(&plan_file, &todo_file).expect("apply");
        assert_eq!(std::fs::read_to_string(&todo_file).expect("read"), todo_text(&plan));
    }

    // ---- keyed messages and the --editor helper ----------------------------------------------

    #[test]
    fn keyed_messages_name_the_reword_and_the_final_step_of_a_squash_chain() {
        let plan = Plan {
            steps: vec![
                step(Action::Reword, "h1", "a", Some("new a")),
                step(Action::Reword, "h2", "b", None), // no text: git keeps its message
                step(Action::Squash, "h3", "c", Some("b and c")),
                step(Action::Drop, "h4", "d", None),
                step(Action::Fixup, "h5", "e", None), // final step of the chain: git prompts here
                step(Action::Pick, "h6", "f", None),
                step(Action::Fixup, "h7", "g", None), // fixup-only chain: silent
            ],
        };
        assert_eq!(
            plan.keyed_messages(),
            vec![("h1".to_string(), "new a".to_string()), ("h5".to_string(), "b and c".to_string())]
        );
    }

    #[test]
    fn current_step_reads_the_last_command_of_done() {
        assert_eq!(current_step("pick abc1234 First\nreword def5678 Second\n"), Some("def5678"));
        assert_eq!(current_step("pick abc1234 First\n\n# comment\n"), Some("abc1234"));
        assert_eq!(current_step("fixup -C abc1234 Subject\n"), Some("abc1234"));
        assert_eq!(current_step("pick abc1234 x\nexec make test\n"), None);
        assert_eq!(current_step("pick abc1234 x\nbreak\n"), None);
        assert_eq!(current_step(""), None);
    }

    /// A fake git dir for [`apply_editor`]: `rebase-merge/done` ends with `done_last`.
    fn editor_fixture(
        messages: &[(&str, &str)],
        done_last: &str,
    ) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let git_dir = dir.path().join(".git");
        std::fs::create_dir_all(git_dir.join("rebase-merge")).expect("mkdir");
        std::fs::write(git_dir.join("rebase-merge").join("done"), format!("pick 1111111 x\n{done_last}\n"))
            .expect("write done");
        let messages_file = dir.path().join("messages.json");
        std::fs::write(&messages_file, serde_json::to_vec(messages).expect("ser")).expect("write");
        let message_file = git_dir.join("COMMIT_EDITMSG");
        std::fs::write(&message_file, "git's default text").expect("seed");
        (dir, messages_file, message_file)
    }

    #[test]
    fn apply_editor_writes_the_text_of_the_step_git_is_committing() {
        let full = "abcdef0123456789abcdef0123456789abcdef01";
        // git abbreviates hashes in `done`; the plan has them in full.
        let (_dir, messages, message) =
            editor_fixture(&[(full, "reworded"), ("9999999999", "other")], "reword abcdef0 Subject");
        apply_editor(&messages, &message).expect("apply");
        assert_eq!(std::fs::read_to_string(&message).expect("read"), "reworded\n");
    }

    /// A pick committed after a conflict, or an amend at an `edit` stop, opens the editor too:
    /// such a step has no text, so git's message stays and no other commit's text is used up.
    #[test]
    fn apply_editor_leaves_other_steps_untouched() {
        let (_dir, messages, message) = editor_fixture(&[("abcdef0", "reworded")], "pick 1234567 Other");
        apply_editor(&messages, &message).expect("apply");
        assert_eq!(std::fs::read_to_string(&message).expect("read"), "git's default text");
    }

    #[test]
    fn apply_editor_outside_a_rebase_leaves_the_file_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let messages = dir.path().join("messages.json");
        std::fs::write(&messages, b"[[\"abcdef0\",\"x\"]]").expect("write");
        let message = dir.path().join("COMMIT_EDITMSG");
        std::fs::write(&message, "mine").expect("seed");
        apply_editor(&messages, &message).expect("apply");
        assert_eq!(std::fs::read_to_string(&message).expect("read"), "mine");
    }

    #[test]
    fn missing_from_plan_names_todo_commits_without_a_step() {
        let plan = Plan {
            steps: vec![
                step(Action::Pick, "aaaaaaa1111", "a", None),
                step(Action::Drop, "bbbbbbb2222", "b", None),
            ],
        };
        let todo = "pick aaaaaaa a\npick bbbbbbb b\npick ccccccc late\n# pick ddddddd comment\n\
                    ; comment with another char\nexec make\nlabel onto\nupdate-ref refs/heads/x\n\
                    fixup -C eeeeeee e\n";
        assert_eq!(missing_from_plan(&plan, todo), ["ccccccc", "eeeeeee"]);
        assert!(missing_from_plan(&plan, "pick aaaaaaa a\n").is_empty());
    }

    #[test]
    fn apply_sequence_editor_refuses_a_todo_with_commits_the_plan_lacks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plan = Plan { steps: vec![step(Action::Pick, "abc1234", "First", None)] };
        let plan_file = dir.path().join("plan.json");
        std::fs::write(&plan_file, serde_json::to_vec(&plan).expect("serialize")).expect("write");
        let todo_file = dir.path().join("git-rebase-todo");
        let generated = "pick abc1234 First\npick def5678 Late\n";
        std::fs::write(&todo_file, generated).expect("seed");
        let err = apply_sequence_editor(&plan_file, &todo_file).expect_err("stale plan");
        assert!(err.to_string().contains("def5678"), "{err}");
        assert_eq!(std::fs::read_to_string(&todo_file).expect("read"), generated, "todo untouched");
    }

    #[test]
    fn comment_char_avoids_the_plans_own_line_starts() {
        let with = |messages: &[&str]| Plan {
            steps: messages.iter().map(|m| step(Action::Reword, "h", "s", Some(m))).collect(),
        };
        assert_eq!(comment_char(&with(&["Subject\n\nbody with a # inside"])), None);
        assert_eq!(comment_char(&with(&["Subject\n\n# Notes"])), Some(';'));
        assert_eq!(comment_char(&with(&["S\n\n# Notes", "T\n; x\n% y"])), Some('!'));
    }

    // ---- range, run files, helpers -------------------------------------------------------------

    #[test]
    fn remote_helper_is_the_installed_cli_or_a_reason() {
        use crate::ssh::steps::CliStatus;
        let helper = Helper::remote(&CliStatus::Present, "/home/ada/", "0.4.1").expect("present"); // portability: allow
        assert_eq!(helper.program, "/home/ada/.amalgum/bin/0.4.1/amalgum"); // portability: allow
        assert!(Helper::remote(&CliStatus::Installed, "/h", "1").is_ok());
        let err = Helper::remote(&CliStatus::Unavailable { reason: "no build for sparc".into() }, "/h", "1")
            .expect_err("unavailable");
        assert!(err.contains("no build for sparc"), "{err}");
    }

    #[test]
    fn helper_env_quotes_the_program_for_gits_shell() {
        let helper = Helper { program: "my dir/amalgum".into() };
        let files = RunFiles::new("repo/.git/", "run1");
        assert_eq!(files.dir, "repo/.git/amalgum-rebase/run1");
        let env = helper.env(&files);
        assert_eq!(env[0], ("GIT_SEQUENCE_EDITOR", "'my dir/amalgum' --sequence-editor".to_string()));
        assert_eq!(env[1], ("GIT_EDITOR", "'my dir/amalgum' --editor".to_string()));
        assert_eq!(env[2], ("AMALGUM_REBASE_PLAN", "repo/.git/amalgum-rebase/run1/plan.json".to_string()));
        assert_eq!(
            env[3],
            ("AMALGUM_REBASE_MSGS", "repo/.git/amalgum-rebase/run1/messages.json".to_string())
        );
    }

    #[test]
    fn parse_published_keeps_protected_branches_of_known_remotes() {
        let out = b"refs/remotes/origin/HEAD\nrefs/remotes/origin/main\nrefs/remotes/origin/feat\n\
                    refs/remotes/up/stream/release/1.0\nrefs/remotes/fork/develop\n";
        let remotes = vec!["origin".to_string(), "up/stream".to_string(), "fork".to_string()];
        let protected = |b: &str| b == "main" || b == "develop" || b.starts_with("release/");
        assert_eq!(
            parse_published(out, &remotes, protected),
            vec!["origin/main", "up/stream/release/1.0", "fork/develop"]
        );
    }

    #[test]
    fn parse_range_log_reads_real_git_output_with_multiline_messages() {
        let mut repo = crate::testutil::TempRepo::new();
        let base = repo.commit_file("a", "a", "Base");
        repo.commit_file("b", "b", "Second\n\nBody line\nwith \u{1f}odd separator");
        repo.commit_file("c", "c", "Third ünïcode");
        let out = repo.git_raw(&range_log_args(Some(&base)).iter().map(String::as_str).collect::<Vec<_>>());
        let commits = parse_range_log(&out);
        let subjects: Vec<&str> = commits.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, ["Second", "Third ünïcode"], "oldest first, base excluded");
        assert_eq!(commits[0].message, "Second\n\nBody line\nwith \u{1f}odd separator");
        assert_eq!(commits[0].parents, vec![base]);

        let all = repo.git_raw(&range_log_args(None).iter().map(String::as_str).collect::<Vec<_>>());
        let all = parse_range_log(&all);
        assert_eq!(all.len(), 3);
        assert!(all[0].parents.is_empty(), "the root commit has no parents");
    }

    fn local(repo: &crate::testutil::TempRepo) -> Git {
        for (key, value) in
            [("user.name", "Ada Tester"), ("user.email", "ada@example.com"), ("commit.gpgsign", "false")]
        {
            repo.git(&["config", key, value]);
        }
        Git::new(crate::git::Location::Local { path: repo.path().to_path_buf() })
    }

    #[test]
    fn read_range_lists_the_commits_from_the_selected_one_to_head() {
        let mut repo = crate::testutil::TempRepo::new();
        let root = repo.commit_file("a", "a", "Root");
        let second = repo.commit_file("b", "b", "Second");
        let head = repo.commit_file("c", "c", "Third");
        let git = local(&repo);
        let range = read_range(&git, &second, &[], |_| true).expect("range");
        assert_eq!(range.branch, "main");
        assert_eq!(range.head, head);
        assert_eq!(range.base.as_deref(), Some(root.as_str()));
        assert_eq!(range.picks(), vec![(second, "Second".to_string()), (head, "Third".to_string())]);
        assert!(range.published.is_empty());

        let from_root = read_range(&git, &root, &[], |_| true).expect("range from root");
        assert_eq!(from_root.base, None, "the root commit rebases with --root");
        assert_eq!(from_root.commits.len(), 3);
    }

    #[test]
    fn read_range_refuses_detached_head_foreign_commits_and_merges() {
        let mut repo = crate::testutil::TempRepo::new();
        let root = repo.commit_file("a", "a", "Root");
        repo.git(&["checkout", "-q", "-b", "side"]);
        let side = repo.commit_file("s", "s", "Side");
        repo.git(&["checkout", "-q", "main"]);
        repo.commit_file("m", "m", "Main");
        let git = local(&repo);

        let err = read_range(&git, &side, &[], |_| true).expect_err("side is not in main");
        assert_eq!(err, RangeError::NotAncestor { commit: side.clone(), branch: "main".into() });
        assert_eq!(err.to_string(), format!("{} is not in the history of `main`", &side[..7]));

        repo.git(&["merge", "-q", "--no-ff", "-m", "Merge side", "side"]);
        let err = read_range(&git, &root, &[], |_| true).expect_err("merge in range");
        assert_eq!(err, RangeError::Merges { count: 1 });

        repo.git(&["checkout", "-q", "--detach"]);
        assert_eq!(read_range(&git, &root, &[], |_| true), Err(RangeError::Detached));
    }

    #[test]
    fn read_range_warns_about_commits_already_on_a_protected_remote_branch() {
        let mut repo = crate::testutil::TempRepo::new();
        let first = repo.commit_file("a", "a", "First");
        repo.git(&["update-ref", "refs/remotes/origin/main", &first]);
        repo.git(&["update-ref", "refs/remotes/origin/feat", &first]);
        repo.commit_file("b", "b", "Second");
        let git = local(&repo);
        let remotes = vec!["origin".to_string()];
        let range = read_range(&git, &first, &remotes, |b| b == "main").expect("range");
        assert_eq!(range.published, vec!["origin/main"]);
    }

    #[test]
    fn run_files_are_written_and_removed_through_git() {
        let mut repo = crate::testutil::TempRepo::new();
        repo.commit_file("a", "a", "First");
        let git = local(&repo);
        let plan =
            Plan { steps: vec![step(Action::Reword, "abc1234", "First", Some("It's \"quoted\"\n$HOME"))] };
        let files = write_run_files(&git, "run 1", &plan).expect("write");
        assert!(files.dir.ends_with("/.git/amalgum-rebase/run 1"), "{}", files.dir);
        let written: Plan =
            serde_json::from_slice(&std::fs::read(&files.plan).expect("read")).expect("parse");
        assert_eq!(written, plan);
        let other = write_run_files(&git, "run 2", &plan).expect("write");
        remove_run_files(&git, &files);
        assert!(!Path::new(&files.dir).exists());
        assert!(Path::new(&other.plan).exists(), "another run's files stay");
        remove_run_files(&git, &other);
        assert!(!repo.path().join(".git").join(RUNS_DIR).exists(), "the last run tidies up");
    }

    /// Runs `git rebase -i <base>` in `repo` with `plan`, through `GIT_SEQUENCE_EDITOR` /
    /// `GIT_EDITOR` shell one-liners standing in for the CLI's `--sequence-editor` / `--editor`
    /// modes (a lib test can't exec the amalgum binary). The editor hands out [`Plan::messages`]
    /// by a call counter. Asserts git opened the editor exactly once per
    /// queued slot, then returns `git log --format=%s`, newest first.
    fn rebase_with(repo: &crate::testutil::TempRepo, base: &str, plan: &Plan) -> Vec<String> {
        let messages = plan.messages();
        let dir = tempfile::tempdir().expect("tempdir");

        // GIT_SEQUENCE_EDITOR: `cp` the precomputed todo over git's, as `apply_sequence_editor`
        // does.
        let todo_precomputed = dir.path().join("todo-precomputed");
        std::fs::write(&todo_precomputed, todo_text(plan)).expect("write todo");

        // GIT_EDITOR: one file per slot with text, taken in order by a counter file; a slot
        // without text has no file, so git's message is left as it is.
        for (i, msg) in messages.iter().enumerate() {
            if let Some(msg) = msg {
                std::fs::write(dir.path().join(format!("msg-{i}")), format!("{msg}\n")).expect("write msg");
            }
        }
        let editor_script = dir.path().join("editor.sh");
        let idx_file = dir.path().join("editor-idx");
        std::fs::write(
            &editor_script,
            format!(
                "#!/bin/sh\nIDX=0\n[ -f {idx} ] && IDX=$(cat {idx})\necho $((IDX + 1)) > {idx}\nMSG={dir}/msg-$IDX\n[ -f \"$MSG\" ] && cp \"$MSG\" \"$1\"\nexit 0\n",
                idx = idx_file.display(),
                dir = dir.path().display(),
            ),
        )
        .expect("write editor script");

        let out = crate::testutil::hermetic_git(repo.path())
            .env("GIT_SEQUENCE_EDITOR", format!("cp {}", todo_precomputed.display()))
            .env("GIT_EDITOR", format!("sh {}", editor_script.display()))
            .args(["rebase", "-i", base])
            .output()
            .expect("spawn git rebase");
        assert!(out.status.success(), "git rebase -i failed:\n{}", String::from_utf8_lossy(&out.stderr));

        let prompts: usize =
            std::fs::read_to_string(&idx_file).map_or(0, |s| s.trim().parse().expect("editor counter"));
        assert_eq!(prompts, messages.len(), "git opens the editor once per queued slot");
        repo.git(&["log", "--format=%s"]).lines().map(str::to_string).collect()
    }

    /// Commits `Base`, then one commit per subject, each adding its own file so that any plan
    /// applies cleanly. Returns the base hash and a pick-everything plan over the later commits.
    fn repo_with_commits(repo: &mut crate::testutil::TempRepo, subjects: &[&str]) -> (String, Plan) {
        let base = repo.commit_file("base.txt", "base", "Base");
        let commits: Vec<(String, String)> = subjects
            .iter()
            .enumerate()
            .map(|(i, subject)| {
                (repo.commit_file(&format!("f{i}.txt"), subject, subject), subject.to_string())
            })
            .collect();
        (base, Plan::from_commits(&commits))
    }

    /// End to end against real git: a 4-commit plan that rewords one commit, squashes one into
    /// its predecessor, drops one, and reorders two. This proves [`Plan::messages`] and
    /// [`todo_text`] agree, in order, with what real git actually asks for.
    #[test]
    fn end_to_end_rebase_matches_real_git() {
        let mut repo = crate::testutil::TempRepo::new();
        let (base, mut plan) = repo_with_commits(&mut repo, &["Alpha", "Bravo", "Charlie", "Delta"]);
        let hashes: Vec<String> = plan.steps.iter().map(|s| s.hash.clone()).collect();

        // Reorder two: swap Charlie and Delta so Delta lands before Charlie.
        plan.move_step(3, 2);
        assert_eq!(
            plan.steps.iter().map(|s| s.hash.as_str()).collect::<Vec<_>>(),
            vec![hashes[0].as_str(), hashes[1].as_str(), hashes[3].as_str(), hashes[2].as_str()]
        );

        plan.steps[0].action = Action::Drop; // Alpha: dropped
        plan.steps[1].action = Action::Reword; // Bravo: reworded standalone
        plan.steps[1].message = Some("Bravo reworded".to_string());
        // Delta (steps[2]) stays `pick`: the squash target/predecessor.
        plan.steps[3].action = Action::Squash; // Charlie: squashed into Delta
        plan.steps[3].message = Some("Delta and Charlie combined".to_string());

        plan.validate().expect("plan is valid");
        assert_eq!(
            plan.messages(),
            vec![Some("Bravo reworded".to_string()), Some("Delta and Charlie combined".to_string())]
        );
        assert_eq!(
            rebase_with(&repo, &base, &plan),
            ["Delta and Charlie combined", "Bravo reworded", "Base"]
        );
    }

    /// `r` pressed on a row without typing text: git still opens the editor for it, so the queue
    /// must hold that slot or the next reword's text lands on this commit instead.
    #[test]
    fn end_to_end_reword_without_text_keeps_later_messages_on_their_commits() {
        let mut repo = crate::testutil::TempRepo::new();
        let (base, mut plan) = repo_with_commits(&mut repo, &["Alpha", "Bravo", "Charlie"]);
        plan.steps[1].action = Action::Reword;
        plan.steps[2].action = Action::Reword;
        plan.steps[2].message = Some("Charlie reworded".to_string());

        assert_eq!(rebase_with(&repo, &base, &plan), ["Charlie reworded", "Bravo", "Alpha", "Base"]);
    }

    /// A squash chain with no text still prompts once (git keeps its combined message), and a
    /// later reword still gets its own text.
    #[test]
    fn end_to_end_squash_without_text_keeps_later_messages_on_their_commits() {
        let mut repo = crate::testutil::TempRepo::new();
        let (base, mut plan) = repo_with_commits(&mut repo, &["Alpha", "Bravo", "Charlie", "Delta"]);
        plan.steps[1].action = Action::Squash;
        plan.steps[3].action = Action::Reword;
        plan.steps[3].message = Some("Delta reworded".to_string());

        // git's combined message is "Alpha\n\nBravo", whose subject is "Alpha".
        assert_eq!(rebase_with(&repo, &base, &plan), ["Delta reworded", "Charlie", "Alpha", "Base"]);
    }

    /// git skips `drop` when looking for the end of a squash chain, so squash, drop, squash is
    /// one chain with one prompt, which takes the last squash's text.
    #[test]
    fn end_to_end_drop_inside_a_squash_chain_is_one_prompt() {
        let mut repo = crate::testutil::TempRepo::new();
        let (base, mut plan) = repo_with_commits(&mut repo, &["Alpha", "Bravo", "Charlie", "Delta", "Echo"]);
        plan.steps[1].action = Action::Squash;
        plan.steps[1].message = Some("Alpha and Bravo".to_string());
        plan.steps[2].action = Action::Drop;
        plan.steps[3].action = Action::Squash;
        plan.steps[3].message = Some("Alpha, Bravo and Delta".to_string());
        plan.steps[4].action = Action::Reword;
        plan.steps[4].message = Some("Echo reworded".to_string());

        assert_eq!(rebase_with(&repo, &base, &plan), ["Echo reworded", "Alpha, Bravo and Delta", "Base"]);
    }
}
