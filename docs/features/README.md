# Feature Briefs

Per-feature design briefs for Engram. Each brief explains one pillar and
links back to the normative [specification](../spec.md); where a brief and
the spec diverge, the spec wins. Cross-link briefs both ways when one
references another.

## V1 pillars

| Brief | One line |
| --- | --- |
| [Typed memory model](typed-memory-model.md) | Kind × authority × delivery axes over immutable, content-addressed versions |
| [Context packets](context-packets.md) | Budgeted retrieval, reproducible hashes, ordered peer deltas, visible review pressure |
| [Write policy & review](write-policy-and-review.md) | Origin × authority promotion matrix; distillation proposes, never writes; review lifecycle |
| [Local tasks & reports](local-tasks-and-reports.md) | Claims/leases, handoffs, participant barrier, frozen reports, receipted publication |
| [SQLite store](sqlite-store.md) | Local append-only canonical store; canonical-bytes contract; deferred Git team backend |
| [Tracker adapter](tracker-adapter.md) | Vendor-neutral port; DummyTrackerAdapter proving the production contract in V1 |
| [Security & trust](security-and-trust.md) | Asserted runtime identity with assurance levels; sensitivity labels; redaction; purge realism |
| [CLI & MCP](cli-and-mcp.md) | One core, two thin faces; one-hook host integration |

## Conventions

- New feature briefs live in this directory as `kebab-case.md`.
- Every brief opens with a blockquote naming its normative spec section and
  related briefs.
- Deferred capabilities are documented where they belong but clearly marked
  deferred, with the trigger for revisiting recorded in the
  [roadmap](../roadmap.md).
