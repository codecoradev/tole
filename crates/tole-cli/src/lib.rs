//! Host-side pieces of the CLI exposed as a library, so integration
//! tests exercise the REAL implementations — the approval flow and the
//! jailed file tools — instead of test-local re-implementations that
//! drift from production behavior (CodeCora scan 2026-09-18).
pub mod advisor_host;
pub mod approvals;
pub mod delegation;
// `delegation/` holds these modules; the re-exports keep the old top-level paths working.
#[doc(inline)]
pub use delegation::approvals_owner;
#[doc(inline)]
pub use delegation::approver as delegated_approver;
#[doc(inline)]
pub use delegation::audit;
#[doc(inline)]
pub use delegation::decide as delegate_decide;
#[doc(inline)]
pub use delegation::delegate;
#[doc(inline)]
pub use delegation::ledger_file;
#[doc(inline)]
pub use delegation::machine_guard;
#[doc(inline)]
pub use delegation::serve_delegation;
pub mod approver;
pub mod audit_discussion;
pub mod auth;
pub mod compaction_host;
pub mod config;
pub mod config_apply;
pub mod config_trust;
#[cfg(feature = "shell-tools")]
pub mod cron;
pub mod curator_cmd;
pub mod discuss;
pub mod discuss_tool;
pub mod discussion_host;
pub mod discussion_roles;
pub mod doctor;
pub mod ecosystem;
pub mod events;
#[cfg(feature = "tui")]
pub mod export;
pub mod handoff;
pub mod hook_cli;
pub mod hook_filter;
pub mod hooks;
pub mod owner_only;
pub mod plugin;
// `plugin/` holds these modules; the re-exports keep the old top-level paths working.
#[doc(inline)]
#[cfg(feature = "mcp")]
pub use plugin::cmd as plugin_cmd;
#[doc(inline)]
pub use plugin::gate as plugin_gate;
pub mod profile;
pub mod protocol;
pub mod provider_chain;
#[cfg(feature = "shell-tools")]
pub mod review_gate;
pub mod routing;
pub mod serve_images;
pub mod session_host;
pub mod session_index;
pub mod session_settings;
pub mod session_tools;
pub mod sibling;
pub mod skill_approve;
pub mod skill_drafts_cli;
pub mod skill_outcome;
pub mod skill_propose;
pub mod skill_recall;
pub mod status;
pub mod tool_roster;
pub mod tools;
pub mod trust;
#[cfg(feature = "tui")]
pub mod tui_host;
pub mod turn_runner;
pub mod usage_report;
#[cfg(feature = "web-builtin")]
pub mod web_builtin;
