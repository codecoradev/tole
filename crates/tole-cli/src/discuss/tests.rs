use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use tole_core::entry::Entry;
use tole_core::provider::{Provider, ProviderError, ProviderOutput};

const SECRET: &str = "discuss-cli-secret-0123456789";

/// How the fake answers: by kind of call (`scribe`, `checker`, or the
/// participant's name).
pub(crate) type Answer = Arc<dyn Fn(&str, &str) -> Result<ProviderOutput, String> + Send + Sync>;

struct Fake {
    answer: Answer,
    calls: Arc<Mutex<Vec<String>>>,
    system: String,
    usage: u64,
}

fn kind_of(system: &str, user: &str) -> String {
    if system.contains("digest of one round") {
        return "scribe".into();
    }
    if system.contains("against its acceptance criteria") {
        return "checker".into();
    }
    user.split("You are `")
        .nth(1)
        .and_then(|r| r.split('`').next())
        .unwrap_or("?")
        .to_string()
}

impl Provider for Fake {
    fn complete(&mut self, t: &[Entry]) -> Result<ProviderOutput, ProviderError> {
        let user = t[0].payload["text"].as_str().unwrap().to_string();
        let kind = kind_of(&self.system, &user);
        self.calls.lock().unwrap().push(kind.clone());
        (self.answer)(&kind, &user).map_err(ProviderError)
    }
    fn last_usage(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({"prompt_tokens": self.usage, "completion_tokens": 0}))
    }
}

pub(crate) fn factory(usage: u64, answer: Answer) -> (Arc<Mutex<Vec<String>>>, Factory) {
    let calls: Arc<Mutex<Vec<String>>> = Arc::default();
    let c2 = Arc::clone(&calls);
    let make: Factory = Arc::new(move |_model, system| {
        Box::new(Fake {
            answer: Arc::clone(&answer),
            calls: Arc::clone(&c2),
            system: system.to_string(),
            usage,
        })
    });
    (calls, make)
}

/// Participants talk; the checker meets the one criterion, citing the
/// first entry it was shown.
pub(crate) fn agreeable() -> Answer {
    Arc::new(|kind, user| {
        let text = match kind {
            "checker" => {
                let e = user
                    .lines()
                    .find_map(|l| l.strip_prefix("entry "))
                    .and_then(|l| l.split_whitespace().next())
                    .unwrap_or("e1")
                    .to_string();
                serde_json::json!([{"met": true, "evidence": [e]}]).to_string()
            }
            "scribe" => "not json".to_string(),
            who => format!("{who} says use sqlite"),
        };
        Ok(ProviderOutput::Final { text })
    })
}

pub(crate) fn tmp(tag: &str) -> crate::test_dir::TestDir {
    crate::test_dir::TestDir::new(&format!("tole-discuss-{tag}"))
}

pub(crate) fn config() -> Config {
    crate::config::parse(
        "[profiles.coder]\n[profiles.ops]\n[profiles.scribe.models]\nlight = \"tiny\"\n",
        Path::new("user.toml"),
    )
    .unwrap()
}

fn args(budget: u64) -> StartArgs {
    StartArgs {
        id: Some("pick-db".into()),
        question: "Which database?".into(),
        criteria: vec!["a choice is named".into()],
        participants: vec!["coder".into(), "ops".into()],
        budget_tokens: budget,
        max_rounds: Some(3),
    }
}

fn env<'a>(dir: &'a Path, cfg: &'a Config, make: Factory) -> Env<'a> {
    Env {
        dir,
        cfg,
        paths: (None, None),
        secrets: SecretSet::default().with("MY_TOKEN", SECRET),
        make,
    }
}

fn state(dir: &Path) -> tole_core::discussion_log::State {
    DiscussionLog::at(dir, "pick-db").unwrap().state().unwrap()
}

