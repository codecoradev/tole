//! #1023: the typed `problem` of a prompt answer decodes, keeps a kind it does
//! not know, and serializes back to the JSON the server sent.

use serde_json::json;
use tole_client::{Problem, PromptReply};

#[test]
fn a_prompt_answer_with_a_problem_decodes_and_round_trips() {
    let wire = json!({"stopReason": "refusal", "problem": {
        "kind": "provider", "subject": null, "retryable": true, "detail": "slow down"}});
    let reply: PromptReply = serde_json::from_value(wire.clone()).unwrap();
    let want = Problem {
        kind: "provider".into(),
        subject: None,
        retryable: true,
        detail: Some("slow down".into()),
    };
    assert_eq!(reply.problem, Some(want));
    assert_eq!(serde_json::to_value(&reply).unwrap(), wire);
}

#[test]
fn a_kind_added_later_still_decodes_and_an_old_server_has_no_problem() {
    let wire = json!({"stopReason": "refusal", "problem": {
        "kind": "rate_limited", "subject": "web_search", "retryable": true}});
    let reply: PromptReply = serde_json::from_value(wire.clone()).unwrap();
    let p = reply.problem.as_ref().expect("a problem");
    assert_eq!(
        (p.kind.as_str(), p.detail.as_deref()),
        ("rate_limited", None)
    );
    assert_eq!(serde_json::to_value(&reply).unwrap(), wire);

    let old: PromptReply = serde_json::from_value(json!({"stopReason": "end_turn"})).unwrap();
    assert_eq!(old.problem, None);
}
