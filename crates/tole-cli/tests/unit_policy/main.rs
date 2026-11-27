//! Unit-style tests of the host policy modules (routing and discussion-role config, delegation audit).
//! Moved out of `src/` (#1249): these tests only use the public API of the crate.

#[path = "../common/test_dir.rs"]
mod test_dir;

mod config_discussion_roles;
mod config_routing;
mod delegation_audit;
mod provider_chain;
