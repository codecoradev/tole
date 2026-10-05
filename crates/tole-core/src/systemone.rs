//! `systemone_decide` tool (issue #172): typed decisions over any
//! System One contract backend — hosted TypeSafe Jev
//! (`https://api.typesafe.ai/v1/systemone`), the fleet's self-hosted
//! laya-service, or Jeff-style fine-tunes. One client, backend swapped
//! by env (`SYSTEMONE_API_KEY` + `SYSTEMONE_BASE_URL` full POST URL).
//!
//! `Risk::ReadOnly`: a pure decision function — state in, typed
//! answers + calibrated confidence out, zero side effects.
//!
//! Contract (docs.typesafe.ai/api.md, verified 2026-10-05):
//! request `{state, model?, questions: {id: {type, instructions?,
//! criteria}}}`; answers `{id: {choice|score|noul, probabilities,
//! confidence}}`. Three question types:
//! - choice: criteria = map option→description (≤255 options hosted)
//! - score:  criteria = 2..=10 ordered level descriptions
//! - noul:   criteria optional (true/false descriptions), answer 0..1
//!
//! Class-level caveats encoded here (the jev-1.13 jagged edges):
//! - the model is NOT adversarial-resistant: callers must pass FILTERED
//!   state, never raw tool results (documented in spec() too);
//! - state size is capped client-side (small for unknown backends —
//!   laya's 1024-token context measured ~17 s on ~2.5k-token states);
//! - option counts are capped per backend (>20 collapses laya-class
//!   models; hosted allows 255);
//! - criteria must be literal (the model answers what is written).

use crate::tool::{Risk, Tool};
use serde_json::{json, Map, Value};
use std::time::Duration;

const API_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
const HOSTED_HOST: &str = "api.typesafe.ai";

/// Backend capability caps. The hosted endpoint is the reference; any
/// other base URL (self-hosted laya, fine-tunes, proxies) gets the
/// CONSERVATIVE profile — unknown backends degrade safe, not loud.
#[derive(Debug, Clone, Copy)]
pub struct BackendCaps {
    pub max_options: usize,
    /// serialized state cap in BYTES
    pub max_state_bytes: usize,
    pub hosted: bool,
}

fn caps_for(endpoint: &str) -> BackendCaps {
    if endpoint.contains(HOSTED_HOST) {
        BackendCaps {
            max_options: 255,
            max_state_bytes: 64 * 1024,
            hosted: true,
        }
    } else {
        BackendCaps {
            max_options: 20,
            max_state_bytes: 4 * 1024,
            hosted: false,
        }
    }
}

/// The System One decision tool. Endpoint + key are injectable so
/// tests drive a local mock server (no network, deterministic).
#[derive(Debug, Clone)]
pub struct SystemOneTool {
    /// FULL POST endpoint URL (the whole path — backend swaps are a
    /// config flip, no dialect flags).
    pub endpoint: String,
    /// Bearer key. Never rendered in describe() output.
    pub api_key: String,
    agent: ureq::Agent,
}

