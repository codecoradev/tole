//! #176 PR 2 — ACP approval controls, end-to-end with the REAL `tole
//! acp` binary: the `approval` config option (ask/auto), the
//! `allow_always` permission option (remembered per session), and the
//! Destructive never-remembered rule.

// These tests spawn the real `tole acp`/`tole serve` binary, which only exists
// with the `shell-tools` feature (#349): the bare profile has no such face.
#![cfg(feature = "shell-tools")]

mod approval;
#[path = "../common/mod.rs"]
mod common;
mod harness;
mod mock;
mod progress;
mod sse;
