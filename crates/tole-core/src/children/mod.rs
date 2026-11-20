//! Child-agent machinery: spawning and supervising `agents`, their `child_env`,
//! the `handoff` contract, `sibling_watch` and the harness `memory` loop.
//! Engines level: the shell-tools-gated modules keep their cfg gates.

#[cfg(feature = "shell-tools")]
pub mod agents;
pub mod child_env;
pub mod handoff;
#[cfg(feature = "shell-tools")]
pub mod memory;
pub mod sibling_watch;
