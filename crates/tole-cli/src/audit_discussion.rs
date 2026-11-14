//! `tole audit --discussions` / `--discussion <ID>` (#588): the multi-round
//! discussions in the tole data dir, read-only.
//!
//! It reads the discussion logs (`<data dir>/discussions/<id>.jsonl`) through
//! `DiscussionLog::state`, which takes no lock and writes nothing, and which
//! opens the file without following a symlink and with a size cap. It shows
//! the goal (question, criteria, participants, budget), each round's summary
//! (a digest's lines, or that the round was passed on as written and why),
//! the checker's checklist, the gate and the tokens used. It never prints a
//! raw contribution. Every string is redacted with the process `SecretSet`
//! before it is capped, and the whole value is redacted again.

use std::path::Path;

use serde_json::{json, Value};
use tole_core::discussion::{Gate, Item, Reason, RoundSummary};
use tole_core::discussion_log::{describe, DiscussionLog, State};
use tole_core::secrets::SecretSet;

use crate::audit::{line, TEXT_CHARS};

/// Longest question, criterion or digest line shown, in characters.
pub const LINE_CHARS: usize = 300;
/// Most discussions listed, and most digest lines shown per section.
pub const MAX_LISTED: usize = 200;
pub const MAX_ITEMS: usize = 12;

fn gate_name(g: &Gate) -> String {
    match g {
        Gate::Report => "report".into(),
        Gate::Continue => "continue".into(),
        Gate::Escalate(r) => format!(
            "escalate: {}",
            match r {
                Reason::PendingApproval => "pending_approval",
                Reason::OwnerDecision => "owner_decision",
                Reason::Budget => "budget",
                Reason::MaxRounds => "max_rounds",
                Reason::Stalled => "stalled",
                Reason::Stopped => "stopped",
            }
        ),
    }
}

fn status(st: &State) -> String {
    match &st.closed {
        Some((g, _)) => format!("closed ({})", gate_name(g)),
        None => "open".into(),
    }
}

fn items(secrets: &SecretSet, list: &[Item]) -> Vec<String> {
    list.iter()
        .take(MAX_ITEMS)
        .map(|i| {
            format!(
                "{} ({}, {})",
                line(secrets, &i.text, LINE_CHARS),
                i.by,
                i.entry
            )
        })
        .collect()
}

fn summary_json(secrets: &SecretSet, s: &RoundSummary) -> Value {
    let strs = |v: &[String]| -> Vec<String> {
        v.iter()
            .take(MAX_ITEMS)
            .map(|t| line(secrets, t, LINE_CHARS))
            .collect()
    };
    match s {
        RoundSummary::Digest(d) => json!({
            "kind": "digest",
            "decisions": items(secrets, &d.decisions),
            "claims": items(secrets, &d.claims),
            "disagreements": d.disagreements.iter().take(MAX_ITEMS).map(|x| json!({
                "topic": line(secrets, &x.topic, LINE_CHARS),
                "positions": items(secrets, &x.positions),
            })).collect::<Vec<_>>(),
            "open_questions": strs(&d.open_questions),
            "actions": items(secrets, &d.actions),
            "delta": strs(&d.delta),
        }),
        // Raw contributions are never shown: only who spoke.
        RoundSummary::Verbatim { contributions, .. } => json!({
            "kind": "verbatim",
            "speakers": contributions.iter().map(|c| c.participant.clone()).collect::<Vec<_>>(),
        }),
        RoundSummary::Fallback {
            contributions,
            reason,
            ..
        } => json!({
            "kind": "fallback",
            "speakers": contributions.iter().map(|c| c.participant.clone()).collect::<Vec<_>>(),
            "reason": line(secrets, reason, TEXT_CHARS),
        }),
    }
}

fn discussion_json(id: &str, st: &State, secrets: &SecretSet) -> Value {
    let c = st.contract.as_ref();
    let rounds: Vec<u32> = {
        let mut r: Vec<u32> = st.contributions.iter().map(|c| c.round).collect();
        r.extend(st.gates.keys());
        r.sort_unstable();
        r.dedup();
        r
    };
    let round_json: Vec<Value> = rounds
        .iter()
        .map(|r| {
            let spent: u64 = st
                .reservations
                .values()
                .filter(|x| x.round == *r)
                .map(|x| x.settled.unwrap_or(x.tokens))
                .sum();
            json!({
                "round": r,
                "speakers": st.round_contributions(*r).iter().map(|c| c.participant.clone()).collect::<Vec<_>>(),
                "summary": st.summaries.get(r).map(|s| summary_json(secrets, s)),
                "checklist": st.verdicts.get(r).map(|v| v.iter().map(|v| json!({
                    "met": v.met, "evidence": v.evidence})).collect::<Vec<_>>()),
                "gate": st.gates.get(r).map(gate_name),
                "tokens": spent,
            })
        })
        .collect();
    json!({
        "id": id,
        "status": status(st),
        "question": c.map(|c| line(secrets, &c.question, LINE_CHARS)),
        "criteria": c.map(|c| c.criteria.iter().map(|t| line(secrets, t, LINE_CHARS)).collect::<Vec<_>>()),
        "participants": c.map(|c| c.participants.clone()),
        "budget": c.map(|c| c.budget_tokens),
        "max_rounds": c.map(|c| c.max_rounds),
        "tokens_used": st.tokens_used,
        "open_calls": st.reservations.values().filter(|r| r.settled.is_none()).count(),
        "rounds": round_json,
        "outcome": st.closed.as_ref().map(|(g, t)| json!({
            "gate": gate_name(g),
            "why": describe(*g),
            "text": line(secrets, t, TEXT_CHARS),
        })),
    })
}

