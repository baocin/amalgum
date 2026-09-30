//! The blame view's state, headless (§5.15, W12): gutter text, the `blame.hot` → `blame.cold`
//! heat of each commit, and the blame stack behind "blame back" (`Mod+←`), "step forward"
//! (`Mod+→`), and the breadcrumb.

use crate::git::blame::{Blame, BlameCommit};

/// One blame in the stack: `path` at `rev` (`None`: the working tree), with the line the user
/// is on (0-based), so stepping back and forth keeps their place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub rev: Option<String>,
    pub path: String,
    pub line: usize,
}

impl Frame {
    pub fn new(rev: Option<String>, path: impl Into<String>, line: usize) -> Self {
        Self { rev, path: path.into(), line }
    }

    /// Breadcrumb label: `working tree`, a short hash, or the rev as given (`main`, `HEAD`).
    pub fn label(&self) -> String {
        match &self.rev {
            None => "working tree".to_string(),
            Some(rev) => short_rev(rev),
        }
    }

    /// Same file at the same revision (the line may differ).
    pub fn same_target(&self, other: &Frame) -> bool {
        self.rev == other.rev && self.path == other.path
    }
}

/// A full hex id shortened to 7 characters; anything else (a ref name) as is.
pub fn short_rev(rev: &str) -> String {
    let is_id = rev.len() >= 40 && rev.bytes().all(|b| b.is_ascii_hexdigit());
    if is_id { rev[..7].to_string() } else { rev.to_string() }
}

/// Frames from the first blame (index 0, newest) to the deepest "blame back"; `pos` is the one
/// shown. Stepping forward keeps the deeper frames, so `Mod+←` on the same line goes back to
/// them without re-reading, and the breadcrumb can jump to them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameStack {
    frames: Vec<Frame>,
    pos: usize,
}

impl BlameStack {
    pub fn new(first: Frame) -> Self {
        Self { frames: vec![first], pos: 0 }
    }

    pub fn current(&self) -> &Frame {
        &self.frames[self.pos]
    }

    pub fn current_mut(&mut self) -> &mut Frame {
        &mut self.frames[self.pos]
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }

    /// "Blame back" to `frame` (the parent of a line's commit). If that is the next deeper
    /// frame already, move there keeping its place; otherwise the deeper frames are replaced.
    pub fn push(&mut self, frame: Frame) {
        if self.frames.get(self.pos + 1).is_some_and(|next| next.same_target(&frame)) {
            self.pos += 1;
            self.frames[self.pos].line = frame.line;
            return;
        }
        self.frames.truncate(self.pos + 1);
        self.frames.push(frame);
        self.pos += 1;
    }

    /// `Mod+→`: one step toward the first (newest) blame. `false` at the first.
    pub fn forward(&mut self) -> bool {
        if self.pos == 0 {
            return false;
        }
        self.pos -= 1;
        true
    }

    /// Back to a deeper frame already visited (the breadcrumb, or re-doing a step).
    pub fn deeper(&mut self) -> bool {
        if self.pos + 1 >= self.frames.len() {
            return false;
        }
        self.pos += 1;
        true
    }

    /// Breadcrumb click. `false` for an index out of range.
    pub fn jump(&mut self, index: usize) -> bool {
        if index >= self.frames.len() {
            return false;
        }
        self.pos = index;
        true
    }

    /// `(label, is_current)` for each frame, first blame first (W12 "main › 3c4d › ab12").
    pub fn breadcrumb(&self) -> Vec<(String, bool)> {
        self.frames.iter().enumerate().map(|(i, f)| (f.label(), i == self.pos)).collect()
    }
}

/// Why "blame back" has nowhere to go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackRefusal {
    NoLine,
    /// An uncommitted line of a file `HEAD` doesn't have (a new file).
    NotCommitted,
    /// The line's commit introduced the file (or is a boundary): nothing earlier to blame.
    Introduced {
        commit: String,
    },
}

impl BackRefusal {
    pub fn message(&self) -> String {
        match self {
            BackRefusal::NoLine => "Select a line to blame back from".to_string(),
            BackRefusal::NotCommitted => {
                "This line is not committed yet: nothing earlier to blame".to_string()
            }
            BackRefusal::Introduced { commit } => {
                format!("{commit} added this line: nothing earlier to blame")
            }
        }
    }
}

/// The frame "blame back" (`Mod+←`) opens for `line`: its commit's parent, at the file's name
/// there (git's `previous`, so renames are followed), on the line's number in that commit. An
/// uncommitted line goes to `HEAD`.
pub fn back_target(blame: &Blame, line: usize) -> Result<Frame, BackRefusal> {
    let row = blame.lines.get(line).ok_or(BackRefusal::NoLine)?;
    let commit = &blame.commits[row.commit];
    match &commit.previous {
        Some((parent, name)) => {
            Ok(Frame::new(Some(parent.clone()), name.clone(), row.orig_line.saturating_sub(1) as usize))
        }
        None if commit.is_uncommitted() => Err(BackRefusal::NotCommitted),
        None => Err(BackRefusal::Introduced { commit: short_rev(&commit.id) }),
    }
}