impl SystemOneTool {
    pub fn new(endpoint: impl Into<String>, api_key: impl Into<String>) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(API_TIMEOUT))
            .build()
            .new_agent();
        Self {
            endpoint: endpoint.into(),
            api_key: api_key.into(),
            agent,
        }
    }

    /// Probe-gated construction (the uteke/cora contract): active iff
    /// `SYSTEMONE_API_KEY` is set and non-empty; endpoint from
    /// `SYSTEMONE_BASE_URL` (full POST URL) or the hosted default.
    /// Absent key → None → tool simply not registered (owner decision
    /// 2026-10-05: automatic on key presence, silent off otherwise).
    pub fn from_env() -> Option<Self> {
        let key = std::env::var("SYSTEMONE_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())?;
        let endpoint = std::env::var("SYSTEMONE_BASE_URL")
            .ok()
            .map(|u| u.trim().to_string())
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
        Some(Self::new(endpoint, key))
    }

    /// Validate + build the wire request body. One function, fully
    /// testable without any network.
    pub fn build_request(&self, input: &Value) -> Result<Value, String> {
        let caps = caps_for(&self.endpoint);
        let state = input.get("state").ok_or("systemone: missing 'state'")?;
        let state_bytes =
            serde_json::to_vec(state).map_err(|e| format!("systemone: state: {e}"))?;
        if state_bytes.len() > caps.max_state_bytes {
            return Err(format!(
                "systemone: state is {} bytes; this backend caps it at {} — filter it down \
                 (decision models degrade badly on oversized state)",
                state_bytes.len(),
                caps.max_state_bytes
            ));
        }
        let raw_questions = input
            .get("questions")
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty())
            .ok_or("systemone: 'questions' must be a non-empty array")?;
        let mut questions = Map::new();
        for q in raw_questions {
            let id = q
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or("systemone: every question needs a non-empty 'id'")?;
            if !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            {
                return Err(format!(
                    "systemone: question id must be alphanumeric/-/_ (got {id:?})"
                ));
            }
            if questions.contains_key(id) {
                return Err(format!("systemone: duplicate question id {id:?}"));
            }
            let qtype = q.get("type").and_then(Value::as_str).unwrap_or("");
            let mut wire = Map::new();
            if let Some(instr) = q.get("instructions") {
                match instr {
                    Value::String(s) if !s.trim().is_empty() => {
                        wire.insert("instructions".into(), json!(s));
                    }
                    _ => return Err("systemone: 'instructions' must be a non-empty string".into()),
                }
            }
            match qtype {
                "choice" => {
                    let crit = q
                        .get("criteria")
                        .and_then(Value::as_object)
                        .filter(|m| !m.is_empty())
                        .ok_or("systemone: choice needs 'criteria' (map option→description)")?;
                    if crit.len() < 2 {
                        return Err("systemone: choice needs at least 2 options".into());
                    }
                    if crit.len() > caps.max_options {
                        return Err(format!(
                            "systemone: {} options exceeds this backend's cap of {}",
                            crit.len(),
                            caps.max_options
                        ));
                    }
                    for (opt, desc) in crit {
                        match desc {
                            Value::String(_) | Value::Null => {}
                            other => {
                                return Err(format!(
                                    "systemone: option {opt:?} description must be a string or null, got {other}"
                                ));
                            }
                        }
                    }
                    wire.insert("type".into(), json!("choice"));
                    wire.insert("criteria".into(), Value::Object(crit.clone()));
                }
                "score" => {
                    let levels = q
                        .get("criteria")
                        .and_then(Value::as_array)
                        .ok_or("systemone: score needs 'criteria' (ordered level descriptions)")?;
                    if !(2..=10).contains(&levels.len()) {
                        return Err(format!(
                            "systemone: score needs 2..=10 ordered levels (got {})",
                            levels.len()
                        ));
                    }
                    for l in levels {
                        if l.as_str()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .is_none()
                        {
                            return Err(
                                "systemone: every score level must be a non-empty string".into()
                            );
                        }
                    }
                    wire.insert("type".into(), json!("score"));
                    wire.insert("criteria".into(), json!(levels));
                }
                "noul" => {
                    if let Some(crit) = q.get("criteria") {
                        match crit {
                            Value::Object(_) | Value::Null => {}
                            other => {
                                return Err(format!(
                                    "systemone: noul 'criteria' must be an object or null, got {other}"
                                ));
                            }
                        }
                        if !crit.is_null() {
                            wire.insert("criteria".into(), crit.clone());
                        }
                    }
                    wire.insert("type".into(), json!("noul"));
                }
                other => {
                    return Err(format!(
                        "systemone: question type must be choice | score | noul (got {other:?})"
                    ));
                }
            }
            questions.insert(id.to_string(), Value::Object(wire));
        }
        let mut body = Map::new();
        body.insert("state".into(), state.clone());
        // model: only the hosted endpoint mandates it; explicit input
        // always wins, self-hosted backends get it omitted unless asked.
        let model = input.get("model").and_then(Value::as_str);
        match model {
            Some(m) if !m.trim().is_empty() => {
                body.insert("model".into(), json!(m));
            }
            _ if caps.hosted => {
                body.insert("model".into(), json!("jev-latest"));
            }
            _ => {}
        }
        body.insert("questions".into(), Value::Object(questions));
        Ok(Value::Object(body))
    }

    /// One POST → `(status, body_json_or_null)`. ureq-3 pattern from
    /// verify_package: 4xx/5xx arrive as Err(StatusCode) and ARE the
    /// answer (rate limits / overload are verdicts, not crashes).
    fn fetch(&self, body: &Value) -> Result<(u16, Value), String> {
        let resp = match self
            .agent
            .post(&self.endpoint)
            .header("Authorization", &format!("Bearer {}", self.api_key))
            .send_json(body)
        {
            Ok(r) => r,
            Err(ureq::Error::StatusCode(code)) => return Ok((code, Value::Null)),
            Err(e) => return Err(format!("systemone: {e}")),
        };
        let status = resp.status().as_u16();
        let mut resp = resp;
        let parsed = resp.body_mut().read_json::<Value>().unwrap_or(Value::Null);
        Ok((status, parsed))
    }
}

