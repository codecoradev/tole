use serde_json::Value;
use tole_cli::webhook_out::event::{turn_event_id, turn_finished, BuildError};
use tole_cli::webhook_out::turn::*;
use tole_cli::webhook_out::Event;
use tole_core::storage::StorageError;
use tole_core::turn::TurnOutcome;

fn keep(s: &str) -> String {
    s.to_string()
}

/// Every `TurnOutcome` variant, each with payload text a leak would show.
fn outcomes(marker: &str) -> Vec<TurnOutcome> {
    vec![
        TurnOutcome::Final {
            text: marker.into(),
            wrote: true,
        },
        TurnOutcome::BudgetExhausted,
        TurnOutcome::LoopDetected {
            tool: marker.into(),
            count: 3,
        },
        TurnOutcome::UnknownTool {
            name: marker.into(),
        },
        TurnOutcome::ApprovalRequired {
            name: marker.into(),
        },
        TurnOutcome::ProviderFailed {
            message: marker.into(),
        },
        TurnOutcome::StopGateBlocked {
            reason: marker.into(),
        },
        TurnOutcome::Storage(StorageError::Corrupt(marker.into())),
        TurnOutcome::Cancelled,
        TurnOutcome::Detached,
    ]
}

/// A marker built at run time, so no secret-shaped literal sits in the source.
fn marker() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("leak{:x}{:x}", std::process::id(), n)
}

#[test]
fn every_outcome_maps_to_the_provisional_words() {
    let want = [
        ("completed", None),
        ("budget_exhausted", None),
        ("blocked", Some("loop")),
        ("blocked", Some("unknown_tool")),
        ("approval_pending", None),
        ("failed", Some("provider")),
        ("blocked", Some("stop_gate")),
        ("failed", Some("storage")),
        ("cancelled", None),
        // D4: detached is folded into cancelled.
        ("cancelled", None),
    ];
    let outs = outcomes("x");
    assert_eq!(outs.len(), want.len());
    for (o, (status, code)) in outs.iter().zip(want) {
        let end = TurnEnd::from_outcome(o, 14).expect("announced");
        assert_eq!(end.status.as_str(), status, "{o:?}");
        assert_eq!(end.code.map(TurnCode::as_str), code, "{o:?}");
        assert_eq!(end.last_seq, 14);
    }
}

#[test]
fn the_provisional_defaults_are_the_note_recommendations() {
    // D3, D4 and D8 are booleans the mapping and queue tests above and in
    // `volatile_tests.rs` exercise (flipping one fails them on purpose).
    assert_eq!(RETRY_HORIZON_SECS, 900, "D9");
    assert_eq!(RETRY_BACKOFF_SECS, [2, 10, 30, 120], "D1");
    assert_eq!(RETRY_TAIL_SECS, 300, "D1");
}

#[test]
fn the_vocabulary_is_closed() {
    let statuses = [
        TurnStatus::Completed,
        TurnStatus::Failed,
        TurnStatus::Cancelled,
        TurnStatus::Detached,
        TurnStatus::BudgetExhausted,
        TurnStatus::Blocked,
        TurnStatus::ApprovalPending,
    ]
    .map(TurnStatus::as_str);
    assert_eq!(
        statuses,
        [
            "completed",
            "failed",
            "cancelled",
            "detached",
            "budget_exhausted",
            "blocked",
            "approval_pending"
        ]
    );
    let codes = [
        TurnCode::Provider,
        TurnCode::Storage,
        TurnCode::Loop,
        TurnCode::UnknownTool,
        TurnCode::StopGate,
        TurnCode::Internal,
        TurnCode::Approval,
    ]
    .map(TurnCode::as_str);
    assert_eq!(
        codes,
        [
            "provider",
            "storage",
            "loop",
            "unknown_tool",
            "stop_gate",
            "internal",
            "approval"
        ]
    );
}

#[test]
fn an_approval_stop_needs_a_pending_entry() {
    let end =
        TurnEnd::from_outcome(&TurnOutcome::ApprovalRequired { name: "t".into() }, 9).unwrap();
    let (e, id) = end.with_approval(Some("ap-1".into()));
    assert_eq!(
        (e.status, id.as_deref()),
        (TurnStatus::ApprovalPending, Some("ap-1"))
    );
    // Nothing queued (a denied or hook-refused call): blocked, code approval.
    let (e, id) = end.with_approval(None);
    assert_eq!(
        (e.status, e.code, id),
        (TurnStatus::Blocked, Some(TurnCode::Approval), None)
    );
    // Any other status never carries an approval id.
    let done = TurnEnd::from_outcome(&TurnOutcome::Cancelled, 1).unwrap();
    assert_eq!(done.with_approval(Some("ap-1".into())), (done, None));
}

