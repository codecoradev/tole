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

pub(crate) fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-discuss-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
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
