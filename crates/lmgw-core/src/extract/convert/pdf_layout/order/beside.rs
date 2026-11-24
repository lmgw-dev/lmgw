//! Which units have another beside them: their vertical extents overlap
//! (each first shrunk by up to [`BESIDE_SLACK_PT`] at either end, so boxes
//! that only touch are not beside each other) and their x-ranges are
//! disjoint.
//!
//! `n log n`: the units are swept by the end of their vertical extent; every
//! unit starting above the swept end is in a Fenwick tree keyed by its own
//! end, holding the largest left edge and the smallest right edge; a unit
//! asks the tree over the units ending below its start. A unit is beside
//! another when the largest left edge among those it overlaps lies right
//! of its own right edge, or the smallest right edge left of its left edge.

use super::super::super::doc::Rect;

/// How much of a box's vertical extent at either end does not count for
/// "beside", in points (a quarter of its height at most).
pub const BESIDE_SLACK_PT: f32 = 2.0;

/// A Fenwick tree over prefix maxima.
struct MaxTree(Vec<f32>);

impl MaxTree {
    fn new(n: usize) -> Self {
        Self(vec![f32::NEG_INFINITY; n + 1])
    }

    /// Raise position `i` (0-based) to at least `v`.
    fn raise(&mut self, i: usize, v: f32) {
        let mut i = i + 1;
        while i < self.0.len() {
            if self.0[i] < v {
                self.0[i] = v;
            }
            i += i.isolate_lowest_one();
        }
    }

    /// The maximum over positions `0..n`.
    fn max_below(&self, n: usize) -> f32 {
        let mut i = n.min(self.0.len() - 1);
        let mut m = f32::NEG_INFINITY;
        while i > 0 {
            m = m.max(self.0[i]);
            i -= i.isolate_lowest_one();
        }
        m
    }
}

/// For each of `rects`, whether another is beside it.
pub(super) fn beside(rects: &[Rect]) -> Vec<bool> {
    let n = rects.len();
    let span = |r: &Rect| {
        let s = (r.height().max(0.0) / 4.0).min(BESIDE_SLACK_PT);
        (r.y0 + s, r.y1 - s)
    };
    let spans: Vec<(f32, f32)> = rects.iter().map(span).collect();
    // The ends, sorted descending: a unit's tree position is the number of
    // ends above its own, so "ends below `y`" is a prefix.
    let mut ends: Vec<f32> = spans.iter().map(|s| s.1).collect();
    ends.sort_by(|a, b| b.total_cmp(a));
    let pos = |y: f32| ends.partition_point(|e| *e > y);
    let mut by_start: Vec<usize> = (0..n).collect();
    by_start.sort_by(|&a, &b| spans[a].0.total_cmp(&spans[b].0));
    let mut by_end: Vec<usize> = (0..n).collect();
    by_end.sort_by(|&a, &b| spans[a].1.total_cmp(&spans[b].1));
    let mut left = MaxTree::new(n);
    let mut right = MaxTree::new(n);
    let mut out = vec![false; n];
    let mut next = 0;
    for &u in &by_end {
        let (a, b) = spans[u];
        // Every unit that starts above `u`'s end.
        while next < n && spans[by_start[next]].0 < b {
            let v = by_start[next];
            let p = pos(spans[v].1);
            left.raise(p, rects[v].x0);
            right.raise(p, -rects[v].x1);
            next += 1;
        }
        // Of those, the ones that end below `u`'s start.
        let k = pos(a);
        let r = &rects[u];
        out[u] = left.max_below(k) > r.x1 || -right.max_below(k) < r.x0;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
        Rect { x0, y0, x1, y1 }
    }

    /// The pairs by brute force, against the sweep.
    #[test]
    fn the_sweep_agrees_with_every_pair() {
        let rects = vec![
            r(57.0, 60.0, 540.0, 80.0),
            r(250.0, 94.0, 345.0, 105.0),
            r(57.0, 196.0, 137.0, 210.0),
            r(309.0, 196.0, 437.0, 207.0),
            r(57.0, 216.0, 286.0, 285.0),
            r(309.0, 213.0, 538.0, 282.0),
            r(57.0, 285.0, 286.0, 290.0),
            r(300.0, 400.0, 310.0, 400.0),
            r(10.0, 0.0, 20.0, 900.0),
        ];
        let slow: Vec<bool> = (0..rects.len())
            .map(|u| {
                let (a, b) = {
                    let s = (rects[u].height() / 4.0).min(BESIDE_SLACK_PT);
                    (rects[u].y0 + s, rects[u].y1 - s)
                };
                rects.iter().enumerate().any(|(v, o)| {
                    let s = (o.height() / 4.0).min(BESIDE_SLACK_PT);
                    v != u
                        && o.y0 + s < b
                        && o.y1 - s > a
                        && (o.x0 > rects[u].x1 || o.x1 < rects[u].x0)
                })
            })
            .collect();
        assert_eq!(beside(&rects), slow);
        // The title and the author line have nothing beside them but the
        // tall margin box.
        assert_eq!(
            beside(&rects[..8]),
            [false, false, true, true, true, true, false, false]
        );
    }
}
