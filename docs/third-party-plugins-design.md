# Third-party plugins (MCP servers, skills) — design note

Status: **research + proposal; the lockfile, tool-manifest hash and
connect-time guard of §4.1–4.2 are implemented by #599 part 1, and
`tole plugin list|diff|approve|revoke` (§4.5) by part 2.** Second half of the
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

## 7. Decisions (owner, 2026-11-11)

1. The third-party surface is **MCP servers + skills only**; no in-process
   plugin code.
2. Sandbox: **detect and use an external wrapper** (`srt` or bubblewrap).
3. **Unverified third-party plugins are allowed only in interactive
   sessions, with the sandbox.**

Tracked in epic #594 (#599–#601).

## 7a. Former open decisions (resolved above)

1. Limit the third-party surface to MCP servers + skills, with no in-process
   plugin code (recommended)?
2. Sandbox: detect and use an external wrapper (`srt` or bubblewrap)
   (recommended), build tole's own wrapper, or require containers/VMs for
   third-party stdio servers?
3. Unverified third-party plugins: allowed in interactive sessions only,
   with a sandbox (recommended), or not at all?

## 8. Migration: the plugin lock (#599 part 1, BREAKING)

**What changed.** An MCP server, the auto-attached `cora` included, only
registers its tools when the owner approved its exact tool manifest and
version in the user plugin lock (`$CODECORA_HOME/tole/plugins.lock`,
default `~/.codecora/tole/plugins.lock`). There is no bypass.

**Approve cora once.** Start an interactive session in a terminal on that
machine (`tole`, `tole chat` or `tole run "..."`). Before the session
starts, tole prints cora's full tool list (every name, description and
input schema) and asks `Approve these tools? [y/N]`; answer `y`. The entry
is written to the user lock and tole uses cora from then on, in every
session and mission of that user. `tole doctor` shows the state of each
configured server. A cora upgrade (new version or new tools) asks again.

**Until then, a mission** (and `serve`, a delegated or child agent) runs
without cora: its tools are not registered, stderr says
`not approved in the plugin lock` with the remedy, the review gate stays
off and the mission prompt has no code-review section. It never asks and
never approves on its own.

**Cron and CI** never prompt, so approve ahead of time: run one interactive
session as the same user on that machine, or provide the user lock in the
`CODECORA_HOME` the job runs with (for example a `plugins.lock` approved on
a machine with the same server version, copied into the job's
`$CODECORA_HOME/tole/`). Re-approve after upgrading a server. A project lock
(`.tole/plugins.lock`) in the repository cannot stand in for it: it can only
narrow what the user lock approves.

