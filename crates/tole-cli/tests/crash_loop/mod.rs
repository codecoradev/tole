//! Shared pieces of the SIGKILL crash-resume loop tests (#1018): a seeded
//! generator for the kill delays, the session-log reader that trusts no torn
//! line, the questions the tests ask of a log, and a scripted provider mock
//! (see [`mock`]). Used by `serve_crash_resume`, `serve_crash_effect`,
//! `mission_crash_resume` and `serve_crash_outbox`.
//!
//! `TOLE_CRASH_LOOP_N` is the number of rounds (a few by default so the PR
//! gate stays short; the nightly job sets more, and N=10 is the issue's
//! figure). `TOLE_CRASH_LOOP_SEED` replays the kill delays of a red run: the
//! seed fixes the delays, not the timing of the machine.

#![allow(dead_code)]

use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

pub mod mock;

/// splitmix64: a seeded generator, so a failing round can be replayed.
pub struct Rng(pub u64);

impl Rng {
    pub fn below(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) % n
    }
}

/// A number from the environment: unset is `None`; set and not a number is a
/// failure, so a typo in the nightly cannot quietly run something else.
pub fn env_number(name: &str) -> Option<u64> {
    let value = std::env::var(name).ok()?;
    Some(
        value
            .parse()
            .unwrap_or_else(|_| panic!("{name}={value:?} is not a number")),
    )
}

pub fn seed() -> u64 {
    env_number("TOLE_CRASH_LOOP_SEED").unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1)
    })
}

/// Rounds per scenario: `TOLE_CRASH_LOOP_N`, or `default` when it is not set.
pub fn rounds(default: u64) -> usize {
    let n = env_number("TOLE_CRASH_LOOP_N").unwrap_or(default);
    assert!(n >= 1, "TOLE_CRASH_LOOP_N must be at least 1");
    n as usize
}

/// Send a prompt and do not wait for the answer: the connection stays open
/// until the caller drops it, so the turn is certainly in flight at the kill.
pub fn send_prompt(port: u16, id: &str, text: &str) -> std::net::TcpStream {
    let body = json!({"text": text}).to_string();
    let req = format!(
        "POST /sessions/{id}/prompt HTTP/1.1\r\nAuthorization: Bearer t0ken\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    s
}

/// What a session file held after a kill.
pub struct Log {
    /// Every item (entries, states, usage), in file order.
    pub items: Vec<Value>,
    /// The file ends in an unterminated line: a write the kill cut short.
    pub torn: bool,
}

/// The session file as the items it holds. The first line is the header; every
/// other line is a commit (an array of items, `entry` and `state`) or one item
/// (a `usage` record), each with a `seq`. Every complete line must parse, and
/// `seq` only goes up across the whole file. An unterminated last line is the
/// one damage tolerated: it is reported in [`Log::torn`] and never parsed, so
/// no torn line is trusted.
pub fn read_log(file: &Path, ctx: &str) -> Log {
    let raw = std::fs::read(file).unwrap_or_else(|e| panic!("{ctx}: {}: {e}", file.display()));
    let torn = raw.last().is_some_and(|b| *b != b'\n');
    let mut lines: Vec<&[u8]> = raw.split(|b| *b == b'\n').collect();
    // `split` leaves an empty piece after the final newline, or the cut line.
    lines.pop();
    let mut items = Vec::new();
    let mut last = 0u64;
    let mut header = false;
    for line in lines.into_iter().filter(|l| !l.is_empty()) {
        let v: Value = serde_json::from_slice(line).unwrap_or_else(|e| {
            panic!("{ctx}: a complete line does not parse ({e}): {line:.200?}")
        });
        if v["kind"] == "header" {
            assert!(!header, "{ctx}: two headers");
            assert!(
                items.is_empty(),
                "{ctx}: a header that is not the first line"
            );
            header = true;
            continue;
        }
        // A line is a commit (an array of items) or one item on its own.
        let commit = match v {
            Value::Array(items) => items,
            single => vec![single],
        };
        for item in &commit {
            let seq = item["seq"]
                .as_u64()
                .unwrap_or_else(|| panic!("{ctx}: an item without a seq: {item}"));
            assert!(seq > last, "{ctx}: seq {seq} does not follow {last}");
            last = seq;
            items.push(item.clone());
        }
    }
    assert!(header, "{ctx}: the session file has no header");
    Log { items, torn }
}

/// How many user messages of the log carry `needle`.
pub fn user_messages_with(log: &[Value], needle: &str) -> usize {
    log.iter()
        .filter(|e| e["kind"] == "entry" && e["type"] == "message")
        .filter(|e| e["payload"]["role"] == "user")
        .filter(|e| e["payload"].to_string().contains(needle))
        .count()
}

/// How many assistant messages of the log carry `needle`, so a turn that is
/// answered twice, or not at all, shows.
pub fn answers_with(log: &[Value], needle: &str) -> usize {
    log.iter()
        .filter(|e| e["kind"] == "entry" && e["type"] == "message")
        .filter(|e| e["payload"]["role"] == "assistant")
        .filter(|e| e["payload"].to_string().contains(needle))
        .count()
}

/// The ids of the `tool` intents of the log.
pub fn intents(log: &[Value], tool: &str) -> Vec<String> {
    log.iter()
        .filter(|e| e["type"] == "intent" && e["payload"]["tool"] == tool)
        .filter_map(|e| e["id"].as_str().map(str::to_string))
        .collect()
}

/// The entries that settle an intent of `tool`: a `tool_result` or an `error`
/// whose parent is that intent. One intent settled twice shows as two.
pub fn settles<'a>(log: &'a [Value], tool: &str) -> Vec<&'a Value> {
    let ids: HashSet<String> = intents(log, tool).into_iter().collect();
    log.iter()
        .filter(|e| e["type"] == "tool_result" || e["type"] == "error")
        .filter(|e| e["parentId"].as_str().is_some_and(|p| ids.contains(p)))
        .collect()
}

