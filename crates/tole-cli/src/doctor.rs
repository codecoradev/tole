//! `tole doctor` — tiered provider diagnostics and a persisted coverage
//! ledger (#378).
//!
//! One command answers "is provider/model X actually usable?":
//!
//! - **offline** — wiring only (config resolves, base URL shape, key
//!   present). No network.
//! - **catalog** — live `GET /models`; the configured model must be served
//!   (when the gateway lists models at all).
//! - **full** — three real calls through the real turn loop: a plain chat
//!   completion, a streamed one, and a tool-call loop (the model must call
//!   a probe tool and the loop must settle). This is the release gate: a
//!   `full` green is the precondition for "ready to release" claims.
//!
//! Tiers are cumulative and stop at the first blocker. Every run is
//! recorded in a ledger on disk (per provider + model: level reached, first
//! blocker, fix command, timestamp); [`Ledger::summary`] renders the
//! `READY n-of-m` line.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tole_core::approval::{Approver, ToolRequest, Verdict};
use tole_core::openai::{fetch_model_ids, OpenAiConfig, OpenAiProvider, ENV_PREFIXES};
use tole_core::storage::JsonlStorage;
use tole_core::tool::{Risk, Tool, ToolRegistry};
use tole_core::turn::{run_turn, TurnOutcome};

/// Ledger file name under the tole data dir.
pub const LEDGER_FILE: &str = "doctor-ledger.json";

/// Per-call timeout of the `full` tier.
const FULL_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Marker the probe tool must receive back from the model (not a secret).
const PROBE_MARKER: &str = "doctor-marker-7f3a";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Offline,
    Catalog,
    Full,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Offline => "offline",
            Tier::Catalog => "catalog",
            Tier::Full => "full",
        }
    }

    pub fn parse(s: &str) -> Option<Tier> {
        match s {
            "offline" => Some(Tier::Offline),
            "catalog" => Some(Tier::Catalog),
            "full" => Some(Tier::Full),
            _ => None,
        }
    }
}

/// The first thing that stopped a run, with the command that fixes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blocker {
    pub tier: Tier,
    pub check: &'static str,
    pub message: String,
    pub fix: String,
}

/// Result of one diagnosis run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// Highest tier fully passed (`None` = not even offline).
    pub reached: Option<Tier>,
    pub blocker: Option<Blocker>,
    /// Human-readable per-check lines (`✓`/`✗`).
    pub lines: Vec<String>,
}

impl Outcome {
    fn new() -> Self {
        Outcome {
            reached: None,
            blocker: None,
            lines: Vec::new(),
        }
    }

    fn pass(&mut self, line: String) {
        self.lines.push(format!("  ✓ {line}"));
    }

    fn fail(&mut self, tier: Tier, check: &'static str, message: String, fix: String) {
        self.lines.push(format!("  ✗ {check}: {message}"));
        self.blocker = Some(Blocker {
            tier,
            check,
            message,
            fix,
        });
    }
}

/// Env var names per prefix, in resolution order, for "what is missing".
fn missing_vars(env: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    let set = |name: &str| env(name).is_some_and(|v| !v.trim().is_empty());
    // The prefix with the most pieces set is the one the user is working
    // on; list what it still lacks. Nothing set → the canonical TOLE_*.
    let count = |p: &str| {
        ["BASE_URL", "MODEL", "API_KEY"]
            .iter()
            .filter(|s| set(&format!("{p}_{s}")))
            .count()
    };
    // First prefix wins ties, so TOLE_* (canonical) is the default.
    let best = ENV_PREFIXES
        .iter()
        .copied()
        .fold(("TOLE", 0usize), |acc, p| {
            let c = count(p);
            if c > acc.1 {
                (p, c)
            } else {
                acc
            }
        })
        .0;
    ["BASE_URL", "MODEL", "API_KEY"]
        .iter()
        .map(|s| format!("{best}_{s}"))
        .filter(|n| !set(n))
        .collect()
}

/// Cheap shape check: `http(s)://<host>[...]`, no whitespace.
fn base_url_ok(url: &str) -> bool {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    match rest {
        Some(r) => !r.is_empty() && !r.starts_with('/') && !url.chars().any(char::is_whitespace),
        None => false,
    }
}

