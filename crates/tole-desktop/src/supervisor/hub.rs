//! What the supervisor shares with the rest of the desktop: the state of the
//! serve, who is told when it changes, and the credential a client needs.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use tole_client::Client;
use tole_desktop_model::ServeState;

use super::token::Token;

/// Called with every new state, on the thread that changed it (the launching
/// thread or the monitor). It must not call back into the supervisor and
/// should hand the state to the window's own thread.
pub type Observer = Box<dyn Fn(&ServeState) + Send + Sync>;

pub struct Hub {
    socket: PathBuf,
    state: Mutex<ServeState>,
    token: Mutex<Option<Token>>,
    observer: Mutex<Option<Observer>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Hub {
    pub fn new(socket: PathBuf, observer: Option<Observer>) -> Arc<Hub> {
        Arc::new(Hub {
            socket,
            state: Mutex::new(ServeState::Starting),
            token: Mutex::new(None),
            observer: Mutex::new(observer),
        })
    }

    pub fn state(&self) -> ServeState {
        lock(&self.state).clone()
    }

    /// Record a new state and tell the observer, once, when it differs from
    /// the one before.
    pub fn set(&self, new: ServeState) {
        {
            let mut state = lock(&self.state);
            if *state == new {
                return;
            }
            *state = new.clone();
        }
        if let Some(observer) = lock(&self.observer).as_ref() {
            observer(&new);
        }
    }

    /// Tell the observer the state as it is now, even though it did not change:
    /// the first thing a window hears is `Starting`.
    pub fn announce(&self) {
        let state = self.state();
        if let Some(observer) = lock(&self.observer).as_ref() {
            observer(&state);
        }
    }

    /// The credential of the serve that is in use, or none.
    pub fn set_credential(&self, token: Option<Token>) {
        *lock(&self.token) = token;
    }

    /// A typed client of the serve, when it can be used now. It has no request
    /// timeout (a prompt answers when its turn ends); set one on the clone
    /// where a bound is wanted.
    pub fn client(&self) -> Option<Client> {
        if !self.state().is_ready() {
            return None;
        }
        let token = lock(&self.token);
        let text = token.as_ref()?.expose().to_owned();
        Client::connect_unix(&self.socket, Some(text)).ok()
    }
}
