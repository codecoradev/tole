//! Harness-level uteke memory loop: pre-turn recall + post-session
//! remember, both host-initiated.
//!
//! These are NOT model tools — the model cannot trigger or suppress
//! them. They are owner-configured behavior (the CLI `--memory uteke`
//! flag / `TOLE_MEMORY` env), the harness equivalent of Hermes' Mode C:
//!
//! - **Pre-turn recall**: on the first turn of a fresh session, memories
//!   relevant to the user's prompt are fetched from the owner's uteke
//!   store and injected into the user message inside a clearly marked,
//!   fenced block. The durable log stores exactly what the provider saw.
//! - **Post-session remember**: when a session settles with at least one
//!   completed turn, a compact summary lands in the namespace — the
//!   next session's recall can find it. Cross-session continuity is the
//!   whole point: the memory loop, not the model, makes tole durable.
//!
//! Failures degrade loudly-on-stderr and never break a turn: memory is
//! an enhancement, not a dependency. All subprocess discipline mirrors
//! `uteke.rs` (scrubbed env, hard timeout, argv-only — no shell).

use crate::subprocess::{run_with_timeout, SUBPROCESS_TIMEOUT};
use serde_json::Value;
use std::process::Command;

/// Memory-loop configuration. `bin` is a single path element on PATH or
/// an absolute path; `namespace` follows the ecosystem `repo-<name>`
/// convention.
#[derive(Debug, Clone)]
pub struct MemoryConfig {
    pub bin: String,
    pub namespace: String,
    /// Max recall hits injected per fresh session.
    pub limit: u32,
}

impl MemoryConfig {
    /// Config for `bin` with the ecosystem namespace convention:
    /// `repo-<directory name>` (e.g. a checkout at `…/riset-ai/tole`
    /// recalls from `repo-tole`). Names without a basename fall back to
    /// `repo-default`.
    pub fn for_cwd(bin: impl Into<String>, cwd: &std::path::Path) -> Self {
        let dir = cwd
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "default".to_string());
        Self {
            bin: bin.into(),
            namespace: format!("repo-{}", sanitize_namespace(&dir)),
            limit: 3,
        }
    }
}

/// Namespace charset for argv safety: `[a-zA-Z0-9._-]`, never starting
/// with a hyphen (a leading `-` would parse as a CLI flag). Offending
/// characters collapse to `_`. Public so hosts that override the
/// namespace (e.g. via env) apply the same rule.
pub fn sanitize_namespace(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.starts_with('-') {
        format!("_{cleaned}")
    } else {
        cleaned
    }
}

/// One recalled hit, already trimmed for injection.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallHit {
    pub score: f64,
    /// Truncated to [`MAX_HIT_CHARS`].
    pub content: String,
}

const MAX_HIT_CHARS: usize = 400;

/// Semantic recall over the namespace. Returns hits best-first; an empty
/// store yields an empty vec (not an error).
pub fn recall(cfg: &MemoryConfig, query: &str) -> Result<Vec<RecallHit>, String> {
    let mut cmd = Command::new(&cfg.bin);
    crate::subprocess::scrub_env_for_child(&mut cmd);
    cmd.arg("recall")
        .arg(query)
        .arg("--namespace")
        .arg(&cfg.namespace)
        .arg("--limit")
        .arg(cfg.limit.to_string())
        .arg("--json");
    let out = run_with_timeout(&mut cmd, SUBPROCESS_TIMEOUT)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let snippet: String = stderr.chars().take(300).collect();
        return Err(format!("uteke recall exited {}: {}", out.status, snippet));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: Value = serde_json::from_str(stdout.trim())
        .map_err(|e| format!("uteke recall output was not valid JSON: {e}"))?;
    let Some(arr) = parsed.as_array() else {
        return Ok(Vec::new());
    };
    Ok(arr
        .iter()
        .filter_map(|hit| {
            let content = hit.get("content").and_then(Value::as_str)?;
            if content.trim().is_empty() {
                return None;
            }
            Some(RecallHit {
                score: hit.get("score").and_then(Value::as_f64).unwrap_or(0.0),
                content: content.to_string(),
            })
        })
        .collect())
}

