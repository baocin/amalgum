//! Terminal split trees (§5.27 "Splits"): each tab is a tree whose leaves are terminal panes.
//! Splits nest without limit; `Mod+D` splits right (new pane second, side by side), `Mod+Shift+D`
//! splits down (new pane second, below). Focus moves geometrically with `Mod+Alt+arrows`.

use serde::{Deserialize, Serialize};

pub type PaneId = u64;

/// Tolerance for float comparisons in geometry (side tests, overlap tests): rects come from
/// `layout`'s ratio arithmetic, which can be off by a hair at deep nesting.
const EPS: f32 = 0.01;

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

impl Rect {
    fn area(self) -> f32 {
        self.w * self.h
    }
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

/// Split `area` into the first/second child rects for a split with `axis` and `ratio` (the
/// first child's share of the split dimension). No gap between them: the UI draws the divider
/// over the shared boundary.
fn split_rect(area: Rect, axis: Axis, ratio: f32) -> (Rect, Rect) {
    match axis {
        Axis::Horizontal => {
            let w1 = area.w * ratio;
            let first = Rect { x: area.x, y: area.y, w: w1, h: area.h };
            let second = Rect { x: area.x + w1, y: area.y, w: area.w - w1, h: area.h };
            (first, second)
        }
        Axis::Vertical => {
            let h1 = area.h * ratio;
            let first = Rect { x: area.x, y: area.y, w: area.w, h: h1 };
            let second = Rect { x: area.x, y: area.y + h1, w: area.w, h: area.h - h1 };
            (first, second)
        }
    }
}

/// Overlap length of two rects along the vertical (`y`) axis; 0 if they don't overlap.
fn overlap_y(a: Rect, b: Rect) -> f32 {
    let top = a.y.max(b.y);
    let bottom = (a.y + a.h).min(b.y + b.h);
    (bottom - top).max(0.0)
}

/// Overlap length of two rects along the horizontal (`x`) axis; 0 if they don't overlap.
fn overlap_x(a: Rect, b: Rect) -> f32 {
    let left = a.x.max(b.x);
    let right = (a.x + a.w).min(b.x + b.w);
    (right - left).max(0.0)
}

impl Tree {
    /// Split `target` along `axis`, the new pane second, ratio 0.5. False if not found.
    pub fn split(&mut self, target: PaneId, axis: Axis, new: PaneId) -> bool {
        match self {
            Tree::Leaf(id) if *id == target => {
                *self = Tree::Split {
                    axis,
                    ratio: 0.5,
                    first: Box::new(Tree::Leaf(*id)),
                    second: Box::new(Tree::Leaf(new)),
                };
                true
            }
            Tree::Leaf(_) => false,
            Tree::Split { first, second, .. } => {
                first.split(target, axis, new) || second.split(target, axis, new)
            }
        }
    }

    /// Close `target`. If this whole tree is just `target`, [`Closed::LastPane`] (the caller
    /// closes the tab instead). Otherwise the `Split` whose direct child is `target` is replaced
    /// in place by its sibling subtree, and the returned focus is the sibling's nearest leaf to
    /// where `target` was: the sibling's *first* leaf (reading order) if `target` was the first
    /// child, its *last* leaf if `target` was the second child. [`Closed::NotFound`] if `target`
    /// isn't in the tree at all.
    pub fn close(&mut self, target: PaneId) -> Closed {
        if let Tree::Leaf(id) = self {
            return if *id == target { Closed::LastPane } else { Closed::NotFound };
        }
        match self.close_inner(target) {
            Some(focus) => Closed::Focus(focus),
            None => Closed::NotFound,
        }
    }

    /// Recursive worker for [`Tree::close`], only ever called on a `Split` (or a subtree of
    /// one). Takes `self` by value via a placeholder swap so the replacement can be built
    /// without fighting the borrow checker over reassigning `self` through a live child borrow.
    fn close_inner(&mut self, target: PaneId) -> Option<PaneId> {
        let owned = std::mem::replace(self, Tree::Leaf(0));
        let (new_self, focus) = match owned {
            Tree::Leaf(id) => (Tree::Leaf(id), None),
            Tree::Split { axis, ratio, first, second } => {
                if matches!(*first, Tree::Leaf(id) if id == target) {
                    let focus = second.first_leaf();
                    (*second, Some(focus))
                } else if matches!(*second, Tree::Leaf(id) if id == target) {
                    let focus = first.last_leaf();
                    (*first, Some(focus))
                } else {
                    let mut first = first;
                    if let Some(focus) = first.close_inner(target) {
                        (Tree::Split { axis, ratio, first, second }, Some(focus))
                    } else {
                        let mut second = second;
                        let focus = second.close_inner(target);
                        (Tree::Split { axis, ratio, first, second }, focus)
                    }
                }
            }
        };
        *self = new_self;
        focus
    }

