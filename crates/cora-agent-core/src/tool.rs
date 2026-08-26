//! Tool trait, risk classification, and registry (§7).
//!
//! The approval gate itself lands in E6; in Phase 1 the turn loop
//! auto-executes `ReadOnly` tools only and settles everything else as a
//! replan-worthy error.

use serde_json::Value;
use std::collections::HashMap;

/// What a tool is allowed to touch. Drives the approval gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    /// Pure read: runs without approval; replay-safe.
    ReadOnly,
    /// Mutates state: requires approval unless allowlisted (E6).
    Write,
    /// Irreversible: always requires approval (E6).
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

/// Name → tool lookup for the turn loop.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool. Duplicate names are a wiring bug and are refused
    /// (no panics in core).
    pub fn register(&mut self, tool: Box<dyn Tool>) -> Result<(), String> {
        let name = tool.name().to_string();
        if self.tools.contains_key(&name) {
            return Err(format!("tool {name} already registered"));
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|b| b.as_ref())
    }
}
