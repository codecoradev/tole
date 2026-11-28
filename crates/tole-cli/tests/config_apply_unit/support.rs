use crate::common::TestDir;
use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tole_cli::config::Config;
use tole_cli::config::CONFIG_REL_PATH;
use tole_cli::config_trust::ConfigTrustIo;

pub fn tmpdir() -> TestDir {
    TestDir::new("tole-apply")
}

pub fn s(v: &str) -> Option<String> {
    Some(v.to_string())
}

pub fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let m: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k| m.get(k).cloned()
}

pub fn cfg(src: &str) -> Config {
    tole_cli::config::parse(src, Path::new("c.toml")).unwrap()
}

pub struct Fake {
    pub interactive: bool,
    pub answer: bool,
    pub shown: String,
    pub confirms: usize,
}

impl Fake {
    pub fn new(interactive: bool, answer: bool) -> Fake {
        Fake {
            interactive,
            answer,
            shown: String::new(),
            confirms: 0,
        }
    }
}

impl ConfigTrustIo for Fake {
    fn is_interactive(&self) -> bool {
        self.interactive
    }
    fn inform(&mut self, text: &str) {
        self.shown.push_str(text);
        self.shown.push('\n');
    }
    fn warn(&mut self, text: &str) {
        self.inform(text)
    }
    fn confirm(&mut self, p: &str) -> bool {
        self.confirms += 1;
        self.shown.push_str(p);
        self.answer
    }
}

pub fn project(content: &str) -> (TestDir, TestDir) {
    let d = tmpdir();
    std::fs::create_dir_all(d.join(".tole")).unwrap();
    std::fs::write(d.join(CONFIG_REL_PATH), content).unwrap();
    (d, tmpdir())
}

pub fn store_of(home: &Path) -> PathBuf {
    home.join("home").join("tole").join("trusted-configs.json")
}

pub fn no_store() -> Result<PathBuf> {
    panic!("the trust store must not be resolved here")
}
