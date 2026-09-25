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
    // private: per-lane "waiting for commit id" + color
}

impl Layout {
    /// Lay out the next commit. `branch` is the branch name decorating it, if any (color hint).
    pub fn push(&mut self, id: &str, parents: &[String], branch: Option<&str>) -> Row {
        todo!()
    }
}

/// Stable color index for a branch name: FNV-1a (`util::fnv1a64`) modulo [`LANE_COLORS`].
/// A branch keeps its color across sessions and repos.
pub fn lane_color(branch: &str) -> usize {
    todo!()
}
