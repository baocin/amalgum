//! End to end §5.16 interactive rebase: `git::rebase::start` / `resume` / `abort` run real
//! `git rebase -i` with the built `amalgum` binary as `GIT_SEQUENCE_EDITOR` / `GIT_EDITOR`,
//! locally and through a stand-in ssh host (`sh` running the remote command line in a fresh
//! environment, as sshd would).
//!
//! Run with `cargo test --no-default-features --test rebase`.

use amalgum::git::rebase::{self, Action, Helper, Plan, Stop};
use amalgum::git::{Git, Location};
use std::path::Path;

fn helper() -> Helper {
    Helper { program: env!("CARGO_BIN_EXE_amalgum").to_string() }
}

/// A repository whose own config pins what the machine's global config might change.
struct Repo {
    dir: tempfile::TempDir,
    git: Git,
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = Git::new(Location::Local { path: dir.path().to_path_buf() });
        let repo = Repo { dir, git };
        repo.run(&["init", "-q", "-b", "main"]);
        for (key, value) in [
            ("user.name", "Ada Tester"),
            ("user.email", "ada@example.com"),
            ("commit.gpgsign", "false"),
            ("rebase.autosquash", "false"),
            ("rebase.updateRefs", "false"),
            ("rebase.abbreviateCommands", "false"),
            ("core.autocrlf", "false"),
        ] {
            repo.run(&["config", key, value]);
        }
        // No hooks from the machine's global config (`core.hooksPath`) run in these repos.
        let no_hooks = repo.path().join(".git").join("no-hooks");
        repo.run(&["config", "core.hooksPath", &no_hooks.display().to_string()]);
        repo
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn run(&self, args: &[&str]) -> String {
        let out = self.git.run(args).unwrap_or_else(|e| panic!("git {args:?}: {}", e.stderr));
        String::from_utf8_lossy(&out).trim_end().to_string()
    }

    fn write(&self, name: &str, contents: &str) {
        std::fs::write(self.path().join(name), contents).expect("write");
    }

    fn commit_file(&self, name: &str, contents: &str, message: &str) -> String {
        self.write(name, contents);
        self.run(&["add", "-A"]);
        self.run(&["commit", "-q", "-m", message]);
        self.run(&["rev-parse", "HEAD"])
    }

    /// Commits `Base`, then one commit per subject, each adding its own file. Returns the base
    /// and a pick-everything plan over the later commits.
    fn with_commits(&self, subjects: &[&str]) -> (String, Plan) {
        let base = self.commit_file("base.txt", "base", "Base");
        let commits: Vec<(String, String)> = subjects
            .iter()
            .enumerate()
            .map(|(i, s)| (self.commit_file(&format!("f{i}.txt"), s, s), s.to_string()))
            .collect();
        (base, Plan::from_commits(&commits))
    }

    fn subjects(&self) -> Vec<String> {
        self.run(&["log", "--format=%s"]).lines().map(str::to_string).collect()
    }

    fn head(&self) -> String {
        self.run(&["rev-parse", "HEAD"])
    }

    fn runs_dir_is_empty(&self) -> bool {
        let dir = self.path().join(".git").join(rebase::RUNS_DIR);
        std::fs::read_dir(&dir).map_or(true, |mut d| d.next().is_none())
    }
}

fn done(stop: Stop) -> amalgum::git::journal::Entry {
    match stop {
        Stop::Done(Some(entry)) => *entry,
        other => panic!("expected a finished rebase, got {other:?}"),
    }
}

