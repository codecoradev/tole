//! The tool-call authorization gate (issue #303).
//!
//! One place answers "may this tool call run?" for the turn loop, for both
//! a fresh provider call ([`Mode::Fresh`]) and a crash-replayed intent
//! ([`Mode::Replay`]). The gate is a policy decision point (PDP): it reads
//! the tool's risk ONCE, consults the approver and then the opt-in
//! pre-hooks, and returns either an [`Authorized`] call or a typed
//! [`Denied`]. The caller stays the enforcement point (PEP): durable
//! records, cancel checkpoints, observer and post-hooks live in `turn.rs`.
//!
//! Complete mediation is a compile-time property inside the crate: the
//! only way the turn loop runs a tool is [`Authorized::execute`], and a
//! Write/Destructive [`Authorized`] can only be produced by [`authorize`]
//! (the [`Permit`] constructor is private to this module). The public
//! `Tool` trait and `ToolRegistry::decide` are unchanged.

use crate::approval::Verdict;
use crate::machine::ReplaySafety;
use crate::tool::{Risk, Tool, ToolRegistry};
use serde_json::Value;

/// How the call reached the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// A fresh provider tool call: gated whenever risk != ReadOnly.
    Fresh,
    /// A crash-replayed intent. Gated when the recorded safety is
    /// `Guarded` OR the CURRENT registry risk != ReadOnly (#249: the
    /// recorded value may be stale if the host wiring changed).
    Replay { recorded: ReplaySafety },
}

impl Mode {
    fn needs_gate(self, current: Option<Risk>) -> bool {
        let non_read_only = matches!(current, Some(r) if r != Risk::ReadOnly);
        match self {
            Mode::Fresh => non_read_only,
            Mode::Replay { recorded } => recorded == ReplaySafety::Guarded || non_read_only,
        }
    }
}

/// Why the gate refused the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Denied {
    /// The approver denied, or no approver is wired (fails closed).
    Approver,
    /// An opt-in pre-hook exited 2 with this reason.
    #[cfg(feature = "shell-tools")]
    PreHook { reason: String },
    /// The tool is not registered. On replay the caller decides what that
    /// means (see `turn.rs`: the meaning depends on the recorded safety).
    UnknownTool,
}

/// Proof that the approver (and pre-hooks) allowed a non-ReadOnly call.
/// Cannot be constructed outside this module.
#[derive(Debug)]
pub(crate) struct Permit(());

/// An authorized call: the tool, plus a [`Permit`] when it is non-ReadOnly.
/// The risk is carried so callers never re-read it from the tool.
pub(crate) struct Authorized<'a> {
    tool: &'a dyn Tool,
    risk: Risk,
    permit: Option<Permit>,
}

impl Authorized<'_> {
    /// True for Write/Destructive (a [`Permit`] was issued).
    pub(crate) fn is_write(&self) -> bool {
        self.risk != Risk::ReadOnly
    }

    /// The crate-internal execute funnel: consumes the authorization.
    pub(crate) fn execute(self, input: Value) -> Result<Value, String> {
        let Authorized { tool, risk, permit } = self;
        debug_assert_eq!(risk != Risk::ReadOnly, permit.is_some());
        tool.execute(input)
    }
}

