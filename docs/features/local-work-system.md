# Local Work System

> Normative references: [spec §2.6](../spec.md#26-local-work-graph--execution)
> and [spec §9](../spec.md#9-local-work-reports--external-systems).
> Related briefs: [Execution Observability](execution-observability.md),
> [local tasks & reports](local-tasks-and-reports.md),
> [tracker adapter](tracker-adapter.md),
> [CLI & MCP](cli-and-mcp.md),
> [atomic work plans](atomic-work-plan.md),
> [write policy & review](write-policy-and-review.md),
> [acceptance-evaluation lifecycle](acceptance-evaluation-lifecycle.md),
> [security & trust](security-and-trust.md), and
> [execution pipeline](execution-pipeline.md).

Engram's target is a first-class, host-local graph of work. A user or model can open
work directly, split it into smaller units, express prerequisites, find ready
work, claim and hand off it, attach evidence, and complete it without any
external tracker. This is the local system of record for execution, not a
cache of Beads, GitHub, Jira, or another backlog.

See the [agent pair workflow proposal](../agent-pair-workflow.md) for a pilot of coordinator, implementer, and independent reviewer responsibilities.

External systems are optional at every boundary:

```text
                         optional immutable intake
 human / model prompt  ─────────────────────────────┐
 Beads / GitHub / Jira ─ snapshot + provenance ────┤
                                                    ▼
┌──────────────────────────────────────────────────────────────────┐
│ Engram core                                                      │── optional publication
│ local work graph · memory · coordination · host observations     │
│ evidence · report/finalization · canonical local store           │◄─► optional backup/portable/sync
└──────────────────────────────┬───────────────────────────────────┘
                               │ local work-session protocol
                               ▼
                     Host work session
                       ├── TermAl adapter
                       ├── generic CLI wrapper
                       ├── native runtime adapters
                       └── custom-agent library
```

An imported item becomes local work with an immutable source snapshot. A
later refresh is another explicit import event, never silent two-way
synchronization. A locally created item needs no external reference. A
completed item needs no publication target. SQLite is the canonical source of
truth in local-only mode. Optional backup, sequential portability, or later
concurrent synchronization increases durability/capability but never becomes
a precondition for valid local execution.

This index keeps the original section links. Follow a topic link for the complete contract.

## Product boundary

Continue in [Product boundary](local-work-model.md#product-boundary).

## Durable concepts

Continue in [Durable concepts](local-work-model.md#durable-concepts).

### Work item

Continue in [Work item](local-work-model.md#work-item).

### Root execution and work run

Continue in [Root execution and work run](local-work-model.md#root-execution-and-work-run).

### Work source snapshot

Continue in [Work source snapshot](local-work-model.md#work-source-snapshot).

### Work claims

Continue in [Work claims](local-work-model.md#work-claims).

### Completion seal and report assembly claim

Continue in [Completion seal and report assembly claim](local-work-model.md#completion-seal-and-report-assembly-claim).

## Lifecycle and derived readiness

Continue in [Lifecycle and derived readiness](local-work-model.md#lifecycle-and-derived-readiness).

### State diagrams

Continue in [State diagrams](local-work-model.md#state-diagrams).

## Graph invariants

Continue in [Graph invariants](local-work-model.md#graph-invariants).

## Agent-native protocol

Continue in [Agent-native protocol](local-work-protocol.md#agent-native-protocol).

### Gates, prerequisites, supersession, and project memories

Continue in [Gates, prerequisites, supersession, and project memories](local-work-protocol.md#gates-prerequisites-supersession-and-project-memories).

## Audited waivers and model autonomy

Continue in [Audited waivers and model autonomy](local-work-protocol.md#audited-waivers-and-model-autonomy).

## Execution Observability integration

Continue in [Execution Observability integration](local-work-integrations.md#execution-observability-integration).

## Optional external intake, storage, and publication

Continue in [Optional external intake, storage, and publication](local-work-integrations.md#optional-external-intake-storage-and-publication).

## Durability modes and integrity

Continue in [Durability modes and integrity](local-work-integrations.md#durability-modes-and-integrity).

## Tracker replacement and interoperability

Continue in [Tracker replacement and interoperability](local-work-integrations.md#tracker-replacement-and-interoperability).

## Delivery boundary

Continue in [Delivery boundary](local-work-integrations.md#delivery-boundary).

## Decisions still needed

Continue in [Decisions still needed](local-work-integrations.md#decisions-still-needed).
