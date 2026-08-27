# Security & Trust

> Normative reference: [spec §7](../spec.md#7-audit-security--compliance).
> Related briefs: [write policy & review](write-policy-and-review.md),
> [sqlite store](sqlite-store.md),
> [behavioral control plane](behavioral-control-plane.md), and
> [execution pipeline](execution-pipeline.md).

Engram is designed for a work deployment, so audit and security properties
are V1 requirements — but the trust model is deliberately honest about what
V1 does and does not assure.

## Asserted identity, stated as such

In V1, actor and authority context arrive from the proprietary runtime: a
text instruction supplied through the tools and skills in use. Engram records
it — `actor_id`, `actor_kind` (human / agent / system), originating
run/session, source/tool/skill metadata when available — together with an
**assurance level** saying how the identity was established. This is
*asserted* instruction/authority context, **not cryptographic identity**, and
no document in this repository may claim otherwise. No SSO/LDAP in V1.

If compliance-grade attribution ever becomes a deployment promise, a trusted
write gateway or `Signer`-backed signatures must ship with that deployment —
the port alone is not attestation.

Control assurance is separate from identity assurance. `advisory` means the
agent can bypass Engram; `turn_gated` means the host mediates every model turn;
`action_gated` means it also mediates every declared material capability. A
host declaration is still asserted context unless a trusted gateway attests
it, and no deployment may claim action gating while an unmediated write,
shell, or network path remains available.

The target `engram doctor` must print the versioned per-tool mediation map,
including unmapped write-capable host tools. The shipped alpha instead reports
its fixed capability envelope and explicit unavailable fields for action
gating, organizational-authority mediation, and action-outcome tracking; it
does not claim a complete mediation map. A host-held session routing token
prevents accidental
cross-session request mix-ups, but neither the token nor a lease is a security
boundary: a caller that bypasses the host or can directly mutate the store is
outside Engram's V1 assurance.

## Immutable history is the audit log

Versions, approvals, retractions, evidence, and tombstones are append-only,
content-addressed objects. There is no separate audit channel to fall out of
sync with the data.

Control decisions and transitions also use immutable intent fingerprints and
receipts, while current sessions, grants, leases, and action state remain
rebuildable projections. Unknown safety-relevant policy/event schemas and
corrupt control storage fail closed for mutations and external effects. Clean
service unavailability may allow only policy-designated reversible local work
with durably spooled reconciliation debt; shared, external, and lifecycle
effects remain closed. Read-only diagnostics remain available where
disclosure policy allows.

## Sensitivity labels

`public` / `internal` / `restricted` / `secret-ref`, enforced at retrieval:
scope and sensitivity authorization run before anything enters a context
packet, an agent-facing local-work delta, or an off-host projection. Dense
local-work delivery retains unauthorized positions as typed omission markers
rather than returning the protected canonical payload. The planned
JSONL/recovery/portable exporters exclude `restricted` records unless
explicitly widened and export `secret-ref` only as its vault reference.
Agent-private scratch will never be portable. A configured remote is a
disclosure boundary even when it is the same provider as the code repository;
repository read access is not automatically work-state access.

The planned portable mode additionally requires complete executable
shared-state closure.
If a restricted shared object is needed to rebuild readiness, policy,
acceptance, completion, or a behavior-affecting feed, export must be authorized
for that object or release fails `portable_projection_incomplete`. A
provenance-only reference into excluded content may use a separately hashed
`ExclusionStub`; because a stub leaks the target's existence, kind, and hash,
the remote must be authorized for that metadata. If it is not, the result is a
marked-truncated backup/export, not an activatable portable store.

## Redaction: the real control

A pluggable `Redactor` port (DLP / secret scanning) runs on **every write**
and fails closed where policy demands. Secrets and PII are stored as vault
references, never as remembered values.

The Redactor may transform a candidate before canonical bytes and identity are
minted. Projection of an existing content-addressed object is pass-or-exclude,
never rewrite-under-the-old-hash. A sanitized derivative must be a new
canonical object with explicit provenance; portable feed closure still uses
the original position plus an exclusion stub/placeholder where permitted.

The agent work protocol applies a separate authorized presentation projection.
`work_next` verifies each canonical source object, then emits a typed compact
summary beside the original dense position and object hash. The hash binds the
canonical source, not the summary. Restricted or out-of-focus memory is a typed
omission at the same position; no body or structured value is delivered.
Body-free memory indexes and bounded history tails direct an authorized caller
to hash-addressed on-demand reads. The host-internal tentative delivery cursor
is never exposed, and no cursor is staged for a changes-free query.

**V1 status:** no redaction backend is selected. The shipped implementation
is a **visibly labeled no-op for development**—surfaced in `engram doctor`
output—and implies no compliance assurance whatsoever. Portable replication
must report that state before its first push and apply the configured
sensitivity allowlist even when redaction is a no-op. Prevention matters
disproportionately here because persistence past a boundary (a portable
history, published report, or future shared store) cannot be reliably recalled.

## Purge realism

V1 purge is a logical tombstone. Physical erasure from the local SQLite store
is technically feasible but breaks the append-only contract and spans every
backup, portable head, and export, so it remains an exceptional, documented runbook with
preview and audit — not a CLI verb. Anything already published in a report
may live on in the external target's history regardless. See
[spec §6.5](../spec.md#65-forgetting-vs-purging).

## No raw transcripts

Raw session logs are not persisted by default; any ephemeral retention is
explicit, bounded, and audited.
