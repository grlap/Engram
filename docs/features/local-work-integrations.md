# Local Work Integrations and Durability

[Local work index](local-work-system.md) · [Architecture](../architecture.md)

## Execution Observability integration

Work calls describe intent. Separately, a host that runs
[Execution Observability](execution-observability.md) records, over its
private work session, what it saw run, how the source changed and which
registered checks passed, each bound to the exact claim epoch it observed:

```text
work_id + work_revision + run_id
claim_id + claim_fence
```

Nothing there admits or refuses a model turn, and a model-facing MCP call can
propose, query or request a transition but never records a host observation.
No host-private value appears in an agent-visible MCP response; the model sees
only references, obligations, reasons, and allowed next operations. Human
authorization is a distinct attributed object or delegation, never a prose
`reason` field that the model can manufacture. Semantic checkpoints add
meaning and evidence, but mutation is never conditioned on the model first
writing a status sentence.

## Optional external intake, storage, and publication

[File intake](source-intake.md) previews and admits one source snapshot as a
local root. An exact project-scoped source key finds its original citation
and latest change notice. Refresh never applies local fields; ordinary show
reports notices with exact counts and detail navigation.

Adapters have independent capability families:

```text
WorkSourceAdapter {
  capabilities()
  normalize_ref(input)
  fetch_snapshot(ref, projection) -> WorkSourceSnapshot
  search(query, cursor) -> [SourceCandidate]
}

BackupAdapter {
  put(project, manifest, artifact) -> BackupReceipt
  confirm(project, manifest) -> confirmed | missing | unknown
  list(project, cursor) -> [BackupManifest]
  get(project, copy) -> artifact
}

PortableStoreAdapter {
  read_head(project) -> PortableHead
  fetch_snapshot(project, head_hash) -> RecoverySnapshot
  publish(project, expected_parent, active_snapshot) -> PortableReceipt
  release_writer(project, expected_active_head, released_snapshot) -> PortableReceipt
  acquire_writer(project, expected_released_head, active_manifest) -> PortableReceipt
  recover_writer(project, expected_head, recovery_intent, active_manifest) -> PortableReceipt
  validate_writer(project, writer_instance_id, writer_epoch) -> WriterValidation
}

PublicationAdapter {
  capabilities()
  publish_report(target, frozen_report, idempotency_key) -> Receipt
  publish_work?(target, work_projection, idempotency_key) -> Receipt
}
```

No adapter is required to initialize or finish local work. A configured
portable store supports sequential host handoff; a later live `Sync` backend
is distinct from both portable replication and snapshot backup. Import never grants
the external source authority to overwrite local execution facts. Publication
requires a frozen payload, a durable intent, an explicit target, an
idempotency key, and authority; retry sends identical bytes. Publishing work
state and publishing a final report are distinct optional capabilities.
Beads migration must be round-trip: previewed import and explicit export retain
ids, hierarchy, prerequisites, fields, and provenance well enough to return to
the source without silent loss. This is interoperability, not mirroring.

## Durability modes and integrity

Engram reports durability separately from host observations and from external
publication:

| Mode | Off-host copy | Writer model | Normal remote reads |
| --- | --- | --- | --- |
| `local` | No | Concurrent sessions on one host | None |
| `local_backed_up` | Verified restore copy at a configured target; off-host asserted or confirmed, as [off-host backup](off-host-backup.md) defines | Concurrent sessions on one host | A confirm during a push, an explicit `backup status --check-target`, and explicit restore; `doctor` and ordinary `status` read nothing remote |
| `portable` | Transferable working snapshot | Release/acquire enforces clean handoff; forced takeover is detected within a bounded validation window | Head/epoch validation plus explicit restore/handoff |
| `synchronized` | Shared working state | Concurrent hosts | Live synchronization |

All modes use the same local work semantics. External storage is optional,
and local-only mode is a valid source of truth when the user accepts its
recovery boundary. `portable` is the V1 cross-machine target: a configured
daemon or session hook publishes on a cadence and at clean session end;
`engram doctor` reports the last verified remote head, unpushed event/byte/age
lag, and a visible degraded state after failure. Moving machines is an
explicit project-level release/acquire around restore of that exact head. A
portable manifest binds its parent, consistent source cut, feed heads,
export-policy hash, writer instance, and monotonic writer epoch. Release
checkpoints/exits local sessions, makes unfinished claims recoverable,
invalidates grants/delivery authority, CAS-publishes a `released`
manifest, and makes the old store mutation-read-only. Acquire CAS-publishes a
new active instance/epoch before enabling local writes. Crash takeover is an
attributed recovery. The remote is not read as a second live database during
active execution.

