//! `tole audit` end to end (#585): the real binary finds this workspace's
//! delegation log in the data dir, prints the tree redacted, and writes
//! nothing.

use std::path::PathBuf;
use std::process::Command;

use tole_cli::ledger_file::FileStore;
use tole_core::delegation::{Budget, Delegation, Goal};

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-audit-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

#[test]
fn tole_audit_reads_the_workspace_log_from_the_data_dir() {
    let home = tmp("home");
    let ws = tmp("ws");
    let log = tole_cli::delegate::log_path(&home.join("tole"), &ws).unwrap();
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    let mut d = Delegation::open(FileStore::new(log.clone())).unwrap();
    let secret = "sk-cli-audit-0123456789";
    let allow = vec![format!("run_command:curl -H {secret} *")];
    d.begin(
        &Goal {
            id: "g-cli",
            task: "orchestrator",
            budget: Budget {
                tokens: 500,
                steps: 50,
                deadline: i64::MAX,
            },
            allow: &allow,
            max_depth: 2,
            max_live: 4,
        },
        0,
    )
    .unwrap();
    let before = std::fs::read(&log).unwrap();
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_tole"))
            .arg("--workspace")
            .arg(&ws)
            .args(args)
            .env("CODECORA_HOME", &home)
            .env("AUDIT_CLI_TOKEN", secret)
            .env("TOLE_NO_UPDATE_CHECK", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    let list = run(&["audit"]);
    assert!(list.starts_with("g-cli running 1 nodes"), "{list}");
    let json: serde_json::Value =
        serde_json::from_str(&run(&["audit", "g-cli", "--json"])).unwrap();
    assert_eq!(json["nodes"][0]["node"], serde_json::json!("g-cli"));
    let shown = json.to_string();
    assert!(!shown.contains(secret), "{shown}");
    assert!(shown.contains("«AUDIT_CLI_TOKEN»"), "{shown}");
    assert_eq!(std::fs::read(&log).unwrap(), before, "audit writes nothing");
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&ws);
}
