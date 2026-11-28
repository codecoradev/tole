//! The tool allowance and the owner's approvals in the ledger (#1039; moved out
//! of `src/` for #1249, only the public API).

use crate::test_dir::TestDir;
use tole_core::discussion::log::{DiscussionLog, Event, Next, MAX_RECORD_BYTES};
use tole_core::discussion::{
    tool_name_ok, Contract, Gate, ToolVerdict, MAX_ALLOWED_TOOLS, MAX_QUESTION_CHARS,
    MAX_TOOL_NAME_CHARS,
};
use tole_core::secrets::SecretSet;

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn contract(id: &str, tools: &[&str]) -> Contract {
    Contract::new(
        id,
        "Which invoice tool?",
        &strs(&["a choice is named"]),
        &strs(&["coder", "ops"]),
        10_000,
        None,
        &SecretSet::default(),
    )
    .unwrap()
    .with_tools(&strs(tools))
    .unwrap()
}

fn log(tag: &str, id: &str, tools: &[&str]) -> (DiscussionLog, TestDir) {
    let d = TestDir::new(&format!("tole-discuss-tools-{tag}"));
    let l = DiscussionLog::at(&d, id).unwrap();
    l.start(contract(id, tools), 1).unwrap();
    (l, d)
}

const SEARCH: &str = "mcp_nginjen_search";
const SCRAPE: &str = "mcp_nginjen_scrape";

#[test]
fn only_connected_tool_names_can_be_allowed() {
    assert!(tool_name_ok("mcp_nginjen_search"));
    assert!(tool_name_ok("mcp_a-b_c_1"));
    for bad in [
        "",
        "mcp_",
        "write_file",
        "run_command",
        "web_search",
        "mcp_a b",
        "mcp_a.b",
        "mcp_a/b",
        "mcp_\u{e9}",
        &format!("mcp_{}", "x".repeat(MAX_TOOL_NAME_CHARS)),
    ] {
        assert!(!tool_name_ok(bad), "{bad:?}");
    }
    let c = contract("t", &[]);
    assert!(c.clone().with_tools(&strs(&["write_file"])).is_err());
    assert!(c.clone().with_tools(&strs(&[SEARCH, SEARCH])).is_err());
    let many: Vec<String> = (0..=MAX_ALLOWED_TOOLS)
        .map(|i| format!("mcp_s_t{i}"))
        .collect();
    assert!(c.clone().with_tools(&many).is_err());
    assert!(c.with_tools(&many[..MAX_ALLOWED_TOOLS]).is_ok());
}

