//! Unit-style tests of the engine modules (router, advisor, plugin scan, skill drafts).
//! Moved out of `src/` (#1249): these tests only use the public API of the crate.

#[path = "../support/test_dir.rs"]
mod test_dir;

mod advisor;
mod plugin_scan;
mod router;
mod skills_curator;
mod skills_draft;
