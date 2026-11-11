use super::*;
use crate::ledger::MemoryStore;

const T: i64 = 1_000_000;
const L: Lease = Lease {
    pid: 7,
    start_time: 1,
};

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

fn budget(tokens: u64, steps: u32) -> Budget {
    Budget {
        tokens,
        steps,
        deadline: T + 3600,
    }
}

/// A goal `g` with 1000 tokens, 100 steps, an hour, depth `max_depth`.
fn goal(max_depth: u32, max_live: u32) -> Delegation<MemoryStore> {
    goal_on(MemoryStore::default(), max_depth, max_live)
}

fn goal_on(store: MemoryStore, max_depth: u32, max_live: u32) -> Delegation<MemoryStore> {
    let mut d = Delegation::open(store).unwrap();
    let allow = s(&["write_*", "!write_file:secrets/*"]);
    d.begin(
        &Goal {
            id: "g",
            task: "ship it",
            budget: budget(1000, 100),
            allow: &allow,
            max_depth,
            max_live,
        },
        T,
    )
    .unwrap();
    d
}

fn small(max_depth: u32) -> Goal<'static> {
    Goal {
        id: "x",
        task: "t",
        budget: budget(10, 10),
        allow: &[],
        max_depth,
        max_live: 1,
    }
}

fn req<'a>(mode: Mode, tokens: Option<u64>, allow: &'a [String]) -> DelegateRequest<'a> {
    DelegateRequest {
        profile: "coder",
        task: "do part",
        mode,
        budget: BudgetRequest {
            tokens,
            steps: None,
            minutes: None,
        },
        profile_allow: allow,
        workspace: None,
    }
}

fn spawn(d: &mut Delegation<MemoryStore>, parent: &str, tokens: u64) -> Handle {
    let h = d
        .delegate(
            parent,
            &req(Mode::Execute, Some(tokens), &[]),
            Spend::default(),
            T,
        )
        .unwrap();
    d.start(&h.node_id, L, None, T).unwrap();
    h
}

fn report() -> Report {
    Report {
        summary: "added the parser".into(),
        files: s(&["src/a.rs"]),
        diff_ref: Some("branch feat/a".into()),
        checks: vec![Check {
            command: "cargo test".into(),
            result: "passed".into(),
        }],
        risks: vec![],
    }
}

#[test]
fn children_are_carved_from_the_parent_and_never_add_to_it() {
    let mut d = goal(2, 8);
    let a = spawn(&mut d, "g", 600);
    assert_eq!(a.budget.tokens, 600);
    // Only 400 left: a request for 600 is clamped, not added.
    let b = spawn(&mut d, "g", 600);
    assert_eq!(b.budget.tokens, 400);
    let e = d
        .delegate("g", &req(Mode::Execute, Some(1), &[]), Spend::default(), T)
        .unwrap_err();
    assert!(
        matches!(e, DelegationError::BudgetExhausted { what: "tokens", .. }),
        "{e}"
    );
    // `a` settles having spent 100: the other 500 come back to the parent.
    d.settle(
        &a.node_id,
        Outcome::Failed,
        Spend {
            tokens: 100,
            steps: 1,
        },
        None,
        None,
        T,
    )
    .unwrap();
    let c = spawn(&mut d, "g", 10_000);
    assert_eq!(c.budget.tokens, 500);
    // The parent's own spend counts too.
    let e = d
        .delegate(
            "g",
            &req(Mode::Execute, None, &[]),
            Spend {
                tokens: 1,
                steps: 0,
            },
            T,
        )
        .unwrap_err();
    assert!(matches!(e, DelegationError::BudgetExhausted { .. }), "{e}");
}

#[test]
fn a_settled_child_is_charged_its_whole_subtree_capped_at_its_carve() {
    let mut d = goal(3, 8);
    let a = spawn(&mut d, "g", 500);
    let a1 = spawn(&mut d, &a.node_id, 300);
    assert_eq!(a1.budget.tokens, 300);
    // a1 is still running when a settles: its 300 stay reserved.
    d.settle(
        &a.node_id,
        Outcome::Failed,
        Spend {
            tokens: 50,
            steps: 1,
        },
        None,
        None,
        T,
    )
    .unwrap();
    let rest = d
        .delegate(
            "g",
            &req(Mode::Execute, Some(10_000), &[]),
            Spend::default(),
            T,
        )
        .unwrap();
    assert_eq!(rest.budget.tokens, 1000 - 350);
}

