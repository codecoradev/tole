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
//! WHO IT APPLIES TO: a process that carries `TOLE_UNDER_SESSION` and is not
//! `serve`, `acp` or `mcp`. `TOLE_UNDER_SESSION` is set by `run_command` and
//! `job_start` on everything THEY start (`session_guard::mark`, their only
//! callers) and by nothing else, so a tole that is a script's descendant has it,
//! while a child tole itself starts (`agent_start`, a delegated child) does not
//! carry it of its own: its allow list is enforced in the parent (`list_covers`
//! against the parent's own scope, which may legitimately be wider than the user
//! config after an owner's `--allow`), so comparing it with the user config would
//! refuse a legitimate child, and it is exempt for lacking the marker. BUT a
//! child inherits the marker when its PARENT is itself marked (`agent_start` and
//! delegation scrub secrets only, so a marked process's children carry
//! `TOLE_UNDER_SESSION`): a child in a chain that already contains a
//! script-started tole is guarded too, and if its flags are glob-covered by the
//! parent but not exact entries of the user config (a script runs
//! `tole run --allow 'read_*'` and its model calls `agent_start` with
//! `allow=["read_file"]`) it is refused. That fails safe and only happens in
//! chains that already contain a marked process. The child
//! variables (`TOLE_AGENT_DEPTH` and the like) are NOT an exemption: they are
//! inherited by every process a child's `run_command` starts, so a script
//! running at depth 1 carries both and is guarded. The unattended faces are
//! started by the owner with their own flags and are exempt. An owner at a
//! terminal has no marker and is unchanged.
//!
//! WITHOUT THE MARKER (#867): the marker is an environment variable a script
//! can strip, so the guard also applies, marker or not, when the process has no
//! terminal (stdin AND stderr, the `--no-sandbox` rule) AND the user config sets
//! `[sandbox]` or `[secrets]` (something worth protecting; a user config that sets
//! neither leaves owner scripts and CI exactly as before). A tole that tole
//! itself started (it carries the child variables, `TOLE_AGENT_DEPTH` and the
//! like, and has no terminal either) is exempt on this path, for the reason
//! above: its scope is enforced in the parent. An owner at a terminal is
//! unchanged. Same limit: a deliberate pty, a forged child variable or a cleared
//! environment gets past it; the sandbox is the boundary.
//!
//! WHAT IT CHECKS: flags and environment only. The project config and `--config`
//! are not inputs here: they pass the owner's trust gate (`tole config trust`).
//! NOT COVERED: `--profile` (a profile's `allow` replaces the allow list when no
//! `--allow` flag is given, and a user config that sets a key only under a
//! profile is unconstrained here), `--yes` (auto-approve), the MCP server flags
//! and `--workspace`. Profiles live in the user config and are an owner choice;
//! the rest are separate surfaces.
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
    /// `TOLE_UNDER_SESSION` is set (non-blank): the process descends from a
    /// `run_command` or `job_start`, directly or through a marked parent's
    /// `agent_start` or delegated child (which inherit it).
    pub session_marker: bool,
    /// The subcommand is `serve`, `acp` or `mcp`.
    pub exempt_face: bool,
    /// stdin and stderr are both terminals.
    pub tty: bool,
    /// A child variable of a tole that tole itself started is present
    /// (`TOLE_AGENT_DEPTH`, `TOLE_AGENT_ID`, the delegation variables). Only
    /// used on the no-terminal path; it never exempts a marked process.
    pub child_variables: bool,
}

impl Context {
    /// Reads the markers from `env`.
    pub fn from_env(env: &dyn Fn(&str) -> Option<String>, exempt_face: bool, tty: bool) -> Self {
        let set = |k: &str| env(k).is_some_and(|v| !v.trim().is_empty());
        Context {
            session_marker: set(child_env::UNDER_SESSION),
            exempt_face,
            tty,
            child_variables: [
                child_env::DEPTH,
                child_env::AGENT_ID,
                child_env::DELEGATION_LOG,
                child_env::DELEGATION_NODE,
                child_env::DELEGATION_REPORT,
            ]
            .iter()
            .any(|k| set(k)),
        }
    }

