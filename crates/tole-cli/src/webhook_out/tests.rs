use super::*;
use crate::config::WebhookOutConfig;

const SECRET: &str = "canary-secret-0123456789abcdef";

fn env(k: &str) -> Option<String> {
    (k == "GW_SECRET").then(|| SECRET.to_string())
}

fn cfg() -> WebhookOutConfig {
    WebhookOutConfig {
        url: Some("https://hooks.example.com/tole".into()),
        hmac_key_env: Some("GW_SECRET".into()),
        ..WebhookOutConfig::default()
    }
}

fn one(c: WebhookOutConfig) -> BTreeMap<String, WebhookOutConfig> {
    BTreeMap::from([("gateway".to_string(), c)])
}

fn err_of(c: WebhookOutConfig) -> String {
    load(&one(c), &env).unwrap_err()
}

/// A key file that removes its directory when dropped (the suites must
/// leave no `tole-*` entry behind, #781).
struct KeyFile(std::path::PathBuf);

impl std::ops::Deref for KeyFile {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for KeyFile {
    fn drop(&mut self) {
        if let Some(dir) = self.0.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn tmp_file(name: &str, bytes: &[u8]) -> KeyFile {
    let dir =
        std::env::temp_dir().join(format!("tole-webhook-out-{}-{}", std::process::id(), name));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("secret");
    std::fs::write(&p, bytes).unwrap();
    KeyFile(p)
}

#[test]
fn a_minimal_destination_loads_with_the_defaults() {
    let d = load(&one(cfg()), &env).unwrap();
    let g = d.get("gateway").unwrap();
    assert_eq!(g.url, "https://hooks.example.com/tole");
    // Approval events are opt-in: only the cron result by default.
    assert_eq!(g.events, BTreeSet::from([Event::CronRunFinished]));
    assert_eq!(g.policy, AddrPolicy::new(false, false));
    assert!(!g.allow_agent_jobs);
    assert_eq!(g.timeout, Duration::from_secs(10));
    assert_eq!(d.names().collect::<Vec<_>>(), ["gateway"]);
    assert!(load(&BTreeMap::new(), &env).unwrap().is_empty());
}

#[test]
fn every_option_is_read() {
    let c = WebhookOutConfig {
        url: Some("http://127.0.0.1:8787/tole/events".into()),
        events: Some(vec![
            "approval_requested".into(),
            "approval_resolved".into(),
            "cron_run_finished".into(),
        ]),
        allow_loopback: Some(true),
        allow_private: Some(true),
        allow_agent_jobs: Some(true),
        timeout_secs: Some(30),
        ..cfg()
    };
    let d = load(&one(c), &env).unwrap();
    let g = d.get("gateway").unwrap();
    assert_eq!(g.events.len(), 3);
    assert_eq!(g.policy, AddrPolicy::new(true, true));
    assert!(g.allow_agent_jobs);
    assert_eq!(g.timeout, Duration::from_secs(30));
}

#[test]
fn names_are_checked() {
    assert!(valid_name("gateway") && valid_name("a-b_c9") && valid_name(&"a".repeat(32)));
    for bad in ["", "Gateway", "a b", "a/b", "a.b", &"a".repeat(33), "é"] {
        assert!(!valid_name(bad), "{bad:?}");
    }
    let bad = BTreeMap::from([("Bad Name".to_string(), cfg())]);
    let e = load(&bad, &env).unwrap_err();
    assert!(
        e.contains("webhook destination `Bad Name`") && e.contains("[a-z0-9_-]"),
        "{e}"
    );
}

#[test]
fn a_url_is_required_and_checked() {
    assert!(err_of(WebhookOutConfig { url: None, ..cfg() }).contains("no `url`"));
    assert!(err_of(WebhookOutConfig {
        url: Some("  ".into()),
        ..cfg()
    })
    .contains("no `url`"));
    for bad in [
        "not a url",
        "ftp://example.com/",
        "https://user:pass@example.com/",
        "https://user@example.com/",
    ] {
        let e = err_of(WebhookOutConfig {
            url: Some(bad.into()),
            ..cfg()
        });
        assert!(e.contains("`url` is refused"), "{bad}: {e}");
        // The URL (it may carry a token) is not echoed.
        assert!(!e.contains("pass"), "{e}");
    }
}

#[test]
fn http_is_only_for_a_loopback_host_with_the_flag() {
    for url in [
        "http://127.0.0.1:8787/e",
        "http://localhost/e",
        "http://[::1]:9/e",
    ] {
        let on = WebhookOutConfig {
            url: Some(url.into()),
            allow_loopback: Some(true),
            ..cfg()
        };
        assert!(load(&one(on), &env).is_ok(), "{url}");
        let off = WebhookOutConfig {
            url: Some(url.into()),
            ..cfg()
        };
        let e = err_of(off);
        assert!(e.contains("allow_loopback = true"), "{url}: {e}");
    }
    // Not a loopback host: refused whatever the flags say.
    for url in [
        "http://example.com/e",
        "http://10.0.0.5/e",
        "http://localhost.evil.com/e",
    ] {
        let c = WebhookOutConfig {
            url: Some(url.into()),
            allow_loopback: Some(true),
            allow_private: Some(true),
            ..cfg()
        };
        assert!(err_of(c).contains("use https"), "{url}");
    }
}

#[test]
fn exactly_one_secret_source_is_required() {
    let both = WebhookOutConfig {
        hmac_key_file: Some("/x".into()),
        ..cfg()
    };
    assert!(err_of(both).contains("not both"));
    let none = WebhookOutConfig {
        hmac_key_env: None,
        ..cfg()
    };
    let e = err_of(none);
    assert!(
        e.contains("no secret") && e.contains("never written in the config"),
        "{e}"
    );
    let unset = WebhookOutConfig {
        hmac_key_env: Some("NOT_SET".into()),
        ..cfg()
    };
    assert!(err_of(unset).contains("`NOT_SET`, which is not set"));
}

#[test]
fn the_secret_must_be_long_enough_and_a_plain_file() {
    let short = |_: &str| Some("short".to_string());
    let e = load(&one(cfg()), &short).unwrap_err();
    assert!(
        e.contains("shorter than 16 bytes") && !e.contains("short\n"),
        "{e}"
    );

    let good = tmp_file("good", format!("{SECRET}\n").as_bytes());
    let c = WebhookOutConfig {
        hmac_key_env: None,
        hmac_key_file: Some(good.to_str().unwrap().into()),
        ..cfg()
    };
    assert!(load(&one(c), &env).is_ok());

    let small = tmp_file("small", b"tiny\n");
    let c = WebhookOutConfig {
        hmac_key_env: None,
        hmac_key_file: Some(small.to_str().unwrap().into()),
        ..cfg()
    };
    assert!(err_of(c).contains("shorter than 16 bytes"));

    let missing = WebhookOutConfig {
        hmac_key_env: None,
        hmac_key_file: Some("/nonexistent/tole-secret".into()),
        ..cfg()
    };
    assert!(err_of(missing).contains("does not exist"));

    let big = tmp_file("big", &vec![b'a'; 5000]);
    let c = WebhookOutConfig {
        hmac_key_env: None,
        hmac_key_file: Some(big.to_str().unwrap().into()),
        ..cfg()
    };
    assert!(load(&one(c), &env).is_err(), "a file over 4 KiB is refused");
}

#[cfg(unix)]
#[test]
fn a_symlinked_hmac_key_file_is_refused() {
    let real = tmp_file("link-real", SECRET.as_bytes());
    let link = real.with_file_name("link");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&*real, &link).unwrap();
    let c = WebhookOutConfig {
        hmac_key_env: None,
        hmac_key_file: Some(link.to_str().unwrap().into()),
        ..cfg()
    };
    assert!(load(&one(c), &env).is_err());
}

#[test]
fn events_and_timeout_are_checked() {
    let e = err_of(WebhookOutConfig {
        events: Some(vec!["cron_run_finished".into(), "approval_exploded".into()]),
        ..cfg()
    });
    assert!(
        e.contains("unknown event `approval_exploded`") && e.contains("known:"),
        "{e}"
    );
    // Hostile text does not shape the message.
    let e = err_of(WebhookOutConfig {
        events: Some(vec!["a\nb\u{1b}[31m".into()]),
        ..cfg()
    });
    assert!(!e.contains('\n') && !e.contains('\u{1b}'), "{e:?}");
    for bad in [0u64, 31, 3600] {
        let e = err_of(WebhookOutConfig {
            timeout_secs: Some(bad),
            ..cfg()
        });
        assert!(e.contains("timeout_secs must be 1-30"), "{bad}: {e}");
    }
    for ok in [1u64, 30] {
        assert!(load(
            &one(WebhookOutConfig {
                timeout_secs: Some(ok),
                ..cfg()
            }),
            &env
        )
        .is_ok());
    }
    assert_eq!(
        Event::parse("cron_run_finished"),
        Some(Event::CronRunFinished)
    );
    assert_eq!(
        Event::parse("webhook_test"),
        None,
        "reserved, not accepted yet"
    );
}

#[test]
fn the_secret_is_never_in_an_error_a_debug_or_a_doctor_line() {
    // Every failure path, with the canary in the environment.
    let cases = [
        WebhookOutConfig {
            url: Some("ftp://x/".into()),
            ..cfg()
        },
        WebhookOutConfig {
            events: Some(vec!["nope".into()]),
            ..cfg()
        },
        WebhookOutConfig {
            timeout_secs: Some(0),
            ..cfg()
        },
        WebhookOutConfig {
            hmac_key_file: Some("/x".into()),
            ..cfg()
        },
    ];
    for c in cases {
        let e = err_of(c);
        assert!(!e.contains(SECRET), "{e}");
    }
    let d = load(&one(cfg()), &env).unwrap();
    let dbg = format!("{d:?}");
    assert!(
        !dbg.contains(SECRET) && !dbg.contains("hooks.example.com"),
        "{dbg}"
    );

    let full = crate::config::Config {
        webhook_out: Some(one(cfg())),
        ..Default::default()
    };
    let lines = doctor_lines_with(&full, &env).join("\n");
    assert!(
        lines.contains("gateway") && lines.contains("cron_run_finished"),
        "{lines}"
    );
    assert!(
        !lines.contains(SECRET) && !lines.contains("hooks.example.com"),
        "{lines}"
    );
}

#[test]
fn the_secret_is_held_by_role_so_it_is_redacted() {
    let c = WebhookOutConfig {
        hmac_key_env: Some("GW_SECRET2".into()),
        ..cfg()
    };
    let unique = "role-held-secret-value-77777777";
    let envf = |k: &str| (k == "GW_SECRET2").then(|| unique.to_string());
    load(&one(c), &envf).unwrap();
    assert!(
        tole_core::secrets::role_secrets()
            .iter()
            .any(|(name, v)| name == "webhook_out.gateway" && v == unique),
        "the secret was not registered by role"
    );
}

#[test]
fn doctor_lines_say_what_loaded_and_what_did_not() {
    assert!(doctor_lines_with(&crate::config::Config::default(), &env).is_empty());
    let mut c = crate::config::Config {
        webhook_out: Some(one(WebhookOutConfig {
            allow_agent_jobs: Some(true),
            allow_loopback: Some(true),
            ..cfg()
        })),
        ..Default::default()
    };
    let l = doctor_lines_with(&c, &env).join("\n");
    assert!(
        l.starts_with("webhook destinations: gateway (cron_run_finished; loopback, agent jobs)"),
        "{l}"
    );
    assert!(l.contains("next start"), "{l}");
    c.webhook_out = Some(one(WebhookOutConfig {
        url: Some("ftp://x/".into()),
        ..cfg()
    }));
    let l = doctor_lines_with(&c, &env).join("\n");
    assert!(l.contains("NOT LOADED") && l.contains("`gateway`"), "{l}");
}

#[test]
fn a_relative_key_file_is_refused() {
    let c = WebhookOutConfig {
        hmac_key_env: None,
        hmac_key_file: Some("wh.key".into()),
        ..cfg()
    };
    assert!(err_of(c).contains("absolute"));
}

#[test]
fn a_literal_private_ip_is_refused_at_load_unless_allowed() {
    for url in ["https://169.254.169.254/x", "https://10.0.0.1/x"] {
        let c = WebhookOutConfig {
            url: Some(url.into()),
            ..cfg()
        };
        assert!(err_of(c).contains("refused"), "{url}");
    }
    let ok = WebhookOutConfig {
        url: Some("https://10.0.0.1/x".into()),
        allow_private: Some(true),
        ..cfg()
    };
    assert!(load(&one(ok), &env).is_ok());
}

/// A token in a destination URL is held on its own, so a job that prints it
/// bare cannot put it on the wire; short words of the URL are not held.
#[test]
fn url_secret_parts_are_the_long_query_values_and_path_segments() {
    use super::url_secret_parts as parts;
    assert_eq!(
        parts("https://h.example/hook?token=abcdef0123456789&x=1"),
        vec!["abcdef0123456789".to_string()]
    );
    assert_eq!(
        parts("https://h.example/services/T0123456789AB/B0123456789CD#frag"),
        vec!["T0123456789AB".to_string(), "B0123456789CD".to_string()]
    );
    assert!(parts("https://h.example/hook?a=short").is_empty());
    assert!(parts("https://h.example").is_empty());
    assert!(parts("not a url").is_empty());
}
