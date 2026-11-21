//! Deciding what to do about the local serve: attach to a running one, start
//! one, or start it again after it stopped.
//!
//! Everything that decides happens under the launch lock (`desktop.lock`), so
//! two desktops launching at once cannot both start a serve or overwrite each
//! other's credential file. The serve's own lock (`serve.sock.lock`, taken by
//! `tole serve` for as long as it lives) is the final word: a second serve on
//! the same socket refuses to start, whoever launched it.

use std::process::ExitStatus;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tole_desktop_model::{ServeFailure, ServeState};

use super::binary;
use super::config::Config;
use super::hub::Hub;
use super::probe::{self, Credential, Probe};
use super::process::Serve;
use super::sys::{self, LaunchLock};
use super::token::{self, Token};

/// A serve this desktop started, with when.
pub(super) struct Owned {
    pub serve: Serve,
    pub started: Instant,
}

/// How an attach-or-start ended: the state is `Attached` or `Running`, or the
/// reason it is neither. The caller puts a failure in the hub (or decides to
/// try again).
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Ready,
    Failed(ServeFailure),
}

pub struct Engine {
    pub(super) cfg: Config,
    pub(super) hub: Arc<Hub>,
    pub(super) owned: Option<Owned>,
    /// Starts in a row that ended in a stop, since the serve last ran steadily.
    pub(super) attempts: u32,
    /// When the next automatic start is due, while the state is `Restarting`.
    pub(super) retry_at: Option<Instant>,
}

/// What `start` found.
enum Started {
    Ready,
    /// Our child lost the race for the socket to a serve that now answers.
    Lost,
    Failed(ServeFailure),
}

impl Engine {
    pub fn new(cfg: Config, hub: Arc<Hub>) -> Engine {
        Engine {
            cfg,
            hub,
            owned: None,
            attempts: 0,
            retry_at: None,
        }
    }

    /// Attach to the serve that answers on the socket, or start one.
    pub fn ensure(&mut self) -> Outcome {
        if !matches!(self.hub.state(), ServeState::Restarting { .. }) {
            self.hub.set(ServeState::Starting);
        }
        let dir = &self.cfg.runtime_dir;
        if let Err(reason) = sys::prepare_private_dir(dir) {
            return self.directory_failure(reason);
        }
        let _launch = match LaunchLock::acquire(&self.cfg.launch_lock(), self.cfg.launch_wait) {
            Ok(lock) => lock,
            Err(e) => {
                return Outcome::Failed(ServeFailure::StartFailed {
                    detail: e.to_string(),
                })
            }
        };
        // Attach, or start and then attach to whoever won the socket.
        for _ in 0..3 {
            match probe::probe(&self.cfg.socket()) {
                Probe::Serve { protocol, version } => return self.attach(protocol, version),
                Probe::NotTole => return Outcome::Failed(ServeFailure::NotTole),
                Probe::Insecure(reason) => return self.directory_failure(reason),
                Probe::Invalid(detail) => {
                    return Outcome::Failed(ServeFailure::StartFailed { detail })
                }
                Probe::Absent => match self.start() {
                    Started::Ready => return Outcome::Ready,
                    Started::Lost => continue,
                    Started::Failed(f) => return Outcome::Failed(f),
                },
            }
        }
        Outcome::Failed(ServeFailure::StartFailed {
            detail: "the socket kept changing hands".into(),
        })
    }

    fn directory_failure(&self, reason: String) -> Outcome {
        Outcome::Failed(ServeFailure::DirectoryNotPrivate {
            path: self.cfg.runtime_dir.display().to_string(),
            reason,
        })
    }

    /// A serve answers: use it if it speaks our protocol and we hold the
    /// credential it accepts.
    fn attach(&mut self, protocol: Option<u32>, version: Option<String>) -> Outcome {
        let version = match probe::check_protocol(protocol, version.as_deref()) {
            Ok(v) => v,
            Err(failure) => return Outcome::Failed(failure),
        };
        // Without a credential file we cannot sign in: not a serve this desktop
        // started (or one whose file was lost). The owner restarts it.
        let token = match token::read(&self.cfg.runtime_dir) {
            Ok(Some(t)) => t,
            Ok(None) | Err(_) => return Outcome::Failed(ServeFailure::CredentialUnavailable),
        };
        match probe::check_credential(&self.cfg.socket(), token.expose()) {
            Credential::Accepted => {}
            Credential::Rejected => return Outcome::Failed(ServeFailure::CredentialUnavailable),
            Credential::Unknown => {
                return Outcome::Failed(ServeFailure::StartFailed {
                    detail: "the serve did not answer".into(),
                })
            }
        }
        self.hub.set_credential(Some(token));
        self.hub.set(if self.owned.is_some() {
            ServeState::Running { version }
        } else {
            ServeState::Attached { version }
        });
        Outcome::Ready
    }

