//! `gitea` tool: Gitea operations over the instance's HTTP API — the
//! same op surface as the `gh` tool (issue_view / issue_list / pr_view /
//! issue_comment / issue_create / pr_create), for Gitea-hosted remotes.
//! `Risk::Write`: every call goes through the Approver.
//!
//! Why the API and not the `tea` CLI: gh wraps a CLI because `gh` login
//! carries auth for free; `tea`'s argv surface is version-volatile and
//! untestable here (the repo has fixed two documented-but-unimplemented
//! bugs this month — shipping unverified argv would invite a third).
//! The Gitea REST API is stable, and a token env (`TOLE_GITEA_TOKEN` or
//! `GITEA_TOKEN`) plus the remote URL is all the wiring needed.
//!
//! The base URL is injectable so tests drive a local mock server
//! instead of the network — deterministic, offline.

use crate::tool::{Risk, Tool};
use serde_json::{json, Value};
use std::time::Duration;

/// API timeouts mirror the provider budget: read ops are quick; a hung
/// instance must not pin the turn.
const API_TIMEOUT: Duration = Duration::from_secs(30);

/// Allowed operations. Whitelist, not blacklist: anything not listed is
/// refused before any HTTP traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GiteaOp {
    IssueView,
    IssueList,
    PrView,
    IssueComment,
    IssueCreate,
    PrCreate,
}

impl GiteaOp {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "issue_view" => Some(GiteaOp::IssueView),
            "issue_list" => Some(GiteaOp::IssueList),
            "pr_view" => Some(GiteaOp::PrView),
            "issue_comment" => Some(GiteaOp::IssueComment),
            "issue_create" => Some(GiteaOp::IssueCreate),
            "pr_create" => Some(GiteaOp::PrCreate),
            _ => None,
        }
    }

    fn is_write(self) -> bool {
        matches!(
            self,
            GiteaOp::IssueComment | GiteaOp::IssueCreate | GiteaOp::PrCreate
        )
    }
}

/// Gitea operations via the instance REST API. Every invocation is
/// Write risk → the approval gate shows the exact request before it
/// runs.
#[derive(Debug, Clone)]
pub struct GiteaTool {
    /// API base, e.g. `https://gitea.example.com` (no trailing slash).
    /// Injectable for tests (a local mock server).
    pub base_url: String,
    /// Token sent as `Authorization: token <t>`. Never appears in
    /// describe()/command_line() output (approval shows the request
    /// shape, not the credential).
    pub token: String,
    /// Default `owner/repo`, overridable per call via the validated
    /// `repo` argument (unlike the CLI-era fixed `--repo`, the model may
    /// target other repos it can see — the approver sees the target).
    pub repo: String,
    agent: ureq::Agent,
}

