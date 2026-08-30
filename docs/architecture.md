# Architecture

Engram is a Rust workspace built around one core library. The target V1
architecture keeps CLI, agent-facing MCP, and the host-private control
transport as thin faces over that core; none reimplements domain logic. This
is a summary — the [specification](spec.md) is normative.

“Local” means one host, not one session: concurrent sessions and isolated
worktrees resolve the same stable project id to one host-local store. The
local work graph is complete without external intake, storage, or publication;
each is an independent optional adapter capability.

## Components

```
optional intake ────────────────────────────────┐
                                               ▼
┌──────────────────┐       ┌─────────────┐  ┌─────────────┐
│ Host runtime     │       │ engram CLI  │  │ Agent MCP   │
│ + enforcement SDK│       └──────┬──────┘  └──────┬──────┘
└───────┬──────────┘              └────────┬─────────┘
        │ private control                  │ work / memory requests
        ▼                                  ▼
┌────────────────────────────────────────────────────────┐
│ core: work graph · control · memory · evidence/report  │
└──────────┬──────────────────┬──────────────────┬───────┘
           │ Store            │ Index            │ optional adapters
           ▼                  ▼                  ▼
┌──────────────────┐   ┌──────────────┐   backup/portable/sync/publication
│ SQLite canonical │   │ FTS5 + work  │
│ + projections    │   │ projections  │
└──────────────────┘   └──────────────┘
```

Ports (Rust traits) keep domain semantics independent of backends:

| Port | Responsibility | V1 implementation |
| --- | --- | --- |
| `Store` | append / get / list-heads over immutable objects | SQLite (canonical) |
| `Index` | rebuild / search derived state | SQLite + FTS5 tables (disposable cache) |
| `WorkSourceAdapter` | explicit external snapshot intake | optional; Beads compatibility first |
| `BackupAdapter` | store/retrieve verified recovery snapshots | optional |
| `PortableStoreAdapter` | sequential publish/handoff/restore under remote-head CAS | optional V1 |
| `Sync` | concurrent fetch / push / verify between active stores | dormant until later |
| `PublicationAdapter` | publish a frozen report/work projection under a receipt | side-effect-free dummy |
| `Redactor` | pre-write DLP / secret scanning | visibly labeled no-op |
| `Signer` | optional cryptographic attestation | not shipped in V1 |

`domain.rs` owns substrate-neutral control records and valid state
representations. `control.rs` owns deterministic turn, turn-begin,
turn-checkpoint, and action-begin decisions. `storage.rs` atomically derives
work/run lifecycle, membership, context, and named feed heads; persists sessions and
short-lived grants; consumes grants at begin; and emits canonical checkpoint
events. `host.rs` is the thin JSON-lines host transport. The earlier shadow
observation path remains available for replay evidence. Scoped leases, action
grant persistence, and finalization projections remain later phases. Front
ends translate requests; they do not decide eligibility.

## Object model

Everything canonical is an **immutable, content-addressed object**: memory
versions, events (approve, retract, verify, tombstone, resolve), edges, and
evidence. Objects serialize as RFC 8785 (JCS) canonical JSON; an object's id
is the SHA-256 of its canonical bytes (hash field excluded), verified at read
time. Readers accept the exact supported schema; policy changes append new
objects and never rewrite an existing object.

A **memory** is a stable id plus an append-only chain of versions with parent
links. Multiple unsuperseded heads mean *contested*. Status (`proposed`,
`active`, `contested`, `stale`, `expired`, `retracted`, `tombstoned`) is
derived from the object graph, never stored as a mutable field. See
[typed memory model](features/typed-memory-model.md).

A **work item** is a stable local planning identity in a parent forest and
completion-dependency DAG. A **root execution** aggregates parallel child
runs, contributions, and required completion seals. A **work run** is one
live execution generation for one item with one executor/claim, resource
leases, evidence, and completion state. Root-scoped shared memory belongs to
the stable root work item, not either execution record. Assignment plans
future responsibility, a claim schedules live execution, and a resource
lease authorizes mutation; none is a substitute for another. Post-completion
report assembly uses a separate fenced assembly claim. See
[local work system](features/local-work-system.md).

## Storage

V1's canonical store is a **local SQLite database**: object rows keyed by
content hash, written transactionally, append-only by core-enforced contract.
Exact-current durable tables include the active heads, status, authority,
ordering, and idempotency projections consumed by runtime writes. Damage to
those tables requires restoring a verified current backup. Declared indexes,
triggers, and FTS5 content are separately rebuildable from verified durable rows
with `engram doctor --repair-projections`; ordinary open never repairs them.
`engram doctor` verifies hashes, graph references, projection bindings, and
configured durability freshness. SQLite is canonical in `local` mode.
Optional recovery snapshots produce `local_backed_up`; sequential
cross-machine handoff produces `portable`; a later concurrent `Sync` backend
produces `synchronized`. These are honest durability claims, not runtime
requirements.
See [SQLite store](features/sqlite-store.md).

