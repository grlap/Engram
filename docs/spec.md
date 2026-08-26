# Engram — Specification

**Draft 0.5 · 2026-08-23 · Multi-session-first working draft**
Authors: Fable::AgentMemory (Claude) · Codex::AgentMemory (Codex) — at Greg's
request. Decision provenance in [Appendix A](#appendix-a--decision-log).

This is the normative design document. The docs under
[`docs/features/`](features/README.md) explain; where they diverge, this
document wins.

---

## 1. Purpose & principles

Engram gives humans and coding agents a memory with two deliberate lifecycle
layers: **local working memory** while work is underway — constraints never
silently dropped, decisions that keep their provenance, findings retrievable
when relevant — and a **durable final report**, assembled and polished at task
completion and published to the organization's tracker.

It is a new standalone project for use in a work environment, with no
dependency on TermAl, Beads, or any one agent runtime. V1 is **local-first,
not single-session**: multiple concurrent sessions and worktrees share one
host-local project store. Coordination and decision memory live on the
machine doing the work, and the external ticketing system is the durable
cross-team publication boundary — reached only through explicit task
finalization (§9.5), never by mirroring Engram's local event stream.

The V1 product definition is deliberately narrow: **a local, concurrent
execution-memory service for multiple agent sessions, producing context,
coordination state, handoffs, and one frozen report — not a general knowledge
graph yet.**

### 1.1 Goals

- Local working memory that coordinates agents and outlives sessions: task
  state, decisions, evidence, constraints, and intermediate findings while
  work is underway.
- Same-host multi-session coordination with atomic ownership, an ordered
  change feed, explicit handoffs, and a finalization barrier.
- A polished final report per task, published to the external tracker under a
  durable receipt — the cross-team handoff artifact.
- Bounded, predictable context cost: memory never crowds out the work it is
  meant to inform.
- Audit-grade provenance: every assertion is attributed, immutable, and
  reproducible.
- Clean ports to external systems — ticketing first — with the core
  containing no vendor-shaped types, and team/service backends addable later
  without changing semantics.
- One capture action producing the task delta, handoff material, and report
  inputs instead of making an agent repeat the same status in several tools.

### 1.2 Non-goals

- Not the organizational tracker. Engram tracks *local operational tasks*
  while work is underway (§2.6), but the external ticketing system stays
  authoritative for the organizational work item; Engram publishes reports to
  it rather than competing with it (§9.1).
- Not a transcript archive. Raw session logs are not persisted by default
  (§7).
- Not a secrets store. Sensitive values are held as vault references, never
  as remembered plaintext (§7).
- Not a RAG framework over arbitrary documents. Engram stores curated,
  attributed claims, not crawled corpora.

### 1.3 Design principles

- **Typed, not a bag of strings.** A hard constraint, a design decision, and
  a session anecdote have different delivery requirements; the model encodes
  that (§2.2).
- **Append-only truth.** Records are immutable and content-addressed; state
  is derived. Nothing is edited in place, so history and audit come for free
  (§2.4, §3).
- **Budgeted delivery.** Injection operates under hard byte budgets with
  visible omission; the constraint tier fails closed rather than truncating
  silently (§4).
- **Local while working, explicit when publishing.** Working memory stays on
  the machine; nothing crosses the organizational boundary except a finalized
  report, published through an idempotent adapter under a receipt (§9.5).
- **One write, many views.** Engram is the execution-time working set bound
  to the tracker item; task deltas, handoffs, and final reports derive from
  that set. It must not become another status ledger (§8, §9).
- **Trust follows origin and authority.** Who asserted something, and how
  binding it claims to be, determine whether it activates immediately or
  awaits approval (§5).
- **Contradiction is a state, not a merge strategy.** Conflicting claims
  coexist visibly as *contested* until a human-attributable resolution
  supersedes them — never last-writer-wins (§6.3).
- **One core, many faces.** CLI, MCP server, and future service front the
  same core API, including packet construction, so delivery semantics cannot
  drift (§8).

## 2. Data model

### 2.1 Identity and versions

A **memory** is a stable identity (`mem-<hash>`, collision-resistant for
concurrent writers) plus an append-only chain of immutable **versions**. A
version is content-addressed (its id is the hash of its canonical
serialization) and names its parent version(s). Asserting a change creates a
new version; it never mutates history. A memory with multiple unsuperseded
head versions is *contested* (§6.3).

### 2.2 Classification axes

Classification is three orthogonal axes, not one enum. Delivery *defaults*
are derived from kind and authority, and can be overridden per memory — but
the default mapping is what tools and UI present, so the simple mental model
("constraints are always in context") holds unless someone deliberately
departs from it.

| Axis | Values | Meaning |
| --- | --- | --- |
| `kind` | `constraint` · `decision` · `convention` · `fact` · `preference` · `episode` | What species of claim this is. `decision` is first-class: valid until superseded, provenance-heavy. |
| `authority` | `hard` · `firm` · `soft` | How binding the claim is. Drives write policy (§5) and delivery defaults. |
| `delivery` | `pinned` · `index` · `on_demand` · `suppressed` | How it reaches an agent's context (§4). Derived by default, overridable with reason. |

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
enter peer packets. **Task scope** is the default working scope: decisions,
constraints, findings, evidence, status, and handoffs visible to every task
participant. **Project scope** holds reviewed knowledge that should outlive a
single task. The published ring is the frozen report, not another memory
scope.

A caller working on a task receives applicable project + task + its own agent
records. Scopes never shadow silently: pinned constraints from every
applicable scope are delivered, and cross-scope conflicts surface as
`contradicts` edges, not as overrides. Resolution is always explicit — scope
proximity never silently outranks authority, and an unresolved contradiction
between applicable pinned records blocks packet construction (§4.1).

V1 operates on one host with a stable project id shared across sessions and
worktrees. `org`/`global` and cross-host team scope activate with a shared
backend later (§3.2, §12).

### 2.4 Version schema

```
Version {
  version_id      // sha-256 of canonical serialization
  memory_id       // stable identity: mem-<hash>
  parents[]       // prior version ids; >1 = conflict resolution (§6.3)

  kind, authority, delivery      // §2.2 (delivery may be "derived")
  scope                          // §2.3

  title           // one line; powers the index tier (§4.2)
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
| `proposed` | Asserted but awaiting required approval (§5) — excluded from packets. |
| `active` | Approved (or auto-activated) head version. |
| `contested` | Multiple unsuperseded heads, or an unresolved `contradicts` edge. For applicable pinned records, blocks packet construction (§4.1). |
| `stale` | `review_by` passed — delivered with a warning; needs re-affirmation. |
| `expired` | `valid_until` passed — excluded unless explicitly requested. |
| `retracted` | Withdrawn by an attributed retraction event. |
| `tombstoned` | Forgotten: excluded everywhere, object retained so sync cannot resurrect it (§6.5). |

### 2.5 Edges

Typed, behavior-bearing links between memories: `supersedes` (lifecycle),
`contradicts` (marks both sides contested until resolved), `derived_from`
(compaction and distillation provenance), `relates_to` (navigation only).
Edges are objects too — attributed and immutable.

### 2.6 Local tasks & finalization

Engram tracks the operational task one or more sessions are executing on the
host. A task carries a `task_id`, stable `project_id`, an optional
`external_ref` to the organizational ticket (a reference, never a mirror),
its participants, its shared working-memory scope, an ordered event cursor,
and a finalization state:

```
active → finalization_pending → report_ready → publishing → published
                                     ↑              |
                                     └── failure ───┘
```

- Finalizing distills the task's working memory into a report (§9.5).
  Reaching `report_ready` **freezes it**: the report is persisted as an
  immutable object with a `report_hash`, bound to a durable publication
  idempotency key.
- An adapter failure returns the task to `report_ready` — recording the last
  error and attempt metadata — *never* back into distillation. Every retry
  sends the exact same frozen report bytes under the same key; same key with
  a different payload is a conflict.
- Revising the report after a failure creates a *superseding report version*
  with a *new* publication intent and idempotency key; the old intent is
  never mutated or reused.
- A task is never marked `published` without an adapter receipt.

While a task is active, its working memory is the operational source of truth
for the agents on it.

**Ownership is an atomic lease, not a convention.** Claim, renewal, handoff,
release, and force-release use compare-and-swap revisions and idempotency
keys. A lease has an expiry and heartbeat policy so a dead session cannot
hold work forever; force-release is explicit and audited. Ownership
transitions append immutable task events even though the current lease is a
mutable coordination projection.

**Finalization is a barrier.** Every expected participant submits a report
contribution and marks ready. The coordinator may freeze only after all are
ready, or after explicitly waiving a missing participant with an attributed
reason. This prevents one session from freezing a report while a peer is
still producing evidence. Deterministic assembly buckets task memories and
participant contributions into the report contract (§9.5); an agent polishes
that single draft before it is frozen.

## 3. Storage & sync

### 3.1 V1 canonical store: local SQLite, append-only

V1's canonical store is a **local SQLite database** holding the same
immutable, content-addressed objects the model defines (§2.4): versions,
events, edges, and evidence as append-only rows keyed by their content hash,
written transactionally. Append-only is a semantic contract enforced by the
core, not a hope — nothing updates or deletes object rows except the purge
runbook (§6.5).

Local means one host, not one process. A stable `project_id` selects one
host-local database regardless of checkout or isolated worktree. Concurrent
CLI and MCP clients use WAL mode, a bounded busy timeout, short transactions,
and atomic compare-and-swap operations for coordination projections.
Deployments must never derive store identity solely from the current working
directory, because that would silently fork memory across worktrees.

Immutable task events also feed a monotonically increasing local cursor.
Current leases and indexes are mutable projections, but their transitions are
auditable through those events. The cursor orders peer deltas; it is not an
object identity and does not cross stores as a global sequence number.

```
engram.db
  objects      // content-addressed rows: versions, events, edges, evidence — write-once
  derived.*    // heads, status, FTS5, usage counters — rebuildable, never truth
  meta         // store schema version; guards old clients
```

Derived tables are a cache rebuilt deterministically from `objects`
(`engram rebuild-index`); `engram doctor` verifies hashes and index
freshness. Durability is transactions plus local backup and JSONL export — no
distributed merge semantics are needed while the store is single-machine.

#### 3.1.1 Canonical-bytes contract

Content addressing is only as interoperable as its byte-level definition, so
the contract is part of the spec, not an implementation detail:

- Objects serialize as **RFC 8785 (JCS) canonical JSON**, UTF-8.
- `version_id` = SHA-256 over the canonical bytes, with the hash field itself
  excluded; the object's storage key — SQLite row key today, filename in a
  Git backend (§3.2) — is that hash.
- Hashes are **verified at read time**, so `engram doctor` distinguishes
  corruption from formatting drift.
- Every object carries a schema version. Objects with an *unknown* schema
  version are retained but not activated — a newer teammate's records never
  break an older client, and never silently apply through it either.
- Migrations mint new objects; they never rewrite old ones.

Without this contract, two implementations could mint different hashes for
semantically identical records, and integrity checking would be impossible to
define. It holds regardless of substrate — which is what keeps the deferred
backend below a drop-in.

### 3.2 Deferred: Git object store for team sync

**Deferred, not rejected.** Draft 0.2's canonical team backend — a dedicated
Git repository of append-only objects (`objects/<sha256>.json`), set-union
merges that never conflict, concurrent heads surfacing as *contested* (§6.3),
tombstones preventing resurrection, offline-first with no server to run —
solves a cross-host shared-memory problem V1 deliberately does not start
with. Same-host concurrent sessions are already a V1 requirement. The
design is recorded here as the intended team backend. Because objects are
already content-addressed under the same bytes contract (§3.1.1), migration
is exporting rows to files and pointing the `Store`/`Sync` ports at the new
backend; no domain semantics change. One of its rules applies from day one
regardless, because it is nearly impossible to retrofit: sensitive values
never enter any shared history — vault references only (§7).

### 3.3 Ports

Domain semantics bind to interfaces, not backends: `Store` (append / get /
list-heads), `Index` (rebuild / search), `Sync` (fetch / push / verify —
dormant in V1), `Tracker` (§9.2), `Redactor` (§7), `Signer` (optional, §7).
V1 ships `SqliteStore` and `DummyTrackerAdapter`; the Git backend (§3.2) or a
Postgres-backed service later implements the same `Store`/`Sync` contract
with nothing above the ports changing.

**Interchange:** deterministic JSONL export exists for viewers and migration
— it is never canonical storage, sync transport, or backup.

## 4. Context packets & retrieval

A **context packet** is the unit of delivery: the block of memory an agent
receives at session start or on request. Packet construction is a first-class
core API used identically by every interface (§8), and every packet is
reproducible: it has a content hash, and `engram context explain <packet>`
shows exactly what was included, omitted, and why.

Every packet also carries the task event cursor observed during construction.
The hash answers “what exact content did I receive?”; the cursor answers “what
changed after that?” `engram context delta --since <cursor>` returns the
ordered peer-visible changes without rebuilding the whole packet. A runtime
may use its own mailbox or notification mechanism as a doorbell, but Engram's
durable change feed is authoritative. Engram is not a chat bus.

### 4.1 Rung 1 — pinned, complete or error

All *active* pinned memories in the caller's applicable scopes, verbatim, under a
hard budget.

> **Fail closed.** The pinned tier is never silently truncated *and never
> self-contradictory*. Packet construction fails *before the agent acts* in
> two cases: the pinned tier cannot fit its budget, or an unresolved
> contradiction stands between two applicable hard/firm pinned records —
> delivering both would ask the model to improvise policy precedence. The
> error names the records needing merge, demotion, or resolution (§6.3).
> Soft-authority conflicts are delivered, flagged contested. A dropped
> convention is a nuisance; a dropped or ambiguous "never do X" is an
> incident.

### 4.2 Rung 2 — the index tier

Titles only — `id · kind · title`, one line each — for every active
index-delivered memory in scope. This is what fixes "agents don't know what
they don't know": fifty memories cost fifty lines, and the agent knows what
it can pull. Capped by budget with ranked eviction (§4.4) and an **omission
manifest**: counts and reasons for anything excluded, so absence is visible.

### 4.3 Rung 3 — on demand

`show <id>`, `history <id>`, and `search <query>` over FTS5. Exact identifier
matches always outrank fuzzy matches. Embeddings are a later, optional
addition — good titles give most of semantic retrieval's value for none of
its machinery.

### 4.4 Budgets and ranking

| Tier | Default budget | On overflow |
| --- | --- | --- |
| Pinned (rung 1) | 4 KiB | Fail closed (§4.1) |
| Index (rung 2) | 8 KiB | Ranked eviction + omission manifest |
| Whole packet | 12 KiB (≈3k tokens) | Hard cap |

Defaults are per-deployment configuration, to be tuned against the evaluation
harness (§10). Non-pinned ranking, in order: scope proximity → exact
identifier match → FTS relevance → authority → confidence →
`last_verified`/validity → prior usefulness → byte cost. Deliberately **no
recency boost** for constraints and decisions: an old, recently-verified
decision outranks a new, unverified one.

Every packet item carries *why it was retrieved* and its evidence pointers,
so an agent can cite — and a human can audit — the chain from context back to
source.

Every packet includes a one-line count of proposed and stale items. Review
pressure is visible in the normal workflow rather than hidden behind a
command nobody remembers to run.

## 5. Write path

Anyone — human or agent — can *assert*; whether an assertion activates
immediately or awaits approval depends on **origin and authority**. Every
promotion is its own attributed audit event. Writes pass through the
`Redactor` port before persistence (§7).

The common capture path is `engram note <prose>` / `memory_note`. It infers
kind, authority, delivery, and scope from the active task plus asserted host
context, returns the inference in its receipt, and asks only when genuinely
ambiguous. The explicit `assert` surface remains for callers that need exact
control. Inference never bypasses the activation matrix below.

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
  heads visible, both flagged in packets.
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
> + vacuum), but it still spans every backup and JSONL export and it breaks
> the append-only contract — so it remains an exceptional, documented runbook
> with preview and audit, not a CLI verb. Two boundaries stay effectively
> irreversible regardless: anything already *published* in a report (§9.5)
> lives on in the external tracker's history, and a future Git-backed team
> store (§3.2) retains history across clones, reflogs, and host backups —
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
  No backend is selected for V1: the shipped implementation is a *visibly
  labeled no-op for development* — surfaced in `engram doctor` output,
  implying no compliance assurance whatsoever.
- **No raw transcript persistence** by default; any ephemeral retention is
  explicit, bounded, and audited.
- **Safe export defaults:** JSONL export excludes restricted and secret-ref
  records unless explicitly widened.
- **Signing is policy, not a dependency:** a `Signer` port supports signed
  objects and signed Git commits where a deployment requires cryptographic
  attestation. Baseline v1 runs without it — at asserted-identity assurance;
  deployments that promise more must deploy more.

## 8. Interfaces

One core library owns the object model, derived state, and — critically —
packet construction. The CLI and the MCP server are thin faces over it; a
future service is a third. No interface reimplements delivery logic.

### 8.1 CLI

```
# write path
engram note "..." [--task <id>]  # infer defaults; return classification receipt
engram assert   --kind decision --authority firm --scope project:x \
                --title "..." [--body-file ...] [--ref jira:ABC-123]
engram approve <id>         engram retract <id>        engram forget <id>

# read path
engram show <id>            engram history <id>        engram search <query>
engram context build [--scope ... --budget ...]
engram context explain <packet-id>
engram context delta --task <id> --since <cursor>

# curation & ops
engram review               engram conflicts           engram compact --dry-run
engram sync                 engram doctor              engram rebuild-index
engram export --jsonl       # purge: exceptional runbook, not a CLI verb (§6.5)

# tasks & reports (§2.6, §9.5)
engram task start [--ref <external-ref>]    engram task status
engram task claim <id>                      engram task handoff <id> --to <session>
engram task contribute <id>                 engram task ready <id>
engram task finalize <task-id>              # barrier → assemble → polish → report_ready
engram report show <task-id>                engram report publish <task-id>  # idempotent, receipted

# ticketing (§9)
engram ticket get <ref>     engram ticket search <query>
```

### 8.2 MCP server

Tools mirror the CLI over the same core: `memory_context`, `memory_delta`,
`memory_note`, `memory_search`, `memory_show`, `memory_history`,
`memory_assert` (policy-gated per §5), `memory_review_queue`, `task_start`,
`task_claim`, `task_handoff`, `task_contribute`, `task_ready`,
`task_finalize`, `report_publish`, `ticket_get`, `ticket_search`.

Host integration has two small responsibilities: inject `memory_context` at
session start, then request `memory_delta` after a host notification or before
the next work turn. The runtime owns wake-up delivery; Engram owns the state
and cursor. No host-specific dependency enters the core.

## 9. Tasks, reports & ticketing

### 9.1 The boundary

> **Division of labor.** The tracker owns the organizational work item.
> Engram owns the local working memory around it — and the finalized report
> it publishes back. Tickets are never mirrored into memory (a local task
> stores an `external_ref`, nothing more); memories reference tickets and may
> snapshot the minimum needed for reproducible provenance. The external
> system is the durable cross-team publication boundary, not a replica of
> Engram's local event stream.

The core contains no Jira-shaped types: everything vendor-specific lives
behind the `Tracker` port in per-backend adapters.

### 9.2 The Tracker port

```
Tracker {
  capabilities()                    // what this backend supports
  normalize_ref(text) → Ref         // "ABC-123", URL, … → canonical ref
  get(ref, field_projection) → TicketDTO
  search(query, cursor) → [TicketDTO]
  fingerprint(ref) → SourceRevision // snapshot hash for provenance (§9.3)
  publish_report(ref, report, idempotency_key) → Receipt
                                    // durable, idempotent; capability-gated (§9.5)
}
// TicketDTO (backend-neutral): ref, title, body, status, owner,
// updated_at, source_revision, canonical_url, raw{} extension data
```

### 9.3 Provenance across a mutable source

When a memory derives from ticket state, it stores the ref *plus* a minimal
immutable `source_snapshot` (revision fingerprint, captured-at, relevant
excerpt hash) — so the claim's basis remains reproducible after the ticket
changes or is deleted. This is snapshot-for-provenance, not mirroring: the
snapshot is evidence attached to one claim, not a synced copy of the ticket.

### 9.4 Adapters & phasing

- **V1 — `DummyTrackerAdapter`:** the tracker at work is proprietary, and no
  proprietary integration ships in V1. Instead, a dummy adapter exercises the
  *exact* production contract with no external side effects: it accepts a
  projected ticket ref, a report, and an idempotency key; writes and returns
  a deterministic local receipt; and supports retry/idempotency tests. The
  real adapter later swaps in behind an already-proven contract.
- **V1 read surface:** `normalize_ref` / `get` / `search` for on-demand
  context enrichment and distillation evidence — served by the dummy in V1.
  No mirroring, no webhook cache.
- **Later — the proprietary adapter and wider outbound:** real publication,
  comments and link-backs, transitions if ever authorized; incremental
  webhook/poll checkpoints with reconciliation. Outbound actions require
  explicit user or policy authorization and durable idempotency receipts.

### 9.5 Finalization & the report contract

Publication is driven by the task state machine (§2.6). Finalization first
opens the participant barrier: every expected session contributes and marks
ready, or an arbiter records an explicit waiver. Engram then deterministically
buckets task memories and contribution fragments into the report sections;
an agent polishes that single draft. Only then is it **frozen at
`report_ready`** — an immutable object whose `report_hash` is bound to the
publication idempotency key.

Publishing hands the frozen bytes to the `Tracker` adapter under that key;
failures retry the identical payload, and only an adapter receipt marks the
task `published`. A revised report is a superseding version under a new intent
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
8. Provenance: local task id, memory/version and participant-contribution
   hashes, timestamps, actors, assurance, and any finalization waivers

The report **cites the local memory and version IDs it was distilled from**,
so the local record can always explain the published artifact.

> **A reporting boundary, not truth promotion.** Facts and constraints
> discovered during a task appear as report sections and as *promotion
> candidates* — they never silently become global or org memory. Promotion
> back into durable memory follows the ordinary write policy (§5).

**Retention after publication** is configurable. Default: retain the local
task and its source memories until a confirmed publication receipt plus a
grace period, then compact to the final report, a provenance index, and
tombstones. Unpublished source state is never auto-deleted.

## 10. Evaluation & telemetry

Retrieval quality is part of the product, not later polish. V1 ships with an
evaluation harness:

- **Golden query set** maintained alongside the memory corpus: task
  descriptions → the memories that should surface.
- **Metrics:** constraint coverage (must be 100% — this one is an invariant,
  not a target), precision@k and recall@k for the index tier, wrong/stale
  memory rate, cited-in-answer rate, packet bytes. **Precision before
  recall:** a plausible wrong memory silently corrupts work; a visible miss
  just prompts a search.
- **Retrieval decision logs** without sensitive bodies: packet hash,
  candidate ids considered and included, scores and reasons, budget
  exclusions, and whether the agent used or cited each item. This is the data
  that turns budget defaults (§4.4) from guesses into tuned values.

## 11. Delivery plan

| Phase | Contents |
| --- | --- |
| **v1** | Rust core; stable project-id keyed host-local SQLite store (append-only canonical objects, WAL, multi-process access) with derived FTS5 tables; one-verb capture; context packets with budgets, fail-closed pinned tier, omission manifest, content hash, event cursor + peer delta, and visible review counts; write policy matrix and proposal/approval; supersede/contradict/contested; task-shared working memory with claims/leases, explicit handoff, participant contributions and finalization barrier; deterministic report assembly, polish/freeze, and `DummyTrackerAdapter` publication under idempotent receipts; audit attribution at asserted-runtime-context assurance; visibly labeled no-op Redactor; CLI + MCP over one core; safe JSONL export + local backup; fixture-level retrieval tests; `doctor` / `rebuild-index`. |
| **v1.x** | Session-end distillation into working memory (proposer + dedup); episodic compaction automation; post-publication retention compaction; budget tuning from retrieval logs. |
| **v2+** | Proprietary tracker adapter (real publication); Git object-store team backend (§3.2) with org/team scopes; optional embeddings; wider outbound ticketing (comments, link-backs, webhook checkpoints); real Redactor/DLP integration; Postgres/service `Store` backend; Signer-based attestation; envelope encryption for crypto-shredding. |

> **Scope discipline.** V1's riskiest cut is doing too much. Everything in v1
> serves one loop: bind a backlog item → coordinate concurrent local sessions
> under shared task memory → assemble and freeze one report → publish it →
> review promotion candidates. Cross-host sync, the real tracker integration,
> decay automation, and embeddings improve that loop; none close it.

## 12. Decisions

The Draft 0.2 open questions, resolved by Greg on 2026-08-23 (relayed via
Codex::AgentMemory):

| Question | Decision |
| --- | --- |
| Name | **Engram** — settled (binary: `engram`). |
| Implementation language | **Rust.** |
| Ticketing backend | Proprietary. V1 ships `DummyTrackerAdapter` against the real port (§9.4); no proprietary integration in V1. |
| Identity source | Proprietary runtime context: instruction/authority arrives as text through the tools and skills in use — asserted context, not cryptographic identity (§7). No SSO/LDAP in V1. |
| Memory-repo hosting / team scope | Not in V1 — start local. The Git team backend is deferred with its design preserved (§3.2). |
| Redaction backend | None selected. Port + safe defaults ship; the no-op development implementation is visibly labeled and implies no compliance assurance (§7). |
| Architecture refinement | Dual-layer model adopted: local working memory during execution; durable final report published at task finalization (§1, §2.6, §9.5). |
| Multi-session operating model | Normal in V1 on one host. Task memory is shared by default; agent scratch is private. Claims/leases, cursor deltas, participant contributions, and a finalization barrier are V1 primitives. |
| Product seam | The external tracker owns backlog; Engram owns the bound execution-time working set. One capture generates task delta, handoff, and report inputs. |

### Still open

- Default grace period for post-publication retention (§9.5) — pick during V1
  implementation.
- Trigger for reviving the team backend (§3.2) — revisit when coordination
  must cross machines. Multiple sessions on one host are already V1.
- Timing of the proprietary tracker adapter — when work authorizes real
  publication.

## Appendix A — Decision log

How this spec was produced: both authors designed independently against the
same brief, merged via a structured mailbox exchange, hardened in a review
round, and revised to Greg's decisions as they landed. Positions and
outcomes:

| Topic | Fable v1 | Codex position | Merged outcome |
| --- | --- | --- | --- |
| Typing | Four species, behavior bound to type | Orthogonal kind / authority / delivery axes; `decision` first-class | **Codex**, plus Fable's derived-default mapping so the simple mental model survives (§2.2) |
| Identity | Single record, edited via supersedes | Stable id + immutable content-addressed versions with parents | **Codex** (§2.1, §2.4) |
| Storage | SQLite canonical, JSONL export | Git object store canonical; SQLite as disposable derived index | **Codex** (Draft 0.2); superseded by Greg's local-first V1 (SQLite canonical, Git deferred, §3) |
| Retrieval shape | Three rungs, hard budgets, titles index | Agree; add fail-closed pinned tier, omission manifest, packet hash/explain | **Fable** structure + **Codex** hardening (§4) |
| Write policy | Trust follows priority; distillation proposes only | Refine by origin × authority; evidence-backed agent writes activate | **Both** — merged matrix (§5) |
| Lifecycle clocks | Single `review_by` | Separate `review_by` from `valid_until`; stale ≠ expired | **Codex** (§6.1) |
| Conflicts | Supersedes chains | Contested state, `contradicts` edges, multi-parent resolution; no LWW | **Codex** (§6.3) |
| Forgetting | Forget = delete | Tombstone vs. physical purge, both audited | **Codex** (§6.5) |
| Compliance | Provenance fields in schema | Full audit set: actor identity, Redactor port, sensitivity labels, vault refs, safe exports, optional Signer | **Codex** (§7) |
| Ticketing | Never mirror; adapter port; read-only first | Agree, but snapshot mutable sources for reproducible provenance | **Fable** boundary + **Codex** snapshots (§9) |
| Evaluation | — | Golden queries, precision-first metrics, retrieval decision logs, in v1 | **Codex** (§10) |
| Round-2 hardening | Draft 0.1 as adjudicated | Four clarifications: fail-closed pinned contradictions; canonical-bytes contract (JCS); identity assurance not overclaimed; purge realism vs. Git history | **Codex**, all four adopted (§4.1, §3.1.1, §7, §6.5) → Draft 0.2, ACKed by both authors |
| Greg's decisions | Round 3 — name Engram; Rust; proprietary tracker → `DummyTrackerAdapter` in V1; runtime-context identity, no SSO/LDAP; no DLP backend selected; no team scope in V1 (Greg, relayed via Codex::AgentMemory) | | Recorded (§12) → Draft 0.3 |
| Dual memory / report model | Greg: local working memory while work runs; polished final report published to the tracker at finalization. Codex elaboration: SQLite canonical in V1; Git store deferred, not rejected; finalization state machine; report contract with cited memory/version IDs; configurable post-publication retention | | Adopted (§1, §2.6, §3, §9.5) → Draft 0.3 |
| Round-4 correction | Failure transition returned to `finalization_pending` | Freeze the report at `report_ready`: immutable report + hash bound to the idempotency key; failure retries identical bytes, never re-enters distillation; revision = superseding version + new intent. Endorsed keeping §3.1.1/JCS in V1 | **Codex**, adopted (§2.6, §9.5) → Draft 0.4, final ACK by both authors |
| Round-5 product test | Greg asked whether the authors would actually use Engram and clarified that multi-session work is normal. Fable identified capture ceremony, double entry, claims, and peer visibility as adoption blockers. | Codex separated packet hash from ordered event cursor, added leases/recovery, finalization barrier, and stable project identity across worktrees. | **Both**, joint position confirmed: narrow Engram to concurrent execution memory with three visibility rings, one-write-many-views, deterministic report assembly, and the backlog/execution seam (§1–5, §8–9) → Draft 0.5 |

## Appendix B — Beads verdict

Both authors studied [Beads](https://github.com/gastownhall/beads) (Codex
read the memoryops, prime rendering, merge settlement, and compaction code;
Fable analyzed the docs and its observed behavior in production use). Shared
conclusion: don't clone it — borrow its operational discipline, replace its
memory model.

**Borrow:** an explicit canonical source of truth distinct from
export/interchange formats; local/offline-first operation;
collision-resistant ids for concurrent writers; task state kept separate from
persistent note memory; typed graph edges with behavior
(supersedes / duplicates / derived-from); immutable audit/change history;
dry-run preview before anything destructive; a session-start context packet
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
