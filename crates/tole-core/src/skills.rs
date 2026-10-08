//! Skill support (issue #161): tole reads and uses SKILL.md files —
//! the fleet-standard agent-skill format (YAML frontmatter `name` +
//! `description`, markdown body).
//!
//! Resolution order (first match wins), validated against the CodeCora
//! data convention (cora-code `data_dir.rs`):
//! 1. `<workspace>/skills/<name>/SKILL.md` — project-level
//! 2. `$CODECORA_HOME/tole/skills/<name>/SKILL.md` →
//!    `~/.codecora/tole/skills/` — user-global
//!
//! A skill body is capped at [`MAX_SKILL_CHARS`] before it enters the
//! context (skills are instructions, not documents); the index shown to
//! the model is one line per skill (`name — description`).

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Hard cap on a skill body entering the context.
pub const MAX_SKILL_CHARS: usize = 16 * 1024;

/// One discovered skill.
#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
}

impl Skill {
    /// The system-prompt index line: `name — description`.
    pub fn index_line(&self) -> String {
        format!("{} — {}", self.name, self.description)
    }
}

/// Parse a SKILL.md: YAML frontmatter (`name`, `description`) + markdown
/// body. Errors loudly on missing frontmatter or fields — a broken skill
/// must be visible, not silently skipped.
pub fn parse_skill(content: &str, fallback_name: &str) -> Result<Skill, String> {
    let rest = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
        .ok_or_else(|| "missing frontmatter (must start with ---)".to_string())?;
    let end = rest
        .find("\n---")
        .ok_or_else(|| "unterminated frontmatter".to_string())?;
    let fm = &rest[..end];
    let body = rest[end + 4..].trim_start().to_string();

    let mut name = None;
    let mut description = None;
    for line in fm.lines() {
        if let Some(v) = line.strip_prefix("name:") {
            name = Some(trim_quote(v));
        } else if let Some(v) = line.strip_prefix("description:") {
            description = Some(trim_quote(v));
        }
    }
    let name = name
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "frontmatter missing `name`".to_string())?;
    let description = description.unwrap_or_default();
    if name != fallback_name {
        return Err(format!(
            "frontmatter name {name:?} does not match directory {fallback_name:?}"
        ));
    }
    Ok(Skill {
        name,
        description,
        body,
    })
}

fn trim_quote(v: &str) -> String {
    let t = v.trim();
    t.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(t)
        .to_string()
}

/// Candidate directories for `<name>/SKILL.md`, in resolution order:
/// project-level first, then the user-global CodeCora data dir
/// (`$CODECORA_HOME/tole/skills/` → `~/.codecora/tole/skills/`).
fn candidate_dirs(workspace: Option<&Path>, name: &str) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(ws) = workspace {
        dirs.push(ws.join("skills").join(name));
    }
    let home = std::env::var("CODECORA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            // std-only home resolution (the `dirs` crate would be a new
            // dep for one call): HOME on unix, USERPROFILE on windows.
            std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".codecora")
        });
    dirs.push(home.join("tole").join("skills").join(name));
    dirs
}

/// Load one skill by name. Errors list every path tried — a missing
/// skill must say where it looked.
pub fn load_skill(workspace: Option<&Path>, name: &str) -> Result<Skill, String> {
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err("invalid skill name".into());
    }
    let mut tried = Vec::new();
    for dir in candidate_dirs(workspace, name) {
        let path = dir.join("SKILL.md");
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let mut skill = parse_skill(&content, name)?;
                truncate_to_char_boundary(&mut skill.body, MAX_SKILL_CHARS);
                return Ok(skill);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tried.push(path.display().to_string());
            }
            Err(e) => tried.push(format!("{}: {e}", path.display())),
        }
    }
    Err(format!(
        "skill {name:?} not found — tried: {}",
        tried.join(", ")
    ))
}

fn truncate_to_char_boundary(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s.push('…');
}

/// List available skills: union of project + user-global dirs, project
/// wins on name conflicts. Sorted by name for a stable index.
pub fn available_skills(workspace: Option<&Path>) -> Vec<Skill> {
    let mut names: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in candidate_dirs(workspace, "").iter() {
        // candidate_dirs(workspace, "") already resolves to the skills
        // ROOT (`<ws>/skills/` / `~/.codecora/tole/skills/`) because an
        // empty name joins as a no-op.
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let skill_md = entry.path().join("SKILL.md");
            if !skill_md.is_file() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if seen.insert(name.clone()) {
                names.push(name);
            }
        }
    }
    names.sort();
    names
        .iter()
        .filter_map(|n| load_skill(workspace, n).ok())
        .collect()
}

