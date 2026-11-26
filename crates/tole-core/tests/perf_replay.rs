//! #1019: how long does opening (replaying) a long session take, and how
//! does it grow with the session's age?
//!
//! Opening a session folds its whole JSONL log into memory
//! (`JsonlStorage::open`), and the first turn after a resume also scans the
//! entries once (taint seeding, the denial check). This file builds long sessions through the REAL turn loop and JSONL writer (a
//! `MockProvider`, no network), then times:
//!
//! - `open`: the writable open (lock + replay + torn-tail check), the cost of
//!   `tole resume` and of every `tole serve` session load;
//! - `open_ro`: the read-only replay that listings and status use;
//! - `first turn`: one text-only turn on the reopened session. It covers the
//!   turn loop's own scans of the transcript (taint seeding, with a
//!   `TaintTracker` set as real hosts do, and the denial check) and its durable
//!   commits. It is one sample that includes an fdatasync, so a size
//!   dependence under about 5 ms is not visible. The stop-gate scan and the
//!   host's assemble work (config, tool roster, MCP) are not measured.
//!
//! The default run is small (a few seconds) and gates on a generous budget
//! plus a linearity check, like `perf_threshold.rs`. The numbers that decide
//! whether a checkpoint is needed come from the large sizes, which are opt-in:
//!
//! ```text
//! TOLE_REPLAY_SIZES=5000,20000,100000 TOLE_REPLAY_PROFILES=small,mixed \
//!   cargo test -p tole-core --release --test perf_replay -- --nocapture
//! ```
//!
//! - `TOLE_REPLAY_SIZES`: comma list of entry counts (default `1000,3000`).
//! - `TOLE_REPLAY_PROFILES`: `small`, `mixed` or both (default `small`).
//! - `TOLE_REPLAY_REPS`: timed opens per size, the median is reported
//!   (default 3).
//! - `TOLE_REPLAY_DIR`: keep the fixtures there and reuse them on the next
//!   run (building 100k entries takes minutes); without it they live in a
//!   `TestDir`. The fixture name carries a generator version (`replay-v1-`),
//!   so a change of the generator needs a new version, never a silent reuse.
//!
//! The `RSS` column is only valid for the first size measured in a process
//! whose fixture was built by an EARLIER process (earlier builds and opens
//! leave heap behind that a later open reuses). Run one profile and one size per
//! process with `TOLE_REPLAY_DIR` set and the fixture already there; any other
//! row prints `n/a`.
//!
//! The numbers are recorded in docs/perf.md ("Long-session replay").

#[path = "support/test_dir.rs"]
mod test_dir;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use test_dir::TestDir;
use tole_core::approval::AllowlistApprover;
use tole_core::cancel::CancelToken;
use tole_core::image::ImageRef;
use tole_core::mock::MockProvider;
use tole_core::provider::ProviderOutput;
use tole_core::skill_draft::TaintTracker;
use tole_core::storage::{JsonlStorage, Storage};
use tole_core::tool::{Risk, Tool, ToolRegistry};
use tole_core::turn::{run_turn, run_turn_with_images, TurnOutcome};

/// Docs/perf.md: ceiling for one open of the default fixture on the
/// unoptimized build CI runs. The observed value is far below; this catches a
/// complexity regression (an O(n^2) replay), it does not race.
const OPEN_BUDGET_MS: u128 = 5_000;

/// Tool calls per fixture turn (below the turn loop's MAX_STEPS of 32).
const CALLS_PER_TURN: usize = 12;

/// A deterministic LCG, so every run builds the same sessions.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

/// Payload shape of a fixture.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Profile {
    /// Every tool result is about 200 bytes: the per-entry cost alone.
    Small,
    /// Long-chat mix: 60% about 200 B, 30% 4 KiB, 10% 32 KiB results (a
    /// file read or a command log), 1.5 KiB final answers, and a user message
    /// with two image references every fifth turn.
    Mixed,
}

impl Profile {
    fn name(self) -> &'static str {
        match self {
            Profile::Small => "small",
            Profile::Mixed => "mixed",
        }
    }
    fn result_bytes(self, rng: &mut Rng) -> usize {
        match self {
            Profile::Small => 200,
            Profile::Mixed => match rng.next() % 10 {
                0 => 32 * 1024,
                1..=3 => 4 * 1024,
                _ => 200,
            },
        }
    }
}

/// Returns `bytes` of source-like text (quotes, newlines, indentation, so the
/// JSON escaping path runs) with no secret shape in it.
fn text_of(bytes: usize, salt: u64) -> String {
    let mut out = String::with_capacity(bytes + 64);
    let mut n = salt;
    while out.len() < bytes {
        n = n.wrapping_add(1);
        out.push_str(&format!(
            "    let item_{n} = \"value {n}\"; // line {n}\n",
            n = n % 9973
        ));
    }
    out.truncate(bytes);
    out
}

