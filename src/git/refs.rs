//! Refs view data (§5.9–5.12, §5.21): branches and tags, stashes, worktrees, remotes.
//! Each `*_ARGS` constant is the exact git invocation its parser expects.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefKind {
    Local,
    Remote,
    Tag,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    /// Full name, `refs/heads/feat/x`.
    pub name: String,
    /// Display name: `feat/x`, `origin/main`, `v1.0`.
    pub short: String,
    pub kind: RefKind,
    /// Object the ref points at.
    pub target: String,
    /// Commit an annotated tag peels to.
    pub peeled: Option<String>,
    /// Short upstream name (`origin/main`).
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Upstream configured but deleted on the remote.
    pub gone: bool,
    /// Checked out in this worktree.
    pub is_head: bool,
    pub time: u64,
    pub subject: String,
}

/// `git for-each-ref` with a `%1f`-separated format, one ref per line.
pub const FOR_EACH_REF_ARGS: &[&str] = &[
    "for-each-ref",
    "--format=%(refname)%1f%(objectname)%1f%(*objectname)%1f%(upstream:short)%1f%(upstream:track,nobracket)%1f%(HEAD)%1f%(creatordate:unix)%1f%(contents:subject)",
    "refs/heads",
    "refs/remotes",
    "refs/tags",
];

/// Remote `HEAD` symrefs (`refs/remotes/origin/HEAD`) are skipped.
pub fn parse_refs(out: &str) -> Vec<Ref> {
    let mut refs = Vec::new();
    for line in out.lines() {
        if line.is_empty() {
            continue;
        }
        let mut f = line.split('\x1f');
        let (
            Some(name),
            Some(target),
            Some(peeled),
            Some(upstream),
            Some(track),
            Some(head),
            Some(time),
            Some(subject),
        ) = (f.next(), f.next(), f.next(), f.next(), f.next(), f.next(), f.next(), f.next())
        else {
            continue; // garbled line: skip rather than fail the whole batch
        };

        let (kind, short) = if let Some(s) = name.strip_prefix("refs/heads/") {
            (RefKind::Local, s)
        } else if let Some(s) = name.strip_prefix("refs/remotes/") {
            if s.ends_with("/HEAD") {
                continue; // the remote's default-branch symref, not a real ref
            }
            (RefKind::Remote, s)
        } else if let Some(s) = name.strip_prefix("refs/tags/") {
            (RefKind::Tag, s)
        } else {
            (RefKind::Other, name)
        };

        let (ahead, behind, gone) = parse_track(track);

        refs.push(Ref {
            name: name.to_string(),
            short: short.to_string(),
            kind,
            target: target.to_string(),
            peeled: (!peeled.is_empty()).then(|| peeled.to_string()),
            upstream: (!upstream.is_empty()).then(|| upstream.to_string()),
            ahead,
            behind,
            gone,
            is_head: head == "*",
            time: time.parse().unwrap_or(0),
            subject: subject.to_string(),
        });
    }
    refs
}

