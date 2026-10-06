# Engram: a work ledger for coding agents

A presentation on what Engram is and why a Markdown file is not enough, for
a mixed audience of engineers and managers, 15 to 20 minutes. Each slide
gives its text first and speaker notes after `>`. Sources: the README,
[vision](vision.md), [shipped capabilities](shipped.md), and one month of
real use by the Engram and TermAl agent teams; drafted by an Engram agent
with feedback from six Engram and TermAl agents folded in.

---

## Slide 1. Why not just a Markdown file?

- For one person, one agent and one short task, a Markdown TODO is enough.
- It stops being enough with two agents, a context reset, or a need to show,
  rather than say, that work was finished.
- A plain file does not enforce anything: no transaction, no bounded view, no
  refusal. Software built around files could; by then you are building a
  ledger.
- This talk is about where the file breaks and what we put in its place.

> Open with the objection, not the product. Concede the small case out loud.
> Do not claim a file "cannot" do these things: Git keeps deleted text and
> files can carry several writers. The point is what a plain file enforces,
> which is nothing.

---

## Slide 2. What it changes for a team

- Less duplicated work: one holder per task, visible to every session.
- Recoverable handoffs: an interrupted agent's task, evidence and next action
  are there for the next one.
- Trustworthy completion: a task closes against its criteria and recorded
  evidence, not against the word "done".
- A readable history of outcomes for the humans, instead of chat scrollback.

> For managers this is the slide; the internals come next and can be
> skimmed. The readable board of what happened belongs here, not in the
> appendix.

---

## Slide 3. What Engram is, in one sentence

Engram is a local-first work tracker and execution memory for coding agents:
tasks, decisions, test results and project notes live outside the chat, in a
SQLite database on the developer's machine, behind a CLI and an MCP server.

- Written in Rust. No required cloud service and no external tracker; the
  canonical ledger and ordinary work stay on the machine. An optional,
  operator-configured off-host backup can export copies.
- Fourteen agent commands, listed in the appendix; agents learn them from one
  skill file.
- The record is append-only (notes, gates, verdicts, seals); live state such
  as who holds what is kept in current rows that recovery reads.

> Say "local-first", not "nothing leaves": backup push and restore already
> ship. Say "append-only record plus current state", not "everything is
> immutable": claims, focus and grants are mutable rows by design.

---

## Slide 4. Break point 1: two agents at once

- Two sessions read the same file, both pick "the next task", both edit it.
  Last writer wins; the loser's work is lost or duplicated.
- Engram: `claim` is a transaction. One holder per task, with an expiry; a
  dead session's claim lapses and the next session recovers it, on the
  record.

> Demo if you can: two terminals claim the same task. One gets it; the other
> is told who holds it and until when. Thirty seconds, no slide needed.

---

## Slide 5. Break point 2: the agent's context resets

- After a restart or a context compaction the agent re-reads the whole file.
  Files grow; either every turn starts with 40 KB of history, or someone
  prunes it and the history is gone.
- Engram: `next --peek` returns a bounded, computed view (what you hold, what
  changed, what is ready) with the full history behind it. Project notes have
  keys and revisions, so a correction does not pile onto the old text.
- The host has to tell the agent to look: after a compaction the hosting
  runtime injects the reminder, and the agent recovers its rules and its task
  from the ledger rather than from a peer's summary.

> Example from one night of use: the coordinator's context was compacted
> mid-landing; it read the peek and the memory keys and resumed the landing
> without asking anyone. Second example: an agent needed an original timing
> measurement of 6,733 members and the later council ruling on it; both came
> back by their note locators while the bounded window paged 29 newer notes,
> so the original could be told from a new benchmark without copying history.
> Call this computed recovery with provenance; a file program could be built
> to do it, nobody builds it into a TODO file.

---

## Slide 6. Break point 3: "done" is a word the agent wrote

- In a file, done means the agent typed "done".
- Engram: a task carries acceptance criteria. For a criterion bound to a
  host-observed check, completion refuses while that check is missing or
  stale. Under an evaluation policy, an evaluator (configurable; today a
  separate session that never held the task) records a verdict per criterion,
  and the executor cannot complete over a failing verdict.
- A verdict is an attributed judgment, not proof the outcome is correct. This
  is a supervised prerelease workflow, not an autonomous guarantee.

> Two real examples from one week. (1) A task's tests looked complete; the
> independent evaluator read the criterion literally, found the test did not
> do what the criterion said, and failed it. The fix was a better test. (2) An
> evaluator passed a task's measurements but failed acceptance because a
> follow-up had not bounded history growth; completion stayed open until the
> council recorded a temporary acceptance of the linear cost, and the ledger
> enforced that recorded verdict and kept the decision. A file would have said
> "done" both times.

---

## Slide 7. Break point 4: the wrong item

- One session often holds several tasks. In a file, "done" lands on whatever
  was touched last.
- Engram: a bare `done` or `evaluate` while more than one claim is live is
  refused; it names every held item and records nothing.

> Example: an agent held two items and a bare completion would have sealed
> the one focused last. Engram refused and listed both. Separately, an
> evaluation that cited a peer's observation was refused, because only the
> holder's own records count as its evidence.

