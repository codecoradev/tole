//! The engine without threads: a test feeds inputs and replies by hand and
//! reads the calls the engine asked for, with a clock it moves itself.

use std::sync::Arc;

use tole_client::{Error, StreamItem};
use tole_desktop_model::approvals::Decision;
use tole_desktop_model::task::{Phase, SendMode};

use super::calls::{Call, Gate, Lane, Reply};
use super::engine::{Effect, Engine, Input, Now};
use super::fixtures::{entry_item, list, S};
use super::store::Store;
use super::view::TaskView;
use super::Config;

pub(super) const T0: u64 = 1_000_000;

pub(super) struct Sim {
    pub engine: Engine,
    pub store: Arc<Store>,
    pub now: Now,
    pending: Vec<(Lane, Call)>,
    pub opened: Vec<u64>,
    pub fatal: bool,
    pub changes: usize,
    /// The generation of the latest prompt call taken.
    pub generation: u64,
    /// Lanes the engine asked to have replaced.
    pub replaced: Vec<Lane>,
}

impl Sim {
    /// A driver for session `s1`; its first step has been taken.
    pub(super) fn new(fresh: bool) -> Sim {
        let mut cfg = Config::new(S);
        cfg.fresh = fresh;
        let store = Arc::new(Store::new(false));
        let now = Now { wall: T0, mono: 0 };
        let mut sim = Sim {
            engine: Engine::new(Arc::clone(&store), &cfg),
            store,
            now,
            pending: Vec::new(),
            opened: Vec::new(),
            fatal: false,
            changes: 0,
            generation: 0,
            replaced: Vec::new(),
        };
        let out = sim.engine.start(now);
        sim.absorb(out);
        sim
    }

    /// A task whose history is in (empty), stream is up and queue was read.
    pub(super) fn live() -> Sim {
        let mut sim = Sim::new(true);
        sim.up(false);
        sim.answer_approvals(vec![]);
        sim
    }

    fn absorb(&mut self, out: super::engine::Out) {
        self.fatal |= out.fatal;
        self.changes += usize::from(out.changed);
        for effect in out.effects {
            match effect {
                Effect::Call(lane, call) => self.pending.push((lane, call)),
                Effect::OpenStream(after) => self.opened.push(after),
                Effect::ReplaceLane(lane) => self.replaced.push(lane),
            }
        }
    }

    pub(super) fn feed(&mut self, input: Input) {
        let out = self.engine.step(input, self.now);
        self.absorb(out);
    }

    /// Move both clocks and tick.
    pub(super) fn advance(&mut self, ms: u64) {
        self.now.wall += ms;
        self.now.mono += ms;
        self.feed(Input::Tick);
    }

    /// Move only the monotonic clock (the wall clock jumps ahead of it with
    /// `suspend`).
    pub(super) fn suspend(&mut self, ms: u64) {
        self.now.wall += ms;
        self.feed(Input::Tick);
    }

    pub(super) fn has(&self, lane: Lane) -> bool {
        self.pending.iter().any(|(l, _)| *l == lane)
    }

    /// The call the engine asked for on `lane`.
    pub(super) fn take(&mut self, lane: Lane) -> Call {
        let at = self.pending.iter().position(|(l, _)| *l == lane);
        let at =
            at.unwrap_or_else(|| panic!("no call on {lane:?}; pending {}", self.pending.len()));
        self.pending.remove(at).1
    }

    pub(super) fn none_pending(&self) -> bool {
        self.pending.is_empty()
    }

    pub(super) fn answer(&mut self, reply: Reply) {
        self.feed(Input::Reply(reply));
    }

    pub(super) fn stream(&mut self, item: StreamItem) {
        self.feed(Input::Stream(Ok(Some(item))));
    }

    pub(super) fn stream_error(&mut self, e: Error) {
        self.feed(Input::Stream(Err(e)));
    }

    pub(super) fn up(&mut self, reopened: bool) {
        self.feed(Input::Up { reopened });
    }

    pub(super) fn view(&self) -> TaskView {
        self.store.view(true)
    }

    pub(super) fn phase(&self) -> Phase {
        self.view().status.phase
    }

    pub(super) fn send_mode(&self) -> SendMode {
        self.view().status.send
    }

    pub(super) fn status_answer(&mut self, busy: bool) {
        match self.take(Lane::Status) {
            Call::Status { token, check } => self.answer(Reply::Status {
                token,
                check,
                reply: Ok(super::fixtures::status(busy)),
            }),
            _ => panic!("not a status call"),
        }
    }

    /// Answer the queue read the engine asked for.
    pub(super) fn answer_approvals(&mut self, rows: Vec<tole_client::Approval>) {
        match self.take(Lane::Read) {
            Call::Approvals { token } => self.answer(Reply::Approvals(token, Ok(list(rows)))),
            _ => panic!("not an approvals call"),
        }
    }

    pub(super) fn entry(&mut self, e: &tole_client::Entry) {
        self.stream(entry_item(e));
    }

    /// Send a prompt and take the call, started (the lane thread is in it).
    pub(super) fn prompt(&mut self, text: &str) -> String {
        self.feed(Input::Send(text.into()));
        match self.take(Lane::Prompt) {
            Call::Prompt {
                req,
                gate,
                generation,
            } => {
                assert!(gate.start(), "the call starts");
                self.generation = generation;
                req.text
            }
            _ => panic!("not a prompt call"),
        }
    }

    /// Send a prompt and take the call, not started.
    pub(super) fn prompt_unstarted(&mut self, text: &str) -> Gate {
        self.feed(Input::Send(text.into()));
        match self.take(Lane::Prompt) {
            Call::Prompt {
                gate, generation, ..
            } => {
                self.generation = generation;
                gate
            }
            _ => panic!("not a prompt call"),
        }
    }

    pub(super) fn answer_prompt(&mut self, r: Result<tole_client::PromptReply, Error>) {
        self.answer(Reply::Prompt(self.generation, Some(r)));
    }

    pub(super) fn decide(&mut self, id: &str, d: Decision) {
        self.feed(Input::Decide(id.into(), d));
    }
}
