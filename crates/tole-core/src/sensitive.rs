//! Sensitive files (#598 part b, design `secrets-integrations-design.md` §4):
//! files whose content is a secret by convention: `.env*`, `*.pem`, `id_*`,
//! `.npmrc`, `.netrc`, `*.key` (file name, case-insensitive).
//!
//! Every tool that returns file bytes shows such a file redacted:
//! `read_file` returns `KEY=«redacted»` lines (a key file one marker), the
//! `git` tool hides the hunks of a sensitive file in a diff, and
//! `cora_search` hides the text of a hit in one. A verbatim read is the
//! separate `read_file_raw` tool, `Risk::Write`, so it needs approval (or an
//! explicit `--allow read_file_raw`).
//!
//! A path counts when the name it was asked by OR its resolved real path
//! (symlinks followed) matches, so `notes.txt -> .env` is sensitive.

use serde_json::Value;
use std::path::Path;

/// What a hidden value (or a whole key file) becomes.
pub const REDACTED: &str = "«redacted»";

/// Whether a file NAME is sensitive (case-insensitive).
pub fn is_sensitive_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with(".env") || n == ".npmrc" || n == ".netrc" || is_key_name(&n)
}

/// Key material: shown as one marker, never line by line.
fn is_key_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.ends_with(".pem") || n.ends_with(".key") || n.starts_with("id_")
}

fn file_name(p: &Path) -> &str {
    p.file_name().and_then(|n| n.to_str()).unwrap_or("")
}

/// Whether reading `requested` (the name it was asked by), whose real path
/// is `real`, is sensitive: either name matches.
pub fn is_sensitive_path(requested: &Path, real: &Path) -> bool {
    is_sensitive_name(file_name(requested)) || is_sensitive_name(file_name(real))
}

/// The redacted view of a sensitive file's content. A key file (either
/// name) is one marker; anything else keeps its line structure: blank lines
/// stay, `KEY=value` becomes `KEY=«redacted»`, every other line (comments
/// included, they may hold a value) becomes the marker alone.
pub fn redact_content(requested: &Path, real: &Path, content: &str) -> String {
    if is_key_name(file_name(requested)) || is_key_name(file_name(real)) {
        return REDACTED.to_string();
    }
    let mut out = String::with_capacity(content.len().min(64 * 1024));
    for line in content.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let eol = &line[body.len()..];
        if body.trim().is_empty() {
            out.push_str(body);
        } else if let Some(i) = body.find('=') {
            out.push_str(body[..i].trim_end());
            out.push('=');
            out.push_str(REDACTED);
        } else {
            out.push_str(REDACTED);
        }
        out.push_str(eol);
    }
    out
}

/// A unified diff with every hunk line of a sensitive file replaced by its
/// `+`/`-`/` ` marker plus [`REDACTED`] (file headers and hunk ranges stay,
/// the text after a hunk range goes).
pub fn redact_diff(diff: &str) -> String {
    let mut out = String::with_capacity(diff.len());
    let mut hidden = false;
    let mut in_hunk = false;
    for line in diff.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let eol = &line[body.len()..];
        if let Some(rest) = body.strip_prefix("diff --git ") {
            hidden = rest.split_whitespace().any(|t| {
                let t = t
                    .strip_prefix("a/")
                    .or_else(|| t.strip_prefix("b/"))
                    .unwrap_or(t);
                is_sensitive_name(file_name(Path::new(t.trim_matches('"'))))
            });
            in_hunk = false;
            out.push_str(line);
            continue;
        }
        if !hidden {
            out.push_str(line);
            continue;
        }
        if let Some(ranges) = body.strip_prefix("@@") {
            in_hunk = true;
            // `@@ -1,2 +1,2 @@ <a line of the file>`: keep only the ranges.
            match ranges.find("@@") {
                Some(i) => out.push_str(&body[..i + 4]),
                None => out.push_str(body),
            }
        } else if in_hunk && body.starts_with(['+', '-', ' ']) {
            out.push_str(&body[..1]);
            out.push_str(REDACTED);
        } else if in_hunk && !body.starts_with('\\') {
            out.push_str(REDACTED);
        } else {
            out.push_str(body);
        }
        out.push_str(eol);
    }
    out
}

/// Keys of a search hit that name its file.
const PATH_KEYS: [&str; 5] = ["file", "path", "file_path", "filename", "location"];

