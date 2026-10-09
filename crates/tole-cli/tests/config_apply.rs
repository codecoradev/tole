//! Startup wiring of the project config (#208 part 3a) end-to-end with the
//! REAL binary: the trust gate, `--config` / `--no-config`, the low-risk
//! keys' precedence and the "no config => nothing changes" invariant.
//! `CODECORA_HOME` (and every provider variable) is set on the CHILD only.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmpdir() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("tole-apply-e2e-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

const SCRUBBED: [&str; 11] = [
    "TOLE_MODEL",
    "TOLE_BASE_URL",
    "TOLE_API_KEY",
    "TOLE_MEMORY",
    "TOLE_SYSTEM_PROMPT",
    "TOLE_TRUST",
    "OPENAI_MODEL",
    "OPENAI_BASE_URL",
    "OPENAI_API_KEY",
    "TOLE_SERVE_TOKEN",
    "TOLE_MEMORY_NAMESPACE",
];

fn cmd(cwd: &Path, home: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_tole"));
    c.args(args)
        .current_dir(cwd)
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("CODECORA_HOME", home)
        .stdin(Stdio::null());
    for k in SCRUBBED {
        c.env_remove(k);
    }
    c
}

fn tole(cwd: &Path, home: &Path, args: &[&str]) -> Output {
    cmd(cwd, home, args).output().unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).to_string()
}

/// A project dir with `.tole/config.toml` = `content`, and a fresh home.
fn project(content: &str) -> (PathBuf, PathBuf) {
    let d = tmpdir();
    std::fs::create_dir_all(d.join(".tole")).unwrap();
    std::fs::write(d.join(".tole/config.toml"), content).unwrap();
    (d, tmpdir())
}

