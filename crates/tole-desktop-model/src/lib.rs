//! The model of the tole desktop (#1028, part of #527): view state, actions,
//! reducers, design tokens and the string table.
//!
//! This crate has no dependencies today, and may never gain a GUI toolkit or
//! anything from tole. It cannot name them, because the compiler has nothing
//! to resolve them to; `scripts/desktop-check.sh deps` checks the resolved
//! graph so it stays so.
//! The `tole-desktop` crate renders it.
//!
//! - [`Section`], [`ShellState`], [`Action`]: the app shell and its reducer,
//!   tested without a window.
//! - [`tokens`]: colours, sizes and fonts (one semantic module).
//! - [`strings`]: every word the window shows.

mod section;
mod shell;
pub mod strings;
pub mod tokens;

pub use section::Section;
pub use shell::{Action, Connection, ConnectionStatus, Folder, FolderId, ShellState, Target};
