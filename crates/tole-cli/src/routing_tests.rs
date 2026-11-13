use super::*;
use crate::config::{layered, parse};
use std::path::Path;
use tole_core::router::{Source, TaskKind};
use tole_core::tool::Risk;

fn cfg(src: &str) -> Config {
    parse(src, Path::new("cfg.toml")).unwrap()
}

fn err(src: &str) -> String {
    parse(src, Path::new("cfg.toml")).unwrap_err().to_string()
}

const USER: &str = r#"
[router]
enabled = true
[router.tiers]
light = "small-model"
standard = "mid-model"
heavy = "big-model"
[[router.rules]]
name = "short-cron"
tier = "light"
kinds = ["cron"]
max_chars = 200
max_risk = "read_only"
[[router.rules]]
name = "design"
tier = "heavy"
keywords = ["architecture"]
[[router.rules]]
name = "coder-heavy"
tier = "heavy"
profiles = ["coder"]
[profiles.coder.models]
standard = "coder-mid"
heavy = "coder-big"
"#;

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

#[test]
fn routing_is_off_unless_the_user_config_enables_it() {
    assert!(Routing::from_config(&Config::default()).unwrap().is_none());
    let off = USER.replace("enabled = true", "");
    assert!(Routing::from_config(&cfg(&off)).unwrap().is_none());
    assert!(Routing::from_config(&cfg(USER)).unwrap().is_some());
}

#[test]
fn rules_resolve_through_the_declared_tiers() {
    let rt = Routing::from_config(&cfg(USER)).unwrap().unwrap();
    let cron = Brief {
        kind: TaskKind::Cron,
        max_risk: Risk::ReadOnly,
        ..brief("digest the inbox")
    };
    let (r, m) = rt.decide(&cron).unwrap();
    assert_eq!(r.source, Source::Rule("short-cron".into()));
    assert_eq!(m.as_deref(), Some("small-model"));
    let (_, m) = rt.decide(&brief("draft the Architecture")).unwrap();
    assert_eq!(m.as_deref(), Some("big-model"));
    // No rule: the configured model stays.
    let (r, m) = rt.decide(&brief("hello")).unwrap();
    assert_eq!((r.source, m), (Source::Default, None));
    // A profile run resolves against that profile's tiers, and the plain
    // rules do not apply to it.
    let coder = Brief {
        profile: Some("coder"),
        ..brief("draft the architecture")
    };
    let (r, m) = rt.decide(&coder).unwrap();
    assert_eq!(r.source, Source::Rule("coder-heavy".into()));
    assert_eq!(m.as_deref(), Some("coder-big"));
}

#[test]
fn an_unknown_or_undeclared_tier_is_a_load_error_with_a_remedy() {
    let e = err("[router]\n[[router.rules]]\nname = \"a\"\ntier = \"huge\"\n");
    assert!(
        e.contains("unknown tier \"huge\"") && e.contains("light, standard or heavy"),
        "{e}"
    );
    for (src, want) in [
        ("kinds = [\"batch\"]", "unknown kind"),
        ("max_risk = \"high\"", "unknown risk"),
        (
            "min_chars = 9\nmax_chars = 3",
            "min_chars is above max_chars",
        ),
        ("keywords = [\"\"]", "keywords"),
        ("profiles = [\"Bad\"]", "not a profile name"),
    ] {
        let e = err(&format!(
            "[router]\n[[router.rules]]\nname = \"a\"\ntier = \"light\"\n{src}\n"
        ));
        assert!(e.contains(want), "{src}: {e}");
    }
    let e = err("[router]\n[[router.rules]]\nname = \"Bad Name\"\ntier = \"light\"\n");
    assert!(e.contains("router rule name"), "{e}");
    let e = err(
        "[router]\n[[router.rules]]\nname = \"a\"\ntier = \"light\"\n\
         [[router.rules]]\nname = \"a\"\ntier = \"heavy\"\n",
    );
    assert!(e.contains("declared twice"), "{e}");
    let e = err("[router]\nallow_tiers = [\"advisor\"]\n");
    assert!(e.contains("router.allow_tiers"), "{e}");
    assert!(err("[router]\nmodel = \"x\"\n").contains("unknown field"));
    // Declared nowhere: refused when the router loads, naming the fix.
    let no_light = USER.replace("light = \"small-model\"\n", "");
    let e = Routing::from_config(&cfg(&no_light)).unwrap_err();
    assert!(
        e.contains("short-cron") && e.contains("[router.tiers]"),
        "{e}"
    );
    let no_coder = USER.replace("heavy = \"coder-big\"\n", "");
    let e = Routing::from_config(&cfg(&no_coder)).unwrap_err();
    assert!(e.contains("profiles.coder.models.heavy"), "{e}");
    let ghost = USER.replace("profiles = [\"coder\"]", "profiles = [\"ghost\"]");
    let e = Routing::from_config(&cfg(&ghost)).unwrap_err();
    assert!(e.contains("unknown profile `ghost`"), "{e}");
}

