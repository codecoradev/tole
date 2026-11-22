//! tole-desktop: the native desktop client (#1028, part of #527).
//!
//! Two crates, so the logic is testable without a window and the compiler
//! keeps the toolkit out of it:
//!
//! - `tole-desktop-model`: view state, reducers, tokens and strings. No
//!   dependencies at all.
//! - this crate: [`ui`] renders the model. The only code that knows a GUI
//!   toolkit, and inside it only `ui/adapter.rs` names GPUI types (one place
//!   to touch on an upgrade). The toolkit is behind the `gpui` feature.
//!
//! The desktop is a client of `tole serve`. It never embeds the session host
//! and holds no agent logic (docs/desktop-design.md section 0, D2). Its one tole
//! dependency is the typed client `tole-client`.
//!
//! - [`supervisor`] (macOS and Linux): attaches to or starts the local serve.
//! - [`adapter`]: pure functions from the typed client's values to the model's
//!   mirror types (no I/O, no toolkit).

pub mod adapter;
#[cfg(unix)]
pub mod supervisor;
pub mod ui;