/// A read-only tool that returns `bytes` of text: the large tool output.
struct BlobTool;
impl Tool for BlobTool {
    fn name(&self) -> &str {
        "blob"
    }
    fn risk(&self) -> Risk {
        Risk::ReadOnly
    }
    fn describe(&self, _input: &Value) -> String {
        "blob".into()
    }
    fn execute(&self, input: Value) -> Result<Value, String> {
        let bytes = input["bytes"].as_u64().unwrap_or(0) as usize;
        let salt = input["salt"].as_u64().unwrap_or(0);
        Ok(json!({ "output": text_of(bytes, salt) }))
    }
}

fn registry() -> ToolRegistry {
    let mut reg = ToolRegistry::with_approver(AllowlistApprover::allow_only(vec![]));
    // As a real host does (tool_roster, gate): without a tracker the turn
    // loop's `seed_taint` returns at once and the first turn would not
    // include it.
    reg.set_taint_tracker(TaintTracker::default());
    reg.register(Box::new(BlobTool)).unwrap();
    reg
}

fn image_refs(turn: u64) -> Vec<ImageRef> {
    (0..2)
        .map(|i| ImageRef {
            path: format!("/work/shots/turn-{turn}-{i}.png"),
            mime: "image/png".into(),
            bytes: 120_000 + turn,
            fnv1a64: format!("{:016x}", turn.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ i),
        })
        .collect()
}

