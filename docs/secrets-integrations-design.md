# Secrets across tole's own integrations — design note

Status: **research + proposal, nothing implemented.** Scope (owner
decision 2026-11-11): tole's **own** integrations first (LLM providers,
`tole serve`, nginjen web backends, MCP over HTTP including Composio and
uteke cloud, laya/Jev, uteke CLI and the memory loop, cora, gh/git/gitea);
third-party plugins come next. Builds on Part B of
[profiles-collaboration-secrets.md](profiles-collaboration-secrets.md).

## 1. Inventory (read from the code, tole @ this branch)

| Integration | Secret | How tole delivers it | Where it can leak today |
|---|---|---|---|
| LLM provider | `TOLE_API_KEY` / `OPENAI_API_KEY` | header to `TOLE_BASE_URL`; never persisted; **E11 redacts this one value** from every outgoing message | inherited by **child agents** (full env, by design) |
| Fallback provider (#209) | `TOLE_FALLBACK_API_KEY` | sent **only** to the fallback host; the primary key never goes there | — (good destination binding) |
| `tole serve` | `TOLE_SERVE_TOKEN` | bearer on the serve API | removed from children by name (ENV-1) |
| nginjen (`web_search` / `web_fetch`, #436/#532) | e.g. `NGINJEN_API_KEY`, referenced as `${NAME}` in `TOLE_WEB_SEARCH_HEADERS` / fetch headers | name in config, value in env, expanded at request time | the **query text** the model writes goes to the backend unredacted |
| MCP over HTTP (#530): Composio, uteke cloud, nginjen MCP | `TOLE_MCP_<NAME>_TOKEN` | sent only to that server's URL; scrubbed from errors | **tool arguments** go to the server unredacted; successful **tool results** are not scrubbed (documented residual); env name folds case |
| laya / Jev (`systemone_decide`) | `SYSTEMONE_API_KEY` | bearer to `SYSTEMONE_BASE_URL` | the decision `state` is model-controlled text |
| uteke CLI, memory loop | uteke's own config | subprocess, ENV-1-scrubbed env | **session summaries leave the process** to the uteke store (cloud when configured). uteke's write path has a pattern-based secret filter (`privacy.rs`, on by default); tole adds nothing |
| cora (auto-attached stdio MCP) | none needed for agent-as-reviewer | scrubbed connection env | — |
| gh / git | `GITHUB_TOKEN` | **kept in every child's env**, including `run_command` and `job_start` | any command the model runs can read it |
| gitea tool | `GITEA_TOKEN` | read in-process | removed from children by name |

## 2. Evidence and current practice

- **Name-based env filtering is table stakes.** Codex CLI's
  `shell_environment_policy` (`inherit` / `include_only` / `exclude`, with
  default excludes for names containing KEY, SECRET or TOKEN) matches tole's
  ENV-1. tole is at parity; the gaps are beyond names.
- **Memory is a leak path.** Long-running agents persist history; a
  credential pasted or read once can travel into long-term memory and to
  third parties (Rafter 2026, arunbaby.com, Doppler 2025). ISACA (2026)
  recommends retention limits and authorization checks on persistent
  memory.
- **MCP specification (2026-07-28), security best practices:** token
  passthrough is a named anti-pattern (MUST NOT); confused-deputy risks
  apply to proxying servers.
- From the earlier rounds: AI-assisted code leaks secrets at ~2× the
  baseline (GitGuardian 2026); s1ngularity used installed agents to
  inventory secrets; credential brokering (placeholder + proxy) is where
  the industry converged for strong isolation.

## 3. Principles

1. **One secret, one destination.** Each secret is bound to the host or
   tool that needs it (already true for the fallback key and the MCP
   tokens). Generalize it.
2. **Every egress sink redacts.** Anything that leaves the tole process
   passes through one redactor.
3. **Processes get only what they are granted.** Default: no secrets in a
   child's environment; grants are explicit, per tool and per profile.
4. **Never pass a token through.** A token received by `tole serve` / `tole
   mcp` is never forwarded downstream, and an MCP server's token is never
   sent to another server (MCP spec).

## 4. Design: `SecretSet` applied at the sinks

```
SecretSet (tole-core)
  collect(env, config)       known values + detector rules
  redact(&str) -> Cow<str>   known values → «NAME»; detector hits → [REDACTED:kind]
  env_for(tool) -> grants    scoped release (principle 3)
  is_sensitive_path(&Path)   .env*, *.pem, id_*, .npmrc, .netrc, *.key
```

**Two detectors, complementary:**
- **Exact known values:** every env var named as secret, the `[secrets]`
  config, values expanded from `${NAME}` headers, and URL userinfo
  passwords. Catches random-looking tokens with no recognizable shape (a
  laya bearer, the serve token), which no pattern can.
- **Shape patterns:** catches secrets tole never knew (pasted by the
  owner, printed by a command). uteke already maintains such a ruleset with
  a precision-first policy (`crates/uteke-core/src/privacy.rs`: AWS,
  GitHub, npm, Slack, Google keys, bearer, JWT, URL passwords,
  `NAME=value` with entropy checks, an allowlist, idempotent). **Reuse it
  rather than writing a second one:** extract it into a small shared crate
  both projects depend on, so one ruleset protects both stores.

**Sinks** (each calls `redact` once):

| Sink | Today |
|---|---|
| provider request body | E11, provider key only |
| MCP tool arguments (to Composio, uteke cloud, …) | none |
| `web_search` query and `web_fetch` URL | none |
| `systemone` state | none |
| memory-loop summary, `uteke_document` | uteke's own pattern filter only |
| export (`/export`), handoff documents, round digests, skill drafts | none |

Deletion test: removing `SecretSet` would scatter the same logic across
roughly eight call sites, so it earns its keep. `scrub_env_for_child` and
`openai::scrub` become its callers (replace, don't layer).

**Scoped release (principle 3):**

```toml
[secrets]
GITHUB_TOKEN = { tools = ["gh", "git"] }
NGINJEN_API_KEY = { tools = ["web_search", "web_fetch"] }
```

- `run_command` / `job_start` receive **no** secrets by default. This closes
  the `GITHUB_TOKEN`-everywhere trade-off. It is a behaviour change: a
  `gh` call made through `run_command` loses auth, so the `gh` tool is the
  supported path, and an explicit grant restores it per project.
- Profiles (Part A) name the secrets they may use; a child receives the
  intersection of its parent's grant and its profile's.
- Child agents receive only the provider variables they need, not the
  parent's whole environment.

**Sensitive paths:** `read_file` on `.env*` and similar returns
`KEY=«redacted»` lines (names, not values); a raw read needs explicit
approval.

## 5. The session log: store raw or redacted?

Today the durable JSONL keeps tool results raw; E11's comment says the
log "keeps the original (local)". The memory loop, however, already
follows the rule "the durable log stores exactly what the provider saw".
Once every sink redacts, the provider never sees the raw value. Storing the
redacted form is then *more* faithful to the replay principle, and it stops
secrets from sitting in logs that get exported, synced or summarized.

**Proposal:** redact known values before the append, and record which
secret *names* were redacted in an entry field (never values). Replay stays
exact, because the provider saw the same redacted text. This is an owner
decision: it changes a documented behaviour.

## 6. Storage of the secrets themselves

- Keep environment variables as the baseline (servers, CI, systemd
  credentials).
- Optional OS keychain for interactive installs (a `tole secret set NAME`
  backed by the platform keychain). It is a convenience, not a vault.
- For unattended or webhook-driven work: run inside a credential-brokering
  sandbox (Docker Sandboxes, agentcage). tole's provider client honours
  `HTTPS_PROXY`, so placeholders and injection happen outside tole.
- `tole doctor` reports: which secrets are set, which tools may use each,
  values too short to redact safely, and any secret that is also present in
  a child's environment.

## 7. Order (own integrations)

1. `SecretSet` with known-value redaction at all sinks, plus the child
   minimum environment (closes the documented gaps, no UX change).
2. Shared shape-detector crate with uteke; wire it into the same sinks.
3. Scoped release (`[secrets]`, `run_command` without secrets by
   default), sensitive-path reads.
4. Log-at-rest redaction (after the §5 decision), `doctor` report, optional
   keychain.

Third-party plugins (MCP servers and skills from others) follow, on the
same principles plus supply-chain controls.

## 8. Open decisions

1. Session log: store the redacted form (recommended), or keep it raw?
2. Share uteke's `privacy.rs` ruleset through a small common crate
   (recommended), or keep separate copies?
3. Remove `GITHUB_TOKEN` from `run_command` by default (recommended, with
   the `gh` tool as the supported path), or keep the current trade-off?
