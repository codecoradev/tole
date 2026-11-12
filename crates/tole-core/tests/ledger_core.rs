//! Automation ledger (issue #578): claim exclusivity, never-overwritten
//! `done`, catch-up accounting, fold properties and crash injection at
//! every append boundary.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Map;
use tole_core::ledger::{
    CatchUp, Delivery, DeliveryStatus, Done, Due, Fire, HookSeen, Job, Lease, Ledger, LedgerError,
    LedgerState, LedgerStore, MemoryStore, MissReason, Missed, Record, Resume, RunStatus,
};

/// 2026-01-01T00:00:00Z.
const T: i64 = 1_767_225_600;
const P1: Lease = Lease {
    pid: 1,
    start_time: 100,
};
const P2: Lease = Lease {
    pid: 2,
    start_time: 200,
};

fn job(id: &str, schedule: &str, at: i64) -> Job {
    Job {
        id: id.into(),
        schedule: schedule.into(),
        timezone: None,
        catch_up: CatchUp::Skip,
        grace_minutes: 0,
        paused: false,
        removed: false,
        at,
        extra: Map::new(),
    }
}

fn fresh(jobs: &[Job]) -> Ledger<MemoryStore> {
    let mut l = Ledger::open(MemoryStore::default()).unwrap();
    for j in jobs {
        l.put_job(j.clone()).unwrap();
    }
    l
}

fn lines(store: &MemoryStore) -> Vec<Record> {
    String::from_utf8(store.bytes())
        .unwrap()
        .lines()
        .filter_map(|l| Record::parse_line(l).ok().flatten())
        .collect()
}

#[test]
fn claim_settle_deliver_lifecycle() {
    let mut l = fresh(&[job("a", "*/5 * * * *", T)]);
    assert!(l.ready(T + 299).unwrap().is_empty(), "not due yet");
    let due = l.ready(T + 300).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].fire, Some(T + 300));
    let fire = l.claim(&due[0], P1, T + 301).unwrap().unwrap();

    // The second claim of the same occurrence is refused deterministically.
    match l.claim(&due[0], P1, T + 302) {
        Err(LedgerError::AlreadyClaimed { fire_id, .. }) => assert_eq!(fire_id, fire.fire_id),
        other => panic!("second claim must be refused, got {other:?}"),
    }
    // A running job is not offered again (no overlapping runs).
    assert!(l.ready(T + 600).unwrap().is_empty());

    l.settle(
        &fire.fire_id,
        RunStatus::Ok,
        Some("s1".into()),
        None,
        T + 400,
    )
    .unwrap();
    // A done is never overwritten: a later error/interrupt is refused.
    for st in [RunStatus::Error, RunStatus::Interrupted] {
        assert!(matches!(
            l.settle(&fire.fire_id, st, None, None, T + 401),
            Err(LedgerError::AlreadySettled {
                status: RunStatus::Ok,
                ..
            })
        ));
    }
    // A failed delivery never changes the run status.
    let d = l
        .deliver(
            &fire.fire_id,
            "file",
            DeliveryStatus::Failed,
            Some("disk".into()),
            T + 402,
        )
        .unwrap();
    assert_eq!(d.attempt, 1);
    let d2 = l
        .deliver(&fire.fire_id, "file", DeliveryStatus::Sent, None, T + 403)
        .unwrap();
    assert_eq!(d2.attempt, 2);
    let s = l.state();
    assert_eq!(s.done(&fire.fire_id).unwrap().status, RunStatus::Ok);
    assert_eq!(
        s.last_outcome("a").unwrap().session_id.as_deref(),
        Some("s1")
    );
    assert_eq!(s.deliveries(&fire.fire_id).count(), 2);
    assert_eq!(s.next_run("a"), Some(T + 600));
}

#[test]
fn concurrent_ticks_cannot_double_claim() {
    let store = MemoryStore::default();
    let mut a = Ledger::open(store.clone()).unwrap();
    a.put_job(job("j", "* * * * *", T)).unwrap();
    let mut b = Ledger::open(store.clone()).unwrap();
    let (da, db) = (a.ready(T + 60).unwrap(), b.ready(T + 60).unwrap());
    assert_eq!(da, db, "both ticks see the same due occurrence");
    assert!(a.claim(&da[0], P1, T + 60).unwrap().is_some());
    assert!(matches!(
        b.claim(&db[0], P2, T + 60),
        Err(LedgerError::AlreadyClaimed { .. })
    ));
    assert_eq!(
        lines(&store)
            .iter()
            .filter(|r| matches!(r, Record::Fire(_)))
            .count(),
        1
    );
}

