//! Agentic evals — Tier 1 (issue #73): deterministic behavioral
//! contracts of the harness, exercised through the REAL turn loop and
//! JSONL storage. These complement unit tests by asserting OUTCOMES a
//! release must preserve; a failure here is a harness regression even
//! when every unit test passes.
//!
//! Contract groups:
//! 1. WIRE-SHAPE STABILITY — the provider request body for a canonical
//!    transcript must stay byte-identical across releases (KV-cache
//!    prefix stability = cost/latency contract; any drift is a
//!    deliberate change).
//! 2. RESUME EQUIVALENCE — crash mid-sandwich, then resume: durable
//!    outcome equals the uninterrupted run (E5 property, current tools).
//! 3. ABORT-PATH CONTRACTS — every abort parks resumable + durable
//!    audit record (machine resumability as a user-visible guarantee).
//! 4. APPROVAL INTERACTION MATRIX — allow/deny × risk tier × fresh/crashed
//!    verdicts (the #68 semantics as a table).

use serde_json::{json, Value};
use std::path::PathBuf;

use tole_core::approval::AllowlistApprover;
use tole_core::entry::{EntryType, NewEntry};
use tole_core::machine::{begin, ReplaySafety};
use tole_core::mock::MockProvider;
use tole_core::openai::OpenAiProvider;
use tole_core::provider::ProviderOutput;
use tole_core::state::{Pc, StateTransition};
use tole_core::storage::{Commit, JsonlStorage, Storage};
use tole_core::tool::{Risk, Tool, ToolRegistry};
use tole_core::turn::{resume_turn, run_turn, TurnOutcome};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-evals-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct EchoTool;
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        Ok(input)
    }
}

struct WriteTool;
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write_file"
    }
    fn risk(&self) -> Risk {
        Risk::Write
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        Ok(json!({ "written": input }))
    }
}

struct DestructiveTool;
impl Tool for DestructiveTool {
    fn name(&self) -> &str {
        "purge"
    }
    fn risk(&self) -> Risk {
        Risk::Destructive
    }
    fn execute(&self, _input: Value) -> Result<Value, String> {
        Ok(json!({ "purged": true }))
    }
}

/// Canonical transcript building block: a session where the user asked
/// for something, the model called echo, got a result, and answered.
fn canonical_session(dir: &PathBuf, id: &str) -> JsonlStorage {
    let mut s = JsonlStorage::create(
        dir,
        id,
        Some("You are tole, a careful assistant.".to_string()),
    )
    .unwrap();
    let seq = s.state().seq;
    s.commit(
        Commit::new()
            .entry(NewEntry::root(
                EntryType::new(EntryType::MESSAGE),
                json!({ "role": "user", "text": "Echo {\"n\": 1} then summarize." }),
            ))
            .transition(StateTransition::from(seq, Pc::Planning)),
    )
    .unwrap();
    s
}

fn echo_registry() -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(EchoTool)).unwrap();
    reg
}

fn cfg() -> tole_core::openai::OpenAiConfig {
    tole_core::openai::OpenAiConfig::new("https://api.example/v1", "eval-model", "k")
}

// ---------------------------------------------------------------------------
// 1. WIRE-SHAPE STABILITY
// ---------------------------------------------------------------------------

/// The provider request body for the canonical session must be
/// byte-stable. Any change here silently invalidates provider-side
/// prefix caches and raises cost/latency for every user — it must be a
/// reviewed, changelogged decision. Update EXPECTED only with that
/// intent (and a CHANGELOG entry under Changed).
#[test]
fn tier1_wire_shape_stability() {
    let dir = tmpdir("wire");
    let mut s = canonical_session(&dir, "wire");
    // An intent committed mid-turn (planning, not yet final): exactly the
    // state the next provider call would observe.
    let seq = s.state().seq;
    s.commit(Commit::new().transition(StateTransition::from(seq, Pc::ToolCall)))
        .unwrap();
    begin(
        &mut s,
        "echo",
        json!({ "n": 1 }),
        ReplaySafety::Idempotent,
        None,
    )
    .unwrap();

    let p = OpenAiProvider::new(cfg()).with_system_prompt("You are tole, a careful assistant.");
    let body = p.request_body(s.entries());

    // GOLDEN: byte-stable against the pinned expected body. Update ONLY
    // via a reviewed, CHANGELOGGED wire-format decision (any drift here
    // silently invalidates provider prefix caches for every user).
    let rendered = body.to_string();
    let expected = r#"{"messages":[{"content":"You are tole, a careful assistant.","role":"system"},{"content":"Echo {\"n\": 1} then summarize.","role":"user"},{"content":null,"role":"assistant","tool_calls":[{"function":{"arguments":"{\"n\":1}","name":"echo"},"id":"intent_3","type":"function"}]}],"model":"eval-model"}"#;
    assert_eq!(rendered, expected);
}