---

## Slide 8. Break point 5: who decided this, and when?

- A file has git blame at best, and only for what survived editing.
- Engram: every note, claim, gate, verdict and completion is an attributed,
  append-only event. The audit trail is the data, not a separate log.
- Project memory holds durable decisions by key and revision, with the
  source of the decision (who said it, when) in the entry.

> Attribution is asserted: the agent says who it is; nothing is
> authenticated. Fine for a single developer's machine, wrong to oversell.

---

## Slide 9. Break point 6: the source changed under the work

- A file does not know that the tests passed on yesterday's tree.
- Engram: with a measuring host, evidence is bound to a source fingerprint
  the host takes for recognised test runners. Change the code and the old
  pass is recorded as stale; the stock rule records the untested change
  rather than blocking, and the evaluation is redone on the exact tree that
  lands.
- Division of labour: the host measures the source and runs the evaluator;
  Engram enforces what was recorded.

> This is why "the gate passed" and "the gate passed on this exact input" are
> different sentences in our workflow. Without a measuring host, fingerprints
> are what the agent asserts.

---

## Slide 10. Break point 7: retries and lost responses

- Agents time out and retry. Appending to a file twice gives two entries and
  no way to tell.
- Engram: mutations carry an idempotency key, and a replay is bound to its
  target. The same key with the same intent returns the same result; the same
  key aimed at a different item is refused, not replayed onto it.
- Retry semantics are per operation, not universal: a same-holder claim
  renews, a late gate on completed work appends.

> Small point, but it is what makes unattended agents safe to retry.

---

## Slide 11. Handoff, and what happens when a worker fails

- Handoff is explicit: the holder offers, the next session accepts, and both
  events are on the record with the task's evidence.
- A failure is carried, not hidden: a worker that cannot finish leaves the
  failed check and its diagnosis on the item, and the next worker starts
  from them.

> Example: a Kimi worker could not finish a change; its failed check and
> notes stayed on the item, a Claude worker picked it up from the handoff and
> finished without redoing the diagnosis.

---

## Slide 12. Before and after, one workflow

- Before: an agent is interrupted mid-task. The next agent reads the chat
  scrollback or a TODO file, guesses what was done, re-runs the tests it can
  find, and may finish a different task than the one that was open.
- After: the next agent calls `next --peek`, sees the open task, the checks
  recorded on it, the last decision and the next command; claims it; runs the
  one check that is missing; completes against the criteria.

> Keep this to one minute. It is the whole product in one picture.

---

## Slide 13. In use today

- Engram is the only writable tracker for two live projects: Engram itself
  (three implementers, one coordinator, one advisor) and TermAl, the agent
  host (five implementers, two architects, a coordinator).
- Gates, reviews, evaluations and landings are recorded in Engram; the team
  coordinates them through its own process (mailboxes, serial landings,
  two-vendor reviews) on top of it.
- The humans read outcomes from the record, not from the chat.

> Do not present the team's ceremony as the product: serial gates, "LAUNCH
> NOW" and two-vendor reviews are our process choices. Engram records them.

---

## Slide 14. What it is not, and what it costs

- Not a scheduler: it says whether selected work is ready; the host runs it.
- Not a shared team board yet: one store per developer's machine; shared
  multi-machine use is deliberately not offered.
- Not a secrets store, not a transcript archive, not a RAG framework, and
  not an artifact store: evidence is recorded as pointers and summaries, so a
  removed worktree takes its raw fixtures with it.
- Prerelease: an upgrade can require a store migration; backups are the
  operator's job; only Windows is validated today.
- Size: the ledger keeps everything. Engram's own project store is about
  540 MB and TermAl's about 410 MB after a month of heavy multi-agent use;
  reads stay bounded because views are computed, not because the file is
  small.
- A learning curve: fourteen commands, learned from one skill file.

> Honesty here is what makes slide 6 credible.

---

## Slide 15. The one-line version

A Markdown TODO is a note. Engram is a ledger with rules. The note is cheaper
until you have more than one writer, more than one session, or a need to show
rather than say that work was finished. Today: a local Windows pilot.

> Close on this line, then take questions. Likely first question: "can I see
> the history?" Answer: `engram work show REF --notes --gates`, plus a static
> board for humans.

---

## Appendix: facts to have ready

- Commands: `next`, `ls`, `show`, `add`, `claim`, `update`, `gate`,
  `evaluate`, `note`, `done`, `handoff`, `remember`, `memories`, `forget`;
  see [CLI and MCP](features/cli-and-mcp.md).
- Store: `ENGRAM_HOME/projects/<project>/engram.db`, plain SQLite; local
  backups with `engram backup`; `engram doctor` verifies; an optional
  off-host backup target is configured by the operator
  ([off-host backup](features/off-host-backup.md)).
- Identity: asserted, not authenticated; the OS login name is the default
  actor.
- Secrets: the redactor is a no-op; never write credentials or customer data
  into notes; rotate anything that slips in.
- Limits in code: depth 4, 255 open descendants per root, 16 children per
  decomposition, 1024 prerequisites per item
  ([local work system](features/local-work-system.md)).
- License: Apache-2.0, no warranty.
