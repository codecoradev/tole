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

pub mod approval;
#[cfg(feature = "shell-tools")]
pub mod cora_search;
pub mod entry;
pub mod file_tools;
#[cfg(feature = "shell-tools")]
pub mod gh;
#[cfg(feature = "shell-tools")]
pub mod git;
pub mod machine;
pub mod mock;
pub mod openai;
pub mod provider;
pub mod read_file;
pub mod register;
#[cfg(feature = "shell-tools")]
pub mod run_command;
pub mod state;
pub mod storage;
#[cfg(feature = "shell-tools")]
pub mod subprocess;
pub mod tool;
pub mod turn;
#[cfg(feature = "shell-tools")]
pub mod uteke;

/// Semantic version of the durable schema (see storage module docs).
pub const STORAGE_VERSION: u32 = 1;
