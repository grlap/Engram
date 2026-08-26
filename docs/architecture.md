# Architecture

Engram is a Rust workspace built around one core library. The CLI and the MCP
server are thin faces over that core; neither reimplements any domain logic.
This is a summary — the [specification](spec.md) is normative.

“Local” means one host, not one session: concurrent sessions and isolated
worktrees resolve the same stable project id to one host-local store.

## Components

```
┌─────────────┐   ┌─────────────┐
│ engram CLI  │   │ MCP server  │          thin faces (§ interfaces)
└──────┬──────┘   └──────┬──────┘
       └───────┬─────────┘
               ▼
┌──────────────────────────────────┐
│            core library          │
│  object model · derived state ·  │
│  context packets · write policy ·│
│  leases · task feed · report gate│
└───┬──────────┬──────────┬────────┘
    │ Store    │ Index    │ Tracker        ports (traits)
    ▼          ▼          ▼
┌────────┐ ┌────────┐ ┌──────────────────┐
│ SQLite │ │ FTS5 + │ │ DummyTracker-    │
│objects │ │derived │ │ Adapter (V1)     │
│(canon.)│ │(cache) │ │ proprietary later│
└────────┘ └────────┘ └──────────────────┘
```

Ports (Rust traits) keep domain semantics independent of backends:

| Port | Responsibility | V1 implementation |
| --- | --- | --- |
| `Store` | append / get / list-heads over immutable objects | SQLite (canonical) |
| `Index` | rebuild / search derived state | SQLite + FTS5 tables (disposable cache) |
| `Sync` | fetch / push / verify between stores | dormant in V1 |
| `Tracker` | normalize / read / publish against the external ticketing system | `DummyTrackerAdapter` |
| `Redactor` | pre-write DLP / secret scanning | visibly labeled no-op |
| `Signer` | optional cryptographic attestation | not shipped in V1 |

## Object model

Everything canonical is an **immutable, content-addressed object**: memory
versions, events (approve, retract, verify, tombstone, resolve), edges, and
evidence. Objects serialize as RFC 8785 (JCS) canonical JSON; an object's id
is the SHA-256 of its canonical bytes (hash field excluded), verified at read
time. Unknown schema versions are retained but not activated; migrations mint
new objects, never rewrite old ones.

A **memory** is a stable id plus an append-only chain of versions with parent
links. Multiple unsuperseded heads mean *contested*. Status (`proposed`,
`active`, `contested`, `stale`, `expired`, `retracted`, `tombstoned`) is
derived from the object graph, never stored as a mutable field. See
[typed memory model](features/typed-memory-model.md).

## Storage

V1's canonical store is a **local SQLite database**: object rows keyed by
content hash, written transactionally, append-only by core-enforced contract.
Derived tables (heads, status, FTS5 full-text, usage counters) are a cache
rebuilt deterministically from the objects (`engram rebuild-index`);
`engram doctor` verifies hashes and index freshness. Durability is
transactions plus local backup and deterministic JSONL export (interchange
only — never canonical). See [SQLite store](features/sqlite-store.md).

The database is selected by stable project id rather than the current
checkout path. A tracked `.engram-project` file is resolved below
`ENGRAM_HOME` using an opaque hash. WAL mode, bounded busy waits, short
transactions, and atomic compare-and-swap operations support multiple processes. Immutable task events
receive a monotonic local cursor; clients use that cursor for peer deltas.
Current leases and indexes are mutable projections whose transitions remain
auditable through the event stream.

### Deferred team backend

A dedicated Git repository of append-only objects (`objects/<sha256>.json`,
set-union merges, concurrent heads surfacing as contested, tombstones
preventing resurrection) is the designed team-sync backend — **deferred, not
rejected**. Because objects are already content-addressed under the same
bytes contract, migration is exporting rows to files behind the same
`Store`/`Sync` ports. One of its rules applies from day one because it cannot
be retrofitted: sensitive values never enter any shared history — vault
references only.

## Data flow

1. **Write**: an assertion passes the `Redactor` port, gets attributed
   (asserted runtime context + assurance level), and either activates or
   lands as `proposed` per the write-policy matrix. See
   [write policy & review](features/write-policy-and-review.md).
2. **Read**: session start (or explicit request) builds a **context packet**
   — pinned constraints (complete or fail-closed), a titles-only index, an
   omission manifest, a packet hash for reproducibility, and a task event
   cursor. Later turns
   request only the peer delta after that cursor. See
   [context packets](features/context-packets.md).
3. **Coordinate**: sessions atomically claim leased work, append decisions and
   evidence to task-shared memory, and explicitly hand off. Host mailboxes may
   wake peers, but Engram's ordered task feed remains authoritative.
4. **Publish**: participant contributions satisfy the finalization barrier;
   Engram deterministically assembles one report for polishing, freezes it at
   `report_ready`, and binds it to a publication idempotency key. The
   `Tracker` adapter publishes it and returns a receipt. See
   [local tasks & reports](features/local-tasks-and-reports.md) and
   [tracker adapter](features/tracker-adapter.md).

## Interfaces

The CLI (`engram …`) and the MCP server expose the same core, including
packet construction and task deltas. Host integration injects
`memory_context` at session start and requests `memory_delta` after a wake or
before the next work turn. The host owns notifications; Engram owns durable
state. See [CLI & MCP](features/cli-and-mcp.md).

## Security posture

Actor identity is asserted runtime context, recorded with an assurance level
and never overclaimed; redaction runs pre-write (labeled no-op in V1);
sensitivity labels gate retrieval; purge is a tombstone in V1 with physical
erasure documented as an exceptional runbook. See
[security & trust](features/security-and-trust.md).

## Further reading

- [Specification](spec.md) — normative
- [Vision](vision.md) — why it is shaped this way
- [Roadmap](roadmap.md) — V1 boundaries and deferrals
