//! Differential test of the session guard (#1307, #684).
//!
//! The guard (`check_session_argv`) reads shell text with its own word cut; a
//! shell reads it with the shell grammar. Wherever the two disagree, a session
//! can run the owner-only `tole` subcommands. This test runs a corpus of
//! command lines twice: as `sh -c <text>` (and `bash -c`, `dash -c` when those
//! exist) against a FAKE `tole` that only writes a marker file, and through
//! `check_session_argv(["sh", "-c", text])`. The real binary never runs: each
//! worker has its own directory with the fake `tole` (and `sudo` / `doas`
//! shims that run no real program), put first on a `PATH` of system directories.
//! The test is skipped when no shell is found, or when a real `tole` sits in a
//! system directory (an `env -i` line could reach it).
//!
//! Rules:
//!
//! 1. A line that a shell ran as `tole` and that the guard ALLOWED must be in
//!    `KNOWN_RESIDUAL` (`support/guard_corpus.rs`). Anything else fails the
//!    test and is printed: it is a hole in the guard, or a new residual.
//! 2. Ratchet: every `KNOWN_RESIDUAL` entry must still be run by a shell and
//!    still be allowed by the guard. When the guard starts refusing an entry,
//!    the test fails: delete the entry. The table only shrinks as #1307 closes
//!    routes.
//! 3. `KNOWN_GOOD` lines run no `tole` and the guard allows them;
//!    `HARMLESS_REFUSED` lines run no `tole` (the guard may refuse them).
//!
//! Corpus: the hand-written tables of `support/guard_tables.rs` plus a
//! fixed-seed generator (`support/guard_generator.rs`): assignments, wrappers,
//! separators, quotes, nested `sh -c` to depth 2, three spellings of the
//! program word. It proves nothing outside that corpus.
//!
//! To add a case: append the text to the table in `support/guard_tables.rs`
//! (`@FAKE@` is the fake's directory; a line a shell runs as `tole` and that
//! the guard allows goes in `KNOWN_RESIDUAL` of `support/guard_corpus.rs`, with the programs it needs), or
//! add an axis value to `support/guard_generator.rs`.
//!
//! Worklist mode: `TOLE_GUARD_DIFF_REPORT=1 cargo test -p tole-core --test
//! session_guard_differential -- --nocapture` prints every disagreement
//! (shell ran, guard allowed) with the shells that ran it, and every
//! over-refusal, and does not fail on them.

#![cfg(all(unix, feature = "shell-tools"))]

#[path = "support/guard_corpus.rs"]
mod guard_corpus;
#[path = "support/guard_generator.rs"]
mod guard_generator;
#[path = "support/guard_tables.rs"]
mod guard_tables;
#[path = "support/test_dir.rs"]
mod test_dir;

use guard_corpus::{Input, Residual, KNOWN_RESIDUAL, RESIDUAL_CLASSES};
use guard_tables::{HARMLESS_REFUSED, KNOWN_BAD, KNOWN_GOOD, LINE_BAD, LINE_GOOD};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use test_dir::{write_executable, TestDir};
use tole_core::run_command::RunCommandTool;
use tole_core::session_guard::check_session_argv;

/// Generated lines; the whole run stays within a few seconds.
const GENERATED: usize = 1800;
const SHELL_TIMEOUT: Duration = Duration::from_secs(10);
/// Where a shell or a `env -i` finds programs when `PATH` is gone.
const SYSTEM_DIRS: [&str; 6] = [
    "/usr/local/sbin",
    "/usr/local/bin",
    "/usr/sbin",
    "/usr/bin",
    "/sbin",
    "/bin",
];

