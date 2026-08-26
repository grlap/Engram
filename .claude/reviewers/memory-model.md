# Memory Model Review

Focus on typed memory semantics, contradiction handling, and context delivery.

## Check

- Kind, authority, and delivery remain orthogonal; defaults may derive from
  them but overrides retain a reason.
- Versions are immutable and linked through parent/supersedes relationships.
- Multiple unresolved heads become contested; no last-writer-wins shortcut.
- Applicable hard/firm pinned contradictions and pinned-budget overflow fail
  before agent action. Scope proximity must not silently override authority.
- `valid_until` and `review_by` have different meanings. Stale is not the same
  as expired or retracted.
- Agent-generated hard/firm claims remain proposed unless an authorized policy
  promotes them.
- Episode decay does not erase required provenance prematurely.
- Retrieval reasons and omission must be explainable under a bounded packet.

Do not require embeddings where exact ids, scope, and FTS satisfy the contract.
