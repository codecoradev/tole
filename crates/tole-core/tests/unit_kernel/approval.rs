use serde_json::Value;
use tole_core::approval::*;
use tole_core::tool::Risk;

fn req(tool: &str, risk: Risk) -> ToolRequest<'_> {
    ToolRequest {
        tool,
        risk,
        input: &Value::Null,
        description: String::new(),
        target: None,
        summary: None,
    }
}

#[test]
fn allowlist_allows_listed_tools() {
    let a = AllowlistApprover::allow_only(vec!["write_file".into()]);
    assert_eq!(a.decide(&req("write_file", Risk::Write)), Verdict::Allow);
    assert_eq!(a.decide(&req("rm_rf", Risk::Write)), Verdict::Deny);
}

#[test]
fn allow_only_is_unambiguous() {
    let a = AllowlistApprover::allow_only(vec!["read_*".into()]);
    assert_eq!(a.decide(&req("read_file", Risk::ReadOnly)), Verdict::Allow);
    assert_eq!(a.decide(&req("rm_rf", Risk::Write)), Verdict::Deny);
}

#[test]
fn deny_only_denies_listed_and_allows_rest() {
    let a = AllowlistApprover::deny_only(vec!["rm_*".into()]);
    assert_eq!(a.decide(&req("rm_rf", Risk::Write)), Verdict::Deny);
    assert_eq!(a.decide(&req("read_file", Risk::ReadOnly)), Verdict::Allow);
}

/// #283: with a Deny/Ask default the verdict is final — listed
/// tools are NOT allowed (fail closed). `allow_only` is the way to
/// build an allowlist.
#[test]
fn new_with_deny_default_denies_listed_tools() {
    let a = AllowlistApprover::new(vec!["bomb".into()], Decision::Deny);
    assert_eq!(a.decide(&req("bomb", Risk::Write)), Verdict::Deny);
    assert_eq!(a.decide(&req("other", Risk::Write)), Verdict::Deny);
    let a = AllowlistApprover::new(vec!["bomb".into()], Decision::Ask);
    assert_eq!(a.decide(&req("bomb", Risk::Write)), Verdict::Deny);
}

#[test]
fn ask_collapses_to_deny_in_core() {
    let a = AllowlistApprover::new(vec![], Decision::Ask);
    assert_eq!(a.decide(&req("anything", Risk::Write)), Verdict::Deny);
}

#[test]
fn default_allow_permits_unlisted() {
    let a = AllowlistApprover::new(vec![], Decision::Allow);
    assert_eq!(a.decide(&req("anything", Risk::Write)), Verdict::Allow);
}

// --- glob patterns (E6 AC) ------------------------------------------

#[test]
fn glob_exact_and_star() {
    assert!(glob_match("cora_search", "cora_search"));
    assert!(!glob_match("cora_search", "cora_searchx"));
    assert!(glob_match("read_*", "read_file"));
    assert!(glob_match("read_*", "read_"));
    assert!(!glob_match("read_*", "write_file"));
    assert!(glob_match("*", "anything_at_all"));
}

#[test]
fn glob_question_mark() {
    assert!(glob_match("file?", "file1"));
    assert!(!glob_match("file?", "file12"));
    assert!(!glob_match("file?", "file"));
}

#[test]
fn glob_multiple_stars() {
    assert!(glob_match("*_file_*", "a_file_b"));
    assert!(glob_match("a*b*c", "abc"));
    assert!(glob_match("a*b*c", "aXbYc"));
    assert!(!glob_match("a*b*c", "aXcYb"));
    assert!(glob_match("**", "x"));
}

#[test]
fn glob_empty_pattern_and_text() {
    assert!(glob_match("", ""));
    assert!(!glob_match("", "x"));
    assert!(glob_match("*", ""));
}

#[test]
fn allowlist_patterns_match_by_glob() {
    let a = AllowlistApprover::allow_only(vec!["read_*".into()]);
    assert_eq!(a.decide(&req("read_file", Risk::Write)), Verdict::Allow);
    assert_eq!(a.decide(&req("reads", Risk::Write)), Verdict::Deny);
}

