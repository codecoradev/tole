//! Description scanner for third-party MCP servers (#600, design
//! `third-party-plugins-design.md` §4.1 step 4).
//!
//! A tool's name, description and input schema are model-visible text, and
//! a hostile server uses them to steer the model (tool poisoning, shadowing
//! another server's tools, reaching for credentials). [`scan`] looks for the
//! signs of that in everything the model will read:
//!
//! - hidden-instruction markers (`<IMPORTANT>`, "do not tell the user",
//!   chat-template tokens, instruction-override phrases);
//! - mentions of other servers or tools (shadowing);
//! - sensitive paths (`~/.ssh`, `.env`, `mcp.json`, cloud credentials);
//! - invisible and bidi Unicode (also used to hide a phrase from the
//!   matching below, so matching ignores them);
//! - secret shapes, and the value of any secret the host holds;
//! - over-long descriptions (room to hide a payload).
//!
//! Pure functions: no I/O, no network. The result is a **report of
//! findings, never a "safe" label**: no finding does not mean the text is
//! benign, it means none of these patterns matched. A [`Finding`] carries
//! only text this module wrote (a fixed phrase from its own lists, a count,
//! code points), never a quotation of the server's text, so showing the
//! report cannot carry the payload to the reader.

use serde_json::Value;

use crate::plugin_lock::ManifestTool;
use crate::secrets::SecretSet;
use crate::skills::draft::{collapse, is_hidden, secret_shape};

/// Characters of one description (or one schema string) past which it is
/// reported as over-long. Real tool descriptions are a few sentences.
pub const MAX_DESCRIPTION_CHARS: usize = 1500;

/// What a finding is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    HiddenInstruction,
    OtherToolOrServer,
    SensitivePath,
    InvisibleChars,
    SecretShape,
    OverLong,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::HiddenInstruction => "hidden instruction",
            Kind::OtherToolOrServer => "mentions another tool or server",
            Kind::SensitivePath => "sensitive path",
            Kind::InvisibleChars => "invisible characters",
            Kind::SecretShape => "secret",
            Kind::OverLong => "over-long text",
        }
    }
}

/// One thing the scan noticed, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The tool (its name as listed; the name itself is scanned too).
    pub tool: String,
    /// `name`, `description` or `schema <path>`.
    pub field: String,
    pub kind: Kind,
    /// Written by the scanner (see the module docs).
    pub detail: String,
}

impl Finding {
    /// `tool `x` (description): hidden instruction: ...`
    pub fn line(&self) -> String {
        format!(
            "tool `{}` ({}): {}: {}",
            self.tool,
            self.field,
            self.kind.label(),
            self.detail
        )
    }
}

/// The other servers and tools the model sees next to the one scanned
/// (their names, for the shadowing check).
#[derive(Debug, Clone, Default)]
pub struct Others {
    pub servers: Vec<String>,
    pub tools: Vec<String>,
}

const HIDDEN_PHRASES: &[&str] = &[
    "<important>",
    "</important>",
    "<system>",
    "</system>",
    "[system]",
    "<instructions>",
    "<secret>",
    "<hidden>",
    "<|im_start|>",
    "<|system|>",
    "<|assistant|>",
    "<|endoftext|>",
    "[inst]",
    "ignore previous",
    "ignore all previous",
    "ignore prior",
    "ignore the above",
    "ignore your instructions",
    "ignore any instructions",
    "disregard previous",
    "disregard the above",
    "disregard your",
    "forget your instructions",
    "forget previous",
    "override your instructions",
    "new instructions:",
    "system prompt",
    "do not tell",
    "don't tell",
    "do not inform",
    "don't inform",
    "do not mention",
    "don't mention",
    "do not reveal",
    "don't reveal",
    "do not show this",
    "don't show this",
    "do not let the user",
    "without telling",
    "without informing",
    "without notifying",
    "without the user knowing",
    "without the user's knowledge",
    "the user must not",
    "the user should not see",
    "hide this from",
    "secretly",
    "you must first",
    "first you must",
    "before using this tool, you must",
    "before using this tool you must",
    "before calling this tool, you must",
    "note to the assistant",
    "note to ai",
    "attention ai",
    "attention assistant",
];

