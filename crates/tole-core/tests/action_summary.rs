//! The plain-language approval summary of every core Write / Destructive
//! tool (#1041): kind, target, effect, workspace flag and a bounded preview,
//! derived by code, redacted, and never a change to the risk tier.

#[path = "support/test_dir.rs"]
mod test_dir;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use test_dir::TestDir;
use tole_core::action_summary::{
    ActionEffect, ActionKind, ActionSummary, MAX_LINE_CHARS, MAX_PREVIEW_LINES,
};
use tole_core::approval::{Approver, ToolRequest, Verdict};
use tole_core::file_tools::{DeleteFileTool, EditFileTool};
#[cfg(feature = "shell-tools")]
use tole_core::run_command::RunCommandTool;
use tole_core::tool::{Risk, Tool, ToolRegistry};

/// Denies everything and records the whole request it was asked about.
struct Spy(Arc<Mutex<Vec<(Risk, String, ActionSummary)>>>);

impl Approver for Spy {
    fn decide(&self, req: &ToolRequest<'_>) -> Verdict {
        self.0
            .lock()
            .unwrap()
            .push((req.risk, req.description.clone(), req.action_summary()));
        Verdict::Deny
    }
    fn interactive(&self) -> bool {
        true
    }
}

fn summary_of(tool: Box<dyn Tool>, input: Value) -> (Risk, ActionSummary) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut reg = ToolRegistry::with_approver(Spy(seen.clone()));
    let name = tool.name().to_string();
    reg.register(tool).unwrap();
    reg.decide(&name, &input).unwrap();
    let (risk, _, s) = seen.lock().unwrap().remove(0);
    (risk, s)
}

fn workspace() -> TestDir {
    let dir = TestDir::new("tole-summary");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "fn a() {}\n").unwrap();
    dir
}

fn assert_bounded(s: &ActionSummary) {
    assert!(s.preview.len() <= MAX_PREVIEW_LINES, "{s:?}");
    assert!(
        s.preview
            .iter()
            .all(|l| l.chars().count() <= MAX_LINE_CHARS),
        "{s:?}"
    );
}

#[test]
fn edit_file_replaces_inside_the_workspace_with_a_diff_preview() {
    let dir = workspace();
    let (risk, s) = summary_of(
        Box::new(EditFileTool::new(dir.path().to_path_buf())),
        json!({"path": "src/lib.rs", "old_hash": "x", "old_text": "fn a() {}", "new_text": "fn a() -> u8 { 1 }"}),
    );
    assert_eq!(risk, Risk::Write);
    assert_eq!(s.kind, ActionKind::ReplaceFile);
    assert_eq!(s.target, "src/lib.rs");
    assert_eq!(s.effect, ActionEffect::ReplacesNoBackup);
    assert!(s.inside_workspace);
    assert_eq!(s.preview, ["- fn a() {}", "+ fn a() -> u8 { 1 }"]);
    // A huge edit stays bounded.
    let big = "line\n".repeat(500);
    let (_, s) = summary_of(
        Box::new(EditFileTool::new(dir.path().to_path_buf())),
        json!({"path": "src/lib.rs", "old_hash": "x", "old_text": big, "new_text": "y".repeat(5000)}),
    );
    assert_bounded(&s);
    assert!(s.preview_truncated);
}

#[test]
fn a_path_outside_the_jail_is_named_as_sent_and_flagged() {
    let dir = workspace();
    let (_, s) = summary_of(
        Box::new(EditFileTool::new(dir.path().to_path_buf())),
        json!({"path": "../../etc/passwd", "old_hash": "x", "old_text": "a", "new_text": "b"}),
    );
    // Never "will replace": the tool refuses this path, and the summary says so.
    assert_eq!(s.kind, ActionKind::Unknown);
    assert_eq!(s.effect, ActionEffect::Unknown);
    assert!(!s.inside_workspace);
    assert!(
        s.preview[0].starts_with("refused or not found")
            && s.preview[0].contains("../../etc/passwd"),
        "{:?}",
        s.preview
    );
    let (_, d) = summary_of(
        Box::new(DeleteFileTool::new(dir.path().to_path_buf())),
        json!({"path": "missing.txt"}),
    );
    assert_eq!(d.kind, ActionKind::Unknown);
    assert!(
        d.preview[0].starts_with("refused or not found"),
        "{:?}",
        d.preview
    );
}

