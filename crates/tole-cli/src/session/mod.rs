//! Session host side: the `assemble` step that composes a new session's system
//! prompt, the `host` that opens and drives sessions, its `index`
//! and `tools`, the `runner` for one turn, `compaction`, `status` and `usage`
//! reports, and the interactive `tui` host with its `export` (both behind the
//! `tui` feature), and the read-only `size` report of a sessions directory.

pub mod assemble;
pub mod compaction;
#[cfg(feature = "tui")]
pub mod export;
pub mod host;
pub mod index;
pub mod interrupted;
pub mod owner_mcp;
pub mod problem;
pub mod runner;
pub mod size;
pub mod status;
pub mod tools;
#[cfg(feature = "tui")]
pub mod tui;
pub mod usage;
