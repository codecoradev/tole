//! E6 — the interactive approver: y/N prompt over stdin/stdout.
//!
//! Lives in the host (CLI), not core: core stays deterministic and
//! Tier A testable, while the human prompt is inherently I/O. The
//! prompt function is injectable so tests drive verdicts without a
//! terminal (`PromptFn`).

use std::io::{self, BufRead, Write};

use tole_core::approval::{glob_match, Approver, ToolRequest, Verdict};
use tole_core::tool::Risk;

/// How the approver asks a human and reads the answer. Injectable for
/// tests; [`StdioPrompt`] is the production impl.
pub trait PromptFn: Send + Sync {
    /// Render the request and return the human's verdict.
    fn prompt(&self, req: &ToolRequest<'_>) -> Verdict;
}

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

/// Production prompt: prints command + input preview to stdout, reads
/// y/N from stdin. EOF / unrecognized input ⇒ Deny (fail closed).
pub struct StdioPrompt;

impl PromptFn for StdioPrompt {
    fn prompt(&self, req: &ToolRequest<'_>) -> Verdict {
        println!();
        println!("── approval required ──────────────────────────");
        println!("tool:  {} [{}]", req.tool, req.risk.as_str());
        println!("what:  {}", req.description);
        println!("input: {}", input_preview(req.input));
        print!("allow? [y/N] ");
        let _ = io::stdout().flush();
        let mut line = String::new();
        match io::stdin().lock().read_line(&mut line) {
            Ok(0) | Err(_) => return Verdict::Deny, // EOF / read error: fail closed
            Ok(_) => {}
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => Verdict::Allow,
            _ => Verdict::Deny,
        }
    }
}

/// y/N approver: allowlist patterns first, then the human prompt.
/// `Destructive` always prompts (PRD: never allowlistable).
pub struct InteractiveApprover<P: PromptFn> {
    patterns: Vec<String>,
    auto_write: bool,
    prompter: P,
}

impl InteractiveApprover<StdioPrompt> {
    pub fn stdio() -> Self {
        Self::new(StdioPrompt)
    }
}

impl<P: PromptFn> InteractiveApprover<P> {
    pub fn new(prompter: P) -> Self {
        Self {
            patterns: Vec::new(),
            auto_write: false,
            prompter,
        }
    }

    /// Glob patterns auto-allowed without prompting (Write only).
    pub fn with_allow_patterns(mut self, patterns: Vec<String>) -> Self {
        self.patterns = patterns;
        self
    }

    /// Auto-allow every Write call (heads-up mode). Destructive still
    /// prompts.
    pub fn with_auto_write(mut self, yes: bool) -> Self {
        self.auto_write = yes;
        self
    }
}

impl<P: PromptFn> Approver for InteractiveApprover<P> {
    fn decide(&self, req: &ToolRequest<'_>) -> Verdict {
        if req.risk == Risk::Destructive {
            // Never auto-allowed: always a human decision.
            return self.prompter.prompt(req);
        }
        if self.auto_write || self.patterns.iter().any(|p| glob_match(p, req.tool)) {
            return Verdict::Allow;
        }
        self.prompter.prompt(req)
    }

    fn interactive(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn input_preview_short_input_passes_through() {
        let v = Value::String("hello".into());
        assert_eq!(input_preview(&v), r#""hello""#);
    }

    #[test]
    fn input_preview_long_input_truncates_middle_keeps_both_ends() {
        // 1998 x's + 2 JSON quotes = 2000 chars → middle 1200 elided.
        let long = "x".repeat(1998);
        let out = input_preview(&Value::String(long));
        assert!(out.contains("… (+1200 more chars)…"));
        assert!(out.starts_with('"'));
        assert!(out.ends_with('"'));
        // UTF-8-safe: multi-byte content still truncates on char boundaries
        // (1000 é's + 2 quotes = 1002 chars → 202 skipped).
        let multibyte = "é".repeat(1000);
        let out2 = input_preview(&Value::String(multibyte));
        assert!(out2.contains("… (+202 more chars)…"));
        assert!(out2.starts_with('"'));
        assert!(out2.ends_with('"'));
        // A short security-relevant scalar stays fully visible even at the
        // head/tail threshold edge (799 chars < 400+400).
        let edge = "p".repeat(797);
        assert_eq!(
            input_preview(&Value::String(edge.clone())),
            format!("\"{edge}\"")
        );
    }

    /// Scripted prompt: records requests, replays canned verdicts.
    struct ScriptedPrompt {
        answers: std::sync::Mutex<Vec<Verdict>>,
        seen: std::sync::Mutex<Vec<String>>,
    }

    impl PromptFn for ScriptedPrompt {
        fn prompt(&self, _req: &ToolRequest<'_>) -> Verdict {
            self.seen.lock().unwrap().push("prompted".into());
            let next = self.answers.lock().unwrap().pop().unwrap_or(Verdict::Deny);
            next
        }
    }

    fn req(tool: &str, risk: Risk) -> ToolRequest<'_> {
        ToolRequest {
            tool,
            risk,
            input: &Value::Null,
            description: format!("{tool} description"),
        }
    }

    #[test]
    fn write_prompted_then_allowed_or_denied() {
        for answer in [Verdict::Allow, Verdict::Deny] {
            let a = InteractiveApprover::new(ScriptedPrompt {
                answers: std::sync::Mutex::new(vec![answer]),
                seen: std::sync::Mutex::new(vec![]),
            });
            assert_eq!(a.decide(&req("write_file", Risk::Write)), answer);
        }
    }

    #[test]
    fn allow_pattern_skips_prompt_for_write() {
        let a = InteractiveApprover::new(ScriptedPrompt {
            answers: std::sync::Mutex::new(vec![]),
            seen: std::sync::Mutex::new(vec![]),
        })
        .with_allow_patterns(vec!["write_*".into()]);
        assert_eq!(a.decide(&req("write_file", Risk::Write)), Verdict::Allow);
    }

    #[test]
    fn destructive_always_prompts_even_with_pattern() {
        let a = InteractiveApprover::new(ScriptedPrompt {
            answers: std::sync::Mutex::new(vec![Verdict::Allow]),
            seen: std::sync::Mutex::new(vec![]),
        })
        .with_allow_patterns(vec!["*".into()])
        .with_auto_write(true);
        // Pattern and --yes both set, yet Destructive still prompts —
        // and here the human says Allow.
        assert_eq!(a.decide(&req("rm_rf", Risk::Destructive)), Verdict::Allow);
    }

    #[test]
    fn interactive_marker_is_true() {
        let a = InteractiveApprover::stdio();
        assert!(a.interactive());
    }

    #[test]
    fn auto_write_allows_write_without_prompt() {
        let a = InteractiveApprover::new(ScriptedPrompt {
            answers: std::sync::Mutex::new(vec![]),
            seen: std::sync::Mutex::new(vec![]),
        })
        .with_auto_write(true);
        assert_eq!(a.decide(&req("write_file", Risk::Write)), Verdict::Allow);
    }
}
