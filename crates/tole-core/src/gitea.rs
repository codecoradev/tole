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
    /// approval prompt. Deliberately excludes the token.
    pub fn request_line(&self, input: &Value) -> Result<String, String> {
        let op = parse_op(input)?;
        let repo = target_repo(input, &self.repo)?;
        let number = opt_number(input)?;
        let (method, path) = op_route(op, &repo, number.as_deref(), input)?;
        Ok(format!("gitea {method} /{path}"))
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

/// Parse a Gitea remote URL into `(api_base, owner/repo)`. GitHub remotes
/// return None (they belong to the `gh` tool). ssh remotes (`git@host:p/r`)
/// map to https; http(s) remotes keep their scheme and may carry a port
/// (`http://host:3000/owner/repo` — common for self-hosted Gitea).
pub fn gitea_from_remote(url: &str) -> Option<(String, String)> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let (scheme, host, path) = match url.strip_prefix("git@") {
        Some(rest) => {
            let (h, p) = rest.split_once(':')?;
            ("https", h, p)
        }
        None => {
            let (scheme, rest) = match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
                (Some(r), _) => ("https", r),
                (None, Some(r)) => ("http", r),
                (None, None) => return None,
            };
            let (h, p) = rest.split_once('/')?;
            (scheme, h, p)
        }
    };
    if host.eq_ignore_ascii_case("github.com") {
        return None;
    }
    // host may carry :port — validate charset so it can't smuggle
    // anything into the API base URL.
    if host.is_empty()
        || !host.contains('.')
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '-' | '_'))
    {
        return None;
    }
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let name = parts.next()?;
    if parts.next().is_some() {
        return None; // deeper path: not a plain repo remote
    }
    let valid = |s: &str| {
        !s.is_empty()
            && !s.starts_with('-')
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if !valid(owner) || !valid(name) {
        return None;
    }
    Some((format!("{scheme}://{host}"), format!("{owner}/{name}")))
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
        // ssh and https remotes both yield (base, owner/repo)
        assert_eq!(
            gitea_from_remote("git@git.example.com:acme/widgets.git"),
            Some((
                "https://git.example.com".to_string(),
                "acme/widgets".to_string()
            ))
        );
        assert_eq!(
            gitea_from_remote("https://git.example.com/acme/widgets.git"),
            Some((
                "https://git.example.com".to_string(),
                "acme/widgets".to_string()
            ))
        );
        assert_eq!(
            gitea_from_remote("https://git.example.com/acme/widgets"),
            Some((
                "https://git.example.com".to_string(),
                "acme/widgets".to_string()
            ))
        );
        // self-hosted Gitea commonly rides an explicit port
        assert_eq!(
            gitea_from_remote("http://git.internal:3000/acme/widgets.git"),
            Some((
                "http://git.internal:3000".to_string(),
                "acme/widgets".to_string()
            ))
        );
        // github remotes are NOT gitea targets
        assert_eq!(gitea_from_remote("git@github.com:acme/widgets.git"), None);
        assert_eq!(
            gitea_from_remote("https://github.com/acme/widgets.git"),
            None
        );
        // deep paths / garbage refuse
        assert_eq!(gitea_from_remote("https://git.example.com/a/b/c.git"), None);
        assert_eq!(gitea_from_remote("not a url"), None);
    }
}
