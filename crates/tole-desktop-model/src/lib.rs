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
//! - [`Connections`], [`Endpoint`]: the servers the desktop can talk to and the
//!   rule for plain `http://` (owner decision D6); a remote entry holds a
//!   reference to its credential, never the text.
//! - [`ServeState`]: what the supervisor of the local serve reports.
//! - [`tokens`]: colours, sizes and fonts (one semantic module).
//! - [`strings`]: every word the window shows.

mod connections;
mod credential;
mod endpoint;
mod section;
mod serve_state;
mod shell;
pub mod strings;
pub mod tokens;

pub use connections::{
    ConnectionId, Connections, ConnectionsError, RemoteDraft, RemoteEntry, RemoteId, SavedRemote,
    MAX_NAME_CHARS, MAX_REMOTES,
};
pub use credential::{CredentialRef, CredentialStore, Secret, StoreError};
pub use endpoint::{Endpoint, EndpointError, PlainHttp, Scheme};
pub use section::Section;
pub use serve_state::{OwnerAction, ServeFailure, ServeState};
pub use shell::{Action, Connection, ConnectionStatus, Folder, FolderId, ShellState, Target};
