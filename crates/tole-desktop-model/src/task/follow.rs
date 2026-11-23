//! Where the transcript list is scrolled to: pure rules, in logical pixels.
//!
//! - **Follow the bottom.** A task opens at its latest words. While the list
//!   follows, it is moved to the bottom whenever the content grows (a streamed
//!   answer, a new row, a measured row). The owner leaves the bottom by
//!   scrolling up (a wheel or a drag, anything that moves the offset
//!   themselves, by however little) and returns to it by scrolling down to
//!   within [`NEAR`] of the end, or with "Jump to latest" ([`Follow::jump`]). A list that grows
//!   under an owner who is reading higher up never moves.
//! - **Anchor.** Rows are drawn from estimated heights until measured, so a row
//!   above the reading position that turns out taller would push the text
//!   being read down. [`Anchor`] remembers the first row on screen and how far
//!   from the top it is; after each layout it says how far the list must move
//!   to put that row back where it was.
//!
//! The window supplies numbers (the offset it reads, the viewport, the total)
//! and applies what these return; neither type knows a toolkit.

/// Within this many pixels of the end counts as at the end.
pub const NEAR: f32 = 24.0;

/// Offsets closer than this are the same offset (pixel rounding).
const EPSILON: f32 = 0.5;

/// What [`Follow::step`] found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Step {
    /// Move the list to this offset (the end), or leave it.
    pub pin: Option<f32>,
    /// The owner moved the list since the last look.
    pub moved: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Follow {
    following: bool,
    /// The offset as of the last step: what the window set or read.
    last_top: f32,
}

impl Default for Follow {
    /// A list starts at the end.
    fn default() -> Self {
        Follow {
            following: true,
            last_top: 0.0,
        }
    }
}

impl Follow {
    pub fn following(&self) -> bool {
        self.following
    }

    /// The window drew the list scrolled to `top`, `viewport` tall, `total`
    /// high. A `top` that is not the last one was set by the owner (the window
    /// only changes it by what [`Step::pin`] and [`Anchor`] return, which are
    /// recorded here), so it decides whether the list follows.
    pub fn step(&mut self, top: f32, viewport: f32, total: f32) -> Step {
        let top = if top.is_finite() { top.max(0.0) } else { 0.0 };
        let end = (total - viewport).max(0.0);
        let moved = (top - self.last_top).abs() > EPSILON;
        if moved {
            // Leaving the end takes any upward move (a slow wheel moves a few
            // pixels at a time, and must not be read as "still near"), unless
            // the list was only clamped to a shorter end. Returning takes
            // coming within `NEAR` of it from above.
            let up = top < self.last_top;
            self.following = if up {
                end - top <= EPSILON
            } else {
                end - top <= NEAR
            };
        }
        let pin = (self.following && (end - top).abs() > EPSILON).then_some(end);
        self.last_top = pin.unwrap_or(top);
        Step { pin, moved }
    }

    /// The window moved the list by `delta` (an [`Anchor`] correction): not
    /// the owner's doing.
    pub fn shifted(&mut self, delta: f32) {
        self.last_top = (self.last_top + delta).max(0.0);
    }

    /// "Jump to latest": follow again; the next step pins the list.
    pub fn jump(&mut self) {
        self.following = true;
    }

    /// Whether to offer "Jump to latest": the owner left the end and the end
    /// is out of sight.
    pub fn away(&self, top: f32, viewport: f32, total: f32) -> bool {
        !self.following && (total - viewport).max(0.0) - top > NEAR
    }
}

/// The first row on screen, and where its top edge is relative to the top of
/// the viewport (negative when it is partly above).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Anchor {
    at: Option<(usize, f32)>,
}

/// A built row as laid out: its index, its top edge relative to the top of
/// the viewport, and its height.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Laid {
    pub row: usize,
    pub top: f32,
    pub height: f32,
}