**`tole plugin` (#599 part 2).** `tole plugin list` shows each server's
state, version, manifest hash, approval date and lock; `tole plugin diff
<name>` shows what changed since approval, tool by tool; `tole plugin
approve <name>` approves from a terminal, or from a script with
`--hash <manifest hash>` (cron and CI can approve this way once the hash is
reviewed); `tole plugin revoke <name>` blocks a server until it is approved
again. An approval made before part 2 (lock version 1) has no tool
snapshot: `diff` shows the live list in full instead, and approving again
stores one. Since #679 an approval is also bound to the configured command
line or URL and, for a stdio program that resolved to a file, to its
SHA-256: a different source or binary is refused at connect (and reported
by `list` and `diff`) until it is approved again. The entry's `secrets`
(the `mcp` grants held at approval) are the server's launch grant,
intersected with the current `[secrets]` release.

## 9. `tole plugin add`, scan and tiers (#600, first slice)

**Built.** `tole plugin list` and `diff` show source, program hash, secrets,
sandbox status and the trust tier, and never start a server the lock would
refuse (revoked, unapproved, another command line or URL, another program
file) nor anything under an agent session. `tole plugin add name=<command
line or https url>` starts the server, shows everything the model would read
(server text behind a bar, as data), the scan report (`tole_core::plugin_scan`:
hidden-instruction markers, mentions of other servers or tools, sensitive
paths, invisible and bidi Unicode, secret shapes, over-long text) and any
unpinned launcher (`npx`, `uvx`, `docker run` without an exact version), and
locks the server after the owner types `add <name> <first 8 hex of the
manifest hash>`. A bare `y` is not accepted and there is no `--yes`.

**Tier rules as built.**

| Tier | How it is set | What `add` and `approve` do |
|---|---|---|
| first-party | the name is on the owner's list: `cora`, `uteke`, `nginjen`, `laya`, plus `[sandbox] skip` | shared connection as a session would use; `add` refuses such a name (use `approve`) |
| third-party verified | a lock entry written by registry resolution (a later slice); nothing sets it yet | probe without the sandbox requirement |
| third-party unverified | everything else, and every server `add` installs | stdio: starts only inside the sandbox (`[sandbox] mode`, Linux with bubblewrap), otherwise refused before anything starts; remote HTTP needs none; interactive only |

Where there is no sandbox (macOS, Windows, Linux without bubblewrap, mode
`off`, or the name skipped) an unverified stdio server is refused. The owner
accepts a server explicitly by listing it under `[sandbox] skip`, which makes
it their own (first-party). A first-party claim in a lock entry is ignored: a
name must be on the owner's list. A `third-party-verified` claim in a lock
entry is ignored too for every decision (`approve`, `list`, `diff`): no ledger
line records a tier and nothing writes it yet, so it counts as unverified
(#841; a registry slice must record the tier in the ledger before it can be
honored).

`tole plugin approve` also decides bundle membership by the ledger: when the
lock's `bundle` record is not the one the last `approve` line recorded
(dropped, added or re-pinned by hand), it refuses, and only
`--manifest <file>` (reviewed like any bundle approval) goes on (#841).

**The probe.** `mcp::probe_manifest` starts the server on its own, lists the
tools, and kills the whole process group on every exit path (returned,
failed, timed out, panicked); the server gets no secret, its stderr is
discarded, and a new third-party server is locked with no secret grant (the
per-plugin grants of `SecretSet::env_for` are a later slice). The program
file's hash is taken before the start and checked after it; what is locked is
the hash taken before, the tool list that was shown, and nothing re-read.

**What is and is not protected.** The lock and the plugin ledger are files of
the owner. `revoke`, `approve` and `add` refuse under `TOLE_UNDER_SESSION` and
`TOLE_AGENT_DEPTH`, and the ledger is checked against the lock: a revoked
entry flipped back by hand, or a re-pinned manifest hash, is treated as
revoked until the owner approves again. A process that can rewrite the lock
and the ledger, or a copied `tole` run with a cleared environment, is not
stopped (same residual as #684). The scan is a report, never a safety label.
For `npx`, `uvx` and `docker` the integrity covers the launcher, not the
package: pin the version in the command line.

**Doctor.** `tole doctor` warns, without starting anything, about an unpinned
launcher in a configured server, a third-party stdio server no sandbox
applies to, and a broad grant (any secret on a third-party server, or more
than three on any server).

**Not built yet** (later slices): resolution through the official MCP Registry
with verified namespaces, skills from the uteke registry (bundle manifests and
per-plugin secret grants are §10). `add` does not edit the owner's config:
after it, configure the same command line (`mcp_server` or `--mcp-server`);
the approval is bound to exactly that source.

## Unreadable plugin ledger (#813 item 3)

A ledger that exists but cannot be read (a symlink, another non-file, an I/O
error, larger than 64 MiB) is no longer treated as "no history" by the
commands that write approvals. `tole plugin add` and `tole plugin approve`
refuse with a message naming the file and the repair (make it a regular
readable file again, or move it aside if losing that history is acceptable)
before anything is started or written. A missing ledger is still fine: there
is nothing to compare. `tole doctor` (the plugins section) and `tole plugin
list|diff` already reported the unreadable ledger; the note now also says
that `add` and `approve` refuse until it is fixed. `tole plugin revoke` is
unchanged on purpose: it only narrows trust, so it must keep working.
Sessions that connect servers keep failing closed as before: revocations
recorded in an unreadable ledger cannot be checked, and the note says so.

## 10. Bundle manifests and per-plugin secret grants (#600 slice 2, #811)

Built in two parts: part 1 is the manifest format, the lock record, the
connect-time pin and the ledger checks; part 2 the commands (`add --bundle`,
per-secret approval, `revoke --secret`).

**The manifest.** `tole-plugin.toml` (strict TOML, unknown keys refused, at
most 64 KiB, no control characters) holds `name`, `version`, optional
`description` and `paths`, one `[mcp_servers.<name>]` table per server (`run`
= a command line, or `url`; `secrets` = names it asks for) and optional
`[[skills]]` and `[[hooks]]` declarations. A remote server cannot ask for
secrets; a secret name must be upper case and secret-shaped
(`is_secret_name`). Parsing starts nothing.

**Decisions where the design left room** (the more restrictive option in
each case):

1. *What a grant covers.* The NAME of one environment variable, delivered to
   the stdio process of that one server when it starts. Not another server of
   the same bundle (each server is granted separately), not a remote server
   (refused in the manifest), not a child tole starts for someone else. It
   never widens the owner's `[secrets]` release: the secret must also be
   released to `mcp`, otherwise the grant is recorded and passes nothing.
   Redaction is unchanged and covers every secret, granted or not.
2. *The manifest is pinned by exact bytes.* The lock entry's `bundle` record
   holds the SHA-256 of the file and its absolute path (lock format version
   3); before a server is started and at every connect the file is hashed
   again. A different hash, a missing or unreadable file, or a path that is no
   regular file blocks the server (`Admission::BundleChanged`, a ledger
   `bundle_change`) and its grants do not apply, until it is approved again. A
   comment-only edit counts: nothing is parsed to decide that a change is
   harmless.
3. *Ledger.* A bundle approval writes `approve` with the manifest and bundle
   hashes, then one `secret_grant` line per granted secret; a withdrawal
   writes `secret_revoke` (`secret_revoke_project` for a project-lock revoke,
   which only narrows). The ledger is the authority and is compared in both
   directions: an entry is blocked when its bundle record is not the last
   approved one (re-pinned, removed, or added where the approval was plain),
   or it holds a secret the ledger does not show as granted since the last
   approval (or shows as revoked), or it carries a bundle record with no
   approval line at all. A plain entry's own `secrets` (#679) are tracked the
   same way since #825: a plain approval writes `approve` with a
   `secrets_recorded` marker, then one `secret_grant` line per grant, and a
   lock grant the ledger does not show is blocked. *Migration:* an `approve`
   line without the marker (an entry approved before) cannot vouch for its
   secrets, so such an entry is reported (`LedgerCheck::secrets_unrecorded`,
   a note in `list` and `doctor`), never blocked; it becomes tracked at its
   next approval or when the owner runs `revoke --secret` on it **at a terminal
   without `--hash`** (the prompt lists the grants that stay and the ledger
   records them; nothing is adopted for an entry already tracked). A script's
   `revoke --secret`, or one with `--hash`, only narrows: it adopts nothing, so
   it cannot make a hand-added secret permanent (#847). Until then a hand-added
   secret on such an entry is not detected.
   A hand-edited lock cannot add or restore a
   secret or drop the manifest pin, and an approval starts from no grant. Same residual as before: a process that can
   rewrite both files defeats it.
4. *Declared skills, hooks and paths.* Shown and covered by the file hash,
   never acted on: no skill is loaded and no hook is run from a bundle (hooks
   stay owner-only, §1; skills come through the registry, §4.4), and the
   declared paths are recorded for the sandbox slice, not enforced yet.

**Install.** `tole plugin add --bundle <file or directory>` is interactive
only. It shows the manifest behind a bar (the plugin wrote it: data), probes
each server the way `add name=<spec>` does (no secret, sandbox required for
an unverified one, the program hash taken before), shows the tool text and the
scan, and locks after the owner types `add <plugin> <first 8 hex of the
manifest file hash>`. Then every secret a server asks for is a separate
`[y/N]`, naming the secret and the server and saying whether it would be
passed now. Declining one does not stop the install. A plugin name already in
the lock is refused.

5. *Re-approval asks again.* `approve` on a bundle server re-reads the
   manifest (the recorded path, or `--manifest`), requires the configured
   command line to be the one it declares, shows both texts and asks for every
   secret anew: grants never carry over a re-approval. A script (no terminal)
   can approve with `--hash` and `--bundle-hash` and grants nothing; secrets
   are granted only at a terminal. `approve` is per server: after a manifest
   change each server of the bundle is approved on its own.
6. *Revoke.* `tole plugin revoke <server> --secret NAME` removes that one
   grant from the lock (the ledger line is written first, as for `revoke`) and
   leaves the server approved; it needs the same consent as `revoke`. It takes
   effect when the server is next started: a process that already runs keeps
   the value, which is why the output says to rotate the secret. `revoke
   <server>` blocks the whole server and lists its grants, as before.

7. *The sandbox profile is part of the approval (#750).* A lock entry
   (format version 4) carries the profile name and a hash of its effective
   grants (`sandbox`, `sandbox_hash`; `docs/sandbox-design.md` §6.1 says what
   is hashed). Before a server starts and at every connect the profile it
   would run in now is compared with the record; a different name or hash, or
   a sandbox that no longer applies, blocks it (`Admission::ProfileChanged`, a
   ledger `profile_change`, `changed (sandbox profile)` in `list`). The
   ledger is authoritative like for secrets: each approval is followed by a
   `sandbox_profile` line and an entry whose record differs from it is
   blocked. Re-approval is interactive only (a script's `--hash` is refused
   for a changed profile). The move is decided from the ledger even when the
   lock has no entry for the server (#847), so deleting the entry does not
   change that. A server the owner REVOKED while it ran in a sandbox profile is
   re-approved only at a terminal, whatever the lock says (entry kept, edited
   or deleted) and even with an unchanged profile: a script cannot undo that
   revoke. A revoked server that ran unsandboxed promised nothing and can still
   be re-approved by a script with `--hash` (cron and CI approve this way). *Migration:* an approval from before has no
   ledger line and no hash; `LedgerCheck::profile_unrecorded` reports it in
   `list` and `doctor`, never blocks it, and the next `tole plugin approve`
   records the profile.

**Out of scope here:** registry resolution, uteke registry skills,
enforcement of the declared paths, a bundle-wide approve.

## The skills pin (#600 slice 3a)

**Skills.** The trust anchor stays the approved hash of the whole skill
directory (skills design §4.2): any file changed after approval is refused,
under `required` approval (the default), in every run including cron and
`serve`. New in this slice:

- `tole skills approve <name> --source-url U --registry-id R --pinned-sha S`
  records the stated origin next to the approved bytes. It is **data**: no
  network fetch or verification (the MCP Registry namespace check is skipped by
  the owner, and no uteke registry API exists in this repo to verify against).
  The SHA is validated for shape only (40 or 64 lowercase hex).
- Every skill approve and revoke is appended to `skill-ledger.jsonl` next to
  the approvals file, before the approvals file is written. The ledger is
  authoritative for the skills it names: an approval whose last ledger line is
  a revoke, or whose hash differs from the ledger's, is dropped at load with a
  note. An approval the ledger never names (made before this slice, or after
  the ledger file was deleted) still loads, is reported once at startup as
  unrecorded, and is not vouched for (`tole skills adopt` records it; since
  #879 a checkpoint catches a deleted ledger, see below): **deleting
  `skill-ledger.jsonl` on an approvals file without a checkpoint makes every
  approval unrecorded and so forgets every revoke.** An unreadable or over-16-MiB ledger fails startup with a remedy; a
  last line torn by a crash is repaired by the next append. Same residual as
  the plugin ledger: a process that can rewrite or delete both files is not
  stopped.
- `tole skills revoke` and `tole skills drafts revoke` are owner-only like
  `approve` (and `drafts promote`).

### Skill ledger checkpoint and `adopt` (#879)

The first cut left four gaps; this closes them as far as a file-based record
can go.

- **Checkpoint.** Every owner write (`approve`, `revoke`, `adopt`, a draft
  promotion or revoke-by-source) appends the ledger line first and then writes
  `skill-approvals.json` with a `ledger_checkpoint`: `{"bytes": N, "sha256":
  "sha256:..."}` of the ledger it just wrote (one read of the ledger, so the
  hash covers the bytes the checkpoint names). At load, an approvals file that
  carries a checkpoint is checked against the ledger read once: missing,
  shorter than `bytes`, or a first `bytes` bytes that hash differently is a
  fault. A fault blocks **every** approval (the skill is listed unavailable)
  with one `ERROR:` startup note naming what is wrong and the remedy, and the
  owner's write commands refuse with the same message instead of quietly
  re-recording; `tole skills list` and `plugin audit` still answer (audit
  reports the fault as an error). A ledger that only grew since the checkpoint
  is valid, so a crash between the ledger append and the approvals write is
  harmless, and the reverse (a checkpoint ahead of the ledger) cannot happen:
  the checkpoint is computed from the ledger as read after the append, and
  `tole` never shortens it. Two owner commands racing leave a checkpoint that
  is a prefix of the ledger at some moment, which is valid.
- **Recovery.** `tole skills recheckpoint` (owner only) prints the ledger as
  it is (path, size, digest, whether the old checkpoint holds, what it says
  about each approval) and, after a `y` or `--sha256 <shown digest>`, records
  it as the new checkpoint. It refuses a ledger that differs from the one
  shown. It changes no approval and brings back no revoke the old ledger held:
  review `tole skills list` afterwards.
- **Old files.** An approvals file without a checkpoint (written before this
  change, or only ever saved by an embedder) keeps loading; with a non-empty
  ledger it adds one note per startup until the first owner write adds the
  checkpoint. The rule for a MISSING field is deliberately "legacy, allowed":
  it cannot be told from an old install, and stripping it from the approvals
  file alone gains nothing because the ledger still applies in full; hiding a
  revoke that way needs a write to the ledger as well.
- **Adopt.** `tole skills adopt NAME... | --all` (owner only, refused under an
  agent session) appends an `approve` line with `"note":"adopted"` for each
  chosen approval the ledger does not name (hash and source exactly as stored),
  then refreshes the checkpoint. It lists what it adopts first, leaves skills
  the ledger already records alone, and refuses a name with no approval. It
  adds no trust: the approvals already loaded.
- **Embedders.** `Approvals::approve`/`approve_from`/`revoke` and
  `Approvals::save` change the approvals file only: they write no ledger line,
  so an approval made that way is `unrecorded` and a revoke leaves no revoke
  line. `save` keeps an existing checkpoint (it never strips one). To record a
  decision, append a `ledger_line` (any `LedgerStore`) and then call the
  additive `Approvals::save_checkpointed`, which verifies the old checkpoint,
  writes the new one and refuses a ledger that lost history.
- **Residuals, plainly.** A process able to rewrite BOTH `skill-ledger.jsonl`
  and `skill-approvals.json` forges a matching checkpoint (same as the plugin
  ledger); the checkpoint only catches deleting, truncating or rewriting the
  ledger alone. Lines appended after the last checkpoint write exist only
  after a crash between the two writes, and are unprotected until the next
  owner write. Appending lines to the ledger is not detected (it is
  append-only by design), but an `approve` line only matters for a skill the
  approvals file names. `plugin list` text output now flattens a newline or
  tab in a lock key like `plugin audit` does (`--json` keeps the key,
  escaped).

**Not built (follow-up):** pulling a skill from the uteke registry at a pinned
SHA into the local directory. That needs the `uteke skill` Cloud API, which
cannot be verified offline; until then a third-party skill is placed in a
skills directory by the owner and approved with its source stated.

## Audit (#600 slice 3b)

**`tole plugin audit [name] [--json]`** is the read-only report of §4.5. Per
server: tier, lock state, approved manifest hash against the live one (the
server is started exactly as `list` would, never one the lock refuses, never
under an agent session), binary integrity, secret grants, the sandbox profile
approved and in force, the scanner's findings (of the live list, or of the
approval's stored snapshot when nothing was started) and the last three
`sandbox_fallback` events of the plugin ledger. Per skill: status (approved,
approved but unrecorded in the skill ledger, changed since approval, refused
by the ledger, not approved), the approved hash against the one on disk, the
stated source and the last ledger lines. It prints no tool text beyond scanner
findings. Every string is redacted, terminal-safe and put on one line. It
writes nothing.