    /// The leaf reached by always descending into the first child (leftmost/topmost).
    fn first_leaf(&self) -> PaneId {
        match self {
            Tree::Leaf(id) => *id,
            Tree::Split { first, .. } => first.first_leaf(),
        }
    }

    /// The leaf reached by always descending into the second child (rightmost/bottommost).
    fn last_leaf(&self) -> PaneId {
        match self {
            Tree::Leaf(id) => *id,
            Tree::Split { second, .. } => second.last_leaf(),
        }
    }

    /// Leaves in reading order (left-to-right, top-to-bottom through the tree).
    pub fn leaves(&self) -> Vec<PaneId> {
        match self {
            Tree::Leaf(id) => vec![*id],
            Tree::Split { first, second, .. } => {
                let mut v = first.leaves();
                v.extend(second.leaves());
                v
            }
        }
    }

    /// Pane rectangles within `area`; `ratio` is the first child's share.
    pub fn layout(&self, area: Rect) -> Vec<(PaneId, Rect)> {
        match self {
            Tree::Leaf(id) => vec![(*id, area)],
            Tree::Split { axis, ratio, first, second } => {
                let (r1, r2) = split_rect(area, *axis, *ratio);
                let mut v = first.layout(r1);
                v.extend(second.layout(r2));
                v
            }
        }
    }

    /// Nearest pane in `dir` from `from` whose span overlaps `from` on the other axis; ties go
    /// to the largest overlap, then the smallest edge distance, then reading order.
    pub fn neighbor(&self, area: Rect, from: PaneId, dir: Dir) -> Option<PaneId> {
        let rects = self.layout(area);
        let from_rect = rects.iter().find(|(id, _)| *id == from)?.1;
        let order = self.leaves();

        // (overlap, -distance, -reading_index): larger is better, compared lexicographically.
        let mut best: Option<(PaneId, f32, f32, usize)> = None;
        for (id, rect) in rects.iter().copied() {
            if id == from {
                continue;
            }
            let (on_side, overlap, distance) = match dir {
                Dir::Right => (
                    rect.x >= from_rect.x + from_rect.w - EPS,
                    overlap_y(rect, from_rect),
                    rect.x - (from_rect.x + from_rect.w),
                ),
                Dir::Left => (
                    rect.x + rect.w <= from_rect.x + EPS,
                    overlap_y(rect, from_rect),
                    from_rect.x - (rect.x + rect.w),
                ),
                Dir::Down => (
                    rect.y >= from_rect.y + from_rect.h - EPS,
                    overlap_x(rect, from_rect),
                    rect.y - (from_rect.y + from_rect.h),
                ),
                Dir::Up => (
                    rect.y + rect.h <= from_rect.y + EPS,
                    overlap_x(rect, from_rect),
                    from_rect.y - (rect.y + rect.h),
                ),
            };
            if !on_side || overlap <= EPS {
                continue;
            }
            let idx = order.iter().position(|l| *l == id).unwrap_or(usize::MAX);
            let candidate = (id, overlap, distance, idx);
            best = Some(match best {
                None => candidate,
                Some(b) => {
                    if candidate.1 > b.1 + EPS {
                        candidate
                    } else if candidate.1 < b.1 - EPS {
                        b
                    } else if candidate.2 < b.2 - EPS {
                        candidate
                    } else if candidate.2 > b.2 + EPS {
                        b
                    } else if candidate.3 < b.3 {
                        candidate
                    } else {
                        b
                    }
                }
            });
        }
        best.map(|(id, ..)| id)
    }

    /// One divider per `Split` node: a zero-thickness rect at the boundary between its two
    /// children, spanning that split's own extent (not the whole area, for a nested split).
    pub fn dividers(&self, area: Rect) -> Vec<Divider> {
        let mut out = Vec::new();
        self.collect_dividers(area, &mut Vec::new(), &mut out);
        out
    }

    fn collect_dividers(&self, area: Rect, path: &mut Vec<bool>, out: &mut Vec<Divider>) {
        if let Tree::Split { axis, ratio, first, second } = self {
            let (r1, r2) = split_rect(area, *axis, *ratio);
            let rect = match axis {
                Axis::Horizontal => Rect { x: r1.x + r1.w, y: area.y, w: 0.0, h: area.h },
                Axis::Vertical => Rect { x: area.x, y: r1.y + r1.h, w: area.w, h: 0.0 },
            };
            out.push(Divider { path: path.clone(), axis: *axis, rect });
            path.push(false);
            first.collect_dividers(r1, path, out);
            path.pop();
            path.push(true);
            second.collect_dividers(r2, path, out);
            path.pop();
        }
    }

