//! `tole sandbox list|show|set` (#810): the per-server sandbox profiles of
//! the user config.
//!
//! - `list` shows the sandbox mode, the default profile, every profile
//!   (built in and defined) and the servers bound to one.
//! - `show <server>` shows the profile one server runs in, where that comes
//!   from, and its grants.
//! - `set <server> --profile <p>` binds a server to a built-in or defined
//!   profile by writing `[sandbox.servers.<server>]` in the USER config
//!   (the rest of the file, comments included, is kept). It never touches a
//!   project config: a project config cannot set the sandbox at all, so a
//!   repository can neither bind a server to a wider profile nor define
//!   one. Like the other owner decisions it refuses to run under an agent
//!   session (#684). Profiles themselves are defined by editing the file.
//!
//! `<server>` is the MCP server name; a connection key `name@workspace` is
//! accepted and reduced to its name.

use std::path::Path;

use anyhow::{anyhow, bail};
use clap::Subcommand;

use crate::config::sandbox::BUILTIN_PROFILES;
use crate::config::SandboxConfig;
use crate::hook_cli::{read_config, write_atomic};

/// `tole sandbox <action>`.
#[derive(Subcommand, Debug)]
pub enum SandboxAction {
    /// The sandbox mode, the profiles and the servers bound to one.
    List,
    /// The profile one server runs in, where it comes from, and its grants.
    Show {
        /// The MCP server name (`name@workspace` is reduced to `name`).
        server: String,
    },
    /// Bind a server to a profile in the user config: `tole sandbox set
    /// <server> --profile <p>`, with `<p>` one of `server`, `strict` or a
    /// profile defined in `[sandbox.profiles.<name>]` there. `--profile` is
    /// the global flag of `tole`; here it names the sandbox profile.
    Set {
        /// The MCP server name (`name@workspace` is reduced to `name`).
        server: String,
    },
}

/// Runs a `tole sandbox` action against the user config. `profile` is the
/// global `--profile` flag, which `set` reads as the sandbox profile.
pub fn cli(action: SandboxAction, profile: Option<&str>) -> anyhow::Result<()> {
    let path = crate::config::user_config_path()
        .ok_or_else(|| anyhow!("cannot resolve the user config (set CODECORA_HOME or HOME)"))?;
    let out = match action {
        SandboxAction::List => list(&path)?,
        SandboxAction::Show { server } => show(&path, &server)?,
        SandboxAction::Set { server } => {
            crate::owner_only::refuse_under_session("tole sandbox set").map_err(|e| anyhow!(e))?;
            let profile =
                profile.ok_or_else(|| anyhow!("`tole sandbox set` needs --profile <name>"))?;
            set(&path, &server, profile)?
        }
    };
    print!("{out}");
    Ok(())
}

/// The `[sandbox]` table of `text` (empty when there is none).
fn sandbox_of(path: &Path, text: &str) -> anyhow::Result<SandboxConfig> {
    let cfg = crate::config::parse(text, path).map_err(|e| anyhow!("{e}"))?;
    Ok(cfg.sandbox.unwrap_or_default())
}

/// The server name of what the owner typed: a connection key loses its
/// `@workspace`.
fn server_name(arg: &str) -> anyhow::Result<&str> {
    let name = arg.split('@').next().unwrap_or(arg);
    if name.is_empty() {
        bail!("give an MCP server name, not {arg:?}");
    }
    Ok(name)
}

fn grants(label: &str, v: &[std::path::PathBuf]) -> String {
    if v.is_empty() {
        format!("{label}: none\n")
    } else {
        let items: Vec<String> = v.iter().map(|p| p.display().to_string()).collect();
        format!("{label}: {}\n", items.join(", "))
    }
}

fn shape(p: &tole_core::sandbox::Profile) -> String {
    use tole_core::sandbox::{Network, Workspace};
    format!(
        "workspace {}, network {}",
        match p.workspace {
            Workspace::None => "none",
            Workspace::Read => "read",
            Workspace::Write => "write",
        },
        if p.network == Network::Host {
            "host"
        } else {
            "none"
        }
    )
}

