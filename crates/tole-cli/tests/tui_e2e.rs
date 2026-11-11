//! #460 acceptance: the real `tole` binary on a pty (bare `tole` = the TUI)
//! against a scripted mock provider: stream an answer, approve a write,
//! cancel a running turn, multi-line input, then the session is an ordinary
//! one (`tole sessions` lists it).
#![cfg(all(unix, feature = "tui"))]

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Reply by the last message: a tool result -> "WROTE-OK"; a prompt with
/// "write" -> a write_file call; "evil" -> an answer carrying ESC/CR; "slow" -> silence for 20s; else a two-chunk
/// streamed answer. Always SSE when the request streams.
fn spawn_mock() -> String {
    spawn_mock_recording(Arc::default())
}

/// Every chat request body the mock received, in order.
type Bodies = Arc<Mutex<Vec<Value>>>;

fn spawn_mock_recording(bodies: Bodies) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let bodies = bodies.clone();
            std::thread::spawn(move || serve(stream, bodies));
        }
    });
    format!("http://{addr}/v1")
}

fn serve(mut s: std::net::TcpStream, bodies: Bodies) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 65536];
    let deadline = Instant::now() + Duration::from_secs(10);
    let (head_end, len) = loop {
        let n = s.read(&mut chunk).unwrap_or(0);
        buf.extend_from_slice(&chunk[..n]);
        if let Some(h) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..h]).to_lowercase();
            let len = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            break (h + 4, len);
        }
        if n == 0 || Instant::now() > deadline {
            return;
        }
    };
    while buf.len() < head_end + len {
        let n = s.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let path_is_models = buf.starts_with(b"GET");
    let body: Value = serde_json::from_slice(&buf[head_end..head_end + len]).unwrap_or(Value::Null);
    let mut reply = |ctype: &str, data: String| {
        let r = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}",
            data.len()
        );
        let _ = s.write_all(r.as_bytes());
    };
    if path_is_models {
        return reply(
            "application/json",
            json!({"data": [{"id": "m"}, {"id": "alt-model"}, {"id": "other-one"}]}).to_string(),
        );
    }
    bodies.lock().unwrap().push(body.clone());
    let last = body["messages"]
        .as_array()
        .and_then(|m| m.last())
        .cloned()
        .unwrap_or(Value::Null);
    let said = last["content"].as_str().unwrap_or("").to_string();
    let call = json!({"index": 0, "id": "c1", "function": {"name": "write_file",
        "arguments": json!({"path": "out.txt", "content": "x"}).to_string()}});
    let deltas: Vec<Value> = if last["role"] == "tool" {
        vec![json!({"content": "WROTE-OK"})]
    } else if said.contains("write") {
        vec![json!({"tool_calls": [call]})]
    } else if said.contains("evil") {
        vec![json!({"content": "EV\x1b[2J\rIL"})]
    } else if said.contains("hold") {
        std::thread::sleep(Duration::from_millis(1500));
        vec![json!({"content": "HELD-ANSWER"})]
    } else if said.contains("slow") {
        std::thread::sleep(Duration::from_secs(20));
        vec![json!({"content": "LATE"})]
    } else {
        vec![json!({"content": "STREAM-"}), json!({"content": "ANSWER"})]
    };
    let usage = json!({"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10});
    if body["stream"].as_bool() == Some(true) {
        let mut out = String::new();
        for d in deltas {
            out.push_str(&format!(
                "data: {}\n\n",
                json!({"choices": [{"index": 0, "delta": d, "finish_reason": null}]})
            ));
        }
        out.push_str(&format!(
            "data: {}\n\n",
            json!({"choices": [], "usage": usage})
        ));
        out.push_str("data: [DONE]\n\n");
        reply("text/event-stream", out);
    } else {
        let d = &deltas[0];
        let msg = if d["tool_calls"].is_array() {
            json!({"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function",
                "function": d["tool_calls"][0]["function"]}]})
        } else {
            json!({"role": "assistant", "content": "STREAM-ANSWER"})
        };
        reply(
            "application/json",
            json!({"id": "c", "object": "chat.completion", "created": 1, "model": "m",
                "choices": [{"index": 0, "message": msg, "finish_reason": "stop"}], "usage": usage})
            .to_string(),
        );
    }
}

