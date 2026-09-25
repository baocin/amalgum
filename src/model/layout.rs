//! Terminal split trees (§5.27 "Splits"): each tab is a tree whose leaves are terminal panes.
//! Splits nest without limit; `Mod+D` splits right (new pane second, side by side), `Mod+Shift+D`
//! splits down (new pane second, below). Focus moves geometrically with `Mod+Alt+arrows`.

use serde::{Deserialize, Serialize};

pub type PaneId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    /// Children side by side (split right).
    Horizontal,
    /// Children stacked (split down).
    Vertical,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tree {
    Leaf(PaneId),
    Split { axis: Axis, ratio: f32, first: Box<Tree>, second: Box<Tree> },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Closed {
    NotFound,
    /// It was the only pane; the caller closes the tab.
    LastPane,
    /// Closed; focus should move here (the nearest leaf of the removed pane's sibling).
    Focus(PaneId),
}

/// A draggable divider between the two children of the split at `path` (`false` = first
/// child, `true` = second child, from the root).
#[derive(Debug, Clone, PartialEq)]
pub struct Divider {
    pub path: Vec<bool>,
    pub axis: Axis,
    pub rect: Rect,
}

impl Tree {
    /// Split `target` along `axis`, the new pane second, ratio 0.5. False if not found.
    pub fn split(&mut self, target: PaneId, axis: Axis, new: PaneId) -> bool {
        todo!()
    }
    pub fn close(&mut self, target: PaneId) -> Closed {
        todo!()
    }
    /// Leaves in reading order (left-to-right, top-to-bottom through the tree).
    pub fn leaves(&self) -> Vec<PaneId> {
        todo!()
    }
    /// Pane rectangles within `area`; `ratio` is the first child's share.
    pub fn layout(&self, area: Rect) -> Vec<(PaneId, Rect)> {
        todo!()
    }
    /// Nearest pane in `dir` from `from` whose span overlaps `from` on the other axis; ties go
    /// to the largest overlap, then the smallest distance.
    pub fn neighbor(&self, area: Rect, from: PaneId, dir: Dir) -> Option<PaneId> {
        todo!()
    }
    pub fn dividers(&self, area: Rect) -> Vec<Divider> {
        todo!()
    }
    /// Set the ratio of the split at `path`, clamped to 0.05..=0.95. False if no split there.
    pub fn set_ratio(&mut self, path: &[bool], ratio: f32) -> bool {
        todo!()
    }
}