// --- Destructive cannot be allowlisted (PRD) ------------------------

#[test]
fn destructive_is_denied_even_on_pattern_match() {
    let a = AllowlistApprover::new(vec!["*".into()], Decision::Allow);
    assert_eq!(a.decide(&req("rm_rf", Risk::Destructive)), Verdict::Deny);
    // Write is still allowlistable.
    assert_eq!(a.decide(&req("write_file", Risk::Write)), Verdict::Allow);
}

#[test]
fn allowlist_is_not_interactive() {
    let a = AllowlistApprover::default();
    assert!(!a.interactive());
}

#[test]
fn workspace_guard_keeps_dot_paths_and_escapes_prompting() {
    for ok in [
        "report.md",
        "docs/out.md",
        "./a/b.txt",
        "a/./b",
        "src/main.rs",
    ] {
        assert!(workspace_guard_ok(ok), "{ok}");
    }
    for bad in [
        "",
        ".git/hooks/pre-commit",
        "sub/.git/config",
        ".envrc",
        ".vscode/tasks.json",
        ".tole/config.toml",
        "../x",
        "a/../b",
        "/etc/passwd",
        "\\\\server\\share",
        "C:\\x",
        "a\\.git\\hooks",
        ".",
        "build.rs",
        "crates/x/build.rs",
        "Makefile",
        "sub/package.json",
        "Cargo.toml",
        "skills/x/SKILL.md",
        "AGENTS.md",
        "scripts/deploy.sh",
        "run.PS1",
        "Makefile.",
        "Makefile. .",
        "MAKEFI~1",
        "docs/BUILD~1.RS",
        ".git./hooks/x",
    ] {
        assert!(!workspace_guard_ok(bad), "{bad}");
    }
    let guarded = AllowlistApprover::allow_only(vec!["write_file@workspace".into()]);
    let input = serde_json::json!({"path": "out.md"});
    let call = |target: Option<&str>| {
        guarded.decide(&ToolRequest {
            tool: "write_file",
            risk: Risk::Write,
            // The raw input never decides: the guard reads the target.
            input: &input,
            description: String::new(),
            target: target.map(str::to_string),
            summary: None,
        })
    };
    assert_eq!(call(Some("out.md")), Verdict::Allow);
    assert_eq!(call(Some(".git/hooks/x")), Verdict::Deny);
    assert_eq!(call(None), Verdict::Deny, "no target, no guard pass");
    // The guard pattern never matches another tool.
    assert_eq!(
        guarded.decide(&req("edit_file", Risk::Write)),
        Verdict::Deny
    );
    // As a DENY pattern it denies the tool outright.
    let deny = AllowlistApprover::deny_only(vec!["write_file@workspace".into()]);
    assert_eq!(deny.decide(&req("write_file", Risk::Write)), Verdict::Deny);
}

// ------------------------------------------- command patterns (#438)

#[cfg(feature = "shell-tools")]
fn cmd_verdict(patterns: &[&str], line: &str) -> PatternVerdict {
    let input = serde_json::json!({ "command": line });
    let req = ToolRequest {
        tool: "run_command",
        risk: Risk::Write,
        input: &input,
        description: String::new(),
        target: None,
        summary: None,
    };
    let pats: Vec<String> = patterns.iter().map(|p| p.to_string()).collect();
    pattern_verdict(&pats, &req)
}

