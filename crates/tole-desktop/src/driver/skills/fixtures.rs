//! What the tests of the skills driver share: answers built from the server's
//! own JSON, and the engine on a clock the test moves by hand.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tole_client::{Error, SkillsState, ToolsState};
use tole_desktop_model::Badge;

use super::engine::{Effect, Engine, Input, Now, Out, Reply, Route};
use super::store::Store;
use super::view::SkillsPageView;
use crate::adapter::skills::LoadId;

pub(super) const T0: u64 = 1_700_000_000_500;
pub(super) const REFRESH: Duration = Duration::from_secs(30);

pub(super) fn skill(name: &str, state: &str, status: &str) -> Value {
    json!({
        "name": name, "state": state, "status": status, "hash": "sha256:aa",
        "approved_hash": null, "approved_at": null, "source": null,
        "source_text": "", "history": []
    })
}

pub(super) fn draft(id: &str) -> Value {
    json!({
        "id": id, "name": format!("skill {id}"), "description": "How to",
        "created_at": "2026-11-17", "hash": "sha256:cd", "tainted": false,
        "warnings": [], "nearest": null
    })
}

pub(super) fn skills_from(skills: Vec<Value>, drafts: Vec<Value>, notes: &[&str]) -> SkillsState {
    serde_json::from_value(json!({
        "skills": skills, "drafts": drafts, "archived": [], "notes": notes
    }))
    .expect("a skills answer")
}

/// An answer with `drafts` drafts waiting and one approved skill.
pub(super) fn skills(drafts: usize) -> SkillsState {
    let drafts = (0..drafts).map(|i| draft(&format!("d{i}"))).collect();
    skills_from(vec![skill("report", "approved", "approved")], drafts, &[])
}

pub(super) fn tool(name: &str, trust: &str, state: &str) -> Value {
    json!({
        "trust": trust, "name": name, "state": state, "tier": "user",
        "source": null, "version": null, "approved_at": null, "secrets": [],
        "sandbox": "none", "manifest_hash": null, "lock": "-"
    })
}

pub(super) fn tools_from(servers: Vec<Value>, notes: &[&str]) -> ToolsState {
    serde_json::from_value(json!({"servers": servers, "notes": notes, "probed": false}))
        .expect("a tools answer")
}

/// An answer with one trusted tool and `untrusted` that are not.
pub(super) fn tools(untrusted: usize) -> ToolsState {
    let mut servers = vec![tool("cora", "trusted", "approved")];
    servers.extend((0..untrusted).map(|i| tool(&format!("t{i}"), "not_trusted", "unapproved")));
    tools_from(servers, &[])
}

pub(super) fn io_err(kind: io::ErrorKind) -> Error {
    Error::Io(io::Error::from(kind))
}

pub(super) fn status(code: u16, message: &str) -> Error {
    Error::Status {
        status: code,
        message: message.into(),
        body: Value::Null,
    }
}

pub(super) fn skills_ok(drafts: usize) -> Reply {
    Reply::Skills(Ok(skills(drafts)))
}

pub(super) fn tools_ok(untrusted: usize) -> Reply {
    Reply::Tools(Ok(tools(untrusted)))
}

pub(super) fn skills_err() -> Reply {
    Reply::Skills(Err(io_err(io::ErrorKind::ConnectionRefused)))
}

pub(super) fn tools_err() -> Reply {
    Reply::Tools(Err(io_err(io::ErrorKind::ConnectionRefused)))
}

/// The call of `route` the output asks for, if any.
pub(super) fn load_of(out: &Out, route: Route) -> Option<LoadId> {
    out.effects.iter().find_map(|e| match e {
        Effect::Load(r, id) if *r == route => Some(*id),
        Effect::Load(..) => None,
    })
}

/// An engine, its store and the time the test says it is.
pub(super) struct T {
    pub e: Engine,
    pub store: Arc<Store>,
    pub now: Now,
}

impl T {
    pub(super) fn new() -> T {
        T::with_refresh(REFRESH)
    }

    pub(super) fn with_refresh(refresh: Duration) -> T {
        let store = Arc::new(Store::default());
        T {
            e: Engine::new(Arc::clone(&store), refresh),
            store,
            now: Now {
                wall: T0,
                mono: 1_000,
            },
        }
    }

    /// Started and both tabs answered: two drafts and one untrusted tool.
    pub(super) fn known() -> T {
        let mut t = T::new();
        let out = t.start();
        let (s, g) = (load_of(&out, Route::Skills), load_of(&out, Route::Tools));
        t.step(Input::Loaded(s.expect("skills"), skills_ok(2)));
        t.step(Input::Loaded(g.expect("tools"), tools_ok(1)));
        let v = t.view();
        assert_eq!(v.skills_waiting, Badge::Count(2));
        assert_eq!(v.tools_needing_attention, Badge::Count(1));
        t
    }

    pub(super) fn start(&mut self) -> Out {
        self.e.start(self.now)
    }

    pub(super) fn step(&mut self, input: Input) -> Out {
        self.e.step(input, self.now)
    }

    /// Both readings move by `ms`, then the engine hears a tick.
    pub(super) fn after(&mut self, ms: u64) -> Out {
        self.now.wall += ms;
        self.now.mono += ms;
        self.step(Input::Tick)
    }

    /// The wall clock alone moves (the machine slept, the clock was set).
    pub(super) fn jump(&mut self, ms: u64) -> Out {
        self.now.wall += ms;
        self.step(Input::Tick)
    }

    /// Move time in quarter seconds until `route` is called, at most `ms`;
    /// the call and the time it took.
    pub(super) fn next_load(&mut self, route: Route, ms: u64) -> Option<(LoadId, u64)> {
        let mut spent = 0;
        while spent < ms {
            spent += 250;
            if let Some(id) = load_of(&self.after(250), route) {
                return Some((id, spent));
            }
        }
        None
    }

    pub(super) fn view(&self) -> SkillsPageView {
        self.store.view()
    }
}
