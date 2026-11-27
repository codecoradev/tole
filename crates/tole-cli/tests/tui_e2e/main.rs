//! #460 acceptance: the real `tole` binary on a pty (bare `tole` = the TUI)
//! against a scripted mock provider: stream an answer, approve a write,
//! cancel a running turn, multi-line input, then the session is an ordinary
//! one (`tole sessions` lists it).
#![cfg(all(unix, feature = "tui"))]

#[path = "../common/mod.rs"]
mod common;
mod flow;
mod fork_export;
mod mock;
mod paste_image;
mod pty;
mod theme_keys;
