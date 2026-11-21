//! The supervisor of the local `tole serve` (issue #1029, owner decisions D1 to
//! D4 in `docs/desktop-design.md` section 0).
//!
//! The desktop is a thin client of `tole serve`, also on the same machine. On
//! launch it attaches to a serve that already listens on its private socket and
//! speaks the protocol it knows, or starts one with a per-launch credential
//! handed over on an inherited descriptor. This module has no window and no
//! toolkit; the window renders the [`tole_desktop_model::ServeState`] it
//! reports.
//!
//! macOS and Linux (a Unix socket). The Windows named pipe is a follow-up
//! (#1051).
//!
//! Building blocks, each tested alone:
//!
//! - [`config`]: the settings, the restart backoff, the runtime directory.
//! - [`binary`]: which `tole` binary to start.
//! - [`token`]: the per-launch credential and its private file.
//! - [`probe`]: is a serve on the socket, does its protocol match, does it
//!   accept the credential.
//! - [`process`]: starting the serve as a child.
//! - [`sys`]: the few system calls (flock, signals, directory checks).

pub mod binary;
pub mod config;
pub mod probe;
pub mod process;
pub mod sys;
pub mod token;

pub use config::{default_runtime_dir, Backoff, CloseBehavior, Config};
