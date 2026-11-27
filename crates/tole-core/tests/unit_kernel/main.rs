//! Unit-style tests of kernel modules (approval, tool registry, turn compaction, provider failover, the
//! problem mapping).
//! Moved out of `src/` (#1249): these tests only use the public API of the crate.

#[path = "../support/test_dir.rs"]
mod test_dir;

mod approval;
mod problem;
mod provider_failover;
mod tool_registry;
mod transcript;
mod turn_compaction;
