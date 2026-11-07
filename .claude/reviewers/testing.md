# Testing Review

Focus on behavioral contracts, deterministic failure cases, and meaningful
cross-platform coverage.

## Check

- New behavior has a test that fails without it; assertions verify exact state
  or output rather than merely “does not throw.”
- Report as Medium, for deletion, any test that reads a Markdown file and
  asserts that it contains given phrases, table rows or markers, and any test
  that only pins text or mirrors the implementation without verifying
  observable behavior, so that every change must be edited twice. Never ask
  for such a test. General-purpose checks that process Markdown uniformly,
  such as link validation or review input fingerprinting, are not such tests
  (Greg, 2026-11-07, relayed by Engram::Asistant in mailbox 43498a6e #490,
  #495 and #498: 'Poprawic reviewers aby nigdy to nie wróciło.').
- Canonical JSON tests cover key ordering and malformed input; identity tests
  preserve minted ids independently of bytes, and fingerprint tests compare
  content without treating a fingerprint as a record id or corruption check.
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
- Context tests cover pinned-budget overflow failing closed.
- Tests avoid timing, global-state, order, ambient network, and machine-specific
  assumptions.
- Do not use arbitrary sleeps, retries, quarantine, or timeout inflation to
  conceal nondeterminism.

Missing functionality that is explicitly deferred is not a test gap. Report
freeze and publication are deferred; do not demand retaining disconnected
barrier/report types or dummy-adapter tests as shipped coverage.
