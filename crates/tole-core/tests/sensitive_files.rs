//! #683: common credential files are sensitive, so `read_file` redacts them.

#[path = "support/test_dir.rs"]
mod test_dir;
use std::path::Path;
use test_dir::TestDir;
use tole_core::sensitive::{is_sensitive_name, is_sensitive_path};

#[test]
fn common_secret_files_are_sensitive() {
    let missed: Vec<&str> = [
        "prod.env",
        "config.env",
        "credentials",
        ".git-credentials",
        ".pgpass",
        ".htpasswd",
        "secrets.yaml",
        "secrets.yml",
        "terraform.tfvars",
        "cert.p12",
        "client.pfx",
        "app.jks",
        "release.keystore",
        "id_rsa",
        ".pypirc",
        ".dockercfg",
        "kubeconfig",
    ]
    .into_iter()
    .filter(|n| !is_sensitive_name(n))
    .collect();
    assert!(missed.is_empty(), "not sensitive: {missed:?}");
    assert!(is_sensitive_path(
        Path::new(".docker/config.json"),
        Path::new("/w/.docker/config.json")
    ));
    assert!(!is_sensitive_path(
        Path::new("app/config.json"),
        Path::new("/w/app/config.json")
    ));
}

#[test]
fn read_file_redacts_a_suffix_env_file() {
    use tole_core::tool::Tool;
    let dir = TestDir::new("tole-683");
    std::fs::write(dir.join("prod.env"), "DB_PASSWORD=hunter2hunter2\n").unwrap();
    let tool = tole_core::read_file::ReadFileTool::new(dir.to_path_buf());
    let out = tool
        .execute(serde_json::json!({"path": "prod.env"}))
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(!out.to_string().contains("hunter2"), "{out}");
}
