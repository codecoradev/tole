//! `tole audit [GOAL] [--json]` (#585, design `profiles-collaboration-
//! secrets.md` A8.2): the delegation tree of this workspace, read-only.
//!
//! It reads ONE file, the delegation log in the tole data dir (outside every
//! workspace, never child-writable), opened without the host lock and never
//! repaired, so it writes nothing. It reads nothing from the children's agent
//! dirs: the reports it shows are the ones the parent recorded in the log.
//! No network.
//!
//! What it prints is bounded and redacted: the task is one capped line
//! (never the full prompt), report summaries and reasons are capped, control
//! characters are dropped, and every string passes through the process
//! `SecretSet` before it is printed.

use std::path::Path;

use serde_json::{json, Value};
use tole_core::delegation::{Delegation, Mode, Node, NodeStatus};
use tole_core::secrets::SecretSet;

use crate::ledger_file::FileStore;

/// Longest task line shown, in characters.
pub const TASK_CHARS: usize = 120;
/// Longest report summary or reason shown, in characters.
pub const TEXT_CHARS: usize = 400;

/// One line for display: secrets redacted FIRST (on the raw text, so a
/// secret with whitespace or control characters, or one that would straddle
/// the cut, is matched whole), then control characters dropped, whitespace
/// collapsed and the result cut at `max` characters. A cut never splits a
/// `«NAME»` marker: a marker the cut would split is dropped whole.
fn line(secrets: &SecretSet, s: &str, max: usize) -> String {
    let redacted = secrets.redact(s);
    let clean: String = redacted
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    if clean.chars().count() <= max {
        return clean;
    }
    let mut cut: String = clean.chars().take(max).collect();
    // An opening marker with no closing one before the cut: drop it whole.
    if let Some(open) = cut.rfind('«') {
        if !cut[open..].contains('»') {
            cut.truncate(open);
        }
    }
    cut.push('…');
    cut
}

fn status(s: NodeStatus) -> &'static str {
    match s {
        NodeStatus::Queued => "queued",
        NodeStatus::Running => "running",
        NodeStatus::Done => "done",
        NodeStatus::Incomplete => "incomplete",
        NodeStatus::Failed => "failed",
        NodeStatus::Cancelled => "cancelled",
        NodeStatus::Budget => "budget",
        NodeStatus::Interrupted => "interrupted",
    }
}

fn node_json(d: &Delegation<FileStore>, n: &Node, secrets: &SecretSet) -> Value {
    let charged = d.charged(&n.id);
    json!({
        "node": n.id,
        "parent": n.parent,
        "depth": n.depth,
        "profile": n.profile.as_deref().filter(|p| *p != "-"),
        "mode": match n.mode { Mode::Consult => "consult", Mode::Execute => "execute" },
        "status": status(n.status),
        "task": line(secrets, &n.task, TASK_CHARS),
        "budget": {"tokens": n.budget.tokens, "steps": n.budget.steps, "deadline": n.budget.deadline},
        "used": n.used.map(|u| json!({"tokens": u.tokens, "steps": u.steps})),
        "charged": {"tokens": charged.tokens, "steps": charged.steps},
        "allow": n.allow,
        "dropped": n.dropped,
        "workspace": n.workspace,
        "leases": d.leases_of(&n.id),
        "session": n.session,
        "report": n.report.as_ref().map(|r| json!({
            "summary": line(secrets, &r.summary, TEXT_CHARS),
            "files": r.files.len(),
            "checks": r.checks.len(),
            "risks": r.risks.len(),
        })),
        "reason": n.reason.as_deref().map(|r| line(secrets, r, TEXT_CHARS)),
    })
}

fn text(nodes: &[Value]) -> String {
    let mut out = String::new();
    for v in nodes {
        let pad = "  ".repeat(v["depth"].as_u64().unwrap_or(0) as usize);
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        // A live node reserves its whole carve; a settled one shows what it
        // was charged against it.
        let spend = if matches!(s("status").as_str(), "running" | "queued") {
            format!(
                "carve tokens {} steps {}",
                v["budget"]["tokens"], v["budget"]["steps"]
            )
        } else {
            format!(
                "charged tokens {}/{} steps {}/{}",
                v["charged"]["tokens"],
                v["budget"]["tokens"],
                v["charged"]["steps"],
                v["budget"]["steps"]
            )
        };
        out.push_str(&format!(
            "{pad}{} {} {} [{}] {spend}\n",
            s("node"),
            match (v["depth"].as_u64(), v["profile"].as_str()) {
                (Some(0), _) => "orchestrator",
                (_, Some(p)) => p,
                _ => "(no profile)",
            },
            s("status"),
            s("mode"),
        ));
        let detail = |out: &mut String, label: &str, val: String| {
            if !val.is_empty() {
                out.push_str(&format!("{pad}    {label}: {val}\n"));
            }
        };
        detail(&mut out, "task", s("task"));
        let list = |k: &str| {
            v[k].as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default()
        };
        detail(&mut out, "allow", list("allow"));
        detail(&mut out, "dropped", list("dropped"));
        detail(&mut out, "leases", list("leases"));
        if let Some(r) = v["report"].as_object() {
            detail(
                &mut out,
                "report",
                format!(
                    "{} ({} files, {} checks, {} risks)",
                    r["summary"].as_str().unwrap_or(""),
                    r["files"],
                    r["checks"],
                    r["risks"]
                ),
            );
        }
        detail(&mut out, "reason", s("reason"));
    }
    out
}

/// The audit of the log at `log`: the goals it holds, or the tree of `goal`,
/// as text or JSON, every string redacted through `secrets`.
pub fn audit(
    log: &Path,
    goal: Option<&str>,
    as_json: bool,
    secrets: &SecretSet,
) -> Result<String, String> {
    let d = Delegation::open(FileStore::new(log.to_path_buf())).map_err(|e| e.to_string())?;
    let value = match goal {
        None => {
            let goals: Vec<Value> = d
                .goals()
                .into_iter()
                .map(|g| {
                    let mut v = node_json(&d, g, secrets);
                    v["nodes"] = json!(d.tree(&g.id).len());
                    v
                })
                .collect();
            json!({ "log": log, "goals": goals })
        }
        Some(goal) => {
            if d.poll(goal).is_none_or(|n| n.parent.is_some()) {
                return Err(format!("no goal `{goal}` in {}", log.display()));
            }
            let nodes: Vec<Value> = d
                .tree(goal)
                .into_iter()
                .map(|n| node_json(&d, n, secrets))
                .collect();
            json!({ "log": log, "goal": goal, "nodes": nodes })
        }
    };
    // Second pass over everything (ids, paths, allow patterns): the capped
    // fields were already redacted before they were cut.
    let value = secrets.redact_value(&value);
    if as_json {
        return Ok(format!(
            "{}\n",
            serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
        ));
    }
    Ok(match goal {
        None => {
            let goals = value["goals"].as_array().cloned().unwrap_or_default();
            if goals.is_empty() {
                format!("no delegation goals in {}\n", log.display())
            } else {
                let mut out = String::new();
                for g in goals {
                    out.push_str(&format!(
                        "{} {} {} nodes, budget tokens {} steps {}\n",
                        g["node"].as_str().unwrap_or(""),
                        g["status"].as_str().unwrap_or(""),
                        g["nodes"],
                        g["budget"]["tokens"],
                        g["budget"]["steps"],
                    ));
                }
                out
            }
        }
        Some(_) => text(value["nodes"].as_array().map_or(&[][..], Vec::as_slice)),
    })
}

#[cfg(test)]
#[path = "audit_tests.rs"]
mod tests;
