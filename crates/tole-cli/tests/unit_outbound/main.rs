//! Unit-style tests of the outbound webhook turn path and the built-in web tools.
//! Moved out of `src/` (#1249): these tests only use the public API of the crate.

mod turn_code_kind;
#[cfg(feature = "web-builtin")]
mod web_builtin;
mod webhook_out_turn;
