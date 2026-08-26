# Architecture Review

Focus on domain boundaries, dependency direction, and local-versus-published
state.

## Check

- Domain meaning must not depend on SQLite, CLI, MCP, or proprietary tracker
  types.
- Canonicalization owns bytes and identity only; storage owns transactions and
  retrieval; adapters own external side effects.
- Local task state may hold an external reference but must not become a shadow
  copy of the organizational ticket.
- A stable project identity must select one host-local store across sessions
  and worktrees; task memory is shared while agent scratch stays private.
- Durable level state/change history belongs to Engram; host mailboxes are
  edge notifications and must not become the authoritative task record.
- One captured record should drive deltas, handoffs, and report inputs rather
  than creating another independently maintained status ledger.
- Report generation and report publication remain separate transitions.
- Deferred team sync or proprietary behavior must enter through ports rather
  than leaking assumptions into V1 core records.
- New shared types should have contract comments explaining lifecycle,
  ordering, failure, and ownership invariants.
- Flag modules that mix unrelated concerns or make rebuildable projections
  authoritative.

Do not flag small modules merely because an alternate arrangement is possible.