impl Anchor {
    /// Take the layout of the built rows (in order). Returns how far the list
    /// must move (positive: down, toward the end) so the row that was first on
    /// screen stays where it was; 0 when `hold` is false (the list follows
    /// the end, or the owner moved it this frame) or the row is not built.
    pub fn settle(&mut self, rows: &[Laid], hold: bool) -> f32 {
        let delta = match self.at {
            Some((row, was)) if hold => rows
                .iter()
                .find(|r| r.row == row)
                .map_or(0.0, |r| r.top - was),
            _ => 0.0,
        };
        let delta = if delta.abs() > EPSILON { delta } else { 0.0 };
        self.at = rows
            .iter()
            .find(|r| r.top + r.height > 0.0)
            .map(|r| (r.row, r.top - delta));
        delta
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEW: f32 = 600.0;

    #[test]
    fn a_list_follows_the_end_while_it_grows_and_until_the_owner_scrolls_up() {
        let mut f = Follow::default();
        // Content grows from 500 to 900 to 1,400: pinned each time.
        assert_eq!(f.step(0.0, VIEW, 500.0).pin, None);
        assert_eq!(f.step(0.0, VIEW, 900.0).pin, Some(300.0));
        assert_eq!(f.step(300.0, VIEW, 900.0).pin, None);
        assert_eq!(f.step(300.0, VIEW, 1_400.0).pin, Some(800.0));
        // The owner scrolls up: not moved back, however much it grows.
        let step = f.step(500.0, VIEW, 1_400.0);
        assert_eq!((step.pin, step.moved, f.following()), (None, true, false));
        assert_eq!(f.step(500.0, VIEW, 3_000.0).pin, None);
        assert!(f.away(500.0, VIEW, 3_000.0));
    }

    #[test]
    fn scrolling_back_near_the_end_or_jumping_follows_again() {
        let mut f = Follow::default();
        f.step(0.0, VIEW, 2_000.0); // pinned to 1,400
        f.step(1_000.0, VIEW, 2_000.0);
        assert!(!f.following());
        f.step(1_390.0, VIEW, 2_000.0); // within NEAR of 1,400
        assert!(f.following());
        f.step(100.0, VIEW, 2_000.0);
        assert!(!f.following());
        f.jump();
        assert_eq!(f.step(100.0, VIEW, 2_000.0).pin, Some(1_400.0));
        assert!(!f.away(1_400.0, VIEW, 2_000.0));
    }

    #[test]
    fn a_slow_upward_scroll_leaves_the_end_and_only_a_downward_one_returns_within_near() {
        let mut f = Follow::default();
        f.step(0.0, VIEW, 2_000.0); // pinned to 1,400
                                    // A few pixels up at a time, as a slow wheel does: the first one leaves.
        let step = f.step(1_390.0, VIEW, 2_000.0);
        assert!(step.moved && !f.following(), "10 px up is leaving");
        assert_eq!(step.pin, None);
        f.step(1_380.0, VIEW, 2_000.0);
        assert!(!f.following());
        // Down again, but still more than NEAR from the end: not following.
        f.step(1_300.0, VIEW, 2_000.0);
        f.step(1_350.0, VIEW, 2_000.0);
        assert!(!f.following());
        // Down to within NEAR: follows.
        f.step(1_380.0, VIEW, 2_000.0);
        assert!(f.following());
        // A shorter end (the viewport grew) clamps the offset upward: still the end.
        f.step(1_400.0, VIEW, 2_000.0);
        f.step(1_300.0, 700.0, 2_000.0);
        assert!(
            f.following(),
            "a clamp to the new end is not the owner leaving"
        );
    }

    #[test]
    fn a_short_transcript_and_odd_numbers_stay_at_the_top() {
        let mut f = Follow::default();
        assert_eq!(f.step(0.0, VIEW, 200.0).pin, None);
        assert_eq!(f.step(f32::NAN, VIEW, f32::NAN).pin, None);
        assert_eq!(f.step(-50.0, VIEW, 100.0).pin, None);
        assert!(f.following());
    }

    #[test]
    fn a_correction_of_the_window_is_not_read_as_the_owner_scrolling() {
        let mut f = Follow::default();
        f.step(0.0, VIEW, 2_000.0); // pinned to 1,400
        f.step(1_400.0, VIEW, 2_000.0);
        // The anchor moved the list by 40 and told the follow: still following.
        f.shifted(40.0);
        assert!(!f.step(1_440.0, VIEW, 2_040.0).moved);
        assert!(f.following());
    }

    fn laid(rows: &[(usize, f32, f32)]) -> Vec<Laid> {
        (rows.iter())
            .map(|&(row, top, height)| Laid { row, top, height })
            .collect()
    }

    #[test]
    fn a_row_above_that_grows_moves_the_list_so_the_text_being_read_stays() {
        let mut a = Anchor::default();
        // Row 5 is the first on screen, 20 px above the top edge.
        assert_eq!(
            a.settle(&laid(&[(5, -20.0, 80.0), (6, 60.0, 80.0)]), true),
            0.0
        );
        // A row above measured 30 px taller: row 5 is now 10 px below the edge.
        let delta = a.settle(&laid(&[(5, 10.0, 80.0), (6, 90.0, 80.0)]), true);
        assert_eq!(delta, 30.0);
        // After the list moved by that much, nothing is left to correct.
        assert_eq!(
            a.settle(&laid(&[(5, -20.0, 80.0), (6, 60.0, 80.0)]), true),
            0.0
        );
    }

    #[test]
    fn nothing_is_held_while_following_or_after_the_owner_scrolled() {
        let mut a = Anchor::default();
        a.settle(&laid(&[(5, -20.0, 80.0)]), true);
        assert_eq!(a.settle(&laid(&[(5, 10.0, 80.0)]), false), 0.0);
        // The anchor is the first row on screen as of the last look.
        assert_eq!(a.settle(&laid(&[(5, 10.0, 80.0)]), true), 0.0);
        // A row that is not built any more is no anchor, and a tiny shift is rounding.
        assert_eq!(a.settle(&laid(&[(9, 0.0, 10.0)]), true), 0.0);
        assert_eq!(a.settle(&laid(&[(9, 0.2, 10.0)]), true), 0.0);
        assert_eq!(a.settle(&[], true), 0.0);
    }
}
