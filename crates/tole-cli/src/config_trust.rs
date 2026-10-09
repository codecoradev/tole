//! Project-config trust (#208, part 2 of 3): the content-bound trust store,
//! the gate function and the terminal-safe review text behind
//! `tole config trust|untrust|check`.
//!
//! NOTHING here is called from startup yet — part 3 wires [`gate`] in. The
//! config file is untrusted input from a cloned repo, so:
//!
//! * the trust record lives OUTSIDE the repo
//!   (`$CODECORA_HOME/tole/trusted-configs.json`, default `~/.codecora`),
//!   keyed by canonical project directory, and stores a SNAPSHOT of the
//!   trusted file content; trust = byte-identical comparison (no crypto dep);
//! * everything shown at the trust decision (path, content, diff) is
//!   attacker-controlled, so it is escaped before printing ([`sanitize`]);
//! * a non-interactive run fails closed with an exact instruction.
//!
//! Pure functions + an injected [`ConfigTrustIo`]; store paths are
//! parameters (only [`default_store_path`] reads the environment).

use crate::config::{self, CONFIG_REL_PATH, MAX_CONFIG_BYTES};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Store file name under `$CODECORA_HOME/tole/`.
pub const STORE_FILE: &str = "trusted-configs.json";
const STORE_VERSION: u64 = 1;
/// Diffs are LCS-based; above this many lines (either side) only a
/// one-line summary is printed.
pub const MAX_DIFF_LINES: usize = 2000;

// ---------------------------------------------------------------- store

/// One trusted snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// The exact file content that was trusted.
    pub content: String,
    /// Unix seconds when it was trusted.
    pub trusted_at: u64,
}

/// In-memory view of `trusted-configs.json`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Store {
    pub entries: BTreeMap<String, Entry>,
}

#[derive(Serialize)]
struct StoreOut<'a> {
    version: u64,
    entries: &'a BTreeMap<String, Entry>,
}

#[derive(Deserialize)]
struct StoreIn {
    entries: BTreeMap<String, Entry>,
}

/// Trust state of a config file against the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustStatus {
    Trusted,
    NotTrusted,
    /// An entry exists but the content differs.
    Changed,
}

/// Store location: `$CODECORA_HOME/tole/trusted-configs.json`, else
/// `$HOME|$USERPROFILE/.codecora/tole/trusted-configs.json` (the layout
/// `skills.rs` uses; tole-core's helpers are private, and widening its
/// published API for three lines was not worth it). Unlike skills there is
/// NO fallback to the cwd: a store inside the (possibly hostile) project
/// would defeat the whole point, so a missing home is an error.
pub fn default_store_path() -> Result<PathBuf> {
    // Empty counts as unset, and a RELATIVE root is refused: either would
    // resolve against the cwd, i.e. into the (possibly hostile) project.
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let root = var("CODECORA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            var("HOME")
                .or_else(|| var("USERPROFILE"))
                .map(|h| PathBuf::from(h).join(".codecora"))
        })
        .ok_or_else(|| anyhow!("cannot locate the trust store: set CODECORA_HOME or HOME"))?;
    if !root.is_absolute() {
        bail!(
            "cannot locate the trust store: {} is not an absolute path",
            root.display()
        );
    }
    Ok(root.join("tole").join(STORE_FILE))
}

fn store_err(path: &Path, what: &str) -> anyhow::Error {
    anyhow!(
        "trust store {} {what}; fix or delete it (it is never overwritten automatically)",
        path.display()
    )
}

/// Read the store. Missing file = empty store; unreadable / corrupt /
/// unsupported-version = hard error naming the file (never overwritten).
pub fn load_store(path: &Path) -> Result<Store> {
    let raw = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Store::default()),
        Err(e) => return Err(store_err(path, &format!("is unreadable ({e})"))),
    };
    let v: serde_json::Value = serde_json::from_slice(&raw)
        .map_err(|e| store_err(path, &format!("is not valid JSON ({e})")))?;
    match v.get("version").and_then(|x| x.as_u64()) {
        Some(STORE_VERSION) => {}
        Some(n) => {
            return Err(store_err(
                path,
                &format!("has unsupported version {n} (this tole understands {STORE_VERSION})"),
            ))
        }
        None => return Err(store_err(path, "has no numeric `version`")),
    }
    let parsed: StoreIn = serde_json::from_value(v)
        .map_err(|e| store_err(path, &format!("has an invalid shape ({e})")))?;
    Ok(Store {
        entries: parsed.entries,
    })
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Atomic write: temp file in the same directory (0600) + rename; the
/// directory is created as needed and set to 0700. A failed write leaves
/// no temp file behind.
pub fn save_store(path: &Path, store: &Store) -> Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("cannot restrict {}", dir.display()))?;
    }
    let body = serde_json::to_vec_pretty(&StoreOut {
        version: STORE_VERSION,
        entries: &store.entries,
    })?;
    let n = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let tmp = dir.join(format!(".{STORE_FILE}.tmp.{}.{n}", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(&body)?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, path)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow!("cannot write trust store {}: {e}", path.display())
    })
}