/// The fenced block appended to the first user message. The markers are
/// explicit about origin and extent so the provider (and anyone reading
/// the durable log) can tell injected memory from the user's own words.
/// Content is truncated HERE — the single choke point before the prompt —
/// so every path into this function is bounded.
pub fn format_recall_block(cfg: &MemoryConfig, hits: &[RecallHit]) -> String {
    if hits.is_empty() {
        return String::new();
    }
    let mut block = format!(
        "\n\n---\n[recalled memory · uteke namespace {} — injected by the host before this turn]\n",
        cfg.namespace
    );
    for (i, hit) in hits.iter().enumerate() {
        let content: String = hit.content.chars().take(MAX_HIT_CHARS).collect();
        block.push_str(&format!("{}. ({:.2}) {}\n", i + 1, hit.score, content));
    }
    block.push_str("[end recalled memory]");
    block
}

/// Convenience: recall + format in one call; `Ok(String::new())` when
/// nothing was found (callers append the empty string, a no-op).
pub fn recall_block(cfg: &MemoryConfig, query: &str) -> Result<String, String> {
    let hits = recall(cfg, query)?;
    Ok(format_recall_block(cfg, &hits))
}

/// Content cap for the remembered summary (the store is a knowledge
/// base, not a transcript dump — full logs stay in the JSONL session).
const MAX_REMEMBER_CHARS: usize = 2000;