#[test]
fn start_runs_the_owner_typed_contract_to_a_report_and_shows_the_cost() {
    let dir = tmp("start");
    let cfg = config();
    let (calls, make) = factory(100, agreeable());
    let out = start(&args(50_000), &env(&dir, &cfg, make)).unwrap();
    assert!(out.contains("Budget: 50000 tokens, a hard cap"), "{out}");
    assert!(
        out.contains("up to 3 rounds, up to 12 model calls"),
        "{out}"
    );
    assert!(out.contains("Tokens used: "), "{out}");
    assert!(out.contains("Outcome: "), "{out}");
    let st = state(&dir);
    let c = st.contract.unwrap();
    // Exactly what the owner typed, nothing the models added.
    assert_eq!(c.question, "Which database?");
    assert_eq!(c.participants, ["coder", "ops"]);
    assert_eq!((c.budget_tokens, c.max_rounds), (50_000, 3));
    assert!(st.closed.is_some());
    assert!(calls.lock().unwrap().iter().any(|k| k == "coder"));
}

#[test]
fn the_budget_is_a_hard_cap_and_a_stop_escalates_to_the_owner() {
    let dir = tmp("cap");
    let cfg = config();
    let (_, make) = factory(100, agreeable());
    // Too small for even one participant call: reserved first, so none runs.
    let out = start(&args(300), &env(&dir, &cfg, make)).unwrap();
    let st = state(&dir);
    assert!(st.tokens_used <= 300, "{}", st.tokens_used);
    assert!(out.contains("needs the owner"), "{out}");
    assert!(matches!(st.closed, Some((g, _)) if g != Gate::Report));
}

#[test]
fn a_bad_contract_or_participant_is_refused_before_anything_is_written() {
    let dir = tmp("refuse");
    let cfg = config();
    let (calls, make) = factory(100, agreeable());
    let e = env(&dir, &cfg, make);
    let mut bad = args(1_000);
    bad.participants = vec!["stranger".into()];
    assert!(start(&bad, &e).unwrap_err().contains("unknown profile"));
    // A built-in role is not a participant.
    bad.participants = vec!["scribe".into()];
    assert!(start(&bad, &e)
        .unwrap_err()
        .contains("built-in discussion role"));
    for (mutate, want) in [
        (
            (|a: &mut StartArgs| a.budget_tokens = 0) as fn(&mut StartArgs),
            "budget",
        ),
        (|a| a.criteria.clear(), "acceptance criteria"),
        (|a| a.max_rounds = Some(11), "max rounds"),
        (|a| a.question = "  ".into(), "question"),
        (|a| a.id = Some("Bad Id".into()), "discussion id"),
    ] {
        let mut a = args(1_000);
        mutate(&mut a);
        let err = start(&a, &e).unwrap_err();
        assert!(err.contains(want), "{want}: {err}");
    }
    assert!(!dir.join("pick-db.jsonl").exists(), "nothing was written");
    assert!(calls.lock().unwrap().is_empty(), "no provider was called");
}

#[test]
fn a_second_start_on_the_same_id_is_refused() {
    let dir = tmp("twice");
    let cfg = config();
    let (_, make) = factory(100, agreeable());
    let e = env(&dir, &cfg, make);
    start(&args(50_000), &e).unwrap();
    assert!(start(&args(50_000), &e)
        .unwrap_err()
        .contains("already has a contract"));
}

#[test]
fn dry_run_shows_the_cost_and_runs_nothing() {
    let dir = tmp("dry");
    let cfg = config();
    let (calls, make) = factory(100, agreeable());
    let out = plan(&args(9_000), &env(&dir, &cfg, make)).unwrap();
    assert!(out.contains("9000 tokens"), "{out}");
    assert!(calls.lock().unwrap().is_empty());
    assert!(!dir.join("pick-db.jsonl").exists());
    // The dry run validates too.
    let mut bad = args(9_000);
    bad.participants = vec!["stranger".into()];
    let (_, make) = factory(100, agreeable());
    assert!(plan(&bad, &env(&dir, &cfg, make)).is_err());
}

