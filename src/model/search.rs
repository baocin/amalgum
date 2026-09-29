//! Commit search state (§5.8, W7), headless: the query field, the last 20 queries recalled with
//! `↑` in an empty field, when a query is due to run (at once over the local corpus, after a
//! 300 ms pause on a remote repository), stale-result guarding, the result cursor, and where
//! `Esc` / `Enter` leave the graph's selection.
//!
//! Matching itself is [`crate::git::search::Query::matches`]; [`Record`] and [`filter`] adapt
//! a commit (plus its message body and changed paths) to it. Local repositories search the
//! commits the graph has loaded (read once per search with their paths, on a worker); the
//! on-disk index of §5.8 is not built yet. Remote repositories run `git log` with
//! [`Query::git_log_args`] and refine the answer with [`Query::matches`].

use crate::git::log::{Commit, Decoration};
use crate::git::search::{Doc, Found, Query};
use std::collections::VecDeque;
use std::sync::Arc;

/// `↑` in an empty field recalls this many past queries (§5.8).
pub const HISTORY_LIMIT: usize = 20;
/// Remote searches wait for the typing to pause this long (seconds) before running git (§5.8).
pub const REMOTE_DEBOUNCE: f64 = 0.3;
/// A remote search reads at most this many commits from `git log`.
pub const REMOTE_LIMIT: usize = 1000;

/// A commit prepared for matching: branch and tag names split out of its decorations once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub commit: Commit,
    pub body: String,
    pub paths: Vec<String>,
    branches: Vec<String>,
    tags: Vec<String>,
}

impl Record {
    pub fn new(found: Found) -> Self {
        let mut branches = Vec::new();
        let mut tags = Vec::new();
        for d in &found.commit.refs {
            match d {
                Decoration::CurrentBranch(n) | Decoration::Branch(n) | Decoration::Remote(n) => {
                    branches.push(n.clone())
                }
                Decoration::Tag(n) => tags.push(n.clone()),
                Decoration::Head | Decoration::Other(_) => {}
            }
        }
        Self { commit: found.commit, body: found.body, paths: found.paths, branches, tags }
    }

    pub fn doc(&self) -> Doc<'_> {
        Doc {
            id: &self.commit.id,
            subject: &self.commit.subject,
            body: &self.body,
            author: &self.commit.author,
            email: &self.commit.email,
            time: self.commit.time,
            branches: &self.branches,
            tags: &self.tags,
            paths: &self.paths,
        }
    }
}

/// The commits of `records` that `query` matches, in their (graph) order.
pub fn filter(query: &Query, records: &[Record]) -> Vec<Commit> {
    records.iter().filter(|r| query.matches(&r.doc())).map(|r| r.commit.clone()).collect()
}

/// Past queries, newest first, without duplicates, at most [`HISTORY_LIMIT`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct History {
    items: VecDeque<String>,
}

impl History {
    pub fn push(&mut self, query: &str) {
        let query = query.trim();
        if query.is_empty() {
            return;
        }
        self.items.retain(|q| q != query);
        self.items.push_front(query.to_string());
        self.items.truncate(HISTORY_LIMIT);
    }

