//! Tool trait, risk classification, registry, and the approval gate
//! wiring (§7, E4/E6).
//!
//! ReadOnly tools auto-execute in the loop. Write tools require an
//! approver wired in; Destructive tools additionally require an
//! *interactive* approver (a human must be reachable — PRD: Destructive
//! cannot be allowlisted). At call time the approver decides per
//! invocation.

use crate::approval::{Approver, ToolRequest, Verdict};
use serde_json::{json, Value};
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
    /// One-line human summary of what this call will do, shown in
    /// approval prompts (E6). Default: tool name + risk.
    fn describe(&self, _input: &Value) -> String {
        format!("{} ({})", self.name(), self.risk().as_str())
    }
    /// JSON Schema for the input object, sent to the provider as this
    /// tool's `parameters` (E4.5). `None` (the default) means "object
    /// with no declared properties" — still callable, just untyped.
    fn spec(&self) -> Option<Value> {
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
        }
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
                        "description": t.describe(&serde_json::Value::Null),
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
        let req = ToolRequest {
            tool: name,
            risk: t.risk(),
            input,
            description: t.describe(input),
        };
        self.approver.as_ref().map(|a| a.decide(&req))
    }

    /// True when an approver is wired in.
    pub fn has_approver(&self) -> bool {
        self.approver.is_some()
    }
}
