# research

Investigate and report. **No production code changes.** The deliverable is a written answer
someone can act on — not a patch, not a refactor, not a "while I was in there".

Scratch files, throwaway scripts and spikes are fine as long as they stay out of the commit
and you say they are spikes.

## The loop

1. **Sharpen the question.** Write the question you are actually answering in one sentence,
   and the two or three sub-questions under it. `q note` it. If the goal in the brief is
   vaguer than that, sharpen it yourself and say what you assumed — then get on with it.
2. **Say what would change your mind.** Before you look: what answer are you expecting, and
   what evidence would falsify it? Written down first, this is what stops the investigation
   from confirming itself.
3. **Gather.** Fan out — one agent per independent sub-question, so no thread pollutes
   another. `q spawn` a worker for anything long (crawling a large codebase, running a
   benchmark, reproducing a bug); a subagent for a bounded lookup. Never more than 3 at once.
4. **Weigh.** Separate what you **measured**, what you **read**, and what you **inferred**.
   These are three different strengths of claim and the reader needs to see which is which.
5. **Answer.** The report, below.

## The report

- **The answer first.** One paragraph, at the top, that stands alone. Someone who reads only
  this must get the actual conclusion — not "it depends" and not a summary of your process.
- **Then the evidence**, each claim with its source: `file:line`, a query, a command and its
  output, a URL. A claim without a source is an opinion, and opinions go in their own section.
- **Then what you could not establish**, and what it would take. This section is not optional;
  an investigation with no unknowns left has usually stopped looking.
- **Then the options**, if there is a decision to make — each with its cost, its risk, and who
  it hurts. Recommend one and say why.
- Write it to a file, register it (`q artifact add findings.md`), and `q note` the one-line
  answer so it lands on the timeline even after a reset.

## Rules

- **No production code changes**, not even an obvious one-line fix. It goes in the report as a
  recommendation, or in beads as an issue — `bd create "<title>" -l repo:<repo>,quest:<id>`.
- **Cite or hedge.** Every factual claim either carries its source or is marked as a guess.
  Never present an inference as a reading.
- **Negative results are results.** "This does not happen, and here is how I know" is a
  finished investigation.
- **Do not tidy the answer into certainty.** If two sources disagree, report the disagreement.
- Timebox: when you have the answer to the question you wrote down in step 1, stop. Interesting
  adjacent things are `bd create` and a line in the report.
- When the report is delivered: closing `q note`, then propose `q close <quest>`.

## worker

You were given one sub-question of a larger investigation. Answer exactly that.

- **Read-only.** You change nothing in the repository. Scratch files under `/tmp` are fine.
- Answer **your** sub-question. Something interesting next door is one line at the end, not a
  new investigation.
- Every claim carries its source: `file:line`, the command you ran and what it printed, the
  URL. Mark inferences as inferences.
- "I could not find out" is an answer — with what you tried and what would settle it. Do not
  fill a gap with a plausible guess.
- Write your findings to a file, `q artifact add <path> --note "<sub-question>"`, and tell the
  master the path plus your one-line answer. Not the whole thing.
- `q phase "<where you are looking>"` as you go; `q note --blocker` if you are stuck on access
  or tooling.
