//! `tole route` (#586) on the real binary: a dry run that names the tier
//! and model `[router]` would pick, calls no provider, and redacts.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const USER: &str = r#"
[router]
enabled = true
[router.tiers]
light = "small-model"
heavy = "big-model"
[[router.rules]]
name = "short-cron"
tier = "light"
kinds = ["cron"]
max_chars = 100
max_risk = "read_only"
[[router.rules]]
name = "coder-heavy"
tier = "heavy"
profiles = ["coder"]
[profiles.coder.models]
heavy = "coder-big"
default = "heavy"
"#;

fn setup(tag: &str, user: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("tole-route-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("home/tole")).unwrap();
    std::fs::create_dir_all(base.join("cwd")).unwrap();
    std::fs::write(base.join("home/tole/config.toml"), user).unwrap();
    base
}

fn tole(base: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tole"))
        .args(args)
        .current_dir(base.join("cwd"))
        .env("CODECORA_HOME", base.join("home"))
        .env("HOME", base.join("home"))
        // A provider that would refuse any connection: the dry run must
        // not need one.
        .env("TOLE_BASE_URL", "http://127.0.0.1:9/v1")
        .env("TOLE_MODEL", "configured-model")
        .env("TOLE_API_KEY", "dry-run-key-0123456789")
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn route_names_the_tier_and_model_without_a_provider() {
    let base = setup("pick", USER);
    let o = tole(
        &base,
        &[
            "route",
            "digest the inbox",
            "--kind",
            "cron",
            "--risk",
            "read_only",
        ],
    );
    let t = text(&o);
    assert!(o.status.success(), "{t}");
    assert!(
        t.contains("tier: light") && t.contains("model: small-model") && t.contains("short-cron"),
        "{t}"
    );
    assert!(t.contains("no provider was called"), "{t}");
    // No rule matches: the configured model stays.
    let t = text(&tole(&base, &["route", "digest the inbox"]));
    assert!(
        t.contains("model: configured-model (configured; no rule matched)")
            && t.contains("configured model: configured-model (from env TOLE_MODEL)"),
        "{t}"
    );
    // --profile uses that profile's tiers.
    let t = text(&tole(&base, &["--profile", "coder", "route", "anything"]));
    assert!(t.contains("model: coder-big"), "{t}");
    for bad in [["--kind", "batch"], ["--risk", "high"]] {
        let o = tole(&base, &["route", "x", bad[0], bad[1]]);
        assert!(!o.status.success(), "{bad:?}");
    }
}

#[test]
fn route_refuses_an_undeclared_tier_and_redacts_secrets() {
    let base = setup("bad", &USER.replace("light = \"small-model\"\n", ""));
    let o = tole(&base, &["route", "x", "--kind", "cron"]);
    assert!(!o.status.success());
    assert!(text(&o).contains("[router.tiers]"), "{}", text(&o));
    // A model name that holds a secret value is redacted on the way out.
    let base = setup(
        "redact",
        &USER.replace("small-model", "small-dry-run-key-0123456789"),
    );
    let t = text(&tole(
        &base,
        &["route", "x", "--kind", "cron", "--risk", "read_only"],
    ));
    assert!(
        t.contains("tier: light") && !t.contains("dry-run-key"),
        "{t}"
    );
}