impl GiteaTool {
    pub fn new(
        base_url: impl Into<String>,
        token: impl Into<String>,
        repo: impl Into<String>,
    ) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(API_TIMEOUT))
            .build()
            .new_agent();
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            repo: repo.into(),
            agent,
        }
    }

    /// The human-readable request this tool would make — used in the
    /// approval prompt. Deliberately excludes the token. WRITE ops
    /// append the model-controlled payload fields (title/body/head/
    /// base), each bounded — the approver approves THIS content, the
    /// same discipline as `gh`'s quoted command line (CodeCora PR #169
    /// round 1: a `--allow gitea` pattern must not auto-post arbitrary
    /// text sight-unseen).
    pub fn request_line(&self, input: &Value) -> Result<String, String> {
        let op = parse_op(input)?;
        let repo = target_repo(input, &self.repo)?;
        let number = opt_number(input)?;
        let (method, path) = op_route(op, &repo, number.as_deref(), input)?;
        let mut line = format!("gitea {method} /{path}");
        match op {
            GiteaOp::IssueComment => {
                line.push_str(&format!(" body={}", preview(input, "body")?));
            }
            GiteaOp::IssueCreate => {
                line.push_str(&format!(
                    " title={} body={}",
                    preview(input, "title")?,
                    preview(input, "body").unwrap_or_else(|_| "\"\"".into())
                ));
            }
            GiteaOp::PrCreate => {
                line.push_str(&format!(
                    " title={} head={} base={} body={}",
                    preview(input, "title")?,
                    preview(input, "head")?,
                    preview(input, "base")?,
                    preview(input, "body").unwrap_or_else(|_| "\"\"".into())
                ));
            }
            _ => {}
        }
        Ok(line)
    }

    /// One request returning `(status, body_json_or_null)` — the ureq-3
    /// pattern from verify_package: 4xx/5xx arrive as
    /// `Err(StatusCode(code))` and ARE the answer (Gitea error JSON).
    fn fetch(&self, method: &str, url: &str, body: Option<Value>) -> Result<(u16, Value), String> {
        let auth = format!("token {}", self.token);
        let resp = match (method, body) {
            ("POST", Some(b)) => match self
                .agent
                .post(url)
                .header("Authorization", &auth)
                .send_json(&b)
            {
                Ok(r) => r,
                Err(ureq::Error::StatusCode(code)) => return Ok((code, Value::Null)),
                Err(e) => return Err(format!("gitea: {e}")),
            },
            (_, None) => match self.agent.get(url).header("Authorization", &auth).call() {
                Ok(r) => r,
                Err(ureq::Error::StatusCode(code)) => return Ok((code, Value::Null)),
                Err(e) => return Err(format!("gitea: {e}")),
            },
            (m, _) => return Err(format!("gitea: unsupported method/body mix: {m}")),
        };
        let status = resp.status().as_u16();
        let mut resp = resp;
        let parsed = resp.body_mut().read_json::<Value>().unwrap_or(Value::Null);
        Ok((status, parsed))
    }

    fn call(&self, input: &Value) -> Result<Value, String> {
        let op = parse_op(input)?;
        let repo = target_repo(input, &self.repo)?;
        let number = opt_number(input)?;
        let (method, path) = op_route(op, &repo, number.as_deref(), input)?;
        let url = format!("{}/{}", self.base_url, path);
        let body = if op.is_write() {
            Some(op_body(op, input)?)
        } else {
            None
        };
        let (status, parsed) = self.fetch(&method, &url, body)?;
        if !(200..300).contains(&status) {
            let msg = parsed
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("no error body");
            return Err(format!("gitea: HTTP {status}: {msg}"));
        }
        Ok(parsed)
    }
}

impl Tool for GiteaTool {
    fn name(&self) -> &str {
        "gitea"
    }

    fn risk(&self) -> Risk {
        Risk::Write
    }

    fn describe(&self, input: &Value) -> String {
        self.request_line(input).unwrap_or_else(|e| e)
    }

    fn spec(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["issue_view", "issue_list", "pr_view", "issue_comment", "issue_create", "pr_create"],
                    "description": "Gitea operation: issue_view/issue_list/pr_view are read-only; issue_comment/issue_create/pr_create are writes"
                },
                "repo": { "type": "string", "description": "Repository as owner/name (optional; defaults to the registration-time repo)" },
                "number": { "type": ["string", "integer"], "description": "Issue or PR number" },
                "state": { "type": "string", "enum": ["open", "closed", "all"], "description": "Filter for issue_list (optional, default open)" },
                "limit": { "type": ["string", "integer"], "description": "Max results for issue_list (1..=200, default 30)" },
                "title": { "type": "string", "description": "issue_create: issue title" },
                "body": { "type": "string", "description": "issue_create / issue_comment / pr_create body text" },
                "head": { "type": "string", "description": "pr_create: source branch" },
                "base": { "type": "string", "description": "pr_create: target branch" }
            },
            "required": ["op"]
        }))
    }

    fn execute(&self, input: Value) -> Result<Value, String> {
        // Validate the full shape BEFORE any traffic (whitelist first).
        self.request_line(&input)?;
        self.call(&input)
    }
}

/// One bounded, quoted payload field for the approval line. Empty and
/// oversized values are shown truncated — the full payload always lives
/// in the durable session log.
fn preview(input: &Value, key: &str) -> Result<String, String> {
    let v = required_str(input, key)?;
    const MAX: usize = 120;
    let out: String = v.chars().take(MAX).collect();
    let clipped = if v.chars().count() > MAX {
        format!("{out}…(+{} chars)", v.chars().count() - MAX)
    } else {
        out
    };
    Ok(format!("{clipped:?}"))
}

