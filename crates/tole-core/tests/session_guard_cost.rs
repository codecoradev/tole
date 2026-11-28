//! #1307: the cost of reading a command stays linear (and refuses what is too
//! big to read). Each test fed a guard that took seconds, minutes or the whole
//! stack on one of these inputs; the bounds are generous for a debug build.

use std::path::Path;
use tole_core::run_command::RunCommandTool;
use tole_core::session_guard::check_session_argv;

fn quote(t: &str) -> String {
    format!("'{}'", t.replace('\'', "'\\''"))
}

/// `sh -c <text>` written `depth` times (depth 0 is the text itself).
fn nest(text: &str, depth: usize) -> String {
    (0..depth).fold(text.to_string(), |t, _| format!("sh -c {}", quote(&t)))
}

fn refused(argv: &[String]) -> bool {
    check_session_argv(argv, Path::new("/tmp")).is_err()
}

fn shell(text: &str) -> Vec<String> {
    vec!["sh".into(), "-c".into(), text.into()]
}

fn line(text: &str) -> Vec<String> {
    RunCommandTool::split_argv(text).expect("a command line")
}

/// The leading words are walked once, with a cap: no recursion, no rebuilt word
/// list (the first version took 2.7 GB and overflowed a 2 MiB stack on 10k words).
#[test]
fn a_hundred_thousand_leading_words_are_refused_quickly_on_a_small_stack() {
    for lead in [">f ", "X+=1 ", "{ ", "$E "] {
        let text = format!("{}tole x", lead.repeat(100_000));
        let started = std::time::Instant::now();
        let (a, b) = std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(move || (refused(&shell(&text)), refused(&line(&text))))
            .unwrap()
            .join()
            .expect("no stack overflow");
        assert!(a && b, "{lead:?}: refused as text {a}, as argv {b}");
        assert!(started.elapsed().as_secs() < 20, "{lead:?} took too long");
    }
    // A long wrapper chain and many nested `-c` stay linear too.
    let chain = format!("{}sh -c 'tole x'", "env ".repeat(20_000));
    let layers = format!("{}echo a", "sh -c ".repeat(2_000));
    let started = std::time::Instant::now();
    let (a, b, c) = std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || {
            (
                refused(&shell(&chain)),
                refused(&line(&chain)),
                refused(&line(&layers)),
            )
        })
        .unwrap()
        .join()
        .expect("no stack overflow");
    assert!(a && b && c, "chain {a} {b}, layers {c}");
    assert!(
        started.elapsed().as_secs() < 30,
        "chain or layers took too long"
    );
    // Nested shells and a glob of many stars stay linear too.
    let text = nest(&format!("{}echo", ">f ".repeat(20_000)), 3);
    let glob = format!("env {}x", "*a".repeat(5_000));
    let classes = format!("env ./{}x", "[".repeat(80_000));
    let started = std::time::Instant::now();
    let _ = (
        refused(&shell(&text)),
        refused(&shell(&glob)),
        refused(&shell(&classes)),
    );
    assert!(
        started.elapsed().as_secs() < 30,
        "nested or glob input took too long"
    );
    // Inside the cap the program behind the words is still found.
    let text = format!("{}tole x", ">f ".repeat(60));
    assert!(refused(&shell(&text)) && refused(&line(&text)));
}

fn quote_double(t: &str) -> String {
    let esc: String = t
        .chars()
        .flat_map(|c| {
            ['\\', '"', '$', '`']
                .contains(&c)
                .then_some('\\')
                .into_iter()
                .chain([c])
        })
        .collect();
    format!("\"{esc}\"")
}

fn quote_backslash(t: &str) -> String {
    t.chars()
        .flat_map(|c| {
            (!c.is_ascii_alphanumeric() && !"_-./=,:@".contains(c))
                .then_some('\\')
                .into_iter()
                .chain([c])
        })
        .collect()
}

/// Nested `sh -c` layers must not multiply the work: each payload was read
/// twice and each reading three ways, so the cost was exponential in the depth
/// (4 s for 3 commands in 3 levels). Every quote style, 3 to 10 commands, 3 to 6
/// layers, a bound per call.
#[test]
fn nested_shell_layers_cost_a_bounded_time() {
    type Style = fn(&str) -> String;
    let styles: [(&str, Style); 3] = [
        ("single", quote),
        ("double", quote_double),
        ("backslash", quote_backslash),
    ];
    for (name, q) in styles {
        for commands in [3, 6, 10] {
            for layers in 3..=6 {
                let mut text = vec!["echo a b"; commands].join("; ");
                for _ in 0..layers {
                    text = format!("sh -c {}", q(&text));
                }
                let started = std::time::Instant::now();
                let _ = (refused(&shell(&text)), refused(&line(&text)));
                let took = started.elapsed();
                assert!(
                    took.as_millis() < 1500,
                    "{name}, {commands} commands, {layers} layers: {took:?}"
                );
            }
        }
    }
}

/// Runs of quotes and backslashes, and many inline shells: each text is read in
/// time linear in its size (a word was trimmed again for every character: 50 KB
/// of backslashes took a minute).
#[test]
fn long_runs_of_quotes_backslashes_and_inline_shells_cost_linear_time() {
    let texts = [
        format!("sh -c {}x", "\\".repeat(50_000)),
        format!("sh -c {} tole", "'\"".repeat(50_000)),
        format!("sh -c {}", "\\'".repeat(50_000)),
        format!("sh -c {}", "\"'".repeat(50_000)),
        format!("{}echo", "sh -c 'echo a'; ".repeat(20_000)),
        format!("{}echo", "sh -c 'echo a' & ".repeat(20_000)),
    ];
    for t in texts {
        let started = std::time::Instant::now();
        let long = t.clone();
        std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(move || {
                let _ = refused(&shell(&long));
                if let Ok(argv) = RunCommandTool::split_argv(&long) {
                    let _ = refused(&argv);
                }
            })
            .unwrap()
            .join()
            .expect("no stack overflow");
        let took = started.elapsed();
        assert!(took.as_secs() < 8, "{} bytes took {took:?}", t.len());
    }
}
