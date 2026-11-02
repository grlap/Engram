# Feature Briefs

Per-feature design briefs for Engram. Each brief explains one pillar and
links back to the normative [specification](../spec.md); where a brief and
the spec diverge, the spec wins. Cross-link briefs both ways when one
references another.

## V1 pillars

| Brief | One line |
| --- | --- |
| [Execution Observability](execution-observability.md) | Host feature, project-level ON/OFF: the work session, named source roots, host observations of execution, source and tests, and registered verification; turn gating was removed |
| [Typed memory model](typed-memory-model.md) | Kind × authority × delivery axes over immutable versions under minted ids |
| [Historical context packet format](context-packets.md) | Retained serialized history; packet construction and generic task-memory capture are removed |
| [Write policy & review](write-policy-and-review.md) | Origin × authority promotion matrix; distillation proposes, never writes; review lifecycle |
| [Local work system](local-work-system.md) | First-class local work graph, claims, typed verification/environment evidence, policy-selected immutable obligation rules, and exact completion seals |
| [Atomic work plans](atomic-work-plan.md) | Shipped host/operator admission of a bounded new work forest and prerequisites in one transaction, with complete key-to-ref retry recovery; no new agent word or MCP tool |
| [Acceptance evaluation](acceptance-evaluation.md) | Temporarily off until Execution Observability is implemented. Agreed contract for host-evaluated, core-enforced per-criterion acceptance before completion: modes, policy, immutable evaluation record, freshness, refusal causes, and the boundary matrix tests trace to |
| [Acceptance-evaluation lifecycle](acceptance-evaluation-lifecycle.md) | Design review of admission and completion owners, historical failure transitions, and the shared host check-and-cut guarantee |
| [Local tasks & reports](local-tasks-and-reports.md) | Root execution, single-executor child runs, handoffs, completion seals, fenced report assembly, and optional receipted publication |
| [SQLite store](sqlite-store.md) | Local append-only canonical store; recovery snapshots; optional agent-merged Git task exchange; deferred cross-host coordination |
| [Git task synchronization](git-task-sync.md) | Selected design: task text on the code branch; Git exchange and reconciliation remain deferred |
| [Task planning text](task-planning-text.md) | Shipped typed deterministic planning directory export/import into an empty graph, with exact repeat no-op |
| [Work-graph snapshot](work-graph-snapshot.md) | Shipped deterministic save/load of the agent-visible work graph, inert history, and keyed project memories |
| [Off-host backup](off-host-backup.md) | Design: two copy kinds, adapter receipts, a host-triggered push, and the exact rule for reporting `local_backed_up` with its off-host assurance |
| [External adapters](tracker-adapter.md) | Optional snapshot intake, backup/portable storage, and separately authorized publication |
| [Source intake](source-intake.md) | Explicit file preview/apply, authored local work, and immutable source-change notices |
| [Execution pipeline](execution-pipeline.md) | Layer map from external ticket to report, including shipped WorkRun-bound environment evidence, obligations, and completion gating |
| [Security & trust](security-and-trust.md) | Asserted runtime identity with assurance levels; sensitivity labels; redaction; purge realism |
| [CLI & MCP](cli-and-mcp.md) | Shipped agent-facing local-work tools and the host-private work-session channel over one core |
| [Host readiness](host-readiness.md) | Scoped read-only enablement checks, separate from the explicit full-store doctor audit |

## Conventions

- New feature briefs live in this directory as `kebab-case.md`.
- Every brief opens with a blockquote naming its normative spec section and
  related briefs.
- Deferred capabilities are documented where they belong but clearly marked
  deferred, with the trigger for revisiting recorded in the
  [roadmap](../roadmap.md).
