//! Host-side pieces of the CLI exposed as a library, so integration
//! tests exercise the REAL implementations — the approval flow and the
//! jailed file tools — instead of test-local re-implementations that
//! drift from production behavior (CodeCora scan 2026-09-18).
pub mod approvals;
pub mod approver;
pub mod auth;
pub mod config;
pub mod config_apply;
pub mod config_trust;
pub mod doctor;
pub mod ecosystem;
pub mod handoff;
pub mod provider_chain;
pub mod session_host;
pub mod session_index;
pub mod session_tools;
pub mod sibling;
pub mod tool_roster;
pub mod tools;
pub mod trust;
pub mod turn_runner;
pub mod usage_report;
