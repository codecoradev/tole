//! #176 ACP session config options — the model picker, end-to-end with
//! the REAL `tole acp` binary: advertisement on session/new, durable
//! override via session/set_config_option (register write, restored on
//! session/load in a fresh process), and the switched model actually
//! reaching the provider request body.

// These tests spawn the real `tole acp`/`tole serve` binary, which only exists
// with the `shell-tools` feature (#349): the bare profile has no such face.
#![cfg(feature = "shell-tools")]

#[path = "../common/mod.rs"]
mod common;
mod harness;
mod mock;
mod picker;
mod probe;
mod sse;
