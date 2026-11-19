//! Face parity (#948, epic #941). The faces (CLI, `tole mcp`, a `serve`/ACP
//! session, cron) build their tool roster and system prompt from the same
//! code, so they must offer the same tools and the same prompt except where
//! a difference is DELIBERATE and written down.
//!
//! `docs/faces.md` holds that table between the markers `<!-- parity:begin -->`
//! and `<!-- parity:end -->`. This test builds a session through each face
//! with the same inputs, then requires:
//!
//! - every tool (and the system prompt) whose presence differs between faces
//!   has a table row, and the row's cells match what was built;
//! - every row is such a difference (a stale row fails too);
//! - every row says why: `policy` (allowed on purpose) or `drift #N` (a known
//!   gap, tracked in issue N).
//!
//! A face that gains or loses a tool, or a prompt, therefore fails the build
//! until the doc says so.

#![cfg(feature = "shell-tools")]

use std::collections::BTreeMap;
use std::path::Path;

mod common;
use common::TestDir;

use tole_cli::session::assemble::{build_default_prompt, roster_for, FacePolicy, RosterInputs};
use tole_core::approval::AllowlistApprover;
use tole_core::tool::ToolRegistry;

const FACES: [&str; 4] = ["cli", "mcp-server", "server-session", "cron"];
/// The row naming the system prompt (not a tool).
const PROMPT_ROW: &str = "(system prompt)";

/// Tools that exist only when an optional binary is on PATH or a repository
/// is detected; they depend on the machine, not on the face.
fn machine_dependent(name: &str) -> bool {
    // `load_skill` (and the skills sections of the prompt) exist when skills are
    // discovered (#945): in the workspace or in the owner's user-global
    // directory. Both the CLI and server sessions run the same `apply_skills`
    // (the test compares the prompt text with it below).
    name == "cora_search"
        || name == "gh"
        || name == "load_skill"
        || name == "skill_search"
        || name.starts_with("uteke_")
        || name.starts_with("mcp_")
}

/// A prompt without its one `Context:` line (working directory and date).
fn without_context(p: &str) -> String {
    p.lines()
        .filter(|l| !l.starts_with("Context: working directory "))
        .collect::<Vec<_>>()
        .join("\n")
}

fn names(reg: &ToolRegistry) -> Vec<String> {
    reg.specs()
        .iter()
        .map(|s| s["function"]["name"].as_str().unwrap().to_string())
        .filter(|n| !machine_dependent(n))
        .collect()
}

fn registry(policy: &FacePolicy, workspace: &Path, todo: bool) -> ToolRegistry {
    let setup = tole_cli::delegate::Setup {
        data_dir: tole_core::paths::tole_data_dir(),
        ..Default::default()
    };
    let roster = roster_for(
        policy,
        RosterInputs {
            workspace: workspace.to_path_buf(),
            plan_mode: false,
            allow_patterns: &[],
            agents_worktree: false,
            delegation: Some(&setup),
            mcp_specs: &[],
        },
    )
    .unwrap();
    let mut reg =
        tole_cli::tool_roster::build(AllowlistApprover::allow_only(vec![]), roster).unwrap();
    if todo {
        tole_cli::tool_roster::register_todo(&mut reg, &[], false).unwrap();
    }
    reg
}

/// Presence of every tool, and of a system prompt, per face.
fn observed(base: &TestDir) -> BTreeMap<String, BTreeMap<&'static str, bool>> {
    let w = base.join("w");
    std::fs::create_dir_all(&w).unwrap();
    let mut out: BTreeMap<String, BTreeMap<&'static str, bool>> = BTreeMap::new();
    let mut put = |face: &'static str, name: String| {
        out.entry(name).or_default().insert(face, true);
    };
    // CLI, mcp-server and cron are built through the shared roster, plus the
    // todo tools where the face registers them (CLI, cron; not `tole mcp`).
    let mut cli_reg = registry(&FacePolicy::CLI, &w, true);
    // The CLI's skills step (the same `apply_skills` a server session runs at
    // creation, #945), whose sections the prompt comparison below includes.
    let cli_skills = tole_cli::session::assemble::apply_skills(
        &[],
        Some(&w.to_str().unwrap().to_string()),
        false,
        &mut cli_reg,
        None,
    )
    .unwrap();
    for n in names(&cli_reg) {
        put("cli", n);
    }
    for n in names(&registry(&FacePolicy::MCP_SERVER, &w, false)) {
        put("mcp-server", n);
    }
    for n in names(&registry(&FacePolicy::CRON, &w, true)) {
        put("cron", n);
    }
    // A server session is built by the real session host; the entrypoint
    // (`tole serve`) sets the process's skills choice first (#945).
    tole_cli::session_host::set_server_skills(
        tole_cli::session::assemble::ServerSkills::new(vec![], false).unwrap(),
    );
    let session = tole_cli::session_host::open_session(
        "face-parity",
        w.to_str().unwrap(),
        false,
        false,
        AllowlistApprover::allow_only(vec![]),
        None,
        Some(&base.join("sessions")),
        vec![],
        vec![],
        tole_core::cancel::CancelToken::default(),
        None,
    )
    .unwrap();
    for n in names(&session.registry) {
        put("server-session", n);
    }
    // The default system prompt: compare the TEXT, not its presence. A server
    // session counts as having "the" prompt only when it equals the CLI's
    // modulo the context line (which names the face's own working directory).
    let cli_prompt = format!("{}{cli_skills}", build_default_prompt(false));
    put("cli", PROMPT_ROW.to_string());
    if session
        .system_prompt
        .as_deref()
        .is_some_and(|p| without_context(p) == without_context(&cli_prompt))
    {
        put("server-session", PROMPT_ROW.to_string());
    }
    // Complete every row to all faces (absent = false), except that cron and
    // `tole mcp` have no prompt row of their own to compare.
    for faces in out.values_mut() {
        for f in FACES {
            faces.entry(f).or_insert(false);
        }
    }
    out
}

