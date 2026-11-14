use super::*;

fn brief(prompt: &str) -> Brief<'_> {
    Brief {
        kind: TaskKind::Run,
        prompt,
        max_risk: Risk::Write,
        profile: None,
        job: None,
        budget_left: None,
    }
}

fn rule(name: &str, tier: Tier, when: When) -> Rule {
    Rule {
        name: name.into(),
        tier,
        when,
    }
}

fn tiers() -> Tiers {
    Tiers {
        light: Some("small".into()),
        standard: Some("mid".into()),
        heavy: Some("big".into()),
    }
}

#[test]
fn no_rule_keeps_the_configured_model() {
    let r = RuleRouter::new(vec![], None).route(&brief("anything"));
    assert_eq!(r.tier, None);
    assert_eq!(r.source, Source::Default);
    assert_eq!(r.model(&tiers()), Ok(None));
    // A rule that does not match is the same as none.
    let only_cron = rule(
        "cron",
        Tier::Light,
        When {
            kinds: vec![TaskKind::Cron],
            ..When::default()
        },
    );
    let r = RuleRouter::new(vec![only_cron], None).route(&brief("x"));
    assert_eq!((r.tier, r.source), (None, Source::Default));
}

#[test]
fn each_condition_must_hold() {
    let b = Brief {
        kind: TaskKind::Cron,
        prompt: "Summarize the Open PRs",
        max_risk: Risk::ReadOnly,
        profile: None,
        job: Some("digest"),
        budget_left: Some(10_000),
    };
    let all = When {
        kinds: vec![TaskKind::Cron, TaskKind::Run],
        min_chars: Some(5),
        max_chars: Some(100),
        min_risk: Some(Risk::ReadOnly),
        max_risk: Some(Risk::ReadOnly),
        profiles: vec![],
        jobs: vec!["digest".into()],
        budget_below: Some(20_000),
        keywords: vec!["open prs".into()],
    };
    assert!(rule("all", Tier::Light, all.clone()).matches(&b));
    // Break each condition alone: the rule no longer matches.
    let broken: Vec<(&str, When)> = vec![
        (
            "kind",
            When {
                kinds: vec![TaskKind::Chat],
                ..all.clone()
            },
        ),
        (
            "min_chars",
            When {
                min_chars: Some(500),
                ..all.clone()
            },
        ),
        (
            "max_chars",
            When {
                max_chars: Some(3),
                ..all.clone()
            },
        ),
        (
            "min_risk",
            When {
                min_risk: Some(Risk::Write),
                ..all.clone()
            },
        ),
        (
            "max_risk",
            When {
                max_risk: Some(Risk::ReadOnly),
                ..all.clone()
            },
        ),
        (
            "profiles",
            When {
                profiles: vec!["coder".into()],
                ..all.clone()
            },
        ),
        (
            "jobs",
            When {
                jobs: vec!["other".into()],
                ..all.clone()
            },
        ),
        (
            "budget",
            When {
                budget_below: Some(5_000),
                ..all.clone()
            },
        ),
        (
            "keywords",
            When {
                keywords: vec!["deploy".into()],
                ..all.clone()
            },
        ),
    ];
    for (what, w) in broken {
        let hold = rule(what, Tier::Light, w).matches(&b);
        // max_risk = read_only still holds for a read-only run: check it
        // against a write run instead.
        if what == "max_risk" {
            let wb = Brief {
                max_risk: Risk::Write,
                ..b
            };
            assert!(!rule(what, Tier::Light, all.clone()).matches(&wb), "{what}");
            continue;
        }
        assert!(!hold, "{what} should not hold");
    }
    // An unknown budget never satisfies budget_below.
    let nb = Brief {
        budget_left: None,
        ..b
    };
    assert!(!rule(
        "b",
        Tier::Light,
        When {
            budget_below: Some(1),
            ..When::default()
        }
    )
    .matches(&nb));
}

#[test]
fn a_rule_without_profiles_applies_only_to_runs_without_one() {
    let any = rule("plain", Tier::Light, When::default());
    let coder = rule(
        "coder",
        Tier::Heavy,
        When {
            profiles: vec!["coder".into()],
            ..When::default()
        },
    );
    let with = Brief {
        profile: Some("coder"),
        ..brief("x")
    };
    assert!(any.matches(&brief("x")) && !any.matches(&with));
    assert!(coder.matches(&with) && !coder.matches(&brief("x")));
}