#[test]
fn an_omitted_request_takes_half_of_what_is_left_and_the_deadline_is_capped() {
    let mut d = goal(2, 8);
    let mut r = req(Mode::Execute, None, &[]);
    r.budget.minutes = Some(10_000);
    let h = d.delegate("g", &r, Spend::default(), T).unwrap();
    assert_eq!((h.budget.tokens, h.budget.steps), (500, 50));
    assert_eq!(
        h.budget.deadline,
        T + 3600,
        "never past the parent's deadline"
    );
    r.budget.minutes = Some(5);
    let h = d.delegate("g", &r, Spend::default(), T).unwrap();
    assert_eq!(h.budget.deadline, T + 300);
    let e = d.delegate("g", &r, Spend::default(), T + 3600).unwrap_err();
    assert!(
        matches!(e, DelegationError::BudgetExhausted { what: "time", .. }),
        "{e}"
    );
    r.budget.tokens = Some(0);
    assert!(d.delegate("g", &r, Spend::default(), T).is_err());
}

#[test]
fn depth_stops_at_the_goal_limit_and_the_ceiling_is_three() {
    let mut d = goal(2, 8);
    let a = spawn(&mut d, "g", 500);
    assert!(d.may_delegate("g") && d.may_delegate(&a.node_id));
    let b = spawn(&mut d, &a.node_id, 100);
    assert_eq!((b.node_id.as_str(), b.depth), ("g.1.1", 2));
    assert!(!d.may_delegate(&b.node_id));
    let e = d
        .delegate(
            &b.node_id,
            &req(Mode::Execute, Some(1), &[]),
            Spend::default(),
            T,
        )
        .unwrap_err();
    assert!(
        matches!(
            e,
            DelegationError::DepthCap {
                depth: 2,
                max_depth: 2,
                ..
            }
        ),
        "{e}"
    );
    let mut d = Delegation::open(MemoryStore::default()).unwrap();
    for bad in [0, 4] {
        assert!(d.begin(&small(bad), T).is_err(), "{bad}");
    }
    d.begin(&small(3), T).unwrap();
    assert!(d.begin(&small(3), T).is_err(), "goal exists");
}

#[test]
fn the_child_allowlist_is_parent_intersect_profile_and_keeps_every_deny() {
    let profile = s(&["write_file", "run_command", "!edit_file", "*"]);
    let mut d = goal(2, 8);
    let h = d
        .delegate(
            "g",
            &req(Mode::Execute, Some(1), &profile),
            Spend::default(),
            T,
        )
        .unwrap();
    assert_eq!(
        h.allow,
        s(&["write_file", "!edit_file", "!write_file:secrets/*"])
    );
    assert_eq!(h.dropped, s(&["run_command", "*"]));
    // An empty profile list pre-authorizes nothing (writes ask).
    let (allow, _) = child_allow(&s(&["write_*"]), &[]);
    assert!(allow.is_empty());
}

#[test]
fn a_consult_node_cannot_spawn_an_executor_or_take_a_lease() {
    let mut d = goal(2, 8);
    let c = d
        .delegate("g", &req(Mode::Consult, Some(10), &[]), Spend::default(), T)
        .unwrap();
    d.start(&c.node_id, L, None, T).unwrap();
    assert!(d
        .delegate(
            &c.node_id,
            &req(Mode::Execute, Some(1), &[]),
            Spend::default(),
            T
        )
        .is_err());
    assert!(d.lease_write(&c.node_id, "/w", T).is_err());
}

#[test]
fn one_writer_per_path_and_leases_end_when_the_holder_settles() {
    let mut d = goal(2, 8);
    let a = spawn(&mut d, "g", 100);
    let b = spawn(&mut d, "g", 100);
    d.lease_write(&a.node_id, "/repo", T).unwrap();
    d.lease_write(&a.node_id, "/repo", T).unwrap(); // re-entrant
    for path in ["/repo", "/repo/src", "/"] {
        let e = d.lease_write(&b.node_id, path, T).unwrap_err();
        assert!(
            matches!(&e, DelegationError::LeaseBusy { holder, .. } if holder == &a.node_id),
            "{path}: {e}"
        );
    }
    d.lease_write(&b.node_id, "/repo-wt", T).unwrap(); // a sibling worktree
    assert!(
        d.lease_write(&a.node_id, "rel", T).is_err(),
        "absolute paths only"
    );
    d.release(&a.node_id, "/repo", T).unwrap();
    d.lease_write(&b.node_id, "/repo/src", T).unwrap();
    d.settle(
        &b.node_id,
        Outcome::Cancelled,
        Spend::default(),
        None,
        None,
        T,
    )
    .unwrap();
    d.lease_write(&a.node_id, "/repo", T).unwrap();
}

