# Feature Briefs

Per-feature design briefs for Engram. Each brief explains one pillar and
links back to the normative [specification](../spec.md); where a brief and
the spec diverge, the spec wins. Cross-link briefs both ways when one
references another.

## V1 pillars

| Brief | One line |
| --- | --- |
| [Behavioral control plane](behavioral-control-plane.md) | Shipped host-private turn admission channel with exact bounded delivery, scoped leases, checkpoints, and restart recovery; enforcement depends on the embedding host and action gating remains planned |
| [Typed memory model](typed-memory-model.md) | Kind × authority × delivery axes over immutable, content-addressed versions |
| [Context packets](context-packets.md) | Budgeted retrieval, reproducible hashes, ordered peer deltas, visible review pressure |
| [Write policy & review](write-policy-and-review.md) | Origin × authority promotion matrix; distillation proposes, never writes; review lifecycle |
| [Local work system](local-work-system.md) | First-class local work graph, claims, typed verification/environment evidence, policy-selected immutable obligation rules, and exact completion seals |
| [Local tasks & reports](local-tasks-and-reports.md) | Root execution, single-executor child runs, scoped leases, handoffs, completion seals, fenced report assembly, and optional receipted publication |
| [SQLite store](sqlite-store.md) | Local append-only canonical store; recovery snapshots; sequential portability; deferred concurrent sync |
| [External adapters](tracker-adapter.md) | Optional snapshot intake, backup/portable storage, and separately authorized publication |
| [Execution pipeline](execution-pipeline.md) | Layer map from external ticket to report, including shipped WorkRun-bound environment evidence, obligations, and completion gating |
| [Security & trust](security-and-trust.md) | Asserted runtime identity with assurance levels; sensitivity labels; redaction; purge realism |
| [CLI & MCP](cli-and-mcp.md) | Shipped agent-facing local-work tools and host-private turn channel over one core; material action mediation remains planned |

## Conventions

- New feature briefs live in this directory as `kebab-case.md`.
- Every brief opens with a blockquote naming its normative spec section and
  related briefs.
- Deferred capabilities are documented where they belong but clearly marked
  deferred, with the trigger for revisiting recorded in the
  [roadmap](../roadmap.md).
