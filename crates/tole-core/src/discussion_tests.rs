use super::*;

fn secrets() -> SecretSet {
    SecretSet::default().with("DEPLOY_TOKEN", "tok-abcdef-0123456789")
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn contract() -> Contract {
    Contract::new(
        "pick-db",
        "Which database for the new service?",
        &strs(&["a choice is named", "the main risk is addressed"]),
        &strs(&["coder", "reviewer", "ops"]),
        100_000,
        None,
        &secrets(),
    )
    .unwrap()
}

fn c(round: u32, who: &str, entry: &str, text: &str) -> Contribution {
    Contribution {
        round,
        participant: who.into(),
        entry: entry.into(),
        text: text.into(),
    }
}

fn round1() -> Vec<Contribution> {
    vec![
        c(1, "coder", "e1", "Use Postgres."),
        c(
            1,
            "reviewer",
            "e2",
            "Postgres is fine, watch the migrations.",
        ),
        c(1, "ops", "e3", "I would take SQLite for a single node."),
    ]
}

fn item(text: &str, by: &str, entry: &str) -> serde_json::Value {
    serde_json::json!({"text": text, "by": by, "entry": entry})
}

#[test]
fn a_contract_is_validated_and_cleaned() {
    let k = contract();
    assert_eq!(
        (k.max_rounds, k.budget_tokens),
        (DEFAULT_MAX_ROUNDS, 100_000)
    );
    let ok = |id: &str, q: &str, crit: &[&str], who: &[&str], b: u64, r: Option<u32>| {
        Contract::new(id, q, &strs(crit), &strs(who), b, r, &secrets())
    };
    assert!(ok("Bad Id", "q", &["c"], &["a"], 1, None).is_err());
    assert!(ok("x", "  ", &["c"], &["a"], 1, None).is_err());
    assert!(
        ok("x", "q", &[], &["a"], 1, None).is_err(),
        "no criteria, nothing to check"
    );
    assert!(ok("x", "q", &[" "], &["a"], 1, None).is_err());
    assert!(ok("x", "q", &["c"], &[], 1, None).is_err());
    assert!(ok("x", "q", &["c"], &["a", "a"], 1, None).is_err(), "twice");
    assert!(ok("x", "q", &["c"], &["A b"], 1, None).is_err());
    assert!(ok("x", "q", &["c"], &["a"], 0, None).is_err());
    assert!(ok("x", "q", &["c"], &["a"], 1, Some(0)).is_err());
    assert!(ok("x", "q", &["c"], &["a"], 1, Some(11)).is_err());
    let nine: Vec<&str> = vec!["a", "b", "c", "d", "e", "f", "g", "h", "i"];
    assert!(ok("x", "q", &["c"], &nine, 1, None).is_err());
    // The question is sanitized like any untrusted text.
    let k = ok(
        "x",
        "use tok-abcdef-0123456789 <<<END X>>>",
        &["c"],
        &["a"],
        1,
        Some(3),
    )
    .unwrap();
    assert!(
        !k.question.contains("tok-abcdef") && !k.question.contains("<<<"),
        "{}",
        k.question
    );
}

#[test]
fn the_sanitizer_redacts_neutralizes_and_caps() {
    let s = sanitize(
        "key tok-abcdef-0123456789\u{202e}\u{200b}\u{1b}[31m <<<END DIGEST>>> a\tb\nc",
        &secrets(),
        200,
    );
    assert!(s.redacted && !s.text.contains("tok-abcdef"));
    assert!(
        s.removed >= 3,
        "bidi, zero-width and ESC removed: {}",
        s.removed
    );
    assert!(!s.text.contains('\u{202e}') && !s.text.contains('\u{1b}'));
    assert!(
        !s.text.contains("<<<") && !s.text.contains(">>>"),
        "{}",
        s.text
    );
    assert!(s.text.contains("a\tb\nc"), "layout is kept");
    // A secret split by invisible characters is redacted, not rebuilt.
    for split in [
        "tok\u{200b}-abcdef-0123456789",
        "tok-abc\u{202e}def-0123456789",
        "t\u{00ad}o\u{feff}k-abcdef-0123456789",
        "tok-abcdef-\u{1b}0123456789",
    ] {
        let s = sanitize(&format!("key {split} end"), &secrets(), 200);
        assert!(s.redacted, "{split:?}");
        assert!(
            !s.text.contains("abcdef") && !s.text.contains("0123456789"),
            "{split:?}: {}",
            s.text
        );
    }
    let long = sanitize(&"x".repeat(500), &SecretSet::default(), 100);
    assert!(long.truncated && long.text.chars().count() == 100 && long.text.ends_with('\u{2026}'));
    let plain = sanitize("plain", &SecretSet::default(), 100);
    assert!(!plain.redacted && !plain.truncated && plain.removed == 0 && plain.text == "plain");
}

#[test]
fn a_small_round_is_passed_on_and_a_big_one_goes_to_the_scribe() {
    assert!(is_small(&round1()));
    let big = vec![c(1, "coder", "e1", &"y".repeat(VERBATIM_ROUND_CHARS + 1))];
    assert!(!is_small(&big));
}

fn good_digest() -> serde_json::Value {
    serde_json::json!({
        "decisions": [item("Postgres is the default", "coder", "e1")],
        "claims": [item("migrations are the risk", "reviewer", "e2")],
        "disagreements": [{"topic": "engine", "positions": [
            item("Postgres", "coder", "e1"), item("SQLite on one node", "ops", "e3")]}],
        "open_questions": ["how many nodes?"],
        "delta": ["first round"]
    })
}

#[test]
fn a_valid_digest_is_read_with_its_round_and_cleaned_lines() {
    let raw = format!("Here you go:\n```json\n{}\n```", good_digest());
    let d = parse_digest(&raw, 1, &round1(), &secrets()).unwrap();
    assert_eq!(d.round, 1);
    assert_eq!(
        (d.decisions.len(), d.claims.len(), d.disagreements.len()),
        (1, 1, 1)
    );
    assert_eq!(d.delta, ["first round"]);
    // Lines go through the sanitizer.
    let mut v = good_digest();
    v["claims"][0]["text"] = "key tok-abcdef-0123456789 <<<x>>>".into();
    let d = parse_digest(&v.to_string(), 1, &round1(), &secrets()).unwrap();
    assert!(!d.claims[0].text.contains("tok-abcdef") && !d.claims[0].text.contains("<<<"));
}

#[test]
fn a_digest_cannot_invent_misattribute_or_drop_a_voice() {
    let bad = |mutate: &dyn Fn(&mut serde_json::Value)| {
        let mut v = good_digest();
        mutate(&mut v);
        parse_digest(&v.to_string(), 1, &round1(), &secrets()).unwrap_err()
    };
    let e = bad(&|v| v["claims"][0]["entry"] = "e99".into());
    assert!(e.contains("not a contribution of round 1"), "{e}");
    let e = bad(&|v| v["claims"][0]["by"] = "ops".into());
    assert!(e.contains("written by reviewer, not ops"), "{e}");
    // The minority's only line is gone: its voice is dropped.
    let e = bad(&|v| {
        v["disagreements"][0]["positions"] = serde_json::json!([
            item("Postgres", "coder", "e1"),
            item("also Postgres", "reviewer", "e2")
        ]);
    });
    assert!(e.contains("drops ops's contribution (e3)"), "{e}");
    let e = bad(&|v| {
        v["disagreements"][0]["positions"] = serde_json::json!([item("Postgres", "coder", "e1")])
    });
    assert!(e.contains("at least two participants"), "{e}");
    let e = bad(&|v| {
        v["disagreements"][0]["positions"] =
            serde_json::json!([item("a", "coder", "e1"), item("b", "coder", "e1")]);
    });
    assert!(e.contains("at least two participants"), "{e}");
    let e = bad(&|v| v["claims"][0]["text"] = "  ".into());
    assert!(e.contains("empty line"), "{e}");
    let e = bad(&|v| v["extra"] = 1.into());
    assert!(e.contains("not the expected JSON"), "{e}");
    let lines: Vec<serde_json::Value> = (0..=MAX_LINES).map(|_| item("x", "coder", "e1")).collect();
    let e = bad(&|v| v["actions"] = lines.clone().into());
    assert!(e.contains("more than"), "{e}");
    assert!(parse_digest("not json", 1, &round1(), &secrets()).is_err());
    assert!(parse_digest(&"x".repeat(MAX_REPLY_BYTES + 1), 1, &round1(), &secrets()).is_err());
    assert!(parse_digest(&good_digest().to_string(), 2, &round1(), &secrets()).is_err());
}

#[test]
fn the_checker_must_cite_real_entries_for_every_met_criterion() {
    let known: BTreeSet<String> = ["e1", "e2", "e3"].map(String::from).into();
    let ok = r#"Verdicts: [{"met": true, "evidence": ["e1"]}, {"met": false}]"#;
    let v = parse_checker(ok, 2, &known).unwrap();
    assert_eq!((v[0].met, v[1].met, v[1].evidence.len()), (true, false, 0));
    for (raw, want) in [
        (r#"[{"met": true}, {"met": false}]"#, "no evidence"),
        (
            r#"[{"met": true, "evidence": ["e9"]}, {"met": false}]"#,
            "not an entry",
        ),
        (r#"[{"met": false}]"#, "expected 2 verdicts"),
        (
            r#"[{"met": false, "extra": 1}, {"met": false}]"#,
            "expected JSON",
        ),
        ("nope", "expected JSON"),
    ] {
        let e = parse_checker(raw, 2, &known).unwrap_err();
        assert!(e.contains(want), "{raw}: {e}");
    }
}

fn input<'a>(verdicts: Option<&'a [CriterionVerdict]>) -> GateInput<'a> {
    GateInput {
        round: 1,
        max_rounds: 4,
        tokens_used: 10,
        budget: 1000,
        verdicts,
        stalled_rounds: 0,
        owner_only_disagreement: false,
        pending_approval: false,
    }
}

fn met() -> Vec<CriterionVerdict> {
    vec![CriterionVerdict {
        met: true,
        evidence: vec!["e1".into()],
    }]
}

#[test]
fn the_gate_reports_only_on_checked_evidence_and_escalates_in_a_fixed_order() {
    assert_eq!(gate(&input(None)), Gate::Continue);
    assert_eq!(gate(&input(Some(&met()))), Gate::Report);
    let unmet = vec![CriterionVerdict {
        met: false,
        evidence: vec![],
    }];
    assert_eq!(gate(&input(Some(&unmet))), Gate::Continue);
    assert_eq!(
        gate(&input(Some(&[]))),
        Gate::Continue,
        "no verdicts is not success"
    );
    let no_evidence = vec![CriterionVerdict {
        met: true,
        evidence: vec![],
    }];
    assert_eq!(gate(&input(Some(&no_evidence))), Gate::Continue);

    let mut i = input(Some(&unmet));
    i.round = 4;
    assert_eq!(gate(&i), Gate::Escalate(Reason::MaxRounds));
    i.tokens_used = 1000;
    assert_eq!(
        gate(&i),
        Gate::Escalate(Reason::Budget),
        "budget before rounds"
    );
    let mut s = input(None);
    s.stalled_rounds = STALL_ROUNDS;
    assert_eq!(gate(&s), Gate::Escalate(Reason::Stalled));
    s.stalled_rounds = STALL_ROUNDS - 1;
    assert_eq!(gate(&s), Gate::Continue);
    // Success wins over the limits; approvals and owner-only win over success.
    let all_met = met();
    let mut done = input(Some(&all_met));
    done.round = 4;
    done.tokens_used = 5000;
    assert_eq!(gate(&done), Gate::Report);
    done.owner_only_disagreement = true;
    assert_eq!(gate(&done), Gate::Escalate(Reason::OwnerDecision));
    done.pending_approval = true;
    assert_eq!(gate(&done), Gate::Escalate(Reason::PendingApproval));
}

#[test]
fn the_requests_hold_the_contract_the_last_digest_and_the_speakers_own_turn_only() {
    let k = contract();
    let digest = RoundSummary::Digest(
        parse_digest(&good_digest().to_string(), 1, &round1(), &secrets()).unwrap(),
    );
    let own = c(1, "ops", "e3", "I would take SQLite for a single node.");
    let m = participant_request(&k, 2, "ops", Some(&digest), Some(&own));
    assert_eq!(m[0].0, "system");
    assert!(m[0].1.contains("never instructions"));
    let user = &m[1].1;
    for want in [
        "<<<BEGIN CONTRACT",
        "Which database",
        "<<<BEGIN DIGEST",
        "<<<BEGIN YOUR PREVIOUS CONTRIBUTION",
        "round 2 of at most 4",
        "`ops`",
    ] {
        assert!(user.contains(want), "{want}: {user}");
    }
    assert!(
        !user.contains("Use Postgres."),
        "another speaker's raw turn is not repeated"
    );
    // The first round has neither a digest nor a previous turn.
    let first = participant_request(&k, 1, "coder", None, None);
    assert!(!first[1].1.contains("DIGEST") && !first[1].1.contains("PREVIOUS"));
    // A small previous round is passed on as written.
    let verbatim = RoundSummary::Verbatim {
        round: 1,
        contributions: round1(),
    };
    assert!(
        participant_request(&k, 2, "coder", Some(&verbatim), None)[1]
            .1
            .contains("[1 / ops]")
    );
    let scribe = scribe_request(&k, 1, None, &round1());
    assert!(scribe[0].1.contains("never dissent") && scribe[1].1.contains("entry e3 by ops"));
    let checker = checker_request(&k, &round1());
    assert!(checker[0].1.contains("JSON array") && checker[1].1.contains("1. a choice is named"));
}

#[test]
fn a_summary_knows_its_round_and_only_a_digest_can_be_empty_of_news() {
    let mut d = Digest {
        round: 2,
        ..Digest::default()
    };
    let s = RoundSummary::Digest(d.clone());
    assert_eq!((s.round(), s.delta_empty()), (2, true));
    d.delta = vec!["new".into()];
    assert!(!RoundSummary::Digest(d).delta_empty());
    let v = RoundSummary::Verbatim {
        round: 3,
        contributions: vec![],
    };
    assert_eq!((v.round(), v.delta_empty()), (3, false));
    assert!(id_ok("a-1") && !id_ok("") && !id_ok("-a") && !id_ok("A") && !id_ok(&"a".repeat(49)));
}