    /// Whether the guard applies: a marked process (not on the faces), or an
    /// unmarked one with no terminal that is not a child tole itself started,
    /// when `protected` (the user config sets something worth protecting).
    fn applies(&self, protected: bool) -> bool {
        !self.exempt_face
            && (self.session_marker || (!self.tty && !self.child_variables && protected))
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
    let protected = user.is_some_and(|u| crate::no_config_guard::protected_keys(u).is_some());
    let Some(user) = user.filter(|_| ctx.applies(protected)) else {
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
            exempt_face: false,
            tty: false,
            child_variables: false,
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
    fn it_applies_only_to_a_session_marked_process_off_the_faces() {
        let wide = Inputs {
            allow: v(&["*"]),
            ..Inputs::default()
        };
        let u = user(|c| c.allow = Some(v(&["read_*"])));
        assert!(check(Some(&u), &wide, &session()).is_err());
        for ctx in [
            // An attended owner, or a child tole itself started (neither has
            // the marker, which only run_command and job_start set).
            Context {
                session_marker: false,
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
    fn the_session_marker_is_read_from_the_environment_and_child_variables_do_not_exempt() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert!(
            Context::from_env(&env(&[("TOLE_UNDER_SESSION", "1")]), false, false).session_marker
        );
        // A blank marker is no marker; child variables alone are not the marker.
        assert!(
            !Context::from_env(&env(&[("TOLE_UNDER_SESSION", " ")]), false, false).session_marker
        );
        assert!(
            !Context::from_env(&env(&[("TOLE_AGENT_DEPTH", "1")]), false, false).session_marker
        );
        // A script a child agent's run_command started carries BOTH, and is
        // guarded: the child variables it inherited do not exempt it.
        let both = Context::from_env(
            &env(&[("TOLE_UNDER_SESSION", "1"), ("TOLE_AGENT_DEPTH", "1")]),
            false,
            false,
        );
        assert!(both.child_variables, "child variables are noted");
        let u = user(|c| c.allow = Some(v(&["read_*"])));
        let wide = Inputs {
            allow: v(&["*"]),
            ..Inputs::default()
        };
        assert!(check(Some(&u), &wide, &both).is_err());
    }

    /// #867: without the marker the guard also applies when there is no
    /// terminal and the user config sets `[sandbox]` or `[secrets]`; a tole
    /// that tole itself started (child variables), an owner at a terminal, the
    /// faces, and a user config that protects nothing are unchanged.
    #[test]
    fn without_the_marker_it_applies_to_a_no_terminal_run_under_a_protected_user_config() {
        let protected = user(|c| {
            c.allow = Some(v(&["read_*"]));
            c.sandbox = Some(Default::default());
        });
        let plain = user(|c| c.allow = Some(v(&["read_*"])));
        let wide = Inputs {
            allow: v(&["*"]),
            ..Inputs::default()
        };
        let script = Context {
            session_marker: false,
            exempt_face: false,
            tty: false,
            child_variables: false,
        };
        // No marker, no terminal, protected: refused.
        let e = check(Some(&protected), &wide, &script).unwrap_err();
        assert!(e.contains("--allow") && e.contains("#865"), "{e}");
        // A user config that protects nothing: unchanged.
        assert_eq!(check(Some(&plain), &wide, &script), Ok(()));
        // At a terminal, a child tole itself started, the faces: unchanged.
        for ctx in [
            Context {
                tty: true,
                ..script
            },
            Context {
                child_variables: true,
                ..script
            },
            Context {
                exempt_face: true,
                ..script
            },
        ] {
            assert_eq!(check(Some(&protected), &wide, &ctx), Ok(()), "{ctx:?}");
        }
        // Child variables never exempt a MARKED process.
        let marked = Context {
            session_marker: true,
            child_variables: true,
            ..script
        };
        assert!(check(Some(&protected), &wide, &marked).is_err());
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
