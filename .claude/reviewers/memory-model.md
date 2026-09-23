# Memory Model Review

Focus on typed memory semantics and context delivery.

## Check

- Kind, authority, and delivery remain orthogonal; defaults may derive from
  them but overrides retain a reason.
- Versions are immutable and linked through parent/supersedes relationships.
- No last-writer-wins shortcut between versions.
- Pinned-budget overflow fails before agent action. Scope proximity must not
  silently override authority.
- `valid_until` and `review_by` have different meanings. Stale is not the same
  as expired or retracted.
- Agent-generated hard/firm claims remain proposed unless an authorized policy
  promotes them.
- Episode decay does not erase required provenance prematurely.
- Retrieval reasons and omission must be explainable under a bounded packet.

Do not require embeddings where exact ids, scope, and FTS satisfy the contract.
