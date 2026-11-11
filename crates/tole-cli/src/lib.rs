//! Host-side pieces of the CLI exposed as a library, so integration
//! tests exercise the REAL implementations — the approval flow and the
//! jailed file tools — instead of test-local re-implementations that
//! drift from production behavior (CodeCora scan 2026-09-18).
pub mod approvals;
pub mod approver;
pub mod auth;
pub mod compaction_host;
pub mod config;
pub mod config_apply;
pub mod config_trust;
pub mod doctor;
pub mod ecosystem;
pub mod events;
#[cfg(feature = "tui")]
pub mod export;
pub mod handoff;
pub mod protocol;
pub mod provider_chain;
#[cfg(feature = "shell-tools")]
pub mod review_gate;
pub mod serve_images;
pub mod session_host;
pub mod session_index;
pub mod session_settings;
pub mod session_tools;
pub mod sibling;
pub mod status;
pub mod tool_roster;
pub mod tools;
pub mod trust;
#[cfg(feature = "tui")]
pub mod tui_host;
pub mod turn_runner;
pub mod usage_report;
