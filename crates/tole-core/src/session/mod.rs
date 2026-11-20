//! The session model: write-once `entry` tree, `register` cells, the `state`
//! machine, the `machine` effect sandwich, the JSONL `storage` backend and
//! its `safe_dir` guard. Kernel level: nothing here depends on an engine.

pub mod entry;
pub mod machine;
pub mod register;
pub(crate) mod safe_dir;
pub mod state;
pub mod storage;
