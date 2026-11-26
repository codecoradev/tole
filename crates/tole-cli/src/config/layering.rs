//! Merging the project layer and the user layer key by key (#433).

use super::secrets_config::{layered_secrets, layered_sensitive};
use super::{Config, MissionConfig};
use std::collections::BTreeSet;

/// Strictness of a `review_gate` value (`block` > `warn` > `off`); `None`
/// when it is not one of the three.
pub(super) fn gate_strictness(v: &str) -> Option<u8> {
    match v.trim().to_ascii_lowercase().as_str() {
        "off" => Some(0),
        "warn" => Some(1),
        "block" => Some(2),
        _ => None,
    }
}

/// Merge the two layers key by key: the project value wins when it is set
/// (by the same rules the precedence functions use: a blank string or an
/// empty list counts as unset), else the user value. Lists are never
/// merged across layers. The result records which keys came from the user
/// layer in [`Config::user_keys`].
pub fn layered(project: &Config, user: &Config) -> Config {
    let mut from_user = BTreeSet::new();
    fn pick<T: Clone>(
        key: &'static str,
        p: &Option<T>,
        u: &Option<T>,
        set: fn(&T) -> bool,
        from_user: &mut BTreeSet<&'static str>,
    ) -> Option<T> {
        match (p, u) {
            (Some(p), _) if set(p) => Some(p.clone()),
            (_, Some(u)) if set(u) => {
                from_user.insert(key);
                Some(u.clone())
            }
            _ => p.clone(),
        }
    }
    let text = |v: &String| !v.trim().is_empty();
    let list = |v: &Vec<String>| !v.is_empty();
    fn any<T>(_: &T) -> bool {
        true
    }
    let pm = project.mission.clone().unwrap_or_default();
    let um = user.mission.clone().unwrap_or_default();
    let u = &mut from_user;
    let mission = MissionConfig {
        max_steps: pick("mission.max_steps", &pm.max_steps, &um.max_steps, any, u),
        max_minutes: pick(
            "mission.max_minutes",
            &pm.max_minutes,
            &um.max_minutes,
            any,
            u,
        ),
        max_tokens: pick("mission.max_tokens", &pm.max_tokens, &um.max_tokens, any, u),
        verify: pick("mission.verify", &pm.verify, &um.verify, text, u),
        verify_timeout: pick(
            "mission.verify_timeout",
            &pm.verify_timeout,
            &um.verify_timeout,
            any,
            u,
        ),
    };
    let mut out = Config {
        model: pick("model", &project.model, &user.model, text, u),
        base_url: pick("base_url", &project.base_url, &user.base_url, text, u),
        sessions_dir: pick(
            "sessions_dir",
            &project.sessions_dir,
            &user.sessions_dir,
            text,
            u,
        ),
        workspace: pick("workspace", &project.workspace, &user.workspace, text, u),
        plan_mode: pick("plan_mode", &project.plan_mode, &user.plan_mode, any, u),
        memory: pick("memory", &project.memory, &user.memory, text, u),
        trust: pick("trust", &project.trust, &user.trust, list, u),
        allow: pick("allow", &project.allow, &user.allow, list, u),
        mcp_server: pick("mcp_server", &project.mcp_server, &user.mcp_server, list, u),
        no_auto_mcp: pick(
            "no_auto_mcp",
            &project.no_auto_mcp,
            &user.no_auto_mcp,
            any,
            u,
        ),
        on_pretool: pick("on_pretool", &project.on_pretool, &user.on_pretool, list, u),
        on_posttool: pick(
            "on_posttool",
            &project.on_posttool,
            &user.on_posttool,
            list,
            u,
        ),
        on_turnend: pick("on_turnend", &project.on_turnend, &user.on_turnend, list, u),
        skill: pick("skill", &project.skill, &user.skill, list, u),
        no_skills: pick("no_skills", &project.no_skills, &user.no_skills, any, u),
        system_prompt: pick(
            "system_prompt",
            &project.system_prompt,
            &user.system_prompt,
            text,
            u,
        ),
        context_window: pick(
            "context_window",
            &project.context_window,
            &user.context_window,
            any,
            u,
        ),
        compact_at: pick("compact_at", &project.compact_at, &user.compact_at, any, u),
        no_compaction: pick(
            "no_compaction",
            &project.no_compaction,
            &user.no_compaction,
            any,
            u,
        ),
        reasoning_effort: pick(
            "reasoning_effort",
            &project.reasoning_effort,
            &user.reasoning_effort,
            text,
            u,
        ),
        // The one key where the project layer does NOT simply win: a cloned
        // repository must not be able to loosen the review gate the user
        // chose, so the STRICTER of the two layers applies (a project may
        // only keep or tighten it).
        review_gate: match (&project.review_gate, &user.review_gate) {
            (Some(p), Some(u)) => {
                if gate_strictness(u) > gate_strictness(p) {
                    u.clone().into()
                } else {
                    p.clone().into()
                }
            }
            (p, u) => p.clone().or_else(|| u.clone()),
        },
        mission: (project.mission.is_some() || user.mission.is_some()).then_some(mission),
        // The project layer NEVER supplies the theme, colors or keys (#463).
        tui: user.tui.clone(),
        // Nor the scheduler limits (#579).
        cron: user.cron.clone(),
        // Nor the discussion request timeout (#786).
        discussions: user.discussions.clone(),
        provider: user.provider.clone(),
        // Nor the sandbox (#601): a repository must not loosen its own.
        sandbox: user.sandbox.clone(),
        // Nor the curator's settings (#593).
        curator: user.curator.clone(),
        // Nor the delegation limits (#585).
        delegation: user.delegation.clone(),
        // Nor profiles (#584): a cloned repository must not define personas,
        // their allow lists or their models.
        profiles: user.profiles.clone(),
        // Nor webhook routes (#582): a repository must not open or widen
        // an endpoint.
        hooks: user.hooks.clone(),
        // Nor outbound destinations (#911): a repository must not open or
        // redirect an outbound path.
        webhook_out: user.webhook_out.clone(),
        // Routing (#586): tiers and rules from the user only; a project can
        // only turn it off or narrow the allowed tiers.
        router: crate::routing::layered(&project.router, &user.router),
        // A project may only narrow the secret release (#598).
        secrets: layered_secrets(&project.secrets, &user.secrets),
        // A project can only add sensitive names (#683).
        sensitive_files: layered_sensitive(&project.sensitive_files, &user.sensitive_files),
        // Nor can it exempt values from the secret-shape detector (#597).
        redaction_allowlist: user.redaction_allowlist.clone(),
        user_keys: BTreeSet::new(),
    };
    if let Some(c) = &user.cron {
        if c.daily_token_cap.is_some() {
            from_user.insert("cron.daily_token_cap");
        }
        if c.max_concurrent.is_some() {
            from_user.insert("cron.max_concurrent");
        }
        if c.timezone.is_some() {
            from_user.insert("cron.timezone");
        }
        if c.tz_source.is_some() {
            from_user.insert("cron.tz_source");
        }
    }
    if user
        .discussions
        .as_ref()
        .is_some_and(|d| d.request_timeout_secs.is_some())
    {
        from_user.insert("discussions.request_timeout_secs");
    }
    if user
        .provider
        .as_ref()
        .is_some_and(|p| p.request_timeout_secs.is_some())
    {
        from_user.insert("provider.request_timeout_secs");
    }
    if let Some(s) = &user.sandbox {
        for (k, set) in [
            ("sandbox.mode", s.mode.is_some()),
            ("sandbox.profile", s.profile.is_some()),
            ("sandbox.network", s.network.is_some()),
        ] {
            if set {
                from_user.insert(k);
            }
        }
        if let Some(c) = &s.consumers {
            for (k, set) in [
                ("sandbox.consumers.run_command", c.run_command.is_some()),
                ("sandbox.consumers.job_start", c.job_start.is_some()),
                ("sandbox.consumers.verify", c.verify.is_some()),
                ("sandbox.consumers.turnend", c.turnend.is_some()),
                ("sandbox.consumers.cron", c.cron.is_some()),
                ("sandbox.consumers.network", c.network.is_some()),
                ("sandbox.consumers.cpu_secs", c.cpu_secs.is_some()),
                ("sandbox.consumers.memory_mb", c.memory_mb.is_some()),
                ("sandbox.consumers.processes", c.processes.is_some()),
                ("sandbox.consumers.file_size_mb", c.file_size_mb.is_some()),
            ] {
                if set {
                    from_user.insert(k);
                }
            }
        }
    }
    if let Some(c) = &user.curator {
        if c.enabled.is_some() {
            from_user.insert("curator.enabled");
        }
        if c.stale_days.is_some() {
            from_user.insert("curator.stale_days");
        }
        if c.archive_days.is_some() {
            from_user.insert("curator.archive_days");
        }
        if c.grace_days.is_some() {
            from_user.insert("curator.grace_days");
        }
    }
    if let Some(t) = &user.tui {
        if t.theme.as_deref().is_some_and(|v| !v.trim().is_empty()) {
            from_user.insert("tui.theme");
        }
        if t.colors.as_ref().is_some_and(|c| !c.is_empty()) {
            from_user.insert("tui.colors");
        }
        if t.keys.as_ref().is_some_and(|k| !k.is_empty()) {
            from_user.insert("tui.keys");
        }
    }
    if out.review_gate.is_some() && out.review_gate != project.review_gate {
        from_user.insert("review_gate");
    }
    out.user_keys = from_user;
    out
}
