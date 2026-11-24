# Architecture Review

Focus on domain boundaries, dependency direction, and local-versus-published
state.

For a new mechanism or a significant behavior change, the item states the goal, the observed problem, the proposed behavior and its cost before code. The project coordinator chooses the design reviewers before code; they check necessity, simpler existing alternatives, real host behavior and possible damage to functionality. Do not require protection for hypothetical scenarios; a severity label is not evidence. Do not dismiss a concrete correctness defect merely because no incident has occurred. Small fixes use ordinary review only.

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