The database is selected by stable project id rather than the current
checkout path. A tracked `.engram-project` file is resolved below
`ENGRAM_HOME` using an opaque hash. WAL mode, bounded busy waits, short
transactions, and atomic compare-and-swap operations support multiple
processes. Project, root-work, and run-execution feeds each allocate a dense
typed `FeedPosition` with their event transaction. The work protocol stages
and acknowledges an exact dense project-feed cursor. Host-control delivery
uses the task-local cursor and stamps focused project/root/run heads into the
context packet; begin rejects a changed focus or head vector. A separate
dense `DeliveryPosition` with exact multi-feed source ranges remains the
broader target. Global SQLite row ids are not cursors.
The shipped host-control alpha adds mutable session and turn-grant projections
plus canonical checkpoint events. The target extends these with delivery,
scoped lease, action, and report-barrier projections. Live grants and
high-volume decisions use bounded noncanonical operational storage; only
behaviorally relevant transitions enter the peer delta feed.

### Portable and synchronized backends

V1 portable mode publishes a canonical human-readable projection on a cadence
and at clean handoff. Clean release freezes old-host mutation; acquire/restore
requires an empty or exact-head destination, advances a writer epoch under
parent-head compare-and-swap, and enables the next host only after activation.
Portable startup/resume and a bounded cadence validate that remote epoch;
divergence refuses and forced crash takeover has an explicit detection window,
not a distributed-lock claim. Restore never activates live claims, resource
leases, grants, delivery state, or agent-private scratch. Executable shared
state is transitively closed; safe stubs/placeholders preserve excluded
provenance and feed positions without rewriting canonical bytes.
For personal/private Git the recommended transport is a dedicated plumbing
ref—not a branch or working-tree file. An access-controlled internal object
store is the organization-scale substrate because shared code-repo readership
does not imply access to in-flight work.

Concurrent cross-host sync—set-union objects, per-origin ordering or a trusted
sequencer, concurrent heads surfacing as contested, and tombstones preventing
resurrection—is **deferred, not rejected**. Canonical bytes keep transfer
simple, but authority and ordering semantics remain distinct. Sensitive
values never enter any shared history—vault references only.

## Data flow

1. **Open and select work**: a user/model creates local work or explicitly
   imports a source snapshot. Engram derives a bounded ready view; the model
   or host focuses one item without requiring an external reference.
2. **Bind and synchronize**: the host binds a durable session to the work run, receives an
   exact context delivery, acknowledges contiguous packet/delta pages, and
   reaches the relevant root/run feed heads under the current control policy.
3. **Admit**: before each prompt, the host asks the deterministic evaluator
   for a short-lived turn grant that normally inlines required context and
   peer deltas. A defer carries retry/wake conditions; a refusal carries typed
   recovery directives.
   Before a declared material capability, an action-gated host obtains and
   begins a single-use action grant, then records the outcome. See
   [behavioral control plane](features/behavioral-control-plane.md).
4. **Write**: an assertion passes the `Redactor` port, gets attributed
   (asserted runtime context + assurance level), and either activates or
   lands as `proposed` per the write-policy matrix. See
   [write policy & review](features/write-policy-and-review.md).
5. **Read**: session start (or explicit request) builds a **context packet**
   — pinned constraints (complete or fail-closed), a titles-only index, an
   omission manifest, a packet hash for reproducibility, and named dense feed
   positions. Later turns request only peer deltas after those positions. See
   [context packets](features/context-packets.md).
6. **Coordinate and complete**: each session claims its own child `WorkRun`,
   separately claims resource leases for mutation, appends decisions/evidence
   to root-shared memory, checkpoints, and explicitly hands off. A
   `RootExecution` consumes required child seals, explicit grant-backed
   waivers for disposed required children, and contributions. Host
   mailboxes may wake peers, but Engram's ordered work/run feeds remain
   authoritative.
7. **Optionally publish**: the root completion seal satisfies the execution
   barrier; a separate fenced `ReportAssemblyClaim` authorizes deterministic
   assembly/polishing and freezes the report at `report_ready`. An explicit
   publication intent then binds it to a target and idempotency key. A
   `PublicationAdapter` returns the receipt. See
   [local tasks & reports](features/local-tasks-and-reports.md) and
   [tracker adapter](features/tracker-adapter.md).

## Interfaces

The CLI (`engram …`) and agent-facing MCP expose the six-operation ambient
work protocol, memory, diagnostics, and coordination requests. A separate
host-private API handles binding, turn
evaluation/begin, delivery acknowledgement, action
authorization/begin/completion, checkpoint, heartbeat, and exit. The host owns prompt/tool mediation and
notifications; Engram owns durable protocol state and decisions. See
[CLI & MCP](features/cli-and-mcp.md).

The reusable Host Enforcement SDK implements that private lifecycle once and
is embedded by the TermAl adapter, generic CLI wrapper, native runtime
adapters, and custom-agent library. Adapters declare only the prompt/tool
coverage they actually mediate.

The hot control path targets one long-lived host-local `engram serve` process
per project store with thin hook clients and short-lived cached grants. This
avoids process/SQLite startup for every mediated tool. The required mediation
map and latency/degraded-mode diagnostics are surfaced through `engram doctor`.

The currently shipped MCP slice is advisory. The separate host-private
transport process-tests restart-safe turn grant/begin/checkpoint behavior for
`observe`, `communicate`, and local-mutation turns under scoped exclusive
execution leases. Grants bind the current lease fence and begin rechecks it,
but a deployment may describe itself as `turn_gated` only when its runtime
makes that channel mandatory before every prompt. No shipped deployment may
claim `action_gated` yet.

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
