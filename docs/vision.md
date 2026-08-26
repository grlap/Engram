# Vision

Engram exists because agent memory today fails in two opposite ways: either
everything is injected into every session until memory crowds out the work it
was meant to inform, or nothing is captured and every session rediscovers the
project from scratch. Both failures were observed directly in production use
of string-keyed memory stores before this design: users hand-encoding type,
author, and date into prose because the schema had nowhere to put them, and
session-start payloads growing linearly until hosts truncated them.

## The dual-layer model

Engram's answer is two deliberate layers with different lifetimes and
audiences:

**Local working memory** serves the machine doing the work. While a task is
underway it coordinates concurrent sessions, tracks local task state, and
records decisions, evidence, constraints, handoffs, and intermediate
findings. Agent-private scratch stays private; task memory is shared by
default among participants. It is append-only, attributed, and retrieved
under strict byte budgets — an agent's context always has room for the task
itself.

**The durable final report** serves everyone else. When a task completes,
Engram distills its working memory into a polished report — outcome, work
performed, decisions and rationale, discovered constraints, validation,
risks, promotion candidates, provenance — freezes it, and publishes it to the
organization's external tracker under an idempotent, receipted handoff. The
tracker is the durable cross-team publication boundary. It never mirrors
Engram's local event stream, and Engram never mirrors tickets.

V1 is **local-first, not single-session**: one machine and one stable
project-id keyed SQLite store shared by concurrent sessions and worktrees, no
cross-host server or team sync. The design preserves an explicit migration
path to a shared backend (see
[architecture](architecture.md#deferred-team-backend)).

## Principles

- **Typed, not a bag of strings.** A hard constraint, a design decision, and
  a session anecdote have different delivery requirements; the model encodes
  that. See [typed memory model](features/typed-memory-model.md).
- **Append-only truth.** Records are immutable and content-addressed; state
  is derived. Nothing is edited in place, so history and audit come for free.
- **Budgeted delivery.** Injection operates under hard byte budgets with
  visible omission; the constraint tier fails closed rather than truncating
  silently. See [context packets](features/context-packets.md).
- **Trust follows origin and authority.** Who asserted something, and how
  binding it claims to be, determine whether it activates immediately or
  awaits approval. See [write policy](features/write-policy-and-review.md).
- **Contradiction is a state, not a merge strategy.** Conflicting claims
  coexist visibly as *contested* until an attributed resolution supersedes
  them — never last-writer-wins.
- **Local while working, explicit when publishing.** Working memory stays on
  the machine; nothing crosses the organizational boundary except a finalized
  report, published through an idempotent adapter under a receipt. See
  [local tasks & reports](features/local-tasks-and-reports.md).
- **One write, many views.** Capture happens in the flow of work; the same
  task record drives peer deltas, handoffs, report assembly, and publication.
  Engram must replace duplicate bookkeeping, never add another ledger.
- **One core, many faces.** CLI, MCP server, and any future service front the
  same core — including packet construction — so delivery semantics cannot
  drift.

## What Engram is not

- **Not the organizational tracker.** Engram tracks local operational tasks
  while work is underway, but the external ticketing system stays
  authoritative for the organizational work item.
- **Not a transcript archive.** Raw session logs are not persisted by
  default.
- **Not a secrets store.** Sensitive values are held as vault references,
  never as remembered plaintext.
- **Not a RAG framework.** Engram stores curated, attributed claims, not
  crawled corpora.

## Further reading

- [Architecture](architecture.md) — how the pieces fit
- [Specification](spec.md) — the normative multi-session design (Draft 0.5)
- [Roadmap](roadmap.md) — the V1 cut and what is deliberately deferred