/// A store whose first append lets a competitor's line land first — the
/// interleaving a tick sees when it races past the host lock.
struct RacingStore {
    inner: MemoryStore,
    competitor: Option<String>,
}
impl LedgerStore for RacingStore {
    fn read_from(&mut self, o: u64) -> Result<Vec<u8>, LedgerError> {
        self.inner.read_from(o)
    }
    fn append(&mut self, line: &[u8]) -> Result<(), LedgerError> {
        if let Some(c) = self.competitor.take() {
            self.inner.append(format!("{c}\n").as_bytes())?;
        }
        self.inner.append(line)
    }
    fn truncate(&mut self, len: u64) -> Result<(), LedgerError> {
        self.inner.truncate(len)
    }
}

#[test]
fn claim_that_loses_a_race_past_the_lock_is_refused() {
    let inner = MemoryStore::default();
    let mut setup = Ledger::open(inner.clone()).unwrap();
    setup.put_job(job("j", "* * * * *", T)).unwrap();
    let competitor = Record::Fire(Fire {
        fire_id: "j@other".into(),
        job_id: "j".into(),
        scheduled_for: T + 60,
        claimed_at: T + 60,
        lease: P2,
    });
    let mut l = Ledger::open(RacingStore {
        inner: inner.clone(),
        competitor: Some(competitor.to_line()),
    })
    .unwrap();
    let due = l.ready(T + 60).unwrap();
    match l.claim(&due[0], P1, T + 60) {
        Err(LedgerError::LostRace { winner }) => assert_eq!(winner, "j@other"),
        other => panic!("expected LostRace, got {other:?}"),
    }
    let fold = Ledger::open(inner).unwrap();
    assert_eq!(fold.state().claim_of("j", T + 60), Some("j@other"));
    assert_eq!(fold.state().pending().count(), 1, "only the winner runs");
}

#[test]
fn catch_up_skip_and_one_always_record_missed() {
    // Hourly job, host down for three hours.
    let mut skip = fresh(&[job("h", "0 * * * *", T)]);
    let due = skip.ready(T + 3 * 3_600 + 30).unwrap();
    assert_eq!(due[0].fire, Some(T + 3 * 3_600), "on time: latest runs");
    let m = due[0].missed.as_ref().unwrap();
    assert_eq!(
        (m.scheduled_for, m.last, m.count),
        (T + 3_600, T + 7_200, 2)
    );
    assert_eq!(m.reason, MissReason::Superseded);

    let late = skip.ready(T + 3 * 3_600 + 300).unwrap();
    assert_eq!(late[0].fire, None, "skip never runs late");
    assert_eq!(late[0].missed.as_ref().unwrap().reason, MissReason::Late);
    assert_eq!(late[0].missed.as_ref().unwrap().count, 3);
    assert_eq!(skip.claim(&late[0], P1, T + 3 * 3_600 + 300).unwrap(), None);
    assert!(
        skip.ready(T + 3 * 3_600 + 301).unwrap().is_empty(),
        "accounted"
    );
    assert_eq!(skip.state().missed().count(), 1);

    let mut one = fresh(&[Job {
        catch_up: CatchUp::One,
        grace_minutes: 10,
        ..job("h", "0 * * * *", T)
    }]);
    let due = one.ready(T + 3 * 3_600 + 300).unwrap();
    assert_eq!(
        due[0].fire,
        Some(T + 3 * 3_600),
        "one late run within grace"
    );
    assert_eq!(due[0].missed.as_ref().unwrap().count, 2);
    let beyond = one.ready(T + 3 * 3_600 + 601).unwrap();
    assert_eq!(beyond[0].fire, None, "beyond grace: missed");
    one.claim(&due[0], P1, T + 3 * 3_600 + 300)
        .unwrap()
        .unwrap();
    assert_eq!(one.state().missed().count(), 1);
    assert_eq!(one.state().accounted("h"), Some(T + 3 * 3_600));
}

fn missed_total(l: &Ledger<MemoryStore>) -> u64 {
    l.state().missed().map(|m| m.count).sum()
}

