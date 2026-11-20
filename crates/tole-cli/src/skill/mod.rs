//! Skill host side: `approve` (owner approval), `drafts` (draft commands),
//! `recall`, `propose` and `outcome` for the skill tools, and the `curator`
//! command.

pub mod approve;
pub mod curator;
pub mod drafts;
#[cfg(feature = "shell-tools")]
pub mod fetch;
pub mod outcome;
pub mod propose;
pub mod recall;