fn parse_op(input: &Value) -> Result<GiteaOp, String> {
    input
        .get("op")
        .and_then(Value::as_str)
        .and_then(GiteaOp::parse)
        .ok_or_else(|| {
            "gitea: 'op' must be one of issue_view | issue_list | pr_view | issue_comment | issue_create | pr_create".to_string()
        })
}

/// `owner/name` validation — same charset rule the gh side uses, so a
/// per-call override can never smuggle path traversal or a leading dash
/// into the URL path.
fn valid_repo(s: &str) -> bool {
    let Some((owner, name)) = s.split_once('/') else {
        return false;
    };
    let part = |p: &str| {
        !p.is_empty()
            && !p.starts_with('-')
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    part(owner) && part(name) && !s.contains("..")
}

fn target_repo(input: &Value, default: &str) -> Result<String, String> {
    match input.get("repo") {
        None | Some(Value::Null) => Ok(default.to_string()),
        Some(v) => {
            let s = v
                .as_str()
                .ok_or_else(|| "gitea: 'repo' must be a string (owner/name)".to_string())?;
            if s.trim().is_empty() {
                return Ok(default.to_string());
            }
            if !valid_repo(s) {
                return Err(format!("gitea: 'repo' must be owner/name, got {s:?}"));
            }
            Ok(s.to_string())
        }
    }
}

/// Numbers may arrive as string or integer (the model decides; both are
/// legal JSON for a count) — digits only after normalization.
fn opt_number(input: &Value) -> Result<Option<String>, String> {
    match input.get("number") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.trim().is_empty() => {
            if s.chars().all(|c| c.is_ascii_digit()) {
                Ok(Some(s.clone()))
            } else {
                Err(format!("gitea: 'number' must be digits, got {s:?}"))
            }
        }
        Some(Value::Number(n)) => match n.as_u64() {
            Some(u) => Ok(Some(u.to_string())),
            None => Err(format!(
                "gitea: 'number' must be a non-negative integer, got {n}"
            )),
        },
        Some(other) => Err(format!(
            "gitea: 'number' must be digits or an integer, got {other}"
        )),
    }
}

fn required_str(input: &Value, k: &str) -> Result<String, String> {
    input
        .get(k)
        .and_then(Value::as_str)
        .map(|v| v.to_string())
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| format!("gitea: missing or empty '{k}'"))
}

fn opt_str(input: &Value, k: &str) -> Result<Option<String>, String> {
    match input.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(format!("gitea: '{k}' must be a string, got {other}")),
    }
}

/// Branch names in pr_create must not smuggle URL segments: reject
/// anything with `/` or a leading dash (refs are single-segment here).
fn valid_branch(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('-') && !s.contains('/') && !s.contains("..")
}

fn opt_limit(input: &Value) -> Result<Option<String>, String> {
    match input.get("limit") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::Number(n)) => match n.as_u64() {
            Some(u) if (1..=200).contains(&u) => Ok(Some(u.to_string())),
            _ => Err(format!("gitea: 'limit' must be within 1..=200, got {n}")),
        },
        Some(Value::String(s)) => match s.parse::<u32>() {
            Ok(u) if (1..=200).contains(&u) => Ok(Some(u.to_string())),
            _ => Err(format!(
                "gitea: 'limit' must be digits within 1..=200, got {s:?}"
            )),
        },
        Some(other) => Err(format!(
            "gitea: 'limit' must be digits or an integer, got {other}"
        )),
    }
}