/// Generic references to other tools or servers.
const OTHER_PHRASES: &[&str] = &[
    "other tool",
    "another tool",
    "other mcp",
    "another mcp",
    "other server",
    "another server",
    "all other tools",
    "any other tool",
    "every other tool",
];

/// Path fragments the model must not be steered to. Matched on the
/// flattened lowercase text; `.env` and `.ssh` also need a boundary on the
/// left (`process.env` is JavaScript, not a file).
const SENSITIVE: &[&str] = &[
    ".ssh",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
    ".aws",
    ".env",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    ".gnupg",
    ".kube/",
    "kubeconfig",
    "mcp.json",
    "claude_desktop_config",
    "/etc/passwd",
    "/etc/shadow",
    ".bash_history",
    ".zsh_history",
    ".docker/config",
    "credentials.json",
    "wallet.dat",
    ".codecora",
    "plugins.lock",
    "cookies.sqlite",
];

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `needle` occurs in `hay` with no word character right before it, and
/// (when `right`) none right after it.
fn find_bounded(hay: &str, needle: &str, right: bool) -> bool {
    hay.match_indices(needle).any(|(i, _)| {
        let left_ok = hay[..i].chars().next_back().is_none_or(|c| !is_word(c));
        let right_ok = !right
            || hay[i + needle.len()..]
                .chars()
                .next()
                .is_none_or(|c| !is_word(c));
        left_ok && right_ok
    })
}

/// Whether `flat` names `name` the way a description steers the model to
/// another tool or server: quoted, called, or followed or preceded by
/// "tool" or "server". A bare word (a tool called `search` next to the
/// sentence "search the index") is not a mention.
fn mentions(flat: &str, name: &str) -> bool {
    let n = name.to_lowercase();
    if n.chars().count() < 3 {
        return false;
    }
    [
        format!("`{n}`"),
        format!("\"{n}\""),
        format!("'{n}'"),
        format!("{n}()"),
    ]
    .iter()
    .any(|p| flat.contains(p))
        || [
            format!("{n} tool"),
            format!("{n} server"),
            format!("{n} mcp"),
        ]
        .iter()
        .any(|p| find_bounded(flat, p, false))
        || [format!("tool {n}"), format!("server {n}")]
            .iter()
            .any(|p| find_bounded(flat, p, true))
        || flat.contains(&format!("mcp_{n}_"))
        || flat.contains(&format!("mcp__{n}__"))
}

fn code_points(text: &str) -> String {
    let mut seen: Vec<char> = Vec::new();
    for c in text.chars().filter(|c| invisible(*c)) {
        if !seen.contains(&c) {
            seen.push(c);
        }
    }
    seen.iter()
        .take(5)
        .map(|c| format!("U+{:04X}", *c as u32))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A server-chosen identifier (tool name, schema key) made safe to print:
/// letters, digits and a few separators only, capped.
fn ident(s: &str) -> String {
    s.chars()
        .take(80)
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '[' | ']' | '(' | ')' | ' ') {
                c
            } else {
                '?'
            }
        })
        .collect()
}

fn invisible(c: char) -> bool {
    is_hidden(c) && !matches!(c, '\n' | '\r' | '\t')
}

