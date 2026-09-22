//! Offline trace replay scorer (issue #107).
//!
//! Walks archived Tier-2 mission traces (JSONL, as written by
//! `evals/tier2/run.py --archive`) under one or more candidate system
//! prompts and reports per-mission scores plus a monotone-selection
//! verdict. Tool settlements come from the ARCHIVE — zero tool
//! executions, zero workspaces. One provider completion call per
//! (candidate, mission) pair is the only cost.

use anyhow::{bail, Context, Result};
use clap::Parser;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::exit;
use tole_core::approval::{Approver, ToolRequest, Verdict};
use tole_core::file_tools::EditFileTool;
#[cfg(feature = "shell-tools")]
use tole_core::gh::GhTool;
#[cfg(feature = "shell-tools")]
use tole_core::git::GitTool;
#[cfg(feature = "shell-tools")]
use tole_core::jobs::{JobPollTool, JobStartTool};
use tole_core::openai::{OpenAiConfig, OpenAiProvider};
use tole_core::provider::Provider;
use tole_core::read_file::ReadFileTool;
use tole_core::replay::MissionTrace;
#[cfg(feature = "shell-tools")]
use tole_core::run_command::RunCommandTool;
use tole_core::tool::ToolRegistry;
#[cfg(feature = "shell-tools")]
use tole_core::uteke::{UtekeDocumentTool, UtekeRecallTool};

/// Auto-allow everything the scorer registers. The registry refuses to
/// hold Write tools without an approver wired; the scorer never EXECUTES
/// tools (replay feeds recorded settlements), so approval never fires —
/// this exists to satisfy the invariant, not to grant anything.
struct ReplayApprover;

impl Approver for ReplayApprover {
    fn decide(&self, _req: &ToolRequest<'_>) -> Verdict {
        Verdict::Deny
    }
}

#[derive(Parser)]
#[command(
    name = "replay",
    about = "Offline trace replay scorer (evals Tier 2.5)",
    after_help = "Examples:\n  replay --traces evals/traces --incumbent evals/replay/prompts/incumbent.txt --revised evals/replay/prompts/no-tool-discipline.txt\n  replay --traces evals/traces --incumbent evals/replay/prompts/incumbent.txt --model gemini-3.7-flash"
)]
struct Args {
    /// Trace archive root (layout: <root>/<model>/<mission>/*.jsonl).
    #[arg(long)]
    traces: PathBuf,
    /// Candidate prompt file. The incumbent (currently deployed prompt)
    /// is always evaluated in the same run — monotone selection.
    #[arg(long)]
    revised: PathBuf,
    /// Only replay traces recorded by this model (default: all).
    #[arg(long)]
    model: Option<String>,
    /// Only replay this mission.
    #[arg(long)]
    mission: Option<String>,
    /// Emit JSON (machine-readable) instead of a human table.
    #[arg(long)]
    json: bool,
}

