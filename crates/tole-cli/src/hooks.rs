//! Webhook routes of `tole serve` (#582, design `docs/automation-design.md`
//! §6): loading `[hooks.<route>]` from the user config into checked routes.
//!
//! A route needs a key (from an environment variable or a file) or the
//! explicit `insecure_loopback`, which only loads when the server binds a
//! loopback address. A route that cannot load stops `tole serve` with the
//! route's name and the fix. The key is never formatted: not in errors,
//! `Debug`, responses, logs or the ledger.

use std::collections::BTreeMap;

use crate::config::HookConfig;

/// Default deliveries per minute.
pub const DEFAULT_RATE: u32 = 30;
/// Most deliveries per minute a route may allow.
pub const MAX_RATE: u32 = 600;
/// Default body cap.
pub const DEFAULT_BODY: u64 = 256 * 1024;
/// Largest body cap (the server's request ceiling).
pub const MAX_BODY: u64 = 1024 * 1024;
/// Shortest key accepted.
pub const MIN_KEY: usize = 16;
/// Largest key file.
pub const MAX_KEY_FILE: u64 = 4096;

/// A loaded route.
pub struct Route {
    /// `None`: an insecure loopback-only route.
    key: Option<Vec<u8>>,
    pub rate_per_minute: u32,
    pub max_body: usize,
    /// The job a matching delivery fires (#582 part 2).
    pub cron_job: Option<String>,
    /// Admitted events; `None`: all.
    pub events: Option<Vec<String>>,
    pub filter: Option<crate::hook_filter::Filter>,
    pub template: Option<crate::hook_filter::Template>,
    /// The fired run may use the job's Write tools (else ReadOnly).
    pub allow_writes: bool,
}

impl Route {
    /// The HMAC key, `None` for an insecure loopback route.
    pub fn key(&self) -> Option<&[u8]> {
        self.key.as_deref()
    }
}

impl std::fmt::Debug for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Route")
            .field("signed", &self.key.is_some())
            .field("rate_per_minute", &self.rate_per_minute)
            .field("max_body", &self.max_body)
            .finish()
    }
}

/// `[a-z0-9_-]`, 1 to 64.
pub fn valid_route(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
}

/// Loads every route. `loopback_bind`: the server binds a loopback address.
/// `env` reads an environment variable.
pub fn load(
    cfg: &BTreeMap<String, HookConfig>,
    loopback_bind: bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<BTreeMap<String, Route>, String> {
    let mut out = BTreeMap::new();
    for (name, h) in cfg {
        let err = |why: String| format!("hook route `{name}`: {why}");
        if !valid_route(name) {
            return Err(err("the name must be 1-64 of [a-z0-9_-]".into()));
        }
        let insecure = h.insecure_loopback == Some(true);
        let key = match (&h.hmac_key_env, &h.hmac_key_file, insecure) {
            (Some(_), Some(_), _) => {
                return Err(err(
                    "set one of hmac_key_env and hmac_key_file, not both".into()
                ))
            }
            (Some(_), _, true) | (_, Some(_), true) => {
                return Err(err("insecure_loopback cannot be combined with a key".into()))
            }
            (Some(var), None, false) => Some(
                env(var)
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| err(format!("hmac_key_env names `{var}`, which is not set")))?
                    .into_bytes(),
            ),
            (None, Some(path), false) => Some(read_key_file(path).map_err(err)?),
            (None, None, true) if loopback_bind => None,
            (None, None, true) => {
                return Err(err(
                    "insecure_loopback needs a loopback --bind (127.0.0.1 or ::1); \
                     give the route a key instead"
                        .into(),
                ))
            }
            (None, None, false) => {
                return Err(err(
                    "no key: set hmac_key_env or hmac_key_file (insecure_loopback = true \
                     is accepted on a loopback bind only)"
                        .into(),
                ))
            }
        };
        if key.as_ref().is_some_and(|k| k.len() < MIN_KEY) {
            return Err(err(format!("the key is shorter than {MIN_KEY} bytes")));
        }
        let rate = h.rate_per_minute.unwrap_or(DEFAULT_RATE);
        if !(1..=MAX_RATE).contains(&rate) {
            return Err(err(format!("rate_per_minute must be 1-{MAX_RATE}")));
        }
        let body = h.max_body_bytes.unwrap_or(DEFAULT_BODY);
        if !(1..=MAX_BODY).contains(&body) {
            return Err(err(format!("max_body_bytes must be 1-{MAX_BODY}")));
        }
        // The action (#582 part 2), compiled now: a bad filter or template
        // stops the server like a missing key.
        let job_ok = |j: &String| {
            (1..=64).contains(&j.len())
                && j.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        };
        if h.cron_job.as_ref().is_some_and(|j| !job_ok(j)) {
            return Err(err(
                "cron_job must be a job id (1-64 of [A-Za-z0-9._-])".into()
            ));
        }
        let event_ok = |e: &String| {
            (1..=64).contains(&e.len())
                && e.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        };
        if h.events.iter().flatten().any(|e| !event_ok(e)) {
            return Err(err("each event must be 1-64 of [A-Za-z0-9._-]".into()));
        }
        let filter = h
            .filter
            .as_ref()
            .map(crate::hook_filter::compile)
            .transpose()
            .map_err(|e| err(format!("filter: {e}")))?;
        let template = h
            .template
            .as_deref()
            .map(crate::hook_filter::Template::parse)
            .transpose()
            .map_err(err)?;
        out.insert(
            name.clone(),
            Route {
                key,
                rate_per_minute: rate,
                max_body: body as usize,
                cron_job: h.cron_job.clone(),
                events: h.events.clone(),
                filter,
                template,
                allow_writes: h.allow_writes == Some(true),
            },
        );
    }
    Ok(out)
}

