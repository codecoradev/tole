//! The automations driver: keeps the model's `AutomationsState` (the
//! Automations page, board S6, #1033) true to a serve, and carries the owner's
//! switch.
//!
//! It connects the adapter's pure functions (`adapter::automations`) to the
//! blocking client. It loads `GET /cron/jobs` when it starts, again on a slow
//! timer ([`Config::refresh`], 30 s by default) and whenever the owner asks
//! ([`Handle::refresh`]); it loads `GET /cron/runs` of the automation the owner
//! opened ([`Handle::select`]) the same way; it pauses or resumes a job once per
//! press ([`Handle::set_enabled`]); it retries a failed read with a backoff for
//! ever, and hands the UI a plain-data [`AutomationsView`]. The rules are in
//! `engine` (`Engine::step`), which has no thread and no I/O and is what the
//! tests drive.
//!
//! The scheduler is not here, and never in the UI: the jobs run in the serve
//! (`tole serve --cron`) or in a `tole cron tick`; this driver reads what they
//! did and flips the same switch `tole cron pause` and `resume` write (the
//! serve routes write the job state `tole cron` reads, so a switch used here
//! is the state a terminal sees, and the other way round at the next read).
//!
//! # Thread model
//!
//! Five threads, all started by [`AutomationsDriver::start`], all stopped by
//! [`AutomationsDriver::shutdown`]:
//!
//! | thread | does | blocks on |
//! |---|---|---|
//! | `worker` (1) | reads one bounded inbox, runs `Engine::step`, hands the effects to the I/O threads, tells the UI | the inbox |
//! | `jobs loader` (1) | `GET /cron/jobs`, one call at a time | the call |
//! | `runs loader` (1) | `GET /cron/runs`, and `GET /approvals` when a run stopped to ask, one at a time | the call |
//! | `toggler` (1) | `POST .../pause` or `.../resume`, one at a time, each once | the call |
//! | `ticker` (1) | wakes the worker once a second | the clock |
//!
//! Only the worker changes the state, and it does so one input at a time. The
//! state sits behind a mutex that is held for the apply and nothing else (never
//! across a call or a send), and the UI's callback runs after it is released,
//! so [`Handle::snapshot`] from the UI thread waits at most that long. A call
//! that blocks blocks its own thread, not the worker nor the other routes. The
//! inbox is bounded (`INBOX`): an I/O thread waits when it is full (an answer is
//! never dropped), the ticker drops its message, and the UI gets [`Busy`]. A
//! call is bounded by the client (see [`ClientApi`](super::api::ClientApi): 30 s
//! per read and per body, so about a minute at worst).
//!
//! # Panics
//!
//! A panic inside an `Api` call is caught where the call is made and is that
//! call's failure (a read is marked failed and retried, a switch shows "could
//! not reach Tole" and the list is read again); the thread goes on. A panic
//! anywhere else in a thread is reported to the worker, which ends the driver:
//! it stops the other threads and clears the state, so the view reads not loaded
//! (no rows) and [`Handle::is_running`] is false.
//!
//! # Stopping
//!
//! `shutdown` raises one stop signal (it wakes every sleep) and joins the
//! threads for up to a timeout. A thread inside a call is not interrupted (the
//! client has no cancel): it is reported as `detached`, ends when the call
//! returns, and sends nothing the worker will read. When the worker ends it
//! clears the state, so a view read afterwards is never a stale list.
//!
//! # When the list is known
//!
//! The rows are the serve's list only after a call succeeded. They stay, marked
//! failed (`page.load` is `Failed`, and the scheduler line says it is not
//! running), when:
//!
//! - the next call fails, whatever the reason (404, no connection, a panic);
//! - the rows are older than twice the refresh interval (calls that hang);
//! - the machine slept, or the wall clock was moved ahead of the monotonic one
//!   by more than a few seconds: the rows may be hours old, and the call that
//!   was out is ignored.
//!
//! [`AutomationsView::counts`] is a number only while the rows are known and
//! are all of the jobs ([`AutomationsView::left_out`] is zero). The history of
//! the selected automation is marked the same way ([`History::load`]).
//!
//! # The switch
//!
//! [`Handle::set_enabled`] makes one request, or none (see `engine`), and the
//! switch of the row changes only when the serve answers, to the state the serve
//! names. A refused switch is a notice in the view (`page.notice`), not a retry:
//! a job Tole made that the owner has not reviewed says so and names the command
//! to run in a terminal, a plan-mode serve says it pauses and does not resume.
//! While the call is out the view says which switch is busy
//! ([`AutomationsView::toggling`]). Whatever the answer, the list is read again
//! at once, and a list call that was out is ignored, so a stale list never puts
//! the old state back under the new one.
//!
//! # Limits
//!
//! - A job created or changed at the terminal shows up at the next refresh (30 s
//!   by default, or at once with [`Handle::refresh`]); there is no event for it.
//! - A hung call holds its route's slot until it returns (the client bounds it
//!   at about a minute), so a refresh after a loss, or the list read that follows
//!   a switch, waits for it.
//! - A plan-mode serve's refusal of a resume (409) blocks every resume until the
//!   driver is restarted (`page.resume_blocked` is the model's and is never
//!   cleared by it); a pause still works.

// The handle these docs name comes with the worker in the next change.
#![allow(rustdoc::broken_intra_doc_links)]

// The worker, the threads and the handle that drive these three are the next
// change; until then nothing outside them uses them.
#[allow(dead_code, unused_imports)]
mod engine;
#[allow(dead_code, unused_imports)]
mod store;
#[allow(dead_code, unused_imports)]
mod view;
