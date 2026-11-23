//! The scripting of the call that starts a task: `POST /sessions`. The trait
//! method is in the parent.

use std::sync::mpsc::{channel, Sender};

use tole_client::{Result, SessionOpened};

use super::{FakeApi, Scripted};

// Part of the scripting surface every driver's tests share.
#[allow(dead_code)]
impl FakeApi {
    scripted!(
        creates,
        SessionOpened,
        reply_create,
        gate_create,
        panic_create
    );

    /// The working folder asked for by every `POST /sessions` call.
    pub(crate) fn create_calls(&self) -> Vec<Option<String>> {
        self.script().create_calls.clone()
    }

    pub(crate) fn wait_creates(&self, n: usize) {
        self.wait("POST /sessions calls", n, |s| s.create_calls.len());
    }
}
