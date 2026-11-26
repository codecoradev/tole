# Session store retention — design note

Status: **proposal for an owner decision** (issue #1020). Nothing here is
implemented and nothing in this note is decided: section 8 lists what the owner
has to decide. The note says what the code does today (with file references),
what retention can mean under the append-only rule, what each reading costs, and
what a reader or a resume sees afterwards. Anything not checked in the code is
marked **(not verified)**; section 9 lists them together.

Code references are to `origin/develop` at 50c7bcc (2026-11-26).

## 1. The rule this has to respect

`AGENTS.md` §6: session storage is append-only JSONL; entries are never mutated
or deleted; corrections are new entries. `docs/architecture.md` §4 says the same
(entries are write-once; registers are the only mutable state) and adds that one
file per session means "deletion = unlink" (line 125).

So "retention" cannot mean editing or trimming a session file. It can only mean
an operation on a **whole file** (keep it, move it, compress it, remove it) or a
**new** file that is a faithful copy of the old one. The owner, not a model,
performs it (section 3).

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
"archive", "prune", "retention" finds only the skill curator, approvals, cron and
unrelated code, as the issue says. The documented position for images is "not
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
and must not be presented as one.

**Context compaction is a different thing.** `crates/tole-core/src/turn/compaction.rs`
(#440) appends a `compaction` entry (a summary plus the id of the first entry
kept) so the provider sees less. The file keeps growing; it saves tokens, not
bytes.

**The session file does not hold images.** The log keeps a reference (path, mime,
size, FNV-1a hash); the bytes live under `$CODECORA_HOME/tole/images/`, and
`tole serve` uploads under `images/serve/<session id>/<hash>.<ext>` with quotas
of 64 MiB per session and 512 MiB in total
(`crates/tole-cli/src/serve_support/images.rs:31`). The CLI `--image` and the
TUI spill to `images/<hash>.<ext>`, content-addressed and shared by every
session that sent the same bytes (`crates/tole-core/src/provider/image.rs:57`,
`crates/tole-cli/src/session/runner.rs:46`). Every request re-reads the file and
re-checks size and hash (`image::data_url`, `image.rs:165`): a missing file is an
error for that image, not a silent skip. How a missing image affects a whole
`resume` turn (does the turn fail, or only that part?) is **not verified**
(`crates/tole-core/src/provider/openai.rs:489` is the call site).

**Readers of the directory.** All of them list `*.jsonl` files by `read_dir` of
the one directory, non-recursively, and replay each file in full:

| Reader | What it does with a file | Reference |
|---|---|---|
| `tole sessions` | replays every file read-only to show state and turn count | `main.rs:3579` |
| `tole sessions --tree` | replays every file, nests forks under `forkedFrom.session` | `session/index.rs:237` |
| `tole sessions --all` | reads the global index, opens each indexed file, skips ones that are gone | `session/index.rs:169` |
| `tole resume <id-or-name>` | exact id first; otherwise replays every file to match a name; then the global index | `main.rs:3332` |
| `tole resume --last` | newest mtime of a valid `*.jsonl` | `main.rs:3772` |
| `tole serve` `GET /sessions/index` | metadata only, bounded: 10,000 entries scanned, 200 opened, files over 16 MiB skipped, 64 MiB replayed in total | `serve/past_sessions.rs:39` |

Consequences: a file that is not named `<valid id>.jsonl` in that directory is
invisible to every one of them (so a `.jsonl.zst` or a subdirectory is simply
not listed); and the cost of `tole sessions` and of a name lookup grows with
the number of files and their sizes, because each file is replayed in memory
(`storage.rs:3`: the file is a replay recipe; queries run in RAM). Serve's
index route already skips files over 16 MiB.

**Failure when the disk is full.** A failed append (the code names ENOSPC)
poisons that open handle: every later commit is refused until the session is
re-opened, and the torn tail is discarded at the next open
(`storage.rs:1010`). `tole serve` has no readiness route today (only
`GET /health`, liveness, `serve.rs:6`); the "readiness issue" the issue text
mentions is separate and is **not verified** here. `[delegation] min_free_disk_mb`
guards child spawns on the data dir, workspace and worktree filesystems
(`docs/threat-model.md:1139`); nothing guards the sessions directory itself.

**Model commands cannot delete sessions.** The command sandbox binds the
effective sessions directory read-only when it is inside the workspace
(#866, `main.rs:1490`, `sandbox/command.rs:285`). A retention command is an
owner action on the host, never a tool.

**Forks are self-contained.** `JsonlStorage::fork` copies the lineage up to a
settled entry into a new file; only the header's `forkedFrom: {session, entry}`
points back (`storage.rs:585`; `docs/architecture.md:154`). Outside the
listings and the `branch`/serve responses that print it, `forkedFrom` is read
nowhere (grep of `forked_from` users: `index.rs:263`, `past_sessions.rs:194`,
`main.rs:3684`, `serve.rs:2490`, `session/tui.rs`). `sessions --tree` already
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
itself is never edited. "Never deleted" is read as "never deleted by the
store"; the active directory loses the file. Variants:

- C1, plain move to `<sessions dir>/archive/<id>.jsonl` or to an owner-chosen
  directory: no new dependency; `*.jsonl` is not listed from a subdirectory, so
  it disappears from every reader in the table above; restore is a move back.
- C2, compressed (`.jsonl.gz` or `.zst`): saves bytes on a text log, needs a
  compression implementation. `flate2` is in `Cargo.lock` but is not a direct
  dependency of `tole-core` or `tole-cli`, and no zstd crate is in the lockfile
  (grep of `Cargo.lock`, `Cargo.toml`); who pulls `flate2` was **not
  verified**. A new dependency goes through the licence gate (`AGENTS.md` §6,
  an owner decision if the licence is outside `deny.toml`), or the command can
  shell out to an installed `gzip` at the cost of a runtime dependency.
- C3, a **pointer record** for the archive: one more line in the global index
  (`sessions-index.jsonl`, append-only, last line for an id wins,
  `index.rs:101`) saying the session is archived and where. This is the only
  place a "pointer entry" fits the rule: the index is already an append-only
  log of "this session lives here" and was designed to be added to. Writing a
  pointer *into the session file* is not available, because the session file
  must stay byte-identical, and the live file is being removed anyway.

**D. Export, then remove, by the owner.** The owner exports (copies) the session
to storage they control (the TUI `/export` writes a JSONL or HTML copy,
`session/export.rs`, owner-only `0600`, never overwrites) and then deletes the
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

Assuming C1 with a C3 index line, from the code in section 2:

| Command | After the session is archived |
|---|---|
| `tole sessions` | not listed (file not in the directory) |
| `tole sessions --tree` | its forks show as roots (source gone) |
| `tole sessions --all` | not listed: `rows` leaves out entries whose file is gone (`index.rs:169`); an archived-aware listing needs a new `--archived` path |
| `tole resume <id>` | "no session with id or name ... (nor in the session index)" today. `index::find` also requires the file to exist and to be named `<id>.jsonl` (`index.rs:122`), so an index line pointing into an archive is ignored by it. A restore step first, or a new "archived, restore with ..." message, would be needed |
| `tole resume --last` | picks the newest remaining file |
| `tole serve` `GET /sessions/index` | not listed |
| a running `serve`/`chat` holding the session | holds the writer lock; archiving must take the same lock or refuse (below) |

**Concurrency.** A session open for writing holds an exclusive advisory `flock`
on `<id>.jsonl.lock` (`storage.rs:1099`); a second opener gets
`StorageError::Locked { holder }`. An archive command should take that same lock
(try-lock, refuse with the holder pid) before it touches the file, and move the
`.lock` sidecar's job away with the file or leave it to be recreated. Moving the
file under a live writer would leave the writer appending to an unlinked inode;
`compact()` already documents that hazard and poisons the handle for it
(`storage.rs:1546`). The mtime of a restored file would be the restore time, not
the last turn, unless restore preserves it; `resume --last`, `sessions` and the
serve index all order by mtime (`main.rs:3614`, `past_sessions.rs` cursor). A
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
  long as it is archived `status`, `resume` and the listings cannot show it.
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
of serve uploads by quota alone, plus unbounded `--image` and TUI spill).

- The image file is not in the session file, and nothing records which sessions
  reference a CLI-spilled image: the name is the content hash and shared. To
  remove or archive an image safely, a command must scan every session file
  for references (`ImageRef.fnv1a64`) before it removes a file another session
  or a fork still names. Serve's `images/serve/<session id>/` directories are
  attributable by directory name and are the safe half.
- A session archived without its images keeps working as a log (the log holds
  references) but any later `resume` that replays those images would hit the
  error in section 2 (impact **not verified**). An archive that moves the
  session-scoped serve images with it, and leaves shared spill images in place
  with a count of remaining references, avoids that.
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

1. Is the goal only to warn (A) and document (B), or to also ship an archive
   command (C)? The issue calls A the cheapest and safe on its own.
2. If C: plain move (C1) or compressed (C2)? If C2: a new dependency under the
   licence gate, or shelling out to `gzip`?
3. Where does an archive live: inside the sessions directory (inherits the
   read-only sandbox bind, section 2), or an owner-chosen directory (needs its
   own protection and permissions)?
4. Does the index get a pointer line (C3), and does `resume` of an archived id
   restore it, print the restore command, or refuse?
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
session (E); a SQLite backend (the optional `sqlite` feature in
`docs/architecture.md` §4; whether it behaves differently was not checked);
deleting anything automatically or on a timer; a model-callable retention tool;
retention of cron, approvals, outbox, delegation and job logs; remote archives
(object storage); a readiness signal on `tole serve`.

Not verified: whether `compact()` has any trigger (grep says none); how a
missing image affects a whole `resume` turn; who depends on `flate2`; whether any
reader opens a session by the id found in cron or delegation logs; what
`face_parity` would require of an archived-sessions listing; the server's
behaviour on ENOSPC beyond the storage layer (the poison path is read, not run);
the size of real session stores. No command was run against a real session
store for this note; everything above is from reading the source and the docs.