#[test]
fn a_crash_is_resumed_without_rerunning_finished_turns_or_resetting_the_budget() {
    let dir = tmp("resume");
    let cfg = config();
    // `ops` fails once: coder's turn is done, ops' is not.
    let failed = Arc::new(AtomicUsize::new(0));
    let f2 = Arc::clone(&failed);
    let flaky: Answer = Arc::new(move |kind, user| {
        if kind == "ops" && f2.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(format!("upstream 500 {SECRET}"));
        }
        agreeable()(kind, user)
    });
    let (_, make) = factory(100, flaky);
    let err = start(&args(50_000), &env(&dir, &cfg, make)).unwrap_err();
    assert!(err.contains("tole discuss resume pick-db"), "{err}");
    assert!(!err.contains(SECRET), "errors are redacted: {err}");
    let before = state(&dir);
    assert!(before.closed.is_none());
    assert_eq!(before.contributions.len(), 1, "coder spoke");

    let (calls, make) = factory(100, agreeable());
    let out = resume("pick-db", &env(&dir, &cfg, make)).unwrap();
    assert!(out.contains("Outcome: "), "{out}");
    let seen = calls.lock().unwrap().clone();
    assert_eq!(
        seen.iter().filter(|k| *k == "coder").count(),
        // Coder's round-1 turn is not run again; later rounds may run it.
        seen.iter().filter(|k| *k == "ops").count() - 1,
        "{seen:?}"
    );
    let after = state(&dir);
    assert!(after.closed.is_some());
    assert!(
        after.tokens_used <= 50_000,
        "the ledger's budget still holds: {}",
        after.tokens_used
    );
    // Resuming a finished discussion prints its outcome and calls nothing.
    let (calls, make) = factory(100, agreeable());
    let again = resume("pick-db", &env(&dir, &cfg, make)).unwrap();
    assert!(again.contains("Outcome: "));
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn a_resumed_discussion_never_gets_a_fresh_allowance() {
    let dir = tmp("resume-cap");
    let cfg = config();
    // `ops` fails in round 1 after coder spent 400 of a 4000 budget.
    let failing: Answer = Arc::new(|kind, user| {
        if kind == "ops" {
            return Err("down".into());
        }
        agreeable()(kind, user)
    });
    let (_, make) = factory(400, failing);
    start(&args(4_000), &env(&dir, &cfg, make)).unwrap_err();
    assert!(state(&dir).tokens_used > 0);
    // Every call costs 400: only what is left of the SAME 4000 can be spent.
    let (_, make) = factory(400, agreeable());
    resume("pick-db", &env(&dir, &cfg, make)).unwrap();
    let st = state(&dir);
    assert!(st.tokens_used <= 4_000, "{}", st.tokens_used);
    assert_eq!(st.contract.unwrap().budget_tokens, 4_000);
}

#[test]
fn resume_of_an_unknown_discussion_is_an_error() {
    let dir = tmp("resume-none");
    let cfg = config();
    let (_, make) = factory(100, agreeable());
    let e = env(&dir, &cfg, make);
    assert!(resume("nope", &e).unwrap_err().contains("no discussion"));
    assert!(resume("../x", &e).unwrap_err().contains("invalid"));
}

#[test]
fn a_participant_that_answers_with_a_tool_call_runs_nothing() {
    let dir = tmp("toolcall");
    let cfg = config();
    let hostile: Answer = Arc::new(|kind, user| {
        if kind == "coder" {
            return Ok(ProviderOutput::ToolCall {
                tool: "run_command".into(),
                input: serde_json::json!({"command": "touch /tmp/pwned"}),
            });
        }
        agreeable()(kind, user)
    });
    let (_, make) = factory(100, hostile);
    start(&args(50_000), &env(&dir, &cfg, make)).unwrap();
    let st = state(&dir);
    assert!(st.contributions[0].text.contains("nothing was run"));
}

