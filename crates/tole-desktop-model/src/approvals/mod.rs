//! The approval card and the Approvals page (issue #1032, boards S4, S5, S11).
//!
//! Approvals are Tole's safety model, so the model fails closed: it never
//! shows a request softer than the server said, never offers a permanent
//! allow, and never offers "Allow once" after the wait ran out. Plain data and
//! reducers, tested without a window.
//!
//! - [`ApprovalRequest`], [`ActionSummary`]: the model's own mirror of the
//!   serve queue row and of the shared action summary (#1041). This crate has
//!   no tole dependency, so the adapter copies the fields from the typed
//!   client; `kind`, `effect`, `status` and the risk tier stay the server's
//!   codes, and an unknown code reads as the most careful value.
//! - [`ApprovalCard`]: what one request shows, in the words of [`strings`].
//! - [`Standing`], [`PageStop`]: what the page says about its list, and the
//!   order Tab visits its controls in.
//! - `ApprovalsState`, `ApprovalsAction`: the Waiting and Decided lists,
//!   the countdown and expiry, the sidebar count and the composer rule.
//!
//! There is no "always allow": [`Decision`] has two values and no card a third
//! button, whatever the risk tier (a Destructive request is never allowlistable).

mod card;
mod kind;
mod page;
mod queue;
mod request;
mod summary;
mod wait;

pub mod strings;

pub use card::{ApprovalCard, Button, CardState, DetailRow, BUTTONS};
pub use kind::{effect_care, risk_care, Care, Wording};
pub use page::{page_stops, PageStop, Standing};
pub use queue::{
    AnswerError, ApprovalsAction, ApprovalsState, ComposerState, FetchId, Row, Scope, Tab,
    FETCH_TIMEOUT_SECS, MAX_DECIDED, MAX_TOMBSTONES, MAX_WAITING,
};
pub use request::{ApprovalRequest, Decision, DecisionIntent, Outcome};
pub use summary::{ActionSummary, LineKind, Preview, PreviewLine, Shape};
pub use wait::{Wait, EXPIRY_SECS};

#[cfg(test)]
mod card_tests;
#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod queue_fetch_tests;
#[cfg(test)]
mod queue_guard_tests;
#[cfg(test)]
mod queue_recovery_tests;
#[cfg(test)]
mod queue_review_tests;
#[cfg(test)]
mod queue_tests;
