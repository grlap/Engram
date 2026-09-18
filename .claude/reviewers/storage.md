# Storage and Integrity Review

Focus on canonical objects, exact-current SQLite definitions, transactions,
and explicitly rebuildable indexes.

## Check

- A record's id is a random UUID minted when it is stored, and a link holds
  that id. Nothing derives an id from bytes, so flag a read that re-derives one
  or compares one against content. Ids stored before this rule are 64 hex
  digits and stay valid beside minted 32-digit ids.
- A SHA-256 over RFC 8785 UTF-8 canonical JSON is a content fingerprint, used
  only where content is compared: idempotency intents, snapshot bodies, build
  identity. It is never an id, a link, or a corruption check — SQLite guards
  the bytes on disk.
- No digest is pinned in source or tests; reference state is derived at runtime
  from the same writer being checked.
- Established stores whose markers or durable shapes differ from the running
  build are refused before mutation; only explicitly rebuildable projections
  may be recreated from verified retained state.
- Immutable rows cannot be silently updated. Reinserting the same record under
  its own id is idempotent; different bytes under a taken id is a hard failure.
- Multi-step writes that define one domain transition are atomic.
- Concurrent local processes use WAL/busy handling deliberately; claims are
  lease/CAS operations and exact idempotent retries return the original result.
- Event cursors are monotonic ordering positions, not content identities;
  packet hashes are content identities, not delta cursors.
- Safety-relevant mutable transitions remain auditable through immutable events;
  exact-current operational tables are restored from a verified backup.
- SQLite foreign keys and required uniqueness constraints are enabled.
- Only declared indexes, triggers, and FTS content are repaired in place;
  heads, status, ordering, authority, and idempotency state are never rebuilt.
- Schema changes are made in place, guarded by the generic different-build
  refusal, and every marker stays 1 until release. A pre-change store is
  refused rather than interpreted, and moves over by a whole-store JSON export
  and import that carries its ids unchanged.
- Backups/exports are not confused with distributed sync or guaranteed erasure.
- SQL uses parameters; paths and database creation cannot escape caller scope.

Flag a check that presents a content fingerprint as if it were a record's
identity, and a claim that recomputing one detects corruption.
