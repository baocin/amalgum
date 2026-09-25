//! Incremental lane layout for the commit graph (§5.4 "Streaming": layout of already-rendered
//! rows never shifts when new rows arrive).
//!
//! Feed commits in topological order (children before parents) to [`Layout::push`]; each call
//! returns that commit's [`Row`] and never revisits earlier rows. A row is drawn as a node on
//! lane `node` plus line segments: [`Half::Top`] segments run from the row's top edge to its
//! vertical middle, [`Half::Bottom`] from the middle to the bottom edge. Lane `x` at the top of
//! row N+1 continues lane `x` at the bottom of row N.
//!
//! Lane reuse: a commit takes the leftmost lane already waiting for it, else the leftmost free
//! lane, else a new one. Its first parent inherits its lane; other parents go to a lane already
//! waiting for them, else a newly allocated one. Several lanes waiting for the same commit merge
//! into its node. Colors: a lane started by a commit whose decoration names a branch gets
//! [`lane_color`] of that name and keeps it down the first-parent chain; otherwise the lane
//! index modulo [`LANE_COLORS`].

/// Number of lane colors in the theme's lane palette (§4 "Graph lane palette").
pub const LANE_COLORS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Half {
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub half: Half,
    /// Lane at the segment's upper end.
    pub from: usize,
    /// Lane at the segment's lower end.
    pub to: usize,
    pub color: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub node: usize,
    pub color: usize,
    pub segments: Vec<Segment>,
    /// Lanes in use after this row (for column width).
    pub width: usize,
}

#[derive(Debug, Default)]
pub struct Layout {
    /// Lane index → the commit id that lane is waiting for, and its color. `None` is a free lane.
    lanes: Vec<Option<(String, usize)>>,
}

impl Layout {
    /// Lay out the next commit. `branch` is the branch name decorating it, if any (color hint).
    ///
    /// A segment's color always follows the lane at its non-node end: a top segment (converging
    /// into, or passing through, the node) keeps the color that lane already had; a bottom
    /// segment (leaving the node) takes the color of the lane it lands in. Where both ends are
    /// the node, that is simply the node's own color.
    pub fn push(&mut self, id: &str, parents: &[String], branch: Option<&str>) -> Row {
        let old = self.lanes.clone();

        // Lanes already waiting for `id`, ascending (enumeration order).
        let waiting: Vec<usize> = old
            .iter()
            .enumerate()
            .filter(|(_, l)| l.as_ref().is_some_and(|(cid, _)| cid == id))
            .map(|(i, _)| i)
            .collect();

        // Leftmost lane waiting for this commit, else leftmost free lane, else a new one.
        let node = match waiting.first() {
            Some(&i) => i,
            None => old.iter().position(Option::is_none).unwrap_or(old.len()),
        };
        let node_color = match waiting.first() {
            Some(&i) => old[i].as_ref().map(|(_, c)| *c).unwrap_or(node % LANE_COLORS),
            None => branch.map(lane_color).unwrap_or(node % LANE_COLORS),
        };

        // Top half: every previously active lane either converges into the node (it was
        // waiting for `id`) or passes straight through, in its own established color.
        let mut segments: Vec<Segment> = old
            .iter()
            .enumerate()
            .filter_map(|(i, l)| {
                let (cid, color) = l.as_ref()?;
                let to = if cid == id { node } else { i };
                Some(Segment { half: Half::Top, from: i, to, color: *color })
            })
            .collect();

        if node >= self.lanes.len() {
            self.lanes.resize(node + 1, None);
        }
        for &w in &waiting {
            self.lanes[w] = None;
        }

        // Bottom half: the first parent inherits the node's lane and color; every other
        // parent takes a lane already waiting for it, else the leftmost free lane, else a new
        // one, colored by lane index (no branch hint is known for a commit not yet visited).
        let mut touched = vec![node];
        match parents.first() {
            Some(first) => {
                self.lanes[node] = Some((first.clone(), node_color));
                segments.push(Segment { half: Half::Bottom, from: node, to: node, color: node_color });
            }
            None => self.lanes[node] = None, // no more commits on this lane: it is freed
        }

        for parent in parents.iter().skip(1) {
            let target = self.lanes.iter().position(|l| l.as_ref().is_some_and(|(cid, _)| cid == parent));
            // A lane already waiting for `parent` before this row (i.e. active in `old`) keeps
            // its own vertical through the bottom half too, in addition to this parent's new
            // diagonal into it — otherwise that lane's column stops dead at the row's middle.
            let already_waiting = target.is_some_and(|t| old.get(t).is_some_and(Option::is_some));
            let target =
                target.or_else(|| self.lanes.iter().position(Option::is_none)).unwrap_or(self.lanes.len());
            if target >= self.lanes.len() {
                self.lanes.resize(target + 1, None);
            }
            if self.lanes[target].is_none() {
                self.lanes[target] = Some((parent.clone(), target % LANE_COLORS));
            }
            let color = self.lanes[target].as_ref().map(|(_, c)| *c).unwrap_or_default();
            if already_waiting {
                segments.push(Segment { half: Half::Bottom, from: target, to: target, color });
            }
            segments.push(Segment { half: Half::Bottom, from: node, to: target, color });
            touched.push(target);
        }

        // Every other still-active lane just passes straight through.
        for (i, l) in self.lanes.iter().enumerate() {
            if touched.contains(&i) {
                continue;
            }
            if let Some((_, color)) = l {
                segments.push(Segment { half: Half::Bottom, from: i, to: i, color: *color });
            }
        }

        let width = self.lanes.len();
        while matches!(self.lanes.last(), Some(None)) {
            self.lanes.pop(); // keep storage (and future free-lane search) tight
        }

        Row { node, color: node_color, segments, width }
    }
}

