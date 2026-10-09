//! #211 `tole status` usage block, end-to-end with the REAL binary on a
//! fixture session: the pre-existing lines are unchanged and the new
//! block appears after them (`n/a` for sessions recorded before
//! `tole_wire` existed).

use serde_json::{json, Value};
use std::process::{Command, Stdio};
use tole_core::entry::Entry;
use tole_core::provider::{Provider, ProviderError, ProviderOutput};
use tole_core::storage::JsonlStorage;
use tole_core::tool::ToolRegistry;
use tole_core::turn::run_turn;

/// Scripted provider reporting fixed usage and (optionally) a growing
/// wire breakdown.
struct Fixture {
    step: usize,
    usage: Value,
    wire: bool,
}

impl Provider for Fixture {
    fn complete(&mut self, _t: &[Entry]) -> Result<ProviderOutput, ProviderError> {
        self.step += 1;
        Ok(ProviderOutput::Final {
            text: "ok".to_string(),
        })
    }
    fn last_usage(&self) -> Option<Value> {
        Some(self.usage.clone())
    }
    fn last_wire_stats(&self) -> Option<Value> {
        self.wire.then(|| {
            json!({"system_chars": 100, "tools_chars": 900,
                   "history_chars": 40 * self.step as u64, "messages": 2})
        })
    }
}

fn tmp(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "tole-usage-report-{tag}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn status(dir: &std::path::Path, id: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_tole"))
        .args(["status", "-s", dir.to_str().unwrap(), id])
        .stdin(Stdio::null())
        .output()
        .expect("run tole status");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn fixture(dir: &std::path::Path, id: &str, wire: bool, usage: Value) {
    let mut s = JsonlStorage::create(dir, id, None).unwrap();
    let mut p = Fixture {
        step: 0,
        usage,
        wire,
    };
    let reg = ToolRegistry::new();
    run_turn(&mut s, &mut p, &reg, "hello").unwrap();
    run_turn(&mut s, &mut p, &reg, "again").unwrap();
}

#[test]
fn old_style_session_keeps_old_lines_and_shows_na_block() {
    let dir = tmp("old");
    fixture(
        &dir,
        "old",
        false,
        json!({"prompt_tokens": 100, "completion_tokens": 10}),
    );
    let out = status(&dir, "old");
    let (head, _) = out.split_once("usage:").expect("usage line");
    // The pre-existing lines, byte for byte, in their original order.
    assert!(head.starts_with("session: old\nentries: "), "{out}");
    assert!(head.contains("\npc:      Final\n"), "{out}");
    assert!(
        head.ends_with("turns:   2 (user messages)\nsystem:  -\n"),
        "{out}"
    );
    let legacy_usage = "usage:   200 in / 20 out tokens, $0.0000 USD\n";
    let block = out.split_once(legacy_usage).expect("legacy usage line").1;
    assert!(block.contains("steps:   2"), "{block}");
    assert!(block.contains("200 prompt / 20 completion"), "{block}");
    assert!(block.contains("n/a cached (hit rate n/a)"), "{block}");
    assert!(block.contains("wire:    n/a"), "{block}");
    assert!(!block.contains("0 prefix"), "no fabricated zeros: {block}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn session_with_tole_wire_shows_split_cache_rate_and_growth() {
    let dir = tmp("new");
    fixture(
        &dir,
        "new",
        true,
        json!({"prompt_tokens": 100, "completion_tokens": 10,
               "prompt_tokens_details": {"cached_tokens": 50}}),
    );
    let out = status(&dir, "new");
    assert!(out.contains("usage:   200 in / 20 out tokens"), "{out}");
    assert!(out.contains("100 cached (hit rate 50.0%)"), "{out}");
    assert!(
        out.contains("first step: 1000 prefix (system+tools) + 40 history chars"),
        "{out}"
    );
    assert!(
        out.contains("last step:  1000 prefix (system+tools) + 80 history chars"),
        "{out}"
    );
    assert!(out.contains("history growth: +40.0 chars/step"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}
