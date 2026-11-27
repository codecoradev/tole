//! The provider chain: reserve config, labels and log secrets.

use tole_cli::provider_chain::*;
use tole_core::openai::OpenAiConfig;
use tole_core::secrets::SecretSet;

fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |k| {
        pairs
            .iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| v.to_string())
    }
}

#[test]
fn the_log_redacts_every_secret_the_providers_are_shown_redacted() {
    // #596: the set build installs for the session log includes both
    // providers keys and URL passwords, which no env name carries.
    let primary = OpenAiConfig::new(
        "https://u:pw-primary-0596@a.example/v1",
        "m",
        "sk-p-0596-key",
    );
    let reserve = OpenAiConfig::new(
        "https://v:pw-reserve-0596@b.example/v1",
        "m2",
        "sk-r-0596-key",
    );
    let set = log_secrets(&SecretSet::default(), &primary, Some(&reserve));
    for v in [
        "pw-primary-0596",
        "pw-reserve-0596",
        "sk-p-0596-key",
        "sk-r-0596-key",
    ] {
        assert!(
            !set.redact(&format!("x {v} y")).contains(v),
            "{v} not in the log set"
        );
    }
}

fn primary() -> OpenAiConfig {
    OpenAiConfig::new("https://a.example/v1", "model-a", "sk-primary")
}

#[test]
fn no_reserve_unless_a_base_url_is_set() {
    assert_eq!(fallback_config(&primary(), &env(&[])), Ok(None));
    assert_eq!(
        fallback_config(&primary(), &env(&[(ENV_FALLBACK_BASE_URL, "  ")])),
        Ok(None)
    );
}

#[test]
fn the_reserve_needs_its_own_key_and_inherits_the_model() {
    let e = fallback_config(
        &primary(),
        &env(&[(ENV_FALLBACK_BASE_URL, "https://b.example/v1")]),
    )
    .unwrap_err();
    assert!(e.contains(ENV_FALLBACK_API_KEY), "{e}");
    assert!(!e.contains("sk-primary"));
    let r = fallback_config(
        &primary(),
        &env(&[
            (ENV_FALLBACK_BASE_URL, "https://b.example/v1"),
            (ENV_FALLBACK_API_KEY, "sk-b"),
        ]),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        (r.base_url.as_str(), r.model.as_str(), r.api_key.as_str()),
        ("https://b.example/v1", "model-a", "sk-b")
    );
    let r = fallback_config(
        &primary(),
        &env(&[
            (ENV_FALLBACK_BASE_URL, "https://b.example/v1"),
            (ENV_FALLBACK_API_KEY, "sk-b"),
            (ENV_FALLBACK_MODEL, "model-b"),
        ]),
    )
    .unwrap()
    .unwrap();
    assert_eq!(r.model, "model-b");
}

#[test]
fn labels_never_carry_credentials() {
    let cfg = OpenAiConfig::new("https://user:pw@b.example/v1?key=k", "m", "sk-x");
    let l = label("fallback", &cfg);
    assert_eq!(l, "fallback: m @ https://b.example/v1");
    assert!(!l.contains("pw") && !l.contains("sk-x"));
}

#[test]
fn build_fails_loud_on_a_half_configured_reserve() {
    let r = build(
        primary(),
        &env(&[(ENV_FALLBACK_BASE_URL, "https://b.example/v1")]),
        |p| p,
    );
    assert!(r.is_err());
    assert!(build(primary(), &env(&[]), |p| p).is_ok());
}
