# Testing Review

Focus on behavioral contracts, deterministic failure cases, and meaningful
cross-platform coverage.

## Check

- New behavior has a test that fails without it; assertions verify exact state
  or output rather than merely “does not throw.”
- Canonical JSON/hash tests cover key ordering and noncanonical/corrupt input.
- Storage tests cover idempotent insert, immutable collision, transaction
  rollback, unknown schema handling, and index rebuild where implemented.
- Publication tests cover identical retry, same-key/different-payload conflict,
  missing receipt, adapter failure, and superseding report intent.
- State-machine tests reject invalid transitions and preserve frozen reports
  after publication failure.
- Coordination tests cover two connections contending for one live lease,
  expiry/recovery, idempotent claim replay, ordered deltas, and report freeze
  blocked by an unaccounted participant.
- Context tests cover budget overflow and hard/firm contradictions fail-closed.
- Tests avoid timing, global-state, order, ambient network, and machine-specific
  assumptions.
- Do not use arbitrary sleeps, retries, quarantine, or timeout inflation to
  conceal nondeterminism.

Missing functionality that is explicitly deferred is not a test gap.
