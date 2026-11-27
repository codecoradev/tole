//! Unit-style tests of the skill host commands (drafts, propose).
//! Moved out of `src/` (#1249): these tests only use the public API of the crate.

#[path = "../common/test_dir.rs"]
mod test_dir;

mod skill_drafts;
mod skill_propose;
