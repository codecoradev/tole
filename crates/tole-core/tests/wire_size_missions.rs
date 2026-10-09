//! Issue #211 (PR 2): reference missions with a loose CI bound on request
//! SIZE. Tokenizer-free, deterministic, offline.
//!
//! The REAL `OpenAiProvider` is driven through the real turn loop
//! (`run_turn` / `resume_turn`) against a local mock OpenAI-compatible
//! HTTP server that answers a scripted sequence (tool calls with fixed
//! arguments, then a final text) and reports NO `usage`. The stored
//! usage rows are therefore wire-only: just the `tole_wire` breakdown
//! `{system_chars, tools_chars, history_chars, messages}` written by the
//! PR 1 path. The mock also captures every request body so the tests can
//! assert on the exact bytes that went over the wire.
//!
//! What the guard fails on (see the self-tests at the bottom, which prove
//! each failure is actually detected):
//!
//! * system prompt bloat   -> prefix bound + total bound
//! * tool-spec bloat       -> prefix bound (+ total bound on small-result missions)
//! * history re-send growth -> linear-history check + total bound
//! * reshuffled / non-append-only history, or an unstable prefix across
//!   steps or across a crash-resume -> prefix-stability / append-only checks
//!
//! ## Re-baselining
//!
//! The bounds below are `ceil(measured * 1.25)` rounded up to a round
//! number. Run
//!
//! ```text
//! cargo test -p tole-core --test wire_size_missions -- --nocapture
//! ```
//!
//! to print the measured values (lines starting with `MEASURED`). A bound
//! bump is a deliberate statement that the request got bigger: it needs a
//! written justification in the PR that makes it (what grew, and why that
//! is worth the extra characters on EVERY request). Never bump a bound to
//! make a red run green without that justification.
//!
//! ## Registry used by the missions
//!
//! Profile-independent on purpose (one set of bounds for every feature
//! profile; the shell-tools tools are covered by the full-registry bound
//! in `crates/tole-cli/tests/tool_specs.rs`). 22 specs:
//!
//! * 8 real `tole-core` tools: `read_file`, `edit_file`, `delete_file`,
//!   `web_fetch`, `load_skill`, `todo_write`, `todo_read`, `verify_package`;
//! * 14 synthetic ReadOnly fixture tools with realistic one-sentence
//!   descriptions and 2-4 described parameters (see `SYNTHETIC`). The
//!   missions only ever CALL synthetic tools, so results have a fixed,
//!   configurable size.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tole_core::approval::{Approver, ToolRequest, Verdict};
use tole_core::entry::Entry;
use tole_core::openai::{wire_stats, OpenAiConfig, OpenAiProvider, WireStats};
use tole_core::provider::{Provider, ProviderError, ProviderOutput};
use tole_core::storage::{JsonlStorage, Storage};
use tole_core::tool::{Risk, Tool, ToolRegistry};
use tole_core::turn::{resume_turn, run_turn, TurnOutcome};

// ---------------------------------------------------------------------------
// Bounds (see "Re-baselining" above)
// ---------------------------------------------------------------------------

/// Upper bound on `system_chars + tools_chars` of any single request
/// (the cacheable prefix every step re-sends). Measured 9,890
/// (system 1,436 + tools 8,454); bound = ceil(9,890 * 1.25) rounded up.
const PREFIX_BOUND: u64 = 13_000;

/// Upper bound on the SUM over all steps of
/// `system_chars + tools_chars + history_chars` for `many_small_steps`.
/// Measured 291,485; bound = ceil(measured * 1.25) rounded up.
const MANY_SMALL_TOTAL_BOUND: u64 = 370_000;
/// Same, for `read_heavy`. Measured 647,498.
const READ_HEAVY_TOTAL_BOUND: u64 = 810_000;
/// Same, for `mixed_with_resume` (pre-crash + post-resume requests).
/// Measured 277,380.
const MIXED_TOTAL_BOUND: u64 = 350_000;

/// Per-step envelope allowed on top of a tool result's own characters in
/// the linear-history check: the assistant `tool_calls` message, the tool
/// message wrapper, the result fence and the JSON escaping of the result.
const HISTORY_SLACK_PER_STEP: u64 = 700;

/// Characters of regression headroom the self-tests inject.
const SYSTEM_BLOAT: usize = 10_000;
const TOOL_BLOAT: usize = 5_000;

