//! `skill_propose` (#592 part 2, design `skills-registry-design.md` §4.4): the
//! one way an agent grows the skill library, and it only ever writes a
//! **draft**.
//!
//! The host decides everything that grants trust. The agent supplies the
//! SKILL.md, why it proposes it and, optionally, extra source session ids.
//! The host adds the session id, the model, the taint (what this session
//! fetched from outside) and the meaning dedup against the index; none of
//! those can be set from the tool input. The draft lands in the drafts
//! directory, which no skill loader reads, and waits for the owner
//! (`tole skills drafts`). A Write tool, so each call is approved like any
//! other.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};
use tole_core::secrets::SecretSet;
use tole_core::skill_draft::{
    gate, Dedup, DraftStore, Finding, HostContext, Proposal, Severity, TaintTracker, Trigger,
};
use tole_core::skill_index::{SkillFilter, SkillIndex};
use tole_core::skills::parse_skill;
use tole_core::tool::{Risk, Tool};

/// What the agent may say about a draft. Anything else is refused by name.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    name: String,
    skill_md: String,
    trigger: String,
    #[serde(default)]
    sources: Vec<String>,
}

/// The `skill_propose` tool of one session.
pub struct SkillProposeTool {
    /// The drafts dir (`None`: the default store at call time).
    store: Option<DraftStore>,
    /// What this session consumed from outside; shared with the registry.
    taint: TaintTracker,
    /// The host's own session id, when the face knows it.
    session: Option<String>,
    model: Option<String>,
    secrets: SecretSet,
    /// The index for meaning dedup; only a `uteke` index scores by cosine.
    index: Option<Arc<dyn SkillIndex>>,
    /// The curator state file (`None`: the default at call time).
    curator_state: Option<std::path::PathBuf>,
}

impl SkillProposeTool {
    pub fn new(
        store: Option<DraftStore>,
        taint: TaintTracker,
        session: Option<String>,
        model: Option<String>,
        secrets: SecretSet,
        index: Option<Arc<dyn SkillIndex>>,
    ) -> Self {
        Self {
            store,
            taint,
            session,
            model,
            secrets,
            index,
            curator_state: None,
        }
    }

    /// Read the locked skills from `path` instead of the default state file.
    pub fn with_curator_state(mut self, path: std::path::PathBuf) -> Self {
        self.curator_state = Some(path);
        self
    }

    /// The nearest existing skill and a note when meaning dedup did not run.
    fn dedup(&self, name: &str, skill_md: &str) -> (Dedup, Option<String>) {
        let Some(index) = self.index.as_ref().filter(|i| i.label() == "uteke") else {
            return (
                Dedup::from_nearest(None),
                Some(
                    "meaning dedup did not run: no uteke skill index (TOLE_SKILL_INDEX=uteke)"
                        .into(),
                ),
            );
        };
        let text = match parse_skill(skill_md, name) {
            Ok(s) => format!("{name} {}", s.description),
            Err(_) => name.to_string(),
        };
        let query = match tole_core::skill_index::clean_query(&text) {
            Ok(q) => self.secrets.redact(&q).into_owned(),
            Err(e) => {
                return (
                    Dedup::from_nearest(None),
                    Some(format!("dedup skipped: {e}")),
                )
            }
        };
        let filter = SkillFilter {
            profile: None,
            floor: 0.0,
        };
        match index.search(&query, &filter, 1) {
            Ok(mut hits) => (
                Dedup::from_nearest(hits.pop().map(|c| (c.name, c.score))),
                None,
            ),
            Err(e) => (
                Dedup::from_nearest(None),
                Some(format!("meaning dedup did not run: {e}")),
            ),
        }
    }
}

fn refusal(findings: &[Finding]) -> String {
    let lines: Vec<String> = findings
        .iter()
        .filter(|f| f.severity == Severity::Block)
        .map(|f| format!("- {}: {}", f.rule, f.detail))
        .collect();
    format!(
        "skill_propose refused, nothing was stored:\n{}",
        lines.join("\n")
    )
}

impl Tool for SkillProposeTool {
    fn name(&self) -> &str {
        "skill_propose"
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn summary(&self) -> String {
        "Propose a new or improved skill as a DRAFT for the owner. Never loadable until the \
         owner reviews and promotes it. Use it after a correction, a dead end that found a \
         working path, or a workflow repeated across tasks. One procedure per skill, rules not \
         an incident log, no secrets."
            .into()
    }

    fn describe(&self, input: &Value) -> String {
        let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("?");
        format!(
            "save a skill draft `{}` (trigger {}) for owner review",
            s("name"),
            s("trigger")
        )
    }

    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "lowercase letters, digits and '-', up to 64"},
                "skill_md": {"type": "string", "description": "the whole SKILL.md: frontmatter with name and description, then the procedure"},
                "trigger": {"type": "string", "enum": ["owner_correction", "dead_end", "repeated_workflow"]},
                "sources": {"type": "array", "items": {"type": "string"}, "description": "other session ids the lesson came from"}
            },
            "required": ["name", "skill_md", "trigger"],
            "additionalProperties": false
        }))
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        let i: Input = serde_json::from_value(input).map_err(|e| format!("skill_propose: {e}"))?;
        let trigger = Trigger::parse(&i.trigger).ok_or_else(|| {
            format!(
                "skill_propose: trigger {:?}: expected owner_correction, dead_end or \
                 repeated_workflow",
                i.trigger
            )
        })?;
        let store = match &self.store {
            Some(s) => s.clone(),
            None => DraftStore::default_store()
                .ok_or("skill_propose: the CodeCora home is unresolvable")?,
        };
        // A locked skill (`tole curator lock`) takes no agent proposals.
        if let Some(path) = self
            .curator_state
            .clone()
            .or_else(tole_core::skill_curator::CuratorState::default_path)
        {
            let state = tole_core::skill_curator::CuratorState::load(&path)
                .map_err(|e| format!("skill_propose: {e}"))?;
            if state.locked.contains(&i.name) {
                return Err(format!(
                    "skill_propose: skill {:?} is locked by the owner; no agent proposals",
                    i.name
                ));
            }
        }
        let (dedup, note) = self.dedup(&i.name, &i.skill_md);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let ctx = HostContext {
            session: self.session.clone(),
            model: self.model.clone(),
            taint: self.taint.sources(),
            dedup,
            now,
        };
        let proposal = Proposal {
            name: i.name,
            skill_md: i.skill_md,
            trigger,
            sources: i.sources,
        };
        let (draft, skill_md) = gate(&proposal, &ctx, &self.secrets).map_err(|f| refusal(&f))?;
        store.create(&draft, &skill_md)?;
        Ok(json!({
            "draft": draft.id,
            "status": "pending",
            "tainted": draft.tainted(),
            "taint": draft.taint,
            "nearest": draft.dedup.nearest,
            "score": draft.dedup.score,
            "band": draft.dedup.band,
            "warnings": draft.warnings.iter().map(|w| format!("{}: {}", w.rule, w.detail)).collect::<Vec<_>>(),
            "dedup_note": note,
            "next": "The owner reviews it with `tole skills drafts`. It cannot be loaded until then.",
        }))
    }
}

#[cfg(test)]
#[path = "skill_propose_tests.rs"]
mod tests;