#[test]
fn two_processes_on_one_log_see_each_others_lease_and_the_first_lease_wins() {
    let store = MemoryStore::default();
    let mut one = goal_on(store.clone(), 2, 8);
    let a = spawn(&mut one, "g", 100);
    let b = spawn(&mut one, "g", 100);
    let mut two = Delegation::open(store.clone()).unwrap();
    two.lease_write(&b.node_id, "/repo", T).unwrap();
    let e = one.lease_write(&a.node_id, "/repo/x", T).unwrap_err();
    assert!(matches!(e, DelegationError::LeaseBusy { .. }), "{e}");
    // Two leases that raced past the check: the fold keeps the first.
    let mut raw = store.clone();
    let rec = Rec::WriteLease {
        node_id: a.node_id.clone(),
        path: "/repo".into(),
        at: T,
    };
    raw.append(format!("{}\n", rec.to_line()).as_bytes())
        .unwrap();
    one.refresh().unwrap();
    assert_eq!(one.state.leases.get("/repo"), Some(&b.node_id));
}

#[test]
fn ok_settles_done_only_with_a_complete_report_and_settled_children() {
    let mut d = goal(2, 8);
    let a = spawn(&mut d, "g", 100);
    assert_eq!(
        d.settle(&a.node_id, Outcome::Ok, Spend::default(), None, None, T)
            .unwrap(),
        NodeStatus::Incomplete
    );
    assert!(d
        .poll(&a.node_id)
        .unwrap()
        .reason
        .as_deref()
        .unwrap()
        .contains("no completion report"));
    assert!(
        matches!(
            d.settle(
                &a.node_id,
                Outcome::Ok,
                Spend::default(),
                Some(report()),
                None,
                T
            ),
            Err(DelegationError::Settled(_))
        ),
        "a terminal record is never overwritten"
    );

    let b = spawn(&mut d, "g", 100);
    let mut r = report();
    r.diff_ref = None;
    r.checks.clear();
    d.settle(&b.node_id, Outcome::Ok, Spend::default(), Some(r), None, T)
        .unwrap();
    let why = d.poll(&b.node_id).unwrap().reason.clone().unwrap();
    assert!(
        why.contains("no diff ref") && why.contains("no checks"),
        "{why}"
    );

    let c = spawn(&mut d, "g", 100);
    let c1 = spawn(&mut d, &c.node_id, 10);
    let st = d
        .settle(
            &c.node_id,
            Outcome::Ok,
            Spend::default(),
            Some(report()),
            None,
            T,
        )
        .unwrap();
    assert_eq!(st, NodeStatus::Incomplete, "a child is still running");
    d.settle(
        &c1.node_id,
        Outcome::Ok,
        Spend::default(),
        Some(report()),
        None,
        T,
    )
    .unwrap();
    assert_eq!(d.poll(&c1.node_id).unwrap().status, NodeStatus::Done);

    let consult = Report {
        summary: "the API is fine".into(),
        ..Report::default()
    };
    assert!(consult.problems(Mode::Consult).is_empty());
    assert!(
        !report().problems(Mode::Consult).is_empty(),
        "consult changed files"
    );
}

