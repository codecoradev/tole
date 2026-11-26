//! The session-file commands (`status`, `sessions`, `branch`,
//! `import-claude`) and the session id helpers.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tole_cli::session::assemble::civil_from_days;
use tole_core::storage::{JsonlStorage, Storage};

/// Session id → path (`<dir>/<id>.jsonl`).
pub(crate) fn session_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.jsonl"))
}

/// Session id validity — the one shared rule (`[A-Za-z0-9_-]`, 1..=64,
/// no traversal), identical to what serve/ACP/storage accept.
pub(crate) fn valid_session_id(id: &str) -> bool {
    tole_core::storage::is_valid_session_id(id)
}

pub(crate) fn status_command(sessions_dir: &Path, id: &str, window: Option<u64>) -> Result<()> {
    if !valid_session_id(id) {
        anyhow::bail!("invalid session id {id:?} (allowed: [a-z0-9-], max 64)");
    }
    let path = session_path(sessions_dir, id);
    if !path.exists() {
        anyhow::bail!("session {id} not found at {}", path.display());
    }
    let storage = JsonlStorage::open_read_only(&path).context("replaying session log")?;
    println!("session: {id}");
    println!("entries: {}", storage.entries().len());
    println!("pc:      {:?}", storage.state().pc);
    println!("seq:     {}", storage.state().seq);
    // B3: orientation fields — turns (user messages), usage totals, and
    // whether a system prompt is pinned.
    let turns = tole_core::transcript::turn_count(storage.entries());
    println!("turns:   {turns} (user messages)");
    for line in tole_cli::compaction_host::status_lines(storage.entries()) {
        println!("{line}");
    }
    println!(
        "system:  {}",
        match storage.system_prompt() {
            Some(_) => "pinned (see header)".to_string(),
            None => "-".to_string(),
        }
    );
    let usage = storage.usages();
    let prompt_tokens: u64 = usage
        .iter()
        .filter_map(|u| u.usage.get("prompt_tokens").and_then(|v| v.as_u64()))
        .sum();
    let completion_tokens: u64 = usage
        .iter()
        .filter_map(|u| u.usage.get("completion_tokens").and_then(|v| v.as_u64()))
        .sum();
    let cost: f64 = usage.iter().filter_map(|u| u.cost_usd).sum::<f64>();
    // `-0.0` renders as "-0.0000" (sum of no records is 0.0, but be
    // explicit: a negative-zero cost display would look like a bug).
    let cost = if cost == 0.0 { 0.0 } else { cost };
    println!("usage:   {prompt_tokens} in / {completion_tokens} out tokens, ${cost:.4} USD");
    // Mission cost report (issue #201): the durable fact/mission register
    // a settled mission leaves behind.
    if let Some(mission) = storage.get_register("fact", "mission") {
        println!("mission: {}", mission);
    }
    // Issue #211: usage report derived from the ledger (additive block,
    // rendered after the pre-existing lines; older sessions read `n/a`).
    for line in tole_cli::usage_report::usage_report(usage).render_lines() {
        println!("{line}");
    }
    // Issue #379: per user-turn stats from the same ledger.
    for line in tole_cli::usage_report::turn_lines(storage.entries(), usage) {
        println!("{line}");
    }
    // #458: model / effort / context / token lines from the one status
    // snapshot, appended last so the pre-existing lines keep their order.
    let cfg = tole_cli::config_apply::provider_config();
    let configured = cfg.as_ref().map(|c| c.model.as_str());
    let settings = tole_cli::compaction_host::CompactionSettings {
        window,
        ..Default::default()
    };
    let snap = match &cfg {
        Some(c) => {
            let model = tole_cli::turn_runner::session_model(&storage, &c.model);
            let mut src = tole_cli::compaction_host::ModelsCatalog::new(c, &model);
            tole_cli::status::session_status(&storage, configured, &settings, &mut src)
        }
        None => tole_cli::status::session_status(&storage, None, &settings, &mut NoWindow),
    };
    for line in snap.lines() {
        println!("{line}");
    }
    Ok(())
}

