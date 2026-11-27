use super::*;
use crate::status::SessionStatus;

#[test]
fn a_settled_turn_maps_to_its_end() {
    let stop = |t: &str| TurnEnd::Stopped(t.into());
    assert_eq!(map(TurnOutcome::Detached), stop("turn detached"));
    assert_eq!(map(TurnOutcome::Cancelled), TurnEnd::Cancelled);
    let budget = map(TurnOutcome::BudgetExhausted);
    assert_eq!(budget, stop("step budget exhausted"));
}

#[test]
fn footer_carries_status_fields_and_effective_effort() {
    let st = SessionStatus {
        model: Some("m1".into()),
        effort: None,
        last_prompt_tokens: 41_000,
        window: Some(128_000),
        input_tokens: 10,
        output_tokens: 5,
        cached_tokens: 0,
        compactions: 0,
    };
    let f = footer_from(&st, Some("high".into()), "proj", "s-1");
    assert_eq!(f.model.as_deref(), Some("m1"));
    assert_eq!(f.effort.as_deref(), Some("high"));
    assert_eq!(f.context, "context: 41k / 128k (32%)");
    assert_eq!(f.tokens, "tokens: 10 in / 5 out");
    assert_eq!((f.folder.as_str(), f.session.as_str()), ("proj", "s-1"));
}

use crate::test_dir::TestDir;
fn tmp(tag: &str) -> TestDir {
    TestDir::new(&format!("tole-tuihost-{tag}"))
}

fn session(dir: &std::path::Path, id: &str) -> TuiSession {
    let storage = JsonlStorage::create_with(dir, id, None, Some("sys")).unwrap();
    let cfg = OpenAiConfig {
        base_url: "http://127.0.0.1:9/v1".into(),
        model: "m0".into(),
        api_key: "k".into(),
    };
    TuiSession::new(
        storage,
        ToolRegistry::new(),
        cfg,
        CompactionSettings::default(),
    )
    .with_sessions(dir.to_path_buf(), Some("sys".into()))
    .with_index(None)
}

#[test]
fn switches_write_the_registers_the_next_call_and_resume_read() {
    let dir = tmp("regs");
    let s = session(&dir, "s-a");
    assert_eq!(s.footer().model.as_deref(), Some("m0"));
    s.set_model("m2").unwrap();
    s.set_effort(Some("low")).unwrap();
    // the very next footer refresh reflects both
    let f = s.footer();
    assert_eq!(
        (f.model.as_deref(), f.effort.as_deref()),
        (Some("m2"), Some("low"))
    );
    assert!(s.set_effort(Some("loud")).is_err());
    assert!(s.set_model("bad\x1b[2Jid").is_err());
    assert!(s.set_model("  ").is_err());
    drop(s);
    // a resumed process sees both (what `tole resume` builds the provider from)
    let r = JsonlStorage::open(dir.join("s-a.jsonl")).unwrap();
    assert_eq!(crate::turn_runner::session_model(&r, "m0"), "m2");
    assert_eq!(
        crate::turn_runner::session_effort(&r, None).as_deref(),
        Some("low")
    );
    drop(r);
    let s = session_reopen(&dir, "s-a");
    s.set_effort(None).unwrap();
    assert_eq!(s.footer().effort, None, "unset falls back to the default");
}

fn session_reopen(dir: &std::path::Path, id: &str) -> TuiSession {
    let storage = JsonlStorage::open(dir.join(format!("{id}.jsonl"))).unwrap();
    let cfg = OpenAiConfig {
        base_url: "http://127.0.0.1:9/v1".into(),
        model: "m0".into(),
        api_key: "k".into(),
    };
    TuiSession::new(
        storage,
        ToolRegistry::new(),
        cfg,
        CompactionSettings::default(),
    )
    .with_sessions(dir.to_path_buf(), Some("sys".into()))
    .with_index(None)
}