/// Reorder, drop, reword, and squash in one plan; the journal entry undoes it.
#[test]
fn reorder_drop_reword_and_squash() {
    let repo = Repo::new();
    let (base, mut plan) = repo.with_commits(&["Alpha", "Bravo", "Charlie", "Delta"]);
    let before = repo.head();
    plan.move_step(3, 2); // Delta before Charlie
    plan.steps[0].action = Action::Drop; // Alpha
    plan.steps[1].action = Action::Reword; // Bravo
    plan.steps[1].message = Some("Bravo reworded\n\nWith a body".into());
    plan.steps[3].action = Action::Squash; // Charlie into Delta
    plan.steps[3].message = Some("Delta and Charlie".into());

    let (_, stop) =
        rebase::start(&repo.git, &helper(), Some(&base), &repo.head(), &plan, "interactive rebase", "r1", 1)
            .expect("start");
    let entry = done(stop);
    assert_eq!(repo.subjects(), ["Delta and Charlie", "Bravo reworded", "Base"]);
    assert_eq!(repo.run(&["log", "-1", "--format=%b", "HEAD~1"]), "With a body");
    assert!(!repo.path().join("f0.txt").exists(), "Alpha's file went with it");
    assert!(repo.runs_dir_is_empty(), "the run's files are removed");

    assert_eq!(entry.description, "interactive rebase");
    assert_eq!(entry.before.head.as_deref(), Some(before.as_str()));
    assert_eq!(entry.inverse, Some(vec![vec!["reset".to_string(), "--hard".to_string(), before.clone()]]));
    amalgum::git::ops::run_plan(&repo.git, entry.inverse.as_ref().expect("inverse")).expect("undo");
    assert_eq!(repo.head(), before, "undo resets to the pre-rebase hash");
}

/// Squash into parent / fixup into parent, as the commit menu builds them.
#[test]
fn squash_and_fixup_into_parent() {
    let repo = Repo::new();
    let (base, mut plan) = repo.with_commits(&["Alpha", "Bravo", "Charlie"]);
    plan.steps[1].action = Action::Squash; // Bravo into Alpha, git's combined message kept
    plan.steps[2].action = Action::Fixup; // Charlie into that, silently
    let (_, stop) = rebase::start(&repo.git, &helper(), Some(&base), &repo.head(), &plan, "squash", "r2", 1)
        .expect("start");
    done(stop);
    assert_eq!(repo.subjects(), ["Alpha", "Base"]);
    assert_eq!(repo.run(&["log", "-1", "--format=%B"]), "Alpha\n\nBravo", "fixup's message is dropped");
    assert_eq!(repo.run(&["show", "--format=", "--name-only", "HEAD"]), "f0.txt\nf1.txt\nf2.txt");
}

/// §5.14 **Squash into one** over a contiguous selection in the middle of the branch, and
/// **Edit commit message** of an older commit, as the commit menu builds them from
/// `read_range`: the selection melds into its oldest commit, later commits are replayed.
#[test]
fn squash_into_one_and_edit_message_from_the_commit_menu() {
    use amalgum::model::rebase_plan::{reword_plan, squash_range_plan};
    let repo = Repo::new();
    let (_, plan) = repo.with_commits(&["Alpha", "Bravo", "Charlie", "Delta"]);
    let hash = |i: usize| plan.steps[i].hash.clone();

    let range = rebase::read_range(&repo.git, &hash(1), &[], |_| false).expect("range");
    let squash = squash_range_plan(&range, &hash(2)).expect("Charlie is after Bravo");
    let (_, stop) =
        rebase::start(&repo.git, &helper(), range.base.as_deref(), &repo.head(), &squash, "squash", "r9", 1)
            .expect("start");
    done(stop);
    assert_eq!(repo.subjects(), ["Delta", "Bravo", "Alpha", "Base"]);
    assert_eq!(
        repo.run(&["log", "-1", "--format=%B", "HEAD~1"]),
        "Bravo\n\nCharlie",
        "git's combined message"
    );

    let alpha = repo.run(&["rev-parse", "HEAD~2"]);
    let range = rebase::read_range(&repo.git, &alpha, &[], |_| false).expect("range");
    let reword = reword_plan(&range, &alpha, "Alpha, edited\n\nWith a body\n");
    let (_, stop) =
        rebase::start(&repo.git, &helper(), range.base.as_deref(), &repo.head(), &reword, "reword", "r10", 1)
            .expect("start");
    done(stop);
    assert_eq!(repo.subjects(), ["Delta", "Bravo", "Alpha, edited", "Base"]);
    assert_eq!(repo.run(&["log", "-1", "--format=%B", "HEAD~2"]), "Alpha, edited\n\nWith a body");
    assert_eq!(repo.run(&["log", "-1", "--format=%B", "HEAD~1"]), "Bravo\n\nCharlie", "later messages kept");
}

