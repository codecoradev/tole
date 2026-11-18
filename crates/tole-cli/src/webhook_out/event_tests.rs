use super::*;
use serde_json::Value;

fn keep(s: &str) -> String {
    s.to_string()
}

fn run<'a>(output: &'a str) -> CronRun<'a> {
    CronRun {
        job_id: "daily",
        job_name: Some("Daily"),
        fire_id: "daily@1795000000/42.7",
        status: RunStatus::Ok,
        scheduled_for: 1_795_000_000,
        finished_at: 1_795_000_030,
        output,
        session_id: Some("s-1"),
    }
}

fn parse(b: &Built) -> Value {
    serde_json::from_slice(&b.body).unwrap()
}

#[test]
fn a_cron_id_is_valid_stable_and_short() {
    let id = cron_event_id("daily@1795000000/42.7");
    assert_eq!(id, cron_event_id("daily@1795000000/42.7"));
    assert!(id.starts_with("cron:daily-1795000000-42.7:"), "{id}");
    assert!(tole_core::webhook::valid_delivery(&id), "{id}");
    // 5 + 64 + 1 + 16
    let long = cron_event_id(&"j".repeat(500));
    assert_eq!(long.len(), 86);
    assert!(tole_core::webhook::valid_delivery(&long));
}

#[test]
fn two_fire_ids_that_read_alike_get_different_ids() {
    // `@` and `/` and `_` all read as `-`/`_` once made readable; the hash
    // keeps them apart.
    let a = cron_event_id("a@1/2.3");
    let b = cron_event_id("a-1-2.3");
    assert_ne!(a, b);
    let c = cron_event_id(&format!("{}x", "k".repeat(64)));
    let d = cron_event_id(&format!("{}y", "k".repeat(64)));
    assert_ne!(c, d, "ids that differ only past the readable cut");
}

#[test]
fn non_ascii_fire_ids_stay_inside_the_charset() {
    let id = cron_event_id("jöb@é/1");
    assert!(tole_core::webhook::valid_delivery(&id), "{id}");
}

#[test]
fn approval_ids_are_checked_not_rewritten() {
    assert_eq!(
        approval_event_id("apr-1a-2b", false).as_deref(),
        Some("approval:apr-1a-2b:requested")
    );
    assert_eq!(
        approval_event_id("dap-9", true).as_deref(),
        Some("approval:dap-9:resolved")
    );
    assert_eq!(approval_event_id("a b", false), None);
    assert_eq!(approval_event_id("a/b", true), None);
    assert_eq!(approval_event_id(&"x".repeat(200), false), None);
}

#[test]
fn only_an_ok_run_can_be_silent() {
    assert!(cron_is_silent(RunStatus::Ok, ""));
    assert!(cron_is_silent(RunStatus::Ok, "  \n"));
    assert!(cron_is_silent(RunStatus::Ok, "[SILENT]\nnothing to report"));
    assert!(!cron_is_silent(RunStatus::Ok, "done"));
    for s in [
        RunStatus::Error,
        RunStatus::Budget,
        RunStatus::Cancelled,
        RunStatus::Interrupted,
        RunStatus::AwaitingApproval,
    ] {
        assert!(!cron_is_silent(s, ""), "{s:?} with no output notifies");
        assert!(!cron_is_silent(s, "[SILENT]"), "{s:?}");
    }
}

#[test]
fn the_cron_envelope_has_the_documented_shape() {
    let b = cron_run_finished("gateway", 1_795_000_031, &run("all good"), &keep).unwrap();
    assert_eq!(b.id, cron_event_id("daily@1795000000/42.7"));
    let v = parse(&b);
    assert_eq!(v["v"], 1);
    assert_eq!(v["id"], b.id.as_str());
    assert_eq!(v["type"], "cron_run_finished");
    assert_eq!(v["destination"], "gateway");
    assert_eq!(v["at"], 1_795_000_031);
    let d = &v["data"];
    assert_eq!(d["job"], "daily");
    assert_eq!(d["job_name"], "Daily");
    assert_eq!(d["fire_id"], "daily@1795000000/42.7");
    assert_eq!(d["status"], "ok");
    assert_eq!(d["scheduled_for"], 1_795_000_000);
    assert_eq!(d["finished_at"], 1_795_000_030);
    assert_eq!(d["summary"], "all good");
    assert_eq!(d["truncated"], false);
    assert_eq!(d["session_id"], "s-1");
}

