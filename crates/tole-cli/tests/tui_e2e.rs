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
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || serve(stream));
        }
    });
    format!("http://{addr}/v1")
}

fn serve(mut s: std::net::TcpStream) {
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
        return reply("application/json", json!({"data": []}).to_string());
    }
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
