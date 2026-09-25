---
name: add-git-operation
description: How to add or change anything that runs git in Amalgum — a git pane action (checkout, merge, rebase, cherry-pick, reset, stash, tag, push/pull/fetch, worktree), a view that reads repo data (log, refs, blame, status, diff), or a parser for git output. Use it even when the request is phrased as a UI feature ("add a Revert button", "show tags in the sidebar") or as a symptom ("the graph shows the wrong branch", "status breaks on filenames with spaces", "undo did nothing"). Not for ssh connection handling (src/ssh.rs).
---

# Adding a git operation

Git runs as the system binary, locally or over ssh, through one runner. Never `Command::new("git")`.

## Where things go
- Runner: `git::Git::run` / `run_with_stdin` (`src/git/cmd.rs`). The same call works for `Location::Remote`.
- Argv: a `pub const *_ARGS` (or `fn *_args`) next to its parser in `src/git/<area>.rs`.
- Parser: `pub fn parse_*(out: &[u8] | &str) -> …`, pure, no I/O. Exemplars: `git/status.rs`, `git/refs.rs`.
- Ref-changing operations record a `git::journal::Entry`: `before`/`after` snapshots, the `forward` plan, and
  the `inverse` plan (`None` only for push). Each SPEC §5 flow's "Undo" line says what the inverse is.
- Destructive operations show the §5.20 confirmation, naming what is lost (commit subjects, paths).

## Rules
- Machine formats only: `-z`, `--porcelain=v2`, `--format` with `%x1f`/`%x00` separators, `--decorate=full`.
  Human output varies with locale and version.
- Paths may contain spaces, newlines, quotes, and non-UTF-8 bytes: use `-z` and `String::from_utf8_lossy`.
- Messages and patches go to git on stdin (`run_with_stdin`), never as arguments or remote temp files (§5.7).
- Remote arguments are quoted by the runner via `ssh::quote`; do not pre-quote.
- Add a pattern to `GitError::summary` when you introduce a new known failure (§5.24 toast text).
- The UI thread never waits on git: run it on a worker, deliver the result by message.

## Test (TDD)
```rust
#[test]
fn parses_detached_head() {
    let mut repo = crate::testutil::TempRepo::new(); // hermetic: ignores global git config
    let first = repo.commit_file("a.txt", "1", "first");
    repo.commit_file("a.txt", "2", "second");
    repo.git(&["checkout", "-q", &first]);
    let s = parse(&repo.git_raw(STATUS_ARGS)).unwrap();
    assert_eq!(s.branch.head, None);
}
```
Generate real git output inside the test; add hand-written cases for what you cannot easily produce.
Finish with `scripts/agent/verify`.
