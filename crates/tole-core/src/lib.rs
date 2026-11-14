//! tole-core — durable agent harness core.
//!
//! Platform-agnostic library. No stdin/stdout, no CLI assumptions.
//! Designed to be embeddable via CLI, desktop app, or flutter_rust_bridge FFI.
//!
//! # Mobile / embedder profile
//!
//! Subprocess-backed tools (`run_command`, `git`, `gh`, `cora_search`,
//! `uteke`) live behind the `shell-tools` feature — default-on for the
//! CLI host. Embedders on platforms without a shell (Android/iOS via
//! flutter_rust_bridge) build with `--no-default-features` and register
//! their own tool set (e.g. in-process memory adapters). Everything
//! else — JSONL storage, state machine, turn loop, provider — is pure
//! std + serde and compiles everywhere.
//!
//! Layout (v0 scope):
//! - `entry`    — write-once conversation tree entries
//! - `register` — namespaced mutable cells (lane/op/pending/fact)
//! - `state`    — operation state machine (program counter)
//! - `storage`  — JSONL backend (default) behind the `Storage` trait
//! - `machine`  — effect sandwich (intent → effect → settlement)
//! - `tool`     — Tool trait with risk tiers
//! - `approval` — Approver trait (Allowlist / Interactive impls live in hosts)
//! - `provider` — LLM provider abstraction
//! - `mock`     — scripted provider for deterministic Tier A tests
//! - `turn`     — the single-threaded turn loop
//! - `memory`   — harness-level uteke memory loop (recall/remember)
//! - `transcript` — read-side view of turn and usage-row conventions

pub mod advisor;
#[cfg(feature = "shell-tools")]
pub mod agents;
pub mod approval;
pub mod cancel;
pub mod ccimport;
pub mod child_env;
pub mod compaction;
pub mod integrations;
// `integrations/` holds these modules; the re-exports keep the old top-level paths working.
#[doc(inline)]
#[cfg(feature = "shell-tools")]
pub use integrations::cora_search;
#[doc(inline)]
#[cfg(feature = "shell-tools")]
pub use integrations::gh;
#[doc(inline)]
#[cfg(feature = "shell-tools")]
pub use integrations::git;
#[doc(inline)]
pub use integrations::gitea;
#[doc(inline)]
#[cfg(feature = "shell-tools")]
pub use integrations::uteke;
#[doc(inline)]
pub use integrations::verify_package;
pub mod entry;
pub mod failover;
pub mod tools;
// `tools/` holds these modules; the re-exports keep the old top-level paths working.
#[doc(inline)]
pub use tools::file_tools;
#[doc(inline)]
pub use tools::read_file;
#[doc(inline)]
pub use tools::todo;
// Tool-call authorization gate (#303): crate-internal, no public API.
pub mod delegation;
// `delegation/` holds the approvals, report and resource-guard modules; these
// re-exports keep the old top-level paths working.
#[doc(inline)]
pub use delegation::approvals as delegation_approvals;
#[doc(inline)]
pub use delegation::report as delegation_report;
#[doc(inline)]
pub use delegation::resource_guard;
pub mod discussion;
// The discussion log lives in `discussion/log.rs`; this keeps the old
// `tole_core::discussion_log` path working.
#[doc(inline)]
pub use discussion::log as discussion_log;
pub(crate) mod gate;
pub mod handoff;
pub mod image;
pub mod interrupt;
pub mod ledger;
// `ledger/` holds the modules below; these re-exports keep the old top-level paths working.
#[doc(inline)]
pub use ledger::cron;
#[doc(inline)]
pub use ledger::tz;
#[doc(inline)]
pub use ledger::webhook;
pub mod systemone;
pub mod transcript;
pub mod web;
// Process hooks spawn subprocesses (issue #110) — same gate as the
// subprocess helper they build on; mobile/embedder profiles have no
// shell and no hooks.
#[cfg(feature = "shell-tools")]
pub mod hooks;
#[cfg(feature = "shell-tools")]
pub mod jobs;
pub mod machine;
#[cfg(feature = "mcp")]
pub mod mcp;
#[cfg(feature = "mcp")]
pub mod mcp_server;
#[cfg(feature = "shell-tools")]
pub mod memory;
pub mod mock;
pub mod openai;
pub mod paths;
pub mod process_guard;
pub mod provider;
pub mod register;
pub mod replay;
pub mod router;
#[cfg(feature = "shell-tools")]
pub mod run_command;
pub(crate) mod safe_dir;
#[cfg(feature = "shell-tools")]
pub mod session_guard;
// Approval-surface text sanitizer (#327): crate-internal, no public API.
pub mod plugin_lock;
pub mod sandbox;
pub(crate) mod sanitize;
pub mod secrets;
// `secrets/` holds the secret-release and sensitive-data modules; these
// re-exports keep the old `tole_core::secret_release` and
// `tole_core::sensitive` paths working.
#[doc(inline)]
pub use secrets::release as secret_release;
#[doc(inline)]
pub use secrets::sensitive;
pub mod sibling_watch;
pub mod skills;
// `skills/` holds the approval, curator, draft and index modules; these
// re-exports keep the old top-level paths working.
#[doc(inline)]
pub use skills::approval as skill_approval;
#[doc(inline)]
pub use skills::curator as skill_curator;
#[doc(inline)]
pub use skills::draft as skill_draft;
#[doc(inline)]
pub use skills::index as skill_index;
pub mod state;
pub mod storage;
#[cfg(feature = "shell-tools")]
pub mod subprocess;
pub mod tool;
pub mod turn;
pub mod update_check;

/// Semantic version of the durable schema (see storage module docs).
pub const STORAGE_VERSION: u32 = 1;