/// Heat per commit (indexed like `blame.commits`): 1.0 for the newest (`blame.hot`), 0.0 for the
/// oldest (`blame.cold`), spaced by age rank so one very old commit doesn't wash out the rest.
/// Uncommitted lines are the hottest.
pub fn heat(blame: &Blame) -> Vec<f32> {
    let mut times: Vec<u64> =
        blame.commits.iter().filter(|c| !c.is_uncommitted()).map(|c| c.author_time).collect();
    times.sort_unstable();
    times.dedup();
    let steps = times.len().saturating_sub(1) as f32;
    blame
        .commits
        .iter()
        .map(|c| {
            if c.is_uncommitted() || steps == 0.0 {
                return 1.0;
            }
            let rank = times.binary_search(&c.author_time).unwrap_or(0) as f32;
            rank / steps
        })
        .collect()
}

/// Gutter cell text (W12 "ab12 mp 2h"): short hash, author, relative date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gutter {
    pub hash: String,
    pub author: String,
    pub when: String,
}

/// Author names longer than this are cut with `…`.
pub const AUTHOR_CHARS: usize = 14;

pub fn gutter(commit: &BlameCommit, now: u64) -> Gutter {
    if commit.is_uncommitted() {
        return Gutter { hash: "·······".into(), author: "Uncommitted".into(), when: String::new() };
    }
    let author = if commit.author.chars().count() > AUTHOR_CHARS {
        let cut: String = commit.author.chars().take(AUTHOR_CHARS - 1).collect();
        format!("{cut}…")
    } else {
        commit.author.clone()
    };
    Gutter { hash: short_rev(&commit.id), author, when: crate::util::relative_time(now, commit.author_time) }
}