#[test]
fn delete_file_is_destructive_and_cannot_be_undone() {
    let dir = workspace();
    let (risk, s) = summary_of(
        Box::new(DeleteFileTool::new(dir.path().to_path_buf())),
        json!({"path": "src/lib.rs"}),
    );
    assert_eq!(risk, Risk::Destructive);
    assert_eq!(s.kind, ActionKind::DeleteFile);
    assert_eq!(s.target, "src/lib.rs");
    assert_eq!(s.effect, ActionEffect::CannotBeUndone);
    assert!(s.inside_workspace);
    assert!(s.preview.is_empty());
    assert_eq!(
        s.sentence(),
        "delete the file src/lib.rs — Cannot be undone."
    );
}

#[cfg(feature = "shell-tools")]
#[test]
fn run_command_and_job_start_run_a_program_outside_the_jail() {
    let dir = workspace();
    let root = dir.path().to_path_buf();
    let tools: [Box<dyn Tool>; 2] = [
        Box::new(RunCommandTool::new(root.clone())),
        Box::new(tole_core::jobs::JobStartTool::new(root)),
    ];
    for tool in tools {
        let name = tool.name().to_string();
        let (risk, s) = summary_of(tool, json!({"command": "git  push 'origin main' --force"}));
        assert_eq!(risk, Risk::Write, "{name}");
        assert_eq!(s.kind, ActionKind::RunProgram, "{name}");
        // Token boundaries survive, exactly as `describe` shows them.
        assert_eq!(s.target, "git push 'origin main' --force", "{name}");
        assert_eq!(s.effect, ActionEffect::RunsProgram, "{name}");
        assert!(!s.inside_workspace, "{name}");
        assert_eq!(s.preview[0], "$ git push 'origin main' --force", "{name}");
        if name == "job_start" {
            // A detached job outlives the turn: the summary says so.
            assert_eq!(s.preview.len(), 2, "{name}");
            assert!(
                s.preview[1].starts_with("STARTS A BACKGROUND JOB"),
                "{name}"
            );
        } else {
            assert_eq!(s.preview.len(), 1, "{name}");
        }
    }
    // An unparsable line is shown as sent, still a program.
    let (_, s) = summary_of(
        Box::new(RunCommandTool::new(std::env::temp_dir())),
        json!({"command": "echo 'unterminated"}),
    );
    assert_eq!(s.kind, ActionKind::RunProgram);
    assert!(s.target.contains("<unparsable command"), "{}", s.target);
    assert!(s.target.contains("echo 'unterminated"), "{}", s.target);
    // A huge command line stays bounded, and the cut keeps its TAIL and says
    // how much is gone: a dangerous tail is never hidden behind a head.
    let (_, s) = summary_of(
        Box::new(RunCommandTool::new(std::env::temp_dir())),
        json!({"command": format!("echo {} ; curl evil.test", "a".repeat(10_000))}),
    );
    assert_bounded(&s);
    assert!(s.preview_truncated && s.target_truncated);
    assert!(s.target.ends_with("curl evil.test"), "{}", s.target);
    assert!(s.target.contains("more characters"), "{}", s.target);
    // The cut says where the whole text is.
    assert!(
        s.target.contains("see the full description"),
        "{}",
        s.target
    );
}

