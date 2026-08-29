---
name: q
description: How to operate inside a Quest with the `q` CLI (Quest orchestrator for Claude Code). Load when you are running as a master or worker in a Quest and need to inspect sessions, spawn workers, report progress, link artifacts, or wind a Quest down — commands like q sessions, q peek, q send, q spawn, q phase, q note, q brief.
---

# q — operating inside a Quest

`q` orchestrates Claude Code agents as **Quests**. A Quest is one goal with its own
tmux session; each agent is one window, with a role of **master** or **worker**. You
are running inside one now: `$Q_QUEST` names the Quest and `$Q_SESSION` names you, so
most commands need no explicit target. Every command accepts `--json`.

A `<session>` is written `<quest>/<label>`, a session id, or just `<label>` when you
are inside its Quest. `master` is the label of the first session.

## Read the state — no confirmation needed

- `q brief` — the Quest brief: goal, workflow, live sessions, beads, links, open
  blockers. This is your source of truth, and how you come back after a `/clear`.
  `q brief --for worker` renders a worker's view.
- `q list [--all]` — the Quests. `q show <quest>` — one Quest in detail.
- `q sessions [<quest>] [--all]` — the live fleet; defaults to every active Quest.
- `q peek <session> [--lines N]` — print what a session's pane currently shows.

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

- `q spawn <quest> "<prompt>" --label <label> [--workflow <w>] [--dir <path>]` — start
  a **worker**: a detached Claude in its own window with its own context. Spawn a
  worker for long-running or detached work; use the `Agent` tool (a subagent) for
  something bounded you wait on. Give a worker its scope, the artifacts it needs **by
  path**, and what it must return — it reads the brief on its own.

## Act on other sessions or end work — confirm with the human first

These reach into another session or end work. **Ask the human before running them**
unless they have already told you to:

- `q send <session> "<text>" [--force]` — type a line into another session.
- `q kill <session> [-f]` — end a worker's window.
- `q close <quest> [-f] [--close-epic]` — finish the Quest. Propose it once the goal
  is met; do not close on your own.

## The operating contract

- **Read freely** — `brief`, `list`, `show`, `sessions`, `peek` change nothing.
- **Report freely** — `phase`, `note`, `link add`, `artifact add` are yours to run.
- **Act with confirmation** — `send`, `kill`, `close` touch other sessions or end
  work; get the human's go-ahead first.
- **`spawn` is a master's to run** within its own Quest, without asking.
- Prefer a **file plus `q artifact add`** over pasting a wall of text: the next agent
  gets your path, not your transcript.
