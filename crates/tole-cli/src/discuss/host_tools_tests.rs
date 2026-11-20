//! The driver and a discussion whose owner allowed its helpers connected
//! tools (#1039): every helper is an independent child started with the
//! scope the ledger gives it, the owner's answers are copied into the ledger
//! once each, and a resume replays them.

use super::*;
use tole_core::discussion::ToolVerdict;

const SEARCH: &str = "mcp_nginjen_search";

/// A runner that records each scoped start and answers at once. `answers`
/// are the decisions it reports for a handle, once the helper has "asked".
#[derive(Default)]
struct Helpers {
    starts: Mutex<Vec<(String, String, DiscussionScope)>>,
    plain: Mutex<u32>,
    answers: Mutex<Vec<(String, Vec<ToolDecision>)>>,
}

impl Helpers {
    /// Helper number `n` (1 based) reports `d` from its first poll on.
    fn answers(&self, n: usize, d: Vec<ToolDecision>) {
        self.answers.lock().unwrap().push((format!("node-{n}"), d));
    }
}

impl Independent for Helpers {
    fn start(&self, _: &str, _: &str, _: u64, _: &str) -> Result<String, String> {
        *self.plain.lock().unwrap() += 1;
        Err("a helper with an allowance is never started unscoped".into())
    }
    fn start_scoped(
        &self,
        who: &str,
        task: &str,
        _: u64,
        _: &str,
        scope: &DiscussionScope,
    ) -> Result<String, String> {
        let mut s = self.starts.lock().unwrap();
        s.push((who.into(), task.into(), scope.clone()));
        Ok(format!("node-{}", s.len()))
    }
    fn find(&self, _: &str) -> Result<Option<String>, String> {
        Ok(None)
    }
    fn poll(&self, handle: &str) -> Result<Option<(String, u64)>, String> {
        Ok(Some((format!("{handle} looked it up"), 500)))
    }
    fn decisions(&self, handle: &str) -> Result<Vec<ToolDecision>, String> {
        Ok(self
            .answers
            .lock()
            .unwrap()
            .iter()
            .filter(|(h, _)| h == handle)
            .flat_map(|(_, d)| d.clone())
            .collect())
    }
}

fn answer(req: &str, verdict: ToolVerdict) -> ToolDecision {
    ToolDecision {
        req: req.into(),
        tool: SEARCH.into(),
        verdict,
    }
}

fn with_tools(budget: u64) -> Contract {
    contract(budget, Some(1))
        .with_tools(&[SEARCH.to_string()])
        .unwrap()
}

fn seated(l: DiscussionLog, runner: &Arc<Helpers>) -> Driver {
    let (_, make) = script(&[("checker", vec![checker_met(1)])]);
    driver(l, make, 10_000_000)
        .with_independent(
            &["coder".to_string(), "ops".to_string()],
            Arc::clone(runner) as Arc<dyn Independent>,
        )
        .unwrap()
        .with_poll_every(std::time::Duration::ZERO)
}

#[test]
fn the_first_approval_is_in_the_ledger_and_the_next_helper_starts_with_it_granted() {
    let h = Arc::new(Helpers::default());
    h.answers(1, vec![answer("dap-1", ToolVerdict::Allow)]);
    let (l, _dir) = log("tools-allow");
    let d = seated(l.clone(), &h);
    d.start(with_tools(1_000_000)).unwrap();
    d.run().unwrap();
    let starts = h.starts.lock().unwrap().clone();
    assert_eq!(starts.len(), 2, "both helpers are independent children");
    assert_eq!(*h.plain.lock().unwrap(), 0);
    // The first helper may ask for the tool and has none granted yet; the
    // second starts with it granted by the answer the first one got.
    assert_eq!(starts[0].2.tools, [SEARCH]);
    assert!(starts[0].2.granted.is_empty());
    assert_eq!(starts[1].2.granted, [SEARCH]);
    assert!(starts.iter().all(|s| s.2.id == "pick-db"));
    assert!(
        starts[0].1.contains(SEARCH) && starts[0].1.contains("shown to the owner"),
        "{}",
        starts[0].1
    );
    let st = l.state().unwrap();
    assert_eq!(st.tool_approvals.len(), 1, "once, however often it is seen");
    assert_eq!(st.tool_approvals[0].verdict, ToolVerdict::Allow);
    // The ledger is closed, so the grant has ended with the discussion.
    assert!(st.closed.is_some() && st.tool_grants().is_empty());
}

