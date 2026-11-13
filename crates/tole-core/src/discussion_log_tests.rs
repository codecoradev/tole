use super::*;
use crate::discussion::{parse_checker, parse_digest, Contract, Digest, Item};
use crate::secrets::SecretSet;

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-discuss-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn contract(rounds: Option<u32>, budget: u64) -> Contract {
    Contract::new(
        "pick-db",
        "Which database?",
        &strs(&["a choice is named"]),
        &strs(&["coder", "ops"]),
        budget,
        rounds,
        &SecretSet::default(),
    )
    .unwrap()
}

fn log(tag: &str) -> DiscussionLog {
    DiscussionLog::at(&dir(tag), "pick-db").unwrap()
}

/// Both participants speak, the (small) round is passed on as written, and
/// the checker answers `met`.
fn play_round(l: &DiscussionLog, round: u32, met: bool, t: u64) -> Vec<Contribution> {
    let mut cs = Vec::new();
    for who in ["coder", "ops"] {
        cs.push(
            l.contribute(round, who, &format!("{who} speaks in {round}"), t)
                .unwrap(),
        );
    }
    // These rounds are small, so they are passed on as written.
    l.summarize(
        RoundSummary::Verbatim {
            round,
            contributions: cs.clone(),
        },
        t,
    )
    .unwrap();
    let verdicts = vec![CriterionVerdict {
        met,
        evidence: if met {
            vec![cs[0].entry.clone()]
        } else {
            vec![]
        },
    }];
    l.check(round, verdicts, t).unwrap();
    cs
}

#[test]
fn a_discussion_runs_to_a_checked_report_and_every_step_is_in_the_file() {
    let l = log("report");
    assert_eq!(l.state().unwrap().next(), Next::NeedContract);
    l.start(contract(None, 10_000), 1).unwrap();
    assert_eq!(
        l.state().unwrap().next(),
        Next::Contribute {
            round: 1,
            missing: strs(&["coder", "ops"])
        }
    );
    let r1 = play_round(&l, 1, false, 2);
    assert_eq!(l.state().unwrap().next(), Next::Decide { round: 1 });
    l.usage(1, 1200, 3).unwrap();
    assert_eq!(l.decide(1, 4).unwrap(), Gate::Continue);
    assert_eq!(
        l.state().unwrap().next(),
        Next::Contribute {
            round: 2,
            missing: strs(&["coder", "ops"])
        }
    );
    let st = l.state().unwrap();
    assert_eq!(st.prior_summary(2).map(RoundSummary::round), Some(1));
    assert_eq!(
        st.own_previous("ops", 2).map(|c| c.entry.clone()),
        Some(r1[1].entry.clone())
    );
    assert!(st.own_previous("ops", 1).is_none());

    play_round(&l, 2, true, 5);
    assert_eq!(l.decide(2, 6).unwrap(), Gate::Report);
    assert_eq!(
        l.state().unwrap().next(),
        Next::Finish {
            outcome: Gate::Report
        }
    );
    assert_eq!(l.close("pick Postgres", 7).unwrap(), Gate::Report);
    let st = l.state().unwrap();
    assert_eq!(st.next(), Next::Done);
    assert_eq!(
        st.closed.as_ref().map(|c| c.1.as_str()),
        Some("pick Postgres")
    );
    assert_eq!(st.tokens_used, 1200);
    // Nothing after the end.
    assert!(l.contribute(3, "coder", "x", 8).is_err());
    assert!(l.usage(3, 1, 8).is_err());
    assert!(l.close("again", 8).is_err());
    // One JSON line per step, sequence numbers 1..n.
    let text = std::fs::read_to_string(dir_of(&l)).unwrap();
    let seqs: Vec<u64> = text
        .lines()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["seq"]
                .as_u64()
                .unwrap()
        })
        .collect();
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
}

fn dir_of(l: &DiscussionLog) -> PathBuf {
    l.path.clone()
}