/// `tole` on a fresh pty; everything it prints lands in `out`.
struct Pty {
    child: Child,
    master: std::fs::File,
    out: Arc<Mutex<Vec<u8>>>,
    seen: usize,
}

impl Pty {
    fn spawn(cwd: &Path, url: &str) -> Pty {
        Pty::spawn_with(cwd, url, &[])
    }

    fn spawn_with(cwd: &Path, url: &str, args: &[String]) -> Pty {
        Pty::spawn_full(cwd, url, args, &[])
    }

    fn spawn_full(cwd: &Path, url: &str, args: &[String], envs: &[(&str, &str)]) -> Pty {
        let (mut m, mut sl) = (0, 0);
        let ws = libc::winsize {
            ws_row: 30,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let rc =
            unsafe { libc::openpty(&mut m, &mut sl, std::ptr::null_mut(), std::ptr::null(), &ws) };
        assert_eq!(rc, 0, "openpty");
        let (master, slave) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(sl)) };
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
        cmd.args(args)
            .current_dir(cwd)
            .env("CODECORA_HOME", cwd.join("home"))
            .env("TOLE_BASE_URL", url)
            .env("TOLE_MODEL", "m")
            .env("TOLE_API_KEY", "k")
            .env("TOLE_NO_UPDATE_CHECK", "1")
            .env("TERM", "xterm-256color")
            .env("PATH", "/usr/bin:/bin")
            .env_remove("TOLE_MEMORY")
            .env_remove("TOLE_TRUST")
            .env_remove("TOLE_VISION")
            .env_remove("TOLE_THEME")
            .env_remove("NO_COLOR")
            .envs(envs.iter().copied())
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
        let child = cmd.spawn().expect("spawn tole");
        drop(slave);
        let master = std::fs::File::from(master);
        let mut reader = master.try_clone().unwrap();
        // The inline viewport asks the terminal for the cursor (CSI 6n); a
        // real terminal answers, so the fixture must too.
        let mut answer = master.try_clone().unwrap();
        let out = Arc::new(Mutex::new(Vec::new()));
        let sink = out.clone();
        std::thread::spawn(move || {
            let mut b = [0u8; 4096];
            while let Ok(n) = reader.read(&mut b) {
                if n == 0 {
                    break;
                }
                if b[..n].windows(4).any(|w| w == b"\x1b[6n") {
                    let _ = answer.write_all(b"\x1b[1;1R");
                }
                sink.lock().unwrap().extend_from_slice(&b[..n]);
            }
        });
        Pty {
            child,
            master,
            out,
            seen: 0,
        }
    }

    fn send(&mut self, s: &str) {
        self.master.write_all(s.as_bytes()).unwrap();
        self.master.flush().unwrap();
    }

    /// Wait until `needle` appears in output not yet consumed; consumes up to it.
    fn expect(&mut self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            {
                let o = self.out.lock().unwrap();
                let text = String::from_utf8_lossy(&o[self.seen..]).to_string();
                if let Some(i) = text.find(needle) {
                    self.seen += text[..i + needle.len()].len();
                    return;
                }
                if Instant::now() > deadline {
                    panic!("timed out waiting for {needle:?}; got: {text:?}");
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// The turn is over: the idle hint (redrawn when `working…` goes away)
    /// is back, so hotkeys and commands are accepted again.
    fn idle(&mut self) {
        self.expect("Shift+Tab thinking");
    }

    fn wait_exit(&mut self) -> Option<i32> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Some(st)) = self.child.try_wait() {
                return st.code();
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = self.child.kill();
        None
    }
}

fn workdir() -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "tole-tui-e2e-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn tui_streams_approves_cancels_and_leaves_an_ordinary_session() {
    let (url, dir) = (spawn_mock(), workdir());
    let mut t = Pty::spawn(&dir, &url);
    t.expect("effort:"); // footer rendered

    // 1. streamed answer
    t.send("hello\r");
    t.expect("STREAM-ANSWER");

    // 2. a write asks inline; `y` lets it run
    t.send("write it\r");
    t.expect("write file out.txt"); // the inline approval prompt
    assert!(
        !dir.join("out.txt").exists(),
        "must not write before approval"
    );
    t.send("y");
    t.expect("WROTE-OK");
    assert!(dir.join("out.txt").exists(), "approved write must run");

    // 3. Shift+Enter (kitty CSI-u) makes a newline instead of submitting
    t.send("multi1\x1b[13;2umulti2\r");
    t.expect("STREAM-ANSWER");

    // 4. Esc cancels a running turn
    t.send("slow\r");
    t.expect("working");
    t.send("\x1b");
    t.expect("(turn cancelled)");

    // 5. quit; the session is an ordinary one
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));

    let sess_dir = dir.join(".tole/sessions");
    let file = std::fs::read_dir(&sess_dir)
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
        .expect("session file");
    let log = std::fs::read_to_string(file.path()).unwrap();
    assert!(
        log.contains("multi1\\nmulti2"),
        "multi-line prompt recorded: {log}"
    );
    let id = file
        .file_name()
        .to_string_lossy()
        .trim_end_matches(".jsonl")
        .to_string();
    let listed = Command::new(env!("CARGO_BIN_EXE_tole"))
        .arg("sessions")
        .current_dir(&dir)
        .env("CODECORA_HOME", dir.join("home"))
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains(&id),
        "sessions lists {id}: {}",
        String::from_utf8_lossy(&listed.stdout)
    );
}

