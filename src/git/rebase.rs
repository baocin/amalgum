//! Interactive rebase without a terminal editor (§5.16). The UI builds a [`Plan`]; the app runs
//! `git rebase -i <base>` with `GIT_SEQUENCE_EDITOR="<exe> --sequence-editor"` and
//! `GIT_EDITOR="<exe> --editor"`, and env `AMALGUM_REBASE_PLAN` / `AMALGUM_REBASE_MSGS` naming
//! JSON files written by [`write_plan_files`]. The helpers run in the CLI (also on remote
//! hosts): `--sequence-editor <todo>` overwrites git's todo file with [`todo_text`];
//! `--editor <msgfile>` overwrites the message file with the next queued message (reword and
//! squash steps, in plan order; a counter file beside the messages file tracks progress). If
//! the queue is exhausted, the editor helper leaves git's message untouched.

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
    /// Messages the editor helper will supply, in the order git asks for them.
    ///
    /// `reword` always prompts once, for its own commit. `squash`/`fixup` steps meld into
    /// whatever commit precedes them; git prompts once per maximal run of consecutive
    /// squash/fixup steps, but only when that run contains at least one `squash` (a run of pure
    /// `fixup`s is silent, keeping the preceding message unedited), and the prompt is pre-filled
    /// toward the message of the *last* `squash` step in the run — which is where this plan's UI
    /// stores the user's final, already-combined text.
    pub fn messages(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.steps.len() {
            match self.steps[i].action {
                Action::Reword => {
                    if let Some(message) = &self.steps[i].message {
                        out.push(message.clone());
                    }
                    i += 1;
                }
                Action::Squash | Action::Fixup => {
                    let mut has_squash = false;
                    let mut last_squash_message = None;
                    while i < self.steps.len()
                        && matches!(self.steps[i].action, Action::Squash | Action::Fixup)
                    {
                        if self.steps[i].action == Action::Squash {
                            has_squash = true;
                            if let Some(message) = &self.steps[i].message {
                                last_squash_message = Some(message.clone());
                            }
                        }
                        i += 1;
                    }
                    if has_squash && let Some(message) = last_squash_message {
                        out.push(message);
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

/// Write `plan.json` and `messages.json` into `dir`; returns their paths.
pub fn write_plan_files(plan: &Plan, dir: &Path) -> io::Result<(std::path::PathBuf, std::path::PathBuf)> {
    std::fs::create_dir_all(dir)?;
    let plan_path = dir.join("plan.json");
    let messages_path = dir.join("messages.json");
    std::fs::write(&plan_path, serde_json::to_vec_pretty(plan).map_err(io::Error::other)?)?;
    std::fs::write(&messages_path, serde_json::to_vec_pretty(&plan.messages()).map_err(io::Error::other)?)?;
    Ok((plan_path, messages_path))
}

/// `--sequence-editor` helper body: replace git's todo file with this plan's [`todo_text`].
pub fn apply_sequence_editor(plan_file: &Path, todo_file: &Path) -> io::Result<()> {
    let plan: Plan = serde_json::from_slice(&std::fs::read(plan_file)?).map_err(io::Error::other)?;
    std::fs::write(todo_file, todo_text(&plan))
}

/// `--editor` helper body: write the next queued message over git's message file. A counter
/// file `<messages_file>.idx` tracks how many messages have already been supplied, so repeated
/// calls (one per reword/squash chain) hand out the queue in order. Once the queue is exhausted,
/// git's own message file is left untouched (git keeps its default, e.g. the squash preview).
pub fn apply_editor(messages_file: &Path, message_file: &Path) -> io::Result<()> {
    let messages: Vec<String> =
        serde_json::from_slice(&std::fs::read(messages_file)?).map_err(io::Error::other)?;
    let idx_path = counter_path(messages_file);
    let idx = read_counter(&idx_path)?;
    if let Some(message) = messages.get(idx) {
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
    fn messages_empty_for_plain_picks_and_unset_reword() {
        let plan =
            Plan { steps: vec![step(Action::Pick, "h1", "a", None), step(Action::Reword, "h2", "b", None)] };
        assert!(plan.messages().is_empty());
    }

    #[test]
    fn messages_includes_reword_message() {
        let plan = Plan { steps: vec![step(Action::Reword, "h1", "a", Some("New subject"))] };
        assert_eq!(plan.messages(), vec!["New subject".to_string()]);
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
        assert_eq!(plan.messages(), vec!["combined".to_string()]);
    }

    #[test]
    fn messages_silent_for_fixup_only_chain() {
        let plan =
            Plan { steps: vec![step(Action::Pick, "h1", "a", None), step(Action::Fixup, "h2", "b", None)] };
        assert!(plan.messages().is_empty());
    }

    #[test]
    fn messages_squash_chain_with_no_message_yields_nothing() {
        let plan =
            Plan { steps: vec![step(Action::Pick, "h1", "a", None), step(Action::Squash, "h2", "b", None)] };
        assert!(plan.messages().is_empty());
    }

    #[test]
    fn messages_reword_then_squash_chain_are_two_separate_prompts() {
        let plan = Plan {
            steps: vec![
                step(Action::Reword, "h1", "a", Some("reworded")),
                step(Action::Squash, "h2", "b", Some("squashed")),
            ],
        };
        assert_eq!(plan.messages(), vec!["reworded".to_string(), "squashed".to_string()]);
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
        assert_eq!(plan.messages(), vec!["first combo".to_string(), "second combo".to_string()]);
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

        let loaded_messages: Vec<String> =
            serde_json::from_slice(&std::fs::read(&messages_path).expect("read")).expect("parse");
        assert_eq!(loaded_messages, vec!["new message".to_string()]);
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

    /// End to end against real git: a 4-commit plan that rewords one commit, squashes one into
    /// its predecessor, drops one, and reorders two, run through `git rebase -i` with
    /// `GIT_SEQUENCE_EDITOR`/`GIT_EDITOR` shell one-liners standing in for the CLI's
    /// `--sequence-editor`/`--editor` modes (a lib test can't exec the amalgum binary). This
    /// proves [`Plan::messages`] and [`todo_text`] agree, in order, with what real git actually
    /// asks for.
    #[test]
    fn end_to_end_rebase_matches_real_git() {
        let mut repo = crate::testutil::TempRepo::new();
        repo.commit_file("base.txt", "base", "Base");
        let alpha = repo.commit_file("a.txt", "a", "Alpha");
        let bravo = repo.commit_file("b.txt", "b", "Bravo");
        let charlie = repo.commit_file("c.txt", "c", "Charlie");
        let delta = repo.commit_file("d.txt", "d", "Delta");
        let base = repo.git(&["rev-parse", "HEAD~4"]);

        let mut plan = Plan::from_commits(&[
            (alpha.clone(), "Alpha".to_string()),
            (bravo.clone(), "Bravo".to_string()),
            (charlie.clone(), "Charlie".to_string()),
            (delta.clone(), "Delta".to_string()),
        ]);

        // Reorder two: swap Charlie and Delta so Delta lands before Charlie.
        plan.move_step(3, 2);
        assert_eq!(
            plan.steps.iter().map(|s| s.hash.as_str()).collect::<Vec<_>>(),
            vec![alpha.as_str(), bravo.as_str(), delta.as_str(), charlie.as_str()]
        );

        plan.steps[0].action = Action::Drop; // Alpha: dropped
        plan.steps[1].action = Action::Reword; // Bravo: reworded standalone
        plan.steps[1].message = Some("Bravo reworded".to_string());
        // Delta (steps[2]) stays `pick`: the squash target/predecessor.
        plan.steps[3].action = Action::Squash; // Charlie: squashed into Delta
        plan.steps[3].message = Some("Delta and Charlie combined".to_string());

        plan.validate().expect("plan is valid");
        let messages = plan.messages();
        assert_eq!(messages, vec!["Bravo reworded".to_string(), "Delta and Charlie combined".to_string()]);

        let dir = tempfile::tempdir().expect("tempdir");

        // GIT_SEQUENCE_EDITOR: precompute the todo file content and `cp` it over git's todo file
        // (standing in for the CLI's `--sequence-editor`, which does exactly this via
        // `apply_sequence_editor`).
        let todo_precomputed = dir.path().join("todo-precomputed");
        std::fs::write(&todo_precomputed, todo_text(&plan)).expect("write todo");

        // GIT_EDITOR: precompute each queued message as its own file and pop them in order,
        // standing in for `apply_editor`'s counter-file behaviour.
        for (i, msg) in messages.iter().enumerate() {
            std::fs::write(dir.path().join(format!("msg-{i}")), format!("{msg}\n")).expect("write msg");
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

        let status = crate::testutil::hermetic_git(repo.path())
            .env("GIT_SEQUENCE_EDITOR", format!("cp {}", todo_precomputed.display()))
            .env("GIT_EDITOR", format!("sh {}", editor_script.display()))
            .args(["rebase", "-i", &base])
            .status()
            .expect("spawn git rebase");
        assert!(status.success(), "git rebase -i failed");

        let subjects = repo.git(&["log", "--format=%s"]);
        let subjects: Vec<&str> = subjects.lines().collect();
        assert_eq!(subjects, vec!["Delta and Charlie combined", "Bravo reworded", "Base"]);
    }
}