#[test]
fn the_steps_cannot_be_skipped_repeated_or_reordered() {
    let l = log("order");
    assert!(l.contribute(1, "coder", "x", 1).is_err(), "no contract yet");
    l.start(contract(None, 10_000), 1).unwrap();
    assert!(l.start(contract(None, 10_000), 2).is_err(), "one contract");
    let c = l.contribute(1, "coder", "first", 2).unwrap();
    assert_eq!(c.entry, "e2", "the entry id is the log's own sequence");
    assert!(
        l.contribute(1, "coder", "again", 3).is_err(),
        "once per round"
    );
    assert!(
        l.contribute(1, "stranger", "x", 3).is_err(),
        "not a participant"
    );
    assert!(
        l.contribute(2, "ops", "x", 3).is_err(),
        "not the current round"
    );
    assert!(l.decide(1, 3).is_err(), "no summary, no verdicts");
    assert!(l.check(1, vec![], 3).is_err());
    let early = RoundSummary::Verbatim {
        round: 1,
        contributions: vec![c.clone()],
    };
    assert!(l.summarize(early, 3).is_err(), "ops has not spoken");
    let o = l.contribute(1, "ops", "second", 3).unwrap();
    // A verbatim summary must be exactly the round; a digest may cite only its entries.
    let partial = RoundSummary::Verbatim {
        round: 1,
        contributions: vec![c.clone()],
    };
    assert!(l.summarize(partial, 4).is_err());
    let invented = RoundSummary::Digest(Digest {
        round: 1,
        claims: vec![Item {
            text: "x".into(),
            by: "coder".into(),
            entry: "e99".into(),
        }],
        ..Digest::default()
    });
    assert!(l.summarize(invented, 4).unwrap_err().contains("e99"));
    let wrong_round = RoundSummary::Verbatim {
        round: 2,
        contributions: vec![],
    };
    assert!(l.summarize(wrong_round, 4).is_err());
    l.summarize(
        RoundSummary::Verbatim {
            round: 1,
            contributions: vec![c.clone(), o.clone()],
        },
        4,
    )
    .unwrap();
    assert!(
        l.summarize(
            RoundSummary::Verbatim {
                round: 1,
                contributions: vec![c.clone(), o.clone()]
            },
            5
        )
        .is_err(),
        "once"
    );
    // The verdicts: one per criterion, real entries, evidence for a met one.
    assert!(l.check(1, vec![], 5).is_err());
    let cite = |ev: &[&str], met| {
        vec![CriterionVerdict {
            met,
            evidence: strs(ev),
        }]
    };
    assert!(l.check(1, cite(&["e77"], true), 5).is_err());
    assert!(l.check(1, cite(&[], true), 5).is_err());
    assert!(l.check(2, cite(&[], false), 5).is_err(), "wrong round");
    l.check(1, cite(&[], false), 5).unwrap();
    assert!(l.check(1, cite(&[], false), 6).is_err(), "once");
    assert!(l.close("too early", 6).is_err());
    assert_eq!(l.decide(1, 6).unwrap(), Gate::Continue);
    assert!(l.decide(1, 7).is_err(), "once");
    assert!(l.flags(1, true, false, 7).is_err(), "decided");
}

#[test]
fn the_gate_is_computed_from_the_ledger_not_supplied() {
    // Max rounds.
    let l = log("maxr");
    l.start(contract(Some(1), 10_000), 1).unwrap();
    play_round(&l, 1, false, 2);
    assert_eq!(l.decide(1, 3).unwrap(), Gate::Escalate(Reason::MaxRounds));
    assert_eq!(
        l.close("stopped at the limit", 4).unwrap(),
        Gate::Escalate(Reason::MaxRounds)
    );

    // Budget, from the usage lines.
    let l = log("budget");
    l.start(contract(None, 1000), 1).unwrap();
    play_round(&l, 1, false, 2);
    l.usage(1, 600, 3).unwrap();
    l.usage(1, 600, 3).unwrap();
    assert_eq!(l.decide(1, 4).unwrap(), Gate::Escalate(Reason::Budget));

    // A flag the host saw: a pending approval beats even a met criterion.
    let l = log("flags");
    l.start(contract(None, 10_000), 1).unwrap();
    play_round(&l, 1, true, 2);
    l.flags(1, false, true, 3).unwrap();
    assert_eq!(
        l.decide(1, 4).unwrap(),
        Gate::Escalate(Reason::PendingApproval)
    );
    assert!(describe(Gate::Escalate(Reason::PendingApproval)).contains("approval"));
}

fn digest_round(l: &DiscussionLog, round: u32, delta: &[&str]) {
    let big = "x".repeat(crate::discussion::VERBATIM_ROUND_CHARS);
    let a = l
        .contribute(round, "coder", &format!("{big} a"), 10)
        .unwrap();
    let b = l.contribute(round, "ops", &format!("{big} b"), 10).unwrap();
    assert!(matches!(
        l.state().unwrap().next(),
        Next::Summarize {
            verbatim: false,
            ..
        }
    ));
    let raw = serde_json::json!({
        "claims": [
            {"text": "a", "by": "coder", "entry": a.entry},
            {"text": "b", "by": "ops", "entry": b.entry}],
        "delta": delta,
    })
    .to_string();
    let cs = l.state().unwrap().round_contributions(round);
    let d = parse_digest(&raw, round, &cs, &SecretSet::default()).unwrap();
    l.summarize(RoundSummary::Digest(d), 11).unwrap();
    l.check(
        round,
        vec![CriterionVerdict {
            met: false,
            evidence: vec![],
        }],
        12,
    )
    .unwrap();
}

