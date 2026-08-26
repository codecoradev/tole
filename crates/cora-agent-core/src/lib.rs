//! cora-agent-core — durable agent harness core.
//!
//! Platform-agnostic library. No stdin/stdout, no CLI assumptions.
//! Designed to be embeddable via CLI, desktop app, or flutter_rust_bridge FFI.
//!
//! Layout (v0 scope):
//! - `entry`    — write-once conversation tree entries
//! - `register` — namespaced mutable cells (lane/op/pending/fact)
//! - `state`    — operation state machine (program counter)
//! - `storage`  — SQLite backend, one file per session
//! - `tool`     — Tool trait with risk tiers
//! - `approval` — Approver trait (Allowlist / Interactive impls live in hosts)
//! - `provider` — LLM provider abstraction

pub mod entry;
pub mod register;
pub mod state;
pub mod storage;
pub mod tool;
pub mod approval;

/// Semantic version of the durable schema (migrate-on-open, see spec.md Part 7).
pub const STORAGE_VERSION: u32 = 1;
