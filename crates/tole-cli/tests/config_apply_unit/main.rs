//! Unit tests of `config::apply` (precedence matrix, startup wiring, the
//! per-key settings), moved out of `src/` as integration tests (#1249).

#[path = "../common/mod.rs"]
mod common;
mod gate_effort;
mod precedence;
mod provider;
mod security;
mod startup;
mod support;
mod theme_keys;
mod timeouts;
