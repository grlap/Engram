# Execution Pipeline

> Normative reference: [spec §9](../spec.md#9-local-work-reports--external-systems).
> Related briefs: [external adapters](tracker-adapter.md),
> [behavioral control plane](behavioral-control-plane.md),
> [local work system](local-work-system.md),
> [local tasks & reports](local-tasks-and-reports.md),
> [context packets](context-packets.md), and
> [security & trust](security-and-trust.md).

This brief is a map, not a new capability. It names the layers between an
external ticket and a published report, says which layer owns which decision,
and fixes where Engram's boundary sits in that chain. Everything Engram-side
here is either shipped or specified in the linked briefs, and the Status
section says which is which; the upstream layers are external, optional, and
separately owned.

## Status

Shipped in this repository today: the local work graph (`WorkItem`,
`RootExecution`, `WorkRun`, fenced claims, decomposition under planning
authority), the host-private turn channel (`session_bind` → `turn_evaluate`
→ `turn_begin` → `turn_checkpoint`) — still bound to legacy tasks rather
than natively to a `RootExecution`/`WorkRun` — bounded context delivery, work
evidence capture, `CompletionSeal`, the `WorkSourceSnapshot` type and its
admission path, and a side-effect-free dummy publication adapter that proves
the idempotency contract.

Specified but not yet shipped: a `WorkSourceAdapter` intake port; native
host-control binding to `root_execution_id` / `work_id` / `run_id` (the host
actor context carries `run_id: None` today); fenced report assembly
(`ReportAssemblyClaim`, `report_ready`) and durable publication intents; real
external adapters; capability requirements on work; an environment
fingerprint on the seal. Per-project required assurance is now selectable at
bootstrap and through an attributed, immutable policy update. Table rows and
diagram nodes marked **planned** are exactly the remaining items.

The upstream layers — intake/enrichment, planning, capability and context
assignment — belong to an external, separately named intake system that is
not in this repository. TermAl is the first hosting environment under
integration through its host adapter; which hosting-environment adapter lands
first is a product decision, not a fact of this brief. Revisit triggers for
the deferred items are recorded in the [roadmap](../roadmap.md).

## One sentence

Engram **supports** execution: before each turn it evaluates whether and on
what basis an agent may act, and after each turn it records the host-supplied
checkpoint and explicitly captured evidence. It does not execute, and it does
not infer execution facts. The hosting environment executes; the intake
system proposes work inputs and supplies evidence snapshots; Engram admits
and owns local work; an explicitly authorized publication adapter reports a
frozen artifact back.

## The layers

| Layer | Owns | What lands in Engram | Mutable after landing? |
| --- | --- | --- | --- |
| **Intake / enrich** (external) | fetching the ticket; gathering evidence — bug reports, linked tickets, production logs; redaction; tiering into pinned / index / on-demand | an immutable `WorkSourceSnapshot` plus an evidence bundle | no — a refreshed ticket is a new snapshot and a visible delta |
| **Planning** (external) | goal, acceptance criteria, approach, sufficiency check (“can repro and acceptance be derived at all?”) | the root `WorkItem` with acceptance; the plan as a *proposal* | yes, under optimistic revision control and bounded planning authority; while a live claim exists, planning is claim-bound |
| **Capability & context assignment** (external, coarse pass) | which capabilities the whole item requires; which context tier is pinned | capability requirements on the item (**planned**); the context packet to deliver | yes |
| **Engram** | work graph, turn admission, exact bounded delivery, claims and leases, evidence, seals, report freeze (**planned**) | — | — |
| **Hosting environment** (TermAl under integration) | agent runtime, workspace (worktree or sandbox), tools and skills, credentials, resource limits; enforcing Engram's decisions | `session_bind` with assurance, mediated effects, and a capability-map revision (a full capability map and native `WorkRun` binding are **planned**); evidence from execution | — |
| **Decomposition** (admitted by Engram) | splitting the root once the code is visible, under the holder's claim-bound planning authority or a bounded planning delegation | child `WorkItem`s and their `WorkRun`s, registered in the existing `RootExecution`; each run is separately claimable, owns a dense run feed, and receives a seal on completion | under Engram admission |
| **Capability & context assignment** (fine pass, under a claim; **planned**) | what each run needs delivered | per-run context packets (packets carry `work_id` and feed heads today, not `run_id`) | yes, per run |
| **Publication** (adapter) | returning the report to the ticket source | a frozen report, a publication intent with an idempotency key, a receipt (**planned** beyond the dummy contract) | no |

## The flow

Within Engram and the host-control path, dashed nodes are **planned** and
solid nodes ship today; solid nodes in the external layers describe upstream
responsibilities, not repository features.

```mermaid
flowchart TD
    classDef planned stroke-dasharray: 6 4,stroke-width:2px;

    subgraph EXT["External ticket source"]
        T["Ticket + bug reports + logs"]
        R["Ticket updated through a real adapter"]
    end

    subgraph UP["Intake system (external, separately named)"]
        I["Intake / enrich<br/>gather · redact · tier"]
        P["Planning<br/>goal · acceptance · sufficiency"]
        A1["Assignment, coarse<br/>pinned context"]
        CR["Capability requirements"]
        H["needs human"]
    end

    subgraph EN["Engram (admits, decides, remembers)"]
        S["WorkSourceSnapshot<br/>(immutable)"]
        W["Root WorkItem<br/>acceptance"]
        C["Context packet<br/>pinned / index / on-demand"]
        RE["Child WorkItems + WorkRuns<br/>in the existing RootExecution"]
        CT["Per turn: turn_evaluate → turn_begin → turn_checkpoint<br/>(legacy task-bound today)"]
        WB["Native WorkRun binding"]
        EV["Evidence · deltas · handoffs"]
        SE["CompletionSeal"]
        RA["ReportAssemblyClaim → report_ready"]
        PI["Publication intent + idempotency key"]
    end

    subgraph HO["Hosting environment (executes; TermAl under integration)"]
        B["session_bind<br/>assurance · effects · capability-map revision"]
        D["Decomposition admitted under planning authority"]
        A2["Assignment, fine<br/>per-run context"]
        X["Agent turns in a workspace"]
    end

    T --> I --> P
    P -- "insufficient" --> H
    P -- "sufficient" --> A1
    A1 -.-> CR
    I --> S
    A1 --> W
    A1 --> C
    S --> W
    W --> B
    CR -.-> B
    B -.-> WB -.-> D
    B --> D --> RE
    RE -.-> A2 -.-> X
    RE --> X
    C --> X
    X <--> CT
    X --> EV
    EV --> SE
    SE -.-> RA -.-> PI -.-> R

    class CR,WB,A2,RA,PI,R planned
```

The boundary between *before Engram* and *under Engram* runs between planning
and decomposition. Intake, planning, and the coarse assignment pass produce
immutable inputs and proposals, so they may run outside. Decomposition depends
on the code and is admitted by Engram under planning authority, so that every
child run is separately claimable, has its own dense feed, and is sealed on
completion — otherwise the upstream plan and the Engram work graph diverge as
soon as the agent starts.

## Rules that keep the layers honest

1. **Engram supports execution; it never executes.** Execution occurs in the
   hosting environment, while canonical claims, leases, grants, and every
   other form of live execution authority remain in Engram's SQLite store on
   the active host; the host enforces those decisions. The intake system
   hands Engram work inputs and evidence, not commands.
2. **Decomposition is admitted by Engram.** An upstream planner may propose a
   split; it remains a proposal until Engram admits it under the holder's
   claim-bound planning authority or a bounded planning delegation, creating
   child `WorkItem`s and `WorkRun`s that are separately claimable and sealed
   only on completion. A dry split done without seeing the code is rewritten
   by the executing agent, and then two task lists exist.
3. **The evidence bundle is tiered, never dumped.** Delivery is exact and
   bounded ([context packets](context-packets.md)), so intake must decide what
   is pinned (acceptance, repro, the decisive stack trace), what is indexed
   (the list of available evidence), and what is on demand (full logs).
4. **Redaction must happen at intake.** The canonical store is immutable and
   content-addressed; what enters it cannot be unwritten. Production logs
   must be redacted before they become a snapshot, not after. Until a real
   redactor is installed, V1's visibly labeled no-op
   ([security & trust](security-and-trust.md)) must not be treated as
   protection or compliance assurance.
5. **Skills are host capabilities, not Engram objects.** Planned work records
   will carry capability requirements; today `session_bind` records only a
   `capability_map_revision`, not a capability map, and Engram does not
   enforce requirement matching. When matching ships, Engram makes the
   admission decision and the host selects the concrete skill when it creates
   the session.
6. **Intake is not an orchestrator.** A changed ticket is a new snapshot and a
   visible delta that the agent or a human decides to rebase onto; never a
   polling mirror or a last-writer-wins update of local work state.
7. **Insufficiency means a human, not an execution.** If repro steps and
   acceptance cannot be derived from the ticket and its evidence, the root
   item is created blocked for a human decision rather than dispatched. The
   largest risk in this pipeline is not missing automation; it is thin
   context turned into a confident wrong fix.
8. **The hosting environment is evidence, not just an executor.** Executions
   of the same work may differ because of environment and exact source state.
   The seal should eventually cite an environment fingerprint — image or
   toolchain, workspace base revision, the capability map declared at bind —
   so a report says not only what was done but in what. This is planned; see
   below.

## Two assignment passes

In the target pipeline, assignment is done twice, not once. The coarse pass happens before
decomposition, on the root: which capabilities the whole item requires and
which context is pinned. That is what decides which hosting environment may
take the item at all. The fine pass happens after decomposition, per run,
under the claim: what each child run needs delivered. Doing it once before
decomposition can only produce the coarse pass, because the runs do not exist
yet. The fine pass is planned: it needs native run binding and per-run
packets.

## Target acceptance sequence

1. One ticket source, one-way import: a `WorkSourceSnapshot` and a root
   `WorkItem` with acceptance and a tiered evidence packet, created by hand
   from a real ticket.
2. Execution through TermAl under the shipped control channel, with
   decomposition admitted under the agent's claim. Prerequisites for this
   step to be honest rather than shadowed: native `WorkRun` control binding.
   The project policy can already require `advisory` for a shadow host and
   later advance to `turn_gated` without rewriting prior policy history.
3. `CompletionSeal`, then the report path. Until fenced report assembly and
   a real side-effecting adapter exist, this step exercises the dummy receipt
   and does not claim the ticket was updated.
4. Only after that loop has closed by hand several times: automated evidence
   gathering, then the sufficiency check, then capability matching.

## Not yet shipped

- **`WorkSourceAdapter` intake port.** The `WorkSourceSnapshot` type and its
  admission path exist; the port trait does not.
- **Native host-control binding** to `root_execution_id` / `work_id` /
  `run_id`. `session_bind` and the grant basis are legacy task-bound; the host
  actor context carries `run_id: None`.
- **Fenced report assembly and durable publication intents**
  (`ReportAssemblyClaim`, `report_ready`). Only `CompletionSeal` and the
  dummy adapter's idempotency contract exist.
- **Capability requirements on work items** matched against the host's
  capability map at bind. Today `session_bind` carries only
  `capability_map_revision`; work items have no requirement field and no
  matcher exists.
- **Environment fingerprint as required seal evidence.** Today
  `CompletionSeal` binds the run-feed cut, child seals or waivers,
  contributions, acceptance, evidence, and a zero-linked-state
  `CompletionDrainAttestation`; the environment is not a required evidence
  kind.
- **Automated evidence gathering and the sufficiency check** live in the
  external intake system, not in this repository.
- **A real publication adapter.** V1 proves the contract against the dummy.
