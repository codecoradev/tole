//! E5 — End-to-end crash-resume proof, Tier A (mock provider, no network).
//!
//! Two layers prove the same property at different granularity:
//!
//! 1. **Deterministic cut points** — a crash between commits leaves exactly
//!    the first k commit lines on disk (each commit is fsynced). So the
//!    interrupted state is reproduced faithfully by truncating the
//!    reference run's file to header + k lines, reopening it, and
//!    resuming — for every k.
//! 2. **Real SIGKILL** — a child process drives a turn whose tool hangs
//!    inside the effect window (intent committed, settlement not); the
//!    parent SIGKILLs it there and resumes the same session file.
//!
//! The golden-file compare canonicalises the session to its durable
//! content — entry kind + payload sequence — plus the final text and
//! `pc == Final`. Wall-clock timestamps (and, for the two replan-window
//! cuts, the seq-derived ids of recovery records) are the only honest
//! differences between a crashed run and a clean one; the *content* must
//! be identical. That is the E5 acceptance criterion.

use cora_agent_core::mock::MockProvider;
use cora_agent_core::provider::ProviderOutput;
use cora_agent_core::storage::{JsonlStorage, Storage};
use cora_agent_core::tool::{Risk, Tool, ToolRegistry};
use cora_agent_core::turn::{resume_turn, run_turn, TurnOutcome};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Read-only echo tool — Idempotent replay, deterministic output.
struct EchoTool;
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        Ok(json!({ "echoed": input }))
    }
}

