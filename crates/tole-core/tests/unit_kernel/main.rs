//! Unit-style tests of kernel modules (approval, turn compaction, provider failover).
//! Moved out of `src/` (#1249): these tests only use the public API of the crate.

#[path = "../support/test_dir.rs"]
mod test_dir;

mod approval;
mod provider_failover;
mod turn_compaction;