/// The TUI face must wire `--on-pretool` like every other command: a deny
/// (exit 2) stops the write even under `--yes`, and the denial is shown.
#[cfg(feature = "shell-tools")]
#[test]
fn tui_enforces_a_deny_only_pretool_hook() {
    let (url, dir) = (spawn_mock(), workdir());
    let script = dir.join("deny.sh");
    std::fs::write(&script, "#!/bin/sh\necho policy says no\nexit 2\n").unwrap();
    let args = [
        "--on-pretool".to_string(),
        format!("/bin/sh {}", script.display()),
        "tui".into(),
        "--yes".into(),
    ];
    let mut t = Pty::spawn_with(&dir, &url, &args);
    t.expect("effort:");
    t.send("write it\r");
    t.expect("approval denied for 'write_file'");
    assert!(!dir.join("out.txt").exists(), "denied write must not run");
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));
}

/// Provider text with ESC/CR must reach the terminal only as visible
/// `\u{..}` escapes, never as the raw sequence (terminal injection).
#[test]
fn tui_never_passes_escape_sequences_from_provider_text() {
    let (url, dir) = (spawn_mock(), workdir());
    let mut t = Pty::spawn(&dir, &url);
    t.expect("effort:");
    t.send("evil\r");
    t.expect("EV\\u{1b}[2J\\u{d}IL");
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));
    let o = t.out.lock().unwrap();
    let seen = String::from_utf8_lossy(&o);
    assert!(
        !seen.contains("\x1b[2J\rIL"),
        "raw injection reached the pty"
    );
    assert!(!seen.contains("[2J\rIL"), "raw injection reached the pty");
}