#[test]
fn name_and_compact_request_are_durable_registers() {
    let dir = tmp("name");
    let s = session(&dir, "s-n");
    s.set_name("  nightly run ").unwrap();
    assert!(s.set_name("a\x1bb").is_err());
    s.request_compaction().unwrap();
    drop(s);
    let r = JsonlStorage::open(dir.join("s-n.jsonl")).unwrap();
    assert_eq!(r.session_name(), Some("nightly run"));
    assert!(tole_core::compaction::overflow_pending(&r));
}

#[test]
fn new_and_switch_keep_one_open_handle_per_session() {
    let dir = tmp("swap");
    let s = session(&dir, "s-1");
    let two = s.new_session().unwrap();
    assert_ne!(two, "s-1");
    assert_eq!(s.footer().session, two);
    // s-1 is closed: a second handle opens without `Locked`
    drop(JsonlStorage::open(dir.join("s-1.jsonl")).expect("old handle released"));
    // switching back releases `two` and re-opens s-1
    assert_eq!(s.switch_session("s-1").unwrap(), "s-1");
    drop(JsonlStorage::open(dir.join(format!("{two}.jsonl"))).expect("released"));
    // the same session again is a no-op, not a self-lock
    assert_eq!(s.switch_session("s-1").unwrap(), "s-1");
    assert!(s.switch_session("s-missing").is_err());
    assert!(s.switch_session("../x").is_err());
    assert_eq!(
        s.footer().session,
        "s-1",
        "a failed switch keeps the session"
    );
}

/// The single host call the UI makes (#526): every command reaches the
/// session operation it names, with a data-only reply.
#[test]
fn apply_dispatches_each_command_to_its_session_operation() {
    let dir = tmp("apply");
    let s = session(&dir, "s-1");

    assert_eq!(s.apply(HostCmd::SetModel("m2".into())), Ok(HostReply::Done));
    assert_eq!(
        s.apply(HostCmd::SetEffort(Some("low".into()))),
        Ok(HostReply::Done)
    );
    assert_eq!(
        s.apply(HostCmd::Name("nightly".into())),
        Ok(HostReply::Done)
    );
    assert_eq!(s.apply(HostCmd::Compact(None)), Ok(HostReply::Done));
    // what each one wrote, read back through the session's own readers
    let footer = s.footer();
    assert_eq!(footer.model.as_deref(), Some("m2"));
    assert_eq!(footer.effort.as_deref(), Some("low"));
    {
        let g = s.lock();
        assert_eq!(g.storage.session_name(), Some("nightly"));
        assert!(tole_core::compaction::overflow_pending(&g.storage));
    }
    // `/compact <instructions>` is accepted and the instructions ignored.
    assert_eq!(
        s.apply(HostCmd::Compact(Some("keep tests".into()))),
        Ok(HostReply::Done)
    );

    // clearing the effort goes back to the process default
    assert_eq!(s.apply(HostCmd::SetEffort(None)), Ok(HostReply::Done));

    // bad input is refused with the operation's own error, nothing replied
    assert!(s.apply(HostCmd::SetModel("bad\x1b[2Jid".into())).is_err());
    assert!(s.apply(HostCmd::SetEffort(Some("loud".into()))).is_err());
    assert!(s.apply(HostCmd::Name("a\x1bb".into())).is_err());

    // sessions: New and Switch answer with the session now in use
    let Ok(HostReply::Session(two)) = s.apply(HostCmd::New) else {
        panic!("New answers with the new session id");
    };
    assert_ne!(two, "s-1");
    assert_eq!(s.session_id(), two);
    assert_eq!(
        s.apply(HostCmd::Switch("s-1".into())),
        Ok(HostReply::Session("s-1".into()))
    );
    assert!(s.apply(HostCmd::Switch("s-missing".into())).is_err());
    assert_eq!(s.session_id(), "s-1", "a failed switch keeps the session");

    // status is lines; the picker needs the index, which this session was
    // built without
    let Ok(HostReply::Lines(lines)) = s.apply(HostCmd::Status) else {
        panic!("Status answers with lines");
    };
    assert!(lines.iter().any(|l| l == "session: s-1"), "{lines:?}");
    assert_eq!(
        s.apply(HostCmd::PickSession),
        Err("no session index".to_string())
    );
}

