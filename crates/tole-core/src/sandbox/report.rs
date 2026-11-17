//! What the sandbox decides and grants, as text (#809, design §11): the
//! `tole doctor` listing of each sandboxed server and consumer, and the
//! start-up log line of each server's decision. Names, modes and paths the
//! owner wrote themselves; never environment values or the `bwrap` command
//! line.

use super::command::{Consumer, Consumers};
use super::{check_grant, Availability, Launch, Mode, Network, Policy, Profile, Workspace};
use std::collections::HashSet;
use std::ffi::OsString;
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
    const CREDS: [(&str, &str); 16] = [
        (".ssh", "ssh keys"),
        (".gnupg", "gpg keys"),
        (".aws", "aws credentials"),
        (".azure", "azure credentials"),
        (".kube", "kube credentials"),
        (".docker", "docker credentials"),
        (".netrc", "netrc credentials"),
        (".config/gcloud", "gcloud credentials"),
        (".config/gh", "github cli credentials"),
        (".config/glab-cli", "gitlab cli credentials"),
        (".config/op", "1password cli credentials"),
        (".password-store", "password store"),
        (".npmrc", "npm credentials"),
        (".pypirc", "pypi credentials"),
        (".cargo/credentials", "cargo credentials"),
        (".cargo/credentials.toml", "cargo credentials"),
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
        // The default flip (#748): no backend at all is an unsandboxed run
        // with a loud note, not a refusal, and it says it is the default.
        let d = match (consumers.implicit(k), avail) {
            (true, Availability::Unavailable(why)) => Some(Decision::Unsandboxed(why.clone())),
            _ => decide(mode, avail, unattended || k.always_unattended()),
        };
        let tag = if consumers.implicit(k) {
            " (default)"
        } else {
            ""
        };
        out.extend(d.map(|d| format!("{label}: {}{tag}", decision_text(&d))));
    }
    out
}

/// The first 12 hex digits of a `sha256:<hex>` profile hash, as the start-up
/// line shows it. The full hash is in the lock and the ledger; this is enough
/// to tell two profiles apart in a log.
pub fn short_hash(hash: &str) -> String {
    let hex = hash.strip_prefix("sha256:").unwrap_or(hash);
    format!("sha256:{}", hex.chars().take(12).collect::<String>())
}

/// The start-up line for one launch of server `name`: one line, with every
/// control and invisible character in it (a server or profile name from the
/// config, a reason) shown as a visible escape. A sandboxed server's line
/// names the profile IT runs in (its own binding, else the default) and the
/// short hash of that profile's grants, the identity the lock and the ledger
/// hold in full. `None` when the sandbox is off (nothing to say).
pub fn startup_line(policy: Option<&Policy>, name: &str, launch: &Launch) -> Option<String> {
    let p = policy.filter(|p| p.mode != Mode::Off)?;
    let line = match launch {
        Launch::Sandboxed { .. } => {
            let (profile_name, profile) = p.profile_for(name);
            format!(
                "mcp[{name}]: sandboxed (profile {profile_name} {}, mode {})",
                short_hash(&profile.hash()),
                mode_word(p.mode)
            )
        }
        Launch::Direct { warning: Some(w) } => w.clone(),
        Launch::Direct { warning: None } => {
            format!("mcp[{name}]: unsandboxed (listed in [sandbox] skip)")
        }
        Launch::Refused(why) => why.clone(),
    };
    Some(crate::sanitize::sanitize_one_line(&line))
}

/// The environment switch that makes the sandbox print the launch command
/// line to stderr (`TOLE_VERBOSE=1`, the same switch as the probe notices).
pub const VERBOSE_ENV: &str = "TOLE_VERBOSE";

/// Whether a `TOLE_VERBOSE` value turns verbose output on.
pub fn verbose_value(v: &str) -> bool {
    matches!(v.trim(), "1" | "true" | "yes")
}

/// `argv` as one printable, single-line, redacted string for the verbose
/// log. Redacted: every value of the process's secret set
/// ([`crate::secrets::process_secrets`], in all its encodings), and the
/// value of a flag whose NAME is secret-shaped
/// ([`crate::secrets::is_secret_name`]: `--token x`, `--api-key=x`). Control
/// and invisible characters are shown as visible escapes. A best-effort view
/// for a human reading stderr: a secret tole does not know, passed under an
/// innocent flag name, is not recognised.
pub fn redact_argv(argv: &[OsString]) -> String {
    redact_argv_with(argv, &crate::secrets::process_secrets())
}

const HIDDEN: &str = "<redacted>";

fn redact_argv_with(argv: &[OsString], set: &crate::secrets::SecretSet) -> String {
    // A secret-shaped NAME, with any leading dashes and `-` read as `_`.
    let secret_name = |n: &str| {
        let name = n.trim_start_matches('-');
        !name.is_empty() && crate::secrets::is_secret_name(&name.replace('-', "_"))
    };
    let secret_flag = |flag: &str| flag.starts_with('-') && secret_name(flag);
    // `NAME=VALUE` with a secret-shaped NAME, at the start or after another
    // `=` (`API_KEY=x`, `--api-key=x`, `--env=API_KEY=x`): the value hidden.
    fn assignment(text: &str, secret_name: &dyn Fn(&str) -> bool) -> Option<String> {
        let (left, right) = text.split_once('=')?;
        if secret_name(left) {
            Some(format!("{left}={HIDDEN}"))
        } else {
            assignment(right, secret_name).map(|r| format!("{left}={r}"))
        }
    }
    let mut hide_next = false;
    let mut out = Vec::with_capacity(argv.len());
    for a in argv {
        let text = a.to_string_lossy();
        let shown = if std::mem::take(&mut hide_next) {
            HIDDEN.to_string()
        } else if let Some(hidden) = assignment(&text, &secret_name) {
            hidden
        } else {
            hide_next = secret_flag(&text);
            set.redact(&text).into_owned()
        };
        let shown = crate::sanitize::sanitize_one_line(&shown);
        let plain = !shown.is_empty()
            && !shown.contains(|c: char| c.is_whitespace() || c == '\'' || c == '"');
        out.push(if plain {
            shown
        } else {
            format!("'{}'", shown.replace('\'', "'\\''"))
        });
    }
    out.join(" ")
}

/// The verbose line for a sandboxed launch of `label` (a server or consumer
/// name): one line, the command line redacted by [`redact_argv_with`].
fn launch_argv_line(
    label: &str,
    program: &Path,
    args: &[OsString],
    set: &crate::secrets::SecretSet,
) -> String {
    let mut argv = vec![program.as_os_str().to_os_string()];
    argv.extend(args.iter().cloned());
    format!(
        "{}: sandbox command line: {}",
        crate::sanitize::sanitize_one_line(label),
        redact_argv_with(&argv, set)
    )
}

/// With `TOLE_VERBOSE=1`: prints the command line of a sandboxed launch of
/// `label` (a server or consumer name) to stderr, redacted by
/// [`redact_argv`]. Stderr only, never the session log. Nothing without the
/// switch.
pub fn log_launch_argv(label: &str, program: &Path, args: &[OsString]) {
    if std::env::var(VERBOSE_ENV).is_ok_and(|v| verbose_value(&v)) {
        let set = crate::secrets::process_secrets();
        eprintln!("tole: {}", launch_argv_line(label, program, args, &set));
    }
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
        if let Launch::Sandboxed { program, args } = launch {
            log_launch_argv(&format!("mcp[{name}]"), program, args);
        }
    }
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod tests;