#[test]
fn a_failed_end_carries_a_code_and_no_text() {
    let e = TurnEnd::failed(TurnCode::Internal, 3);
    assert_eq!(
        (e.status, e.code, e.last_seq),
        (TurnStatus::Failed, Some(TurnCode::Internal), 3)
    );
}

fn body(end: &TurnEnd, approval: Option<&str>) -> Value {
    let b = turn_finished("gateway", 1_795_000_123, "s-1f2e3d", end, approval, &keep).unwrap();
    assert_eq!(b.id, turn_event_id("s-1f2e3d", end.last_seq, end.settle));
    serde_json::from_slice(&b.body).unwrap()
}

/// The exact key set per status: a new key fails this test.
#[test]
fn the_body_has_exactly_the_allowed_keys_and_closed_values() {
    let m = marker();
    for o in outcomes(&m) {
        let end = TurnEnd::from_outcome(&o, 14).unwrap();
        let (end, approval) = end.with_approval(Some("ap-9".into()));
        let v = body(&end, approval.as_deref());
        let mut top: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        top.sort();
        assert_eq!(top, ["at", "data", "destination", "id", "type", "v"]);
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "turn_finished");
        let data = v["data"].as_object().unwrap();
        let mut keys: Vec<&str> = data.keys().map(String::as_str).collect();
        keys.sort();
        let mut want = vec!["last_seq", "session_id", "status"];
        if end.status == TurnStatus::ApprovalPending {
            want.push("approval_id");
        }
        if end.code.is_some() {
            want.push("code");
        }
        want.sort();
        assert_eq!(keys, want, "{o:?}");
        assert_eq!(data["last_seq"], 14);
        assert_eq!(data["session_id"], "s-1f2e3d");
        // Every string value is an id or a closed word; the payload text of the
        // outcome appears nowhere.
        let text = v.to_string();
        assert!(!text.contains(&m), "{text}");
        for (k, val) in data {
            if let Some(s) = val.as_str() {
                let closed = match k.as_str() {
                    "status" => [
                        "completed",
                        "failed",
                        "cancelled",
                        "detached",
                        "budget_exhausted",
                        "blocked",
                        "approval_pending",
                    ]
                    .contains(&s),
                    "code" => [
                        "provider",
                        "storage",
                        "loop",
                        "unknown_tool",
                        "stop_gate",
                        "internal",
                        "approval",
                    ]
                    .contains(&s),
                    _ => tole_core::webhook::valid_delivery(s),
                };
                assert!(closed, "{k}={s}");
            }
        }
    }
}

#[test]
fn the_largest_body_is_far_below_the_cap() {
    let session = "s".repeat(64);
    let approval = "a".repeat(128);
    let end = TurnEnd {
        status: TurnStatus::ApprovalPending,
        code: Some(TurnCode::UnknownTool),
        last_seq: u64::MAX,
        settle: u64::MAX,
    };
    let dest = "d".repeat(32);
    let b = turn_finished(&dest, i64::MAX, &session, &end, Some(&approval), &keep).unwrap();
    assert!(b.body.len() <= MAX_TURN_BODY, "{}", b.body.len());
    assert!(b.id.len() <= 128 && tole_core::webhook::valid_delivery(&b.id));
}

#[test]
fn the_id_is_valid_deterministic_and_changes_with_the_log_position() {
    let a = turn_event_id("s-1f2e3d", 14, 3);
    assert_eq!(a, turn_event_id("s-1f2e3d", 14, 3));
    assert!(
        a.starts_with("turn:s-1f2e3d:") && a.ends_with(":14:3"),
        "{a}"
    );
    assert_ne!(a, turn_event_id("s-1f2e3d", 15, 3), "a resumed leg differs");
    assert_ne!(turn_event_id("a_b", 1, 1), turn_event_id("a-b", 1, 1));
    let long = turn_event_id(&"j".repeat(64), u64::MAX, u64::MAX);
    assert!(
        long.len() <= 128 && tole_core::webhook::valid_delivery(&long),
        "{}",
        long.len()
    );
}

/// D10: a turn that fails before it appends anything settles at the same
/// `last_seq` as the turn before it; its id must still differ, and a retry of
/// one settle (the same built event) must keep its id.
#[test]
fn a_failed_before_append_turn_gets_a_different_id_and_a_retry_keeps_its_own() {
    let done = TurnEnd::from_outcome(
        &TurnOutcome::Final {
            text: String::new(),
            wrote: false,
        },
        14,
    )
    .unwrap()
    .with_settle(1);
    let failed = TurnEnd::failed(TurnCode::Provider, 14).with_settle(2);
    assert_eq!(done.last_seq, failed.last_seq, "nothing was appended");
    let a = turn_finished("g", 1, "s-1", &done, None, &keep).unwrap();
    let b = turn_finished("g", 1, "s-1", &failed, None, &keep).unwrap();
    assert_ne!(
        a.id, b.id,
        "a receiver deduping on the id must see two events"
    );
    // The same settle built again (a different `at`, as a retry would not be,
    // but the id never depends on it) has the same id; a stored body is resent
    // as is, so the id is unchanged across retries.
    let again = turn_finished("g", 99, "s-1", &failed, None, &keep).unwrap();
    assert_eq!(b.id, again.id);
}

