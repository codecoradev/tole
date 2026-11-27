//! Settling a turn a crash cut off, with no model call and no tool run (#1230).
use super::{append_turn_error, settle_final, transcript, REPLAY_UNKNOWN};
use crate::machine::{finish, resume, settle_err, Resume};
use crate::state::{Pc, StateTransition};
use crate::storage::{Commit, Storage, StorageError};

/// Close the turn a process death left mid-flight, appending only: an open
/// intent is settled as failed (never re-run, whatever its replay contract),
/// the register goes back to Planning, then `error: interrupted` and
/// Planning -> Final, the shape of a cancelled or detached turn. The next
/// prompt then runs as on any settled session, and the model sees the
/// unanswered user message and the UNKNOWN outcome of the cut-off call.
///
/// `Ok(false)` and no write for a session that needs no settling: Idle, Final,
/// or parked after a denial (#476: the next prompt settles that one).
pub fn settle_interrupted(s: &mut dyn Storage) -> Result<bool, StorageError> {
    let pc = s.state().pc;
    let parked = pc == Pc::Planning && transcript::parked_after_denial(s.entries());
    if matches!(pc, Pc::Idle | Pc::Final) || parked {
        return Ok(false);
    }
    match resume(s)? {
        Resume::Clean if pc == Pc::Settling => finish(s)?,
        Resume::ReExecute { intent_id, .. } | Resume::Fail { intent_id, .. } => {
            let why = format!("interrupted: the process stopped before this call settled; it was not re-run{REPLAY_UNKNOWN}");
            settle_err(s, &crate::machine::EffectHandle { intent_id }, &why)?;
        }
        Resume::Clean => {}
    }
    // ToolCall, AwaitingApproval and Executing without an intent: nothing
    // durable to settle, replan is the legal exit (as `resume_turn` takes).
    let seq = s.state().seq;
    if s.state().pc != Pc::Planning {
        s.commit(Commit::new().transition(StateTransition::from(seq, Pc::Planning)))?;
    }
    append_turn_error(s, "interrupted", "the process stopped while the turn was in flight; it was settled without calling the model")?;
    settle_final(s)?;
    Ok(true)
}
