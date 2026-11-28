# Session store retention — design note

Status: **proposal for an owner decision** (issue #1020). Nothing here is
implemented and nothing in this note is decided: section 8 lists what the owner
has to decide, section 10 says what can be said about volume sizing today and
section 11 recommends an order. The note says what the code does today (with file references),
what retention can mean under the append-only rule, what each reading costs, and
what a reader or a resume sees afterwards. Anything not checked in the code is
marked **(not verified)**; section 9 lists them together.

Code references are to `origin/develop` at 50c7bcc (2026-11-26), except where a
sentence names a later change. Revision of 2026-11-28 (`origin/develop` at
af83a4e): the readiness route that landed after the first version (#1318) is
now stated in section 2, the container facts of #1325 are in section 10, and
sections 10 and 11 are new. The file and line references of sections 2 to 7 were
not re-read for this revision; a line number there can have moved, the symbol
named next to it is what to search for.

## 1. The rule this has to respect

`AGENTS.md` §6: session storage is append-only JSONL; entries are never mutated
or deleted; corrections are new entries. `docs/architecture.md` §4 says the same
(entries are write-once; registers are the only mutable state) and adds that one
file per session means "deletion = unlink" (line 125).

So "retention" cannot mean editing or trimming a session file. It can only mean
an operation on a **whole file** (keep it, move it, compress it, remove it) or a
**new** file that is a faithful copy of the old one. Whether §6 permits moving
or removing a whole file at all is itself an owner question (decision 1 in
section 8): the text forbids changing or deleting *entries*, and `architecture.md`
calls removal of a session file "deletion = unlink", but nothing says an owner
may or may not do it. Options C and D below assume the answer is yes; D is
literal deletion. This note also proposes, as a constraint for the owner to
accept, that such an operation is an owner command on the host and never a
model-callable tool (see "Model commands" in section 2).

## 2. What the code does today

Verified by reading; paths are relative to the repository root.

**Storage.** One file per session, `<id>.jsonl`, in the sessions directory
(default `<cwd>/.tole/sessions`, `crates/tole-cli/src/session/host.rs:819`;
`--sessions-dir` or the `sessions_dir` setting moves it, `serve` keeps one per
session workspace unless `-s` is given, `crates/tole-cli/src/serve.rs:1436`).
Next to each file sit `<id>.jsonl.lock` (a never-renamed writer-lock sidecar,
`crates/tole-core/src/session/storage.rs:1096`) and, only during `compact()`, a
`.jsonl.tmp`. So "the store" is one directory per project, not one global
directory. The files are created through `openat` with mode `0o666` before the
umask (`crates/tole-core/src/session/safe_dir.rs:164`), not `0600`; the global
index is `0600` (`crates/tole-cli/src/session/index.rs:82`).

**Growth.** Nothing removes or rotates a session file. A grep of `crates/` for
"archive", "prune", "retention" has many hits (skill curator, approvals, cron,
the webhook-delivery ledger `ledger/mod.rs`, `children/agents.rs`,
`provider/replay.rs`, `integrations/git.rs`, desktop UI code) but none is
retention of session files; I did not read every hit. The documented position for images is "not
pruned, an operator cleanup" (`docs/threat-model.md:893`) and, for serve uploads,
"not garbage-collected; removing a session does not remove its uploads"
(`docs/mobile-control.md:487`). Cron's `jobs.jsonl` and the approvals log are
append-only and not rotated either (`docs/automation-design.md:351`, `:533`,
`:1006`); they are outside this note.

**`Storage::compact()` exists but removes no entry.** It rewrites the file
through a temp file and an atomic rename, keeping every entry with its original
`seq`, and only drops superseded register values and deleted register cells
(`storage.rs:242`, `:1450`). It has no call site outside tests: the
`compact()` calls in `crates/tole-core/tests/` and `storage.rs:1676` are tests,
and no host calls it (grep of `tole-cli/src`, `tole-core/src`, `tole-tui/src`,
`tole-client/src`). `docs/architecture.md:127` says compaction happens "only
when dead-bytes ratio crosses a threshold"; no such trigger exists in the code
(**not verified** beyond the grep). It is therefore not a retention mechanism
and must not be presented as one. (It does clear a poisoned handle after a
failed append, which its comment calls "the sanctioned recovery path"
(`storage.rs:1555`); since no host calls it, today the recovery is to re-open
the session.)

**Context compaction is a different thing.** `crates/tole-core/src/turn/compaction.rs`
(#440) appends a `compaction` entry (a summary plus the id of the first entry
kept) so the provider sees less. The file keeps growing; it saves tokens, not
bytes.

**The session file does not hold images.** The log keeps a reference (path, mime,
size, FNV-1a hash), in two forms (`crates/tole-core/src/provider/image.rs:120`
to `:149`). An inline `data:` URL (and an ACP image block) is spilled,
content-addressed, to `$CODECORA_HOME/tole/images/<hash>.<ext>`, shared by every
session that sent the same bytes (`image.rs:57`, `session/runner.rs:46`); `tole
serve` uploads go to `images/serve/<session id>/<hash>.<ext>` with quotas of
64 MiB per session and 512 MiB in total (`serve_support/images.rs:31`). A
`--image <path>` or `/image <path>` is a workspace-jailed path **referenced in
place**: the file stays where it was, outside every sessions directory and
outside any archive of it. Every request re-reads the file, re-checks the jail
(workspace plus the spill dir) and size and hash (`image::data_url`,
`image.rs:165`). A missing or unreadable image does not fail the turn: its one
non-test caller turns the error into a text part `[image unavailable: <err>]`
and the turn continues (`provider/openai.rs:489`). A session resumed from a
different workspace therefore loses its path images the same way, through the
jail check.

**Readers of the directory.** All of them list `*.jsonl` files by `read_dir` of
the one directory, non-recursively; most replay each file in full:

| Reader | What it does with a file | Reference |
|---|---|---|
| `tole sessions` | replays every file read-only to show state and turn count | `sessions.rs:106` |
| `tole sessions --tree` | replays every file, nests forks under `forkedFrom.session` | `session/index.rs:237` |
| `tole sessions --all` | reads the global index, opens each indexed file, skips ones that are gone | `session/index.rs:169` |
| `tole resume <id-or-name>` | exact id first; otherwise replays every file to match a name; then the global index | `run.rs:161` |
| `tole resume --last` | newest mtime of a valid `*.jsonl` | `sessions.rs:295` |
| `tole serve` `GET /sessions/index` | metadata only, bounded: 10,000 entries scanned, 200 opened, files over 16 MiB skipped, 64 MiB replayed in total | `serve/past_sessions.rs:39` |

Consequences: a file that is not named `<valid id>.jsonl` in that directory is
invisible to the directory readers (so a `.jsonl.gz` or a subdirectory is
simply not listed); and the cost of `tole sessions`, `--tree` and a name lookup
grows with the number of files and their sizes, because each file is replayed
in memory (`storage.rs:3`: the file is a replay recipe; queries run in RAM).
`resume --last` reads only mtimes, an exact-id `resume` opens one file, and
serve's index route is bounded and metadata only (it skips files over 16 MiB).

**Failure when the disk is full.** A failed append (the code names ENOSPC)
poisons that open handle: every later commit is refused until the session is
re-opened (or `compact()` is run, see above), and the torn tail is discarded at
the next open (`storage.rs:1010`). `tole serve` has a readiness route since
#1318 (`GET /ready`, next to `GET /health`, which stays liveness;
`crates/tole-cli/src/serve_support/ready.rs`, `docs/mobile-control.md` "`GET /ready`").
Its `state` check creates and removes one empty file in the data directory, in
the cron directory when `--cron` or a webhook destination is configured, and in
`--sessions-dir` when that is given (the container image always gives it,
`Dockerfile:76`); without `--sessions-dir` the sessions directories are not
probed. So a **read-only volume** or one with no free inode turns the answer
into `503 not ready`. The probe writes no byte (`writable` in `ready.rs`,
`create_new` then `remove_file`), so whether it also fails on a volume that has
inodes but no free data blocks depends on the filesystem and is **not verified**:
do not rely on `/ready` as a disk-space alarm. `[delegation] min_free_disk_mb`
guards child spawns on the data dir, workspace and worktree filesystems
(`docs/threat-model.md:1141`); nothing guards the sessions directory itself.

**Model commands and the sessions directory.** By default the barrier is the
approval gate on `run_command` and `job_start`, plus a best-effort blocklist
that the threat model itself calls bypassable (`docs/threat-model.md:44`). The
command sandbox is opt-in (`[sandbox.consumers] run_command` / `job_start`,
default `off`); only when it is enabled, and the effective sessions directory is
inside the workspace, is that directory bound read-only (#866, `main.rs:1208`,
`sandbox/command.rs:285`). So the note does not claim a model cannot touch
session files; it proposes that retention be an owner command, not a tool.

**Forks are self-contained.** `JsonlStorage::fork` copies the lineage up to a
settled entry into a new file; only the header's `forkedFrom: {session, entry}`
points back (`storage.rs:585`; `docs/architecture.md:154`). Outside the
listings and the `branch`/serve responses that print it, `forkedFrom` is read
nowhere (grep of `forked_from` users: `index.rs:263`, `past_sessions.rs:194`,
`sessions.rs:211`, `serve.rs:2490`, `session/tui.rs`). `sessions --tree` already
copes with a missing source: a fork whose source is gone is listed as a root
(doc comment of `tree_lines`, `index.rs:216`).

## 3. What retention can mean without breaking the rule

Each reading below touches only whole files. None edits a line.

**A. Measure and warn (no change to any file).** Report the number of session
files, total bytes, oldest, largest (and the image directories) in `tole
sessions` and `tole doctor`, with an optional warning threshold. Does nothing
destructive. This is the "size report" of the issue.

**B. Documented manual procedure (docs only).** Tell the owner how to move or
delete whole files safely: the writer lock, the sidecar, the images, the index.
No new code. Works today because deletion is an unlink.

**C. Archive: move a whole file out of the active directory.** The file is moved
(or copied, verified, then the original removed) byte for byte to an archive
location, optionally compressed. The bytes of the log are unchanged, so the log
itself is never edited. This reads "never deleted" as "never deleted by the
store" (decision 1); the active directory loses the file. Variants:

- C1, plain move to `<sessions dir>/archive/<id>.jsonl` or to an owner-chosen
  directory: no new dependency; `*.jsonl` is not listed from a subdirectory, so
  it disappears from the directory readers in the table above (what the global
  index does depends on C3, section 4); restore is a move back.
- C2, compressed (`.jsonl.gz` or `.zst`): saves bytes on a text log, needs a
  compression implementation. `flate2` is in `Cargo.lock`, pulled by `ureq` (a
  direct `tole-core` dependency; the workspace takes `ureq` with default
  features, and `ureq` 3.5.2's default includes `gzip`, which enables
  `flate2`), `png`, `tiff` and `usvg`; no zstd crate is in the lockfile. It is
  not a direct dependency of `tole-core` or `tole-cli`, so using it as the codec
  means adding it to a manifest (same crate, already in the tree; whether its
  licence passes `deny.toml` was not run). Otherwise a new dependency goes
  through the licence gate (`AGENTS.md` §6), or the command can shell out to an
  installed `gzip` at the cost of a runtime dependency.
- C3, a **pointer record** for the archive: one more line in the global index
  (`sessions-index.jsonl`, append-only, last line for an id wins,
  `index.rs:101`) saying where the session now is. This is the only place a
  "pointer entry" fits the rule: the index is already an append-only log of
  "this session lives here". Writing a pointer *into the session file* is not
  available, because the session file must stay byte-identical, and the live
  file is being removed anyway. C3 needs a schema decision: `index::Entry` has
  fixed fields (`id`, `file`, `cwd`, `face`, `created_at`, `index.rs:30`), so
  the pointer either reuses `file` (the archive path) or adds a field, and a
  last-line-wins line also replaces `created_at`, which orders `--all`. That
  choice decides which rows of the table in section 4 apply.

**D. Export, then remove, by the owner.** The owner exports (copies) the session
to storage they control (the TUI `/export` writes a JSONL or HTML copy,
`session/export.rs`, owner-only `0600`, never overwrites; its JSONL is only the
entries, `render_jsonl` at `export.rs:63`, with no header, registers, usage or
state, so it is a transcript, not a file `resume` can open) and then deletes the
file. Retention is then "the owner deleted it": the same as `rm`, which the
architecture already calls deletion. It satisfies a request to erase; C does not
(section 5).

**E. Rotation inside one session (a new segment file with a pointer entry).**
The session id, the in-memory replay and the `parentId` tree are one file
today; resuming needs the whole tree (entries link by `parentId`, and
`compaction` points at `from_entry`). Splitting a live session into segments
means a new header kind, a pointer entry, and readers that follow it across
files. That is a storage-format change (`STORAGE_VERSION`: newer files are a hard
error in older builds, `storage.rs` header check) and is not needed for the
problem in the issue, which is cold, finished sessions. Not proposed; listed so
the owner can rule it out.

**F. In-place edits or trimming (drop old entries, cut a prefix, rewrite
redacted).** Forbidden by the rule. Listed only to say that no option above
does this, and that `compact()` does not either (section 2).

### What each option needs

| | Code | New file format | New dependency | Reversible |
|---|---|---|---|---|
| A measure and warn | small, read-only | no | no | n/a |
| B manual procedure | none | no | no | n/a |
| C1 move | a command plus locking | no (index line, C3) | no | yes (move back) |
| C2 compress | C1 plus codec | no, but a new on-disk form `.jsonl.*` | maybe | yes |
| D export then remove | exists in part (TUI `/export`) | no | no | no, after the removal |
| E segments | large | yes (version bump) | no | hard |

## 4. What a reader or a resume sees after an archive (C)

From the code in section 2. The first column is what the code does **today** for
a session whose file is gone from the directory; the last two columns differ by
whether C3 writes an index line.

| Command | C1 move, no C3 line (index still names the old path) | C1 move plus a C3 line whose `file` is the archive path |
|---|---|---|
| `tole sessions` | not listed | not listed |
| `tole sessions --tree` | its forks show as roots (source gone) | same |
| `tole sessions --all` | not listed: `rows` skips entries whose file cannot be opened (`index.rs:169`) | **listed**, from the archive file (and ordered by the pointer line's `created_at`) |
| `tole resume <id>` | "no session with id or name ... (nor in the session index)": `find` needs `is_file()` on the old path (`index.rs:122`) | `find` checks only that the file name is `<id>.jsonl` and `is_file()`, not the directory, so it returns the archive entry and `resume` bails with "belongs to another project; resume it there" and a `cd <cwd> && tole resume <id>` hint that would not work (`run.rs:205` to `:221`). A new "archived, restore with ..." message or a restore step is needed |
| `tole resume --last` | newest remaining file | same |
| `tole serve` `GET /sessions/index` | not listed | not listed (directory only) |

With C2 (`.jsonl.gz`) the name check in `find` fails, so a pointer to a
compressed archive is ignored by `find`; `rows` would fail to open it as JSONL
and skip it. A running `serve`/`chat` holding the session holds the writer lock,
so archiving must take the same lock or refuse (below).

**Concurrency.** A session open for writing holds an exclusive advisory `flock`
on `<id>.jsonl.lock` (`storage.rs:1099`); a second opener gets
`StorageError::Locked { holder }`. An archive command should take that same lock
(try-lock, refuse with the holder pid) before it touches the file, and move the
`.lock` sidecar's job away with the file or leave it to be recreated. Moving the
file under a live writer would leave the writer appending to an unlinked inode;
`compact()` already documents that hazard and poisons the handle for it
(`storage.rs:1546`). The mtime of a restored file would be the restore time, not
the last turn, unless restore preserves it; `resume --last`, `sessions` and the
serve index all order by mtime (`sessions.rs:141`, `past_sessions.rs` cursor). A
copy-based archive must preserve mtime or record the last-activity time in the
index line (**not designed**; a decision of the implementation).

## 5. The two open questions from the issue

These are analysis for the owner, not decisions.

**Does an archived session count as deleted for the redaction and audit
promises?** From the code and the threat model, no, and the note suggests the
docs say so plainly:

- Redaction (#596) happens when a line is written; older lines stay as written
  (`storage.rs:8`, `docs/threat-model.md:13`). Archiving or compressing keeps
  the bytes, so an archive holds exactly what the live file held, secrets that
  predate #596 included, and needs the same care as a live log (owner-only
  permissions; the session files themselves are only umask-protected today,
  section 2).
- The audit promises are that entries are append-only and never dropped
  (`docs/threat-model.md:54`). An archive that preserves the file satisfies "not
  dropped", but the log is no longer in the place the host reads, so for as
  long as it is archived `status`, `resume` and the directory listings cannot show it
  (`--all` may, depending on C3).
  Other logs name sessions by id (cron run records carry `session_id`,
  `cron/agent.rs:234`; the delegation tree records sessions,
  `docs/threat-model.md:1013`); whether any reader opens a session by that id
  from those logs, and so would fail on an archived one, is **not verified**.
- A request to *erase* (a leaked secret, a privacy request) is not served by an
  archive. That is D: export if wanted, then remove, with the images and the
  index line (below).

**What happens to a fork whose parent was archived?** Nothing breaks: the fork
holds its own copy of the lineage (section 2), is resumable, and is listed as a
root by `--tree`. What is lost is only the "forked from" link in the tree view.
The reverse (archiving a fork and keeping the parent) is likewise safe. If the
owner wants the link kept, the index line (C3) can carry it; the doc comment of
`tree_lines` is the only place that currently defines the "source gone"
rendering. Archiving a **family** together (a parent and its forks) is a design
choice, not a need.

## 6. Images and the index

Images are the larger and more awkward half of the disk problem (up to 512 MiB
of serve uploads by quota alone, plus the unbounded spill of inline and ACP
images).

- The image file is not in the session file, and nothing records which sessions
  reference a spilled image: the name is the content hash and shared. To
  remove or archive one safely, a command must scan every session file for
  references (`ImageRef.fnv1a64`) before it removes a file another session or a
  fork still names. Serve's `images/serve/<session id>/` directories are
  attributable by directory name and are the safe half. Path images
  (`--image <path>`) live in the workspace and are outside any sessions-dir
  archive; retention would not touch them.
- A session archived without its images keeps working as a log (the log holds
  references); a later `resume` that replays a missing image degrades to the
  text `[image unavailable: ...]` and continues (section 2), so the loss is
  that image's content for the model, not the resume. An archive that moves
  the session-scoped serve images with it, and leaves shared spill images in
  place until no session references them, avoids that.
- The global index (`sessions-index.jsonl`) is append-only and never shrinks;
  a stale line for a removed session is skipped at list time. Retention does
  not need to touch it except for the pointer line of C3 and, for D, an owner
  decision about leaving or rewriting it. Rewriting it is an edit of an
  append-only file; the repository text does not say whether the index is held
  to the rule as strictly as the session files (the threat model calls it
  append-only, `docs/threat-model.md:865`).

## 7. Costs and risks of C, honestly

- **A new owner-facing command** with locking, copy-verify-remove ordering, crash
  safety (a crash between copy and remove must leave one good copy, never zero
  or two writable ones), restore, and a listing; all of it new surface in a
  crate whose size guard allows 500 lines per file (`scripts/arch-check.sh`).
- **A directory format to keep**: the archive layout and the index pointer
  become a compatibility surface, like the session format.
- **A second copy of sensitive data** during the operation, and a long-lived one
  if the archive sits on another volume with other permissions.
- **Face parity** (`AGENTS.md` §6): a listing of archived sessions on the CLI
  needs a row in `docs/faces.md` or the same capability on serve and the TUI
  (**not checked** what `face_parity` would require).
- **Value is unmeasured.** There is no measurement in this repository of typical
  session sizes or counts on a long-lived volume. Option A produces exactly that
  data.

## 8. Open decisions for the owner

Not decided here.

1. Does `AGENTS.md` §6 permit moving or removing a whole session file at all?
   C and D depend on a yes (D is literal deletion), and so does the reading
   "never deleted by the store". If no, only A and B remain. Separately: is the
   goal only to warn (A) and document (B), or also an archive command (C)? The
   issue calls A the cheapest and safe on its own.
2. If C: plain move (C1) or compressed (C2)? If C2: a new dependency under the
   licence gate, or shelling out to `gzip`?
3. Where does an archive live: inside the sessions directory (covered by the
   read-only bind only when the opt-in command sandbox is on and the directory
   is inside the workspace, section 2), or an owner-chosen directory (needs its
   own protection and permissions)? Should a retention command be strictly an
   owner command and never a model tool (proposed, section 1)?
4. Does the index get a pointer line (C3), and with which schema (reuse `file`
   or a new field, which also decides whether `--all` lists archived sessions
   and what `created_at` means)? And what does `resume` of an archived id do:
   restore it, print the restore command, or refuse? Today a pointer into the
   archive makes `resume` print a misleading "belongs to another project" hint
   (section 4).
5. Selection: age only (`--older-than DAYS`), size, or an explicit id list; by
   last-activity time (which the file's mtime only approximates).
6. Are images in scope of the first piece, or documented as an operator cleanup
   as today? (Needs the reference scan of section 6 to be safe.)
7. Are sessions in a non-terminal state (`Planning`, `ToolCall` with a pending
   intent, `Final`) all eligible, or only settled ones?
8. Archive a family (parent and forks) together, or each file alone?
9. Should the session files' mode (`0o666` before umask) be tightened to `0600`
   for new files, independent of retention?
10. Does "retention" cover cron `jobs.jsonl`, the approvals log, outbox and job
    logs in the same decision, or one issue each?

## 9. Out of scope, and what is not verified

Out of scope for this note: editing or trimming any entry; rotation inside one
session (E); a SQLite backend (`docs/architecture.md` §4 describes an optional `sqlite`
feature, but none exists in the tree: no crate has a `sqlite` feature or uses
`rusqlite`, which is only declared in the workspace `Cargo.toml` and is absent
from `Cargo.lock`, so that text is stale);
deleting anything automatically or on a timer; a model-callable retention tool;
retention of cron, approvals, outbox, delegation and job logs; remote archives
(object storage); a disk-space signal on `tole serve` (`/ready` is a write
probe, see section 2).

Not verified: whether `compact()` has any trigger (grep says none); whether
`flate2`'s licence passes `deny.toml` (`cargo deny` not run); whether any
reader opens a session by the id found in cron or delegation logs; the
unread grep hits for "archive/prune/retention" (section 2); what
`face_parity` would require of an archived-sessions listing; the server's
behaviour on ENOSPC beyond the storage layer (the poison path is read, not run),
and whether `GET /ready` fails on a volume with no free data blocks (its probe
creates an empty file only);
the size of real session stores. No command was run against a real session
store for this note; everything above is from reading the source and the docs.

## 10. Volume sizing: what can be said today

There is **no measurement** of real session sizes or counts in this repository
(section 7), so this section gives the facts the code fixes and a way to
measure; it gives no "N GB per month" figure, because none can be backed.

**Where the bytes go in the container recipe** (`Dockerfile`, `docs/container.md`
"The image"). One named volume, `tole-data`, mounted at `/data`, holds all of
this, so one full volume stops all of it:

| Path | What | Grows by | Bounded by |
|---|---|---|---|
| `/data/sessions` (`--sessions-dir`) | one `<id>.jsonl` and one `.lock` per session | every turn, append only | nothing (section 2) |
| `/data/codecora/tole/images/<hash>.<ext>` | spilled inline and ACP images, shared by hash | a new image | nothing |
| `/data/codecora/tole/images/serve/<session>/` | `tole serve` uploads | an upload | 64 MiB per session and 512 MiB in total by default (`serve_support/images.rs:28-34`; overridable by `TOLE_SERVE_IMAGE_SESSION_QUOTA` and `TOLE_SERVE_IMAGE_TOTAL_QUOTA`) |
| `/data/codecora` (rest) | user config, cron store, discussion logs, trust records, skills (`docs/container.md`, "The image"); the approvals log, webhook outbox and job logs are append-only too, where each one lives was not checked | cron runs, approvals, deliveries | nothing (outside this note) |

The workspace is a second volume, `tole-workspace`; it holds the working
folders and is the operator's own data, not a retention target here.

**What drives the size of one session file.** Every tool result and every
message is an entry, kept whole so that a replay is exact. What a single tool
may return is bounded per tool, not per entry: `read_file` returns at most
20,000 characters per call (`MAX_READ_CHARS`, `tools/read_file.rs:27`) and a
subprocess capture stops at 32 MiB (`MAX_CAPTURE`, `subprocess.rs:21`); whether
the turn layer trims a result further before it is written was **not checked**.
Context compaction appends a summary and saves tokens, not bytes (section 2).
A long mission is the heavy case: each step adds to the same file.

**How to measure, on the host or with `docker exec`** (read-only, none of these
is a tole command and none was run for this note):

```bash
du -sh /data/sessions /data/codecora/tole/images       # total bytes
ls -lS /data/sessions | head                            # the largest sessions
ls /data/sessions/*.jsonl | wc -l                       # how many
find /data/sessions -name '*.jsonl' -mtime +90 | wc -l  # how many untouched for 90 days
df -h /data                                             # free space on the volume
```

**Sizing rule that follows.** Take the measured growth per week of `/data` after
a representative period, multiply by the weeks until you will next review it,
and add headroom for the images (at least the 512 MiB serve quota when the
desktop or mobile clients upload) and for the backup copy the recipe makes while
the service is stopped (`docs/container.md`, "Persistence and upgrades": a
`tar` of the volume needs room somewhere, not necessarily on the same volume).
Watch free space from outside tole (the host's `df`, the orchestrator's volume
metric): `GET /ready` fails on a read-only volume and, possibly, on a full one
(section 2), but it is a write probe, not a space alarm. If the volume is shared
with other state, size the whole of `/data`, not only `/data/sessions`.

## 11. Recommendation

A proposal; every step that touches a session file waits for decision 1 of
section 8. The order is chosen so that each step is useful alone and the next one
is only built if the data from the previous one calls for it.

1. **Now, no code and no new rule: the measurement and the sizing text.** The
   measure commands above, and a short "Disk growth" paragraph in
   `docs/container.md` that points at section 10 (written in the change that
   added this section). It decides nothing and moves no file. This is the only
   step that needs no owner answer. The other half of option B, a written
   procedure for moving whole files by hand, is **not** written down: it tells an
   operator to move or remove session files, so it waits for decision 1.
2. **First code: option A, measure and warn.** `tole sessions` and `tole doctor`
   print file count, total bytes, oldest and largest (and the size of the image
   directories), with a warning threshold that is off by default. Read-only, no
   file format change, and it produces the real numbers section 7 says are
   missing. It is the "smallest piece" the issue asks for.
3. **Only if step 2 shows a problem: option C1, archive, and nothing more.**
   `tole sessions archive --older-than DAYS --to DIR` moves **whole, settled**
   session files (decision 7) out of the active directory; it is named
   `archive`, not `prune`, because it moves and never deletes. Shape, each point
   resting on section 2 to 5:
   - an **owner command on the host**, never a model tool, and never run by a
     timer or on a threshold (section 1);
   - **dry run by default** (it lists what it would move and the bytes), the move
     needs an explicit flag;
   - **copy, verify, remove**: it takes the writer lock of the file (try-lock,
     refuse with the holder's pid), copies byte for byte keeping the mtime,
     compares the copy, and only then removes the original and its `.lock`
     sidecar, in that order, so a crash leaves at least one whole copy
     (section 4, section 7);
   - **no compression** in the first piece (C2 needs a dependency or a runtime
     `gzip`, and the licence gate of `AGENTS.md` section 6 for a new crate);
   - **no index pointer** (C3): an archived session is invisible to `sessions`,
     `--all` and `resume`, and the way back is moving the file back by hand
     (section 4, first column). This is the cheapest reading and avoids
     deciding the index schema; it costs that a person must remember where they
     put the archive, which is acceptable for a first piece an owner runs by hand;
   - **each file alone**: forks are self-contained (section 2), so archiving a
     family together is not needed (decision 8);
   - **images out of scope**: documented as an operator cleanup, as today. The
     safe half later is `images/serve/<session id>/`, which is attributable by
     directory name; the shared spill needs the reference scan of section 6.
4. **Not recommended:** compaction as retention (it removes no entry,
   section 2); rotation inside a live session (E, a format change for a problem
   that lives in cold sessions); any in-place edit (F); a deleting command
   (D stays the owner's own `rm`, after an export if wanted); a retention
   tool callable by the model.

**What needs the owner** (numbers are those of section 8): 1 (may a whole file
be moved at all; steps 1 and 2 stand without it), 3 (where the archive lives and
that the command is owner-only), 5 and 7 (selection by age, and settled
sessions only), plus one new question: **is a first piece without an index
pointer acceptable**, given that `resume` of an archived id then answers "no
session" and `sessions --all` does not list it (section 4). Decisions 2, 6, 8, 9
and 10 can wait: the recommendation does not need them. Face parity
(`AGENTS.md` section 6) still has to be checked for step 3: a CLI-only archive
command is a row in `docs/faces.md` with its reason, or the same capability on
the other faces; what `face_parity` would require was not checked.