/// Window source for `tole status` without a provider config: never knows.
struct NoWindow;
impl tole_cli::compaction_host::WindowSource for NoWindow {
    fn window(&mut self) -> Option<u64> {
        None
    }
}

/// B3: list every session in the dir, newest first, one line each.
pub(crate) fn sessions_command(sessions_dir: &Path) -> Result<()> {
    if !sessions_dir.exists() {
        println!(
            "no sessions in {} (dir does not exist) — start one with: tole chat",
            sessions_dir.display()
        );
        return Ok(());
    }
    // (epoch_secs, id, pc, seq, turns) — epoch secs first so a plain
    // sort_by_key ascending gives newest-first via Reverse.
    // (epoch_secs, id, pc, seq, turns, name, profile) — epoch secs first so a
    // plain sort_by_key ascending gives newest-first via Reverse.
    // The last field is the recorded serve profile (#740), shown only when
    // some session has one.
    type Row = (
        u64,
        String,
        String,
        u64,
        usize,
        Option<String>,
        Option<String>,
    );
    let mut rows: Vec<Row> = Vec::new();
    for entry in std::fs::read_dir(sessions_dir)?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // strip_suffix, not trim_end_matches: a (weird but possible)
        // "x.jsonl.jsonl" would otherwise yield the unusable id "x.jsonl"
        // (CodeCora scan 2026-09-18).
        let Some(stem) = name.strip_suffix(".jsonl") else {
            continue;
        };
        if !valid_session_id(stem) {
            continue;
        }
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let id = stem.to_string();
        // Read-only peek: replay the file to surface pc + turn count.
        let s = match JsonlStorage::open_read_only(entry.path()) {
            Ok(s) => s,
            Err(_) => continue, // unreadable/corrupt: skip, don't fail the listing
        };
        let turns = tole_core::transcript::turn_count(s.entries());
        rows.push((
            mtime,
            id,
            format!("{:?}", s.state().pc),
            s.state().seq,
            turns,
            s.session_name().map(str::to_string),
            tole_cli::serve_support::profiles::recorded(&s)
                .label()
                .map(str::to_string),
        ));
    }
    if rows.is_empty() {
        println!("no sessions in {}", sessions_dir.display());
        return Ok(());
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.0)); // newest first
    let with_profile = rows.iter().any(|r| r.6.is_some());
    let head = |profile: &str| {
        println!(
            "{:<26} {:<18} {:<12} {:>5} {:>6}  {profile}mtime",
            "session", "name", "pc", "seq", "turns"
        )
    };
    head(if with_profile {
        "profile          "
    } else {
        ""
    });
    for row in &rows {
        let mtime = fmt_mtime(std::time::UNIX_EPOCH + std::time::Duration::from_secs(row.0));
        let name = row.5.clone().unwrap_or_else(|| "-".to_string());
        let profile = if with_profile {
            format!("{:<16} ", row.6.as_deref().unwrap_or("-"))
        } else {
            String::new()
        };
        println!(
            "{:<26} {:<18} {:<12} {:>5} {:>6}  {profile}{mtime}",
            row.1, name, row.2, row.3, row.4
        );
    }
    Ok(())
}

/// `tole branch` (#210): fork `id` at a settled entry into a new session.
pub(crate) fn branch_command(sessions_dir: &Path, id: &str, at: Option<&str>) -> Result<()> {
    if !valid_session_id(id) {
        anyhow::bail!("invalid session id {id:?} (allowed: [A-Za-z0-9_-], max 64)");
    }
    let src = session_path(sessions_dir, id);
    if !src.exists() {
        anyhow::bail!("session {id} not found at {}", src.display());
    }
    let new_id = new_session_id();
    let f = JsonlStorage::fork(&src, sessions_dir, &new_id, at).context("forking session")?;
    let cut = f.forked_from().map(|o| o.entry.clone()).unwrap_or_default();
    drop(f);
    tole_cli::session_index::record(sessions_dir, &new_id, "branch");
    println!("branched: {new_id}");
    println!("from:     {id} @ {cut}");
    println!("continue: tole resume {new_id} \"<prompt>\"");
    Ok(())
}

