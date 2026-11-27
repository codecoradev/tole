//! #1230, step 1: a prompt on a session whose turn was cut off mid-flight is a
//! defined refusal (`409`, `{"error": "interrupted turn", "state": ...}`), not
//! a 500. `session::interrupted` decides it from the register; these tests pin
//! that it agrees with the core's own `run_turn` refusal on a real storage, so
//! a change of the core's rule cannot leave the two apart.

mod common;
use common::TestDir;
use serde_json::json;
use tole_cli::session::interrupted::{prompt_error, refuse};
use tole_core::entry::{EntryType, NewEntry};
use tole_core::mock::MockProvider;
use tole_core::provider::ProviderOutput;
use tole_core::state::{Pc, StateTransition};
use tole_core::storage::{Commit, JsonlStorage, Storage};
use tole_core::tool::ToolRegistry;
use tole_core::turn::{self, transcript::DENIAL_NOTE};

/// A user-role message and the register move to `to`, as a turn writes them.
fn commit_message(s: &mut JsonlStorage, payload: serde_json::Value, to: Pc) {
    let seq = s.state().seq;
    s.commit(
        Commit::new()
            .entry(NewEntry::root(EntryType::new(EntryType::MESSAGE), payload))
            .transition(StateTransition::from(seq, to)),
    )
    .unwrap();
}

/// What the core does with a prompt on `s`: true when it refuses it.
fn core_refuses(s: &mut JsonlStorage) -> bool {
    let mut p = MockProvider::scripted(vec![ProviderOutput::Final { text: "ok".into() }]);
    let out = turn::run_turn(s, &mut p, &ToolRegistry::new(), "next");
    matches!(out, Err(e) if e.to_string().contains("resolve the session first"))
}

#[test]
fn a_cut_off_turn_is_refused_with_its_state_and_nothing_is_written() {
    let dir = TestDir::new("interrupted-planning");
    let mut s = JsonlStorage::create(&dir, "s", None).unwrap();
    commit_message(&mut s, json!({"role": "user", "text": "q"}), Pc::Planning);

    let before = (s.entries().len(), s.state().seq);
    let message = refuse(&s).unwrap_err();
    assert_eq!(message, "interrupted turn: planning");
    assert_eq!(
        prompt_error(message),
        (
            409,
            json!({"error": "interrupted turn", "state": "planning"})
        )
    );
    assert_eq!(
        (s.entries().len(), s.state().seq),
        before,
        "nothing written"
    );
    assert!(core_refuses(&mut s), "the core refuses the same session");
}

#[test]
fn a_settled_session_and_a_turn_parked_after_a_denial_take_a_prompt() {
    let dir = TestDir::new("interrupted-ok");
    let mut s = JsonlStorage::create(&dir, "s", None).unwrap();
    assert_eq!(refuse(&s), Ok(()), "idle");

    commit_message(&mut s, json!({"role": "user", "text": "q"}), Pc::Planning);
    let note = json!({"role": "user", "text": "[tole] denied", DENIAL_NOTE: true});
    s.commit(Commit::new().entry(NewEntry::root(EntryType::new(EntryType::MESSAGE), note)))
        .unwrap();
    assert_eq!(refuse(&s), Ok(()), "parked after a denial (#476)");
    assert!(!core_refuses(&mut s), "the core settles it and runs");
}

/// Every register value, reached by legal steps: `refuse` and the core's own
/// `run_turn` refusal agree on each, and the refusal names the register.
#[test]
fn refuse_agrees_with_the_core_for_every_register_value() {
    use Pc::*;
    // (case, steps after Idle, denial note last, refused, state named)
    let cases: [(&str, &[Pc], bool, bool, &str); 8] = [
        ("idle", &[], false, false, ""),
        ("planning", &[Planning], false, true, "planning"),
        ("planning, denial note last", &[Planning], true, false, ""),
        ("tool_call", &[Planning, ToolCall], false, true, "tool_call"),
        (
            "awaiting_approval",
            &[Planning, ToolCall, AwaitingApproval],
            false,
            true,
            "awaiting_approval",
        ),
        (
            "executing",
            &[Planning, ToolCall, Executing],
            false,
            true,
            "executing",
        ),
        (
            "settling",
            &[Planning, ToolCall, Executing, Settling],
            false,
            true,
            "settling",
        ),
        ("final", &[Planning, Final], false, false, ""),
    ];
    for (name, steps, denial, refused, state) in cases {
        let dir = TestDir::new("interrupted-table");
        let mut s = JsonlStorage::create(&dir, "s", None).unwrap();
        for (i, pc) in steps.iter().enumerate() {
            if i == 0 {
                commit_message(&mut s, json!({"role": "user", "text": "q"}), *pc);
            } else {
                let seq = s.state().seq;
                s.commit(Commit::new().transition(StateTransition::from(seq, *pc)))
                    .unwrap();
            }
        }
        if denial {
            let note = json!({"role": "user", "text": "[tole] denied", DENIAL_NOTE: true});
            s.commit(Commit::new().entry(NewEntry::root(EntryType::new(EntryType::MESSAGE), note)))
                .unwrap();
        }
        let ours = refuse(&s);
        assert_eq!(ours.is_err(), refused, "{name}: refuse");
        if refused {
            let (code, body) = prompt_error(ours.unwrap_err());
            assert_eq!(code, 409, "{name}");
            assert_eq!(body["state"], state, "{name}");
        }
        assert_eq!(core_refuses(&mut s), refused, "{name}: the core");
    }
}

#[test]
fn only_busy_and_interrupted_are_a_409() {
    let body = json!({"error": "interrupted turn", "state": "tool_call"});
    assert_eq!(
        prompt_error("interrupted turn: tool_call".into()),
        (409, body)
    );
    assert_eq!(prompt_error("session is busy: x".into()).0, 409);
    assert_eq!(prompt_error("boom".into()), (500, json!({"error": "boom"})));
}
