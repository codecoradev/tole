//! Ecosystem integrations: probe-gated CLI tools (`gh`, `git`, `cora_search`,
//! `uteke`) and API-backed ones (`gitea`, `verify_package`). Each is a leaf
//! that only depends on `tool` and the subprocess helpers.

#[cfg(feature = "shell-tools")]
pub mod cora_search;
#[cfg(feature = "shell-tools")]
pub mod gh;
#[cfg(feature = "shell-tools")]
pub mod git;
pub mod gitea;
#[cfg(feature = "shell-tools")]
pub mod uteke;
pub mod verify_package;