fn main() {
    match run() {
        Ok(()) => {}
        Err(e) => {
            eprintln!("error: {e:#}");
            exit(1);
        }
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let revised_prompt = std::fs::read_to_string(&args.revised)
        .with_context(|| format!("reading revised prompt {}", args.revised.display()))?;

    // The incumbent prompt is the shipped default — read from the repo
    // source so the scorer can never silently drift from what production
    // actually runs.
    let incumbent_prompt = incumbent_default_prompt().map_err(|e| anyhow::anyhow!("{e}"))?;

    let cfg = OpenAiConfig::from_env()
        .context("no provider env (TOLE_* / OPENAI_* complete triple) — replay needs one completion call per (candidate, mission)")?;
    // Model ids may contain '/' (e.g. "glm/glm-5.1"), so the archive
    // layout <root>/<model>/<mission>/*.jsonl is discovered by walking
    // for *.jsonl and reading (model, mission) from the path relative to
    // the root — never by fixed depth.
    let traces = collect_traces(&args.traces, args.model.as_deref(), args.mission.as_deref())?;
    if traces.is_empty() {
        bail!(
            "no traces found under {} (expected <root>/<model>/<mission>/*.jsonl; --model matches the model path relative to the root)",
            args.traces.display()
        );
    }

    let mut rows: Vec<serde_json::Value> = Vec::new();
    let mut totals: BTreeMap<&str, (f64, u32, u32)> = BTreeMap::new(); // candidate -> (sum, missions, wins)
    totals.insert("incumbent", (0.0, 0, 0));
    totals.insert("revised", (0.0, 0, 0));

    for (model, mission, trace_path) in &traces {
        let trace = match MissionTrace::parse(trace_path, mission, model) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("skip: {e}");
                continue;
            }
        };
        let mut per_candidate: Vec<(&str, f64, bool, f64, Option<String>)> = Vec::new();
        for (name, prompt) in [
            ("incumbent", &incumbent_prompt),
            ("revised", &revised_prompt),
        ] {
            let mut provider = make_provider(&cfg, prompt)?;
            let mut s = trace.score_candidate(provider.as_mut());
            // Transient provider errors get ONE retry (house pattern from
            // #66: 429/timeout/5xx are transient-retry class; the
            // provider layer itself never retries).
            if s.walk
                .error
                .as_deref()
                .is_some_and(|e| e.contains("provider error"))
            {
                std::thread::sleep(std::time::Duration::from_secs(2));
                let mut provider = make_provider(&cfg, prompt)?;
                s = trace.score_candidate(provider.as_mut());
            }
            per_candidate.push((
                name,
                s.score,
                s.final_ok,
                s.path_agreement,
                s.walk.error.clone(),
            ));
            let e = totals.get_mut(name).unwrap();
            e.0 += s.score;
            e.1 += 1;
        }
        let inc = per_candidate[0].1;
        let rev = per_candidate[1].1;
        let winner = if rev > inc { "revised" } else { "incumbent" };
        if rev > inc {
            totals.get_mut("revised").unwrap().2 += 1;
        }
        if !args.json {
            println!("=== {model} / {mission} / {} ===", trace.session_id);
            for (name, score, final_ok, agreement, err) in &per_candidate {
                println!(
                    "  {name:9} score={score:.3} path={agreement:.2} final={final_ok}{}",
                    err.as_deref()
                        .map(|e| format!(" err={e}"))
                        .unwrap_or_default()
                );
            }
            println!("  -> winner: {winner}");
        }
        rows.push(json!({
            "model": model,
            "mission": mission,
            "session": trace.session_id,
            "incumbent": candidate_json(&per_candidate[0]),
            "revised": candidate_json(&per_candidate[1]),
            "winner": winner,
        }));
    }

    // Monotone selection: the revision ships only when its AVERAGE score
    // over the whole corpus is not worse than the incumbent's. Ties keep
    // the incumbent (never churn the deployed prompt without a win).
    let (inc_sum, n, _) = totals["incumbent"];
    let (rev_sum, _, rev_wins) = totals["revised"];
    let ship = if n > 0 && rev_sum / f64::from(n) > inc_sum / f64::from(n) {
        "revised"
    } else {
        "incumbent"
    };
    if args.json {
        println!(
            "{}",
            json!({
                "per_trace": rows,
                "incumbent_avg": if n > 0 { inc_sum / f64::from(n) } else { 0.0 },
                "revised_avg": if n > 0 { rev_sum / f64::from(n) } else { 0.0 },
                "traces": n,
                "revision_wins": rev_wins,
                "ship": ship,
            })
        );
    } else {
        println!();
        println!(
            "incumbent avg = {:.3}   revised avg = {:.3}   ({n} traces, revision wins {rev_wins})",
            if n > 0 { inc_sum / f64::from(n) } else { 0.0 },
            if n > 0 { rev_sum / f64::from(n) } else { 0.0 },
        );
        println!("SHIP: {ship}");
    }
    Ok(())
}

fn candidate_json(c: &(&str, f64, bool, f64, Option<String>)) -> serde_json::Value {
    json!({
        "score": c.1,
        "final_ok": c.2,
        "path_agreement": c.3,
        "error": c.4,
    })
}

fn make_provider(cfg: &OpenAiConfig, system_prompt: &str) -> Result<Box<dyn Provider>> {
    let mut p = OpenAiProvider::new(cfg.clone()).with_system_prompt(system_prompt);
    // Mirror the CLI's registered tool set so the candidate sees (and is
    // scored on choosing among) the same choice space as the live run.
    // Same deviation list as production's probe/env gates: delete_file is
    // interactive-registration-only (Destructive — the PRD forbids
    // registering it without a human approver) and MCP servers are
    // env-dependent; neither appears in live traces for these missions.
    let tmp = std::env::temp_dir();
    let mut reg = ToolRegistry::with_approver(ReplayApprover);
    macro_rules! add {
        ($t:expr) => {
            reg.register(Box::new($t))
                .map_err(|e| anyhow::anyhow!("{e}"))?
        };
    }
    add!(ReadFileTool::new(tmp.clone()));
    add!(tole_cli::tools::WriteFileTool::new(tmp.clone()));
    add!(EditFileTool::new(tmp.clone()));
    #[cfg(feature = "shell-tools")]
    {
        add!(RunCommandTool::new(tmp.clone()));
        add!(GitTool::new().in_dir(tmp.clone()));
        add!(GhTool::new("codecoradev/tole".to_string()));
        add!(JobStartTool::new(tmp.clone()));
        add!(JobPollTool::new(tmp.clone()));
        add!(UtekeRecallTool::new());
        add!(UtekeDocumentTool::new(None));
    }
    p = p.with_tool_specs(reg.specs());
    Ok(Box::new(p))
}

