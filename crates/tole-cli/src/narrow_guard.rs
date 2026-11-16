//! Under an agent session, a flag or an environment variable may only NARROW
//! the owner's policy lists (#865).
//!
//! The resolved lists (`allow`, `trust`, the hook lists) follow
//! `flag > env > config`, and a higher layer REPLACES the whole list below it.
//! So a `tole run --allow '*'` that a script started by `run_command` or
//! `job_start` runs would replace the owner's `allow` from the user config,
//! `--on-turnend x` would drop the owner's stop gate, `TOLE_TRUST=all` the
//! owner's trust presets. This guard refuses that, for the keys the USER config
//! actually sets:
//!
//! - `allow`: every entry must be one of the user's entries (exact text), and
//!   every deny pattern (`!x`) of the user must be kept (a deny only narrows;
//!   the flag may add its own);
//! - `trust`: every preset must be one of the user's;
//! - `on_pretool`, `on_posttool`, `on_turnend`: the list must contain every hook
//!   of the user's (a flag may add hooks, never drop one);
//! - `plan_mode`: a flag can only turn it on (`--plan-mode`; there is no way to
//!   turn it off from the command line or the environment), so nothing here
//!   can loosen it and no refusal path exists for it.
//!
//! WHO IT APPLIES TO: a process that carries `TOLE_UNDER_SESSION` (what
//! `run_command` and `job_start` set on everything they start) and NO child
//! marker (`TOLE_AGENT_DEPTH`, `TOLE_AGENT_ID`, the delegation variables), and
//! that is not `serve`, `acp` or `mcp`. A child tole itself starts
//! (`agent_start`, a delegated child) is exempt: its allow list is enforced in
//! the parent (`list_covers` against the parent's own scope, which may legitimately
//! be wider than the user config after an owner's `--allow`), so comparing it
//! with the user config would refuse a legitimate child. The unattended faces
//! are started by the owner with their own flags and are exempt too. An owner
//! at a terminal has no marker and is unchanged.
//!
//! WHAT IT CHECKS: flags and environment only. The project config and `--config`
//! are not inputs here: they pass the owner's trust gate (`tole config trust`).
//!
//! WHAT IT IS NOT: a boundary. It is keyed on the same environment marker as
//! every #684 guard (and #867): a process that clears its environment (a copied
//! binary run with `env -i`), forges a depth marker, or runs under a pty with no
//! marker is not stopped. The boundary is the command sandbox, which masks the
//! data dir (`[sandbox.consumers] run_command = "required"`).

use crate::config::Config;
use tole_core::child_env;

/// What the host knows about this run.
#[derive(Debug, Clone, Copy)]
pub struct Context {
    /// `TOLE_UNDER_SESSION` is set (non-blank).
    pub session_marker: bool,
    /// A marker of a child tole itself started is present.
    pub child_marker: bool,
    /// The subcommand is `serve`, `acp` or `mcp`.
    pub exempt_face: bool,
}

impl Context {
    /// Reads the markers from `env`.
    pub fn from_env(env: &dyn Fn(&str) -> Option<String>, exempt_face: bool) -> Self {
        let set = |k: &str| env(k).is_some_and(|v| !v.trim().is_empty());
        Context {
            session_marker: set(child_env::UNDER_SESSION),
            child_marker: [
                child_env::DEPTH,
                child_env::AGENT_ID,
                child_env::DELEGATION_LOG,
                child_env::DELEGATION_NODE,
                child_env::DELEGATION_REPORT,
            ]
            .iter()
            .any(|k| set(k)),
            exempt_face,
        }
    }

    fn applies(&self) -> bool {
        self.session_marker && !self.child_marker && !self.exempt_face
    }
}

/// The policy lists a process was given by flag or environment.
#[derive(Debug, Default, Clone)]
pub struct Inputs {
    pub allow: Vec<String>,
    /// `--trust`, else `TOLE_TRUST` (the effective input, flag first).
    pub trust: Vec<String>,
    pub on_pretool: Vec<String>,
    pub on_posttool: Vec<String>,
    pub on_turnend: Vec<String>,
}

