//! Rendering a config as `key = value  [layer]` lines for `tole config check`.

use super::report::{
    CRON_IGNORED_NOTE, CURATOR_IGNORED_NOTE, DELEGATION_IGNORED_NOTE, DISCUSSIONS_IGNORED_NOTE,
    PROFILES_IGNORED_NOTE, PROVIDER_IGNORED_NOTE, SANDBOX_IGNORED_NOTE, SECRETS_PROJECT_NOTE,
    TUI_IGNORED_NOTE,
};
use super::{Config, TzSource};

/// Longest `system_prompt` rendered by `config check`, in chars.
const PROMPT_DISPLAY_CHARS: usize = 80;

fn quote(s: &str) -> String {
    format!("{s:?}")
}

/// Per-key text appended after `[config]` on the key's line (effective
/// value + source, or the "not applied" marker). Keys use the rendered
/// names (`model`, `mission.max_steps`, ...).
pub type Annotations = std::collections::BTreeMap<&'static str, String>;

/// Render `key = value  [config]` lines for every key that is set. List
/// values go one entry per line; unset keys are not listed.
pub fn render(cfg: &Config) -> Vec<String> {
    render_with(cfg, PROJECT_TAG, &Annotations::new())
}

/// The `[tag]` on a project config line.
pub const PROJECT_TAG: &str = "config";
/// The `[tag]` on a user config line.
pub const USER_TAG: &str = "user config";

