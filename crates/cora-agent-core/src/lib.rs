//! cora-agent-core — durable agent harness core.
//!
//! Platform-agnostic library. No stdin/stdout, no CLI assumptions.
//! Designed to be embeddable via CLI, desktop app, or flutter_rust_bridge FFI.
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

pub mod approval;
pub mod cora_search;
pub mod entry;
pub mod machine;
pub mod mock;
pub mod openai;
pub mod provider;
pub mod register;
pub mod state;
pub mod storage;
pub mod tool;
pub mod turn;

/// Semantic version of the durable schema (see storage module docs).
pub const STORAGE_VERSION: u32 = 1;