/// #461 acceptance: switch the model through the picker (Ctrl+L) and the
/// thinking level through `/thinking`; the NEXT provider request carries
/// both, the status shows them, and a later `tole resume` still sends them
/// (they are session registers, not process state).
#[test]
fn tui_model_picker_and_thinking_reach_the_next_request_and_survive_resume() {
    let bodies = Bodies::default();
    let (url, dir) = (spawn_mock_recording(bodies.clone()), workdir());
    let mut t = Pty::spawn(&dir, &url);
    t.expect("effort:");
    t.send("hello\r");
    t.expect("STREAM-ANSWER");
    t.idle();

    // Ctrl+L: picker over /models, filter as you type, Enter picks
    t.send("\x0c");
    t.expect("model>");
    t.send("alt");
    t.expect("alt-model");
    t.send("\r");
    t.expect("model → alt-model");

    // /thinking low (the editor border shows the level)
    t.send("/thinking low\r");
    t.expect("thinking → low");
    t.expect("thinking: low");

    // unknown commands are refused with a hint and nothing is sent
    t.send("/mod\r");
    t.expect("did you mean /model");

    t.send("second\r");
    t.expect("STREAM-ANSWER");
    t.idle();
    {
        let b = bodies.lock().unwrap();
        assert_eq!(b.len(), 2, "only the two prompts reached the provider");
        assert_eq!(b[0]["model"], "m");
        assert!(b[0].get("reasoning_effort").is_none(), "{}", b[0]);
        assert_eq!(
            b[1]["model"], "alt-model",
            "next call carries the new model"
        );
        assert_eq!(b[1]["reasoning_effort"], "low", "and the new effort");
    }

    t.send("/name e2e-run\r");
    t.expect("named e2e-run");
    t.send("/session\r");
    t.expect("alt-model");
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));

    let file = std::fs::read_dir(dir.join(".tole/sessions"))
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
        .expect("session file");
    let id = file
        .file_name()
        .to_string_lossy()
        .trim_end_matches(".jsonl")
        .to_string();
    let tole = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_tole"))
            .args(args)
            .current_dir(&dir)
            .env("CODECORA_HOME", dir.join("home"))
            .env("TOLE_BASE_URL", &url)
            .env("TOLE_MODEL", "m")
            .env("TOLE_API_KEY", "k")
            .env("TOLE_NO_UPDATE_CHECK", "1")
            .output()
            .unwrap()
    };
    let out = tole(&["resume", &id, "third"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let b = bodies.lock().unwrap();
    let last = b.last().unwrap();
    assert_eq!(last["model"], "alt-model", "model register survives resume");
    assert_eq!(
        last["reasoning_effort"], "low",
        "effort register survives resume"
    );
    drop(b);
    let listed = tole(&["sessions", "--all"]);
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("e2e-run"),
        "{}",
        String::from_utf8_lossy(&listed.stdout)
    );
}

/// #462 acceptance: steer a running turn (soft interrupt, #377), queue
/// follow-ups that run one by one after it settles, and get the queue back in
/// the editor on cancel: all on a pty against the mock provider.
#[test]
fn tui_steers_queues_and_returns_the_queue_on_cancel() {
    let bodies = Bodies::default();
    let (url, dir) = (spawn_mock_recording(bodies.clone()), workdir());
    let mut t = Pty::spawn(&dir, &url);
    t.expect("effort:");

    // 1. Enter while the turn runs steers: the first answer is withdrawn at
    //    the safe point and the next provider call carries the steering text.
    t.send("hold-a\r");
    t.expect("working");
    t.send("go-left\r");
    t.expect("[steer]");
    t.expect("STREAM-ANSWER");
    t.idle();

    // 2. Alt+Enter (ESC CR) queues; the rows show; after the turn settles the
    //    head runs, then the next, one at a time.
    t.send("hold-b\r");
    t.expect("working");
    t.send("qa\x1b\r");
    t.send("qb\x1b\r");
    t.expect("queued: qb");
    t.expect("HELD-ANSWER");
    t.expect("> qa");
    t.expect("> qb");
    t.idle();

    // 3. Esc cancels and puts the queue back in the editor (not lost).
    t.send("slow\r");
    t.expect("working");
    t.send("qc\x1b\r");
    t.expect("queued: qc");
    t.send("\x1b");
    t.expect("(turn cancelled)");
    t.send("\r"); // the editor holds "qc" again: sending it runs it
    t.expect("STREAM-ANSWER");
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));

    let all = bodies.lock().unwrap();
    // the steering text reached a provider call as the last (user) message
    assert!(
        all.iter().any(|b| {
            let last = b["messages"].as_array().and_then(|m| m.last()).unwrap();
            last["role"] == "user"
                && last["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("owner interrupt") && c.contains("go-left"))
        }),
        "steering text never reached the provider: {all:?}"
    );
    // the last request carries every prompt, in the order they ran
    let last = all.last().unwrap()["messages"].as_array().unwrap();
    let users: Vec<&str> = last
        .iter()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| m["content"].as_str())
        .collect();
    let mut at = 0;
    for want in ["hold-a", "go-left", "hold-b", "qa", "qb", "slow", "qc"] {
        let i = users[at..]
            .iter()
            .position(|u| u.contains(want))
            .unwrap_or_else(|| panic!("{want} missing/out of order in {users:?}"));
        at += i + 1;
    }
}