#[test]
fn editing_an_active_job_records_what_it_still_owed() {
    // Daily 09:00 job created day 0 08:00; nothing ticks until day 3 09:30.
    let (created, now) = (T + 8 * 3_600, T + 3 * 86_400 + 9 * 3_600 + 1_800);
    let daily = || job("d", "0 9 * * *", created);
    let mut control = fresh(&[daily()]);
    let due = control.ready(now).unwrap();
    control.claim(&due[0], P1, now).unwrap();
    assert_eq!(missed_total(&control), 4, "control: four late occurrences");

    // Same job, only a host field edited a minute before the tick.
    let mut edited = fresh(&[daily()]);
    let mut v2 = Job {
        at: now - 60,
        ..daily()
    };
    v2.extra.insert("prompt".into(), "new prompt".into());
    edited.put_job(v2).unwrap();
    assert!(edited.ready(now).unwrap().is_empty());
    assert_eq!(
        missed_total(&edited),
        missed_total(&control),
        "no silent skip"
    );
    let m = edited.state().missed().next().unwrap();
    assert_eq!(
        (m.scheduled_for, m.last, m.reason),
        (
            T + 9 * 3_600,
            T + 3 * 86_400 + 9 * 3_600,
            MissReason::Edited
        )
    );
}

#[test]
fn edit_accounting_uses_the_previous_version_only() {
    // A fresh job edited before anything is owed: no spurious record.
    let mut l = fresh(&[job("f", "0 * * * *", T)]);
    l.put_job(job("f", "0 * * * *", T + 10)).unwrap();
    assert_eq!(l.state().missed().count(), 0);

    // A schedule change accounts the old schedule's owed occurrences
    // (hourly: T+1h..T+3h), not the new one's (daily: none in the window).
    let mut l = fresh(&[job("s", "0 * * * *", T)]);
    l.put_job(job("s", "@daily", T + 3 * 3_600 + 1_800))
        .unwrap();
    let m: Vec<_> = l.state().missed().cloned().collect();
    assert_eq!(m.len(), 1);
    assert_eq!(
        (m[0].scheduled_for, m[0].last, m[0].count),
        (T + 3_600, T + 3 * 3_600, 3)
    );

    // Pausing an active job records what it owed; the paused period itself
    // is not owed, so resuming records nothing more.
    let mut l = fresh(&[job("p", "0 * * * *", T)]);
    l.put_job(Job {
        paused: true,
        ..job("p", "0 * * * *", T + 2 * 3_600 + 5)
    })
    .unwrap();
    assert_eq!(missed_total(&l), 2);
    l.put_job(job("p", "0 * * * *", T + 9 * 3_600 + 5)).unwrap();
    assert_eq!(missed_total(&l), 2, "resume from paused records nothing");
    assert_eq!(
        l.ready(T + 10 * 3_600).unwrap()[0].fire,
        Some(T + 10 * 3_600)
    );
}

/// #581: who created a job and the grant it was created under are fixed by
/// the line that created it. A later line for the same live job, hand
/// written or not, cannot change them; a job saved again after its removal
/// starts afresh.
#[test]
fn a_later_line_cannot_change_who_created_a_job() {
    let with = |at: i64, by: &str, grant: &[&str], removed: bool| {
        let mut j = job("g", "0 * * * *", at);
        j.extra.insert("created_by".into(), by.into());
        j.extra
            .insert("creator_allow".into(), serde_json::json!(grant));
        j.removed = removed;
        j
    };
    let mut l = fresh(&[with(T, "agent", &["write_file"], false)]);
    l.put_job(with(T + 10, "cli", &["write_file", "edit_file"], false))
        .unwrap();
    let extra = |l: &Ledger<MemoryStore>| l.state().job("g").unwrap().job.extra.clone();
    assert_eq!(extra(&l)["created_by"], "agent");
    assert_eq!(
        extra(&l)["creator_allow"],
        serde_json::json!(["write_file"])
    );
    // A line that drops the fields does not drop them either.
    l.put_job(job("g", "0 * * * *", T + 20)).unwrap();
    assert_eq!(extra(&l)["created_by"], "agent");
    // The fold gives the same answer on replay.
    let replayed = LedgerState::fold(&lines(l.store()));
    assert_eq!(replayed.job("g").unwrap().job.extra["created_by"], "agent");
    // Removed, then saved again: a new job.
    l.put_job(with(T + 30, "agent", &["write_file"], true))
        .unwrap();
    l.put_job(with(T + 40, "cli", &[], false)).unwrap();
    assert_eq!(extra(&l)["created_by"], "cli");
    assert_eq!(extra(&l)["creator_allow"], serde_json::json!([]));
}

