# q — Quest orchestrator for Claude Code agents

`q` is a Rust CLI + TUI that organizes Claude Code agent sessions into
**Quests**: a group of tmux sessions working one goal, with hooks as the event
source, a SQLite database per machine, and a regenerable brief that tells each
agent who it is and what the others are doing.

## Fleet topology (v2)

A Quest is a **fleet of tmux sessions**, not a single window:

- one **main** session `q-<slug>` (its Claude is the *master*),
- any number of **worker** sessions `q-<slug>+<label>`.

Each tmux session runs a **login shell**; Claude is a child the master launches
with `q start`. Claude can come and go — exit it and you land back in the shell,
`cd`, rerun, or `q start` it again — while the tmux session lives until `q kill`
removes it. The main session's *shell* cwd defines the Quest cwd; while Claude
is up, move it with `q cd`.

### Session states

Every row derives its state from hooks (the authoritative push) plus the pane's
current command (the fallback that catches a crash or Ctrl-C):

| state | glyph | meaning |
|---|---|---|
| `off` | `○` | tmux pane alive, no Claude running (a bare shell) |
| `starting` | `◐` | `q start` launched Claude; booting |
| `busy` | `●` | Claude is working |
| `idle` / `idle · your turn` | `◑` | finished its turn; `· your turn` = your reply moves it |
| `waiting:permission` / `waiting:input` / `waiting:question` | `◆` | honestly blocked on the human |
| `ended` | `×` | session killed or Quest closed |

`off` is never reached from `ended`: a killed session stays ended even if a
late `SessionEnd` arrives. A worker raises a blocking question with the
**AskUserQuestion** tool, which shows the fleet `waiting:question`; a plain-text
question that only ends a turn reads as `idle · your turn`.

## Commands

```
q                                   TUI (Quests · Sessions · Templates · Events)
q new [--name] [--goal] [--dir] [--workflow] [--template] [--prompt|--prompt-file] [-d]
q list [--all] [--state active|idle|finished] [--machine]
q show <quest>                      details + sessions + links + progress
q enter <quest> [--session <label>] attach to the main (or a worker) tmux session
q close <quest> [-f]                kill every tmux session, wind the Quest down
q resume <quest>                    bring a Quest's main back up, re-adopt live workers
q rename <quest> <slug> · q set <quest> <field> <value> · q cd <quest> <path>

q sessions [<quest>]                the fleet, or one Quest's
q spawn <quest> ["<prompt>"] [--label <l>] [--shell] [--enter]   new worker session
q start <session> ["<prompt>"] [--resume] [--force]   launch Claude in a shell pane
q stop <session> [--force]          type /exit; the shell stays, the row goes off
q prompt <session>                  print the stored first prompt
q peek <session> [--lines N] · q send <session> "<text>" [--shell] · q reset <session>
q kill <session> [-f]               remove a worker's tmux session

q brief [<quest>] [--for master|worker] · q phase "<text>" · q note "<text>"
q link add <ref> · q artifact add <path> · q tpl … · q workflow …
q events [<quest>] [--follow] · q watch [--notify] · q doctor · q config …
```

Every command supports a global `--json`. Human output goes to stdout, errors
to stderr, and a failure exits non-zero.

## Multi-machine (remote parity)

Configure `[[remotes]]` and `q` treats the fleet as one across machines:

- `q list` and the TUI merge the local database with each remote's
  `q list --json`, and the TUI **Sessions** tab fans out `q sessions --json` to
  show every machine's sessions in one list (down machines show a note, never
  an error).
- Any command that resolves a Quest on another machine is **proxied** over ssh
  with the same arguments (`q --machine ws start <session>`, `stop`, `peek`,
  `send`, `kill`, …); `--no-remote` breaks the recursion.
- The fleet wire carries `tmux_session`, the `off` status and `waiting_for`.
  Parsing is **tolerant**: a remote on an older `q` that omits a field renders
  `?` / a graceful default rather than dropping the whole machine.
- Registry, hooks and the database stay **local to each machine** — there is no
  sync. `q doctor` probes every remote (`ssh <alias> q --version`) and reports
  one that is unreachable, has no `q`, or speaks a wire this `q` cannot.

Run `q doctor` for a full health check (tmux version, hooks, the `claude`
launcher, schema, remotes) with a fix line for anything wrong.