/// The currently deployed default prompt, extracted from the CLI source.
/// Grep-based on purpose: a literal copy here WOULD drift (that is the
/// exact failure class the monotone guard exists for).
fn incumbent_default_prompt() -> Result<String> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let main_rs = manifest
        .join("../tole-cli/src/main.rs")
        .canonicalize()
        .context("locating tole-cli/src/main.rs")?;
    let src = std::fs::read_to_string(&main_rs)?;
    let start = src
        .find("fn default_system_prompt()")
        .context("default_system_prompt not found in tole-cli/src/main.rs")?;
    let rest = &src[start..];
    let open = rest.find('"').context("prompt literal not found")?;
    // The literal is a sequence of adjacent "..." segments joined with `\`
    // line continuations; collect until the closing `"` of the last piece.
    let mut out = String::new();
    let bytes = rest.as_bytes();
    let mut i = open + 1;
    loop {
        match bytes.get(i) {
            Some(b'"') => {
                // Adjacent-segment join or end: peek past whitespace and
                // an optional `\` continuation.
                let mut j = i + 1;
                while matches!(
                    bytes.get(j),
                    Some(b' ') | Some(b'\n') | Some(b'\t') | Some(b'\\')
                ) {
                    j += 1;
                }
                if bytes.get(j) == Some(&b'"') {
                    i = j + 1;
                    continue;
                }
                break;
            }
            Some(b'\\') => {
                // Escape decoding (cora full-scan #30): the pushed prompt
                // must be what RUST sees at runtime, not the raw source
                // text. Handle the escapes that can legally appear in a
                // string literal; `\<newline>` is a line continuation
                // (the escaped char IS the newline).
                match bytes.get(i + 1) {
                    Some(b'n') => out.push('\n'),
                    Some(b't') => out.push('\t'),
                    Some(b'r') => out.push('\r'),
                    Some(b'"') => out.push('"'),
                    Some(b'\\') => out.push('\\'),
                    Some(b'\'') => out.push('\''),
                    Some(b'0') => bail!("unexpected \\0 in prompt literal"),
                    Some(_) => {
                        // Line continuation: `\<newline>[whitespace]` —
                        // Rust strips the newline AND leading whitespace
                        // of the next line.
                        let mut j = i + 1;
                        while matches!(
                            bytes.get(j),
                            Some(b'\n') | Some(b'\r') | Some(b' ') | Some(b'\t')
                        ) {
                            j += 1;
                        }
                        i = j;
                        continue;
                    }
                    None => bail!("unterminated default prompt literal"),
                }
                i += 2;
            }
            Some(_) => {
                let ch = rest[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
            None => bail!("unterminated default prompt literal"),
        }
    }
    Ok(out)
}

/// Walk the archive for `*.jsonl` (skipping `*.meta.json` sidecars) and
/// derive (model, mission) from the path relative to the root. Model ids
/// may contain '/' — the FIRST path segment relative to the root is the
/// model only because run.py writes one mission level under it and the
/// mission dir contains the jsonl files; for deeper trees the model is
/// everything except the last directory. Matches:
/// - `--model` on the model path relative to the root (exact),
/// - `--mission` on the trace's parent directory name (exact).
fn collect_traces(
    root: &Path,
    model_filter: Option<&str>,
    mission_filter: Option<&str>,
) -> Result<Vec<(String, String, PathBuf)>> {
    let mut out = Vec::new();
    let stack = vec![root.to_path_buf()];
    let mut dirs = Vec::new();
    let mut stack = stack;
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let e = e?;
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
                dirs.push(p);
            }
        }
    }
    dirs.sort();
    for p in dirs {
        let rel = p
            .strip_prefix(root)
            .with_context(|| format!("{} not under {}", p.display(), root.display()))?;
        let mut parts: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect();
        let file = parts.pop().context("empty relative path")?;
        let _ = file; // jsonl filename is informational; (model, mission) drive the corpus
        let mission = parts.pop().context(format!(
            "{}: no mission directory (expected <root>/<model>/<mission>/<file>.jsonl)",
            p.display()
        ))?;
        let model = parts.join("/");
        if let Some(f) = model_filter {
            if model != f {
                continue;
            }
        }
        if let Some(f) = mission_filter {
            if mission != f {
                continue;
            }
        }
        out.push((model, mission, p));
    }
    Ok(out)
}
