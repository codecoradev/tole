//! Tool trait, risk classification, registry, and the approval gate
//! wiring (§7, E4/E6).
//!
//! ReadOnly tools auto-execute in the loop. Write tools require an
//! approver wired in; Destructive tools additionally require an
//! *interactive* approver (a human must be reachable — PRD: Destructive
//! cannot be allowlisted). At call time the approver decides per
//! invocation.

use crate::action_summary::ActionSummary;
use crate::approval::{Approver, ToolRequest, Verdict};
use serde_json::{json, Value};
use std::collections::HashMap;

/// What a tool is allowed to touch. Drives the approval gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    /// Pure read: runs without approval; replay-safe.
    ReadOnly,
    /// Mutates state: requires approval unless allowlisted.
    Write,
    /// Irreversible: always requires approval (allowlist may opt in).
    Destructive,
}

impl Risk {
    /// Stable label for prompts and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Risk::ReadOnly => "ReadOnly",
            Risk::Write => "Write",
            Risk::Destructive => "Destructive",
        }
    }
}

/// A capability the agent may call. Phase 1: synchronous, in-process.
pub trait Tool: Send + Sync {
    /// Stable name the provider refers to in a `ToolCall`.
    fn name(&self) -> &str;
    /// Risk classification; the loop auto-executes `ReadOnly` only.
    fn risk(&self) -> Risk;
    /// Poll-style classification (issue #231): poll tools may repeat
    /// identical calls for the whole duration of a detached job — the
    /// turn loop grants them the dedicated poll budget
    /// (`POLL_LOOP_TRIP_AFTER`) instead of consuming `MAX_STEPS`, and
    /// raises their loop guard accordingly. Default false; the built-in
    /// poll tools (`job_poll`, `agent_poll`) opt in.
    fn is_poll(&self) -> bool {
        false
    }
    /// One-line human summary of what this call will do, shown in
    /// approval prompts (E6). Default: tool name + risk.
    fn describe(&self, _input: &Value) -> String {
        format!("{} ({})", self.name(), self.risk().as_str())
    }
    /// Static, provider-facing description of the tool, sent as the
    /// `description` of its spec (issue #320). Unlike `describe`, which
    /// renders ONE concrete call for an approval prompt and may validate
    /// its input (and so return an error string for `Null`), this takes
    /// no input and must never fail.
    ///
    /// Default: tool name + risk tier. Deliberately NOT derived from
    /// `describe(Null)`: there is no way to tell a real description from
    /// a validation error string. Override with one sentence on what the
    /// tool does and its main operations.
    fn summary(&self) -> String {
        format!("{} ({})", self.name(), self.risk().as_str())
    }
    /// JSON Schema for the input object, sent to the provider as this
    /// tool's `parameters` (E4.5). `None` (the default) means "object
    /// with no declared properties" — still callable, just untyped.
    fn spec(&self) -> Option<Value> {
        None
    }
    /// Where this call would write, as a path RELATIVE to the tool's
    /// canonical jail root with symlinks already resolved — the file the
    /// tool will actually touch, not the text the model sent (#437). Only
    /// jailed file writers implement it; the approver's workspace guard
    /// ([`crate::approval::WORKSPACE_GUARD`]) fails closed (prompts) on
    /// `None`. Decorators must forward it.
    fn approval_target(&self, _input: &Value) -> Option<String> {
        None
    }
    /// The structured summary of what this call will do, shown first in every
    /// approval surface (#1041). Derive it from the tool's own knowledge and
    /// the input by code, never from model-written text; it never changes
    /// [`Tool::risk`]. `None` (the default) makes the registry use the neutral
    /// [`ActionSummary::unknown`]. Decorators must forward it.
    fn action_summary(&self, _input: &Value) -> Option<ActionSummary> {
        None
    }
    /// Execute with JSON input, return JSON output or a plain error.
    fn execute(&self, input: Value) -> Result<Value, String>;
}

