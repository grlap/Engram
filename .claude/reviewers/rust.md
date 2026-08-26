# Rust Review

Focus on error handling, type safety, portability, and maintainable APIs.

## Check

- No `unwrap`, `expect`, `panic`, or `unreachable` in non-test recoverable
  paths without a documented invariant.
- Errors retain useful context and do not discard SQLite, serialization, I/O,
  adapter, or validation failures.
- Serde representations are stable and explicit; hashes and large identifiers
  are strings rather than lossy JSON numbers.
- Match statements make new lifecycle variants visible instead of hiding them
  behind broad wildcard arms.
- Avoid needless cloning on large report bodies/canonical bytes.
- Thread-shared state uses appropriate synchronization and never holds a lock
  around an external side effect.
- Public APIs express invalid states through types where practical.
- Paths, newlines, and executable behavior work on Windows, macOS, and Linux.

Do not flag small, clear clones or test-only `unwrap` calls.