/// Stable color index for a branch name: FNV-1a (`util::fnv1a64`) modulo [`LANE_COLORS`].
/// A branch keeps its color across sessions and repos.
pub fn lane_color(branch: &str) -> usize {
    (crate::util::fnv1a64(branch.as_bytes()) % LANE_COLORS as u64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_all(spec: &[(&str, &[&str], Option<&str>)]) -> Vec<Row> {
        let mut layout = Layout::default();
        spec.iter()
            .map(|(id, parents, branch)| {
                let parents: Vec<String> = parents.iter().map(|s| s.to_string()).collect();
                layout.push(id, &parents, *branch)
            })
            .collect()
    }

    /// Checks the structural invariants from the module doc against a full run of rows:
    /// 1. continuity — the `to` lanes of row N's bottom segments equal the `from` lanes of row
    ///    N+1's top segments.
    /// 2. every top segment ends at the node or passes straight through (`to == node || to ==
    ///    from`).
    /// 3. `width` covers every lane index the row's segments (and its node) reference.
    fn assert_invariants(rows: &[Row]) {
        use std::collections::BTreeSet;

        for row in rows {
            for s in &row.segments {
                assert!(
                    s.half != Half::Top || s.to == row.node || s.to == s.from,
                    "top segment {s:?} neither converges on node {} nor passes straight",
                    row.node
                );
                assert!(row.width > s.from, "width {} must exceed lane {} (from)", row.width, s.from);
                assert!(row.width > s.to, "width {} must exceed lane {} (to)", row.width, s.to);
            }
            assert!(row.width > row.node, "width {} must exceed node lane {}", row.width, row.node);
        }

        for w in rows.windows(2) {
            let bottom_to: BTreeSet<usize> =
                w[0].segments.iter().filter(|s| s.half == Half::Bottom).map(|s| s.to).collect();
            let top_from: BTreeSet<usize> =
                w[1].segments.iter().filter(|s| s.half == Half::Top).map(|s| s.from).collect();
            assert_eq!(bottom_to, top_from, "lane continuity broken between consecutive rows");
        }
    }

    // ---- lane_color ----

    #[test]
    fn lane_color_is_stable() {
        // Computed once from `crate::util::fnv1a64` and hard-coded; must never change (the
        // module doc promises a branch keeps its color across sessions and repos).
        assert_eq!(lane_color("main"), 0);
        assert_eq!(lane_color("feature"), 5);
        assert_eq!(lane_color("origin/x"), 0);
        assert_eq!(lane_color("release/1.0"), 0);
        assert_eq!(lane_color(""), 5);
    }

    #[test]
    fn lane_color_in_range() {
        for name in ["a", "b", "main", "feature/long-name-here", "🚀", "x".repeat(500).as_str()] {
            assert!(lane_color(name) < LANE_COLORS, "{name:?} -> out-of-range color");
        }
    }

    // ---- hand-built scenarios ----

    #[test]
    fn linear_chain_stays_on_lane_zero() {
        let rows = push_all(&[
            ("c1", &["c2"], None),
            ("c2", &["c3"], None),
            ("c3", &["c4"], None),
            ("c4", &[], None),
        ]);
        assert_invariants(&rows);
        for r in &rows {
            assert_eq!(r.node, 0);
            assert_eq!(r.color, 0);
            assert_eq!(r.width, 1);
        }
    }

    #[test]
    fn branch_and_merge_second_parent_joins_lane_one_then_back() {
        // M merges A and B, both of which descend from a shared ancestor C.
        let rows =
            push_all(&[("M", &["A", "B"], None), ("A", &["C"], None), ("B", &["C"], None), ("C", &[], None)]);
        assert_invariants(&rows);

        assert_eq!(rows[0].node, 0, "M starts a fresh lane 0");
        assert_eq!(
            rows[0].segments,
            vec![
                Segment { half: Half::Bottom, from: 0, to: 0, color: 0 },
                Segment { half: Half::Bottom, from: 0, to: 1, color: 1 },
            ],
            "first parent stays on lane 0, second parent B is pushed out to lane 1"
        );

        assert_eq!(rows[1].node, 0, "A (first parent) continues on lane 0");
        assert_eq!(rows[2].node, 1, "B (second parent) is laid out on lane 1");

        // C is awaited by both lanes and they converge back onto the leftmost, lane 0.
        assert_eq!(rows[3].node, 0);
        let tops: Vec<&Segment> = rows[3].segments.iter().filter(|s| s.half == Half::Top).collect();
        assert_eq!(tops.len(), 2, "both lane 0 and lane 1 converge into C's node");
        assert!(tops.iter().all(|s| s.to == 0));
        assert!(tops.iter().any(|s| s.from == 0) && tops.iter().any(|s| s.from == 1));
        assert!(rows[3].segments.iter().all(|s| s.half == Half::Top), "C is a root: no bottom segments");
    }

    #[test]
    fn two_branches_off_a_shared_base() {
        let rows = push_all(&[
            ("t2", &["base"], Some("topic")),
            ("m2", &["base"], Some("main")),
            ("base", &["root"], None),
            ("root", &[], None),
        ]);
        assert_invariants(&rows);

        assert_eq!(rows[0].node, 0);
        assert_eq!(rows[0].color, lane_color("topic"));
        assert_eq!(rows[1].node, 1, "m2 gets its own lane; base is not reached yet");
        assert_eq!(rows[1].color, lane_color("main"));

        // Both lanes 0 and 1 are waiting for the shared "base" commit; they converge.
        assert_eq!(rows[2].node, 0);
        let tops: Vec<&Segment> = rows[2].segments.iter().filter(|s| s.half == Half::Top).collect();
        assert_eq!(tops.len(), 2);
        assert!(tops.iter().any(|s| s.from == 0) && tops.iter().any(|s| s.from == 1));
    }

    #[test]
    fn octopus_merge_three_parents_get_three_lanes() {
        let rows = push_all(&[
            ("O", &["P1", "P2", "P3"], None),
            ("P1", &["R"], None),
            ("P2", &["R"], None),
            ("P3", &["R"], None),
            ("R", &[], None),
        ]);
        assert_invariants(&rows);

        assert_eq!(rows[0].node, 0);
        let bottoms: Vec<&Segment> = rows[0].segments.iter().filter(|s| s.half == Half::Bottom).collect();
        assert_eq!(bottoms.len(), 3, "an octopus merge opens one bottom segment per parent");
        let mut targets: Vec<usize> = bottoms.iter().map(|s| s.to).collect();
        targets.sort_unstable();
        assert_eq!(targets, vec![0, 1, 2], "three distinct lanes, one per parent");

        // All three lanes converge back into R.
        assert_eq!(rows[4].node, 0);
        let tops: Vec<&Segment> = rows[4].segments.iter().filter(|s| s.half == Half::Top).collect();
        assert_eq!(tops.len(), 3);
    }

    #[test]
    fn criss_cross_merges_reuse_waiting_lanes_and_converge() {
        // M2 merges A2/B1, M1 merges A1/B2: two independent merges that criss-cross the same
        // A1/B1 ancestry, so A1 and B1 each end up awaited by two different lanes.
        let rows = push_all(&[
            ("M2", &["A2", "B1"], None),
            ("M1", &["A1", "B2"], None),
            ("A2", &["A1"], None),
            ("B2", &["B1"], None),
            ("A1", &["Base"], None),
            ("B1", &["Base"], None),
            ("Base", &[], None),
        ]);
        assert_invariants(&rows);

        // A1 (row index 4) and B1 (row index 5) each have two lanes converging on them.
        let a1_tops: Vec<&Segment> = rows[4].segments.iter().filter(|s| s.half == Half::Top).collect();
        assert_eq!(a1_tops.iter().filter(|s| s.to == rows[4].node).count(), 2, "two lanes await A1");
        let b1_tops: Vec<&Segment> = rows[5].segments.iter().filter(|s| s.half == Half::Top).collect();
        assert_eq!(b1_tops.iter().filter(|s| s.to == rows[5].node).count(), 2, "two lanes await B1");

        // Base is awaited by whatever lanes A1 and B1 folded into, and is a root.
        let base = rows.last().unwrap();
        assert!(base.segments.iter().all(|s| s.half == Half::Top), "Base is a root: no bottom segments");
    }

    #[test]
    fn merges_second_parent_into_a_lane_already_waiting_for_it_keeps_its_vertical() {
        // Regression: F's next commit is P2 (lane 0 becomes, and stays, "waiting for P2"). M's
        // second parent is also P2, reusing that same lane — the pre-existing lane must keep
        // its own top-to-bottom vertical *in addition to* M's new diagonal into it, not lose it
        // to the diagonal. This is the shape of "merge main into feature" once main's tip has
        // already been laid out, or of `--all` where a merged branch keeps going.
        let rows = push_all(&[("F", &["P2"], None), ("M", &["P1", "P2"], None)]);
        assert_invariants(&rows);

        let m = &rows[1];
        assert!(
            m.segments.contains(&Segment { half: Half::Bottom, from: 0, to: 0, color: 0 }),
            "lane 0 must keep its own vertical through the bottom half: {:?}",
            m.segments
        );
        assert!(
            m.segments.iter().any(|s| s.half == Half::Bottom && s.from == m.node && s.to == 0),
            "M's second parent must still draw its diagonal into lane 0: {:?}",
            m.segments
        );
    }

    #[test]
    fn lane_reuse_is_leftmost_first() {
        let mut layout = Layout::default();
        let p = |s: &str| vec![s.to_string()];

        let r0 = layout.push("b1_tip", &p("b1_root"), Some("b1"));
        let r1 = layout.push("b2_tip", &p("b2_root"), Some("b2"));
        let r2 = layout.push("b3_tip", &p("b3_root"), Some("b3"));
        assert_eq!((r0.node, r1.node, r2.node), (0, 1, 2), "three tips claim lanes 0, 1, 2 in order");

        // Free the middle lane: b2_root is a root, so lane 1 is freed but not trailing (lane 2
        // is still active), so it stays a gap rather than shrinking the vector.
        let r3 = layout.push("b2_root", &[], None);
        assert_eq!(r3.node, 1);

        // A brand-new, unrelated tip must take the leftmost free lane (1), not append at 3.
        let r4 = layout.push("new_tip", &p("new_next"), Some("newb"));
        assert_eq!(r4.node, 1, "the freed middle lane is reused before allocating a new one");
        assert_eq!(r4.width, 3, "no new lane was appended");
    }

    #[test]
    fn dangling_parent_that_never_appears_does_not_panic() {
        // A shallow / limited `git log` can end before a commit's parent is reached.
        let mut layout = Layout::default();
        let r0 = layout.push("tip", &["missing_ancestor".to_string()], None);
        assert_eq!(r0.node, 0);
        assert_eq!(r0.segments, vec![Segment { half: Half::Bottom, from: 0, to: 0, color: 0 }]);

        // Further, unrelated commits must still lay out fine; the dangling lane just sits there.
        let r1 = layout.push("other_root", &[], None);
        assert_eq!(r1.node, 1, "the dangling lane 0 is still occupied, so this gets a new lane");
        assert_eq!(r1.width, 2);
    }

    #[test]
    fn incremental_push_never_revisits_earlier_rows() {
        let spec: Vec<(&str, &[&str], Option<&str>)> = vec![
            ("M2", &["A2", "B1"], None),
            ("M1", &["A1", "B2"], None),
            ("A2", &["A1"], None),
            ("B2", &["B1"], None),
            ("A1", &["Base"], None),
            ("B1", &["Base"], None),
            ("Base", &[], None),
        ];
        let full = push_all(&spec);
        for k in 1..=spec.len() {
            let prefix = push_all(&spec[..k]);
            assert_eq!(prefix, full[..k], "rows 0..{k} must match a fresh layout of just that prefix");
        }
    }

    // ---- against a real TempRepo history ----

    /// The first branch/current-branch decoration on a commit, as the color hint `Layout::push`
    /// expects (module doc: "a commit whose decoration names a branch").
    fn branch_hint(commit: &crate::git::log::Commit) -> Option<&str> {
        commit.refs.iter().find_map(|d| match d {
            crate::git::log::Decoration::Branch(name) | crate::git::log::Decoration::CurrentBranch(name) => {
                Some(name.as_str())
            }
            _ => None,
        })
    }

    #[test]
    fn real_repo_history_satisfies_invariants() {
        use crate::git::log;
        use crate::testutil::TempRepo;

        let mut repo = TempRepo::new();
        repo.commit_file("a.txt", "a", "base");
        repo.git(&["checkout", "-q", "-b", "topic"]);
        repo.commit_file("b.txt", "b", "on topic");
        repo.git(&["checkout", "-q", "main"]);
        repo.commit_file("c.txt", "c", "on main");
        repo.git(&["merge", "-q", "--no-ff", "-m", "merge topic", "topic"]);
        repo.git(&["checkout", "-q", "-b", "other"]);
        repo.commit_file("d.txt", "d", "on other");
        repo.git(&["checkout", "-q", "main"]);

        let args = log::log_args(&["--all"]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = repo.git_raw(&args);
        let commits = log::parse_log(&out);
        assert!(commits.len() >= 5, "sanity: history has the commits we made");

        let mut layout = Layout::default();
        let rows: Vec<Row> = commits.iter().map(|c| layout.push(&c.id, &c.parents, branch_hint(c))).collect();
        assert_invariants(&rows);
    }

    // ---- performance ----

    #[test]
    fn lays_out_fifty_thousand_commits_quickly() {
        // A long main chain with a short side branch merged in every 7th commit, so the active
        // lane count stays small and per-push work stays proportional to it, not to history
        // length. IDs are synthetic; only shape (parents, branch hints) matters here.
        const MAIN: usize = 45_000;
        let mut layout = Layout::default();
        let mut total = 0usize;

        let start = std::time::Instant::now();
        for i in 0..MAIN {
            let next = format!("m{}", i + 1);
            if i > 0 && i % 7 == 0 {
                let side = format!("b{i}");
                layout.push(&format!("m{i}"), &[next.clone(), side.clone()], None);
                layout.push(&side, &[next], None);
                total += 2;
            } else {
                layout.push(&format!("m{i}"), &[next], None);
                total += 1;
            }
        }
        layout.push(&format!("m{MAIN}"), &[], None); // root
        total += 1;
        let elapsed = start.elapsed();

        assert!(total >= 50_000, "sanity: laid out at least 50,000 commits ({total})");
        assert!(
            elapsed.as_secs_f64() < 1.0,
            "layout of {total} commits took {elapsed:?}, expected well under 1s"
        );
    }
}