/// `%(upstream:track,nobracket)`: `"ahead 2, behind 1"`, `"ahead 2"`, `"behind 1"`, `"gone"`, `""`.
fn parse_track(track: &str) -> (u32, u32, bool) {
    if track == "gone" {
        return (0, 0, true);
    }
    let mut ahead = 0;
    let mut behind = 0;
    for part in track.split(',') {
        let part = part.trim();
        if let Some(n) = part.strip_prefix("ahead ") {
            ahead = n.parse().unwrap_or(0);
        } else if let Some(n) = part.strip_prefix("behind ") {
            behind = n.parse().unwrap_or(0);
        }
    }
    (ahead, behind, false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stash {
    pub index: usize,
    pub oid: String,
    pub time: u64,
    /// Message without the `On <branch>: ` / `WIP on <branch>: ` prefix.
    pub message: String,
    pub branch: Option<String>,
}

pub const STASH_ARGS: &[&str] = &["stash", "list", "-z", "--format=%gd%x1f%H%x1f%ct%x1f%gs"];

pub fn parse_stashes(out: &[u8]) -> Vec<Stash> {
    let mut stashes = Vec::new();
    let mut records: Vec<&[u8]> = out.split(|&b| b == b'\0').collect();
    if records.last().is_some_and(|r| r.is_empty()) {
        records.pop();
    }
    for record in records {
        if record.is_empty() {
            continue;
        }
        let line = String::from_utf8_lossy(record);
        let mut f = line.splitn(4, '\x1f');
        let (Some(gd), Some(oid), Some(ct), Some(gs)) = (f.next(), f.next(), f.next(), f.next()) else {
            continue;
        };
        let Some(index) =
            gd.strip_prefix("stash@{").and_then(|s| s.strip_suffix('}')).and_then(|n| n.parse().ok())
        else {
            continue;
        };
        let (branch, message) = split_stash_subject(gs);
        stashes.push(Stash { index, oid: oid.to_string(), time: ct.parse().unwrap_or(0), message, branch });
    }
    stashes
}

/// `%gs` is `"WIP on <branch>: <rest>"`, `"On <branch>: <rest>"`, or (rarely) neither. Ref names
/// can't contain `: `, so the first occurrence unambiguously ends the branch name.
fn split_stash_subject(gs: &str) -> (Option<String>, String) {
    for prefix in ["WIP on ", "On "] {
        if let Some(rest) = gs.strip_prefix(prefix)
            && let Some((branch, message)) = rest.split_once(": ")
        {
            return (Some(branch.to_string()), message.to_string());
        }
    }
    (None, gs.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: String,
    pub head: Option<String>,
    /// Short branch name; `None` when detached or bare.
    pub branch: Option<String>,
    pub bare: bool,
    pub detached: bool,
    pub locked: bool,
    pub prunable: bool,
}

pub const WORKTREE_ARGS: &[&str] = &["worktree", "list", "--porcelain", "-z"];

pub fn parse_worktrees(out: &[u8]) -> Vec<Worktree> {
    let text = String::from_utf8_lossy(out);
    let mut worktrees = Vec::new();
    let mut cur: Option<Worktree> = None;

    for field in text.split('\0') {
        if field.is_empty() {
            // Blank field: end of record (also matches the trailing NUL of the whole stream,
            // where `cur` is already `None` and this is a no-op).
            if let Some(wt) = cur.take() {
                worktrees.push(wt);
            }
            continue;
        }
        if let Some(path) = field.strip_prefix("worktree ") {
            if let Some(wt) = cur.take() {
                worktrees.push(wt); // defensive: a missing blank terminator shouldn't drop a record
            }
            cur = Some(Worktree {
                path: path.to_string(),
                head: None,
                branch: None,
                bare: false,
                detached: false,
                locked: false,
                prunable: false,
            });
        } else if let Some(wt) = cur.as_mut() {
            if let Some(h) = field.strip_prefix("HEAD ") {
                wt.head = Some(h.to_string());
            } else if let Some(b) = field.strip_prefix("branch ") {
                wt.branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
            } else if field == "bare" {
                wt.bare = true;
            } else if field == "detached" {
                wt.detached = true;
            } else if field == "locked" || field.starts_with("locked ") {
                wt.locked = true;
            } else if field == "prunable" || field.starts_with("prunable ") {
                wt.prunable = true;
            }
            // Unknown keys are skipped (forward compatible).
        }
    }
    if let Some(wt) = cur.take() {
        worktrees.push(wt);
    }
    worktrees
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub fetch_url: String,
    pub push_url: String,
}

pub const REMOTE_ARGS: &[&str] = &["remote", "-v"];

/// Parse `git remote -v` (one fetch and one push line per remote), in first-seen order. A
/// partial clone's fetch line carries an extra ` [<filter>]` suffix after the tag (e.g.
/// `... (fetch) [blob:none]`); it is stripped before the tag is split off, so the URL is never
/// swallowed into it.
pub fn parse_remotes(out: &str) -> Vec<Remote> {
    let mut remotes: Vec<Remote> = Vec::new();
    for line in out.lines() {
        let Some((name, rest)) = line.split_once('\t') else { continue };
        let rest = match rest.rsplit_once(' ') {
            Some((before_bracket, bracket)) if bracket.starts_with('[') && bracket.ends_with(']') => {
                before_bracket
            }
            _ => rest,
        };
        let Some((url, tag)) = rest.rsplit_once(' ') else { continue };
        let tag = tag.trim_matches(|c| c == '(' || c == ')');

        let entry = match remotes.iter().position(|r| r.name == name) {
            Some(i) => &mut remotes[i],
            None => {
                remotes.push(Remote {
                    name: name.to_string(),
                    fetch_url: String::new(),
                    push_url: String::new(),
                });
                remotes.last_mut().expect("just pushed")
            }
        };
        match tag {
            "fetch" => entry.fetch_url = url.to_string(),
            "push" => entry.push_url = url.to_string(),
            _ => {}
        }
    }
    remotes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempRepo;

    #[test]
    fn parses_local_remote_and_tag_refs_from_real_git() {
        let mut repo = TempRepo::new();
        repo.commit_file("a.txt", "1\n", "init commit");
        repo.git(&["tag", "v1.0"]);

        let out = repo.git(FOR_EACH_REF_ARGS);
        let refs = parse_refs(&out);

        let main = refs.iter().find(|r| r.name == "refs/heads/main").expect("main branch");
        assert_eq!(main.kind, RefKind::Local);
        assert_eq!(main.short, "main");
        assert!(main.is_head);
        assert_eq!(main.subject, "init commit");
        assert!(main.upstream.is_none());
        assert_eq!(main.ahead, 0);
        assert_eq!(main.behind, 0);
        assert!(!main.gone);

        let tag = refs.iter().find(|r| r.name == "refs/tags/v1.0").expect("tag");
        assert_eq!(tag.kind, RefKind::Tag);
        assert_eq!(tag.short, "v1.0");
        assert!(!tag.is_head);
    }

    #[test]
    fn local_branch_named_like_a_remote_keeps_its_full_short_name() {
        // `short` strips only the matched refs/heads|remotes|tags/ prefix, so a local branch
        // literally named `origin/x` stays `origin/x`, still kind `Local`.
        let mut repo = TempRepo::new();
        repo.commit_file("a.txt", "1\n", "init");
        repo.git(&["branch", "origin/x"]);

        let out = repo.git(FOR_EACH_REF_ARGS);
        let refs = parse_refs(&out);

        let r = refs.iter().find(|r| r.name == "refs/heads/origin/x").expect("branch");
        assert_eq!(r.kind, RefKind::Local);
        assert_eq!(r.short, "origin/x");
    }

    #[test]
    fn annotated_tag_has_peeled_commit_lightweight_does_not() {
        let mut repo = TempRepo::new();
        repo.commit_file("a.txt", "1\n", "c1");
        let commit = repo.git(&["rev-parse", "HEAD"]);
        repo.git(&["tag", "light"]);
        repo.git(&["tag", "-a", "-m", "annotated message", "ann"]);

        let out = repo.git(FOR_EACH_REF_ARGS);
        let refs = parse_refs(&out);

        let light = refs.iter().find(|r| r.short == "light").expect("lightweight tag");
        assert_eq!(light.peeled, None);
        assert_eq!(light.target, commit);

        let ann = refs.iter().find(|r| r.short == "ann").expect("annotated tag");
        assert_eq!(ann.peeled.as_deref(), Some(commit.as_str()));
        assert_eq!(ann.subject, "annotated message");
        assert_ne!(ann.target, commit); // target is the tag object itself, not the commit
    }

    #[test]
    fn skips_remote_head_symref() {
        let base = tempfile::tempdir().expect("tempdir");
        let remote = base.path().join("remote.git");
        let out = crate::testutil::hermetic_git(base.path())
            .args(["init", "-q", "-b", "main", "--bare", remote.to_str().expect("utf8 path")])
            .output()
            .expect("spawn git");
        assert!(out.status.success());

        let mut seed = TempRepo::new();
        seed.git(&["remote", "add", "origin", remote.to_str().expect("utf8 path")]);
        seed.commit_file("a.txt", "1\n", "c1");
        seed.git(&["push", "-q", "origin", "main"]);

        let clone_dir = base.path().join("clone");
        let out = crate::testutil::hermetic_git(base.path())
            .args([
                "clone",
                "-q",
                remote.to_str().expect("utf8 path"),
                clone_dir.to_str().expect("utf8 path"),
            ])
            .output()
            .expect("spawn git");
        assert!(out.status.success());
        // `origin/HEAD` is only created by `git remote set-head`, which `clone` runs implicitly
        // when the remote is non-empty at clone time.
        let out2 =
            crate::testutil::hermetic_git(&clone_dir).args(FOR_EACH_REF_ARGS).output().expect("spawn git");
        assert!(out2.status.success());
        let text = String::from_utf8_lossy(&out2.stdout);

        let refs = parse_refs(&text);
        assert!(!refs.iter().any(|r| r.name == "refs/remotes/origin/HEAD"), "origin/HEAD leaked through");
        assert!(refs.iter().any(|r| r.name == "refs/remotes/origin/main"));
    }

    #[test]
    fn parses_ahead_behind_and_gone_tracking() {
        let ahead_behind =
            "refs/heads/main\x1fabc\x1f\x1forigin/main\x1fahead 2, behind 1\x1f*\x1f1700000000\x1fc1\n";
        let refs = parse_refs(ahead_behind);
        assert_eq!(refs[0].ahead, 2);
        assert_eq!(refs[0].behind, 1);
        assert!(!refs[0].gone);

        let gone = "refs/heads/main\x1fabc\x1f\x1forigin/main\x1fgone\x1f*\x1f1700000000\x1fc1\n";
        let refs = parse_refs(gone);
        assert!(refs[0].gone);
        assert_eq!(refs[0].ahead, 0);
        assert_eq!(refs[0].behind, 0);

        let no_upstream = "refs/heads/main\x1fabc\x1f\x1f\x1f\x1f*\x1f1700000000\x1fc1\n";
        let refs = parse_refs(no_upstream);
        assert_eq!(refs[0].upstream, None);
        assert_eq!(refs[0].ahead, 0);
        assert_eq!(refs[0].behind, 0);
    }

    #[test]
    fn other_kind_is_the_fallback_for_an_unexpected_prefix() {
        let line = "refs/notes/commits\x1fabc\x1f\x1f\x1f\x1f \x1f1700000000\x1f\n";
        let refs = parse_refs(line);
        assert_eq!(refs[0].kind, RefKind::Other);
        assert_eq!(refs[0].short, "refs/notes/commits");
        assert_eq!(refs[0].subject, "");
    }

    #[test]
    fn empty_subject_is_preserved() {
        let repo = TempRepo::new();
        repo.git(&["commit", "-q", "--allow-empty", "--allow-empty-message", "-m", ""]);
        let out = repo.git(FOR_EACH_REF_ARGS);
        let refs = parse_refs(&out);
        assert_eq!(refs[0].subject, "");
    }

    // --- stashes ---------------------------------------------------------

    #[test]
    fn parses_stash_list_wip_and_named() {
        let mut repo = TempRepo::new();
        repo.commit_file("a.txt", "1\n", "c1");
        repo.write("a.txt", "2\n");
        repo.git(&["stash", "push", "-q", "-m", "my message"]);
        repo.write("a.txt", "3\n");
        let out = crate::testutil::hermetic_git(repo.path())
            .args(["stash", "push", "-q"])
            .output()
            .expect("spawn git");
        assert!(out.status.success());

        let raw = repo.git_raw(STASH_ARGS);
        let stashes = parse_stashes(&raw);

        assert_eq!(stashes.len(), 2);
        // Most recent push is stash@{0}.
        assert_eq!(stashes[0].index, 0);
        assert_eq!(stashes[0].branch.as_deref(), Some("main"));
        assert!(stashes[0].message.starts_with("main") || stashes[0].message.contains("c1"));

        assert_eq!(stashes[1].index, 1);
        assert_eq!(stashes[1].branch.as_deref(), Some("main"));
        assert_eq!(stashes[1].message, "my message");
    }

    #[test]
    fn stash_subject_without_a_recognised_prefix_has_no_branch() {
        let (branch, message) = split_stash_subject("custom entry with no prefix");
        assert_eq!(branch, None);
        assert_eq!(message, "custom entry with no prefix");
    }

    // --- worktrees ---------------------------------------------------------

    #[test]
    fn parses_worktrees_with_locked_and_detached() {
        let mut repo = TempRepo::new();
        repo.commit_file("a.txt", "1\n", "c1");
        let base = tempfile::tempdir().expect("tempdir");
        let wt1 = base.path().join("wt1");
        let wt2 = base.path().join("wt2");
        repo.git(&["worktree", "add", "-q", "-b", "feat", wt1.to_str().expect("utf8 path")]);
        repo.git(&["worktree", "add", "-q", "--detach", wt2.to_str().expect("utf8 path")]);
        repo.git(&["worktree", "lock", wt1.to_str().expect("utf8 path"), "--reason", "testing"]);

        let raw = repo.git_raw(WORKTREE_ARGS);
        let worktrees = parse_worktrees(&raw);
        // git reports canonical paths (macOS temp dirs live under the /var -> /private/var symlink).
        let (wt1, wt2) = (wt1.canonicalize().expect("wt1"), wt2.canonicalize().expect("wt2"));

        assert_eq!(worktrees.len(), 3);
        let main = &worktrees[0];
        assert_eq!(main.branch.as_deref(), Some("main"));
        assert!(!main.bare && !main.detached && !main.locked);

        let locked = worktrees.iter().find(|w| w.path == wt1.to_str().expect("utf8 path")).expect("wt1");
        assert_eq!(locked.branch.as_deref(), Some("feat"));
        assert!(locked.locked);
        assert!(!locked.detached);

        let detached = worktrees.iter().find(|w| w.path == wt2.to_str().expect("utf8 path")).expect("wt2");
        assert!(detached.detached);
        assert_eq!(detached.branch, None);
    }

    #[test]
    fn parses_bare_worktree() {
        // Hand-written: a bare repo's own `worktree list --porcelain -z` has no HEAD/branch line.
        let out = b"worktree /repo.git\0bare\0\0";
        let worktrees = parse_worktrees(out);
        assert_eq!(worktrees.len(), 1);
        assert!(worktrees[0].bare);
        assert_eq!(worktrees[0].head, None);
        assert_eq!(worktrees[0].branch, None);
    }

    #[test]
    fn parses_prunable_worktree() {
        let out = b"worktree /tmp/gone\0HEAD abc123\0detached\0prunable gitdir file points to non-existent location\0\0";
        let worktrees = parse_worktrees(out);
        assert_eq!(worktrees.len(), 1);
        assert!(worktrees[0].prunable);
        assert!(worktrees[0].detached);
    }

    // --- remotes ---------------------------------------------------------

    #[test]
    fn parses_remotes_with_a_different_push_url() {
        let repo = TempRepo::new();
        repo.git(&["remote", "add", "origin", "https://example.com/repo.git"]);
        repo.git(&["remote", "add", "upstream", "https://example.com/upstream.git"]);
        repo.git(&["remote", "set-url", "--push", "origin", "https://example.com/repo-push.git"]);

        let out = repo.git(REMOTE_ARGS);
        let remotes = parse_remotes(&out);

        assert_eq!(remotes.len(), 2);
        assert_eq!(remotes[0].name, "origin");
        assert_eq!(remotes[0].fetch_url, "https://example.com/repo.git");
        assert_eq!(remotes[0].push_url, "https://example.com/repo-push.git");
        assert_eq!(remotes[1].name, "upstream");
        assert_eq!(remotes[1].fetch_url, remotes[1].push_url);
    }

    #[test]
    fn parses_remotes_empty_output() {
        assert_eq!(parse_remotes(""), Vec::new());
    }

    #[test]
    fn parses_remotes_of_a_partial_clone_keeps_fetch_url_despite_filter_suffix() {
        // Regression: `git remote -v` prints an extra ` [<filter>]` after the `(fetch)` tag for
        // a partial clone's fetch line; it must not be swallowed into the URL or the tag.
        let base = tempfile::tempdir().expect("tempdir");
        let remote = base.path().join("remote.git");
        let out = crate::testutil::hermetic_git(base.path())
            .args(["init", "-q", "-b", "main", "--bare", remote.to_str().expect("utf8 path")])
            .output()
            .expect("spawn git");
        assert!(out.status.success());

        let mut seed = TempRepo::new();
        seed.git(&["remote", "add", "origin", remote.to_str().expect("utf8 path")]);
        seed.commit_file("a.txt", "1\n", "c1");
        seed.git(&["push", "-q", "origin", "main"]);

        let clone_dir = base.path().join("clone");
        let remote_url = format!("file://{}", remote.to_str().expect("utf8 path"));
        let out = crate::testutil::hermetic_git(base.path())
            .args(["clone", "-q", "--filter=blob:none", &remote_url, clone_dir.to_str().expect("utf8 path")])
            .output()
            .expect("spawn git");
        assert!(out.status.success(), "clone failed:\n{}", String::from_utf8_lossy(&out.stderr));

        let out2 = crate::testutil::hermetic_git(&clone_dir).args(REMOTE_ARGS).output().expect("spawn git");
        assert!(out2.status.success());
        let text = String::from_utf8_lossy(&out2.stdout);
        assert!(text.contains("[blob:none]"), "sanity: the filter tag is really in git's output: {text}");

        let remotes = parse_remotes(&text);
        let origin = remotes.iter().find(|r| r.name == "origin").expect("origin remote");
        assert_eq!(origin.fetch_url, remote_url);
        assert_eq!(origin.push_url, remote_url);
    }
}