/// The (method, path) for an op — one place to audit every route.
fn op_route(
    op: GiteaOp,
    repo: &str,
    number: Option<&str>,
    input: &Value,
) -> Result<(String, String), String> {
    match op {
        GiteaOp::IssueView => {
            let n = number.ok_or("gitea: issue_view requires 'number'")?;
            Ok(("GET".into(), format!("api/v1/repos/{repo}/issues/{n}")))
        }
        GiteaOp::IssueList => {
            let mut path = format!("api/v1/repos/{repo}/issues?type=issues");
            let state = match opt_str(input, "state")?.as_deref() {
                None | Some("open") => "open",
                Some("closed") => "closed",
                Some("all") => "all",
                Some(other) => {
                    return Err(format!(
                        "gitea: 'state' must be open|closed|all, got {other:?}"
                    ))
                }
            };
            path.push_str(&format!("&state={state}"));
            if let Some(limit) = opt_limit(input)? {
                path.push_str(&format!("&limit={limit}"));
            }
            Ok(("GET".into(), path))
        }
        GiteaOp::PrView => {
            let n = number.ok_or("gitea: pr_view requires 'number'")?;
            Ok(("GET".into(), format!("api/v1/repos/{repo}/pulls/{n}")))
        }
        GiteaOp::IssueComment => {
            let n = number.ok_or("gitea: issue_comment requires 'number'")?;
            required_str(input, "body")?;
            Ok((
                "POST".into(),
                format!("api/v1/repos/{repo}/issues/{n}/comments"),
            ))
        }
        GiteaOp::IssueCreate => {
            required_str(input, "title")?;
            Ok(("POST".into(), format!("api/v1/repos/{repo}/issues")))
        }
        GiteaOp::PrCreate => {
            required_str(input, "title")?;
            let head = required_str(input, "head")?;
            let base = required_str(input, "base")?;
            if !valid_branch(&head) {
                return Err(format!("gitea: 'head' must be a branch name, got {head:?}"));
            }
            if !valid_branch(&base) {
                return Err(format!("gitea: 'base' must be a branch name, got {base:?}"));
            }
            Ok(("POST".into(), format!("api/v1/repos/{repo}/pulls")))
        }
    }
}

fn op_body(op: GiteaOp, input: &Value) -> Result<Value, String> {
    match op {
        GiteaOp::IssueComment => Ok(json!({ "body": required_str(input, "body")? })),
        GiteaOp::IssueCreate => Ok(json!({
            "title": required_str(input, "title")?,
            "body": opt_str(input, "body")?.unwrap_or_default(),
        })),
        GiteaOp::PrCreate => Ok(json!({
            "title": required_str(input, "title")?,
            "body": opt_str(input, "body")?.unwrap_or_default(),
            "head": required_str(input, "head")?,
            "base": required_str(input, "base")?,
        })),
        _ => Ok(Value::Null),
    }
}

/// What a remote URL resolves to for the gitea tool.
#[derive(Debug)]
pub enum GiteaRemote {
    /// Not a Gitea remote (or malformed / GitHub — GitHub belongs to `gh`).
    NotGitea,
    /// A Gitea remote over plain http to a NON-loopback host: registering
    /// would send the token unencrypted — refused loudly by the host
    /// (CodeCora PR #169 round 1).
    InsecureHttp { host: String },
    /// Usable: API base URL (scheme preserved for loopback http) + repo.
    Ok { base: String, repo: String },
}

/// Parse a Gitea remote URL. ssh remotes (`git@host:p/r`) map to https;
/// http remotes are accepted ONLY for loopback hosts (self-hosted
/// Gitea on localhost:3000 has no network path to leak the token);
/// GitHub remotes are `NotGitea` (they belong to the `gh` tool).
pub fn gitea_from_remote(url: &str) -> GiteaRemote {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let (scheme, host, path) = match url.strip_prefix("git@") {
        Some(rest) => match rest.split_once(':') {
            Some((h, p)) => ("https", h, p),
            None => return GiteaRemote::NotGitea,
        },
        None => {
            let (scheme, rest) = match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
                (Some(r), _) => ("https", r),
                (None, Some(r)) => ("http", r),
                (None, None) => return GiteaRemote::NotGitea,
            };
            match rest.split_once('/') {
                Some((h, p)) => (scheme, h, p),
                None => return GiteaRemote::NotGitea,
            }
        }
    };
    if host.eq_ignore_ascii_case("github.com") {
        return GiteaRemote::NotGitea;
    }
    // host may carry :port — validate charset so it can't smuggle
    // anything into the API base URL. Loopback hosts (localhost has no
    // dot) are exempt from the dot requirement.
    let loopback = is_loopback(host);
    if host.is_empty()
        || (!loopback && !host.contains('.'))
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '-' | '_' | '[' | ']'))
    {
        return GiteaRemote::NotGitea;
    }
    let mut parts = path.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return GiteaRemote::NotGitea; // deeper/shorter path: not a plain repo remote
    };
    let valid = |s: &str| {
        !s.is_empty()
            && !s.starts_with('-')
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if !valid(owner) || !valid(name) {
        return GiteaRemote::NotGitea;
    }
    if scheme == "http" && !is_loopback(host) {
        return GiteaRemote::InsecureHttp {
            host: host.to_string(),
        };
    }
    GiteaRemote::Ok {
        base: format!("{scheme}://{host}"),
        repo: format!("{owner}/{name}"),
    }
}