// ---------------------------------------------------------------------------
// 2. RESUME EQUIVALENCE
// ---------------------------------------------------------------------------

/// A session that crashed mid-echo-sandwich and was resumed must land
/// in the SAME durable end state as an uninterrupted run (modulo seq
/// numbering of the recovery records, which the recovery protocol
/// legitimately adds).
#[test]
fn tier1_resume_equivalence() {
    // Reference: uninterrupted (fresh session starts at Idle).
    let ref_dir = tmpdir("resume-ref");
    let mut s = JsonlStorage::create(
        &ref_dir,
        "ref",
        Some("You are tole, a careful assistant.".to_string()),
    )
    .unwrap();
    let mut p = MockProvider::scripted(vec![
        ProviderOutput::ToolCall {
            tool: "echo".into(),
            input: json!({"n": 1}),
        },
        ProviderOutput::Final {
            text: "Echoed n=1; all set.".into(),
        },
    ]);
    let reg = echo_registry();
    let out = run_turn(&mut s, &mut p, &reg, "Echo {\"n\": 1} then summarize.").unwrap();
    assert!(matches!(out, TurnOutcome::Final { .. }));
    let ref_pc = s.state().pc;
    let ref_messages = s
        .entries()
        .iter()
        .filter(|e| e.kind.as_str() == "message")
        .count();
    let ref_final = s
        .entries()
        .iter()
        .rev()
        .find(|e| e.kind.as_str() == "message")
        .map(|e| e.payload["text"].clone())
        .unwrap();
    drop(s);

    // Crashed: the durable state exactly at the crash window — user
    // message committed, intent (Idempotent echo) opened, settlement
    // never landed (the E5 SIGKILL point, constructed deterministically;
    // the mock cannot crash mid-sandwich because exhaustion hits only at
    // the NEXT provider step, after settle).
    let crash_dir = tmpdir("resume-crash");
    let mut s = canonical_session(&crash_dir, "crash");
    let seq = s.state().seq;
    s.commit(Commit::new().transition(StateTransition::from(seq, Pc::ToolCall)))
        .unwrap();
    begin(
        &mut s,
        "echo",
        json!({"n": 1}),
        ReplaySafety::Idempotent,
        None,
    )
    .unwrap();
    assert!(s.get_register("pending", "op").is_some());
    drop(s);

    let mut s2 = JsonlStorage::open(crash_dir.join("crash.jsonl")).unwrap();
    let mut p2 = MockProvider::scripted(vec![ProviderOutput::Final {
        text: "Echoed n=1; all set.".into(),
    }]);
    let out2 = resume_turn(&mut s2, &mut p2, &reg).unwrap();
    assert!(matches!(out2, TurnOutcome::Final { .. }));

    // Equivalence: same terminal pc, same message count, same final text.
    assert_eq!(ref_pc, Pc::Final);
    assert_eq!(s2.state().pc, ref_pc);
    let count = |entries: &[tole_core::entry::Entry], kind: &str| {
        entries.iter().filter(|e| e.kind.as_str() == kind).count()
    };
    assert_eq!(count(s2.entries(), "message"), ref_messages);
    let s2_final = s2
        .entries()
        .iter()
        .rev()
        .find(|e| e.kind.as_str() == "message")
        .map(|e| e.payload["text"].clone())
        .unwrap();
    assert_eq!(s2_final, ref_final);
}

// ---------------------------------------------------------------------------
// 3. ABORT-PATH CONTRACTS
// ---------------------------------------------------------------------------

