//! The host wiring for sibling change notices (#380).
//! Moved out of `src/` (#1249): these tests only use the public API of the crate.

use std::path::Path;
use std::time::Duration;
use tole_cli::sibling::{start_watcher, track, tracking_from_env, Tracking};
use tole_core::cancel::CancelToken;
use tole_core::sibling_watch::{Op, TouchRecorder, ENV_AGENT_ID, ENV_TOUCH_LOG};
use tole_core::tool::Tool;

fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |k| {
        pairs
            .iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| v.to_string())
    }
}

#[test]
fn tracking_needs_a_well_formed_id_and_a_log() {
    let ok = env_of(&[(ENV_AGENT_ID, "a-1f-2-0"), (ENV_TOUCH_LOG, "/x/touch.log")]);
    assert_eq!(
        tracking_from_env(&ok),
        Some(Tracking {
            agent: "a-1f-2-0".into(),
            log: "/x/touch.log".into()
        })
    );
    assert_eq!(tracking_from_env(&env_of(&[])), None);
    assert_eq!(
        tracking_from_env(&env_of(&[(ENV_AGENT_ID, "a-1")])),
        None,
        "no log"
    );
    for bad in ["", "A-1", "a b", "a\n[x]", "../x"] {
        let pairs = [(ENV_AGENT_ID, bad), (ENV_TOUCH_LOG, "/x")];
        let e = env_of(&pairs);
        assert_eq!(tracking_from_env(&e), None, "{bad:?}");
    }
}

#[test]
fn the_watcher_turns_a_sibling_write_into_a_queued_notice() {
    let dir = crate::test_dir::TestDir::new("tole-sibwire");
    let log = dir.join("tole-agents").join("touch.log");
    let mine = Tracking {
        agent: "a-1".into(),
        log: log.clone(),
    };
    let cancel = CancelToken::default();
    let _w = start_watcher(&mine, &cancel, Duration::from_millis(20));

    let me = TouchRecorder::new("a-1", &log, "/ws");
    let sib = TouchRecorder::new("a-2", &log, "/ws");
    me.record(Op::Read, "src/lib.rs").unwrap();
    sib.record(Op::Write, "src/lib.rs").unwrap();

    let mut got = Vec::new();
    crate::test_dir::wait_until(crate::test_dir::WAIT, "the sibling notice", || {
        got.extend(cancel.interrupts().drain());
        !got.is_empty()
    });
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].source, tole_core::interrupt::Source::Notice);
    assert!(got[0].text.contains("child agent a-2"), "{}", got[0].text);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn track_is_the_identity_outside_a_child() {
    struct T;
    impl Tool for T {
        fn name(&self) -> &str {
            "read_file"
        }
        fn risk(&self) -> tole_core::tool::Risk {
            tole_core::tool::Risk::ReadOnly
        }
        fn execute(&self, i: serde_json::Value) -> Result<serde_json::Value, String> {
            Ok(i)
        }
    }
    let t = track(Box::new(T), None, Path::new("/ws"));
    assert_eq!(t.name(), "read_file");
    assert_eq!(
        t.execute(serde_json::json!({"path": "x"})).unwrap()["path"],
        "x"
    );
}