#[test]
fn show_lists_and_reads_a_discussion_and_refuses_a_bad_id() {
    let dir = tmp("show");
    let cfg = config();
    let (_, make) = factory(100, agreeable());
    let e = env(&dir, &cfg, make);
    start(&args(50_000), &e).unwrap();
    let list = show(&dir, None, false, &e.secrets).unwrap();
    assert!(list.contains("pick-db"), "{list}");
    let one = show(&dir, Some("pick-db"), true, &e.secrets).unwrap();
    assert!(one.contains("pick-db"));
    assert!(show(&dir, Some("../etc"), false, &e.secrets).is_err());
    assert!(show(&dir, Some("nope"), false, &e.secrets).is_err());
}

// ---- #856: the participant count is advice, not a limit ----

fn voice_names(n: usize) -> Vec<String> {
    (1..=n).map(|i| format!("voice-{i}")).collect()
}

fn wide_config(n: usize) -> Config {
    let mut toml: String = voice_names(n)
        .iter()
        .map(|v| format!("[profiles.{v}]\n"))
        .collect();
    toml.push_str("[profiles.scribe.models]\nlight = \"tiny\"\n");
    crate::config::parse(&toml, Path::new("user.toml")).unwrap()
}

fn wide_args(n: usize, budget: u64) -> StartArgs {
    StartArgs {
        participants: voice_names(n),
        ..args(budget)
    }
}

/// Every voice speaks 400 characters (a big round, so the scribe digests
/// it); the scribe cites every entry it was shown; the checker is met.
fn wide_answers() -> Answer {
    Arc::new(|kind, user| {
        let entries = || {
            user.lines()
                .filter_map(|l| l.strip_prefix("entry "))
                .filter_map(|l| {
                    let id = l.split_whitespace().next()?.to_string();
                    let by = l.split(" by ").nth(1)?.trim_end_matches(':').to_string();
                    Some((id, by))
                })
                .collect::<Vec<_>>()
        };
        let text = match kind {
            "scribe" => {
                let claims: Vec<_> = entries()
                    .into_iter()
                    .map(|(e, by)| serde_json::json!({"text": "argued", "by": by, "entry": e}))
                    .collect();
                serde_json::json!({"claims": claims, "delta": ["round one"]}).to_string()
            }
            "checker" => {
                serde_json::json!([{"met": true, "evidence": [entries()[0].0.clone()]}]).to_string()
            }
            who => format!("{who} says use sqlite. {}", "because ".repeat(50)),
        };
        Ok(ProviderOutput::Final { text })
    })
}

#[test]
fn the_advice_starts_above_eight_voices_and_never_refuses() {
    let s = SecretSet::default();
    for n in [1, 7, 8] {
        assert!(advice_for(&wide_args(n, 500_000), &s).is_empty(), "{n}");
    }
    for n in [9, 12, 16] {
        let a = advice_for(&wide_args(n, 500_000), &s);
        assert_eq!(a.len(), 1, "{a:?}");
        assert!(
            a[0].starts_with(&format!(
                "{n} participants: more than 8 voices mostly add cost"
            )) && a[0].contains("3 to 8 is the sweet spot"),
            "{a:?}"
        );
    }
    // An invalid contract has no advice; the start reports the error.
    assert!(advice_for(&wide_args(17, 500_000), &s).is_empty());
}