/// Decide whether `name(input)` may run. Order is pinned: lookup, risk
/// (read once), approver, then pre-hook.
pub(crate) fn authorize<'a>(
    registry: &'a ToolRegistry,
    name: &str,
    input: &Value,
    mode: Mode,
) -> Result<Authorized<'a>, Denied> {
    let Some(tool) = registry.get(name) else {
        return Err(Denied::UnknownTool);
    };
    let risk = tool.risk();
    // `needs_gate` can be true for a Guarded replay whose tool is now
    // ReadOnly; the inner check is on the CURRENT risk, so no approver is
    // consulted and no permit issued (quirk pinned by PR 1).
    if mode.needs_gate(Some(risk)) && risk != Risk::ReadOnly {
        if !matches!(registry.decide(name, input), Some(Verdict::Allow)) {
            return Err(Denied::Approver);
        }
        #[cfg(feature = "shell-tools")]
        if let Some(reason) = registry.pre_hook_denial(name, input) {
            return Err(Denied::PreHook { reason });
        }
        return Ok(Authorized {
            tool,
            risk,
            permit: Some(Permit(())),
        });
    }
    // A non-ReadOnly tool is always gated in both modes, so only ReadOnly
    // reaches here.
    debug_assert_eq!(risk, Risk::ReadOnly);
    Ok(Authorized {
        tool,
        risk,
        permit: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::{Approver, ToolRequest};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct Probe(Risk);
    impl Tool for Probe {
        fn name(&self) -> &str {
            "t"
        }
        fn risk(&self) -> Risk {
            self.0
        }
        fn execute(&self, input: Value) -> Result<Value, String> {
            Ok(input)
        }
    }

    struct Fixed(Verdict, Arc<AtomicUsize>);
    impl Approver for Fixed {
        fn decide(&self, _r: &ToolRequest<'_>) -> Verdict {
            self.1.fetch_add(1, Ordering::SeqCst);
            self.0
        }
        fn interactive(&self) -> bool {
            true
        }
    }

    fn reg(verdict: Verdict, risk: Risk) -> (ToolRegistry, Arc<AtomicUsize>) {
        let asked = Arc::new(AtomicUsize::new(0));
        let mut r = ToolRegistry::with_approver(Fixed(verdict, asked.clone()));
        r.register(Box::new(Probe(risk))).unwrap();
        (r, asked)
    }

    const MODES: [Mode; 3] = [
        Mode::Fresh,
        Mode::Replay {
            recorded: ReplaySafety::Idempotent,
        },
        Mode::Replay {
            recorded: ReplaySafety::Guarded,
        },
    ];

    #[test]
    fn non_read_only_allow_issues_permit_in_every_mode() {
        for risk in [Risk::Write, Risk::Destructive] {
            for mode in MODES {
                let (r, asked) = reg(Verdict::Allow, risk);
                let a = authorize(&r, "t", &json!({}), mode).unwrap();
                assert!(a.permit.is_some() && a.is_write() && a.risk == risk);
                assert_eq!(asked.load(Ordering::SeqCst), 1);
                assert_eq!(a.execute(json!(1)), Ok(json!(1)));
            }
        }
    }

    #[test]
    fn non_read_only_deny_is_approver_denial_in_every_mode() {
        for mode in MODES {
            let (r, _) = reg(Verdict::Deny, Risk::Write);
            assert_eq!(
                authorize(&r, "t", &json!({}), mode).err(),
                Some(Denied::Approver)
            );
        }
    }

    #[test]
    fn read_only_never_asks_the_approver_in_any_mode() {
        // Includes the pinned quirk: Guarded recorded, tool now ReadOnly.
        for mode in MODES {
            let (r, asked) = reg(Verdict::Deny, Risk::ReadOnly);
            let a = authorize(&r, "t", &json!({}), mode).unwrap();
            assert!(a.permit.is_none() && !a.is_write());
            assert_eq!(asked.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn unknown_tool_is_typed_in_every_mode() {
        for mode in MODES {
            let (r, asked) = reg(Verdict::Allow, Risk::Write);
            assert_eq!(
                authorize(&r, "nope", &json!({}), mode).err(),
                Some(Denied::UnknownTool)
            );
            assert_eq!(asked.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn needs_gate_matrix() {
        let g = Mode::Replay {
            recorded: ReplaySafety::Guarded,
        };
        let i = Mode::Replay {
            recorded: ReplaySafety::Idempotent,
        };
        assert!(!Mode::Fresh.needs_gate(Some(Risk::ReadOnly)));
        assert!(Mode::Fresh.needs_gate(Some(Risk::Write)));
        assert!(!Mode::Fresh.needs_gate(None));
        assert!(g.needs_gate(Some(Risk::ReadOnly)));
        assert!(g.needs_gate(None));
        assert!(i.needs_gate(Some(Risk::Write)));
        assert!(!i.needs_gate(Some(Risk::ReadOnly)));
        assert!(!i.needs_gate(None));
    }
}