struct Row {
    /// `None` = `n/a`: not compared (only the system-prompt row has them).
    cells: BTreeMap<&'static str, Option<bool>>,
    status: String,
    reason: String,
}

fn parse_table(doc: &str) -> BTreeMap<String, Row> {
    let begin = doc
        .find("<!-- parity:begin -->")
        .expect("docs/faces.md has no <!-- parity:begin --> marker");
    let end = doc
        .find("<!-- parity:end -->")
        .expect("docs/faces.md has no <!-- parity:end --> marker");
    let mut rows = BTreeMap::new();
    for line in doc[begin..end].lines().filter(|l| l.starts_with('|')) {
        let cols: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        // header, separator, or a row: tool | 4 faces | status | reason
        if cols.len() != 7 || cols[0] == "tool" || cols[0].starts_with('-') {
            continue;
        }
        let tool = cols[0].trim_matches('`').to_string();
        let mut cells = BTreeMap::new();
        for (i, f) in FACES.iter().enumerate() {
            cells.insert(
                *f,
                match cols[1 + i] {
                    "yes" => Some(true),
                    "no" => Some(false),
                    "n/a" if tool == PROMPT_ROW => None,
                    other => panic!("row {tool}: cell for {f} must be yes or no, got {other:?}"),
                },
            );
        }
        let row = Row {
            cells,
            status: cols[5].to_string(),
            reason: cols[6].to_string(),
        };
        assert!(
            rows.insert(tool.clone(), row).is_none(),
            "duplicate row {tool}"
        );
    }
    assert!(!rows.is_empty(), "the parity table has no rows");
    rows
}

#[test]
fn every_difference_between_faces_is_a_row_of_the_policy_table() {
    let base = TestDir::new("tole-face-parity");
    std::fs::create_dir_all(base.join("home")).unwrap();
    // One test in this binary, so the process environment is ours: a
    // hermetic home and a provider config (the discussion tools need one).
    std::env::set_var("CODECORA_HOME", base.join("home"));
    std::env::set_var("TOLE_BASE_URL", "http://127.0.0.1:9/v1");
    std::env::set_var("TOLE_MODEL", "m");
    std::env::set_var("TOLE_API_KEY", "sk-test-parity");
    std::env::remove_var("TOLE_SYSTEM_PROMPT");

    let doc =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/faces.md"))
            .expect("docs/faces.md");
    let table = parse_table(&doc);
    let seen = observed(&base);

    // The prompt row compares the two faces that can have one.
    let differs = |name: &str, faces: &BTreeMap<&'static str, bool>| {
        if name == PROMPT_ROW {
            faces["cli"] != faces["server-session"]
        } else {
            faces.values().any(|v| *v != faces["cli"])
        }
    };
    let mut problems = Vec::new();
    for (name, faces) in &seen {
        if !differs(name, faces) {
            continue;
        }
        match table.get(name) {
            None => problems.push(format!(
                "`{name}` differs between faces ({faces:?}) and has no row in docs/faces.md"
            )),
            Some(row) => {
                let compared: Vec<&&str> = if name == PROMPT_ROW {
                    vec![&"cli", &"server-session"]
                } else {
                    FACES.iter().collect()
                };
                for f in compared {
                    if row.cells[*f] != Some(faces[*f]) {
                        problems.push(format!(
                            "`{name}`: docs/faces.md says {f}={:?} but it is {}",
                            row.cells[*f], faces[*f]
                        ));
                    }
                }
            }
        }
    }
    for (name, row) in &table {
        match seen.get(name) {
            Some(faces) if differs(name, faces) => {}
            _ => problems.push(format!(
                "docs/faces.md has a row for `{name}` but the faces do not differ on it"
            )),
        }
        let ok_status = row.status == "policy"
            || row
                .status
                .strip_prefix("drift #")
                .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if !ok_status {
            problems.push(format!(
                "`{name}`: status must be `policy` or `drift #N`, got {:?}",
                row.status
            ));
        }
        if row.reason.len() < 12 {
            problems.push(format!("`{name}`: give the reason for the difference"));
        }
    }
    assert!(
        problems.is_empty(),
        "face parity:\n  {}",
        problems.join("\n  ")
    );
}

#[test]
fn the_table_parser_rejects_a_bad_cell() {
    let doc = "<!-- parity:begin -->\n| tool | cli | mcp-server | server-session | cron | status | reason |\n|---|---|---|---|---|---|---|\n| `x` | yes | maybe | no | no | policy | a reason here |\n<!-- parity:end -->";
    let r = std::panic::catch_unwind(|| parse_table(doc));
    assert!(r.is_err());
}