#[test]
fn above_the_ceiling_a_start_is_refused_naming_it_and_writes_nothing() {
    let dir = tmp("ceiling");
    let cfg = wide_config(17);
    let (calls, make) = factory(100, wide_answers());
    let err = start(&wide_args(17, 500_000), &env(&dir, &cfg, make)).unwrap_err();
    assert!(
        err.contains("at most 16") && err.contains("ledger record"),
        "{err}"
    );
    assert!(!dir.join("pick-db.jsonl").exists());
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn twelve_voices_reach_the_gate_and_the_digest_keeps_every_voice() {
    let dir = tmp("wide12");
    let cfg = wide_config(12);
    let (calls, make) = factory(100, wide_answers());
    let out = start(&wide_args(12, 500_000), &env(&dir, &cfg, make)).unwrap();
    assert!(out.contains("12 participants"), "{out}");
    assert!(out.contains("the acceptance criteria are met"), "{out}");
    let st = state(&dir);
    assert!(matches!(st.closed, Some((Gate::Report, _))));
    // One round: every voice once, a scribe, a checker.
    let seen = calls.lock().unwrap().clone();
    assert_eq!(seen.len(), 12 + 2, "{seen:?}");
    match st.summaries.get(&1).unwrap() {
        tole_core::discussion::RoundSummary::Digest(d) => {
            let by: std::collections::BTreeSet<&str> =
                d.claims.iter().map(|i| i.by.as_str()).collect();
            assert_eq!(by.len(), 12, "every voice appears in the digest: {by:?}");
        }
        other => panic!("a digest was expected: {other:?}"),
    }
    assert!(st.tokens_used <= 500_000);
}

#[test]
fn a_many_voice_discussion_resumes_where_the_ledger_stands() {
    let dir = tmp("wide-resume");
    let cfg = wide_config(12);
    // voice-7 fails once: six voices spoke, the rest did not.
    let failed = Arc::new(AtomicUsize::new(0));
    let f2 = Arc::clone(&failed);
    let flaky: Answer = Arc::new(move |kind, user| {
        if kind == "voice-7" && f2.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err("upstream 500".into());
        }
        wide_answers()(kind, user)
    });
    let (_, make) = factory(100, flaky);
    let err = start(&wide_args(12, 500_000), &env(&dir, &cfg, make)).unwrap_err();
    assert!(err.contains("tole discuss resume pick-db"), "{err}");
    assert_eq!(state(&dir).contributions.len(), 6);
    let (calls, make) = factory(100, wide_answers());
    let out = resume("pick-db", &env(&dir, &cfg, make)).unwrap();
    assert!(out.contains("the acceptance criteria are met"), "{out}");
    let seen = calls.lock().unwrap().clone();
    for done in 1..=6 {
        assert!(
            !seen.contains(&format!("voice-{done}")),
            "voice-{done} is not run again: {seen:?}"
        );
    }
    assert_eq!(seen.len(), 6 + 2, "{seen:?}");
    assert_eq!(state(&dir).contributions.len(), 12);
}

#[test]
fn the_ceiling_runs_end_to_end_when_the_scribe_fails_and_the_round_is_kept_as_written() {
    let dir = tmp("wide16");
    let cfg = wide_config(16);
    // A refused digest: all sixteen big contributions go into one record.
    let hostile: Answer = Arc::new(|kind, user| {
        if kind == "scribe" {
            return Ok(ProviderOutput::Final {
                text: "not json".into(),
            });
        }
        wide_answers()(kind, user)
    });
    let (_, make) = factory(100, hostile);
    let out = start(&wide_args(16, 900_000), &env(&dir, &cfg, make)).unwrap();
    assert!(out.contains("Outcome: "), "{out}");
    let st = state(&dir);
    assert!(st.closed.is_some());
    assert!(matches!(
        st.summaries.get(&1),
        Some(tole_core::discussion::RoundSummary::Fallback { .. })
    ));
}

#[test]
fn the_cost_plan_scales_with_the_voices_and_a_tiny_budget_is_warned_about_first() {
    let dir = tmp("wide-dry");
    let s = SecretSet::default();
    let c = |n: usize, b: u64| contract(&wide_args(n, b), 1, &s).unwrap();
    let (few, many) = (host::round_floor(&c(2, 1)), host::round_floor(&c(12, 1)));
    assert!(
        many > few * 5,
        "the floor grows with the voices: {few} -> {many}"
    );
    // A big budget: the plan shows the floor and what it covers; no warning.
    let cfg = wide_config(12);
    let (calls, make) = factory(100, wide_answers());
    let e = env(&dir, &cfg, make);
    let out = plan(&wide_args(12, 500_000), &e).unwrap();
    assert!(
        out.contains(&format!("a round reserves at least {many} tokens"))
            && out.contains("covers about 3 round(s)"),
        "{out}"
    );
    assert_eq!(advice_for(&wide_args(12, 500_000), &s).len(), 1);
    // A tiny budget: the plan and the advice both say one round does not fit,
    // and the dry run still passes (the owner may want it).
    let out = plan(&wide_args(12, 2_000), &e).unwrap();
    assert!(
        out.contains("does not fit one round of 12 participants"),
        "{out}"
    );
    let a = advice_for(&wide_args(12, 2_000), &s);
    assert_eq!(a.len(), 2, "{a:?}");
    assert!(a[1].contains("budget escalation in round 1"), "{a:?}");
    assert!(calls.lock().unwrap().is_empty());
    assert!(!dir.join("pick-db.jsonl").exists());
}