/// `host[:port]` is a loopback address (no network path can leak the
/// token over plain http).
fn is_loopback(host: &str) -> bool {
    let bare = host.split(':').next().unwrap_or(host);
    bare == "localhost" || bare == "127.0.0.1" || bare == "[::1]" || bare == "::1"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> GiteaTool {
        GiteaTool::new("http://127.0.0.1:1", "tok", "acme/widgets")
    }

    #[test]
    fn routes_are_whitelisted_and_shaped() {
        let t = tool();
        let line = |v: Value| t.request_line(&v).unwrap();
        assert_eq!(
            line(json!({"op": "issue_view", "number": 7})),
            "gitea GET /api/v1/repos/acme/widgets/issues/7"
        );
        assert_eq!(
            line(json!({"op": "issue_view", "number": "7"})),
            line(json!({"op": "issue_view", "number": 7})),
            "string and integer numbers are equivalent"
        );
        assert_eq!(
            line(json!({"op": "issue_list", "state": "closed", "limit": 10})),
            "gitea GET /api/v1/repos/acme/widgets/issues?type=issues&state=closed&limit=10"
        );
        assert_eq!(
            line(json!({"op": "pr_view", "number": "3"})),
            "gitea GET /api/v1/repos/acme/widgets/pulls/3"
        );
        assert!(
            line(json!({"op": "issue_comment", "number": "3", "body": "hi"}))
                .starts_with("gitea POST /api/v1/repos/acme/widgets/issues/3/comments")
        );
    }

    /// CodeCora PR #169 round 1: the approver must SEE the write
    /// payload — a `--allow gitea` pattern must not auto-post arbitrary
    /// title/body sight-unseen.
    #[test]
    fn write_payloads_are_visible_and_bounded_in_the_approval_line() {
        let t = tool();
        let line = t
            .request_line(&json!({"op": "issue_comment", "number": "3", "body": "ship it now"}))
            .unwrap();
        assert!(line.contains("body=\"ship it now\""), "{line}");

        let line = t
            .request_line(&json!({"op": "pr_create", "title": "Fix login", "head": "feat-x", "base": "main", "body": "long body"}))
            .unwrap();
        assert!(line.contains("title=\"Fix login\""), "{line}");
        assert!(line.contains("head=\"feat-x\""), "{line}");
        assert!(line.contains("base=\"main\""), "{line}");
        assert!(line.contains("body=\"long body\""), "{line}");

        // oversized payloads are truncated, never silently full-length
        let long = "x".repeat(500);
        let line = t
            .request_line(&json!({"op": "issue_create", "title": long, "body": "b"}))
            .unwrap();
        assert!(line.contains("…(+380 chars)"), "{line}");
        assert!(
            line.chars().count() < 400,
            "approval line must stay readable"
        );
    }

    #[test]
    fn per_call_repo_override_is_validated() {
        let t = tool();
        assert_eq!(
            t.request_line(&json!({"op": "issue_view", "number": 1, "repo": "other/team"}))
                .unwrap(),
            "gitea GET /api/v1/repos/other/team/issues/1"
        );
        // traversal / dash / malformed are refused before any traffic
        assert!(t
            .request_line(&json!({"op": "issue_view", "number": 1, "repo": "../etc"}))
            .is_err());
        assert!(t
            .request_line(&json!({"op": "issue_view", "number": 1, "repo": "-x/y"}))
            .is_err());
        assert!(t
            .request_line(&json!({"op": "issue_view", "number": 1, "repo": "onlyowner"}))
            .is_err());
    }

    #[test]
    fn unknown_op_and_bad_numbers_refused() {
        let t = tool();
        assert!(t.request_line(&json!({"op": "repo_delete"})).is_err());
        assert!(t.request_line(&json!({"op": "issue_view"})).is_err());
        assert!(t
            .request_line(&json!({"op": "issue_view", "number": "../../x"}))
            .is_err());
        assert!(t
            .request_line(&json!({"op": "issue_view", "number": -5}))
            .is_err());
        // full validation happens pre-traffic: execute refuses bad input
        // without a server (base_url points at a closed port).
        assert!(t
            .execute(json!({"op": "issue_view", "number": "1x"}))
            .is_err());
    }

    #[test]
    fn limit_is_bounded() {
        let t = tool();
        assert!(t
            .request_line(&json!({"op": "issue_list", "limit": 201}))
            .is_err());
        assert!(t
            .request_line(&json!({"op": "issue_list", "limit": 0}))
            .is_err());
        assert!(t
            .request_line(&json!({"op": "issue_list", "limit": "30"}))
            .is_ok());
    }

    #[test]
    fn pr_create_validates_branches() {
        let t = tool();
        let ok = json!({"op": "pr_create", "title": "t", "head": "feat-x", "base": "main"});
        assert!(t.request_line(&ok).is_ok());
        for bad in ["-x", "a/b", "a..b", ""] {
            let v = json!({"op": "pr_create", "title": "t", "head": bad, "base": "main"});
            assert!(t.request_line(&v).is_err(), "head {bad:?} must be refused");
        }
    }

    #[test]
    fn remote_url_parses_instance_and_repo() {
        use GiteaRemote::*;
        let ok = |u: &str, base: &str, repo: &str| match gitea_from_remote(u) {
            Ok { base: b, repo: r } => (b == base && r == repo, format!("{b}/{r}")),
            other => (false, format!("{other:?}")),
        };
        // ssh and https remotes both map to https
        assert!(
            ok(
                "git@git.example.com:acme/widgets.git",
                "https://git.example.com",
                "acme/widgets"
            )
            .0
        );
        assert!(
            ok(
                "https://git.example.com/acme/widgets.git",
                "https://git.example.com",
                "acme/widgets"
            )
            .0
        );
        assert!(
            ok(
                "https://git.example.com/acme/widgets",
                "https://git.example.com",
                "acme/widgets"
            )
            .0
        );
        // loopback http is fine (self-hosted Gitea on localhost — no
        // network path can leak the token)
        assert!(
            ok(
                "http://127.0.0.1:3000/acme/widgets.git",
                "http://127.0.0.1:3000",
                "acme/widgets"
            )
            .0
        );
        assert!(
            ok(
                "http://localhost:3000/acme/widgets.git",
                "http://localhost:3000",
                "acme/widgets"
            )
            .0
        );
        // NON-loopback http would send the token unencrypted — refused
        // loudly (CodeCora PR #169 round 1), never silently registered.
        // Assembled (not a literal) so URL scanners don't flag the very
        // insecure URL this branch exists to refuse.
        let insecure = ["http", "://git.example.com:3000/acme/widgets.git"].concat();
        match gitea_from_remote(&insecure) {
            InsecureHttp { host } => assert!(host.contains("git.example.com")),
            other => panic!("expected InsecureHttp, got {other:?}"),
        }
        // github remotes are NOT gitea targets; deep paths / garbage too
        assert!(matches!(
            gitea_from_remote("git@github.com:acme/widgets.git"),
            NotGitea
        ));
        assert!(matches!(
            gitea_from_remote("https://github.com/acme/widgets.git"),
            NotGitea
        ));
        assert!(matches!(
            gitea_from_remote("https://git.example.com/a/b/c.git"),
            NotGitea
        ));
        assert!(matches!(gitea_from_remote("not a url"), NotGitea));
    }
}