/// Records every route's key as a secret tole holds by role (#682): from
/// then on it is redacted wherever text leaves the process, and the child
/// scrub removes any variable holding it (`hmac_key_env` may name any
/// variable). A key that is not UTF-8 cannot appear in text and is skipped.
pub fn hold_keys(routes: &BTreeMap<String, Route>) {
    for (name, route) in routes {
        if let Some(key) = route.key().and_then(|k| std::str::from_utf8(k).ok()) {
            tole_core::secrets::add_role_secret(&format!("hooks.{name}"), key);
        }
    }
}

/// The key file: a regular file (no link, no FIFO), at most
/// [`MAX_KEY_FILE`] bytes, one trailing newline dropped.
fn read_key_file(path: &str) -> Result<Vec<u8>, String> {
    let bytes =
        tole_core::delegation_report::read_plain_file(std::path::Path::new(path), MAX_KEY_FILE)
            .map_err(|e| format!("the key file {path}: {e}"))?
            .ok_or_else(|| format!("the key file {path}: does not exist"))?;
    let end = bytes
        .iter()
        .rposition(|b| !b"\r\n".contains(b))
        .map_or(0, |i| i + 1);
    Ok(bytes[..end].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef-route-key";

    fn one(h: HookConfig) -> BTreeMap<String, HookConfig> {
        [("gh".to_string(), h)].into_iter().collect()
    }

    fn env(k: &str) -> Option<String> {
        (k == "HOOK_KEY").then(|| KEY.to_string())
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("tole-hooks-unit-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_route_loads_with_a_key_and_shows_none_of_it() {
        let r = load(
            &one(HookConfig {
                hmac_key_env: Some("HOOK_KEY".into()),
                ..HookConfig::default()
            }),
            false,
            &env,
        )
        .unwrap();
        let route = &r["gh"];
        assert_eq!(route.key(), Some(KEY.as_bytes()));
        assert_eq!((route.rate_per_minute, route.max_body), (30, 256 * 1024));
        assert!(!format!("{route:?}").contains(KEY));
    }

    #[test]
    fn routes_that_cannot_load_name_the_route_and_never_the_key() {
        let d = tmp("bad");
        let short = d.join("short");
        std::fs::write(&short, "too-short\n").unwrap();
        let big = d.join("big");
        std::fs::write(&big, "k".repeat(5_000)).unwrap();
        let link = d.join("link");
        std::fs::write(d.join("real"), KEY).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(d.join("real"), &link).unwrap();
        let file = |p: &std::path::Path| HookConfig {
            hmac_key_file: Some(p.display().to_string()),
            ..HookConfig::default()
        };
        let mut cases = vec![
            (HookConfig::default(), "no key"),
            (
                HookConfig {
                    hmac_key_env: Some("HOOK_KEY".into()),
                    hmac_key_file: Some("/x".into()),
                    ..HookConfig::default()
                },
                "not both",
            ),
            (
                HookConfig {
                    hmac_key_env: Some("UNSET_VAR".into()),
                    ..HookConfig::default()
                },
                "not set",
            ),
            (file(&short), "shorter"),
            (file(&big), "larger"),
            (file(&d.join("missing")), "does not exist"),
            (
                HookConfig {
                    insecure_loopback: Some(true),
                    ..HookConfig::default()
                },
                "loopback",
            ),
            (
                HookConfig {
                    hmac_key_env: Some("HOOK_KEY".into()),
                    rate_per_minute: Some(601),
                    ..HookConfig::default()
                },
                "rate_per_minute",
            ),
            (
                HookConfig {
                    hmac_key_env: Some("HOOK_KEY".into()),
                    max_body_bytes: Some(MAX_BODY + 1),
                    ..HookConfig::default()
                },
                "max_body_bytes",
            ),
        ];
        #[cfg(unix)]
        cases.push((file(&link), "cannot be read"));
        for (h, want) in cases {
            let e = load(&one(h), false, &env).unwrap_err();
            assert!(e.contains("`gh`") && e.contains(want), "{want}: {e}");
            assert!(!e.contains(KEY), "{e}");
        }
        let bad_name: BTreeMap<String, HookConfig> =
            [("Bad/Name".to_string(), HookConfig::default())]
                .into_iter()
                .collect();
        assert!(load(&bad_name, true, &env).unwrap_err().contains("name"));
    }

    #[test]
    fn an_insecure_route_loads_only_on_a_loopback_bind() {
        let open = one(HookConfig {
            insecure_loopback: Some(true),
            ..HookConfig::default()
        });
        assert!(load(&open, true, &env).unwrap()["gh"].key().is_none());
        assert!(load(&open, false, &env).is_err());
    }

    /// #582 part 2: the action keys are compiled with the route; a bad one
    /// stops the server like a missing key. Writes are off by default.
    #[test]
    fn the_action_keys_load_or_name_the_route() {
        let base = || HookConfig {
            hmac_key_env: Some("HOOK_KEY".into()),
            ..HookConfig::default()
        };
        let ok = load(
            &one(HookConfig {
                cron_job: Some("pr-review".into()),
                events: Some(vec!["pull_request".into()]),
                template: Some("PR {number}".into()),
                filter: Some(crate::hook_filter::FilterConfig {
                    path: Some("action".into()),
                    equals: Some("opened".into()),
                    ..Default::default()
                }),
                ..base()
            }),
            false,
            &env,
        )
        .unwrap();
        let r = &ok["gh"];
        assert!(r.filter.is_some() && r.template.is_some() && !r.allow_writes);
        for (h, want) in [
            (
                HookConfig {
                    cron_job: Some("bad job".into()),
                    ..base()
                },
                "cron_job",
            ),
            (
                HookConfig {
                    events: Some(vec!["a b".into()]),
                    ..base()
                },
                "event",
            ),
            (
                HookConfig {
                    template: Some("{".into()),
                    ..base()
                },
                "template",
            ),
            (
                HookConfig {
                    filter: Some(crate::hook_filter::FilterConfig::default()),
                    ..base()
                },
                "filter",
            ),
        ] {
            let e = load(&one(h), false, &env).unwrap_err();
            assert!(e.contains("`gh`") && e.contains(want), "{want}: {e}");
        }
    }

    #[test]
    fn a_key_file_drops_one_trailing_newline() {
        let d = tmp("file");
        let p = d.join("key");
        std::fs::write(&p, format!("{KEY}\r\n")).unwrap();
        let r = load(
            &one(HookConfig {
                hmac_key_file: Some(p.display().to_string()),
                ..HookConfig::default()
            }),
            false,
            &env,
        )
        .unwrap();
        assert_eq!(r["gh"].key(), Some(KEY.as_bytes()));
    }
}