/// `tole sandbox list`.
pub fn list(path: &Path) -> anyhow::Result<String> {
    let sb = sandbox_of(path, &read_config(path)?)?;
    let policy = sb.policy().map_err(|e| anyhow!(e))?;
    let default = sb.effective("").map_err(|e| anyhow!(e))?;
    let mode = sb.mode.as_deref().unwrap_or("off");
    let mut out = format!(
        "sandbox mode: {mode}\ndefault profile: {} [{}]\nprofiles:\n",
        default.name,
        default.source.describe()
    );
    for name in BUILTIN_PROFILES {
        let p = tole_core::sandbox::builtin_profile(name).expect("built in");
        out.push_str(&format!("  {name} (built in): {}\n", shape(&p)));
    }
    for (name, _) in sb.profiles.iter().flatten() {
        let p = sb.resolve_profile(name).map_err(|e| anyhow!(e))?;
        out.push_str(&format!(
            "  {name}: {}, {} read, {} write\n",
            shape(&p),
            p.read.len(),
            p.write.len()
        ));
    }
    out.push_str("servers:\n");
    if policy.servers.is_empty() {
        out.push_str("  none bound (every server runs in the default profile)\n");
    }
    for (server, sp) in &policy.servers {
        out.push_str(&format!("  {server} -> {}\n", sp.name));
    }
    Ok(out)
}

/// `tole sandbox show <server>`.
pub fn show(path: &Path, server: &str) -> anyhow::Result<String> {
    let server = server_name(server)?;
    let sb = sandbox_of(path, &read_config(path)?)?;
    let policy = sb.policy().map_err(|e| anyhow!(e))?;
    let e = sb.effective(server).map_err(|e| anyhow!(e))?;
    let mut out = format!(
        "server: {server}\nprofile: {} [{}]\n{}\n",
        e.name,
        e.source.describe(),
        shape(&e.profile)
    );
    out.push_str(&grants("read", &e.profile.read));
    out.push_str(&grants("write", &e.profile.write));
    let mode = sb.mode.as_deref().unwrap_or("off");
    out.push_str(&format!("sandbox mode: {mode}\n"));
    if policy.skip.iter().any(|s| s == server) {
        out.push_str(
            "skipped: yes, listed in [sandbox] skip, so it runs unsandboxed and the profile is not used\n",
        );
    }
    Ok(out)
}

/// The header of a `[sandbox.servers.<name>]` table written either way.
fn is_server_header(line: &str, name: &str) -> bool {
    let t = line.trim();
    if t.starts_with("[[") || !t.starts_with('[') {
        return false;
    }
    let Some(end) = t.find(']') else {
        return false;
    };
    let inner: String = t[1..end].chars().filter(|c| !c.is_whitespace()).collect();
    inner == format!("sandbox.servers.{name}") || inner == format!("sandbox.servers.\"{name}\"")
}

fn key(name: &str) -> String {
    let bare = name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if bare {
        name.to_string()
    } else {
        toml::Value::String(name.to_string()).to_string()
    }
}

/// `text` with `[sandbox.servers.<server>] profile = "<profile>"` set: the
/// existing table is edited in place, else one is appended.
fn with_binding(text: &str, server: &str, profile: &str) -> String {
    let value = toml::Value::String(profile.to_string()).to_string();
    let line = format!("profile = {value}\n");
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let header = lines.iter().position(|l| is_server_header(l, server));
    let mut out = String::new();
    match header {
        Some(h) => {
            let end = lines[h + 1..]
                .iter()
                .position(|l| l.trim_start().starts_with('['))
                .map_or(lines.len(), |i| h + 1 + i);
            let existing = (h + 1..end).find(|&i| {
                let t = lines[i].trim_start();
                t.strip_prefix("profile")
                    .is_some_and(|r| r.trim_start().starts_with('='))
            });
            for (i, l) in lines.iter().enumerate() {
                if Some(i) == existing {
                    out.push_str(&line);
                } else {
                    out.push_str(l);
                }
                if i == h && existing.is_none() {
                    if !l.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push_str(&line);
                }
            }
        }
        None => {
            out.push_str(text);
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!("\n[sandbox.servers.{}]\n{line}", key(server)));
        }
    }
    out
}