/// Map key for a project directory / explicit file.
fn key_str(key: &Path) -> String {
    key.to_string_lossy().into_owned()
}

/// Trusted iff an entry exists AND its content is byte-identical.
pub fn status(store_path: &Path, key: &Path, content: &str) -> Result<TrustStatus> {
    let store = load_store(store_path)?;
    Ok(match store.entries.get(&key_str(key)) {
        None => TrustStatus::NotTrusted,
        Some(e) if e.content.as_bytes() == content.as_bytes() => TrustStatus::Trusted,
        Some(_) => TrustStatus::Changed,
    })
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Record `content` as trusted for `key` (replacing any older snapshot).
pub fn record(store_path: &Path, key: &Path, content: &str) -> Result<()> {
    if content.len() as u64 > MAX_CONFIG_BYTES {
        bail!(
            "config is larger than the {} KiB limit; refusing to store it",
            MAX_CONFIG_BYTES / 1024
        );
    }
    let mut store = load_store(store_path)?;
    store.entries.insert(
        key_str(key),
        Entry {
            content: content.to_string(),
            trusted_at: now_secs(),
        },
    );
    save_store(store_path, &store)
}

/// Remove the entry for `key`; `Ok(false)` when there was none (the store
/// is then not touched).
pub fn remove(store_path: &Path, key: &Path) -> Result<bool> {
    let mut store = load_store(store_path)?;
    if store.entries.remove(&key_str(key)).is_none() {
        return Ok(false);
    }
    save_store(store_path, &store)?;
    Ok(true)
}

// -------------------------------------------------- terminal-safe output

fn is_unsafe(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{061C}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}'
                | '\u{FEFF}'
        )
}

/// Make attacker-controlled text safe to print at a trust decision.
/// `\n` and spaces pass through, `\t` becomes the literal two characters
/// `\t`, every other control character (C0 incl. ESC/CR/NUL, DEL, C1),
/// bidi override/isolate/mark, line/paragraph separator and zero-width
/// invisible becomes a visible `\u{hex}`. Same policy as tole-core's
/// crate-private `sanitize.rs` (kept local so its API is not widened),
/// plus `\n`/`\t` handling for multi-line text.
pub fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\n' => out.push('\n'),
            '\t' => out.push_str("\\t"),
            c if is_unsafe(c) => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Line diff old -> new, hand-rolled LCS. Output lines are prefixed
/// `- `/`+ `/`  ` (context, at most 2 lines around a change, gaps shown as
/// `  ...`). Above [`MAX_DIFF_LINES`] on either side: a one-line summary.
/// Output is sanitized.
pub fn line_diff(old: &str, new: &str) -> String {
    let a: Vec<&str> = old.split('\n').collect();
    let b: Vec<&str> = new.split('\n').collect();
    if a.len() > MAX_DIFF_LINES || b.len() > MAX_DIFF_LINES {
        return format!("changed ({} lines -> {} lines)", a.len(), b.len());
    }
    let (n, m) = (a.len(), b.len());
    // lcs[i][j] = LCS length of a[i..] and b[j..].
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    // (tag, line): ' ' keep, '-' removed, '+' added.
    let mut ops: Vec<(char, &str)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push((' ', a[i]));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            ops.push(('-', a[i]));
            i += 1;
        } else {
            ops.push(('+', b[j]));
            j += 1;
        }
    }
    ops.extend(a[i..].iter().map(|l| ('-', *l)));
    ops.extend(b[j..].iter().map(|l| ('+', *l)));

    let near_change = |idx: usize| {
        let lo = idx.saturating_sub(2);
        let hi = (idx + 2).min(ops.len() - 1);
        ops[lo..=hi].iter().any(|(t, _)| *t != ' ')
    };
    let mut lines = Vec::new();
    let mut gap = false;
    for (idx, (t, l)) in ops.iter().enumerate() {
        if *t == ' ' && !near_change(idx) {
            gap = true;
            continue;
        }
        if gap {
            lines.push("  ...".to_string());
            gap = false;
        }
        lines.push(format!("{t} {}", sanitize(l)));
    }
    if gap {
        lines.push("  ...".to_string());
    }
    lines.join("\n")
}

