use crate::support::*;
use tole_cli::config::apply::*;
use tole_cli::config::Config;

// ------------------------------------------------- review gate (#534)

fn gate(cfg_src: &str, env: &[(&str, &str)]) -> Option<(String, Source)> {
    Settings::resolve(&Flags::default(), &cfg(cfg_src), &env_of(env))
        .review_gate
        .map(|e| (e.value, e.source))
}

#[test]
fn review_gate_precedence_env_config_default() {
    let c = "review_gate = \"warn\"\n";
    assert_eq!(
        gate(c, &[("TOLE_REVIEW_GATE", "OFF")]),
        Some(("off".into(), Source::Env)),
        "env wins and is normalized"
    );
    assert_eq!(gate(c, &[]), Some(("warn".into(), Source::Config)));
    assert_eq!(gate("", &[]), None, "default = the built-in block");
    assert_eq!(gate("", &[("TOLE_REVIEW_GATE", " ")]), None);
}

#[cfg(feature = "shell-tools")]
#[test]
fn the_review_gate_env_name_matches_the_gate_module() {
    assert_eq!(
        tole_cli::config_apply::REVIEW_GATE_ENV,
        tole_cli::review_gate::ENV
    );
}

// ------------------------------------------------- reasoning effort (#457)

fn effort(flags: &Flags, cfg_src: &str, env: &[(&str, &str)]) -> Option<(String, Source)> {
    Settings::resolve(flags, &cfg(cfg_src), &env_of(env))
        .reasoning_effort
        .map(|e| (e.value, e.source))
}

#[test]
fn reasoning_effort_precedence_flag_env_config_default() {
    let flag = Flags {
        reasoning_effort: s("minimal"),
        ..Default::default()
    };
    let env = [("TOLE_REASONING_EFFORT", "low")];
    let c = "reasoning_effort = \"medium\"\n";
    assert_eq!(
        effort(&flag, c, &env),
        Some(("minimal".into(), Source::Flag))
    );
    let none = Flags::default();
    assert_eq!(effort(&none, c, &env), Some(("low".into(), Source::Env)));
    assert_eq!(
        effort(&none, c, &[]),
        Some(("medium".into(), Source::Config))
    );
    assert_eq!(effort(&none, "", &[]), None, "default = unset");
    assert_eq!(effort(&none, "", &[("TOLE_REASONING_EFFORT", "  ")]), None);
}

#[test]
fn reasoning_effort_from_the_user_layer_is_labelled() {
    let user = cfg("reasoning_effort = \"high\"\n");
    let merged = tole_cli::config::layered(&Config::default(), &user);
    let e = Settings::resolve(&Flags::default(), &merged, &env_of(&[]))
        .reasoning_effort
        .unwrap();
    assert_eq!((e.value.as_str(), e.source), ("high", Source::UserConfig));
}

#[test]
fn invalid_env_effort_is_reported_not_swallowed() {
    let st = Settings::resolve(
        &Flags::default(),
        &Config::default(),
        &env_of(&[("TOLE_REASONING_EFFORT", "turbo")]),
    );
    let msg = st.effort_error().expect("invalid env value is an error");
    assert!(
        msg.contains("TOLE_REASONING_EFFORT") && msg.contains("turbo"),
        "{msg}"
    );
    let ok = Settings::resolve(
        &Flags::default(),
        &Config::default(),
        &env_of(&[("TOLE_REASONING_EFFORT", " HIGH ")]),
    );
    assert_eq!(ok.effort_error(), None);
    assert_eq!(ok.reasoning_effort.unwrap().value, "high", "canonical form");
}