struct Shell {
    name: &'static str,
    path: PathBuf,
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// `sh`, `bash` and `dash` that exist; a name that is a link to one already
/// listed (`sh` -> `dash`) is not run twice.
fn find_shells() -> Vec<Shell> {
    let mut seen = HashSet::new();
    ["sh", "bash", "dash"]
        .into_iter()
        .filter_map(|name| {
            let path = find_in_path(name)?;
            seen.insert(path.canonicalize().ok()?)
                .then_some(Shell { name, path })
        })
        .collect()
}

/// One worker's scratch directory: the fake `tole`, shims, and the marker.
struct Rig {
    dir: TestDir,
}

impl Rig {
    fn new() -> Rig {
        let dir = TestDir::in_system_temp("tole-guard-diff");
        let d = dir.display();
        write_executable(
            dir.join("tole"),
            format!("#!/bin/sh\necho ran >> '{d}/ran'\n"),
        );
        // The real sudo / doas would look `tole` up in their own path.
        let shim = "#!/bin/sh\nwhile [ $# -gt 0 ]; do case \"$1\" in \
            -u|-g|-h|-p|-C|-U|-D|-R|-T) shift 2;; -*) shift;; *) break;; esac; done\n\
            exec env \"$@\"\n";
        write_executable(dir.join("sudo"), shim);
        write_executable(dir.join("doas"), shim);
        Rig { dir }
    }

    fn marker(&self) -> PathBuf {
        self.dir.join("ran")
    }

    /// Did `shell -c text` run the fake `tole`?
    fn ran(&self, shell: &Shell, text: &str) -> bool {
        self.run(Command::new(&shell.path).arg("-c").arg(text), text)
    }

    /// Did running `argv` directly (no outer shell) run the fake `tole`?
    fn ran_argv(&self, argv: &[String]) -> bool {
        self.run(Command::new(&argv[0]).args(&argv[1..]), &argv.join(" "))
    }

