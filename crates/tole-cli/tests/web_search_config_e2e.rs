//! #436: a malformed `TOLE_WEB_SEARCH_*` config fails startup loudly with
//! the REAL binary, naming the variable and never the header value.

use std::process::Command;

#[test]
fn a_malformed_search_config_fails_startup_without_echoing_values() {
    let home = std::env::temp_dir().join(format!("tole-web-search-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_tole"))
        .args(["run", "hi"])
        .current_dir(&home)
        .env("CODECORA_HOME", &home)
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("TOLE_BASE_URL", "http://127.0.0.1:9")
        .env("TOLE_MODEL", "m")
        .env("TOLE_API_KEY", "k")
        .env("TOLE_WEB_SEARCH_URL", "http://127.0.0.1:9/search")
        .env(
            "TOLE_WEB_SEARCH_HEADERS",
            "Authorization=Bearer ${UNSET_FOR_E2E}",
        )
        .env("TOLE_WEB_SEARCH_FIELDS", "snippet=description")
        .env("SECRET_FOR_E2E", "sk-e2e")
        .env_remove("UNSET_FOR_E2E")
        .env_remove("TOLE_WEB_SEARCH_METHOD")
        .output()
        .unwrap();
    assert!(!o.status.success());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(
        err.contains("TOLE_WEB_SEARCH_HEADERS: `${UNSET_FOR_E2E}` is not set"),
        "{err}"
    );
    // A bad header with a set secret: the error names the header only.
    let o = Command::new(env!("CARGO_BIN_EXE_tole"))
        .args(["run", "hi"])
        .current_dir(&home)
        .env("CODECORA_HOME", &home)
        .env("TOLE_NO_UPDATE_CHECK", "1")
        .env("TOLE_BASE_URL", "http://127.0.0.1:9")
        .env("TOLE_MODEL", "m")
        .env("TOLE_API_KEY", "k")
        .env("TOLE_WEB_SEARCH_URL", "http://127.0.0.1:9/search")
        .env("TOLE_WEB_SEARCH_HEADERS", "Host=${SECRET_FOR_E2E}")
        .env("SECRET_FOR_E2E", "sk-e2e")
        .output()
        .unwrap();
    assert!(!o.status.success());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("`Host` is set by tole itself"), "{err}");
    assert!(!err.contains("sk-e2e"), "{err}");
    let _ = std::fs::remove_dir_all(&home);
}
