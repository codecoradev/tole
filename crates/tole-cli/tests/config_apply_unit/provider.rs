use crate::support::*;
use tole_cli::config::apply::*;

// ------------------------------------------------------------- provider

const FULL_TOLE: [(&str, &str); 3] = [
    ("TOLE_BASE_URL", "https://env"),
    ("TOLE_MODEL", "env-model"),
    ("TOLE_API_KEY", "k"),
];

#[test]
fn provider_without_config_equals_from_env_semantics() {
    let none = resolve_provider(None, None, &env_of(&FULL_TOLE));
    let c = none.config.unwrap();
    assert_eq!(
        (c.base_url.as_str(), c.model.as_str()),
        ("https://env", "env-model")
    );
    // Incomplete env + no config = unusable, exactly like from_env().
    let r = resolve_provider(None, None, &env_of(&[("TOLE_API_KEY", "k")]));
    assert!(r.config.is_none());
    // OPENAI_* fallback when TOLE_* is incomplete.
    let r = resolve_provider(
        None,
        None,
        &env_of(&[
            ("TOLE_MODEL", "x"),
            ("OPENAI_BASE_URL", "https://o"),
            ("OPENAI_MODEL", "om"),
            ("OPENAI_API_KEY", "k"),
        ]),
    );
    assert_eq!(r.config.unwrap().model, "om");
}

#[test]
fn provider_env_beats_config() {
    let r = resolve_provider(Some("https://cfg"), Some("cfg-model"), &env_of(&FULL_TOLE));
    let (b, m) = (r.base_url.unwrap(), r.model.unwrap());
    assert_eq!(
        (b.value.as_str(), b.source, b.env),
        ("https://env", Source::Env, Some("TOLE_BASE_URL"))
    );
    assert_eq!(
        (m.value.as_str(), m.source, m.env),
        ("env-model", Source::Env, Some("TOLE_MODEL"))
    );
}

#[test]
fn provider_config_fills_missing_env_values() {
    // Key + base in env, model only in config.
    let r = resolve_provider(
        None,
        Some("cfg-model"),
        &env_of(&[("TOLE_BASE_URL", "https://env"), ("TOLE_API_KEY", "k")]),
    );
    let m = r.model.clone().unwrap();
    assert_eq!((m.value.as_str(), m.source), ("cfg-model", Source::Config));
    assert_eq!(r.base_url.unwrap().source, Source::Env);
    assert_eq!(r.config.unwrap().model, "cfg-model");
    // Key only in env; base + model from config.
    let r = resolve_provider(
        Some("https://cfg"),
        Some("cfg-model"),
        &env_of(&[("TOLE_API_KEY", "k")]),
    );
    let c = r.config.unwrap();
    assert_eq!(
        (c.base_url.as_str(), c.model.as_str()),
        ("https://cfg", "cfg-model")
    );
    // The API key never comes from the config: no key = unusable.
    let r = resolve_provider(Some("https://cfg"), Some("cfg-model"), &env_of(&[]));
    assert!(r.config.is_none());
    assert_eq!(r.model.unwrap().source, Source::Config);
}

#[test]
fn provider_complete_env_prefix_wins_over_config_filled_one() {
    // TOLE has only a key (config could fill it) but OPENAI is complete:
    // today's behavior (OPENAI) is kept.
    let r = resolve_provider(
        Some("https://cfg"),
        Some("cfg-model"),
        &env_of(&[
            ("TOLE_API_KEY", "k1"),
            ("OPENAI_BASE_URL", "https://o"),
            ("OPENAI_MODEL", "om"),
            ("OPENAI_API_KEY", "k2"),
        ]),
    );
    assert_eq!(r.config.unwrap().model, "om");
}