struct DenyAll;
impl Approver for DenyAll {
    fn decide(&self, _req: &ToolRequest<'_>) -> Verdict {
        Verdict::Deny
    }
}

/// ReadOnly probe the model must call in the `full` tier.
struct ProbeTool {
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    saw_token: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Tool for ProbeTool {
    fn name(&self) -> &str {
        "doctor_probe"
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    fn summary(&self) -> String {
        "Diagnostic probe: echoes the given marker back. Call it exactly once when asked.".into()
    }
    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {"marker": {"type": "string", "description": "The marker to echo."}},
            "required": ["marker"]
        }))
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        use std::sync::atomic::Ordering;
        self.calls.fetch_add(1, Ordering::SeqCst);
        if input["marker"].as_str() == Some(PROBE_MARKER) {
            self.saw_token.store(true, Ordering::SeqCst);
        }
        Ok(json!({ "echo": input["marker"], "ok": true }))
    }
}

fn scratch_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir =
        std::env::temp_dir().join(format!("tole-doctor-{}-{tag}-{nanos}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Run one prompt through the real turn loop against the real provider.
fn drive(
    cfg: &OpenAiConfig,
    streaming: bool,
    registry: &ToolRegistry,
    prompt: &str,
    tag: &str,
) -> Result<TurnOutcome, String> {
    let dir = scratch_dir(tag);
    let result = (|| {
        let mut storage = JsonlStorage::create(&dir, format!("doctor-{tag}"), None)
            .map_err(|e| format!("cannot create the scratch session: {e}"))?;
        let mut provider = OpenAiProvider::new(cfg.clone())
            .with_streaming(streaming)
            .with_timeout(FULL_CALL_TIMEOUT)
            .with_tool_specs(registry.specs());
        run_turn(&mut storage, &mut provider, registry, prompt).map_err(|e| e.to_string())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn describe_outcome(o: &TurnOutcome) -> String {
    match o {
        TurnOutcome::Final { .. } => "final answer".into(),
        TurnOutcome::ProviderFailed { message } => format!("provider failed: {message}"),
        TurnOutcome::UnknownTool { name } => format!("model asked for unknown tool {name:?}"),
        TurnOutcome::LoopDetected { tool, count } => {
            format!("model repeated {tool:?} {count} times")
        }
        TurnOutcome::BudgetExhausted => "step budget exhausted".into(),
        TurnOutcome::ApprovalRequired { name } => format!("tool {name:?} needed approval"),
        other => format!("unexpected turn outcome: {other:?}"),
    }
}

/// One plain-text check (`chat` or `stream`): a non-empty final answer.
fn text_check(cfg: &OpenAiConfig, streaming: bool, out: &mut Outcome) -> bool {
    let check: &'static str = if streaming { "stream" } else { "chat" };
    let reg = ToolRegistry::with_approver(DenyAll);
    match drive(
        cfg,
        streaming,
        &reg,
        "Reply with the single word PONG.",
        check,
    ) {
        Ok(TurnOutcome::Final { text, .. }) if !text.trim().is_empty() => {
            out.pass(format!("{check}: model answered"));
            true
        }
        Ok(TurnOutcome::Final { .. }) => {
            out.fail(
                Tier::Full,
                check,
                "the model returned an empty answer".into(),
                "tole run \"Reply with the single word PONG.\"".into(),
            );
            false
        }
        Ok(other) => {
            out.fail(
                Tier::Full,
                check,
                describe_outcome(&other),
                "tole run \"Reply with the single word PONG.\"".into(),
            );
            false
        }
        Err(e) => {
            out.fail(Tier::Full, check, e, "tole run \"ping\"".into());
            false
        }
    }
}

/// The tool-call loop: the model must call the probe with the token and
/// the loop must settle on a final answer afterwards.
fn tools_check(cfg: &OpenAiConfig, out: &mut Outcome) -> bool {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    let calls = Arc::new(AtomicUsize::new(0));
    let saw_token = Arc::new(AtomicBool::new(false));
    let mut reg = ToolRegistry::with_approver(DenyAll);
    let probe = ProbeTool {
        calls: Arc::clone(&calls),
        saw_token: Arc::clone(&saw_token),
    };
    if let Err(e) = reg.register(Box::new(probe)) {
        out.fail(
            Tier::Full,
            "tools",
            format!("cannot register the probe tool: {e}"),
            "report this as a tole bug".into(),
        );
        return false;
    }
    let prompt = format!(
        "Call the tool doctor_probe exactly once with {{\"marker\":\"{PROBE_MARKER}\"}}, \
         then reply with the word DONE."
    );
    let fix = "pick a model with tool-calling support: export TOLE_MODEL=<model>".to_string();
    match drive(cfg, true, &reg, &prompt, "tools") {
        Ok(TurnOutcome::Final { .. }) => {
            if calls.load(Ordering::SeqCst) == 0 {
                out.fail(
                    Tier::Full,
                    "tools",
                    "the model answered without calling the probe tool (tool calling not working)"
                        .into(),
                    fix,
                );
                false
            } else if !saw_token.load(Ordering::SeqCst) {
                out.fail(
                    Tier::Full,
                    "tools",
                    "the model called the probe tool with the wrong arguments".into(),
                    fix,
                );
                false
            } else {
                out.pass("tools: tool-call loop completed".into());
                true
            }
        }
        Ok(other) => {
            out.fail(Tier::Full, "tools", describe_outcome(&other), fix);
            false
        }
        Err(e) => {
            out.fail(Tier::Full, "tools", e, fix);
            false
        }
    }
}

/// `scheme://host[:port]/path` only: userinfo, query and fragment can carry
/// credentials and never belong in output or on disk.
pub fn redact_url(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (format!("{s}://"), r),
        None => (String::new(), url),
    };
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let (authority, path) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    let host = authority.rsplit('@').next().unwrap_or("");
    format!("{scheme}{host}{path}")
}

/// Strip the API key and any URL credentials from a diagnostic string.
fn sanitize(text: &str, cfg: &OpenAiConfig) -> String {
    // URL first: replacing the key first would break the URL match.
    let mut t = text.to_string();
    let shown = redact_url(&cfg.base_url);
    if shown != cfg.base_url {
        t = t.replace(&cfg.base_url, &shown);
    }
    if !cfg.api_key.is_empty() {
        t = t.replace(&cfg.api_key, "<redacted>");
    }
    t
}

/// Run the diagnosis up to `requested`, stopping at the first blocker.
/// `cfg` is the resolved provider (None = incomplete); `env` is only used
/// to name what is missing.
pub fn diagnose(
    cfg: Option<&OpenAiConfig>,
    env: &dyn Fn(&str) -> Option<String>,
    requested: Tier,
) -> Outcome {
    let mut out = diagnose_raw(cfg, env, requested);
    if let Some(cfg) = cfg {
        for l in &mut out.lines {
            *l = sanitize(l, cfg);
        }
        if let Some(b) = &mut out.blocker {
            b.message = sanitize(&b.message, cfg);
            b.fix = sanitize(&b.fix, cfg);
        }
    }
    out
}

fn diagnose_raw(
    cfg: Option<&OpenAiConfig>,
    env: &dyn Fn(&str) -> Option<String>,
    requested: Tier,
) -> Outcome {
    let mut out = Outcome::new();

    // ---- offline ----
    let Some(cfg) = cfg else {
        let missing = missing_vars(env);
        let fix = format!(
            "export {}",
            missing
                .iter()
                .map(|n| format!("{n}=<value>"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        out.fail(
            Tier::Offline,
            "config",
            format!(
                "provider is not configured (missing: {})",
                missing.join(", ")
            ),
            fix,
        );
        return out;
    };
    if !base_url_ok(&cfg.base_url) {
        out.fail(
            Tier::Offline,
            "base_url",
            format!("base URL {:?} is not an http(s) URL", cfg.base_url),
            "export TOLE_BASE_URL=https://<host>/v1".into(),
        );
        return out;
    }
    out.pass(format!("offline: {} @ {}", cfg.model, cfg.base_url));
    out.reached = Some(Tier::Offline);
    if requested == Tier::Offline {
        return out;
    }

    // ---- catalog ----
    match fetch_model_ids(&cfg.base_url, &cfg.api_key) {
        Err(e) => {
            out.fail(
                Tier::Catalog,
                "models",
                e,
                format!(
                    "curl -sS -H \"Authorization: Bearer $TOLE_API_KEY\" {}/models",
                    cfg.base_url.trim_end_matches('/')
                ),
            );
            return out;
        }
        Ok(ids) if ids.is_empty() => {
            out.pass("catalog: provider lists no models (model check skipped)".into());
        }
        Ok(ids) if ids.iter().any(|i| i == &cfg.model) => {
            out.pass(format!(
                "catalog: {} is served ({} models)",
                cfg.model,
                ids.len()
            ));
        }
        Ok(ids) => {
            let some: Vec<&str> = ids.iter().take(5).map(String::as_str).collect();
            out.fail(
                Tier::Catalog,
                "model",
                format!(
                    "{:?} is not served ({} models listed)",
                    cfg.model,
                    ids.len()
                ),
                format!("export TOLE_MODEL=<one of: {}>", some.join(", ")),
            );
            return out;
        }
    }
    out.reached = Some(Tier::Catalog);
    if requested == Tier::Catalog {
        return out;
    }

    // ---- full ----
    if text_check(cfg, false, &mut out)
        && text_check(cfg, true, &mut out)
        && tools_check(cfg, &mut out)
    {
        out.reached = Some(Tier::Full);
    }
    out
}

// ------------------------------------------------------------------ ledger

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub provider: String,
    pub model: String,
    /// `none` | `offline` | `catalog` | `full`.
    pub level: String,
    pub blocker: Option<String>,
    pub fix: Option<String>,
    pub at_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    pub entries: Vec<LedgerEntry>,
}

/// `<tole data dir>/doctor-ledger.json`.
pub fn ledger_path() -> Result<PathBuf, String> {
    tole_core::paths::tole_data_dir()
        .map(|d| d.join(LEDGER_FILE))
        .ok_or_else(|| "cannot locate the ledger: set CODECORA_HOME or HOME".to_string())
}

impl Ledger {
    /// A missing file is an empty ledger; a corrupt one is an error (it is
    /// never silently replaced).
    pub fn load(path: &Path) -> Result<Ledger, String> {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s)
                .map_err(|e| format!("ledger {} is corrupt: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Ledger::default()),
            Err(e) => Err(format!("cannot read ledger {}: {e}", path.display())),
        }
    }

    /// Atomic write (temp file + rename).
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {parent:?}: {e}"))?;
        }
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        write_private(&tmp, body.as_bytes()).map_err(|e| format!("cannot write {tmp:?}: {e}"))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {path:?}: {e}"))
    }

    /// Record the latest run for `provider`+`model` (replaces the previous
    /// entry for the pair; the latest evidence wins).
    pub fn record(&mut self, provider: &str, model: &str, outcome: &Outcome, at_ms: u64) {
        let entry = LedgerEntry {
            provider: provider.to_string(),
            model: model.to_string(),
            level: outcome.reached.map_or("none", Tier::as_str).to_string(),
            blocker: outcome
                .blocker
                .as_ref()
                .map(|b| format!("{}: {}", b.check, b.message)),
            fix: outcome.blocker.as_ref().map(|b| b.fix.clone()),
            at_ms,
        };
        match self
            .entries
            .iter_mut()
            .find(|e| e.provider == provider && e.model == model)
        {
            Some(slot) => *slot = entry,
            None => self.entries.push(entry),
        }
    }

    /// `READY n-of-m` (n = entries proven at `full`), then the first
    /// blocker and its fix command when there is one.
    pub fn summary(&self) -> String {
        let m = self.entries.len();
        let n = self.entries.iter().filter(|e| e.level == "full").count();
        let mut out = format!("READY {n}-of-{m}");
        if let Some(e) = self.entries.iter().find(|e| e.blocker.is_some()) {
            out.push_str(&format!(
                "\nfirst blocker: {}/{} ({}): {}",
                e.provider,
                e.model,
                e.level,
                e.blocker.as_deref().unwrap_or_default()
            ));
            if let Some(fix) = &e.fix {
                out.push_str(&format!("\nfix: {fix}"));
            }
        }
        out
    }
}

/// Write `bytes` readable by the owner only (0600 on Unix).
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(bytes)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// CLI entry: diagnose, persist to the ledger (unless `no_ledger`), print.
/// Returns `true` when the requested tier was reached.
pub fn run_cli(
    cfg: Option<&OpenAiConfig>,
    env: &dyn Fn(&str) -> Option<String>,
    requested: Tier,
    no_ledger: bool,
) -> bool {
    let outcome = diagnose(cfg, env, requested);
    println!("tole doctor — tier {}", requested.as_str());
    for l in &outcome.lines {
        println!("{l}");
    }
    let ok = outcome.reached >= Some(requested) && outcome.blocker.is_none();
    if let Some(b) = &outcome.blocker {
        println!("blocked at {}: {}", b.tier.as_str(), b.message);
        println!("fix: {}", b.fix);
    }
    // Only a configured provider has a ledger identity.
    if let (Some(cfg), false) = (cfg, no_ledger) {
        let saved = ledger_path().and_then(|p| {
            let mut l = Ledger::load(&p)?;
            l.record(&redact_url(&cfg.base_url), &cfg.model, &outcome, now_ms());
            l.save(&p)?;
            Ok(l)
        });
        match saved {
            Ok(l) => println!("{}", l.summary()),
            Err(e) => eprintln!("tole doctor: ledger not updated: {e}"),
        }
    }
    ok
}

/// `tole doctor --summary`: print the ledger summary without running.
pub fn print_summary() -> Result<(), String> {
    let l = Ledger::load(&ledger_path()?)?;
    println!("{}", l.summary());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// Scripted OpenAI-compatible server: `handler(method, path, body)`
    /// returns `(status, content_type, body)`.
    type Handler = dyn Fn(&str, &str, &Value) -> (u16, &'static str, String) + Send + Sync;

    fn serve(handler: Arc<Handler>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut s) = conn else { continue };
                let h = Arc::clone(&handler);
                std::thread::spawn(move || {
                    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let (head, body) = loop {
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..i]).to_string();
                            let clen = head
                                .lines()
                                .find_map(|l| {
                                    let (k, v) = l.split_once(':')?;
                                    k.eq_ignore_ascii_case("content-length")
                                        .then(|| v.trim().parse::<usize>().ok())
                                        .flatten()
                                })
                                .unwrap_or(0);
                            if buf.len() >= i + 4 + clen {
                                break (head, buf[i + 4..i + 4 + clen].to_vec());
                            }
                        }
                        match s.read(&mut chunk) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    };
                    let mut first = head.lines().next().unwrap_or("").split_whitespace();
                    let (method, path) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
                    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let (status, ctype, payload) = h(method, path, &json);
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                    let _ = s.write_all(resp.as_bytes());
                });
            }
        });
        format!("http://{addr}/v1")
    }

    fn sse_text(text: &str) -> String {
        format!(
            "data: {}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"delta":{"role":"assistant","content":text}}]})
        )
    }

    fn sse_tool(args: &str) -> String {
        format!(
            "data: {}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1",
                "function":{"name":"doctor_probe","arguments":args}}]}}]})
        )
    }

    /// A well-behaved provider; `calls_tool` toggles tool-calling.
    fn good_provider(models: Vec<&'static str>, calls_tool: bool) -> String {
        serve(Arc::new(move |method, path, body| {
            if method == "GET" && path.ends_with("/models") {
                let data: Vec<Value> = models.iter().map(|m| json!({"id": m})).collect();
                return (200, "application/json", json!({"data": data}).to_string());
            }
            let stream = body["stream"].as_bool() == Some(true);
            let has_tools = body["tools"].as_array().is_some_and(|t| !t.is_empty());
            let last_is_tool = body["messages"]
                .as_array()
                .and_then(|m| m.last())
                .is_some_and(|m| m["role"] == "tool");
            let want_call = has_tools && calls_tool && !last_is_tool;
            if stream {
                let sse = if want_call {
                    sse_tool(&json!({"marker": PROBE_MARKER}).to_string())
                } else {
                    sse_text("PONG")
                };
                (200, "text/event-stream", sse)
            } else {
                let msg = if want_call {
                    json!({"role":"assistant","content":null,"tool_calls":[{"id":"call_1",
                        "type":"function","function":{"name":"doctor_probe",
                        "arguments": json!({"marker": PROBE_MARKER}).to_string()}}]})
                } else {
                    json!({"role":"assistant","content":"PONG"})
                };
                (
                    200,
                    "application/json",
                    json!({"choices":[{"message": msg}]}).to_string(),
                )
            }
        }))
    }

    fn cfg_for(url: String, model: &str) -> OpenAiConfig {
        OpenAiConfig::new(url, model, "sk-test")
    }

    #[test]
    fn offline_names_what_is_missing() {
        let env = |k: &str| (k == "TOLE_API_KEY").then(|| "sk".to_string());
        let o = diagnose(None, &env, Tier::Offline);
        let b = o.blocker.unwrap();
        assert_eq!(b.tier, Tier::Offline);
        assert!(b.message.contains("TOLE_BASE_URL") && b.message.contains("TOLE_MODEL"));
        assert!(!b.message.contains("TOLE_API_KEY"), "{}", b.message);
        assert!(
            b.fix.starts_with("export TOLE_BASE_URL=<value>"),
            "{}",
            b.fix
        );
        assert_eq!(o.reached, None);
    }

    #[test]
    fn offline_defaults_to_the_canonical_prefix() {
        let o = diagnose(None, &no_env, Tier::Offline);
        let b = o.blocker.unwrap();
        assert!(
            b.fix.starts_with(
                "export TOLE_BASE_URL=<value> TOLE_MODEL=<value> TOLE_API_KEY=<value>"
            ),
            "{}",
            b.fix
        );
        // A half-configured OPENAI_* set is the one to complete.
        let env = |k: &str| (k == "OPENAI_API_KEY").then(|| "sk".to_string());
        let b = diagnose(None, &env, Tier::Offline).blocker.unwrap();
        assert!(
            b.message.contains("OPENAI_BASE_URL") && !b.message.contains("TOLE_"),
            "{}",
            b.message
        );
    }

    #[test]
    fn offline_rejects_a_non_http_base_url() {
        let cfg = cfg_for("ftp://x".into(), "m");
        let o = diagnose(Some(&cfg), &no_env, Tier::Offline);
        assert_eq!(o.blocker.unwrap().check, "base_url");
        let ok = cfg_for("https://api.example.com/v1".into(), "m");
        let o = diagnose(Some(&ok), &no_env, Tier::Offline);
        assert_eq!(o.reached, Some(Tier::Offline));
        assert!(o.blocker.is_none());
    }

    #[test]
    fn catalog_flags_an_unserved_model() {
        let url = good_provider(vec!["alpha", "beta"], true);
        let o = diagnose(Some(&cfg_for(url, "gamma")), &no_env, Tier::Catalog);
        let b = o.blocker.unwrap();
        assert_eq!((b.tier, b.check), (Tier::Catalog, "model"));
        assert!(b.fix.contains("alpha, beta"), "{}", b.fix);
        assert_eq!(o.reached, Some(Tier::Offline));
    }

    #[test]
    fn catalog_flags_an_unreachable_provider() {
        // Nothing listens on this port.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let o = diagnose(
            Some(&cfg_for(format!("http://127.0.0.1:{port}/v1"), "m")),
            &no_env,
            Tier::Catalog,
        );
        let b = o.blocker.unwrap();
        assert_eq!(b.check, "models");
        assert!(b.fix.contains("/models"), "{}", b.fix);
        assert!(!b.message.contains("sk-test"));
    }

    #[test]
    fn full_passes_against_a_well_behaved_provider() {
        let url = good_provider(vec!["m"], true);
        let o = diagnose(Some(&cfg_for(url, "m")), &no_env, Tier::Full);
        assert!(o.blocker.is_none(), "{:?}", o.blocker);
        assert_eq!(o.reached, Some(Tier::Full));
        assert!(o.lines.iter().any(|l| l.contains("tools: tool-call loop")));
    }

    #[test]
    fn full_fails_when_the_model_never_calls_the_tool() {
        let url = good_provider(vec!["m"], false);
        let o = diagnose(Some(&cfg_for(url, "m")), &no_env, Tier::Full);
        let b = o.blocker.unwrap();
        assert_eq!((b.tier, b.check), (Tier::Full, "tools"));
        assert!(b.message.contains("without calling"), "{}", b.message);
        assert_eq!(o.reached, Some(Tier::Catalog));
    }

    #[test]
    fn full_fails_at_chat_when_completions_are_broken() {
        let url = serve(Arc::new(|method, path, _| {
            if method == "GET" && path.ends_with("/models") {
                (
                    200,
                    "application/json",
                    json!({"data":[{"id":"m"}]}).to_string(),
                )
            } else {
                (500, "application/json", json!({"error":"boom"}).to_string())
            }
        }));
        let o = diagnose(Some(&cfg_for(url, "m")), &no_env, Tier::Full);
        let b = o.blocker.unwrap();
        assert_eq!((b.tier, b.check), (Tier::Full, "chat"));
        assert_eq!(o.reached, Some(Tier::Catalog));
    }

    fn outcome(reached: Option<Tier>, blocker: Option<Blocker>) -> Outcome {
        Outcome {
            reached,
            blocker,
            lines: vec![],
        }
    }

    #[test]
    fn ledger_records_summarises_and_roundtrips() {
        let mut l = Ledger::default();
        l.record("https://a/v1", "m1", &outcome(Some(Tier::Full), None), 1);
        let blocker = Blocker {
            tier: Tier::Full,
            check: "tools",
            message: "no tool call".into(),
            fix: "export TOLE_MODEL=<model>".into(),
        };
        l.record(
            "https://b/v1",
            "m2",
            &outcome(Some(Tier::Catalog), Some(blocker)),
            2,
        );
        assert_eq!(
            l.summary(),
            "READY 1-of-2\nfirst blocker: https://b/v1/m2 (catalog): tools: no tool call\nfix: export TOLE_MODEL=<model>"
        );
        // Latest evidence wins for the same pair.
        l.record("https://b/v1", "m2", &outcome(Some(Tier::Full), None), 3);
        assert_eq!(l.entries.len(), 2);
        assert_eq!(l.summary(), "READY 2-of-2");

        let dir = scratch_dir("ledger");
        let path = dir.join("nested").join(LEDGER_FILE);
        l.save(&path).unwrap();
        assert_eq!(Ledger::load(&path).unwrap(), l);
        assert!(!path.with_extension("json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ledger_load_missing_is_empty_but_corrupt_is_an_error() {
        let dir = scratch_dir("ledger-load");
        let path = dir.join(LEDGER_FILE);
        assert_eq!(Ledger::load(&path).unwrap(), Ledger::default());
        assert_eq!(Ledger::default().summary(), "READY 0-of-0");
        std::fs::write(&path, "{not json").unwrap();
        assert!(Ledger::load(&path).unwrap_err().contains("corrupt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn urls_are_reduced_to_scheme_host_path() {
        assert_eq!(
            redact_url("https://u:p@host:8443/v1?key=abc#frag"),
            "https://host:8443/v1"
        );
        assert_eq!(redact_url("http://host"), "http://host");
        assert_eq!(redact_url("https://a@b@host/v1"), "https://host/v1");
    }

    #[test]
    fn diagnostics_never_carry_the_key_or_url_credentials() {
        let cfg = OpenAiConfig::new("ftp://user:hunter2@x/v1?k=sk-secret", "m", "sk-secret");
        let o = diagnose(Some(&cfg), &no_env, Tier::Offline);
        let all = format!("{:?}", o);
        assert!(
            !all.contains("hunter2") && !all.contains("sk-secret"),
            "{all}"
        );
        assert!(all.contains("ftp://x/v1"), "{all}");
    }

    #[cfg(unix)]
    #[test]
    fn ledger_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch_dir("ledger-mode");
        let path = dir.join(LEDGER_FILE);
        Ledger::default().save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tier_parse_roundtrip() {
        for t in [Tier::Offline, Tier::Catalog, Tier::Full] {
            assert_eq!(Tier::parse(t.as_str()), Some(t));
        }
        assert_eq!(Tier::parse("nope"), None);
        assert!(Tier::Offline < Tier::Catalog && Tier::Catalog < Tier::Full);
    }
}
