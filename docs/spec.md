# Engram — Specification

**Draft 0.9 · 2026-10-09 · Local work and optional Git task sync**
Authors: Fable::AgentMemory (Claude) · Codex::AgentMemory (Codex) — at Greg's
request. Decision provenance in [Appendix A](#appendix-a--decision-log).

This is the normative design document. The docs under
[`docs/features/`](features/README.md) explain; where they diverge, this
document wins.

---

## 1. Purpose & principles

Engram gives humans and coding agents a first-class local work system backed
by memory with two deliberate lifecycle layers: **local working memory** while work is underway — constraints never
silently dropped, decisions that keep their provenance, findings retrievable
when relevant — and a **durable final report**, assembled and polished at task
completion and optionally published through an external adapter.

It is a new standalone project for use in a work environment, with no
dependency on TermAl, Beads, or any one agent runtime. V1 is **local-first,
not single-session**: multiple concurrent sessions and worktrees share the
active host's project store. Coordination authority lives on that host;
optional Git task sync exchanges task text between independently writable
hosts, with an external agent resolving divergent changes and no transfer of
live authority. Work may originate locally
or from an explicit external snapshot. Publication is a separate optional
boundary reached only through explicit authorization (§9.5), never by
mirroring Engram's local event stream.

The V1 product definition is deliberately narrow: **a host-local work graph
and Execution Observability for multiple agent sessions, backed
by typed execution memory and producing context, controlled handoffs,
evidence-gated completion, and optional frozen reports — not a general
knowledge graph or process scheduler.**

### 1.1 Goals

- A first-class host-local work graph: roots, bounded decomposition,
  prerequisites, derived readiness, assignment, claims, evidence, and
  acceptance-gated completion without an external tracker dependency.
- Local working memory that coordinates agents and outlives sessions: task
  state, decisions, evidence, constraints, and intermediate findings while
  work is underway.
- Same-host multi-session coordination with atomic ownership, an ordered
  change feed, explicit handoffs, and a finalization barrier.
- Optional explicit Git push/pull of task text on the data branch, local
  offline writes on every machine, and agent-mediated conflict resolution
  without live-authority transfer.
- Execution Observability: the host records execution, source and test
  observations and registered verification against the claim they belong
  to, without admitting or refusing a model turn.
- An optional polished final report per completed root, published only under a
  separately authorized durable receipt.
- Bounded, predictable context cost: memory never crowds out the work it is
  meant to inform.
- Audit-grade provenance: every assertion is attributed, immutable, and
  reproducible.
- Clean optional ports to external systems — Beads migration first — with the core
  containing no vendor-shaped types, and team/service backends addable later
  without changing semantics.
- One capture action producing work/run deltas, handoff material, and report
  inputs instead of making an agent repeat the same status in several tools.

### 1.2 Non-goals

- Not a **concurrent** cross-host organizational planning service in V1.
  Engram is each host's local work source of truth. Optional task-text sync
  permits divergent edits and explicit reconciliation; it does not coordinate
  cross-host claims or prevent duplicate execution. External storage is never
  a precondition for local authority (§3). Wider organization-wide commitments may still live in external
  systems and enter only as explicit snapshots (§9.1).
- Not a transcript archive. Raw session logs are not persisted by default
  (§7).
- Not a secrets store. Sensitive values are held as vault references, never
  as remembered plaintext (§7).
- Not a RAG framework over arbitrary documents. Engram stores curated,
  attributed claims, not crawled corpora.
- Not an agent process supervisor. Engram derives ready work and deterministic
  priority ordering; the host chooses models, prompts, and process lifecycle.
  Engram never starts, pauses, wakes, or stops a session (§2.7, §8).

### 1.3 Design principles

- **Typed, not a bag of strings.** A hard constraint, a design decision, and
  a session anecdote have different delivery requirements; the model encodes
  that (§2.2).
- **Append-only truth.** Records are immutable and named by a minted id; state
  is derived. Nothing is edited in place, so history and audit come for free
  (§2.4, §3).
- **Budgeted delivery.** Injection operates under hard byte budgets with
  visible omission; the constraint tier fails closed rather than truncating
  silently (§4).
- **Local while working, explicit at every external boundary.** Local work and
  memory need no external system. Intake is an immutable snapshot; publication
  sends a frozen payload through an idempotent adapter under a receipt (§9.5).
- **One write, many views.** The local work graph and execution memory produce
  ready views, deltas, handoffs, evidence, and final reports. External adapters
  receive projections; they are not a second live ledger (§8, §9).
- **Durability is explicit, not implicit.** A host-local SQLite store is a
  valid local source of truth. Engram reports whether it is `local`,
  `local_backed_up`, or configured for optional Git task exchange. Sync status
  reports exported/imported commits and pending changes separately; a task
  projection is not a full-store backup or a global coordination guarantee (§3).
- **Trust follows origin and authority.** Who asserted something, and how
  binding it claims to be, determine whether it activates immediately or
  awaits approval (§5).
- **Contradiction is a state, not a merge strategy.** Conflicting claims
  coexist visibly as *contested* until a human-attributable resolution
  supersedes them — never last-writer-wins (§6.3).
- **One core, many faces.** CLI, MCP server, and future service front the
  same core API for work and project memory, so delivery semantics cannot
  drift (§8).
- **Control requires a reference monitor.** An agent-visible tool is
  advisory. Engram controls behavior only to the extent that a host mediates
  turns and declared material capabilities through its decisions (§2.7,
  §8.3).

## 2. Data model

### 2.1 Identity and versions

A **memory** is a stable identity (`mem-<hash>`, collision-resistant for
concurrent writers) plus an append-only chain of immutable **versions**. A
version has a minted id (a random UUID, independent of its bytes) and names
its parent version(s). Asserting a change creates a
new version; it never mutates history. A memory with multiple unsuperseded
head versions is *contested* (§6.3).

### 2.2 Classification axes

Classification is three orthogonal axes, not one enum. Historical capture
derived stored delivery defaults from kind and authority, with per-memory
overrides. Those labels remain readable, but do not guarantee that a memory
is in an agent's context: current delivery is bounded, and project memories
require explicit reads (§4).

| Axis | Values | Meaning |
| --- | --- | --- |
| `kind` | `constraint` · `decision` · `convention` · `fact` · `preference` · `episode` | What species of claim this is. `decision` is first-class: valid until superseded, provenance-heavy. |
| `authority` | `hard` · `firm` · `soft` | How binding the claim is. Drives write policy (§5) and delivery defaults. |
| `delivery` | `pinned` · `index` · `on_demand` · `suppressed` | Stored delivery classification; it does not cause automatic injection (§4). |

Default delivery by kind × authority:

| Default delivery | hard | firm | soft |
| --- | --- | --- | --- |
| `constraint` | pinned | pinned | index |
| `decision` | pinned | index | index |
| `convention` / `fact` / `preference` | index | index | index |
| `episode` | — | on_demand | on_demand |

### 2.3 Scope and working-memory visibility

The two lifecycle layers contain three visibility rings. **Agent scope** is
private scratch: hypotheses, incomplete reasoning, and preferences that never
enter peer context. **Task scope** is the default working scope: decisions,
constraints, findings, evidence, status, and handoffs visible to every task
participant. **Project scope** holds reviewed knowledge that should outlive
a single task, plus explicitly attributed active Episodes/notes admitted by
the existing episode exception. The published ring is the frozen report,
not another memory scope.

The shipped agent surface recovers work context through `next` and reads
project memory explicitly through `memories`. Historical task and private
records retain their stored scope boundaries; there is no generic task-memory
capture or search surface. Delivery classifications in stored records do not
promise automatic pinned injection (§4).

V1 execution authority is host-local, with a stable project id shared across
that host's sessions and worktrees. Optional Git task sync transfers shared
task state, never local execution authority. Independently writable machines
may diverge; cross-host claim exclusion and `org`/`global` coordination remain
separate deferred capabilities (§3.2–3.3, §12).

### 2.4 Version schema

```
Version {
  version_id      // minted random UUID; never derived from the bytes
  memory_id       // stable identity: mem-<hash>
  project_key?    // safe permanent key for the constrained project-episode surface
  parents[]       // prior version ids; >1 = conflict resolution (§6.3)

  kind, authority, delivery      // §2.2 (delivery may be "derived")
  scope                          // §2.3

  title           // one-line summary for memory discovery (§4)
  body            // full prose; loaded on demand
  structured_value?              // optional machine-readable payload
  tags[]

  provenance {
    actor_id, actor_kind         // human | agent | system
    run_id?, session_id?         // originating agent run, if any
    chain[]                      // asserted-by / relayed-by / derived-from
    reason
  }
  evidence[]                     // refs to evidence objects (§3.1)
  refs[]                         // external: ticket refs, URLs, repo@rev
  source_snapshot?               // fingerprint of mutable source (§9.3)

  confidence      // 0..1
  sensitivity     // public | internal | restricted | secret-ref
  valid_from?, valid_until?      // factual validity window
  review_by?      // re-affirmation deadline (≠ validity, §6.1)
  last_verified?
  created_at
}
```

Alongside versions, the store holds append-only **event objects** —
`approve`, `retract`, `verify`, `tombstone`, `resolve` — each attributed like
a version. Status is *derived* from the object graph:

| Derived status | Condition |
| --- | --- |
| `proposed` | Asserted but awaiting required approval (§5) — retained for attributed inspection. |
| `active` | Approved (or auto-activated) head version. |
| `contested` | Multiple unsuperseded heads, or an unresolved `contradicts` edge. |
| `stale` | `review_by` passed — delivered with a warning; needs re-affirmation. |
| `expired` | `valid_until` passed — excluded unless explicitly requested. |
| `retracted` | Withdrawn by an attributed retraction event. |
| `tombstoned` | Forgotten: excluded everywhere, object retained so sync cannot resurrect it (§6.5). |

### 2.5 Edges

Typed, behavior-bearing links between memories: `supersedes` (lifecycle),
`contradicts` (marks both sides contested until resolved), `derived_from`
(compaction and distillation provenance), `relates_to` (navigation only).
Edges are objects too — attributed and immutable.

### 2.6 Local work graph & execution

Engram is the system of record for a host-local graph of `WorkItem`s. A work
item has a stable collision-resistant id plus a short display ref, project and
root ids, optional parent, kind, title, intended outcome, acceptance criteria,
integer priority, labels, optional assignment and deferral, origin/provenance,
stable project binding, and immutable revision history. Local-work exception
reasons are attributed audit facts rather than permission-bearing authority.
No external reference is required.

Parent/child decomposition is a forest. Explicit prerequisite edges plus the
implicit completion edge `parent requires required-child` form one
completion-dependency graph. Every hierarchy, required/optional, and
prerequisite mutation rejects cycles in that union transactionally; optional
children add no implicit edge. `ready` is derived—not stored—from admission,
deferral, prerequisites, required parent constraints, and live claim state.
Project policy bounds depth, fanout, and open descendants. Assignment records
future planning intent. A **work claim** is fenced live responsibility.
Neither authorizes resource mutation.

A `RootExecution` is one aggregate execution generation for a root. It owns
the contributor roster, current child-run membership, required child
`CompletionSeal` ids, root decisions/waivers, and the root completion
barrier. The shipped
[work-graph snapshot](features/work-graph-snapshot.md) adds the one
completion proof that is not a seal: an item loaded completed carries an
inert `RestoredRecord`, a later parent seal lists it under
`restored_child_completions` beside required seals and waivers, and report
assembly refuses any root whose completion transitively rests on one.
A `WorkRun` is one execution generation for one work item. V1 gives
it exactly one ordinary executor and at most one live `WorkClaim`; parallel
sessions execute distinct child runs under the same root execution. A run
owns its ordered execution feed, executor checkpoint, evidence, claim,
and completion state. Root members without that run's claim
may inspect permitted root memory and communicate but cannot mutate or
complete the run. V1 permits one active run per item. Reopen creates a new run
generation so old claims, grants, and evidence cannot revive. The shared
working-memory ring belongs to the **root work item**, not `RootExecution` or
`WorkRun`, and survives every generation; memories reference narrower
work/run ids for filtering and provenance. Reopen preserves memory and resets
execution state. Memory scope does not recursively nest with the work graph.

The target durable work lifecycle is `proposed → open → completion_pending →
completed | cancelled | superseded`; an attributed completion abort returns
to `open`. The shipped alpha seals `open → completed` directly only when its
linked action-outcome drains are empty; the historical resource-lease drain
field stays empty. It refuses a nonempty action-outcome drain until controlled
`completion_pending` ships. Availability
(`ready`, `claimed`, `active`, `blocked`, `deferred`,
`waiting`) is a derived projection. Completion binds the accepted work
revision, run generation, claim fence, checkpoint position, acceptance
results, and evidence ids. Required children and prerequisites must be
complete or an explicit reason-attributed waiver must account for them. Root
completion additionally binds required child seals or reason-attributed waivers for
disposed required children, plus the `RootExecution`
roster/contributions. Acceptance records whether evidence was independently
verified or self-attested.

Every claimed mutation revalidates the full ancestor chain and the run's
membership in the currently active `RootExecution`. Root completion refuses
while any descendant has a live claim or handoff offer. Optional unfinished
children may be named in the seal, but the closed ancestor and completed root
execution fence their old runs immediately. They must be disposed leaf-first
before root reopen; the new root generation never silently inherits an old
child run.

**Claims schedule execution, not filesystem mutation.** A work claim protects
the work/run lifecycle with a holder, expiry, and fencing epoch. Handoff and
recovery advance the fence so stale holders cannot mutate the run.
Host/user authority governs filesystem and external effects. Resource leases,
their acquisition/release operations, and lease-bound grants have been removed.
Supplied resource intents are normalized scope descriptions, not locks; an
empty list remains valid. Each behaviorally relevant work transition appends
an immutable event.

**Completion is the execution barrier.** In the target controlled path, `work_complete` enters
`completion_pending`, denies new ordinary mutation, drains in-flight actions,
terminalizes the run claim and waits for its executor checkpoint or an authorized decision. A
root barrier additionally freezes the expected `RootExecution` contributor
roster and waits for required child seals or explicit disposed-child waivers
(the shipped work-graph snapshot adds restored child completions as the
third admissible proof), plus contributions or attributed, audited waivers by
a project-bound session.
`completion_seal` captures one dense run-feed cut
plus the accepted work revision, run/claim fences, action reconciliation,
acceptance results, and evidence ids; a root seal also binds those child
seals and aggregate contributions. The seal makes the work `completed`; an
attributed abort before it returns to `open`. Reopen preserves root-work
memory but creates a clean run generation.

The `gate` word is an audit wrapper over existing machinery: one pass/fail
`WorkEvidence` entry on held open work, or an attributed late-finding entry on
completed work, with a gate name, bounded failure-label list, and optional
reference. `--work-ref REF` selects an exact item without relying on ambient
focus. Bare `gate NAME` records a pass; a failure carries at least one label,
using the check command or check name when no test id exists. Hidden
idempotency replays a consecutive identical result after a lost response only
within the same open claim generation or the same immutable completed-seal
basis. Release, handoff, recovery, or different asserted attribution creates a
fresh observation even when the result is unchanged; the same result after a
different state is also fresh — nothing else. The gate payload is a structural
field on `WorkEvidence`, not a
classification inferred from generic evidence prose. It adds no canonical kind,
no obligation, no completion barrier, and no waiver. Before completion,
[Required Quality Gates](../AGENTS.md#required-quality-gates) governs failure
classification: small in-scope corrections stay on the held item with diagnosis,
fix and verification; required children are for separate ownership,
independently scoped work or real dependencies. Pre-existing defects outside
the changed scope get independent roots with provenance. After completion, the
agent records the failed gate on the closed item and files a separate follow-up
with `engram work add "Follow up the late gate failure" --kind bug`; a completed
item is never reopened merely to attach the evidence or made the parent of new
work. The feature brief defines the input bounds; the domain/storage boundary
owns and revalidates their normalized form.

Optional report state is separate from work completion:

```
not_requested → finalization_pending → report_ready → publishing → published
                       └── abort → not_requested            └── failure → report_ready
```

Optional report finalization consumes the completed run's `CompletionSeal`;
it never quiesces or drains execution a second time. Participant completion
contributions and attributed, audited waivers are already bound to the cut. The
host creates a `ReportAssembly` anchored to the root seal and acquires a
fenced `ReportAssemblyClaim` for its designated assembler. This planned
post-completion claim is not a `WorkClaim`, requires no live execution claim,
and permits no ordinary workspace mutation. It binds the seal and assembly
generation for deterministic assembly and one polishing pass. The host
protocol has no finalizer turn purpose or phase. Handoff or recovery bumps
that fence. Reaching `report_ready` terminalizes the claim and freezes
immutable bytes and a report hash. A publication intent and idempotency key
exist only when a target is requested; retry sends the same bytes, and
revision creates a superseding report.

### 2.7 Execution Observability

Execution Observability is the host feature that records what happens while
agents work, switched on or off per project in the host. Engram stores what
the host records and links it to the work it belongs to; it never admits,
delays or refuses a model turn, and nothing it stores grants filesystem, tool
or external-action authority. With the feature off, ordinary work words,
claims, notes, gates and completion behave exactly as with it on.

The host's **work session** (`engram control --work-session`) carries:

- **host observations**: execution, source and test facts the host saw, as
  `execution_observe` records bound to the exact claim epoch observed. They
  are attributed history: observed checks are facts without test credit, and
  an observed source change may open a source-change obligation (§2.6);
- **named source roots**: a claim's selected workspace, as `named_root_bind`
  events, read back with `named_root_read` and `named_root_sighting_read`;
- **registered verification**: terminal results of a durably registered host
  run, as `verification_record`. Registration is asserted by the host, not
  proven;
- **remote verification**: a test run on another host, delivered as its
  launcher's completion facts and a stated destination, as
  `remote_verification_record`, also asserted by the host.

**Assignment** is future intent on a work item; a fenced **claim** schedules
live execution of one run. Neither grants file or external authority.
**Acceptance evaluation** is temporarily off until Execution Observability is
implemented (see [acceptance evaluation](features/acceptance-evaluation.md)).

The project `ControlPolicy` selects the obligation rule set and the
acceptance-evaluation policy. Initialization installs and selects a versioned
built-in policy atomically in a new store. An existing store must match the
complete current schema and policy chain exactly; missing, different-build,
corrupt, or ambiguous state fails ordinary store open for every surface before
DDL. `doctor --recover-policy` is a read-only diagnostics-only path: it
exposes no service or mutation handle and never selects or rewrites policy. A
new immutable version records a canonical `project_policy_admin`-shaped
host/operator authority decision, recorded honestly as asserted context, not
authenticated identity. Selecting the new policy and advancing the project
epoch is one transaction. A host/operator-only activation may append a
validated `set_obligation_rule_set` successor under an epoch/id
compare-and-swap. The operator CLI accepts at most 64 KiB of strict nested V1
JSON inline or through `@file`; rollback re-supplies the desired typed JSON
and never activates an id alone. No MCP operation can administer it. Unknown
schemas, unknown fields or triggers, duplicate rule identities, and missing
selected objects fail closed. Host/operator project-policy mutations use a
store-scoped durable idempotency key; the normalized intent and exact receipt
commit atomically with activation, and same-key different-intent reuse is a
conflict. Policy epochs, not wall-clock timestamps, order immutable policy
history.

The full protocol is specified in the
[Execution Observability brief](features/execution-observability.md). Its
[history note](features/execution-observability.md#records-from-earlier-builds)
describes retained records and completed format conversions.

## 3. Storage & sync

### 3.1 V1 canonical store: local SQLite, append-only

V1's canonical store is a **local SQLite database** holding the same
immutable objects the model defines (§2.4): versions, events, edges, and
evidence as append-only rows keyed by their minted id, written
transactionally. Append-only is a semantic contract enforced by the
core, not a hope — nothing updates or deletes object rows except the purge
runbook (§6.5).

Local means one host, not one process. A stable `project_id` selects one
host-local database regardless of checkout or isolated worktree. Concurrent
CLI and MCP clients use WAL mode, a bounded busy timeout, short transactions,
and atomic compare-and-swap operations for coordination projections.
Deployments must never derive store identity solely from the current working
directory, because that would silently fork memory across worktrees.

Each project, root-work, and run-execution feed allocates its own dense
`feed_position` inside the same transaction as the event append. Delivery has
its own dense per-session sequence over emitted pages. "Contiguous" always means adjacent
positions in one named feed; a global SQLite row id may be useful internally
but is never a cursor. This identity is fixed by the work graph so
safety CAS never depends on sparse cross-feed numbering.
Delivery progress, the root completion barrier and indexes are mutable
projections; the barrier's transitions append immutable work events (§2.6).
Host observations, named-root events and registered verification are
canonical work records on the project, root and run feeds, which obligations
and completion read while the run is unfinished (§2.7). The cursor orders peer
deltas; it is not an object identity and does not cross
stores as a global sequence number.

```
engram.db
  objects      // rows keyed by minted id: versions, events, edges, evidence — write-once
  projections  // exact-current heads/status/order plus rebuildable indexes, FTS5 and derived tables
  control.*    // operational records and diagnostics, never memory
  meta         // current-build marker; refuses stores created by another build
```

Runtime heads, status, ordering, authority, and idempotency tables are durable
parts of the exact-current store and are recovered from a verified current
backup. `engram doctor --repair-projections` rebuilds declared indexes, triggers,
FTS5 content, project-memory state (`project_memory_state`), and the work
projection tables (`work_restored_records`, `work_observations`, and
`work_restored_evidence`) from verified retained state. Rebuildable task-sync
views include affected tasks (`work_project_feed_tasks`) and their per-position
coverage (`work_project_feed_coverage`), baseline members and
heads (`work_task_sync_baseline`, `work_task_sync_baseline_heads`), active holds
(`work_task_sync_reconciliation_active`), exchange operation/head links
(`work_task_sync_exchange_operations`, `work_task_sync_exchange_heads`),
producing-origin members/heads (`work_task_sync_producing_origin_members`,
`work_task_sync_producing_origin_heads`), embedded documents and identities
(`work_task_sync_document_members`, `work_task_sync_shared_identity_members`),
and graph nodes, edges and heads (`work_task_sync_graph_nodes`,
`work_task_sync_graph_edges`, `work_task_sync_graph_heads`). It recreates disposable
project-memory advertisement bookkeeping (`project_memory_advertisements`)
empty; it never recreates durable tables from `objects`.
`engram doctor` checks that stored records decode and agree, along with graph
references, projection bindings, and index freshness
(§3.1.1). `local` mode relies on SQLite
transactions. An optional verified restore-only copy at a configured target
provides `local_backed_up`: a full-store copy, or the deterministic
work-graph recovery snapshot as a second, smaller kind. Optional Git task
sync (§3.2) exchanges an editable task projection between local stores; it
does not change the durability mode into a full-store recovery claim. Semantic
merge belongs to an external agent; the core validates and imports the result.

#### 3.1.1 Canonical-bytes contract

Stored bytes and identity are separate concerns, and the contract is part of
the spec, not an implementation detail:

- Objects serialize as **RFC 8785 (JCS) canonical JSON**, UTF-8, so equal
  content has equal bytes and can be compared directly.
- An object's id is a **random UUID minted when it is stored**. The id is the
  storage key — SQLite object row key today — and the only thing an object
  link holds. It never depends on the bytes, so a record can be re-expressed
  in a new shape under the id it has,
  and no link ever needs rewriting. Ids written before this rule are 64 hex
  digits and remain valid as opaque strings.
  A Git task document (§3.2) is keyed by stable task identity, not by the
  identity of an immutable canonical version.
- A **SHA-256 over canonical bytes is a content fingerprint**: it compares
  content (idempotency intents, snapshot bodies, build identity). It is never
  an id or a link, and it is not a corruption check. SQLite guards the bytes
  on disk; `engram doctor` checks that stored rows decode and agree.
- Every executable object carries the exact supported schema version.
  State from a different build is refused rather than interpreted.
- State changes mint new objects; they never rewrite old ones.
- A whole store changes format by [plain JSON export and
  import](features/full-store-migration.md), which carries ids unchanged.

### 3.2 Optional Git task sync

**Selected design, not shipped.** Each machine keeps its writable local SQLite
store. Explicit Git push/pull exchanges human-readable task documents on the
dedicated long-lived orphan data branch. Divergence is expected: an external agent reads
the common base, local and incoming text, merges tasks independently of code, and
supplies the final state for Engram to validate and import. Engram never calls
a model. This replaces sequential portable release/acquire as the selected
personal cross-machine workflow; previous single-writer, remote-epoch,
dedicated-ref and scheduled-push proposals do not govern this task-sync mode.

Sync is optional. Without export/sync configuration, Engram does not create or
modify `.engram/` and performs no background activity. Local work needs no
remote. Enablement, target selection, export/import, push and pull are deliberate
visible actions. Disabling exchange leaves the local store usable and writable.
There is no online startup check or global writer claim. Duplicate execution
across machines is possible, and merging data cannot undo external effects.

The tracked layout is `.engram/project.json` plus one pretty-printed UTF-8 JSON
document at `.engram/tasks/<full-task-uuid>.json` per task, with stable key order,
multiline arrays, LF and deterministic bytes for unchanged shared state. Project
metadata names the project and exact interchange format, not machine/store
identity. Task identity is
stable; it is neither a content fingerprint nor a canonical version id. The
typed export profile contains task planning fields, graph links, machine-qualified
informational work-activity notices keyed by origin machine/activity and explicitly
opted-in generic/status notes under a narrow disclosure profile. Runtime
evidence and path-bearing notes are excluded by default; repository visibility
and readership are recorded at enablement. Changed note text becomes a new
attributed note citing its original, never a rewrite under the original id.
Cancelled/superseded documents carried in the folder retain their terminal
states. For a first import into an empty, unenrolled store, a missing file
cannot dispose an existing task: there is no prior exchange inventory. An
omitted isolated task cannot be detected; omitted referenced tasks refuse.
An enrolled destination instead applies the
[folder-removal rules](features/task-planning-text.md#edits-on-an-enrolled-destination):
omitting an eligible member cancels it locally and permanently retires its id
from the exchange. A partial input whose remaining graph is valid can therefore
cause irreversible cancellation; review with `--dry-run` before applying it.
Initial export includes terminal tasks. Imported
completion is attributed historical/reported outcome, not a fabricated local
completion seal or an acceptance-policy bypass. Live claims, grants, sessions,
retained prompts, delivery progress and private scratch stay local. Foreign
provenance remains machine-qualified and conveys no local host-verification
credit or session/source-root binding. This task projection is not
a full-store backup or a claim to transfer complete native execution history.

A dedicated persistent `MAIN/.engram-data/` worktree owns task/evidence export,
import and resolution on the project's orphan data branch. Code branches and
code worktrees never carry exported metadata; their complete code `source_id`
stays unchanged by exports. No linked-code sparse-checkout contract is needed.
`task-sync data-init` explicitly creates the ignored, registered and locked data
worktree without SQLite, code edits, commits or network work; private setup and
selected materialization are separate. Only `.engram/` belongs in a data commit.
The worktree survives ordinary coding-task cleanup and is retired explicitly.
All sessions/worktrees still share one host-local project database. Code branch
changes never replace that database or its progress. Hosts fetch the data ref,
use Engram's merge, then fast-forward without Git merge, rebase or forced push.
A deliberate data-branch rebind reconciles current state instead of rolling it
backward. Full historical evidence packs share that carrier, while local live
claims, delivery and execution authority remain private.

Two distinct durable local progress markers answer the transfer questions:

- **What to send:** read the named local project feed after the last exported
  position through a captured head, deduplicate affected task ids, and export
  their final documents from that same SQLite snapshot. Reconcile both
  committed-but-unimported and uncommitted Git task changes before replacing
  any document; bind/recheck the candidate against the branch and exact task
  tree/index/worktree basis. Clean Git status alone proves no import. Every exported change
  family must have proven transactional feed/index coverage. Do not infer
  changes from ids or timestamps. Advance export progress only when its output
  is in a recoverable local Git commit; failed push retries that commit.
- **What to receive:** diff the last successfully imported commit's task tree
  against the incoming committed tree. Include all intervening changes and
  merge commits. Preserve and reconcile pending local database/file edits;
  fetching alone never advances import progress. Import records the exact
  resolved commit, including any local merge commit, in the same transaction
  as applying its task changes.

Progress is scoped to the local store/project and repository/branch binding.
Retain the common task-tree merge base independently of export/import progress;
local feed positions are never global cross-machine cursors. No-op re-export
after an import must not generate feedback commits or consume unrelated
unexported local changes.

For an initial push of 1,000 tasks to an empty remote branch, capture all tasks
and the feed head in one read snapshot, write 1,000 task documents plus project
metadata, validate their graph, create one commit, record its export position,
then push. Writes after the captured head remain pending. An existing remote
task tree is fetched and reconciled first, never overwritten by force. A new
machine imports the full committed tree once; subsequent transfers use changed
task ids/paths. Initial export does not require an agent to read every task.

TermAl remote collaboration can use this exchange: an agent claims locally,
exports/pushes, and peers fetch, merge and import through their own main
checkouts. Git task sync works independently of TermAl mailbox availability
or implementation. Ordinary peer messages may coordinate this work, but
carry no task-sync protocol: a message about a commit neither triggers an
automatic import nor acknowledges task application. Publication and import
recover from their own retained intents and the local atomic ImportReceipt;
mailbox append, wake or acknowledgement receipts prove neither.

Shared work-activity notices make remote started/released/finished state
visible through Git even when notes are not exported; actual claims, fences
and grants never travel. Each activity has a stable identity and
origin-machine/actor provenance; its successive states use source-local
ordering, not cross-machine timestamps. Imported notices are scheduling
context, not local claims or fresh evidence. Concurrent/stale activity
remains visible for agent reconciliation. Renewals or unrecorded expiry
do not create activity exports.

TermAl separately owns central session/mailbox coordination: one explicitly
configured central TermAl host per coordination domain owns its mailboxes;
satellite hosts use its API over OpenSSH. This does not centralize Engram
or establish a task-sync notification route or dependency. Mailbox content
grants no Git, import or execution authority.

With Git alone, discover activity on the next explicit fetch, never by
polling. A commit mentioned by a peer must be verified against the fetched
configured branch history; ancestry alone does not prove that its changes
were applied. Route exchange to the main-checkout sync session under the
existing gate/turn serialization.

Import validates the exact supported schema, stable identities, reference
closure, graph invariants, missing-document refusal and lifecycle admission. It checks
that affected local task bases have not changed while the agent prepared the
merge; a changed basis requires reconciliation, never silent overwrite.
In the same write transaction, revalidate the combined graph (including the
parent/required-child and prerequisite union), referenced lifecycles and run
admission, or fence that complete relevant read set. Changed-row checks alone
do not prevent a concurrent A→B/B→A cycle. Apply the batch atomically and
idempotently, appending attributed
versions/events while preserving existing immutable canonical objects. Git
commit/push and SQLite import remain separate recoverable steps, not one
distributed transaction. Ordinary non-fast-forward conflicts return to merge;
force-push is not required by this design.

The detailed workflow, initial profile, explicit imported-lifecycle mapping,
recovery conditions and implementation acceptance cases are in
[Git task synchronization](features/git-task-sync.md). Reported completions can
satisfy ordinary planning prerequisites; required-child accounting names inert
restored completions separately and never fabricates a native seal or host
credit. A terminal import against an unfinished local run requires explicit
affected-run admission hold, checkpoint/drain, attributed claim release and an
audited superseded-by-import disposition; if these cannot finish safely this
attempt is refused with the candidate retained. Native seals remain immutable.
Reopen uses a new locally admitted execution generation. The new reconciliation
transition must be implemented explicitly, not assumed to exist today.
Changed work bindings reach sessions through the work protocol's context
refresh. Requests with retained or uncertain outcomes keep their original
intent and binding until the host reconciles them.
Import never fabricates host/source credit, rebinds a named root, or silently
clears a pending request. The host receives a structured import cause and remedy.
Those cases require implementation proof; this specification does not claim
that the exporter, importer, Git orchestration or worktree setup already exist.

### 3.3 Deferred: concurrent cross-host coordination

Concurrent offline editing and explicit agent-mediated task merge are in §3.2.
Globally exclusive execution, automatic semantic merge, shared control grants,
cross-host run-feed ordering and organization-scale service coordination remain
deferred. Task import does not splice two host-local dense feeds into one global
sequence or transfer a live claim. It records local import events with source
provenance. No remote head comparison can guarantee that another offline agent
has stopped executing a task.

### 3.4 Ports

Domain semantics bind to interfaces, not backends: `Store`, `Index`, optional
`BackupAdapter`, task-text export/import with Git transport, `WorkSourceAdapter`
and `PublicationAdapter` (§9.2), `Redactor` (§7), and optional `Signer` (§7).
These are architectural boundaries, not assertions that Rust traits or commands
are shipped. SQLite and recovery snapshot/restore remain current source-tree
capabilities; Git task sync is a selected design and publication adapters remain
deferred. Full-store backup and migration retain their separate contracts.
The backup mode `local_backed_up` is always reported with whether its target
is off the host by the operator's assertion or by the remote's confirmation;
`doctor` reports recorded backup evidence without implying remote contact
([off-host backup](features/off-host-backup.md)).

Local sync progress and import receipts need an explicit durable schema design.
If implementation changes the durable format, it follows full-store migration;
JSON interchange is not evidence that migration is unnecessary. Sync status
must show its configured branch, export/import progress, pending local changes,
conflicts and last observed remote state without implying continuous remote
contact or globally exclusive authority. Data-only commits must serialize with
source landings and cannot move a branch underneath an active source freeze.
A project-specific data validation/authorization policy is a prerequisite for
unattended export commits; this spec grants no gate/review exception or new
commit/push authority. Mixed code/data commits retain source delivery rules.
Tracked task bytes still participate in source content identity; HEAD also
participates in the current review freeze. No path exclusion redefines either.
Use pinned detached code roots and serialized main-checkout exchange; importing
planning changes can still stale evaluations even when those source roots do
not change. See the detailed host contract in the task-sync brief.

## 4. Memory retrieval

The work protocol's `next` operation delivers bounded work context and ordered
peer changes. Project memory has a separate explicit surface: `remember`,
`memories`, and `forget`. An unfiltered `memories` listing discovers keys;
an exact-key full read returns the current body, with attributed revisions
available on request. Listing or advertising a note does not prove its body
was read. Work search and live work-memory retrieval use the persisted focus
and scope boundaries. See [CLI & MCP](features/cli-and-mcp.md).

Generic task-scoped prose capture and task-memory search have been retired;
they never had a production CLI or MCP surface. Historical task, work, private,
and unkeyed project memories remain readable by storage, doctor, projection
repair, and graph restore. They share object kinds with live project memory;
scope and keyed project identity distinguish them, never object kind alone.

Context-packet construction and delivery are removed. No new packet is
constructed. The `ContextPacket` type still describes the historical
serialized format. See
[historical context packet format](features/context-packets.md) and the
[history note](features/execution-observability.md#records-from-earlier-builds).

## 5. Write path

Anyone — human or agent — can *assert*; whether an assertion activates
immediately or awaits approval depends on **origin and authority**. Every
promotion is its own attributed audit event. Writes pass through the
`Redactor` port before persistence (§7).

The shipped agent capture path is `engram work note` / MCP `note`. The core
derives a stable idempotency key so a lost response can be retried without
duplicating the finding; changed prose is a new intent. Project notes use the
separate `remember` / `memories` / `forget` surface. Generic task-memory
capture is retired. The general promotion matrix below remains target design;
it does not describe a shipped generic `assert` command.

| Origin | soft | firm | hard |
| --- | --- | --- | --- |
| Human, explicit | active | active | active |
| Agent, with machine-verifiable evidence | active | proposed | proposed |
| Agent, unsupported claim | proposed † | proposed | proposed |
| Session-end distillation | proposed | proposed | proposed |

† Exception: `episode` records activate directly regardless of evidence —
they are attributed observations, deliver on-demand only, and decay by
default (§6.4).

**Distillation** — an end-of-session pass that drafts candidate memories from
what happened — is a proposer, never a writer. It gives automation's capture
rate with review's quality gate, and must deduplicate against existing
memories before proposing: exact duplicates no-op; semantic near-duplicates
are proposed *linked* to the existing memory, never auto-merged. Distillation
writes only to *local* working memory; nothing crosses the organizational
boundary through it — publication happens solely via explicit task
finalization (§9.5). Raw transcripts are not retained by it (§7).

## 6. Lifecycle

### 6.1 Review vs. validity

Two clocks, deliberately separate. `review_by` is an *epistemic* deadline:
past it, the memory is `stale` — still delivered, flagged with a warning,
queued in `engram review` for re-affirmation (bump), demotion, or retirement.
`valid_until` is *factual*: past it, the memory is `expired` and excluded
unless explicitly requested. "Review overdue" does not mean "false" — the
states are distinct because the correct behavior differs.

Individual hard constraints may be configured to fail closed on overdue
review, for claims where acting on an unverified rule is worse than stopping.

### 6.2 Supersession

Changing a memory means asserting a new version that supersedes the old —
never editing in place. The chain preserves what was believed, when, and on
whose word.

### 6.3 Contradiction and contested state

Conflicts are first-class, never resolved by timestamp:

- Concurrent heads after a sync merge → the memory is **contested**; both
  heads visible for explicit inspection.
- Cross-memory conflicts get an explicit `contradicts` edge; both sides show
  as contested until resolved.
- Resolution is a new version citing *all* conflicting parents, with
  rationale recorded. `engram conflicts` lists everything awaiting
  resolution.

### 6.4 Episodic compaction

Episodes decay on a schedule: full record → one-line summary → gone. Each
compaction step is a new object with a `derived_from` edge to its source,
which is retained until retention policy deletes it. Compaction previews
before it acts (`--dry-run` is the default posture for anything destructive).

### 6.5 Forgetting vs. purging

**Forgetting is a tombstone**: excluded from every packet and search, object
retained so sync cannot resurrect it. The distinction from purging is what
makes "forget" safe to use freely.

> **Purge is not an ordinary operation.** V1 purge = logical tombstone. In
> the local SQLite store, physical erasure is *technically* feasible (delete
> + vacuum), but it still spans every backup, Git task history, and JSONL export and it breaks
> the append-only contract — so it remains an exceptional, documented runbook
> with preview and audit, not a CLI verb. Two boundaries stay effectively
> irreversible regardless: anything already *published* in a report (§9.5)
> may live on in the external target's history, and any Git task exchange (§3.2)
> or a future Git-backed team store (§3.3) retains history across clones,
> reflogs, and host backups —
> where erasure means coordinated history rewrite, force-push, and clone
> invalidation. A v2 option is envelope encryption of sensitive payloads with
> per-record keys, so crypto-shredding can render retained ciphertext
> unreadable. The practical consequence: *prevention is the real control* —
> the Redactor port (§7) matters precisely because persistence past a
> boundary cannot be reliably recalled.

## 7. Audit, security & compliance

Work deployment is the design center, so these are v1 requirements, not
add-ons:

- **Attribution on every object:** `actor_id`, `actor_kind` (human / agent /
  system), originating run or session, timestamp, reason — plus an
  **assurance level** recording how that identity was established. In V1,
  actor and authority context arrive from the proprietary runtime: a text
  instruction supplied through the tools and skills in use. That is
  *asserted* instruction/authority context, not cryptographic identity, and
  the spec says so — objects preserve source/tool/skill metadata when
  available and record assurance accordingly. No SSO/LDAP integration in V1.
  Where compliance-grade attribution ever becomes a deployment promise, a
  trusted write gateway or Signer-backed signatures (below) must ship with
  that deployment — the port alone is not attestation.
- **Immutable history:** versions, approvals, retractions, evidence, and
  tombstones are append-only objects (§3.1) — the audit log is the data
  structure, not a side channel.
- **Sensitivity labels** (`public` / `internal` / `restricted` /
  `secret-ref`) enforced at retrieval: scope and sensitivity authorization
  run before anything enters a packet.
- **Pre-write redaction:** a pluggable `Redactor` port (DLP / secret
  scanning) runs on every write and fails closed where policy demands.
  Secrets and PII are stored as vault references, never as remembered values.
  It may transform a candidate before canonical identity is minted. Export of
  an existing canonical object is pass-or-exclude, never byte rewrite under
  the old hash; sanitized derivatives receive new ids and provenance.
  No backend is selected for V1: the shipped implementation is a *visibly
  labeled no-op for development* — surfaced in `engram doctor` output,
  implying no compliance assurance whatsoever.
- **No raw transcript persistence** by default; any ephemeral retention is
  explicit, bounded, and audited.
- **Safe export defaults:** they differ by artifact. A generic JSONL export,
  the work-graph file and a Git task export exclude `restricted` records
  unless explicitly widened; a `graph` backup copy is never widened. A
  `secret-ref` label is writer-asserted: the work-graph file carries the
  labelled body as written, and no export resolves a reference into its
  secret. Two whole-store artifacts are the exception to these defaults: a
  full-store backup copy, and the file that `migration export` writes to
  move a store to a new format
  ([full store migration](features/full-store-migration.md)). Each carries
  every row, restricted bodies, agent-private scratch and host authority
  records included. A backup copy goes only to a target the operator has
  authorized for the whole store
  ([off-host backup](features/off-host-backup.md)); a migration file is kept
  where the store itself may be kept. Git task export (§3.2) applies the
  explicit disclosure profile and restricted-record exclusions in
  [Git task sync](features/git-task-sync.md); it transfers editable planning
  state, not an executable snapshot or portable writer authority. Apart from
  those two whole-store artifacts, agent-private scratch never leaves the host.
  They are how the rule that scratch and live authority stay on the active
  host is read: their rows leave it only as inert bytes of a whole-store
  artifact, in a place the operator chose for the whole store. Nothing
  reads them there as work state, the artifact grants nothing, and no
  generic export, work-graph file, Git task payload or publication carries
  them.
- **Signing is policy, not a dependency:** a `Signer` port supports signed
  objects and signed Git commits where a deployment requires cryptographic
  attestation. Baseline v1 runs without it — at asserted-identity assurance;
  deployments that promise more must deploy more.

## 8. Interfaces

One core library owns the object model, derived state, work delivery,
and control decisions. The CLI, MCP server, and host control transport are
thin faces over it; a future service is another. No interface reimplements
delivery or admission logic.

### 8.1 CLI

```
# write path
engram work note "..."       # attributed work finding
engram work remember "..." --key KEY
# generic assertions (target only; not built)
engram assert   --kind decision --authority firm --scope project:x \
                --title "..." [--body-file ...] [--ref jira:ABC-123]
engram approve <id>         engram retract <id>        engram forget <id>

# read path
engram work show <ref>      engram work search <query>
engram work memories        engram work memories KEY --full

# generic curation and sync (target only; not built)
engram review               engram conflicts           engram compact --dry-run
engram sync
engram export --jsonl       # target export; purge is a runbook, not a verb (§6.5)

# shipped store diagnostics
engram doctor              engram doctor --repair-projections

# local work and reports (§2.6, §9.5)
engram work next              engram work show <ref>       engram work search <query>
engram work propose [...]     engram work focus <ref>      engram work update [...]
engram work complete [...]    engram work handoff --to <actor>
engram work list [filters]    engram work stats            engram work preflight
engram report finalize <root-ref>            # optional barrier → polish → report_ready
engram report show <root-ref>                engram report publish <root-ref> # optional, receipted


# optional external adapters (§9)
engram import preview <adapter> <ref>        engram import apply <snapshot>
engram export preview <adapter> <work-ref>   engram export apply <intent>

# shipped save/load (features/work-graph-snapshot.md)
engram graph save [--out FILE | --stdout] [--include-restricted --reason "<why>"]
engram graph load FILE [--dry-run]
```

### 8.2 Agent-facing MCP server

The agent-facing MCP work tools are exactly `next`, `ls`, `show`, `add`, `claim`,
`update`, `gate`, `evaluate`, `note`, `done`, `search`, `handoff`, `remember`, `memories`,
and `forget`. The ordinary lifecycle work tools translate into the
six-operation work core: `work_next`, `work_focus`, `work_propose`,
`work_update`, `work_complete`, and `work_handoff`; the session binding
supplies project, actor, current work, and cursors. Typed `gate` and atomic
`note` are word-only `work_update:gate` and `work_update:note` service
suboperations, not variants exposed by the direct `work core update` surface.
They require the live holder on open work. After completion, any project-bound
session may append either as attributed, provenance-marked late evidence after
the immutable completion cut; this does not checkpoint, reopen, reseal, or add
a completion barrier, and the existing `CompletionSeal` never absorbs it.
Optional generic memory capture, import, publication, and additional
administrative queries remain outside the work-word surface. The shipped
`task_sync_settings` configuration tool is separate from the fourteen work words
and `search`; see [project opt-in](features/task-planning-text.md#project-opt-in-setting).

The work surface is fourteen words and `search` — fifteen work tools. Ordinary
MCP registers the separate configuration tool as well, for sixteen tools total.
The six-operation work core is unchanged: `gate` already wraps the existing
evidence path, `remember`, `memories`, and `forget` are a thin project-memory
surface outside it — no focus mutation, no claim renewal — and `evaluate` is
the service's separate evaluation entry, not a core operation and not exposed
through `engram work core` or the host-private protocol. Reads use
the cooperative asserted project binding. `remember` and `forget` validate the
same non-empty actor/session binding inside the memory mutation transaction.
`memory_binding_invalid` means that binding is absent or inconsistent. The
implementation keeps this tool count, the MCP registration and instructions,
and every agent instruction file atomic with the code.

Project-memory list/search and full-read receipts fit both their structured
JSON/MCP representation and terminal-safe shell rendering under the 12 KiB
agent response ceiling. A full body is admitted before persistence only when
both exact envelopes fit with room left for a read reminder; list/search
sheds rows with an omission or continuation signal when escaping would
otherwise exceed the ceiling. A version stored before that reminder room was
reserved may exceed the ceiling by at most the reserve when its full read
carries a reminder (see
[work claims](features/local-work-system.md#work-claims)).

Agent-facing work has no grant token, validity window, revocation object, or
routine remint requirement. Its lifecycle mutations are governed by project
binding, current item state, fenced claims, and reason-attributed audit records.

Project-memory advertisement in `next` is advisory and carries no memory
text: a count of retained project notes, a changed flag and, while it is
set, the keys changed since the session's recorded memory position, with
no exactly-once guarantee. `memories` is the source of truth, and a
host-passed `context_generation` asserts a fresh or compacted context and
reannounces the count: until an unfiltered `memories` listing of the session
carries that value, `next --peek` directs the session to list its memories
before acting. Only a domain-separated digest of that asserted
value is persisted; its raw text is never retained.

This surface is for capture, retrieval, explanation, and coordination
requests. It is not an authorization channel: a model-callable MCP tool
grants no turn, file or external authority, and the host-private
work-session operations are never exposed through it.

### 8.3 Host work-session channel

A separate host-private transport, `engram control --work-session`, exposes
the §2.7 protocol as JSON lines:

```text
work_session_read   control_connection_read
named_root_bind   named_root_read   named_root_sighting_read
execution_observe   verification_record
remote_verification_record
acceptance_binding_read   acceptance_verification_read
```

None is exposed through agent-facing MCP, and none grants a model turn,
filesystem, tool or external-action authority. Exact retry evidence survives
restart, and a new connection fences its predecessor for the same session.
Every request variant is strict: the paired consumer must send exactly the
current field set for every operation. Any additive, unknown, or removed
request field is an `invalid_request` refusal that names the request kind and
offending field. There is deliberately no version-negotiation or legacy-frame
shim: the paired TermAl consumer updates its host-frame shapes in step with
each Engram build. See
[work-session identifiers](features/cli-and-mcp.md#identifiers-and-fingerprints-on-the-work-session-channel)
for current field semantics.

An `execution_observe` record binds the exact claim epoch the host observed.
With `account_if_eligible`, it resolves the obligation rule set its
`policy_basis` names, which must be the project's current one, and records
its id on the canonical observation. The built-in set maps every
observation accounted as a new `source_change` to one immutable test
obligation regardless of outcome or source-basis presence. That accounting is
the core's reading of the host's report: a request that reports no change is
`no_source_change`, a `source_sighting` only records where the source was,
and a measured change whose revision equals that of the newest change
recorded in its workspace under the same claim, with no other revision seen
there since, is a `repeat`; none of these opens an obligation. An
observation an earlier build admitted from a turn keeps the
`source_changed` reading recorded with it, and only one recorded as true
opened an obligation. Each definition binds the same rule-set id, rule identity/version, trigger,
and requirement. A later policy activation applies only to later
observations and cannot reinterpret existing history. Obligation definitions
and terminal satisfaction/waiver events are direct project, root-work, and
run-execution feed objects; query rows are verified projections.

Registered verification, from `verification_record`, is the typed
verification that may satisfy an obligation. A typed V1 requirement may
leave the verification command open, as the stock set does, or pin an exact
`check_fingerprint`. It never names an environment. A mismatched command or
source basis leaves the obligation open; only exact passed evidence at the
post-mutation cut satisfies it. A passed test satisfies open definitions only
against the latest mutation at the evaluated run-feed cut. For a claim
without a named root, a latest basisless mutation therefore leaves the open
set waiver-only until a later basis-bearing mutation and passed test; that
test may satisfy earlier definitions too. Under a named root, a fresh check
in the root that ran after a basisless change accounts for it. Observed
checks are facts without test credit. Agent-authored generic evidence never
satisfies verification.

Focus, nested next views, updates, and both completion outcomes expose one
count- and byte-bounded, authority-redacted `obligation_page` with an
explicit omission count and deterministic typed guidance. Waiver is absent
from MCP and `work_update`. The operator shell command has no credential
check; its separation is a local convention, not an authenticated boundary.
The stock `source_mutation_requires_test` rule records rather than blocks:
completion resolves each of that rule's still-open definitions as a waiver in
the completing actor's name, inside the sealed cut, and discloses the change
as untested, unless a named root, active now or bound when the change was
recorded, changes its disposition. A foreign change from before the binding
receives a terminal `displaced` resolution under any source-change rule and
is disclosed in the seal. A foreign change recorded under a bound name, or an
unknown-root change recorded while a root was bound, stays open and refuses
completion, even after that root ended or the claim was released. Completion
with a [live registered-input decision](features/acceptance-evaluation.md#registered-git-input-evidence)
instead uses the named root's Git state for an unlocated report; located
foreign changes keep their separate guard. Completion
then evaluates the cut-aware set at the exact pre-seal run-feed position. Any
other open definition returns a bounded `open_work_obligations` protocol
result recomputed from one coherent current snapshot. A new seal declares
obligation schema V1 and binds every applicable definition to its satisfied,
waived or displaced resolution, and declares environment schema V1 with the
sorted, distinct environment-evidence ids at or before the exact dense cut,
at most 64. Required child seals are decoded and checked recursively.

The transport may be an in-process API, native host integration, wrapper, or
local gateway. The runtime owns notification delivery and action execution.
Engram owns durable protocol state and cursors. No host-specific dependency
enters the domain core. The full contract is in the
[Execution Observability brief](features/execution-observability.md).

## 9. Local work, reports & external systems

### 9.1 The boundary

> **Division of labor.** Engram owns host-local work and execution memory.
> External systems are optional snapshot sources, backup or task-exchange substrates, or
> publication targets. None is the live local work database, and none is
> required to open, decompose, execute, or complete work.

The core contains no Jira-, GitHub-, or Beads-shaped types. Everything
vendor-specific lives behind neutral adapter ports. Optional durability
storage is separate from work intake and publication; configuring one does not
silently enable the others.

### 9.2 External adapter ports

```
WorkSourceAdapter {
  capabilities()                    // what this backend supports
  normalize_ref(text) → Ref         // "ABC-123", URL, … → canonical ref
  fetch_snapshot(ref, projection) → WorkSourceSnapshot
  search(query, cursor) → [SourceCandidate]
}

BackupAdapter {
  put(project, manifest, artifact) → BackupReceipt
  confirm(project, manifest) → confirmed | missing | unknown
  list(project, cursor) → [BackupManifest]
  get(project, copy) → artifact
}

PublicationAdapter {
  capabilities()
  publish_report(target, report, idempotency_key) → Receipt
  publish_work?(target, projection, idempotency_key) → Receipt
}
```

`WorkSourceSnapshot` is backend-neutral: canonical ref, projected title/body/
status/owner, captured time, source revision, canonical URL, canonical payload
hash, and bounded extension data. A `PublicationAdapter` accepts only an
explicit target and frozen payload under a durable idempotency key.
`BackupAdapter` stores, confirms, lists and returns immutable backup copies
of either kind, as [off-host backup](features/off-host-backup.md) defines.
The earlier `PortableStoreAdapter` release/acquire interface is superseded for
the selected personal workflow by task-text exchange (§3.2). Git transport
carries ordinary commits; it does not transfer writer ownership. Backup remains
separate from task exchange and a future live multi-writer `Sync` backend.

### 9.3 Provenance across a mutable source

An import stores an immutable source snapshot and creates a local root from
the caller's explicitly authored draft, citing the snapshot's minted id.
External status and owner remain source facts, not local lifecycle or
assignment. The local item then evolves independently.

For an existing source key, an explicit refresh with a changed snapshot
records an attributed, immutable source-change notice. It captures the new
snapshot, original citation and local basis seen at capture; it applies no
local revision and leaves priority, acceptance, graph edges, claims, evidence
and completion intact. Notices enter the project and root feeds, never run
evidence, and are not completion obligations. An already cited or previously
notified snapshot creates no duplicate notice.

The earlier proposed-local-revision model is deliberately superseded:
accepting such a proposal would synchronize external planning into locally
owned work. There is no accept-proposal command. Any local change is a
separate authored revision through `work update`, as the shipped
[source-intake contract](features/source-intake.md) describes. A notice records
divergence at capture, not current external state or proof of reconciliation.
Memories derived from mutable external state cite the relevant snapshot id
so their basis remains reproducible after the source changes or disappears.

### 9.4 Adapters & phasing

- **V1 local:** no external adapter is required. SQLite is canonical and the
  work graph is fully functional in `local` durability mode.
- **V1 portability/compatibility:** previewed, round-trip Beads snapshot
  import/export; deterministic work-graph recovery snapshot/restore;
  optional Git task-text export/import on the data branch, explicit push/pull,
  local change cursors and agent-mediated merge (§3.2). Publication remains a separate deferred
  capability whose frozen-payload idempotency must be proved end to end.
- **Later optional modes:** live concurrent `Sync`, real GitHub/Jira/
  proprietary intake and publication, comments, and link-backs. Automatic
  tracker mirroring and autonomous reprioritization remain non-goals.
  Outbound actions require explicit user authority or a bounded delegation and
  durable idempotency receipts.

### 9.5 Finalization & the report contract

Report finalization is an optional path after local work completion (§2.6).
It consumes the run's immutable `CompletionSeal`; it never quiesces or drains
execution again. The root seal already proves every expected contributor
supplied a required child seal or authorized omission, reconciled every action
outcome, contributed, and satisfied
acceptance, and closed every obligation applicable at the exact completion cut
with a bound terminal resolution—or an attributed, audited waiver by a
project-bound session records the omission. New seals also cite the exact
bounded environment-evidence id set
at that cut; the component objects remain separate canonical evidence. A
root whose completion transitively rests on a restored completion from the
shipped work-graph snapshot is refused here with `report_input_restored`.
Reopening the root before report
freeze supersedes that run and aborts assembly; reopening after `report_ready`
requires a superseding report rather than mutating frozen bytes.

After the barrier, Engram creates a `ReportAssembly` anchored to the root
`CompletionSeal`. The designated assembler holds a fenced
`ReportAssemblyClaim`; this planned authority is distinct from the terminalized
work claim and binds the seal and assembly generation. It is restricted to
deterministic report assembly and polishing. It cannot
authorize ordinary execution mutation. Only then is the report **frozen at
`report_ready`**—an immutable object with a `report_hash`—and the assembly
claim terminalizes. If publication is requested, a separate immutable intent
binds those bytes to a target and idempotency key.

Publishing hands the frozen bytes to a `PublicationAdapter` under that key;
failures retry the identical payload, and only an adapter receipt marks the
report `published`. Local work remains completed regardless of publication
state. A revised report is a superseding version under a new intent
and key — the pairing of one immutable payload with one idempotency key is
what makes "retry" a safe word. Session-end distillation may update local
working memory at any time, but nothing reaches the external system except
through this explicit step.

The report contract, in order:

1. Outcome / summary
2. Work performed
3. Decisions and rationale
4. Constraints and conventions discovered
5. Validation and evidence
6. Unresolved risks, blockers, follow-ups
7. Durable-memory promotion candidates
8. Provenance: local root/work/run ids, memory/version and participant-contribution
   hashes, timestamps, actors, assurance, and any finalization waivers

The report **cites the local memory and version IDs it was distilled from**,
so the local record can always explain the published artifact.

> **A reporting boundary, not truth promotion.** Facts and constraints
> discovered during a task appear as report sections and as *promotion
> candidates* — they never silently become global or org memory. Promotion
> back into durable memory follows the ordinary write policy (§5).

**Retention after completion or publication** is configurable. Completed local
work remains durable without a publication target. When publication is
requested, retain source memories through the confirmed receipt plus a grace
period, then compact according to policy to the final report, provenance
index, and tombstones. No source state is auto-deleted merely because no
external adapter exists.

## 10. Evaluation & telemetry

Retrieval quality is part of the product. Tests cover bounded work delivery,
scope isolation, Unicode FTS queries, and project-memory revision reads.
No context-packet evaluation harness or packet-ranking telemetry is shipped.
Further retrieval measurement may use:

- **Golden query set** maintained alongside the memory corpus: task
  descriptions → the memories that should surface.
- **Metrics:** precision@k and recall@k for explicit search, wrong/stale
  memory rate, cited-in-answer rate, and receipt bytes. **Precision before
  recall:** a plausible wrong memory silently corrupts work; a visible miss
  just prompts a search.
- **Retrieval decision logs** without sensitive bodies: candidate ids
  considered and included, scope and query basis, bounded omissions, and
  whether the agent used or cited each item. This is a measurement proposal,
  not a packet-construction API.

## 11. Delivery plan

| Phase | Contents |
| --- | --- |
| **v1** | Rust core; stable project-id keyed active-host SQLite store (append-only canonical objects, WAL, multi-process access) with derived FTS5 tables; first-class local work items/root executions/single-executor runs, parent forest + combined completion-dependency DAG, assignment, priority, labels, deferral, derived ready views, fenced work claims distinct from mutation authority, evidence-gated completion, human decision objects, and the six-operation ambient agent protocol; attributed work notes and keyed project memories; bounded work context, dense ordered peer deltas, and policy epochs; work handoff, contributions/child seals, separate fenced report assembly and optional publication; deterministic recovery snapshot/restore, optional explicit Git task-text push/pull with agent-mediated merge and main-checkout exchange, plus round-trip Beads compatibility; audit attribution at asserted-runtime-context assurance; visibly labeled no-op Redactor; CLI + agent MCP + host-private work-session transport over one core; integrity/preflight and hostile-process tests; `doctor` / explicit projection repair. |
| **v1.x** | Session-end distillation into working memory (proposer + dedup); episodic compaction; completed-work retention compaction; budget and ready-ranking tuning; optional configured external backup automation. |
| **v2+** | Optional live cross-host `Sync`/team backend; real GitHub/Jira/proprietary source and publication adapters; optional embeddings; comments/link-backs; real Redactor/DLP; Postgres/service `Store`; Signer-based attestation; envelope encryption for crypto-shredding. |

> **Scope discipline.** V1's riskiest cut is doing too much. Everything in v1
> serves one loop: open or import local work → decompose/select ready work →
> coordinate concurrent sessions through claims and host observations →
> complete with evidence → optionally freeze/publish a report → review
> promotion candidates. Cross-host sync and real external adapters widen that
> loop but are not dependencies of local execution.

## 12. Decisions

The Draft 0.2 open questions, resolved by Greg on 2026-08-23 (relayed via
Codex::AgentMemory):

| Question | Decision |
| --- | --- |
| Name | **Engram** — settled (binary: `engram`). |
| Implementation language | **Rust.** |
| External adapters | Optional on intake, durability, and publication. V1 targets deterministic recovery plus optional agent-mediated Git task sync, defers publication adapters, and targets round-trip Beads compatibility; no proprietary integration is required (§9.4). |
| Identity source | Proprietary runtime context: instruction/authority arrives as text through the tools and skills in use — asserted context, not cryptographic identity (§7). No SSO/LDAP in V1. |
| Cross-host storage / team scope | Each host remains locally writable; optional task-text Git sync permits divergence and explicit merge (§3.2). Global execution coordination remains deferred (§3.3). |
| Redaction backend | None selected. Port + safe defaults ship; the no-op development implementation is visibly labeled and implies no compliance assurance (§7). |
| Architecture refinement | Local work graph plus dual-layer memory/report model; final report and publication are optional state machines after local completion (§1, §2.6, §9.5). |
| Multi-session operating model | Normal in V1 on one host. Root-work memory is shared by default; agent scratch is private. Each `WorkRun` has one ordinary executor/claim, parallel sessions claim distinct children under a `RootExecution`, and claims, typed source/delivery positions, contributions, child seals, and a completion barrier are V1 primitives. |
| Product seam | Engram owns host-local work from creation/decomposition through completion. External intake, storage/sync, and publication are independent optional ports. One capture generates work delta, handoff, evidence, and report inputs. |
| Execution Observability | The host records execution, source and test observations and registered verification, switched on or off per project; Engram stores and links them and never admits or refuses a turn. Authority stays with the user and host (§2.7, §8.3). |

### Still open

- Default grace period for post-publication retention (§9.5) — pick during V1
  implementation.
- Git task sync (§3.2) is selected for cross-machine use. The exact import
  schema, completion/evidence mapping, transactional change coverage and
  worktree setup require implementation design and validation. Global live
  coordination remains deferred (§3.3).
- Timing of the proprietary tracker adapter — when work authorizes real
  publication.
- The user configures the Git repository and orphan data branch for optional task
  exchange; there is no background cadence in the selected workflow. Backup
  copy kinds and defaults remain separate, as decided in
  [off-host backup](features/off-host-backup.md).

## Appendix A — Decision log

How this spec was produced: both authors designed independently against the
same brief, merged via a structured mailbox exchange, hardened in a review
round, and revised to Greg's decisions as they landed. Positions and
outcomes:

| Topic | Fable v1 | Codex position | Merged outcome |
| --- | --- | --- | --- |
| Typing | Four species, behavior bound to type | Orthogonal kind / authority / delivery axes; `decision` first-class | **Codex**, plus Fable's derived-default mapping so the simple mental model survives (§2.2) |
| Identity | Single record, edited via supersedes | Stable id + immutable content-addressed versions with parents | **Codex** (§2.1, §2.4). *Superseded 2026-09-17: a version's id is a random UUID minted when it is stored, never derived from its bytes; hashes remain content fingerprints only (§3.1.1).* |
| Storage | SQLite canonical, JSONL export | Git object store canonical; SQLite as disposable derived index | **Codex** (Draft 0.2); superseded by Greg's local-first V1 (SQLite canonical, Git deferred, §3) |
| Retrieval shape | Three rungs, hard budgets, titles index | Agree; add fail-closed pinned tier, omission manifest, packet fingerprint/explain | **Fable** structure + **Codex** hardening (§4) |
| Write policy | Trust follows priority; distillation proposes only | Refine by origin × authority; evidence-backed agent writes activate | **Both** — merged matrix (§5) |
| Lifecycle clocks | Single `review_by` | Separate `review_by` from `valid_until`; stale ≠ expired | **Codex** (§6.1) |
| Conflicts | Supersedes chains | Contested state, `contradicts` edges, multi-parent resolution; no LWW | **Codex** (§6.3) |
| Forgetting | Forget = delete | Tombstone vs. physical purge, both audited | **Codex** (§6.5) |
| Compliance | Provenance fields in schema | Full audit set: actor identity, Redactor port, sensitivity labels, vault refs, safe exports, optional Signer | **Codex** (§7) |
| Ticketing | Never mirror; adapter port; read-only first | Agree, but snapshot mutable sources for reproducible provenance | **Fable** boundary + **Codex** snapshots (§9) |
| Evaluation | — | Golden queries, precision-first metrics, retrieval decision logs, in v1 | **Codex** (§10) |
| Round-2 hardening | Draft 0.1 as adjudicated | Four clarifications: fail-closed pinned contradictions; canonical-bytes contract (JCS); identity assurance not overclaimed; purge realism vs. Git history | **Codex**, all four adopted (§4.1, §3.1.1, §7, §6.5) → Draft 0.2, ACKed by both authors |
| Greg's decisions | Round 3 — name Engram; Rust; proprietary tracker → `DummyTrackerAdapter` in V1; runtime-context identity, no SSO/LDAP; no DLP backend selected; no team scope in V1 (Greg, relayed via Codex::AgentMemory) | | Recorded (§12) → Draft 0.3. *Publication scaffolding superseded 2026-09-20: the unwired dummy adapter is removed; publication remains a deferred optional capability (§9.2).* |
| Dual memory / report model | Greg: local working memory while work runs; polished final report published to the tracker at finalization. Codex elaboration: SQLite canonical in V1; Git store deferred, not rejected; finalization state machine; report contract with cited memory/version IDs; configurable post-publication retention | | Adopted (§1, §2.6, §3, §9.5) → Draft 0.3 |
| Round-4 correction | Failure transition returned to `finalization_pending` | Freeze the report at `report_ready`: immutable report + hash bound to the idempotency key; failure retries identical bytes, never re-enters distillation; revision = superseding version + new intent. Endorsed keeping §3.1.1/JCS in V1 | **Codex**, adopted (§2.6, §9.5) → Draft 0.4, final ACK by both authors |
| Round-5 product test | Greg asked whether the authors would actually use Engram and clarified that multi-session work is normal. Fable identified capture ceremony, double entry, claims, and peer visibility as adoption blockers. | Codex separated packet fingerprint from ordered event cursor, added leases/recovery, finalization barrier, and stable project identity across worktrees. | **Both**, joint position confirmed: narrow Engram to concurrent execution memory with three visibility rings, one-write-many-views, deterministic report assembly, and the backlog/execution seam (§1–5, §8–9) → Draft 0.5 |
| Round-6 control correction | Greg identified the missing behavioral and coordination layer and asked Engram to control it. Engram::Opus independently argued that the hot path should mediate by inlining fresh context, with refusal as a narrow tail; semantic capture must not gate edits; enforcement needs observe/replay evidence and honest coverage. | A voluntary memory loop is not control. Separate deterministic decisions from host enforcement; add recovery/finalizer grants and a stable quiescence cut to avoid refusal/finalization deadlocks; mediate declared material capabilities with scoped fenced leases, checkpoints, and crash-safe receipts. | **Adopted:** Engram becomes the task-bound behavioral/coordination decision plane while the host remains actuator/reference monitor. Normal pre-turn grants inline delivery; capability-specific failure policy, observe-first rollout, and advisory/turn-gated/action-gated assurance keep the claim honest (§2.7, §8.3) → Draft 0.6 *Historical: superseded 2026-11-02, when turn gating was removed (see the last row).* |
| Round-7 local-work correction | Greg required Engram to replace Beads for local work: external injection and publication are both optional because heavy external systems do not scale to execution-time decomposition. Engram::Opus endorsed local ownership, claim/lease separation, derived readiness, and optional boundaries, while challenging durability, assignment, agent ceremony, root memory scope, and authority escalation. | Add a first-class work graph and six-operation ambient model protocol; keep assignment, claim, and resource lease distinct; bind grants to work/claim/lease/context fences; make completion evidence-based; separate publication; expose explicit durability modes. | **Adopted with Greg's clarification:** SQLite is immediately authoritative in valid `local` mode; optional durability improves over time but is never a prerequisite for local execution. Honest mode claims, restore/integrity tooling, and round-trip Beads compatibility bound the replacement promise (§2.6, §3.4, §8, §9) → Draft 0.7. |
| Round-7 independent rereview | Engram::Opus accepted Greg's optional-storage clarification but found stale durability-gate prose, run-owned memory that would disappear on reopen, ambiguous sparse/global cursors in safety CAS, focus/claim coupling, and completion/report drain ordering. | Preserve memory on the root work item across runs; use dense named project/root/run source feeds plus a separate per-session delivery sequence; make focus navigation-only; put draining in `CompletionSeal` and make optional report assembly consume it. | **Adopted before implementation.** The rereview identified no additional P0/P1 category beyond these exact corrections (§1.2, §2.6–2.7, §3.1, §8–9). |
| Round-7 Codex verification | Independent read-only verification found four implementation-blocking ambiguities: finalizer authority survived a terminal work claim, participant plurality conflicted with a singular run claim, scalar cursors lacked feed identity, and separately acyclic hierarchy/prerequisite graphs could still create a completion deadlock. | Add fenced post-completion `ReportAssemblyClaim`; make one executor own each child `WorkRun` under a root aggregate; type source feed positions separately from session delivery positions; cycle-check explicit prerequisites plus implicit required-child completion edges as one graph. | **Adopted before implementation.** No external storage is required by any correction (§2.6–2.7, §3.1, §9.5). |
| Round-8 portability correction | Greg stated that Engram starts local but must persist remotely, that he moves between machines, and that repository scale may reach hundreds of developers. He agreed to a canonical human-readable work projection and proposed a branch. | Engram::Opus separated restore-only backup, sequential portability, and concurrent sync; recommended a dedicated plumbing ref rather than a branch/working tree, scheduled push with visible lag, divergence refusal, sensitivity filtering, and no transfer of live claims/leases. | **Historical Draft 0.8 adoption, superseded for the selected personal workflow by §3.2:** optional V1 `portable` mode has one active host, explicit handoff/restore, head CAS, and no live-authority transfer. A private dedicated Git ref is the recommended personal transport; organization-scale substrate remains a product choice (§3.2–3.4, §7, §9.2–9.4) → Draft 0.8. |
| Round-8 portability verification | Independent Codex verification and Engram::Opus both found that push-time CAS alone did not protect restore/startup and that sensitivity-filtered content-addressed projections could sever object/feed references. | Add writer-epoch release/acquire, exact-base restore, bounded startup/resume validation, and honest forced-takeover detection; require executable shared-state closure, separately hashed exclusion stubs, dense feed placeholders, export pass-or-exclude, coverage diagnostics, and policy-hash equality on acquire. | **Historical portable proposal, superseded for the selected personal workflow by §3.2.** Steps 1–4 remain independent of remote storage; the portable step must satisfy these conformance rules (§2.7, §3.2, §7, §9.2–9.4). |
| Cleanup correction (2026-09-23) | Greg approved direct control-session binding, resource-lease and host-waiver removal, finalizer turn removal, object-id naming, and self-asserted acceptance disclosure. | Current contracts above supersede the lease/finalizer mechanisms described in the historical rows of this table; optional report assembly remains deferred. | Approved cleanup; paired host changes and explicit store conversion are required before deployment. |
| Turn-gating removal (2026-11-02) | Greg: turn gating caused the most stops and problems; replace it with Execution Observability, keep no legacy, and keep acceptance evaluation but switch it off until Execution Observability is implemented. | Delete turn admission (Evaluate/Begin grants, control-session binding, settlement and recovery), its operations, settings and init flags; keep host observations, named source roots, registered verification and claims on a work-session channel, switched on or off per project in the host. | **Adopted:** Execution Observability (§2.7, §8.3). Canonical records of the removed gating stay readable; the completed store migration retired its operational tables by name with their counts, and its converter has been removed. The rows above that describe grants, turn admission or control assurance are history. |

## Appendix B — Beads verdict

Both authors studied [Beads](https://github.com/gastownhall/beads) (Codex
read the memoryops, prime rendering, merge settlement, and compaction code;
Fable analyzed the docs and its observed behavior in production use). Shared
The earlier conclusion was "don't clone it—borrow its operational discipline,
replace its memory model." Draft 0.8 supersedes the product boundary behind
that sentence: Engram now also owns local work. It still does not copy Beads'
implementation or make Beads a dependency; it must cover the useful local
workflow while integrating task state with typed memory, evidence, and
Execution Observability.

**Borrow:** an explicit canonical source of truth distinct from
export/interchange formats; local/offline-first operation;
collision-resistant ids for concurrent writers; task state kept separate from
persistent note memory; typed graph edges with behavior
(supersedes / duplicates / derived-from); immutable audit/change history;
assignment distinct from live claim, priority/labels/deferral, ready and
blocked indexes, acceptance criteria, round-trip migration, dry-run preview before anything destructive; bounded work context
with explicit caps and visible omission; human and machine interfaces over
one core.

**Reject:** string-key/string-value memory as the domain model (observed
effect: users hand-encode type, author, date, and provenance in prose);
injecting the whole memory plane every session (observed effect: 11 memories
≈ 11.5 KB at every session start, growing linearly, already truncated by
hosts); substring-only search and alphabetical selection under caps; same-key
remote-wins merge — convergent but silently lossy (Engram makes the same
situation a visible contested state instead); no scope, evidence, confidence,
sensitivity, validity, or usage signals; coupling the project to Dolt;
"memory decay" that summarizes closed issues while durable memory notes have
no lifecycle at all.

The 2026-10-09 Git task-sync decision (§3.2) supersedes the historical Round-8
sequential-portability and dedicated-ref decisions above for the selected
personal cross-machine workflow. The retained log records provenance, not a
requirement to implement writer-epoch handoff before task sync.