#[test]
fn pause_resume_and_remove_are_new_records() {
    let mut l = fresh(&[job("p", "0 * * * *", T)]);
    l.put_job(Job {
        paused: true,
        ..job("p", "0 * * * *", T + 10)
    })
    .unwrap();
    assert!(l.ready(T + 3_600).unwrap().is_empty());
    // Resume later: occurrences during the pause are not owed.
    l.put_job(job("p", "0 * * * *", T + 3 * 3_600 + 1)).unwrap();
    assert!(l.ready(T + 4 * 3_600 - 1).unwrap().is_empty());
    assert_eq!(l.ready(T + 4 * 3_600).unwrap()[0].fire, Some(T + 4 * 3_600));
    l.put_job(Job {
        removed: true,
        ..job("p", "0 * * * *", T + 5 * 3_600)
    })
    .unwrap();
    assert!(l.ready(T + 9 * 3_600).unwrap().is_empty());
    let jobs = lines(l.store())
        .iter()
        .filter(|r| matches!(r, Record::Job(_)))
        .count();
    assert_eq!(jobs, 4, "every change is appended, nothing deleted");
}

#[test]
fn invalid_jobs_are_rejected_on_save() {
    let mut l = fresh(&[]);
    for (j, want) in [
        (job("x", "0 0 30 2 *", T), "never fires"),
        (job("bad id", "@hourly", T), "must be 1-64"),
        (
            Job {
                timezone: Some("Asia/Jakarta".into()),
                ..job("x", "@hourly", T)
            },
            "#580",
        ),
        (
            Job {
                grace_minutes: 100_000,
                ..job("x", "@hourly", T)
            },
            "grace_minutes",
        ),
        (
            Job {
                extra: [("kind".to_string(), "fire".into())].into_iter().collect(),
                ..job("x", "@hourly", T)
            },
            "collides",
        ),
    ] {
        let e = l.put_job(j).unwrap_err().to_string();
        assert!(e.contains(want), "{e}");
    }
    assert!(l.store().bytes().is_empty());
}

#[test]
fn recover_interrupts_only_dead_leases() {
    let mut l = fresh(&[job("a", "* * * * *", T), job("b", "* * * * *", T)]);
    let mut fires = Vec::new();
    for (d, lease) in l.ready(T + 60).unwrap().into_iter().zip([P1, P2]) {
        fires.push(l.claim(&d, lease, T + 60).unwrap().unwrap());
    }
    // P1 is gone; P2's pid was reused by another process (start mismatch).
    let alive = |x: &Lease| {
        *x == Lease {
            pid: 2,
            start_time: 999,
        }
    };
    let rec = l.recover(T + 120, alive).unwrap();
    assert_eq!(rec.len(), 2);
    assert!(rec.iter().all(|d| d.status == RunStatus::Interrupted));
    // An owner that is alive is never touched (no timeouts).
    let mut l = fresh(&[job("c", "* * * * *", T)]);
    let d = l.ready(T + 60).unwrap();
    let f = l.claim(&d[0], P1, T + 60).unwrap().unwrap();
    assert!(l.recover(T + 86_400 * 30, |x| *x == P1).unwrap().is_empty());
    l.settle(&f.fire_id, RunStatus::Ok, None, None, T + 86_400 * 30)
        .unwrap();
}

#[test]
fn fold_skips_unknown_kinds_and_recovers_a_torn_tail() {
    let store = MemoryStore::default();
    let mut l = Ledger::open(store.clone()).unwrap();
    l.put_job(job("a", "* * * * *", T)).unwrap();
    let mut s = store.clone();
    s.append(b"{\"v\":9,\"kind\":\"future_thing\",\"x\":1}\n")
        .unwrap();
    s.append(b"\n{\"v\":1,\"kind\":\"fire\",\"fire_id\":\"to")
        .unwrap();
    let mut l = Ledger::open(store.clone()).unwrap();
    assert_eq!(l.state().unknown, 1);
    assert!(l.state().pending().next().is_none(), "torn fire ignored");
    let torn_len = store.bytes().len();
    let d = l.ready(T + 60).unwrap();
    assert!(
        store.bytes().len() < torn_len,
        "mutating call truncated the tail"
    );
    l.claim(&d[0], P1, T + 60).unwrap().unwrap();
    let text = String::from_utf8(store.bytes()).unwrap();
    assert!(text.ends_with('\n'));
    for line in text.lines().filter(|x| !x.is_empty()) {
        Record::parse_line(line).unwrap();
    }

    // A known kind from a newer format is refused; a corrupt middle line too.
    let mut s = MemoryStore::default();
    s.append(b"{\"v\":2,\"kind\":\"done\"}\n").unwrap();
    assert!(matches!(
        Ledger::open(s),
        Err(LedgerError::NewerVersion { found: 2, .. })
    ));
    let mut s = MemoryStore::default();
    s.append(
        b"{oops\n{\"v\":1,\"kind\":\"hook_seen\",\"route\":\"r\",\"delivery_id\":\"d\",\"at\":1}\n",
    )
    .unwrap();
    assert!(matches!(
        Ledger::open(s),
        Err(LedgerError::Corrupt { offset: 0, .. })
    ));
}