#[test]
fn two_rounds_without_news_stall_the_discussion() {
    let l = log("stall");
    l.start(contract(None, 1_000_000), 1).unwrap();
    digest_round(&l, 1, &[]);
    assert_eq!(l.state().unwrap().stalled_rounds(), 1);
    assert_eq!(l.decide(1, 13).unwrap(), Gate::Continue);
    digest_round(&l, 2, &[]);
    assert_eq!(l.state().unwrap().stalled_rounds(), 2);
    assert_eq!(l.decide(2, 14).unwrap(), Gate::Escalate(Reason::Stalled));

    // News in between resets the count.
    let l = log("news");
    l.start(contract(None, 1_000_000), 1).unwrap();
    digest_round(&l, 1, &[]);
    l.decide(1, 13).unwrap();
    digest_round(&l, 2, &["something new"]);
    assert_eq!(l.state().unwrap().stalled_rounds(), 0);
    assert_eq!(l.decide(2, 14).unwrap(), Gate::Continue);
}

#[test]
fn a_big_round_cannot_be_passed_on_verbatim() {
    let l = log("bigverbatim");
    l.start(contract(None, 10_000), 1).unwrap();
    let big = "y".repeat(crate::discussion::VERBATIM_ROUND_CHARS);
    let a = l.contribute(1, "coder", &big, 2).unwrap();
    let b = l.contribute(1, "ops", "short", 2).unwrap();
    let v = RoundSummary::Verbatim {
        round: 1,
        contributions: vec![a, b],
    };
    assert!(l
        .summarize(v, 3)
        .unwrap_err()
        .contains("only a small round"));
}

#[test]
fn the_roster_can_change_between_rounds_within_the_contract() {
    let l = log("roster");
    l.start(contract(None, 10_000), 1).unwrap();
    assert!(l.set_roster(1, &strs(&["stranger"]), 2).is_err());
    assert!(l.set_roster(1, &[], 2).is_err());
    assert!(l.set_roster(1, &strs(&["coder", "coder"]), 2).is_err());
    play_round(&l, 1, false, 3);
    l.decide(1, 4).unwrap();
    // Round 2 with only `ops`.
    l.set_roster(2, &strs(&["ops"]), 5).unwrap();
    assert_eq!(
        l.state().unwrap().next(),
        Next::Contribute {
            round: 2,
            missing: strs(&["ops"])
        }
    );
    assert!(l.contribute(2, "coder", "x", 6).is_err(), "left the roster");
    let c = l.contribute(2, "ops", "alone", 6).unwrap();
    assert!(
        l.set_roster(2, &strs(&["coder"]), 7).is_err(),
        "someone has spoken"
    );
    l.summarize(
        RoundSummary::Verbatim {
            round: 2,
            contributions: vec![c],
        },
        7,
    )
    .unwrap();
    assert_eq!(
        l.state().unwrap().roster(3),
        strs(&["ops"]),
        "a roster holds until changed"
    );
}

#[test]
fn a_crash_resumes_from_the_file_and_a_torn_last_line_is_ignored() {
    let l = log("resume");
    l.start(contract(None, 10_000), 1).unwrap();
    l.contribute(1, "coder", "before the crash", 2).unwrap();
    let before = l.state().unwrap().next();
    assert_eq!(
        before,
        Next::Contribute {
            round: 1,
            missing: strs(&["ops"])
        }
    );
    // A new handle on the same file (a restarted process) sees the same step.
    let again = DiscussionLog::at(l.path.parent().unwrap(), "pick-db").unwrap();
    assert_eq!(again.state().unwrap().next(), before);
    // The process died mid-write: a torn tail with no newline.
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&l.path)
        .unwrap();
    f.write_all(br#"{"seq":3,"t":3,"event":"contribution","round":1,"parti"#)
        .unwrap();
    drop(f);
    assert_eq!(
        again.state().unwrap().next(),
        before,
        "the torn line was never a step"
    );
    // But damage in the middle is corruption, loudly.
    let text = std::fs::read_to_string(&l.path).unwrap();
    let broken = text.replacen("\"event\":\"contribution\"", "\"event\":\"nonsense\"", 1);
    std::fs::write(&l.path, broken).unwrap();
    let e = again.state().unwrap_err();
    assert!(e.contains("not a discussion event"), "{e}");
}

