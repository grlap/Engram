# Engram — Specification

**Draft 0.8 · 2026-08-26 · Local-work/control/portability working draft**
Authors: Fable::AgentMemory (Claude) · Codex::AgentMemory (Codex) — at Greg's
request. Decision provenance in [Appendix A](#appendix-a--decision-log).

This is the normative design document. The docs under
[`docs/features/`](features/README.md) explain; where they diverge, this
document wins.

---

## 1. Purpose & principles

Engram gives humans and coding agents a first-class local work system backed
by memory with two deliberate lifecycle layers: **local working memory** while work is underway — constraints never
silently dropped, decisions that keep their provenance, findings retrievable
when relevant — and a **durable final report**, assembled and polished at task
completion and optionally published through an external adapter.

It is a new standalone project for use in a work environment, with no
dependency on TermAl, Beads, or any one agent runtime. V1 is **local-first,
not single-session**: multiple concurrent sessions and worktrees share the
active host's project store. Coordination authority lives on that host;
optional `portable` mode moves the canonical shared projection sequentially
between hosts without transferring live authority. Work may originate locally
or from an explicit external snapshot. Publication is a separate optional
boundary reached only through explicit authorization (§9.5), never by
mirroring Engram's local event stream.

The V1 product definition is deliberately narrow: **a host-local work graph
and behavioral/coordination control plane for multiple agent sessions, backed
by typed execution memory and producing context, controlled handoffs,
evidence-gated completion, and optional frozen reports — not a general
knowledge graph or process scheduler.**

### 1.1 Goals

- A first-class host-local work graph: roots, bounded decomposition,
  prerequisites, derived readiness, assignment, claims, evidence, and
  acceptance-gated completion without an external tracker dependency.
- Local working memory that coordinates agents and outlives sessions: task
  state, decisions, evidence, constraints, and intermediate findings while
  work is underway.
- Same-host multi-session coordination with atomic ownership, an ordered
  change feed, explicit handoffs, and a finalization barrier.
- Optional sequential cross-machine portability with scheduled durable push,
  explicit handoff/restore, divergence refusal, and no live-authority transfer.
- Host-enforced turn admission and material-action authorization: context,
  peer-delta, lease, checkpoint, and finalization obligations are protocol
  preconditions rather than optional agent habits.
- An optional polished final report per completed root, published only under a
  separately authorized durable receipt.
- Bounded, predictable context cost: memory never crowds out the work it is
  meant to inform.
- Audit-grade provenance: every assertion is attributed, immutable, and
  reproducible.
- Clean optional ports to external systems — Beads migration first — with the core
  containing no vendor-shaped types, and team/service backends addable later
  without changing semantics.
- One capture action producing work/run deltas, handoff material, and report
  inputs instead of making an agent repeat the same status in several tools.

### 1.2 Non-goals

- Not a **concurrent** cross-host organizational planning service in V1.
  Engram is the active-host work source of truth in `local` mode and may be
  handed to the next host in `portable` mode; §3 reports durability and writer
  assumptions, never making external storage a precondition for local
  authority. Wider organization-wide commitments may still live in external
  systems and enter only as explicit snapshots (§9.1).
- Not a transcript archive. Raw session logs are not persisted by default
  (§7).
- Not a secrets store. Sensitive values are held as vault references, never
  as remembered plaintext (§7).
- Not a RAG framework over arbitrary documents. Engram stores curated,
  attributed claims, not crawled corpora.
- Not an agent process supervisor. Engram derives ready work and deterministic
  priority ordering; the host chooses models, prompts, and process lifecycle.
  Engram never starts, pauses, wakes, or stops a session (§2.7, §8).

### 1.3 Design principles

- **Typed, not a bag of strings.** A hard constraint, a design decision, and
  a session anecdote have different delivery requirements; the model encodes
  that (§2.2).
- **Append-only truth.** Records are immutable and content-addressed; state
  is derived. Nothing is edited in place, so history and audit come for free
  (§2.4, §3).
- **Budgeted delivery.** Injection operates under hard byte budgets with
  visible omission; the constraint tier fails closed rather than truncating
  silently (§4).
- **Local while working, explicit at every external boundary.** Local work and
  memory need no external system. Intake is an immutable snapshot; publication
  sends a frozen payload through an idempotent adapter under a receipt (§9.5).
- **One write, many views.** The local work graph and execution memory produce
  ready views, deltas, handoffs, evidence, and final reports. External adapters
  receive projections; they are not a second live ledger (§8, §9).
- **Durability is explicit, not implicit.** A host-local SQLite store is a
  valid local source of truth. Engram reports whether it is `local`,
  `local_backed_up`, sequentially `portable`, or concurrently `synchronized`;
  it claims off-host recovery only when a verified optional backend and
  restore path provide it (§3).
- **Trust follows origin and authority.** Who asserted something, and how
  binding it claims to be, determine whether it activates immediately or
  awaits approval (§5).
- **Contradiction is a state, not a merge strategy.** Conflicting claims
  coexist visibly as *contested* until a human-attributable resolution
  supersedes them — never last-writer-wins (§6.3).
- **One core, many faces.** CLI, MCP server, and future service front the
  same core API, including packet construction, so delivery semantics cannot
  drift (§8).
- **Control requires a reference monitor.** An agent-visible tool is
  advisory. Engram controls behavior only to the extent that a host mediates
  turns and declared material capabilities through its decisions (§2.7,
  §8.3).

## 2. Data model

### 2.1 Identity and versions

A **memory** is a stable identity (`mem-<hash>`, collision-resistant for
concurrent writers) plus an append-only chain of immutable **versions**. A
version is content-addressed (its id is the hash of its canonical
serialization) and names its parent version(s). Asserting a change creates a
new version; it never mutates history. A memory with multiple unsuperseded
head versions is *contested* (§6.3).

### 2.2 Classification axes

Classification is three orthogonal axes, not one enum. Delivery *defaults*
are derived from kind and authority, and can be overridden per memory — but
the default mapping is what tools and UI present, so the simple mental model
("constraints are always in context") holds unless someone deliberately
departs from it.

| Axis | Values | Meaning |
| --- | --- | --- |
| `kind` | `constraint` · `decision` · `convention` · `fact` · `preference` · `episode` | What species of claim this is. `decision` is first-class: valid until superseded, provenance-heavy. |
| `authority` | `hard` · `firm` · `soft` | How binding the claim is. Drives write policy (§5) and delivery defaults. |
| `delivery` | `pinned` · `index` · `on_demand` · `suppressed` | How it reaches an agent's context (§4). Derived by default, overridable with reason. |

Default delivery by kind × authority:

| Default delivery | hard | firm | soft |
| --- | --- | --- | --- |
| `constraint` | pinned | pinned | index |
| `decision` | pinned | index | index |
| `convention` / `fact` / `preference` | index | index | index |
| `episode` | — | on_demand | on_demand |

### 2.3 Scope and working-memory visibility

The two lifecycle layers contain three visibility rings. **Agent scope** is
private scratch: hypotheses, incomplete reasoning, and preferences that never
enter peer packets. **Task scope** is the default working scope: decisions,
constraints, findings, evidence, status, and handoffs visible to every task
participant. **Project scope** holds reviewed knowledge that should outlive
a single task, plus explicitly attributed active Episodes/notes admitted by
the existing episode exception. The published ring is the frozen report,
not another memory scope.

A caller working on a task receives applicable project + task + its own agent
records. Scopes never shadow silently: pinned constraints from every
applicable scope are delivered, and cross-scope conflicts surface as
`contradicts` edges, not as overrides. Resolution is always explicit — scope
proximity never silently outranks authority, and an unresolved contradiction
between applicable pinned records blocks packet construction (§4.1).

V1 execution authority operates on one active host with a stable project id
shared across sessions and worktrees. Sequential `portable` handoff may move
that local authority between hosts; `org`/`global` and concurrent cross-host
team scope activate with a shared backend later (§3.2–3.3, §12).

### 2.4 Version schema

```
Version {
  version_id      // sha-256 of canonical serialization
  memory_id       // stable identity: mem-<hash>
  project_key?    // safe permanent key for the constrained project-episode surface
  parents[]       // prior version ids; >1 = conflict resolution (§6.3)

  kind, authority, delivery      // §2.2 (delivery may be "derived")
  scope                          // §2.3

  title           // one line; powers the index tier (§4.2)
  body            // full prose; loaded on demand
  structured_value?              // optional machine-readable payload
  tags[]

  provenance {
    actor_id, actor_kind         // human | agent | system
    run_id?, session_id?         // originating agent run, if any
    chain[]                      // asserted-by / relayed-by / derived-from
    reason
  }
  evidence[]                     // refs to evidence objects (§3.1)
  refs[]                         // external: ticket refs, URLs, repo@rev
  source_snapshot?               // fingerprint of mutable source (§9.3)

  confidence      // 0..1
  sensitivity     // public | internal | restricted | secret-ref
  valid_from?, valid_until?      // factual validity window
  review_by?      // re-affirmation deadline (≠ validity, §6.1)
  last_verified?
  created_at
}
```

Alongside versions, the store holds append-only **event objects** —
`approve`, `retract`, `verify`, `tombstone`, `resolve` — each attributed like
a version. Status is *derived* from the object graph:

| Derived status | Condition |
| --- | --- |
| `proposed` | Asserted but awaiting required approval (§5) — excluded from packets. |
| `active` | Approved (or auto-activated) head version. |
| `contested` | Multiple unsuperseded heads, or an unresolved `contradicts` edge. For applicable pinned records, blocks packet construction (§4.1). |
| `stale` | `review_by` passed — delivered with a warning; needs re-affirmation. |
| `expired` | `valid_until` passed — excluded unless explicitly requested. |
| `retracted` | Withdrawn by an attributed retraction event. |
| `tombstoned` | Forgotten: excluded everywhere, object retained so sync cannot resurrect it (§6.5). |

### 2.5 Edges

Typed, behavior-bearing links between memories: `supersedes` (lifecycle),
`contradicts` (marks both sides contested until resolved), `derived_from`
(compaction and distillation provenance), `relates_to` (navigation only).
Edges are objects too — attributed and immutable.

### 2.6 Local work graph & execution

Engram is the system of record for a host-local graph of `WorkItem`s. A work
item has a stable collision-resistant id plus a short display ref, project and
root ids, optional parent, kind, title, intended outcome, acceptance criteria,
integer priority, labels, optional assignment and deferral, origin/provenance,
stable project binding, and immutable revision history. Local-work exception
reasons are attributed audit facts rather than permission-bearing authority.
No external reference is required.

Parent/child decomposition is a forest. Explicit prerequisite edges plus the
implicit completion edge `parent requires required-child` form one
completion-dependency graph. Every hierarchy, required/optional, and
prerequisite mutation rejects cycles in that union transactionally; optional
children add no implicit edge. `ready` is derived—not stored—from admission,
deferral, prerequisites, required parent constraints, and live claim state.
Project policy bounds depth, fanout, and open descendants. Assignment records
future planning intent. A **work claim** is fenced live responsibility.
Neither authorizes resource mutation.

A `RootExecution` is one aggregate execution generation for a root. It owns
the contributor roster, current child-run membership, required child
`CompletionSeal` hashes, root decisions/waivers, and the root completion
barrier. A `WorkRun` is one execution generation for one work item. V1 gives
it exactly one ordinary executor and at most one live `WorkClaim`; parallel
sessions execute distinct child runs under the same root execution. A run
owns its ordered execution feed, executor checkpoint, evidence, claim,
resource leases, and completion state. Root members without that run's claim
may inspect permitted root memory and communicate but cannot mutate or
complete the run. V1 permits one active run per item. Reopen creates a new run
generation so old claims, grants, and evidence cannot revive. The shared
working-memory ring belongs to the **root work item**, not `RootExecution` or
`WorkRun`, and survives every generation; memories reference narrower
work/run ids for filtering and provenance. Reopen preserves memory and resets
execution state. Memory scope does not recursively nest with the work graph.

The target durable work lifecycle is `proposed → open → completion_pending →
completed | cancelled | superseded`; an attributed completion abort returns
to `open`. The shipped alpha seals `open → completed` directly only when its
linked action-outcome and resource-lease drain sets are empty; it refuses a
nonempty drain until controlled `completion_pending` ships. Availability
(`ready`, `claimed`, `active`, `blocked`, `deferred`,
`waiting`) is a derived projection. Completion binds the accepted work
revision, run generation, claim fence, checkpoint position, acceptance
results, and evidence hashes. Required children and prerequisites must be
complete or an explicit reason-attributed waiver must account for them. Root
completion additionally binds required child seals or reason-attributed waivers for
disposed required children, plus the `RootExecution`
roster/contributions. Acceptance records whether evidence was independently
verified or self-attested.

Every claimed mutation revalidates the full ancestor chain and the run's
membership in the currently active `RootExecution`. Root completion refuses
while any descendant has a live claim or handoff offer. Optional unfinished
children may be named in the seal, but the closed ancestor and completed root
execution fence their old runs immediately. They must be disposed leaf-first
before root reopen; the new root generation never silently inherits an old
child run.

**Claims schedule; leases authorize mutation.** A resource lease can be
issued only to a session holding a live claim whose work scope covers the
resource. Mutating work uses fenced execution leases over canonical
project-relative path or versioned logical subjects; absolute worktree paths
are forbidden. An intent lease reserves planned mutation; a coordination
lease authorizes exclusive run/report transitions; shared analysis needs no
exclusive resource lease. Releasing, handing off, or recovering a claim bumps
its fence and transactionally revokes or transfers dependent leases. Lease
claim, renewal, handoff, release, and recovery use compare-and-swap revisions,
monotonic fencing epochs, expiry/heartbeat, and idempotency keys. Every
behaviorally relevant transition appends an immutable work/run event even
when its current view is a mutable projection.

Mediated actions heartbeat implicitly. A host-reported pause suspends expiry
only through a bounded `max_suspension`; afterward ownership is recoverable,
not silently free. Recovery is attributed, increments fences, and forces a
returning holder to reconcile. Pause remains host process state.

**Completion is the execution barrier.** In the target controlled path, `work_complete` enters
`completion_pending`, denies new ordinary mutation, drains in-flight actions,
terminalizes the run claim, releases or transfers every dependent resource
lease, and waits for its executor checkpoint or an authorized decision. A
root barrier additionally freezes the expected `RootExecution` contributor
roster and waits for required child seals or explicit disposed-child waivers,
plus contributions or attributed, audited waivers by a project-bound session.
`completion_seal` captures one dense run-feed cut
plus the accepted work revision, run/claim fences, action reconciliation,
acceptance results, and evidence hashes; a root seal also binds those child
seals and aggregate contributions. The seal makes the work `completed`; an
attributed abort before it returns to `open`. Reopen preserves root-work
memory but creates a clean run generation.

The `gate` word is an audit wrapper over existing machinery: one pass/fail
`WorkEvidence` entry on the focused item the session holds (gate name, bounded
failure-label list, optional reference) through the ordinary evidence path.
Bare `gate NAME` records a pass; a failure carries at least one label, using
the check command or check name when no test id exists. Hidden
idempotency replays a consecutive identical result after a lost response only
within the same claim generation. Release, handoff, or recovery creates a new
claim identity and therefore a fresh observation even when the result is
unchanged; the same result after a different state is also fresh — nothing
else. The gate payload is a structural field on `WorkEvidence`, not a
classification inferred from generic evidence prose. It adds no canonical kind,
no obligation, no completion barrier, and no waiver; a product defect becomes
a required child through the ordinary propose path, which existing
completion machinery already enforces. The feature brief defines the input
bounds; the domain/storage boundary owns and revalidates their normalized
form.

Optional report state is separate from work completion:

```
not_requested → finalization_pending → report_ready → publishing → published
                       └── abort → not_requested            └── failure → report_ready
```

Optional report finalization consumes the completed run's `CompletionSeal`;
it never quiesces or drains execution a second time. Participant completion
contributions and attributed, audited waivers are already bound to the cut. The
host creates a `ReportAssembly` anchored to the root seal and acquires a
fenced `ReportAssemblyClaim` for its designated finalizer. This post-completion
claim is neither a `WorkClaim` nor a `ResourceLease`, requires no live work
claim, and permits no ordinary workspace mutation. The narrow finalizer grant
binds the seal hash, assembly generation/revision, and assembly-claim fence
for deterministic assembly and one polishing pass. Handoff or recovery bumps
that fence. Reaching `report_ready` terminalizes the claim and freezes
immutable bytes and a report hash. A publication intent and idempotency key
exist only when a target is requested; retry sends the same bytes, and
revision creates a superseding report.

### 2.7 Execution control

Memory can govern behavior only when the host makes Engram's decision a
precondition for execution. Engram is the policy decision point; the host
runtime is the policy enforcement point and actuator. The agent may request
coordination operations but cannot grant itself permission. Effective
authority is always the intersection of user/host policy and an Engram grant:
an Engram grant is necessary for a controlled operation and can never widen
the host's authority.

Each bound session follows a durable protocol:

```text
unbound -> sync_required -> ready -> turn_open -> checkpoint_required -> ready
             ^                |             |                 |
             +-- restart -----+             +-- handoff ------+

sync_required -> recovery_open -> checkpoint_required -> sync_required
                                                       +-- reevaluate -> ready

ready -> completion_required -> participant_ready -> exited
                                  +-- optional finalizer_open -> exited
```

Before a turn, the core deterministically evaluates store and schema health,
the declared control policy, root-execution membership, focused work/run,
work revision, claim fence and run state, exact context-delivery
acknowledgements, host-confirmed delivery position and source-feed progress,
applicable
pinned context, lease fences, and outstanding checkpoints. It returns one of:

- a short-lived `TurnGrant` bound to one canonical turn intent, session,
  work item/revision, run generation, claim fence, context packet,
  named source-feed position vector, host-confirmed session delivery position,
  project-policy and work-admission epochs, optional portable writer epoch and
  validation deadline, mediated capability envelope, resource-lease fences,
  expiry,
  and any exact bounded context/delta injections still required; or
- a typed refusal with stable code, blocking directives, and the capabilities
  that remain safe for recovery. A refusal whose repair needs model reasoning
  may include a short-lived `TurnGrant { purpose: recovery }` restricted to
  exact directive ids and read/capture/coordination capabilities; ordinary
  mutation and new external effects remain denied. Its checkpoint returns to
  `sync_required` for reevaluation. Host-automatic and human-only directives
  do not create agent turns; or
- a typed `defer` for ordinary contention, with retry/wake conditions distinct
  from an authority or safety refusal.

The common pre-turn path grants with missing context/deltas inlined; it does
not refuse merely to tell the agent to call another retrieval tool.
Immediately before prompt dispatch, `turn_begin` atomically rechecks grant
expiry, project-policy
epoch, work-admission epoch, work revision, claim fence, resource-lease
fences, the optional portable writer epoch/validation deadline, and the
session blocking watermark,
then records those exact delivery tokens as **tentative** and activates the grant. Checkpoint
atomically promotes the contiguous session delivery position and its exact
source-feed progress vector. Restart never promotes an uncheckpointed advance.
An observe-only partial recovery page remains attached to its begun grant and
is returned exactly through session status for safe redelivery; other uncertain
begun prompts remain checkpoint/reconciliation required and are not replayed.
Refusal is reserved for an unsafe packet,
unreconciled recovery, lifecycle hold, unknown prior action, missing authority,
or another condition that cannot be delivered safely.

An action-gated host obtains a single-use `ActionGrant` immediately before a
material capability call. It is bound to the parent turn, effect class,
canonical structured resource subjects, authority references, and request
fingerprint. Authorization rechecks the session blocking watermark as well as
policy and lease fences. A transactional `action_begin` fences replay and
atomically rechecks the complete grant basis: parent/grant state and expiry,
project-policy and work-admission epochs, optional portable writer
epoch/validation deadline, blocking watermark, session/run phase, work
revision and claim fence, capability-map revision, request fingerprint, authority references,
and every lease subject/holder/fence/expiry. A stale basis refuses without
consuming the grant. `action_complete` stores a minimal redacted
receipt. A crash after begin but before a terminal receipt produces
`outcome_unknown`, never an assumed failure or blind retry. Read-only
diagnostics and precisely scoped recovery operations remain available when a
write or external effect fails closed.

Filesystem action gating also requires symlink-safe, execution-bound
resolution. A conforming host traverses from a registered project-root handle,
retains the resolved or nearest-ancestor handles through invocation, binds
their identities and any unresolved tail into the grant, and creates or
renames relative to those handles. `action_begin` rejects a changed binding as
`resource_remapped`. A host that can only re-resolve and compare immediately
before invocation still has a final check/use race and must report filesystem
coverage as detection-only rather than `action_gated`.

Turn completion is one structured checkpoint, not a second status dialogue.
The host supplies recorded action receipts; the agent adds durable findings,
typed blocker references, a bounded next-intent value, and lease disposition
beside its ordinary response. Meaningful progress prose is captured once as
typed memory and cited by hash. The checkpoint drives deltas, handoff
material, and report input without storing raw reasoning traces or creating
another work ledger.

An ordinary mutation is never gated on creating a memory, and checkpoint
capture hashes may be empty. Semantic capture is prompted at meaningful
boundaries and required only for the participant contribution; structural
action metadata is captured automatically. This prevents a capture quota from
being satisfied with low-value prose.

Context delivery is itself durable protocol state. Cursor identity is typed:

```text
FeedPosition { kind: project | root_work | run_execution, id, position }
DeliveryPosition { session_id, position }
FeedRange { kind, id, from_position, to_position, observed_head_position }
```

Initial packets and delta pages receive a dense per-session
`DeliveryPosition` and record their exact `FeedRange` sources, `has_more`
state, content digest, and delivery token. A `TurnGrant` binds
`basis_feed_positions[]` separately from `basis_delivery_position`.
Standalone acknowledgement stages only exact contiguous source ranges under
the next contiguous session delivery position; it never assumes adjacent
global row ids belong to the same feed. `turn_begin` records tentative use.
Checkpoint compare-and-swap supplies the expected checkpointed delivery
position, promotes through one contiguous delivery position, and atomically
records the resulting source-feed position vector. It means the host asserts
delivery, not that Engram proved model comprehension.

Work/run events store intrinsic type plus audience/resource selectors. A named
built-in classifier derives per-session admission impact and blocking
watermark: `blocking` events such as an applicable pinned change, lease
recovery, freeze, or addressed handoff must be injected or reconciled before
the affected operation; `advisory` events are bundled under budget with
visible omission and create a next-turn `delta_backlog` obligation only after
a configured count/age/bytes threshold; `informational` events never block by
lag count. `turn_begin` and action authorization recheck the watermark. A
reported context compaction invalidates packet delivery and forces pinned
re-injection even when the cursor did not change.

The packet hash reproduces content; the event cursor orders behaviorally
relevant work/run changes; a project policy epoch invalidates grants after global
control/mediation changes; a work admission epoch invalidates grants after
applicable pinned-rule, participant-access, work-revision, claim, or run-state
changes; resource-lease fencing epochs invalidate grants after ownership changes. These values are not
interchangeable. Unknown safety-relevant schema or policy versions block
admission rather than being ignored.

State-changing control transitions are idempotent and emit immutable canonical
events; current session, delivery, action, and lease records are durable
operational projections. Live grants and high-volume allow/refusal diagnostics are
immutable operational records with bounded retention, not canonical memory;
restart discards their authority. Compact durable request-key tombstones bind
kind/key/intent/terminal state through work retention so pruning or expiry can
never reinterpret an old key as fresh authority. Only a transition that can change peer
behavior enters the work/run delta feed, so control does not become a second
status ledger.

Host/operator project-policy mutations use a separate store-scoped durable
idempotency key because they are not owned by an agent session. The normalized
intent and exact receipt commit atomically with activation. Retry-time wall
clock is excluded from intent, so a lost-response retry with the original
expected head returns the committed receipt before re-evaluating that now-stale
compare-and-swap guard; same-key different-intent reuse is a conflict.

Initialization installs and selects a versioned safe project-scoped
`ControlPolicy` atomically in a new store. An existing store must match the
complete current schema and policy chain exactly; missing, different-build,
corrupt, or ambiguous state fails ordinary store open for every surface before
DDL. `doctor --recover-policy` is a read-only diagnostics-only path:
it exposes no service or mutation handle and never selects or rewrites policy.
A new immutable version records a canonical
`project_policy_admin`-shaped host/operator authority decision. V1 honestly
records that attribution as asserted context, not authenticated identity;
authenticated policy administration is deferred. The implicit bootstrap
default uses synthetic system attribution; an explicit initial assurance
requires and records an asserted operator plus reason. Selecting the new policy
and advancing the project epoch is one transaction.
The active policy also selects one canonical `ObligationRuleSet` hash.
Assurance-only transitions preserve the selected set. A host/operator-only
activation may append a validated
`set_obligation_rule_set` successor under the same epoch/hash compare-and-swap.
The shipped operator CLI accepts at most 64 KiB of strict nested V1 JSON inline
or through `@file`; rollback re-supplies the desired typed JSON and never
activates a hash alone. No MCP or model-turn operation can administer it.
Unknown schemas, unknown fields or triggers, duplicate rule identities, and
missing selected objects fail closed.
Every active run rechecks that shared epoch at turn/action boundaries—no
single work claim or resource lease controls project policy. Host/user
authority is the ceiling; control policy and work-applicable pinned rules may
only restrict it. External action intents cite durable authority references bound to work/run, effect,
payload fingerprint, and validity window.

Failure policy is capability-specific. Observation remains available where
disclosure permits. On a decision-service deadline or clean unavailability,
policy may allow only reversible local work as `degraded_open` under a
previously issued unexpired envelope bound to policy/epoch, mediation map,
resources, lease fences, and action/debt limits. The host durably spools typed
`DegradedActionDebt` for idempotent upload and reconciliation; communication
remains closed while Engram is unavailable, and the host must not create an
offline message ledger. After recovery, a scoped recovery turn may capture a
durable semantic finding through the ordinary typed-memory path. Shared
mutation, external effects, and lifecycle actions remain closed. Store
corruption, unknown safety schemas,
user/host denial, missing mediation, and unverifiable external authority are
non-overridable. Break-glass applies only to explicitly policy-authorized
coordination exceptions after those invariants pass.

Control assurance is recorded honestly:

- `advisory`: Engram tools can be bypassed;
- `turn_gated`: the host mediates every model turn;
- `action_gated`: the host also mediates every declared material capability.

The project policy is a floor, not the only assurance check. `observe` and
`communicate` require `advisory`; internal `coordinate`, mutation,
external-side-effect, and lifecycle effects require at least `turn_gated`. A
bind records the declared mediated set and returns its assurance-capped
effective subset; evaluation and lease acquisition refuse effects above that
subset. Policy epochs, not wall-clock timestamps, order immutable policy
history; activation and decision timestamps are attribution only.

The full protocol, effect classes, failure semantics, and host conformance
contract are specified in the
[behavioral control-plane brief](features/behavioral-control-plane.md).

## 3. Storage & sync

### 3.1 V1 canonical store: local SQLite, append-only

V1's canonical store is a **local SQLite database** holding the same
immutable, content-addressed objects the model defines (§2.4): versions,
events, edges, and evidence as append-only rows keyed by their content hash,
written transactionally. Append-only is a semantic contract enforced by the
core, not a hope — nothing updates or deletes object rows except the purge
runbook (§6.5).

Local means one host, not one process. A stable `project_id` selects one
host-local database regardless of checkout or isolated worktree. Concurrent
CLI and MCP clients use WAL mode, a bounded busy timeout, short transactions,
and atomic compare-and-swap operations for coordination projections.
Deployments must never derive store identity solely from the current working
directory, because that would silently fork memory across worktrees.

Each project, root-work, and run-execution feed allocates its own dense
`feed_position` inside the same transaction as the event append. Delivery has
its own dense per-session sequence over emitted pages. "Contiguous" always means adjacent
positions in one named feed; a global SQLite row id may be useful internally
but is never a cursor. This identity is fixed by the work graph so
safety CAS never depends on sparse cross-feed numbering.
Current sessions, delivery progress, leases, actions, finalization barriers,
and indexes are mutable projections, but their safety-relevant transitions
are auditable through canonical events. Live grants and decision diagnostics
occupy a separate bounded operational tier and never become peer context. The
cursor orders peer deltas; it is not an object identity and does not cross
stores as a global sequence number.

```
engram.db
  objects      // content-addressed rows: versions, events, edges, evidence — write-once
  projections  // exact-current heads/status/order plus rebuildable indexes and FTS5
  control.*    // live grants + bounded diagnostics — operational, never memory
  meta         // current-build marker; refuses stores created by another build
```

Runtime heads, status, ordering, authority, and idempotency tables are durable
parts of the exact-current store and are recovered from a verified current
backup. `engram doctor --repair-projections` rebuilds only declared indexes,
triggers, and FTS5 content from those verified durable rows; it never recreates
durable tables from `objects`. `engram doctor` verifies hashes, graph references,
projection bindings, and index freshness. `local` mode relies on SQLite
transactions. An optional
deterministic recovery snapshot and verified restore provide
`local_backed_up`. Sequential off-host transfer under one active host provides
`portable`. A later concurrent `Sync` backend provides `synchronized`.
Distributed merge semantics are not required for valid local-only or portable
operation.

#### 3.1.1 Canonical-bytes contract

Content addressing is only as interoperable as its byte-level definition, so
the contract is part of the spec, not an implementation detail:

- Objects serialize as **RFC 8785 (JCS) canonical JSON**, UTF-8.
- `version_id` = SHA-256 over the canonical bytes, with the hash field itself
  excluded; the object's storage key — SQLite row key today, filename in a
  portable/shared backend (§3.2–3.3) — is that hash.
- Hashes are **verified at read time**, so `engram doctor` distinguishes
  corruption from formatting drift.
- Every executable object carries the exact supported schema version.
  State from a different build is refused rather than interpreted.
- State changes mint new objects; they never rewrite old ones.

Without this contract, two implementations could mint different hashes for
semantically identical records, and integrity checking would be impossible to
define. It holds regardless of substrate — which is what keeps the deferred
backend below a drop-in.

### 3.2 Optional portable replication

`portable` is the V1 cross-machine mode for sequential handoff, not live team
coordination. SQLite remains canonical on the active host. Engram projects a
canonical, human-readable recovery tree containing an immutable manifest,
shared objects, work/events, typed feed ordering, schemas, and permitted
evidence references. A configured transport publishes that tree on a cadence
and at clean session end. `engram doctor` reports the verified remote head and
unpushed event, byte, and age lag; failed publication is a visible degraded
durability state, not a work-execution failure.

Each projection comes from one consistent SQLite read cut. Its manifest binds:

```text
PortableManifest {
  project_id, lineage_id, parent_manifest_hash?
  schema_versions[], object_set_hash, feed_heads[]
  writer_epoch, writer_instance_id, writer_state: active | released
  export_policy_hash, projection_coverage_hash, created_at
}
```

Routine cadence pushes preserve the active writer epoch. A cross-machine move
uses `engram portable release`: checkpoint/exit every local control session,
make unfinished work claims recoverable, release every resource lease,
invalidate grants/delivery authority, advance the writer epoch, publish a
`released` manifest by head CAS, and make that local store mutation-read-only.
The next host runs `engram portable acquire`, restores the exact released head,
CAS-publishes its new instance/epoch as `active`, and only then enables local
mutation. If the old host crashed without release, acquisition requires an
attributed recovery command that advances the epoch. An unreachable remote
blocks acquiring the optional portable writer; it never blocks a project that
was configured and operated as `local`.

Acquire/restore never overwrites an arbitrary local store. Its destination
must be empty or already at the exact expected manifest with no unpushed local
tail. Otherwise Engram preserves the destination as a recovery bundle and
refuses `portable_local_diverged`. On every process/session start or crash
resume in portable mode, the host performs a bounded remote head/epoch check
before issuing any mutation-capable turn or action grant. Remote mismatch
makes the local store mutation-read-only and routes to reconcile; remote
unavailability fails portable authority acquisition closed while leaving
permitted reads/diagnostics available. This head check is authority
validation, not use of the remote as a second work database. During an active
session, a configured bounded cadence revalidates the epoch; forced takeover
of an actually still-running old host therefore has an explicit detection
window rather than an impossible distributed-lock claim. A detected mismatch
advances the local admission epoch and invalidates outstanding grants.

Each push compare-and-swaps the expected parent manifest. A changed remote
head produces `portable_diverged`; Engram refuses to push or merge. The named
`engram portable reconcile` workflow previews both immutable lineages and
either continues one while retaining the other as a recovery bundle/proposed
import, or forks a new project identity. It never silently drops a lineage or
renumbers its dense feeds. Moving to another machine is an explicit clean
flush/handoff followed by `engram portable restore` of that head. Normal
active execution never reads the remote as a second live database.

Portable state never restores execution authority. Live work claims,
resource leases, control sessions, grants, action state, delivery progress,
and agent-private scratch are excluded. Immutable claim lifecycle events may
be retained for audit and fencing history, but an unfinished prior-host claim
restores as `recoverable`; attributed recovery advances its generation/fence,
and resource leases must be reacquired. Inert lease lifecycle audit events may
cross, but never rebuild an active lease. This prevents a user from locking
their new machine out with authority held by the old one.

Portable projection is closed, not an arbitrary filtered object subset:

- **Executable shared-state closure** includes every transitive object needed
  to rebuild work items/edges/events, readiness, policy/authority, root-shared
  context, acceptance/evidence, completion seals, and typed feed heads. If
  export policy will not permit one of those objects, release fails
  `portable_projection_incomplete`; a stub may not stand in for executable
  state.
- A provenance-only reference into excluded private or non-executable content
  resolves through an `ExclusionStub { target_hash, object_kind, reason,
  export_policy_hash, stub_hash }`. The stub is a projection record under its
  own canonical `stub_hash`; it asserts but never impersonates the excluded
  object's content hash. `doctor` treats a matching stub as deliberately
  excluded and a missing target/stub as corruption.
- An excluded non-semantic feed payload leaves an `ExcludedFeedEntry` binding
  feed identity, dense position, original event hash, exclusion-stub hash, and
  policy hash. Positions are never removed or renumbered. A behavior-affecting
  shared event cannot be replaced this way and must be included or fail the
  release.
- Projection of an already canonical object is **pass or exclude, never byte
  rewrite**. A Redactor may transform a candidate before its canonical id is
  minted on initial write; it cannot mutate bytes during export while keeping
  the old hash. Sanitized derivatives are new canonical objects with explicit
  provenance.

The manifest coverage hash commits included objects, stubs, excluded feed
entries, and closure results. `doctor` reports counts/reasons and may claim
`portable` only when executable shared-state closure is complete. Stubs leak
existence, kind, and hashes; the portable target must be authorized for that
metadata. If policy forbids even stubs, Engram can make a marked-truncated
backup/export but cannot call it portable or activate it as a working store.
Acquire requires the same recognized `export_policy_hash`; mismatch refuses
`portable_policy_mismatch` until an attributed policy adoption or a
new lineage is chosen.

The port is substrate-neutral. For a personal or small private Git transport,
the recommended layout is a dedicated plumbing ref such as
`refs/engram/<project-id>/<scope>`, never a checked-out branch or the working
tree. The configured remote is a disclosure boundary: export policy and the
`Redactor` run before projection, `secret-ref` values remain references,
agent-private scratch never leaves the host, and `engram doctor` reports the
no-op redactor honestly. A shared code repository is not the organization-
scale default: hundreds of developers require per-user private repositories,
an access-controlled internal object store, or the service backend so ref
count, privacy, and lifecycle do not couple to the code remote.

### 3.3 Deferred: concurrent cross-host sync

**Deferred, not rejected.** Draft 0.2's shared backend—append-only
content-addressed objects, set-union object transfer, concurrent heads
surfacing as *contested* (§6.3), and tombstones preventing resurrection—solves
live cross-host coordination. Same-host concurrent sessions and sequential
portable handoff are already V1 requirements. Concurrent sync additionally
needs per-origin feed positions or a trusted sequencer; it may not reinterpret
one portable dense sequence as globally contiguous after divergent writes.

Because objects already use the same canonical-bytes contract (§3.1.1), the
object transfer remains simple, but claim/resource coordination, feed merge,
privacy, and conflict semantics are not. The intended organization-scale
substrate is a dedicated service/object store or private per-user/task stores,
not hundreds of automation-generated refs in a shared code repository.
Sensitive values never enter any shared history—vault references only (§7).

### 3.4 Ports

Domain semantics bind to interfaces, not backends: `Store` (append / get /
list-heads), `Index` (rebuild / search), optional `BackupAdapter`,
`PortableStoreAdapter`, and later `Sync`, `WorkSourceAdapter` and
`PublicationAdapter` (§9.2), `Redactor` (§7), and `Signer` (optional, §7). V1
ships `SqliteStore`, recovery snapshot/restore, the portable sequential
contract, and a side-effect-free dummy publication adapter. Git, internal
object storage, and later service transports implement the appropriate port
without changing work semantics.

**Interchange and durability modes:** SQLite is canonical in `local` mode.
`local_backed_up` adds a verified restore-only off-host snapshot. `portable`
adds a transferable working snapshot with exactly one active host, explicit
handoff/restore, scheduled push, and divergence refusal. `synchronized`
activates a later shared multi-writer backend. `engram doctor` reports mode,
remote head, recovery point, lag/degradation, and writer assumption rather
than implying durability or concurrency that is not present.

## 4. Context packets & retrieval

A **context packet** is the unit of delivery: the block of memory an agent
receives at session start or on request. Packet construction is a first-class
core API used identically by every interface (§8), and every packet is
reproducible: it has a content hash, and `engram context explain <packet>`
shows exactly what was included, omitted, and why.

Every packet also carries the observed positions of its named dense project,
root-work, and run-execution feeds. The hash answers “what exact content did I
receive?”; feed positions answer “what changed after that?” `engram context
delta --feed <id> --since <position>` returns the ordered peer-visible changes
without rebuilding the whole packet. A runtime
may use its own mailbox or notification mechanism as a doorbell, but Engram's
durable change feed is authoritative. Engram is not a chat bus.

### 4.1 Rung 1 — pinned, complete or error

All *active* pinned memories in the caller's applicable scopes, verbatim, under a
hard budget.

> **Fail closed.** The pinned tier is never silently truncated *and never
> self-contradictory*. Packet construction fails *before the agent acts* in
> two cases: the pinned tier cannot fit its budget, or an unresolved
> contradiction stands between two applicable hard/firm pinned records —
> delivering both would ask the model to improvise policy precedence. The
> error names the records needing merge, demotion, or resolution (§6.3).
> Soft-authority conflicts are delivered, flagged contested. A dropped
> convention is a nuisance; a dropped or ambiguous "never do X" is an
> incident.

### 4.2 Rung 2 — the index tier

Titles only — `id · kind · title`, one line each — for every active
index-delivered memory in scope. This is what fixes "agents don't know what
they don't know": fifty memories cost fifty lines, and the agent knows what
it can pull. Capped by budget with ranked eviction (§4.4) and an **omission
manifest**: counts and reasons for anything excluded, so absence is visible.

### 4.3 Rung 3 — on demand

`show <id>`, `history <id>`, and `search <query>` over FTS5. Exact identifier
matches always outrank fuzzy matches. Embeddings are a later, optional
addition — good titles give most of semantic retrieval's value for none of
its machinery.

### 4.4 Budgets and ranking

| Tier | Default budget | On overflow |
| --- | --- | --- |
| Pinned (rung 1) | 4 KiB | Fail closed (§4.1) |
| Index (rung 2) | 8 KiB | Ranked eviction + omission manifest |
| Whole packet | 12 KiB (≈3k tokens) | Hard cap |

Defaults are per-deployment configuration, to be tuned against the evaluation
harness (§10). Non-pinned ranking, in order: scope proximity → exact
identifier match → FTS relevance → authority → confidence →
`last_verified`/validity → prior usefulness → byte cost. Deliberately **no
recency boost** for constraints and decisions: an old, recently-verified
decision outranks a new, unverified one.

Every packet item carries *why it was retrieved* and its evidence pointers,
so an agent can cite — and a human can audit — the chain from context back to
source.

Every packet includes a one-line count of proposed and stale items. Review
pressure is visible in the normal workflow rather than hidden behind a
command nobody remembers to run.

## 5. Write path

Anyone — human or agent — can *assert*; whether an assertion activates
immediately or awaits approval depends on **origin and authority**. Every
promotion is its own attributed audit event. Writes pass through the
`Redactor` port before persistence (§7).

The shipped agent capture path is `engram work note` / MCP `note`. The core
derives a stable idempotency key so a lost response can be retried without
duplicating the finding; changed prose is a new intent. A future generic memory
capture surface may infer kind, authority, delivery, and scope from the active
task plus asserted host context, return the inference in its receipt, and ask
only when genuinely ambiguous. The explicit `assert` surface remains for
callers that need exact
control. Inference never bypasses the activation matrix below.

| Origin | soft | firm | hard |
| --- | --- | --- | --- |
| Human, explicit | active | active | active |
| Agent, with machine-verifiable evidence | active | proposed | proposed |
| Agent, unsupported claim | proposed † | proposed | proposed |
| Session-end distillation | proposed | proposed | proposed |

† Exception: `episode` records activate directly regardless of evidence —
they are attributed observations, deliver on-demand only, and decay by
default (§6.4).

**Distillation** — an end-of-session pass that drafts candidate memories from
what happened — is a proposer, never a writer. It gives automation's capture
rate with review's quality gate, and must deduplicate against existing
memories before proposing: exact duplicates no-op; semantic near-duplicates
are proposed *linked* to the existing memory, never auto-merged. Distillation
writes only to *local* working memory; nothing crosses the organizational
boundary through it — publication happens solely via explicit task
finalization (§9.5). Raw transcripts are not retained by it (§7).

## 6. Lifecycle

### 6.1 Review vs. validity

Two clocks, deliberately separate. `review_by` is an *epistemic* deadline:
past it, the memory is `stale` — still delivered, flagged with a warning,
queued in `engram review` for re-affirmation (bump), demotion, or retirement.
`valid_until` is *factual*: past it, the memory is `expired` and excluded
unless explicitly requested. "Review overdue" does not mean "false" — the
states are distinct because the correct behavior differs.

Individual hard constraints may be configured to fail closed on overdue
review, for claims where acting on an unverified rule is worse than stopping.

### 6.2 Supersession

Changing a memory means asserting a new version that supersedes the old —
never editing in place. The chain preserves what was believed, when, and on
whose word.

### 6.3 Contradiction and contested state

Conflicts are first-class, never resolved by timestamp:

- Concurrent heads after a sync merge → the memory is **contested**; both
  heads visible, both flagged in packets.
- Cross-memory conflicts get an explicit `contradicts` edge; both sides show
  as contested until resolved.
- Resolution is a new version citing *all* conflicting parents, with
  rationale recorded. `engram conflicts` lists everything awaiting
  resolution.

### 6.4 Episodic compaction

Episodes decay on a schedule: full record → one-line summary → gone. Each
compaction step is a new object with a `derived_from` edge to its source,
which is retained until retention policy deletes it. Compaction previews
before it acts (`--dry-run` is the default posture for anything destructive).

### 6.5 Forgetting vs. purging

**Forgetting is a tombstone**: excluded from every packet and search, object
retained so sync cannot resurrect it. The distinction from purging is what
makes "forget" safe to use freely.

> **Purge is not an ordinary operation.** V1 purge = logical tombstone. In
> the local SQLite store, physical erasure is *technically* feasible (delete
> + vacuum), but it still spans every backup, portable head, and JSONL export and it breaks
> the append-only contract — so it remains an exceptional, documented runbook
> with preview and audit, not a CLI verb. Two boundaries stay effectively
> irreversible regardless: anything already *published* in a report (§9.5)
> may live on in the external target's history, and any Git-backed portable or
> team store (§3.2–3.3) retains history across clones, reflogs, and host backups —
> where erasure means coordinated history rewrite, force-push, and clone
> invalidation. A v2 option is envelope encryption of sensitive payloads with
> per-record keys, so crypto-shredding can render retained ciphertext
> unreadable. The practical consequence: *prevention is the real control* —
> the Redactor port (§7) matters precisely because persistence past a
> boundary cannot be reliably recalled.

## 7. Audit, security & compliance

Work deployment is the design center, so these are v1 requirements, not
add-ons:

- **Attribution on every object:** `actor_id`, `actor_kind` (human / agent /
  system), originating run or session, timestamp, reason — plus an
  **assurance level** recording how that identity was established. In V1,
  actor and authority context arrive from the proprietary runtime: a text
  instruction supplied through the tools and skills in use. That is
  *asserted* instruction/authority context, not cryptographic identity, and
  the spec says so — objects preserve source/tool/skill metadata when
  available and record assurance accordingly. No SSO/LDAP integration in V1.
  Where compliance-grade attribution ever becomes a deployment promise, a
  trusted write gateway or Signer-backed signatures (below) must ship with
  that deployment — the port alone is not attestation.
- **Immutable history:** versions, approvals, retractions, evidence, and
  tombstones are append-only objects (§3.1) — the audit log is the data
  structure, not a side channel.
- **Sensitivity labels** (`public` / `internal` / `restricted` /
  `secret-ref`) enforced at retrieval: scope and sensitivity authorization
  run before anything enters a packet.
- **Pre-write redaction:** a pluggable `Redactor` port (DLP / secret
  scanning) runs on every write and fails closed where policy demands.
  Secrets and PII are stored as vault references, never as remembered values.
  It may transform a candidate before canonical identity is minted. Export of
  an existing canonical object is pass-or-exclude, never byte rewrite under
  the old hash; sanitized derivatives receive new ids and provenance.
  No backend is selected for V1: the shipped implementation is a *visibly
  labeled no-op for development* — surfaced in `engram doctor` output,
  implying no compliance assurance whatsoever.
- **No raw transcript persistence** by default; any ephemeral retention is
  explicit, bounded, and audited.
- **Safe export defaults:** JSONL/backup/portable export excludes restricted
  records unless explicitly widened and exports `secret-ref` only as a vault
  reference. Portable mode additionally requires complete executable
  shared-state closure; excluded provenance uses policy-authorized stubs and
  feed placeholders (§3.2), while agent-private scratch never leaves the host.
- **Signing is policy, not a dependency:** a `Signer` port supports signed
  objects and signed Git commits where a deployment requires cryptographic
  attestation. Baseline v1 runs without it — at asserted-identity assurance;
  deployments that promise more must deploy more.

## 8. Interfaces

One core library owns the object model, derived state, packet construction,
and control decisions. The CLI, MCP server, and host control transport are
thin faces over it; a future service is another. No interface reimplements
delivery or admission logic.

### 8.1 CLI

```
# write path
engram note "..." [--task <id>]  # infer defaults; return classification receipt
engram assert   --kind decision --authority firm --scope project:x \
                --title "..." [--body-file ...] [--ref jira:ABC-123]
engram approve <id>         engram retract <id>        engram forget <id>

# read path
engram show <id>            engram history <id>        engram search <query>
engram context build [--scope ... --budget ...]
engram context explain <packet-id>
engram context delta --task <id> --since <cursor>

# curation & ops
engram review               engram conflicts           engram compact --dry-run
engram sync                 engram doctor              engram doctor --repair-projections
engram export --jsonl       # purge: exceptional runbook, not a CLI verb (§6.5)

# local work and reports (§2.6, §9.5)
engram work next              engram work show <ref>       engram work search <query>
engram work propose [...]     engram work focus <ref>      engram work update [...]
engram work complete [...]    engram work handoff --to <actor>
engram work list [filters]    engram work stats            engram work preflight
engram report finalize <root-ref>            # optional barrier → polish → report_ready
engram report show <root-ref>                engram report publish <root-ref> # optional, receipted

# control diagnostics and recovery (§2.7)
engram control status                       engram control explain <decision-id>
engram lease renew <resource>               engram lease release <resource>
engram action reconcile <action-id>

# optional external adapters (§9)
engram import preview <adapter> <ref>        engram import apply <snapshot>
engram export preview <adapter> <work-ref>   engram export apply <intent>
```

### 8.2 Agent-facing MCP server

Agent-facing MCP exposes exactly `next`, `ls`, `show`, `add`, `claim`,
`update`, `gate`, `note`, `done`, `search`, `handoff`, `remember`, `memories`,
and `forget`. The ordinary lifecycle work tools translate into the
six-operation work core: `work_next`, `work_focus`, `work_propose`,
`work_update`, `work_complete`, and `work_handoff`; the session binding
supplies project, actor, current work, and cursors. Typed `gate` and atomic
`note` are word-only `work_update:gate` and `work_update:note` service
suboperations, not variants exposed by the direct `work core update` surface.
Optional generic memory capture, import, publication, and administrative
queries remain separate tools rather than expanding every model turn.

The agent surface is thirteen words plus `search` — fourteen MCP tools, with no new
work-core operation: `gate` already wraps the existing evidence path, and
`remember`, `memories`, and `forget` are a thin project-memory surface outside
the six-operation work core — no focus mutation, no claim renewal. Reads use
the cooperative asserted project binding. `remember` and `forget` validate the
same non-empty actor/session binding inside the memory mutation transaction.
`memory_binding_invalid` means that binding is absent or inconsistent. The
implementation keeps this tool count, the MCP registration and instructions,
and every agent instruction file atomic with the code.

Project-memory list/search and full-read receipts fit both their structured
JSON/MCP representation and terminal-safe shell rendering under the 12 KiB
agent response ceiling. A full body is admitted before persistence only when
both exact envelopes fit; list/search sheds rows with an omission or
continuation signal when escaping would otherwise exceed the ceiling.

Agent-facing work has no grant token, validity window, revocation object, or
routine remint requirement. Its lifecycle mutations are governed by project
binding, current item state, fenced claims, and reason-attributed audit records.

Project-memory advertisement in `next` is advisory and
content-free: a count of retained project notes and a changed flag, with
no exactly-once guarantee. `memories` is the source of truth, and a
host-passed `context_generation` marks a fresh or compacted context and
may reannounce the count. Only a domain-separated digest of that asserted
value is persisted; its raw text is never retained.

This surface is for capture, retrieval, explanation, and coordination
requests. It is not a self-authorization channel: a model-callable MCP tool
cannot prove that the host withheld a turn or a side effect. Replayable grant
tokens never appear in model-visible results; the private host channel owns
them.

### 8.3 Host control channel

A separate host-private transport exposes the §2.7 protocol:

```text
control_bootstrap   session_bind        turn_evaluate       turn_begin
delivery_ack        action_authorize    action_begin        action_complete
turn_checkpoint     session_heartbeat   session_exit
```

Current implementation status: `engram control` ships the JSON-lines subset
`session_bind`, `session_status`, `turn_evaluate`, `turn_begin`, and
`turn_checkpoint`, plus `lease_acquire` and `lease_release`, for the built-in
`observe`/`communicate`/internal-`coordinate`/lease-backed-`mutate_local`
policy. `coordinate` is a lease-boundary effect, not a model-turn capability.
It persists exact retry evidence across restart. Lease acquisition applies the
active project floor first, then the per-effect floor, declared/effective
mediation, supported-effect set, and policy epoch before any reservation event;
policy refusals are sticky under their bind-scoped idempotency key, an
older-bind key conflicts, and an epoch refusal atomically adopts the new epoch
for a fresh-key retry. A successful acquisition key is not reused after
terminal release within the same bind. It invalidates unbegun grants when a new
control connection opens, fails stale begin-time rechecks closed, and binds a
local-mutation turn to the live exclusive execution lease and overlap fence
covering each declared resource. A begun mutation turn pins its bound leases:
release refuses and successor acquisition defers across nominal expiry until
the begun grant is checkpointed, preserving the fence across restart.
Replacement connections fence live
predecessors; begun grants remain checkpoint-required and discoverable, with
the exact frozen grant returned only for safely replayable observe-only partial
recovery. Path
subjects are bound to the project and conservatively NFC/case normalized,
and cross-task rebind is rejected while active leases remain. The remaining
operations and all individual action/shared/external/lifecycle authority are
not yet shipped;
`action_gated` declarations are rejected.
Every `HostControlRequest` variant is strict: the paired consumer must send
exactly the current field set for every operation. Any additive, unknown, or
removed request field is an `invalid_request` refusal that names the request
kind and offending field, including the former grant field on
`obligation_waive`. This pre-release request-shape revision is coordinated
atomically with the live TermAl consumer. There is deliberately no
version-negotiation or legacy-frame shim: the paired consumer must update all
host-frame shapes before this Engram build lands.

For a work-bound begun turn, the private checkpoint may atomically append up to
64 host execution observations, mint up to 16 typed verification objects, and
mint up to four source-bound environment identities. Verification derives its
source/run/session/check/result/time binding from a producer observation and
may link an environment by object hash or same-request index. Environment
evidence may use an opaque fingerprint or supply a bounded closed
component identity: toolchain, optional sandbox/image, workspace id, and the
bound session's capability-map revision. Engram derives the component
fingerprint and rejects a workspace, revision, run, or capability-map mismatch.
These fields are asserted host context, not attestation, and must not contain
secrets. `source_revision` fingerprints committed plus dirty content. A later
source revision reopens the requirement. Agent-authored generic evidence never
satisfies verification; the work protocol may only attach an existing typed
hash to the focused run.

Every checkpoint resolves the obligation rule set selected by the begun
grant's frozen project-policy epoch and records its hash on the canonical
`ExecutionObservation`. The built-in set maps every observation with
`source_changed=true` to one immutable test obligation regardless of outcome
or source-basis presence. Each definition binds the same rule-set hash, rule
identity/version, trigger, and requirement. A later policy activation applies
only to later observations and cannot reinterpret existing history. Every
observation and obligation definition carries its exact rule-set hash.
Obligation definitions
and terminal satisfaction/waiver events are direct project, root-work, and
run-execution feed objects; query rows are verified projections.
A typed V1 requirement may leave the verification command and environment
open, as the stock set does, or pin an exact `check_fingerprint` and previously
recorded `EnvironmentEvidence` object hash. A mismatched command, environment,
or source basis leaves the obligation open; only exact passed evidence at the
post-mutation cut satisfies it.
A passed test satisfies open definitions only against the latest mutation at
the evaluated run-feed cut. A latest basisless mutation therefore leaves the
open set waiver-only until a later basis-bearing mutation and passed test; that
test may satisfy earlier definitions too. Focus, nested next views, updates,
and both completion outcomes expose one count- and byte-bounded,
authority-redacted `obligation_page` with an explicit omission count and
deterministic typed guidance. Waiver is absent from MCP and `work_update`.
The operator-intended shell command has no credential or run-binding check;
its separation is a local convention, not an authenticated boundary. The
private JSON-lines request is the enforced alternative: it is bound to the
session's exact run and records the server-fixed actor beside an asserted
`waived_by` human and reason. Typed binding refusals are replayable while token
and transport faults remain request errors. Completion evaluates the cut-aware
set at the exact pre-seal run-feed position. Open definitions return a bounded
`open_work_obligations` protocol result recomputed from one coherent current
snapshot; it is guidance rather than a durable replay result. A new seal
declares obligation schema V1 and binds every applicable definition to its
satisfied/waived resolution; success and fresh-session focus reconstruct their
pages from canonical history, and the final checkpoint acknowledges the
matching typed verification evidence.
The current built-in requirement does not pin an environment hash. New seals
nevertheless declare environment schema V1 and bind the sorted, distinct
environment-evidence hashes at or before the exact dense cut, with a maximum
of 64 and without copying component bytes. Required child seals are decoded
and checked recursively; every accepted seal carries the current obligation
and environment schema bindings.

The transport may be an in-process API, native host integration, wrapper, or
local gateway. It is never exposed as an agent-callable way to mint grants.
The host must bind a local work item/run before delivering a task prompt, obtain a `TurnGrant`
before every ordinary model turn, inject all required deliveries/directives,
activate them through `turn_begin`, and persist
a checkpoint before the next turn. A scoped recovery/finalizer grant may
admit only its named repair/report prompt while ordinary turns remain denied.
An `action_gated` host must additionally
intercept every declared material capability, obtain and begin a matching
single-use action grant, and record its outcome even if the model turn later
fails.

Hooks can satisfy `turn_gated` conformance. `action_gated` conformance needs
native mediation around the declared tools; MCP alone remains `advisory`.
The runtime owns notification delivery and action execution. Engram owns
durable protocol state, deterministic decisions, and cursors. No
host-specific dependency enters the domain core.

## 9. Local work, reports & external systems

### 9.1 The boundary

> **Division of labor.** Engram owns host-local work and execution memory.
> External systems are optional snapshot sources, backup/portable/sync substrates, or
> publication targets. None is the live local work database, and none is
> required to open, decompose, execute, or complete work.

The core contains no Jira-, GitHub-, or Beads-shaped types. Everything
vendor-specific lives behind neutral adapter ports. Optional durability
storage is separate from work intake and publication; configuring one does not
silently enable the others.

### 9.2 External adapter ports

```
WorkSourceAdapter {
  capabilities()                    // what this backend supports
  normalize_ref(text) → Ref         // "ABC-123", URL, … → canonical ref
  fetch_snapshot(ref, projection) → WorkSourceSnapshot
  search(query, cursor) → [SourceCandidate]
}

BackupAdapter {
  put_snapshot(project, manifest, bytes) → BackupReceipt
  get_snapshot(project, snapshot_id) → RecoverySnapshot
  list_snapshots(project, cursor) → [SnapshotMetadata]
}

PortableStoreAdapter {
  read_head(project) → PortableHead
  fetch_snapshot(project, head_hash) → RecoverySnapshot
  publish(project, expected_parent, active_snapshot) → PortableReceipt
  release_writer(project, expected_active_head, released_snapshot) → PortableReceipt
  acquire_writer(project, expected_released_head, active_manifest) → PortableReceipt
  recover_writer(project, expected_head, recovery_intent, active_manifest) → PortableReceipt
  validate_writer(project, writer_instance_id, writer_epoch) → WriterValidation
}

PublicationAdapter {
  capabilities()
  publish_report(target, report, idempotency_key) → Receipt
  publish_work?(target, projection, idempotency_key) → Receipt
}
```

`WorkSourceSnapshot` is backend-neutral: canonical ref, projected title/body/
status/owner, captured time, source revision, canonical URL, canonical payload
hash, and bounded extension data. A `PublicationAdapter` accepts only an
explicit target and frozen payload under a durable idempotency key.
`BackupAdapter` stores/restores immutable recovery snapshots.
`PortableStoreAdapter` publishes and restores a sequential working snapshot
under parent-head compare-and-swap; it never merges or restores live execution
authority. Both are separate from a later live multi-writer `Sync` backend.

### 9.3 Provenance across a mutable source

An import creates immutable `source_snapshot` evidence and a local work
revision. The local item then evolves independently. An explicit refresh
creates another snapshot and a proposed revision; it never overwrites local
priority, graph edges, claims, evidence, or completion. Memories derived from
mutable external state cite the relevant snapshot hash so their basis remains
reproducible after the source changes or disappears.

### 9.4 Adapters & phasing

- **V1 local:** no external adapter is required. SQLite is canonical and the
  work graph is fully functional in `local` durability mode.
- **V1 portability/compatibility:** previewed, round-trip Beads snapshot
  import/export; deterministic work-graph recovery snapshot/restore;
  sequential portable publish/handoff/restore with cadence, lag reporting,
  head CAS, and divergence refusal; a dummy publication adapter proving
  frozen-payload idempotency with no external side effects. The dummy adapter
  remains explicitly side-effect free.
- **Later optional modes:** live concurrent `Sync`, real GitHub/Jira/
  proprietary intake and publication, comments, and link-backs. Automatic
  tracker mirroring and autonomous reprioritization remain non-goals.
  Outbound actions require explicit user authority or a bounded delegation and
  durable idempotency receipts.

### 9.5 Finalization & the report contract

Report finalization is an optional path after local work completion (§2.6).
It consumes the run's immutable `CompletionSeal`; it never quiesces or drains
execution again. The root seal already proves every expected contributor
supplied a required child seal or authorized omission, reconciled every action
outcome, released or transferred resource leases, contributed, and satisfied
acceptance, and closed every obligation applicable at the exact completion cut
with a bound terminal resolution—or an attributed, audited waiver by a
project-bound session records the omission. New seals also cite the exact
bounded environment-evidence hash set
at that cut; the component objects remain separate canonical evidence.
Reopening the root before report
freeze supersedes that run and aborts assembly; reopening after `report_ready`
requires a superseding report rather than mutating frozen bytes.

After the barrier, Engram creates a `ReportAssembly` anchored to the root
`CompletionSeal`. The designated finalizer holds a fenced
`ReportAssemblyClaim`; this authority is distinct from the terminalized work
claim and released execution/resource leases. Its finalizer grant binds the
seal hash, assembly generation/revision, and assembly-claim fence and is
restricted to deterministic report assembly and polishing. It cannot
authorize ordinary execution mutation. Only then is the report **frozen at
`report_ready`**—an immutable object with a `report_hash`—and the assembly
claim terminalizes. If publication is requested, a separate immutable intent
binds those bytes to a target and idempotency key.

Publishing hands the frozen bytes to a `PublicationAdapter` under that key;
failures retry the identical payload, and only an adapter receipt marks the
report `published`. Local work remains completed regardless of publication
state. A revised report is a superseding version under a new intent
and key — the pairing of one immutable payload with one idempotency key is
what makes "retry" a safe word. Session-end distillation may update local
working memory at any time, but nothing reaches the external system except
through this explicit step.

The report contract, in order:

1. Outcome / summary
2. Work performed
3. Decisions and rationale
4. Constraints and conventions discovered
5. Validation and evidence
6. Unresolved risks, blockers, follow-ups
7. Durable-memory promotion candidates
8. Provenance: local root/work/run ids, memory/version and participant-contribution
   hashes, timestamps, actors, assurance, and any finalization waivers

The report **cites the local memory and version IDs it was distilled from**,
so the local record can always explain the published artifact.

> **A reporting boundary, not truth promotion.** Facts and constraints
> discovered during a task appear as report sections and as *promotion
> candidates* — they never silently become global or org memory. Promotion
> back into durable memory follows the ordinary write policy (§5).

**Retention after completion or publication** is configurable. Completed local
work remains durable without a publication target. When publication is
requested, retain source memories through the confirmed receipt plus a grace
period, then compact according to policy to the final report, provenance
index, and tombstones. No source state is auto-deleted merely because no
external adapter exists.

## 10. Evaluation & telemetry

Retrieval quality is part of the product, not later polish. V1 ships with an
evaluation harness:

- **Golden query set** maintained alongside the memory corpus: task
  descriptions → the memories that should surface.
- **Metrics:** constraint coverage (must be 100% — this one is an invariant,
  not a target), precision@k and recall@k for the index tier, wrong/stale
  memory rate, cited-in-answer rate, packet bytes. **Precision before
  recall:** a plausible wrong memory silently corrupts work; a visible miss
  just prompts a search.
- **Retrieval decision logs** without sensitive bodies: packet hash,
  candidate ids considered and included, scores and reasons, budget
  exclusions, and whether the agent used or cited each item. This is the data
  that turns budget defaults (§4.4) from guesses into tuned values.

## 11. Delivery plan

| Phase | Contents |
| --- | --- |
| **v1** | Rust core; stable project-id keyed active-host SQLite store (append-only canonical objects, WAL, multi-process access) with derived FTS5 tables; first-class local work items/root executions/single-executor runs, parent forest + combined completion-dependency DAG, assignment, priority, labels, deferral, derived ready views, fenced work claims distinct from resource leases, evidence-gated completion, human decision objects, and the six-operation ambient agent protocol; one-verb memory capture; context packets with fail-closed pinned tier, omission manifest, content hash, typed source-feed vectors, per-session delivery positions, peer deltas, and policy/admission epochs; deterministic turn admission and typed recovery; scoped resource leases, handoff, contributions/child seals, separate fenced report assembly and optional publication; single-use action grants and crash-safe receipts; deterministic recovery snapshot/restore, sequential portable push/handoff/restore with writer-epoch validation, closed shared-state projection, and divergence refusal, plus round-trip Beads compatibility; audit attribution at asserted-runtime-context assurance; visibly labeled no-op Redactor; CLI + agent MCP + host-private control transport over one core; integrity/preflight and hostile-process tests; `doctor` / explicit projection repair. |
| **v1.x** | Session-end distillation into working memory (proposer + dedup); episodic compaction; completed-work retention compaction; budget and ready-ranking tuning; optional configured external backup automation. |
| **v2+** | Optional live cross-host `Sync`/team backend; real GitHub/Jira/proprietary source and publication adapters; optional embeddings; comments/link-backs; real Redactor/DLP; Postgres/service `Store`; Signer-based attestation; envelope encryption for crypto-shredding. |

> **Scope discipline.** V1's riskiest cut is doing too much. Everything in v1
> serves one loop: open or import local work → decompose/select ready work →
> coordinate concurrent sessions under enforced turn/action preconditions →
> complete with evidence → optionally freeze/publish a report → review
> promotion candidates. Cross-host sync and real external adapters widen that
> loop but are not dependencies of local execution.

## 12. Decisions

The Draft 0.2 open questions, resolved by Greg on 2026-08-23 (relayed via
Codex::AgentMemory):

| Question | Decision |
| --- | --- |
| Name | **Engram** — settled (binary: `engram`). |
| Implementation language | **Rust.** |
| External adapters | Optional on intake, durability, and publication. V1 targets deterministic recovery plus sequential portable handoff, keeps a dummy publication adapter, and targets round-trip Beads compatibility; no proprietary integration is required (§9.4). |
| Identity source | Proprietary runtime context: instruction/authority arrives as text through the tools and skills in use — asserted context, not cryptographic identity (§7). No SSO/LDAP in V1. |
| Cross-host storage / team scope | V1 starts local and adds optional sequential `portable` handoff for one active host. Concurrent team scope remains deferred with its design preserved (§3.3). |
| Redaction backend | None selected. Port + safe defaults ship; the no-op development implementation is visibly labeled and implies no compliance assurance (§7). |
| Architecture refinement | Local work graph plus dual-layer memory/report model; final report and publication are optional state machines after local completion (§1, §2.6, §9.5). |
| Multi-session operating model | Normal in V1 on one host. Root-work memory is shared by default; agent scratch is private. Each `WorkRun` has one ordinary executor/claim, parallel sessions claim distinct children under a `RootExecution`, and claims/leases, typed source/delivery positions, contributions, child seals, and a completion barrier are V1 primitives. |
| Product seam | Engram owns host-local work from creation/decomposition through completion. External intake, storage/sync, and publication are independent optional ports. One capture generates work delta, handoff, evidence, and report inputs. |
| Behavioral control | Engram decides task-bound readiness and capability eligibility; the host enforces those decisions at turn and declared tool boundaries. MCP alone is advisory. Effective authority is the intersection of Engram and user/host policy (§2.7, §8.3). |

### Still open

- Default grace period for post-publication retention (§9.5) — pick during V1
  implementation.
- Trigger for sequential portability (§3.2) has fired: cross-machine handoff
  is a V1 target. Concurrent sync (§3.3) remains deferred until two hosts must
  coordinate live; same-host sessions do not trigger it.
- Timing of the proprietary tracker adapter — when work authorizes real
  publication.
- Recovery snapshot format and recovery-point defaults for optional
  `local_backed_up` mode; portable push cadence and first transport substrate
  (recommended: a private dedicated Git ref, not a branch; internal object
  storage for organization scale).

## Appendix A — Decision log

How this spec was produced: both authors designed independently against the
same brief, merged via a structured mailbox exchange, hardened in a review
round, and revised to Greg's decisions as they landed. Positions and
outcomes:

| Topic | Fable v1 | Codex position | Merged outcome |
| --- | --- | --- | --- |
| Typing | Four species, behavior bound to type | Orthogonal kind / authority / delivery axes; `decision` first-class | **Codex**, plus Fable's derived-default mapping so the simple mental model survives (§2.2) |
| Identity | Single record, edited via supersedes | Stable id + immutable content-addressed versions with parents | **Codex** (§2.1, §2.4) |
| Storage | SQLite canonical, JSONL export | Git object store canonical; SQLite as disposable derived index | **Codex** (Draft 0.2); superseded by Greg's local-first V1 (SQLite canonical, Git deferred, §3) |
| Retrieval shape | Three rungs, hard budgets, titles index | Agree; add fail-closed pinned tier, omission manifest, packet hash/explain | **Fable** structure + **Codex** hardening (§4) |
| Write policy | Trust follows priority; distillation proposes only | Refine by origin × authority; evidence-backed agent writes activate | **Both** — merged matrix (§5) |
| Lifecycle clocks | Single `review_by` | Separate `review_by` from `valid_until`; stale ≠ expired | **Codex** (§6.1) |
| Conflicts | Supersedes chains | Contested state, `contradicts` edges, multi-parent resolution; no LWW | **Codex** (§6.3) |
| Forgetting | Forget = delete | Tombstone vs. physical purge, both audited | **Codex** (§6.5) |
| Compliance | Provenance fields in schema | Full audit set: actor identity, Redactor port, sensitivity labels, vault refs, safe exports, optional Signer | **Codex** (§7) |
| Ticketing | Never mirror; adapter port; read-only first | Agree, but snapshot mutable sources for reproducible provenance | **Fable** boundary + **Codex** snapshots (§9) |
| Evaluation | — | Golden queries, precision-first metrics, retrieval decision logs, in v1 | **Codex** (§10) |
| Round-2 hardening | Draft 0.1 as adjudicated | Four clarifications: fail-closed pinned contradictions; canonical-bytes contract (JCS); identity assurance not overclaimed; purge realism vs. Git history | **Codex**, all four adopted (§4.1, §3.1.1, §7, §6.5) → Draft 0.2, ACKed by both authors |
| Greg's decisions | Round 3 — name Engram; Rust; proprietary tracker → `DummyTrackerAdapter` in V1; runtime-context identity, no SSO/LDAP; no DLP backend selected; no team scope in V1 (Greg, relayed via Codex::AgentMemory) | | Recorded (§12) → Draft 0.3 |
| Dual memory / report model | Greg: local working memory while work runs; polished final report published to the tracker at finalization. Codex elaboration: SQLite canonical in V1; Git store deferred, not rejected; finalization state machine; report contract with cited memory/version IDs; configurable post-publication retention | | Adopted (§1, §2.6, §3, §9.5) → Draft 0.3 |
| Round-4 correction | Failure transition returned to `finalization_pending` | Freeze the report at `report_ready`: immutable report + hash bound to the idempotency key; failure retries identical bytes, never re-enters distillation; revision = superseding version + new intent. Endorsed keeping §3.1.1/JCS in V1 | **Codex**, adopted (§2.6, §9.5) → Draft 0.4, final ACK by both authors |
| Round-5 product test | Greg asked whether the authors would actually use Engram and clarified that multi-session work is normal. Fable identified capture ceremony, double entry, claims, and peer visibility as adoption blockers. | Codex separated packet hash from ordered event cursor, added leases/recovery, finalization barrier, and stable project identity across worktrees. | **Both**, joint position confirmed: narrow Engram to concurrent execution memory with three visibility rings, one-write-many-views, deterministic report assembly, and the backlog/execution seam (§1–5, §8–9) → Draft 0.5 |
| Round-6 control correction | Greg identified the missing behavioral and coordination layer and asked Engram to control it. Engram::Opus independently argued that the hot path should mediate by inlining fresh context, with refusal as a narrow tail; semantic capture must not gate edits; enforcement needs observe/replay evidence and honest coverage. | A voluntary memory loop is not control. Separate deterministic decisions from host enforcement; add recovery/finalizer grants and a stable quiescence cut to avoid refusal/finalization deadlocks; mediate declared material capabilities with scoped fenced leases, checkpoints, and crash-safe receipts. | **Adopted:** Engram becomes the task-bound behavioral/coordination decision plane while the host remains actuator/reference monitor. Normal pre-turn grants inline delivery; capability-specific failure policy, observe-first rollout, and advisory/turn-gated/action-gated assurance keep the claim honest (§2.7, §8.3) → Draft 0.6 |
| Round-7 local-work correction | Greg required Engram to replace Beads for local work: external injection and publication are both optional because heavy external systems do not scale to execution-time decomposition. Engram::Opus endorsed local ownership, claim/lease separation, derived readiness, and optional boundaries, while challenging durability, assignment, agent ceremony, root memory scope, and authority escalation. | Add a first-class work graph and six-operation ambient model protocol; keep assignment, claim, and resource lease distinct; bind grants to work/claim/lease/context fences; make completion evidence-based; separate publication; expose explicit durability modes. | **Adopted with Greg's clarification:** SQLite is immediately authoritative in valid `local` mode; optional durability improves over time but is never a prerequisite for local execution. Honest mode claims, restore/integrity tooling, and round-trip Beads compatibility bound the replacement promise (§2.6, §3.4, §8, §9) → Draft 0.7. |
| Round-7 independent rereview | Engram::Opus accepted Greg's optional-storage clarification but found stale durability-gate prose, run-owned memory that would disappear on reopen, ambiguous sparse/global cursors in safety CAS, focus/claim coupling, and completion/report drain ordering. | Preserve memory on the root work item across runs; use dense named project/root/run source feeds plus a separate per-session delivery sequence; make focus navigation-only; put draining in `CompletionSeal` and make optional report assembly consume it. | **Adopted before implementation.** The rereview identified no additional P0/P1 category beyond these exact corrections (§1.2, §2.6–2.7, §3.1, §8–9). |
| Round-7 Codex verification | Independent read-only verification found four implementation-blocking ambiguities: finalizer authority survived a terminal work claim, participant plurality conflicted with a singular run claim, scalar cursors lacked feed identity, and separately acyclic hierarchy/prerequisite graphs could still create a completion deadlock. | Add fenced post-completion `ReportAssemblyClaim`; make one executor own each child `WorkRun` under a root aggregate; type source feed positions separately from session delivery positions; cycle-check explicit prerequisites plus implicit required-child completion edges as one graph. | **Adopted before implementation.** No external storage is required by any correction (§2.6–2.7, §3.1, §9.5). |
| Round-8 portability correction | Greg stated that Engram starts local but must persist remotely, that he moves between machines, and that repository scale may reach hundreds of developers. He agreed to a canonical human-readable work projection and proposed a branch. | Engram::Opus separated restore-only backup, sequential portability, and concurrent sync; recommended a dedicated plumbing ref rather than a branch/working tree, scheduled push with visible lag, divergence refusal, sensitivity filtering, and no transfer of live claims/leases. | **Adopted as the transport-neutral contract:** optional V1 `portable` mode has one active host, explicit handoff/restore, head CAS, and no live-authority transfer. A private dedicated Git ref is the recommended personal transport; organization-scale substrate remains a product choice (§3.2–3.4, §7, §9.2–9.4) → Draft 0.8. |
| Round-8 portability verification | Independent Codex verification and Engram::Opus both found that push-time CAS alone did not protect restore/startup and that sensitivity-filtered content-addressed projections could sever object/feed references. | Add writer-epoch release/acquire, exact-base restore, bounded startup/resume validation, and honest forced-takeover detection; require executable shared-state closure, separately hashed exclusion stubs, dense feed placeholders, export pass-or-exclude, coverage diagnostics, and policy-hash equality on acquire. | **Adopted before portable implementation.** Steps 1–4 remain independent of remote storage; the portable step must satisfy these conformance rules (§2.7, §3.2, §7, §9.2–9.4). |

## Appendix B — Beads verdict

Both authors studied [Beads](https://github.com/gastownhall/beads) (Codex
read the memoryops, prime rendering, merge settlement, and compaction code;
Fable analyzed the docs and its observed behavior in production use). Shared
The earlier conclusion was "don't clone it—borrow its operational discipline,
replace its memory model." Draft 0.8 supersedes the product boundary behind
that sentence: Engram now also owns local work. It still does not copy Beads'
implementation or make Beads a dependency; it must cover the useful local
workflow while integrating task state with typed memory, evidence, and
behavioral control.

**Borrow:** an explicit canonical source of truth distinct from
export/interchange formats; local/offline-first operation;
collision-resistant ids for concurrent writers; task state kept separate from
persistent note memory; typed graph edges with behavior
(supersedes / duplicates / derived-from); immutable audit/change history;
assignment distinct from live claim, priority/labels/deferral, ready and
blocked indexes, acceptance criteria, round-trip migration, dry-run preview before anything destructive; a session-start context packet
with explicit caps and visible omission; human and machine interfaces over
one core.

**Reject:** string-key/string-value memory as the domain model (observed
effect: users hand-encode type, author, date, and provenance in prose);
injecting the whole memory plane every session (observed effect: 11 memories
≈ 11.5 KB at every session start, growing linearly, already truncated by
hosts); substring-only search and alphabetical selection under caps; same-key
remote-wins merge — convergent but silently lossy (Engram makes the same
situation a visible contested state instead); no scope, evidence, confidence,
sensitivity, validity, or usage signals; coupling the project to Dolt;
"memory decay" that summarizes closed issues while durable memory notes have
no lifecycle at all.