/// `tole import-claude` (#381): Claude Code transcript → new tole session.
pub(crate) fn import_claude_command(sessions_dir: &Path, path: &Path) -> Result<()> {
    std::fs::create_dir_all(sessions_dir)?;
    let r = tole_core::ccimport::import_file(path, sessions_dir).context("importing transcript")?;
    for w in &r.warnings {
        eprintln!("warning: {w}");
    }
    tole_cli::session_index::record(sessions_dir, &r.session_id, "import");
    println!("imported: {} ({} entries)", r.session_id, r.imported);
    if r.dropped > 0 {
        println!("dropped:  {} unsettled trailing entries", r.dropped);
    }
    println!("continue: tole resume {} \"<prompt>\"", r.session_id);
    Ok(())
}

/// `tole sessions --tree` (#210): sessions with their forks nested.
pub(crate) fn sessions_tree_command(sessions_dir: &Path) -> Result<()> {
    let nodes = tole_cli::session_index::tree_nodes(sessions_dir);
    if nodes.is_empty() {
        println!("no sessions in {}", sessions_dir.display());
        return Ok(());
    }
    for line in tole_cli::session_index::tree_lines(nodes) {
        println!("{line}");
    }
    Ok(())
}

/// `tole sessions --all` (#439): every indexed session whose file still
/// exists, newest first, with its project directory.
pub(crate) fn all_sessions_command(grep: Option<&str>) -> Result<()> {
    let index = tole_cli::session_index::index_path()
        .context("cannot locate the session index: set CODECORA_HOME or HOME")?;
    let rows = tole_cli::session_index::rows(&index, grep);
    if rows.is_empty() {
        println!("no indexed sessions in {}", index.display());
        return Ok(());
    }
    println!(
        "{:<26} {:<16} {:<10} {:>5}  {:<16}  cwd / first prompt",
        "session", "name", "pc", "turns", "started"
    );
    for r in &rows {
        let started =
            fmt_mtime(std::time::UNIX_EPOCH + std::time::Duration::from_secs(r.entry.created_at));
        println!(
            "{:<26} {:<16} {:<10} {:>5}  {:<16}  {}",
            r.entry.id,
            r.name.as_deref().unwrap_or("-"),
            r.pc,
            r.turns,
            started,
            r.entry.cwd.display()
        );
        if !r.preview.is_empty() {
            println!("{:<26} {}", "", r.preview);
        }
    }
    Ok(())
}

/// `YYYY-mm-dd HH:MM` in UTC — stable, no chrono dep.
fn fmt_mtime(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m) = (rem / 3600, (rem % 3600) / 60);
    format!("{} {h:02}:{m:02}", civil_from_days(days as i64))
}

/// Most recently modified session id in `dir` (None when empty).
pub(crate) fn latest_session_id(dir: &Path) -> Option<String> {
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(stem) = name.strip_suffix(".jsonl") else {
            continue;
        };
        if !valid_session_id(stem) {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
            best = Some((mtime, name.trim_end_matches(".jsonl").to_string()));
        }
    }
    best.map(|(_, id)| id)
}

/// Time-ordered, filesystem-safe session id (no new deps).
pub(crate) fn new_session_id() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("s-{ms:x}-{pid:x}", pid = std::process::id())
}

#[cfg(test)]
mod session_id_rule_tests {
    /// Issue #295: the CLI (`--resume`/`status`) and serve/ACP must agree
    /// on every id, including uppercase and `_`.
    #[test]
    fn cli_and_serve_acp_session_id_rules_agree() {
        let long = "a".repeat(65);
        let cases = [
            ("s-abc123", true),
            ("Session_ID-1", true),
            ("UPPER", true),
            ("under_score", true),
            ("", false),
            ("..", false),
            ("../evil", false),
            ("a/b", false),
            ("a\\b", false),
            (long.as_str(), false),
        ];
        for (id, ok) in cases {
            assert_eq!(super::valid_session_id(id), ok, "valid_session_id({id:?})");
            assert_eq!(
                tole_cli::session_host::validate_session_id(id).is_some(),
                ok,
                "validate_session_id({id:?})"
            );
        }
    }
}