#[test]
fn a_status_is_its_snake_case_name() {
    let mut r = run("x");
    r.status = RunStatus::AwaitingApproval;
    let b = cron_run_finished("g", 1, &r, &keep).unwrap();
    assert_eq!(parse(&b)["data"]["status"], "awaiting_approval");
}

#[test]
fn redaction_happens_before_the_cut() {
    // The secret straddles the 4 KiB cut: cutting first would leave a
    // fragment the redactor no longer recognises.
    let secret = "s3cr3t-token-value";
    let out = format!("{}{secret} tail", "a".repeat(MAX_CRON_SUMMARY - 5));
    let redact = |s: &str| s.replace(secret, "[REDACTED]");
    let b = cron_run_finished("g", 1, &run(&out), &redact).unwrap();
    let text = String::from_utf8(b.body.clone()).unwrap();
    assert!(!text.contains(secret));
    assert!(!text.contains("s3cr3t"), "no fragment of it either");
    let v = parse(&b);
    assert!(v["data"]["summary"].as_str().unwrap().len() <= MAX_CRON_SUMMARY);
}

#[test]
fn a_long_summary_is_cut_at_a_character_boundary() {
    // 3-byte characters: a byte cut at 4096 lands inside one.
    let out = "€".repeat(2000);
    let b = cron_run_finished("g", 1, &run(&out), &keep).unwrap();
    let v = parse(&b);
    let s = v["data"]["summary"].as_str().unwrap();
    assert!(s.len() <= MAX_CRON_SUMMARY);
    assert!(s.chars().all(|c| c == '€'));
    assert_eq!(v["data"]["truncated"], true);
}

#[test]
fn text_fields_are_capped_and_redacted() {
    let mut r = run("x");
    let name = "n".repeat(10_000);
    r.job_name = Some(&name);
    let redact = |s: &str| s.replace("daily", "[R]");
    let b = cron_run_finished("g", 1, &r, &redact).unwrap();
    let v = parse(&b);
    assert_eq!(
        v["data"]["job_name"].as_str().unwrap().len(),
        MAX_FIELD_CHARS
    );
    assert_eq!(v["data"]["job"], "[R]");
    assert!(b.body.len() <= MAX_BODY);
}

#[test]
fn an_envelope_never_exceeds_the_cap() {
    // Every field at its limit still fits, and a body is always valid JSON.
    let name = "é".repeat(5000);
    let out = "b".repeat(100_000);
    let mut r = run(&out);
    r.job_name = Some(&name);
    let b = cron_run_finished("g", 1, &r, &keep).unwrap();
    assert!(b.body.len() <= MAX_BODY, "{}", b.body.len());
    parse(&b);
}

#[test]
fn the_summary_is_dropped_when_the_envelope_is_still_too_big() {
    let big = json!({
        "v": 1, "id": "x", "type": "cron_run_finished", "destination": "g", "at": 1,
        "data": {"summary": "s".repeat(20_000), "truncated": false}
    });
    let bytes = fit(big).unwrap();
    assert!(bytes.len() <= MAX_BODY);
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["data"]["summary"], "");
    assert_eq!(v["data"]["truncated"], true);

    let hopeless = json!({"v": 1, "data": {"other": "o".repeat(20_000)}});
    assert_eq!(fit(hopeless), Err(BuildError::TooLarge));
}

#[test]
fn a_failure_with_no_output_still_builds() {
    let mut r = run("");
    r.status = RunStatus::Error;
    r.session_id = None;
    let v = parse(&cron_run_finished("g", 1, &r, &keep).unwrap());
    assert_eq!(v["data"]["status"], "error");
    assert_eq!(v["data"]["summary"], "");
    assert!(v["data"]["session_id"].is_null());
}