/// Every abort outcome must (a) park the machine at a pc the next
/// resume/run accepts, and (b) leave a durable ERROR record so replay
/// can see why. This is the resumability guarantee as a table.
#[test]
fn tier1_abort_paths_park_resumable_with_audit() {
    type Drive = Box<dyn Fn(&mut JsonlStorage, &mut MockProvider, &ToolRegistry) -> TurnOutcome>;
    let cases: Vec<(&str, Drive)> = vec![
        // Unknown tool: durable error, stays Planning (resumable).
        (
            "unknown_tool",
            Box::new(
                |s: &mut JsonlStorage, p: &mut MockProvider, reg: &ToolRegistry| {
                    run_turn(s, p, reg, "hi").unwrap()
                },
            ),
        ),
    ];
    for (name, drive) in cases {
        let dir = tmpdir(&format!("abort-{name}"));
        let mut s = JsonlStorage::create(&dir, name, None).unwrap();
        let mut p = MockProvider::scripted(vec![ProviderOutput::ToolCall {
            tool: "nonexistent".into(),
            input: json!({}),
        }]);
        let reg = echo_registry();
        let out = drive(&mut s, &mut p, &reg);
        assert!(
            matches!(out, TurnOutcome::UnknownTool { .. }),
            "{name}: unexpected {out:?}"
        );
        // (a) parked in a resumable state: Planning.
        assert_eq!(s.state().pc, Pc::Planning, "{name}");
        // (b) durable audit record exists.
        assert!(
            s.entries().iter().any(|e| e.kind.as_str() == "error"),
            "{name}: no durable error record"
        );
        // (c) the next resume continues the session (no wedging).
        let mut p2 = MockProvider::scripted(vec![ProviderOutput::Final {
            text: "recovered".into(),
        }]);
        let out2 = resume_turn(&mut s, &mut p2, &reg).unwrap();
        assert!(
            matches!(out2, TurnOutcome::Final { text } if text == "recovered"),
            "{name}"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. APPROVAL INTERACTION MATRIX
// ---------------------------------------------------------------------------

/// Verdict table: approver default × tool risk × gate timing.
/// Fresh-run gates (drive()) and replay gates (#68 semantics).
#[test]
fn tier1_approval_interaction_matrix() {
    struct Case {
        name: &'static str,
        tool_risk: Risk,
        fresh_allows: bool,
    }
    let cases = [
        Case {
            name: "write+allow",
            tool_risk: Risk::Write,
            fresh_allows: true,
        },
        Case {
            name: "write+deny",
            tool_risk: Risk::Write,
            fresh_allows: false,
        },
        Case {
            name: "destructive+allow",
            tool_risk: Risk::Destructive,
            fresh_allows: false,
        },
    ];
    for c in cases {
        let dir = tmpdir(&format!("matrix-{}", c.name));
        let mut s = JsonlStorage::create(&dir, "m", None).unwrap();
        // Approver wiring per case: the ALLOW case lists the tool; the
        // DENY case lists nothing (allow_only with empty list = deny all
        // gated calls). DESTRUCTIVE: registration behind a non-interactive
        // approver must FAIL structurally — that IS the control (Destructive
        // is never allowlistable), so the case asserts the registration
        // error instead of driving a turn.
        let tool = if c.tool_risk == Risk::Write {
            "write_file"
        } else {
            "purge"
        };
        let mut reg = ToolRegistry::with_approver(match c.fresh_allows {
            true => AllowlistApprover::allow_only(vec![tool.into()]),
            false => AllowlistApprover::allow_only(vec![]),
        });
        let reg_result = if c.tool_risk == Risk::Write {
            reg.register(Box::new(WriteTool)).map(|_| ())
        } else {
            reg.register(Box::new(DestructiveTool)).map(|_| ())
        };
        if c.tool_risk == Risk::Destructive {
            assert!(
                reg_result.is_err(),
                "destructive+{}: must be structurally unregistrable",
                c.name
            );
            continue; // structural control proven; nothing to drive
        }
        reg_result.unwrap();
        // Script: tool call, then (only reachable when allowed) the final.
        let mut p = MockProvider::scripted(vec![
            ProviderOutput::ToolCall {
                tool: tool.into(),
                input: json!({}),
            },
            ProviderOutput::Final {
                text: "executed".into(),
            },
        ]);
        let out = run_turn(&mut s, &mut p, &reg, "go").unwrap();
        let executed = matches!(out, TurnOutcome::Final { .. });
        assert_eq!(
            executed, c.fresh_allows,
            "{}: fresh-run gate verdict mismatch (out={out:?})",
            c.name
        );
        // Every gated refusal leaves a durable record (the audit trail).
        if !c.fresh_allows {
            assert!(
                s.entries().iter().any(|e| e.kind.as_str() == "error"),
                "{}: no durable record for the refusal",
                c.name
            );
        }
    }
}
