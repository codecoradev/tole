//! #682: secrets tole holds by role, or by value, are scrubbed from the
//! children it starts and redacted on egress, whatever their variable is
//! called. One test in its own process: it edits the process environment.
#![cfg(feature = "shell-tools")]

use std::process::Command;

const ROLE_VALUE: &str = "role-held-key-0123456789abcdef";
const URL_PW: &str = "redis-pw-0123456789";
const DB_PASS: &str = "db-pass-0123456789";
const PROXY_PW: &str = "proxy-pw-0123456789";
const HEADER_VALUE: &str = "header-expansion-0123456789";

#[test]
fn role_and_value_secrets_leave_no_child_and_no_text() {
    // SAFETY (edition 2021: safe call): the only test in this binary, so no
    // other thread reads the environment meanwhile.
    std::env::set_var("ODDLY_NAMED_SETTING", ROLE_VALUE);
    std::env::set_var("REDIS_URL", format!("redis://:{URL_PW}@cache:6379/0"));
    std::env::set_var("DB_PASS", DB_PASS);
    std::env::set_var("HTTPS_PROXY", format!("http://u:{PROXY_PW}@proxy:3128"));
    std::env::set_var("ORDINARY_SETTING", "plain-value-kept");
    // A web backend header expanded from `${NAME}`: a secret whatever NAME is.
    std::env::set_var("HDR_SETTING", HEADER_VALUE);
    std::env::set_var("TOLE_WEB_SEARCH_HEADERS", "X-Api: ${HDR_SETTING}");

    // Held by role (a webhook key named by `hmac_key_env`, the serve token).
    tole_core::secrets::add_role_secret("hooks.gh", ROLE_VALUE);

    let mut cmd = Command::new("printenv");
    tole_core::subprocess::scrub_env_for(&mut cmd, "run_command");
    let env = String::from_utf8(cmd.output().expect("printenv").stdout).unwrap();
    for leaked in [ROLE_VALUE, URL_PW, DB_PASS, HEADER_VALUE] {
        assert!(!env.contains(leaked), "child env holds {leaked:?}");
    }
    assert!(env.contains("ORDINARY_SETTING=plain-value-kept"));
    assert!(
        env.contains(PROXY_PW),
        "proxy settings stay, every network command needs them"
    );

    let set = tole_core::secrets::process_secrets();
    let text = format!("{ROLE_VALUE} {URL_PW} {DB_PASS} {PROXY_PW} {HEADER_VALUE}");
    let out = set.redact(&text);
    assert_eq!(
        out,
        "«hooks.gh» «REDIS_URL» «DB_PASS» «HTTPS_PROXY» «HDR_SETTING»"
    );
    assert!(tole_core::secrets::role_secrets()
        .iter()
        .any(|(n, _)| n == "hooks.gh"));
}