/// The `seq` of the entries of the file, which is what `GET .../entries`
/// lists (the state items take numbers of their own).
pub fn entry_seqs(log: &[Value]) -> Vec<u64> {
    log.iter()
        .filter(|e| e["kind"] == "entry")
        .filter_map(|e| e["seq"].as_u64())
        .collect()
}

/// The register state the log ends in (`planning`, `final`, ...).
pub fn last_pc(log: &[Value]) -> Option<String> {
    log.iter()
        .rev()
        .find(|e| e["kind"] == "state")
        .and_then(|e| e["pc"].as_str())
        .map(str::to_string)
}

/// A turn is mid-flight when the register is neither at rest nor settled.
pub fn mid_flight(pc: Option<&str>) -> bool {
    !matches!(pc, None | Some("idle") | Some("final"))
}

pub fn last_entry(log: &[Value]) -> Option<&Value> {
    log.iter().rev().find(|e| e["kind"] == "entry")
}

/// The lines of a file the effect appends to (`effect.log`): how many times
/// the effect ran.
pub fn effect_runs(file: &Path) -> usize {
    std::fs::read_to_string(file).map_or(0, |t| t.lines().count())
}

/// What a `tole` command printed and how it ended.
pub struct Ran {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// `tole resume <id> [extra]` over the sessions directory of `workspace`
/// (the `.tole/sessions` a `tole serve --workspace` writes): the documented
/// recovery of a turn that was cut off. `extra` is `--allow run_command` or
/// `--yes` to pre-approve a replayed write, nothing to refuse it.
pub fn cli_resume(base: &str, workspace: &Path, home: &Path, id: &str, extra: &[&str]) -> Ran {
    let mut cmd = crate::common::hermetic_command(env!("CARGO_BIN_EXE_tole"), home, &[]);
    cmd.arg("--sessions-dir")
        .arg(workspace.join(".tole/sessions"))
        .arg("resume")
        .args(extra)
        .arg(id)
        .current_dir(workspace)
        .env("TOLE_BASE_URL", base)
        .env("TOLE_MODEL", "mock-model")
        .env("TOLE_API_KEY", "sk-test")
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .stdin(std::process::Stdio::null());
    let out = cmd.output().unwrap();
    Ran {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    }
}