// ---------------------------------------------------------------------------
// Local mock OpenAI-compatible server
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum Reply {
    Tool { name: String, args: Value },
    Final(String),
}

/// Scripted `/chat/completions` server on `127.0.0.1:0` (never a fixed
/// port). One thread per connection; every response carries
/// `Connection: close` so the client never reuses a socket and request
/// `n` is always answered with `replies[n]`. The request is read until
/// the Content-Length body is complete (a single `read` may return the
/// headers only, which is the classic source of flaky mock readers), and
/// every socket has a read timeout so a bug fails the test instead of
/// hanging it. Responses carry no `usage` object.
struct MockServer {
    addr: SocketAddr,
    bodies: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
}

impl MockServer {
    fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let bodies: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let replies = Arc::new(replies);
        let (b, st) = (Arc::clone(&bodies), Arc::clone(&stop));
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if st.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = conn else { continue };
                let (b, replies) = (Arc::clone(&b), Arc::clone(&replies));
                std::thread::spawn(move || serve_one(stream, &b, &replies));
            }
        });
        MockServer { addr, bodies, stop }
    }

    fn url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().unwrap().clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        // Unblock `accept` so the listener thread exits.
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
    }
}

fn read_request_body(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        if let Some(h) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..h]).to_string();
            let clen: usize = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if buf.len() >= h + 4 + clen {
                return Some(buf[h + 4..h + 4 + clen].to_vec());
            }
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn serve_one(mut stream: TcpStream, bodies: &Mutex<Vec<Value>>, replies: &[Reply]) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    let Some(raw) = read_request_body(&mut stream) else {
        return;
    };
    let Ok(body) = serde_json::from_slice::<Value>(&raw) else {
        return;
    };
    // Index assignment and capture are one critical section.
    let idx = {
        let mut g = bodies.lock().unwrap();
        g.push(body);
        g.len() - 1
    };
    let (status, payload) = match replies.get(idx) {
        Some(Reply::Tool { name, args }) => (
            "200 OK",
            json!({"choices":[{"message":{
                "role":"assistant","content":null,
                "tool_calls":[{"id":format!("call_{idx}"),"type":"function",
                    "function":{"name":name,"arguments":args.to_string()}}]
            }}]}),
        ),
        Some(Reply::Final(text)) => (
            "200 OK",
            json!({"choices":[{"message":{"role":"assistant","content":text}}]}),
        ),
        None => (
            "500 Internal Server Error",
            json!({"error":"mock script exhausted"}),
        ),
    };
    let payload = payload.to_string();
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    let _ = stream.write_all(resp.as_bytes());
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

struct DenyAllInteractive;
impl Approver for DenyAllInteractive {
    fn decide(&self, _req: &ToolRequest<'_>) -> Verdict {
        Verdict::Deny
    }
    fn interactive(&self) -> bool {
        true
    }
}

/// (name, description, [(param, type, param description)])
type SyntheticSpec = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str, &'static str)],
);

