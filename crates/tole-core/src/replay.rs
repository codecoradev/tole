//! Offline trace replay (issue #107) — the Dream-RSI replay-evaluation
//! pattern (arXiv:2609.14858) applied to tole's eval harness.
//!
//! A Tier-2 mission run records a session JSONL: the user prompt, the
//! tool calls the model made (`intent`), and their REALIZED outcomes
//! (`tool_result`). That history is an exact simulator of what the model
//! saw: a candidate prompt/policy can be walked over the same recorded
//! outcomes at ZERO tool executions — every observation it would need is
//! already on disk. This module replays a mission trace under a candidate
//! system prompt and scores how well the candidate reproduces the live
//! run's realized tool path and final answer.
//!
//! Scope: this is an evaluation pattern, not a product change. Tole stays
//! chat-first; no discovery loop, no session branching.

use crate::entry::{Entry, EntryType};
use crate::provider::{Provider, ProviderOutput};
use serde_json::Value;
use std::path::Path;

/// A parsed mission trace: the replayable prefix plus the outcome the
/// live run actually produced.
#[derive(Debug, Clone)]
pub struct MissionTrace {
    /// Session id from the trace header.
    pub session_id: String,
    /// Mission name (the directory under `traces/<model>/`).
    pub mission: String,
    /// Model that recorded the trace (the `traces/<model>/` segment).
    pub model: String,
    /// The recorded mission prompt (first user message).
    pub prompt: String,
    /// Recorded calls in commit order: tool name, input, and the
    /// settlement the live run observed (`tool_result` payload, or the
    /// `error` payload for error entries).
    pub recorded_calls: Vec<RecordedCall>,
    /// The final assistant text the live run produced (empty if none).
    pub recorded_final_text: String,
    /// The recorded user-message entry, re-used as the replay prefix's
    /// first message (request_body renders only role + text).
    user_entry: Entry,
}

/// One recorded generation–evaluation step.
#[derive(Debug, Clone)]
pub struct RecordedCall {
    /// Registry name of the tool the live run called.
    pub tool: String,
    /// Input the live run sent.
    pub input: Value,
    /// Realized outcome payload (`output` for tool_result, `error` for
    /// error entries).
    pub output: Value,
    /// Whether the settlement was a `tool_result` or an `error` entry.
    pub is_error: bool,
}

/// Result of walking one mission trace under one candidate.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayWalk {
    /// Provider steps consumed (tool steps + the final step).
    pub steps: u32,
    /// The path the candidate took: (tool, input) in order.
    pub calls: Vec<(String, Value)>,
    /// Final text if the candidate finished the mission.
    pub final_text: Option<String>,
    /// Why the walk ended without a final answer (divergence, provider
    /// error, step budget).
    pub error: Option<String>,
}

/// Score of one candidate over one mission trace.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayScore {
    /// Underlying [`ReplayWalk`].
    pub walk: ReplayWalk,
    /// Positional tool-path agreement vs the recorded path (0.0–1.0).
    pub path_agreement: f64,
    /// Whether the final text (trimmed, case-insensitive) matches the
    /// recorded final answer.
    pub final_ok: bool,
    /// Composite score: 0.7 × path_agreement + 0.3 × final_ok for a
    /// COMPLETED walk; 0.0 for a walk that errored/diverged.
    pub score: f64,
}

