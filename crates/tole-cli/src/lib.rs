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
pub mod serve_support;
// `serve_support/` holds these modules; the re-exports keep the old top-level paths working.
#[doc(inline)]
pub use serve_support::auth;
#[doc(inline)]
pub use serve_support::events;
#[doc(inline)]
pub use serve_support::images as serve_images;
#[doc(inline)]
pub use serve_support::protocol;
pub mod session;
// `session/` holds these modules; the re-exports keep the old top-level paths working.
#[doc(inline)]
pub use session::compaction as compaction_host;
#[doc(inline)]
#[cfg(feature = "tui")]
pub use session::export;
#[doc(inline)]
pub use session::host as session_host;
#[doc(inline)]
pub use session::index as session_index;
#[doc(inline)]
pub use session::runner as turn_runner;
#[doc(inline)]
pub use session::status;
#[doc(inline)]
pub use session::tools as session_tools;
#[doc(inline)]
#[cfg(feature = "tui")]
pub use session::tui as tui_host;
#[doc(inline)]
pub use session::usage as usage_report;
pub mod config;
// `config/` holds the modules below; these re-exports keep the old top-level paths working.
#[doc(inline)]
pub use config::apply as config_apply;
#[doc(inline)]
pub use config::discussion_roles;
#[doc(inline)]
pub use config::hook_filter;
#[doc(inline)]
pub use config::owner_only;
#[doc(inline)]
pub use config::profile;
#[doc(inline)]
pub use config::project_trust as config_trust;
#[doc(inline)]
pub use config::routing;
#[doc(inline)]
pub use config::session_settings;
#[doc(inline)]
pub use config::trust;
#[cfg(feature = "shell-tools")]
pub mod cron;
pub mod skill;
// `skill/` holds these modules; the re-exports keep the old top-level paths working.
#[doc(inline)]
pub use skill::approve as skill_approve;
#[doc(inline)]
pub use skill::curator as curator_cmd;
#[doc(inline)]
pub use skill::drafts as skill_drafts_cli;
#[doc(inline)]
pub use skill::outcome as skill_outcome;
#[doc(inline)]
pub use skill::propose as skill_propose;
#[doc(inline)]
pub use skill::recall as skill_recall;
pub mod discuss;
// `discuss/` holds the modules below; these re-exports keep the old top-level paths working.
#[doc(inline)]
pub use discuss::audit as audit_discussion;
#[doc(inline)]
pub use discuss::host as discussion_host;
#[doc(inline)]
pub use discuss::tool as discuss_tool;
pub mod doctor;
pub mod ecosystem;
pub mod handoff;
pub mod hook_cli;
pub mod hooks;
pub mod plugin;
// `plugin/` holds these modules; the re-exports keep the old top-level paths working.
#[doc(inline)]
#[cfg(feature = "mcp")]
pub use plugin::cmd as plugin_cmd;
#[doc(inline)]
pub use plugin::gate as plugin_gate;
pub mod provider_chain;
#[cfg(feature = "shell-tools")]
pub mod review_gate;
pub mod sibling;
pub mod tool_roster;
pub mod tools;
#[cfg(feature = "web-builtin")]
pub mod web_builtin;
