# Vision

Engram exists because agent memory today fails in two opposite ways: either
everything is injected into every session until memory crowds out the work it
was meant to inform, or nothing is captured and every session rediscovers the
project from scratch. Both failures were observed directly in production use
of string-keyed memory stores before this design: users hand-encoding type,
author, and date into prose because the schema had nowhere to put them, and
session-start payloads growing linearly until hosts truncated them.

There is a third failure: even correct memory is merely advice if an agent can
start the next turn, mutate shared work, or publish while stale, unclaimed, or
unfinished. Engram's target architecture therefore owns the behavioral and
coordination decision layer around locally owned work. A host runtime must enforce
its grants at prompt and declared tool boundaries; Engram does not pretend an
optional agent tool is control.

## The controlled execution loop

For each session, the target architecture determines whether required context
and peer deltas were host-confirmed as delivered, ownership is current,
previous effects are reconciled, and lifecycle barriers are satisfied. The
normal pre-turn result is a bounded grant with required delivery inlined; a
typed recovery directive handles the unsafe tail. At the strongest integration
level, the host also requests a single-use grant immediately before every
declared material action and records its outcome before the next turn.

Engram derives a bounded, deterministic ready-work view, but it does not
supervise processes. The host or model selects among allowed candidates; the
host chooses the model, process, prompts, and user-authorized tools. Engram
decides whether that selected execution is ready and coordinated. Effective
authority is the intersection of host/user policy and Engram state.

## The dual-layer model

Engram's answer combines a local work graph with two memory/report layers:

**The local work graph** records roots, decomposition, prerequisites,
assignment, priority, readiness, fenced claims, evidence, and completion.
Work may originate from a human/model prompt or an explicit external snapshot.
No external tracker is required.

**Local working memory** serves the machine doing the work. While a task is
underway it coordinates concurrent sessions, tracks local task state, and
records decisions, evidence, constraints, handoffs, and intermediate
findings. Agent-private scratch stays private; task memory is shared by
default among participants. It is append-only, attributed, and retrieved
under strict byte budgets — an agent's context always has room for the task
itself.

**The optional durable final report** serves wider audiences. When a root completes,
Engram distills its working memory into a polished report — outcome, work
performed, decisions and rationale, discovered constraints, validation,
risks, promotion candidates, provenance—and freezes it. A separately
authorized adapter may publish those bytes under an idempotent, receipted
handoff. Intake, external durability, and publication are independent
options. Publication never mirrors Engram's local event stream; an optional
portable store replicates the canonical work projection for recovery/handoff.

V1 is **local-first, not single-session**: one active host and one stable
project-id keyed SQLite store shared by concurrent sessions and worktrees.
Optional `portable` mode moves that store between hosts sequentially through a
canonical projection, explicit handoff/restore, and divergence refusal. Live
cross-host team sync remains later (see
[architecture](architecture.md#portable-and-synchronized-backends)).

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
- **Local authority while working, explicit at every remote boundary.** Live
  claims, leases, grants, delivery state, and agent-private scratch stay on the
  active host. A configured portable target may receive a sensitivity-filtered
  shared work projection for sequential handoff; publication separately sends
  a frozen report or explicit work projection under an idempotent receipt. See
  [local tasks & reports](features/local-tasks-and-reports.md).
- **One write, many views.** Capture happens in the flow of work; the same
  task record drives peer deltas, handoffs, report assembly, and publication.
  Engram must replace duplicate bookkeeping, never add another ledger.
- **One core, many faces.** CLI, MCP server, and any future service front the
  same core — including packet construction — so delivery semantics cannot
  drift.
- **Control requires mediation.** Engram decides; the host enforces. Turn and
  action grants are bounded, checkpointed, and invalidated by relevant policy
  or ownership changes. An agent never self-authorizes through MCP. See the
  [behavioral control plane](features/behavioral-control-plane.md).

## What Engram is not

- **Not a concurrent cross-host organizational planning service in V1.**
  Engram owns the active host's work graph. Optional external systems may
  retain wider organizational commitments, provide backup/publication, or
  transfer a portable snapshot to the next active host.
- **Not a scheduler or process supervisor.** It refuses or directs execution
  selected by the host; it does not choose agents, prompts, or backlog order.
- **Not a transcript archive.** Raw session logs are not persisted by
  default.
- **Not a secrets store.** Sensitive values are held as vault references,
  never as remembered plaintext.
- **Not a RAG framework.** Engram stores curated, attributed claims, not
  crawled corpora.

## Further reading

- [Architecture](architecture.md) — how the pieces fit
- [Specification](spec.md) — the normative local-work/control design (Draft 0.8)
- [Roadmap](roadmap.md) — the V1 cut and what is deliberately deferred
