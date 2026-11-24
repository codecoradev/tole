//! What the window learns from the last frame of the transcript list: where
//! each row it built ended up, and which heights the driver does not know yet.
//! Plain numbers, no toolkit; the list in `list` feeds it what the toolkit laid out.
//!
//! The list is a column of direct children: a spacer for the rows before the
//! window, then each built row, then a spacer for the rows after
//! (`Visible::before` and `Visible::after`), then anything else (the mark).

use std::collections::HashMap;

use tole_desktop_model::task::Laid;

/// Where the rows of a frame sit among the children of the list.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Drawn {
    /// The first built row (`Visible::start`).
    pub start: usize,
    /// How many rows were built.
    pub count: usize,
    /// The heights of the two spacers the frame was built with
    /// (`Visible::before` and `Visible::after`): how a layout is told to be of
    /// that frame and not of the one before.
    pub before: f32,
    pub after: f32,
}

/// The built rows as laid out, with their tops relative to the top of the
/// viewport (`top` is how far the list is scrolled). `children` is each direct
/// child's start in the content and its height. A layout that is not of the
/// frame that built the rows (fewer children, or spacers of other heights)
/// says nothing.
pub fn rows(drawn: Drawn, children: &[(f32, f32)], top: f32) -> Vec<Laid> {
    let spacers = [(0, drawn.before), (1 + drawn.count, drawn.after)];
    let of_this_frame = spacers.iter().all(|&(at, height)| {
        children
            .get(at)
            .is_some_and(|c| (c.1 - height).abs() <= SAME)
    });
    let built = children.get(1..1 + drawn.count).filter(|_| of_this_frame);
    let Some(built) = built else {
        return Vec::new();
    };
    (built.iter().enumerate())
        .map(|(i, &(start, height))| Laid {
            row: drawn.start + i,
            top: start - top,
            height,
        })
        .collect()
}

/// Heights already given to the driver, so a frame that measures the same rows
/// again says nothing.
#[derive(Debug, Default)]
pub struct Reported(HashMap<usize, f32>);

/// Heights this close are the same height (pixel rounding).
const SAME: f32 = 0.5;

impl Reported {
    /// The heights of `laid` that the driver does not have yet or has wrong;
    /// they count as given from now on.
    pub fn news(&mut self, laid: &[Laid]) -> Vec<(usize, f32)> {
        let mut news = Vec::new();
        for row in laid {
            let known = self.0.get(&row.row).copied();
            if known.is_none_or(|h| (h - row.height).abs() > SAME) {
                self.0.insert(row.row, row.height);
                news.push((row.row, row.height));
            }
        }
        news
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_found_between_the_spacers_and_placed_against_the_viewport() {
        // spacer 0..400, rows 400..480 and 480..600, spacer 600..900, a mark.
        let children = [
            (0.0, 400.0),
            (400.0, 80.0),
            (480.0, 120.0),
            (600.0, 300.0),
            (900.0, 30.0),
        ];
        let drawn = Drawn {
            start: 5,
            count: 2,
            before: 400.0,
            after: 300.0,
        };
        let laid = rows(drawn, &children, 450.0);
        let got: Vec<_> = laid.iter().map(|l| (l.row, l.top, l.height)).collect();
        assert_eq!(got, [(5, -50.0, 80.0), (6, 30.0, 120.0)]);
    }

    #[test]
    fn a_layout_older_than_the_rows_says_nothing() {
        let old = [(0.0, 10.0), (10.0, 50.0)];
        let at = |count, before, after| Drawn {
            start: 0,
            count,
            before,
            after,
        };
        assert!(rows(at(3, 10.0, 0.0), &old, 0.0).is_empty());
        assert!(rows(at(0, 0.0, 0.0), &[], 0.0).is_empty());
        // The right number of children, but spacers of the frame before.
        let children = [(0.0, 400.0), (400.0, 80.0), (480.0, 300.0)];
        assert!(rows(at(1, 350.0, 300.0), &children, 0.0).is_empty());
        assert!(rows(at(1, 400.0, 250.0), &children, 0.0).is_empty());
        assert_eq!(rows(at(1, 400.0, 300.0), &children, 0.0).len(), 1);
    }

    fn laid(row: usize, height: f32) -> Laid {
        Laid {
            row,
            top: 0.0,
            height,
        }
    }

    #[test]
    fn only_a_height_the_driver_lacks_or_has_wrong_is_reported() {
        let mut reported = Reported::default();
        assert_eq!(
            reported.news(&[laid(3, 80.0), laid(4, 40.0)]),
            [(3, 80.0), (4, 40.0)]
        );
        // The same frame again, with a rounding wobble: nothing.
        assert!(reported.news(&[laid(3, 80.2), laid(4, 39.8)]).is_empty());
        // Row 4 grew (its text wrapped), row 5 is new.
        assert_eq!(
            reported.news(&[laid(4, 64.0), laid(5, 20.0)]),
            [(4, 64.0), (5, 20.0)]
        );
        assert!(reported.news(&[laid(4, 64.0)]).is_empty());
    }
}