/// `tole sandbox set <server> --profile <p>`.
pub fn set(path: &Path, server: &str, profile: &str) -> anyhow::Result<String> {
    let server = server_name(server)?;
    let old = read_config(path)?;
    let before = crate::config::parse(&old, path).map_err(|e| anyhow!("{e}"))?;
    let sb = before.sandbox.clone().unwrap_or_default();
    sb.resolve_profile(profile).map_err(|e| anyhow!(e))?;
    if sb
        .servers
        .as_ref()
        .and_then(|m| m.get(server))
        .is_some_and(|s| s.profile.as_deref() == Some(profile))
    {
        return Ok(format!(
            "{server} is already bound to profile `{profile}` in {}\n",
            path.display()
        ));
    }
    let new = with_binding(&old, server, profile);
    // The edit must change that one binding and nothing else.
    let after = crate::config::parse(&new, path).map_err(|e| anyhow!("{e}"))?;
    let mut expect = before.clone();
    expect
        .sandbox
        .get_or_insert_with(Default::default)
        .servers
        .get_or_insert_with(Default::default)
        .insert(
            server.to_string(),
            crate::config::sandbox::SandboxServerConfig {
                profile: Some(profile.to_string()),
            },
        );
    if after != expect {
        bail!(
            "could not edit {} safely (is [sandbox.servers] written another way?); add \
             `[sandbox.servers.{}]` with `profile = \"{profile}\"` by hand",
            path.display(),
            key(server)
        );
    }
    write_atomic(path, &new)?;
    let mut msg = format!(
        "bound {server} to profile `{profile}` in {}\n",
        path.display()
    );
    if sb.mode.as_deref().is_none_or(|m| m == "off") {
        msg.push_str(
            "note: [sandbox] mode is off, so the binding applies once the sandbox is on\n",
        );
    }
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bound(text: &str, server: &str, profile: &str) -> String {
        let new = with_binding(text, server, profile);
        let cfg = crate::config::parse(&new, Path::new("c.toml")).unwrap();
        let sb = cfg.sandbox.unwrap();
        assert_eq!(
            sb.servers.unwrap()[server].profile.as_deref(),
            Some(profile),
            "{new}"
        );
        new
    }

    #[test]
    fn appends_a_table_after_a_file_without_a_final_newline() {
        let new = bound("model = \"m\"", "gh", "strict");
        assert_eq!(
            new,
            "model = \"m\"\n\n[sandbox.servers.gh]\nprofile = \"strict\"\n"
        );
    }

    #[test]
    fn edits_the_profile_line_of_an_existing_table_and_keeps_its_neighbours() {
        let old = "[sandbox.servers.a]\nprofile = \"server\"\n[sandbox.servers.b]\nprofile = \"server\"\n";
        let new = bound(old, "b", "strict");
        assert_eq!(
            new,
            "[sandbox.servers.a]\nprofile = \"server\"\n[sandbox.servers.b]\nprofile = \"strict\"\n"
        );
    }

    #[test]
    fn inserts_the_profile_into_a_table_that_has_none_and_reads_quoted_headers() {
        let new = bound("[ sandbox.servers.\"a\" ]\n# note\n", "a", "strict");
        assert_eq!(
            new,
            "[ sandbox.servers.\"a\" ]\nprofile = \"strict\"\n# note\n"
        );
        let new = bound("[sandbox.servers.a]", "a", "strict");
        assert_eq!(new, "[sandbox.servers.a]\nprofile = \"strict\"\n");
    }

    #[test]
    fn a_name_that_is_not_a_bare_key_is_quoted() {
        let new = bound("", "my.srv", "strict");
        assert!(new.contains("[sandbox.servers.\"my.srv\"]"), "{new}");
    }

    #[test]
    fn a_connection_key_is_reduced_to_the_server_name() {
        assert_eq!(server_name("gh@/w/x").unwrap(), "gh");
        assert_eq!(server_name("gh").unwrap(), "gh");
        assert!(server_name("@/w").is_err());
        assert!(server_name("").is_err());
    }
}