impl MissionTrace {
    /// Parse a session JSONL written by `JsonlStorage` (header record,
    /// then entry/state/usage/register records — unknown kinds and a
    /// torn final line are skipped, crash-safe like the storage replay).
    /// `mission` and `model_dir` come from the archive layout
    /// `traces/<model>/<mission>/<session>.jsonl`.
    pub fn parse(jsonl: &Path, mission: &str, model_dir: &str) -> Result<Self, String> {
        let text =
            std::fs::read_to_string(jsonl).map_err(|e| format!("{}: {e}", jsonl.display()))?;
        let mut session_id = String::new();
        let mut entries: Vec<Entry> = Vec::new();
        let mut user_entry: Option<Entry> = None;
        let mut final_text = String::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue, // torn tail — discard whole line
            };
            // One physical line = one commit = a single record OR an array
            // of records (batched transaction writes — same shape
            // `durable_metrics` in run.py already handles).
            let records: Vec<Value> = if v.is_array() {
                v.as_array().cloned().unwrap_or_default()
            } else {
                vec![v]
            };
            for v in records {
                match v.get("kind").and_then(Value::as_str) {
                    Some("header") => {
                        session_id = v
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                    }
                    Some("entry") => {
                        let Ok(e) = serde_json::from_value::<Entry>(v) else {
                            continue;
                        };
                        match e.kind.as_str() {
                            "message" => {
                                let role = e.payload.get("role").and_then(Value::as_str);
                                let msg = e.payload.get("text").and_then(Value::as_str);
                                match (role, msg) {
                                    (Some("user"), Some(_)) => {
                                        if entries.is_empty() {
                                            user_entry = Some(e.clone());
                                        }
                                        entries.push(e);
                                    }
                                    (Some("assistant"), Some(t)) => final_text = t.to_string(),
                                    _ => {}
                                }
                            }
                            "intent" | "tool_result" | "error" => entries.push(e),
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        }
        if entries.is_empty() || entries[0].kind != "message" {
            return Err(format!(
                "{}: no user message — not a replayable mission trace",
                jsonl.display()
            ));
        }
        let prompt = entries[0]
            .payload
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let recorded_calls = interleave_calls(&entries[1..]);
        Ok(Self {
            session_id,
            mission: mission.to_string(),
            model: model_dir.to_string(),
            prompt,
            recorded_calls,
            recorded_final_text: final_text,
            user_entry: user_entry.expect("parse guarantees a leading user entry"),
        })
    }

    /// Walk the mission once under `provider` (whose system prompt IS the
    /// candidate). Zero tool executions: when the candidate calls a tool,
    /// the settlement is the RECORDED outcome for that call — exact match
    /// on (tool, input) first, then the oldest unconsumed call to the
    /// same tool. Calling a tool the recorded run never called (or a
    /// step budget overrun) ends the walk as a divergence: that is the
    /// signal the scorer exists to surface, not an error to hide.
    pub fn replay(&self, provider: &mut dyn Provider) -> ReplayWalk {
        let budget = self.recorded_calls.len() as u32 + 1; // calls + final
        let mut walk = ReplayWalk {
            steps: 0,
            calls: Vec::new(),
            final_text: None,
            error: None,
        };
        // Replayable prefix: the recorded user prompt, then synthetic
        // intent/result pairs appended as the candidate walks. Synthetic
        // ids (`rp<n>`) are opaque on the wire; the rendered assistant
        // tool_calls and tool messages are otherwise identical to what
        // production sent (see the wire-equivalence test below).
        let mut transcript = vec![self.prompt_entry()];
        let mut consumed = vec![false; self.recorded_calls.len()];
        loop {
            if walk.steps >= budget {
                walk.error = Some(format!(
                    "step budget exhausted ({} steps, recorded run used {} tool calls)",
                    walk.steps,
                    self.recorded_calls.len()
                ));
                return walk;
            }
            let out = match provider.complete(&transcript) {
                Ok(o) => o,
                Err(e) => {
                    walk.error = Some(format!("provider error: {}", e.0));
                    return walk;
                }
            };
            walk.steps += 1;
            match out {
                ProviderOutput::Final { text } => {
                    walk.final_text = Some(text);
                    return walk;
                }
                ProviderOutput::InvalidToolArgs { tool, raw, .. } => {
                    // The candidate produced a malformed call the loop
                    // would have rejected: count it and stop — the live
                    // run has no recorded outcome to feed back.
                    walk.calls.push((tool, Value::String(raw)));
                    walk.error = Some("invalid tool arguments (loop would reject)".into());
                    return walk;
                }
                ProviderOutput::ToolCall { tool, input } => {
                    let Some(m) = self.match_call(&tool, &input, &mut consumed) else {
                        let err = format!("path divergence: no recorded outcome for `{tool}` call");
                        walk.calls.push((tool, input));
                        walk.error = Some(err);
                        return walk;
                    };
                    let intent_id = format!("rp{}_{}", walk.steps, tool);
                    let result_id = format!("rpx{}_{}", walk.steps, tool);
                    transcript.push(Self::synth_intent(&intent_id, &tool, &input));
                    transcript.push(Self::synth_settlement(
                        &result_id,
                        &intent_id,
                        m.output.clone(),
                        m.is_error,
                    ));
                    walk.calls.push((tool, input));
                }
            }
        }
    }

    /// Score one candidate walk against the recorded outcome. A walk that
    /// ended in an error/divergence scores ZERO regardless of its prefix
    /// agreement: dreaming is exact only INSIDE the recorded world, so
    /// credit past the divergence point would be fiction
    /// (arXiv:2609.14858 — the simulator covers the realized search space
    /// and nothing beyond it). `path_agreement` stays for diagnosis.
    pub fn score(&self, walk: ReplayWalk) -> ReplayScore {
        let matched = self
            .recorded_calls
            .iter()
            .zip(walk.calls.iter())
            .filter(|(r, c)| r.tool == c.0 && r.input == c.1)
            .count();
        let denom = self.recorded_calls.len().max(walk.calls.len()).max(1);
        let agreement = matched as f64 / denom as f64;
        let final_ok = walk.error.is_none()
            && walk
                .final_text
                .as_deref()
                .map(|t| {
                    t.trim()
                        .eq_ignore_ascii_case(self.recorded_final_text.trim())
                })
                .unwrap_or(false);
        let score = if walk.error.is_some() {
            0.0
        } else {
            0.7 * agreement + 0.3 * f64::from(final_ok)
        };
        ReplayScore {
            walk,
            path_agreement: agreement,
            final_ok,
            score,
        }
    }

    /// Run and score in one call.
    pub fn score_candidate(&self, provider: &mut dyn Provider) -> ReplayScore {
        self.score(self.replay(provider))
    }

    fn prompt_entry(&self) -> Entry {
        let mut e = self.user_entry.clone();
        e.payload["text"] = Value::String(self.prompt.clone());
        e
    }

    fn synth_intent(id: &str, tool: &str, input: &Value) -> Entry {
        Entry {
            seq: 0,
            id: id.to_string(),
            parent_id: None,
            kind: EntryType::new(EntryType::INTENT),
            timestamp: 0,
            payload: serde_json::json!({ "input": input, "replay": "guarded", "tool": tool }),
        }
    }

    fn synth_settlement(id: &str, parent: &str, output: Value, is_error: bool) -> Entry {
        let (kind, payload) = if is_error {
            (EntryType::ERROR, serde_json::json!({ "error": output }))
        } else {
            (
                EntryType::TOOL_RESULT,
                serde_json::json!({ "ok": true, "output": output }),
            )
        };
        Entry {
            seq: 0,
            id: id.to_string(),
            parent_id: Some(parent.to_string()),
            kind: EntryType::new(kind),
            timestamp: 0,
            payload,
        }
    }

    fn match_call<'a>(
        &'a self,
        tool: &str,
        input: &Value,
        consumed: &mut [bool],
    ) -> Option<&'a RecordedCall> {
        // Exact (tool, input) match first…
        for (i, c) in self.recorded_calls.iter().enumerate() {
            if !consumed[i] && c.tool == tool && c.input == *input {
                consumed[i] = true;
                return Some(c);
            }
        }
        // …then the oldest unconsumed call to the same tool (the candidate
        // reordered or retried onto the same tool).
        for (i, c) in self.recorded_calls.iter().enumerate() {
            if !consumed[i] && c.tool == tool {
                consumed[i] = true;
                return Some(c);
            }
        }
        None
    }
}