/// "Edit commit message": a reword plan for one commit, down to the root commit (`--root`).
#[test]
fn reword_the_root_commit() {
    let repo = Repo::new();
    let root = repo.commit_file("a.txt", "a", "Root");
    repo.commit_file("b.txt", "b", "Next");
    let range = rebase::read_range(&repo.git, &root, &[], |_| false).expect("range");
    assert_eq!(range.base, None);
    let mut plan = Plan::from_commits(&range.picks());
    plan.steps[0].action = Action::Reword;
    plan.steps[0].message = Some("Root, reworded".into());
    let (_, stop) =
        rebase::start(&repo.git, &helper(), None, &repo.head(), &plan, "reword", "r3", 1).expect("start");
    done(stop);
    assert_eq!(repo.subjects(), ["Next", "Root, reworded"]);
}

/// `edit` pauses at the commit; staged changes are amended into it by `--continue` (git opens
/// the editor for that amend, which must not use up the later reword's text).
#[test]
fn edit_pauses_then_continue_finishes_with_later_messages_on_their_commits() {
    let repo = Repo::new();
    let (base, mut plan) = repo.with_commits(&["Alpha", "Bravo", "Charlie"]);
    let bravo = plan.steps[1].hash.clone();
    plan.steps[1].action = Action::Edit;
    plan.steps[2].action = Action::Reword;
    plan.steps[2].message = Some("Charlie reworded".into());

    let (session, stop) =
        rebase::start(&repo.git, &helper(), Some(&base), &repo.head(), &plan, "interactive rebase", "r4", 1)
            .expect("start");
    assert_eq!(stop, Stop::Paused { at: bravo, error: None });
    assert_eq!(rebase::probe(&repo.git, &session, 1).expect("probe"), stop, "probing changes nothing");

    repo.write("f1.txt", "Bravo, amended");
    repo.run(&["add", "f1.txt"]);
    let entry = done(rebase::resume(&repo.git, &session, 2).expect("continue"));
    assert_eq!(repo.subjects(), ["Charlie reworded", "Bravo", "Alpha", "Base"]);
    assert_eq!(repo.run(&["show", "HEAD~1:f1.txt"]), "Bravo, amended");
    assert_eq!(entry.time, 2);
    assert!(repo.runs_dir_is_empty());
}

/// A conflict stops the rebase; after resolving, `--continue` commits the conflicted pick (git
/// opens the editor for it) and the later reword still gets its own text.
#[test]
fn conflict_then_continue_keeps_messages_on_their_commits() {
    let repo = Repo::new();
    let base = repo.commit_file("f.txt", "base\n", "Base");
    let one = repo.commit_file("f.txt", "one\n", "One");
    let two = repo.commit_file("f.txt", "two\n", "Two");
    let three = repo.commit_file("g.txt", "g\n", "Three");
    let mut plan =
        Plan::from_commits(&[(one, "One".into()), (two.clone(), "Two".into()), (three, "Three".into())]);
    plan.move_step(1, 0); // Two before One: conflicts on f.txt
    plan.steps[2].action = Action::Reword;
    plan.steps[2].message = Some("Three reworded".into());

    let (session, stop) =
        rebase::start(&repo.git, &helper(), Some(&base), &repo.head(), &plan, "interactive rebase", "r5", 1)
            .expect("start");
    let Stop::Conflicts { at, .. } = stop else { panic!("expected conflicts, got {stop:?}") };
    assert_eq!(at, two);
    let mut stops = 0;
    let entry = loop {
        repo.write("f.txt", &format!("resolved {stops}\n"));
        repo.run(&["add", "f.txt"]);
        match rebase::resume(&repo.git, &session, 2).expect("continue") {
            Stop::Done(entry) => break entry.expect("entry"),
            Stop::Conflicts { .. } => stops += 1,
            other => panic!("unexpected {other:?}"),
        }
        assert!(stops < 3, "the rebase keeps stopping");
    };
    assert_eq!(repo.subjects(), ["Three reworded", "One", "Two", "Base"]);
    assert_eq!(entry.description, "interactive rebase");
}

