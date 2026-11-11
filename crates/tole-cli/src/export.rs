//! Transcript export for the TUI's `/export` and answer lookup for `/copy`
//! (#463). Pure functions over a session's entries: the host (`tui_host`)
//! hands them the entries of the open session and the workspace root, so
//! nothing here touches the session file (it is never mutated or truncated;
//! the export is a read of the in-memory replay).
//!
//! Safety rules, all enforced here and tested:
//! - the output path is jailed to the workspace (canonicalized, symlink-safe,
//!   `..` and outside-absolute paths refused), like the file tools and
//!   `--image`;
//! - an existing file is NEVER overwritten (`create_new`, which also refuses
//!   a symlink sitting at the target, dangling or not);
//! - the HTML page escapes every piece of text from the log, has no script and
//!   no external resource, and says so in a CSP `<meta>`.
//!
//! Transcripts contain whatever the tools printed, including secrets; the
//! file is created owner-only (0600) on Unix, and docs/tui.md says so.

use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;
use tole_core::entry::{Entry, EntryType};

/// The formats `/export` writes, chosen by the output file's extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Html,
    Jsonl,
}

/// The supported extensions, as shown in refusals.
pub const SUPPORTED: &str = ".html, .jsonl";

impl Format {
    /// By extension, case-insensitive; `None` for anything else.
    pub fn of(path: &Path) -> Option<Format> {
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "html" => Some(Format::Html),
            "jsonl" => Some(Format::Jsonl),
            _ => None,
        }
    }
}

/// The text of the last assistant answer: the newest `message` entry whose
/// role is `assistant` and whose text is non-empty. `None` when the session
/// has none yet.
pub fn last_answer(entries: &[Entry]) -> Option<String> {
    entries.iter().rev().find_map(|e| {
        if e.kind.as_str() != EntryType::MESSAGE || e.payload["role"] != "assistant" {
            return None;
        }
        e.payload["text"]
            .as_str()
            .filter(|t| !t.trim().is_empty())
            .map(str::to_string)
    })
}

/// The entries exactly as stored: one JSON object per line, the same
/// serialization the serve `entries` route returns.
pub fn render_jsonl(entries: &[Entry]) -> Result<String, String> {
    let mut out = String::new();
    for e in entries {
        out.push_str(&serde_json::to_string(e).map_err(|e| format!("serializing entry: {e}"))?);
        out.push('\n');
    }
    Ok(out)
}

/// Escape text for an HTML text or attribute position.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

/// A JSON value as page text: strings verbatim, everything else pretty JSON.
fn text_of(v: &Value) -> String {
    v.as_str().map(str::to_string).unwrap_or_else(|| pretty(v))
}

/// `(css class, label, body)` of one entry. Classes come from a fixed set,
/// never from log text.
fn describe(e: &Entry) -> (&'static str, String, String) {
    let p = &e.payload;
    match e.kind.as_str() {
        EntryType::MESSAGE => {
            let role = p["role"].as_str().unwrap_or("message").to_string();
            let mut body = match p.get("text") {
                Some(t) => text_of(t),
                None => pretty(p),
            };
            if let Some(images) = p["images"].as_array() {
                for i in images {
                    body.push_str(&format!("\n[image: {}]", text_of(&i["path"])));
                }
            }
            let class = match role.as_str() {
                "user" => "user",
                "assistant" => "assistant",
                _ => "other",
            };
            (class, role, body)
        }
        EntryType::INTENT => (
            "tool",
            format!("tool call: {}", text_of(&p["tool"])),
            pretty(&p["input"]),
        ),
        EntryType::TOOL_RESULT => ("tool", "tool result".into(), text_of(&p["output"])),
        EntryType::ERROR => ("error", "error".into(), text_of(&p["error"])),
        other => ("other", other.to_string(), pretty(p)),
    }
}

/// A self-contained static page: no script, no external resource, every text
/// escaped. Light and dark through `prefers-color-scheme`.
pub fn render_html(session_id: &str, entries: &[Entry]) -> String {
    let mut out = String::new();
    out.push_str(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"color-scheme\" content=\"light dark\">\n",
    );
    out.push_str(&format!(
        "<title>tole session {}</title>\n",
        escape_html(session_id)
    ));
    out.push_str(STYLE);
    out.push_str("</head>\n<body>\n<main>\n");
    out.push_str(&format!(
        "<h1>tole session {}</h1>\n<p class=\"note\">{} entries. Tool output is included as recorded and may contain secrets.</p>\n",
        escape_html(session_id),
        entries.len()
    ));
    for e in entries {
        let (class, label, body) = describe(e);
        out.push_str(&format!(
            "<section class=\"entry {class}\">\n<h2>#{} {}</h2>\n<pre>{}</pre>\n</section>\n",
            e.seq,
            escape_html(&label),
            escape_html(&body)
        ));
    }
    out.push_str("</main>\n</body>\n</html>\n");
    out
}

