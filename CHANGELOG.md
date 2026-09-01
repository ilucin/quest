# Changelog

All notable changes to `q` are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased] — v2: the tmux-session fleet

A Quest is now a **fleet of tmux sessions** (one main + workers) rather than a
single window. Claude comes and goes inside each pane; the tmux session outlives
it. Epic `quest: v2 — tmux-session fleet` (bd-v1d), milestones M0–M6.

### Added

- **Topology.** One tmux session per q session: `q-<slug>` for the main,
  `q-<slug>+<label>` for workers. Each pane runs a login shell; Claude is a
  child launched into it.
- **New verbs.** `q start <session> [prompt] [--resume] [--force]` launches
  Claude in a shell pane; `q stop <session>` types `/exit` and leaves the shell;
  `q prompt <session>` prints the stored first prompt; `q cd <quest> <path>`
  (alias of `q set cwd`) moves the Quest cwd while Claude is up.
- **`off` state.** A new session status for "tmux pane alive, no Claude". It is
  never reached from `ended` — a killed or closed session stays ended.
- **Honest waiting.** `waiting:question` (via the `AskUserQuestion` hook pair),
  `waiting:permission` and `waiting:input`; an idle row that just finished its
  turn reads `idle · your turn`.
- **cwd follows the main shell**, edge-triggered on `pane_current_path`
  changing; `q cd` / `q set cwd` reseed the edge and win while Claude is up
  (`[quest] follow_main_cwd`).
- **TUI fleet.** The Sessions tab is a fleet across machines with columns
  quest / label / tmux / state / phase / ctx / age, per-state glyphs, and keys
  `S` (start), `X` (stop), `k` (kill), `W` (spawn a shell-only worker); the
  Quests tab summarises `N tmux · M claude · K waiting`.
- **Remote parity.** `q --machine <name> start/stop/prompt/cd …` proxy over ssh
  like the other Quest commands. The fleet wire carries `tmux_session`, the
  `off` status and `waiting_for`, and remote consumption parses them; an older
  remote missing a field renders `?` / a graceful default and never fails the
  whole machine.

### Changed

- The session state label is a single canonical form everywhere:
  `waiting:<what>` (no space), aligned across the CLI, TUI, brief, spec and
  skill docs.
- `q enter` attaches to the row's own `tmux_session` (workers get their own
  session; `off` rows are enterable). `q close` / `q rm` / `q rename` /
  `q resume` act over the whole fleet (`sessions_of_quest`), and `q resume`
  re-adopts live workers when only the main is gone.
- `q kill` removes a session by pane, so it is safe for pre-v2 window rows.
- `q doctor` checks tmux ≥ 3.2, the `AskUserQuestion` hook pair, that `claude`
  is not a non-exec wrapper, and that shell rc has no `reset`/`stty` flush;
  remote probes detect an older/incompatible `q` and warn.

### Migration

- Schema v4 → v5 (additive): `session.last_pane_path`,
  `session.claude_started_at`. Existing Quests keep working — old worker
  windows survive until killed; new spawns create sessions.
