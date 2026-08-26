# Tracker Adapter

> Normative reference: [spec §9](../spec.md#9-tasks-reports--ticketing).
> Related briefs: [local tasks & reports](local-tasks-and-reports.md).

The external ticketing system at work is proprietary, and V1 ships **no
proprietary integration**. Instead, the core defines a vendor-neutral
`Tracker` port — no Jira-shaped (or any-vendor-shaped) types anywhere in the
core — and V1 proves the entire contract against a dummy implementation.

## The port

```
Tracker {
  capabilities()                    // what this backend supports
  normalize_ref(text) → Ref         // "ABC-123", URL, … → canonical ref
  get(ref, field_projection) → TicketDTO
  search(query, cursor) → [TicketDTO]
  fingerprint(ref) → SourceRevision // snapshot hash for provenance
  publish_report(ref, report, idempotency_key) → Receipt
                                    // durable, idempotent; capability-gated
}
```

`TicketDTO` is backend-neutral: ref, title, body, status, owner, updated_at,
source_revision, canonical_url, plus `raw{}` extension data.

## The boundary

The tracker owns the organizational work item. Engram owns the local working
memory around it and the finalized report it publishes back. **Tickets are
never mirrored into memory** — a local task stores an `external_ref`, nothing
more. When a memory derives from mutable ticket state it stores a minimal
immutable `source_snapshot` (revision fingerprint, captured-at, excerpt hash)
so the claim's basis stays reproducible after the ticket changes — evidence
attached to one claim, not a synced copy.

This seam prevents double entry: the tracker remains the backlog of record;
Engram is the execution-time workspace bound by reference. Task notes,
handoffs, and report sections are views of one Engram working set rather than
independently maintained tracker state.

## DummyTrackerAdapter (V1)

The dummy exercises the *exact* production contract with no external side
effects:

- accepts a projected ticket ref, a frozen report, and an idempotency key;
- writes and returns a **deterministic local receipt**;
- supports retry/idempotency tests — same key + same payload returns the
  original receipt; same key + different payload is a conflict.

Because the finalization pipeline
([local tasks & reports](local-tasks-and-reports.md)) is fully proven against
this contract, the proprietary adapter later swaps in with no changes above
the port.

## Deferred

Real publication via the proprietary adapter, comments and link-backs,
transitions (if ever authorized), webhook/poll checkpoints with
reconciliation. All outbound actions require explicit user or policy
authorization and durable idempotency receipts. See the
[roadmap](../roadmap.md).