    /// Set the ratio of the split at `path`, clamped to 0.05..=0.95. False if no split there.
    pub fn set_ratio(&mut self, path: &[bool], ratio: f32) -> bool {
        match self {
            Tree::Leaf(_) => false,
            Tree::Split { ratio: r, first, second, .. } => match path.split_first() {
                None => {
                    *r = ratio.clamp(0.05, 0.95);
                    true
                }
                Some((false, rest)) => first.set_ratio(rest, ratio),
                Some((true, rest)) => second.set_ratio(rest, ratio),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(first: Tree, second: Tree, ratio: f32) -> Tree {
        Tree::Split { axis: Axis::Horizontal, ratio, first: Box::new(first), second: Box::new(second) }
    }
    fn v(first: Tree, second: Tree, ratio: f32) -> Tree {
        Tree::Split { axis: Axis::Vertical, ratio, first: Box::new(first), second: Box::new(second) }
    }
    fn leaf(id: PaneId) -> Tree {
        Tree::Leaf(id)
    }

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.01
    }
    fn rect_approx(a: Rect, b: Rect) -> bool {
        approx(a.x, b.x) && approx(a.y, b.y) && approx(a.w, b.w) && approx(a.h, b.h)
    }

    // --- split ---------------------------------------------------------

    #[test]
    fn split_replaces_leaf_with_split_new_second_ratio_half() {
        let mut t = leaf(1);
        assert!(t.split(1, Axis::Horizontal, 2));
        match &t {
            Tree::Split { axis, ratio, first, second } => {
                assert_eq!(*axis, Axis::Horizontal);
                assert_eq!(*ratio, 0.5);
                assert_eq!(**first, leaf(1));
                assert_eq!(**second, leaf(2));
            }
            _ => panic!("expected a split"),
        }
    }

    #[test]
    fn split_finds_target_deep_in_the_tree() {
        let mut t = h(leaf(1), v(leaf(2), leaf(3), 0.5), 0.5);
        assert!(t.split(3, Axis::Vertical, 4));
        assert_eq!(t.leaves(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn split_missing_target_is_false_and_leaves_tree_untouched() {
        let mut t = h(leaf(1), leaf(2), 0.5);
        let before = t.clone();
        assert!(!t.split(99, Axis::Horizontal, 3));
        assert_eq!(t, before);
    }

    // --- close -----------------------------------------------------------

    #[test]
    fn close_only_pane_is_last_pane() {
        let mut t = leaf(1);
        assert_eq!(t.close(1), Closed::LastPane);
    }

    #[test]
    fn close_missing_target_is_not_found() {
        let mut t = h(leaf(1), leaf(2), 0.5);
        assert_eq!(t.close(99), Closed::NotFound);
        // Untouched.
        assert_eq!(t.leaves(), vec![1, 2]);
    }

    #[test]
    fn close_first_child_focuses_siblings_first_leaf() {
        // Removing the first child of a split whose sibling is itself a split: focus lands on
        // the sibling's first leaf in reading order (nearest the removed pane).
        let mut t = h(leaf(1), v(leaf(2), leaf(3), 0.5), 0.5);
        assert_eq!(t.close(1), Closed::Focus(2));
        assert_eq!(t, v(leaf(2), leaf(3), 0.5));
    }

    #[test]
    fn close_second_child_focuses_siblings_last_leaf() {
        let mut t = h(v(leaf(1), leaf(2), 0.5), leaf(3), 0.5);
        assert_eq!(t.close(3), Closed::Focus(2));
        assert_eq!(t, v(leaf(1), leaf(2), 0.5));
    }

    #[test]
    fn close_leaf_sibling_focuses_that_leaf() {
        let mut t = h(leaf(1), leaf(2), 0.5);
        assert_eq!(t.close(1), Closed::Focus(2));
        assert_eq!(t, leaf(2));
    }

    #[test]
    fn close_nested_pane_only_replaces_its_immediate_parent_split() {
        // Tree: H(V(1,2), 3). Closing 2 should collapse only the inner V split, leaving the
        // outer H split's second child (3) untouched.
        let mut t = h(v(leaf(1), leaf(2), 0.5), leaf(3), 0.3);
        assert_eq!(t.close(2), Closed::Focus(1));
        assert_eq!(t, h(leaf(1), leaf(3), 0.3));
    }

    // --- layout ------------------------------------------------------------

    #[test]
    fn layout_horizontal_splits_width_first_on_left() {
        let t = h(leaf(1), leaf(2), 0.25);
        let area = Rect { x: 0.0, y: 0.0, w: 400.0, h: 100.0 };
        let rects: std::collections::HashMap<_, _> = t.layout(area).into_iter().collect();
        assert!(rect_approx(rects[&1], Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 }));
        assert!(rect_approx(rects[&2], Rect { x: 100.0, y: 0.0, w: 300.0, h: 100.0 }));
    }

    #[test]
    fn layout_vertical_splits_height_first_on_top() {
        let t = v(leaf(1), leaf(2), 0.75);
        let area = Rect { x: 0.0, y: 0.0, w: 100.0, h: 400.0 };
        let rects: std::collections::HashMap<_, _> = t.layout(area).into_iter().collect();
        assert!(rect_approx(rects[&1], Rect { x: 0.0, y: 0.0, w: 100.0, h: 300.0 }));
        assert!(rect_approx(rects[&2], Rect { x: 0.0, y: 300.0, w: 100.0, h: 100.0 }));
    }

    /// A deterministic, non-trivial nested tree mixing both axes and several ratios.
    fn sample_tree() -> Tree {
        h(leaf(1), v(h(leaf(2), leaf(3), 0.4), leaf(4), 0.7), 0.3)
    }

    fn rects_overlap(a: Rect, b: Rect) -> bool {
        overlap_x(a, b) > 1e-4 && overlap_y(a, b) > 1e-4
    }

    #[test]
    fn layout_tiles_the_area_exactly_no_gaps_no_overlaps() {
        let t = sample_tree();
        let area = Rect { x: 10.0, y: 20.0, w: 837.0, h: 611.0 };
        let rects = t.layout(area);

        assert_eq!(rects.len(), t.leaves().len());
        let total: f32 = rects.iter().map(|(_, r)| r.area()).sum();
        assert!(approx(total, area.area()), "total {total} != area {}", area.area());

        for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                assert!(!rects_overlap(rects[i].1, rects[j].1), "{:?} and {:?} overlap", rects[i], rects[j]);
            }
        }
    }

    // --- neighbor ------------------------------------------------------------

    #[test]
    fn neighbor_w4_two_panes_side_by_side() {
        let t = h(leaf(1), leaf(2), 0.5);
        let area = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };
        assert_eq!(t.neighbor(area, 1, Dir::Right), Some(2));
        assert_eq!(t.neighbor(area, 2, Dir::Left), Some(1));
        assert_eq!(t.neighbor(area, 1, Dir::Left), None);
        assert_eq!(t.neighbor(area, 1, Dir::Up), None);
        assert_eq!(t.neighbor(area, 1, Dir::Down), None);
    }

    /// A 2x2 grid: two columns, each split top/bottom.
    ///   A(top-left)  B(top-right)
    ///   C(bot-left)  D(bot-right)
    fn grid_2x2() -> Tree {
        h(v(leaf(1), leaf(3), 0.5), v(leaf(2), leaf(4), 0.5), 0.5)
    }

    #[test]
    fn neighbor_2x2_grid() {
        let t = grid_2x2();
        let area = Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 };
        // A=1, B=2, C=3, D=4
        assert_eq!(t.neighbor(area, 1, Dir::Right), Some(2)); // A -> B
        assert_eq!(t.neighbor(area, 1, Dir::Down), Some(3)); // A -> C
        assert_eq!(t.neighbor(area, 4, Dir::Up), Some(2)); // D -> B
        assert_eq!(t.neighbor(area, 4, Dir::Left), Some(3)); // D -> C
        assert_eq!(t.neighbor(area, 2, Dir::Left), Some(1)); // B -> A
        assert_eq!(t.neighbor(area, 3, Dir::Right), Some(4)); // C -> D
    }

