//! #1025: every child mission ends with the missing-input rule, with or
//! without a role, and an unroled child is otherwise left as it was.
//!
//! The mission is checked where the child sees it: the last argument of the
//! spawned process. A stand-in script dumps its argv and the handoff
//! environment, so no real agent turn runs.
#![cfg(all(feature = "shell-tools", unix))]

#[path = "support/test_dir.rs"]
mod test_dir;
use test_dir::{wait_for, write_executable, TestDir, WAIT};

use serde_json::{json, Value};
use tole_core::agents::AgentStartTool;
use tole_core::tool::Tool;

/// The wording is pinned here on purpose: changing it is a decision, not a
/// side effect.
const RULE: &str = "If something you need is missing (a file, a credential, access, a decision), say exactly what and stop. Do not mock, substitute or guess.";

/// Start a child through a script that dumps its argv (one line per line of
/// every argument) and the handoff environment; return both.
fn start_and_dump(tag: &str, input: Value) -> (Vec<String>, Vec<String>) {
    let d = TestDir::new(&format!("tole-missing-input-{tag}"));
    let bin = d.join("dump-all");
    let (argv, env) = (d.join("argv.txt"), d.join("env.txt"));
    // Each dump is renamed into place, so a reader never sees half a file.
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{a}.tmp\" && mv \"{a}.tmp\" \"{a}\"\n\
         printf '%s\\n%s\\n' \"$TOLE_AGENT_ROLE\" \"$TOLE_AGENT_HANDOFF\" > \"{e}.tmp\" && mv \"{e}.tmp\" \"{e}\"\nexit 0\n",
        a = argv.display(),
        e = env.display()
    );
    write_executable(&bin, script);
    AgentStartTool::new(&bin, d.to_path_buf())
        .with_parent_allows(vec!["write_*".into()])
        .execute(input)
        .unwrap();
    wait_for(WAIT, "the child's argv and env dumps", || {
        let (a, e) = (
            std::fs::read_to_string(&argv).ok()?,
            std::fs::read_to_string(&env).ok()?,
        );
        let lines = |s: String| s.lines().map(str::to_string).collect::<Vec<_>>();
        Some((lines(a), lines(e)))
    })
}

fn rule_count(argv: &[String]) -> usize {
    argv.iter().filter(|l| l.as_str() == RULE).count()
}

#[test]
fn every_role_ends_its_mission_with_the_rule() {
    for role in ["explore", "implement", "verify", "fix"] {
        let (argv, _) = start_and_dump(role, json!({"prompt": "do the thing", "role": role}));
        assert_eq!(rule_count(&argv), 1, "{role}: {argv:?}");
        assert_eq!(argv.last().map(String::as_str), Some(RULE), "{role}");
        assert!(
            argv.join("\n").contains(&format!("You are an `{role}`"))
                || argv.join("\n").contains(&format!("You are a `{role}`")),
            "{role} brief is still there: {argv:?}"
        );
    }
}

#[test]
fn a_child_without_a_role_gets_the_rule_and_nothing_else_new() {
    let (argv, env) = start_and_dump("norole", json!({"prompt": "just do it"}));
    let at = argv.iter().position(|l| l == "just do it").expect("task");
    assert_eq!(
        &argv[at..],
        ["just do it".to_string(), String::new(), RULE.to_string()],
        "task, a blank line, the rule: {argv:?}"
    );
    assert!(!argv.iter().any(|a| a == "handoff_submit"));
    assert_eq!(env, ["", ""], "still no handoff env: {env:?}");
}

#[test]
fn the_rule_comes_after_the_artifacts_to_read() {
    let (argv, _) = start_and_dump(
        "consumes",
        json!({
            "prompt": "review it",
            "role": "verify",
            "consumes": [{"path": "notes/plan.md", "rev": "abc1234"}]
        }),
    );
    let all = argv.join("\n");
    let (inputs, rule) = (all.find("notes/plan.md").unwrap(), all.find(RULE).unwrap());
    assert!(inputs < rule, "{all}");
    assert_eq!(rule_count(&argv), 1, "{argv:?}");
}
