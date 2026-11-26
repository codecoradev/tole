//! Golden files for the serve wire (#1024): the JSON a client reads from
//! `tole serve`, committed under `tests/golden/serve/`, with the values that
//! change from run to run replaced by a placeholder.
//!
//! A diff fails the test. A deliberate change is accepted on purpose:
//!
//! ```text
//! TOLE_UPDATE_GOLDEN=1 cargo test -p tole-cli --test <name>
//! ```
//!
//! rewrites the files; review the diff and read the additive-only rule in
//! `docs/mobile-control.md` ("Wire compatibility") before committing it. A
//! removed or renamed key, or a value that means something else, is a protocol
//! break (`SERVE_PROTOCOL_VERSION`, a BREAKING line in `CHANGELOG.md`).

#![allow(dead_code)]

use serde_json::{Map, Value};
use std::path::PathBuf;

use crate::serve_harness::{home_of, Serve};

/// The placeholders for what differs between runs of the same scenario.
pub struct Volatile {
    /// Literal strings (a session id, an approval id, a directory) and the
    /// placeholder each becomes wherever it occurs inside a string.
    pub strings: Vec<(String, &'static str)>,
}

/// Keys whose number or text is a clock reading or a time left, never a contract.
const CLOCK_KEYS: &[&str] = &[
    "age_seconds",
    "created_at",
    "createdAtMs",
    "decidedAtMs",
    "expiresInSeconds",
    "lastActivity",
    "next",
    "next_run",
    "snapshotAt",
    "timestamp",
];

/// Keys whose number counts characters of the system prompt and the tool
/// schemas, which change with every prompt edit and with the length of the
/// workspace path: the contract is that the key exists, not its value.
const SIZE_KEYS: &[&str] = &[
    "history_chars",
    "prefix_chars",
    "system_chars",
    "tools_chars",
];

impl Volatile {
    /// The workspace and home of `serve` and the ids a scenario has made.
    pub fn of(serve: &Serve, ids: &[(&str, &'static str)]) -> Self {
        let mut strings = vec![
            (home_of(&serve.dir).display().to_string(), "<HOME>"),
            (serve.dir.display().to_string(), "<WORKSPACE>"),
        ];
        // The directory the OS resolves it to (a symlinked temp dir).
        if let Ok(real) = std::fs::canonicalize(&serve.dir) {
            strings.push((real.display().to_string(), "<WORKSPACE>"));
        }
        for (id, ph) in ids {
            strings.push(((*id).to_string(), *ph));
        }
        // Longest first, so a path is replaced before the id inside it.
        strings.sort_by_key(|(s, _)| std::cmp::Reverse(s.len()));
        Self { strings }
    }

    pub fn apply(&self, v: &Value) -> Value {
        self.walk(None, v)
    }

    fn walk(&self, key: Option<&str>, v: &Value) -> Value {
        match v {
            Value::Object(m) => Value::Object(
                m.iter()
                    .map(|(k, x)| (self.text(k), self.walk(Some(k), x)))
                    .collect::<Map<_, _>>(),
            ),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.walk(key, x)).collect()),
            // The binary's own version moves with every release.
            Value::String(s) if key == Some("version") && s == env!("CARGO_PKG_VERSION") => {
                "<VERSION>".into()
            }
            // `null` stays: "no time yet" is part of the contract.
            Value::Number(_) | Value::String(_) if key.is_some_and(|k| CLOCK_KEYS.contains(&k)) => {
                "<clock>".into()
            }
            // The assembled prompt is pinned by `assemble_golden`, not by the wire;
            // it also holds today's date.
            Value::String(_) if key == Some("systemPrompt") => "<SYSTEM PROMPT>".into(),
            Value::String(s) => Value::String(self.text(s)),
            Value::Number(_) if key.is_some_and(|k| SIZE_KEYS.contains(&k)) => "<chars>".into(),
            v => v.clone(),
        }
    }

    fn text(&self, s: &str) -> String {
        let mut out = s.to_string();
        for (from, to) in &self.strings {
            if !from.is_empty() {
                out = out.replace(from.as_str(), to);
            }
        }
        out
    }
}

/// `seq` numbers and the ids built from them (`e_13`, `intent_8`) depend on
/// how many other lines the log held before, which a client must not rely on
/// ("strictly increasing but not dense"). Rewrite each to its rank among the
/// `seq`s of the document, so a new log line in a turn does not rewrite every
/// golden file, while an ordering or a parent link that breaks still shows.
pub fn renumber(v: &Value) -> Value {
    let mut seqs = std::collections::BTreeSet::new();
    collect_seqs(v, &mut seqs);
    let rank: std::collections::BTreeMap<u64, u64> = seqs.into_iter().zip(1..).collect();
    rewrite(None, v, &rank)
}

fn collect_seqs(v: &Value, out: &mut std::collections::BTreeSet<u64>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                match (k.as_str(), x.as_u64()) {
                    ("seq", Some(n)) => {
                        out.insert(n);
                    }
                    _ => collect_seqs(x, out),
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect_seqs(x, out)),
        _ => {}
    }
}

fn rewrite(key: Option<&str>, v: &Value, rank: &std::collections::BTreeMap<u64, u64>) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| (k.clone(), rewrite(Some(k), x, rank)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(|x| rewrite(key, x, rank)).collect()),
        Value::Number(n) if matches!(key, Some("seq" | "lastSeq" | "nextAfter")) => n
            .as_u64()
            .and_then(|n| rank.get(&n))
            .map_or_else(|| v.clone(), |r| Value::from(*r)),
        // `<prefix>_<number>` with a number that is a seq of the document.
        Value::String(s) => match s.rsplit_once('_') {
            Some((prefix, n)) if !prefix.is_empty() => n
                .parse::<u64>()
                .ok()
                .and_then(|n| rank.get(&n))
                .map_or_else(|| v.clone(), |r| Value::String(format!("{prefix}_{r}"))),
            _ => v.clone(),
        },
        v => v.clone(),
    }
}

/// Objects with their keys sorted, so the file does not depend on whether
/// another crate turned on `serde_json/preserve_order`.
fn sorted(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut e: Vec<_> = m.iter().collect();
            e.sort_by(|a, b| a.0.cmp(b.0));
            Value::Object(e.into_iter().map(|(k, x)| (k.clone(), sorted(x))).collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
        v => v.clone(),
    }
}

fn path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/serve")
        .join(format!("{name}.json"))
}

/// Compare `actual` (already normalised) with the committed file `name`.
#[track_caller]
pub fn check(name: &str, actual: &Value) {
    let text = format!(
        "{}\n",
        serde_json::to_string_pretty(&sorted(actual)).unwrap()
    );
    let file = path(name);
    if std::env::var_os("TOLE_UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, &text).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&file).unwrap_or_else(|e| {
        panic!(
            "no golden file {}: {e}\n(TOLE_UPDATE_GOLDEN=1 writes it)\n{text}",
            file.display()
        )
    });
    assert_eq!(
        text,
        want,
        "{name}: the wire changed. If it is deliberate, read \"Wire compatibility\" in \
         docs/mobile-control.md, then TOLE_UPDATE_GOLDEN=1 cargo test -p tole-cli --test <name> \
         and review the diff of {}",
        file.display()
    );
}
