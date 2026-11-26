//! Where the config files live, how they are read, and the error they report.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Hard cap on the config file size (64 KiB).
pub const MAX_CONFIG_BYTES: u64 = 64 * 1024;

/// Project-relative location of the config file.
pub const CONFIG_REL_PATH: &str = ".tole/config.toml";

/// File name of the user config under the tole data dir (#433).
pub const USER_CONFIG_FILE: &str = "config.toml";

/// Where the user config lives: `$CODECORA_HOME/tole/config.toml`, default
/// `~/.codecora/tole/config.toml` (next to the config trust store). `None`
/// when neither `CODECORA_HOME` nor a home directory is set.
pub fn user_config_path() -> Option<PathBuf> {
    tole_core::paths::tole_data_dir().map(|d| d.join(USER_CONFIG_FILE))
}

/// Longest path shown in a message about the user config.
const SHOWN_PATH_MAX: usize = 200;

/// `p` for a one-line message (#864): the path comes from the environment
/// (`CODECORA_HOME`), so a newline or an escape sequence in it must not forge
/// a second stderr line or drive the terminal. Control characters are shown as
/// `\n`, `\r` or `\u{..}`, and a path over 200 characters is cut with `...`.
pub fn one_line_path(p: &std::path::Path) -> String {
    let mut out = String::new();
    let mut n = 0;
    for c in p.display().to_string().chars() {
        let piece = match c {
            '\n' => "\\n".to_string(),
            '\r' => "\\r".to_string(),
            '\t' => "\\t".to_string(),
            c if c.is_control() => format!("\\u{{{:x}}}", c as u32),
            c => c.to_string(),
        };
        n += piece.chars().count();
        if n > SHOWN_PATH_MAX {
            out.push('\u{2026}');
            return out;
        }
        out.push_str(&piece);
    }
    out
}

/// Whether the user config file is there (#864): `Ok(true)` it is, `Ok(false)`
/// it is plainly absent, `Err(message)` it cannot be inspected (permission
/// denied, a parent that is a file, an I/O error). `Path::exists` would read
/// the last two as "absent" and the owner's sandbox, approvals and secret
/// settings would silently turn into the defaults. The message names the way
/// out.
pub fn user_config_present(path: &std::path::Path) -> Result<bool, String> {
    path.try_exists().map_err(|e| {
        format!(
            "cannot inspect the user config {} ({e}); not running with the default sandbox, \
             approval and secret settings instead. Fix the path or its permissions, fix or \
             unset CODECORA_HOME, or pass --no-config to ignore the file for this one run \
             (owner at a terminal only)",
            one_line_path(path)
        )
    })
}

/// A config problem, rendered as `<path>:<line>:<col>: <message>` when a
/// source position is known, `<path>: <message>` otherwise.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigError {
    pub path: PathBuf,
    pub line_col: Option<(usize, usize)>,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line_col {
            Some((l, c)) => write!(f, "{}:{l}:{c}: {}", self.path.display(), self.message),
            None => write!(f, "{}: {}", self.path.display(), self.message),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Where the config applies: an explicit path wins; otherwise
/// `<cwd>/.tole/config.toml` if present. No parent walk. `None` = no
/// config. An explicit path is returned as-is even when missing so the
/// load reports the failure instead of silently ignoring a typo.
pub fn discover(cwd: &Path, explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    let p = cwd.join(CONFIG_REL_PATH);
    p.exists().then_some(p)
}

/// Read at most `MAX_CONFIG_BYTES` (+1 to detect overflow) — never the
/// whole file — and decode as UTF-8.
pub fn read_source(path: &Path) -> Result<String, ConfigError> {
    let err = |message: String| ConfigError {
        path: path.to_path_buf(),
        line_col: None,
        message,
    };
    let file = std::fs::File::open(path).map_err(|e| err(format!("cannot read config: {e}")))?;
    let mut buf = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| err(format!("cannot read config: {e}")))?;
    if buf.len() as u64 > MAX_CONFIG_BYTES {
        return Err(err(format!(
            "config file is larger than the {} KiB limit",
            MAX_CONFIG_BYTES / 1024
        )));
    }
    String::from_utf8(buf).map_err(|_| err("config file is not valid UTF-8".to_string()))
}