struct Quiet;
impl TurnObserver for Quiet {}

fn img(tag: &str) -> ImageRef {
    ImageRef {
        path: format!("/ws/{tag}.png"),
        mime: "image/png".into(),
        bytes: 33,
        fnv1a64: format!("{tag:0>16}"),
    }
}

fn user_messages(dir: &std::path::Path, id: &str) -> Vec<serde_json::Value> {
    let raw = std::fs::read_to_string(dir.join(format!("{id}.jsonl"))).unwrap();
    raw.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .flat_map(|v| match v {
            serde_json::Value::Array(a) => a,
            one => vec![one],
        })
        .map(|v| v["payload"].clone())
        .filter(|p| p["role"] == "user")
        .collect()
}

/// #559: images queued on the host ride on the NEXT message only, and a
/// message that never reached the log keeps its images queued.
#[test]
fn pending_images_ride_on_the_next_message_only() {
    let dir = tmp("images");
    let s = session(&dir, "s-img");
    let cancel = CancelToken::default();
    let quiet: Arc<dyn TurnObserver> = Arc::new(Quiet);

    // A path outside the (unset) jail is refused at attach time.
    assert!(s.attach_image("/definitely/not/here.png").is_err());
    assert!(s.pending_images().is_empty());

    // A turn parked mid-flight (as after a crash): the next turn settles
    // it first, which fails (the provider on port 9 is unreachable), so
    // the new message is never recorded — and its image stays queued
    // instead of being lost.
    {
        use tole_core::entry::{EntryType, NewEntry};
        use tole_core::state::{Pc, StateTransition};
        use tole_core::storage::Commit;
        let mut g = s.lock();
        let seq = g.storage.state().seq;
        g.storage
            .commit(
                Commit::new()
                    .entry(NewEntry::root(
                        EntryType::new(EntryType::MESSAGE),
                        serde_json::json!({"role": "user", "text": "parked"}),
                    ))
                    .transition(StateTransition::from(seq, Pc::Planning)),
            )
            .unwrap();
    }
    s.attach_image_ref(img("a"));
    let _ = s.run_turn("dropped", &cancel, Arc::clone(&quiet));
    assert_eq!(user_messages(&dir, "s-img").len(), 1, "only the parked one");
    assert_eq!(s.pending_images(), vec![img("a")], "kept for the resend");

    // The failed settle closed the parked turn: the next message runs,
    // and carries everything queued so far — the kept image first.
    s.attach_image_ref(img("b"));
    assert_eq!(s.pending_images(), vec![img("a"), img("b")]);
    let _ = s.run_turn("look", &cancel, Arc::clone(&quiet));
    assert!(s.pending_images().is_empty(), "taken by the turn");
    let users = user_messages(&dir, "s-img");
    assert_eq!(users.len(), 2);
    assert!(users[0].get("images").is_none());
    assert_eq!(users[1]["text"], "look");
    assert_eq!(
        users[1]["images"],
        serde_json::json!([img("a").to_value(), img("b").to_value()])
    );

    s.attach_image_ref(img("c"));
    assert_eq!(s.clear_pending_images(), 1);
    assert!(s.pending_images().is_empty());
}

/// #560: the TUI's image commands reach the #559 queue and answer with
/// the whole queue; a refused attach is the resolver's own error.
#[test]
fn apply_maps_the_image_commands_onto_the_pending_queue() {
    let dir = tmp("apply-img");
    let s = session(&dir, "s-ai");
    assert_eq!(s.apply(HostCmd::Images), Ok(HostReply::Images(vec![])));
    let refused = s.apply(HostCmd::AttachImage("/definitely/not/here.png".into()));
    assert!(
        matches!(&refused, Err(e) if e.starts_with("image: ")),
        "{refused:?}"
    );
    assert!(s.pending_images().is_empty());
    s.attach_image_ref(img("a"));
    let want = vec![tole_tui::ImageItem {
        path: "/ws/a.png".into(),
        mime: "image/png".into(),
        bytes: 33,
    }];
    assert_eq!(s.apply(HostCmd::Images), Ok(HostReply::Images(want)));
    assert_eq!(s.apply(HostCmd::ClearImages), Ok(HostReply::Images(vec![])));
    assert!(s.pending_images().is_empty());
}