#[test]
fn a_denial_or_an_expiry_is_recorded_and_the_next_helper_asks_again() {
    for verdict in [ToolVerdict::Deny, ToolVerdict::Expired] {
        let h = Arc::new(Helpers::default());
        h.answers(1, vec![answer("dap-1", verdict)]);
        let (l, _dir) = log(&format!("tools-{verdict:?}"));
        let d = seated(l.clone(), &h);
        d.start(with_tools(1_000_000)).unwrap();
        d.run().unwrap();
        let starts = h.starts.lock().unwrap().clone();
        assert!(starts[1].2.granted.is_empty(), "{verdict:?}");
        let st = l.state().unwrap();
        assert_eq!(st.tool_approvals[0].verdict, verdict);
    }
}

#[test]
fn a_resume_replays_the_recorded_approval_and_never_records_it_twice() {
    let h = Arc::new(Helpers::default());
    h.answers(1, vec![answer("dap-1", ToolVerdict::Allow)]);
    let (l, _dir) = log("tools-resume");
    // The first driver gets as far as the first helper, then the host dies
    // before the second one speaks.
    let d = seated(l.clone(), &h);
    d.start(with_tools(1_000_000)).unwrap();
    let st = l.state().unwrap();
    d.contribute(&st, 1, "coder").unwrap();
    assert_eq!(l.state().unwrap().tool_grants().len(), 1);
    // A new driver on the same ledger starts the next helper granted, and the
    // queue still reports the old answer (node-1 is polled by nobody now, but
    // a bound helper would be): it is not recorded again.
    let h2 = Arc::new(Helpers::default());
    h2.answers(1, vec![answer("dap-1", ToolVerdict::Allow)]);
    let d2 = seated(l.clone(), &h2);
    d2.run().unwrap();
    let starts = h2.starts.lock().unwrap().clone();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].2.granted, [SEARCH]);
    let st = l.state().unwrap();
    assert_eq!(st.tool_approvals.len(), 1);
}

#[test]
fn inline_voices_and_a_runner_without_tools_are_refused_not_run_without_them() {
    // A contract with tools and a participant that is not a helper.
    let h = Arc::new(Helpers::default());
    let (l, dir) = log("tools-inline");
    let (_, make) = script(&[]);
    let d = driver(l, make, 1_000_000)
        .with_independent(&["coder".to_string()], h.clone() as Arc<dyn Independent>)
        .unwrap();
    let e = d.start(with_tools(100_000)).unwrap_err();
    assert!(
        e.contains("`ops` is not seated as an independent helper"),
        "{e}"
    );
    assert!(!dir.join("pick-db.jsonl").exists(), "nothing was written");
    // A runner that cannot give tools refuses to start a scoped helper.
    struct Plain;
    impl Independent for Plain {
        fn start(&self, _: &str, _: &str, _: u64, _: &str) -> Result<String, String> {
            Ok("node".into())
        }
        fn find(&self, _: &str) -> Result<Option<String>, String> {
            Ok(None)
        }
        fn poll(&self, _: &str) -> Result<Option<(String, u64)>, String> {
            Ok(None)
        }
    }
    let (l, _d) = log("tools-plain");
    let (_, make) = script(&[]);
    let d = driver(l.clone(), make, 1_000_000)
        .with_independent(
            &["coder".to_string(), "ops".to_string()],
            Arc::new(Plain) as Arc<dyn Independent>,
        )
        .unwrap();
    d.start(with_tools(100_000)).unwrap();
    let e = d.run().unwrap_err();
    assert!(e.contains("cannot give a helper tools"), "{e}");
    // A name that is not a connected tool's, planted in a ledger, is refused.
    let mut bad = contract(100_000, None);
    bad.tools = vec!["write_file".into()];
    let (l, _d) = log("tools-badname");
    let (_, make) = script(&[]);
    let d = driver(l, make, 1_000_000);
    assert!(d
        .start(bad)
        .unwrap_err()
        .contains("not a connected tool name"));
}
