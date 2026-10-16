//! Provider construction with an optional reserve (#209).
//!
//! Every session-building site configures an `OpenAiProvider` (tool specs,
//! system prompt, live-token sinks, ...). `build` applies that SAME
//! configuration to the primary and, when `TOLE_FALLBACK_BASE_URL` is set,
//! to a reserve, and returns a `FailoverProvider` over the two. Without a
//! reserve the plain provider is returned: no behaviour change.
//!
//! The reserve needs its own `TOLE_FALLBACK_API_KEY`: the primary's key is
//! never sent to another host. `TOLE_FALLBACK_MODEL` defaults to the
//! primary's model.

use tole_core::failover::{FailoverProvider, Link};
use tole_core::openai::{OpenAiConfig, OpenAiProvider};
use tole_core::provider::Provider;

pub const ENV_FALLBACK_BASE_URL: &str = "TOLE_FALLBACK_BASE_URL";
pub const ENV_FALLBACK_MODEL: &str = "TOLE_FALLBACK_MODEL";
// The env var NAME, not a secret value.
// cora-ignore: sec-hardcoded-secret
pub const ENV_FALLBACK_API_KEY: &str = "TOLE_FALLBACK_API_KEY";

fn nonblank(env: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    env(name)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The reserve's config, if one is configured. A base URL without a key is
/// an error (fail loud rather than run without the reserve the operator
/// asked for).
pub fn fallback_config(
    primary: &OpenAiConfig,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<OpenAiConfig>, String> {
    let Some(base_url) = nonblank(env, ENV_FALLBACK_BASE_URL) else {
        return Ok(None);
    };
    let api_key = nonblank(env, ENV_FALLBACK_API_KEY).ok_or_else(|| {
        format!(
            "{ENV_FALLBACK_BASE_URL} is set but {ENV_FALLBACK_API_KEY} is not: the reserve needs \
             its own key (the primary's key is never sent to another host)"
        )
    })?;
    let model = nonblank(env, ENV_FALLBACK_MODEL).unwrap_or_else(|| primary.model.clone());
    Ok(Some(OpenAiConfig::new(base_url, model, api_key)))
}

/// `role: model @ host` with URL credentials stripped (recorded in
/// `tole_route`, so it must never carry a secret).
pub fn label(role: &str, cfg: &OpenAiConfig) -> String {
    format!(
        "{role}: {} @ {}",
        cfg.model,
        crate::doctor::redact_url(&cfg.base_url)
    )
}

/// Build the session's provider: `configure` is applied to the primary and,
/// when configured, to the reserve.
pub fn build(
    primary: OpenAiConfig,
    env: &dyn Fn(&str) -> Option<String>,
    configure: impl Fn(OpenAiProvider) -> OpenAiProvider,
) -> Result<Box<dyn Provider>, String> {
    let Some(reserve) = fallback_config(&primary, env)? else {
        return Ok(Box::new(configure(OpenAiProvider::new(primary))));
    };
    let chain = vec![
        Link {
            label: label("primary", &primary),
            provider: Box::new(configure(OpenAiProvider::new(primary))),
        },
        Link {
            label: label("fallback", &reserve),
            provider: Box::new(configure(OpenAiProvider::new(reserve))),
        },
    ];
    Ok(Box::new(FailoverProvider::new(chain)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
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
}