#[test]
fn hooks_are_idempotent_across_restarts() {
    let store = MemoryStore::default();
    let mut l = Ledger::open(store.clone()).unwrap();
    assert!(l.record_hook("gh", "d1", T).unwrap());
    assert!(!l.record_hook("gh", "d1", T + 1).unwrap());
    let mut again = Ledger::open(store).unwrap();
    assert!(!again.record_hook("gh", "d1", T + 2).unwrap());
    assert!(again.record_hook("gh", "d2", T + 2).unwrap());
}

// ---- properties ----------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            v.swap(i, self.below(i as u64 + 1) as usize);
        }
    }
}

/// A canonical, order-free view of everything the fold exposes.
fn summary(s: &LedgerState, recs: &[Record]) -> String {
    let mut out = BTreeMap::new();
    for r in recs {
        if let Record::Fire(f) = r {
            out.insert(
                format!("fire {}", f.fire_id),
                format!(
                    "{:?} {:?} {:?}",
                    s.claim_of(&f.job_id, f.scheduled_for),
                    s.done(&f.fire_id),
                    s.deliveries(&f.fire_id).collect::<Vec<_>>()
                ),
            );
            out.insert(
                format!("job {}", f.job_id),
                format!(
                    "{:?} {:?}",
                    s.accounted(&f.job_id),
                    s.last_outcome(&f.job_id)
                ),
            );
        }
        if let Record::HookSeen(h) = r {
            out.insert(
                format!("hook {}", h.delivery_id),
                s.hook_seen(&h.route, &h.delivery_id).to_string(),
            );
        }
    }
    let pending: BTreeSet<_> = s.pending().map(|f| f.fire_id.clone()).collect();
    format!(
        "{out:?}\n{pending:?}\n{:?}\n{:?}",
        s.missed().collect::<Vec<_>>(),
        s.blocked().collect::<Vec<_>>()
    )
}

fn random_records(r: &mut Rng) -> Vec<Record> {
    let mut v = Vec::new();
    for i in 0..(5 + r.below(30)) {
        let job_id = format!("j{}", r.below(4));
        let at = T + 60 * i as i64;
        let fire_id = format!("{job_id}@{at}");
        v.push(Record::Fire(Fire {
            fire_id: fire_id.clone(),
            job_id: job_id.clone(),
            scheduled_for: at,
            claimed_at: at,
            lease: P1,
        }));
        if r.below(3) > 0 {
            v.push(Record::Done(Done {
                fire_id: fire_id.clone(),
                job_id: job_id.clone(),
                status: [RunStatus::Ok, RunStatus::Error, RunStatus::Interrupted]
                    [r.below(3) as usize],
                session_id: None,
                reason: None,
                usage: None,
                at,
            }));
            for attempt in 1..=r.below(3) as u32 {
                v.push(Record::Delivery(Delivery {
                    fire_id: fire_id.clone(),
                    target: "session".into(),
                    attempt,
                    status: DeliveryStatus::Sent,
                    error: None,
                    at,
                }));
            }
        }
        match r.below(4) {
            0 => v.push(Record::Missed(Missed {
                job_id: job_id.clone(),
                scheduled_for: at - 30,
                last: at - 20,
                count: 2,
                reason: MissReason::Late,
                at,
            })),
            1 => v.push(Record::HookSeen(HookSeen {
                route: "r".into(),
                delivery_id: format!("d{i}"),
                at,
            })),
            _ => {}
        }
    }
    v
}

#[test]
fn fold_is_order_independent_and_idempotent() {
    let mut r = Rng(0x1ed9e7);
    for _ in 0..200 {
        let recs = random_records(&mut r);
        let base = summary(&LedgerState::fold(&recs), &recs);
        let mut shuffled = recs.clone();
        r.shuffle(&mut shuffled);
        assert_eq!(summary(&LedgerState::fold(&shuffled), &recs), base, "order");
        let doubled: Vec<_> = recs.iter().chain(shuffled.iter()).collect();
        assert_eq!(
            summary(&LedgerState::fold(doubled), &recs),
            base,
            "re-append"
        );
    }
}

