# Third-party plugins (MCP servers, skills) — design note

Status: **research + proposal, nothing implemented.** Second half of the
integration track (own integrations first, see
[secrets-integrations-design.md](secrets-integrations-design.md)).
Related: [skills-registry-design.md](skills-registry-design.md).

## 1. What "plugin" means for tole

tole's extension surfaces today:
- **MCP servers**: stdio (`--mcp-server name=cmd …`) and Streamable HTTP
  (#530).
- **Skills** (SKILL.md).
- **Owner hooks** (`--on-pretool` / `--on-posttool` / `--on-turnend`).

Hooks are owner scripts and stay owner-only.

**Proposal: the third-party surface is MCP servers + skills only.** No
in-process plugin code (no dynamic libraries, no embedded scripting). A
"plugin" is at most a **bundle manifest** that groups an MCP server, its
skills and the secrets it needs, so they install and get revoked as one
unit. This follows the design rule that adding code is a last resort, and
it keeps third-party code outside tole's process.

## 2. Evidence

**MCP-specific attacks** (Invariant Labs, 2025):
- **Tool poisoning:** hidden instructions in a tool *description* (model-
  visible, rarely shown to the user) made Cursor read `~/.ssh/id_rsa` and
  config files and pass them in a parameter.
- **Rug pull:** a server changes its tool descriptions *after* the user
  approved it.
- **Shadowing:** one server's description rewrites how the model uses
  *another, trusted* server's tool (all mail redirected to the attacker),
  without the malicious tool ever being called.
- Their mitigations: show the full model-visible description; **pin the
  server version and a hash of its tool descriptions**; enforce boundaries
  and dataflow between servers.

**Real incidents and measurements:**
- **postmark-mcp** (npm, Sept 2025): an impersonating package BCC'd every
  email to an attacker; 1,643 downloads before removal. "A single backdoored
  line executes hundreds of times daily without human review" (ReversingLabs).
- USENIX Security 2026 (*Detecting and Understanding Malicious Agent Skills
  in the Wild*): 98,380 skills analysed, **157 confirmed malicious**
  (0.16% of the corpus).
- *Agent Skills in the Wild*: **26.1%** of community skills have at least
  one vulnerability.
- ClawHub: 386 malicious skills (competitor audit).
- SkillJack: skill backdoors persist after their source is deleted.

**Sandboxes in other agents** (Riptides measurement, Sept 2026):
- Claude Code, Codex, Gemini CLI, Copilot CLI and Cursor all ship a
  sandbox, mostly bubblewrap or Landlock/seccomp on Linux and Seatbelt on
  macOS.
- Reads usually cover the whole machine.
- **MCP servers run outside the sandbox with full user rights in three of
  the five.**
- **In every one, credentials still reach the commands.**
- Process sandboxes have a steady stream of escapes (DuneSlide in Cursor,
  Codex writable root, a Docker-socket escape across three CLIs).

Conclusion: a sandbox is one perimeter, not the boundary. What a plugin
*receives* (secrets, files, other servers' context) matters as much.

**Provenance:** the official MCP Registry verifies namespaces:
`io.github.<user>/…` through GitHub OAuth, and `com.example/…` through DNS
TXT records.

## 3. tole today against these attacks

| Attack | tole now | Gap |
|---|---|---|
| Tool poisoning | descriptions sanitized and truncated (200 chars) on the **approval** line; every MCP tool is `Risk::Write`; results fenced | the **model** still sees the full description; nothing scans it |
| Rug pull | — | no pinning of versions or descriptions |
| Shadowing | tool names are namespaced `mcp_<server>_<tool>` | a description may still talk about other tools; all servers share one context |
| Malicious stdio server code | env scrubbed (ENV-1) | runs with the user's full filesystem and network authority |
| Token theft across servers | each HTTP token bound to its server (#530) | — |
| Data leaving through arguments | — | closed by the `SecretSet` sinks (secrets design) |

## 4. Design

### 4.1 Install: resolve → show → scan → approve → lock

`tole plugin add <spec>` (MCP server from npm/PyPI/OCI/binary/URL, or a
bundle):
1. **Resolve** to an exact version and an integrity hash (package tarball
   or binary sha256). Prefer an official MCP Registry entry with a verified
   namespace; record the provenance.
2. **Start it once in a probe mode** and fetch its tool list.
3. **Show the full model-visible text** of every tool description and
   input schema, not a summary (Invariant mitigation 1).
4. **Scan** the descriptions:
   - hidden-instruction markers (`<IMPORTANT>`, "do not tell the user");
   - references to other servers' or tools' names (shadowing);
   - sensitive paths (`~/.ssh`, `mcp.json`, `.env`);
   - invisible and bidi Unicode (reuse the rules in `sanitize.rs`);
   - secret shapes (the shared `privacy.rs` ruleset).

   Findings are a report, never a "safe" label.
5. **Owner approves** → write `.tole/plugins.lock` (project) or the
   user-level lock:
   - source and version
   - integrity hash
   - **tool-manifest hash** (names + descriptions + schemas)
   - granted secrets
   - sandbox profile
   - approving date

### 4.2 Run: verify, isolate, scope

- **Rug-pull guard:** on every connect, hash the live tool manifest. On a
  mismatch, that server's tools are **not registered**; `tole plugin diff`
  shows what changed, and the owner re-approves or revokes. Version bumps
  go through the same scan.
- **Secrets:** a plugin receives only its granted secrets (`SecretSet::
  env_for(plugin)`), never the provider key or other plugins' tokens.
- **Process isolation for stdio servers**, by tier:
  - First-party (cora, uteke, nginjen, laya): scrubbed env (today).
  - Third-party: run under an OS sandbox wrapper when one is available, with
    the manifest's declared filesystem paths and network hosts; deny reads
    of the home directory except the granted paths (the Copilot-style
    default the measurements favour).
  - Use an existing wrapper instead of building one: Anthropic's
    `sandbox-runtime` (`srt`, research preview, wraps any command with
    bubblewrap/Seatbelt and a domain-allowlist proxy) or plain
    bubblewrap, detected on PATH like cora and uteke.
  - When no wrapper is available, a third-party stdio server needs an
    explicit owner override.
  - Remote HTTP servers are isolated by nature (another machine), which
    makes them the preferred form for untrusted code.
- **Cross-server isolation:**
  - A profile lists the plugins it may load, so a profile that handles
    email does not share its context with an unvetted add-on.
  - Tool definitions are **retrieved by meaning when relevant**, the same
    seam as skills (Tool Search: −85% tokens, better selection), so most
    descriptions never enter the context at all.
- **Lazy start:** a plugin's process starts on first use and stops when
  idle. Dormant profiles start nothing (resource goal of the profile
  design).
- **Unattended runs** (cron, webhook) may only use plugins that are
  locked, approved and not marked unverified. An agent can never install,
  approve or re-approve a plugin.

### 4.3 Trust tiers

| Tier | Example | Rules |
|---|---|---|
| First-party | cora, uteke, laya, nginjen, Composio (own account) | lock + manifest hash; scrubbed env; still `Risk::Write` |
| Third-party, verified | verified namespace in the MCP Registry, uteke skill registry approved | full install flow; sandbox when available |
| Third-party, unverified | any other package or URL | explicit flag; sandbox required; interactive sessions only |

### 4.4 Third-party skills

- They come only through the uteke skill registry (Cloud): GitHub source,
  quarantine and scan, no modification. They are pulled at a pinned SHA,
  and tole loads them only when the local approved hash matches (skills
  design §4.2).
- The curator never modifies or merges third-party skills (Hermes rule);
  the dedup pass may only *flag* overlap with them.

### 4.5 Revoke and audit

- `tole plugin revoke <name>` marks the lock entry revoked (append-only).
  The server is never started again, and its granted secrets are listed so
  the owner can rotate them.
- `tole plugin list` / `audit` shows tier, source, hashes, grants, last
  manifest change and usage.
- `tole doctor` warns about unpinned servers, missing sandbox wrappers for
  third-party stdio servers, and plugins with broad grants.
- Every install, approval, manifest change and revoke is a ledger entry.

## 5. Not building

- In-process plugins (dynamic libraries, embedded scripting). WASM can be
  revisited later if a real need appears.
- A marketplace: MCP servers come from the official registry or a URL;
  skills from the uteke registry.
- Automatic updates, and installation by an agent.

## 6. Order

1. Lockfile + tool-manifest hash + rug-pull guard + full-description display
   (protects first-party servers too, no new dependency).
2. Description scanner + `plugin add/list/diff/revoke` + per-plugin secret
   grants.
3. Optional sandbox wrapper for third-party stdio servers; tiers in profiles
   and unattended runs.
4. Tool retrieval by meaning (shared seam with skills); lazy start.

## 7. Open decisions

1. Limit the third-party surface to MCP servers + skills, with no in-process
   plugin code (recommended)?
2. Sandbox: detect and use an external wrapper (`srt` or bubblewrap)
   (recommended), build tole's own wrapper, or require containers/VMs for
   third-party stdio servers?
3. Unverified third-party plugins: allowed in interactive sessions only,
   with a sandbox (recommended), or not at all?
