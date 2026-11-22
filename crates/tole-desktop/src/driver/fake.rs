//! A scripted [`Api`] for driver tests: no socket, no serve.
//!
//! Replies are queued in the order the driver will ask for them. A reply is
//! either ready (`reply_*`) or held back by a gate (`gate_*`): the call blocks
//! until the test sends its result, which is how a test makes a reply arrive
//! late, after a loss or a newer fetch. A call with nothing queued fails as a
//! refused connection, so a test that forgot to script it sees a failure, not a
//! hang. The test waits for calls with `wait_*`, which are bounded in real time
//! only so that a broken driver fails the test instead of hanging it.
//!
//! Every stream a test wants the driver to open is queued with
//! [`FakeApi::open_stream`], which returns the sender of its frames. Dropping
//! the sender ends the stream cleanly.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use tole_client::{Approval, ApprovalList, Decision, DecisionReply, Error, Result, StreamItem};

use super::api::{Api, EventSource};

const PATIENCE: Duration = Duration::from_secs(10);

enum Scripted<T> {
    Now(Result<T>),
    Gate(Receiver<Result<T>>),
    /// The call panics.
    Panic,
}

impl<T> Scripted<T> {
    fn settle(self) -> Result<T> {
        match self {
            Scripted::Now(r) => r,
            Scripted::Gate(rx) => rx.recv().unwrap_or_else(|_| Err(refused())),
            Scripted::Panic => panic!("a scripted panic in an Api call"),
        }
    }
}

/// What a scripted stream yields next.
pub(crate) enum Frame {
    Item(StreamItem),
    Fail(Error),
    /// The source panics.
    Panic,
}

struct FakeSource(Receiver<Frame>);

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

#[derive(Default)]
struct Script {
    lists: VecDeque<Scripted<ApprovalList>>,
    decisions: VecDeque<Scripted<DecisionReply>>,
    streams: HashMap<String, VecDeque<Receiver<Frame>>>,
    /// Senders of streams nobody scripted: they stay silent, open.
    parked: Vec<Sender<Frame>>,
    list_calls: Vec<Option<String>>,
    decision_calls: Vec<(String, Decision, Option<String>)>,
    opens: HashMap<String, usize>,
}

#[derive(Default)]
pub(crate) struct FakeApi {
    script: Mutex<Script>,
    changed: Condvar,
}

impl FakeApi {
    pub(crate) fn new() -> Arc<Self> {
        Arc::default()
    }

    fn script(&self) -> MutexGuard<'_, Script> {
        self.script.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn reply_list(&self, r: Result<ApprovalList>) {
        self.script().lists.push_back(Scripted::Now(r));
    }

    /// The next `GET /approvals` blocks until the returned sender is used.
    pub(crate) fn gate_list(&self) -> Sender<Result<ApprovalList>> {
        let (tx, rx) = channel();
        self.script().lists.push_back(Scripted::Gate(rx));
        tx
    }

    /// The next `GET /approvals` panics.
    pub(crate) fn panic_list(&self) {
        self.script().lists.push_back(Scripted::Panic);
    }

    /// The next decision call panics.
    pub(crate) fn panic_decision(&self) {
        self.script().decisions.push_back(Scripted::Panic);
    }

    pub(crate) fn reply_decision(&self, r: Result<DecisionReply>) {
        self.script().decisions.push_back(Scripted::Now(r));
    }

    pub(crate) fn gate_decision(&self) -> Sender<Result<DecisionReply>> {
        let (tx, rx) = channel();
        self.script().decisions.push_back(Scripted::Gate(rx));
        tx
    }

    /// The next stream the driver opens for `session`; its frames come from
    /// the returned sender.
    pub(crate) fn open_stream(&self, session: &str) -> Sender<Frame> {
        let (tx, rx) = channel();
        self.script()
            .streams
            .entry(session.to_owned())
            .or_default()
            .push_back(rx);
        tx
    }

    pub(crate) fn list_calls(&self) -> usize {
        self.script().list_calls.len()
    }

    /// `(id, verdict, fingerprint)` of every decision call so far.
    pub(crate) fn decisions(&self) -> Vec<(String, Decision, Option<String>)> {
        self.script().decision_calls.clone()
    }

    pub(crate) fn opens(&self, session: &str) -> usize {
        self.script().opens.get(session).copied().unwrap_or(0)
    }

    /// Wait until `count(script)` reaches `n`; panics after [`PATIENCE`].
    fn wait(&self, what: &str, n: usize, count: impl Fn(&Script) -> usize) {
        let guard = self.script();
        let (guard, out) = self
            .changed
            .wait_timeout_while(guard, PATIENCE, |s| count(s) < n)
            .unwrap_or_else(|p| p.into_inner());
        assert!(
            !out.timed_out(),
            "timed out waiting for {n} {what} (saw {})",
            count(&guard)
        );
    }

    pub(crate) fn wait_list_calls(&self, n: usize) {
        self.wait("GET /approvals calls", n, |s| s.list_calls.len());
    }

    pub(crate) fn wait_decisions(&self, n: usize) {
        self.wait("decision calls", n, |s| s.decision_calls.len());
    }

    pub(crate) fn wait_opens(&self, session: &str, n: usize) {
        let session = session.to_owned();
        self.wait("stream opens", n, move |s| {
            s.opens.get(&session).copied().unwrap_or(0)
        });
    }
}

impl Api for FakeApi {
    fn approvals(&self, session: Option<&str>) -> Result<ApprovalList> {
        let next = {
            let mut s = self.script();
            s.list_calls.push(session.map(str::to_owned));
            self.changed.notify_all();
            s.lists.pop_front()
        };
        next.map_or_else(|| Err(refused()), Scripted::settle)
    }

    fn decide_row(&self, row: &Approval, decision: Decision) -> Result<DecisionReply> {
        let next = {
            let mut s = self.script();
            let fingerprint = row
                .extra
                .get("fingerprint")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            s.decision_calls
                .push((row.id.clone(), decision, fingerprint));
            self.changed.notify_all();
            s.decisions.pop_front()
        };
        next.map_or_else(|| Err(refused()), Scripted::settle)
    }

    fn follow_events(&self, session: &str, _after: u64) -> Box<dyn EventSource> {
        let mut s = self.script();
        *s.opens.entry(session.to_owned()).or_default() += 1;
        self.changed.notify_all();
        let rx = s
            .streams
            .get_mut(session)
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(|| {
                let (tx, rx) = channel();
                s.parked.push(tx);
                rx
            });
        Box::new(FakeSource(rx))
    }
}
