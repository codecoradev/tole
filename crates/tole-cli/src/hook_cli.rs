//! `tole hook add|list|remove|test` (#582 part 3): webhook routes in the
//! user config, never a project config.
//!
//! - `add` appends one `[hooks.<route>]` section to the user config (the
//!   rest of the file, comments included, is kept), after checking the new
//!   file with the same loader `tole serve` uses. A route needs a key source,
//!   `--key-env NAME` or `--key-file PATH`; there is no flag for the key
//!   itself. `allow_writes` and `insecure_loopback` have no flag either:
//!   they are a manual edit of the user config (design P8).
//! - `list` shows route names, the job, events, the filter's shape and
//!   whether writes are allowed. It shows no key and not where a key comes
//!   from.
//! - `remove <route>` cuts that route's sections and nothing else.
//! - `test <route>` signs a sample payload with the route's key and runs it
//!   through the events allowlist, the filter and the template: a local dry
//!   run that fires nothing. `--fire --url http://127.0.0.1:PORT` sends the
//!   signed delivery to that `tole serve`, loopback only.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use clap::Subcommand;

use crate::config::HookConfig;
use crate::hook_filter;

/// Largest user config `add` / `remove` edit.
const MAX_CONFIG: u64 = 1024 * 1024;

/// `tole hook <action>`.
#[derive(Subcommand, Debug)]
pub enum HookAction {
    /// Add a route to the user config: `POST /hooks/ROUTE` on `tole serve`.
    /// The key is never given here: name the variable or the file holding
    /// it. Filters are written by hand in `[hooks.ROUTE.filter]`.
    Add {
        route: String,
        /// Environment variable holding the HMAC key (at least 16 bytes).
        #[arg(long)]
        key_env: Option<String>,
        /// File holding the HMAC key (a regular file, at most 4 KiB).
        #[arg(long)]
        key_file: Option<PathBuf>,
        /// The existing cron job a matching delivery fires.
        #[arg(long)]
        job: Option<String>,
        /// Admitted event, repeatable (default: all).
        #[arg(long = "event")]
        events: Vec<String>,
        /// `{dot.path}` template of the context the fired run sees.
        #[arg(long)]
        template: Option<String>,
        #[arg(long)]
        rate_per_minute: Option<u32>,
        #[arg(long)]
        max_body_bytes: Option<u64>,
    },
    /// List the routes (never their keys).
    List,
    /// Remove a route from the user config.
    Remove { route: String },
    /// Dry-run a route: sign a payload, check events and filter, print the
    /// fenced context. Fires nothing unless --fire.
    Test {
        route: String,
        /// JSON payload file (default: `{}`).
        #[arg(long)]
        payload: Option<PathBuf>,
        /// The event to send (`X-Tole-Event`).
        #[arg(long)]
        event: Option<String>,
        /// Send the signed delivery to the `tole serve` at --url.
        #[arg(long, requires = "url")]
        fire: bool,
        /// `http://127.0.0.1:PORT` (loopback only).
        #[arg(long)]
        url: Option<String>,
    },
}

/// Runs a `tole hook` action against the user config.
pub fn cli(action: HookAction) -> anyhow::Result<()> {
    let path = crate::config::user_config_path()
        .ok_or_else(|| anyhow!("cannot resolve the user config (set CODECORA_HOME or HOME)"))?;
    let out = match action {
        HookAction::Add {
            route,
            key_env,
            key_file,
            job,
            events,
            template,
            rate_per_minute,
            max_body_bytes,
        } => {
            let cfg = HookConfig {
                hmac_key_env: key_env,
                hmac_key_file: key_file.map(|p| p.display().to_string()),
                cron_job: job,
                events: (!events.is_empty()).then_some(events),
                template,
                rate_per_minute,
                max_body_bytes,
                ..HookConfig::default()
            };
            add(&path, &route, &cfg)?
        }
        HookAction::List => list(&path)?,
        HookAction::Remove { route } => remove(&path, &route)?,
        HookAction::Test {
            route,
            payload,
            event,
            fire,
            url,
        } => test(
            &path,
            &route,
            payload.as_deref(),
            event.as_deref(),
            fire.then_some(url).flatten(),
        )?,
    };
    print!("{out}");
    Ok(())
}

