//! The adapter from the typed client to the model (#1040 to #1028, part of
//! #527): pure functions that turn `tole-client` values into the plain-data
//! mirror types of `tole-desktop-model`, and the model's intents back into
//! client requests.
//!
//! It is not the [`ui`](crate::ui) module and names no GUI toolkit. It does no
//! I/O: no HTTP call, no thread, no clock (a function that needs the time
//! takes `now_ms`), so a page test builds a client value from JSON and asserts
//! the model value. Whoever owns the connection (the session worker) calls the
//! client and feeds the results through these functions.
//!
//! Rules every function here follows:
//!
//! - Text passes through raw. The model makes it safe to draw and bounds it
//!   (the approvals queue on the way in, `printable` in the skills page), so a
//!   second, different cleaning here would only hide what the server sent.
//! - Nothing is invented. A field the client does not carry stays `None`; a
//!   code this version does not know is passed on as the server's text, and
//!   the model reads it as its most careful value.
//! - Server data never panics: no `unwrap`, no indexing, no slicing of a server
//!   string, and counters saturate.
//!
//! One module per capability: [`approvals`] and [`skills`] (with the tools
//! page); `wire` holds what both read the same way.

pub mod approvals;
pub mod skills;
mod wire;
