//! Tools for a discussion's helpers (#1039): what the owner allows, how the
//! driver reads the owner's answers back into the ledger, and how the helpers
//! are seated.
//!
//! The allowance is the owner's: `tole discuss start --tool <server>.<tool>`
//! names connected tools, the contract records them, and nothing a model
//! writes can add one. Every helper of a discussion that has an allowance runs
//! as an independent read-only child (plan mode, no allow pattern, see
//! [`crate::delegate`]); the first call of each named tool goes to the
//! owner's approval queue ([`crate::delegated_approver`]), and the driver
//! copies each answer into the discussion ledger as it appears, so a resume
//! replays it and the next helper starts with the tools already approved.

use std::sync::Arc;

use tole_core::child_env::DiscussionScope;
use tole_core::delegation_approvals::{Choice, ExpireReason, Status};
use tole_core::discussion::{self, Contract, ToolVerdict};
use tole_core::discussion_log::State;

use crate::config::Config;
use crate::delegated_approver::Store;
use crate::discussion_host::{DelegateIndependent, Driver};

/// One answer the owner's approval queue holds about a helper's request, as
/// the driver records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDecision {
    /// The queue's request id: a decision is recorded once per request.
    pub req: String,
    pub tool: String,
    pub verdict: ToolVerdict,
}

/// The registry names for the owner's `--tool <server>.<tool>` values.
/// `servers` are the connected servers this host knows (`name` of each
/// `--mcp-server name=...` or config entry): naming a tool of a server that is
/// not configured is refused, because the helper would silently lack it.
pub fn resolve(specs: &[String], servers: &[String]) -> Result<Vec<String>, String> {
    specs
        .iter()
        .map(|spec| {
            let (server, tool) = spec
                .split_once('.')
                .filter(|(s, t)| !s.is_empty() && !t.is_empty())
                .ok_or_else(|| {
                    format!("--tool {spec:?}: write it as <server>.<tool>, e.g. nginjen.search")
                })?;
            if !servers.iter().any(|s| s == server) {
                return Err(format!(
                    "--tool {spec:?}: no connected server `{server}` is configured here \
                     (configured: {})",
                    if servers.is_empty() {
                        "none".to_string()
                    } else {
                        servers.join(", ")
                    }
                ));
            }
            Ok(discussion::mcp_registry_name(server, tool))
        })
        .collect()
}

/// The first call of a named tool needs somewhere to be asked: the durable
/// approvals queue, which exists only with `[delegation] approvals` set to
/// `owner` or `chain`. With it off the helper would be denied every call, so
/// a discussion that allows tools is refused instead, before anything runs.
pub fn need_approvals(cfg: &Config) -> Result<(), String> {
    use tole_core::delegation_approvals::ApprovalsMode;
    match cfg.delegation.as_ref().and_then(|d| d.approvals) {
        Some(ApprovalsMode::Owner | ApprovalsMode::Chain) => Ok(()),
        _ => Err(
            "this discussion allows helper tools, which the owner approves one by one in the \
             approvals queue: set `[delegation] approvals = \"owner\"` in the user config (it is \
             off, so every call would be denied)"
                .into(),
        ),
    }
}

/// What a helper starting now may ask for: the contract's tools and, of them,
/// the ones the ledger says the owner already approved for this discussion.
pub fn scope(st: &State, c: &Contract) -> DiscussionScope {
    DiscussionScope {
        id: c.id.clone(),
        tools: c.tools.clone(),
        granted: st.tool_grants().into_iter().collect(),
    }
}

/// The line added to a helper's task: which connected tools it may use, and
/// that the owner decides.
pub fn task_note(tools: &[String]) -> String {
    format!(
        "The owner allowed you these connected tools for this discussion: {}. The first call of \
         each is shown to the owner, who may refuse it: if a call is denied, carry on without \
         that tool and say so in your contribution.",
        tools.join(", ")
    )
}

/// The decisions the approvals queue holds about requests of child `node`:
/// an allow (also once consumed), a deny, or a request that ended unanswered
/// (the window, an orphaned or settled child). A request still waiting is not
/// listed.
pub fn decisions(store: &Store, node: &str, now: i64) -> Result<Vec<ToolDecision>, String> {
    store.read(|d, a| {
        let state = a.state();
        state
            .entries()
            .filter(|e| e.req.node == node)
            .filter_map(|e| {
                let verdict = match (&e.decision, e.consumed_at) {
                    (Some(x), _) if x.verdict == Choice::Allow => ToolVerdict::Allow,
                    (None, Some(_)) => ToolVerdict::Allow,
                    (Some(_), _) => ToolVerdict::Deny,
                    (None, None) => match state.status(&e.req.req, d, now)? {
                        Status::Expired(
                            ExpireReason::Timeout
                            | ExpireReason::Orphaned
                            | ExpireReason::NodeSettled,
                        ) => ToolVerdict::Expired,
                        _ => return None,
                    },
                };
                Some(ToolDecision {
                    req: e.req.req.clone(),
                    tool: e.req.tool.clone(),
                    verdict,
                })
            })
            .collect()
    })
}

/// Seats every participant of `c` as an independent helper when it allows
/// tools (nothing changes otherwise): they run as read-only children of
/// `runner`, which asks the owner. Refused when the approvals queue is off or
/// there is no host to start children.
pub fn seat(
    driver: Driver,
    c: &Contract,
    cfg: &Config,
    delegator: Option<&Arc<crate::delegate::Delegator>>,
) -> Result<Driver, String> {
    if c.tools.is_empty() {
        return Ok(driver);
    }
    need_approvals(cfg)?;
    let d = delegator.ok_or("this host cannot start helper children (no delegation host)")?;
    driver.with_independent(
        &c.participants,
        Arc::new(DelegateIndependent(Arc::clone(d))),
    )
}

#[cfg(test)]
#[path = "helper_tools_tests.rs"]
mod tests;
