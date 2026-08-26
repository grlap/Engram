# Engram

Audit-grade memory for humans and coding agents — local working memory while
the work runs, a polished durable report when it lands.

Engram is a standalone Rust tool (CLI + MCP server): a local, concurrent
execution-memory service for multiple agent sessions. It gives agents working
on one machine two deliberate lifecycle layers:

1. **Local working memory** — agent-private scratch plus a task-shared working
   set for constraints, decisions, evidence, ownership, handoffs, and
   intermediate findings. The shared task ring is the operational source of
   truth during execution.
2. **A durable final report** — at task completion, Engram assembles working
   memory and participant contributions for one polish/freeze step, then
   publishes the immutable report to the
   organization's external ticketing system under an idempotent, receipted
   handoff. The tracker is the durable cross-team publication boundary — never
   a mirror of Engram's local event stream.

The multi-session-first design is in the [specification](docs/spec.md) (Draft
0.5). Start with the [vision](docs/vision.md) for the short version.

## Highlights

- **Typed memory, not a bag of strings** — every memory has a `kind`
  (constraint / decision / convention / fact / preference / episode), an
  `authority` (hard / firm / soft), and a delivery mode derived from both.
  See [typed memory model](docs/features/typed-memory-model.md).
- **Budgeted context packets** — three retrieval rungs under hard byte
  budgets: pinned constraints (complete or fail-closed), a titles-only index,
  and on-demand full-text search. Every packet has a reproducible content hash
  plus an ordered event cursor for peer deltas. See
  [context packets](docs/features/context-packets.md).
- **Multi-session coordination by default** — task-shared memory, atomic
  claims/leases, explicit handoff, ordered peer deltas, and a participant
  barrier before report freeze.
- **One write, many views** — low-friction prose capture feeds task state,
  handoffs, and the final report instead of creating another status ledger.
- **Append-only truth** — immutable, content-addressed versions and events;
  state is derived, history is the audit log. Contradictions become visible
  *contested* state, never last-writer-wins.
- **Local tasks with a finalization pipeline** — `active →
  finalization_pending → report_ready → publishing → published`, where
  `report_ready` freezes an immutable report bound to a publication
  idempotency key, and only an adapter receipt marks `published`. See
  [local tasks & reports](docs/features/local-tasks-and-reports.md).
- **Vendor-neutral tracker boundary** — the core contains no ticket-shaped
  types; V1 ships a `DummyTrackerAdapter` that proves the exact production
  contract with deterministic local receipts. See
  [tracker adapter](docs/features/tracker-adapter.md).
- **Honest trust model** — actor identity is asserted runtime context (tool +
  skill instruction metadata), recorded with an assurance level, never
  overclaimed. See [security & trust](docs/features/security-and-trust.md).

## Architecture at a glance

```
CLI (engram) ─┐
              ├──► core library ──► canonical store: local SQLite
MCP server  ──┘        │            (append-only, content-addressed objects)
                       │                     │ rebuild
                       │                     ▼
                       │            derived tables (heads, status, FTS5, usage)
                       │
                       └─ finalize → frozen report → publish (idempotent, receipted)
                                                      │
                                                      ▼
                                        Tracker adapter (DummyTrackerAdapter in V1)
```

One core library owns the object model, derived state, and context-packet
construction; the CLI and the MCP server are thin faces over it. Details in
[architecture](docs/architecture.md).

## Status

The first coding-agent memory loop is implemented and process-level tested:
sessions rendezvous by external task reference, capture task-shared or
agent-private prose, receive bounded context and ordered deltas, inspect
provenance, survive MCP process restart, and coordinate with expiring claims.
Natural rule cues become pinned constraints, claim retries preserve their
original lease, and explicitly declared pinned contradictions stop context
assembly before an agent acts.
Report assembly/publication and the broader administrative CLI remain under
development.

The CLI requires `ENGRAM_HOME` (or `--home`) and resolves this repository's
tracked `.engram-project` identity to one database shared across worktrees.
Initialize with `engram init`, verify with `engram doctor`, and run the MCP
server with `engram mcp --actor-id <agent> --session-id <session>`. See the
[CLI and MCP guide](docs/features/cli-and-mcp.md) for host configuration and
the exact shipped tool set.

## Documentation

| Doc | What it covers |
| --- | --- |
| [Vision](docs/vision.md) | Why Engram exists; the dual-layer model; principles |
| [Architecture](docs/architecture.md) | Components, object model, ports, data flow |
| [Specification](docs/spec.md) | The normative Draft 0.5 working design |
| [Feature briefs](docs/features/README.md) | Per-feature design briefs |
| [Development](docs/development.md) | Dev & review workflow, quality gates, conventions |
| [Roadmap](docs/roadmap.md) | V1 cut, V1.x, and what is deliberately deferred |
