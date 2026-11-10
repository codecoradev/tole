//! `tole setup cora` (#535): install or upgrade the cora binary that the
//! review flow, `mcp_cora_*` and `cora_search` need.
//!
//! Same distribution model and trust as `tole upgrade` (#220): crates.io is
//! the source of truth and the install IS `cargo install`, so cargo verifies
//! the registry checksum and tole downloads and executes nothing itself. The
//! command is a fixed constant ([`ecosystem::Sibling::install`]), never built
//! from user input, and it never runs unprompted: it needs a `y` at a
//! terminal or an explicit `--yes`. It is a CLI subcommand only; no tool
//! exposes it to a model.
//!
//! Embedding cora instead is deliberately not done: `cora-code` ships a CLI
//! binary (`[[bin]] cora`) and no library target, and its tree-sitter parsers
//! would be a large size and release-coupling cost for tole.

use std::io::{IsTerminal, Write as _};
use std::process::Command;
use tole_cli::ecosystem::{self, Sibling, Status};
use tole_cli::tool_roster::binary_available;

/// What can be set up. Only `cora` today (the issue's scope); the table
/// behind it already knows `uteke`, so adding it later is one variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Tool {
    Cora,
}

impl Tool {
    fn sibling(self) -> &'static Sibling {
        let bin = match self {
            Tool::Cora => "cora",
        };
        ecosystem::SIBLINGS
            .iter()
            .find(|s| s.bin == bin)
            .expect("every setup tool has a sibling entry")
    }
}

/// What `tole setup` does for a probe result; pure so it is testable.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Already at or above the supported floor.
    Nothing,
    /// `--check`: report only.
    ReportOnly,
    /// Needs an install but `cargo` is not on PATH.
    NoCargo,
    /// Needs an install, there is no terminal to confirm at, and no `--yes`.
    NeedsYes,
    /// Ask `[y/N]` first.
    Prompt,
    /// `--yes`: go.
    Install,
}

pub fn decide(
    status: &Status,
    cargo_available: bool,
    check: bool,
    yes: bool,
    interactive: bool,
) -> Action {
    if matches!(status, Status::Supported(_)) {
        return Action::Nothing;
    }
    if check {
        return Action::ReportOnly;
    }
    if !cargo_available {
        return Action::NoCargo;
    }
    if yes {
        return Action::Install;
    }
    if !interactive {
        return Action::NeedsYes;
    }
    Action::Prompt
}

/// `$CARGO_HOME/bin` (default `~/.cargo/bin`): where `cargo install` puts
/// the binary, which may not be on PATH yet.
fn cargo_bin_dir() -> Option<std::path::PathBuf> {
    if let Some(h) = std::env::var_os("CARGO_HOME").filter(|h| !h.is_empty()) {
        return Some(std::path::PathBuf::from(h).join("bin"));
    }
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|h| std::path::PathBuf::from(h).join(".cargo").join("bin"))
}