#[test]
fn ids_sizes_and_missing_files_are_handled() {
    let d = dir("misc");
    assert!(DiscussionLog::at(&d, "../x").is_err());
    assert!(DiscussionLog::at(&d, "").is_err());
    let l = DiscussionLog::at(&d, "never-started").unwrap();
    assert_eq!(l.state().unwrap().next(), Next::NeedContract);
    std::fs::write(&l.path, vec![b' '; (MAX_LOG_BYTES + 1) as usize]).unwrap();
    assert!(l.state().unwrap_err().contains("over"));
    // The checker's parse and the ledger agree on what evidence is.
    let known: BTreeSet<String> = ["e2"].map(String::from).into();
    assert!(parse_checker(r#"[{"met":true,"evidence":["e2"]}]"#, 1, &known).is_ok());
}

#[test]
fn concurrent_writers_never_share_a_turn_or_an_entry_id() {
    use std::sync::{Arc, Barrier};
    let l = log("race");
    l.start(contract(None, 10_000), 1).unwrap();
    // Both participants try to speak at once, over and over across fresh
    // discussions' worth of attempts: each can speak once, ids stay unique.
    for attempt in 0..20 {
        let l = log(&format!("race-{attempt}"));
        l.start(contract(None, 10_000), 1).unwrap();
        let gate = Arc::new(Barrier::new(4));
        let results: Vec<_> = ["coder", "ops", "coder", "ops"]
            .into_iter()
            .map(|who| {
                let (l, gate) = (l.clone(), gate.clone());
                std::thread::spawn(move || {
                    gate.wait();
                    l.contribute(1, who, "racing", 2)
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect();
        let ok: Vec<&Contribution> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
        assert_eq!(ok.len(), 2, "one turn each: {results:?}");
        let mut ids: Vec<&str> = ok.iter().map(|c| c.entry.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 2, "distinct entry ids");
        let st = l.state().unwrap();
        assert_eq!(st.contributions.len(), 2);
        assert_eq!(
            st.next(),
            Next::Summarize {
                round: 1,
                verbatim: true
            }
        );
    }
}

#[test]
fn the_step_after_a_crash_is_not_glued_onto_the_torn_line() {
    let l = log("torn-append");
    l.start(contract(None, 10_000), 1).unwrap();
    l.contribute(1, "coder", "before the crash", 2).unwrap();
    // The process died mid-write of ops's line: bytes with no newline.
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&l.path)
        .unwrap();
    f.write_all(br#"{"seq":3,"t":3,"event":"contribution","round":1,"part"#)
        .unwrap();
    drop(f);
    // The resumed process takes the same step again.
    let c = l.contribute(1, "ops", "after the crash", 4).unwrap();
    assert_eq!(c.entry, "e3", "the torn line never took a sequence number");
    let st = l.state().unwrap();
    assert_eq!(st.contributions.len(), 2);
    assert_eq!(
        st.next(),
        Next::Summarize {
            round: 1,
            verbatim: true
        }
    );
    // Every physical line parses; the torn bytes are gone.
    let text = std::fs::read_to_string(&l.path).unwrap();
    assert!(text.ends_with('\n'));
    for line in text.lines() {
        serde_json::from_str::<serde_json::Value>(line).unwrap();
    }
    assert!(
        !text.contains("\"t\":3,"),
        "the torn bytes are gone: {text}"
    );
}

#[test]
fn a_tear_inside_a_multibyte_character_does_not_make_the_log_unreadable() {
    let l = log("torn-utf8");
    l.start(contract(None, 10_000), 1).unwrap();
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&l.path)
        .unwrap();
    // "é" is two bytes; the write stopped after the first.
    f.write_all(b"{\"seq\":2,\"t\":2,\"event\":\"usage\",\"note\":\"caf\xC3")
        .unwrap();
    drop(f);
    let st = l.state().unwrap();
    assert_eq!(
        st.next(),
        Next::Contribute {
            round: 1,
            missing: strs(&["coder", "ops"])
        }
    );
    // And a step still works, replacing the torn bytes.
    l.contribute(1, "coder", "ok", 3).unwrap();
    assert!(String::from_utf8(std::fs::read(&l.path).unwrap()).is_ok());
    assert_eq!(l.state().unwrap().contributions.len(), 1);
}

#[test]
fn a_log_that_is_only_a_torn_line_restarts_clean() {
    let l = log("torn-only");
    std::fs::create_dir_all(l.path.parent().unwrap()).unwrap();
    std::fs::write(&l.path, br#"{"seq":1,"t":1,"event":"contr"#).unwrap();
    assert_eq!(l.state().unwrap().next(), Next::NeedContract);
    l.start(contract(None, 10_000), 2).unwrap();
    let text = std::fs::read_to_string(&l.path).unwrap();
    assert_eq!(text.lines().count(), 1, "{text}");
    assert_eq!(
        l.state().unwrap().next(),
        Next::Contribute {
            round: 1,
            missing: strs(&["coder", "ops"])
        }
    );
}
