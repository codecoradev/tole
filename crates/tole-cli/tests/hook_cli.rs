//! `tole hook add|list|remove|test` (#582, part 3) on the real binary:
//! routes are written to the user config only, a key is never taken on the
//! command line nor shown, and `test` is a local dry run unless `--fire`
//! names a loopback server.

#![cfg(feature = "shell-tools")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const KEY: &str = "route-key-for-hook-cli-tests-0123";

/// A home (user config) and a project dir with its own config.
fn setup(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("tole-hookcli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("home/tole")).unwrap();
    std::fs::create_dir_all(base.join("proj/.tole")).unwrap();
    std::fs::write(
        base.join("home/tole/config.toml"),
        "# my settings\nmodel = \"m\"\n\n[cron]\n# keep this comment\nmax_concurrent = 2\n",
    )
    .unwrap();
    std::fs::write(base.join("proj/.tole/config.toml"), "model = \"p\"\n").unwrap();
    base
}

fn tole(base: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tole"))
        .args(args)
        .current_dir(base.join("proj"))
        .env("CODECORA_HOME", base.join("home"))
        .env("TOLE_CRON_DIR", base.join("cron"))
        .env("TOLE_HOOK_TEST_KEY", KEY)
        .env("MY_API_TOKEN", "payload-secret-value-42")
        .env("TZDIR", "/nonexistent")
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn user(base: &Path) -> String {
    std::fs::read_to_string(base.join("home/tole/config.toml")).unwrap()
}

fn add_gh(base: &Path) {
    let o = tole(
        base,
        &[
            "hook",
            "add",
            "gh",
            "--key-env",
            "TOLE_HOOK_TEST_KEY",
            "--job",
            "ping",
            "--event",
            "push",
            "--template",
            "commit: {head_commit.message}",
        ],
    );
    assert!(o.status.success(), "{}", text(&o));
}

#[test]
fn add_writes_the_user_config_only_and_keeps_its_comments() {
    let base = setup("add");
    add_gh(&base);
    let u = user(&base);
    assert!(
        u.contains("[hooks.gh]") && u.contains("cron_job = \"ping\""),
        "{u}"
    );
    assert!(
        u.contains("# my settings") && u.contains("# keep this comment"),
        "{u}"
    );
    assert!(!u.contains(KEY), "the key value is never written");
    let p = std::fs::read_to_string(base.join("proj/.tole/config.toml")).unwrap();
    assert_eq!(p, "model = \"p\"\n", "the project config is untouched");
    // The same route again is refused.
    assert!(!tole(&base, &["hook", "add", "gh", "--key-env", "X"])
        .status
        .success());
}

#[test]
fn add_refuses_a_missing_key_source_or_a_key_value() {
    let base = setup("refuse");
    let key_file = base.join("missing-key");
    for (args, want) in [
        (vec!["hook", "add", "a"], "key source"),
        (
            vec!["hook", "add", "a", "--key-env", "X", "--key-file", "/k"],
            "key source",
        ),
        (vec!["hook", "add", "a", "--key", KEY], "--key"),
        // Wider tools are a manual config edit, never a flag (design P8).
        (
            vec!["hook", "add", "a", "--key-env", "X", "--allow-writes"],
            "--allow-writes",
        ),
        (
            vec!["hook", "add", "a", "--key-env", "X", "--insecure-loopback"],
            "--insecure-loopback",
        ),
        (
            vec!["hook", "add", "a", "--key-env", "BAD NAME"],
            "--key-env",
        ),
        (
            vec!["hook", "add", "a", "--key-file", key_file.to_str().unwrap()],
            "key file",
        ),
        (
            vec!["hook", "add", "a", "--key-env", "X", "--template", "{"],
            "template",
        ),
        (vec!["hook", "add", "Bad/Name", "--key-env", "X"], "name"),
    ] {
        let o = tole(&base, &args);
        let t = text(&o);
        assert!(!o.status.success(), "{args:?}: {t}");
        assert!(t.contains(want), "{args:?}: {t}");
        assert!(!t.contains(KEY) || args.contains(&"--key"), "{t}");
    }
    assert!(!user(&base).contains("[hooks"), "nothing was written");
}

#[test]
fn list_shows_routes_without_keys_and_remove_is_explicit() {
    let base = setup("list");
    add_gh(&base);
    let key_file = base.join("key");
    std::fs::write(&key_file, KEY).unwrap();
    let o = tole(
        &base,
        &[
            "hook",
            "add",
            "other",
            "--key-file",
            key_file.to_str().unwrap(),
        ],
    );
    assert!(o.status.success(), "{}", text(&o));
    // allow_writes and a filter are added by hand; the filter shows as its
    // shape.
    let mut u = user(&base);
    u.push_str("allow_writes = true\n");
    u.push_str("\n[hooks.gh.filter]\npath = \"ref\"\nequals = \"refs/heads/main\"\n");
    std::fs::write(base.join("home/tole/config.toml"), u).unwrap();
    let l = text(&tole(&base, &["hook", "list"]));
    assert!(
        l.contains("gh") && l.contains("ping") && l.contains("push"),
        "{l}"
    );
    assert!(
        l.contains("ref equals") && !l.contains("refs/heads/main"),
        "{l}"
    );
    assert!(l.contains("other") && l.contains("writes: yes"), "{l}");
    for hidden in [KEY, "TOLE_HOOK_TEST_KEY", key_file.to_str().unwrap()] {
        assert!(!l.contains(hidden), "{hidden}: {l}");
    }
    // Remove takes a route name; the filter goes with it, the rest stays.
    assert!(!tole(&base, &["hook", "remove"]).status.success());
    assert!(!tole(&base, &["hook", "remove", "nope"]).status.success());
    let o = tole(&base, &["hook", "remove", "gh"]);
    assert!(o.status.success(), "{}", text(&o));
    let u = user(&base);
    assert!(
        !u.contains("hooks.gh") && u.contains("[hooks.other]"),
        "{u}"
    );
    assert!(
        u.contains("# keep this comment") && u.contains("max_concurrent = 2"),
        "{u}"
    );
}

#[test]
fn test_is_a_local_dry_run_that_redacts_and_fires_nothing() {
    let base = setup("dry");
    add_gh(&base);
    let payload = base.join("push.json");
    std::fs::write(
        &payload,
        r#"{"head_commit":{"message":"bump with payload-secret-value-42"}}"#,
    )
    .unwrap();
    let o = tole(
        &base,
        &[
            "hook",
            "test",
            "gh",
            "--event",
            "push",
            "--payload",
            payload.to_str().unwrap(),
        ],
    );
    let t = text(&o);
    assert!(o.status.success(), "{t}");
    assert!(
        t.contains("signature: ok") && t.contains("would fire `ping`"),
        "{t}"
    );
    assert!(
        t.contains("UNTRUSTED WEBHOOK DATA") && t.contains("«MY_API_TOKEN»"),
        "{t}"
    );
    assert!(
        !t.contains("payload-secret-value-42") && !t.contains(KEY),
        "{t}"
    );
    let i = text(&tole(&base, &["hook", "test", "gh", "--event", "issues"]));
    assert!(i.contains("ignored: event"), "{i}");
    assert!(
        !base.join("cron/jobs.jsonl").exists(),
        "a dry run records nothing"
    );
}

#[test]
fn fire_sends_a_signed_delivery_only_to_a_loopback_server() {
    let base = setup("fire");
    add_gh(&base);
    for url in [
        "http://10.0.0.1:8080",
        "http://example.com:80",
        "https://127.0.0.1:1",
    ] {
        let o = tole(
            &base,
            &[
                "hook", "test", "gh", "--event", "push", "--fire", "--url", url,
            ],
        );
        assert!(
            !o.status.success() && text(&o).contains("loopback"),
            "{url}: {}",
            text(&o)
        );
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut r = BufReader::new(s.try_clone().unwrap());
        let mut head = Vec::new();
        loop {
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            if line.trim().is_empty() {
                break;
            }
            head.push(line.trim().to_string());
        }
        let len: usize = head
            .iter()
            .find_map(|h| {
                h.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse().unwrap())
            })
            .unwrap_or(0);
        let mut body = vec![0; len];
        r.read_exact(&mut body).unwrap();
        s.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
            .unwrap();
        (head, body)
    });
    let url = format!("http://127.0.0.1:{port}");
    let o = tole(
        &base,
        &[
            "hook", "test", "gh", "--event", "push", "--fire", "--url", &url,
        ],
    );
    assert!(
        o.status.success() && text(&o).contains("202"),
        "{}",
        text(&o)
    );
    let (head, body) = server.join().unwrap();
    assert_eq!(head[0], "POST /hooks/gh HTTP/1.1");
    let get = |name: &str| {
        head.iter().find_map(|h| {
            h.split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.trim().to_string())
        })
    };
    let headers = tole_core::webhook::Headers {
        timestamp: get("x-tole-timestamp"),
        signature: get("x-tole-signature"),
        delivery: get("x-tole-delivery"),
        ..Default::default()
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!(tole_core::webhook::verify(KEY.as_bytes(), &headers, &body, now).is_ok());
    assert_eq!(get("x-tole-event").as_deref(), Some("push"));
}
