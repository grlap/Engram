# Testing Review

Focus on behavioral contracts, deterministic failure cases, and meaningful
cross-platform coverage.

## Check

- New behavior has a test that fails without it; assertions verify exact state
  or output rather than merely “does not throw.”
- Canonical JSON/hash tests cover key ordering and noncanonical/corrupt input.
- Storage tests cover idempotent insert, immutable collision, transaction
  rollback, unknown schema handling, and index rebuild where implemented.
- When implementing the deferred publication capability, tests cover identical
  retry, same-key/different-payload conflict, missing receipt, adapter failure,
  and superseding report intent.
- State-machine tests reject invalid transitions. When implementing deferred
  report freeze/publication, they preserve frozen reports after publication
  failure and block report freeze for an unaccounted participant.
- Coordination tests cover two connections contending for one live lease,
  expiry/recovery, idempotent claim replay, and ordered deltas.
- Context tests cover budget overflow and hard/firm contradictions fail-closed.
- Tests avoid timing, global-state, order, ambient network, and machine-specific
  assumptions.
- Do not use arbitrary sleeps, retries, quarantine, or timeout inflation to
  conceal nondeterminism.

Missing functionality that is explicitly deferred is not a test gap. Report
freeze and publication are deferred; do not demand retaining disconnected
barrier/report types or dummy-adapter tests as shipped coverage.
