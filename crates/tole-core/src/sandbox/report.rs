//! What the sandbox decides and grants, as text (#809, design §11): the
//! `tole doctor` listing of each sandboxed server and consumer, and the
//! start-up log line of each server's decision. Names, modes and paths the
//! owner wrote themselves; never environment values or the `bwrap` command
//! line.

use super::command::{Consumer, Consumers};
use super::{check_grant, Availability, Launch, Mode, Network, Policy, Profile, Workspace};
use std::collections::HashSet;
use std::path::{Component, Path};
use std::sync::Mutex;

/// What happens to a server or consumer that asks for a sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Sandboxed,
    /// `optional` and no working sandbox, a human attached: runs as before.
    Unsandboxed(String),
    /// `required`, or `optional` in an unattended run, and no sandbox.
    Refused(String),
}

/// The decision for `mode` on this machine. `None` when the mode is off.
/// Pure; [`super::plan`] reaches the same outcome for a launch.
pub fn decide(mode: Mode, avail: &Availability, unattended: bool) -> Option<Decision> {
    match (mode, avail) {
        (Mode::Off, _) => None,
        (_, Availability::Usable { .. }) => Some(Decision::Sandboxed),
        (Mode::Optional, Availability::Unavailable(why)) if !unattended => {
            Some(Decision::Unsandboxed(why.clone()))
        }
        (_, Availability::Unavailable(why)) => Some(Decision::Refused(why.clone())),
    }
}

fn decision_text(d: &Decision) -> String {
    match d {
        Decision::Sandboxed => "sandboxed".into(),
        Decision::Unsandboxed(why) => format!("unsandboxed, no sandbox ({why}); optional"),
        Decision::Refused(why) => format!("refused, no sandbox ({why})"),
    }
}

fn mode_word(m: Mode) -> &'static str {
    match m {
        Mode::Off => "off",
        Mode::Optional => "optional",
        Mode::Required => "required",
    }
}

/// Why a path is sensitive: it holds `$HOME` (or is `$HOME`), or it is or is
/// inside a credential directory. `None` for anything else. Judged on the
/// path as given and, when it resolves, on the resolved one too.
pub fn sensitive_kind(path: &Path, home: Option<&Path>) -> Option<&'static str> {
    const CREDS: [(&str, &str); 8] = [
        (".ssh", "ssh keys"),
        (".gnupg", "gpg keys"),
        (".aws", "aws credentials"),
        (".azure", "azure credentials"),
        (".kube", "kube credentials"),
        (".docker", "docker credentials"),
        (".netrc", "netrc credentials"),
        (".config/gcloud", "gcloud credentials"),
    ];
    let judge = |p: &Path| -> Option<&'static str> {
        if let Some(h) = home {
            if h.starts_with(p) {
                return Some("holds $HOME");
            }
        } else if ["/home", "/root"]
            .iter()
            .any(|h| Path::new(h).starts_with(p))
        {
            return Some("holds $HOME");
        }
        let parts: Vec<&str> = p
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => s.to_str(),
                _ => None,
            })
            .collect();
        CREDS.iter().find_map(|(name, what)| {
            let want: Vec<&str> = name.split('/').collect();
            parts
                .windows(want.len())
                .any(|w| w == want)
                .then_some(*what)
        })
    };
    let resolved = std::fs::canonicalize(path).ok();
    judge(path).or_else(|| resolved.as_deref().and_then(judge))
}

fn grant_line(
    label: &str,
    kind: &str,
    p: &Path,
    home: Option<&Path>,
    data_dir: Option<&Path>,
) -> String {
    let shown = p.display();
    let flag = match check_grant(p, home, data_dir) {
        Err(e) => format!(" REFUSED ({e})"),
        Ok(_) => match sensitive_kind(p, home) {
            Some(why) => format!(" SENSITIVE ({why})"),
            None => String::new(),
        },
    };
    format!("{label}: {kind} {shown}{flag}")
}

/// Grant lines of one profile: workspace access, network, and every extra
/// read and write path with its sensitivity. A line with `SENSITIVE` or
/// `REFUSED` marks a grant that touches a credential path or `$HOME`.
pub fn profile_lines(
    label: &str,
    mode: Mode,
    profile: &Profile,
    home: Option<&Path>,
    data_dir: Option<&Path>,
) -> Vec<String> {
    let ws = match profile.workspace {
        Workspace::None => "none",
        Workspace::Read => "read-only",
        Workspace::Write => "writable",
    };
    let net = match profile.network {
        Network::None => "none",
        Network::Host => "host (reaches the network)",
    };
    let mut out = vec![format!(
        "{label}: mode {}, workspace {ws}, network {net}",
        mode_word(mode)
    )];
    for p in &profile.read {
        out.push(grant_line(label, "read", p, home, data_dir));
    }
    for p in &profile.write {
        out.push(grant_line(label, "write", p, home, data_dir));
    }
    out
}

/// The `tole doctor` listing for the stdio MCP servers (`servers` are their
/// names) and the model-command consumers. Nothing for what is off.
pub fn doctor_grant_lines(
    policy: Option<&Policy>,
    consumers: &Consumers,
    avail: &Availability,
    unattended: bool,
    servers: &[String],
    home: Option<&Path>,
    data_dir: Option<&Path>,
) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(p) = policy.filter(|p| p.mode != Mode::Off) {
        let d = decide(p.mode, avail, unattended);
        for name in servers {
            let label = format!("sandbox mcp[{name}]");
            if p.skip.iter().any(|s| s == name) {
                out.push(format!("{label}: unsandboxed (listed in [sandbox] skip)"));
                continue;
            }
            out.extend(profile_lines(
                &label,
                p.mode,
                p.profile_for(name).1,
                home,
                data_dir,
            ));
            out.extend(d.iter().map(|d| format!("{label}: {}", decision_text(d))));
        }
    }
    for k in Consumer::ALL {
        let mode = consumers.mode(k);
        if mode == Mode::Off {
            continue;
        }
        let label = format!("sandbox consumer {}", k.name());
        let profile = super::command::command_profile(consumers.network);
        out.extend(profile_lines(&label, mode, &profile, home, data_dir));
        out.push(format!(
            "{label}: protected read-only in the workspace: {}",
            super::command::PROTECTED.join(", ")
        ));
        out.extend(
            decide(mode, avail, unattended || k.always_unattended())
                .map(|d| format!("{label}: {}", decision_text(&d))),
        );
    }
    out
}

/// The start-up line for one launch of server `name`. `None` when the
/// sandbox is off (nothing to say).
pub fn startup_line(policy: Option<&Policy>, name: &str, launch: &Launch) -> Option<String> {
    let p = policy.filter(|p| p.mode != Mode::Off)?;
    Some(match launch {
        Launch::Sandboxed { .. } => format!(
            "mcp[{name}]: sandboxed (profile {}, mode {})",
            p.profile_name,
            mode_word(p.mode)
        ),
        Launch::Direct { warning: Some(w) } => w.clone(),
        Launch::Direct { warning: None } => {
            format!("mcp[{name}]: unsandboxed (listed in [sandbox] skip)")
        }
        Launch::Refused(why) => why.clone(),
    })
}

/// Logs [`startup_line`] to stderr once per server name and process (a
/// server that restarts after idling does not repeat it).
pub fn log_startup_once(name: &str, launch: &Launch) {
    static SEEN: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let Some(line) = startup_line(super::session_policy().as_ref(), name, launch) else {
        return;
    };
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if seen
        .get_or_insert_with(HashSet::new)
        .insert(name.to_string())
    {
        eprintln!("tole: {line}");
    }
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod tests;