/// Read-only tool that parks inside the effect window when
/// `CORA_E5_HANG` is set (child mode), and completes instantly otherwise
/// (reference run / parent resume).
struct HangTool;
impl Tool for HangTool {
    fn name(&self) -> &str {
        "hang"
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    fn execute(&self, _input: Value) -> Result<Value, String> {
        if std::env::var_os("CORA_E5_HANG").is_some() {
            std::thread::sleep(std::time::Duration::from_secs(30));
        }
        Ok(json!({ "slept": true }))
    }
}

fn registry(with_hang: bool) -> ToolRegistry {
    let mut r = ToolRegistry::new();
    r.register(Box::new(EchoTool)).unwrap();
    if with_hang {
        r.register(Box::new(HangTool)).unwrap();
    }
    r
}

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cora-e5-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Canonical golden rendering of a session: the durable content only.
fn golden(s: &JsonlStorage) -> Vec<String> {
    s.entries()
        .iter()
        .map(|e| {
            json!({
                "kind": e.kind.as_str(),
                "payload": e.payload,
            })
            .to_string()
        })
        .collect()
}

fn final_text(out: Result<TurnOutcome, cora_agent_core::storage::StorageError>) -> String {
    match out.unwrap() {
        TurnOutcome::Final { text } => text,
        other => panic!("expected Final, got {other:?}"),
    }
}

/// Script: user → echo(1) → echo(2) → final. Ten commits total.
fn full_script() -> Vec<ProviderOutput> {
    vec![
        ProviderOutput::ToolCall {
            tool: "echo".into(),
            input: json!({ "n": 1 }),
        },
        ProviderOutput::ToolCall {
            tool: "echo".into(),
            input: json!({ "n": 2 }),
        },
        ProviderOutput::Final {
            text: "done".into(),
        },
    ]
}

/// The uninterrupted reference run: (golden lines, final text).
fn reference(dir: &Path) -> (Vec<String>, String) {
    let mut s = JsonlStorage::create(dir, "ref", None).unwrap();
    let mut p = MockProvider::scripted(full_script());
    let text = final_text(run_turn(&mut s, &mut p, &registry(false), "hello"));
    (golden(&s), text)
}

/// Header + first k commit lines of the reference file, as a fresh session.
/// This is byte-for-byte what a process killed after its k-th commit
/// leaves behind (one fsynced line per commit).
fn cut_file(dir: &Path, k: usize) -> PathBuf {
    let lines: Vec<String> = std::fs::read_to_string(dir.join("ref.jsonl"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert!(k < lines.len(), "cut k={k} beyond file");
    let path = dir.join(format!("cut-{k}.jsonl"));
    let mut body = lines[..=k].join("\n");
    body.push('\n');
    std::fs::write(&path, body).unwrap();
    path
}

/// How many scripted decisions the dead process had already consumed when
/// it died after commit k (the resumed run re-derives from there):
/// k=0,1: none asked; k=2: asked but re-derived after replan; k=3..6: one
/// consumed; k=7..9: two consumed; k=10: turn already finished.
fn script_offset(k: usize) -> usize {
    match k {
        0..=2 => 0,
        3..=6 => 1,
        _ => 2,
    }
}

// ---------------------------------------------------------------------------
// Layer 1: crash at every commit boundary (deterministic)
// ---------------------------------------------------------------------------

#[test]
fn crash_at_every_commit_boundary_resumes_to_same_result() {
    let dir = tmpdir("boundaries");
    let (ref_lines, ref_text) = reference(&dir);
    // 10 commits: 1 user message + 2×(tool sandwich = 4) + 1 final message.
    for k in 0..=10usize {
        let file = cut_file(&dir, k);
        let mut s = JsonlStorage::open(&file).unwrap();
        let text = if k == 0 {
            // Nothing was persisted: the honest recovery is a fresh turn.
            let mut p = MockProvider::scripted(full_script());
            final_text(run_turn(&mut s, &mut p, &registry(false), "hello"))
        } else if k == 10 {
            // The turn completed before the "crash": nothing to resume.
            assert_eq!(s.state().pc, cora_agent_core::state::Pc::Final);
            ref_text.clone()
        } else {
            let script: Vec<ProviderOutput> =
                full_script().into_iter().skip(script_offset(k)).collect();
            let mut p = MockProvider::scripted(script);
            final_text(resume_turn(&mut s, &mut p, &registry(false)))
        };
        assert_eq!(s.state().pc, cora_agent_core::state::Pc::Final, "k={k}");
        assert_eq!(text, ref_text, "k={k} final text diverged");
        assert_eq!(golden(&s), ref_lines, "k={k} golden diverged");
    }
}

// ---------------------------------------------------------------------------
// Layer 2: real SIGKILL mid-sandwich
// ---------------------------------------------------------------------------

const CHILD_ENV: &str = "CORA_E5_CHILD";
const FILE_ENV: &str = "CORA_E5_FILE";
const TEST_NAME: &str = "sigkill_mid_sandwich_resumes_to_same_result";

#[test]
fn sigkill_mid_sandwich_resumes_to_same_result() {
    if std::env::var_os(CHILD_ENV).is_some() {
        child_main();
        return;
    }
    let dir = tmpdir("sigkill");
    // Reference: the same flow without the kill (hang tool completes
    // instantly outside child mode).
    let mut s_ref = JsonlStorage::create(&dir, "ref", None).unwrap();
    let mut p_ref = MockProvider::scripted(vec![
        ProviderOutput::ToolCall {
            tool: "hang".into(),
            input: json!({ "why": "kill window" }),
        },
        ProviderOutput::Final {
            text: "survived".into(),
        },
    ]);
    let ref_text = final_text(run_turn(&mut s_ref, &mut p_ref, &registry(true), "hello"));
    let ref_lines = golden(&s_ref);

    let file = dir.join("killed.jsonl");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg(TEST_NAME)
        .env(CHILD_ENV, "1")
        .env(FILE_ENV, &file)
        .env("CORA_E5_HANG", "1")
        .spawn()
        .unwrap();
    // The child commits: user message, ToolCall transition, begin (intent
    // + pending). Then it parks inside the tool. Poll for those 4 lines.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let n = std::fs::read_to_string(&file)
            .map(|c| c.lines().count())
            .unwrap_or(0);
        if n >= 4 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "child never reached the effect window; file so far: {:?}",
            std::fs::read_to_string(&file).unwrap_or_default()
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    child.kill().unwrap();
    child.wait().unwrap();

    // The killed session is mid-sandwich: intent durable, no settlement.
    let mut s = JsonlStorage::open(&file).unwrap();
    assert_eq!(s.state().pc, cora_agent_core::state::Pc::Executing);
    let mut p = MockProvider::scripted(vec![ProviderOutput::Final {
        text: "survived".into(),
    }]);
    let text = final_text(resume_turn(&mut s, &mut p, &registry(true)));
    assert_eq!(s.state().pc, cora_agent_core::state::Pc::Final);
    assert_eq!(text, ref_text);
    assert_eq!(golden(&s), ref_lines);
}

/// Child half of the SIGKILL test: drive the turn until the hang tool
/// parks inside the effect window, then die by parental SIGKILL.
fn child_main() {
    let file = PathBuf::from(std::env::var(FILE_ENV).expect("CORAGENT_E5_FILE missing"));
    let mut s = JsonlStorage::create(file.parent().unwrap(), "killed", None).unwrap();
    let _guard = &mut s; // keep the writer alive while parked
    let mut p = MockProvider::scripted(vec![
        ProviderOutput::ToolCall {
            tool: "hang".into(),
            input: json!({ "why": "kill window" }),
        },
        ProviderOutput::Final {
            text: "survived".into(),
        },
    ]);
    let out = run_turn(&mut s, &mut p, &registry(true), "hello");
    // Unreachable in the kill scenario; only hit if hang mode is broken.
    eprintln!("child finished without being killed: {out:?}");
    std::process::exit(1);
}
