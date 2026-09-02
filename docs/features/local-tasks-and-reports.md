# Local Tasks & Reports

> Normative references: [spec §2.6](../spec.md#26-local-work-graph--execution)
> and [spec §9.5](../spec.md#95-finalization--the-report-contract).
> Related briefs: [tracker adapter](tracker-adapter.md),
> [local work system](local-work-system.md),
> [write policy & review](write-policy-and-review.md),
> [behavioral control plane](behavioral-control-plane.md), and
> [execution pipeline](execution-pipeline.md).

An Engram `RootExecution` coordinates the live multi-session execution of one
root and its descendants. Each `WorkRun` is the execution generation for one
item in the [local work graph](local-work-system.md), with one ordinary
executor and at most one live claim in V1. The run owns that executor's
checkpoint, evidence, resource leases, and completion state; parallel
sessions claim distinct child runs. The root execution owns the contributor
roster and root completion barrier. Shared working memory belongs to the
stable root work item—not either execution record—and survives reopened
generations. A work item may originate locally or carry an immutable snapshot
from an external system; neither intake nor publication is required. While a
run is active, its working memory is the operational source of truth for its
  root members. Root memory is shared by default; agent-scoped scratch remains
private.

## Multi-session coordination

- A stable project id resolves every session and isolated worktree to one
  host-local store.
- Root membership grants visibility, not execution ownership. One executor
  claims each `WorkRun`; parallel sessions claim distinct child runs. An
  execution lease covers canonical project-relative path or logical subjects;
  a coordination lease covers exclusive open-run lifecycle changes; shared
  analysis requires neither. Report assembly uses its own post-completion
  claim, not a retained coordination lease.
- Lease claim is atomic and idempotent, with revision, monotonic ownership
  fence, expiry, heartbeat, explicit handoff/release, and audited recovery.
  Independent resource subjects may proceed concurrently; a stale holder cannot
  act after transfer or recovery.
- Every work/run mutation appends an immutable event with a dense position in
  its named feed. Peers request deltas after their last processed feed
  position; a host mailbox may
  wake them but is not the source of truth.
- The Engram work graph is the local backlog and execution system of record.
  External adapters import snapshots or publish explicit artifacts; they do
  not create a second live task state.

## Completion and report state

```text
shipped: open → completed
target when a mediated drain is nonempty:
         open → completion_pending → completed
                    └── abort ───────→ open

not_requested → finalization_pending → report_ready → publishing → published
                       └── abort → not_requested            └── failure → report_ready
```

- **Complete** is the execution barrier. The shipped zero-linked-state path
  requires empty action-outcome and resource-lease drain sets, terminalizes the
  work claim, and seals in one transaction. A root barrier also freezes the
  `RootExecution` contributor roster and waits for each required child seal or
  explicit reason-attributed disposed-child waiver, plus contributions or a
  reason-attributed, audited participant waiver by a project-bound session.
  The target controlled path enters
  `completion_pending` to deny new ordinary mutation while the host reconciles
  actions and releases/transfers linked leases; nonempty drains are refused in
  the current alpha.
  `completion_seal` atomically captures the dense run-feed cut, accepted work
  revision, run/claim fences, reconciled action outcomes, released/transferred
  leases, acceptance results, and evidence hashes. A root seal also binds the
  required child seal hashes and aggregate roster/decisions/waivers. Discovery
  of more work before the seal aborts to `open`. Reopen after completion
  creates a new run generation while preserving root-work memory.
- **Finalize** is optional and consumes the immutable `CompletionSeal`; it
  never drains execution again. Engram creates a `ReportAssembly` anchored to
  the root seal and gives the designated holder a fenced
  `ReportAssemblyClaim`. A narrow finalizer grant binds the seal hash, assembly
  generation/revision, and claim fence, then deterministically buckets
  root-work memories and completion contributions into report sections for
  one polishing pass. It cannot authorize ordinary workspace mutation.
  Reaching
  `report_ready` **freezes it**: an immutable object with a `report_hash`.
  When publication is requested, a separate immutable intent binds those
  bytes to its target and durable idempotency key.
- **Failure returns to `report_ready`** — recording the last error and
  attempt metadata — never back into distillation. Every retry sends the
  exact same frozen bytes under the same key; same key with a different
  payload is a conflict.
- **Revision after a failure** creates a superseding report version with a
  *new* publication intent and idempotency key. The old intent is never
  mutated or reused. One immutable payload per idempotency key is the
  invariant that makes "retry" a safe word.
- A task is never marked `published` without an adapter **receipt**.

An executor can satisfy run completion only after the work item's required
children and prerequisites are complete or explicitly waived, its last turn
is checkpointed, its session delivery position/source-feed vector reaches the
completion cut, every action has a known outcome, and every resource lease was
released or transferred. Root completion additionally consumes required
child seals and the root contribution roster. Report freeze requires the
completion seal plus a live `ReportAssemblyClaim` and matching finalizer grant;
publication is a separately authorized external effect. `report_ready` is the
irreversible bytes/hash boundary. A publication intent separately freezes
target and idempotency key; later corrections create a superseding report.

## The report contract

In order:

1. Outcome / summary
2. Work performed
3. Decisions and rationale
4. Constraints and conventions discovered
5. Validation and evidence
6. Unresolved risks, blockers, follow-ups
7. Durable-memory promotion candidates
8. Provenance: local root/work/run ids, memory/version and contribution hashes,
   timestamps, actors, assurance, and any participant waivers

The report cites the local memory and version IDs it was distilled from, so
the local record can always explain the published artifact.

## A reporting boundary, not truth promotion

Facts and constraints discovered during a task appear as report sections and
as *promotion candidates* — they never silently become broader durable
memory. Promotion follows the ordinary
[write policy](write-policy-and-review.md).

## Retention after completion or publication

Configurable. A completed local work item remains durable even when it has no
publication target. When publication is requested, retain its source memories
through the confirmed receipt plus a grace period, then compact according to
policy to the final report, evidence/provenance index, and tombstones.
Unpublished source state is never auto-deleted merely because no adapter was
configured.