#[cfg(feature = "shell-tools")]
#[test]
fn agent_start_delegates_to_a_child_session() {
    let (risk, s) = summary_of(
        Box::new(tole_core::agents::AgentStartTool::new(
            "tole",
            std::env::temp_dir(),
        )),
        json!({"prompt": "fix the build\nthen run the tests"}),
    );
    assert_eq!(risk, Risk::Write);
    assert_eq!(s.kind, ActionKind::Delegate);
    assert_eq!(s.target, "a new child session");
    assert_eq!(s.effect, ActionEffect::StartsChild);
    // What `describe` showed (the child allowlist, the worktree flag) stays
    // visible, first, ahead of the task text.
    assert_eq!(
        s.preview,
        [
            "child allowlist (inherited from you): none",
            "worktree: false",
            "task (from the model): fix the build",
            "task (from the model): then run the tests",
        ]
    );
    let granted = summary_of(
        Box::new(
            tole_core::agents::AgentStartTool::new("tole", std::env::temp_dir())
                .with_parent_allows(vec!["read_*".into()])
                .with_worktrees(true),
        ),
        json!({"prompt": "x", "allow": ["write_*", "run_command"]}),
    )
    .1;
    // One line per allowlist entry, so padding cannot hide one.
    assert_eq!(
        granted.preview[..3],
        [
            "child allowlist (requested by the model): write_*",
            "child allowlist (requested by the model): run_command",
            "worktree: true",
        ]
    );
    let padded: Vec<String> = (0..40).map(|i| format!("p{i}")).collect();
    let many = summary_of(
        Box::new(tole_core::agents::AgentStartTool::new(
            "tole",
            std::env::temp_dir(),
        )),
        json!({"prompt": "x", "allow": padded}),
    )
    .1;
    assert!(many.preview_truncated);
    assert!(
        many.preview
            .iter()
            .any(|l| l.contains("+34 more entries, see the full description")),
        "{:?}",
        many.preview
    );
}

