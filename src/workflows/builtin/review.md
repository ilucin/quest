# review

Review an existing diff — a PR, a branch, a working tree. You **read and judge; you do not
fix**. A review that quietly rewrites the code it was asked to assess is no longer a review,
and the author never learns what was wrong.

## First

Establish exactly what is under review and record it, or every finding afterwards is about an
unknown quantity:

- `q link add <pr-url>` — the PR, if there is one.
- `gh pr diff <n>` / `git diff <base>...<head>` — the diff itself. Read **all** of it before
  judging any of it.
- The PR body, the linked issue, the beads epic: what was this *supposed* to do?
- `q phase "reading the diff (<n> files, <m> lines)"`.

If the intent is not written down anywhere, that is finding number one — ask before reviewing.

## The lenses

Run each as its own agent so the findings do not contaminate each other, then merge them into
one report. Spawn a worker (`q spawn`) for a lens that has to bring up an environment or read
a large codebase; a subagent is enough for the rest.

| Lens | Looks for |
|---|---|
| `correctness` | Bugs. Deviations from the stated intent. Edge cases, error paths, concurrency, N+1s, data loss. Missing tests for the behaviour that changed. |
| `knowledge` | The diff against this repo's conventions — CLAUDE.md, existing patterns, the knowledge docs. Where it goes against the grain, and why that matters here. |
| `bloat` | Premature abstraction, dead code, needless indirection, a 200-line change where 20 would do. |
| `blast` | What else this touches: shared code, background jobs, migrations, integrations, permissions, billing. Reversibility. |

Skip a lens that plainly does not apply and say that you skipped it.

## Reporting

- **Every finding carries `file:line`, what is wrong, and what to do instead.** A finding
  without a location is a feeling.
- **Rank by severity**, not by file order: blocking (correctness, data, security) first, then
  should-fix, then nits. Label the nits as nits.
- **Say what is good.** A review that only lists problems is unreadable and untrustworthy.
- **Confidence out loud.** "I could not run this" and "I am not sure this path is reachable"
  are useful; a confident wrong finding costs the author more than silence.
- Write the report to a file and register it: `q artifact add review.md --note "<pr> review"`.
- `q note` the verdict in one line: approve / approve-with-nits / changes-requested, and the
  single reason.

## Rules

- **Never push to the branch under review** and never `gh pr merge`. If the author asked for
  fixes, that is a different Quest with a different workflow.
- **Never post to GitHub without explicit confirmation from the human.** Draft the comments,
  show them, wait.
- Disagreement between lenses goes in the report as a disagreement — do not average it away.
- When the report is delivered: `q note` it, then propose `q close <quest>`.

## worker

You are one lens of a review. You were given a diff and a single thing to look for.

- Look for **your lens only**. Another lens's territory is not your finding; if you trip over
  something important outside your scope, one line at the end under "outside my lens".
- **Read; do not edit.** No file in the repository changes because of you. No commits, no
  pushes, no comments posted anywhere.
- Every finding: `file:line`, what is wrong, why it matters, what to do instead. Ranked by
  severity, nits labelled as nits.
- Write it to a file, register it (`q artifact add <path> --note "<lens> findings"`), and tell
  the master the path and the headline count — not the whole report.
- Nothing found is a real answer. Say so plainly rather than manufacturing a nit.
- `q phase "<lens>: <where you are>"` as you go; `q note --blocker` if you cannot get the diff.
