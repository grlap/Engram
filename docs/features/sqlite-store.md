# SQLite Store

> Normative reference: [spec §3](../spec.md#3-storage--sync). Related briefs:
> [typed memory model](typed-memory-model.md),
> [security & trust](security-and-trust.md).

V1's canonical store is a single local SQLite database holding immutable,
content-addressed objects — versions, events, edges, evidence — as
append-only rows keyed by their content hash, written transactionally.
Append-only is a contract enforced by the core: nothing updates or deletes
object rows except the exceptional purge runbook.

“Single local database” means one per stable project id on the host, not one
per checkout or session. All concurrent sessions and isolated worktrees for
that project resolve the same database. Deriving the store solely from the
current working directory is forbidden because it silently forks memory.
The tracked `.engram-project` file supplies that identity; the database lives
under `ENGRAM_HOME/projects/<project-id-hash>/engram.db`, outside the checkout.

```
engram.db
  objects      # content-addressed rows — write-once
  work_items / work_prerequisites / work_blockers # shipped work graph projections
  work_root_executions / work_runs / work_claims  # shipped execution projections
  work_handoff_offers / work_run_evidence / work_completion_seals
  work_authority_grants / work_authority_revocations # host-installed authority
  work_feed_heads / work_feed_entries # shipped typed dense project/root/run feeds
  work_operation_results # local-work idempotency receipts
  work_session_state # mutable ambient focus + processed project-feed cursor; never authority
  task_changes # dense task-local feed positions plus an internal global row sequence
  context_deliveries # target dense per-session delivery + exact source ranges
  task_claims  # legacy whole-task advisory claim; removed after work-claim migration
  control_observations # non-authoritative, hash-verified shadow decisions
  control_sessions     # durable host routing, phase, cursors, epochs
  control_connections  # current host-process generation; fences predecessors
  control_turn_results # idempotent enforced decisions
  control_turn_grants  # short-lived issued/begun/completed authority
  control_work_leases  # scoped live/released lease projection + fences
  control_operation_results # begin/checkpoint/lease retry receipts
  control_policy_operation_results # store-scoped operator-policy receipts
  report_assemblies / report_assembly_claims # target post-completion authority
  derived.*    # status, FTS5, usage counters — rebuildable cache
  meta         # store schema version; guards old clients
```

`control_observations` is the first observe/replay implementation slice. It
stores canonical input and decision bytes plus their hashes, binds an
idempotency key to one session/turn intent, and returns the original decision
after restart. The write transaction derives task state, task head, and
session participation from durable tables; `doctor` validates the input and
decision hashes and their redundant row bindings. That observation table
neither enters the canonical object graph nor grants a turn; authority is
issued only through the separate host-control projections below.

The host-private alpha adds `control_sessions`, `control_turn_results`,
`control_turn_grants`, `control_work_leases`, and
`control_operation_results`. Their intent and result payloads use canonical
bytes and hashes even though live grants and leases are operational rather
than durable memory. Acquisition/release emits a canonical
`work_lease_event`; a successful checkpoint emits a canonical
`turn_checkpoint_event`. Each projection and its task event commit together.
Expired leases stop covering new grants without erasing their historical fence.
Path resources are project-bound and normalized before they enter these rows;
cross-task rebind is rejected while an active lease remains. Replacement host
connections atomically rotate `control_connections`, making requests from a
still-live predecessor fail closed.
Store-scoped policy administration uses a separate
`control_policy_operation_results` table because operator updates do not belong
to an agent session. It hash-verifies the complete normalized intent and exact
receipt, and commits the receipt in the same transaction as policy activation.
The caller's wall-clock retry time is attribution rather than intent, so a
lost-response retry can replay the original receipt after restart even when
the original compare-and-swap hash is no longer the active head.
`doctor` verifies the redundant row bindings and canonical hashes for all of
these tiers. Context contents, pinned-safety evaluation, packet hash, and the
stamped task head are read in one immediate transaction before a grant is
persisted.

Host delivery reconstructs canonical task objects for an exact dense
`(confirmed_cursor, page_cursor]` range. Pages are capped by event and byte
budgets; partial pages carry deltas only and advance through recovery
turn begin/checkpoint, while the final page also binds the context packet.
Task events larger than the single-object delivery limit are rejected before
they enter the task feed.

`task_changes.task_cursor` is dense and local to one task; an internal
`sequence` is only a SQLite row identity. Stores created with the earlier
global-cursor schema fail open with an explicit export/rebind/reset error
because silently renumbering durable cursors could skip future delivery.
First-class work uses `work_feed_heads` and `work_feed_entries` to allocate a
typed dense `feed_kind + feed_id + position` for project, root-work, and
run-execution feeds in the same transaction as each
object/event. Context delivery still needs its separate dense
task cursor; its packet additionally stamps the focused project/root/run feed
heads plus monotonic project-visible and owner-private context revisions. Turn
begin rechecks that basis and rejects intervening work, project-memory, or
same-agent private-memory changes. Private revisions are keyed by project and
agent and never publish private object identities to shared feeds.
Exact work deltas and their staged/confirmed project cursor remain on the
six-operation work protocol in V1. The legacy global row id remains internal
and is never promoted into a work safety cursor.

## Canonical-bytes contract

Objects serialize as RFC 8785 (JCS) canonical JSON, UTF-8. An object's id is
the SHA-256 of its canonical bytes (hash field excluded); the storage key is
that hash; hashes are verified at read time so `engram doctor` distinguishes
corruption from formatting drift. Unknown schema versions are retained but
not activated; migrations mint new objects, never rewrite old ones. The
contract is substrate-neutral — it is what keeps the deferred Git backend a
drop-in and gives reports stable provenance hashes.

The first-class work schema currently advertises version 8. Open preflights
that metadata before any DDL, refuses future or unversioned non-empty work
schemas without mutation, and performs supported ALTER/backfill/version steps
inside one `BEGIN IMMEDIATE` transaction. Handoff backfills must match their
latest hash-verified work event before a projection can be activated.
Once both core and work schemas are current, open takes the read-only fast
path: it verifies required objects, columns, indexes, policy rows, and host
path identity without starting a write transaction. A current store can be
opened through a SQLite read-only connection; only actual migration or repair
needs the schema write lock. If a current-version state table is missing or
has the wrong SQLite object type, open refuses before any DDL rather than
recreating erased claims, feeds, authority, idempotency, or delivery state.
Declared indexes are rebuildable from retained table rows and may be recreated
transactionally; a uniqueness violation makes that repair roll back.

Version 5 binds every pending project-feed cursor and opaque acknowledgement
token to the exact canonical agent change page that was delivered. Replay
returns those stored bytes even if work focus or legacy-task binding changes.
Fresh staging is an exact compare-and-swap over the confirmed cursor, absence
of another pending page, focused work id, and bound legacy task. A losing
concurrent caller discards its local projection and returns the durable winning
page; if focus or task binding won first, it reprojects against that new basis.
Version 6 adds the redundant typed verification/environment evidence binding;
version 7 adds rebuildable obligation state; version 8 adds the verification→
environment reference and canonical environment-component projection; version
9 adds the nullable rule-set hash that binds each obligation projection to its
immutable definition. The v1–v8 upgrade runs atomically and current-version
open requires the V9 column rather than silently interpreting a partial
projection. `NULL` retains the stock V1 meaning for legacy definitions.
Upgrading an older store clears only an unacknowledged tentative page, because
versions 1-4 did not retain enough information to reconstruct that exact
projection; the confirmed cursor remains unchanged and the page is delivered
again under a new token.

Projection blobs have an explicit per-column convention. Authority grant and
revocation `*_json` columns contain canonical bytes because their binding
verifiers call `CanonicalObject::verify` directly. Mutable work/run/item/seal
projection blobs are semantic serde projections; their canonical source object
is stored in `objects`, and integrity checks compare decoded meaning plus the
bound object hash. Changing either convention requires changing its verifier in
the same migration.

## Derived tables are disposable

Status, full-text search (FTS5), and usage counters are disposable. Current
work projections are checked by `engram doctor` against canonical typed work
events: exact item/run/root/claim/handoff/blocker snapshots, prerequisite and
blocker-event bindings, evidence/run bindings, completion seals, dense feed
heads, typed feed membership, and cross-feed order. Canonical events plus feed
ordering remain the recovery basis; the operator rebuild command is delivered
with the recovery/portability slice rather than being silently implied here.

Ordinary lifecycle mutations validate the exact canonical item/run/root/claim,
handoff, authority, and relation basis they consume under the write lock. A
corrupt target projection therefore cannot be promoted into fresh canonical
history, while unrelated project history does not make every mutation slower.
The exhaustive reconstruction remains an operator `doctor`, migration, and
recovery check.

Completed `work_protocol_attempts` retain their request hash and exact bounded
caller-visible response, but discard the inferred basis. The 12 KiB protocol
ceiling keeps exact lost-response replay bounded without retaining unbounded
history or memory bodies.

## Durability modes

WAL mode, bounded busy timeouts, short transactions, and atomic claims/CAS
provide `local`: SQLite is the complete canonical local source of truth.
A deterministic work-graph recovery snapshot with manifest hashes and a
previewed, tested restore path may be copied to configured external storage to
provide `local_backed_up`. `portable` adds scheduled publication of that
canonical, human-readable working snapshot plus explicit sequential
handoff/restore under remote-head compare-and-swap. A later shared `Sync`
backend provides `synchronized`. External storage is optional and never
required on the hot execution path; `doctor` reports the actual mode, remote
head, recovery point, unpushed lag, degraded pushes, and writer assumption.

Portable restore rebuilds SQLite but never restores live authority. Work
claims, resource leases, control sessions/grants, delivery progress, and
agent-private scratch do not cross hosts; unfinished claim history becomes
recoverable and leases must be reacquired. Remote divergence refuses rather
than merging dense feed sequences. A manifest binds one consistent read cut,
parent head, feed heads, export policy, writer instance/state, and monotonic
writer epoch. Release publishes a CAS-protected `released` head and freezes
old-host mutation; acquire CAS-publishes the next active epoch before enabling
new-host writes. Acquire refuses to overwrite a nonempty destination unless it
is exactly at the expected head with no local tail. Portable startup/resume
and a bounded active cadence validate the remote writer epoch before mutation;
mismatch/unavailability makes the local store read-only. The recommended
personal Git transport is a dedicated
plumbing ref, never a branch or working-tree projection; an access-controlled
object store/service is the organization-scale substrate.

Projection integrity distinguishes three cases: included canonical content,
a separately hashed `ExclusionStub` for a provenance-only excluded target,
and a typed excluded-feed placeholder preserving an original dense position.
Executable shared-state references must resolve to included content; otherwise
release fails. Existing canonical bytes are passed or excluded, never rewritten
under their old hash. The manifest hashes inclusion/stub/placeholder coverage,
`doctor` reports it, and only complete executable closure qualifies as
`portable`. Export-policy mismatch blocks acquire.

Generic JSONL export remains interchange only and is not automatically a
backup of record. The recovery snapshot is a separate versioned contract with
referential/projection integrity checks. Safe defaults exclude `restricted`
and `secret-ref` records unless explicitly widened.

## Portable projection and deferred concurrent sync

V1 portable mode projects append-only `objects/<sha256>.json` plus a manifest,
work/event data, and feed ordering behind `PortableStoreAdapter`. It supports
one active host, explicit handoff/restore, and divergence refusal. Concurrent
set-union transfer, per-origin ordering, contested-on-concurrency semantics,
and cross-host claims remain **deferred, not rejected** behind `Sync`.
Sensitive values never enter any shared history—vault references only. See
[spec §3.2](../spec.md#32-optional-portable-replication),
[spec §3.3](../spec.md#33-deferred-concurrent-cross-host-sync), and the
[roadmap](../roadmap.md).