#[test]
fn the_live_process_guard_queues_and_recovery_settles_the_gone() {
    let mut d = goal(2, 1);
    let a = spawn(&mut d, "g", 100);
    let b = d
        .delegate(
            "g",
            &req(Mode::Execute, Some(100), &[]),
            Spend::default(),
            T,
        )
        .unwrap();
    let e = d.start(&b.node_id, L, None, T).unwrap_err();
    assert!(
        matches!(e, DelegationError::Queued { live: 1, max: 1 }),
        "{e}"
    );
    assert_eq!(d.next_queued("g").unwrap().id, b.node_id);
    // `a`'s process died: recovery settles it and frees the slot.
    let gone = d.recover(T, |l| l.pid != 7, |_| None).unwrap();
    assert_eq!(gone, vec![a.node_id.clone()]);
    assert_eq!(d.poll(&a.node_id).unwrap().status, NodeStatus::Interrupted);
    d.start(
        &b.node_id,
        Lease {
            pid: 8,
            start_time: 1,
        },
        Some("s-1"),
        T,
    )
    .unwrap();
    // A queued child of a settled parent is cancelled, never orphaned.
    let c = d
        .delegate(
            &b.node_id,
            &req(Mode::Execute, Some(1), &[]),
            Spend::default(),
            T,
        )
        .unwrap();
    d.settle(&b.node_id, Outcome::Failed, Spend::default(), None, None, T)
        .unwrap();
    assert_eq!(
        d.recover(T, |_| true, |_| None).unwrap(),
        vec![c.node_id.clone()]
    );
    assert_eq!(d.poll(&c.node_id).unwrap().status, NodeStatus::Cancelled);
}

#[test]
fn the_log_replays_to_the_same_tree_and_renders_the_audit() {
    let mut d = goal(2, 8);
    let a = spawn(&mut d, "g", 400);
    let a1 = spawn(&mut d, &a.node_id, 100);
    d.settle(
        &a1.node_id,
        Outcome::Ok,
        Spend {
            tokens: 40,
            steps: 2,
        },
        Some(report()),
        None,
        T,
    )
    .unwrap();
    let replay = Delegation::open(d.store.clone()).unwrap();
    assert_eq!(replay.tree("g"), d.tree("g"));
    let lines = replay.render_tree("g");
    assert_eq!(lines.len(), 3);
    assert!(
        lines[0].starts_with("g orchestrator Running"),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].starts_with("  g.1 coder Running [execute] tokens 400/400"),
        "{}",
        lines[1]
    );
    assert!(
        lines[2].starts_with("    g.1.1 coder Done [execute] tokens 40/100")
            && lines[2].contains("report: added the parser"),
        "{}",
        lines[2]
    );
    // Unknown kinds are skipped; a newer version of a known kind is refused.
    let mut fwd = MemoryStore::default();
    fwd.append(b"{\"kind\":\"future\",\"v\":9}\n").unwrap();
    assert!(Delegation::open(fwd).is_ok());
    let mut newer = MemoryStore::default();
    newer.append(b"{\"kind\":\"goal\",\"v\":2}\n").unwrap();
    assert!(Delegation::open(newer).is_err());
}

#[test]
fn only_normalized_absolute_lease_paths_are_accepted() {
    let mut d = goal(2, 8);
    let a = spawn(&mut d, "g", 100);
    let b = spawn(&mut d, "g", 100);
    d.lease_write(&a.node_id, "/work/src/a.rs", T).unwrap();
    // Other spellings of the same file are refused, not let through.
    for bad in [
        "/work/src/../src/a.rs",
        "/work/src/./a.rs",
        "//work/src/a.rs",
        "/work/src/a.rs/",
        "/work//src/a.rs",
        "work/src/a.rs",
        "",
    ] {
        let e = d.lease_write(&b.node_id, bad, T).unwrap_err();
        assert!(matches!(e, DelegationError::Invalid(_)), "{bad:?}: {e}");
        let e = d.release(&a.node_id, bad, T).unwrap_err();
        assert!(matches!(e, DelegationError::Invalid(_)), "{bad:?}: {e}");
    }
    // Normalized equal, ancestor and descendant paths still conflict.
    for path in ["/work/src/a.rs", "/work", "/work/src/a.rs/x", "/"] {
        let e = d.lease_write(&b.node_id, path, T).unwrap_err();
        assert!(
            matches!(e, DelegationError::LeaseBusy { .. }),
            "{path}: {e}"
        );
    }
    d.lease_write(&a.node_id, "/work/src/a.rs", T).unwrap(); // re-entrant
    d.lease_write(&b.node_id, "/work/src/b.rs", T).unwrap();
    // A forged record with a non-normal path never becomes a lease.
    let mut raw = d.store.clone();
    let rec = Rec::WriteLease {
        node_id: b.node_id.clone(),
        path: "//other/".into(),
        at: T,
    };
    raw.append(format!("{}\n", rec.to_line()).as_bytes())
        .unwrap();
    d.refresh().unwrap();
    assert!(!d.state.leases.contains_key("//other/"));
}