#[test]
fn ties_go_to_the_cheaper_tier_and_the_first_rule_of_it() {
    let rules = vec![
        rule("big", Tier::Heavy, When::default()),
        rule("small-a", Tier::Light, When::default()),
        rule("small-b", Tier::Light, When::default()),
        rule("mid", Tier::Standard, When::default()),
    ];
    let r = RuleRouter::new(rules, None).route(&brief("x"));
    assert_eq!(r.tier, Some(Tier::Light));
    assert_eq!(r.source, Source::Rule("small-a".into()));
    assert!(r.reason.contains("small-a"), "{}", r.reason);
    assert_eq!(r.model(&tiers()), Ok(Some("small")));
}

#[test]
fn allowed_tiers_narrow_the_pick_and_never_widen_it() {
    let rules = vec![
        rule("small", Tier::Light, When::default()),
        rule("big", Tier::Heavy, When::default()),
    ];
    let r = RuleRouter::new(rules.clone(), Some(vec![Tier::Heavy])).route(&brief("x"));
    assert_eq!(r.tier, Some(Tier::Heavy));
    // Nothing allowed matches: the configured model stays.
    let r = RuleRouter::new(rules, Some(vec![Tier::Standard])).route(&brief("x"));
    assert_eq!((r.tier, r.source), (None, Source::Default));
}

#[test]
fn routing_is_deterministic() {
    let rules = vec![
        rule(
            "a",
            Tier::Standard,
            When {
                max_chars: Some(10),
                ..When::default()
            },
        ),
        rule(
            "b",
            Tier::Heavy,
            When {
                min_chars: Some(11),
                ..When::default()
            },
        ),
    ];
    let router = RuleRouter::new(rules, None);
    for p in ["short", "a much longer prompt"] {
        assert_eq!(router.route(&brief(p)), router.route(&brief(p)));
    }
    assert_eq!(router.route(&brief("short")).tier, Some(Tier::Standard));
    assert_eq!(
        router.route(&brief("a much longer prompt")).tier,
        Some(Tier::Heavy)
    );
}

#[test]
fn check_refuses_a_tier_not_declared_where_it_resolves() {
    let partial = Tiers {
        light: None,
        ..tiers()
    };
    let full = tiers();
    let plain = RuleRouter::new(vec![rule("p", Tier::Light, When::default())], None);
    let e = plain.check(|_| Some(&partial)).unwrap_err();
    assert!(
        e.contains("`p`") && e.contains("light") && e.contains("[router.tiers]"),
        "{e}"
    );
    assert!(plain.check(|_| Some(&full)).is_ok());
    // A profile rule is checked against that profile's tiers.
    let prof = RuleRouter::new(
        vec![rule(
            "c",
            Tier::Heavy,
            When {
                profiles: vec!["coder".into()],
                ..When::default()
            },
        )],
        None,
    );
    let no_heavy = Tiers {
        heavy: None,
        ..tiers()
    };
    let e = prof
        .check(|p| {
            if p == Some("coder") {
                Some(&no_heavy)
            } else {
                Some(&full)
            }
        })
        .unwrap_err();
    assert!(e.contains("profiles.coder.models.heavy"), "{e}");
    let e = prof.check(|p| p.is_none().then_some(&full)).unwrap_err();
    assert!(e.contains("unknown profile `coder`"), "{e}");
    assert!(prof.check(|_| Some(&full)).is_ok());
}

#[test]
fn a_missing_tier_is_an_error_not_another_model() {
    let r = Route {
        tier: Some(Tier::Heavy),
        source: Source::Rule("x".into()),
        reason: String::new(),
    };
    let t = Tiers {
        heavy: None,
        ..tiers()
    };
    assert!(r.model(&t).is_err());
}

#[test]
fn the_record_is_redacted_before_it_is_capped() {
    let secrets = SecretSet::default().with("MY_TOKEN", "sekret-value-123456");
    let r = Route {
        tier: Some(Tier::Light),
        source: Source::Rule("sekret-value-123456".into()),
        reason: format!("{}sekret-value-123456", "x".repeat(MAX_REASON_CHARS - 5)),
    };
    let rec = r
        .record(Some("model-sekret-value-123456"), &secrets)
        .to_string();
    assert!(!rec.contains("sekret"), "{rec}");
    // Capping first would have left `sekre`; redacting first leaves none.
    let reason = r.record(None, &secrets)["reason"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(reason.chars().count() <= MAX_REASON_CHARS, "{reason}");
    assert!(!reason.contains("sek"), "{reason}");
    assert_eq!(r.record(None, &secrets)["tier"].as_str(), Some("light"));
}

#[test]
fn names_parse_both_ways() {
    for t in Tier::ALL {
        assert_eq!(Tier::parse(t.as_str()), Some(t));
    }
    for k in TaskKind::ALL {
        assert_eq!(TaskKind::parse(k.as_str()), Some(k));
    }
    assert_eq!(Tier::parse("advisor"), None);
    assert_eq!(parse_risk("write"), Some(Risk::Write));
    assert_eq!(parse_risk("Write"), None);
}
