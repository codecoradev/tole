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
use tole_core::uteke::{UtekeDocumentTool, UtekeRecallTool};

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
    let store = codecora_home.join("uteke");
    let log_of = |store: &std::path::Path| -> Vec<String> {
        std::fs::read_to_string(store.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    };
    let exercise = || {
        let mut errs = Vec::new();
        errs.extend(recall(&cfg, "anything").err());
        errs.extend(
            remember_session(&cfg, "s1", "prompt", "answer", false, &SecretSet::default()).err(),
        );
        let idx = UtekeDocIndex::new(&bin);
        errs.extend(idx.search("q", &SkillFilter::default(), 3).err());
        errs.extend(
            idx.upsert(&SkillEntry {
                name: "demo".into(),
                description: "d".into(),
                profiles: vec![],
                status: SkillStatus::Approved,
                content_hash: "sha256:00".into(),
            })
            .err(),
        );
        // The bare-`uteke` (PATH) spawns.
        errs.extend(
            UtekeRecallTool::new()
                .execute(json!({"query": "anything", "room": "repo-proj"}))
                .err(),
        );
        errs.extend(
            UtekeDocumentTool::new(None)
                .execute(json!({"slug": "demo-doc", "markdown": "# d", "room": "repo-proj"}))
                .err(),
        );
        errs
    };

    let errs = exercise();
    assert!(errs.is_empty(), "{errs:?}");
    let log = log_of(&store);
    let want = [
        "room recall --limit 3 --json -- repo-proj anything",
        "recall --namespace repo-proj --limit 3 --json -- anything",
        "remember [tole session s1] prompt \u{2192} answer --room repo-proj --type context \
         --tags tole,session --source tole-harness --source-type system",
        "doc search q --mode semantic --limit 9 --json",
        "doc create skill-demo --file - --tags skill",
        "doc update skill-demo --metadata {\"content_hash\":\"sha256:00\",\"description\":\"d\",\
         \"kind\":\"tole-skill\",\"name\":\"demo\",\"profiles\":[],\"status\":\"approved\"}",
        "room recall repo-proj anything --limit 5 --json",
        "doc create demo-doc --file -",
        "room add-document repo-proj demo-doc",
    ];
    assert_eq!(log, want, "every spawn, with its exact argv, in the store");
    assert_eq!(
        files_under(&decoy_home),
        Vec::<String>::new(),
        "nothing may appear under the real HOME"
    );

    // A relative CODECORA_HOME fails closed: no spawn, an error each, and no
    // file under HOME or the working directory.
    std::fs::remove_file(store.join("calls.log")).unwrap();
    std::env::set_var("CODECORA_HOME", "rel-home");
    let errs = exercise();
    assert_eq!(errs.len(), 6, "{errs:?}");
    assert!(
        errs.iter().all(|e| e.contains("not an absolute path")),
        "{errs:?}"
    );
    assert!(log_of(&store).is_empty());
    assert!(files_under(&decoy_home).is_empty());
    assert!(!std::path::Path::new("rel-home").exists());

    // An empty CODECORA_HOME is unset (tole's own rule): uteke keeps its own
    // default under HOME, like tole's data does.
    std::env::set_var("CODECORA_HOME", "");
    let _ = recall(&cfg, "anything");
    assert!(decoy_home.join(".codecora/uteke/calls.log").exists());

    // Unset: the same.
    std::env::remove_var("CODECORA_HOME");
    std::fs::remove_file(decoy_home.join(".codecora/uteke/calls.log")).unwrap();
    let _ = recall(&cfg, "anything");
    assert_eq!(log_of(&decoy_home.join(".codecora/uteke")).len(), 2);

    // A UTEKE_HOME the user chose wins over CODECORA_HOME.
    let chosen = root.join("chosen");
    std::env::set_var("CODECORA_HOME", &codecora_home);
    std::env::set_var("UTEKE_HOME", &chosen);
    let _ = recall(&cfg, "anything");
    assert!(chosen.join("calls.log").exists());
}