impl Inputs {
    /// From the resolved flags and the environment.
    pub fn from_flags(
        flags: &crate::config_apply::Flags,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Self {
        let trust = if flags.trust.is_empty() {
            env("TOLE_TRUST")
                .map(|v| crate::config_apply::split_env_list(&v))
                .unwrap_or_default()
        } else {
            flags.trust.clone()
        };
        Inputs {
            allow: flags.allow.clone(),
            trust,
            on_pretool: flags.on_pretool.clone(),
            on_posttool: flags.on_posttool.clone(),
            on_turnend: flags.on_turnend.clone(),
        }
    }
}

fn is_deny(p: &str) -> bool {
    p.starts_with(tole_core::approval::DENY_PREFIX)
}

fn shown(v: &[String]) -> String {
    format!("[{}]", v.join(", "))
}

const WAY_OUT: &str = "an agent session may only narrow the owner's policy; change the user \
                       config yourself, or run this from a terminal (issue #865)";

/// Decides one run. `user` is the USER config (not the layered one), `None`
/// when there is none. `Ok(())`: nothing to refuse. A refusal names the key,
/// the user-config value and the way out.
pub fn check(user: Option<&Config>, input: &Inputs, ctx: &Context) -> Result<(), String> {
    let Some(user) = user.filter(|_| ctx.applies()) else {
        return Ok(());
    };
    // allow: non-deny entries a subset, every user deny kept.
    if let Some(u) = &user.allow {
        let extra: Vec<&String> = input
            .allow
            .iter()
            .filter(|p| !is_deny(p) && !u.contains(p))
            .collect();
        if !extra.is_empty() {
            return Err(format!(
                "--allow {extra:?} is not in the user config's allow {}: {WAY_OUT}",
                shown(u)
            ));
        }
        if !input.allow.is_empty() {
            let dropped: Vec<&String> = u
                .iter()
                .filter(|p| is_deny(p) && !input.allow.contains(p))
                .collect();
            if !dropped.is_empty() {
                return Err(format!(
                    "--allow replaces the user config's allow {} and drops its deny \
                     pattern(s) {dropped:?}: {WAY_OUT}",
                    shown(u)
                ));
            }
        }
    }
    // trust: presets a subset.
    if let Some(u) = &user.trust {
        let extra: Vec<&String> = input.trust.iter().filter(|p| !u.contains(p)).collect();
        if !extra.is_empty() {
            return Err(format!(
                "--trust / TOLE_TRUST {extra:?} is not in the user config's trust {}: {WAY_OUT}",
                shown(u)
            ));
        }
    }
    // hooks: a superset of the user's list.
    for (key, flag, user_list) in [
        ("on_pretool", &input.on_pretool, &user.on_pretool),
        ("on_posttool", &input.on_posttool, &user.on_posttool),
        ("on_turnend", &input.on_turnend, &user.on_turnend),
    ] {
        let (Some(u), false) = (user_list, flag.is_empty()) else {
            continue;
        };
        let dropped: Vec<&String> = u.iter().filter(|h| !flag.contains(h)).collect();
        if !dropped.is_empty() {
            return Err(format!(
                "--{} replaces the user config's {key} {} and drops {dropped:?}: {WAY_OUT}",
                key.replace('_', "-"),
                shown(u)
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Context {
        Context {
            session_marker: true,
            child_marker: false,
            exempt_face: false,
        }
    }

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    fn user(f: impl FnOnce(&mut Config)) -> Config {
        let mut c = Config::default();
        f(&mut c);
        c
    }

    #[test]
    fn it_applies_only_to_a_session_marked_process_without_child_markers_off_the_faces() {
        let wide = Inputs {
            allow: v(&["*"]),
            ..Inputs::default()
        };
        let u = user(|c| c.allow = Some(v(&["read_*"])));
        assert!(check(Some(&u), &wide, &session()).is_err());
        for ctx in [
            // An attended owner: no marker.
            Context {
                session_marker: false,
                ..session()
            },
            // A child tole itself started.
            Context {
                child_marker: true,
                ..session()
            },
            // serve, acp, mcp.
            Context {
                exempt_face: true,
                ..session()
            },
        ] {
            assert_eq!(check(Some(&u), &wide, &ctx), Ok(()), "{ctx:?}");
        }
        // No user config at all: nothing to narrow.
        assert_eq!(check(None, &wide, &session()), Ok(()));
    }

    #[test]
    fn markers_are_read_from_the_environment() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        let c = Context::from_env(&env(&[("TOLE_UNDER_SESSION", "1")]), false);
        assert!(c.session_marker && !c.child_marker);
        for k in [
            "TOLE_AGENT_DEPTH",
            "TOLE_AGENT_ID",
            "TOLE_DELEGATION_LOG",
            "TOLE_DELEGATION_NODE",
            "TOLE_DELEGATION_REPORT",
        ] {
            let pairs: &'static [(&str, &str)] =
                Box::leak(vec![("TOLE_UNDER_SESSION", "1"), (k, "x")].into_boxed_slice());
            assert!(Context::from_env(&env(pairs), false).child_marker, "{k}");
        }
        // A blank marker is no marker.
        let c = Context::from_env(&env(&[("TOLE_UNDER_SESSION", " ")]), false);
        assert!(!c.session_marker);
    }

    #[test]
    fn allow_must_be_an_exact_subset_and_keep_the_users_denies() {
        let u = user(|c| c.allow = Some(v(&["read_*", "write_*", "!write_secret"])));
        let ok = |allow: &[&str]| {
            check(
                Some(&u),
                &Inputs {
                    allow: v(allow),
                    ..Inputs::default()
                },
                &session(),
            )
        };
        // Equal, narrower (keeping the deny), adding a deny of its own, nothing given.
        assert_eq!(ok(&["read_*", "write_*", "!write_secret"]), Ok(()));
        assert_eq!(ok(&["read_*", "!write_secret"]), Ok(()));
        assert_eq!(ok(&["read_*", "!write_secret", "!read_env"]), Ok(()));
        assert_eq!(ok(&[]), Ok(()));
        // A wider entry, a narrower-looking glob that is not listed, dropping a deny.
        let e = ok(&["read_*", "edit_*", "!write_secret"]).unwrap_err();
        assert!(
            e.contains("edit_*") && e.contains("read_*, write_*") && e.contains("#865"),
            "{e}"
        );
        assert!(ok(&["read_a", "!write_secret"]).is_err());
        let e = ok(&["read_*"]).unwrap_err();
        assert!(
            e.contains("drops its deny") && e.contains("!write_secret"),
            "{e}"
        );
        // An empty user list: any entry is wider.
        let u = user(|c| c.allow = Some(vec![]));
        assert!(check(
            Some(&u),
            &Inputs {
                allow: v(&["read_*"]),
                ..Inputs::default()
            },
            &session()
        )
        .is_err());
    }

    #[test]
    fn trust_presets_must_be_a_subset_flag_first_then_env() {
        let u = user(|c| c.trust = Some(v(&["read_only"])));
        let chk = |t: &[&str]| {
            check(
                Some(&u),
                &Inputs {
                    trust: v(t),
                    ..Inputs::default()
                },
                &session(),
            )
        };
        assert_eq!(chk(&["read_only"]), Ok(()));
        assert_eq!(chk(&[]), Ok(()));
        let e = chk(&["workspace"]).unwrap_err();
        assert!(e.contains("workspace") && e.contains("[read_only]"), "{e}");
        // The effective input: the flag when given, else the environment.
        let flags = crate::config_apply::Flags {
            trust: v(&["read_only"]),
            ..Default::default()
        };
        let env = |k: &str| (k == "TOLE_TRUST").then(|| "workspace".to_string());
        assert_eq!(Inputs::from_flags(&flags, &env).trust, v(&["read_only"]));
        let none = crate::config_apply::Flags::default();
        assert_eq!(Inputs::from_flags(&none, &env).trust, v(&["workspace"]));
    }

    #[test]
    fn hook_lists_must_keep_every_user_hook() {
        let u = user(|c| {
            c.on_pretool = Some(v(&["a.sh", "b.sh"]));
            c.on_turnend = Some(v(&["gate.sh"]));
        });
        let chk = |i: Inputs| check(Some(&u), &i, &session());
        // A superset is fine, and so is not giving the flag at all.
        assert_eq!(
            chk(Inputs {
                on_pretool: v(&["a.sh", "b.sh", "c.sh"]),
                ..Inputs::default()
            }),
            Ok(())
        );
        assert_eq!(chk(Inputs::default()), Ok(()));
        // Fewer hooks, or a replacement.
        let e = chk(Inputs {
            on_pretool: v(&["a.sh"]),
            ..Inputs::default()
        })
        .unwrap_err();
        assert!(
            e.contains("--on-pretool") && e.contains("b.sh") && e.contains("[a.sh, b.sh]"),
            "{e}"
        );
        let e = chk(Inputs {
            on_turnend: v(&["other.sh"]),
            ..Inputs::default()
        })
        .unwrap_err();
        assert!(e.contains("--on-turnend") && e.contains("gate.sh"), "{e}");
        // A key the user config does not set is not constrained.
        assert_eq!(
            chk(Inputs {
                on_posttool: v(&["x.sh"]),
                ..Inputs::default()
            }),
            Ok(())
        );
    }

    /// `plan_mode` has no off switch on the command line or in the environment:
    /// whatever the flags say, a user `plan_mode = true` stays on, so the guard
    /// has nothing to refuse for it.
    #[test]
    fn plan_mode_cannot_be_turned_off_by_a_flag() {
        let cfg =
            crate::config::parse("plan_mode = true\n", std::path::Path::new("u.toml")).unwrap();
        for given in [false, true] {
            let s = crate::config_apply::Settings::resolve(
                &crate::config_apply::Flags {
                    plan_mode: given,
                    ..Default::default()
                },
                &cfg,
                &|_| None,
            );
            assert!(s.plan_mode.value, "flag {given}");
        }
    }
}
