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
  task_changes # ordered immutable-object feed; monotonic local cursor
  task_claims  # current lease projection; transitions are immutable events
  derived.*    # heads, status, FTS5, usage counters — rebuildable cache
  meta         # store schema version; guards old clients
```

## Canonical-bytes contract

Objects serialize as RFC 8785 (JCS) canonical JSON, UTF-8. An object's id is
the SHA-256 of its canonical bytes (hash field excluded); the storage key is
that hash; hashes are verified at read time so `engram doctor` distinguishes
corruption from formatting drift. Unknown schema versions are retained but
not activated; migrations mint new objects, never rewrite old ones. The
contract is substrate-neutral — it is what keeps the deferred Git backend a
drop-in and gives reports stable provenance hashes.

## Derived tables are disposable

Heads, status, full-text search (FTS5), and usage counters are rebuilt
deterministically from `objects` via `engram rebuild-index`. They are never
truth and never exported.

## Durability

WAL mode, bounded busy timeouts, short transactions, atomic claims/CAS,
local backup, and deterministic JSONL export. Export is
interchange only — never canonical storage, sync transport, or backup of
record. Safe defaults exclude `restricted` and `secret-ref` records unless
explicitly widened.

## Deferred: Git object store for cross-host sync

The designed team backend — a dedicated Git repository of append-only
`objects/<sha256>.json` files with set-union merges, contested-on-concurrency
semantics, and tombstones — is **deferred, not rejected**. Migration is
exporting rows to files behind the same `Store`/`Sync` ports. One of its
rules applies from day one because it cannot be retrofitted: sensitive values
never enter any shared history — vault references only. See
[spec §3.2](../spec.md#32-deferred-git-object-store-for-team-sync) and the
[roadmap](../roadmap.md).
