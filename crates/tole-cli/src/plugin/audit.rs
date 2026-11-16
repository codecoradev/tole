//! `tole plugin audit [name] [--json]` (#600, design
//! `third-party-plugins-design.md` §4.5): one read-only report of every
//! locked plugin and every skill, for the owner to review.
//!
//! - Per MCP server: tier, state, approved manifest hash against the live
//!   one, binary integrity, secret grants, sandbox profile (approved and
//!   live), the scanner's findings, and the last `sandbox_fallback` events
//!   of the plugin ledger.
//! - Per skill: status, approved hash against the one on disk, the stated
//!   source, and the last ledger lines.
//! - It writes nothing and starts a server only the way `list` does: never
//!   one the lock refuses, never under an agent session.
//! - It prints no more tool text than `plugin diff` does: only the scanner's
//!   findings (names and scanner-written details). Everything goes through
//!   the process `SecretSet` and is made terminal-safe.

use std::collections::BTreeMap;

use serde_json::{json, Value};
use tole_core::mcp::Listed;
use tole_core::plugin_lock::{self, ManifestTool};
use tole_core::plugin_scan;
use tole_core::skill_approval::format_time;

use super::cmd::{collect, probe_listed, short, Ctx, Listing};

/// Ledger events shown per server or skill.
const LAST_EVENTS: usize = 3;

/// The last `sandbox_fallback` events of `server` in `ledger`, oldest first.
fn fallbacks(ledger: &str, server: &str) -> Vec<Value> {
    let mut all: Vec<Value> = ledger
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            v["kind"] == "plugin" && v["event"] == "sandbox_fallback" && v["server"] == server
        })
        .map(|v| {
            json!({
                "at": v["at"].as_u64().map(format_time),
                "profile": v["profile"],
                "reason": v["reason"],
            })
        })
        .collect();
    let from = all.len().saturating_sub(LAST_EVENTS);
    all.drain(..from);
    all
}

/// The tools the scanner reads: the live list when the server was started,
/// otherwise what the approval stored (already redacted, possibly cut).
fn scanned(
    live: Option<&Listed>,
    entry: Option<&plugin_lock::LockEntry>,
) -> (&'static str, Vec<ManifestTool>) {
    if let Some((_, tools)) = live {
        return ("live", tools.clone());
    }
    match entry {
        Some(e) if !e.manifest.is_empty() => (
            "approved snapshot",
            e.manifest
                .iter()
                .map(|t| ManifestTool {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    input_schema: Value::String(t.schema.clone()),
                })
                .collect(),
        ),
        _ => ("none", Vec::new()),
    }
}

/// Strings of `v` redacted, made terminal-safe and put on one line (every
/// field of the report is a single line, so none can forge another), in
/// place.
fn clean_value(ctx: &Ctx<'_>, v: &mut Value) {
    match v {
        Value::String(s) => *s = ctx.clean(s).replace(['\n', '\t'], " "),
        Value::Array(a) => a.iter_mut().for_each(|x| clean_value(ctx, x)),
        Value::Object(o) => o.values_mut().for_each(|x| clean_value(ctx, x)),
        _ => {}
    }
}