/// The text shown at a trust decision: canonical path, the FULL content,
/// and (when a previously trusted version exists) the diff old -> new.
pub fn review_text(shown_path: &Path, content: &str, previous: Option<&str>) -> String {
    let mut out = format!(
        "config: {}\n----- content -----\n{}\n----- end -----",
        sanitize(&shown_path.display().to_string()),
        sanitize(content.trim_end_matches('\n')),
    );
    if let Some(old) = previous {
        out.push_str("\n----- changes since it was last trusted (- old, + new) -----\n");
        out.push_str(&line_diff(old, content));
        out.push_str("\n----- end -----");
    }
    out
}

// ------------------------------------------------------------------ io

/// Injected user interaction (real stdin/stderr impl: [`StdIo`]).
pub trait ConfigTrustIo {
    /// Can the user be asked a question right now?
    fn is_interactive(&self) -> bool;
    /// Informational text (already sanitized by the caller).
    fn inform(&mut self, text: &str);
    /// Warning text (already sanitized by the caller).
    fn warn(&mut self, text: &str);
    /// Ask `prompt_text`; `true` only for an explicit yes.
    fn confirm(&mut self, prompt_text: &str) -> bool;
}

/// Real terminal IO. All output goes to STDERR (stdout may carry a
/// protocol or piped result). Interactive = stdin AND stderr are both
/// terminals: the question is read from stdin and shown on stderr, so
/// either being redirected means nobody can answer it.
pub struct StdIo;

impl ConfigTrustIo for StdIo {
    fn is_interactive(&self) -> bool {
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }
    fn inform(&mut self, text: &str) {
        eprintln!("{text}");
    }
    fn warn(&mut self, text: &str) {
        eprintln!("warning: {text}");
    }
    fn confirm(&mut self, prompt_text: &str) -> bool {
        eprint!("{prompt_text}");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        match std::io::stdin().lock().read_line(&mut line) {
            Ok(0) | Err(_) => false,
            Ok(_) => is_yes(&line),
        }
    }
}

/// Only `y` / `yes` (case-insensitive, surrounding space ignored).
pub fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

// ---------------------------------------------------------------- gate

/// How the config file was selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Named on the command line (`--config`): user intent, always trusted.
    Explicit,
    /// Found at `<cwd>/.tole/config.toml`: needs a trust record.
    Discovered,
}

/// What the gate decided. `content` is the exact text that was vetted —
/// part 3 should parse THIS, not re-read the file (no check/use race).
#[derive(Debug, Clone, PartialEq)]
pub struct GateOutcome {
    pub kind: GateKind,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateKind {
    /// Explicit path: store not consulted.
    Explicit,
    /// Already trusted, byte-identical.
    AlreadyTrusted,
    /// Approved at the prompt and persisted.
    TrustedNow,
}

/// Canonical store key + file path for a config.
#[derive(Debug, Clone, PartialEq)]
pub struct Subject {
    /// Canonical project directory (discovered) or canonical file path
    /// (explicit).
    pub key: PathBuf,
    /// The file to read.
    pub file: PathBuf,
}

impl Subject {
    /// Same selection rule as [`config::discover`]: `explicit` wins, else
    /// `<cwd>/.tole/config.toml`. Returns the origin too. The cwd
    /// must be canonicalizable; an explicit path that no longer exists is
    /// keyed by its absolute form (so `untrust` still works after delete).
    pub fn resolve(cwd: &Path, explicit: Option<&Path>) -> Result<(Subject, Origin)> {
        if let Some(p) = explicit {
            let abs = if p.is_absolute() {
                p.to_path_buf()
            } else {
                cwd.join(p)
            };
            let key = abs.canonicalize().unwrap_or_else(|_| abs.clone());
            return Ok((Subject { key, file: abs }, Origin::Explicit));
        }
        let dir = cwd
            .canonicalize()
            .with_context(|| format!("cannot canonicalize {}", cwd.display()))?;
        let file = dir.join(CONFIG_REL_PATH);
        Ok((Subject { key: dir, file }, Origin::Discovered))
    }

    /// The path to show a human (canonical).
    pub fn shown(&self) -> PathBuf {
        self.file
            .canonicalize()
            .unwrap_or_else(|_| self.file.clone())
    }

    fn shown_safe(&self) -> String {
        sanitize(&self.shown().display().to_string())
    }

