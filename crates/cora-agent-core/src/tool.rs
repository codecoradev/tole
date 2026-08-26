//! Tool trait, risk classification, registry, and the approval gate
//! wiring (§7, E4).
//!
//! ReadOnly tools auto-execute in the loop. Write/Destructive tools can
//! only be registered when an approver is wired in; at call time the
//! approver decides Allow/Deny per invocation (E6 wires the interactive
//! prompt; core stays deterministic).

use crate::approval::{AllowlistApprover, Verdict};
use serde_json::Value;
use std::collections::HashMap;

/// What a tool is allowed to touch. Drives the approval gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    /// Pure read: runs without approval; replay-safe.
    ReadOnly,
    /// Mutates state: requires approval unless allowlisted.
    Write,
    /// Irreversible: always requires approval (allowlist may opt in).
    Destructive,
}

/// A capability the agent may call. Phase 1: synchronous, in-process.
pub trait Tool {
    /// Stable name the provider refers to in a `ToolCall`.
    fn name(&self) -> &str;
    /// Risk classification; the loop auto-executes `ReadOnly` only.
    fn risk(&self) -> Risk;
    /// Execute with JSON input, return JSON output or a plain error.
    fn execute(&self, input: Value) -> Result<Value, String>;
}

/// Name → tool lookup for the turn loop, with approval gating.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
    approver: Option<AllowlistApprover>,
}

impl ToolRegistry {
    /// Empty registry with no approver: **ReadOnly tools only**.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registry with an approval gate wired in: Write/Destructive tools
    /// become registrable, subject to per-call decisions.
    pub fn with_approver(approver: AllowlistApprover) -> Self {
        Self {
            tools: HashMap::new(),
            approver: Some(approver),
        }
    }

    /// Register a tool. Duplicate names are a wiring bug and are refused
    /// (no panics in core). Write/Destructive tools require the approver.
    pub fn register(&mut self, tool: Box<dyn Tool>) -> Result<(), String> {
        let name = tool.name().to_string();
        if self.tools.contains_key(&name) {
            return Err(format!("tool {name} already registered"));
        }
        match tool.risk() {
            Risk::ReadOnly => {}
            Risk::Write | Risk::Destructive => {
                let Some(approver) = &self.approver else {
                    return Err(format!(
                        "tool {name} is {}-risk; refusing registration without an approval gate",
                        if tool.risk() == Risk::Write {
                            "Write"
                        } else {
                            "Destructive"
                        }
                    ));
                };
                let _ = approver;
            }
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|b| b.as_ref())
    }

    /// Per-call approval decision for a Write/Destructive tool.
    pub fn decide(&self, name: &str) -> Option<Verdict> {
        self.approver.as_ref().map(|a| a.decide(name))
    }

    /// True when an approver is wired in.
    pub fn has_approver(&self) -> bool {
        self.approver.is_some()
    }
}