fn text_of(v: &Value) -> String {
    let s = |v: &Value| v.as_str().unwrap_or("").to_string();
    let mut out = vec![
        format!("{} {}", s(&v["id"]), s(&v["status"])),
        format!("  question: {}", s(&v["question"])),
    ];
    for (i, c) in v["criteria"].as_array().into_iter().flatten().enumerate() {
        out.push(format!("  criterion {}: {}", i + 1, s(c)));
    }
    out.push(format!(
        "  participants: {}; tokens used {} of {}; max rounds {}",
        v["participants"]
            .as_array()
            .map(|a| a.iter().map(s).collect::<Vec<_>>().join(", "))
            .unwrap_or_default(),
        v["tokens_used"],
        v["budget"],
        v["max_rounds"]
    ));
    for r in v["rounds"].as_array().into_iter().flatten() {
        out.push(format!(
            "  round {}: {} tokens, gate {}",
            r["round"],
            r["tokens"],
            r["gate"].as_str().unwrap_or("-")
        ));
        let sm = &r["summary"];
        match sm["kind"].as_str() {
            Some("digest") => {
                for key in ["decisions", "claims", "actions", "open_questions", "delta"] {
                    for l in sm[key].as_array().into_iter().flatten() {
                        out.push(format!("    {key}: {}", s(l)));
                    }
                }
                for d in sm["disagreements"].as_array().into_iter().flatten() {
                    out.push(format!("    disagreement: {}", s(&d["topic"])));
                    for p in d["positions"].as_array().into_iter().flatten() {
                        out.push(format!("      - {}", s(p)));
                    }
                }
            }
            Some("verbatim") => out.push("    passed on as written (small round)".into()),
            Some("fallback") => out.push(format!(
                "    passed on as written, the digest was refused: {}",
                s(&sm["reason"])
            )),
            _ => out.push("    not summarized yet".into()),
        }
        if let Some(list) = r["checklist"].as_array() {
            let met = list.iter().filter(|c| c["met"] == json!(true)).count();
            out.push(format!("    checklist: {met} of {} met", list.len()));
        }
    }
    if let Some(o) = v["outcome"].as_object() {
        out.push(format!(
            "  outcome: {} ({})",
            o["gate"].as_str().unwrap_or(""),
            o["why"].as_str().unwrap_or("")
        ));
    }
    out.join("\n") + "\n"
}

/// The discussions in `dir`, or discussion `id`, as text or JSON, every
/// string redacted through `secrets`.
pub fn discussions(
    dir: &Path,
    id: Option<&str>,
    as_json: bool,
    secrets: &SecretSet,
) -> Result<String, String> {
    let value = match id {
        Some(id) => {
            let st = DiscussionLog::at(dir, id)?.state()?;
            if st.contract.is_none() {
                return Err(format!("no discussion `{id}` in {}", dir.display()));
            }
            discussion_json(id, &st, secrets)
        }
        None => {
            let mut ids: Vec<String> = match std::fs::read_dir(dir) {
                Ok(rd) => rd
                    .filter_map(Result::ok)
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                    .filter_map(|e| {
                        e.file_name()
                            .to_str()?
                            .strip_suffix(".jsonl")
                            .map(str::to_string)
                    })
                    .filter(|id| tole_core::discussion::id_ok(id))
                    .collect(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(e) => return Err(format!("{}: {e}", dir.display())),
            };
            ids.sort();
            ids.truncate(MAX_LISTED);
            let list: Vec<Value> = ids
                .iter()
                .map(|id| match DiscussionLog::at(dir, id).and_then(|l| l.state()) {
                    Ok(st) => json!({
                        "id": id,
                        "status": status(&st),
                        "rounds": st.summaries.len(),
                        "tokens_used": st.tokens_used,
                        "budget": st.contract.as_ref().map(|c| c.budget_tokens),
                        "question": st.contract.as_ref().map(|c| line(secrets, &c.question, 120)),
                    }),
                    Err(e) => json!({"id": id, "status": "unreadable", "error": line(secrets, &e, TEXT_CHARS)}),
                })
                .collect();
            json!({ "dir": dir, "discussions": list })
        }
    };
    let value = secrets.redact_value(&value);
    if as_json {
        return Ok(format!(
            "{}\n",
            serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
        ));
    }
    Ok(match id {
        Some(_) => text_of(&value),
        None => {
            let list = value["discussions"].as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                format!("no discussions in {}\n", dir.display())
            } else {
                list.iter()
                    .map(|d| {
                        format!(
                            "{} {} {} rounds, tokens {} of {}: {}\n",
                            d["id"].as_str().unwrap_or(""),
                            d["status"].as_str().unwrap_or(""),
                            d["rounds"],
                            d["tokens_used"],
                            d["budget"],
                            d["question"].as_str().or(d["error"].as_str()).unwrap_or("")
                        )
                    })
                    .collect()
            }
        }
    })
}

#[cfg(test)]
#[path = "audit_discussion_tests.rs"]
mod tests;