/// [`render`] with the layer's tag and a per-key suffix after it.
pub fn render_with(cfg: &Config, tag: &str, ann: &Annotations) -> Vec<String> {
    let src = tag;
    let mut out = Vec::new();
    let note = |key: &str| {
        if tag == PROJECT_TAG && key.starts_with("tui.") {
            return TUI_IGNORED_NOTE;
        }
        if tag == PROJECT_TAG && key.starts_with("cron.") {
            return CRON_IGNORED_NOTE;
        }
        if tag == PROJECT_TAG && key.starts_with("provider.") {
            return PROVIDER_IGNORED_NOTE;
        }
        if tag == PROJECT_TAG && key.starts_with("discussions.") {
            return DISCUSSIONS_IGNORED_NOTE;
        }
        if tag == PROJECT_TAG && key.starts_with("sandbox.") {
            return SANDBOX_IGNORED_NOTE;
        }
        if tag == PROJECT_TAG && key.starts_with("curator.") {
            return CURATOR_IGNORED_NOTE;
        }
        if tag == PROJECT_TAG && key.starts_with("delegation.") {
            return DELEGATION_IGNORED_NOTE;
        }
        if tag == PROJECT_TAG && key == "profiles" {
            return PROFILES_IGNORED_NOTE;
        }
        ann.get(key).map_or("", String::as_str)
    };
    let scalar = |out: &mut Vec<String>, key: &str, v: Option<String>| {
        if let Some(v) = v {
            out.push(format!("{key} = {v}  [{src}]{}", note(key)));
        }
    };
    let list = |out: &mut Vec<String>, key: &str, v: &Option<Vec<String>>| {
        if let Some(items) = v {
            out.push(format!(
                "{key} = [{} entries]  [{src}]{}",
                items.len(),
                note(key)
            ));
            for i in items {
                out.push(format!("  - {}", quote(i)));
            }
        }
    };
    scalar(&mut out, "model", cfg.model.as_deref().map(quote));
    scalar(&mut out, "base_url", cfg.base_url.as_deref().map(quote));
    scalar(
        &mut out,
        "sessions_dir",
        cfg.sessions_dir.as_deref().map(quote),
    );
    scalar(&mut out, "workspace", cfg.workspace.as_deref().map(quote));
    scalar(&mut out, "plan_mode", cfg.plan_mode.map(|b| b.to_string()));
    scalar(&mut out, "memory", cfg.memory.as_deref().map(quote));
    list(&mut out, "trust", &cfg.trust);
    list(&mut out, "allow", &cfg.allow);
    list(&mut out, "mcp_server", &cfg.mcp_server);
    scalar(
        &mut out,
        "no_auto_mcp",
        cfg.no_auto_mcp.map(|b| b.to_string()),
    );
    list(&mut out, "on_pretool", &cfg.on_pretool);
    list(&mut out, "on_posttool", &cfg.on_posttool);
    list(&mut out, "on_turnend", &cfg.on_turnend);
    list(&mut out, "skill", &cfg.skill);
    list(&mut out, "sensitive_files", &cfg.sensitive_files);
    list(&mut out, "redaction_allowlist", &cfg.redaction_allowlist);
    scalar(&mut out, "no_skills", cfg.no_skills.map(|b| b.to_string()));
    scalar(
        &mut out,
        "system_prompt",
        cfg.system_prompt.as_deref().map(|p| {
            let total = p.chars().count();
            if total > PROMPT_DISPLAY_CHARS {
                let head: String = p.chars().take(PROMPT_DISPLAY_CHARS).collect();
                format!("{}… ({total} chars)", quote(&head))
            } else {
                quote(p)
            }
        }),
    );
    scalar(
        &mut out,
        "context_window",
        cfg.context_window.map(|n| n.to_string()),
    );
    scalar(
        &mut out,
        "compact_at",
        cfg.compact_at.map(|n| n.to_string()),
    );
    scalar(
        &mut out,
        "no_compaction",
        cfg.no_compaction.map(|b| b.to_string()),
    );
    scalar(
        &mut out,
        "reasoning_effort",
        cfg.reasoning_effort.as_deref().map(quote),
    );
    scalar(
        &mut out,
        "review_gate",
        cfg.review_gate.as_deref().map(quote),
    );
    if let Some(t) = &cfg.tui {
        scalar(&mut out, "tui.theme", t.theme.as_deref().map(quote));
        if let Some(c) = &t.colors {
            out.push(format!(
                "tui.colors = [{} entries]  [{src}]{}",
                c.len(),
                note("tui.colors")
            ));
            for (role, spec) in c {
                out.push(format!("  - {role} = {}", quote(spec)));
            }
        }
        if let Some(k) = &t.keys {
            out.push(format!(
                "tui.keys = [{} entries]  [{src}]{}",
                k.len(),
                note("tui.keys")
            ));
            for (action, keys) in k {
                let shown: Vec<String> = keys.to_vec().iter().map(|s| quote(s)).collect();
                out.push(format!("  - {action} = [{}]", shown.join(", ")));
            }
        }
    }
    if let Some(p) = cfg.profiles.as_ref().filter(|p| !p.is_empty()) {
        // Names only: a profile is shown by `config check --profile <name>`.
        out.push(format!(
            "profiles = [{} entries]  [{src}]{}",
            p.len(),
            note("profiles")
        ));
        for name in p.keys() {
            out.push(format!("  - {}", quote(name)));
        }
    }
    if let Some(w) = cfg.webhook_out.as_ref().filter(|w| !w.is_empty()) {
        // Destination names only (#911): never a URL, and never where a
        // secret comes from.
        let ignored = if tag == PROJECT_TAG {
            "  IGNORED: a project config cannot define webhook destinations (user config only)"
        } else {
            ""
        };
        out.push(format!(
            "webhook_out = [{} entries]  [{src}]{ignored}",
            w.len()
        ));
        for name in w.keys() {
            out.push(format!("  - {}", quote(name)));
        }
    }
    if let Some(h) = cfg.hooks.as_ref().filter(|h| !h.is_empty()) {
        // Route names only (#582): never where a key comes from.
        let ignored = if tag == PROJECT_TAG {
            "  IGNORED: a project config cannot define webhook routes (user config only)"
        } else {
            ""
        };
        out.push(format!("hooks = [{} entries]  [{src}]{ignored}", h.len()));
        for name in h.keys() {
            out.push(format!("  - {}", quote(name)));
        }
    }
    if let Some(r) = &cfg.router {
        // Names only (#586): never models or keywords.
        let narrow = if tag == PROJECT_TAG {
            "  a project config can only set enabled = false or narrow allow_tiers (tiers and rules are ignored)"
        } else {
            ""
        };
        let on = if r.enabled == Some(true) { "on" } else { "off" };
        out.push(format!("router = {on}  [{src}]{narrow}"));
        if let Some(a) = &r.allow_tiers {
            out.push(format!("  allow_tiers = [{}]", a.join(", ")));
        }
        if let Some(t) = &r.tiers {
            let set: Vec<&str> = [
                ("light", &t.light),
                ("standard", &t.standard),
                ("heavy", &t.heavy),
            ]
            .into_iter()
            .filter(|(_, m)| m.is_some())
            .map(|(n, _)| n)
            .collect();
            out.push(format!("  tiers = [{}]", set.join(", ")));
        }
        for rule in r.rules.iter().flatten() {
            out.push(format!(
                "  - rule {} -> {}",
                quote(&rule.name),
                quote(&rule.tier)
            ));
        }
    }
    for (name, b) in cfg.secrets.iter().flatten() {
        let key = format!("secrets.{name}");
        let note = if tag == PROJECT_TAG {
            SECRETS_PROJECT_NOTE
        } else {
            ""
        };
        out.push(format!(
            "{key} = [{}]  [{src}]{note}",
            b.tools
                .iter()
                .map(|t| quote(t))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(m) = &cfg.mission {
        scalar(
            &mut out,
            "mission.max_steps",
            m.max_steps.map(|n| n.to_string()),
        );
        scalar(
            &mut out,
            "mission.max_minutes",
            m.max_minutes.map(|n| n.to_string()),
        );
        scalar(
            &mut out,
            "mission.max_tokens",
            m.max_tokens.map(|n| n.to_string()),
        );
        scalar(&mut out, "mission.verify", m.verify.as_deref().map(quote));
        scalar(
            &mut out,
            "mission.verify_timeout",
            m.verify_timeout.map(|n| n.to_string()),
        );
    }
    if let Some(d) = &cfg.discussions {
        scalar(
            &mut out,
            "discussions.request_timeout_secs",
            d.request_timeout_secs.map(|n| n.to_string()),
        );
    }
    if let Some(p) = &cfg.provider {
        scalar(
            &mut out,
            "provider.request_timeout_secs",
            p.request_timeout_secs.map(|n| n.to_string()),
        );
    }
    if let Some(c) = &cfg.cron {
        scalar(
            &mut out,
            "cron.daily_token_cap",
            c.daily_token_cap.map(|n| n.to_string()),
        );
        scalar(
            &mut out,
            "cron.max_concurrent",
            c.max_concurrent.map(|n| n.to_string()),
        );
        scalar(&mut out, "cron.timezone", c.timezone.as_deref().map(quote));
        scalar(
            &mut out,
            "cron.tz_source",
            c.tz_source.map(|s| {
                match s {
                    TzSource::System => "\"system\"",
                    TzSource::OffsetOnly => "\"offset-only\"",
                }
                .to_string()
            }),
        );
    }
    if let Some(s) = &cfg.sandbox {
        scalar(&mut out, "sandbox.mode", s.mode.as_deref().map(quote));
        scalar(&mut out, "sandbox.profile", s.profile.as_deref().map(quote));
        scalar(&mut out, "sandbox.network", s.network.as_deref().map(quote));
        for (name, p) in s.profiles.iter().flatten() {
            let k = |f: &str| format!("sandbox.profiles.{name}.{f}");
            scalar(&mut out, &k("workspace"), p.workspace.as_deref().map(quote));
            scalar(&mut out, &k("network"), p.network.as_deref().map(quote));
            list(&mut out, &k("read"), &p.read);
            list(&mut out, &k("write"), &p.write);
        }
        for (name, sv) in s.servers.iter().flatten() {
            scalar(
                &mut out,
                &format!("sandbox.servers.{name}.profile"),
                sv.profile.as_deref().map(quote),
            );
        }
        if let Some(c) = &s.consumers {
            let n = |v: Option<u64>| v.map(|n| n.to_string());
            for (key, v) in [
                ("sandbox.consumers.run_command", &c.run_command),
                ("sandbox.consumers.job_start", &c.job_start),
                ("sandbox.consumers.verify", &c.verify),
                ("sandbox.consumers.turnend", &c.turnend),
                ("sandbox.consumers.cron", &c.cron),
                ("sandbox.consumers.network", &c.network),
            ] {
                scalar(&mut out, key, v.as_deref().map(quote));
            }
            for (key, v) in [
                ("sandbox.consumers.cpu_secs", c.cpu_secs),
                ("sandbox.consumers.memory_mb", c.memory_mb),
                ("sandbox.consumers.processes", c.processes),
                ("sandbox.consumers.file_size_mb", c.file_size_mb),
            ] {
                scalar(&mut out, key, n(v));
            }
        }
    }
    if let Some(c) = &cfg.curator {
        scalar(
            &mut out,
            "curator.enabled",
            c.enabled.map(|b| b.to_string()),
        );
        for (key, v) in [
            ("curator.stale_days", c.stale_days),
            ("curator.archive_days", c.archive_days),
            ("curator.grace_days", c.grace_days),
        ] {
            scalar(&mut out, key, v.map(|n| n.to_string()));
        }
    }
    if let Some(d) = &cfg.delegation {
        let n = |v: Option<u64>| v.map(|n| n.to_string());
        scalar(
            &mut out,
            "delegation.max_depth",
            n(d.max_depth.map(u64::from)),
        );
        scalar(
            &mut out,
            "delegation.max_live_processes",
            n(d.max_live_processes.map(u64::from)),
        );
        scalar(&mut out, "delegation.goal_budget", n(d.goal_budget));
        scalar(
            &mut out,
            "delegation.goal_steps",
            n(d.goal_steps.map(u64::from)),
        );
        scalar(&mut out, "delegation.goal_minutes", n(d.goal_minutes));
        for (key, v) in [
            ("min_free_memory_mb", d.min_free_memory_mb),
            ("child_memory_mb", d.child_memory_mb),
            ("min_free_disk_mb", d.min_free_disk_mb),
            ("min_free_pids_percent", d.min_free_pids_percent),
            ("spawn_interval_secs", d.spawn_interval_secs),
            ("machine_max_live", d.machine_max_live.map(u64::from)),
            ("child_nice", d.child_nice.map(u64::from)),
        ] {
            scalar(&mut out, &format!("delegation.{key}"), n(v));
        }
        scalar(
            &mut out,
            "delegation.max_load_per_cpu",
            d.max_load_per_cpu.map(|v| v.to_string()),
        );
        let mode = d.approvals.map(|m| format!("{m:?}").to_ascii_lowercase());
        scalar(&mut out, "delegation.approvals", mode);
        scalar(&mut out, "delegation.approval_secs", n(d.approval_secs));
        scalar(
            &mut out,
            "delegation.approval_level_secs",
            n(d.approval_level_secs),
        );
    }
    out
}