/// The user messages of one recorded request, as the provider saw them.
fn user_contents(body: &Value) -> Vec<Value> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].clone())
        .collect()
}

/// #560 regression. Before the fix the TUI never asked for bracketed paste,
/// so a multi-line paste arrived as keys: its first line was SENT at the
/// first CR, the next line steered that turn, and the rest was left in the
/// editor; a paste wrapped in bracketed-paste markers was dropped entirely.
/// Now the terminal is asked for bracketed paste, the block lands in the
/// editor as one edit (CRLF/CR become LF, control chars are dropped) and
/// nothing is sent until Enter.
#[test]
fn tui_bracketed_paste_is_one_block_that_waits_for_enter() {
    let bodies = Bodies::default();
    let (url, dir) = (spawn_mock_recording(bodies.clone()), workdir());
    let mut t = Pty::spawn(&dir, &url);
    t.expect("effort:");
    assert!(
        String::from_utf8_lossy(&t.out.lock().unwrap()).contains("\x1b[?2004h"),
        "the TUI enables bracketed paste"
    );

    // What a terminal in bracketed-paste mode sends for a 3-line paste.
    t.send("\x1b[200~line-one\r\nline-two\rline-\x07three\x1b[201~");
    t.expect("line-three");
    std::thread::sleep(Duration::from_millis(800));
    assert!(
        bodies.lock().unwrap().is_empty(),
        "a paste never submits: {:?}",
        bodies.lock().unwrap()
    );

    t.send("\r");
    t.expect("STREAM-ANSWER");
    t.idle();
    {
        let b = bodies.lock().unwrap();
        assert_eq!(b.len(), 1, "one message, sent by Enter");
        assert_eq!(
            user_contents(&b[0]),
            vec![json!("line-one\nline-two\nline-three")]
        );
    }
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));
    assert!(
        String::from_utf8_lossy(&t.out.lock().unwrap()).contains("\x1b[?2004l"),
        "bracketed paste is disabled again on exit"
    );
}

/// A 1x1 PNG (the host sniffs the bytes, so it must be a real one).
const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

/// Whether a user message content carries an image part.
fn has_image(content: &Value) -> bool {
    content
        .as_array()
        .is_some_and(|parts| parts.iter().any(|p| p["type"] == "image_url"))
}

/// #560: a pasted image path is attached (shown as a row, sent with the next
/// message only); `/image` outside the jail is refused with the host's error;
/// `/image clear` drops the queue; a non-image paste stays text.
#[test]
fn tui_attaches_pasted_and_slash_image_paths_to_the_next_message() {
    let bodies = Bodies::default();
    let (url, dir) = (spawn_mock_recording(bodies.clone()), workdir());
    std::fs::write(dir.join("shot.png"), PNG_1X1).unwrap();
    let outside = dir
        .parent()
        .unwrap()
        .join(format!("tole-e2e-outside-{}.png", std::process::id()));
    std::fs::write(&outside, PNG_1X1).unwrap();
    let mut t = Pty::spawn(&dir, &url);
    t.expect("effort:");

    // A pasted path to an image inside the workspace: attached, not inserted.
    t.send("\x1b[200~shot.png\x1b[201~");
    t.expect("image attached to your next message: shot.png (image/png, 67 B)");
    t.expect("image: shot.png (image/png, 67 B) · next message");
    t.send("look\r");
    t.expect("STREAM-ANSWER");
    t.idle();
    // The next message goes without it.
    t.send("again\r");
    t.expect("STREAM-ANSWER");
    t.idle();

    // Outside the jail: refused with the host's error, nothing queued.
    t.send(&format!("/image {}\r", outside.display()));
    t.expect("outside the workspace jail");
    // A pasted path outside the jail is text after all (and says why).
    t.send("see ");
    t.send(&format!("\x1b[200~{}\x1b[201~", outside.display()));
    t.expect("pasted as text, not attached");
    t.send("\r");
    t.expect("STREAM-ANSWER");
    t.idle();

    // `/image` + `/image clear`: the next message carries nothing.
    t.send("/image shot.png\r");
    t.expect("image: shot.png");
    t.send("/image clear\r");
    t.expect("(1 queued image(s) dropped)");
    t.send("plain\r");
    t.expect("STREAM-ANSWER");
    t.idle();
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));
    let _ = std::fs::remove_file(&outside);

    let b = bodies.lock().unwrap();
    assert_eq!(b.len(), 4, "{b:?}");
    let last_user = |i: usize| user_contents(&b[i]).last().cloned().unwrap();
    assert!(
        has_image(&last_user(0)),
        "the paste rode on `look`: {:?}",
        b[0]
    );
    assert!(!has_image(&last_user(1)), "only on the next message");
    assert_eq!(
        last_user(2),
        json!(format!("see {}", outside.display())),
        "a refused path was sent as the text it was"
    );
    assert!(!has_image(&last_user(3)), "`/image clear` dropped it");
}

