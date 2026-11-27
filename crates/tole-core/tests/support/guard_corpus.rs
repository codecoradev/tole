//! The types, the residual routes and the tables' meaning of
//! `tests/session_guard_differential.rs` (#1307); the tables are in `guard_tables.rs`.
//!
//! Every entry is the text of one `sh -c` call. `@FAKE@` stands for the
//! directory that holds the fake `tole` (the real binary never runs), so an
//! absolute program path is written `@FAKE@/tole`.
//!
//! Which table a line goes into:
//!
//! * [`KNOWN_BAD`]: a line that runs `tole` in some shell. Nothing is asserted
//!   per line; the harness applies its general rule (a shell ran the fake and
//!   the guard allowed it is a failure unless the line is in
//!   [`KNOWN_RESIDUAL`]).
//! * [`KNOWN_GOOD`]: a line that runs no `tole` and that the guard must ALLOW.
//! * [`HARMLESS_REFUSED`]: a line that runs no `tole` and that the guard
//!   refuses today on purpose (the over-refusal of #1302). Only "no shell ran
//!   the fake" is asserted, so a guard that learns to allow them needs no edit.
//! * [`LINE_BAD`], [`LINE_GOOD`]: the same for a command line run with no
//!   outer shell (split like `run_command`; see [`Input`]).
//! * [`KNOWN_RESIDUAL`]: a line a shell runs as `tole` and the guard allows
//!   today: the routes of `docs/threat-model.md` ("Owner-only records").

/// One corpus input. `line == false`: the text is handed to `sh -c` and the
/// guard sees `["sh", "-c", text]`. `line == true`: the text is a command line
/// split into an argv the way `run_command` does and run directly, no outer
/// shell (`bash -c '...'` as the model writes it); the guard sees that argv.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Input {
    pub line: bool,
    pub text: String,
}

impl Input {
    pub fn sh(text: &str) -> Input {
        Input {
            line: false,
            text: text.into(),
        }
    }

    pub fn line(text: &str) -> Input {
        Input {
            line: true,
            text: text.into(),
        }
    }
}

/// A residual route: the input, and the programs that must exist for a shell
/// to run it (when one is missing on this machine the "a shell still runs it"
/// half of the ratchet is skipped for that entry, never the guard half).
pub struct Residual {
    pub line: bool,
    pub text: &'static str,
    pub needs: &'static [&'static str],
}

impl Residual {
    pub fn input(&self) -> Input {
        Input {
            line: self.line,
            text: self.text.into(),
        }
    }
}

const fn r(text: &'static str, needs: &'static [&'static str]) -> Residual {
    Residual {
        line: false,
        text,
        needs,
    }
}

const fn rl(text: &'static str) -> Residual {
    Residual {
        line: true,
        text,
        needs: &[],
    }
}

/// A family of generated lines that a shell runs as `tole` and the guard
/// allows, for routes whose exact texts are too many to list. Same ratchet as
/// an entry: the family must still have a live member, or it is deleted.
pub struct ResidualClass {
    pub name: &'static str,
    /// Found by this harness and not listed in #1307 or the threat model.
    pub new_finding: bool,
    pub matches: fn(&Input) -> bool,
}

/// A nested `sh -c` / `bash -c` whose payload is written unquoted, with
/// backslash-escaped blanks (`sh -c \(\ sudo\ tole\ x\ \)`): the guard cuts
/// words at the escaped blank, the shell does not.
fn backslash_payload(i: &Input) -> bool {
    ["sh -c ", "bash -c "].iter().any(|k| {
        i.text.match_indices(k).any(|(at, _)| {
            let rest = &i.text[at + k.len()..];
            !rest.starts_with(['\'', '"']) && rest.contains("\\ ")
        })
    })
}

pub const RESIDUAL_CLASSES: &[ResidualClass] = &[ResidualClass {
    name: "nested shell payload with backslash-escaped blanks",
    new_finding: true,
    matches: backslash_payload,
}];

/// Run by a shell as `tole`, allowed by the guard. THE LIST ONLY SHRINKS: when
/// a change to the guard makes it refuse an entry, the test fails and says to
/// delete the entry (#1307).
pub const KNOWN_RESIDUAL: &[Residual] = &[
    // Found in the reviews of #1298 and #1305, listed in #1307.
    r("X+=1 tole x", &["bash"]),
    r("X[0]=1 tole x", &["bash"]),
    r(">/dev/null tole x", &[]),
    r("2>&1 tole x", &[]),
    r("! tole x", &[]),
    r("coproc tole x", &["bash"]),
    r("builtin command tole x", &["bash"]),
    r("builtin exec tole x", &["bash"]),
    // As a command line (an argv): in shell text the guard reads `'tole`.
    rl("env -S 'tole x'"),
    r("$(echo tole) x", &[]),
    r("env \"X=a;c\" tole x", &[]),
    r("env 'X'='a;c' tole x", &[]),
    r("env \\X='a;c' tole x", &[]),
    r("X=a${IFS}b tole x", &[]),
    r("env X=a}b tole x", &[]),
    r("env X{=1 tole x", &[]),
    // Found by this harness (not in #1307): a nested shell whose payload has
    // backslash-escaped blanks. The same text as a command line is refused.
    r("sh -c echo\\ a\\;\\ tole\\ x", &[]),
    r("sh -c \\(\\ tole\\ x\\ \\)", &[]),
    // Documented and not part of #1307: wrappers are not followed into a
    // shell's `-c` text, and the guard does not follow eval, a pipe into a
    // shell, a variable, xargs, find -exec or rg --pre.
    rl("env -i sh -c '@FAKE@/tole x'"),
    rl("env sh -c 'tole x'"),
    rl("sudo sh -c 'tole x'"),
    rl("nohup sh -c 'tole x'"),
    rl("timeout 5 sh -c 'tole x'"),
    r("eval 'tole x'", &[]),
    r("eval tole x", &[]),
    r("echo 'tole x' | sh", &[]),
    r("x=tole; $x y", &[]),
    r("echo x | xargs tole", &["xargs"]),
    r(r"find . -maxdepth 0 -exec tole x \;", &["find"]),
    r("rg --pre ./tole x .", &["rg"]),
];