    /// `tole config trust` (+ ` --config <path>` when the file is not the
    /// default `<project>/.tole/config.toml`).
    pub fn trust_command(&self) -> String {
        if self.file == self.key.join(CONFIG_REL_PATH) {
            "tole config trust".to_string()
        } else {
            format!("tole config trust --config {}", self.shown_safe())
        }
    }
}

fn read_content(file: &Path) -> Result<String> {
    config::read_source(file).map_err(|e| anyhow!("{e}"))
}

/// Decide whether the config may be used. See the module docs.
///
/// * `Explicit` -> trusted, store untouched.
/// * `Discovered` + trusted -> ok.
/// * untrusted/changed + interactive -> show path + full content (+ diff
///   vs the previous version), ask `trust this config? [y/N]`; yes
///   persists, no is the error `config not trusted`.
/// * untrusted/changed + NOT interactive -> hard error with the exact
///   instruction; never prompts.
pub fn gate(
    subject: &Subject,
    origin: Origin,
    store_path: &Path,
    io: &mut dyn ConfigTrustIo,
) -> Result<GateOutcome> {
    let content = read_content(&subject.file)?;
    if origin == Origin::Explicit {
        return Ok(GateOutcome {
            kind: GateKind::Explicit,
            content,
        });
    }
    let store = load_store(store_path)?;
    let previous = store.entries.get(&key_str(&subject.key));
    if previous.is_some_and(|e| e.content.as_bytes() == content.as_bytes()) {
        return Ok(GateOutcome {
            kind: GateKind::AlreadyTrusted,
            content,
        });
    }
    if !io.is_interactive() {
        bail!(
            "{} is not trusted (or changed since it was trusted). Run: {}",
            subject.shown_safe(),
            subject.trust_command()
        );
    }
    io.inform(&review_text(
        &subject.shown(),
        &content,
        previous.map(|e| e.content.as_str()),
    ));
    if !io.confirm("trust this config? [y/N] ") {
        bail!("config not trusted");
    }
    record(store_path, &subject.key, &content)?;
    Ok(GateOutcome {
        kind: GateKind::TrustedNow,
        content,
    })
}

// ------------------------------------------------------------ commands

/// `tole config trust [--config <path>] [--yes]`. The file must parse and
/// validate first. Prints exactly what is being trusted even with `--yes`.
pub fn trust_cmd(
    cwd: &Path,
    explicit: Option<&Path>,
    yes: bool,
    store_path: &Path,
    io: &mut dyn ConfigTrustIo,
) -> Result<()> {
    let (subject, _) = Subject::resolve(cwd, explicit)?;
    if explicit.is_none() && !subject.file.exists() {
        bail!("no {CONFIG_REL_PATH} in {}", cwd.display());
    }
    let content = read_content(&subject.file)?;
    config::parse(&content, &subject.file).map_err(|e| anyhow!("{e}"))?;
    let store = load_store(store_path)?;
    let previous = store.entries.get(&key_str(&subject.key));
    if previous.is_some_and(|e| e.content.as_bytes() == content.as_bytes()) {
        io.inform(&format!("already trusted: {}", subject.shown_safe()));
        return Ok(());
    }
    io.inform(&review_text(
        &subject.shown(),
        &content,
        previous.map(|e| e.content.as_str()),
    ));
    if !yes {
        if !io.is_interactive() {
            bail!(
                "cannot ask for confirmation here (not a terminal); \
                 re-run with --yes to trust the content shown above"
            );
        }
        if !io.confirm("trust this config? [y/N] ") {
            bail!("config not trusted");
        }
    }
    record(store_path, &subject.key, &content)?;
    io.inform(&format!("trusted: {}", subject.shown_safe()));
    Ok(())
}

/// `tole config untrust [--config <path>]`: idempotent.
pub fn untrust_cmd(
    cwd: &Path,
    explicit: Option<&Path>,
    store_path: &Path,
    io: &mut dyn ConfigTrustIo,
) -> Result<()> {
    let (subject, _) = Subject::resolve(cwd, explicit)?;
    let shown = subject.shown_safe();
    if remove(store_path, &subject.key)? {
        io.inform(&format!("untrusted: {shown}"));
    } else {
        io.inform(&format!("not trusted (nothing to remove): {shown}"));
    }
    Ok(())
}

/// The `trust:` line of `tole config check`.
pub fn trust_line(subject: &Subject, store_path: &Path) -> Result<String> {
    let content = read_content(&subject.file)?;
    let cmd = subject.trust_command();
    Ok(match status(store_path, &subject.key, &content)? {
        TrustStatus::Trusted => "trust: trusted".to_string(),
        TrustStatus::NotTrusted => format!("trust: NOT trusted — run: {cmd}"),
        TrustStatus::Changed => format!("trust: CHANGED since it was trusted — run: {cmd}"),
    })
}

#[cfg(test)]
#[path = "config_trust_tests.rs"]
mod tests;
