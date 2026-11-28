//! `tole serve token add|list|revoke` (#1320, part 1) on the real binary: the
//! token is printed once on stdout and lands nowhere else, the file is
//! owner-only and append-only, and the owner commands refuse under a session.
//! `tole serve` checks the tokens (`serve_device_scope`); nothing here starts one.

#![cfg(feature = "shell-tools")]

mod common;
use common::TestDir;
use std::path::Path;
use std::process::Output;

fn tole(base: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    common::hermetic_command(env!("CARGO_BIN_EXE_tole"), &base.join("home"), env)
        .args(args)
        .current_dir(base.join("proj"))
        .output()
        .unwrap()
}

fn setup(tag: &str) -> TestDir {
    let base = TestDir::new(&format!("tole-servetok-{tag}"));
    std::fs::create_dir_all(base.join("home")).unwrap();
    std::fs::create_dir_all(base.join("proj")).unwrap();
    base
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn tree_text(dir: &Path, acc: &mut String) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            tree_text(&p, acc);
        } else {
            acc.push_str(&String::from_utf8_lossy(&std::fs::read(&p).unwrap()));
        }
    }
}

#[test]
fn add_list_revoke_on_the_real_binary() {
    let base = setup("flow");
    let add = tole(
        &base,
        &["serve", "token", "add", "phone", "--scope", "read-only"],
        &[],
    );
    assert!(add.status.success(), "{}", err(&add));
    let token = out(&add).trim_end().to_string();
    assert!(
        token.starts_with("tole_dt_") && token.len() == 51,
        "stdout is the token alone"
    );
    assert_eq!(out(&add), format!("{token}\n"));
    assert!(!err(&add).contains(&token));

    let full = tole(
        &base,
        &["serve", "token", "add", "laptop", "--scope", "full"],
        &[],
    );
    let token2 = out(&full).trim_end().to_string();
    assert!(full.status.success() && token2 != token);

    let list = tole(&base, &["serve", "token", "list"], &[]);
    assert!(list.status.success());
    let text = format!("{}{}", out(&list), err(&list));
    assert!(text.contains("phone") && text.contains("read-only") && text.contains("laptop"));
    assert!(text.contains("active") && text.contains("never"));
    assert!(!text.contains(&token) && !text.contains(&token2));

    let dup = tole(
        &base,
        &["serve", "token", "add", "phone", "--scope", "full"],
        &[],
    );
    assert!(!dup.status.success());
    assert!(out(&dup).is_empty() && err(&dup).contains("\"phone\""));
    for bad in ["Phone", "a b", "-x", ""] {
        let o = tole(
            &base,
            &["serve", "token", "add", bad, "--scope", "full"],
            &[],
        );
        assert!(!o.status.success(), "{bad:?}");
        assert!(out(&o).is_empty());
    }
    // The scope is required: no default grants anything.
    assert!(!tole(&base, &["serve", "token", "add", "x"], &[])
        .status
        .success());
    assert!(!tole(
        &base,
        &["serve", "token", "add", "x", "--scope", "root"],
        &[]
    )
    .status
    .success());

    // A name is never echoed raw: no escape sequence reaches the terminal.
    let esc = tole(&base, &["serve", "token", "revoke", "\u{1b}[31mphone"], &[]);
    assert!(
        !esc.status.success() && !err(&esc).contains('\u{1b}'),
        "{:?}",
        err(&esc)
    );
    let esc = tole(
        &base,
        &[
            "serve",
            "token",
            "add",
            "\u{1b}]0;x\u{7}",
            "--scope",
            "full",
        ],
        &[],
    );
    assert!(!esc.status.success() && !err(&esc).contains('\u{1b}') && out(&esc).is_empty());

    let rev = tole(&base, &["serve", "token", "revoke", "phone"], &[]);
    assert!(rev.status.success() && out(&rev).is_empty());
    assert!(!tole(&base, &["serve", "token", "revoke", "phone"], &[])
        .status
        .success());
    let list = tole(&base, &["serve", "token", "list"], &[]);
    let rows: Vec<String> = out(&list).lines().map(str::to_string).collect();
    assert!(rows
        .iter()
        .any(|r| r.starts_with("phone") && r.contains("revoked")));
    assert!(rows
        .iter()
        .any(|r| r.starts_with("laptop") && r.contains("active")));

    // Plant and grep: the token is in no file of the home, only its hash is.
    let mut all = String::new();
    tree_text(&base.join("home"), &mut all);
    for t in [&token, &token2] {
        assert!(!all.contains(t.as_str()) && !all.contains(&t[8..]));
    }
    assert_eq!(all.matches("\"op\":\"add\"").count(), 2);
    assert!(all.contains("\"op\":\"revoke\""));
}