/// Name → tool lookup for the turn loop, with approval gating.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
    approver: Option<Box<dyn Approver>>,
    /// Opt-in process hooks (issue #110, shell-tools only): deny-only
    /// policy injection at the tool boundary. `None` = off, zero cost.
    #[cfg(feature = "shell-tools")]
    hooks: Option<crate::hooks::ToolHooks>,
    /// Records the untrusted tools a session ran (#592): web and MCP output
    /// taints a skill draft. `None` = nothing is recorded.
    taint: Option<crate::skill_draft::TaintTracker>,
    /// MCP servers whose tools this registry's admission check accepted
    /// (#680). Only these may offer resources and prompts to this session.
    /// Per registry, so sessions with different plugin locks in one process
    /// (serve, the cron loop) never share a decision.
    mcp_admitted: std::collections::BTreeSet<String>,
    /// Guards released when the registry (the session) is dropped, e.g. the
    /// lease of a sandboxed MCP server (#601).
    holds: Vec<std::sync::Arc<dyn std::any::Any + Send + Sync>>,
}

impl ToolRegistry {
    /// Empty registry with no approver: **ReadOnly tools only**.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registry with an approval gate wired in: Write tools become
    /// registrable, subject to per-call decisions. Destructive tools
    /// additionally require an interactive approver.
    pub fn with_approver(approver: impl Approver + 'static) -> Self {
        Self {
            tools: HashMap::new(),
            approver: Some(Box::new(approver)),
            #[cfg(feature = "shell-tools")]
            hooks: None,
            taint: None,
            mcp_admitted: Default::default(),
            holds: Vec::new(),
        }
    }

    /// Keeps `guard` alive as long as this registry lives.
    pub fn hold(&mut self, guard: std::sync::Arc<dyn std::any::Any + Send + Sync>) {
        self.holds.push(guard);
    }

    /// Records whether the MCP server `name` passed this registry's
    /// admission check (#680).
    pub fn set_mcp_admitted(&mut self, name: &str, admitted: bool) {
        if admitted {
            self.mcp_admitted.insert(name.to_string());
        } else {
            self.mcp_admitted.remove(name);
        }
    }

    /// Whether the MCP server `name` passed this registry's admission check
    /// (#680): only such a server may offer resources and prompts.
    pub fn mcp_admitted(&self, name: &str) -> bool {
        self.mcp_admitted.contains(name)
    }

    /// Record every untrusted tool this registry executes into `tracker`
    /// (#592). The turn loop's execute funnel does the recording, so no
    /// tool can run without being seen.
    pub fn set_taint_tracker(&mut self, tracker: crate::skill_draft::TaintTracker) {
        self.taint = Some(tracker);
    }

    pub(crate) fn taint_tracker(&self) -> Option<&crate::skill_draft::TaintTracker> {
        self.taint.as_ref()
    }

    /// Recompute the tool taint from the tool calls in `entries`, the
    /// session about to run a turn (#592). It runs at the start of every
    /// turn, so a resumed or forked session that fetched from outside
    /// before this process started is not clean, and a registry reused for
    /// another session (`/new`, `/resume`) never carries the wrong
    /// session's taint. Intents carry the tool name in `payload.tool`.
    pub(crate) fn seed_taint(&self, entries: &[crate::entry::Entry]) {
        let Some(t) = &self.taint else { return };
        t.reseed(
            entries
                .iter()
                .filter_map(|e| e.payload.get("tool")?.as_str()),
        );
    }

    /// Attach opt-in tool-boundary hooks (issue #110). Default OFF.
    #[cfg(feature = "shell-tools")]
    pub fn set_hooks(&mut self, hooks: crate::hooks::ToolHooks) {
        self.hooks = Some(hooks);
    }

    /// Pre-hook pass: returns the deny reason when any pre-hook exits 2.
    /// Hook FAILURES (crash, timeout, non-2 exit) are logged and
    /// non-blocking by contract.
    #[cfg(feature = "shell-tools")]
    pub(crate) fn pre_hook_denial(&self, tool: &str, input: &Value) -> Option<String> {
        let hooks = self.hooks.as_ref()?;
        for h in &hooks.pre {
            match h.run("pretool", tool, input, None) {
                Ok(Some(reason)) => return Some(reason),
                Ok(None) => {}
                Err(e) => eprintln!("tole: pre-hook failure (non-blocking): {e}"),
            }
        }
        None
    }