/// The gutter tooltip (§5.15 "Hover a gutter cell shows the full commit message"): header line,
/// then `message` (the full message once loaded, else the summary).
pub fn hover_text(commit: &BlameCommit, message: Option<&str>, now: u64) -> String {
    if commit.is_uncommitted() {
        return "Not committed yet".to_string();
    }
    let header = format!(
        "{} · {} <{}> · {} ago",
        short_rev(&commit.id),
        commit.author,
        commit.author_mail,
        crate::util::relative_time(now, commit.author_time)
    );
    let body = message.map(str::trim_end).unwrap_or(&commit.summary);
    format!("{header}\n\n{body}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::blame::{BlameLine, UNCOMMITTED};

    fn commit(id: &str, time: u64) -> BlameCommit {
        BlameCommit { id: id.into(), author: "Ada".into(), author_time: time, ..BlameCommit::default() }
    }

    fn blame_of(commits: Vec<BlameCommit>) -> Blame {
        let lines = (0..commits.len())
            .map(|i| BlameLine {
                commit: i,
                orig_line: i as u32 + 1,
                final_line: i as u32 + 1,
                text: String::new(),
            })
            .collect();
        Blame { commits, lines }
    }

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn heat_runs_from_cold_oldest_to_hot_newest_by_rank() {
        let b = blame_of(vec![commit("n", 1000), commit("o", 10), commit("m", 999), commit("o2", 10)]);
        assert_eq!(heat(&b), [1.0, 0.0, 0.5, 0.0]);
    }

    #[test]
    fn heat_of_one_commit_or_uncommitted_is_hot() {
        assert_eq!(heat(&blame_of(vec![commit("a", 5)])), [1.0]);
        let b = blame_of(vec![commit(UNCOMMITTED, 0), commit("a", 5), commit("b", 9)]);
        assert_eq!(heat(&b), [1.0, 0.0, 1.0]);
        assert!(heat(&Blame::default()).is_empty());
    }

    #[test]
    fn heat_against_a_real_blame() {
        let mut repo = crate::testutil::TempRepo::new();
        repo.commit_file("f", "1\n2\n3\n", "one");
        repo.commit_file("f", "1\n2 b\n3\n", "two");
        repo.commit_file("f", "1\n2 b\n3 c\n", "three");
        let git = crate::git::Git::new(crate::git::Location::Local { path: repo.path().to_path_buf() });
        let b = crate::git::blame::run(&git, None, "f").unwrap();
        let per_line: Vec<f32> = b.lines.iter().map(|l| heat(&b)[l.commit]).collect();
        assert_eq!(per_line, [0.0, 0.5, 1.0]);
    }

    #[test]
    fn back_target_is_the_parent_at_its_name_and_refuses_at_the_introduction() {
        let mut child = commit(B, 2);
        child.previous = Some((A.into(), "old name.rs".into()));
        let mut b = blame_of(vec![commit(A, 1), child]);
        b.lines[1].orig_line = 7;
        assert_eq!(back_target(&b, 1), Ok(Frame::new(Some(A.into()), "old name.rs", 6)));
        assert_eq!(back_target(&b, 0), Err(BackRefusal::Introduced { commit: "aaaaaaa".into() }));
        assert_eq!(back_target(&b, 9), Err(BackRefusal::NoLine));
        assert!(BackRefusal::Introduced { commit: "aaaaaaa".into() }.message().contains("aaaaaaa"));
    }

    #[test]
    fn blame_back_through_real_history_with_a_rename() {
        let mut repo = crate::testutil::TempRepo::new();
        let c1 = repo.commit_file("old.txt", "a\nb\n", "one");
        repo.git(&["mv", "old.txt", "new.txt"]);
        repo.commit("move");
        let c3 = repo.commit_file("new.txt", "a\nB\n", "three");
        repo.write("new.txt", "a\nB\nlocal\n");
        let git = crate::git::Git::new(crate::git::Location::Local { path: repo.path().to_path_buf() });

        let wt = crate::git::blame::run(&git, None, "new.txt").unwrap();
        let head = back_target(&wt, 2).expect("uncommitted → HEAD");
        assert_eq!(head.rev.as_deref(), Some(c3.as_str()));
        let at3 = crate::git::blame::run(&git, head.rev.as_deref(), &head.path).unwrap();
        let parent = back_target(&at3, 1).expect("line B was changed in c3");
        assert_eq!(parent.path, "new.txt");
        let before = crate::git::blame::run(&git, parent.rev.as_deref(), &parent.path).unwrap();
        assert_eq!(before.lines[1].text, "b");
        assert_eq!(before.commits[before.lines[1].commit].id, c1);
        assert_eq!(before.commits[before.lines[1].commit].filename, "old.txt");
        assert!(matches!(back_target(&before, 1), Err(BackRefusal::Introduced { .. })));
    }

    #[test]
    fn back_from_an_uncommitted_line_of_a_new_file_says_so() {
        let mut repo = crate::testutil::TempRepo::new();
        repo.commit_file("a", "x\n", "one");
        repo.write("new.txt", "fresh\n");
        repo.git(&["add", "new.txt"]);
        let git = crate::git::Git::new(crate::git::Location::Local { path: repo.path().to_path_buf() });
        let b = crate::git::blame::run(&git, None, "new.txt").unwrap();
        assert_eq!(back_target(&b, 0), Err(BackRefusal::NotCommitted));
        assert!(!BackRefusal::NotCommitted.message().contains("0000000"));
    }

    #[test]
    fn stack_steps_back_forward_and_keeps_deeper_frames() {
        let mut s = BlameStack::new(Frame::new(None, "f", 3));
        assert!(!s.forward());
        s.push(Frame::new(Some(B.into()), "f", 2));
        s.push(Frame::new(Some(A.into()), "f", 1));
        assert_eq!(s.pos(), 2);
        assert_eq!(
            s.breadcrumb(),
            [("working tree".to_string(), false), ("bbbbbbb".into(), false), ("aaaaaaa".into(), true)]
        );
        assert!(s.forward());
        assert!(s.forward());
        assert_eq!(s.current().line, 3, "the first frame kept its line");
        // Blame back to the same parent again: reuse the frame, no truncation.
        s.push(Frame::new(Some(B.into()), "f", 5));
        assert_eq!((s.pos(), s.frames().len(), s.current().line), (1, 3, 5));
        assert!(s.deeper());
        assert!(!s.deeper());
        // A different target replaces the deeper frames.
        s.jump(0);
        s.push(Frame::new(Some("main".into()), "f", 0));
        assert_eq!(s.frames().len(), 2);
        assert_eq!(s.current().label(), "main");
        assert!(!s.jump(5));
    }

    #[test]
    fn gutter_and_hover_text() {
        let mut c = commit(A, 1_000);
        c.author = "Margaret Hamilton-Smythe".into();
        c.author_mail = "mh@x".into();
        c.summary = "Fix it".into();
        let g = gutter(&c, 1_000 + 7_200);
        assert_eq!(g, Gutter { hash: "aaaaaaa".into(), author: "Margaret Hami…".into(), when: "2h".into() });
        assert_eq!(gutter(&commit(UNCOMMITTED, 0), 5).author, "Uncommitted");
        let hover = hover_text(&c, Some("Fix it\n\nBecause.\n"), 1_000 + 60);
        assert_eq!(hover, "aaaaaaa · Margaret Hamilton-Smythe <mh@x> · 1m ago\n\nFix it\n\nBecause.");
        assert!(hover_text(&c, None, 1_060).ends_with("\n\nFix it"));
        assert_eq!(hover_text(&commit(UNCOMMITTED, 0), None, 0), "Not committed yet");
    }
}
