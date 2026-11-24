//! The Automations page (issue #1033, board S6): the table of automations with
//! their switch, and the notices under it. Everything that decides is in the
//! model (`tole_desktop_model::automations`): the words, what a list that was
//! not read says, when a count may be shown, and the order Tab visits the
//! controls in. These files only draw it, and report a press to the host as the
//! id of the row or switch that was pressed.
//!
//! - `types`: what the page asks of its host, and its own small state.
//! - `cells`: the cell, the small text and the tone ink the rows and the page share.
//! - `switch`: the on/off switch of a row.
//! - `notices`: what stands where the table would be, and the warnings under it.
//! - `row`: one row of the table.
//! - `table`: the column titles and the rows.
//! - `page`: the page: title, top bar, table or stand-in, notices, scheduler note.

pub mod cells;
pub mod notices;
pub mod page;
pub mod row;
pub mod switch;
pub mod table;
pub mod types;
