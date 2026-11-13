use super::*;
use tole_core::discussion::{parse_digest, Contract, CriterionVerdict};
use tole_core::discussion_log::Reserve;

const SECRET: &str = "sk-audit-disc-0123456789abcdef";

fn secrets() -> SecretSet {
    SecretSet::collect([("AUDIT_API_KEY".to_string(), SECRET.to_string())], [])
}

fn tmp(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("tole-auditdisc-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Round 1: two speakers, a digest (a claim quoting the secret), a met
/// checklist and a report; the raw turns carry a marker that must never be
/// shown.
fn discussion(dir: &Path) {
    let l = DiscussionLog::at(dir, "pick-db").unwrap();
    let question = format!("Which database? key {SECRET} {}", "q".repeat(2000));
    let c = Contract::new(
        "pick-db",
        &question,
        &["a choice is named".to_string()],
        &["coder".to_string(), "ops".to_string()],
        10_000,
        None,
        // No secrets known when it was written: the audit must redact.
        &SecretSet::default(),
    )
    .unwrap();
    l.start(c, 1).unwrap();
    let Reserve::Go(call) = l.reserve(1, "participant-coder", 500, 2).unwrap() else {
        panic!()
    };
    l.settle(&call, 120, 3).unwrap();
    let raw = format!("RAW-TURN {}", "x".repeat(1300));
    let a = l.contribute(1, "coder", &raw, 4).unwrap();
    let b = l.contribute(1, "ops", &raw, 5).unwrap();
    let digest = serde_json::json!({
        "claims": [
            {"text": format!("use Postgres, token {SECRET}"), "by": "coder", "entry": a.entry},
            {"text": "SQLite is enough", "by": "ops", "entry": b.entry},
        ],
        // A secret across the display cap: redacted before the cut, so no
        // prefix of it survives.
        "delta": ["a first position each", format!("{}{SECRET}", "d".repeat(285))],
    })
    .to_string();
    let d = parse_digest(&digest, 1, &[a.clone(), b], &SecretSet::default()).unwrap();
    l.summarize(RoundSummary::Digest(d), 6).unwrap();
    l.check(
        1,
        vec![CriterionVerdict {
            met: true,
            evidence: vec![a.entry],
        }],
        7,
    )
    .unwrap();
    l.decide(1, 8).unwrap();
    l.close(&format!("picked Postgres {SECRET}"), 9).unwrap();
}

#[test]
fn a_discussion_shows_its_goal_rounds_digests_gate_and_spend_redacted() {
    let dir = tmp("one");
    discussion(&dir);
    let log = dir.join("pick-db.jsonl");
    let before = std::fs::read(&log).unwrap();
    // The writer's lock goes; the audit must not take a new one.
    std::fs::remove_file(dir.join("pick-db.jsonl.lock")).unwrap();
    let text = discussions(&dir, Some("pick-db"), false, &secrets()).unwrap();
    for want in [
        "pick-db closed (report)",
        "criterion 1: a choice is named",
        "participants: coder, ops; tokens used 120 of 10000",
        "round 1: 120 tokens, gate report",
        "claims: SQLite is enough (ops, e",
        "delta: a first position each",
        "checklist: 1 of 1 met",
        "outcome: report (the acceptance criteria are met)",
        "«AUDIT_API_KEY»",
    ] {
        assert!(text.contains(want), "{want}:\n{text}");
    }
    assert!(!text.contains(&SECRET[..8]), "{text}");
    assert!(
        !text.contains("RAW-TURN"),
        "never a raw contribution:\n{text}"
    );
    let q = text.lines().find(|l| l.contains("question:")).unwrap();
    assert!(q.chars().count() < LINE_CHARS + 20, "capped: {q}");
    let json = discussions(&dir, Some("pick-db"), true, &secrets()).unwrap();
    assert!(!json.contains(SECRET) && !json.contains("RAW-TURN"));
    let v: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["rounds"][0]["summary"]["kind"], "digest");
    // Read-only: the log is unchanged and no lock was taken.
    assert_eq!(std::fs::read(&log).unwrap(), before);
    assert!(!dir.join("pick-db.jsonl.lock").exists());
}

#[test]
fn the_list_names_each_discussion_and_skips_what_is_not_one() {
    let dir = tmp("list");
    discussion(&dir);
    std::fs::write(dir.join("notes.txt"), "x").unwrap();
    std::fs::write(dir.join("Bad Name.jsonl"), "x").unwrap();
    let text = discussions(&dir, None, false, &secrets()).unwrap();
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.starts_with("pick-db closed (report) 1 rounds, tokens 120 of 10000: Which database? key «AUDIT_API_KEY»"), "{text}");
    let none = discussions(&dir.join("missing"), None, false, &secrets()).unwrap();
    assert!(none.starts_with("no discussions"), "{none}");
    let e = discussions(&dir, Some("other"), false, &secrets()).unwrap_err();
    assert!(e.contains("no discussion `other`"), "{e}");
    assert!(discussions(&dir, Some("../x"), false, &secrets()).is_err());
}

#[cfg(unix)]
#[test]
fn a_symlinked_log_is_not_read() {
    let dir = tmp("link");
    let elsewhere = tmp("link-target");
    discussion(&elsewhere);
    std::os::unix::fs::symlink(elsewhere.join("pick-db.jsonl"), dir.join("pick-db.jsonl")).unwrap();
    assert!(discussions(&dir, Some("pick-db"), false, &secrets()).is_err());
    // The listing skips it (not a regular file).
    let text = discussions(&dir, None, false, &secrets()).unwrap();
    assert!(text.starts_with("no discussions"), "{text}");
}
