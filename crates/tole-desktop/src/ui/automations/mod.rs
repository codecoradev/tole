//! The Automations page (issue #1033, board S6): the table of automations with
//! their switch, and the notices under it. Everything that decides is in the
//! model (`tole_desktop_model::automations`): the words, what a list that was
//! not read says, when a count may be shown, and the order Tab visits the
//! controls in. These files only draw it, and report a press to the host as the
//! id of the row or switch that was pressed.
//!
//! - `page`: the page: title, the line of the top bar, the table or the line
//!   that stands in its place, the notices and the note about the scheduler.
//! - `table`: one row of the table and its on/off switch.

#[cfg(all(feature = "gpui", unix))]
mod page;
#[cfg(all(feature = "gpui", unix))]
mod table;

#[cfg(all(feature = "gpui", unix))]
pub use self::page::{automations_page, standing, AutomationsHost, AutomationsUi};