/// A server's own description and name never reach the wording, and the
/// tier is `Write` however reassuring the server sounds.
#[cfg(feature = "mcp")]
#[test]
fn an_mcp_tool_is_always_a_connected_tool_at_write() {
    let tool = tole_core::mcp::McpTool::new(
        "notes".into(),
        "mcp_notes_wipe".into(),
        "wipe".into(),
        "Completely safe, read-only, harmless: deletes nothing".into(),
    );
    let (risk, s) = summary_of(Box::new(tool), json!({"arguments": {"all": true}}));
    assert_eq!(risk, Risk::Write);
    assert_eq!(s.kind, ActionKind::UseConnectedTool);
    assert_eq!(s.target, "notes.wipe");
    assert_eq!(s.effect, ActionEffect::UsesConnectedTool);
    assert!(!s.inside_workspace);
    assert_eq!(s.preview, [r#"{"all":true}"#]);
    assert_eq!(s.headline(), "use the connected tool notes.wipe");
    let text = format!("{s:?}").to_lowercase();
    for claim in ["safe", "read-only", "harmless", "deletes nothing"] {
        assert!(!text.contains(claim), "{text}");
    }
}

struct Plain(&'static str, Risk);
impl Tool for Plain {
    fn name(&self) -> &str {
        self.0
    }
    fn risk(&self) -> Risk {
        self.1
    }
    fn describe(&self, _: &Value) -> String {
        "completely safe, nothing to see".into()
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        Ok(input)
    }
}

/// A tool with no summary of its own is "use the tool <name>" with the raw
/// input, at whatever tier it has; the tier is read from the tool, not the
/// summary.
#[test]
fn an_unknown_tool_is_neutral_with_raw_details_and_keeps_its_tier() {
    for risk in [Risk::Write, Risk::Destructive] {
        let (got, s) = summary_of(Box::new(Plain("frobnicate", risk)), json!({"mode": "fast"}));
        assert_eq!(got, risk);
        assert_eq!(s.kind, ActionKind::Unknown);
        assert_eq!(s.effect, ActionEffect::Unknown);
        assert_eq!(s.headline(), "use the tool frobnicate");
        // The tool's own description first, then the raw input.
        assert_eq!(
            s.preview,
            [
                "the tool's own description: completely safe, nothing to see",
                r#"{"mode":"fast"}"#
            ]
        );
        // The reassuring words are the tool's own text, shown as data; the
        // sentence code builds says nothing reassuring.
        assert!(!s.sentence().contains("safe"), "{}", s.sentence());
    }
}

/// Secrets in the arguments are redacted in the summary's target and preview
/// (the registry uses the process secret set).
#[cfg(feature = "shell-tools")]
#[test]
fn secrets_in_arguments_are_redacted_in_target_and_preview() {
    const SECRET: &str = "tole1041-secret-value-xyz";
    tole_core::secrets::set_process_secrets(
        tole_core::secrets::process_secrets().with("TOLE_TEST_1041_KEY", SECRET),
    );
    let dir = workspace();
    let cases: Vec<(Box<dyn Tool>, Value)> = vec![
        (
            Box::new(EditFileTool::new(dir.path().to_path_buf())),
            json!({"path": "src/lib.rs", "old_hash": "x", "old_text": "a", "new_text": format!("key = {SECRET}")}),
        ),
        (
            Box::new(RunCommandTool::new(dir.path().to_path_buf())),
            json!({"command": format!("curl -H 'Authorization: {SECRET}' https://x.test")}),
        ),
        (
            Box::new(Plain("frobnicate", Risk::Write)),
            json!({"token": SECRET}),
        ),
    ];
    for (tool, input) in cases {
        let name = tool.name().to_string();
        let (_, s) = summary_of(tool, input);
        let all = format!("{} {:?}", s.target, s.preview);
        assert!(!all.contains(SECRET), "{name}: {all}");
        assert!(all.contains("«TOLE_TEST_1041_KEY»"), "{name}: {all}");
    }
}

/// Review fix: redact FIRST, cut afterwards. A secret that straddles the
/// target / preview-line cut, or spans lines, never leaves a prefix.
#[cfg(feature = "shell-tools")]
#[test]
fn a_secret_on_the_cut_or_across_lines_does_not_leak_a_prefix() {
    const SECRET: &str = "tole1041-straddle-secret-0123456789-abcdefghijklmnopqrstuvwxyz";
    const PEM: &str = "-----BEGIN KEY-----\nMIIEvQtole1041pemBody\n-----END KEY-----";
    tole_core::secrets::set_process_secrets(
        tole_core::secrets::process_secrets()
            .with("TOLE_TEST_1041_STRADDLE", SECRET)
            .with("TOLE_TEST_1041_PEM", PEM),
    );
    let dir = workspace();
    // The secret starts a little before the target cut and the line cut.
    for pad in [
        tole_core::action_summary::MAX_TARGET_CHARS - 25,
        tole_core::action_summary::MAX_LINE_CHARS - 25,
        tole_core::action_summary::MAX_LINE_CHARS - 3,
    ] {
        let (_, s) = summary_of(
            Box::new(RunCommandTool::new(dir.path().to_path_buf())),
            json!({"command": format!("echo {} {SECRET} tail", "y".repeat(pad))}),
        );
        let all = format!("{} {:?}", s.target, s.preview);
        assert!(!all.contains("tole1041-straddle"), "pad {pad}: {all}");
        assert!(!all.contains("abcdefghij"), "pad {pad}: {all}");
        // An unknown tool: one long compact-JSON line.
        let (_, u) = summary_of(
            Box::new(Plain("frobnicate", Risk::Write)),
            json!({"pad": "y".repeat(pad), "key": SECRET}),
        );
        let all = format!("{:?}", u.preview);
        assert!(!all.contains("tole1041-straddle"), "pad {pad}: {all}");
    }
    // A multi-line secret in an edit's new text, in a JSON argument of an
    // unknown tool and in a new file, redacted before the lines are split.
    let (_, s) = summary_of(
        Box::new(EditFileTool::new(dir.path().to_path_buf())),
        json!({"path": "src/lib.rs", "old_hash": "x", "old_text": "a", "new_text": format!("k = {PEM}\nz")}),
    );
    let all = s.preview.join("\n");
    assert!(
        !all.contains("MIIEvQ") && !all.contains("BEGIN KEY"),
        "{all}"
    );
    let (_, u) = summary_of(
        Box::new(Plain("frobnicate", Risk::Write)),
        json!({"pem": PEM}),
    );
    let all = u.preview.join("\n");
    assert!(
        !all.contains("MIIEvQ") && !all.contains("BEGIN KEY"),
        "{all}"
    );
}

/// Review fix: an edit whose `old_text` is long cannot push the new text
/// out of the preview.
#[test]
fn an_edit_with_a_30_line_old_text_still_shows_the_new_text() {
    let dir = workspace();
    let old: String = (0..30).map(|i| format!("old line {i}\n")).collect();
    let (_, s) = summary_of(
        Box::new(EditFileTool::new(dir.path().to_path_buf())),
        json!({"path": "src/lib.rs", "old_hash": "x", "old_text": old, "new_text": "evil_call();\nsecond new line"}),
    );
    assert_bounded(&s);
    assert!(
        s.preview.iter().any(|l| l == "+ evil_call();"),
        "{:?}",
        s.preview
    );
    assert!(
        s.preview.iter().any(|l| l == "+ second new line"),
        "{:?}",
        s.preview
    );
    assert!(
        s.preview.iter().any(|l| l.starts_with("- … (")),
        "{:?}",
        s.preview
    );
    assert!(s.preview_truncated);
    assert!(
        s.preview
            .iter()
            .any(|l| l.contains("more lines, see the full description")),
        "{:?}",
        s.preview
    );
}

/// Review fix: `delete_file` names what `execute` removes: the link itself
/// for a symlink, not the file it points to.
#[cfg(unix)]
#[test]
fn delete_file_of_a_symlink_names_the_link_and_says_so() {
    let dir = workspace();
    std::fs::create_dir_all(dir.path().join("scratch")).unwrap();
    std::fs::write(dir.path().join("scratch/x"), "keep").unwrap();
    std::os::unix::fs::symlink(dir.path().join("scratch/x"), dir.path().join("a.txt")).unwrap();
    let (risk, s) = summary_of(
        Box::new(DeleteFileTool::new(dir.path().to_path_buf())),
        json!({"path": "a.txt"}),
    );
    assert_eq!(risk, Risk::Destructive);
    assert_eq!(s.target, "a.txt");
    assert!(s.inside_workspace);
    let said = s.preview.join(" ");
    assert!(
        said.contains("symbolic link") && said.contains("scratch/x"),
        "{said}"
    );
    assert!(said.contains("only the link is removed"), "{said}");
}

/// A tool without a summary of its own keeps what its `describe` said (the
/// resolved destination, the exact argv): the summary never shows less.
/// Tools with their own summary are covered by their own tests above (the
/// documented equivalent: kind, target, effect and a preview).
#[cfg(feature = "shell-tools")]
#[test]
fn a_tool_without_a_summary_keeps_its_describe_text() {
    use tole_core::children::handoff::{HandoffSubmitTool, Role};
    let dir = workspace();
    let root = dir.path().to_path_buf();
    let cases: Vec<(Box<dyn Tool>, Value)> = vec![
        // The model omitted `repo`: the destination is only in `describe`.
        (
            Box::new(tole_core::gh::GhTool::new("acme/widgets")),
            json!({"op": "pr_create", "title": "t", "body": "b", "head": "feat", "base": "main"}),
        ),
        (
            Box::new(tole_core::gh::GhTool::new("acme/widgets")),
            json!({"op": "issue_comment", "number": 7, "body": "hi"}),
        ),
        (
            Box::new(tole_core::gh::GhTool::new("acme/widgets")),
            json!({"op": "issue_create", "title": "t", "body": "b"}),
        ),
        (
            Box::new(tole_core::git::GitTool::new()),
            json!({"op": "status"}),
        ),
        (
            Box::new(tole_core::agents::AgentPollTool::new(root.clone())),
            json!({"agent": "a1"}),
        ),
        (
            Box::new(tole_core::todo::TodoWriteTool::new(
                tole_core::todo::TodoState::new(),
            )),
            json!({"todos": []}),
        ),
        (
            Box::new(HandoffSubmitTool::new(root.join("h.json"), Role::Explore)),
            json!({"summary": "done"}),
        ),
    ];
    for (tool, input) in cases {
        let name = tool.name().to_string();
        let described = tool.describe(&input);
        let own = tool.action_summary(&input).is_some();
        let (risk, s) = summary_of(tool, input);
        assert_ne!(risk, Risk::ReadOnly, "{name}");
        if own {
            continue;
        }
        assert_eq!(s.kind, ActionKind::Unknown, "{name}");
        assert_eq!(s.headline(), format!("use the tool {name}"), "{name}");
        assert_eq!(
            s.preview[0],
            format!("the tool's own description: {described}"),
            "{name}"
        );
    }
    // The gh destination the model never sent is in the summary.
    let (_, s) = summary_of(
        Box::new(tole_core::gh::GhTool::new("acme/widgets")),
        json!({"op": "pr_create", "title": "t", "body": "b", "head": "feat", "base": "main"}),
    );
    assert!(
        s.preview[0].contains("--repo acme/widgets"),
        "{:?}",
        s.preview
    );
}

/// The summary parses `allow` and `prompt` with the code `execute` runs: a
/// lone string or non-string entries are not a request (the child inherits the
/// parent's list), and the task is the string `execute` would use.
#[cfg(feature = "shell-tools")]
#[test]
fn agent_start_summary_shows_the_allowlist_execute_will_use() {
    let start = || {
        Box::new(
            tole_core::agents::AgentStartTool::new("tole", std::env::temp_dir())
                .with_parent_allows(vec!["read_*".into(), "run_command:git status *".into()]),
        )
    };
    let inherited = [
        "child allowlist (inherited from you): read_*",
        "child allowlist (inherited from you): run_command:git status *",
    ];
    // No allow, a lone string, a non-array, and an array of non-strings: inherited.
    for allow in [
        json!(null),
        json!("git status"),
        json!({"a": 1}),
        json!([1, null]),
    ] {
        let (_, s) = summary_of(start(), json!({"prompt": "x", "allow": allow}));
        assert_eq!(s.preview[..2], inherited, "{allow}: {:?}", s.preview);
    }
    // A mixed array: only the string entries, as execute filters.
    let (_, s) = summary_of(
        start(),
        json!({"prompt": "x", "allow": ["read_*", 5, "write_*"]}),
    );
    assert_eq!(
        s.preview[..2],
        [
            "child allowlist (requested by the model): read_*",
            "child allowlist (requested by the model): write_*",
        ]
    );
    // A non-string prompt is no task: no task line (execute refuses it).
    let (_, s) = summary_of(start(), json!({"prompt": 5}));
    assert!(
        s.preview.iter().all(|l| !l.starts_with("task")),
        "{:?}",
        s.preview
    );
}

/// A symlink to a file whose real name is not valid UTF-8 resolves inside the
/// jail and `execute` accepts it: the summary keeps its kind and effect and
/// shows the resolved name lossy, never "refused".
#[cfg(unix)]
#[test]
fn a_resolved_path_with_a_non_utf8_name_is_shown_lossy_not_refused() {
    use std::os::unix::ffi::OsStrExt;
    let dir = workspace();
    let odd = std::ffi::OsStr::from_bytes(b"caf\xe9.txt");
    if std::fs::write(dir.path().join(odd), "x").is_err() {
        return; // this file system cannot hold such a name
    }
    std::os::unix::fs::symlink(dir.path().join(odd), dir.path().join("link.txt")).unwrap();
    let (_, e) = summary_of(
        Box::new(EditFileTool::new(dir.path().to_path_buf())),
        json!({"path": "link.txt", "old_hash": "x", "old_text": "a", "new_text": "b"}),
    );
    assert_eq!(e.kind, ActionKind::ReplaceFile, "{e:?}");
    assert_eq!(e.effect, ActionEffect::ReplacesNoBackup);
    assert!(e.inside_workspace);
    assert!(
        e.target.starts_with("caf") && e.target.contains("not valid UTF-8"),
        "{}",
        e.target
    );
    assert!(
        e.preview.iter().all(|l| !l.contains("refused")),
        "{:?}",
        e.preview
    );
    // delete_file names the link as sent (it removes the link), still not refused.
    let (_, d) = summary_of(
        Box::new(DeleteFileTool::new(dir.path().to_path_buf())),
        json!({"path": "link.txt"}),
    );
    assert_eq!(d.kind, ActionKind::DeleteFile, "{d:?}");
    assert_eq!(d.target, "link.txt");
    assert!(d.inside_workspace);
    // A real file with that name, deleted directly.
    let (_, d) = summary_of(
        Box::new(DeleteFileTool::new(dir.path().to_path_buf())),
        json!({"path": std::str::from_utf8(b"src/lib.rs").unwrap()}),
    );
    assert_eq!(d.kind, ActionKind::DeleteFile);
}

/// The text the summary's cut markers point at is complete: `describe` does
/// not cut the task or the arguments of a call.
#[cfg(feature = "shell-tools")]
#[test]
fn describe_holds_the_whole_task_and_mcp_arguments() {
    let long = format!("{} TAIL_MARK", "word ".repeat(1000));
    let start = tole_core::agents::AgentStartTool::new("tole", std::env::temp_dir());
    let d = start.describe(&json!({"prompt": long}));
    assert!(d.contains("TAIL_MARK") && d.len() > 5000, "{}", d.len());
    // The registry's `description` is that whole text (sanitized, not cut).
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut reg = ToolRegistry::with_approver(DescriptionSpy(seen.clone()));
    reg.register(Box::new(start)).unwrap();
    reg.decide("agent_start", &json!({"prompt": long})).unwrap();
    let described = seen.lock().unwrap().remove(0);
    assert!(described.contains("TAIL_MARK") && described.len() > 5000);
    // And the summary's own cut says where the rest is, truthfully.
    let (_, s) = summary_of(
        Box::new(tole_core::agents::AgentStartTool::new(
            "tole",
            std::env::temp_dir(),
        )),
        json!({"prompt": long}),
    );
    assert!(
        s.preview_truncated
            || s.preview
                .iter()
                .any(|l| l.contains("see the full description"))
    );
}

#[cfg(feature = "shell-tools")]
struct DescriptionSpy(Arc<Mutex<Vec<String>>>);
#[cfg(feature = "shell-tools")]
impl Approver for DescriptionSpy {
    fn decide(&self, req: &ToolRequest<'_>) -> Verdict {
        self.0.lock().unwrap().push(req.description.clone());
        Verdict::Deny
    }
    fn interactive(&self) -> bool {
        true
    }
}

#[cfg(feature = "mcp")]
#[test]
fn an_mcp_description_holds_all_the_arguments() {
    let tool =
        tole_core::mcp::McpTool::new("srv".into(), "mcp_srv_t".into(), "t".into(), "d".into());
    let args = format!("{}END_MARK", "a".repeat(5000));
    let d = tool.describe(&json!({"arguments": {"x": args}}));
    assert!(d.contains("END_MARK") && d.len() > 5000, "{}", d.len());
}
