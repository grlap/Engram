# Engram: a work ledger for coding agents

A presentation on what Engram is and why a Markdown file is not enough, for
a mixed audience of engineers and managers, 15 to 20 minutes. Each slide
gives its text first and speaker notes after `>`. Sources: the README,
[vision](vision.md), [shipped capabilities](shipped.md), and operational
feedback from the Engram and TermAl agent teams. Drafted 2026-10-05 and
refreshed 2026-10-19.

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

- Less duplicated work: one holder per task, visible to sessions using the
  same local project store.
- Recoverable handoffs: an interrupted agent's task, evidence and next action
  are there for the next one.
- Trustworthy completion: a task closes against its criteria and recorded
  evidence, not against the word "done".
- A readable history of outcomes for the humans, instead of chat scrollback.

> For managers this is the slide; the internals come next and can be skimmed.
> The actual history is available from the task record; a human-facing board
> is outside the shipped product.

---

## Slide 3. What Engram is, in one sentence

Engram is a local-first work tracker and execution memory for coding agents:
tasks, decisions, test results and project notes live outside the chat, in a
SQLite database on the developer's machine, behind a CLI and an MCP server.

- Written in Rust. No required cloud service and no external tracker; the
  canonical ledger and ordinary work stay on the machine. An optional,
  operator-configured off-host backup can export copies.
- Fourteen core work commands, listed in the appendix; agents learn them from
  one skill file. Search is MCP `search` or CLI `ls --search`; host-local
  task-sync settings are available from both surfaces.
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

- Git can recover deleted history, but a plain file does not make each decision
  or check a typed, queryable event.
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
  rather than blocking. Our landing workflow gates and reviews the exact
  frozen tree that lands.
- Division of labour: the host measures the source and runs the evaluator;
  Engram enforces what was recorded.

> This is why "the gate passed" and "the gate passed on this exact input" are
> different sentences in our workflow. Without a measuring host, fingerprints
> are what the agent asserts.

---

## Slide 10. Break point 7: retries and lost responses

- Agents time out and retry. Appending to a file twice gives two entries and
  no way to tell.
- Engram: a completion mutation carries an idempotency key, and a replay is
  bound to its target. The same key with the same intent returns the same
  result; the same key aimed at a different item is refused, not replayed onto
  it.
- Retry semantics are per operation, not universal: a same-holder claim
  renews, a late gate on completed work appends, and evaluation attempts use a
  per-item, per-run key scope.

> Small point, but it is what makes unattended agents safe to retry.

---

## Slide 11. Handoff, and what happens when a worker fails

- Handoff is explicit: the holder can offer, the next session can accept, and
  both events are on the record with the task's evidence. In day-to-day
  recovery, a released claim followed by a new claim is also a common transfer.
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

- Engram is the writable tracker for the Engram and TermAl projects. The
  number of active agents varies by the available coding and validation slots.
- Gates, evaluations and landings are recorded in Engram; the session that
  commissioned a review records its findings and disposition on the item.
  Mailboxes, serial landings and two-vendor reviews are process on top of it.
- The humans read outcomes from the record, not from the chat.

> Do not present the team's ceremony as the product: serial gates, limited
> validation capacity and two-vendor reviews are process choices. Engram
> records their evidence and decisions.

---

## Slide 14. What it is not, and what it costs

- Not a scheduler: it says whether selected work is ready; the host runs it.
- Not a shared team board yet: each host has its own SQLite store. A Git-based
  task-text exchange is being built. Export and import can bootstrap an empty
  store; merging several hosts' changes into an existing store is not shipped.
  TermAl, the host, separately provides coordination mailboxes and is building
  its remote session support; those mailboxes carry no Engram task state.
- Not a secrets store, not a transcript archive, not a RAG framework, and
  not an artifact store: evidence is recorded as pointers and summaries. Our
  workflow may retain raw artifacts under the repository while an open task
  needs them.
- Prerelease: an upgrade can require a store migration; backups are the
  operator's job. Day-to-day use and landing gates are on Windows; CI also
  tests Linux and macOS, while some features remain Windows-only.
- Size, observed 2026-10-19 UTC: the Engram store is about 982 MB plus a
  173 MB WAL; the TermAl store is about 887 MB plus an observed 5 MB WAL.
  Views bound returned results; they do not make every query or maintenance
  operation cheap.
- A learning curve: fourteen core commands, plus locators, revisions and
  host workflow concepts. Claims can lapse while a worker waits for capacity.

> Honesty here is what makes slide 6 credible.

---

## Slide 15. The one-line version

A Markdown TODO is a note. Engram is a ledger with rules. The note is cheaper
until you have more than one writer, more than one session, or a need to show
rather than say that work was finished. Today: local-first on each host, with
Git task exchange in active development. TermAl separately provides the
coordination mailbox.

> Close on this line, then take questions. Likely first question: "can I see
> the history?" Answer: `engram work show REF --notes --gates`. A human
> dashboard is outside the shipped product.

---

## Appendix: facts to have ready

- Commands: `next`, `ls`, `show`, `add`, `claim`, `update`, `gate`,
  `evaluate`, `note`, `done`, `handoff`, `remember`, `memories`, `forget`;
  search is MCP `search` or CLI `ls --search`; host-local task-sync settings
  are available from both CLI and MCP; see [CLI and MCP](features/cli-and-mcp.md).
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