pub(super) fn audit(ctx: &Ctx<'_>, only: Option<&str>, as_json: bool) -> Result<String, String> {
    let mut live: BTreeMap<String, Listed> = BTreeMap::new();
    let Listing {
        mut rows,
        mut notes,
        eff,
        raw,
    } = collect(ctx, &mut |cfg, eff| {
        if only.is_some_and(|n| n != cfg.name) {
            return "not probed (not the one asked for)".to_string();
        }
        let (state, listed) = probe_listed(ctx, eff, cfg);
        if let Some(l) = listed {
            live.insert(cfg.name.clone(), l);
        }
        state
    });
    if let Some(n) = only {
        if !raw.iter().any(|r| r == n) {
            return Err(format!(
                "`{}` is neither configured nor in your plugin lock",
                ctx.clean(n)
            ));
        }
    }
    let ledger = match ctx.user.as_deref().and_then(|u| u.parent()) {
        Some(dir) => match plugin_lock::read_ledger(&dir.join(plugin_lock::LEDGER_FILE)) {
            Ok(t) => t.unwrap_or_default(),
            Err(e) => {
                notes.push(format!(
                    "plugin ledger unreadable ({e}): fallback events are not shown"
                ));
                String::new()
            }
        },
        None => String::new(),
    };
    let mut kept = Vec::new();
    for (row, name) in rows.drain(..).zip(&raw) {
        if only.is_some_and(|n| n != name) {
            continue;
        }
        let entry = eff.get(name);
        let others = plugin_scan::Others {
            servers: raw.iter().filter(|n| *n != name).cloned().collect(),
            tools: eff
                .iter()
                .filter(|(n, _)| *n != name)
                .flat_map(|(_, e)| e.manifest.iter().map(|t| t.name.clone()))
                .collect(),
        };
        let (from, tools) = scanned(live.get(name), entry);
        let findings: Vec<String> = plugin_scan::scan(&tools, &others, &ctx.secrets)
            .iter()
            .map(|f| f.line())
            .collect();
        let mut row = row;
        row["approved_manifest_hash"] = row["manifest_hash"].clone();
        row["live_manifest_hash"] = live
            .get(name)
            .map(|(_, t)| Value::String(plugin_lock::manifest_hash(t)))
            .unwrap_or(Value::Null);
        row["scan"] = json!({"source": from, "findings": findings});
        row["sandbox_fallbacks"] = Value::Array(fallbacks(&ledger, name));
        kept.push(row);
    }
    let skills = if only.is_some() {
        Vec::new()
    } else {
        match crate::skill_approve::audit_rows(Some(&ctx.workspace)) {
            Ok(r) => r,
            Err(e) => {
                notes.push(format!("skills not audited: {e}"));
                Vec::new()
            }
        }
    };
    let mut report = json!({"servers": kept, "skills": skills, "notes": notes});
    clean_value(ctx, &mut report);
    if as_json {
        return Ok(report.to_string());
    }
    Ok(render(&report))
}

fn render(report: &Value) -> String {
    let s = |v: &Value, k: &str| v[k].as_str().unwrap_or("-").to_string();
    let mut out = String::new();
    for r in report["servers"].as_array().into_iter().flatten() {
        let secrets: Vec<&str> = r["secrets"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        out.push_str(&format!(
            "server {}  {}  {}\n  source {}\n  manifest approved {}  live {}\n  grants {}\n  sandbox {}",
            s(r, "name"),
            s(r, "state"),
            s(r, "tier"),
            s(r, "source"),
            short(&s(r, "approved_manifest_hash")),
            match r["live_manifest_hash"].as_str() {
                Some(h) => short(h).to_string(),
                None => "not fetched".to_string(),
            },
            if secrets.is_empty() { "none".to_string() } else { secrets.join(", ") },
            s(r, "sandbox"),
        ));
        if let Some(p) = r["profile"].as_object().filter(|p| p["recorded"] == true) {
            let f = |k: &str| p[k].as_str().unwrap_or("-");
            out.push_str(&format!(
                "\n  profile approved {} {}  live {} {}{}",
                f("name"),
                short(f("hash")),
                f("live_name"),
                short(f("live_hash")),
                if p["changed"] == true {
                    "  ! CHANGED"
                } else {
                    ""
                }
            ));
        }
        if let Some(b) = r["binary"].as_str() {
            out.push_str(&format!("\n  ! {b}"));
        }
        let findings = r["scan"]["findings"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        out.push_str(&format!(
            "\n  scan ({}): {} finding(s)",
            s(&r["scan"], "source"),
            findings.len()
        ));
        for f in &findings {
            out.push_str(&format!("\n    ! {}", f.as_str().unwrap_or("-")));
        }
        let fb = r["sandbox_fallbacks"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if fb.is_empty() {
            out.push_str("\n  sandbox fallbacks: none recorded");
        }
        for e in &fb {
            out.push_str(&format!(
                "\n  ! sandbox fallback {}: ran unsandboxed instead of profile {}: {}",
                s(e, "at"),
                s(e, "profile"),
                s(e, "reason")
            ));
        }
        out.push('\n');
    }
    for k in report["skills"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "skill {}  {}  hash {}",
            s(k, "name"),
            s(k, "status"),
            short(&s(k, "hash"))
        ));
        if let Some(h) = k["approved_hash"].as_str() {
            out.push_str(&format!("  approved {}", short(h)));
        }
        out.push_str(&format!("{}\n", k["source_text"].as_str().unwrap_or("")));
        for e in k["history"].as_array().into_iter().flatten() {
            out.push_str(&format!("  ledger {}  {}\n", s(e, "at"), s(e, "event")));
        }
    }
    for n in report["notes"].as_array().into_iter().flatten() {
        out.push_str(&format!("! {}\n", n.as_str().unwrap_or("-")));
    }
    if out.is_empty() {
        out.push_str("no MCP server or skill to audit\n");
    }
    out
}
