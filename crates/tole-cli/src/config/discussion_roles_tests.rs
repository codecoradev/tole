use super::*;
use std::path::Path;

fn cfg(src: &str) -> Config {
    crate::config::parse(src, Path::new("user.toml")).unwrap()
}

fn load_err(src: &str) -> String {
    crate::config::parse(src, Path::new("user.toml"))
        .unwrap_err()
        .to_string()
}

#[test]
fn the_built_in_roles_run_on_the_light_tier() {
    for role in Role::ALL {
        let r = resolve(&Config::default(), role).unwrap();
        assert_eq!(
            (r.tier, r.model, r.source),
            (Tier::Light, None, "built-in, the configured model")
        );
        // The light tier of [router.tiers], whether or not routing is on.
        let c = cfg("[router.tiers]\nlight = \"small-model\"\nheavy = \"big-model\"\n");
        let r = resolve(&c, role).unwrap();
        assert_eq!(
            (r.tier, r.model.as_deref(), r.source),
            (Tier::Light, Some("small-model"), "built-in, [router.tiers]")
        );
    }
    assert_eq!(Role::of("claim-checker"), Some(Role::ClaimChecker));
    assert_eq!(Role::of("coder"), None);
}

#[test]
fn the_owner_overrides_a_role_with_models_only() {
    let c = cfg("[profiles.scribe.models]\nlight = \"tiny\"\n\
                 [profiles.claim-checker.models]\nlight = \"tiny\"\nheavy = \"careful\"\n\
                 default = \"heavy\"\n");
    let s = resolve(&c, Role::Scribe).unwrap();
    assert_eq!(
        (s.tier, s.model.as_deref(), s.source),
        (Tier::Light, Some("tiny"), "user override")
    );
    let k = resolve(&c, Role::ClaimChecker).unwrap();
    assert_eq!((k.tier, k.model.as_deref()), (Tier::Heavy, Some("careful")));
}

#[test]
fn an_override_that_does_not_resolve_is_a_load_error_never_a_fallback() {
    for (src, want) in [
        ("[profiles.scribe]\n", "sets no models"),
        (
            "[profiles.scribe.models]\nstandard = \"m\"\n",
            "resolves to the light tier but profiles.scribe.models.light is not set",
        ),
        (
            "[profiles.claim-checker.models]\nlight = \"m\"\ndefault = \"heavy\"\n",
            "profiles.claim-checker.models.heavy is not set",
        ),
        (
            "[profiles.scribe.models]\nlight = \"m\"\ndefault = \"advisor\"\n",
            "must be light, standard or heavy",
        ),
        (
            "[profiles.scribe.models]\nlight = \"m\"\ndefault = \"bogus\"\n",
            "must be light, standard or heavy",
        ),
        ("[profiles.scribe.models]\nlight = \"  \"\n", "is not set"),
    ] {
        let e = load_err(src);
        assert!(e.contains(want), "{src}: {e}");
    }
}

#[test]
fn an_override_cannot_give_a_role_tools_instructions_or_secrets() {
    for key in [
        "system_prompt_file = \"x.md\"",
        "allow = [\"write_file\"]",
        "skills = [\"s/SKILL.md\"]",
        "grants = [\"GITHUB_TOKEN\"]",
        "memory_room = \"r\"",
        "advisor_max_uses = 2",
    ] {
        let src = format!("[profiles.scribe]\n{key}\n[profiles.scribe.models]\nlight = \"m\"\n");
        let e = load_err(&src);
        let name = key.split(' ').next().unwrap();
        assert!(
            e.contains(&format!("profiles.scribe.{name}")) && e.contains("only its models"),
            "{key}: {e}"
        );
    }
    let e = load_err("[profiles.claim-checker.models]\nlight = \"m\"\nadvisor = \"w\"\n");
    assert!(e.contains("profiles.claim-checker.models.advisor"), "{e}");
}

#[test]
fn a_project_config_can_neither_override_nor_widen_a_role() {
    let project = cfg("[profiles.scribe.models]\nlight = \"evil\"\n\
                       [router.tiers]\nlight = \"evil\"\n");
    let user = cfg("[router.tiers]\nlight = \"small-model\"\n");
    let m = crate::config::layered(&project, &user);
    let r = resolve(&m, Role::Scribe).unwrap();
    assert_eq!(
        (r.model.as_deref(), r.source),
        (Some("small-model"), "built-in, [router.tiers]")
    );
    let m = crate::config::layered(&project, &Config::default());
    assert_eq!(resolve(&m, Role::Scribe).unwrap().model, None);
}

#[test]
fn doctor_names_where_each_role_comes_from_never_the_model() {
    let c = cfg("[profiles.scribe.models]\nlight = \"tiny-model\"\n\
                 [router.tiers]\nlight = \"small-model\"\n");
    let text = doctor_lines(&c).join("\n");
    assert!(
        text.contains("discussion scribe: light tier (user override)")
            && text.contains("discussion claim-checker: light tier (built-in, [router.tiers])"),
        "{text}"
    );
    assert!(
        !text.contains("tiny-model") && !text.contains("small-model"),
        "{text}"
    );
    let text = doctor_lines(&Config::default()).join("\n");
    assert!(text.contains("declare [router.tiers] light"), "{text}");
}