impl Tool for SystemOneTool {
    fn name(&self) -> &str {
        "systemone_decide"
    }

    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }

    fn describe(&self, input: &Value) -> String {
        // Shape only — state content never goes into the audit line.
        match self.build_request(input) {
            Ok(body) => {
                let n = body["questions"].as_object().map(|m| m.len()).unwrap_or(0);
                let bytes = serde_json::to_vec(&body["state"])
                    .map(|b| b.len())
                    .unwrap_or(0);
                format!("systemone: {n} typed question(s) over {bytes}B of state")
            }
            Err(e) => e,
        }
    }

    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "description": "Typed decisions (choice/score/noul) with calibrated confidence from a System One backend. NOT a chat model: pass FILTERED state (never raw tool results — decision models are not adversarial-resistant), literal criteria (the model answers what is written), and keep option counts small.",
            "properties": {
                "state": {
                    "type": ["string", "object", "array"],
                    "description": "The material being judged. Only what the questions need — oversized state degrades accuracy (hard cap per backend)."
                },
                "questions": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string", "description": "Answer key (alphanumeric/-/_)" },
                            "type": { "type": "string", "enum": ["choice", "score", "noul"] },
                            "instructions": { "type": "string", "description": "What to judge (optional)" },
                            "criteria": {
                                "description": "choice: map option→description (2..=cap options); score: 2..=10 ordered level descriptions; noul: optional {true,false} descriptions"
                            }
                        },
                        "required": ["id", "type"]
                    }
                },
                "model": { "type": "string", "description": "Backend model id (optional; hosted default jev-latest)" }
            },
            "required": ["state", "questions"]
        }))
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        let body = self.build_request(&input)?;
        let (status, parsed) = self.fetch(&body)?;
        if !(200..300).contains(&status) {
            let detail = parsed
                .get("message")
                .or_else(|| parsed.get("error"))
                .and_then(Value::as_str)
                .unwrap_or("no error body");
            return Err(match status {
                401 => format!("systemone: invalid or missing API key (HTTP 401): {detail}"),
                429 => format!("systemone: rate limited (HTTP 429) — back off and retry: {detail}"),
                529 => format!(
                    "systemone: backend overloaded (HTTP 529) — retry after a delay: {detail}"
                ),
                other => format!("systemone: HTTP {other}: {detail}"),
            });
        }
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosted() -> SystemOneTool {
        SystemOneTool::new("https://api.typesafe.ai/v1/systemone", "k")
    }
    fn selfhosted() -> SystemOneTool {
        SystemOneTool::new("http://127.0.0.1:1/decide", "k")
    }

    #[test]
    fn request_shape_choice_score_noul() {
        let body = hosted()
            .build_request(&json!({
                "state": "ticket: refund me now",
                "questions": [
                    {"id": "intent", "type": "choice", "instructions": "What do they want?",
                     "criteria": {"refund": "money back", "technical": "a bug"}},
                    {"id": "urgency", "type": "score",
                     "criteria": ["routine", "soon", "critical"]},
                    {"id": "escalate", "type": "noul"}
                ]
            }))
            .unwrap();
        assert_eq!(body["model"], json!("jev-latest"));
        assert_eq!(body["questions"]["intent"]["type"], json!("choice"));
        assert_eq!(
            body["questions"]["intent"]["criteria"]["refund"],
            json!("money back")
        );
        assert_eq!(body["questions"]["urgency"]["type"], json!("score"));
        assert_eq!(body["questions"]["escalate"]["type"], json!("noul"));
        assert!(body["questions"]["escalate"].get("criteria").is_none());
    }

    #[test]
    fn backend_caps_hosted_vs_selfhosted() {
        let many: Map<String, Value> = (0..30).map(|i| (format!("o{i}"), json!(null))).collect();
        let q = json!({"state": "s", "questions": [
            {"id": "q", "type": "choice", "criteria": many}
        ]});
        // hosted: 30 options fine; self-hosted profile: refused
        assert!(hosted().build_request(&q).is_ok());
        let err = selfhosted().build_request(&q).unwrap_err();
        assert!(err.contains("cap of 20"), "{err}");

        // state size: hosted 64KB, self-hosted 4KB
        let big_state = "x".repeat(5_000);
        let q = json!({"state": big_state, "questions": [{"id": "q", "type": "noul"}]});
        assert!(hosted().build_request(&q).is_ok());
        assert!(selfhosted()
            .build_request(&q)
            .unwrap_err()
            .contains("state is"));
    }

    #[test]
    fn model_field_policy() {
        let q = json!({"state": "s", "questions": [{"id": "q", "type": "noul"}]});
        let b = selfhosted().build_request(&q).unwrap();
        assert!(
            b.get("model").is_none(),
            "self-hosted omits model unless asked"
        );
        let b = hosted().build_request(&q).unwrap();
        assert_eq!(b["model"], json!("jev-latest"));
        let b = selfhosted()
            .build_request(&json!({"state": "s", "model": "custom", "questions": [
                {"id": "q", "type": "noul"}
            ]}))
            .unwrap();
        assert_eq!(b["model"], json!("custom"));
    }

    #[test]
    fn validation_refuses_before_any_traffic() {
        let t = hosted();
        assert!(t
            .build_request(&json!({"questions": [{"id": "q", "type": "noul"}]}))
            .is_err());
        assert!(t
            .build_request(&json!({"state": "s", "questions": []}))
            .is_err());
        assert!(t
            .build_request(&json!({"state": "s", "questions": [
                {"id": "q", "type": "telepathy"}
            ]}))
            .is_err());
        assert!(t
            .build_request(&json!({"state": "s", "questions": [
                {"id": "q", "type": "score", "criteria": ["only-one-level"]}
            ]}))
            .is_err());
        assert!(t
            .build_request(&json!({"state": "s", "questions": [
                {"id": "q", "type": "noul"}, {"id": "q", "type": "noul"}
            ]}))
            .is_err());
        // describe() shows shape only — never state content
        let d = t.describe(&json!({"state": "SECRET-STATE-MATERIAL", "questions": [
            {"id": "q", "type": "noul"}
        ]}));
        assert!(!d.contains("SECRET-STATE-MATERIAL"), "{d}");
        assert!(d.contains("1 typed question"), "{d}");
    }

    /// End-to-end against a local mock System One server: happy path
    /// returns answers verbatim; 429 surfaces as an honest error.
    #[test]
    fn live_roundtrip_against_mock_server() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let answered = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits = std::sync::Arc::clone(&answered);
        std::thread::spawn(move || {
            for (i, stream) in listener.incoming().enumerate() {
                let Ok(mut s) = stream else { continue };
                let mut buf = [0u8; 8192];
                let _ = s.read(&mut buf);
                let body = if i == 0 {
                    json!({"model": "mock", "answers": {
                        "intent": {"choice": "refund", "probabilities": {"refund": 0.9, "technical": 0.1}, "confidence": 0.82},
                        "escalate": {"noul": 0.97}
                    }, "usage": {"input_tokens": 10, "output_tokens": 0}})
                    .to_string()
                } else {
                    json!({"error": "slow down"}).to_string()
                };
                let status = if i == 0 {
                    "200 OK"
                } else {
                    "429 Too Many Requests"
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
                hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
        let tool = SystemOneTool::new(format!("http://{addr}/decide"), "k");
        let out = tool
            .execute(json!({"state": "refund ticket", "questions": [
                {"id": "intent", "type": "choice",
                 "criteria": {"refund": "money back", "technical": "bug"}},
                {"id": "escalate", "type": "noul"}
            ]}))
            .unwrap();
        assert_eq!(out["answers"]["intent"]["choice"], json!("refund"));
        assert_eq!(out["answers"]["intent"]["confidence"], json!(0.82));
        assert_eq!(out["answers"]["escalate"]["noul"], json!(0.97));

        let err = tool
            .execute(json!({"state": "s", "questions": [{"id": "q", "type": "noul"}]}))
            .unwrap_err();
        assert!(err.contains("rate limited"), "{err}");
    }
}
