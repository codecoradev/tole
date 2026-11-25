//! The chat and mission request timeout (#1236), through the real binary: an
//! invalid `TOLE_REQUEST_TIMEOUT_SECS` or `[provider] request_timeout_secs`
//! stops tole at startup naming the key and the accepted range. The real
//! 30 s floor makes a live timeout too slow for a test; the resolution
//! precedence and the failover classification are unit-tested.

mod common;
use common::TestDir;
use std::process::Output;

fn home(tag: &str, user_config: &str) -> TestDir {
    let home = TestDir::new(&format!("tole-req-timeout-{tag}"));
    std::fs::create_dir_all(home.join("tole")).unwrap();
    std::fs::write(home.join("tole/config.toml"), user_config).unwrap();
    home
}

fn run(home: &std::path::Path, timeout_env: Option<&str>) -> Output {
    let mut c = common::hermetic_command(env!("CARGO_BIN_EXE_tole"), home, &[]);
    c.args(["run", "hi"])
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("TOLE_BASE_URL", "http://127.0.0.1:1/v1")
        .env("TOLE_MODEL", "mock")
        .env("TOLE_API_KEY", "k")
        .env_remove("OPENAI_BASE_URL")
        .env_remove("OPENAI_MODEL")
        .env_remove("TOLE_REQUEST_TIMEOUT_SECS");
    if let Some(t) = timeout_env {
        c.env("TOLE_REQUEST_TIMEOUT_SECS", t);
    }
    c.output().unwrap()
}

fn out(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn an_invalid_request_timeout_env_is_a_startup_error_naming_the_variable() {
    for bad in ["soon", "0", "29", "99999"] {
        let o = run(&home("env", ""), Some(bad));
        let text = out(&o);
        assert!(!o.status.success(), "{bad}: {text}");
        assert!(
            text.contains("TOLE_REQUEST_TIMEOUT_SECS") && text.contains("30 to 3600"),
            "{bad}: {text}"
        );
    }
}

#[test]
fn a_config_request_timeout_out_of_range_is_a_startup_error_naming_the_key() {
    for bad in ["1", "29", "3601"] {
        let h = home(
            "cfg",
            &format!("[provider]\nrequest_timeout_secs = {bad}\n"),
        );
        let o = run(&h, None);
        let text = out(&o);
        assert!(!o.status.success(), "{bad}: {text}");
        assert!(
            text.contains("provider.request_timeout_secs") && text.contains("30 to 3600"),
            "{bad}: {text}"
        );
    }
}