#[cfg(feature = "shell-tools")]
#[test]
fn command_allow_patterns_match_the_argv_that_runs() {
    use PatternVerdict::*;
    let allow = ["run_command:git status *", "run_command:docker ps"];
    assert_eq!(cmd_verdict(&allow, "git status"), Allow);
    assert_eq!(cmd_verdict(&allow, "git status --porcelain -b"), Allow);
    assert_eq!(cmd_verdict(&allow, "docker ps"), Allow);
    // Exact arity without a trailing `*`.
    assert_eq!(cmd_verdict(&allow, "docker ps -a"), NoMatch);
    assert_eq!(cmd_verdict(&allow, "git push"), NoMatch);
    // No shell: `;` is just an argument, never a second command, and
    // the program token is literal (no path or glob tricks).
    assert_eq!(cmd_verdict(&allow, "git status; rm -rf /"), NoMatch);
    // Spaced out it matches, but runs only `git` with literal args
    // `;`, `rm`, `-rf`, `/` — nothing is chained.
    assert_eq!(cmd_verdict(&allow, "git status ; rm -rf /"), Allow);
    assert_eq!(
        cmd_verdict(&["run_command:echo *"], "rm -rf / ; echo"),
        NoMatch
    );
    assert_eq!(cmd_verdict(&allow, "/usr/bin/git status"), NoMatch);
    assert_eq!(cmd_verdict(&["run_command:*"], "rm -rf /"), NoMatch);
    // Per-argument globs.
    assert_eq!(
        cmd_verdict(
            &["run_command:cargo test -p tole-*"],
            "cargo test -p tole-cli"
        ),
        Allow
    );
    // A command pattern never matches another tool, and an
    // unterminated quote never matches.
    assert_eq!(cmd_verdict(&allow, "git status 'oops"), NoMatch);
    assert_eq!(
        AllowlistApprover::allow_only(vec!["run_command:git *".into()])
            .decide(&req("git", Risk::Write)),
        Verdict::Deny
    );
}

#[cfg(feature = "shell-tools")]
#[test]
fn deny_wins_sees_through_paths_and_sends_wrappers_to_a_human() {
    use PatternVerdict::*;
    let pats = ["run_command:ls *", "run_command:env *", "!run_command:rm *"];
    assert_eq!(cmd_verdict(&pats, "ls -la"), Allow);
    assert_eq!(cmd_verdict(&pats, "rm -rf build"), Deny);
    assert_eq!(cmd_verdict(&pats, "/bin/rm -rf build"), Deny);
    assert_eq!(cmd_verdict(&pats, "RM -rf build"), Deny, "case");
    assert_eq!(cmd_verdict(&pats, "rm.exe -rf build"), Deny, "extension");
    assert_eq!(
        cmd_verdict(&pats, "rm"),
        Deny,
        "trailing * allows zero args"
    );
    // Wrappers can hide the real program: ask, even when allowed.
    assert_eq!(cmd_verdict(&pats, "env rm -rf build"), Ask);
    // Fail closed: under a command deny, a command no explicit
    // `run_command:` allow names asks — a bare tool allow is not enough.
    let bare = ["run_command", "!run_command:rm *"];
    assert_eq!(cmd_verdict(&bare, "ls -la"), Ask);
    assert_eq!(cmd_verdict(&bare, "find . -exec rm {} ;"), Ask);
    assert_eq!(cmd_verdict(&bare, "sh -c 'rm -rf build'"), Ask);
    assert_eq!(cmd_verdict(&bare, "/usr/bin/sudo ls"), Ask);
    // Without a command deny, wrappers follow the normal rules.
    assert_eq!(cmd_verdict(&["run_command"], "env ls"), Allow);
    // A tool-name deny.
    assert_eq!(cmd_verdict(&["*", "!run_command"], "ls"), Deny);
    // Headless (nobody to ask): Ask is a refusal.
    let a = AllowlistApprover::allow_only(bare.iter().map(|p| p.to_string()).collect());
    let input = serde_json::json!({ "command": "ls x" });
    let r = ToolRequest {
        tool: "run_command",
        risk: Risk::Write,
        input: &input,
        description: String::new(),
        target: None,
        summary: None,
    };
    assert_eq!(a.decide(&r), Verdict::Deny);
    // deny_only treats `run_command:` patterns command-wise too.
    let d = AllowlistApprover::deny_only(vec!["run_command:rm *".into()]);
    assert_eq!(
        d.decide(&r),
        Verdict::Deny,
        "fail closed: a command deny refuses every run_command here"
    );
    // Other tools still follow the default.
    assert_eq!(d.decide(&req("write_file", Risk::Write)), Verdict::Allow);
    let input = serde_json::json!({ "command": "rm x" });
    let r = ToolRequest { input: &input, ..r };
    assert_eq!(d.decide(&r), Verdict::Deny);
}