    /// Nothing answers: start a serve, wait until it does.
    fn start(&mut self) -> Started {
        let path_var = self
            .cfg
            .path_override
            .clone()
            .or_else(|| std::env::var_os("PATH"));
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(std::path::Path::to_owned));
        let Ok((bin, _)) = binary::resolve(
            self.cfg.binary.as_deref(),
            exe_dir.as_deref(),
            path_var.as_deref(),
        ) else {
            return Started::Failed(ServeFailure::BinaryNotFound);
        };
        // Whatever credential file is there belongs to a serve that is gone.
        token::remove(&self.cfg.runtime_dir);
        self.hub.set_credential(None);
        let fail = |detail: String| Started::Failed(ServeFailure::StartFailed { detail });
        let token = match Token::generate() {
            Ok(t) => t,
            Err(e) => return fail(format!("cannot make a credential: {e}")),
        };
        let mut serve = match Serve::spawn(
            &bin,
            &self.cfg.socket(),
            &self.cfg.workspace,
            self.cfg.cron,
            &token,
        ) {
            Ok(s) => s,
            Err(e) => return fail(format!("cannot run {}: {e}", bin.display())),
        };

        let socket = self.cfg.socket();
        let deadline = Instant::now() + self.cfg.startup_timeout;
        loop {
            if let Ok(Some(status)) = serve.try_wait() {
                std::thread::sleep(Duration::from_millis(100)); // let stderr drain
                let said = serve.last_lines();
                // It may have lost the socket to a serve that is starting or
                // started elsewhere (it says so): give that one time to answer.
                let contested =
                    said.contains("another tole serve") || said.contains("already answers");
                let grace = if contested {
                    Duration::from_secs(2)
                } else {
                    Duration::ZERO
                };
                if wait_for_serve(&socket, grace) {
                    return Started::Lost;
                }
                return fail(describe_exit(status, &said));
            }
            match probe::probe(&socket) {
                Probe::Serve { protocol, version } => {
                    // Ours only if it accepts our credential: another serve may
                    // answer while ours is about to give up the socket.
                    if probe::check_credential(&socket, token.expose()) == Credential::Accepted {
                        return self.adopt(serve, token, protocol, version);
                    }
                }
                Probe::Absent => {}
                Probe::NotTole => {
                    serve.kill();
                    return Started::Failed(ServeFailure::NotTole);
                }
                Probe::Insecure(reason) => {
                    serve.kill();
                    return Started::Failed(ServeFailure::DirectoryNotPrivate {
                        path: self.cfg.runtime_dir.display().to_string(),
                        reason,
                    });
                }
                Probe::Invalid(detail) => {
                    serve.kill();
                    return fail(detail);
                }
            }
            if Instant::now() >= deadline {
                serve.kill();
                let tail = serve.last_lines();
                return fail(format!(
                    "it did not answer within {} s{}",
                    self.cfg.startup_timeout.as_secs().max(1),
                    if tail.is_empty() {
                        String::new()
                    } else {
                        format!(": {tail}")
                    }
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Our child answers and accepts our credential: it is the serve in use.
    fn adopt(
        &mut self,
        mut serve: Serve,
        token: Token,
        protocol: Option<u32>,
        version: Option<String>,
    ) -> Started {
        let version = match probe::check_protocol(protocol, version.as_deref()) {
            Ok(v) => v,
            Err(failure) => {
                serve.kill();
                return Started::Failed(failure);
            }
        };
        if let Err(e) = token::write(&self.cfg.runtime_dir, &token) {
            serve.kill();
            return Started::Failed(ServeFailure::StartFailed {
                detail: format!("cannot save the credential for a second window: {e}"),
            });
        }
        self.owned = Some(Owned {
            serve,
            started: Instant::now(),
        });
        self.hub.set_credential(Some(token));
        self.hub.set(ServeState::Running { version });
        Started::Ready
    }
}

/// Whether a serve answers on `socket` within `wait`.
fn wait_for_serve(socket: &std::path::Path, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    loop {
        if matches!(probe::probe(socket), Probe::Serve { .. }) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// How a child ended, with the last thing it said.
pub(super) fn describe_exit(status: ExitStatus, last_lines: &str) -> String {
    use std::os::unix::process::ExitStatusExt;
    let how = match (status.code(), status.signal()) {
        (Some(code), _) => format!("it exited with code {code}"),
        (None, Some(sig)) => format!("it was stopped by signal {sig}"),
        (None, None) => "it stopped".to_owned(),
    };
    match last_lines.lines().last() {
        Some(line) => format!("{how}: {line}"),
        None => how,
    }
}