#[test]
fn a_contract_without_tools_is_written_as_before_and_one_with_tools_replays() {
    let plain = serde_json::to_string(&Event::Contract(contract("t", &[]))).unwrap();
    assert!(!plain.contains("tools"), "{plain}");
    let (l, d) = log("replay", "t", &[SEARCH, SCRAPE]);
    let line = std::fs::read_to_string(d.join("t.jsonl")).unwrap();
    assert!(line.contains(r#""tools":["mcp_nginjen_search","mcp_nginjen_scrape"]"#));
    let st = l.state().unwrap();
    assert_eq!(st.contract.unwrap().tools, strs(&[SEARCH, SCRAPE]));
}

#[test]
fn a_ledger_from_before_the_allowance_replays_with_none() {
    let d = TestDir::new("tole-discuss-tools-old");
    std::fs::write(
        d.join("pick-db.jsonl"),
        concat!(
            r#"{"seq":1,"t":1,"event":"contract","id":"pick-db","question":"Which database?","criteria":["a choice is named"],"participants":["coder","ops"],"budget_tokens":10000,"max_rounds":3}"#,
            "\n"
        ),
    )
    .unwrap();
    let st = DiscussionLog::at(&d, "pick-db").unwrap().state().unwrap();
    assert!(st.contract.as_ref().unwrap().tools.is_empty());
    assert!(st.tool_approvals.is_empty() && st.tool_grants().is_empty());
}

#[test]
fn the_first_approval_grants_the_tool_for_this_discussion_only() {
    let (l, _d) = log("grant", "t", &[SEARCH, SCRAPE]);
    assert!(l.state().unwrap().tool_grants().is_empty());
    assert!(l
        .tool_approval("dap-1", SEARCH, ToolVerdict::Allow, 2)
        .unwrap());
    let st = l.state().unwrap();
    assert_eq!(st.tool_grants(), ["mcp_nginjen_search".to_string()].into());
    // Another tool of the same discussion still asks.
    assert!(!st.tool_grants().contains(SCRAPE));
    // Another discussion never inherits it.
    let (other, _d2) = log("grant-other", "u", &[SEARCH, SCRAPE]);
    assert!(other.state().unwrap().tool_grants().is_empty());
}

#[test]
fn a_decision_is_recorded_once_per_request() {
    let (l, d) = log("once", "t", &[SEARCH]);
    assert!(l
        .tool_approval("dap-1", SEARCH, ToolVerdict::Allow, 2)
        .unwrap());
    let before = std::fs::read_to_string(d.join("t.jsonl")).unwrap();
    // Seen again (a poll, a resume): nothing is written, and nothing flips.
    assert!(!l
        .tool_approval("dap-1", SEARCH, ToolVerdict::Deny, 3)
        .unwrap());
    assert_eq!(std::fs::read_to_string(d.join("t.jsonl")).unwrap(), before);
    assert_eq!(l.state().unwrap().tool_approvals.len(), 1);
}

#[test]
fn a_tool_the_contract_does_not_allow_takes_no_decision() {
    let (l, _d) = log("foreign", "t", &[SEARCH]);
    let e = l
        .tool_approval("dap-1", SCRAPE, ToolVerdict::Allow, 2)
        .unwrap_err();
    assert!(e.contains("not a tool this discussion allows"), "{e}");
    assert!(l.state().unwrap().tool_approvals.is_empty());
    assert!(l
        .tool_approval("bad id", SEARCH, ToolVerdict::Allow, 2)
        .is_err());
}

#[test]
fn denial_and_expiry_grant_nothing_and_a_later_one_takes_a_grant_back() {
    let (l, _d) = log("deny", "t", &[SEARCH]);
    l.tool_approval("dap-1", SEARCH, ToolVerdict::Deny, 2)
        .unwrap();
    l.tool_approval("dap-2", SEARCH, ToolVerdict::Expired, 3)
        .unwrap();
    assert!(l.state().unwrap().tool_grants().is_empty());
    l.tool_approval("dap-3", SEARCH, ToolVerdict::Allow, 4)
        .unwrap();
    assert_eq!(l.state().unwrap().tool_grants().len(), 1);
    l.tool_approval("dap-4", SEARCH, ToolVerdict::Deny, 5)
        .unwrap();
    assert!(l.state().unwrap().tool_grants().is_empty());
    // Nothing was edited: four records, in order.
    assert_eq!(l.state().unwrap().tool_approvals.len(), 4);
}

#[test]
fn a_stopped_or_closed_discussion_grants_nothing() {
    let (l, _d) = log("ends", "t", &[SEARCH]);
    l.tool_approval("dap-1", SEARCH, ToolVerdict::Allow, 2)
        .unwrap();
    assert_eq!(l.state().unwrap().tool_grants().len(), 1);
    // The stop ends the grant at once, before the driver closes anything.
    l.stop(3).unwrap();
    let st = l.state().unwrap();
    assert!(matches!(st.next(), Next::Finish { .. }));
    assert!(st.tool_grants().is_empty());
    l.close("stopped", 4).unwrap();
    let st = l.state().unwrap();
    assert!(st.tool_grants().is_empty());
    // The record stays in the ledger for the audit; no new one is taken.
    assert_eq!(st.tool_approvals.len(), 1);
    let e = l
        .tool_approval("dap-2", SEARCH, ToolVerdict::Allow, 5)
        .unwrap_err();
    assert!(e.contains("closed"), "{e}");
    assert_eq!(
        st.closed.unwrap().0,
        Gate::Escalate(tole_core::discussion::Reason::Stopped)
    );
}

#[test]
fn the_allowance_and_an_approval_fit_a_record_beside_the_largest_question() {
    // The contract record: the longest question in 4-byte characters and
    // every allowed tool at the longest name.
    let q = "\u{1F600}".repeat(MAX_QUESTION_CHARS);
    let tools: Vec<String> = (0..MAX_ALLOWED_TOOLS)
        .map(|i| format!("mcp_{i}{}", "x".repeat(MAX_TOOL_NAME_CHARS - 5)))
        .collect();
    let c = Contract::new(
        "t",
        &q,
        &strs(&["c"]),
        &strs(&["coder"]),
        1000,
        None,
        &SecretSet::default(),
    )
    .unwrap()
    .with_tools(&tools)
    .unwrap();
    let d = TestDir::new("tole-discuss-tools-cap");
    let l = DiscussionLog::at(&d, "t").unwrap();
    l.start(c, 1).unwrap();
    l.tool_approval("dap-1", &tools[0], ToolVerdict::Allow, 2)
        .unwrap();
    let longest = std::fs::read_to_string(d.join("t.jsonl"))
        .unwrap()
        .lines()
        .map(str::len)
        .max()
        .unwrap();
    assert!(longest < MAX_RECORD_BYTES / 8, "{longest}");
}
