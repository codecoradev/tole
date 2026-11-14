//! #596: the session log stores the redacted form. No known secret value,
//! in any form `SecretSet` knows, reaches the JSONL; an entry that hid one
//! names it; replay from the log reproduces the provider's view exactly;
//! an old raw log still loads unchanged.

use base64::Engine;
use serde_json::{json, Value};
use tole_core::entry::{Entry, EntryType, NewEntry};
use tole_core::openai::{OpenAiConfig, OpenAiProvider};
use tole_core::provider::{Provider, ProviderError, ProviderOutput};
use tole_core::secrets::SecretSet;
use tole_core::storage::{Commit, JsonlStorage, Storage};
use tole_core::tool::{Risk, Tool, ToolRegistry};
use tole_core::turn::run_turn;

/// A secret with characters that make every encoded form differ.
const V: &str = "s3cr3t/v\"al+0596";
const NAME: &str = "TOLE_T596_TOKEN";

fn set() -> SecretSet {
    SecretSet::default().with(NAME, V)
}

fn tmpdir(tag: &str) -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("tole-596-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Every form of `V` a sink could carry.
fn forms() -> Vec<String> {
    let json = serde_json::to_string(V).unwrap();
    let pct: String = V
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    vec![
        V.to_string(),
        json[1..json.len() - 1].to_string(),
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(V),
        pct,
        // A fragment no redaction form may leave behind.
        "s3cr3t/v".to_string(),
    ]
}

fn assert_clean(file: &str) {
    for f in forms() {
        assert!(!file.contains(&f), "{f:?} reached the log:\n{file}");
    }
}

/// The raw payloads of a session that met the secret everywhere.
fn raw_payloads() -> Vec<(&'static str, Value)> {
    let f = forms();
    vec![
        (
            "message",
            json!({"role": "user", "text": format!("my key is {V}")}),
        ),
        (
            "intent",
            json!({"tool": "run_command", "input": {"command": format!("curl -H 'X: {}'", f[3])},
                   "replay": "unsafe"}),
        ),
        (
            "tool_result",
            json!({"ok": true, "output": {"stdout": format!("{{\"token\":\"{}\"}}", f[1]),
                                          "b64": f[2], "pin": V}}),
        ),
        (
            "error",
            json!({"ok": false, "error": format!("auth failed for {V}")}),
        ),
        (
            "message",
            json!({"role": "assistant", "text": format!("done, used {V}")}),
        ),
        (
            "message",
            json!({"role": "assistant", "text": "no secret here"}),
        ),
    ]
}

fn commit_all(s: &mut JsonlStorage) -> Vec<Entry> {
    let mut parent: Option<String> = None;
    let mut out = Vec::new();
    for (kind, payload) in raw_payloads() {
        let e = NewEntry {
            id: None,
            parent_id: parent.clone(),
            kind: EntryType::from_string(kind),
            payload,
            timestamp: 1,
        };
        let c = s.commit(Commit::new().entry(e)).unwrap();
        parent = Some(c[0].id.clone());
        out.extend(c);
    }
    out
}

#[test]
fn no_form_of_a_secret_reaches_the_log_and_each_entry_names_what_it_hid() {
    let dir = tmpdir("forms");
    let mut s = JsonlStorage::create(&dir, "f", None).unwrap();
    s.set_secrets(set());
    let committed = commit_all(&mut s);
    let file = std::fs::read_to_string(dir.join("f.jsonl")).unwrap();
    assert_clean(&file);
    // Memory holds the same redacted form as the disk.
    assert_clean(&serde_json::to_string(&committed).unwrap());
    let lines: Vec<Value> = file
        .lines()
        .skip(1)
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 6);
    for l in &lines[..5] {
        assert_eq!(l["redacted"], json!([NAME]), "{l}");
        assert!(l.to_string().contains(&format!("«{NAME}»")), "{l}");
    }
    assert!(
        lines[5].get("redacted").is_none(),
        "nothing hidden: no field"
    );
}

#[test]
fn replay_from_the_log_reproduces_the_provider_view_exactly() {
    let dir = tmpdir("replay");
    let mut s = JsonlStorage::create(&dir, "r", None).unwrap();
    s.set_secrets(set());
    let committed = commit_all(&mut s);
    let p = OpenAiProvider::new(OpenAiConfig::new(
        "https://x.example/v1",
        "m",
        "sk-test-0596-key",
    ))
    .with_secrets(&set());
    let live = serde_json::to_string(&p.request_body(s.entries())).unwrap();
    // The provider redacts the raw entries to the same body: one rule.
    let raw: Vec<Entry> = committed
        .iter()
        .zip(raw_payloads())
        .map(|(e, (_, payload))| Entry {
            payload,
            ..e.clone()
        })
        .collect();
    assert_eq!(serde_json::to_string(&p.request_body(&raw)).unwrap(), live);
    drop(s);
    let back = JsonlStorage::open(dir.join("r.jsonl")).unwrap();
    assert_eq!(back.entries(), &committed[..], "disk == memory");
    let replayed = serde_json::to_string(&p.request_body(back.entries())).unwrap();
    assert_eq!(replayed, live, "replay == what the provider saw");
    assert_clean(&live);
}

