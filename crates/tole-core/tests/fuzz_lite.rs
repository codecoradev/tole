//! E9 fuzz-lite (#9): panic-free contracts under hostile input —
//! tool-argument fuzzing and output-truncation invariants. Deterministic
//! (seeded LCG, no rand dep): the same corpus runs in CI every time;
//! randomized-but-reproducible beats exhaustive for the harness's
//! surface.
//!
//! Every test here drives shell-tools-gated modules (git, run_command,
//! subprocess), so the whole file is gated (#310).
#![cfg(feature = "shell-tools")]

use serde_json::{json, Value};
use tole_core::file_tools::{DeleteFileTool, EditFileTool};
use tole_core::git::GitTool;
use tole_core::read_file::ReadFileTool;
use tole_core::run_command::RunCommandTool;
use tole_core::subprocess::check_destructive_argv;
use tole_core::tool::Tool;

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("tole-fuzz-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Tiny deterministic LCG — same corpus every run.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn pick<'a>(&mut self, items: &'a [String]) -> &'a str {
        &items[(self.next() % items.len() as u64) as usize]
    }
    fn pick_key<'a>(&mut self, items: &'a [&'a str]) -> &'a str {
        items[(self.next() % items.len() as u64) as usize]
    }
}

fn hostile_strings() -> Vec<String> {
    vec![
        "".into(),
        " ".into(),
        "\n".into(),
        "\0".into(),
        "\u{1}".into(),
        "\u{7f}".into(),
        "\u{e9}".into(),
        "\u{1f980}".into(),
        "../..".into(),
        "..\\..".into(),
        "/".into(),
        "//".into(),
        "\\".into(),
        "\\\\?".into(),
        "C:\\evil".into(),
        "-".into(),
        "--".into(),
        "-rf".into(),
        "--upload-pack=x".into(),
        "--exec=/bin/sh".into(),
        "-C".into(),
        "--repo".into(),
        "%s%n".into(),
        "{{}}".into(),
        "${X}".into(),
        "`x`".into(),
        "$(x)".into(),
        "; rm -rf /".into(),
        "| cat".into(),
        "&& touch /tmp/pwn".into(),
        "a".repeat(300),
        "%".repeat(64),
        "\u{200b}".into(),
        "\u{feff}".into(),
    ]
}

const HOSTILE_KEYS: &[&str] = &[
    "path",
    "cwd",
    "content",
    "old_text",
    "new_text",
    "op",
    "paths",
    "message",
    "body",
    "query",
    "text",
    "session_id",
];

fn hostile_json(rng: &mut Lcg, strings: &[String], depth: u8) -> Value {
    if depth == 0 {
        return json!(rng.pick(strings));
    }
    match rng.next() % 5 {
        0 => json!(rng.pick(strings)),
        1 => json!(rng.next()),
        2 => json!(rng.next().is_multiple_of(2)),
        3 => json!(null),
        4 => {
            let mut obj = serde_json::Map::new();
            // Guarantee the tool's recognized key is present with a
            // hostile value at least sometimes.
            if rng.next().is_multiple_of(2) {
                let k = rng.pick_key(HOSTILE_KEYS);
                obj.insert(k.to_string(), json!(rng.pick(strings)));
            }
            let filler = rng.next() % 3;
            for _ in 0..filler {
                obj.insert(
                    rng.pick_key(HOSTILE_KEYS).to_string(),
                    hostile_json(rng, strings, depth - 1),
                );
            }
            Value::Object(obj)
        }
        _ => unreachable!(),
    }
}

/// Every tool must return Ok/Err — never panic — on hostile input
/// (E9: "panic-free on bad input").
#[test]
fn tools_never_panic_on_hostile_input() {
    let dir = tmpdir("hostile");
    let mut rng = Lcg(0x746f6c65); // "tole"
    let strings = hostile_strings();
    let read = ReadFileTool::new(dir.clone());
    let edit = EditFileTool::new(dir.clone());
    let delete = DeleteFileTool::new(dir.clone());
    let git = GitTool::new().in_dir(dir.clone());
    let run = RunCommandTool::new(dir.clone());

    for i in 0..400 {
        let input = hostile_json(&mut rng, &strings, 3);
        // Each call must settle (Ok or Err) — a panic fails the test.
        let _ = read.execute(input.clone());
        let _ = edit.execute(input.clone());
        let _ = delete.execute(input.clone());
        let _ = git.execute(input.clone());
        let _ = run.execute(input.clone());
        let _ = check_destructive_argv(&[
            "sh".to_string(),
            "-c".to_string(),
            format!("{i}-{}", rng.pick(&strings)),
        ]);
    }
}

/// The argv blocklist holds under fuzzed argv shapes: destructive
/// patterns stay refused, benign ones stay allowed (no panics either
/// way).
#[test]
fn destructive_argv_fuzz_stays_correct() {
    let mut rng = Lcg(0x626c6f6b); // "block"
    let progs = [
        "sh", "bash", "env", "xargs", "nohup", "sudo", "rm", "dd", "cat", "git",
    ];
    let flags = [
        "-c",
        "-rf",
        "-r",
        "--",
        "-n",
        "1",
        "-C",
        ".",
        "",
        "--exec=/bin/sh",
    ];
    let targets = [
        "/",
        "/etc",
        "..",
        "~",
        "./build",
        "notes.txt",
        "",
        "a/../b",
        "-weird",
    ];

    for _ in 0..600 {
        let mut argv: Vec<String> = vec![rng.pick_key(&progs).to_string()];
        for _ in 0..3 {
            argv.push(rng.pick_key(&flags).to_string());
        }
        for _ in 0..2 {
            argv.push(rng.pick_key(&targets).to_string());
        }
        // Must settle; the specific verdict is already covered by the
        // targeted tests — here we assert no panic and that hard
        // destruction never passes.
        if let Ok(()) = check_destructive_argv(&argv) {
            let joined = argv.join(" ").to_ascii_lowercase();
            let has_teardown = [
                "dd ", "shutdown", "mkfs", " fdisk", "halt", "reboot", "poweroff",
            ]
            .iter()
            .any(|t| joined.contains(t));
            assert!(!has_teardown, "teardown payload must never pass: {argv:?}");
        }
    }
}

/// Output truncation invariants survive hostile payloads: truncated
/// text is ALWAYS marked (an approver must never mistake a partial view
/// for the whole input) and bounded regardless of input size.
#[test]
fn truncation_is_always_marked_and_bounded() {
    // McpTool::describe's truncate logic (mcp_server) is private; the
    // contract is exercised through jobs' log tail (bounded window) and
    // subprocess capture (32 MiB cap + marker). Both are covered by
    // their targeted tests; here we pin the INVARIANT: any bounded view
    // of a hostile payload keeps the marker text recognizable.
    let marker = "[tole: output truncated at 32 MiB]";
    // The subprocess capture cap is 32 MiB — a 40 MiB stream must come
    // back marked (not silently cut). The stream is generated IN the
    // child (argv size limits forbid shipping 40 MiB as an argument).
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c")
        .arg("head -c 41943040 /dev/zero | tr '\\0' 'x'");
    let out = tole_core::subprocess::run_with_timeout(&mut cmd, std::time::Duration::from_secs(60))
        .unwrap();
    assert!(out.stdout.len() <= 32 * 1024 * 1024 + 256, "cap must hold");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(marker),
        "truncation must be marked"
    );
}