    /// `0` is the newest.
    pub fn get(&self, i: usize) -> Option<&str> {
        self.items.get(i).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Where the query runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Over the loaded commits, on a worker, as soon as the text changes.
    Local,
    /// `git log` with the query's filters, after [`REMOTE_DEBOUNCE`].
    Remote,
}

/// A search the caller should run now on a worker; its answer goes to [`Search::accept`].
#[derive(Debug, Clone)]
pub enum Job {
    /// Match `query` against `corpus`.
    Local { req: u64, query: Query, corpus: Arc<Vec<Record>> },
    /// Read the corpus first: [`Search::set_corpus`] with the result, keyed by `key`.
    LoadCorpus { key: u64 },
    /// `git log` with `query`'s filters, then [`filter`].
    Remote { req: u64, query: Query },
}

/// The search field above the graph and its results.
#[derive(Debug, Clone)]
pub struct Search {
    mode: Mode,
    open: bool,
    pub text: String,
    history: History,
    /// Which history entry `text` was recalled from, while it is unedited.
    recall: Option<usize>,
    /// The graph's selection when search opened, restored by `Esc`.
    prev_selection: Option<String>,
    results: Option<Vec<Commit>>,
    cursor: usize,
    /// The id of the last query dispatched; answers to older ones are dropped.
    req: u64,
    /// When the current text is due to run (frame time, seconds).
    due: Option<f64>,
    /// The local corpus and what it was read for (see [`Search::corpus_key`]).
    corpus: Option<(u64, Arc<Vec<Record>>)>,
    corpus_loading: Option<u64>,
    /// Ask the UI to focus the field on its next frame.
    pub focus_field: bool,
}

impl Search {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            open: false,
            text: String::new(),
            history: History::default(),
            recall: None,
            prev_selection: None,
            results: None,
            cursor: 0,
            req: 0,
            due: None,
            corpus: None,
            corpus_loading: None,
            focus_field: false,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Whether results replace the graph rows: the field holds a query.
    pub fn showing_results(&self) -> bool {
        self.open && !Query::parse(&self.text).is_empty()
    }

