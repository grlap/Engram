# Security & Trust

> Normative reference: [spec §7](../spec.md#7-audit-security--compliance).
> Related briefs: [write policy & review](write-policy-and-review.md),
> [sqlite store](sqlite-store.md),
> [behavioral control plane](behavioral-control-plane.md),
> [local work system](local-work-system.md), and
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

The work CLI and MCP binding may also carry optional `ENGRAM_ACTOR_CONTEXT`:
free text describing execution context,
for example `model=opus-4.1;reasoning=high`. It is retained in the existing
actor provenance and passes the same Redactor inspection as its containing
event, note, evidence, or project memory. It is attribution only and never
changes the `actor_id` principal used for assignment, `--mine`, handoff, or
authority checks. Agent projections render it parenthetically and omit it when
absent. Relative actor words hide principal identifiers, but the context is
whatever bounded text the host asserted and may itself identify its source.
Host mistakes do not refuse every word: each unsafe-control run becomes one
space, surrounding whitespace is trimmed, and the result is cut on a UTF-8
boundary to 256 bytes. Altered input receives an
`actor_context:normalized` provenance marker; an empty result is absent.
Context is excluded from agent-protocol attempts, gate-observation replay, and
project-memory replay identity: an identical retry through those surfaces
returns the originally attributed result. Core storage request hashes still
bind the complete asserted actor context supplied to that request.

The shell work surface does not turn missing host attribution into false
assurance. When `ENGRAM_ACTOR_ID` is absent it derives an actor from the first
nonblank conventional OS-user environment variable, or uses a synthetic
process actor if none exists. This is still asserted context, not an
authenticated OS identity. When `ENGRAM_SESSION_ID` is absent it creates one
stable id for that process. Durable actor provenance distinguishes
`defaulted:os_user_environment`, `defaulted:process_actor`, and
`defaulted:process_session`; injected values remain verbatim. A process-local
session id does not itself provide cross-command continuity between separate
CLI processes, so the CLI prints its generated id and an exact `--session-id`
reuse instruction on every such invocation.

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
durable operational projections. A new database receives the built-in policy
atomically; partial, missing, different-build, or corrupt control storage and
unknown safety-relevant policy/event schemas fail closed for every
ordinary store-open surface. `engram doctor --recover-policy` is the one
separately constrained exception: it opens the existing database read-only,
verifies the active selector plus every reachable policy, authority, and rule
set, and names invalid bindings with restore/inspection guidance. It returns
only a report, never a usable store handle; it cannot run MCP, control, work,
grants, schema initialization, or repair, and never selects or rewrites a
policy. Clean service unavailability may allow only
policy-designated reversible local work with durably spooled reconciliation
debt; shared, external, and lifecycle effects remain closed.

Ordinary open also refuses missing or malformed rebuildable indexes, triggers,
or FTS tables without DDL. Only the explicit
`engram doctor --repair-projections` operator path may recreate those declared
objects and repopulate FTS, after full durable-definition and policy preflight;
it never repairs canonical, authority, ordering, or idempotency state.

The active immutable policy also selects a canonical obligation-rule-set hash.
Checkpointing resolves that selection from the begun grant's frozen policy
epoch, and both the execution observation and each generated obligation retain
it. A later operator policy change cannot retroactively weaken or strengthen an
old execution fact. Unknown rule-set schemas or missing/corrupt selected
objects fail store open; V1's rule-set administrator is host/operator-only and
records asserted, redactor-inspected attribution rather than authenticated
identity.

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

Host-supplied environment components (toolchain, sandbox/image label,
workspace id, and capability-map revision) are asserted audit context rather
than attestation. Every textual component passes through the write-time
redactor and a 256-byte bound before canonicalization. The shipped development
redactor is deliberately a visible no-op, so hosts must never place credentials
or other secret values in these fields.

Project-memory reads and writes use the cooperative asserted project binding.
Before persistence, mutations validate a non-empty, consistent actor/session
binding. This is not authenticated identity and does not invent a separate
memory policy, grant token, revocation object, or validity timeout.

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