/// #560: with a model declared non-vision, an attached image gives the same
/// model-capability error as `--image`; it surfaces as the turn's stop note.
#[test]
fn tui_image_on_a_non_vision_model_reports_the_capability_error() {
    let bodies = Bodies::default();
    let (url, dir) = (spawn_mock_recording(bodies.clone()), workdir());
    std::fs::write(dir.join("shot.png"), PNG_1X1).unwrap();
    let mut t = Pty::spawn_full(&dir, &url, &[], &[("TOLE_VISION", "0")]);
    t.expect("effort:");
    t.send("/image shot.png\r");
    t.expect("image: shot.png");
    t.send("look\r");
    t.expect("model-capability error");
    t.idle();
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));
    assert!(
        bodies.lock().unwrap().is_empty(),
        "refused before any request"
    );
}

/// The user-message texts recorded in a session file, and its `forkedFrom`
/// header (`session`, `entry`) if it is a fork.
fn session_facts(path: &Path) -> (Vec<String>, Option<(String, String)>) {
    let text = std::fs::read_to_string(path).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    let origin = header.get("forkedFrom").map(|f| {
        (
            f["session"].as_str().unwrap().to_string(),
            f["entry"].as_str().unwrap().to_string(),
        )
    });
    let mut users = Vec::new();
    for l in lines {
        let v: Value = serde_json::from_str(l).unwrap();
        let recs = v.as_array().cloned().unwrap_or_else(|| vec![v]);
        for r in recs {
            if r["kind"] == "entry" && r["payload"]["role"] == "user" {
                users.push(r["payload"]["text"].as_str().unwrap_or("").to_string());
            }
        }
    }
    (users, origin)
}

/// #463: `/fork` cuts the open session at its last settled entry into a new
/// session and switches to it (the next message lands there, with the
/// history, and not in the original); `/tree` lists the family with the open
/// session marked and picking the parent switches back to it.
#[test]
fn tui_fork_continues_in_the_new_session_and_tree_switches_back() {
    let bodies = Bodies::default();
    let (url, dir) = (spawn_mock_recording(bodies.clone()), workdir());
    let sessions = dir.join(".tole/sessions");
    let tole = |args: &[&str]| {
        let o = Command::new(env!("CARGO_BIN_EXE_tole"))
            .args(args)
            .current_dir(&dir)
            .env("CODECORA_HOME", dir.join("home"))
            .env("TOLE_NO_UPDATE_CHECK", "1")
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    let mut t = Pty::spawn(&dir, &url);
    t.expect("effort:");
    t.send("first\r");
    t.expect("STREAM-ANSWER");
    t.idle();

    // a bogus entry is refused with the core's error
    t.send("/fork no-such-entry\r");
    t.expect("(fork: forking");
    t.idle();

    t.send("/fork\r");
    t.expect("forked → ");
    t.idle();
    let mut all: Vec<String> = std::fs::read_dir(&sessions)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter_map(|n| n.strip_suffix(".jsonl").map(str::to_string))
        .collect();
    all.sort();
    assert_eq!(all.len(), 2, "the original and one fork: {all:?}");
    let file = |id: &str| sessions.join(format!("{id}.jsonl"));
    let (orig, fork) = if session_facts(&file(&all[0])).1.is_some() {
        (all[1].clone(), all[0].clone())
    } else {
        (all[0].clone(), all[1].clone())
    };
    let (src, _entry) = session_facts(&file(&fork)).1.expect("fork header");
    assert_eq!(src, orig, "forkedFrom names the original");

    // the next message lands in the fork, with the carried history
    t.send("second\r");
    t.expect("STREAM-ANSWER");
    t.idle();
    {
        let b = bodies.lock().unwrap();
        assert_eq!(
            user_contents(b.last().unwrap()),
            [json!("first"), json!("second")],
            "the fork carries the history"
        );
    }
    assert_eq!(
        session_facts(&file(&orig)).0,
        ["first"],
        "original untouched"
    );
    assert_eq!(session_facts(&file(&fork)).0, ["first", "second"]);

    // /tree: both sessions, the open one marked and preselected
    t.send("/tree\r");
    t.expect("tree>");
    t.expect(&orig);
    t.expect("(current)");
    // Up to the parent, Enter switches there
    t.send("\x1b[A");
    t.send("\r");
    t.expect(&format!("resumed {orig}"));
    t.idle();
    t.send("third\r");
    t.expect("STREAM-ANSWER");
    t.idle();
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));
    assert_eq!(session_facts(&file(&orig)).0, ["first", "third"]);
    assert_eq!(
        session_facts(&file(&fork)).0,
        ["first", "second"],
        "nothing sent after switching back reached the fork"
    );

    // the fork is listed like any session (face "branch") and nests in the tree
    let listed = tole(&["sessions", "--all"]);
    assert!(listed.contains(&fork), "{listed}");
    let tree = tole(&["sessions", "--tree"]);
    assert!(tree.contains(&format!("└─ {fork}")), "{tree}");
}