/// Entry point for `tole setup <tool> [--check] [--yes]`. Exit codes: 0 done
/// or nothing to do, 1 could not install, 2 refused (no terminal, no `--yes`).
pub fn run(tool: Tool, check: bool, yes: bool) -> anyhow::Result<i32> {
    let sib = tool.sibling();
    let status = ecosystem::probe(sib);
    println!("{}", ecosystem::line(sib, &status));

    let action = decide(
        &status,
        binary_available("cargo"),
        check,
        yes,
        std::io::stdin().is_terminal(),
    );
    match action {
        Action::Nothing => {
            println!("nothing to do: {} is at a supported version", sib.bin);
            return Ok(0);
        }
        Action::ReportOnly => {
            println!("run `tole setup {}` to install ({})", sib.bin, sib.install);
            return Ok(0);
        }
        Action::NoCargo => {
            eprintln!(
                "cargo not found on PATH: install Rust first (https://rustup.rs), then run: {}",
                sib.install
            );
            return Ok(1);
        }
        Action::NeedsYes => {
            eprintln!(
                "refusing to install without a terminal to confirm: re-run with --yes, or run: {}",
                sib.install
            );
            return Ok(2);
        }
        Action::Prompt => {
            print!("proceed with `{}`? [y/N] ", sib.install);
            std::io::stdout().flush().ok();
            let mut input = String::new();
            std::io::stdin()
                .read_line(&mut input)
                .map_err(|e| anyhow::anyhow!("stdin read: {e}"))?;
            if !matches!(input.trim().to_lowercase().as_str(), "y" | "yes") {
                println!("setup cancelled");
                return Ok(0);
            }
        }
        Action::Install => {}
    }

    // The install command is the one constant the doctor also prints; split
    // it instead of keeping a second copy that could drift.
    let mut parts = sib.install.split_whitespace();
    let program = parts.next().unwrap_or("cargo");
    println!("running: {} ...", sib.install);
    let st = Command::new(program)
        .args(parts)
        .status()
        .map_err(|e| anyhow::anyhow!("could not run {program}: {e}"))?;
    if !st.success() {
        eprintln!("{} failed ({st})", sib.install);
        return Ok(1);
    }

    // Verify the binary cargo just installed, at its cargo path: PATH may
    // not list ~/.cargo/bin, or may still resolve an older copy first.
    let installed = cargo_bin_dir().map(|d| d.join(sib.bin));
    let found = installed.as_ref().and_then(|p| {
        let mut cmd = Command::new(p);
        cmd.arg("--version");
        tole_core::subprocess::run_with_timeout(&mut cmd, std::time::Duration::from_secs(5))
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
    });
    match ecosystem::classify(sib, found.as_deref()) {
        Status::Supported(v) => {
            println!(
                "installed: {} {}.{}.{} ({})",
                sib.bin,
                v.0,
                v.1,
                v.2,
                installed
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            );
            // What tole will actually run is whatever PATH resolves first.
            if !matches!(ecosystem::probe(sib), Status::Supported(_)) {
                if let Some(dir) = cargo_bin_dir() {
                    eprintln!(
                        "note: {} on PATH is still missing or too old; put {} first on PATH",
                        sib.bin,
                        dir.display()
                    );
                }
            }
            Ok(0)
        }
        other => {
            eprintln!(
                "the install finished but the result is not usable: {}",
                ecosystem::line(sib, &other).trim()
            );
            Ok(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: Status = Status::Supported((0, 20, 0));

    #[test]
    fn a_supported_binary_is_never_touched() {
        for (check, yes, tty) in [
            (false, false, true),
            (false, true, false),
            (true, true, true),
        ] {
            assert_eq!(decide(&OK, true, check, yes, tty), Action::Nothing);
            assert_eq!(decide(&OK, false, check, yes, tty), Action::Nothing);
        }
    }

    #[test]
    fn check_only_reports_for_every_non_supported_state() {
        for st in [
            Status::Missing,
            Status::TooOld((0, 18, 0)),
            Status::Unknown("?".into()),
        ] {
            assert_eq!(decide(&st, true, true, true, true), Action::ReportOnly);
            assert_eq!(decide(&st, false, true, false, false), Action::ReportOnly);
        }
    }

    #[test]
    fn an_install_needs_cargo_and_a_yes_or_a_terminal() {
        let st = Status::Missing;
        assert_eq!(decide(&st, false, false, true, true), Action::NoCargo);
        assert_eq!(decide(&st, true, false, true, false), Action::Install);
        assert_eq!(decide(&st, true, false, false, true), Action::Prompt);
        // Never silent: no terminal and no --yes is a refusal.
        assert_eq!(decide(&st, true, false, false, false), Action::NeedsYes);
    }

    #[test]
    fn the_cora_entry_is_the_floor_and_command_the_doctor_prints() {
        let s = Tool::Cora.sibling();
        assert_eq!(s.bin, "cora");
        assert_eq!(s.install, "cargo install cora-code --locked");
    }
}
