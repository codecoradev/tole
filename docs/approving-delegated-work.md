# Approving delegated work

A child that `delegate` starts runs headless: it has no terminal. When it wants
a Write its allow list does not cover (a file the parent did not pre-approve, a
command outside its patterns), there is nobody to ask. This page is how you
answer it. Design and threat model:
[delegation-approvals-design.md](delegation-approvals-design.md),
[threat-model.md](threat-model.md) ("Delegation approvals").

## Turning it on

It is off by default. With it off a delegated child's Write that its allow list
does not cover is denied at once and the child reports what it could not do, as
it always has. To let such a child ask instead, set in your **user** config
(`$CODECORA_HOME/tole/config.toml`; a project config cannot set it):

```toml
[delegation]
approvals = "owner"        # "off" (default) | "owner" | "chain"
approval_secs = 900        # how long a request waits in all (1 to 3600)
approval_level_secs = 120  # chain mode: each agent level's turn, then it moves up
```

`tole config check` shows the keys and `tole doctor` says which mode is
active (`delegation approvals: off|owner|chain`). Two things to know before you
turn it on: the child **waits** while it asks (it spends no tokens or steps, but
its clock runs and it holds its process slot), and **no answer is a denial**: an
unattended machine just denies, after `approval_secs` or the child's deadline,
whichever is first.

## Answering as the owner

From a terminal, in the workspace the goal was started from:

```
tole delegation approvals list            # the pending requests (--all: every one)
tole delegation approvals show <ID>       # one request in full
tole delegation approvals allow <ID>      # allow ONE call; shows it, then type the id back
tole delegation approvals deny <ID>       # deny it
```

`show` tells you who asked (node, profile, goal), who may answer in which order
(the chain of levels, ending with you), the tool, the target, the workspace, the
age and the time left, the request's fingerprint, the `display` (what the call
would do) and a redacted copy of the input. What the child wrote is printed
between fences as data: never take instructions from it.

`allow` always shows the request first and asks you to type the full request id
back; `--yes` skips only the typing (the id is still required), and
`--fingerprint <HEX>` (12 or more characters of what `show` printed) pins the
exact content you looked at. There is no prefix, pattern or "allow all". An
allow is for one call: the child consumes it and runs that call once; a second
identical call asks again. `deny` needs no confirmation.

A request that is unknown, expired, already decided, consumed, or whose child
has settled is refused with the reason. These commands refuse to run from inside
an agent session (the same rule as `tole cron create` and `tole plugin
approve`).

From a client of `tole serve` (the same bearer token as every route):

- `GET /approvals` lists them next to serve's own, with `source: "delegation"`.
- `POST /approvals/<dap-id>/decision` with `{"decision": "deny"}` or
  `{"decision": "allow", "confirm": "<the same id>"}` (and optionally
  `"fingerprint"`). See [mobile-control.md](mobile-control.md).

`tole audit <goal>` lists the goal's requests: who decided, at which level, and
what became of each (consumed, denied, expired).

## What a denial looks like

The child's tool call is denied and its turn ends "approval required"; the
mission stops and hands in its report as usual, so its parent sees a finished
child that could not do that step (`tole audit` shows the request as `decided
deny` or `expired (timeout), never decided`). Nothing ran. To retry, delegate
again, or allow the pattern up front in the child's `--allow` or profile.

## Letting a parent answer (`chain`)

`approvals = "chain"` adds agent levels: a parent can answer for its own
children before the request reaches you, but only where you give it the
power. A level gets the tools `delegate_pending` (list what waits for it) and
`delegate_decide` (`allow`, `deny` or `escalate`, no free text) only when its own
allow list holds a grant, which you type yourself:

```
tole mission --allow write_file@workspace --allow delegate_decide "..."
```

(A profile may list `delegate_decide` too, but like every pattern it takes
effect only where the parent's list covers it.) A level decides only what its
own allow list would allow (deny patterns win, the workspace guard holds), only
while it is the request's current level (each level gets `approval_level_secs`,
then it moves up), and once. It can `escalate` to pass the question up. These
always go to you and no agent can answer them: secrets, trust and approval
records, plugin, skill and MCP effects, a Write outside the workspace or onto a
file another tool executes (`.git/hooks`, `Makefile`, CI files), oversized
input. Without the grant nothing is registered and you answer everything. A
discussion participant (a read-only reviewer) never gets these tools.