fn read_config(path: &Path) -> anyhow::Result<String> {
    match tole_core::delegation_report::read_plain_file(path, MAX_CONFIG) {
        Ok(Some(b)) => String::from_utf8(b).map_err(|_| anyhow!("{} is not UTF-8", path.display())),
        Ok(None) => Ok(String::new()),
        Err(e) => bail!("the user config {} is {e}", path.display()),
    }
}

fn hooks_of(
    path: &Path,
    text: &str,
) -> anyhow::Result<std::collections::BTreeMap<String, HookConfig>> {
    let cfg = crate::config::parse(text, path).map_err(|e| anyhow!("{e}"))?;
    Ok(cfg.hooks.unwrap_or_default())
}

/// Writes `text` to `path` through a temporary file and a rename.
fn write_atomic(path: &Path, text: &str) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("no parent for {}", path.display()))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".config.toml.tmp-{}", std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

fn quote(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// `tole hook add`: appends the route, checked like `tole serve` loads it.
pub fn add(path: &Path, route: &str, h: &HookConfig) -> anyhow::Result<String> {
    // Wider tools for a webhook run are a manual edit of the user config,
    // never the CLI or an agent (design P8).
    if h.allow_writes.is_some() || h.insecure_loopback.is_some() {
        bail!("allow_writes and insecure_loopback are set by editing the user config by hand");
    }
    if h.hmac_key_env.is_some() == h.hmac_key_file.is_some() {
        bail!("a route needs exactly one key source: --key-env NAME or --key-file PATH");
    }
    if let Some(v) = &h.hmac_key_env {
        let ok = !v.is_empty()
            && !v.starts_with(|c: char| c.is_ascii_digit())
            && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !ok {
            bail!("--key-env must be a variable name ([A-Za-z_][A-Za-z0-9_]*)");
        }
    }
    if h.hmac_key_file
        .as_deref()
        .is_some_and(|f| !Path::new(f).is_absolute())
    {
        bail!("--key-file must be an absolute path");
    }
    let old = read_config(path)?;
    if hooks_of(path, &old)?.contains_key(route) {
        bail!(
            "route `{route}` already exists in {}; remove it first",
            path.display()
        );
    }
    // Checked like `tole serve` loads it; a key variable unset in this
    // shell is checked with a stand-in (serve reads the real one).
    let probe: std::collections::BTreeMap<String, HookConfig> =
        [(route.to_string(), h.clone())].into_iter().collect();
    let stand_in = |_: &str| Some("x".repeat(crate::hooks::MIN_KEY));
    crate::hooks::load(&probe, true, &stand_in).map_err(|e| anyhow!(e))?;
    let mut block = format!("\n[hooks.{route}]\n");
    let mut kv = |k: &str, v: String| block.push_str(&format!("{k} = {v}\n"));
    if let Some(v) = &h.hmac_key_env {
        kv("hmac_key_env", quote(v));
    }
    if let Some(v) = &h.hmac_key_file {
        kv("hmac_key_file", quote(v));
    }
    if let Some(v) = &h.cron_job {
        kv("cron_job", quote(v));
    }
    if let Some(es) = &h.events {
        let items: Vec<String> = es.iter().map(|e| quote(e)).collect();
        kv("events", format!("[{}]", items.join(", ")));
    }
    if let Some(v) = &h.template {
        kv("template", quote(v));
    }
    if let Some(n) = h.rate_per_minute {
        kv("rate_per_minute", n.to_string());
    }
    if let Some(n) = h.max_body_bytes {
        kv("max_body_bytes", n.to_string());
    }
    let mut new = old.clone();
    if !new.is_empty() && !new.ends_with('\n') {
        new.push('\n');
    }
    new.push_str(&block);
    if !hooks_of(path, &new)?.contains_key(route) {
        bail!("route `{route}` did not round-trip through the config parser");
    }
    write_atomic(path, &new)?;
    let mut msg = format!(
        "added hook route `{route}` to {}; `tole serve` loads it at start\n",
        path.display()
    );
    if let Some(v) = &h.hmac_key_env {
        if std::env::var_os(v).is_none_or(|x| x.is_empty()) {
            msg.push_str(&format!(
                "note: {v} is not set in this shell; `tole serve` refuses to start until it is\n"
            ));
        }
    }
    Ok(msg)
}

/// `tole hook list`.
pub fn list(path: &Path) -> anyhow::Result<String> {
    let hooks = hooks_of(path, &read_config(path)?)?;
    if hooks.is_empty() {
        return Ok("no hook routes\n".into());
    }
    let mut out = String::new();
    for (name, h) in hooks {
        out.push_str(&format!(
            "{name}  job: {}  events: {}  filter: {}  writes: {}  auth: {}\n",
            h.cron_job.as_deref().unwrap_or("-"),
            h.events.map_or("all".into(), |e| e.join(",")),
            h.filter.as_ref().map_or("-".into(), hook_filter::summary),
            if h.allow_writes == Some(true) {
                "yes"
            } else {
                "no"
            },
            if h.insecure_loopback == Some(true) {
                "insecure (loopback only)"
            } else {
                "signed"
            },
        ));
    }
    Ok(out)
}

/// The route a TOML section header belongs to, if it is under `hooks`.
fn header_route(line: &str) -> Option<String> {
    let t = line.trim();
    let inner = t.strip_prefix("[[").or_else(|| t.strip_prefix('['))?;
    let inner = inner.trim_end_matches(']').trim();
    let rest = inner.strip_prefix("hooks.")?;
    Some(rest.split('.').next().unwrap_or(rest).trim().to_string())
}

/// `tole hook remove`: cuts the route's sections, keeps everything else.
pub fn remove(path: &Path, route: &str) -> anyhow::Result<String> {
    let old = read_config(path)?;
    if !hooks_of(path, &old)?.contains_key(route) {
        bail!("no hook route `{route}` in {}", path.display());
    }
    let (mut new, mut inside) = (String::new(), false);
    for line in old.split_inclusive('\n') {
        if line.trim_start().starts_with('[') {
            inside = header_route(line).as_deref() == Some(route);
        }
        if !inside {
            new.push_str(line);
        }
    }
    if hooks_of(path, &new)?.contains_key(route) {
        bail!(
            "route `{route}` is not written as [hooks.{route}] sections; edit {} by hand",
            path.display()
        );
    }
    write_atomic(path, &new)?;
    Ok(format!(
        "removed hook route `{route}` from {}\n",
        path.display()
    ))
}

/// `tole hook test`: a local dry run, or a signed delivery to a loopback
/// `tole serve` with `fire_url`.
pub fn test(
    path: &Path,
    name: &str,
    payload: Option<&Path>,
    event: Option<&str>,
    fire_url: Option<String>,
) -> anyhow::Result<String> {
    use tole_core::webhook;
    let hooks = hooks_of(path, &read_config(path)?)?;
    let cfg = hooks
        .get(name)
        .ok_or_else(|| anyhow!("no hook route `{name}` in {}", path.display()))?;
    let one: std::collections::BTreeMap<String, HookConfig> =
        [(name.to_string(), cfg.clone())].into_iter().collect();
    let routes =
        crate::hooks::load(&one, true, &|k| std::env::var(k).ok()).map_err(|e| anyhow!(e))?;
    let route = &routes[name];
    let body = match payload {
        Some(p) => tole_core::delegation_report::read_plain_file(p, route.max_body as u64)
            .map_err(|e| anyhow!("the payload {} is {e}", p.display()))?
            .ok_or_else(|| anyhow!("no payload file {}", p.display()))?,
        None => b"{}".to_vec(),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let (ts, id) = (
        now.to_string(),
        format!("test-{now}-{}", std::process::id()),
    );
    let mut out = String::new();
    let mut headers = vec![("X-Tole-Delivery", id.clone())];
    if let Some(e) = event {
        headers.push(("X-Tole-Event", e.to_string()));
    }
    match route.key() {
        Some(key) => {
            let signed = webhook::Headers {
                timestamp: Some(ts.clone()),
                signature: Some(webhook::sign(key, &ts, &body)),
                delivery: Some(id.clone()),
                ..Default::default()
            };
            webhook::verify(key, &signed, &body, now)
                .map_err(|r| anyhow!("signature: {}", r.reason()))?;
            out.push_str("signature: ok (native scheme, the route's key)\n");
            headers.push(("X-Tole-Timestamp", ts));
            headers.push(("X-Tole-Signature", signed.signature.unwrap_or_default()));
        }
        None => out.push_str("signature: none (insecure loopback route)\n"),
    }
    if let Some(url) = fire_url {
        return fire(&url, name, &headers, &body).map(|r| format!("{out}{r}"));
    }
    if !hook_filter::event_allowed(route.events.as_deref(), event) {
        out.push_str("ignored: event (not in the route's events)\n");
        return Ok(out);
    }
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) if route.filter.is_none() && route.template.is_none() => serde_json::Value::Null,
        Err(e) => bail!("the payload is not JSON: {e}"),
    };
    if route
        .filter
        .as_ref()
        .is_some_and(|f| !hook_filter::matches(f, &value))
    {
        out.push_str("ignored: filter (the payload does not match)\n");
        return Ok(out);
    }
    let Some(job) = &route.cron_job else {
        out.push_str("accepted: the route has no cron_job, nothing would fire\n");
        return Ok(out);
    };
    let secrets = crate::provider_chain::process_secrets(&crate::config_apply::real_env);
    let rendered = route
        .template
        .as_ref()
        .map(|t| t.render(&value, &secrets))
        .unwrap_or_default();
    out.push_str(&format!(
        "would fire `{job}` ({}), nothing fired; the run would see:\n{}\n",
        if route.allow_writes {
            "writes allowed"
        } else {
            "read-only"
        },
        hook_filter::fenced(name, event, &id, &rendered)
    ));
    Ok(out)
}

