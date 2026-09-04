# External Adapters

> Normative reference: [spec §9](../spec.md#9-local-work-reports--external-systems).
> Related briefs: [local work system](local-work-system.md),
> [local tasks & reports](local-tasks-and-reports.md), and
> [execution pipeline](execution-pipeline.md).

External systems are optional sources, backup/portable/sync substrates, and publication
targets, never a prerequisite for local work. The core defines vendor-neutral ports—no
Jira-, GitHub-, or Beads-shaped types—and proves outbound idempotency against
a side-effect-free dummy implementation.

## The ports

```
WorkSourceAdapter {
  capabilities()                    // what this backend supports
  normalize_ref(text) → Ref         // "ABC-123", URL, … → canonical ref
  fetch_snapshot(ref, projection) → WorkSourceSnapshot
  search(query, cursor) → [SourceCandidate]
}

BackupAdapter {
  put_snapshot(project, manifest, bytes) → BackupReceipt
  get_snapshot(project, snapshot_id) → RecoverySnapshot
  list_snapshots(project, cursor) → [SnapshotMetadata]
}

PortableStoreAdapter {
  read_head(project) → PortableHead
  fetch_snapshot(project, head_hash) → RecoverySnapshot
  publish(project, expected_parent, active_snapshot) → PortableReceipt
  release_writer(project, expected_active_head, released_snapshot) → PortableReceipt
  acquire_writer(project, expected_released_head, active_manifest) → PortableReceipt
  recover_writer(project, expected_head, recovery_intent, active_manifest) → PortableReceipt
  validate_writer(project, writer_instance_id, writer_epoch) → WriterValidation
}

PublicationAdapter {
  capabilities()
  publish_report(target, report, idempotency_key) → Receipt
  publish_work?(target, projection, idempotency_key) → Receipt
}
```

`WorkSourceSnapshot` is backend-neutral: ref, selected title/body/status/owner,
captured time, source revision, canonical URL, payload hash, plus bounded
`raw{}` extension data. It is immutable evidence for one explicit import.
`BackupAdapter` stores immutable, verified recovery snapshots.
`PortableStoreAdapter` transfers a canonical working snapshot between hosts
under parent-head compare-and-swap and refuses divergence. It is single-writer
handoff, not live synchronization. A later `Sync` backend provides concurrent
cross-host coordination; none is intake/publication.

Portable payloads include shared canonical objects, the work graph, feed
ordering, schemas, and permitted evidence references. They exclude live work
claims, resource leases, control sessions/grants, delivery progress, and
agent-private scratch. Restore invalidates old execution authority: unfinished
prior-host claims are recoverable and leases must be reacquired. A configured
cadence and clean-session flush create durable receipts; `doctor` surfaces
remote head, lag, and degraded pushes.

Project movement is stronger than a routine push. `portable release`
checkpoints/exits local sessions, clears live authority, advances a writer
epoch, CAS-publishes a released manifest, and makes the old store
mutation-read-only. `portable acquire` restores that exact head and
CAS-publishes a new active writer instance/epoch before allowing mutation.
Crash takeover is attributed recovery; it is not silent lock inheritance.
Acquire never overwrites a destination with an unpushed local tail. Portable
startup/resume and a bounded cadence validate only the remote head/epoch before
mutation; mismatch or unavailability makes the store read-only. This metadata
check is authority validation, not remote work-state retrieval.

Portable export must include the transitive shared-state closure needed to
rebuild work/readiness/policy/context/acceptance/completion. A disallowed
executable object fails release. Provenance-only excluded targets use
separately hashed `ExclusionStub` records, and excluded non-semantic feed
payloads use typed placeholders that preserve dense positions. Existing
canonical objects are pass-or-exclude, never rewritten under their old hash.
The manifest commits projection coverage and export policy; `doctor` reports
both, and acquire rejects a policy mismatch. If even stub metadata is not
allowed, the filtered result is backup/export only, not `portable`.

The reference personal Git transport uses a plumbing ref such as
`refs/engram/<project-id>/<scope>`, not a branch or working-tree file. The port
also supports private repositories and internal object storage. A shared code
remote is not the organization-scale default because repo readership, ref
lifecycle, and in-flight work privacy are different concerns.

## The boundary

Engram owns the host-local work item after import. The source snapshot records
what was injected and why; it does not grant the external system live write
authority over local priority, dependency, claim, evidence, or completion.
A refresh is an explicit new snapshot and proposed local revision, never a
polling mirror or last-writer-wins update.

Outbound publication has its own target, authority check, frozen payload,
idempotency key, and receipt. Intake and publication are independent: local
work may use neither, one, or both. Task notes, handoffs, readiness, evidence,
and report sections remain views of one Engram working set.

## DummyPublicationAdapter (V1)

The dummy exercises the *exact* production contract with no external side
effects:

- accepts an explicit target, a frozen report, and an idempotency key;
- writes and returns a **deterministic local receipt**;
- supports retry/idempotency tests — same key + same payload returns the
  original receipt; same key + different payload is a conflict.

Because the finalization pipeline proves this neutral contract, a real adapter
later swaps in without changing work or report semantics. `DummyTrackerAdapter`
implements this publication role, not external tracker ownership.

## Portable durability and deferred integrations

Beads snapshot import/export and portable sequential handoff are the first
compatibility/durability targets. Producing Engram's own work-graph file is
core — the designed [work-graph snapshot](work-graph-snapshot.md) — while
storing it off-host remains `BackupAdapter` and `PortableStoreAdapter` work.
A configured portable target carries durable replication authority, so
scheduled pushes do not need a model to approve each
transition; configuring or changing that disclosure boundary is a user/policy
decision. Real publication, comments, link-backs, concurrent sync, and tracker
transitions remain later adapter capabilities. Automatic webhook/poll
mirroring is not planned; an explicit refresh may fetch another source
snapshot. Publication actions still require user/policy authority and durable
idempotency receipts. See the [roadmap](../roadmap.md).