#[test]
fn fold_keeps_first_done_and_first_claim() {
    let f = |id: &str| {
        Record::Fire(Fire {
            fire_id: id.into(),
            job_id: "j".into(),
            scheduled_for: T,
            claimed_at: T,
            lease: P1,
        })
    };
    let d = |st| {
        Record::Done(Done {
            fire_id: "x".into(),
            job_id: "j".into(),
            status: st,
            session_id: None,
            reason: None,
            usage: None,
            at: T,
        })
    };
    let s = LedgerState::fold(&[f("x"), f("y"), d(RunStatus::Ok), d(RunStatus::Interrupted)]);
    assert_eq!(s.claim_of("j", T), Some("x"));
    assert!(
        s.fire("y").is_none(),
        "a second claim of the occurrence never owns it"
    );
    assert_eq!(s.done("x").unwrap().status, RunStatus::Ok);
}

#[test]
fn random_interleaved_ticks_never_double_claim() {
    let mut r = Rng(0xc1a1);
    for round in 0..40 {
        let store = MemoryStore::default();
        let mut ls: Vec<_> = (0..3)
            .map(|_| Ledger::open(store.clone()).unwrap())
            .collect();
        for j in 0..3 {
            ls[0]
                .put_job(job(&format!("j{j}"), "*/2 * * * *", T))
                .unwrap();
        }
        let leases = [
            P1,
            P2,
            Lease {
                pid: 3,
                start_time: 3,
            },
        ];
        let mut won: BTreeSet<(String, i64)> = BTreeSet::new();
        let mut stale: Vec<(usize, Due)> = Vec::new();
        let mut now = T;
        for _ in 0..200 {
            now += r.below(90) as i64;
            let w = r.below(3) as usize;
            match r.below(4) {
                0 => stale.extend(ls[w].ready(now).unwrap().into_iter().map(|d| (w, d))),
                1 if !stale.is_empty() => {
                    // Claim with a possibly stale `Due` from an earlier tick.
                    let (_, d) = stale.remove(r.below(stale.len() as u64) as usize);
                    if let Ok(Some(f)) = ls[w].claim(&d, leases[w], now) {
                        assert!(
                            won.insert((f.job_id.clone(), f.scheduled_for)),
                            "round {round}: double claim"
                        );
                    }
                }
                2 => {
                    let p: Vec<_> = ls[w].state().pending().map(|f| f.fire_id.clone()).collect();
                    for id in p {
                        let _ = ls[w].settle(&id, RunStatus::Ok, None, None, now);
                    }
                }
                _ => ls[w].refresh().unwrap(),
            }
        }
        let fires = lines(&store)
            .into_iter()
            .filter(|x| matches!(x, Record::Fire(_)))
            .count();
        assert_eq!(fires, won.len(), "every fire line is a won claim");
    }
}

// ---- crash injection -----------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Crash {
    /// Nothing reaches the log.
    Lost,
    /// Half the line reaches the log (torn write).
    Torn,
    /// The whole line is durable but the writer dies before the ack.
    AfterWrite,
}

struct FaultyStore {
    inner: MemoryStore,
    crash_at: usize,
    mode: Crash,
    appends: usize,
}
impl LedgerStore for FaultyStore {
    fn read_from(&mut self, o: u64) -> Result<Vec<u8>, LedgerError> {
        self.inner.read_from(o)
    }
    fn append(&mut self, line: &[u8]) -> Result<(), LedgerError> {
        let n = self.appends;
        self.appends += 1;
        if n < self.crash_at {
            return self.inner.append(line);
        }
        match self.mode {
            Crash::Lost => {}
            Crash::Torn => self.inner.append(&line[..line.len() / 2])?,
            Crash::AfterWrite => self.inner.append(line)?,
        }
        Err(LedgerError::Store("killed".into()))
    }
    fn truncate(&mut self, len: u64) -> Result<(), LedgerError> {
        self.inner.truncate(len)
    }
}