    /// True when at least one post-hook is wired (callers skip input
    /// cloning otherwise).
    #[cfg(feature = "shell-tools")]
    pub(crate) fn has_post_hooks(&self) -> bool {
        self.hooks
            .as_ref()
            .map(|h| !h.post.is_empty())
            .unwrap_or(false)
    }

    /// Post-hook pass (observe-only): failures are logged, never block.
    #[cfg(feature = "shell-tools")]
    pub(crate) fn post_hook_notify(&self, tool: &str, input: &Value, ok: bool) {
        let Some(hooks) = &self.hooks else { return };
        for h in &hooks.post {
            if let Err(e) = h.run("posttool", tool, input, Some(ok)) {
                eprintln!("tole: post-hook failure (non-blocking): {e}");
            }
        }
    }

    /// True when at least one turn-end gate is wired.
    #[cfg(feature = "shell-tools")]
    pub(crate) fn has_turnend_hooks(&self) -> bool {
        self.hooks
            .as_ref()
            .map(|h| !h.turnend.is_empty())
            .unwrap_or(false)
    }

    /// Turn-end gate pass (issue #145). `tools` is the per-turn tool/risk
    /// summary the payload carries. Returns the FIRST deny reason (exit
    /// 2); hook failures are logged and non-blocking per the #110
    /// contract. Stop after the first deny: the model fixes one complaint
    /// at a time — re-running all gates on every denial would burn the
    /// step budget re-reporting the same failures.
    #[cfg(feature = "shell-tools")]
    pub(crate) fn turnend_denial(
        &self,
        final_preview: &str,
        tools: &[(String, crate::tool::Risk)],
    ) -> Option<String> {
        let hooks = self.hooks.as_ref()?;
        let payload = serde_json::json!({
            "event": "turnend",
            "final_text_preview": &final_preview.chars().take(8192).collect::<String>(),
            "tools": tools
                .iter()
                .map(|(t, r)| {
                    let risk = match r {
                        Risk::ReadOnly => "readonly",
                        Risk::Write => "write",
                        Risk::Destructive => "destructive",
                    };
                    serde_json::json!({"tool": t, "risk": risk})
                })
                .collect::<Vec<_>>(),
        });
        for h in &hooks.turnend {
            // GATE semantics (issue #145): exit 0 = pass; ANY non-zero
            // exit denies, with stdout (or a default) as the reason the
            // model will see. A verification gate's nonzero exit is its
            // verdict ("tests failed" exits 1, cargo exits 101) — the
            // #110 exit-2-only rule would classify real verdicts as
            // non-blocking failures, which the first live E2E disproved.
            match h.run_raw(&payload) {
                Ok((0, _)) => {}
                Ok((_, stdout)) => {
                    let reason = if stdout.is_empty() {
                        "denied by turnend gate (no reason given)".to_string()
                    } else {
                        format!("denied by turnend gate: {stdout}")
                    };
                    return Some(reason);
                }
                Err(e) => {
                    // True INFRASTRUCTURE failures (spawn error, timeout)
                    // stay non-blocking: a broken gate must not hold the
                    // agent hostage (same contract as #110 hook failures).
                    eprintln!("tole: turnend-hook failure (non-blocking): {e}");
                }
            }
        }
        None
    }

