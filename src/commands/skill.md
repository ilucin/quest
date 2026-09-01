---
name: q
description: How to operate inside a Quest with the `q` CLI (Quest orchestrator for Claude Code). Load when you are running as a master or worker in a Quest and need to inspect sessions, spawn or start workers, report progress, link artifacts, or wind a Quest down — commands like q sessions, q peek, q send, q spawn, q start, q stop, q phase, q note, q brief.
---

# q — operating inside a Quest

`q` orchestrates Claude Code agents as **Quests**. A Quest is one goal run as a
**fleet of tmux sessions**: the master lives in `q-<slug>`, and each worker gets its
own tmux session `q-<slug>+<label>`. Every session has a role of **master** or
**worker**. You are running inside one now: `$Q_QUEST` names the Quest and
`$Q_SESSION` names you, so most commands need no explicit target. Every command
accepts `--json`.

A `<session>` is written `<quest>/<label>`, a session id, or just `<label>` when you
are inside its Quest. `master` is the label of the first session.

A session's tmux pane is a login shell; Claude is a child launched inside it. A
session with no Claude running shows status **off** — its shell is still there, ready
for `q start`. **`off` means no Claude, not dead.**

Session states — `q sessions` and the TUI name them one way, the brief's section 5
another, for the same session:

- **busy** / **starting** in `q sessions` and the TUI — Claude is working, or booting.
  The brief's section 5 shows both as **running**.
- **idle**, softened to **idle · your turn** in `q sessions` and the TUI when Claude
  just handed a turn back to you. The brief's section 5 shows that same turn as plain
  **idle**.
- **waiting:permission / waiting:input / waiting:question** — honestly blocked on the human. A worker
  that calls the **AskUserQuestion** tool shows `waiting:question`; a plain-text
  question that merely ends a turn only reads as `idle · your turn` (plain `idle` in the
  brief). If you are a worker and need a decision to proceed, ask with **AskUserQuestion**
  so the fleet sees it.
- **off** — no Claude in the pane (a bare shell); `ended` — the session is wound down.

The Quest's cwd follows the **main** session's shell — a `cd` there moves the Quest,
but only once Claude has exited to the shell. While Claude is up in main, move the
Quest explicitly with `q cd <quest> <path>` (config `[quest] follow_main_cwd`).

## Read the state — no confirmation needed

- `q brief` — the Quest brief: goal, workflow, live sessions, beads, links, open
  blockers. This is your source of truth, and how you come back after a `/clear`.
  `q brief --for worker` renders a worker's view.
- `q list [--all]` — the Quests. `q show <quest>` — one Quest in detail.
- `q sessions [<quest>] [--all]` — the live fleet; defaults to every active Quest.
- `q peek <session> [--lines N]` — print what a session's pane currently shows.
- `q prompt <session>` — print the first prompt a session was (or will be) launched
  with.

## Report your own work — no confirmation needed

- `q phase "<text>"` — say what you are doing right now. Your master watches this.
- `q note "<text>" [--blocker]` — leave a durable note on the Quest timeline; it
  survives a context reset. Use `--blocker` for something the master must resolve.
- `q link add <ref> [--kind pr|task|worktree|url|branch] [--title <t>]` — attach an
  external reference. The kind is auto-detected from a URL when omitted.
- `q artifact add <path> [--note "<what it is>"]` — register a file you produced.
- `q set <quest> goal "<text>"` — record the goal when the brief has none (a Quest
  started bare gets it from you). `q set <quest> beads_epic new` creates the epic
  for a Quest that has none.

## Delegate — master only

- `q spawn <quest> "<prompt>" --label <label> [--workflow <w>] [--dir <path>] [--shell] [--enter]` —
  start a **worker**: a detached Claude in its **own tmux session** with its own
  context. `--shell` opens the worker's shell (status `off`) without launching
  Claude, for a `q start` later; `--enter` attaches you to it instead of leaving it
  detached. Spawn a worker for long-running or detached work; use the `Agent` tool (a
  subagent) for something bounded you wait on. Give a worker its scope, the artifacts
  it needs **by path**, and what it must return — it reads the brief on its own.
- `q start <session> ["<prompt>"] [--resume] [--force]` — launch Claude in a session
  whose pane is a bare shell (status `off`), e.g. one spawned with `--shell` or one
  that exited. `--resume` re-attaches Claude's own last session in that pane.
- `q stop <session> [--force]` — type `/exit` into a session so Claude leaves; the
  shell and the tmux session stay, and the row goes `off`. Idle-gated like `q send`.

## Act on other sessions or end work — confirm with the human first

These reach into another session or end work. **Ask the human before running them**
unless they have already told you to:

- `q send <session> "<text>" [--force] [--shell]` — type a line into another session.
  `--shell` types into an `off` session's shell instead of into Claude.
- `q stop <session>` — end the Claude running in a session (see above).
- `q kill <session> [-f]` — end a worker's tmux session entirely.
- `q close <quest> [-f] [--close-epic]` — finish the Quest. Propose it once the goal
  is met; do not close on your own.

## The operating contract

- **Read freely** — `brief`, `list`, `show`, `sessions`, `peek`, `prompt` change
  nothing.
- **Report freely** — `phase`, `note`, `link add`, `artifact add` are yours to run.
- **Act with confirmation** — `send`, `start`, `stop`, `kill`, `close` touch other
  sessions or end work; get the human's go-ahead first.
- **`spawn` is a master's to run** within its own Quest, without asking.
- Prefer a **file plus `q artifact add`** over pasting a wall of text: the next agent
  gets your path, not your transcript.