    /// Everything is cleared but a `PATH` that puts the fake first.
    fn run(&self, cmd: &mut Command, what: &str) -> bool {
        let _ = std::fs::remove_file(self.marker());
        let path = format!("{}:/usr/bin:/bin", self.dir.display());
        let Ok(mut child) = cmd
            .current_dir(&self.dir)
            .env_clear()
            .env("PATH", path)
            .env("HOME", self.dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        else {
            return false; // the program does not exist here
        };
        let start = Instant::now();
        while child.try_wait().expect("wait for a shell").is_none() {
            if start.elapsed() > SHELL_TIMEOUT {
                let _ = child.kill();
                eprintln!("timeout: {what:?}");
                break;
            }
            std::thread::sleep(Duration::from_micros(300));
        }
        // Reading to the end waits for a backgrounded fake too, which holds
        // the pipes until it has written its marker.
        let _ = child.wait_with_output();
        self.marker().exists()
    }
}

struct Row {
    /// The corpus input, `@FAKE@` still in its text.
    input: Input,
    ran_by: Vec<&'static str>,
    allowed: bool,
}

fn run_corpus(inputs: &[Input], shells: &[Shell]) -> Vec<Row> {
    let workers = std::thread::available_parallelism()
        .map_or(2, |n| n.get())
        .min(4);
    // Rigs are built one after the other: a script being written while another
    // thread forks is the classic "text file busy".
    let rigs: Vec<Rig> = (0..workers).map(|_| Rig::new()).collect();
    let (next, rows) = (AtomicUsize::new(0), Mutex::new(Vec::new()));
    std::thread::scope(|s| {
        for rig in &rigs {
            let (next, rows) = (&next, &rows);
            s.spawn(move || {
                let fake = rig.dir.display().to_string();
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(input) = inputs.get(i) else { break };
                    let real = input.text.replace("@FAKE@", &fake);
                    let (ran_by, argv) = if input.line {
                        let argv = RunCommandTool::split_argv(&real)
                            .unwrap_or_else(|e| panic!("corpus line {real:?}: {e}"));
                        (
                            rig.ran_argv(&argv).then_some("exec").into_iter().collect(),
                            argv,
                        )
                    } else {
                        let ran = shells.iter().filter(|sh| rig.ran(sh, &real));
                        let ran_by = ran.map(|sh| sh.name).collect();
                        (ran_by, vec!["sh".to_string(), "-c".to_string(), real])
                    };
                    let allowed = check_session_argv(&argv, &rig.dir).is_ok();
                    let row = Row {
                        input: input.clone(),
                        ran_by,
                        allowed,
                    };
                    rows.lock().unwrap().push((i, row));
                }
            });
        }
    });
    let mut rows = rows.into_inner().unwrap();
    rows.sort_by_key(|(i, _)| *i);
    rows.into_iter().map(|(_, r)| r).collect()
}

/// The corpus without repeats: residual routes, the hand tables, the
/// generated lines.
fn corpus() -> Vec<Input> {
    let sh = |t: &&str| Input::sh(t);
    let line = |t: &&str| Input::line(t);
    let mut seen = HashSet::new();
    KNOWN_RESIDUAL
        .iter()
        .map(Residual::input)
        .chain(KNOWN_BAD.iter().map(sh))
        .chain(KNOWN_GOOD.iter().map(sh))
        .chain(HARMLESS_REFUSED.iter().map(sh))
        .chain(LINE_BAD.iter().map(line))
        .chain(LINE_GOOD.iter().map(line))
        .chain(guard_generator::generate(GENERATED))
        .filter(|i| seen.insert(i.clone()))
        .collect()
}

fn shown(i: &Input) -> String {
    format!("{} {:?}", if i.line { "line" } else { "sh" }, i.text)
}

/// Where the test may run at all: `None` is a reason to skip.
fn skip_reason(shells: &[Shell]) -> Option<String> {
    if shells.is_empty() {
        return Some("no sh, bash or dash on PATH".into());
    }
    for d in SYSTEM_DIRS {
        for n in ["tole", "tole-cli"] {
            let p = Path::new(d).join(n);
            if p.exists() {
                return Some(format!(
                    "{} exists: `env -i` lines could run a real tole",
                    p.display()
                ));
            }
        }
    }
    None
}

#[test]
fn the_guard_refuses_what_a_shell_runs_as_tole_except_the_known_residual() {
    let shells = find_shells();
    if let Some(why) = skip_reason(&shells) {
        eprintln!("SKIPPED session_guard_differential: {why}");
        return;
    }
    let names: Vec<_> = shells.iter().map(|s| s.name).collect();
    let report = std::env::var_os("TOLE_GUARD_DIFF_REPORT").is_some_and(|v| v == "1");
    let inputs = corpus();
    let started = Instant::now();
    let rows = run_corpus(&inputs, &shells);
    println!(
        "session_guard_differential: {} lines, shells {names:?}, {:.1}s",
        rows.len(),
        started.elapsed().as_secs_f32()
    );

    let by_input: HashMap<&Input, &Row> = rows.iter().map(|r| (&r.input, r)).collect();
    let residual_inputs: HashSet<Input> = KNOWN_RESIDUAL.iter().map(Residual::input).collect();
    let class_of = |i: &Input| RESIDUAL_CLASSES.iter().position(|c| (c.matches)(i));
    let have = |prog: &str| names.contains(&prog) || find_in_path(prog).is_some();
    let mut fail = Vec::new();

    // Rule 1: a shell ran tole, the guard allowed it.
    let worklist: Vec<&Row> = rows
        .iter()
        .filter(|r| !r.ran_by.is_empty() && r.allowed)
        .collect();
    let mut class_live = vec![0usize; RESIDUAL_CLASSES.len()];
    for r in &worklist {
        if residual_inputs.contains(&r.input) {
            continue;
        }
        if let Some(c) = class_of(&r.input) {
            class_live[c] += 1;
        } else {
            fail.push(format!(
                "NEW HOLE, a shell ran tole and the guard allowed it ({:?}): {}",
                r.ran_by,
                shown(&r.input)
            ));
        }
    }
    // Rule 2: the ratchet.
    for e in KNOWN_RESIDUAL {
        let row = by_input[&e.input()];
        if !row.allowed {
            fail.push(format!(
                "RATCHET, the guard now refuses {}: delete it from KNOWN_RESIDUAL",
                shown(&row.input)
            ));
        } else if row.ran_by.is_empty() && e.needs.iter().all(|p| have(p)) {
            fail.push(format!(
                "RATCHET, no shell runs {} as tole any more: fix or delete the entry",
                shown(&row.input)
            ));
        }
    }
    for (c, live) in RESIDUAL_CLASSES.iter().zip(&class_live) {
        if *live == 0 {
            fail.push(format!(
                "RATCHET, no line of the class {:?} is allowed and run any more: \
                 delete it from RESIDUAL_CLASSES",
                c.name
            ));
        }
    }
    // Rule 3: the tables of harmless lines.
    let tables = [(false, KNOWN_GOOD), (true, LINE_GOOD)];
    for (line, table) in tables {
        for t in table {
            let row = by_input[&Input {
                line,
                text: t.to_string(),
            }];
            if !row.allowed || !row.ran_by.is_empty() {
                fail.push(format!(
                    "GOOD must be allowed and run no tole (allowed {}, ran by {:?}): {}",
                    row.allowed,
                    row.ran_by,
                    shown(&row.input)
                ));
            }
        }
    }
    for t in HARMLESS_REFUSED {
        let row = by_input[&Input::sh(t)];
        if !row.ran_by.is_empty() {
            fail.push(format!(
                "HARMLESS_REFUSED line runs tole ({:?}): {}",
                row.ran_by,
                shown(&row.input)
            ));
        }
    }

    if report {
        report_mode(
            &rows,
            &worklist,
            &residual_inputs,
            &class_live,
            &fail,
            class_of,
        );
        return;
    }
    assert!(fail.is_empty(), "\n{}\n", fail.join("\n"));
}

/// `TOLE_GUARD_DIFF_REPORT=1`: the worklist of #1307, nothing fails.
fn report_mode(
    rows: &[Row],
    worklist: &[&Row],
    residual: &HashSet<Input>,
    class_live: &[usize],
    fail: &[String],
    class_of: impl Fn(&Input) -> Option<usize>,
) {
    println!(
        "--- WORKLIST: a shell ran tole, the guard allowed it ({}) ---",
        worklist.len()
    );
    for r in worklist {
        let tag = match class_of(&r.input) {
            _ if residual.contains(&r.input) => "known".to_string(),
            Some(c) => format!("class {:?}", RESIDUAL_CLASSES[c].name),
            None => "NEW".to_string(),
        };
        println!("{tag}\t{:?}\t{}", r.ran_by, shown(&r.input));
    }
    for (c, live) in RESIDUAL_CLASSES.iter().zip(class_live) {
        println!(
            "class {:?}: {live} lines, new finding: {}",
            c.name, c.new_finding
        );
    }
    let over: Vec<_> = rows
        .iter()
        .filter(|r| r.ran_by.is_empty() && !r.allowed)
        .collect();
    println!(
        "--- over-refused: no shell ran tole, the guard refused ({}) ---",
        over.len()
    );
    for r in over {
        println!("{}", shown(&r.input));
    }
    println!("--- other failures ({}) ---", fail.len());
    for f in fail {
        println!("{f}");
    }
}

#[test]
fn the_harness_sees_a_shell_run_the_fake() {
    let shells = find_shells();
    if let Some(why) = skip_reason(&shells) {
        eprintln!("SKIPPED session_guard_differential: {why}");
        return;
    }
    let rig = Rig::new();
    for sh in &shells {
        assert!(rig.ran(sh, "tole x"), "{} must run the fake", sh.name);
        assert!(rig.ran(sh, "./tole x"), "{} must run ./tole", sh.name);
        assert!(!rig.ran(sh, "echo tole"), "{} ran it for echo", sh.name);
        assert!(!rig.ran(sh, "true"), "{} ran it for true", sh.name);
        assert!(
            rig.ran(sh, "echo a | cat & tole x"),
            "{} lost a backgrounded run",
            sh.name
        );
    }
    let argv = |t: &str| ["sh".to_string(), "-c".to_string(), t.to_string()];
    assert!(check_session_argv(&argv("tole x"), &rig.dir).is_err());
    assert!(check_session_argv(&argv("echo tole"), &rig.dir).is_ok());
}
