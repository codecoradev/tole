//! Tests of the config loader, layering and renderer.

use super::*;
use crate::test_dir::TestDir;
use std::path::PathBuf;

mod sandbox_keys;
mod table_keys;
mod validation;

fn tmpdir() -> TestDir {
    TestDir::new("tole-config")
}

fn p() -> PathBuf {
    PathBuf::from("cfg.toml")
}

const FULL: &str = r#"
model = "m"
base_url = "http://x"
sessions_dir = "s"
workspace = "w"
plan_mode = true
memory = "uteke"
trust = ["internal", "none"]
allow = ["read_file"]
mcp_server = ["a=b c"]
no_auto_mcp = true
on_pretool = ["p"]
on_posttool = ["q"]
on_turnend = ["cargo check"]
skill = ["skills/x/SKILL.md"]
no_skills = false
system_prompt = "hi"
context_window = 131072
compact_at = 0.75
no_compaction = false

[mission]
max_steps = 10
max_minutes = 5
max_tokens = 1000
verify = "cargo test"
verify_timeout = 60
"#;
