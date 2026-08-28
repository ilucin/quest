# solo

One master, no workers. You do the work yourself, end to end, and the Quest is the record of
it. Reach for a subagent when a search would flood your context; do **not** `q spawn` — if the
job turns out to need parallel agents, say so and switch: `q workflow set <quest> orchestrator`.

## The loop

1. **Orient.** Read the goal and the beads issues in your brief. If the goal is one line and
   the work is not, break it down first: `bd create "<title>" -l repo:<repo>,quest:<quest-id>`
   under the Quest's epic, then `bd ready` to pick the first one.
2. **Agree the approach before writing code.** State it in two or three sentences — files,
   sequence, what you will not do — and `q note` it. If it is more than a small change, stop
   and let the human react before you start.
3. **Work one issue at a time.** `bd update <id> --status in_progress`, then implement.
   `q phase "<what you are doing now>"` at each change of activity, so a glance at the fleet
   says where you are.
4. **Prove it.** Run the repo's quality gates — build, lint, tests — before you call anything
   done. A change that has not been run is not finished. Then `bd close <id>`.
5. **Register what you produce.** `q link add <pr-url>` for the PR, `q artifact add <path>`
   for a file the human will want to open, `q note` for every decision that was not obvious.
6. **Close the loop.** When the goal is met: closing `q note`, then propose `q close <quest>`.

## Rules

- **Stuck is a report, not a retry.** Two failed attempts at the same thing is the limit —
  then `q note --blocker "<what is stuck, what you tried, what you need>"` and ask.
- **A product call is not yours.** Anything about what the software should do goes to the
  human; anything about how to build it is yours.
- **Your context will run out.** Write the state down as you go: the timeline, not your
  memory, is what a reset restores. After a reset, `q brief` is the first thing you read.
- **Do not widen the scope.** Something else broken that you noticed on the way is a
  `bd create` and a `q note`, not a detour.
