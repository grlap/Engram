# Architecture

Engram is a Rust workspace built around one core library. The target V1
architecture keeps CLI, agent-facing MCP, and the host-private work-session
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
│ + observability  │       └──────┬──────┘  └──────┬──────┘
└───────┬──────────┘              └────────┬─────────┘
        │ work session                     │ work / memory requests
        ▼                                  ▼
┌────────────────────────────────────────────────────────┐
│ core: work graph · observations · memory · evidence    │
└──────────┬──────────────────┬──────────────────┬───────┘
           │ Store            │ Index            │ optional adapters
           ▼                  ▼                  ▼
┌──────────────────┐   ┌──────────────┐   backup/task exchange/publication
│ SQLite canonical │   │ FTS5 + work  │
│ + projections    │   │ projections  │
└──────────────────┘   └──────────────┘
```

Port contracts keep domain semantics independent of backends. These describe
the target boundaries, not a catalog of shipped Rust traits; only `Redactor`
is currently a trait. See the [source-tree inventory](shipped.md) for shipped
capabilities.

| Port | Responsibility | V1 implementation |
| --- | --- | --- |
| `Store` | append / get / list-heads over immutable objects | SQLite (canonical) |
| `Index` | rebuild / search derived state | SQLite + FTS5 tables (disposable cache) |
| `WorkSourceAdapter` | explicit external snapshot intake | optional; Beads snapshot import first, other trackers by immutable snapshot |
| `BackupAdapter` | store, confirm, list and return verified backup copies | optional; designed, not built |
| Task-text sync | typed planning directory export/import; later Git exchange and agent merge | explicit directory profile shipped; Git exchange deferred |
| Cross-host coordination | global claims, execution ordering and shared authority | deferred |
| `PublicationAdapter` | publish a frozen report/work projection under a receipt | deferred; no adapter shipped |
| `Redactor` | pre-write DLP / secret scanning | visibly labeled no-op |
| `Signer` | optional cryptographic attestation | not shipped in V1 |

`domain.rs` owns substrate-neutral records and valid state representations.
`storage/` atomically derives work/run lifecycle, membership, context, and
named feed heads, and records host observations, named-root events and
registered verification as canonical work records. `host.rs` is the thin
JSON-lines transport of the host's work session (see
[Execution Observability](features/execution-observability.md)).
Report-assembly projections remain a later phase. Front ends translate
requests; they do not decide eligibility.

## Object model

Everything canonical is an **immutable object under a minted id**: memory
versions, events (approve, retract, verify, tombstone, resolve), edges, and
evidence. Objects serialize as RFC 8785 (JCS) canonical JSON; an object's id
is a random UUID minted when it is stored, never derived from its bytes, and
links hold that id. A SHA-256 over canonical bytes is only a content
fingerprint for comparing content. Readers accept the exact supported schema;
policy changes append new objects and never rewrite an existing object.

A **memory** is a stable id plus an append-only chain of versions with parent
links. Status (`proposed`, `active`, `stale`, `expired`, `retracted`,
`tombstoned`) is derived from the object graph, never stored as a mutable
field. See
[typed memory model](features/typed-memory-model.md).

A **work item** is a stable local planning identity in a parent forest and
completion-dependency DAG. A **root execution** aggregates parallel child
runs, contributions, and required completion seals. A **work run** is one
live execution generation for one item with one executor/claim, evidence,
and completion state. Root-scoped shared memory belongs to
the stable root work item, not either execution record. Assignment plans
future responsibility and a claim schedules live execution; neither grants
filesystem or external-action authority. Resource leases have been removed.
Post-completion report assembly uses a separate fenced assembly claim. See
[local work system](features/local-work-system.md).

## Storage

V1's canonical store is a **local SQLite database**: object rows keyed by a
minted id, written transactionally, append-only by core-enforced contract.
Exact-current durable tables include the active heads, status, authority,
ordering, and idempotency projections consumed by runtime writes. Damage to
those tables requires restoring a verified current backup. Declared indexes,
triggers, FTS5 content, and derived project-memory and work tables are separately
rebuildable from verified durable rows with `engram doctor --repair-projections`;
ordinary open never repairs them. Disposable project-memory advertisements are
recreated empty. See the [projection inventory](features/sqlite-store.md#rebuildable-and-durable-projections).
`engram doctor` checks that every stored record decodes and agrees with the
projections built from it, along with graph references, projection bindings, and
configured durability freshness. It does not re-derive a record's id from its
bytes: SQLite guards the bytes on disk. SQLite is canonical in `local` mode.
A verified restore-only copy at a configured target produces
`local_backed_up` ([off-host backup](features/off-host-backup.md)). Optional
Git task sync exchanges task projections between independently writable local
stores; it is not full-store recovery or global execution coordination.
See [SQLite store](features/sqlite-store.md).

The database is selected by stable project id rather than the current
checkout path. A tracked `.engram-project` file is resolved below
`ENGRAM_HOME` using an opaque hash. WAL mode, bounded busy waits, short
transactions, and atomic compare-and-swap operations support multiple
processes. Project, root-work, and run-execution feeds each allocate a dense
typed `FeedPosition` with their event transaction. The work protocol stages
and acknowledges an exact dense project-feed cursor; that is the only context
delivery. Global SQLite row ids are not cursors. Work-session operation
results use noncanonical operational storage. The canonical records of the
removed turn gating, such as checkpoint events, remain readable as stored; its
operational tables are retired by the store migration (see
[records from earlier builds](features/execution-observability.md#records-from-earlier-builds)).

### Optional Git task sync

The selected [task-sync design](features/git-task-sync.md) exports current task
documents under tracked `.engram/` on the code branch. The main checkout owns
export/import and normal merge; linked worktrees use sparse-checkout to omit
these files while sharing the same local SQLite store. Git may materialize
excluded files for conflicts, which must be preserved/resolved or the operation
aborted; no task import runs in a linked worktree. Local project-feed progress
selects outgoing task changes; Git commit/tree differences select incoming
changes. These are local, separate progress markers, never a global feed
sequence.

An external agent merges divergent task text alongside code. Engram validates
the final graph and atomically imports it as attributed new versions/events;
it never calls a model or rewrites immutable stored records. Shared note bodies
are opt-in; modifying one creates a new attributed note citing the original.
Claims, sessions and private scratch remain local. Offline machines
can both write and may duplicate execution; task merge does not promise
global exclusion.

Sync is optional, explicit and has no background networking. Disabling it
leaves local work writable. The prior sequential writer-epoch handoff and
dedicated-ref proposal is superseded for this workflow. Full-store backup and
migration remain separate capabilities. The explicit
[task planning directory profile](features/task-planning-text.md) is shipped;
Git enablement, exchange and reconciliation remain deferred.

## Data flow

1. **Open and select work**: a user/model creates local work or explicitly
   imports a source snapshot. Engram derives a bounded ready view; the model
   or host focuses one item without requiring an external reference.
2. **Claim**: a session claims the work run. The agent reads its work
   context and peer changes through `next`.
3. **Observe**: a host that runs Execution Observability records what it saw
   run, how the source changed and which registered checks passed, against
   that claim. Nothing admits or refuses a model turn (see
   [Execution Observability](features/execution-observability.md)).
4. **Write**: an assertion passes the `Redactor` port, gets attributed
   (asserted runtime context + assurance level), and either activates or
   lands as `proposed` per the write-policy matrix. See
   [write policy & review](features/write-policy-and-review.md).
5. **Read**: `next --peek` gives the agent its work context, and `next`
   stages and confirms peer changes. Project notes are discovered and read
   explicitly through `memories`. Context-packet construction is removed;
   [serialized history](features/context-packets.md) remains readable.
6. **Coordinate and complete**: each session claims its own child `WorkRun`,
   appends decisions/evidence
   to root-shared memory, checkpoints, and explicitly hands off. A
   `RootExecution` consumes required child seals, explicit reason-attributed
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
work protocol, memory, diagnostics, and coordination requests. The host-private
work-session channel is defined in [spec §8.3](spec.md#83-host-work-session-channel);
its shipped operation inventory is in
[CLI & MCP](features/cli-and-mcp.md#host-private-work-session-channel).
The host owns its tools, processes and notifications; Engram owns durable
protocol state. See [CLI & MCP](features/cli-and-mcp.md).

The MCP work tools are agent-facing and grant no authority. The work session
records host observations and registered verification; it never admits or
refuses a model turn.

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