/// One tick of process `lease`: recover, ready, claim, run, settle, deliver.
fn tick(l: &mut Ledger<impl LedgerStore>, lease: Lease, now: i64) -> Result<(), LedgerError> {
    l.recover(now, |x| *x == lease)?;
    for due in l.ready(now)? {
        if let Some(f) = l.claim(&due, lease, now)? {
            l.settle(
                &f.fire_id,
                RunStatus::Ok,
                Some(format!("s-{}", f.fire_id)),
                None,
                now,
            )?;
            l.deliver(
                &f.fire_id,
                "file",
                DeliveryStatus::Failed,
                Some("x".into()),
                now,
            )?;
        }
    }
    Ok(())
}

#[test]
fn crash_at_every_append_boundary_loses_nothing_and_fires_once() {
    // Hourly job; first tick at T+2h+10s owes T+1h (superseded) and T+2h:
    // appends = missed, fire, done, delivery.
    for mode in [Crash::Lost, Crash::Torn, Crash::AfterWrite] {
        for crash_at in 0..=4 {
            let inner = MemoryStore::default();
            Ledger::open(inner.clone())
                .unwrap()
                .put_job(job("h", "0 * * * *", T))
                .unwrap();
            let now = T + 7_200 + 10;
            let faulty = FaultyStore {
                inner: inner.clone(),
                crash_at,
                mode,
                appends: 0,
            };
            let mut p1 = Ledger::open(faulty).unwrap();
            let first = tick(&mut p1, P1, now);
            assert_eq!(first.is_err(), crash_at < 4, "{mode:?}@{crash_at}");
            // What process 1 durably settled before dying, if anything.
            let settled_before: Vec<Done> = lines(&inner)
                .into_iter()
                .filter_map(|r| match r {
                    Record::Done(d) => Some(d),
                    _ => None,
                })
                .collect();

            // Restart: process 2 (P1 is dead) at the same instant and the next hour.
            let mut p2 = Ledger::open(inner.clone()).unwrap();
            tick(&mut p2, P2, now + 5).unwrap();
            tick(&mut p2, P2, T + 3 * 3_600 + 5).unwrap();

            let ctx = format!("{mode:?} crash_at={crash_at}");
            let recs = lines(&inner);
            let text = String::from_utf8(inner.bytes()).unwrap();
            for line in text.lines().filter(|x| !x.is_empty()) {
                Record::parse_line(line).unwrap_or_else(|e| panic!("{ctx}: bad line {e}"));
            }
            let s = p2.state();
            // No job lost: every owed occurrence is accounted, the next one ran.
            assert_eq!(s.accounted("h"), Some(T + 3 * 3_600), "{ctx}");
            let missed: u64 = s.missed().map(|m| m.count).sum();
            let fired: BTreeSet<i64> = recs
                .iter()
                .filter_map(|r| match r {
                    Record::Fire(f) => Some(f.scheduled_for),
                    _ => None,
                })
                .collect();
            for occ in [T + 3_600, T + 7_200, T + 3 * 3_600] {
                let in_missed = s.missed().any(|m| m.scheduled_for <= occ && occ <= m.last);
                assert!(
                    fired.contains(&occ) ^ in_missed,
                    "{ctx}: {occ} lost or double-accounted"
                );
            }
            assert_eq!(missed, 1, "{ctx}: only T+1h is missed");
            // None fires twice: one fire line per occurrence.
            let fire_lines = recs.iter().filter(|r| matches!(r, Record::Fire(_))).count();
            assert_eq!(fire_lines, fired.len(), "{ctx}: an occurrence fired twice");
            // Every fire is settled exactly once; a done is never overwritten.
            let mut dones: BTreeMap<String, Vec<RunStatus>> = BTreeMap::new();
            for r in &recs {
                if let Record::Done(d) = r {
                    dones.entry(d.fire_id.clone()).or_default().push(d.status);
                }
            }
            assert_eq!(dones.len(), fire_lines, "{ctx}: unsettled fire");
            for (id, sts) in &dones {
                assert_eq!(sts.len(), 1, "{ctx}: {id} settled twice: {sts:?}");
                assert_eq!(s.done(id).unwrap().status, sts[0], "{ctx}");
            }
            for d in &settled_before {
                assert_eq!(
                    s.done(&d.fire_id).unwrap().status,
                    d.status,
                    "{ctx}: a durable done was overwritten"
                );
                assert_eq!(d.status, RunStatus::Ok, "{ctx}: delivery changed status");
            }
            assert!(s.pending().next().is_none(), "{ctx}: dangling claim");
        }
    }
}