/// What one string (a name, a description, a schema value) shows. The
/// secrets the host holds are matched on the raw text.
pub fn scan_text(text: &str, others: &Others, secrets: &SecretSet) -> Vec<(Kind, String)> {
    let mut out: Vec<(Kind, String)> = Vec::new();
    // Hidden characters removed, lowercase, whitespace collapsed: a phrase
    // split by a soft hyphen or a line break still matches.
    let flat = collapse(text);
    for p in HIDDEN_PHRASES {
        if flat.contains(p) {
            out.push((Kind::HiddenInstruction, format!("contains {p:?}")));
        }
    }
    for p in OTHER_PHRASES {
        if flat.contains(p) {
            out.push((Kind::OtherToolOrServer, format!("contains {p:?}")));
        }
    }
    for s in &others.servers {
        if mentions(&flat, s) {
            out.push((
                Kind::OtherToolOrServer,
                format!("names the other server `{s}`"),
            ));
        }
    }
    for t in &others.tools {
        if mentions(&flat, t) {
            out.push((
                Kind::OtherToolOrServer,
                format!("names the other tool `{t}`"),
            ));
        }
    }
    let namespaced = flat.match_indices("mcp_").any(|(i, _)| {
        let tail: String = flat[i + 4..].chars().take_while(|c| is_word(*c)).collect();
        tail.contains('_') && flat[..i].chars().next_back().is_none_or(|c| !is_word(c))
    });
    if namespaced {
        out.push((
            Kind::OtherToolOrServer,
            "refers to a namespaced tool (`mcp_<server>_<tool>`)".into(),
        ));
    }
    for p in SENSITIVE {
        let right = matches!(*p, ".env" | ".ssh" | ".aws");
        let hit = if p.starts_with('.') || *p == "id_rsa" {
            find_bounded(&flat, p, right)
        } else {
            flat.contains(p)
        };
        if hit {
            out.push((Kind::SensitivePath, format!("names {p:?}")));
        }
    }
    let hidden = text.chars().filter(|c| invisible(*c)).count();
    if hidden > 0 {
        out.push((
            Kind::InvisibleChars,
            format!(
                "{hidden} invisible, bidi or control character(s): {}",
                code_points(text)
            ),
        ));
    }
    if let Some(what) = secret_shape(text) {
        out.push((Kind::SecretShape, format!("looks like {what}")));
    }
    if !secrets.is_empty() && secrets.redact_known(text) != text {
        out.push((
            Kind::SecretShape,
            "holds the value of a secret you hold".into(),
        ));
    }
    if text.chars().count() > MAX_DESCRIPTION_CHARS {
        out.push((
            Kind::OverLong,
            format!(
                "{} characters (more than {MAX_DESCRIPTION_CHARS})",
                text.chars().count()
            ),
        ));
    }
    out.dedup();
    out
}

/// Every string of a JSON value with its path, keys included (a property
/// name is model-visible too).
fn strings(v: &Value, path: &str, out: &mut Vec<(String, String)>) {
    match v {
        Value::String(s) => out.push((path.to_string(), s.clone())),
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                strings(x, &format!("{path}[{i}]"), out);
            }
        }
        Value::Object(m) => {
            for (k, x) in m {
                let p = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                out.push((format!("{p} (key)"), k.clone()));
                strings(x, &p, out);
            }
        }
        _ => {}
    }
}

/// Scans every tool's name, description and input schema. `others` are the
/// other servers and tools in the same session. Findings come in tool
/// order, each tool's fields in order.
pub fn scan(tools: &[ManifestTool], others: &Others, secrets: &SecretSet) -> Vec<Finding> {
    let mut out = Vec::new();
    for t in tools {
        let mut fields: Vec<(String, String)> = vec![
            ("name".into(), t.name.clone()),
            ("description".into(), t.description.clone()),
        ];
        let mut schema = Vec::new();
        strings(&t.input_schema, "", &mut schema);
        fields.extend(schema.into_iter().map(|(p, s)| (format!("schema {p}"), s)));
        for (field, text) in fields {
            for (kind, detail) in scan_text(&text, others, secrets) {
                let f = Finding {
                    tool: ident(&t.name),
                    field: ident(&field),
                    kind,
                    detail,
                };
                if !out.contains(&f) {
                    out.push(f);
                }
            }
        }
    }
    out
}

/// The report printed to the owner: the findings, or the sentence that
/// says nothing matched (and that this is not a safety verdict).
pub fn report(findings: &[Finding]) -> String {
    if findings.is_empty() {
        return "scan: no known pattern matched. This is not a safety verdict; read the full \
                descriptions above.\n"
            .into();
    }
    let mut out = format!(
        "scan: {} finding(s). A finding is a pattern, not proof; read the descriptions above.\n",
        findings.len()
    );
    for f in findings {
        out.push_str(&format!("  ! {}\n", f.line()));
    }
    out
}