/// Abort returns to where the rebase started and removes the run's files.
#[test]
fn abort_after_a_pause() {
    let repo = Repo::new();
    let (base, mut plan) = repo.with_commits(&["Alpha", "Bravo"]);
    let before = repo.head();
    plan.steps[0].action = Action::Edit;
    let (session, stop) =
        rebase::start(&repo.git, &helper(), Some(&base), &repo.head(), &plan, "x", "r6", 1).expect("start");
    assert!(matches!(stop, Stop::Paused { .. }), "{stop:?}");
    rebase::abort(&repo.git, &session).expect("abort");
    assert_eq!(repo.head(), before);
    assert!(repo.runs_dir_is_empty());
    assert_eq!(rebase::probe(&repo.git, &session, 3).expect("probe"), Stop::Done(None), "nothing to journal");
}

/// A dirty tree: git refuses to start, the error surfaces, nothing is left behind.
#[test]
fn a_dirty_tree_is_an_error() {
    let repo = Repo::new();
    let (base, mut plan) = repo.with_commits(&["Alpha"]);
    plan.steps[0].action = Action::Reword;
    plan.steps[0].message = Some("x".into());
    repo.write("f0.txt", "uncommitted");
    let err = rebase::start(&repo.git, &helper(), Some(&base), &repo.head(), &plan, "x", "r7", 1)
        .expect_err("dirty");
    let amalgum::git::ops::OpError::Git(e) = err else { panic!("expected git's refusal, got {err:?}") };
    assert!(e.stderr.contains("unstaged changes"), "{}", e.stderr);
    assert!(repo.runs_dir_is_empty());
}

/// A stand-in ssh host: `<ssh_bin> <host> -- <command>` with `ssh_bin = "sh"` and this script
/// as `<host>` runs `<command>` in a fresh environment, as sshd does.
fn fake_host(dir: &Path) -> String {
    let path = dir.join("fake-host.sh");
    let script = format!(
        "while [ \"$1\" != -- ]; do shift; done\n\
         exec env -i PATH=\"$PATH\" HOME={} GIT_CONFIG_NOSYSTEM=1 sh -c \"$2\"\n",
        amalgum::ssh::quote(&dir.display().to_string())
    );
    std::fs::write(&path, script).expect("write fake host");
    path.display().to_string()
}

/// The same flow over "ssh": the run's files are written and the helpers found where git runs.
#[test]
fn remote_rebase_through_a_stand_in_host() {
    let repo = Repo::new();
    let (base, mut plan) = repo.with_commits(&["Alpha", "Bravo", "Charlie"]);
    plan.move_step(2, 0);
    plan.steps[1].action = Action::Reword;
    plan.steps[1].message = Some("Alpha, remotely".into());
    plan.steps[2].action = Action::Drop;

    let host_dir = tempfile::tempdir().expect("tempdir");
    let mut remote = Git::new(Location::Remote {
        host: fake_host(host_dir.path()),
        path: repo.path().display().to_string(),
    });
    remote.ssh_bin = "sh".into();
    let (_, stop) =
        rebase::start(&remote, &helper(), Some(&base), &repo.head(), &plan, "interactive rebase", "r8", 1)
            .expect("start");
    done(stop);
    assert_eq!(repo.subjects(), ["Alpha, remotely", "Charlie", "Base"]);
    assert!(repo.runs_dir_is_empty());
}