const STYLE: &str = "<style>\n\
:root{color-scheme:light dark;--bg:#fff;--fg:#1c1c1e;--mute:#6b6b70;--line:#d8d8de;--user:#eef3ff;--asst:#f6f6f8;--tool:#fffbea;--err:#ffecec}\n\
@media (prefers-color-scheme:dark){:root{--bg:#161618;--fg:#e6e6ea;--mute:#9a9aa2;--line:#35353b;--user:#1b2433;--asst:#1f1f23;--tool:#262417;--err:#321c1c}}\n\
body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.5 system-ui,sans-serif}\n\
main{max-width:52rem;margin:0 auto;padding:1rem 16px 3rem}\n\
h1{font-size:1.25rem}.note{color:var(--mute)}\n\
.entry{border:1px solid var(--line);border-radius:6px;margin:.75rem 0;padding:.25rem .75rem;background:var(--asst)}\n\
.user{background:var(--user)}.tool{background:var(--tool)}.error{background:var(--err)}\n\
h2{font-size:.8rem;margin:.4rem 0;color:var(--mute);font-weight:600}\n\
pre{margin:.25rem 0 .6rem;white-space:pre-wrap;overflow-wrap:anywhere;font:13px/1.45 ui-monospace,monospace}\n\
</style>\n";

/// Create the (new) file `leaf` after `dirs` under `root`, one level at a
/// time, refusing a level that resolves outside `root` (a `.tole` symlink
/// pointing away must not make `create_dir` build directories elsewhere).
fn ensure_dirs(root: &Path, dirs: &[&str]) -> Result<PathBuf, String> {
    let mut cur = root.to_path_buf();
    for d in dirs {
        cur.push(d);
        match std::fs::symlink_metadata(&cur) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&cur).map_err(|e| format!("{}: {e}", cur.display()))?;
            }
            Err(e) => return Err(format!("{}: {e}", cur.display())),
        }
        let canon = cur
            .canonicalize()
            .map_err(|e| format!("{}: {e}", cur.display()))?;
        if !canon.starts_with(root) || !canon.is_dir() {
            return Err(format!(
                "{}: outside the workspace jail or not a directory — refused",
                cur.display()
            ));
        }
        cur = canon;
    }
    Ok(cur)
}

/// Resolve the output file: the format, and the jailed, not-yet-existing
/// target. `requested` is the user's path (absolute, or relative to the
/// workspace); `None` is `<workspace>/.tole/exports/<session id>.html`.
fn resolve_target(
    workspace: &Path,
    session_id: &str,
    requested: Option<&str>,
) -> Result<(PathBuf, Format), String> {
    let root = workspace
        .canonicalize()
        .map_err(|e| format!("workspace {}: {e}", workspace.display()))?;
    let Some(req) = requested.map(str::trim).filter(|r| !r.is_empty()) else {
        let dir = ensure_dirs(&root, &[".tole", "exports"])?;
        return Ok((dir.join(format!("{session_id}.html")), Format::Html));
    };
    let raw = Path::new(req);
    let format = Format::of(raw).ok_or_else(|| {
        format!("{req}: unsupported extension (supported: {SUPPORTED}); nothing written")
    })?;
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        root.join(raw)
    };
    let name = match joined.components().next_back() {
        Some(Component::Normal(n)) => n.to_os_string(),
        _ => return Err(format!("{req}: not a file path")),
    };
    // The parent must already exist (only the default directory is created);
    // canonicalizing it resolves `..` and symlinks before the jail check.
    let parent = joined.parent().unwrap_or(&root);
    let canon_parent = parent
        .canonicalize()
        .map_err(|e| format!("{req}: {}: {e}", parent.display()))?;
    if !canon_parent.starts_with(&root) {
        return Err(format!("{req}: outside the workspace jail — refused"));
    }
    Ok((canon_parent.join(name), format))
}

