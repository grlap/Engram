# Storage and Integrity Review

Focus on canonical objects, SQLite transactions, migrations, and rebuildable
indexes.

## Check

- Hashes are computed from RFC 8785 UTF-8 canonical JSON excluding any
  self-referential hash field.
- Reads verify canonical bytes and the stored digest before activation.
- Unknown schema versions may be retained but not interpreted as active.
- Immutable rows cannot be silently updated. Exact reinsert is idempotent;
  same-hash/different-bytes is a hard failure.
- Multi-step writes that define one domain transition are atomic.
- Concurrent local processes use WAL/busy handling deliberately; claims are
  lease/CAS operations and exact idempotent retries return the original result.
- Event cursors are monotonic ordering positions, not content identities;
  packet hashes are content identities, not delta cursors.
- Mutable lease/index projections remain derivable from immutable task events.
- SQLite foreign keys and required uniqueness constraints are enabled.
- FTS, heads, status, and usage projections can be rebuilt from canonical rows.
- Migration logic creates new immutable objects when meaning changes rather
  than rewriting historical payloads.
- Backups/exports are not confused with distributed sync or guaranteed erasure.
- SQL uses parameters; paths and database creation cannot escape caller scope.

Flag integrity checks that report a hash derived from corrupted bytes as if it
were the expected stored identity.