/// #463 `/copy`: the last answer reaches the terminal clipboard as ONE
/// OSC 52 sequence whose payload is base64 only; hostile answer text never
/// appears raw, and the note says what was attempted, not that it worked.
#[test]
fn tui_copy_sends_the_last_answer_as_osc52_base64() {
    let (url, dir) = (spawn_mock(), workdir());
    let mut t = Pty::spawn(&dir, &url);
    t.expect("effort:");
    t.send("/copy\r");
    t.expect("nothing to copy");
    t.send("hello\r");
    t.expect("STREAM-ANSWER");
    t.idle();
    t.send("/copy\r");
    t.expect("\x1b]52;c;U1RSRUFNLUFOU1dFUg==\x07");
    t.expect("characters to the terminal clipboard");
    // An answer with ESC / CR bytes: only its base64 is sent.
    t.send("evil\r");
    t.expect("EV\\u{1b}[2J\\u{d}IL");
    t.idle();
    t.send("/copy\r");
    t.expect("\x1b]52;c;RVYbWzJKDUlM\x07");
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));
    let o = t.out.lock().unwrap();
    let seen = String::from_utf8_lossy(&o);
    assert!(
        !seen.contains("[2J\rIL"),
        "raw answer bytes reached the pty"
    );
    assert!(!seen.contains("copied"), "never claims success");
}

/// #463 `/export`: the transcript is written under the workspace, escaped,
/// and a second run to the same path is refused instead of overwriting.
#[test]
fn tui_export_writes_the_transcript_and_refuses_to_overwrite() {
    let (url, dir) = (spawn_mock(), workdir());
    let mut t = Pty::spawn(&dir, &url);
    t.expect("effort:");
    t.send("hello\r");
    t.expect("STREAM-ANSWER");
    t.idle();
    t.send("/export\r");
    t.expect("transcript written to");
    t.idle();
    let exports = dir.join(".tole/exports");
    let html = std::fs::read_dir(&exports)
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().ends_with(".html"))
        .expect("exported page")
        .path();
    let page = std::fs::read_to_string(&html).unwrap();
    assert!(page.contains("STREAM-ANSWER") && page.contains("Content-Security-Policy"));
    assert!(!page.contains("<script"));
    t.send("/export\r");
    t.expect("pick another path");
    t.idle();
    t.send("/export ../escape.html\r");
    t.expect("outside the workspace jail");
    t.idle();
    t.send("/export t.jsonl\r");
    t.expect("transcript written to");
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));
    assert_eq!(
        std::fs::read_to_string(&html).unwrap(),
        page,
        "not overwritten"
    );
    assert!(!dir.parent().unwrap().join("escape.html").exists());
    let jsonl = std::fs::read_to_string(dir.join("t.jsonl")).unwrap();
    assert!(jsonl
        .lines()
        .all(|l| serde_json::from_str::<Value>(l).is_ok()));
    assert!(jsonl.contains("STREAM-ANSWER"));
}