#[test]
fn a_project_config_can_only_turn_routing_off_or_narrow_it() {
    let user = cfg(USER);
    // A project cannot enable, add tiers or add rules.
    let pushy = cfg(
        "[router]\nenabled = true\n[router.tiers]\nlight = \"evil-model\"\n\
         [[router.rules]]\nname = \"all-light\"\ntier = \"light\"\n",
    );
    let m = layered(&pushy, &Config::default());
    assert!(
        Routing::from_config(&m).unwrap().is_none(),
        "no user router, no routing"
    );
    let m = layered(&pushy, &user);
    let rt = Routing::from_config(&m).unwrap().unwrap();
    assert_eq!(rt.tiers.light.as_deref(), Some("small-model"));
    assert!(rt.router.rules().iter().all(|r| r.name != "all-light"));
    assert!(project_warning(&pushy, "p").is_some_and(|w| w.contains("user config")));
    // It can turn routing off ...
    let off = cfg("[router]\nenabled = false\n");
    assert!(Routing::from_config(&layered(&off, &user))
        .unwrap()
        .is_none());
    assert!(project_warning(&off, "p").is_none());
    // ... or narrow the tiers rules may pick, never widen them.
    let narrow = cfg("[router]\nallow_tiers = [\"heavy\"]\n");
    let rt = Routing::from_config(&layered(&narrow, &user))
        .unwrap()
        .unwrap();
    let cron = Brief {
        kind: TaskKind::Cron,
        max_risk: Risk::ReadOnly,
        ..brief("digest")
    };
    assert_eq!(rt.decide(&cron).unwrap().1, None, "light is not allowed");
    let user_narrow = cfg(&USER.replace(
        "enabled = true",
        "enabled = true\nallow_tiers = [\"light\"]",
    ));
    let wide = cfg("[router]\nallow_tiers = [\"light\", \"heavy\"]\n");
    let m = layered(&wide, &user_narrow);
    assert_eq!(
        m.router.unwrap().allow_tiers,
        Some(vec!["light".to_string()])
    );
}

#[test]
fn doctor_shows_names_only() {
    let text = doctor_lines(&cfg(USER)).join("\n");
    assert!(
        text.contains("router: on") && text.contains("short-cron, design, coder-heavy"),
        "{text}"
    );
    assert!(
        text.contains("router tiers: light, standard, heavy"),
        "{text}"
    );
    for hidden in ["small-model", "big-model", "architecture", "coder-big"] {
        assert!(!text.contains(hidden), "{hidden}: {text}");
    }
    assert!(doctor_lines(&Config::default())[0].starts_with("router: off"));
    let broken = USER.replace("light = \"small-model\"\n", "");
    assert!(doctor_lines(&cfg(&broken))[0].starts_with("router: ERROR"));
}

#[test]
fn config_check_shows_names_only() {
    let text = crate::config::render_with(&cfg(USER), crate::config::USER_TAG, &Default::default())
        .join("\n");
    assert!(
        text.contains("router = on") && text.contains("\"short-cron\" -> \"light\""),
        "{text}"
    );
    assert!(
        !text.contains("small-model") && !text.contains("architecture"),
        "{text}"
    );
}

#[test]
fn the_dry_run_redacts_and_calls_no_provider() {
    let secrets =
        tole_core::secrets::SecretSet::default().with("MY_TOKEN", "tok-secret-0123456789");
    let leaky = USER.replace("small-model", "small-tok-secret-0123456789");
    let cron = Brief {
        kind: TaskKind::Cron,
        max_risk: Risk::ReadOnly,
        ..brief("digest")
    };
    let base = Some(("cfg-model", "env TOLE_MODEL"));
    let out = explain(&cfg(&leaky), &cron, base, &secrets).unwrap();
    assert!(
        out.contains("tier: light") && out.contains("short-cron"),
        "{out}"
    );
    assert!(!out.contains("tok-secret"), "{out}");
    // Every answer names the configured model and where it came from, and a
    // routed answer says the rule replaces it.
    let base_line = "configured model: cfg-model (from env TOLE_MODEL)";
    assert!(
        out.contains(base_line) && out.contains("replaces the configured model"),
        "{out}"
    );
    let out = explain(&cfg(USER), &brief("hi"), base, &secrets).unwrap();
    assert!(
        out.contains("model: cfg-model (configured") && out.contains(base_line),
        "{out}"
    );
    let out = explain(&Config::default(), &brief("hi"), base, &secrets).unwrap();
    assert!(
        out.contains("router: off") && out.contains(base_line),
        "{out}"
    );
    let out = explain(&cfg(USER), &brief("hi"), None, &secrets).unwrap();
    assert!(out.contains("configured model: unset"), "{out}");
}