/// Hides the text of every JSON object that names a sensitive file in one
/// of [`PATH_KEYS`] (a `path:line` value counts): each of its other string
/// fields, at any depth, becomes [`REDACTED`].
pub fn redact_hits(v: &mut Value) {
    match v {
        Value::Array(items) => items.iter_mut().for_each(redact_hits),
        Value::Object(map) => {
            let names_sensitive = PATH_KEYS.iter().any(|k| {
                map.get(*k).and_then(Value::as_str).is_some_and(|p| {
                    let p = p.split(':').next().unwrap_or(p);
                    is_sensitive_name(file_name(Path::new(p)))
                })
            });
            for (k, val) in map.iter_mut() {
                if names_sensitive && !PATH_KEYS.contains(&k.as_str()) {
                    hide_strings(val);
                } else {
                    redact_hits(val);
                }
            }
        }
        _ => {}
    }
}

fn hide_strings(v: &mut Value) {
    match v {
        Value::String(s) => *s = REDACTED.to_string(),
        Value::Array(items) => items.iter_mut().for_each(hide_strings),
        Value::Object(map) => map.values_mut().for_each(hide_strings),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sensitive_names_match_case_insensitively() {
        for n in [
            ".env",
            ".env.local",
            ".ENV.production",
            "server.pem",
            "TLS.KEY",
            "id_rsa",
            "id_ed25519",
            ".npmrc",
            ".NETRC",
        ] {
            assert!(is_sensitive_name(n), "{n}");
        }
        for n in [
            "env.rs",
            "notes.txt",
            "keyboard.rs",
            "my.envelope",
            "pem.rs",
            "",
            "npmrc",
        ] {
            assert!(!is_sensitive_name(n), "{n}");
        }
        // By the name asked OR the real path.
        assert!(is_sensitive_path(
            Path::new("notes.txt"),
            Path::new("/w/.env")
        ));
        assert!(is_sensitive_path(
            Path::new(".env"),
            Path::new("/w/plain.txt")
        ));
        assert!(!is_sensitive_path(Path::new("a.rs"), Path::new("/w/a.rs")));
    }

    #[test]
    fn env_lines_keep_their_keys_and_key_files_become_one_marker() {
        let env =
            "# db\nexport DB_PASSWORD=hunter2hunter2\n\nAPI_KEY = abc=def\r\nplain secret line\n";
        let got = redact_content(Path::new(".env"), Path::new("/w/.env"), env);
        assert_eq!(
            got,
            "«redacted»\nexport DB_PASSWORD=«redacted»\n\nAPI_KEY=«redacted»\r\n«redacted»\n"
        );
        assert!(!got.contains("hunter2") && !got.contains("abc"));
        let pem = "key material line one\nline two\n";
        assert_eq!(
            redact_content(Path::new("x"), Path::new("/w/tls.pem"), pem),
            REDACTED
        );
    }

    #[test]
    fn a_diff_hides_only_the_hunks_of_sensitive_files() {
        let diff = "diff --git a/src/a.rs b/src/a.rs\n@@ -1 +1 @@ fn a\n-old\n+new\n\
                    diff --git a/.env b/.env\nindex 1..2 100644\n--- a/.env\n+++ b/.env\n\
                    @@ -1,2 +1,2 @@ TOKEN=ctx-leak\n-TOKEN=old-secret\n+TOKEN=new-secret\n KEEP=x\n\
                    \\ No newline at end of file\n";
        let got = redact_diff(diff);
        assert!(got.contains("-old\n+new\n"), "{got}");
        assert!(
            got.contains("--- a/.env\n+++ b/.env\n@@ -1,2 +1,2 @@\n"),
            "{got}"
        );
        assert!(
            got.contains("-«redacted»\n+«redacted»\n «redacted»\n"),
            "{got}"
        );
        for leak in ["old-secret", "new-secret", "ctx-leak", "KEEP"] {
            assert!(!got.contains(leak), "{leak}: {got}");
        }
    }

    #[test]
    fn search_hits_in_sensitive_files_lose_their_text() {
        let mut v = json!({"results": [
            {"file": "src/main.rs:3", "snippet": "fn main()"},
            {"path": "config/.env.local:2", "snippet": "TOKEN=leaky", "ctx": {"lines": ["A=b"]}}
        ]});
        redact_hits(&mut v);
        assert_eq!(v["results"][0]["snippet"], json!("fn main()"));
        assert_eq!(v["results"][1]["path"], json!("config/.env.local:2"));
        assert_eq!(v["results"][1]["snippet"], json!(REDACTED));
        assert_eq!(v["results"][1]["ctx"]["lines"][0], json!(REDACTED));
    }
}
