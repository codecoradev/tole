//! The Approvals page and the approval card (issue #1032, boards S4 and S5).
//!
//! Everything that decides is in the model (`tole_desktop_model::approvals`):
//! the words, the care level, which buttons exist and when. These files only
//! draw it, and send the owner's press to the host as an id and a [`Decision`]
//! of the card that was pressed, never of "whatever is selected". There is no
//! "always allow" anywhere: the card has the two buttons of the model.
//!
//! - `card`: the card of one request, a reusable component: the Approvals
//!   page draws it in its detail pane and the task view can draw it in the
//!   conversation. It is generic over the view that hosts it (see
//!   `CardHost`).
//! - `page`: the page itself, a pure function of the driver's view
//!   (`crate::driver::approvals::ApprovalsView`) and a small state of its own.
//! - `focus`: the Tab stops of the page and of a card, in the order the model
//!   gives ([`tole_desktop_model::approvals::page_stops`]).
//!
//! [`Decision`]: tole_desktop_model::approvals::Decision

#[cfg(feature = "gpui")]
mod card;
#[cfg(feature = "gpui")]
mod focus;
#[cfg(all(feature = "gpui", unix))]
mod page;
#[cfg(feature = "gpui")]
mod parts;
#[cfg(feature = "gpui")]
mod preview;

#[cfg(feature = "gpui")]
pub use self::card::{approval_card, CardHost};
#[cfg(feature = "gpui")]
pub use self::focus::StopFocus;
#[cfg(all(feature = "gpui", unix))]
pub use self::page::{approvals_page, standing, PageHost, PageUi};
#[cfg(feature = "render-harness")]
pub mod scenes;
#[cfg(all(feature = "render-harness", unix))]
pub mod scenes_page;