/// The one-line index block for the system prompt. Empty → empty string
/// (callers skip appending).
pub fn index_block(workspace: Option<&Path>) -> String {
    let skills = available_skills(workspace);
    if skills.is_empty() {
        return String::new();
    }
    let mut out =
        String::from("\n\n# Skills available\nCall the load_skill tool with a name to load one:\n");
    for s in &skills {
        out.push_str(&format!("- {}\n", s.index_line()));
    }
    out
}

/// The ReadOnly `load_skill` tool.
#[derive(Debug, Clone, Default)]
pub struct LoadSkillTool {
    pub workspace: Option<PathBuf>,
}

impl LoadSkillTool {
    pub fn new(workspace: Option<PathBuf>) -> Self {
        Self { workspace }
    }
}

impl crate::tool::Tool for LoadSkillTool {
    fn name(&self) -> &str {
        "load_skill"
    }

    fn risk(&self) -> crate::tool::Risk {
        crate::tool::Risk::ReadOnly
    }

    fn summary(&self) -> String {
        "Load the full instructions of a named skill from the skills index.".into()
    }

    fn describe(&self, input: &Value) -> String {
        let name = input.get("name").and_then(Value::as_str).unwrap_or("?");
        format!("load skill {name:?}")
    }

    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Skill name (from the skills index)" }
            },
            "required": ["name"]
        }))
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        let name = input
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| "input must be {\"name\": \"...\"}".to_string())?;
        let skill = load_skill(self.workspace.as_deref(), name)?;
        Ok(json!({
            "name": skill.name,
            "description": skill.description,
            "body": skill.body,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::Tool as _;
    use serde_json::json;

    fn write_skill(dir: &Path, name: &str, fm_name: &str, desc: &str, body: &str) {
        let d = dir.join("skills").join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {fm_name}\ndescription: \"{desc}\"\n---\n{body}"),
        )
        .unwrap();
    }

    #[test]
    fn parses_frontmatter_and_body() {
        let s = parse_skill(
            "---\nname: demo\ndescription: \"a demo skill\"\n---\nBody here.",
            "demo",
        )
        .unwrap();
        assert_eq!(s.name, "demo");
        assert_eq!(s.description, "a demo skill");
        assert_eq!(s.body, "Body here.");
        assert_eq!(s.index_line(), "demo — a demo skill");
    }

    #[test]
    fn broken_skills_fail_loudly() {
        assert!(parse_skill("no frontmatter", "x").is_err());
        assert!(parse_skill("---\nunterminated", "x").is_err());
        assert!(parse_skill("---\ndescription: \"d\"\n---\nb", "x").is_err());
        assert!(parse_skill("---\nname: other\n---\nb", "x").is_err());
    }

    #[test]
    fn project_beats_user_and_codecora_home_is_honored() {
        let tmp = std::env::temp_dir().join(format!("tole-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let ws = tmp.join("ws");
        let user = tmp.join("userhome").join(".codecora").join("tole");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&user).unwrap();

        // SAFETY: serial test (cargo runs lib tests on one thread per
        // binary by default only when not using threads; this env var is
        // read per-call, so we accept the race in exchange for coverage —
        // the other tests do not read CODECORA_HOME).
        std::env::set_var("CODECORA_HOME", user.parent().unwrap().parent().unwrap());
        write_skill(
            user.parent().unwrap().parent().unwrap(),
            "demo",
            "demo",
            "user-global",
            "user body",
        );
        write_skill(&ws, "demo", "demo", "project", "project body");

        let s = load_skill(Some(&ws), "demo").unwrap();
        assert_eq!(s.description, "project", "project must win");

        // user-global found when project lacks it
        write_skill(&ws, "other", "other", "only in project", "x");
        let s2 = load_skill(Some(&ws), "demo").unwrap();
        assert_eq!(s2.body, "project body");
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::remove_var("CODECORA_HOME");
    }

    #[test]
    fn load_skill_tool_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("tole-skills-tool-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        write_skill(&tmp, "demo", "demo", "d", "b");
        let tool = LoadSkillTool::new(Some(tmp.clone()));
        let out = tool.execute(json!({"name": "demo"})).unwrap();
        assert_eq!(out["name"], json!("demo"));
        assert_eq!(out["body"], json!("b"));
        let err = tool.execute(json!({"name": "missing"})).unwrap_err();
        assert!(err.contains("tried"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