/// #581: a parked run is continued once, under its own `fire_id`; the
/// parked `done` stays, and only an `awaiting_approval` run can be resumed.
#[test]
fn a_parked_run_resumes_once_as_a_new_pending_run() {
    let mut l = fresh(&[job("a", "*/5 * * * *", T)]);
    let due = l.ready(T + 300).unwrap();
    let fire = l.claim(&due[0], P1, T + 301).unwrap().unwrap();
    let parked = Some("s1".to_string());
    l.settle(
        &fire.fire_id,
        RunStatus::AwaitingApproval,
        parked.clone(),
        None,
        T + 302,
    )
    .unwrap();
    let cont = l.resume(&fire.fire_id, P2, T + 400).unwrap();
    assert_ne!(cont.fire_id, fire.fire_id);
    assert_eq!(
        l.state().resumed_by(&fire.fire_id),
        Some(cont.fire_id.as_str())
    );
    assert!(l.state().pending().any(|f| f.fire_id == cont.fire_id));
    let e = l.resume(&fire.fire_id, P2, T + 401).unwrap_err();
    assert!(e.to_string().contains("resumed already"), "{e}");
    // The continuation settles like any run; the parked done stays.
    l.settle(&cont.fire_id, RunStatus::Ok, parked, None, T + 410)
        .unwrap();
    assert_eq!(
        l.state().done(&fire.fire_id).unwrap().status,
        RunStatus::AwaitingApproval
    );
    assert_eq!(l.state().last_outcome("a").unwrap().status, RunStatus::Ok);
    // A finished run, and an unsettled one, cannot be resumed.
    let e = l.resume(&cont.fire_id, P2, T + 420).unwrap_err();
    assert!(e.to_string().contains("not awaiting approval"), "{e}");
    let due = l.ready(T + 600).unwrap();
    let running = l.claim(&due[0], P1, T + 601).unwrap().unwrap();
    assert!(l.resume(&running.fire_id, P2, T + 602).is_err());
}

/// A hand-written `resume` line for a run that is not parked, or a second
/// one for a resumed run, is ignored by the fold: it never runs.
#[test]
fn a_forged_resume_line_never_revives_a_run() {
    let mut l = fresh(&[job("a", "*/5 * * * *", T)]);
    let due = l.ready(T + 300).unwrap();
    let fire = l.claim(&due[0], P1, T + 301).unwrap().unwrap();
    l.settle(&fire.fire_id, RunStatus::Ok, None, None, T + 302)
        .unwrap();
    let forged = Record::Resume(Resume {
        fire_id: "forged".into(),
        resumes: fire.fire_id.clone(),
        job_id: "a".into(),
        claimed_at: T + 303,
        lease: P2,
    });
    let st = LedgerState::fold(&[
        Record::Job(job("a", "*/5 * * * *", T)),
        Record::Fire(fire.clone()),
        Record::Done(l.state().done(&fire.fire_id).unwrap().clone()),
        forged,
    ]);
    assert!(st.fire("forged").is_none(), "an ok run is not resumable");
    assert!(st.pending().next().is_none());
    // A pending continuation dies with its lease like any run.
    let due = l.ready(T + 600).unwrap();
    let f2 = l.claim(&due[0], P1, T + 601).unwrap().unwrap();
    l.settle(
        &f2.fire_id,
        RunStatus::AwaitingApproval,
        None,
        None,
        T + 602,
    )
    .unwrap();
    let cont = l.resume(&f2.fire_id, P2, T + 603).unwrap();
    // A second, hand-written resume of the same parked run is ignored.
    let second = Record::Resume(Resume {
        fire_id: "forged-second".into(),
        resumes: f2.fire_id.clone(),
        job_id: "a".into(),
        claimed_at: T + 604,
        lease: P1,
    });
    let st = LedgerState::fold(&[
        Record::Job(job("a", "*/5 * * * *", T)),
        Record::Fire(f2.clone()),
        Record::Done(l.state().done(&f2.fire_id).unwrap().clone()),
        Record::Resume(Resume {
            fire_id: cont.fire_id.clone(),
            resumes: f2.fire_id.clone(),
            job_id: "a".into(),
            claimed_at: T + 603,
            lease: P2,
        }),
        second,
    ]);
    assert_eq!(st.resumed_by(&f2.fire_id), Some(cont.fire_id.as_str()));
    assert!(st.fire("forged-second").is_none());
    let done = l.recover(T + 700, |lease| lease.pid != P2.pid).unwrap();
    assert_eq!(done[0].fire_id, cont.fire_id);
    assert_eq!(done[0].status, RunStatus::Interrupted);
}
