//! #992: with `CODECORA_HOME` set, every uteke tole spawns uses
//! `<CODECORA_HOME>/uteke`, never the store under the real `HOME`.
//! One test in its own process: it edits the process environment.
#![cfg(all(feature = "shell-tools", unix))]

#[path = "support/test_dir.rs"]
mod test_dir;
use test_dir::{write_executable, TestDir};

use serde_json::json;
use tole_core::memory::{recall, remember_session, MemoryConfig};
use tole_core::secrets::SecretSet;
use tole_core::skill_index::{SkillEntry, SkillFilter, SkillIndex, SkillStatus, UtekeDocIndex};
use tole_core::tool::Tool;
use tole_core::uteke::UtekeRecallTool;

/// Every file under `dir`, recursively (relative paths as text).
fn files_under(dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p.display().to_string());
            }
        }
    }
    out
}

#[test]
fn spawned_uteke_uses_the_codecora_home_store_not_the_real_home() {
    let root = TestDir::new("tole-uteke-992");
    let decoy_home = root.join("decoy-home");
    let codecora_home = root.join("cc-home");
    let bin_dir = root.join("bin");
    for d in [&decoy_home, &codecora_home, &bin_dir] {
        std::fs::create_dir_all(d).unwrap();
    }
    // A stand-in with uteke's real store resolution: UTEKE_HOME, else
    // $HOME/.codecora/uteke. Each call leaves a file in the store it picked.
    let script = "#!/bin/sh\n\
        store=\"${UTEKE_HOME:-$HOME/.codecora/uteke}\"\n\
        mkdir -p \"$store\"\n\
        echo \"$*\" >> \"$store/calls.log\"\n\
        cat > /dev/null\n\
        echo '[]'\n";
    write_executable(bin_dir.join("uteke"), script);

    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    std::env::set_var("PATH", path);
    std::env::set_var("HOME", &decoy_home);
    std::env::remove_var("USERPROFILE");
    std::env::remove_var("UTEKE_HOME");
    std::env::set_var("CODECORA_HOME", &codecora_home);

    let bin = bin_dir.join("uteke").display().to_string();
    let cfg = MemoryConfig::for_cwd(&bin, &root.join("proj"));
    let _ = recall(&cfg, "anything");
    let _ = remember_session(&cfg, "s1", "prompt", "answer", false, &SecretSet::default());
    let idx = UtekeDocIndex::new(&bin);
    let _ = idx.search("q", &SkillFilter::default(), 3);
    let _ = idx.upsert(&SkillEntry {
        name: "demo".into(),
        description: "d".into(),
        profiles: vec![],
        status: SkillStatus::Approved,
        content_hash: "sha256:00".into(),
    });
    // The bare-`uteke` (PATH) spawn of the recall tool.
    let _ = UtekeRecallTool::new().execute(json!({"query": "anything"}));

    let store = codecora_home.join("uteke");
    let log = std::fs::read_to_string(store.join("calls.log")).unwrap_or_default();
    assert!(
        log.lines().count() >= 4,
        "expected every spawn in <CODECORA_HOME>/uteke, saw: {log:?}"
    );
    assert_eq!(
        files_under(&decoy_home),
        Vec::<String>::new(),
        "nothing may appear under the real HOME"
    );

    // Without CODECORA_HOME uteke keeps its own default under HOME.
    std::env::remove_var("CODECORA_HOME");
    let _ = recall(&cfg, "anything");
    assert!(decoy_home.join(".codecora/uteke/calls.log").exists());

    // A UTEKE_HOME the user chose wins over CODECORA_HOME.
    let chosen = root.join("chosen");
    std::env::set_var("CODECORA_HOME", &codecora_home);
    std::env::set_var("UTEKE_HOME", &chosen);
    let _ = recall(&cfg, "anything");
    assert!(chosen.join("calls.log").exists());
}