#[test]
fn the_estimate_grows_with_the_question_and_says_how_long_it_is() {
    // #1038: a question of 12 000 characters travels with every call, so the
    // dry run's floor and the plan text both carry it.
    let s = SecretSet::default();
    let with = |q: &str| StartArgs {
        question: q.into(),
        ..args(500_000)
    };
    let short = contract(&with("Which database?"), 1, &s).unwrap();
    let long_q = "q".repeat(tole_core::discussion::MAX_QUESTION_CHARS);
    let long = contract(&with(&long_q), 1, &s).unwrap();
    let (a, b) = (host::round_floor(&short), host::round_floor(&long));
    // Two participants and the checker each read the question once: about
    // 3 000 tokens apiece on top of the short question's floor.
    assert!(
        b >= a + 3 * 2_900,
        "the floor grows with the question: {a} -> {b}"
    );
    let (pa, pb) = (cost_plan(&short), cost_plan(&long));
    assert!(pa.contains("Question: 15 characters"), "{pa}");
    assert!(
        pb.contains("Question: 12000 characters (about 3000 tokens)"),
        "{pb}"
    );
    assert!(
        pb.contains(&format!("a round reserves at least {b} tokens")),
        "{pb}"
    );
    // The dry run passes a 12 000-character question and writes nothing.
    let dir = tmp("long-dry");
    let cfg = config();
    let (calls, make) = factory(100, agreeable());
    let out = plan(&with(&long_q), &env(&dir, &cfg, make)).unwrap();
    assert!(out.contains("Question: 12000 characters"), "{out}");
    assert!(calls.lock().unwrap().is_empty());
    assert!(!dir.join("pick-db.jsonl").exists());
}

#[test]
fn a_twelve_thousand_character_question_runs_and_the_closing_text_stays_readable() {
    let dir = tmp("long-run");
    let cfg = config();
    let (_, make) = factory(100, agreeable());
    let q = format!("{}END-OF-QUESTION", "q".repeat(11_985));
    let a = StartArgs {
        question: q.clone(),
        ..args(500_000)
    };
    let out = start(&a, &env(&dir, &cfg, make)).unwrap();
    let st = state(&dir);
    // The ledger holds the whole question; nothing was cut.
    assert_eq!(st.contract.as_ref().unwrap().question, q);
    // The closing text shows an excerpt and the length, so the checklist and
    // the last round are not pushed out of the capped text.
    let (_, text) = st.closed.unwrap();
    assert!(text.contains("12000 characters; the whole question is in the discussion ledger"));
    assert!(!text.contains("END-OF-QUESTION"), "{}", text.len());
    assert!(text.contains("Checklist (claim-checker") && text.contains("<<<END LAST ROUND>>>"));
    assert!(out.contains("Outcome: "), "{out}");
}

#[test]
fn a_round_one_budget_escalation_says_why() {
    let dir = tmp("wide-r1");
    let cfg = wide_config(12);
    let (_, make) = factory(100, wide_answers());
    // Not even the first participant's reservation fits.
    let out = start(&wide_args(12, 300), &env(&dir, &cfg, make)).unwrap();
    assert!(
        out.contains("Round 1 did not finish")
            && out.contains("one round of 12 participants is reserved at about")
            && out.contains("needs the owner"),
        "{out}"
    );
    assert!(state(&dir).tokens_used <= 300);
}
