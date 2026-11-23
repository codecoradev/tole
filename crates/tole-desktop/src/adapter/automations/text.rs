//! Bounding what a server sends before it is copied into the model's mirror
//! types.
//!
//! The model cleans every string it draws (control characters, marks that hide
//! or reorder text, a cut with an ellipsis), so the adapter does not clean a
//! second, different way. What it adds is a bound on the size of the copy: a
//! megabyte of text in a job name costs the model one pass to clean and the
//! page 80 characters, so there is no reason to keep a megabyte in the mirror
//! type. A cut always ends in an ellipsis, which no word the model compares
//! against (`active`, `cli`, a status) and no cron expression ends in, so a cut
//! value never reads as a known one.

/// Longest text field kept (names, schedules, words of the server). Far above
/// what the model draws (80 to 256 characters).
pub(super) const TEXT_MAX: usize = 1_024;
/// Longest run output kept; the model keeps 600 of it.
pub(super) const OUTPUT_MAX: usize = 4_096;

/// The first `max` characters of `s` and an ellipsis when it is longer, and
/// whether it was cut. Never slices a string: it counts characters.
pub(super) fn cut(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max || s.chars().nth(max).is_none() {
        return (s.to_owned(), false);
    }
    let mut kept: String = s.chars().take(max).collect();
    kept.push('\u{2026}');
    (kept, true)
}

/// [`cut`] without the flag.
pub(super) fn bounded(s: &str, max: usize) -> String {
    cut(s, max).0
}