/// Write the transcript of `entries` and return the path written.
///
/// Refuses (writing nothing) for an unsupported extension, a path outside the
/// workspace, a missing directory, or any existing file or symlink at the
/// target; the existence check is the `create_new` open itself, so there is
/// no check-then-write race.
pub fn export(
    entries: &[Entry],
    session_id: &str,
    workspace: &Path,
    requested: Option<&str>,
) -> Result<PathBuf, String> {
    let (target, format) = resolve_target(workspace, session_id, requested)?;
    let body = match format {
        Format::Html => render_html(session_id, entries),
        Format::Jsonl => render_jsonl(entries)?,
    };
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut file = opts.open(&target).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            format!(
                "{} already exists; pick another path (an existing file is never overwritten)",
                target.display()
            )
        } else {
            format!("{}: {e}", target.display())
        }
    })?;
    if let Err(e) = file
        .write_all(body.as_bytes())
        .and_then(|()| file.sync_all())
    {
        drop(file);
        let _ = std::fs::remove_file(&target); // our own half-written file
        return Err(format!("{}: {e}", target.display()));
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(seq: u64, kind: &str, payload: Value) -> Entry {
        Entry {
            seq,
            id: format!("e_{seq}"),
            parent_id: None,
            kind: EntryType::from_string(kind),
            timestamp: 1_700_000_000_000 + seq,
            payload,
        }
    }

    fn sample() -> Vec<Entry> {
        vec![
            entry(
                1,
                "message",
                json!({"role":"user","text":"hi <b>there</b>"}),
            ),
            entry(
                2,
                "intent",
                json!({"tool":"read_file","input":{"path":"a.txt"}}),
            ),
            entry(3, "tool_result", json!({"ok":true,"output":"line1\nline2"})),
            entry(4, "error", json!({"ok":false,"error":"boom & bust"})),
            entry(
                5,
                "message",
                json!({"role":"assistant","text":"first answer"}),
            ),
            entry(6, "message", json!({"role":"user","text":"more"})),
            entry(
                7,
                "message",
                json!({"role":"assistant","text":"the last answer"}),
            ),
            entry(8, "state", json!({"pc":"Final"})),
        ]
    }

    fn ws(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!("tole-export-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    #[test]
    fn last_answer_is_the_newest_nonempty_assistant_message() {
        assert_eq!(last_answer(&sample()).as_deref(), Some("the last answer"));
        assert_eq!(last_answer(&[]), None);
        let only_user = vec![entry(1, "message", json!({"role":"user","text":"q"}))];
        assert_eq!(last_answer(&only_user), None);
        // An empty assistant message (a tool-only step) is skipped.
        let mut v = sample();
        v.push(entry(9, "message", json!({"role":"assistant","text":"  "})));
        assert_eq!(last_answer(&v).as_deref(), Some("the last answer"));
        // A tool result that merely mentions the role is not an answer.
        let tr = vec![entry(
            1,
            "tool_result",
            json!({"role":"assistant","text":"x"}),
        )];
        assert_eq!(last_answer(&tr), None);
    }

    #[test]
    fn html_escapes_everything_and_carries_no_script() {
        let evil = "<script>alert(1)</script> \"onload=\"x\" '&'";
        let entries = vec![
            entry(1, "message", json!({"role":"user","text":evil})),
            entry(2, "message", json!({"role":"assistant","text":evil})),
            entry(3, "intent", json!({"tool":evil,"input":{"k":evil}})),
            entry(4, "tool_result", json!({"ok":true,"output":evil})),
            entry(5, "error", json!({"ok":false,"error":evil})),
            entry(6, evil, json!({"x":evil})),
            entry(
                7,
                "message",
                json!({"role":evil,"text":"t","images":[{"path":evil}]}),
            ),
        ];
        let page = render_html(evil, &entries);
        assert!(
            page.contains(
                "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'\">"
            ),
            "CSP meta"
        );
        assert!(!page.to_lowercase().contains("<script"), "no script tag");
        assert!(!page.contains("<b>"));
        assert!(
            !page.contains("onload=\"x"),
            "attribute-breaking quote escaped"
        );
        assert!(page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(page.contains("&quot;onload=&quot;x&quot;"));
        // Nothing external: no src/href/url(), no @import.
        for bad in [" src=", "href=", "url(", "@import", "<link", "<iframe"] {
            assert!(!page.contains(bad), "{bad}");
        }
        assert!(page.contains("prefers-color-scheme:dark"));
        // The only tags are the fixed ones: count '<' that are not entities.
        let opened = page.matches('<').count();
        let closed = page.matches('>').count();
        assert_eq!(opened, closed);
    }

    #[test]
    fn html_shows_the_whole_transcript() {
        let page = render_html("s-1", &sample());
        for want in [
            "tole session s-1",
            "hi &lt;b&gt;there&lt;/b&gt;",
            "tool call: read_file",
            "line1\nline2",
            "boom &amp; bust",
            "the last answer",
            "#8 state",
        ] {
            assert!(page.contains(want), "{want}");
        }
    }

    #[test]
    fn jsonl_is_the_entries_exactly_as_stored() {
        let entries = sample();
        let text = render_jsonl(&entries).unwrap();
        assert!(text.ends_with('\n'));
        let back: Vec<Entry> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(back, entries);
        assert_eq!(text.lines().count(), entries.len());
    }

    #[test]
    fn export_writes_default_and_explicit_paths_inside_the_workspace() {
        let w = ws("ok");
        let p = export(&sample(), "s-1", &w, None).unwrap();
        assert_eq!(p, w.join(".tole/exports/s-1.html"));
        assert!(std::fs::read_to_string(&p)
            .unwrap()
            .contains("<!doctype html>"));
        let j = export(&sample(), "s-1", &w, Some("out.JSONL")).unwrap();
        assert_eq!(j, w.join("out.JSONL"));
        assert_eq!(
            std::fs::read_to_string(&j).unwrap(),
            render_jsonl(&sample()).unwrap()
        );
        let abs = w.join("abs.html");
        let a = export(&sample(), "s-1", &w, Some(abs.to_str().unwrap())).unwrap();
        assert_eq!(a, abs);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn an_existing_file_is_never_overwritten() {
        let w = ws("exists");
        std::fs::write(w.join("keep.html"), "precious").unwrap();
        let e = export(&sample(), "s-1", &w, Some("keep.html")).unwrap_err();
        assert!(
            e.contains("already exists") && e.contains("pick another path"),
            "{e}"
        );
        assert_eq!(
            std::fs::read_to_string(w.join("keep.html")).unwrap(),
            "precious"
        );
        // The default path too, on the second run.
        export(&sample(), "s-1", &w, None).unwrap();
        let e = export(&sample(), "s-1", &w, None).unwrap_err();
        assert!(e.contains("already exists"), "{e}");
    }

    #[test]
    fn paths_outside_the_workspace_are_refused() {
        let w = ws("jail");
        let outside = ws("jail-out");
        for bad in [
            "../x.html".to_string(),
            "sub/../../x.html".to_string(),
            outside.join("x.html").to_string_lossy().into_owned(),
            "/etc/x.html".to_string(),
        ] {
            let e = export(&sample(), "s-1", &w, Some(&bad)).unwrap_err();
            assert!(
                e.contains("outside the workspace jail") || e.contains("No such file"),
                "{bad}: {e}"
            );
        }
        assert!(!outside.join("x.html").exists());
        assert!(!w.parent().unwrap().join("x.html").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_lead_the_export_out_or_be_followed() {
        use std::os::unix::fs::symlink;
        let w = ws("sym");
        let outside = ws("sym-out");
        // A directory symlink pointing away.
        symlink(&outside, w.join("link")).unwrap();
        let e = export(&sample(), "s-1", &w, Some("link/x.html")).unwrap_err();
        assert!(e.contains("outside the workspace jail"), "{e}");
        // A file symlink at the target, to an outside path and a dangling one.
        symlink(outside.join("victim.html"), w.join("f.html")).unwrap();
        let e = export(&sample(), "s-1", &w, Some("f.html")).unwrap_err();
        assert!(e.contains("already exists"), "{e}");
        assert!(
            !outside.join("victim.html").exists(),
            "symlink not followed"
        );
        std::fs::write(outside.join("real.html"), "real").unwrap();
        symlink(outside.join("real.html"), w.join("g.html")).unwrap();
        assert!(export(&sample(), "s-1", &w, Some("g.html")).is_err());
        assert_eq!(
            std::fs::read_to_string(outside.join("real.html")).unwrap(),
            "real"
        );
        // `.tole` itself a symlink out: the default path must not build
        // directories elsewhere.
        let w2 = ws("sym2");
        symlink(&outside, w2.join(".tole")).unwrap();
        let e = export(&sample(), "s-1", &w2, None).unwrap_err();
        assert!(e.contains("outside the workspace jail"), "{e}");
        assert!(!outside.join("exports").exists());
    }

    #[test]
    fn unknown_extensions_and_missing_dirs_are_refused() {
        let w = ws("ext");
        for bad in ["out.txt", "out", "out.md", "out.html.exe"] {
            let e = export(&sample(), "s-1", &w, Some(bad)).unwrap_err();
            assert!(
                e.contains("unsupported extension") && e.contains(".html, .jsonl"),
                "{e}"
            );
            assert!(!w.join(bad).exists());
        }
        let e = export(&sample(), "s-1", &w, Some("nodir/x.html")).unwrap_err();
        assert!(e.contains("nodir"), "{e}");
        assert!(!w.join("nodir").exists(), "only the default dir is created");
        assert!(export(&sample(), "s-1", &w, Some("..")).is_err());
    }
}