/// Builds `<dir>/<id>.jsonl` with at least `target` entries through the real
/// turn loop; returns the path and whether this call built it. An existing
/// file is reused (see `TOLE_REPLAY_DIR`).
fn build(dir: &Path, profile: Profile, target: usize) -> (PathBuf, bool) {
    let id = format!("replay-v1-{}-{target}", profile.name());
    let path = dir.join(format!("{id}.jsonl"));
    if path.exists() {
        return (path, false);
    }
    // Build under a temporary name and rename, so an interrupted build never
    // leaves a short file that a later run would take for the fixture. (The
    // header of the renamed file still carries the `-partial` id: harmless,
    // the id is not checked against the file name.)
    let tmp_id = format!("{id}-partial");
    let tmp = dir.join(format!("{tmp_id}.jsonl"));
    let _ = std::fs::remove_file(&tmp);
    let mut s = JsonlStorage::create(dir, &tmp_id, None).unwrap();
    let reg = registry();
    let mut rng = Rng(0x1019 ^ target as u64);
    let mut turn = 0u64;
    while s.entries().len() < target {
        turn += 1;
        let mut script = Vec::new();
        for call in 0..CALLS_PER_TURN {
            script.push(ProviderOutput::ToolCall {
                tool: "blob".into(),
                input: json!({
                    "bytes": profile.result_bytes(&mut rng),
                    "salt": turn * 100 + call as u64,
                }),
            });
        }
        let answer = if profile == Profile::Mixed { 1536 } else { 200 };
        script.push(ProviderOutput::Final {
            text: text_of(answer, turn),
        });
        let mut p = MockProvider::scripted(script);
        let prompt = format!("turn {turn}: {}", text_of(160, turn));
        let out = if profile == Profile::Mixed && turn.is_multiple_of(5) {
            run_turn_with_images(
                &mut s,
                &mut p,
                &reg,
                &prompt,
                &image_refs(turn),
                &CancelToken::default(),
                None,
            )
        } else {
            run_turn(&mut s, &mut p, &reg, &prompt)
        }
        .unwrap();
        assert!(
            matches!(out, TurnOutcome::Final { .. }),
            "fixture turn {turn} must finish, got {out:?}"
        );
    }
    // Anti-vacuity: the log holds real tool traffic, not just prompts.
    let results = s
        .entries()
        .iter()
        .filter(|e| e.kind.as_str() == "tool_result")
        .count();
    assert!(
        results >= target / 3,
        "fixture must hold tool results ({results} of {} entries)",
        s.entries().len()
    );
    drop(s);
    std::fs::rename(&tmp, &path).unwrap();
    let _ = std::fs::remove_file(dir.join(format!("{tmp_id}.jsonl.lock")));
    (path, true)
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

/// Resident set size in KiB (Linux only; `None` elsewhere).
fn rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

struct Measured {
    entries: usize,
    bytes: u64,
    open: Duration,
    open_ro: Duration,
    first_turn: Duration,
    rss_delta_kib: Option<u64>,
}

fn measure(path: &Path, reps: usize, rss_valid: bool) -> Measured {
    let bytes = std::fs::metadata(path).unwrap().len();
    let rss_before = rss_kib().filter(|_| rss_valid);
    let mut opens = Vec::new();
    let mut entries = 0;
    let mut rss_delta_kib = None;
    for _ in 0..reps {
        let t = Instant::now();
        let s = JsonlStorage::open(path).unwrap();
        opens.push(t.elapsed());
        entries = s.entries().len();
        if rss_delta_kib.is_none() {
            rss_delta_kib = rss_kib().zip(rss_before).map(|(a, b)| a.saturating_sub(b));
        }
    }
    let mut ros = Vec::new();
    for _ in 0..reps {
        let t = Instant::now();
        let s = JsonlStorage::open_read_only(path).unwrap();
        ros.push(t.elapsed());
        assert_eq!(s.entries().len(), entries, "both opens replay every entry");
    }
    // The first turn after a resume, on a session that is a copy, so the
    // fixture stays the same size for the next run.
    let copy = TestDir::new("tole-perf-replay-copy");
    let copy_path = copy.join("copy.jsonl");
    std::fs::copy(path, &copy_path).unwrap();
    // Flush the copy first: the turn's durable commit does an fdatasync, which
    // would otherwise also write out every dirty page of the copy and be
    // timed as if it were replay cost.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&copy_path)
        .and_then(|f| f.sync_all())
        .unwrap();
    let mut s = JsonlStorage::open(&copy_path).unwrap();
    let mut p = MockProvider::scripted(vec![ProviderOutput::Final { text: "ok".into() }]);
    let t = Instant::now();
    let out = run_turn(&mut s, &mut p, &registry(), "and now?").unwrap();
    let first_turn = t.elapsed();
    assert!(matches!(out, TurnOutcome::Final { .. }), "{out:?}");
    Measured {
        entries,
        bytes,
        open: median(opens),
        open_ro: median(ros),
        first_turn,
        rss_delta_kib,
    }
}

fn list(name: &str, default: &str) -> Vec<String> {
    std::env::var(name)
        .unwrap_or_else(|_| default.to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[test]
fn replay_of_long_sessions_stays_linear_and_under_budget() {
    let sizes: Vec<usize> = list("TOLE_REPLAY_SIZES", "1000,3000")
        .iter()
        .map(|s| s.parse().expect("TOLE_REPLAY_SIZES: comma list of numbers"))
        .collect();
    let profiles: Vec<Profile> = list("TOLE_REPLAY_PROFILES", "small")
        .iter()
        .map(|s| match s.as_str() {
            "small" => Profile::Small,
            "mixed" => Profile::Mixed,
            other => panic!("TOLE_REPLAY_PROFILES: unknown profile {other}"),
        })
        .collect();
    let reps: usize = list("TOLE_REPLAY_REPS", "3")
        .first()
        .and_then(|s| s.parse().ok())
        .expect("TOLE_REPLAY_REPS: a number >= 1");
    assert!(
        !sizes.is_empty() && !profiles.is_empty() && reps >= 1,
        "nothing to measure"
    );

    let scratch = TestDir::new("tole-perf-replay");
    let (dir, kept) = match std::env::var("TOLE_REPLAY_DIR") {
        Ok(d) => {
            std::fs::create_dir_all(&d).unwrap();
            (PathBuf::from(d), true)
        }
        Err(_) => (scratch.path().to_path_buf(), false),
    };

    eprintln!(
        "replay-measure: profile  entries   file MiB   open ms  open_ro ms  first turn ms  RSS MiB  (median of {reps}, {} build)",
        if cfg!(debug_assertions) { "debug" } else { "release" }
    );
    let mut rss_valid = true;
    for profile in profiles {
        let mut rows: Vec<Measured> = Vec::new();
        for &size in &sizes {
            let t = Instant::now();
            let (path, did_build) = build(&dir, profile, size);
            let built = t.elapsed();
            // RSS is only meaningful for the first open of a process that
            // built nothing (see the module doc).
            rss_valid &= !did_build;
            let m = measure(&path, reps, rss_valid);
            rss_valid = false;
            eprintln!(
                "replay-measure: {:<8} {:>7} {:>10.2} {:>9.1} {:>11.1} {:>14.1} {:>8}  (fixture {:.1}s{})",
                profile.name(),
                m.entries,
                m.bytes as f64 / (1024.0 * 1024.0),
                ms(m.open),
                ms(m.open_ro),
                ms(m.first_turn),
                m.rss_delta_kib
                    .map_or("n/a".to_string(), |k| format!("{:.0}", k as f64 / 1024.0)),
                built.as_secs_f64(),
                if kept { ", kept" } else { "" },
            );
            assert!(
                m.entries >= size,
                "the fixture must hold the {size} entries asked for, got {}",
                m.entries
            );
            if size <= 4000 {
                assert!(
                    m.open.as_millis() < OPEN_BUDGET_MS,
                    "open of {} entries took {:?} (budget {OPEN_BUDGET_MS}ms): replay regression",
                    m.entries,
                    m.open
                );
            }
            rows.push(m);
        }
        // Linearity: opening k times the entries must cost about k times as
        // much. A factor 3 of slack on the ratio (and no check below 20 ms,
        // where timer noise rules) catches an O(n^2) replay without flaking.
        for pair in rows.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            if b.bytes > a.bytes && ms(a.open) >= 20.0 {
                let growth = b.bytes as f64 / a.bytes as f64;
                let cost = ms(b.open) / ms(a.open);
                assert!(
                    cost <= growth * 3.0,
                    "open grew {cost:.1}x for {growth:.1}x the bytes ({} -> {} entries): not linear",
                    a.entries,
                    b.entries
                );
            }
        }
    }
}