/// Every SGR parameter in `seen` that selects a foreground or background
/// color (`30-38`, `40-48`, `90-97`, `100-107`); `39`/`49` only reset.
fn sgr_colors(seen: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = seen;
    while let Some(i) = rest.find("\x1b[") {
        rest = &rest[i + 2..];
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == ';' || c == ':'))
            .unwrap_or(rest.len());
        if rest[end..].starts_with('m') {
            for p in rest[..end].split([';', ':']) {
                match p.parse::<u16>() {
                    Ok(38 | 48) => {
                        found.push(rest[..end].to_string());
                        break; // extended color: the rest are its operands
                    }
                    Ok(30..=37 | 40..=47 | 90..=97 | 100..=107) => found.push(p.to_string()),
                    _ => {}
                }
            }
        }
    }
    found
}

/// One approval prompt, denied, then quit; returns everything printed.
fn approval_screen(envs: &[(&str, &str)]) -> String {
    let (url, dir) = (spawn_mock(), workdir());
    let mut t = Pty::spawn_full(&dir, &url, &[], envs);
    t.expect("effort:");
    t.send("write it\r");
    t.expect("write file out.txt");
    t.send("n");
    t.expect("approval denied");
    t.idle();
    t.send("/");
    t.expect("/thinking");
    t.send("\x7f/quit\r");
    assert_eq!(t.wait_exit(), Some(0));
    let o = t.out.lock().unwrap();
    String::from_utf8_lossy(&o).into_owned()
}

/// #463 themes: `TOLE_THEME=mono` (and `NO_COLOR`) leave no color sequence on
/// the screen, the default theme still colors it, and an unknown theme falls
/// back to the default with one note.
#[test]
fn tui_mono_theme_emits_no_color_sequences_and_default_still_does() {
    let default = approval_screen(&[]);
    assert!(
        !sgr_colors(&default).is_empty(),
        "the default theme must color the screen: {default:?}"
    );
    for envs in [
        &[("TOLE_THEME", "mono")][..],
        &[("NO_COLOR", "1")][..],
        &[("NO_COLOR", "1"), ("TOLE_THEME", "MONO")][..],
    ] {
        let mono = approval_screen(envs);
        assert_eq!(sgr_colors(&mono), Vec::<String>::new(), "{envs:?}");
        assert!(!mono.contains("\x1b[38;"), "{envs:?}");
        // Still readable: the approval prompt is on screen, and bold.
        assert!(mono.contains("write file out.txt"), "{mono:?}");
        assert!(
            mono.contains("\x1b[1m"),
            "bold carries the prompt: {mono:?}"
        );
    }
    // An explicit theme is selected over NO_COLOR, but crossterm (the
    // terminal backend) itself drops color escapes while NO_COLOR is set, so
    // the screen stays color-free and the prompt stays bold.
    let both = approval_screen(&[("NO_COLOR", "1"), ("TOLE_THEME", "light")]);
    assert_eq!(sgr_colors(&both), Vec::<String>::new(), "{both:?}");
    assert!(both.contains("\x1b[1m"), "{both:?}");
    // Without NO_COLOR the light theme colors the screen.
    let light = approval_screen(&[("TOLE_THEME", "light")]);
    assert!(!sgr_colors(&light).is_empty(), "{light:?}");
}

#[test]
fn tui_unknown_theme_shows_one_note_and_uses_the_default() {
    let (url, dir) = (spawn_mock(), workdir());
    let mut t = Pty::spawn_full(&dir, &url, &[], &[("TOLE_THEME", "neon")]);
    t.expect("TOLE_THEME = \"neon\" is not a theme");
    t.expect("effort:");
    t.send("/quit\r");
    assert_eq!(t.wait_exit(), Some(0));
    let o = t.out.lock().unwrap();
    let seen = String::from_utf8_lossy(&o).into_owned();
    assert_eq!(seen.matches("is not a theme").count(), 1, "{seen:?}");
    assert!(!sgr_colors(&seen).is_empty(), "default theme colors");
}
