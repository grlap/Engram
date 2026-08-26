---
name: engram-repo
description: Work in the Engram repository when changing its typed memory model, local task lifecycle, canonical object storage, SQLite backend, report finalization, tracker adapters, CLI/MCP surfaces, or repository review system. Do not use for unrelated Rust projects.
---

# Engram Repository

Engram is a host-local concurrent execution-memory service for multiple agent
sessions. It keeps agent-private scratch and task-shared operational memory
local while agents work, then publishes one frozen report to the referenced
external task. Preserve that boundary in code, tests, docs, and commands.

## Read the Relevant Contract

- Start with `docs/architecture.md` for component and data-flow boundaries.
- Read `docs/features/typed-memory-model.md` for kinds, authority, delivery,
  visibility, versioning, and contradiction behavior.
- Read `docs/features/local-tasks-and-reports.md` when changing tasks, report generation,
  publication, retry, receipts, or retention.
- Read `docs/features/security-and-trust.md` for identity assurance, redaction, secrets, and
  irreversible publication constraints.

If a referenced document does not exist yet, use `AGENTS.md` as the active
contract and keep the change narrow.

## Hard Invariants

- Memory kind, authority, and delivery are orthogonal fields.
- Immutable versions supersede; they are never edited in place.
- Canonical object identity is SHA-256 over RFC 8785 UTF-8 JSON bytes.
- Unknown schema versions may be retained but are never activated.
- Applicable hard/firm pinned contradictions and pinned-budget overflow fail
  context assembly before an agent acts.
- Local tasks reference external tickets; they do not mirror ticket state.
- Local does not mean single-session: one stable project id resolves every
  session and worktree to the same host-local store.
- Agent scope is private; task scope is shared among participants and is the
  default for execution findings.
- Claims are atomic, idempotent leases with expiry and explicit handoff or
  audited recovery. Every transition emits an immutable task event.
- Packet hashes reproduce content; monotonic event cursors order peer deltas.
  Never substitute one for the other.
- Report freeze requires every expected participant contribution or an
  explicit attributed waiver.
- One capture must feed peer deltas, handoffs, and report assembly. Do not add
  a second status ledger beside the external backlog tracker.
- `report_ready` freezes report bytes, hash, and publication idempotency key.
  Failed publication returns to the same frozen report. A revision creates a
  superseding version and a new publication intent.
- No adapter receipt means the task is not published.
- Host-provided actor/authority text is asserted context unless a stronger
  assurance mechanism actually verified it.
- Do not persist secrets. The V1 redactor may be a visibly labeled no-op, but
  no code or documentation may imply that provides compliance assurance.

## Ownership Boundaries

- `domain`: substrate-neutral meaning and state transitions.
- `canonical`: serialization and content identity only.
- `storage`: SQLite transactions, migrations, immutable rows, integrity, and
  rebuildable indexes.
- `tracker`: backend-neutral capabilities, idempotency, and receipts.
- CLI/MCP front doors translate requests; they do not redefine domain rules.

Keep proprietary tracker types, authentication schemes, and organization
policy outside the core. Extend ports using neutral request/response records.

## Verification

Run the smallest focused test while iterating, then finish with:

```bash
cargo fmt --check
cargo check
cargo clippy --all-targets --all-features -- -D warnings
scripts/test-rust.sh
node --test scripts/review-freeze-fingerprint.test.mjs
node --test scripts/mcp-dogfood.test.mjs
node scripts/check-doc-links.mjs
```

Use `/review-changes` for the two-agent read-only review after the gates pass.
Do not commit, push, close beads, or sync remotes without explicit authority.
