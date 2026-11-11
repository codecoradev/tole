# Skills: retrieval through uteke, growth with curation — design note

Status: **research + proposal; #590 part 1 implemented:** the
`SkillIndex` seam, the `uteke doc` adapter, pinned + top-K selection and
`skill_search` in `crates/tole-core/src/skill_index.rs` (not yet wired into
sessions). Part of the
integration/plugin track (scope order decided 2026-11-11: **own
integrations first, third-party second**). Related:
[profiles-collaboration-secrets.md](profiles-collaboration-secrets.md),
[automation-design.md](automation-design.md), and the uteke room
`uteke-skill-planning` (document `uteke-skill-registry-design`).

## 1. Owner direction (2026-11-11)

- Use uteke as the skill registry/repository; **recall only the top 5–10
  skills by meaning** per task, so the number of installed skills no longer
  drives token cost.
- Skills should **grow**: auto-created and auto-updated like Hermes, but
  **curated** so the library does not balloon.

## 2. How it works today

| | Hermes | tole (`crates/tole-core/src/skills.rs`, #161/#162) |
|---|---|---|
| What rides in every prompt | Level 0 `skills_list`: name + description + category of **every** skill (~3k tokens for its catalog), in the system prompt | `index_block`: one line `name — description` per discovered skill |
| Loading the body | `skill_view(name)` when the model decides | `load_skill` (ReadOnly) when the model decides; `--skill` pins one upfront |
| Growth | `skill_manage` create/patch/delete; background review stages changes; a curator consolidation pass; advisory linter (incident-log shape, >60 references, >24k-char body); optional write-approval gate | none |

So the owner's recollection is right in substance: in both, the **index of
all skills** is always in context and the model chooses. Cost and
selection quality both degrade linearly with the catalog.

## 3. Evidence

**Retrieval instead of listing everything:**
- Anthropic Tool Search Tool (2025): deferring definitions and searching
  on demand cut context use **85%** (~77K → ~8.7K tokens in their
  example) and raised MCP-eval accuracy (Opus 4: 49% → 74%; Opus 4.5:
  79.5% → 88.1%). Their guidance: keep the **3–5 most-used** always loaded,
  defer the rest; worthwhile beyond ~10 items or ~10K tokens.

**Do skills help, and which ones:**
- SkillsBench (arXiv 2602.12670): **curated** skills raise pass rate
  **+16.2 pp** on average (v4: 33.9% → 50.5%), but **16 of 84 tasks got
  worse**; **focused skills with 2–3 modules beat exhaustive ones**; and
  **self-generated skills provide no benefit on average** ("models cannot
  reliably author the procedural knowledge they benefit from consuming").
- *Are Online Skill and Memory Modules Always Worth Their Tokens?* (EMNLP
  2026, arXiv 2606.15017): against a **token-matched** baseline, AWM, ASI and
  ReasoningBank lose their apparent gains on WebArena; gains are domain-
  specific and run-to-run variance matters.

**Security of a growing library:**
- *SkillJack* (arXiv 2608.03509): poisoned experiences turned into skills
  by the agent itself. Extraction whitewashes intent (safety detection
  **98.5% → 11.4%**), attacks succeed 56–89%, **80% persist after the
  poisoned source records are deleted**, and some skills fire on benign
  queries.
- *Agent Skills in the Wild* (arXiv 2601.10338): **26.1%** of community
  skills contain at least one vulnerability (14 patterns). ClawHub: 386
  malicious skills (see the competitor audit).

**Conclusions:**
- Retrieval: yes, strongly supported.
- Growth: yes, but as **proposals under a gate**, never self-activating.
  The evidence says unreviewed self-authored skills do not pay and are a
  persistent attack surface.
- Every gain must be measured against a token-matched run without the skill.

## 4. Design

### 4.1 Exposure: pinned core + retrieved top-K

```
per task (start of a run, each delegation, each discussion participant):
  pinned   3–5 skills per profile, always listed       (owner-chosen)
  recalled top-K by meaning, K = 5 default, max 10       (uteke hybrid recall)
  on demand  `skill_search(query)` tool, ReadOnly        (Tool-Search pattern)
```

- **Recall query** = the owner-written task brief plus the profile role.
  Never raw tool output: a web page must not steer which instructions get
  loaded.
- **Similarity floor:** below it, return fewer (zero is fine). Irrelevant
  skills are not free; SkillsBench shows regressions.
- **Filters by meaning *and* by metadata:** status `approved`, profile
  scope, tags/domain, platform, and not `retired`.
- **Prompt cache:** the list is fixed at task start. Additions mid-task
  arrive as a one-time note in the next message (Hermes' approach), never
  as a system-prompt rewrite.
- Measured by the existing `usage_report` (index tokens, cache hits).

### 4.2 Trust: uteke finds, tole verifies

uteke wraps recall results as *data, not instructions*. A skill, however,
is meant to be followed. This is also an open question in
`uteke-skill-registry-design` §15. Split the roles:
- **uteke** stores the searchable index (name, description, tags,
  status) and returns candidates.
- **tole** loads the body from its **local, hash-pinned copy**, and only if
  that content hash matches an approval record (a ledger entry or lockfile
  written when the owner approved it). A recalled entry whose hash is not
  approved is shown as unavailable, never loaded.

The trust anchor is therefore the owner's approval of exact bytes, not the
fact that uteke returned it.

### 4.3 Where skills live

| Kind | Source of truth | Index |
|---|---|---|
| Own and agent-grown skills | a local git-tracked skills directory (today's `<workspace>/skills` and `~/.codecora/tole/skills`) | uteke documents (`uteke doc`, available in OSS and Cloud), tag `skill`, one parent document per category (#576 gives the hierarchy) |
| Third-party skills (phase 2) | `uteke skill` registry, **Cloud-only**, GitHub source, no-modification, quarantine → `pull` a pinned SHA into the local directory | the registry's own RRF search (BM25 + vectors) |

Constraint found in the uteke skill plan: `uteke skill` is Cloud-only and
v1 has **no `push`** (GitHub-sourced only), so agent-grown skills cannot
live there in v1. Hence local git + `uteke doc` index for own skills.

### 4.4 Growth: propose → gate → promote

```
trigger (correction by owner | dead end then working path | repeated workflow)
  → skill_propose (Write tier) creates a DRAFT with provenance
  → gate: lint · secret scan (SecretSet) · injection patterns · size
          · meaning-dedup against existing skills · taint check
  → review: owner (default) or curator profile + owner sign-off
  → promote: git commit, content hash approved, index in uteke
```

- **Drafts are never loadable.** Only approved hashes are.
- **Provenance** on every draft: source session ids, trigger, model. Lets
  the owner see where an instruction came from, and **revoke derived skills
  when a source turns out poisoned** (SkillJack's persistence-after-delete
  finding).
- **Taint:** a draft derived from a session that consumed untrusted
  content (web fetch, webhook payload, MCP result, third-party skill) is
  marked `tainted` and needs explicit owner review; it is never
  fast-tracked.
- **Meaning-dedup:** if a draft is semantically close to an existing skill,
  the proposal becomes a **patch** to that skill, not a new one. This is the
  main brake on library bloat.
- **Shape rules** (from SkillsBench and the Hermes linter): one procedure
  per skill, 2–3 modules at most, rules not incident logs, body size cap
  (today `MAX_SKILL_CHARS`).
- **Updates** are patches with a version history; rollback is a new commit,
  in the append-only spirit.

### 4.5 Curation over time

- **Usage ledger:** per skill, how often it was recalled, loaded, and the
  outcome of the runs that used it (success, failure, verify result).
- **A weekly curator job** runs on the scheduler from
  [automation-design.md](automation-design.md), as a `light`-tier
  profile. It proposes merges of near-duplicates, retirement of skills
  unused for N days, and retirement of skills whose runs do worse than
  comparable runs without them. Proposals only; the owner approves.
- **Retired** means not recalled, still on disk and in git, restorable.
- **Adoption gate**, same discipline as the laya router: the eval harness
  (`evals/` Tier 2) compares pinned-only, top-K retrieval, and top-K plus
  grown skills on success **and** tokens, against a token-matched baseline.

## 5. Open decisions

1. Own skills: local git + `uteke doc` index (recommended; works with uteke
   OSS and Cloud), or wait for `uteke skill` to accept pushes?
2. K = 5 default (max 10), with a similarity floor; pinned core 3–5 per
   profile?
3. Growth mode: **propose-only with owner approval** (recommended, given
   SkillsBench and SkillJack), or auto-promote untainted drafts that pass an
   eval?

## 6. Round 2 (2026-11-11): decisions, uteke in OSS, embedding dedup

**Decided:** K = 5 by default (max 10) with a similarity floor and a
pinned core of 3–5 per profile; growth is **propose-only with owner
approval**, plus meaning-based dedup that turns a near-duplicate into a
patch or a `references/` file instead of a new skill.

**Owner proposal:** move `uteke skill` into uteke **OSS** so everyone can
use it and tole can build on it directly.

### 6.1 What uteke OSS already has (read from `codecoradev/uteke` @ c98ff38)

- Embedding backends `onnx` (local, default), `openai` (any
  OpenAI-compatible `base_url`, e.g. the fleet's embor), `ollama`, `cohere`,
  plus an `embed_fallback` endpoint; precedence via `UTEKE_EMBEDDING_*`.
- Documents with a parent tree (max depth 10), hybrid/semantic search.
- Similarity machinery: after `remember`, `auto_link_cosine` adds
  `similar_to` edges at cosine ≥ 0.80 and `possible_duplicate` at ≥ 0.92;
  the dream cycle merges at `dedup_threshold` 0.92 and scans
  contradictions. **These run on memories, not on document chunks**
  (chunk keys are explicitly excluded as link targets), so skill dedup on
  documents is not automatic today.
- No skill-library feature in OSS (the only "skill" code ships uteke's own
  usage SKILL.md).

### 6.2 Should `uteke skill` move to OSS?

It reverses the 2026-11-10 uteke decision "Uteke Cloud only", so it is a
**uteke decision** to record there; tole should not block on it.

- Open-core rule of thumb (GitLab's buyer-based open core): features for
  an *individual* contributor are open; features a *team or organisation*
  buys are paid.
- A **personal skill library** fits OSS: index local skill directories,
  hybrid search, similarity report, validity lint, approved-hash records.
  It is useful to every uteke user and costs nothing to run.
- The **registry** stays Cloud: the shared pool, GitHub ingest with
  scanning at scale, private-repo sync, team sharing, yank notifications,
  quotas. These need runtime, cost money, and carry the legal exposure
  (DMCA/DSA) the uteke plan already describes. This matches the recorded
  uteke direction to monetise runtime services, not features.
- Caution from the same uteke analysis: the OSS feature surface is already
  large for one maintainer (a one-cycle OSS feature freeze was proposed), so
  a small, well-bounded OSS subset is safer than porting the registry.

**Recommendation for decision 1:** do **not** wait. tole builds against a
`SkillIndex` seam with a `uteke doc` adapter now (works with today's OSS
and Cloud). When an OSS `uteke skill` library lands, it becomes the second
adapter: a real seam, two adapters. Keep the CLI/HTTP boundary: embedding
`uteke-core` (~100k LOC, vector engines, ONNX runtime) into tole would cost
binary size and coupling, the same reasons cora was not embedded. Recall
runs once per task, not per turn, so a CLI or `uteke-server` call is
affordable; measure it. Without uteke, tole falls back to today's local
index.

### 6.3 Meaning-based dedup, made safe

Embedding similarity **finds candidates; it does not decide.** Embedding
models are weakly sensitive to negation (the Jina Embeddings paper built a
dedicated negation set for this reason), so "deploy to staging" vs "deploy to
production", or "always X" vs "never X", can score as near-duplicates.

| Cosine (calibrated per embedder) | Proposed action (owner approves) |
|---|---|
| ≥ 0.92 | **patch** the existing skill (new lesson into its procedure); no new skill |
| 0.80–0.92 | **reference**: add as `references/<topic>.md` of the closer skill, or propose an umbrella if two existing skills overlap; respect the 2–3 module limit (SkillsBench) |
| < 0.80 | new skill candidate |
| similar but contradictory | **conflict**: never merged automatically, shown side by side |

The verdict (patch / reference / new / conflict) comes from the curator
profile on the `light` tier (or a laya `choice` over the two short
descriptions), and the owner approves. The thresholds start at uteke's
memory values (0.80 / 0.92) and are recalibrated on our own library.

Embedding hygiene (from the embor notes): one embedder per store (local
ONNX and an API model are different vector spaces, and so are different
ONNX Runtime builds), so record the embedder id with each vector and
re-embed everything when switching. Texts above ~2048 tokens are silently
truncated, so embed the frontmatter + main procedure, and each reference
file separately. Skill text sent to an API embedder leaves the machine:
prefer local ONNX or the owner's own embor.

### 6.4 Lessons adopted from the Hermes curator

- Deterministic lifecycle without an LLM: active → stale → archived by
  inactivity (Hermes defaults 14 / 30 days); never-used skills get a grace
  period; skills referenced by a cron job are protected; archived is
  restorable.
- **pin** (no archiving) vs **lock** (no agent edits).
- LLM consolidation is **off by default** in Hermes: a sweep costs 50–100
  API calls and makes broad structural changes. Here it runs as a weekly
  proposal on the `light` tier, never applying changes itself.
- Package-aware moves: a skill with `references/`, `scripts/` or `assets/`
  moves as a whole package; never flatten only its SKILL.md into another
  skill's references.
- An append-only mutation ledger with before/after content hashes, a
  snapshot before any consolidation, single-entry rollback, a rename map,
  and `--dry-run`.
- The curator manages only agent-grown skills by default; hand-written and
  third-party skills are untouched unless the owner adopts them.

## 7. Round 3 decisions (2026-11-11)

1. **Start now:** tole builds against the `SkillIndex` seam with a
   `uteke doc` adapter (tag `skill` plus metadata), without waiting for an
   OSS skill library.
2. **Open-core split agreed** (uteke Cloud is heading toward teams):
   - OSS gets the personal skill library.
   - Cloud keeps the registry (pool, ingest and scanning, private sync,
     sharing, yank, quotas).
   - This changes the 2026-11-10 "Cloud only" decision; it is recorded on
     the uteke side.
3. **Similarity detection must cover documents**, not only memories. Today
   `auto_link_cosine` (similar_to ≥ 0.80, possible_duplicate ≥ 0.92) and the
   dream dedup run on memories; document chunks are excluded. Filed as codecoradev/uteke#1730.
4. **The curator is on by default and can be switched off** (config and
   per-run), and it can be paused, as in Hermes.

### 7.1 OSS skill storage: a `skill` tag on documents, or new tables?

The OSS `documents` table already has `tags` (JSON), `metadata` (JSON),
`content_type`, a parent tree, and `document_chunks` with embeddings. A
tag gets search for free, but a skill is more than one markdown document:

- it is a **package** (`references/`, `scripts/`, `assets/`);
- it has **lifecycle state** (draft, approved, stale, archived, pinned,
  locked);
- it has **approved content hashes per version**, **usage counters**, and a
  **mutation ledger**;
- the Cloud design already decided on a separate entity (`skills`,
  `skill_versions`, …).

**Recommendation: hybrid.**
- New tables in OSS for identity and lifecycle, with the same names and
  fields as the Cloud sketch so moving a library to Cloud is a copy, not a
  migration: `skills`, `skill_versions` (hash, status, validity), `skill_usage`,
  and a mutation ledger.
- The existing document/chunk/embedding engine stays the **only** search
  engine: each approved version's SKILL.md and reference files are indexed
  as documents with `content_type = 'skill'`, linked to the version.
- Those documents are hidden from ordinary `doc list` / `doc search` and
  recall unless asked for, so skills do not pollute document recall.
- One search engine, one embedding pipeline, and a clean lifecycle model.
- tole's phase-1 adapter uses a `skill` tag on plain documents; the
  phase-2 adapter talks to the new `uteke skill` commands.

### 7.2 uteke issues (filed 2026-11-11: codecoradev/uteke#1730, #1731, #1732)

1. **feat(docs): similarity edges and duplicate report for documents.**
   - Extend the cosine auto-link to documents: one document-level
     embedding (title + first section, or the mean of chunk vectors),
     `similar_to` / `possible_duplicate` edges between documents.
   - Add `uteke doc similar <slug> [--threshold]` and a namespace-scoped
     duplicate report.
   - Make the thresholds configurable (the code notes "configurable in
     future via graph config").
   - Keep contradiction checks out of scope for v1.
2. **feat: personal skill library in OSS (open-core split).**
   - The tables of 7.1, plus `uteke skill index <dir>`, `search`, `show`,
     `similar`, `lint`, and an approved-hash record.
   - Reuses the document engine.
   - No pool, no GitHub ingest, no sharing: those stay in Cloud.

