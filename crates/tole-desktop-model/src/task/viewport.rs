//! Virtualised list arithmetic: which rows of a long transcript to build.
//! Rows start at an estimate and are corrected when measured; the rows outside
//! the window are not built, [`Visible::before`] and [`Visible::after`] pad
//! the list. Pure numbers in logical pixels.

/// Heights of every row, with running offsets.
#[derive(Debug, Clone)]
pub struct ListLayout {
    estimate: f32,
    heights: Vec<f32>,
    /// `tops[i]` is where row `i` starts; the last entry is the total.
    tops: Vec<f32>,
}

impl ListLayout {
    /// No rows; `estimate` is the height of an unmeasured row.
    pub fn new(estimate: f32) -> Self {
        let estimate = if estimate.is_finite() && estimate > 0.0 {
            estimate
        } else {
            1.0
        };
        ListLayout {
            estimate,
            heights: Vec::new(),
            tops: vec![0.0],
        }
    }

    pub fn len(&self) -> usize {
        self.heights.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heights.is_empty()
    }

    /// Height of all rows.
    pub fn total(&self) -> f32 {
        self.tops[self.heights.len()]
    }

    /// Where row `i` starts (the total for `i >= len`).
    pub fn top(&self, i: usize) -> f32 {
        self.tops[i.min(self.heights.len())]
    }

    /// Grow with estimates, or cut from the end, to `n` rows.
    pub fn set_len(&mut self, n: usize) {
        let from = self.heights.len().min(n);
        self.heights.resize(n, self.estimate);
        self.rebuild(from);
    }

    /// Record the measured height of row `i`. A row out of range, or a height
    /// that is not a finite number of at least zero, changes nothing.
    pub fn set_height(&mut self, i: usize, height: f32) {
        if i < self.heights.len() && height.is_finite() && height >= 0.0 {
            self.heights[i] = height;
            self.rebuild(i);
        }
    }

    /// The row containing offset `y`, clamped to the first and last row.
    pub fn index_at(&self, y: f32) -> usize {
        let n = self.heights.len();
        if n == 0 || y.is_nan() {
            return 0;
        }
        // Rows whose bottom is at or above y lie before it.
        self.tops[1..]
            .partition_point(|bottom| *bottom <= y)
            .min(n - 1)
    }

    fn rebuild(&mut self, from: usize) {
        self.tops.truncate(from + 1);
        let mut top = self.tops[from];
        for h in &self.heights[from..] {
            top += h;
            self.tops.push(top);
        }
    }
}

/// The rows `start..end` to build, and the height of the rows before `start`
/// and from `end` on, which stand in as padding.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Visible {
    pub start: usize,
    pub end: usize,
    pub before: f32,
    pub after: f32,
}

/// The rows a window of `viewport` height at `scroll_top` shows, plus
/// `overscan` pixels each side so a fast scroll shows no gap.
pub fn visible(layout: &ListLayout, scroll_top: f32, viewport: f32, overscan: f32) -> Visible {
    let (start, end) = if layout.is_empty() {
        (0, 0)
    } else {
        let first = (scroll_top - overscan).max(0.0);
        let last = scroll_top.max(0.0) + viewport.max(0.0) + overscan;
        let end = layout.index_at(last) + 1;
        (layout.index_at(first), end.min(layout.len()))
    };
    Visible {
        start,
        end,
        before: layout.top(start),
        after: layout.total() - layout.top(end),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(n: usize, h: f32) -> ListLayout {
        let mut l = ListLayout::new(h);
        l.set_len(n);
        l
    }

    #[test]
    fn a_long_list_builds_only_the_window_and_the_padding_adds_up() {
        let l = layout(100_000, 40.0);
        let v = visible(&l, 1_000_000.0, 800.0, 200.0);
        assert!(v.end - v.start <= 32, "built {} rows", v.end - v.start);
        let built = l.top(v.end) - l.top(v.start);
        assert_eq!(v.before + built + v.after, l.total());
        let tail = layout(2_000, 48.0);
        let v = visible(&tail, tail.total() - 900.0, 900.0, 300.0);
        assert!(v.end == 2_000 && v.end - v.start < 40);
    }

    #[test]
    fn the_window_covers_every_pixel_of_the_viewport() {
        let mut l = layout(500, 30.0);
        (0..500)
            .step_by(7)
            .for_each(|i| l.set_height(i, 90.0 + i as f32 % 13.0));
        for top in [0.0, 1.0, 999.5, 4_321.0, l.total() - 600.0] {
            let v = visible(&l, top, 600.0, 0.0);
            assert!(l.top(v.start) <= top && l.top(v.end) >= (top + 600.0).min(l.total()));
        }
    }

    #[test]
    fn index_at_clamps_and_bad_heights_change_nothing() {
        let mut l = layout(10, 10.0);
        let found = [-5.0, 9.9, 10.0, 1e9, f32::NAN].map(|y| l.index_at(y));
        assert_eq!(found, [0, 0, 1, 9, 0]);
        l.set_height(1, 25.0);
        assert_eq!((l.top(2), l.total()), (35.0, 115.0));
        [(99, 5.0), (0, f32::NAN), (0, -1.0)]
            .iter()
            .for_each(|(i, h)| l.set_height(*i, *h));
        assert_eq!(l.total(), 115.0);
        assert_eq!(visible(&ListLayout::new(40.0), 0.0, 800.0, 100.0).end, 0);
    }
}