/// Store the post-session summary. Returns the raw CLI output (id line)
/// on success.
pub fn remember_session(
    cfg: &MemoryConfig,
    session_id: &str,
    first_prompt: &str,
    last_answer: &str,
) -> Result<String, String> {
    fn cap(s: &str, max: usize) -> String {
        let t = s.trim();
        let one_line = t.replace('\n', " ");
        let mut out: String = one_line.chars().take(max).collect();
        if one_line.chars().count() > max {
            out.push('…');
        }
        out
    }
    let content = format!(
        "[tole session {}] {} → {}",
        session_id,
        cap(first_prompt, 300),
        cap(last_answer, 1200)
    );
    if content.chars().count() > MAX_REMEMBER_CHARS {
        // Belt-and-suspenders: never dump oversized payloads into the
        // store even if the caps above change.
        return Err("remember content exceeds cap".into());
    }
    let mut cmd = Command::new(&cfg.bin);
    crate::subprocess::scrub_env_for_child(&mut cmd);
    cmd.arg("remember")
        .arg(&content)
        .arg("--namespace")
        .arg(&cfg.namespace)
        .arg("--type")
        .arg("context")
        .arg("--tags")
        .arg("tole,session")
        .arg("--source")
        .arg("tole-harness")
        .arg("--source-type")
        .arg("system");
    let out = run_with_timeout(&mut cmd, SUBPROCESS_TIMEOUT)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let snippet: String = stderr.chars().take(300).collect();
        return Err(format!("uteke remember exited {}: {}", out.status, snippet));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_sanitized_for_argv_safety() {
        assert_eq!(sanitize_namespace("tole"), "tole");
        assert_eq!(sanitize_namespace("my repo.v2"), "my_repo.v2");
        assert_eq!(sanitize_namespace("-evil"), "_-evil");
        assert_eq!(sanitize_namespace("a/b c"), "a_b_c");
    }

    #[test]
    fn cwd_namespace_follows_repo_convention() {
        let dir = std::env::temp_dir().join("tole-memory-ns-test");
        let cfg = MemoryConfig::for_cwd("uteke", &dir);
        assert_eq!(cfg.namespace, "repo-tole-memory-ns-test");
        assert_eq!(cfg.limit, 3);
        // Root (no basename) falls back, never panics.
        let cfg_root = MemoryConfig::for_cwd("uteke", std::path::Path::new("/"));
        assert_eq!(cfg_root.namespace, "repo-default");
    }

    #[test]
    fn recall_block_empty_when_no_hits() {
        let cfg = MemoryConfig {
            bin: "uteke".into(),
            namespace: "repo-x".into(),
            limit: 3,
        };
        assert_eq!(format_recall_block(&cfg, &[]), "");
    }

    #[test]
    fn recall_block_is_fenced_and_ordered() {
        let cfg = MemoryConfig {
            bin: "uteke".into(),
            namespace: "repo-tole".into(),
            limit: 3,
        };
        let hits = vec![
            RecallHit {
                score: 0.91,
                content: "decision: state machine stays synchronous".into(),
            },
            RecallHit {
                score: 0.72,
                content: "namespace convention is repo-<name>".into(),
            },
        ];
        let block = format_recall_block(&cfg, &hits);
        assert!(block.contains("[recalled memory · uteke namespace repo-tole"));
        assert!(block.contains("1. (0.91) decision: state machine"));
        assert!(block.contains("2. (0.72) namespace convention"));
        assert!(block.ends_with("[end recalled memory]"));
    }

    #[test]
    fn recall_block_truncates_long_content() {
        let cfg = MemoryConfig {
            bin: "uteke".into(),
            namespace: "repo-tole".into(),
            limit: 1,
        };
        let hits = vec![RecallHit {
            score: 0.5,
            content: "x".repeat(MAX_HIT_CHARS + 50),
        }];
        let block = format_recall_block(&cfg, &hits);
        assert!(!block.contains(&"x".repeat(MAX_HIT_CHARS + 50)));
        assert!(block.contains(&"x".repeat(MAX_HIT_CHARS)));
    }

    /// Fake-binary e2e for recall: the script prints a plausible JSON
    /// payload; the parse path must produce hits. Also records argv so
    /// the exact CLI contract stays pinned.
    #[test]
    fn recall_via_fake_binary() {
        let dir = std::env::temp_dir().join(format!("tole-mem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("uteke");
        let log = dir.join("argv.log");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\necho \"$@\" > {}\necho '[{{\"score\":0.8,\"content\":\"remembered fact\",\"result_type\":\"memory\"}}]'\n",
                log.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let cfg = MemoryConfig {
            bin: bin.display().to_string(),
            namespace: "repo-tole".into(),
            limit: 2,
        };
        let hits = recall(&cfg, "state machine").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].content, "remembered fact");
        assert!((hits[0].score - 0.8).abs() < 1e-9);
        let argv = std::fs::read_to_string(&log).unwrap();
        assert!(argv.contains("recall state machine"));
        assert!(argv.contains("--namespace repo-tole"));
        assert!(argv.contains("--limit 2"));
        assert!(argv.contains("--json"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Fake-binary e2e for remember: the summary lands as ONE argv
    /// element (no shell), typed and tagged for the store.
    #[test]
    fn remember_via_fake_binary() {
        let dir = std::env::temp_dir().join(format!("tole-mem-w-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("uteke");
        let log = dir.join("argv.log");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\necho \"$@\" > {}\necho \"id: mem-123\"\n",
                log.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let cfg = MemoryConfig {
            bin: bin.display().to_string(),
            namespace: "repo-tole".into(),
            limit: 3,
        };
        let out = remember_session(&cfg, "s-abc", "fix the jail", "done — tests added").unwrap();
        assert!(out.contains("id: mem-123"));
        let argv = std::fs::read_to_string(&log).unwrap();
        assert!(argv.contains("remember [tole session s-abc] fix the jail → done — tests added"));
        assert!(argv.contains("--namespace repo-tole"));
        assert!(argv.contains("--type context"));
        assert!(argv.contains("--tags tole,session"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remember_newlines_collapsed_to_one_line() {
        let dir = std::env::temp_dir().join(format!("tole-mem-nl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("uteke");
        let log = dir.join("argv.log");
        std::fs::write(
            &bin,
            format!("#!/bin/sh\necho \"$@\" > {}\necho ok\n", log.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let cfg = MemoryConfig {
            bin: bin.display().to_string(),
            namespace: "repo-tole".into(),
            limit: 3,
        };
        remember_session(&cfg, "s-abc", "multi\nline\nprompt", "answer").unwrap();
        let argv = std::fs::read_to_string(&log).unwrap();
        assert!(!argv.contains('\n') || argv.lines().count() == 1);
        assert!(argv.contains("multi line prompt"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