    /// Register a tool. Duplicate names are a wiring bug and are refused
    /// (no panics in core). Write tools require an approver; Destructive
    /// tools require an *interactive* approver (structural enforcement —
    /// a config pattern can never silently opt them in).
    pub fn register(&mut self, tool: Box<dyn Tool>) -> Result<(), String> {
        let name = tool.name().to_string();
        if self.tools.contains_key(&name) {
            return Err(format!("tool {name} already registered"));
        }
        match tool.risk() {
            Risk::ReadOnly => {}
            Risk::Write => {
                if self.approver.is_none() {
                    return Err(format!(
                        "tool {name} is Write-risk; refusing registration without an approval gate"
                    ));
                }
            }
            Risk::Destructive => {
                let Some(a) = &self.approver else {
                    return Err(format!(
                        "tool {name} is Destructive-risk; refusing registration without an approval gate"
                    ));
                };
                if !a.interactive() {
                    return Err(format!(
                        "tool {name} is Destructive-risk; cannot be registered behind a \
                         non-interactive approver (Destructive is never allowlistable)"
                    ));
                }
            }
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|b| b.as_ref())
    }

    /// Plan-mode filter (issue #109): drop every non-ReadOnly tool from
    /// the registry. The guarantee is ABSENCE, not approval — filtered
    /// tools never appear in `specs()`, so the model cannot even see
    /// them on the wire. Irreversible by design: a registry filtered
    /// this way cannot grow Write tools back (no re-register path).
    /// Read-only in-place mutation of the registry is safe because
    /// callers build registries fresh per session.
    pub fn retain_read_only(&mut self) {
        self.tools.retain(|_, t| t.risk() == Risk::ReadOnly);
    }

    /// Plan mode for a discussion participant that its owner allowed some
    /// connected tools (#1039): [`retain_read_only`](Self::retain_read_only),
    /// plus the tools in `keep` that are `Write` (never `Destructive`).
    /// Naming a tool keeps it registered; every call of it still goes through
    /// the approver.
    pub fn retain_read_only_and(&mut self, keep: &[String]) {
        self.tools.retain(|name, t| {
            t.risk() == Risk::ReadOnly || (t.risk() == Risk::Write && keep.contains(name))
        });
    }

    /// The highest risk among the registered tools (`ReadOnly` when
    /// empty): the model router's (#586) view of what a run can touch.
    pub fn max_risk(&self) -> Risk {
        let rank = |r: Risk| match r {
            Risk::ReadOnly => 0,
            Risk::Write => 1,
            Risk::Destructive => 2,
        };
        self.tools
            .values()
            .map(|t| t.risk())
            .max_by_key(|r| rank(*r))
            .unwrap_or(Risk::ReadOnly)
    }

    /// OpenAI-format `tools` array for every registered tool (E4.5).
    /// Sorted by name so the wire payload is deterministic — the same
    /// registry always serializes to the same request body.
    pub fn specs(&self) -> Vec<Value> {
        let mut names: Vec<&String> = self.tools.keys().collect();
        names.sort();
        names
            .into_iter()
            .map(|n| {
                let t = &self.tools[n];
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name(),
                        "description": t.summary(),
                        "parameters": t.spec().unwrap_or_else(|| json!({
                            "type": "object",
                            "properties": {},
                        })),
                    },
                })
            })
            .collect()
    }

    /// Per-call approval verdict for a non-ReadOnly tool. `None` when no
    /// approver is wired (callers treat that as Deny). The tool must be
    /// registered — the loop calls this only after a successful lookup.
    pub fn decide(&self, name: &str, input: &Value) -> Option<Verdict> {
        let t = self.tools.get(name)?;
        // Single fan-out point (#327): neutralize control/bidi/separator
        // chars before any approval surface sees the description.
        let description = crate::sanitize::sanitize_one_line(&t.describe(input));
        // A tool with no summary of its own keeps what it said about itself
        // (the resolved repo, the exact argv, ...): never less than `describe`.
        let summary = t
            .action_summary(input)
            .unwrap_or_else(|| ActionSummary::unknown_with_description(name, input, &description))
            .finalized(&crate::secrets::process_secrets());
        let req = ToolRequest {
            tool: name,
            risk: t.risk(),
            input,
            description,
            target: t.approval_target(input),
            summary: Some(summary),
        };
        self.approver.as_ref().map(|a| a.decide(&req))
    }

    /// True when an approver is wired in.
    pub fn has_approver(&self) -> bool {
        self.approver.is_some()
    }
}
