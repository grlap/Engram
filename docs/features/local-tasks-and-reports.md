# Local Tasks & Reports

> Normative reference: [spec §2.6, §9.5](../spec.md#26-local-tasks--finalization).
> Related briefs: [tracker adapter](tracker-adapter.md),
> [write policy & review](write-policy-and-review.md).

Engram tracks the operational task an agent — or a team of agents — is
executing, locally. The external tracker stays authoritative for the
organizational work item; a local task stores an `external_ref` and nothing
more. While a task is active, its working memory is the operational source of
truth for the agents on it. Task memory is shared among participants by
default; agent-scoped scratch remains private.

## Multi-session coordination

- A stable project id resolves every session and isolated worktree to one
  host-local store.
- `claim` is an atomic, idempotent lease with revision, expiry, heartbeat,
  explicit handoff/release, and audited force-release recovery.
- Every task mutation appends an immutable event with a monotonic cursor.
  Peers request deltas after their last processed cursor; a host mailbox may
  wake them but is not the source of truth.
- The external tracker remains the backlog of record. Engram is the bound
  execution workspace, so agents never maintain two independent task states.

## The finalization state machine

```
active → finalization_pending → report_ready → publishing → published
                                     ↑              |
                                     └── failure ───┘
```

- **Finalize** first waits for every expected participant to submit a
  contribution and mark ready. An arbiter may waive a missing participant
  only with an attributed reason. Engram then deterministically buckets task
  memories and contributions into the report sections for one polishing pass.
  Reaching
  `report_ready` **freezes it**: an immutable object with a `report_hash`,
  bound to a durable publication idempotency key.
- **Failure returns to `report_ready`** — recording the last error and
  attempt metadata — never back into distillation. Every retry sends the
  exact same frozen bytes under the same key; same key with a different
  payload is a conflict.
- **Revision after a failure** creates a superseding report version with a
  *new* publication intent and idempotency key. The old intent is never
  mutated or reused. One immutable payload per idempotency key is the
  invariant that makes "retry" a safe word.
- A task is never marked `published` without an adapter **receipt**.

## The report contract

In order:

1. Outcome / summary
2. Work performed
3. Decisions and rationale
4. Constraints and conventions discovered
5. Validation and evidence
6. Unresolved risks, blockers, follow-ups
7. Durable-memory promotion candidates
8. Provenance: local task id, memory/version and contribution hashes,
   timestamps, actors, assurance, and any participant waivers

The report cites the local memory and version IDs it was distilled from, so
the local record can always explain the published artifact.

## A reporting boundary, not truth promotion

Facts and constraints discovered during a task appear as report sections and
as *promotion candidates* — they never silently become broader durable
memory. Promotion follows the ordinary
[write policy](write-policy-and-review.md).

## Retention after publication

Configurable. Default: retain the local task and its source memories until a
confirmed publication receipt plus a grace period, then compact to the final
report, a provenance index, and tombstones. Unpublished source state is never
auto-deleted.