    /// An L-shaped layout where the naive "tree sibling" answer is wrong:
    ///   H( V(A, B),  C )
    /// A is top-left, B is bottom-left (A's tree sibling!), C is the full-height right column.
    /// Moving Right from A must reach C (geometry), not B (tree adjacency).
    fn l_shape() -> Tree {
        h(v(leaf(1), leaf(2), 0.5), leaf(3), 0.5)
    }

    #[test]
    fn neighbor_l_shape_ignores_tree_adjacency() {
        let t = l_shape();
        let area = Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 };
        // A(1) tree-sibling is B(2), but geometrically Right of A is C(3).
        assert_eq!(t.neighbor(area, 1, Dir::Right), Some(3));
        assert_eq!(t.neighbor(area, 2, Dir::Right), Some(3));
        // Down from A is its true sibling B.
        assert_eq!(t.neighbor(area, 1, Dir::Down), Some(2));
        // Left from C: both A and B overlap fully; A has the smaller edge distance tie... both
        // touch C's left edge equally, so reading order (A before B) breaks the tie.
        assert_eq!(t.neighbor(area, 3, Dir::Left), Some(1));
    }

    #[test]
    fn neighbor_missing_from_pane_is_none() {
        let t = h(leaf(1), leaf(2), 0.5);
        let area = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
        assert_eq!(t.neighbor(area, 99, Dir::Right), None);
    }

