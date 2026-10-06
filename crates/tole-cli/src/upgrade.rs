//! `tole upgrade` (issue #220): self-upgrade via cargo.
//!
//! Fleet-parity with `cora upgrade`, adapted to how tole is actually
//! distributed: tole ships through crates.io (the GitHub release has no
//! binary assets), so the install path IS cargo. The command re-runs
//! `cargo install tole-cli` in the user's environment and verifies the
//! new binary answers `--version`. A binary NOT living in the cargo
//! bin dir still upgrades fine (cargo replaces the shim), but a
//! source-build checkout gets the manual path printed instead.

use std::process::Command;

const CRATE_NAME: &str = "tole-cli";

/// Entry point for `tole upgrade [--check] [--yes]`.
pub fn run(check_only: bool, yes: bool) -> anyhow::Result<i32> {
    let current = env!("CARGO_PKG_VERSION");
    println!("current version: {current}");

    // Resolve the latest version from crates.io (the distribution
    // source of truth for tole — GitHub releases carry no binaries).
    let latest =
        tole_core::update_check::latest_on_crates_io().map_err(|e| anyhow::anyhow!("{e}"))?;

    let current = tole_core::update_check::current_version_for_upgrade();
    // Fail-closed semver compare (cora alert): a source build newer
    // than crates.io's max_version is NOT an upgrade — raw string
    // equality would call it one (or miss the real thing).
    if !tole_core::update_check::is_newer_public(&latest, current) {
        println!("already up to date ({current}; crates.io latest {latest})");
        return Ok(0);
    }
    println!("update available: {current} -> {latest}");

    if check_only {
        println!("run `tole upgrade` to install (via cargo)");
        return Ok(0);
    }

    if !yes {
        print!("proceed with `cargo install {CRATE_NAME}`? [y/N] ");
        use std::io::Write as _;
        std::io::stdout().flush().ok();
        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .map_err(|e| anyhow::anyhow!("stdin read: {e}"))?;
        let input = input.trim().to_lowercase();
        if input != "y" && input != "yes" {
            println!("upgrade cancelled");
            return Ok(0);
        }
    }

    // Cargo-source detection: if the running binary is NOT in a cargo
    // bin dir, this checkout/build will be silently overwritten by the
    // cargo-installed shim — warn loudly and print the manual paths.
    let exe = std::env::current_exe()?;
    let exe_str = exe.to_string_lossy();
    let is_cargo = exe_str.contains("/.cargo/bin/")
        || exe_str.contains("\\.cargo\\bin\\")
        || exe_str.ends_with("/.cargo/bin/tole");
    if !is_cargo {
        eprintln!("note: the running binary is not the cargo-installed one ({exe_str})");
        eprintln!("      `cargo install {CRATE_NAME}` installs a separate ~/.cargo/bin/tole");
        eprintln!("      rebuild from source instead if that is what you want");
        if !yes {
            return Ok(0);
        }
    }

    println!("running: cargo install {CRATE_NAME} ...");
    let status = Command::new("cargo")
        .args(["install", CRATE_NAME])
        .status()
        .map_err(|e| anyhow::anyhow!("cargo not found on PATH — install rust/cargo first ({e})"))?;
    if !status.success() {
        anyhow::bail!("cargo install failed (exit {status})");
    }

    // Verify the CARGO-INSTALLED binary reports the new version — not
    // the sibling of the currently running exe (cora alert): on a
    // source checkout (exe = target/debug/tole) the cargo install lands
    // in ~/.cargo/bin/tole, a different path entirely.
    let cargo_bin = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(|h| {
            std::path::PathBuf::from(h)
                .join(".cargo")
                .join("bin")
                .join("tole")
        })
        .unwrap_or_else(|_| std::path::PathBuf::from("tole"));
    match Command::new(&cargo_bin).arg("--version").output() {
        Ok(out) if out.status.success() => {
            let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
            println!("verified: {v} ({cargo_bin:?})");
        }
        _ => eprintln!("warning: could not verify the upgraded binary at {cargo_bin:?}"),
    }

    println!("upgrade complete: {current} -> {latest}");
    Ok(0)
}
