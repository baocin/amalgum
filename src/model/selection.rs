//! The graph's multi-commit selection (§5.14), headless: a plain click selects one commit,
//! `Shift+click` / `Shift+↓/↑` select the contiguous range from the anchor, `Mod+click` toggles
//! one commit. Commits are named by id and placed by the caller's row order (graph order,
//! newest first), so the selection survives the log being reloaded or paginated.

use std::collections::BTreeSet;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MultiSelect {
    ids: BTreeSet<String>,
    /// Where a `Shift` range starts: the last plain- or `Mod`-clicked commit.
    anchor: Option<String>,
    /// The commit the keyboard moves from and the details follow: the last one clicked or
    /// reached with `Shift+↓/↑`.
    cursor: Option<String>,
}

impl MultiSelect {
    /// A plain click: just `id`, which also becomes the anchor.
    pub fn select(&mut self, id: &str) {
        self.ids = BTreeSet::from([id.to_string()]);
        self.anchor = Some(id.to_string());
        self.cursor = Some(id.to_string());
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// `Mod+click`: add `id`, or remove it when already selected. The toggled commit becomes
    /// the anchor; removing the cursor moves it to the nearest remaining commit in `order`.
    pub fn toggle<S: AsRef<str>>(&mut self, order: &[S], id: &str) {
        if !self.ids.remove(id) {
            self.ids.insert(id.to_string());
            self.anchor = Some(id.to_string());
            self.cursor = Some(id.to_string());
            return;
        }
        if self.ids.is_empty() {
            self.clear();
            return;
        }
        let at = index_of(order, id);
        let nearest = self
            .ordered(order)
            .into_iter()
            .min_by_key(|(i, _)| at.map_or(*i, |a| i.abs_diff(a)))
            .map(|(_, id)| id)
            .or_else(|| self.ids.iter().next().cloned());
        self.anchor = nearest.clone();
        self.cursor = nearest;
    }

    /// `Shift+click`: the contiguous rows from the anchor to `id`, replacing the rest. Without
    /// an anchor in `order` (nothing selected, or it is no longer loaded) it is a plain click.
    pub fn extend_to<S: AsRef<str>>(&mut self, order: &[S], id: &str) {
        let (Some(from), Some(to)) =
            (self.anchor.as_deref().and_then(|a| index_of(order, a)), index_of(order, id))
        else {
            self.select(id);
            return;
        };
        let (lo, hi) = (from.min(to), from.max(to));
        self.ids = order[lo..=hi].iter().map(|s| s.as_ref().to_string()).collect();
        self.cursor = Some(id.to_string());
    }

    /// `Shift+↓` (`delta` 1) / `Shift+↑` (-1): move the cursor one row, clamped, and select from
    /// the anchor to it. From nothing it selects the first row. Returns the new cursor.
    pub fn step<S: AsRef<str>>(&mut self, order: &[S], delta: isize) -> Option<String> {
        if order.is_empty() {
            return None;
        }
        let Some(cur) = self.cursor.as_deref().and_then(|c| index_of(order, c)) else {
            let first = order[0].as_ref().to_string();
            self.select(&first);
            return Some(first);
        };
        let next = cur.saturating_add_signed(delta).min(order.len() - 1);
        let id = order[next].as_ref().to_string();
        self.extend_to(order, &id);
        Some(id)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.ids.contains(id)
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn cursor(&self) -> Option<&str> {
        self.cursor.as_deref()
    }

    /// The selected commits found in `order`, as `(row, id)` in row order (newest first).
    pub fn ordered<S: AsRef<str>>(&self, order: &[S]) -> Vec<(usize, String)> {
        order
            .iter()
            .enumerate()
            .filter(|(_, s)| self.ids.contains(s.as_ref()))
            .map(|(i, s)| (i, s.as_ref().to_string()))
            .collect()
    }

    /// Whether the selection is one unbroken run of rows in `order` (every id found).
    pub fn is_contiguous<S: AsRef<str>>(&self, order: &[S]) -> bool {
        let rows = self.ordered(order);
        rows.len() == self.ids.len()
            && !rows.is_empty()
            && rows.last().map(|l| l.0) == Some(rows[0].0 + rows.len() - 1)
    }
}

fn index_of<S: AsRef<str>>(order: &[S], id: &str) -> Option<usize> {
    order.iter().position(|s| s.as_ref() == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROWS: [&str; 6] = ["f", "e", "d", "c", "b", "a"];

    fn ids(s: &MultiSelect) -> Vec<String> {
        s.ordered(&ROWS).into_iter().map(|(_, id)| id).collect()
    }

    #[test]
    fn plain_click_selects_one_and_anchors_there() {
        let mut s = MultiSelect::default();
        s.select("e");
        s.select("c");
        assert_eq!(ids(&s), ["c"]);
        assert_eq!(s.cursor(), Some("c"));
    }

    #[test]
    fn shift_click_selects_the_contiguous_range_from_the_anchor_either_way() {
        let mut s = MultiSelect::default();
        s.select("e");
        s.extend_to(&ROWS, "b");
        assert_eq!(ids(&s), ["e", "d", "c", "b"]);
        assert!(s.is_contiguous(&ROWS));
        // A second Shift+click re-ranges from the same anchor, it does not grow the old range.
        s.extend_to(&ROWS, "f");
        assert_eq!(ids(&s), ["f", "e"]);
        assert_eq!(s.cursor(), Some("f"));
    }

    #[test]
    fn shift_click_without_an_anchor_is_a_plain_click() {
        let mut s = MultiSelect::default();
        s.extend_to(&ROWS, "c");
        assert_eq!(ids(&s), ["c"]);
        s.select("gone");
        s.extend_to(&ROWS, "b");
        assert_eq!(ids(&s), ["b"], "an anchor that is no longer loaded");
    }

    #[test]
    fn mod_click_toggles_and_moves_the_anchor() {
        let mut s = MultiSelect::default();
        s.select("f");
        s.toggle(&ROWS, "d");
        s.toggle(&ROWS, "a");
        assert_eq!(ids(&s), ["f", "d", "a"]);
        assert!(!s.is_contiguous(&ROWS));
        // Shift+click ranges from the last toggled commit.
        s.extend_to(&ROWS, "c");
        assert_eq!(ids(&s), ["c", "b", "a"]);
        // Toggling the cursor off moves it to the nearest remaining row.
        s.toggle(&ROWS, "c");
        assert_eq!(ids(&s), ["b", "a"]);
        assert_eq!(s.cursor(), Some("b"));
        s.toggle(&ROWS, "b");
        s.toggle(&ROWS, "a");
        assert!(s.is_empty());
        assert_eq!(s.cursor(), None);
    }

    #[test]
    fn shift_arrows_grow_and_shrink_around_the_anchor() {
        let mut s = MultiSelect::default();
        s.select("d");
        assert_eq!(s.step(&ROWS, 1).as_deref(), Some("c"));
        s.step(&ROWS, 1);
        assert_eq!(ids(&s), ["d", "c", "b"]);
        s.step(&ROWS, -1);
        s.step(&ROWS, -1);
        s.step(&ROWS, -1);
        assert_eq!(ids(&s), ["e", "d"], "past the anchor the range flips upward");
        for _ in 0..10 {
            s.step(&ROWS, -1);
        }
        assert_eq!(s.cursor(), Some("f"), "clamped at the first row");
        for _ in 0..10 {
            s.step(&ROWS, 1);
        }
        assert_eq!(ids(&s), ["d", "c", "b", "a"], "clamped at the last row");
    }

    #[test]
    fn shift_arrow_from_nothing_selects_the_first_row() {
        let mut s = MultiSelect::default();
        assert_eq!(s.step(&ROWS, 1).as_deref(), Some("f"));
        assert_eq!(s.len(), 1);
        assert_eq!(MultiSelect::default().step(&[] as &[&str], 1), None);
    }

    #[test]
    fn contiguity_needs_every_id_loaded() {
        let mut s = MultiSelect::default();
        s.select("b");
        s.toggle(&ROWS, "a");
        assert!(s.is_contiguous(&ROWS));
        s.toggle(&ROWS, "zz");
        assert!(!s.is_contiguous(&ROWS), "a commit outside the loaded rows");
        assert!(!MultiSelect::default().is_contiguous(&ROWS));
    }
}
