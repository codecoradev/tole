//! The link between the backend and the local serve, as a trait, so the
//! backend is tested without a real serve.
//!
//! The backend asks the link three things: what the serve is doing
//! ([`ServeLink::state`]), whether it may have been replaced
//! ([`ServeLink::epoch`]) and, when it is ready, an [`Api`] to give the drivers
//! ([`ServeLink::api`]). The real link is [`SupervisorLink`], over the
//! [`Supervisor`]; the tests have a fake that flips the state by hand.
//!
//! All three are cheap and never block for long: the backend calls them on its
//! worker thread and on the UI's thread (a snapshot).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tole_desktop_model::ServeState;

use crate::driver::api::{Api, ClientApi};
use crate::driver::guard::guarded;
use crate::supervisor::{Config, Supervisor};

/// What the backend needs from the serve it follows.
pub trait ServeLink: Send + Sync + 'static {
    /// What the serve is doing now.
    fn state(&self) -> ServeState;

    /// A number that changes whenever the serve may have been replaced: every
    /// change of [`state`](Self::state) counts, also one the polling would not
    /// see (a restart that comes back before the next look). The backend
    /// builds new drivers, over a new [`Api`], when it differs from the one the
    /// drivers were started under. Only a difference means anything.
    fn epoch(&self) -> u64;

    /// An [`Api`] for the serve as it is now, or `None` when there is none
    /// yet (the serve is not ready, or the client could not be built): the
    /// backend asks again, with a growing wait. Called once per attach, because
    /// a new serve may have a new credential.
    fn api(&self) -> Option<Arc<dyn Api>>;
}

/// The local serve as the supervisor keeps it: the state is the supervisor's,
/// the [`Api`] is a [`ClientApi`] over its client, and the epoch counts the
/// states its observer was told about.
pub struct SupervisorLink {
    supervisor: Arc<Supervisor>,
    epoch: Arc<AtomicU64>,
}

impl SupervisorLink {
    /// [`Supervisor::launch_observed`], with the observer the epoch needs.
    /// Blocks until the launch is settled (see [`Supervisor::launch`]), so call
    /// it from a worker thread.
    pub fn launch(cfg: Config) -> Self {
        let epoch = Arc::new(AtomicU64::new(0));
        let count = Arc::clone(&epoch);
        // The observer only counts: it must not call back into the supervisor.
        let supervisor = Supervisor::launch_observed(cfg, move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        });
        SupervisorLink {
            supervisor: Arc::new(supervisor),
            epoch,
        }
    }

    /// The supervisor, for what the window does to it (`retry`,
    /// `restart_serve`, and `close_window` or `quit_completely` once the
    /// backend is shut down and this is the last `Arc`).
    pub fn supervisor(&self) -> &Arc<Supervisor> {
        &self.supervisor
    }

    /// The supervisor, by value of its `Arc`: `Arc::try_unwrap` it for
    /// `close_window` once nothing else holds it (the backend is shut down).
    pub fn into_supervisor(self) -> Arc<Supervisor> {
        self.supervisor
    }
}

impl ServeLink for SupervisorLink {
    fn state(&self) -> ServeState {
        self.supervisor.state()
    }

    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    fn api(&self) -> Option<Arc<dyn Api>> {
        // A client or a bound that cannot be built is "no api yet", not a panic.
        let api = ClientApi::new(self.supervisor.client()?).ok()?;
        Some(Arc::new(api))
    }
}

/// One look at the link.
pub(super) struct Look {
    pub state: ServeState,
    pub epoch: u64,
}

/// The link as the backend uses it: a panic in it is contained. A link that
/// panics reads as a serve that is not ready, so no driver outlives it.
#[derive(Clone)]
pub(super) struct Link(Arc<dyn ServeLink>);

impl Link {
    pub(super) fn new(link: Arc<dyn ServeLink>) -> Self {
        Link(link)
    }

    /// The state and the epoch, the epoch first: a change between the two
    /// reads leaves the epoch older than the state, so the next look sees a
    /// difference and nothing is missed (the reverse could miss one).
    pub(super) fn look(&self) -> Look {
        guarded(|| {
            let epoch = self.0.epoch();
            Look {
                epoch,
                state: self.0.state(),
            }
        })
        .unwrap_or(Look {
            state: ServeState::Starting,
            epoch: 0,
        })
    }

    pub(super) fn api(&self) -> Option<Arc<dyn Api>> {
        guarded(|| self.0.api()).flatten()
    }
}