fn trust(d: &Path, home: &Path) {
    let o = tole(d, home, &["config", "trust", "--yes"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
}

// ------------------------------------------------------------ (a) no config

#[test]
fn a_no_config_changes_nothing() {
    let d = tmpdir();
    // No HOME / CODECORA_HOME at all: the trust store must not even be
    // looked up when there is no config.
    let mut c = Command::new(env!("CARGO_BIN_EXE_tole"));
    c.args(["sessions"])
        .current_dir(&d)
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env_remove("HOME")
        .env_remove("USERPROFILE")
        .env_remove("CODECORA_HOME")
        .stdin(Stdio::null());
    let plain = c.output().unwrap();
    assert!(plain.status.success(), "{}", text(&plain.stderr));
    assert_eq!(text(&plain.stderr), "", "nothing extra on stderr");
    assert!(
        text(&plain.stdout).contains("no sessions in .tole/sessions"),
        "{}",
        text(&plain.stdout)
    );
    // Byte-identical to --no-config.
    let home = tmpdir();
    let nc = tole(&d, &home, &["--no-config", "sessions"]);
    assert_eq!(plain.stdout, nc.stdout);
    assert_eq!(nc.stderr, b"");
    // `status` of an unknown id keeps today's failure and message.
    let st = tole(&d, &home, &["status", "nope"]);
    assert!(!st.status.success());
    assert_eq!(
        text(&st.stderr),
        "tole: session nope not found at .tole/sessions/nope.jsonl\n"
    );
}

#[test]
fn a_empty_tole_dir_without_config_toml_is_still_no_config() {
    let d = tmpdir();
    std::fs::create_dir_all(d.join(".tole/sessions")).unwrap();
    let o = tole(&d, &tmpdir(), &["sessions"]);
    assert!(o.status.success());
    assert_eq!(text(&o.stderr), "");
}

// ---------------------------------------------- (b) untrusted + non-tty

#[test]
fn b_untrusted_config_fails_closed_and_the_command_does_not_run() {
    let (d, home) = project("sessions_dir = \"cfg-sessions\"\n");
    let o = tole(&d, &home, &["sessions"]);
    assert!(!o.status.success());
    let err = text(&o.stderr);
    assert!(err.contains("is not trusted"), "{err}");
    assert!(err.contains("Run: tole config trust"), "{err}");
    assert!(!err.contains("using config"), "{err}");
    assert_eq!(text(&o.stdout), "", "the command must not have run");
    // A run is gated too (and never reaches the provider check).
    let o = tole(&d, &home, &["run", "hi"]);
    assert!(!o.status.success());
    assert!(text(&o.stderr).contains("Run: tole config trust"));
    assert!(!text(&o.stderr).contains("missing provider config"));
}

#[test]
fn b_prompt_from_stdin_never_prompts_even_when_a_tty_would_be_there() {
    // stdin is /dev/null here; the point is the exit is immediate and the
    // instruction exact (the --prompt-file - policy is unit-tested).
    let (d, home) = project("model = \"m\"\n");
    let o = tole(&d, &home, &["run", "--prompt-file", "-"]);
    assert!(!o.status.success());
    assert!(text(&o.stderr).contains("Run: tole config trust"));
}

// ------------------------------------------------ (c) trusted => applied

#[test]
fn c_trusted_config_is_used_and_sessions_dir_is_honored_everywhere() {
    let (d, home) = project("sessions_dir = \"cfg-sessions\"\n");
    trust(&d, &home);
    let o = tole(&d, &home, &["sessions"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    let err = text(&o.stderr);
    assert_eq!(err.lines().count(), 1, "{err}");
    assert!(
        err.starts_with("tole: using config ") && err.contains(".tole/config.toml"),
        "{err}"
    );
    assert!(text(&o.stdout).contains("no sessions in cfg-sessions"));
    // `status` looks in the same directory as `sessions`.
    let o = tole(&d, &home, &["status", "nope"]);
    assert!(text(&o.stderr).contains("not found at cfg-sessions/nope.jsonl"));
}

#[test]
fn c_config_dir_and_flag_dir_agree_between_sessions_and_status() {
    let (d, home) = project("sessions_dir = \"cfg-sessions\"\n");
    trust(&d, &home);
    let o = tole(&d, &home, &["--sessions-dir", "flag-s", "sessions"]);
    assert!(
        text(&o.stdout).contains("no sessions in flag-s"),
        "{}",
        text(&o.stdout)
    );
    let o = tole(&d, &home, &["status", "x", "--sessions-dir", "flag-s"]);
    assert!(text(&o.stderr).contains("flag-s/x.jsonl"));
}

// ----------------------------------------------- (d) --no-config

#[test]
fn d_no_config_ignores_an_untrusted_or_invalid_file_silently() {
    let (d, home) = project("this is = not valid toml [[[\n");
    let o = tole(&d, &home, &["--no-config", "sessions"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert_eq!(text(&o.stderr), "");
    assert!(text(&o.stdout).contains("no sessions in .tole/sessions"));
    // Without the flag the invalid file would have been a hard error
    // (after trust; untrusted fails closed first).
    assert!(!tole(&d, &home, &["sessions"]).status.success());
    // Conflicts with --config.
    let o = tole(
        &d,
        &home,
        &["--no-config", "--config", "x.toml", "sessions"],
    );
    assert!(!o.status.success());
    assert!(text(&o.stderr).contains("cannot be used with"));
}

// ------------------------------------------------ (e) explicit --config

#[test]
fn e_explicit_config_runs_without_a_trust_record_or_store() {
    let d = tmpdir();
    let f = d.join("custom.toml");
    std::fs::write(&f, "sessions_dir = \"explicit-s\"\n").unwrap();
    // A home that does not exist and no HOME fallback: the store is not
    // consulted for an explicit path.
    let mut c = cmd(
        &d,
        Path::new("/nonexistent-tole-home"),
        &["--config", "custom.toml", "sessions"],
    );
    c.env_remove("HOME");
    let o = c.output().unwrap();
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(text(&o.stdout).contains("no sessions in explicit-s"));
    assert!(text(&o.stderr).starts_with("tole: using config "));
    // The flag may follow the subcommand's position for global flags too.
    let o = tole(&d, &tmpdir(), &["sessions", "--config", "custom.toml"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(text(&o.stdout).contains("explicit-s"));
    // A missing explicit file is an error, not a silent skip.
    let o = tole(&d, &tmpdir(), &["--config", "missing.toml", "sessions"]);
    assert!(!o.status.success());
}

#[test]
fn e_config_subcommands_accept_the_global_flag_before_and_after() {
    let d = tmpdir();
    let f = d.join("x.toml");
    std::fs::write(&f, "model = \"m\"\n").unwrap();
    let home = tmpdir();
    for args in [
        vec!["config", "check", "--config", "x.toml"],
        vec!["--config", "x.toml", "config", "check"],
    ] {
        let o = tole(&d, &home, &args);
        assert!(o.status.success(), "{args:?}: {}", text(&o.stderr));
        assert!(text(&o.stdout).contains("model = \"m\"  [config]"));
    }
}

// ----------------------------------------------------- (f) modified file

#[test]
fn f_modifying_the_trusted_file_fails_closed_again() {
    let (d, home) = project("sessions_dir = \"a\"\n");
    trust(&d, &home);
    assert!(tole(&d, &home, &["sessions"]).status.success());
    std::fs::write(d.join(".tole/config.toml"), "sessions_dir = \"evil\"\n").unwrap();
    let o = tole(&d, &home, &["sessions"]);
    assert!(!o.status.success());
    assert!(text(&o.stderr).contains("Run: tole config trust"));
}

// ------------------------------------------- (g) server faces never hang

fn assert_fails_fast_untrusted(args: &[&str]) {
    let (d, home) = project("model = \"m\"\n");
    // Piped (never closed by us) stdin: a prompt would block forever.
    let mut child = cmd(&d, &home, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if start.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            panic!("{args:?} hung instead of failing closed");
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    assert!(!status.success(), "{args:?}");
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    assert!(err.contains("Run: tole config trust"), "{args:?}: {err}");
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    assert_eq!(out, "", "protocol stdout must stay clean: {args:?}");
}

#[cfg(feature = "shell-tools")]
#[test]
fn g_serve_and_acp_fail_closed_without_prompting() {
    assert_fails_fast_untrusted(&["serve", "--port", "0", "--token", "t"]);
    assert_fails_fast_untrusted(&["acp"]);
}

#[cfg(all(feature = "shell-tools", feature = "mcp"))]
#[test]
fn g_mcp_fails_closed_without_prompting() {
    assert_fails_fast_untrusted(&["mcp"]);
}

// --------------------------------------------- (h) flag/env beat config

#[test]
fn h_workspace_flag_beats_config_and_config_is_applied() {
    let (d, home) = project("workspace = \"/nonexistent-cfg-ws\"\n");
    trust(&d, &home);
    // The mission fails on the workspace before touching the provider.
    let o = tole(&d, &home, &["mission", "goal"]);
    assert!(!o.status.success());
    assert!(
        text(&o.stderr).contains("workspace directory /nonexistent-cfg-ws"),
        "{}",
        text(&o.stderr)
    );
    let o = tole(
        &d,
        &home,
        &["--workspace", "/nonexistent-flag-ws", "mission", "goal"],
    );
    assert!(
        text(&o.stderr).contains("workspace directory /nonexistent-flag-ws"),
        "{}",
        text(&o.stderr)
    );
}

#[cfg(feature = "shell-tools")]
#[test]
fn h_memory_config_is_applied_and_flag_and_env_beat_it() {
    let (d, home) = project("memory = \"bogus-backend\"\n");
    trust(&d, &home);
    // Applied: the bogus backend is rejected at startup.
    let o = tole(&d, &home, &["sessions"]);
    assert!(!o.status.success());
    assert!(text(&o.stderr).contains("unsupported memory backend \"bogus-backend\""));
    // Flag beats config.
    let o = tole(&d, &home, &["--memory", "uteke", "sessions"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    // Env beats config.
    let o = cmd(&d, &home, &["sessions"])
        .env("TOLE_MEMORY", "uteke")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", text(&o.stderr));
    // Empty env counts as unset: the config applies again.
    let o = cmd(&d, &home, &["sessions"])
        .env("TOLE_MEMORY", "")
        .output()
        .unwrap();
    assert!(!o.status.success());
}

#[test]
fn h_security_sensitive_keys_are_not_applied_yet() {
    // If plan_mode / on_pretool / on_turnend were applied, `mission` would
    // refuse with the matching message; instead it reaches the provider
    // check (no provider env in the child).
    let (d, home) = project(
        "plan_mode = true\non_pretool = [\"x\"]\non_posttool = [\"y\"]\non_turnend = [\"z\"]\n\
         no_skills = true\ntrust = [\"internal\"]\nallow = [\"write_*\"]\nmcp_server = [\"a=b\"]\n",
    );
    trust(&d, &home);
    let o = tole(&d, &home, &["mission", "goal"]);
    let err = text(&o.stderr);
    assert!(!err.contains("--plan-mode"), "{err}");
    assert!(!err.contains("--on-pretool"), "{err}");
    assert!(!err.contains("--on-turnend"), "{err}");
    assert!(err.contains("missing provider config"), "{err}");
}

// ------------------------------------------------- model / base_url

/// A mock that records the `model` of the first request and answers 500.
fn spawn_model_probe() -> (String, std::sync::mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut s = stream;
            let mut buf = Vec::new();
            let mut chunk = [0u8; 16384];
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                let n = s.read(&mut chunk).unwrap_or(0);
                buf.extend_from_slice(&chunk[..n]);
                if let Some(h) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf[h + 4..]) {
                        let _ = tx.send(v["model"].as_str().unwrap_or("").to_string());
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let _ = s.write_all(
                b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    });
    (format!("http://{addr}"), rx)
}

#[test]
fn model_and_base_url_from_config_reach_the_provider_and_env_wins() {
    let (url, rx) = spawn_model_probe();
    let (d, home) = project(&format!("model = \"cfg-model\"\nbase_url = \"{url}\"\n"));
    trust(&d, &home);
    // Config supplies base_url + model; the API key is env-only.
    let o = cmd(&d, &home, &["run", "hi"])
        .env("TOLE_API_KEY", "k")
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        "cfg-model"
    );
    // Env TOLE_MODEL beats the config model.
    let _ = cmd(&d, &home, &["run", "hi"])
        .env("TOLE_API_KEY", "k")
        .env("TOLE_MODEL", "env-model")
        .output()
        .unwrap();
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        "env-model"
    );
    // No key anywhere: the config never supplies one.
    let o = tole(&d, &home, &["run", "hi"]);
    assert!(text(&o.stderr).contains("missing provider config"));
}

// ------------------------------------------------------ config check

#[test]
fn config_check_shows_effective_values_and_sources() {
    let (d, home) = project(
        "model = \"cfg-model\"\nsessions_dir = \"cfg-s\"\nplan_mode = true\n[mission]\nmax_steps = 7\n",
    );
    let o = cmd(&d, &home, &["--sessions-dir", "flag-s", "config", "check"])
        .env("TOLE_MODEL", "env-model")
        .output()
        .unwrap();
    let out = text(&o.stdout);
    assert!(o.status.success(), "{out} {}", text(&o.stderr));
    assert!(
        out.contains("model = \"cfg-model\"  [config]  effective: \"env-model\"  (env TOLE_MODEL)"),
        "{out}"
    );
    assert!(
        out.contains("sessions_dir = \"cfg-s\"  [config]  effective: \"flag-s\"  (flag)"),
        "{out}"
    );
    assert!(
        out.contains("mission.max_steps = 7  [config]  effective: 7  (config)"),
        "{out}"
    );
    assert!(
        out.contains("plan_mode = true  [config]  (parsed, not applied yet — 3b)"),
        "{out}"
    );
    // `config check` itself never prints the "using config" line.
    assert_eq!(text(&o.stderr), "");
}
