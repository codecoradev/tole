//! The Skills and Connected tools pages (boards S7 and S8, issue #1034): the
//! model, without a window. Read-only: nothing here can approve a skill or
//! trust a tool (approval is a terminal command, `tole skills approve` and
//! `tole plugin approve`, never a serve route), and no type has a method that
//! asks for it.
//!
//! Input is the model's own mirror of `GET /skills` and `GET /tools` (#1042).
//! The adapter from the `tole-client` types is a later step:
//!
//! - `SkillsState.skills` (`SkillRow`) maps to [`RawSkill`], `.drafts`
//!   (`SkillDraft`) to [`RawDraft`], `.archived` (`ArchivedSkill`) to
//!   [`RawArchived`], `.notes` as they are.
//! - `ToolsState.servers` (`ToolServer`) maps to [`RawTool`]; `.notes` and
//!   `.probed` as they are.
//!
//! Not in these routes today, so not in the model: the full text of a skill or
//! draft, its author and last use, and the names of tools that changed.

mod skill;
mod state;
mod tool;

pub use skill::{
    ApprovedSkill, ArchivedSkill, BlockedReason, BlockedSkill, RawArchived, RawDraft, RawSkill,
    ReviewItem, ReviewReason, SkillsView,
};
pub use state::{Badge, SkillsPageState, Tab};
pub use tool::{RawTool, ToolChange, ToolRow, ToolTrust};
