//! `tole doctor` warnings for MCP servers (#600): an unpinned launcher, a
//! third-party stdio server no sandbox applies to, and a broad secret grant.
//! Pure: the host passes what it knows.

use std::collections::BTreeMap;

use tole_core::plugin_launcher::unpinned_launcher;
use tole_core::plugin_lock::{tier_of, LockEntry, Tier};
use tole_core::sandbox::{Availability, Mode, Policy};

/// A server as configured (`--mcp-server` or `mcp_server`).
#[derive(Debug, Clone)]
pub struct Configured {
    pub name: String,
    /// The program and arguments of a stdio server; `None` for a URL.
    pub stdio: Option<(String, Vec<String>)>,
}

/// More secrets than this on one server is a broad grant.
pub const BROAD_GRANT: usize = 3;

/// One line per finding, each starting with `  ! `.
pub fn warnings(
    lock: &BTreeMap<String, LockEntry>,
    servers: &[Configured],
    first_party: &[String],
    sandbox: Option<&Policy>,
    backend: &Availability,
) -> Vec<String> {
    let mut out = Vec::new();
    for s in servers {
        let entry = lock.get(&s.name);
        let tier = tier_of(&s.name, entry, first_party);
        if let Some((command, args)) = &s.stdio {
            if let Some(why) = unpinned_launcher(command, args) {
                out.push(format!("  ! {}: unpinned launcher: {why}", s.name));
            }
            let wrapped = sandbox
                .is_some_and(|p| p.mode != Mode::Off && !p.skip.iter().any(|n| n == &s.name))
                && matches!(backend, Availability::Usable { .. });
            if tier != Tier::FirstParty && !wrapped {
                out.push(format!(
                    "  ! {}: {} stdio server runs with no sandbox here (`[sandbox] mode`, \
                     bubblewrap on Linux); `tole plugin add` and `approve` refuse such a server",
                    s.name,
                    tier.as_str()
                ));
            }
        }
    }
    for (name, e) in lock {
        if e.revoked {
            continue;
        }
        let tier = tier_of(name, Some(e), first_party);
        let n = e.secrets.len();
        if (tier != Tier::FirstParty && n > 0) || n > BROAD_GRANT {
            out.push(format!(
                "  ! {name}: broad grant: {} server holds {n} secret(s) ({}); narrow the \
                 `[secrets]` release and approve it again",
                tier.as_str(),
                e.secrets.join(", ")
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tole_core::sandbox::builtin_profile;

    fn cfg(name: &str, command: &str, args: &[&str]) -> Configured {
        Configured {
            name: name.into(),
            stdio: Some((command.into(), args.iter().map(|s| s.to_string()).collect())),
        }
    }

    fn entry(secrets: &[&str]) -> LockEntry {
        LockEntry {
            secrets: secrets.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn policy() -> Policy {
        Policy {
            mode: Mode::Required,
            profile_name: "server".into(),
            profile: builtin_profile("server").unwrap(),
            skip: Vec::new(),
            max_servers: 4,
            idle_secs: 60,
        }
    }

    fn usable() -> Availability {
        Availability::Usable {
            bwrap: "/usr/bin/bwrap".into(),
            version: "0.9.0".into(),
        }
    }

    fn gone() -> Availability {
        Availability::Unavailable("no bwrap".into())
    }

    fn fp() -> Vec<String> {
        vec!["cora".into()]
    }

    #[test]
    fn an_unpinned_launcher_is_reported_and_a_pinned_one_is_not() {
        let servers = [
            cfg("a", "npx", &["-y", "some-mcp"]),
            cfg("b", "npx", &["-y", "some-mcp@1.2.3"]),
            cfg("c", "docker", &["run", "-i", "img"]),
        ];
        let w = warnings(
            &BTreeMap::new(),
            &servers,
            &fp(),
            Some(&policy()),
            &usable(),
        );
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w[0].contains("a: unpinned launcher") && w[1].contains("c: unpinned launcher"));
    }

    #[test]
    fn a_third_party_server_without_a_sandbox_is_reported() {
        let servers = [cfg("third", "./srv", &[]), cfg("cora", "./cora", &[])];
        for (p, b) in [
            (None, usable()),
            (Some(policy()), gone()),
            (
                Some(Policy {
                    mode: Mode::Off,
                    ..policy()
                }),
                usable(),
            ),
        ] {
            let w = warnings(&BTreeMap::new(), &servers, &fp(), p.as_ref(), &b);
            assert_eq!(w.len(), 1, "{w:?}");
            assert!(
                w[0].contains("third:") && w[0].contains("no sandbox"),
                "{w:?}"
            );
        }
        // Wrapped: nothing to say. A skipped name is the owner's own.
        assert!(warnings(
            &BTreeMap::new(),
            &servers,
            &fp(),
            Some(&policy()),
            &usable()
        )
        .is_empty());
        let mut skip = policy();
        skip.skip.push("third".into());
        let skipped = [cfg("third", "./srv", &[])];
        let mut fp2 = fp();
        fp2.push("third".into());
        assert!(warnings(&BTreeMap::new(), &skipped, &fp2, Some(&skip), &usable()).is_empty());
        // A remote server has no process to sandbox.
        let remote = [Configured {
            name: "r".into(),
            stdio: None,
        }];
        assert!(warnings(&BTreeMap::new(), &remote, &fp(), None, &gone()).is_empty());
    }

    #[test]
    fn broad_grants_are_reported_by_tier() {
        let mut lock = BTreeMap::new();
        lock.insert("third".to_string(), entry(&["A_KEY"]));
        lock.insert("cora".to_string(), entry(&["A_KEY", "B_KEY"]));
        lock.insert("wide".to_string(), entry(&["A", "B", "C", "D"]));
        lock.insert("none".to_string(), entry(&[]));
        let mut revoked = entry(&["X"]);
        revoked.revoked = true;
        lock.insert("gone".to_string(), revoked);
        let w = warnings(&lock, &[], &fp(), None, &gone());
        let names: Vec<&str> = w
            .iter()
            .map(|l| l.split(':').next().unwrap().trim())
            .collect();
        assert_eq!(names, vec!["! third", "! wide"], "{w:?}");
        assert!(w[1].contains("4 secret(s)"));
        // First-party with many secrets is broad too.
        lock.insert("cora".to_string(), entry(&["A", "B", "C", "D"]));
        assert!(warnings(&lock, &[], &fp(), None, &gone())
            .iter()
            .any(|l| l.contains("cora: broad grant")));
    }
}
