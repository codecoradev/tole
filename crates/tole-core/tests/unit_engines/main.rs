//! Unit-style tests of the engine modules (router, advisor, skill drafts).
//! Moved out of `src/` (#1249): these tests only use the public API of the crate.

#[path = "../support/test_dir.rs"]
mod test_dir;

mod advisor;
mod router;
mod skills_draft;