Restore/acquire refuses unless its destination is empty or exactly at the
expected manifest with no unpushed local tail; a divergent destination is
preserved as a recovery bundle. Portable process/session start and crash
resume perform a bounded remote head/epoch validation before any mutation
grant, and a configured cadence revalidates it. That metadata check is writer
authority validation, not a remote work-database read. Mismatch, expiry, or
unavailability makes the store mutation-read-only and invalidates grants;
local-mode projects are unaffected.

A portable push compare-and-swaps the expected remote parent manifest. A
changed remote head is `portable_diverged`: Engram refuses to push or merge
and directs the operator to `engram portable reconcile`. Reconciliation
previews both immutable lineages and either continues one while retaining the
other as a recovery bundle/proposed import, or forks a new project identity;
it never silently renumbers dense feeds or drops a lineage. This preserves a
single dense feed sequence in portable mode. Live concurrent synchronization
must instead add per-origin ordering or a server sequencer and remains later.

A portable payload contains canonical shared objects, the local work graph,
feed ordering, evidence references, schemas, and a manifest. A graph backup
copy contains only what the copy-kind table of
[off-host backup](off-host-backup.md#copy-kinds) lists for it, and restores
no claim at all. Neither restores a live `WorkClaim` or delivery state. A full-store backup copy is the exception: it
restores every row as stored, under the preconditions that
[off-host backup](off-host-backup.md#restore) sets. In a portable payload, immutable claim lifecycle
facts may remain for audit, but an
unfinished old-host claim restores as `recoverable`; a new host performs an
attributed recovery and advances the generation/fence. Historical lease audit
events confer no live authority. Agent-private scratch never enters a portable payload.

The projection must be closed under executable shared-state references. Every
object required to rebuild the work graph, readiness, policy, root context,
acceptance/evidence, completion, and behavior-affecting feed history is either
included or release fails `portable_projection_incomplete`. Provenance-only
references into excluded content use an `ExclusionStub` under its own minted
id that names the excluded object's id and kind, the reason, and the
export-policy fingerprint without pretending to be that object. Excluded
non-semantic feed payloads leave typed placeholders at their original dense
positions. Export passes or excludes an existing canonical object; it never
rewrites its bytes under its id. `doctor`
distinguishes missing/corrupt from deliberately excluded, reports coverage,
and claims `portable` only for a complete shared-state closure. Acquire refuses
an export-policy hash mismatch. If policy forbids even stub metadata, the
result may be a marked-truncated backup but not a portable working store.

Before Engram claims Beads-equivalent off-host durability, it should ship:

- deterministic, human-readable work-graph recovery snapshots that can be
  committed or copied off-host without copying a live SQLite file — the
  shipped [work-graph snapshot](work-graph-snapshot.md);
- manifest hashes and a previewed restore path, exercised in CI;
- referential-integrity verification for work/events/edges/evidence plus
  projection rebuild-and-compare checks, not only canonical object ids;
- crash tests proving event/cursor ordering and atomic packet/head snapshots;
- configured-backup/portable freshness surfaced by `engram doctor`; and
- a documented recovery-point objective for each durability mode, with no
  implication that optional report publication protects the local backlog.

SQLite remains the live canonical store on the active host in V1. A configured
recovery snapshot is a restore artifact; a portable head is a sequentially
transferable canonical projection and cannot be mounted concurrently as a
second writer. The reference Git transport should use a dedicated plumbing
ref such as `refs/engram/<project-id>/<scope>`, never a checked-out branch or
the working tree. The port also permits a private repository or internal
object store. At organization scale, Engram must not create hundreds of refs
in a shared code repository: execution state needs an access-controlled store
whose lifetime and privacy are independent of the code remote. Cross-host live
coordination remains a later optional mode.

## Tracker replacement and interoperability

The local workflow dogfood has cut this repository and one migrated project
over to Engram as their only writable local tracker. The broader replacement
claim still waits for the off-host durability and control-binding
[roadmap](../roadmap.md#v1--close-the-loop) gates. The mapping below is the
daily workflow it covers, kept for anyone arriving from the previous tracker
(Beads):

| Beads workflow | Engram equivalent |
| --- | --- |
| `bd create`, parent/child | `work_propose` |
| `bd ready` | `work_next` with typed readiness reasons |
| `bd show`, search/list | Explicit `show REF` plus `ls`/`search` query views, without focus selection |
| `bd dep add`, blocked | prerequisite edges and typed blockers through `update --after` / `--drop-after` |
| assignee vs. `bd update --claim` | durable assignment vs. fenced live claim; resource mutation still needs host/user authority |
| notes/design/acceptance | typed work fields plus work-scoped shared/private memory and evidence |
| comments and handoff | one checkpoint feeding deltas, handoff, and report input |
| `bd close`, reopen, supersede | one-call evidence capture/seal when compact, or explicit evidence/checkpoint steps; audited reopen/cancel/supersede events through `update --supersede-with` |
| `bd remember` | Engram's typed durable memory, not a pseudo-task (`remember`/`memories`/`forget`) |
| `bd stats`, stale/orphans/preflight | rebuildable operational indexes and integrity diagnostics |
| Dolt cross-machine sync | V1 sequential `portable` handoff; later concurrent Engram `Sync` backend |

A Beads adapter imports selected issues as source snapshots and may optionally
publish an explicit projection. It does not make `.beads`, Dolt, or `bd` a
runtime dependency. The accurate current claim matches the
[roadmap](../roadmap.md#v1--close-the-loop): a writable local tracker in a
running dogfood, with the broader replacement declared only after the
off-host durability and control-binding gates; teams needing a concurrently
writable multi-machine backlog still need an external system or later
Engram sync.

For open work, the agent-facing `note` tool binds to persisted local focus or
an explicit `work_ref`. The holder records one shared finding plus its
evidence/checkpoint state for peers, handoff, and report assembly; a non-holder
records only the [marked work observation](local-work-protocol.md#gates-prerequisites-supersession-and-project-memories). For completed work,
`note` instead requires only a project-bound
session and appends attributed, marked post-seal evidence: it creates no
checkpoint or handoff contribution and never changes the frozen completion or
report basis. The service derives the idempotency key; an exact retry returns
the first receipt while changed prose is a new intent. Generic task memory and
private scratch are not separate MCP tools. Work focus still returns a bounded,
actor-filtered memory summary built from authorized canonical state, so the
work graph and its execution memory do not require an external tracker identity
shim.
Restricted-sensitivity bodies are omitted from task and work search/focus
views and remain unavailable through direct show in V1. `work_next` also
replaces restricted work memory and memory outside the currently focused root
with a typed omission marker, preserving its exact dense feed position without
exposing protected content.

## Delivery boundary

The smallest coherent implementation sequence is:

1. Define dense per-feed cursor identity, then add canonical work
   items/events, graph projections, local roots, hierarchy, prerequisites,
   ready queries, claims, and evidence-gated completion.
2. Bind host observations, the memory feed and registered verification to
   work revisions and claim fences.
3. Ship the six-operation agent protocol plus administrative CLI/query views,
   assignment, deferral, human decisions, and use Engram to track its own
   local implementation work.
4. Add the TermAl Execution Observability adapter; follow with the generic
   wrapper and native/custom integrations.
5. Add deterministic recovery snapshot/restore, scheduled portable push plus
   explicit cross-machine handoff/divergence refusal, integrity tests, and
   round-trip Beads import/export for migration and dogfooding.
6. Add optional report/work publication adapters. Revisit cross-host sync only
   when one project must coordinate live work concurrently across machines.

V1 does not include an autonomous planner, automatic external polling,
cross-project dependency solving, fairness scheduling, concurrent cross-host
sync, or an LLM-based duplicate/priority oracle. Models propose plans;
deterministic core rules admit, order, explain, and enforce them.

## Decisions still needed

The design can proceed with the [fixed project-bound work limits](local-work-protocol.md#audited-waivers-and-model-autonomy),
but product policy must eventually select:

- the completed-work retention and compaction period when nothing is
  published;
- the recovery snapshot format and acceptable recovery-point objective;
- the first portable substrate (recommended: private dedicated Git ref, with
  internal object storage for organization scale); and
- the trigger and substrate for concurrent cross-host synchronization.