fn approval<'a>(desc: &'a str) -> ApprovalRequest<'a> {
    ApprovalRequest {
        approval_id: "apr-1a-2b",
        session_id: "s-1",
        tool: "write_file",
        description: desc,
        created_at: 100,
        expires_at: 1000,
    }
}

#[test]
fn an_approval_request_has_the_documented_shape() {
    let b = approval_requested("gateway", 101, &approval("write notes.md"), &keep).unwrap();
    assert_eq!(b.id, "approval:apr-1a-2b:requested");
    let v = parse(&b);
    assert_eq!(v["type"], "approval_requested");
    assert_eq!(v["destination"], "gateway");
    let d = &v["data"];
    assert_eq!(d["source"], "serve");
    assert_eq!(d["approval_id"], "apr-1a-2b");
    assert_eq!(d["session_id"], "s-1");
    assert_eq!(d["tool"], "write_file");
    assert_eq!(d["risk"], "write");
    assert_eq!(d["summary"], "write notes.md");
    assert_eq!(d["created_at"], 100);
    assert_eq!(d["expires_at"], 1000);
}

#[test]
fn an_approval_summary_is_redacted_then_cut_to_512_bytes() {
    let secret = "tok-abcdef-123456";
    let desc = format!("{}{secret}", "d".repeat(MAX_APPROVAL_SUMMARY - 4));
    let redact = |s: &str| s.replace(secret, "[R]");
    let v = parse(&approval_requested("g", 1, &approval(&desc), &redact).unwrap());
    let s = v["data"]["summary"].as_str().unwrap();
    assert!(!s.contains("tok-"), "{s}");
    assert!(s.len() <= MAX_APPROVAL_SUMMARY);

    let long = "ü".repeat(1000);
    let v = parse(&approval_requested("g", 1, &approval(&long), &keep).unwrap());
    assert!(v["data"]["summary"].as_str().unwrap().len() <= MAX_APPROVAL_SUMMARY);
    assert_eq!(v["data"]["truncated"], true);
}

#[test]
fn a_bad_approval_id_is_refused() {
    let mut a = approval("x");
    a.approval_id = "apr 1";
    assert_eq!(
        approval_requested("g", 1, &a, &keep),
        Err(BuildError::BadId)
    );
    let o = ApprovalOutcome {
        approval_id: "a/b",
        session_id: "s",
        tool: "t",
        resolution: Resolution::Denied,
        resolved_at: 1,
    };
    assert_eq!(approval_resolved("g", 1, &o, &keep), Err(BuildError::BadId));
}

#[test]
fn a_resolution_carries_its_status_and_no_identity() {
    for (r, name) in [
        (Resolution::Approved, "approved"),
        (Resolution::Denied, "denied"),
        (Resolution::Expired, "expired"),
    ] {
        let o = ApprovalOutcome {
            approval_id: "apr-1a-2b",
            session_id: "s-1",
            tool: "write_file",
            resolution: r,
            resolved_at: 500,
        };
        let b = approval_resolved("gateway", 501, &o, &keep).unwrap();
        assert_eq!(b.id, "approval:apr-1a-2b:resolved");
        let v = parse(&b);
        assert_eq!(v["type"], "approval_resolved");
        assert_eq!(v["data"]["status"], name);
        assert_eq!(v["data"]["resolved_at"], 500);
        assert!(v["data"].get("decided_by").is_none());
    }
}

#[test]
fn the_same_input_builds_the_same_bytes() {
    let a = cron_run_finished("g", 7, &run("out"), &keep).unwrap();
    let b = cron_run_finished("g", 7, &run("out"), &keep).unwrap();
    assert_eq!(a, b, "the body is fixed at enqueue time");
}

#[test]
fn the_destination_is_inside_the_signed_body() {
    let a = cron_run_finished("one", 7, &run("out"), &keep).unwrap();
    let b = cron_run_finished("two", 7, &run("out"), &keep).unwrap();
    assert_eq!(a.id, b.id, "the same event id for every destination");
    assert_ne!(a.body, b.body);
}