const SYNTHETIC: &[SyntheticSpec] = &[
    ("search_docs", "Search the project documentation for a phrase and return the best matching sections with their file paths.", &[("query","string","Phrase or keywords to look for."),("limit","integer","Maximum number of sections to return (default 5)."),("path_prefix","string","Only search docs under this path prefix.")]),
    ("list_dir", "List the entries of a directory inside the workspace, directories first, with sizes.", &[("path","string","Directory to list, relative to the workspace root."),("recursive","boolean","Descend into subdirectories."),("max_entries","integer","Cap on returned entries.")]),
    ("grep_repo", "Search tracked files for a regular expression and return matching lines with file and line number.", &[("pattern","string","Regular expression to search for."),("glob","string","Restrict the search to files matching this glob."),("context","integer","Lines of context around each match."),("max_matches","integer","Cap on returned matches.")]),
    ("read_symbol", "Return the source of a named function, type or constant from the code index.", &[("symbol","string","Fully qualified or bare symbol name."),("with_callers","boolean","Also list direct callers.")]),
    ("fetch_issue", "Fetch one issue from the tracker: title, state, labels and the comment thread.", &[("number","integer","Issue number."),("comments","boolean","Include the comment thread.")]),
    ("list_prs", "List pull requests of the repository filtered by state and author.", &[("state","string","One of open, closed, merged."),("author","string","Only PRs by this login."),("limit","integer","Maximum number of PRs.")]),
    ("get_ci_status", "Report the CI check runs for a commit or branch with their conclusions.", &[("ref","string","Commit SHA or branch name."),("only_failed","boolean","Hide passing checks.")]),
    ("query_metrics", "Query a recorded metric series over a time window and return summarized points.", &[("metric","string","Metric name."),("window_minutes","integer","Size of the window ending now."),("step_seconds","integer","Resolution of the returned points.")]),
    ("lookup_dependency", "Resolve a dependency to its locked version, license and the crates that use it.", &[("name","string","Dependency name."),("ecosystem","string","Package ecosystem, for example cargo or npm.")]),
    ("inspect_schema", "Describe the shape of a JSON or SQL schema file: tables, columns and types.", &[("path","string","Schema file to inspect."),("table","string","Only describe this table or definition.")]),
    ("tail_log", "Return the last lines of a log file, optionally filtered by severity.", &[("path","string","Log file path."),("lines","integer","Number of trailing lines."),("level","string","Minimum severity to keep.")]),
    ("diff_summary", "Summarize the diff between two revisions per file: additions, deletions and a short note.", &[("base","string","Base revision."),("head","string","Head revision."),("paths","array","Restrict to these paths.")]),
    ("resolve_path", "Resolve a symbolic location such as a module name or a test name to a file path.", &[("name","string","Module, test or fixture name."),("kind","string","One of module, test, fixture.")]),
    ("count_lines", "Count lines of code per language under a path, excluding vendored directories.", &[("path","string","Root to count under."),("exclude","array","Extra directory names to skip.")]),
];

/// ReadOnly fixture tool: returns `out_chars` deterministic characters.
struct FixtureTool {
    spec: SyntheticSpec,
    out_chars: usize,
}