    pub fn results(&self) -> Option<&[Commit]> {
        self.results.as_deref()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn history(&self) -> &History {
        &self.history
    }

    /// `/`, `Mod+F`, or the palette: show the field and focus it. `selected` is the graph's
    /// selection, restored by `Esc`. Opening an already open search only refocuses it.
    pub fn open(&mut self, selected: Option<String>) {
        self.focus_field = true;
        if self.open {
            return;
        }
        self.open = true;
        self.text.clear();
        self.recall = None;
        self.results = None;
        self.cursor = 0;
        self.due = None;
        self.prev_selection = selected;
    }

    /// The field's text changed (typing); schedules the query.
    pub fn edited(&mut self, now: f64) {
        self.recall = None;
        self.schedule(now);
    }

    fn schedule(&mut self, now: f64) {
        self.req += 1; // whatever is in flight answers an older text
        if Query::parse(&self.text).is_empty() {
            self.results = None;
            self.due = None;
            return;
        }
        self.due = Some(match self.mode {
            Mode::Local => now,
            Mode::Remote => now + REMOTE_DEBOUNCE,
        });
    }

    /// When the next [`Search::poll`] has work, for a repaint request.
    pub fn due(&self) -> Option<f64> {
        self.due
    }

    /// The job to run now, if the pending query is due. `corpus_key` names what the local
    /// corpus must cover (the graph's generation and loaded count): a different key reloads it.
    pub fn poll(&mut self, now: f64, corpus_key: u64) -> Option<Job> {
        let due = self.due?;
        if now < due {
            return None;
        }
        match self.mode {
            Mode::Remote => {
                self.due = None;
                Some(Job::Remote { req: self.req, query: Query::parse(&self.text) })
            }
            Mode::Local => match &self.corpus {
                Some((key, corpus)) if *key == corpus_key => {
                    self.due = None;
                    Some(Job::Local {
                        req: self.req,
                        query: Query::parse(&self.text),
                        corpus: Arc::clone(corpus),
                    })
                }
                _ if self.corpus_loading == Some(corpus_key) => None,
                _ => {
                    self.corpus_loading = Some(corpus_key);
                    Some(Job::LoadCorpus { key: corpus_key })
                }
            },
        }
    }

    /// The corpus read for `key`; the pending query runs over it on the next poll.
    pub fn set_corpus(&mut self, key: u64, records: Vec<Record>) {
        if self.corpus_loading == Some(key) {
            self.corpus_loading = None;
        }
        self.corpus = Some((key, Arc::new(records)));
    }

    /// Reading the corpus failed: stop waiting for it (the caller shows the error).
    pub fn corpus_failed(&mut self, key: u64) {
        if self.corpus_loading == Some(key) {
            self.corpus_loading = None;
            self.due = None;
        }
    }

    /// Whether a query is typed but its answer has not arrived yet.
    pub fn searching(&self) -> bool {
        self.showing_results() && (self.due.is_some() || self.results.is_none())
    }

    /// The answer to query `req`; `false` (dropped) when the text changed since.
    pub fn accept(&mut self, req: u64, hits: Vec<Commit>) -> bool {
        if req != self.req || !self.open {
            return false;
        }
        self.results = Some(hits);
        self.cursor = 0;
        true
    }

    /// A query failed: stop showing "Searching…" for it.
    pub fn failed(&mut self, req: u64) {
        if req == self.req && self.open {
            self.results = Some(Vec::new());
        }
    }

    /// "142 matches" (W7).
    pub fn count_label(&self) -> String {
        match &self.results {
            _ if self.searching() => "Searching…".to_string(),
            Some(r) if r.len() == 1 => "1 match".to_string(),
            Some(r) => format!("{} matches", r.len()),
            None => String::new(),
        }
    }

    /// `↑` in the field. In an empty field (or one still holding a recalled query) this steps
    /// back through the history and returns `true`; otherwise it moves the result cursor up.
    pub fn up(&mut self, now: f64) -> bool {
        if self.text.is_empty() || self.recall.is_some() {
            let next = self.recall.map_or(0, |i| i + 1);
            if let Some(q) = self.history.get(next) {
                self.text = q.to_string();
                self.schedule(now);
                self.recall = Some(next);
            }
            return true;
        }
        self.cursor = self.cursor.saturating_sub(1);
        false
    }

    /// `↓` in the field: forward through recalled history (back to an empty field), else the
    /// result cursor down.
    pub fn down(&mut self, now: f64) {
        match self.recall {
            Some(0) => {
                self.text.clear();
                self.schedule(now);
                self.recall = None;
            }
            Some(i) => {
                if let Some(q) = self.history.get(i - 1) {
                    self.text = q.to_string();
                    self.schedule(now);
                }
                self.recall = Some(i - 1);
            }
            None => {
                let len = self.results.as_ref().map_or(0, Vec::len);
                if len > 0 {
                    self.cursor = (self.cursor + 1).min(len - 1);
                }
            }
        }
    }

    /// A click on result row `i`.
    pub fn set_cursor(&mut self, i: usize) {
        let len = self.results.as_ref().map_or(0, Vec::len);
        if i < len {
            self.cursor = i;
        }
    }

    pub fn current(&self) -> Option<&Commit> {
        self.results.as_ref()?.get(self.cursor)
    }

    /// `Esc`: close and return the selection to restore (the one from before the search).
    pub fn escape(&mut self) -> Option<String> {
        self.close();
        self.prev_selection.take()
    }

    /// `Enter`: the result under the cursor, remembering the query and closing the search.
    /// `None` (search stays open) when there is no result to take.
    pub fn enter(&mut self) -> Option<Commit> {
        let hit = self.current()?.clone();
        self.history.push(&self.text);
        self.close();
        self.prev_selection = None;
        Some(hit)
    }

    fn close(&mut self) {
        self.open = false;
        self.focus_field = false;
        self.text.clear();
        self.recall = None;
        self.results = None;
        self.cursor = 0;
        self.due = None;
        self.req += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(id: &str, subject: &str) -> Commit {
        Commit {
            id: id.into(),
            parents: vec![],
            author: "Mary Poppins".into(),
            email: "mp@example.com".into(),
            time: 0,
            committer: String::new(),
            committer_email: String::new(),
            commit_time: 0,
            refs: vec![],
            subject: subject.into(),
        }
    }

    fn record(id: &str, subject: &str, paths: &[&str], refs: Vec<Decoration>) -> Record {
        let mut c = commit(id, subject);
        c.refs = refs;
        Record::new(Found {
            commit: c,
            body: String::new(),
            paths: paths.iter().map(|p| p.to_string()).collect(),
        })
    }

    fn ids(hits: &[Commit]) -> Vec<&str> {
        hits.iter().map(|c| c.id.as_str()).collect()
    }

    // ---- matching -----------------------------------------------------------------------

    #[test]
    fn filter_uses_paths_branches_and_tags_and_keeps_graph_order() {
        let records = vec![
            record("c3", "Handle 401", &["src/auth/token.rs"], vec![]),
            record("c2", "Docs", &["README.md"], vec![Decoration::Tag("v1.0".into())]),
            record(
                "c1",
                "Fix login timeout",
                &["src/auth/login.rs"],
                vec![Decoration::Branch("feat/x".into())],
            ),
        ];
        assert_eq!(ids(&filter(&Query::parse("author:mp path:src/auth"), &records)), vec!["c3", "c1"]);
        assert_eq!(ids(&filter(&Query::parse("tag:v1"), &records)), vec!["c2"]);
        assert_eq!(ids(&filter(&Query::parse("feat/x"), &records)), vec!["c1"]);
        assert!(filter(&Query::parse("nothing-like-this"), &records).is_empty());
    }

    #[test]
    fn remote_branch_decorations_count_as_branches() {
        let r = record("c1", "s", &[], vec![Decoration::Remote("origin/main".into()), Decoration::Head]);
        assert!(Query::parse("branch:origin/main").matches(&r.doc()));
    }

    // ---- history ------------------------------------------------------------------------

    #[test]
    fn history_keeps_the_last_twenty_newest_first_without_duplicates() {
        let mut h = History::default();
        for i in 0..25 {
            h.push(&format!("q{i}"));
        }
        assert_eq!(h.len(), HISTORY_LIMIT);
        assert_eq!(h.get(0), Some("q24"));
        assert_eq!(h.get(19), Some("q5"));
        h.push("q10");
        assert_eq!(h.get(0), Some("q10"));
        assert_eq!(h.len(), HISTORY_LIMIT, "a repeat moves to the front");
        h.push("   ");
        assert_eq!(h.get(0), Some("q10"), "blank queries are not remembered");
    }

    // ---- scheduling ---------------------------------------------------------------------

    fn typed(s: &mut Search, text: &str, now: f64) {
        s.text = text.into();
        s.edited(now);
    }

    #[test]
    fn local_search_loads_the_corpus_once_then_matches_immediately() {
        let mut s = Search::new(Mode::Local);
        s.open(Some("sel".into()));
        typed(&mut s, "login", 1.0);
        assert!(matches!(s.poll(1.0, 7), Some(Job::LoadCorpus { key: 7 })));
        assert!(s.poll(1.1, 7).is_none(), "already loading");
        assert!(s.searching());
        s.set_corpus(7, vec![record("c1", "Fix login", &[], vec![])]);
        let Some(Job::Local { req, query, corpus }) = s.poll(1.2, 7) else { panic!("local job") };
        let hits = filter(&query, &corpus);
        assert!(s.accept(req, hits));
        assert_eq!(s.count_label(), "1 match");
        typed(&mut s, "fix", 2.0);
        assert!(matches!(s.poll(2.0, 7), Some(Job::Local { .. })), "corpus reused");
    }

    #[test]
    fn a_new_corpus_key_reloads_it() {
        let mut s = Search::new(Mode::Local);
        s.open(None);
        s.set_corpus(1, vec![]);
        typed(&mut s, "x", 0.0);
        assert!(matches!(s.poll(0.0, 2), Some(Job::LoadCorpus { key: 2 })));
    }

    #[test]
    fn remote_search_waits_for_the_debounce() {
        let mut s = Search::new(Mode::Remote);
        s.open(None);
        typed(&mut s, "a", 10.0);
        typed(&mut s, "au", 10.1);
        assert!(s.poll(10.3, 0).is_none(), "0.2 s after the last keystroke");
        assert!(matches!(s.poll(10.4, 0), Some(Job::Remote { .. })));
        assert!(s.poll(10.5, 0).is_none(), "dispatched once");
    }

    #[test]
    fn stale_answers_are_dropped() {
        let mut s = Search::new(Mode::Remote);
        s.open(None);
        typed(&mut s, "a", 0.0);
        let Some(Job::Remote { req: old, .. }) = s.poll(1.0, 0) else { panic!() };
        typed(&mut s, "ab", 1.0);
        assert!(!s.accept(old, vec![commit("x", "x")]));
        assert!(s.results().is_none());
        assert_eq!(s.count_label(), "Searching…");
    }

    #[test]
    fn clearing_the_field_shows_the_graph_again() {
        let mut s = Search::new(Mode::Local);
        s.open(None);
        typed(&mut s, "a", 0.0);
        assert!(s.showing_results());
        typed(&mut s, "  ", 0.0);
        assert!(!s.showing_results());
        assert!(s.poll(1.0, 0).is_none());
    }

    // ---- keys ---------------------------------------------------------------------------

    fn with_results(n: usize) -> Search {
        let mut s = Search::new(Mode::Remote);
        s.open(Some("before".into()));
        typed(&mut s, "fix", 0.0);
        let Some(Job::Remote { req, .. }) = s.poll(1.0, 0) else { panic!() };
        s.accept(req, (0..n).map(|i| commit(&format!("c{i}"), "fix")).collect());
        s
    }

    #[test]
    fn escape_restores_the_previous_selection_and_closes() {
        let mut s = with_results(3);
        assert_eq!(s.escape().as_deref(), Some("before"));
        assert!(!s.is_open());
        assert!(!s.showing_results());
        assert!(s.history().is_empty(), "Esc does not remember the query");
    }

    #[test]
    fn enter_takes_the_cursor_result_and_remembers_the_query() {
        let mut s = with_results(3);
        s.down(1.0);
        s.down(1.0);
        s.down(1.0);
        assert_eq!(s.cursor(), 2, "clamped to the last result");
        s.up(1.0);
        assert_eq!(s.enter().map(|c| c.id).as_deref(), Some("c1"));
        assert!(!s.is_open());
        assert_eq!(s.history().get(0), Some("fix"));
    }

    #[test]
    fn enter_without_results_keeps_the_search_open() {
        let mut s = with_results(0);
        assert!(s.enter().is_none());
        assert!(s.is_open());
        assert_eq!(s.count_label(), "0 matches");
    }

    #[test]
    fn up_in_an_empty_field_walks_back_through_history_and_down_returns() {
        let mut s = Search::new(Mode::Local);
        for q in ["old", "mid", "new"] {
            s.history.push(q);
        }
        s.open(None);
        assert!(s.up(0.0));
        assert_eq!(s.text, "new");
        assert!(s.showing_results(), "a recalled query runs");
        assert!(s.up(0.0));
        assert_eq!(s.text, "mid");
        assert!(s.up(0.0));
        assert!(s.up(0.0), "past the oldest stays put");
        assert_eq!(s.text, "old");
        s.down(0.0);
        assert_eq!(s.text, "mid");
        s.down(0.0);
        s.down(0.0);
        assert_eq!(s.text, "", "back past the newest is the empty field");
    }

    #[test]
    fn up_after_typing_moves_the_result_cursor_not_the_history() {
        let mut s = with_results(3);
        s.history.push("older");
        s.set_cursor(2);
        assert!(!s.up(0.0));
        assert_eq!(s.cursor(), 1);
        assert_eq!(s.text, "fix");
    }

    #[test]
    fn editing_a_recalled_query_stops_recall() {
        let mut s = Search::new(Mode::Local);
        s.history.push("author:mp");
        s.open(None);
        s.up(0.0);
        s.text.push_str(" path:src");
        s.edited(0.0);
        s.set_cursor(0);
        assert!(!s.up(0.0), "↑ now belongs to the results");
    }

    #[test]
    fn reopening_keeps_the_query_and_refocuses() {
        let mut s = with_results(2);
        s.focus_field = false;
        s.open(Some("other".into()));
        assert_eq!(s.text, "fix");
        assert!(s.focus_field);
        assert_eq!(s.escape().as_deref(), Some("before"));
    }
}