#[test]
fn an_old_raw_log_loads_unchanged_and_only_new_lines_are_redacted() {
    let dir = tmpdir("old");
    let path = dir.join("old.jsonl");
    let old = format!(
        "{}\n{}\n",
        json!({"v": 1, "kind": "header", "id": "old", "storageVersion": 1, "created_at": 1, "cwd": ""}),
        json!({"kind": "entry", "seq": 1, "id": "e_1", "type": "message", "timestamp": 1,
               "payload": {"role": "user", "text": format!("raw {V}")}})
    );
    std::fs::write(&path, &old).unwrap();
    let mut s = JsonlStorage::open(&path).unwrap();
    s.set_secrets(set());
    assert_eq!(
        s.entries()[0].payload["text"],
        json!(format!("raw {V}")),
        "no rewrite"
    );
    let p = OpenAiProvider::new(OpenAiConfig::new(
        "https://x.example/v1",
        "m",
        "sk-test-0596-key",
    ))
    .with_secrets(&set());
    assert_clean(&p.request_body(s.entries()).to_string());
    s.commit(Commit::new().entry(NewEntry::with_parent(
        "e_1",
        EntryType::new(EntryType::MESSAGE),
        json!({"role": "assistant", "text": format!("ok {V}")}),
    )))
    .unwrap();
    let file = std::fs::read_to_string(&path).unwrap();
    assert!(file.starts_with(&old), "old lines stay byte for byte");
    assert_clean(&file[old.len()..]);
    drop(s);
    let back = JsonlStorage::open(&path).unwrap();
    assert_eq!(back.entries().len(), 2);
}

#[test]
fn a_secret_split_across_streamed_chunks_is_redacted_in_the_committed_message() {
    // Deltas only feed the live display; the log gets the assembled message
    // in one entry, so a value split across two chunks is still whole there.
    let chunks = ["answer: s3cr3t/v", "\"al+0596 end"];
    let dir = tmpdir("chunks");
    let mut s = JsonlStorage::create(&dir, "c", None).unwrap();
    s.set_secrets(set());
    s.commit(Commit::new().entry(NewEntry::root(
        EntryType::new(EntryType::MESSAGE),
        json!({"role": "assistant", "text": chunks.concat()}),
    )))
    .unwrap();
    assert_clean(&std::fs::read_to_string(dir.join("c.jsonl")).unwrap());
}

/// Records the input it ran with.
struct Recorder(std::sync::Mutex<Vec<Value>>);
impl Tool for Recorder {
    fn name(&self) -> &str {
        "echo"
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        self.0.lock().unwrap().push(input.clone());
        Ok(json!({"echo": input}))
    }
}

/// Records every transcript it is handed.
struct Seer {
    seen: Vec<String>,
    script: Vec<ProviderOutput>,
}
impl Provider for Seer {
    fn complete(&mut self, t: &[Entry]) -> Result<ProviderOutput, ProviderError> {
        self.seen.push(serde_json::to_string(t).unwrap());
        if self.script.is_empty() {
            return Err(ProviderError("exhausted".into()));
        }
        Ok(self.script.remove(0))
    }
}

#[test]
fn a_whole_turn_logs_only_redacted_text_and_feeds_the_provider_the_same() {
    let dir = tmpdir("turn");
    let mut s = JsonlStorage::create(&dir, "t", None).unwrap();
    s.set_secrets(set());
    let tool = std::sync::Arc::new(Recorder(Default::default()));
    struct Shared(std::sync::Arc<Recorder>);
    impl Tool for Shared {
        fn name(&self) -> &str {
            "echo"
        }
        fn risk(&self) -> Risk {
            Risk::ReadOnly
        }
        fn execute(&self, input: Value) -> Result<Value, String> {
            self.0.execute(input)
        }
    }
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(Shared(tool.clone()))).unwrap();
    let mut p = Seer {
        seen: Vec::new(),
        script: vec![
            ProviderOutput::ToolCall {
                tool: "echo".into(),
                input: json!({"arg": V}),
            },
            ProviderOutput::Final {
                text: format!("final {V}"),
            },
        ],
    };
    run_turn(&mut s, &mut p, &reg, &format!("use {V}")).unwrap();
    // The tool ran with the arguments the model sent.
    assert_eq!(tool.0.lock().unwrap()[0], json!({"arg": V}));
    // Disk, and every transcript the provider was handed, hold no form.
    assert_clean(&std::fs::read_to_string(dir.join("t.jsonl")).unwrap());
    for t in &p.seen {
        assert_clean(t);
    }
}

#[test]
fn the_pinned_system_prompt_is_stored_redacted() {
    // The header is written at creation, with the process set.
    tole_core::secrets::set_process_secrets(set());
    let dir = tmpdir("header");
    let s = JsonlStorage::create_with(&dir, "h", None, Some(&format!("you hold {V}"))).unwrap();
    assert_eq!(
        s.system_prompt(),
        Some(format!("you hold «{NAME}»").as_str())
    );
    assert_clean(&std::fs::read_to_string(dir.join("h.jsonl")).unwrap());
}
