# Storage and Integrity Review

Focus on canonical objects, exact-current SQLite definitions, transactions,
and explicitly rebuildable indexes.

## Check

- Hashes are computed from RFC 8785 UTF-8 canonical JSON excluding any
  self-referential hash field.
- No digest is pinned in source or tests; reference state is derived at runtime
  from the same writer being checked.
- Reads verify canonical bytes and the stored digest before activation.
- Established stores whose markers or durable shapes differ from the running
  build are refused before mutation; only explicitly rebuildable projections
  may be recreated from verified retained state.
- Immutable rows cannot be silently updated. Exact reinsert is idempotent;
  same-hash/different-bytes is a hard failure.
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
- Schema changes bump current markers and refuse pre-change stores rather than
  interpreting or rewriting their payloads.
- Backups/exports are not confused with distributed sync or guaranteed erasure.
- SQL uses parameters; paths and database creation cannot escape caller scope.

Flag integrity checks that report a hash derived from corrupted bytes as if it
were the expected stored identity.