/// Continue at an `edit` stop over an unstaged change: git refuses and the rebase stays where
/// it was. The stop carries git's error (§5.24), so the UI says why Continue did nothing.
#[test]
fn a_failed_continue_at_an_edit_stop_carries_gits_error() {
    let repo = Repo::new();
    let (base, mut plan) = repo.with_commits(&["Alpha", "Bravo"]);
    let alpha = plan.steps[0].hash.clone();
    plan.steps[0].action = Action::Edit;
    let (session, stop) =
        rebase::start(&repo.git, &helper(), Some(&base), &repo.head(), &plan, "x", "r11", 1).expect("start");
    assert_eq!(stop, Stop::Paused { at: alpha.clone(), error: None });

    repo.write("f0.txt", "changed, not staged");
    match rebase::resume(&repo.git, &session, 2).expect("still a stop") {
        Stop::Paused { at, error: Some(e) } => {
            assert_eq!(at, alpha);
            assert!(!e.stderr.trim().is_empty(), "git's reason is kept: {e:?}");
        }
        other => panic!("expected a paused stop with git's error, got {other:?}"),
    }
    rebase::abort(&repo.git, &session).expect("abort");
}

/// Starting while another rebase is stopped is refused up front: the second run would otherwise
/// take over the first one's state.
#[test]
fn start_refuses_while_another_rebase_is_in_progress() {
    let repo = Repo::new();
    let (base, mut plan) = repo.with_commits(&["Alpha", "Bravo"]);
    plan.steps[0].action = Action::Edit;
    let (session, _) =
        rebase::start(&repo.git, &helper(), Some(&base), &repo.head(), &plan, "x", "r12", 1).expect("start");
    let err = rebase::start(&repo.git, &helper(), Some(&base), &repo.head(), &plan, "x", "r13", 1)
        .expect_err("already rebasing");
    assert!(err.to_string().contains("already in progress"), "{err}");
    rebase::abort(&repo.git, &session).expect("abort");
    assert!(repo.runs_dir_is_empty());
}

/// A commit that lands after the plan was read (an agent committing while W9 is open) is never
/// dropped: `start` refuses a moved HEAD, and the sequence editor refuses a todo with commits
/// the plan doesn't list (the race between that check and git reading HEAD).
#[test]
fn a_stale_plan_never_drops_newer_commits() {
    let repo = Repo::new();
    let (base, mut plan) = repo.with_commits(&["Alpha", "Bravo"]);
    let read_at = repo.head();
    plan.steps[1].action = Action::Reword;
    plan.steps[1].message = Some("Bravo reworded".into());
    repo.commit_file("late.txt", "late", "Late agent commit");
    let head = repo.head();

    let err = rebase::start(&repo.git, &helper(), Some(&base), &read_at, &plan, "x", "r14", 1)
        .expect_err("HEAD moved");
    assert!(err.to_string().contains("moved"), "{err}");
    assert_eq!(repo.head(), head);

    // The same plan past the HEAD check: the helper sees git's todo and refuses.
    let err = rebase::start(&repo.git, &helper(), Some(&base), &head, &plan, "x", "r15", 1)
        .expect_err("todo has a commit the plan lacks");
    let amalgum::git::ops::OpError::Git(e) = err else { panic!("expected git's error, got {err:?}") };
    assert!(e.stderr.contains("doesn't list"), "{}", e.stderr);
    assert_eq!(repo.head(), head, "nothing rewritten");
    assert_eq!(repo.subjects(), ["Late agent commit", "Bravo", "Alpha", "Base"]);
    assert_eq!(amalgum::git::ops::in_progress(&repo.git), None, "no rebase left behind");
    assert!(repo.runs_dir_is_empty());
}

/// A reworded message keeps its `# Heading` lines: git's cleanup would strip them as comments.
#[test]
fn reword_keeps_lines_starting_with_a_hash() {
    let repo = Repo::new();
    let (base, mut plan) = repo.with_commits(&["Alpha", "Bravo"]);
    plan.steps[0].action = Action::Reword;
    plan.steps[0].message = Some("Alpha new\n\n# Notes\nbody".into());
    plan.steps[1].action = Action::Squash;
    plan.steps[1].message = Some("Alpha and Bravo\n\n# Why\n; and more".into());
    let (_, stop) =
        rebase::start(&repo.git, &helper(), Some(&base), &repo.head(), &plan, "x", "r16", 1).expect("start");
    done(stop);
    assert_eq!(repo.run(&["log", "-1", "--format=%B"]), "Alpha and Bravo\n\n# Why\n; and more");
}
