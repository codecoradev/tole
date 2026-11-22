//! The calls the drivers make on the serve, as a trait.
//!
//! A driver is written against [`Api`], not against `tole_client::Client`, so
//! its tests script the replies and the event stream without a socket. The
//! implementation for the real client, [`ClientApi`], only forwards: no retry,
//! no decoding, no decision of its own (the client already never retries a
//! call). Its one job besides forwarding is to give the client the bounds a
//! driver relies on: a `Client` from `Client::new` has no request timeout, so
//! a hung server would hold a driver's single call slot for ever.
//!
//! Every method blocks, and a call may take as long as [`CALL_TIMEOUT`] per
//! read and per body, so a driver never calls one on the thread that owns its
//! state or while holding a lock.
//!
//! Each driver adds the calls it needs below the marker; a method that no
//! driver calls yet is not here.

use std::time::Duration;

use tole_client::{
    Approval, ApprovalList, Client, Decision, DecisionReply, Error, ReconnectPolicy,
    ResumingStream, StreamItem,
};

/// One session's event stream, as a source of items. It reconnects by itself
/// where the transport can (the real one is a [`ResumingStream`]); an `Err` or
/// an `Ok(None)` means it is over and will yield nothing more.
pub trait EventSource: Send {
    /// The next item. Blocks, also while reconnecting.
    fn next_item(&mut self) -> Result<Option<StreamItem>, Error>;
}

impl EventSource for ResumingStream {
    fn next_item(&mut self) -> Result<Option<StreamItem>, Error> {
        ResumingStream::next_item(self)
    }
}

/// The `after` of a stream that wants no entries: the approvals driver reads
/// the approval frames only, and a server that replays every entry of a long
/// session on each (re)connect would be a cost for nothing.
pub const NO_REPLAY: u64 = u64::MAX;

pub trait Api: Send + Sync + 'static {
    // ---- approvals ----

    /// `GET /approvals`, of one session or of all.
    fn approvals(&self, session: Option<&str>) -> Result<ApprovalList, Error>;

    /// `POST /approvals/{id}/decision`. The row carries the fingerprint a
    /// delegated child's allow needs (`adapter::approvals::row_for_decision`).
    fn decide_row(&self, row: &Approval, decision: Decision) -> Result<DecisionReply, Error>;

    /// The event stream of one session, from the entry after `after`. Does not
    /// block: the connection is made by the first `next_item`.
    fn follow_events(&self, session: &str, after: u64) -> Box<dyn EventSource>;

    // ---- room for the task driver (prompt, cancel, entries, status) and the
    // ---- skills driver (skills, tools): each adds its calls here.
}

/// The longest one blocking read of a call, and one answer's body, may take.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// The real client as an [`Api`], with the call bounds applied.
pub struct ClientApi(Client);

impl ClientApi {
    /// Wrap `client`, bounding each read of a call and each body by
    /// [`CALL_TIMEOUT`] (a call is then over after about a minute at worst).
    pub fn new(client: Client) -> Result<Self, Error> {
        let client = client
            .with_request_timeout(CALL_TIMEOUT)?
            .with_body_deadline(CALL_TIMEOUT)?;
        Ok(ClientApi(client))
    }
}

impl Api for ClientApi {
    fn approvals(&self, session: Option<&str>) -> Result<ApprovalList, Error> {
        self.0.approvals(session)
    }

    fn decide_row(&self, row: &Approval, decision: Decision) -> Result<DecisionReply, Error> {
        self.0.decide_row(row, decision)
    }

    fn follow_events(&self, session: &str, after: u64) -> Box<dyn EventSource> {
        Box::new(
            self.0
                .follow_events(session, after, ReconnectPolicy::default()),
        )
    }
}
