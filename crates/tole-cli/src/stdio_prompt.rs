//! The plain-terminal approval prompt (`tole run`, `chat`, `resume`): the
//! action summary (#1041) first, then the tool's own text, the input and a y/N
//! question. Every value printed is neutralized to one safe line.

use std::io::{self, BufRead, Write};

use tole_core::approval::{ToolRequest, Verdict};
use tole_core::secrets::process_secrets;

use crate::approver::PromptFn;
use crate::config_trust::sanitize;

/// Preview of the tool input shown in the approval prompt. HEAD **and**
/// TAIL are kept (cora MAJOR, 2026-09-19): a head-only cut could hide a
/// dangerous path/command suffix beyond the preview from the human
/// approver, weakening the fail-closed gate. Short scalar fields (paths,
/// commands — the security-relevant values) stay fully visible; only
/// oversized blobs lose their middle. The full input always lives in the
/// durable session log; `tole status` is the audit surface.
const INPUT_PREVIEW_HEAD: usize = 400;
const INPUT_PREVIEW_TAIL: usize = 400;

fn input_preview(input: &serde_json::Value) -> String {
    let rendered = input.to_string();
    let total = rendered.chars().count();
    if total <= INPUT_PREVIEW_HEAD + INPUT_PREVIEW_TAIL {
        return rendered;
    }
    let head: String = rendered.chars().take(INPUT_PREVIEW_HEAD).collect();
    let tail: String = rendered.chars().skip(total - INPUT_PREVIEW_TAIL).collect();
    let skipped = total - INPUT_PREVIEW_HEAD - INPUT_PREVIEW_TAIL;
    format!("{head}… (+{skipped} more chars)…{tail}")
}

/// One printed value on one line: control, bidi and invisible characters
/// become visible `\u{hex}` escapes (a newline too).
fn one_line(text: &str) -> String {
    sanitize(text).replace('\n', "\\u{a}")
}

/// The prompt text. The `does:` line is the summary's sentence (what the call
/// does and its effect), built by code and redacted like the TUI's and
/// `serve`'s; a request without a summary shows only the tool's `what:` text,
/// as this prompt always did. The tier is the request's own.
pub fn render(req: &ToolRequest<'_>) -> String {
    let does = req.summary.as_ref().map_or(String::new(), |s| {
        format!(
            "does:  {}\n",
            one_line(&s.finalized(&process_secrets()).sentence())
        )
    });
    format!(
        "\n── approval required ──────────────────────────\ntool:  {} [{}]\n{does}what:  {}\ninput: {}\nallow? [y/N] ",
        one_line(req.tool),
        req.risk.as_str(),
        one_line(&req.description),
        one_line(&input_preview(req.input)),
    )
}

/// Ask over `out` / `input`: EOF, a read error or anything but `y` / `yes`
/// is Deny (fail closed).
pub fn ask(req: &ToolRequest<'_>, input: &mut impl BufRead, out: &mut impl Write) -> Verdict {
    // A failed prompt flush is not actionable.
    // cora-ignore: qual-error-ignore
    let _ = write!(out, "{}", render(req)).and_then(|()| out.flush());
    let mut line = String::new();
    match input.read_line(&mut line) {
        Ok(0) | Err(_) => return Verdict::Deny,
        Ok(_) => {}
    }
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Verdict::Allow,
        _ => Verdict::Deny,
    }
}

/// Production prompt: prints the request to stdout, reads y/N from stdin.
pub struct StdioPrompt;

impl PromptFn for StdioPrompt {
    fn prompt(&self, req: &ToolRequest<'_>) -> Verdict {
        ask(req, &mut io::stdin().lock(), &mut io::stdout())
    }
}
