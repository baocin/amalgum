//! Interactive rebase without a terminal editor (§5.16). The UI builds a [`Plan`]; the app runs
//! `git rebase -i <base>` with `GIT_SEQUENCE_EDITOR="<exe> --sequence-editor"` and
//! `GIT_EDITOR="<exe> --editor"`, and env `AMALGUM_REBASE_PLAN` / `AMALGUM_REBASE_MSGS` naming
//! JSON files written by [`write_plan_files`]. The helpers run in the CLI (also on remote
//! hosts): `--sequence-editor <todo>` overwrites git's todo file with [`todo_text`];
//! `--editor <msgfile>` overwrites the message file with the next queued message (one slot per
//! prompt git opens, i.e. per reword and per squash chain, in plan order; a counter file beside
//! the messages file tracks progress). A slot without text, or an exhausted queue, leaves git's
//! message untouched.

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
    /// git's todo-list keyword for this action.
    fn keyword(self) -> &'static str {
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
    /// One slot per editor prompt git opens, in the order it opens them: the text the editor
    /// helper writes, or `None` to keep git's pre-filled message (the commit's own message for a
    /// `reword`, git's combined message for a squash chain). A prompt without text still takes
    /// its slot, because the helper hands slots out by counting its calls.
    ///
    /// `reword` always prompts once, for its own commit. `squash`/`fixup` steps meld into
    /// whatever commit precedes them; git prompts once per maximal run of squash/fixup steps
    /// (a `drop` inside the run doesn't end it: git skips it as a no-op), but only when that run
    /// contains at least one `squash` (a run of pure `fixup`s is silent, keeping the preceding
    /// message unedited). The prompt's slot takes the message of the *last* `squash` step in the
    /// run that has one — which is where this plan's UI stores the user's final, already-combined
    /// text.
    pub fn messages(&self) -> Vec<Option<String>> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.steps.len() {
            match self.steps[i].action {
                Action::Reword => {
                    out.push(self.steps[i].message.clone());
                    i += 1;
                }
                Action::Squash | Action::Fixup => {
                    let mut has_squash = false;
                    let mut last_squash_message = None;
                    while i < self.steps.len()
                        && matches!(self.steps[i].action, Action::Squash | Action::Fixup | Action::Drop)
                    {
                        let step = &self.steps[i];
                        if step.action == Action::Squash {
                            has_squash = true;
                            last_squash_message = step.message.clone().or(last_squash_message);
                        }
                        i += 1;
                    }
                    if has_squash {
                        out.push(last_squash_message);
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

/// Write `plan.json` and `messages.json` into `dir`, and reset [`apply_editor`]'s counter so a
/// reused directory starts the new queue at its first slot; returns their paths.
pub fn write_plan_files(plan: &Plan, dir: &Path) -> io::Result<(std::path::PathBuf, std::path::PathBuf)> {
    std::fs::create_dir_all(dir)?;
    let plan_path = dir.join("plan.json");
    let messages_path = dir.join("messages.json");
    std::fs::write(&plan_path, serde_json::to_vec_pretty(plan).map_err(io::Error::other)?)?;
    std::fs::write(&messages_path, serde_json::to_vec_pretty(&plan.messages()).map_err(io::Error::other)?)?;
    write_counter(&counter_path(&messages_path), 0)?;
    Ok((plan_path, messages_path))
}

/// `--sequence-editor` helper body: replace git's todo file with this plan's [`todo_text`].
pub fn apply_sequence_editor(plan_file: &Path, todo_file: &Path) -> io::Result<()> {
    let plan: Plan = serde_json::from_slice(&std::fs::read(plan_file)?).map_err(io::Error::other)?;
    std::fs::write(todo_file, todo_text(&plan))
}

/// `--editor` helper body: write the next queued message over git's message file. A counter
/// file `<messages_file>.idx` tracks how many prompts have already been answered, so repeated
/// calls (one per reword/squash chain) take the queue's slots in order. A slot without text, or
/// a call past the end of the queue, leaves git's own message file untouched (git keeps its
/// default, e.g. the squash preview) but still advances the counter.
pub fn apply_editor(messages_file: &Path, message_file: &Path) -> io::Result<()> {
    let messages: Vec<Option<String>> =
        serde_json::from_slice(&std::fs::read(messages_file)?).map_err(io::Error::other)?;
    let idx_path = counter_path(messages_file);
    let idx = read_counter(&idx_path)?;
    if let Some(Some(message)) = messages.get(idx) {
        std::fs::write(message_file, format!("{message}\n"))?;
    }
    write_counter(&idx_path, idx + 1)
}

fn counter_path(messages_file: &Path) -> std::path::PathBuf {
    let mut name = messages_file.as_os_str().to_owned();
    name.push(".idx");
    std::path::PathBuf::from(name)
}

fn read_counter(path: &Path) -> io::Result<usize> {
    match std::fs::read_to_string(path) {
        Ok(s) => s.trim().parse().map_err(io::Error::other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e),
    }
}

fn write_counter(path: &Path, value: usize) -> io::Result<()> {
    std::fs::write(path, value.to_string())
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

        let loaded_messages: Vec<Option<String>> =
            serde_json::from_slice(&std::fs::read(&messages_path).expect("read")).expect("parse");
        assert_eq!(loaded_messages, vec![Some("new message".to_string())]);
    }

    #[test]
    fn write_plan_files_creates_missing_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("a").join("b");
        write_plan_files(&Plan::default(), &nested).expect("write");
        assert!(nested.join("plan.json").exists());
    }

    #[test]
    fn write_plan_files_resets_the_editor_counter_of_a_reused_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let message_file = dir.path().join("COMMIT_EDITMSG");

        // First rebase: one reword, fully consumed.
        let first = Plan { steps: vec![step(Action::Reword, "h1", "a", Some("first rebase"))] };
        let (_, messages_path) = write_plan_files(&first, dir.path()).expect("write first");
        apply_editor(&messages_path, &message_file).expect("editor, first rebase");

        // Second rebase in the same directory starts from its own first message.
        let second = Plan { steps: vec![step(Action::Reword, "h2", "b", Some("second rebase"))] };
        let (_, messages_path) = write_plan_files(&second, dir.path()).expect("write second");
        std::fs::write(&message_file, "git's default text").expect("seed");
        apply_editor(&messages_path, &message_file).expect("editor, second rebase");
        assert_eq!(std::fs::read_to_string(&message_file).expect("read"), "second rebase\n");
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

    #[test]
    fn apply_editor_pops_messages_in_order_then_leaves_the_file_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let messages_file = dir.path().join("messages.json");
        std::fs::write(
            &messages_file,
            serde_json::to_vec(&vec!["one".to_string(), "two".to_string()]).expect("ser"),
        )
        .expect("write");
        let message_file = dir.path().join("COMMIT_EDITMSG");

        std::fs::write(&message_file, "git's default text").expect("seed");
        apply_editor(&messages_file, &message_file).expect("apply 1");
        assert_eq!(std::fs::read_to_string(&message_file).expect("read"), "one\n");

        std::fs::write(&message_file, "git's default text").expect("seed");
        apply_editor(&messages_file, &message_file).expect("apply 2");
        assert_eq!(std::fs::read_to_string(&message_file).expect("read"), "two\n");

        // Queue exhausted: a third call must not touch the file.
        std::fs::write(&message_file, "git's default text").expect("seed");
        apply_editor(&messages_file, &message_file).expect("apply 3");
        assert_eq!(std::fs::read_to_string(&message_file).expect("read"), "git's default text");

        // The counter file keeps incrementing even past exhaustion.
        let idx = std::fs::read_to_string(counter_path(&messages_file)).expect("read counter");
        assert_eq!(idx, "3");
    }

    #[test]
    fn apply_editor_keeps_git_text_for_an_empty_slot_and_moves_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        let messages_file = dir.path().join("messages.json");
        std::fs::write(&messages_file, serde_json::to_vec(&vec![None, Some("two")]).expect("ser"))
            .expect("write");
        let message_file = dir.path().join("COMMIT_EDITMSG");

        std::fs::write(&message_file, "git's default text").expect("seed");
        apply_editor(&messages_file, &message_file).expect("apply 1");
        assert_eq!(std::fs::read_to_string(&message_file).expect("read"), "git's default text");

        std::fs::write(&message_file, "git's default text").expect("seed");
        apply_editor(&messages_file, &message_file).expect("apply 2");
        assert_eq!(std::fs::read_to_string(&message_file).expect("read"), "two\n");
    }

    /// Runs `git rebase -i <base>` in `repo` with `plan`, through `GIT_SEQUENCE_EDITOR` /
    /// `GIT_EDITOR` shell one-liners standing in for the CLI's `--sequence-editor` / `--editor`
    /// modes (a lib test can't exec the amalgum binary). The editor hands out [`Plan::messages`]
    /// by a call counter, as [`apply_editor`] does. Asserts git opened the editor exactly once per
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