/// Pair each intent with the settlement that follows it (intent →
/// tool_result/error). Unpaired tails are dropped.
fn interleave_calls(entries: &[Entry]) -> Vec<RecordedCall> {
    let mut calls = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        let e = &entries[i];
        if e.kind == EntryType::INTENT {
            let tool = e
                .payload
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let input = e.payload.get("input").cloned().unwrap_or(Value::Null);
            let settlement = entries[i + 1..]
                .iter()
                .find(|s| s.parent_id.as_deref() == Some(e.id.as_str()));
            let (output, is_error) = match settlement {
                Some(s) if s.kind == EntryType::TOOL_RESULT => (
                    s.payload.get("output").cloned().unwrap_or(Value::Null),
                    false,
                ),
                Some(s) if s.kind == EntryType::ERROR => {
                    (s.payload.get("error").cloned().unwrap_or(Value::Null), true)
                }
                _ => (Value::Null, false),
            };
            calls.push(RecordedCall {
                tool,
                input,
                output,
                is_error,
            });
        }
        i += 1;
    }
    calls
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockProvider;
    use crate::openai::{OpenAiConfig, OpenAiProvider};
    use crate::provider::{Provider, ProviderError};
    use serde_json::json;
    use std::io::Write;

    const SAMPLE: &str = concat!(
        r#"{"v":1,"kind":"header","id":"s-test","storageVersion":1,"created_at":1,"cwd":""}"#,
        "\n",
        r#"{"kind":"entry","seq":1,"id":"e_1","type":"message","timestamp":1,"payload":{"role":"user","text":"Do the echo mission"}}"#,
        "\n",
        r#"{"kind":"state","seq":2,"pc":"planning","snapshot":false}"#,
        "\n",
        r#"{"kind":"entry","seq":5,"id":"intent_4","type":"intent","timestamp":2,"payload":{"input":{"command":"echo step-1"},"replay":"guarded","tool":"run_command"}}"#,
        "\n",
        r#"{"kind":"entry","seq":8,"id":"e_8","parentId":"intent_4","type":"tool_result","timestamp":3,"payload":{"ok":true,"output":{"status":0,"stderr":"","stdout":"step-1\n"}}}"#,
        "\n",
        r#"{"kind":"entry","seq":22,"id":"e_22","type":"message","timestamp":4,"payload":{"role":"assistant","text":"CHAIN-OK"}}"#,
        "\n",
    );

    /// Writes SAMPLE into a unique tempdir and returns (path, dir).
    fn sample_file(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("tole-replay-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s-test.jsonl");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(SAMPLE.as_bytes()).unwrap();
        (p, dir)
    }

    fn parsed(tag: &str) -> MissionTrace {
        let (p, dir) = sample_file(tag);
        let t = MissionTrace::parse(&p, "echo_chain", "test-model").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        t
    }

    #[test]
    fn parse_extracts_prompt_calls_and_final() {
        let (p, dir) = sample_file("parse");
        let t = MissionTrace::parse(&p, "echo_chain", "test-model").unwrap();
        assert_eq!(t.session_id, "s-test");
        assert_eq!(t.mission, "echo_chain");
        assert_eq!(t.model, "test-model");
        assert_eq!(t.prompt, "Do the echo mission");
        assert_eq!(t.recorded_calls.len(), 1);
        assert_eq!(t.recorded_calls[0].tool, "run_command");
        assert_eq!(t.recorded_calls[0].input["command"], "echo step-1");
        assert_eq!(t.recorded_calls[0].output["stdout"], "step-1\n");
        assert!(!t.recorded_calls[0].is_error);
        assert_eq!(t.recorded_final_text, "CHAIN-OK");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_rejects_trace_without_user_message() {
        let dir = std::env::temp_dir().join(format!("tole-replay-{}-empty", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s-bad.jsonl");
        std::fs::write(&p, "{\"kind\":\"header\",\"id\":\"s-bad\"}\n").unwrap();
        assert!(MissionTrace::parse(&p, "m", "mm").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_handles_batched_array_lines() {
        // One physical line = one commit, which JsonlStorage may write as
        // a JSON ARRAY of records (durable_metrics in run.py already
        // handles this shape — the Rust parser must too).
        let dir = std::env::temp_dir().join(format!("tole-replay-{}-arr", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s-arr.jsonl");
        let batched = format!(
            "[{},{}]",
            r#"{"kind":"entry","seq":1,"id":"e_1","type":"message","timestamp":1,"payload":{"role":"user","text":"array mission"}}"#,
            r#"{"kind":"state","seq":2,"pc":"planning","snapshot":false}"#,
        );
        let second = r#"{"kind":"entry","seq":22,"id":"e_22","type":"message","timestamp":4,"payload":{"role":"assistant","text":"DONE"}}"#;
        std::fs::write(&p, format!("{batched}\n{second}\n")).unwrap();
        let t = MissionTrace::parse(&p, "echo_chain", "test-model").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(t.prompt, "array mission");
        assert_eq!(t.recorded_final_text, "DONE");
    }

    /// Records every transcript it is handed so tests can assert the
    /// synthetic wire shape the replay walker builds.
    struct CaptureProvider {
        script: std::collections::VecDeque<ProviderOutput>,
        transcripts: Vec<Vec<Entry>>,
    }

    impl CaptureProvider {
        fn scripted(steps: Vec<ProviderOutput>) -> Self {
            Self {
                script: steps.into_iter().collect(),
                transcripts: Vec::new(),
            }
        }
    }

    impl Provider for CaptureProvider {
        fn complete(&mut self, transcript: &[Entry]) -> Result<ProviderOutput, ProviderError> {
            self.transcripts.push(transcript.to_vec());
            self.script
                .pop_front()
                .ok_or_else(|| ProviderError("script exhausted".into()))
        }
    }

    #[test]
    fn synthetic_pairs_render_like_production_on_the_wire() {
        let t = parsed("wire");
        let mut provider = CaptureProvider::scripted(vec![
            ProviderOutput::ToolCall {
                tool: "run_command".into(),
                input: json!({"command": "echo step-1"}),
            },
            ProviderOutput::Final {
                text: "CHAIN-OK".into(),
            },
        ]);
        let walk = t.replay(&mut provider);
        assert!(walk.error.is_none(), "{:?}", walk.error);
        assert_eq!(walk.final_text.as_deref(), Some("CHAIN-OK"));
        assert_eq!(walk.steps, 2);
        // Last handed transcript = prefix + one synthetic intent/result pair.
        let last = provider.transcripts.last().unwrap();
        assert_eq!(last.len(), 3);
        assert_eq!(last[0].kind, "message");
        assert_eq!(last[1].kind, "intent");
        assert_eq!(last[1].payload["tool"], "run_command");
        assert_eq!(last[2].kind, "tool_result");
        assert_eq!(last[2].parent_id.as_deref(), Some(last[1].id.as_str()));
        assert_eq!(last[2].payload["output"]["stdout"], "step-1\n");
        // The rendered wire body matches what production would send for
        // the same shape (assistant tool_call + answering tool message).
        let prov = OpenAiProvider::new(OpenAiConfig::new("http://x", "m", "k"));
        let body = prov.request_body(last);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "Do the echo mission");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["tool_calls"][0]["function"]["name"], "run_command");
        assert_eq!(
            msgs[1]["tool_calls"][0]["function"]["arguments"],
            json!({"command": "echo step-1"}).to_string()
        );
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], last[1].id.as_str());
        assert!(msgs[2]["content"].as_str().unwrap().contains("step-1"));
    }

    #[test]
    fn realized_path_scores_perfect_and_degraded_scores_lower() {
        let t = parsed("score");
        let mut faithful = MockProvider::scripted(vec![
            ProviderOutput::ToolCall {
                tool: "run_command".into(),
                input: json!({"command": "echo step-1"}),
            },
            ProviderOutput::Final {
                text: "CHAIN-OK".into(),
            },
        ]);
        let good = t.score_candidate(&mut faithful);
        assert!((good.score - 1.0).abs() < 1e-9);
        assert!(good.final_ok);
        assert!((good.path_agreement - 1.0).abs() < 1e-9);

        // Degraded candidate: answers immediately without the recorded
        // tool path. Final still "right", path agreement zero.
        let mut lazy = MockProvider::scripted(vec![ProviderOutput::Final {
            text: "CHAIN-OK".into(),
        }]);
        let poor = t.score_candidate(&mut lazy);
        assert!((poor.path_agreement - 0.0).abs() < 1e-9);
        assert!(poor.final_ok);
        assert!(poor.score < good.score);
    }

    #[test]
    fn divergence_is_surfaced_not_hidden() {
        let t = parsed("diverge");
        let mut wild = MockProvider::scripted(vec![ProviderOutput::ToolCall {
            tool: "write_file".into(),
            input: json!({"path": "x", "text": "y"}),
        }]);
        let s = t.score_candidate(&mut wild);
        assert!(
            s.walk.error.as_deref().unwrap().contains("path divergence"),
            "{:?}",
            s.walk.error
        );
        assert!(!s.final_ok);
        assert!((s.score - 0.0).abs() < 1e-9);
    }

    #[test]
    fn exact_input_match_wins_over_oldest_same_tool_fallback() {
        // Two recorded run_command calls with different inputs.
        let dir = std::env::temp_dir().join(format!("tole-replay-{}-two", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s-two.jsonl");
        let two = concat!(
            r#"{"kind":"entry","seq":1,"id":"e_1","type":"message","timestamp":1,"payload":{"role":"user","text":"chain"}}"#,
            "\n",
            r#"{"kind":"entry","seq":5,"id":"i1","type":"intent","timestamp":2,"payload":{"input":{"command":"echo step-1"},"replay":"guarded","tool":"run_command"}}"#,
            "\n",
            r#"{"kind":"entry","seq":8,"id":"r1","parentId":"i1","type":"tool_result","timestamp":3,"payload":{"ok":true,"output":{"stdout":"step-1\n"}}}"#,
            "\n",
            r#"{"kind":"entry","seq":14,"id":"i2","type":"intent","timestamp":4,"payload":{"input":{"command":"echo step-2"},"replay":"guarded","tool":"run_command"}}"#,
            "\n",
            r#"{"kind":"entry","seq":17,"id":"r2","parentId":"i2","type":"tool_result","timestamp":5,"payload":{"ok":true,"output":{"stdout":"step-2\n"}}}"#,
            "\n",
        );
        std::fs::write(&p, two).unwrap();
        let t = MissionTrace::parse(&p, "echo_chain", "test-model").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(t.recorded_calls.len(), 2);
        // Candidate calls step-2 FIRST: the exact (tool, input) match must
        // bind it to the step-2 outcome, not the oldest step-1 one.
        let mut provider = CaptureProvider::scripted(vec![
            ProviderOutput::ToolCall {
                tool: "run_command".into(),
                input: json!({"command": "echo step-2"}),
            },
            ProviderOutput::Final {
                text: "CHAIN-OK".into(),
            },
        ]);
        let walk = t.replay(&mut provider);
        assert!(walk.error.is_none(), "{:?}", walk.error);
        let last = provider.transcripts.last().unwrap();
        // The settlement fed back for step-2 carries step-2's stdout.
        assert_eq!(last[2].payload["output"]["stdout"], "step-2\n");
    }
}
