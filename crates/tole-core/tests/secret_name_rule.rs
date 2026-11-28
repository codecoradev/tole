//! The env var NAME rule of the secret redactor (`is_secret_name`, #682): the
//! public-API tests moved unchanged from the unit tests of `secrets/mod.rs`.

use tole_core::secrets::is_secret_name;

#[test]
fn the_name_rule_matches_the_env1_shapes() {
    for n in [
        "TOLE_API_KEY",
        "openai_apikey",
        "SECRET_KEY",
        "TOLE_TEST_AUTHTOKEN",
        "DB_PASSWD",
        "MY_PASSWORD",
        "AWS_CREDENTIALS",
        "SSH_PRIVATE_KEY",
        "GITHUB_TOKEN",
    ] {
        assert!(is_secret_name(n), "{n}");
    }
    for n in ["PATH", "HOME", "TOLE_MODEL", "TOKENIZERS_PARALLELISM"] {
        assert!(!is_secret_name(n), "{n}");
    }
}

/// #682: secret-bearing names the first rule missed (the audit's list,
/// less the `*_URL` ones, which are secrets by value).
#[test]
fn the_name_rule_covers_the_pass_key_dsn_and_auth_families() {
    for n in [
        "DB_PASS",
        "PGPASS",
        "DATABASE_URL",
        "TEST_DATABASE_URL",
        "SENTRY_DSN",
        "NGINJEN_KEY",
        "GH_HOOK_KEY",
        "HOOK_KEY",
        "STRIPE_SK",
        "OPENAI_KEY",
        "AWS_SECRET",
        "NPM_AUTH",
        "SESSION_COOKIE",
        "AUTH_BEARER",
        "GPG_PASSPHRASE",
        "ssh_key_pass",
    ] {
        assert!(is_secret_name(n), "{n}");
    }
    // Ordinary names stay ordinary, including the near misses.
    for n in [
        "SSH_AUTH_SOCK",
        "GPG_KEY",
        "SSH_PUBLIC_KEY",
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "MONKEY",
        "KEYBOARD",
        "TOLE_BASE_URL",
        "REDIS_URL",
        "NO_PROXY",
        "PROXY_BYPASS",
        "XDG_SESSION_TYPE",
        "DBUS_SESSION_BUS_ADDRESS",
        "TERM",
        "LANG",
        "PWD",
        "OLDPWD",
    ] {
        assert!(!is_secret_name(n), "{n}");
    }
}
