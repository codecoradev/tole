//! `cron_create` (#581 part 2): the one way an agent creates a cron job.
//!
//! The host decides everything that grants power. The job is saved paused,
//! with `created_by = agent`, the creator's grant (`creator_allow`: the
//! session's own allow list) and the session's workspace as its workdir. The
//! agent picks the schedule, the prompt and an `allow` within its own. The
//! tool cannot enable the job, set a script, skills, a profile or a
//! `command:` delivery: a human reviews the job and enables it with
//! `tole cron resume`. A Write tool, so each call is approved like any
//! other.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{json, Value};
use tole_core::tool::{Risk, Tool};

use super::CreateSpec;

/// What the agent may say about a job. Anything else is refused by name.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    schedule: String,
    prompt: String,
    name: Option<String>,
    #[serde(default)]
    allow: Vec<String>,
    model: Option<String>,
    max_steps: Option<u64>,
    max_minutes: Option<u64>,
    max_tokens: Option<u64>,
    #[serde(default)]
    deliver: Vec<String>,
}

/// The `cron_create` tool of one session.
pub struct CronCreateTool {
    /// The cron dir (`None`: [`super::default_dir`] at call time).
    dir: Option<PathBuf>,
    /// The session's workspace: the job's workdir.
    workdir: PathBuf,
    /// The session's allow list: the most a created job may hold.
    creator_allow: Vec<String>,
}

impl CronCreateTool {
    pub fn new(dir: Option<PathBuf>, workdir: PathBuf, creator_allow: Vec<String>) -> Self {
        Self {
            dir,
            workdir,
            creator_allow,
        }
    }
}

impl Tool for CronCreateTool {
    fn name(&self) -> &str {
        "cron_create"
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn summary(&self) -> String {
        "Save a scheduled agent job in this workspace. It is saved paused; the owner reviews \
         it and enables it with `tole cron resume`. Its allow list must be within this \
         session's."
            .into()
    }

    fn describe(&self, input: &Value) -> String {
        let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("?");
        format!(
            "save a paused cron job `{}` ({}) with allow {}: {:?}",
            s("schedule"),
            input
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unnamed"),
            input.get("allow").unwrap_or(&json!([])),
            s("prompt")
        )
    }

    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "schedule": {"type": "string", "description": "5-field cron in UTC (`0 9 * * 1-5`), a macro (`@daily`) or `@every 30m`"},
                "prompt": {"type": "string", "description": "What each run does (re-sent verbatim every run)"},
                "name": {"type": "string"},
                "allow": {"type": "array", "items": {"type": "string"}, "description": "Write pre-authorization; only patterns this session holds"},
                "model": {"type": "string"},
                "max_steps": {"type": "integer", "minimum": 1},
                "max_minutes": {"type": "integer", "minimum": 1},
                "max_tokens": {"type": "integer", "minimum": 1},
                "deliver": {"type": "array", "items": {"type": "string"}, "description": "`session` (default), `log` or `file:PATH` inside the workspace"}
            },
            "required": ["schedule", "prompt"],
            "additionalProperties": false
        }))
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        let i: Input = serde_json::from_value(input).map_err(|e| format!("cron_create: {e}"))?;
        let dir = match &self.dir {
            Some(d) => d.clone(),
            None => super::default_dir().map_err(|e| e.to_string())?,
        };
        let spec = CreateSpec {
            schedule: i.schedule,
            prompt: Some(i.prompt),
            name: i.name,
            workdir: self.workdir.clone(),
            allow: i.allow,
            model: i.model,
            max_steps: i.max_steps,
            max_minutes: i.max_minutes,
            max_tokens: i.max_tokens,
            deliver: i.deliver,
            ..CreateSpec::default()
        };
        let job = super::create_by_agent(&dir, spec, &self.creator_allow, super::now())
            .map_err(|e| format!("cron_create: {e}"))?;
        Ok(json!({
            "id": job.id,
            "paused": true,
            "allow": job.extra.get("allow").cloned().unwrap_or(json!([])),
            "note": format!(
                "saved paused; it runs only after the owner reviews it and runs `tole cron resume {}`",
                job.id
            ),
        }))
    }
}
