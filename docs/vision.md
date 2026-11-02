# Vision

Engram exists because agent memory today fails in two opposite ways: either
everything is injected into every session until memory crowds out the work it
was meant to inform, or nothing is captured and every session rediscovers the
project from scratch. Both failures were observed directly in production use
of string-keyed memory stores before this design: users hand-encoding type,
author, and date into prose because the schema had nowhere to put them, and
session-start payloads growing linearly until hosts truncated them.

There is a third failure: even correct memory says nothing about what an
agent actually did. Engram therefore records what the host observes around
locally owned work: which session held which claim, what ran, how the source
changed and which registered checks passed. This is Execution Observability,
switched on or off per project in the host. It records facts; it never
admits, delays or refuses a model turn, and an optional agent tool is never
presented as control.

## The observed execution loop

For each session, a claim schedules live execution of one run, and the agent
reads its work context and peer changes through `next`. A host that runs
Execution Observability records the execution, source and test facts it saw
against that claim, and registered verification of the checks it ran. A
source change opens a test obligation that a later registered check
satisfies; completion binds what was recorded. An observed check is a fact
without test credit.

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
options. Publication never mirrors Engram's local event stream. Full-store
backup and restore remain separate from task exchange.

V1 is **local-first, not single-session**: each host keeps its own stable
project-id keyed SQLite store shared by concurrent sessions and worktrees.
The selected optional Git task-sync design explicitly exports task text on
the code branch for agent merge and validated import. Independent hosts can
both write; this provides no global execution exclusion. Task sync is not
shipped, and cross-host execution coordination remains deferred (see
[architecture](architecture.md#optional-git-task-sync)).

## Principles

- **Typed, not a bag of strings.** A hard constraint, a design decision, and
  a session anecdote have different delivery requirements; the model encodes
  that. See [typed memory model](features/typed-memory-model.md).
- **Append-only truth.** Records are immutable and named by a minted id; state
  is derived. Nothing is edited in place, so history and audit come for free.
- **Bounded recovery.** Work context reports visible omissions and directs
  agents to further reads. Project-memory listings discover keys; full reads
  recover their bodies. Stored delivery labels do not inject constraints.
  See [agent usage](features/cli-and-mcp.md#using-engram-as-an-agent).
- **Trust follows origin and authority.** Who asserted something, and how
  binding it claims to be, determine whether it activates immediately or
  awaits approval. See [write policy](features/write-policy-and-review.md).
- **Contradiction is a state, not a merge strategy.** Conflicting claims
  coexist visibly as *contested* until an attributed resolution supersedes
  them — never last-writer-wins.
- **Local authority while working, explicit at every remote boundary.** Live
  claims, delivery state, and agent-private scratch stay on their local
  host. Optional Git task-text exchange uses an explicit disclosure profile,
  with repository visibility and readership recorded at enablement. Publication
  separately sends a frozen report or explicit work projection under an
  idempotent receipt. See
  [local tasks & reports](features/local-tasks-and-reports.md).
- **One write, many views.** Capture happens in the flow of work; the same
  task record drives peer deltas, handoffs, report assembly, and publication.
  Engram must replace duplicate bookkeeping, never add another ledger.
- **One core, many faces.** CLI, MCP server, and any future service front the
  same core for work and project memory, so delivery semantics cannot
  drift.
- **Observation, not authority.** The host records what happened; Engram
  stores and links it. Claims schedule work and grant no file or external
  authority, and no agent authorizes itself through MCP. See
  [Execution Observability](features/execution-observability.md).

## What Engram is not

- **Not a concurrent cross-host organizational planning service in V1.**
  Engram owns each host's local work graph. Optional Git task-text exchange
  shares planning text and gives no globally exclusive execution authority.
  External systems may retain wider organizational commitments or provide
  backup/publication.
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
- [Specification](spec.md) — the normative local-work and Execution Observability design (Draft 0.8)
- [Roadmap](roadmap.md) — the V1 cut and what is deliberately deferred