#[cfg(feature = "shell-tools")]
#[test]
fn child_command_patterns_are_covered_per_argument() {
    let c = |p: &str, ch: &str| pattern_covers(p, ch);
    assert!(c("run_command:git status *", "run_command:git status -s"));
    assert!(c("run_command:git status *", "run_command:git status *"));
    assert!(c("run_command:git *", "run_command:git log --oneline"));
    assert!(c(
        "run_command:cargo test -p *",
        "run_command:cargo test -p tole-cli"
    ));
    // Text globbing would have said yes to all of these.
    // `git *` allows zero further arguments: plain `git` is narrower.
    assert!(c("run_command:git *", "run_command:git"));
    assert!(
        !c("run_command:git log *", "run_command:git"),
        "fewer args than the parent's fixed ones"
    );
    assert!(!c("run_command:git status", "run_command:git status *"));
    assert!(!c(
        "run_command:git status",
        "run_command:git status; rm -rf /"
    ));
    assert!(!c("run_command:git s*", "run_command:git status *"));
    assert!(!c("run_command:git *", "run_command:/usr/bin/git log"));
    assert!(!c("run_command:git status *", "run_command"));
    // Cora #118: a parent whose program token is a glob allows nothing
    // (the program is literal for an allow), so it covers nothing.
    assert!(!c("run_command:*", "run_command:git push *"));
    assert!(!c("run_command:g*", "run_command:rm *"));
    // A child wildcard is covered only by the same glob or a lone `*`.
    assert!(!c("run_command:git a?c", "run_command:git a*c"));
    assert!(
        c("run_command:git *", "run_command:git log*"),
        "trailing * is any args"
    );
    assert!(c("run_command:git log *", "run_command:git log -n*"));
    assert!(!c("w?", "w*"));
    assert!(c("write_*", "write_*"));
    // Tool-wide parents cover command children; denies cover nothing.
    assert!(c("run_command", "run_command:rm -rf *"));
    assert!(c("*", "run_command:anything *"));
    assert!(!c("!run_command", "run_command:ls"));
    assert!(!c("run_command", "!run_command:ls"));
    assert!(c("write_*", "write_file"));
}

#[cfg(feature = "shell-tools")]
#[test]
fn under_a_parent_command_deny_only_command_allows_cover_a_child() {
    let parent: Vec<String> = ["run_command", "!run_command:rm *"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    // The parent itself would ASK for `ls`: a child may not get it free.
    assert!(!list_covers(&parent, "run_command:ls *"));
    assert!(list_covers(&parent, "run_command"), "same as the parent");
    let parent: Vec<String> = ["run_command:ls *", "!run_command:rm *"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert!(list_covers(&parent, "run_command:ls -la"));
    assert!(!list_covers(&parent, "run_command:cat x"));
    // Without a command deny a bare tool allow covers command children.
    assert!(list_covers(
        &["run_command".to_string()],
        "run_command:ls *"
    ));
}

#[test]
fn deny_patterns_never_allow() {
    assert!(!pattern_allows(
        "!write_file",
        &req("write_file", Risk::Write)
    ));
    assert!(pattern_denies("!write_*", &req("write_file", Risk::Write)));
    let a = AllowlistApprover::allow_only(vec!["*".into(), "!write_file".into()]);
    assert_eq!(a.decide(&req("write_file", Risk::Write)), Verdict::Deny);
    assert_eq!(a.decide(&req("edit_file", Risk::Write)), Verdict::Allow);
}