#[test]
fn a_bad_or_secret_shaped_id_is_refused_not_sent() {
    let end = TurnEnd::from_outcome(&TurnOutcome::Cancelled, 1).unwrap();
    for bad in ["", "a/b", "a b", "..", &"x".repeat(65)] {
        assert_eq!(
            turn_finished("g", 1, bad, &end, None, &keep).unwrap_err(),
            BuildError::BadId,
            "{bad:?}"
        );
    }
    let pending =
        TurnEnd::from_outcome(&TurnOutcome::ApprovalRequired { name: "t".into() }, 1).unwrap();
    assert_eq!(
        turn_finished("g", 1, "s-1", &pending, Some("a b"), &keep).unwrap_err(),
        BuildError::BadId
    );
    // An id the redactor would change is never sent as a stand-in.
    let m = marker();
    let hide = |s: &str| s.replace(&m, "«X»");
    assert_eq!(
        turn_finished("g", 1, &m, &end, None, &hide).unwrap_err(),
        BuildError::BadId
    );
    assert!(turn_finished("g", 1, "s-1", &end, None, &hide).is_ok());
}

#[test]
fn the_event_is_a_known_name_and_not_a_default() {
    use tole_cli::config::WebhookOutConfig;
    assert_eq!(Event::parse("turn_finished"), Some(Event::TurnFinished));
    assert!(Event::ALL.contains(&Event::TurnFinished) && Event::ALL.len() == 4);
    assert!(tole_core::webhook::valid_event("turn_finished"));
    let env = |k: &str| (k == "K").then(|| "0".repeat(32));
    let mut cfg = WebhookOutConfig {
        url: Some("https://hooks.example.com/tole".into()),
        hmac_key_env: Some("K".into()),
        ..WebhookOutConfig::default()
    };
    // An existing destination (no `events`) does not start receiving it.
    let d = tole_cli::webhook_out::load(&[("g".to_string(), cfg.clone())].into(), &env).unwrap();
    assert!(!d.any_lists(&[Event::TurnFinished]));
    // Listing the cron event and approvals alone does not either.
    cfg.events = Some(vec![
        "cron_run_finished".into(),
        "approval_requested".into(),
    ]);
    let d = tole_cli::webhook_out::load(&[("g".to_string(), cfg.clone())].into(), &env).unwrap();
    assert!(!d.any_lists(&[Event::TurnFinished]));
    cfg.events = Some(vec!["turn_finished".into()]);
    let d = tole_cli::webhook_out::load(&[("g".to_string(), cfg)].into(), &env).unwrap();
    assert!(d.any_lists(&[Event::TurnFinished]));
}

/// A second pinned signing vector (the first is `sign_tests.rs`), for a
/// `turn_finished` body, computed once with an independent HMAC-SHA256 (Python
/// `hmac`) over the exact bytes below, not with this code.
#[test]
fn a_turn_finished_body_has_a_pinned_signature() {
    let end = TurnEnd::from_outcome(&TurnOutcome::Cancelled, 14)
        .unwrap()
        .with_settle(1);
    let end = TurnEnd {
        status: TurnStatus::Completed,
        ..end
    };
    let b = turn_finished("gateway", 1_795_000_000, "s-1f2e3d", &end, None, &keep).unwrap();
    let text = String::from_utf8(b.body.clone()).unwrap();
    assert_eq!(
        text,
        r#"{"at":1795000000,"data":{"last_seq":14,"session_id":"s-1f2e3d","status":"completed"},"destination":"gateway","id":"turn:s-1f2e3d:357319c7c674b505:14:1","type":"turn_finished","v":1}"#
    );
    let h = tole_cli::webhook_out::sign::request_headers(
        b"tole-test-key-0123456789abcdef",
        1_795_000_000,
        &b.id,
        "turn_finished",
        &b.body,
    );
    let sig = h.iter().find(|(k, _)| k == "X-Tole-Signature").unwrap();
    assert_eq!(sig.1, PINNED_SIGNATURE);
}

const PINNED_SIGNATURE: &str =
    "v2=40b6e20fcf55ad9521b43149ff2314ff1c852b65d8e1463d3b1d23c84270c168";
