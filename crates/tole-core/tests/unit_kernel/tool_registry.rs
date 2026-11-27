//! The tool registry: registration, retention and the approval description.

use serde_json::{json, Value};
use tole_core::read_file::ReadFileTool;
use tole_core::tool::*;

struct FakeWrite;
struct FakeDestructive;

impl Tool for FakeWrite {
    fn name(&self) -> &str {
        "fake_write"
    }
    fn risk(&self) -> Risk {
        Risk::Write
    }
    fn execute(&self, _: Value) -> Result<Value, String> {
        Ok(json!({}))
    }
}

impl Tool for FakeDestructive {
    fn name(&self) -> &str {
        "fake_destructive"
    }
    fn risk(&self) -> Risk {
        Risk::Destructive
    }
    fn execute(&self, _: Value) -> Result<Value, String> {
        Ok(json!({}))
    }
}

/// Deny-all AND interactive: satisfies BOTH registration invariants
/// (Write needs an approver, Destructive needs an interactive one)
/// without granting anything.
struct DenyAllInteractive;

impl tole_core::approval::Approver for DenyAllInteractive {
    fn decide(&self, _req: &tole_core::approval::ToolRequest<'_>) -> tole_core::approval::Verdict {
        tole_core::approval::Verdict::Deny
    }
    fn interactive(&self) -> bool {
        true
    }
}

/// Write tool whose `describe` echoes a model-controlled string.
struct EchoDescribe;

impl Tool for EchoDescribe {
    fn name(&self) -> &str {
        "echo_describe"
    }
    fn risk(&self) -> Risk {
        Risk::Write
    }
    fn describe(&self, input: &Value) -> String {
        format!("start job: {}", input["cmd"].as_str().unwrap_or(""))
    }
    fn execute(&self, _: Value) -> Result<Value, String> {
        Ok(json!({}))
    }
}

/// Records every description it is asked to decide on.
struct Recording(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

impl tole_core::approval::Approver for Recording {
    fn decide(&self, req: &tole_core::approval::ToolRequest<'_>) -> tole_core::approval::Verdict {
        self.0.lock().unwrap().push(req.description.clone());
        tole_core::approval::Verdict::Deny
    }
    fn interactive(&self) -> bool {
        true
    }
}

fn decide_description(cmd: &str) -> String {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut reg = ToolRegistry::with_approver(Recording(seen.clone()));
    reg.register(Box::new(EchoDescribe)).unwrap();
    reg.decide("echo_describe", &json!({ "cmd": cmd })).unwrap();
    let v = seen.lock().unwrap();
    v[0].clone()
}

#[test]
fn decide_sanitizes_hostile_description() {
    let d = decide_description("ls\x1b[2K\rAPPROVED \u{202E}gnp.exe\u{2028}rm -rf /");
    for bad in ['\x1b', '\r', '\u{202E}', '\u{2028}'] {
        assert!(!d.contains(bad), "raw {bad:?} reached approver: {d:?}");
    }
    assert!(d.contains("\\u{1b}") && d.contains("\\u{202e}"), "{d:?}");
}

#[test]
fn decide_leaves_normal_description_unchanged() {
    let cmd = "echo \"héllo\" 日本語 🚀 'x'";
    assert_eq!(decide_description(cmd), format!("start job: {cmd}"));
}

fn registry_with_all_risks() -> ToolRegistry {
    // Write/Destructive registration REQUIRES an (interactive)
    // approver — a deny-all one is enough for this test.
    let mut reg = ToolRegistry::with_approver(DenyAllInteractive);
    reg.register(Box::new(ReadFileTool::new(std::env::temp_dir())))
        .unwrap();
    reg.register(Box::new(FakeWrite)).unwrap();
    reg.register(Box::new(FakeDestructive)).unwrap();
    reg
}

#[test]
fn retain_read_only_keeps_only_readonly_tools() {
    let mut reg = registry_with_all_risks();
    assert_eq!(reg.specs().len(), 3);
    reg.retain_read_only();
    let names: Vec<String> = reg
        .specs()
        .iter()
        .filter_map(|s| s["function"]["name"].as_str())
        .map(str::to_string)
        .collect();
    assert_eq!(names, vec!["read_file".to_string()]);
    assert!(reg.get("fake_write").is_none());
    assert!(reg.get("fake_destructive").is_none());
    assert!(reg.get("read_file").is_some());
}

/// #1039: naming a tool keeps a Write one beside the ReadOnly ones, never
/// a Destructive one, and never a Write tool that was not named.
#[test]
fn retain_read_only_and_keeps_only_named_write_tools() {
    let mut reg = registry_with_all_risks();
    reg.retain_read_only_and(&["fake_write".into(), "fake_destructive".into()]);
    assert!(reg.get("read_file").is_some() && reg.get("fake_write").is_some());
    assert!(reg.get("fake_destructive").is_none());
    let mut none = registry_with_all_risks();
    none.retain_read_only_and(&["other".into()]);
    assert!(none.get("read_file").is_some() && none.get("fake_write").is_none());
}

/// Issue #320: an external `Tool` that predates `summary()` (it
/// implements only the required methods) must keep compiling, and
/// gets the safe default — never an error string, even when its
/// `describe` validates input and fails for `Null`.
#[test]
fn external_tool_without_summary_gets_safe_default() {
    struct Legacy;
    impl Tool for Legacy {
        fn name(&self) -> &str {
            "legacy"
        }
        fn risk(&self) -> Risk {
            Risk::ReadOnly
        }
        fn describe(&self, input: &Value) -> String {
            match input.get("x") {
                Some(_) => "legacy call".into(),
                None => "legacy: missing 'x'".into(),
            }
        }
        fn execute(&self, _: Value) -> Result<Value, String> {
            Ok(json!({}))
        }
    }
    assert_eq!(Legacy.summary(), "legacy (ReadOnly)");
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(Legacy)).unwrap();
    let specs = reg.specs();
    assert_eq!(specs[0]["function"]["description"], "legacy (ReadOnly)");
}

#[test]
fn retain_read_only_is_idempotent() {
    let mut reg = registry_with_all_risks();
    reg.retain_read_only();
    reg.retain_read_only();
    assert_eq!(reg.specs().len(), 1);
}