#[test]
fn a_second_settle_that_raced_past_the_check_does_not_overwrite_the_first() {
    let store = MemoryStore::default();
    let mut d = goal_on(store.clone(), 2, 8);
    let a = spawn(&mut d, "g", 100);
    d.settle(&a.node_id, Outcome::Failed, Spend::default(), None, None, T)
        .unwrap();
    let late = Rec::Settle {
        node_id: a.node_id.clone(),
        status: NodeStatus::Done,
        used: Spend::default(),
        report: Some(report()),
        reason: None,
        at: T,
    };
    let mut raw = store.clone();
    raw.append(format!("{}\n", late.to_line()).as_bytes())
        .unwrap();
    d.refresh().unwrap();
    assert_eq!(d.poll(&a.node_id).unwrap().status, NodeStatus::Failed);
    let fresh = Delegation::open(store).unwrap();
    assert_eq!(fresh.poll(&a.node_id).unwrap().status, NodeStatus::Failed);
}

/// What `g` can still carve: a probe request for more than everything.
fn left(d: &mut Delegation<MemoryStore>) -> u64 {
    let probe = DelegateRequest {
        profile: "probe",
        ..req(Mode::Consult, Some(u64::MAX), &[])
    };
    match d.delegate("g", &probe, Spend::default(), T) {
        Ok(h) => {
            // Give the probe back as never started.
            d.settle(
                &h.node_id,
                Outcome::Cancelled,
                Spend::default(),
                None,
                None,
                T,
            )
            .unwrap();
            h.budget.tokens
        }
        Err(DelegationError::BudgetExhausted { .. }) => 0,
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn a_crashed_child_is_charged_its_whole_carve_unless_measured() {
    let mut d = goal(2, 8);
    let a = spawn(&mut d, "g", 600);
    assert_eq!(left(&mut d), 400);
    // The process is gone and nobody measured it: nothing comes back.
    d.recover(T, |_| false, |_| None).unwrap();
    assert_eq!(d.poll(&a.node_id).unwrap().status, NodeStatus::Interrupted);
    assert_eq!(left(&mut d), 400, "a crash must not refund its carve");
    // A measured spend lower than the carve refunds the difference.
    let b = spawn(&mut d, "g", 300);
    let b_id = b.node_id.clone();
    d.recover(
        T,
        |_| false,
        move |n| {
            (n.id == b_id).then_some(Spend {
                tokens: 100,
                steps: 1,
            })
        },
    )
    .unwrap();
    assert_eq!(left(&mut d), 400 - 100);
}

#[test]
fn spawn_and_crash_cycles_run_out_of_budget() {
    let mut d = goal(2, 8);
    let mut cycles = 0;
    loop {
        let mut r = req(Mode::Execute, Some(100), &[]);
        r.budget.steps = Some(10);
        match d.delegate("g", &r, Spend::default(), T) {
            Ok(h) => {
                d.start(&h.node_id, L, None, T).unwrap();
                d.recover(T, |_| false, |_| None).unwrap();
                cycles += 1;
                assert!(cycles <= 10, "spawn+crash outlived a 1000-token goal");
            }
            Err(DelegationError::BudgetExhausted { .. }) => break,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(cycles, 10);
}

#[test]
fn a_queued_child_cancelled_before_it_started_refunds_in_full() {
    let mut d = goal(3, 8);
    let a = spawn(&mut d, "g", 600);
    let r = req(Mode::Execute, Some(300), &[]);
    let a1 = d.delegate(&a.node_id, &r, Spend::default(), T).unwrap();
    // `a` measured nothing spent and stopped; its queued child never ran.
    d.settle(&a.node_id, Outcome::Failed, Spend::default(), None, None, T)
        .unwrap();
    let gone = d.recover(T, |_| true, |_| None).unwrap();
    assert_eq!(gone, vec![a1.node_id.clone()]);
    assert_eq!(d.poll(&a1.node_id).unwrap().status, NodeStatus::Cancelled);
    assert_eq!(left(&mut d), 1000);
}

#[test]
fn a_budget_stop_is_charged_the_whole_carve_and_own_spend_only_grows() {
    let mut d = goal(2, 8);
    let a = spawn(&mut d, "g", 600);
    let small = Spend {
        tokens: 5,
        steps: 1,
    };
    d.settle(&a.node_id, Outcome::Budget, small, None, None, T)
        .unwrap();
    assert_eq!(left(&mut d), 400, "budget means the carve was spent");
    // The parent's own spend: a lower report than before is refused.
    let r = req(Mode::Execute, Some(10), &[]);
    let fifty = Spend {
        tokens: 50,
        steps: 5,
    };
    d.delegate("g", &r, fifty, T).unwrap();
    let lower = Spend {
        tokens: 10,
        steps: 5,
    };
    let e = d.delegate("g", &r, lower, T).unwrap_err();
    assert!(matches!(e, DelegationError::Invalid(_)), "{e}");
    // It survives a reopen of the log.
    let mut fresh = Delegation::open(d.store.clone()).unwrap();
    assert!(fresh.delegate("g", &r, lower, T).is_err());
    assert_eq!(fresh.poll("g").unwrap().own, fifty);
}

/// Appends a raw spawn record, as a forger with write access to the log would.
fn raw_spawn(store: &MemoryStore, node: &str, parent: &str, allow: &[&str], tokens: u64) {
    let rec = Rec::Spawn {
        node_id: node.into(),
        parent_id: parent.into(),
        profile: "forged".into(),
        task: "forged".into(),
        mode: Mode::Execute,
        budget: Budget {
            tokens,
            steps: 1,
            deadline: T + 60,
        },
        allow: s(allow),
        dropped: vec![],
        workspace: Some("/w".into()),
        parent_own: Spend::default(),
        at: T,
    };
    let mut raw = store.clone();
    raw.append(format!("{}\n", rec.to_line()).as_bytes())
        .unwrap();
}

#[test]
fn the_fold_ignores_a_spawn_that_breaks_a_cap() {
    let store = MemoryStore::default();
    let mut d = goal_on(store.clone(), 2, 8);
    let a = spawn(&mut d, "g", 600);
    let a1 = spawn(&mut d, &a.node_id, 100);
    let b = spawn(&mut d, "g", 100);
    d.settle(&b.node_id, Outcome::Failed, Spend::default(), None, None, T)
        .unwrap();
    let keep = "!write_file:secrets/*";
    for (node, parent, allow, tokens, why) in [
        (
            "g.3",
            "g",
            vec!["*", keep],
            10,
            "allow wider than the parent",
        ),
        (
            "g.3",
            "g",
            vec!["run_command", keep],
            10,
            "uncovered pattern",
        ),
        ("g.3", "g", vec!["write_file"], 10, "parent deny dropped"),
        (
            "g.3",
            "g",
            vec![keep],
            1_000_000_000,
            "budget past the carve",
        ),
        ("g.1.1.1", "g.1.1", vec![keep], 1, "depth past the cap"),
        ("zz.1", "zz", vec![keep], 1, "unknown parent"),
        ("g.2.1", "g.2", vec![keep], 1, "terminal parent"),
        ("g.9", "g", vec![keep], 1, "not the next id"),
        ("g.3", "g", vec![keep], 0, "empty budget"),
    ] {
        raw_spawn(&store, node, parent, &allow, tokens);
        d.refresh().unwrap();
        assert!(d.poll(node).is_none(), "{why}: {node} was accepted");
    }
    assert!(d.next_queued("g").is_none(), "nothing forged is queued");
    assert_eq!(d.poll(&a1.node_id).unwrap().children.len(), 0);
    // A spawn within every cap is still accepted (the legitimate shape).
    raw_spawn(&store, "g.3", "g", &["write_file", keep], 10);
    d.refresh().unwrap();
    assert_eq!(d.poll("g.3").unwrap().status, NodeStatus::Queued);
}

#[test]
fn a_raw_settle_cannot_charge_below_the_rules() {
    let store = MemoryStore::default();
    let mut d = goal_on(store.clone(), 2, 8);
    let a = spawn(&mut d, "g", 600);
    let late = Rec::Settle {
        node_id: a.node_id.clone(),
        status: NodeStatus::Budget,
        used: Spend::default(),
        report: None,
        reason: None,
        at: T,
    };
    let mut raw = store.clone();
    raw.append(format!("{}\n", late.to_line()).as_bytes())
        .unwrap();
    d.refresh().unwrap();
    assert_eq!(
        d.poll(&a.node_id).unwrap().used,
        Some(Spend {
            tokens: 600,
            steps: 50
        }),
        "a budget stop is its whole carve"
    );
}
