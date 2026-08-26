# Security & Trust

> Normative reference: [spec §7](../spec.md#7-audit-security--compliance).
> Related briefs: [write policy & review](write-policy-and-review.md),
> [sqlite store](sqlite-store.md).

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

## Immutable history is the audit log

Versions, approvals, retractions, evidence, and tombstones are append-only,
content-addressed objects. There is no separate audit channel to fall out of
sync with the data.

## Sensitivity labels

`public` / `internal` / `restricted` / `secret-ref`, enforced at retrieval:
scope and sensitivity authorization run before anything enters a context
packet. JSONL export excludes `restricted` and `secret-ref` records unless
explicitly widened.

## Redaction: the real control

A pluggable `Redactor` port (DLP / secret scanning) runs on **every write**
and fails closed where policy demands. Secrets and PII are stored as vault
references, never as remembered values.

**V1 status:** no redaction backend is selected. The shipped implementation
is a **visibly labeled no-op for development** — surfaced in `engram doctor`
output — and implies no compliance assurance whatsoever. Prevention matters
disproportionately here because persistence past a boundary (a published
report, a future shared history) cannot be reliably recalled.

## Purge realism

V1 purge is a logical tombstone. Physical erasure from the local SQLite store
is technically feasible but breaks the append-only contract and spans every
backup and export, so it remains an exceptional, documented runbook with
preview and audit — not a CLI verb. Anything already published in a report
lives on in the tracker's history regardless. See
[spec §6.5](../spec.md#65-forgetting-vs-purging).

## No raw transcripts

Raw session logs are not persisted by default; any ephemeral retention is
explicit, bounded, and audited.