impl Tool for FixtureTool {
    fn name(&self) -> &str {
        self.spec.0
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    fn summary(&self) -> String {
        self.spec.1.to_string()
    }
    fn spec(&self) -> Option<Value> {
        let props: serde_json::Map<String, Value> = self
            .spec
            .2
            .iter()
            .map(|(n, t, d)| ((*n).to_string(), json!({"type": t, "description": d})))
            .collect();
        Some(json!({"type": "object", "properties": props}))
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        let tag = input["n"].as_u64().unwrap_or(0);
        Ok(json!({ "content": filler(self.out_chars, tag) }))
    }
}

/// Exactly `n` alphanumeric characters, a pure function of `(n, tag)`
/// (no JSON escaping, so one char on the wire is one char of content).
fn filler(n: usize, tag: u64) -> String {
    const ALPHA: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    (0..n)
        .map(|i| ALPHA[(i * 7 + tag as usize * 13) % ALPHA.len()] as char)
        .collect()
}

/// An extra, deliberately oversized spec for the self-test.
struct BloatedTool(usize);
impl Tool for BloatedTool {
    fn name(&self) -> &str {
        "bloated_tool"
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    fn summary(&self) -> String {
        filler(self.0, 99)
    }
    fn execute(&self, _input: Value) -> Result<Value, String> {
        Ok(json!({}))
    }
}

/// `size_of(i)` = result characters of the i-th synthetic tool.
fn build_registry(size_of: &dyn Fn(usize) -> usize, extra: Option<Box<dyn Tool>>) -> ToolRegistry {
    let root = PathBuf::from(".");
    let mut reg = ToolRegistry::with_approver(DenyAllInteractive);
    let real: Vec<Box<dyn Tool>> = {
        let todo = tole_core::todo::TodoState::new();
        vec![
            Box::new(tole_core::read_file::ReadFileTool::new(root.clone())),
            Box::new(tole_core::file_tools::EditFileTool::new(root.clone())),
            Box::new(tole_core::file_tools::DeleteFileTool::new(root)),
            Box::new(tole_core::web::WebFetchTool),
            Box::new(tole_core::skills::LoadSkillTool::new(None)),
            Box::new(tole_core::todo::TodoWriteTool::new(todo.clone())),
            Box::new(tole_core::todo::TodoReadTool::new(todo)),
            Box::new(tole_core::verify_package::VerifyPackageTool::new()),
        ]
    };
    for t in real {
        let n = t.name().to_string();
        reg.register(t).unwrap_or_else(|e| panic!("{n}: {e}"));
    }
    for (i, spec) in SYNTHETIC.iter().enumerate() {
        reg.register(Box::new(FixtureTool {
            spec: *spec,
            out_chars: size_of(i),
        }))
        .unwrap();
    }
    if let Some(t) = extra {
        reg.register(t).unwrap();
    }
    reg
}

/// A fixed, realistic-sized system prompt (about 1.5k characters).
fn system_prompt() -> String {
    let para = "You are tole, a careful coding agent working inside the user's repository. \
Prefer reading before editing, keep changes small and reviewable, never invent file contents, \
and explain failures plainly. Every write goes through the approval gate; never try to bypass it. \
When a task is finished, answer with a short summary of what changed and what was verified. ";
    para.repeat(4)
}

// ---------------------------------------------------------------------------
// Mission runner
// ---------------------------------------------------------------------------

/// One request as measured from the stored usage row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Step {
    system: u64,
    tools: u64,
    history: u64,
}

impl Step {
    fn total(&self) -> u64 {
        self.system + self.tools + self.history
    }
    fn prefix(&self) -> u64 {
        self.system + self.tools
    }
}

/// The measured outcome of a (possibly resumed) mission.
struct MissionRun {
    /// Captured request bodies, in request order (pre-crash first).
    bodies: Vec<Value>,
    /// Per-step stats from the stored `tole_wire`, aligned with `bodies`.
    steps: Vec<Step>,
    /// The largest single tool result the mission produces.
    max_result_chars: u64,
}

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-wsm-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn provider(url: String, system: &str, reg: &ToolRegistry) -> OpenAiProvider {
    OpenAiProvider::new(OpenAiConfig::new(url, "mock-model", "sk-test"))
        .with_streaming(false)
        .with_system_prompt(system)
        .with_tool_specs(reg.specs())
}

/// Steps from the stored usage rows. Asserts the rows are wire-only (the
/// mock reports no `usage`) and that stored stats equal `wire_stats` of
/// the captured body — the PR 1 path and the mock agree.
fn steps_from_storage(s: &dyn Storage, bodies: &[Value]) -> Vec<Step> {
    let rows = s.usages();
    assert_eq!(rows.len(), bodies.len(), "one usage row per request");
    rows.iter()
        .zip(bodies)
        .map(|(r, b)| {
            let obj = r.usage.as_object().expect("usage is an object");
            assert_eq!(
                obj.keys().collect::<Vec<_>>(),
                vec!["tole_wire"],
                "wire-only"
            );
            let w: WireStats = wire_stats(b);
            assert_eq!(r.usage["tole_wire"], w.to_value(), "stored == measured");
            Step {
                system: r.usage["tole_wire"]["system_chars"].as_u64().unwrap(),
                tools: r.usage["tole_wire"]["tools_chars"].as_u64().unwrap(),
                history: r.usage["tole_wire"]["history_chars"].as_u64().unwrap(),
            }
        })
        .collect()
}

/// Script: `tool_steps` tool calls (cycling the synthetic tools, args
/// varying per step so the loop guard never trips) then a final text.
/// Total requests = `tool_steps + 1`.
fn script(tool_steps: usize) -> Vec<Reply> {
    let mut v: Vec<Reply> = (0..tool_steps)
        .map(|i| Reply::Tool {
            name: SYNTHETIC[i % SYNTHETIC.len()].0.to_string(),
            args: json!({"query": format!("step-{i:02}"), "n": i}),
        })
        .collect();
    v.push(Reply::Final("done".to_string()));
    v
}

fn expect_final(out: Result<TurnOutcome, tole_core::storage::StorageError>) {
    match out.expect("turn ran") {
        TurnOutcome::Final { text, .. } => assert_eq!(text, "done"),
        other => panic!("expected Final, got {other:?}"),
    }
}

/// Run a mission uninterrupted. `wrap` lets a self-test interpose a
/// misbehaving provider around the real one.
fn run_with(
    dir: &Path,
    id: &str,
    steps: usize,
    result_chars: usize,
    system: &str,
    extra: Option<Box<dyn Tool>>,
    doubling: bool,
) -> (MissionRun, JsonlStorage) {
    let reg = build_registry(&|_| result_chars, extra);
    let server = MockServer::start(script(steps - 1));
    let mut s = JsonlStorage::create(dir, id, None).unwrap();
    let inner = provider(server.url(), system, &reg);
    if doubling {
        let mut p = DoublingProvider(inner);
        expect_final(run_turn(&mut s, &mut p, &reg, "start the mission"));
    } else {
        let mut p = inner;
        expect_final(run_turn(&mut s, &mut p, &reg, "start the mission"));
    }
    let bodies = server.bodies();
    let run = MissionRun {
        steps: steps_from_storage(&s, &bodies),
        bodies,
        max_result_chars: result_chars as u64,
    };
    (run, s)
}

/// Misbehaving provider for the self-test: re-sends the whole history
/// twice (what a bug that appends the transcript to itself would do).
struct DoublingProvider(OpenAiProvider);
impl Provider for DoublingProvider {
    fn complete(&mut self, transcript: &[Entry]) -> Result<ProviderOutput, ProviderError> {
        let doubled: Vec<Entry> = transcript
            .iter()
            .chain(transcript.iter())
            .cloned()
            .collect();
        self.0.complete(&doubled)
    }
    fn last_usage(&self) -> Option<Value> {
        self.0.last_usage()
    }
    fn last_reasoning(&self) -> Option<String> {
        self.0.last_reasoning()
    }
    fn last_wire_stats(&self) -> Option<Value> {
        self.0.last_wire_stats()
    }
}

// ---------------------------------------------------------------------------
// Checks (Result-returning so self-tests can observe a failure)
// ---------------------------------------------------------------------------

fn check_bound(what: &str, measured: u64, bound: u64) -> Result<(), String> {
    if measured <= bound {
        Ok(())
    } else {
        Err(format!(
            "{what}: measured {measured} chars exceeds bound {bound} \
             (re-baseline only with a justification, see the file header)"
        ))
    }
}

/// History is only the append-only sum of results: the last step's
/// history may exceed the first step's by at most
/// `steps * (max single result + slack)`.
fn check_linear_history(run: &MissionRun) -> Result<(), String> {
    let first = run.steps.first().unwrap().history;
    let last = run.steps.last().unwrap().history;
    let allowed = first + run.steps.len() as u64 * (run.max_result_chars + HISTORY_SLACK_PER_STEP);
    if last <= allowed {
        Ok(())
    } else {
        Err(format!(
            "history grew super-linearly: last-step history {last} > allowed {allowed} \
             (first {first}, {} steps, max result {})",
            run.steps.len(),
            run.max_result_chars
        ))
    }
}

/// `system_chars` and `tools_chars` identical on every step.
fn check_prefix_stable(steps: &[Step]) -> Result<(), String> {
    let (s0, t0) = (steps[0].system, steps[0].tools);
    for (i, st) in steps.iter().enumerate() {
        if st.system != s0 || st.tools != t0 {
            return Err(format!(
                "prefix unstable at step {i}: system {}/{s0}, tools {}/{t0}",
                st.system, st.tools
            ));
        }
    }
    Ok(())
}

/// Body n+1 starts with exactly the messages and the same tools array of
/// body n: append-only, no reshuffling, byte-identical prefix.
fn check_append_only(bodies: &[Value]) -> Result<(), String> {
    for (n, pair) in bodies.windows(2).enumerate() {
        let (a, b) = (
            pair[0]["messages"].as_array().unwrap(),
            pair[1]["messages"].as_array().unwrap(),
        );
        if b.len() <= a.len() || b[..a.len()] != a[..] {
            return Err(format!("request {} is not an append of request {n}", n + 1));
        }
        if pair[0]["tools"] != pair[1]["tools"] {
            return Err(format!(
                "tools array changed between request {n} and {}",
                n + 1
            ));
        }
    }
    Ok(())
}

/// All checks for one mission; empty = clean.
fn violations(run: &MissionRun, total_bound: u64) -> Vec<String> {
    let total: u64 = run.steps.iter().map(Step::total).sum();
    let prefix = run.steps[0].prefix();
    [
        check_bound("total request chars", total, total_bound),
        check_bound("first-step prefix (system+tools)", prefix, PREFIX_BOUND),
        check_linear_history(run),
        check_prefix_stable(&run.steps),
        check_append_only(&run.bodies),
    ]
    .into_iter()
    .filter_map(Result::err)
    .collect()
}

fn report(name: &str, run: &MissionRun) {
    let total: u64 = run.steps.iter().map(Step::total).sum();
    let first = run.steps[0];
    let last = run.steps.last().unwrap();
    println!(
        "MEASURED {name}: steps={} total={total} prefix={} (system={} tools={}) \
         first_history={} last_history={}",
        run.steps.len(),
        first.prefix(),
        first.system,
        first.tools,
        first.history,
        last.history
    );
}

// ---------------------------------------------------------------------------
// Missions
// ---------------------------------------------------------------------------

fn many_small_steps(dir: &Path, id: &str) -> MissionRun {
    run_with(dir, id, 20, 200, &system_prompt(), None, false).0
}

fn read_heavy(dir: &Path, id: &str) -> MissionRun {
    run_with(dir, id, 8, 20_000, &system_prompt(), None, false).0
}

#[test]
fn many_small_steps_stays_within_bound() {
    let dir = tmpdir("many-small");
    let run = many_small_steps(&dir, "ms");
    report("many_small_steps", &run);
    assert_eq!(run.steps.len(), 20);
    let v = violations(&run, MANY_SMALL_TOTAL_BOUND);
    assert!(v.is_empty(), "{v:#?}");
}

#[test]
fn read_heavy_stays_within_bound() {
    let dir = tmpdir("read-heavy");
    let run = read_heavy(&dir, "rh");
    report("read_heavy", &run);
    assert_eq!(run.steps.len(), 8);
    let v = violations(&run, READ_HEAVY_TOTAL_BOUND);
    assert!(v.is_empty(), "{v:#?}");
}

/// 12 requests (11 tool calls + final) with varied result sizes, killed
/// after the 5th tool settlement and resumed from the on-disk file.
/// Result sizes cycle 200 / 2,000 / 5,000.
#[test]
fn mixed_with_resume_keeps_prefix_and_stays_within_bound() {
    const STEPS: usize = 12;
    const CRASH_AFTER_TOOLS: usize = 5;
    let dir = tmpdir("mixed");
    let system = system_prompt();
    let sizes = [200usize, 2_000, 5_000];

    // Tool i returns sizes[i % 3] characters.
    let mixed_registry = || build_registry(&|i| sizes[i % sizes.len()], None);

    // Reference (uninterrupted) run.
    let reg = mixed_registry();
    let server = MockServer::start(script(STEPS - 1));
    let mut s = JsonlStorage::create(&dir, "ref", None).unwrap();
    let mut p = provider(server.url(), &system, &reg);
    expect_final(run_turn(&mut s, &mut p, &reg, "start the mission"));
    let ref_bodies = server.bodies();
    assert_eq!(ref_bodies.len(), STEPS);
    let ref_steps = steps_from_storage(&s, &ref_bodies);

    // Crash right after the 5th tool settled and the machine returned to
    // planning: the file is cut after that `state: planning` line. Each
    // tool step is 5 lines (state tool_call, intent, tool_result, state
    // planning, usage row of the next request); the file starts with the
    // header, the user message, the first usage row and the first
    // tool_call state. A cut at a line boundary is byte-for-byte what a
    // process killed there leaves behind (one fsynced line per commit).
    let lines: Vec<String> = std::fs::read_to_string(dir.join("ref.jsonl"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    let k = 6 + 5 * (CRASH_AFTER_TOOLS - 1);
    let cut_line: Value = serde_json::from_str(&lines[k]).unwrap();
    assert_eq!(cut_line["kind"], json!("state"));
    assert_eq!(cut_line["pc"], json!("planning"));
    let cut = dir.join("cut.jsonl");
    std::fs::write(&cut, format!("{}\n", lines[..=k].join("\n"))).unwrap();

    // Resume with a brand-new process-equivalent: new provider, new
    // registry, new mock answering the remaining script.
    let reg2 = mixed_registry();
    let server2 = MockServer::start(script(STEPS - 1)[CRASH_AFTER_TOOLS..].to_vec());
    let mut s2 = JsonlStorage::open(&cut).unwrap();
    let mut p2 = provider(server2.url(), &system, &reg2);
    expect_final(resume_turn(&mut s2, &mut p2, &reg2));
    let post_bodies = server2.bodies();
    assert_eq!(post_bodies.len(), STEPS - CRASH_AFTER_TOOLS);

    // Prefix stability across the crash boundary: identical system and
    // tools characters before and after, and the resumed requests are
    // BYTE-IDENTICAL to the uninterrupted run's (same cache prefix).
    for (i, b) in post_bodies.iter().enumerate() {
        let (got, want) = (b.to_string(), ref_bodies[CRASH_AFTER_TOOLS + i].to_string());
        if got != want {
            let at = got
                .chars()
                .zip(want.chars())
                .take_while(|(a, b)| a == b)
                .count();
            let ctx = |t: &str| {
                t.chars()
                    .skip(at.saturating_sub(80))
                    .take(200)
                    .collect::<String>()
            };
            panic!(
                "post-resume request {i} differs at char {at} (len {} vs {}):\n got: {}\nwant: {}",
                got.len(),
                want.len(),
                ctx(&got),
                ctx(&want)
            );
        }
    }
    // The resumed session's rows only cover post-crash requests, but the
    // file kept the pre-crash rows: pre + post == the whole mission.
    let all_steps = steps_from_storage(&s2, &ref_bodies);
    assert_eq!(all_steps, ref_steps);
    let post0 = wire_stats(&post_bodies[0]);
    let pre_last = wire_stats(&ref_bodies[CRASH_AFTER_TOOLS - 1]);
    assert_eq!(post0.system_chars, pre_last.system_chars);
    assert_eq!(post0.tools_chars, pre_last.tools_chars);

    let run = MissionRun {
        bodies: ref_bodies[..CRASH_AFTER_TOOLS]
            .iter()
            .chain(post_bodies.iter())
            .cloned()
            .collect(),
        steps: all_steps,
        max_result_chars: *sizes.iter().max().unwrap() as u64,
    };
    report("mixed_with_resume", &run);
    assert_eq!(run.steps.len(), STEPS);
    let v = violations(&run, MIXED_TOTAL_BOUND);
    assert!(v.is_empty(), "{v:#?}");
}

// ---------------------------------------------------------------------------
// Self-tests: the guard has teeth
// ---------------------------------------------------------------------------

fn has(v: &[String], needle: &str) -> bool {
    v.iter().any(|m| m.contains(needle))
}

#[test]
fn check_bound_reports_instead_of_panicking() {
    assert!(check_bound("x", 10, 10).is_ok());
    assert!(check_bound("x", 11, 10).is_err());
}

#[test]
fn bloated_system_prompt_is_a_violation() {
    let dir = tmpdir("bloat-system");
    let bloated = format!("{}{}", system_prompt(), filler(SYSTEM_BLOAT, 1));
    let (run, _s) = run_with(&dir, "bs", 20, 200, &bloated, None, false);
    let v = violations(&run, MANY_SMALL_TOTAL_BOUND);
    println!("bloated system -> {v:#?}");
    assert!(has(&v, "first-step prefix"), "{v:#?}");
    assert!(has(&v, "total request chars"), "{v:#?}");
}

#[test]
fn one_extra_bloated_tool_spec_is_a_violation() {
    let dir = tmpdir("bloat-tool");
    let (run, _s) = run_with(
        &dir,
        "bt",
        20,
        200,
        &system_prompt(),
        Some(Box::new(BloatedTool(TOOL_BLOAT))),
        false,
    );
    let v = violations(&run, MANY_SMALL_TOTAL_BOUND);
    println!("bloated tool -> {v:#?}");
    assert!(has(&v, "first-step prefix"), "{v:#?}");
    assert!(has(&v, "total request chars"), "{v:#?}");
}

#[test]
fn history_sent_twice_is_a_violation() {
    let dir = tmpdir("history-twice");
    let (run, _s) = run_with(&dir, "ht", 8, 20_000, &system_prompt(), None, true);
    let v = violations(&run, READ_HEAVY_TOTAL_BOUND);
    println!("history twice -> {v:#?}");
    assert!(has(&v, "super-linearly"), "{v:#?}");
    assert!(has(&v, "total request chars"), "{v:#?}");
    assert!(has(&v, "not an append"), "{v:#?}");
}

#[test]
fn unstable_prefix_and_reshuffled_history_are_violations() {
    let dir = tmpdir("unstable");
    let mut run = many_small_steps(&dir, "us");
    assert!(check_prefix_stable(&run.steps).is_ok());
    assert!(check_append_only(&run.bodies).is_ok());
    // A system prompt that drifts by one character on one step.
    run.steps[3].system += 1;
    assert!(check_prefix_stable(&run.steps).is_err());
    // Two messages swapped in a later request.
    let msgs = run.bodies[5]["messages"].as_array_mut().unwrap();
    msgs.swap(1, 2);
    assert!(check_append_only(&run.bodies).is_err());
}
