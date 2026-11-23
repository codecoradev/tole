//! What the scripted replies and streams of the fake are made of.

use std::collections::VecDeque;
use std::io;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use tole_client::{Error, Result, StreamItem};

use crate::driver::api::EventSource;

pub(super) const PATIENCE: Duration = Duration::from_secs(10);

pub(super) enum Scripted<T> {
    Now(Result<T>),
    Gate(Receiver<Result<T>>),
    /// The call panics.
    Panic,
}

impl<T> Scripted<T> {
    pub(super) fn settle(self) -> Result<T> {
        match self {
            Scripted::Now(r) => r,
            Scripted::Gate(rx) => rx.recv().unwrap_or_else(|_| Err(refused())),
            Scripted::Panic => panic!("a scripted panic in an Api call"),
        }
    }
}

pub(super) type Queue<T> = VecDeque<Scripted<T>>;

/// What a scripted stream yields next.
pub(crate) enum Frame {
    Item(StreamItem),
    Fail(Error),
    /// The source panics.
    Panic,
}

pub(super) struct FakeSource(pub(super) Receiver<Frame>);

impl EventSource for FakeSource {
    fn next_item(&mut self) -> Result<Option<StreamItem>> {
        match self.0.recv() {
            Ok(Frame::Item(item)) => Ok(Some(item)),
            Ok(Frame::Fail(e)) => Err(e),
            Ok(Frame::Panic) => panic!("a scripted panic in a stream"),
            Err(_) => Ok(None),
        }
    }
}

pub(crate) fn refused() -> Error {
    Error::Connect(io::Error::from(io::ErrorKind::ConnectionRefused))
}
