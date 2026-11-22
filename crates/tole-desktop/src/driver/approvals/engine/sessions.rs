//! The followed set while the driver runs: sessions come and go, readers start
//! and are retired, and the serve's stream slots are never over-asked.
//!
//! The engine decides, the worker carries out (`Out::readers`). A session the
//! app wants is `Pending` until it gets a reader, which needs a free slot: a
//! reader that was retired still holds its stream (the client cannot cancel a
//! read), so it counts until it ends (`Input::ReaderEnded`), and the number of
//! readers alive, retired ones included, never passes `MAX_STREAMS`. A session
//! also waits for the end of its own previous reader, so there is never a
//! second reader of one session whose late message could be taken for the new
//! one's (a reader's messages are in the inbox before its `ReaderEnded`).
//!
//! What the change does to knowledge. A session that gets a reader is `Down`
//! like a stream that dropped: the list is unknown until its stream is up and
//! a fetch that started after it has succeeded, so a fetch that began before
//! the new stream was up (out now, or answering late) never makes the list
//! known for the new set. A session that is removed only lowers what the list
//! must prove, so nothing is lost: a fetch that was valid for the old set is
//! valid for the smaller one (`epoch` counts every change of which streams are
//! up, and a removal changes none).

use std::collections::btree_map::Entry;

use super::{Alive, Cover, Engine, Now, Out, ReaderId, ReaderOp};
use crate::driver::approvals::{followed_sessions, MAX_STREAMS};

impl Engine {
    /// The app's wanted set replaces the followed one. The same set (in any
    /// order) changes nothing. Returns whether a view changed.
    pub(super) fn set_sessions(&mut self, sessions: Vec<String>, out: &mut Out) -> bool {
        let wanted = followed_sessions(sessions);
        let removed: Vec<String> = (self.followed.keys())
            .filter(|s| !wanted.contains(s))
            .cloned()
            .collect();
        let mut changed = false;
        let mut was_down = false;
        for session in removed {
            was_down |= self.followed.remove(&session) == Some(Cover::Down);
            for (id, reader) in &mut self.readers {
                if reader.session == session && !reader.retired {
                    reader.retired = true;
                    out.readers.push(ReaderOp::Retire(*id));
                }
            }
            changed = true;
        }
        for session in wanted {
            if let Entry::Vacant(slot) = self.followed.entry(session) {
                slot.insert(Cover::Pending);
                changed = true;
            }
        }
        // A removed session that held the list unknown no longer does.
        if was_down && self.all_up() {
            self.want = true;
        }
        if changed {
            self.publish_parked();
        }
        changed
    }

    /// A reader thread is gone and its slot is free. One that was not retired
    /// is not expected to end (a reader never gives up): its session waits for
    /// a new reader rather than staying `Down` for ever.
    pub(super) fn reader_ended(&mut self, id: ReaderId) -> bool {
        let Some(Alive { session, retired }) = self.readers.remove(&id) else {
            return false;
        };
        if retired {
            return false;
        }
        match self.followed.get_mut(&session) {
            Some(cover) => {
                *cover = Cover::Pending;
                self.epoch = self.epoch.wrapping_add(1);
                self.publish_parked()
            }
            None => false,
        }
    }

    /// Give a reader to every pending session that can have one: a free slot
    /// and no reader of its own still alive. After every input, so a freed slot
    /// is used at once.
    pub(super) fn start_readers(&mut self, now: Now, out: &mut Out) -> bool {
        let pending: Vec<String> = (self.followed.iter())
            .filter(|(_, c)| **c == Cover::Pending)
            .map(|(s, _)| s.clone())
            .collect();
        let mut started = false;
        for session in pending {
            let busy = self.readers.values().any(|r| r.session == session);
            if busy || self.readers.len() >= MAX_STREAMS {
                continue;
            }
            self.next_reader = self.next_reader.wrapping_add(1);
            let id = self.next_reader;
            self.readers.insert(
                id,
                Alive {
                    session: session.clone(),
                    retired: false,
                },
            );
            self.followed.insert(session.clone(), Cover::Down);
            out.readers.push(ReaderOp::Start(id, session));
            started = true;
        }
        if !started {
            return false;
        }
        self.epoch = self.epoch.wrapping_add(1);
        self.publish_parked() | self.lose(now, false)
    }
}