#[test]
fn copy_and_export_read_the_open_session_without_touching_its_file() {
    use tole_core::entry::{EntryType, NewEntry};
    use tole_core::storage::Commit;
    let dir = tmp("export");
    let ws = dir.canonicalize().unwrap();
    let s = session(&dir, "s-1").with_workspace(ws.clone());
    assert_eq!(s.apply(HostCmd::LastAnswer), Ok(HostReply::Answer(None)));
    for (role, text) in [("user", "q <b>"), ("assistant", "the answer")] {
        let payload = serde_json::json!({ "role": role, "text": text });
        s.lock()
            .storage
            .commit(
                Commit::new().entry(NewEntry::root(EntryType::new(EntryType::MESSAGE), payload)),
            )
            .unwrap();
    }
    let file = dir.join("s-1.jsonl");
    let before = std::fs::read(&file).unwrap();
    assert_eq!(
        s.apply(HostCmd::LastAnswer),
        Ok(HostReply::Answer(Some("the answer".into())))
    );
    let want = ws.join(".tole/exports/s-1.html");
    assert_eq!(
        s.apply(HostCmd::Export(None)),
        Ok(HostReply::Path(want.display().to_string()))
    );
    let html = std::fs::read_to_string(&want).unwrap();
    assert!(html.contains("q &lt;b&gt;") && html.contains("the answer"));
    // JSONL equals the stored entries.
    let j = s.apply(HostCmd::Export(Some("t.jsonl".into())));
    assert!(matches!(j, Ok(HostReply::Path(_))), "{j:?}");
    let lines: Vec<tole_core::entry::Entry> = std::fs::read_to_string(ws.join("t.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines, s.lock().storage.entries());
    // A second run to the same path is refused; the session file is untouched.
    let again = s.apply(HostCmd::Export(Some("t.jsonl".into())));
    assert!(again.unwrap_err().contains("already exists"));
    assert!(s.apply(HostCmd::Export(Some("../x.html".into()))).is_err());
    assert_eq!(std::fs::read(&file).unwrap(), before);
}

#[test]
fn a_locked_target_is_refused_and_the_current_session_stays() {
    let dir = tmp("lock");
    let s = session(&dir, "s-1");
    let held = JsonlStorage::create_with(&dir, "s-2", None, Some("x")).unwrap();
    assert!(s.switch_session("s-2").is_err());
    assert_eq!(s.footer().session, "s-1");
    drop(held);
}

#[test]
fn session_rows_skip_the_current_one_and_label_names() {
    let dir = tmp("rows");
    let index = dir.join("index.jsonl");
    for id in ["s-1", "s-2"] {
        let st = JsonlStorage::create_with(&dir, id, None, Some("x")).unwrap();
        drop(st);
        crate::session_index::append(
            &index,
            &crate::session_index::Entry {
                id: id.into(),
                file: dir.join(format!("{id}.jsonl")),
                cwd: dir.to_path_buf(),
                face: "tui".into(),
                created_at: 1,
            },
        )
        .unwrap();
    }
    let rows = sessions_from(&index, "s-1");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "s-2");
}

#[test]
fn status_lines_reuse_the_session_status_snapshot() {
    let dir = tmp("status");
    let s = session(&dir, "s-st");
    s.set_effort(Some("high")).unwrap();
    let lines = s.status_lines();
    assert!(lines.iter().any(|l| l == "effort:  high"), "{lines:?}");
    assert!(lines.iter().any(|l| l.starts_with("model:")));
}

mod family;