#[cfg(unix)]
#[test]
fn the_file_is_owner_only_and_a_revoke_appends() {
    use std::os::unix::fs::PermissionsExt;
    let base = setup("mode");
    tole(
        &base,
        &["serve", "token", "add", "phone", "--scope", "full"],
        &[],
    );
    let dir = base.join("home/tole");
    let file = dir.join("serve-tokens.jsonl");
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&file), 0o600);
    assert_eq!(mode(&dir), 0o700);
    let before = std::fs::read(&file).unwrap();
    tole(&base, &["serve", "token", "revoke", "phone"], &[]);
    let after = std::fs::read(&file).unwrap();
    assert!(after.starts_with(&before) && after.len() > before.len());

    // A file others can read is refused, not repaired.
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let list = tole(&base, &["serve", "token", "list"], &[]);
    assert!(!list.status.success() && err(&list).contains("not safe to use"));
    assert_eq!(mode(&file), 0o644);
}

#[test]
fn a_corrupt_file_is_an_error_and_is_left_alone() {
    let base = setup("corrupt");
    tole(
        &base,
        &["serve", "token", "add", "phone", "--scope", "full"],
        &[],
    );
    let file = base.join("home/tole/serve-tokens.jsonl");
    let mut text = std::fs::read_to_string(&file).unwrap();
    text.push_str("{\"op\":\"revoke\",\"v\":1,\"na");
    std::fs::write(&file, &text).unwrap();
    for args in [
        vec!["serve", "token", "list"],
        vec!["serve", "token", "revoke", "phone"],
        vec!["serve", "token", "add", "x", "--scope", "full"],
    ] {
        let o = tole(&base, &args, &[]);
        assert!(
            !o.status.success() && err(&o).contains("corrupt at line 2"),
            "{args:?}"
        );
        assert!(out(&o).is_empty());
    }
    assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
}

#[test]
fn add_and_revoke_refuse_under_an_agent_session_and_list_does_not() {
    let base = setup("session");
    tole(
        &base,
        &["serve", "token", "add", "phone", "--scope", "full"],
        &[],
    );
    let under = [("TOLE_UNDER_SESSION", "1")];
    let add = tole(
        &base,
        &["serve", "token", "add", "x", "--scope", "full"],
        &under,
    );
    assert!(!add.status.success() && err(&add).contains("refuses to run under an agent session"));
    assert!(out(&add).is_empty(), "no token reaches a session's output");
    let rev = tole(
        &base,
        &["serve", "token", "revoke", "phone"],
        &[("TOLE_AGENT_DEPTH", "1")],
    );
    assert!(!rev.status.success() && err(&rev).contains("agent session"));
    let list = tole(&base, &["serve", "token", "list"], &under);
    assert!(list.status.success() && out(&list).contains("phone"));
    // Nothing was written by the refusals.
    let file = base.join("home/tole/serve-tokens.jsonl");
    assert_eq!(std::fs::read_to_string(file).unwrap().lines().count(), 1);
}

#[test]
fn the_token_commands_ignore_an_untrusted_project_config() {
    let base = setup("gate");
    std::fs::create_dir_all(base.join("proj/.tole")).unwrap();
    std::fs::write(base.join("proj/.tole/config.toml"), "model = \"p\"\n").unwrap();
    let o = tole(&base, &["serve", "token", "list"], &[]);
    assert!(o.status.success(), "{}", err(&o));
}

#[test]
fn server_flags_and_the_token_subcommand_do_not_mix() {
    let base = setup("mix");
    let o = tole(&base, &["serve", "--port", "7999", "token", "list"], &[]);
    assert!(!o.status.success());
    assert!(!base.join("home/tole/serve-tokens.jsonl").exists());
}
