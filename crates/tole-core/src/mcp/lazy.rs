//! Lazy start of a locked, sandboxed stdio server (#750, design 9.1).
//!
//! The tools of a server whose approval stored a complete tool snapshot are
//! registered from that snapshot, and no process starts. The first call to
//! one of its tools starts it through the SAME path an eager registration
//! uses (`list_manifest`: the sandbox decision, the startup line logged
//! once, the `max_servers` cap) and then runs the SAME two checks the eager
//! path runs, in the same order:
//!
//! 1. `pre_start` before the process exists: the lock's decision on the
//!    launch identity (command line or URL, program file). The host runs it
//!    at registration too; running it again here means a binary swapped
//!    between registration and first use is never started.
//! 2. `admit` on the LIVE manifest and version: a mismatch with the
//!    approval refuses the call, stops the server and refuses every later
//!    call of the session (the tools stay registered, the model sees the
//!    "changed since approval" note), as an eager refusal registers nothing.
//!
//! Nothing is skipped or deferred past first use: until `ensure` succeeds no
//! call reaches the server. A server whose snapshot is missing, stale or not
//! stored in full is never lazy ([`lazy_tools`] returns `None`) and starts
//! eagerly as before.

use super::{list_manifest, register_tools, runtime, McpRequest, McpServerConfig, ServerLease};
use crate::plugin_lock::{self, ManifestTool, SnapshotTool};
use std::sync::{Arc, Mutex};

/// The lock's decision before the process starts: `Err(note)` refuses.
pub type PreStart = Box<dyn Fn() -> Result<(), String> + Send + Sync>;
/// The lock's decision on the live manifest (the eager `Admit`, owned).
pub type AdmitOwned = Box<dyn Fn(&str, &str, &[ManifestTool]) -> Result<(), String> + Send + Sync>;

/// The two checks of an eager start, owned by the lazy server.
pub struct LazyGuard {
    pub pre_start: PreStart,
    pub admit: AdmitOwned,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    /// Not started: the first call runs the checks.
    Dormant,
    /// Started and admitted this session.
    Admitted,
    /// Refused this session; every call returns the note.
    Refused(String),
}

/// One lazily registered server, shared by its tools.
pub struct LazyServer {
    cfg: McpServerConfig,
    guard: LazyGuard,
    state: Mutex<State>,
}

impl LazyServer {
    /// Runs the checks and starts the server on first use; a later call is
    /// a lock and a compare. Serialised: two first calls start it once.
    pub(super) fn ensure(&self) -> Result<(), String> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match &*st {
            State::Admitted => return Ok(()),
            State::Refused(note) => return Err(note.clone()),
            State::Dormant => {}
        }
        let refuse = |st: &mut State, note: String| {
            eprintln!("tole: mcp[{}]: {note}", self.cfg.name);
            // Kill a process an admission refusal left running.
            let _ = runtime().send(McpRequest::Stop {
                key: self.cfg.conn_key(),
            });
            *st = State::Refused(note.clone());
            Err(note)
        };
        if let Err(note) = (self.guard.pre_start)() {
            return refuse(&mut st, note);
        }
        // The start itself (sandbox plan and its logged decision, the cap).
        // A start that fails is not sticky: the next call tries again, as
        // a slot may free up; a refused sandbox decision is logged once.
        let (version, manifest) = list_manifest(&self.cfg)
            .map_err(|e| format!("mcp[{}]: not started: {e}", self.cfg.name))?;
        if let Err(note) = (self.guard.admit)(&self.cfg.name, &version, &manifest) {
            return refuse(&mut st, note);
        }
        *st = State::Admitted;
        Ok(())
    }

    /// Whether the first call has run the checks and they passed.
    pub fn admitted(&self) -> bool {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) == State::Admitted
    }
}

/// The `(tool name, description)` pairs to register for a locked server, or
/// `None` when the server must start eagerly: the snapshot is empty, a field
/// is cut or not stored, a text differs from its recorded hash (redaction or
/// cleaning changed it), a name is not a plain tool name, or the rebuilt
/// manifest does not hash to the lock entry's `manifest_hash` (a stale or
/// hand-edited snapshot).
pub fn lazy_tools(snapshot: &[SnapshotTool], manifest_hash: &str) -> Option<Vec<(String, String)>> {
    if snapshot.is_empty() {
        return None;
    }
    let name_ok = |n: &str| {
        !n.is_empty()
            && n.len() <= 64
            && n.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    };
    let mut rebuilt = Vec::with_capacity(snapshot.len());
    for t in snapshot {
        if !name_ok(&t.name)
            || plugin_lock::sha(t.description.as_bytes()) != t.description_hash
            || plugin_lock::sha(t.schema.as_bytes()) != t.schema_hash
        {
            return None;
        }
        rebuilt.push(ManifestTool {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: serde_json::from_str(&t.schema).ok()?,
        });
    }
    (plugin_lock::manifest_hash(&rebuilt) == manifest_hash).then(|| {
        rebuilt
            .into_iter()
            .map(|t| (t.name, t.description))
            .collect()
    })
}

/// Registers `tools` (from [`lazy_tools`]) of `cfg` without starting it.
/// The tools run the checks of `guard` before their first call reaches the
/// server. Returns the registered tool names. The registry does NOT mark the
/// server admitted (#680): until a call admitted it, it offers no prompts or
/// resources.
pub fn register_server_tools_lazy(
    reg: &mut crate::tool::ToolRegistry,
    cfg: &McpServerConfig,
    secrets: &crate::secrets::SecretSet,
    tools: Vec<(String, String)>,
    guard: LazyGuard,
) -> Vec<String> {
    let start = std::time::Instant::now();
    reg.set_mcp_admitted(&cfg.name, false);
    if let Some(l) = ServerLease::acquire(cfg) {
        reg.hold(l);
    }
    let lazy = Arc::new(LazyServer {
        cfg: cfg.clone(),
        guard,
        state: Mutex::new(State::Dormant),
    });
    let names = register_tools(reg, cfg, secrets, tools, Some(&lazy));
    eprintln!(
        "tole: mcp[{}]: {} tool(s) registered from the locked snapshot in {:?} (starts on first use)",
        cfg.name,
        names.len(),
        start.elapsed()
    );
    names
}