/// Posts the signed delivery to a loopback `tole serve`.
fn fire(url: &str, route: &str, headers: &[(&str, String)], body: &[u8]) -> anyhow::Result<String> {
    let loopback = || {
        anyhow!("--fire only targets a loopback `tole serve`: http://127.0.0.1:PORT (or [::1], localhost)")
    };
    let rest = url.strip_prefix("http://").ok_or_else(loopback)?;
    let host_port = rest.split('/').next().unwrap_or(rest);
    let (host, port) = host_port.rsplit_once(':').ok_or_else(loopback)?;
    let host = host.trim_matches(['[', ']']);
    let ip: std::net::IpAddr = match host {
        "localhost" => std::net::Ipv4Addr::LOCALHOST.into(),
        h => h.parse().map_err(|_| loopback())?,
    };
    if !ip.is_loopback() {
        return Err(loopback());
    }
    let port: u16 = port.parse().map_err(|_| loopback())?;
    let mut s = std::net::TcpStream::connect((ip, port))?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
    let mut req = format!(
        "POST /hooks/{route} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes())?;
    s.write_all(body)?;
    let mut resp = String::new();
    s.take(64 * 1024).read_to_string(&mut resp)?;
    let status = resp.lines().next().unwrap_or("").to_string();
    let answer = resp.split("\r\n\r\n").nth(1).unwrap_or("");
    Ok(format!("fired: {status}\n{answer}\n"))
}