    // --- dividers / set_ratio ------------------------------------------------

    #[test]
    fn dividers_one_per_split_with_path_and_boundary_rect() {
        let t = h(leaf(1), leaf(2), 0.5);
        let area = Rect { x: 0.0, y: 0.0, w: 400.0, h: 200.0 };
        let divs = t.dividers(area);
        assert_eq!(divs.len(), 1);
        assert_eq!(divs[0].path, Vec::<bool>::new());
        assert_eq!(divs[0].axis, Axis::Horizontal);
        assert!(rect_approx(divs[0].rect, Rect { x: 200.0, y: 0.0, w: 0.0, h: 200.0 }));
    }

    #[test]
    fn dividers_nested_paths_and_extents_are_scoped_to_their_split() {
        // H(V(1,2), 3) at ratio .5 / .25
        let t = h(v(leaf(1), leaf(2), 0.25), leaf(3), 0.5);
        let area = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };
        let divs = t.dividers(area);
        assert_eq!(divs.len(), 2);

        let root = divs.iter().find(|d| d.path.is_empty()).expect("root divider");
        assert_eq!(root.axis, Axis::Horizontal);
        assert!(rect_approx(root.rect, Rect { x: 400.0, y: 0.0, w: 0.0, h: 400.0 }));

        let inner = divs.iter().find(|d| d.path == vec![false]).expect("inner divider");
        assert_eq!(inner.axis, Axis::Vertical);
        // Inner split's own area is the left half (0..400 wide), ratio .25 of height 400 = 100.
        assert!(rect_approx(inner.rect, Rect { x: 0.0, y: 100.0, w: 400.0, h: 0.0 }));
    }

    #[test]
    fn set_ratio_by_path_updates_the_right_split_and_clamps() {
        let mut t = h(v(leaf(1), leaf(2), 0.5), leaf(3), 0.5);
        assert!(t.set_ratio(&[false], 0.9));
        match &t {
            Tree::Split { first, .. } => match first.as_ref() {
                Tree::Split { ratio, .. } => assert_eq!(*ratio, 0.9),
                _ => panic!("expected inner split"),
            },
            _ => panic!("expected split"),
        }
        // Root ratio untouched.
        match &t {
            Tree::Split { ratio, .. } => assert_eq!(*ratio, 0.5),
            _ => panic!("expected split"),
        }

        assert!(t.set_ratio(&[], 5.0)); // clamps to 0.95
        match &t {
            Tree::Split { ratio, .. } => assert_eq!(*ratio, 0.95),
            _ => panic!("expected split"),
        }
        assert!(t.set_ratio(&[], -1.0)); // clamps to 0.05
        match &t {
            Tree::Split { ratio, .. } => assert_eq!(*ratio, 0.05),
            _ => panic!("expected split"),
        }
    }

    #[test]
    fn set_ratio_invalid_path_is_false() {
        let mut t = h(leaf(1), leaf(2), 0.5);
        assert!(!t.set_ratio(&[false], 0.5)); // path descends into a leaf
        assert!(!t.set_ratio(&[false, true, false], 0.5));
        let mut leaf_only = leaf(1);
        assert!(!leaf_only.set_ratio(&[], 0.5));
    }

    // --- leaves ----------------------------------------------------------

    #[test]
    fn leaves_reading_order() {
        let t = sample_tree();
        assert_eq!(t.leaves(), vec![1, 2, 3, 4]);
    }

    // --- serde round-trip --------------------------------------------------

    #[test]
    fn tree_serde_round_trips_nested_structure() {
        let t = sample_tree();
        let json = serde_json::to_string(&t).expect("serialize");
        let back: Tree = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(t, back);
    }

    #[test]
    fn leaf_serde_round_trip() {
        let t = leaf(42);
        let json = serde_json::to_string(&t).expect("serialize");
        let back: Tree = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(t, back);
    }
}
