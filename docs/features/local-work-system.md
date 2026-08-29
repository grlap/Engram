# Local Work System

> Normative references: [spec §2.6](../spec.md#26-local-work-graph--execution)
> and [spec §9](../spec.md#9-local-work-reports--external-systems).
> Related briefs: [behavioral control plane](behavioral-control-plane.md),
> [local tasks & reports](local-tasks-and-reports.md),
> [tracker adapter](tracker-adapter.md),
> [CLI & MCP](cli-and-mcp.md), and
> [execution pipeline](execution-pipeline.md).

Engram's target is a first-class, host-local graph of work. A user or model can open
work directly, split it into smaller units, express prerequisites, find ready
work, claim and hand off it, attach evidence, and complete it without any
external tracker. This is the local system of record for execution, not a
cache of Beads, GitHub, Jira, or another backlog.

External systems are optional at every boundary:

```text
                         optional immutable intake
 human / model prompt  ─────────────────────────────┐
 Beads / GitHub / Jira ─ snapshot + provenance ────┤
                                                    ▼
┌──────────────────────────────────────────────────────────────────┐
│ Engram core                                                      │── optional publication
│ local work graph · memory · coordination · behavioral control    │
│ evidence · report/finalization · canonical local store           │◄─► optional backup/portable/sync
└──────────────────────────────┬───────────────────────────────────┘
                               │ local control protocol
                               ▼
                     Host Enforcement SDK
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

## Product boundary

| Component | Owns | Does not own |
| --- | --- | --- |
| Engram | Local work graph, readiness, dependencies, decomposition, claims, execution memory, evidence, completion, and publication intents | Starting/stopping model processes or silently changing external systems |
| Host runtime | Model/session lifecycle, prompt delivery, tool interception, user approvals, and enforcement of Engram grants | Reimplementing work readiness or granting authority independently |
| Agent or human | Goals, judgment, proposed plans, evidence, and choices allowed by current authority | Self-minting claims, leases, grants, waivers, or publication permission |
| External adapter | Explicit source snapshot, optional backup/portable/sync, or separately authorized export/publication | Becoming an undeclared dependency or continuously mirroring tracker state |

The host may choose a model and start a process. Engram determines which local
work is ready and whether a selected session may act on it. Process scheduling
remains outside Engram; work scheduling belongs inside it.

## Durable concepts

### Work item

A `WorkItem` is a stable local planning identity. Its current view is derived
from immutable events and includes:

```text
WorkItem {
  work_id, short_ref, project_id, root_id, parent_id?
  title, outcome, acceptance[]
  kind, priority, labels[], assigned_to?, deferred_until?
  origin: local | imported
  source_snapshot_id?
  authority_policy_ref
  revision, created_by, created_at
}
```

`short_ref` is human- and model-friendly display syntax; `work_id` is the
stable collision-resistant identity. Titles, outcomes, priority, and
acceptance criteria change through attributed revision events rather than
in-place history loss.

`parent_id` expresses decomposition. A child is part of its parent's outcome;
it is not automatically a prerequisite of every sibling. Each child is
`required` or `optional` for parent completion. Parent and child remain
independently claimable work units.

Assignment is durable planning intent ("this actor should take this later").
It is neither a live work claim nor resource authority. Priority is an
explicit integer ordered by project policy and is user/policy-authorized by
default; models do not silently reprioritize the backlog. Work kind and labels
are typed/indexed fields, and children inherit configured labels.

### Root execution and work run

A `RootExecution` is the aggregate execution generation for one root work
item. It owns the expected contributor roster, membership of current child
runs, required child `CompletionSeal` hashes, grant-backed waivers for
cancelled or superseded required children, root-level decisions and other
waivers, and the root completion barrier. It does not own working memory.
Each capture retains its focused `WorkItem` as the provenance subject, while
shared applicability is keyed to that item's stable root. It therefore
survives reopen and replacement execution generations and is visible from
sibling or descendant focus in the same root.

A `WorkRun` is one execution generation for exactly one work item. In V1 it
has exactly one ordinary executor and at most one live `WorkClaim`. It owns
that executor's ordered execution feed, checkpoints, evidence, resource
leases, and completion state. Parallel sessions execute distinct child work
runs under the same `RootExecution`; they do not share mutation authority for
one run. Root members that do not claim the focused run may inspect permitted
root memory and communicate, but cannot receive an ordinary mutation or
completion grant for that run. V1 permits at most one active run per work
item. Reopening completed work creates a new run instead of reviving stale
grants, claims, or evidence from the previous generation. Every claimed
mutation also rechecks that all ancestors remain open and that the run still
belongs to the one active `RootExecution`; a completed root therefore fences
unfinished optional descendants even when their item lifecycle remains open.

This separates durable planning identity from live execution authority. The
current implementation's `task_id` maps temporarily to a run; new protocol
records bind both `work_id` and `run_id` during migration.

Memory scope does not recursively walk arbitrary ancestors. A shared
`Scope::Work` records the focused work id for provenance and feed routing, but
its read applicability is the verified root id. Private `Scope::Agent { work
}` scratch remains exact-item and owner-only. This lets reopen preserve shared
decisions and constraints while resetting execution state, lets sibling work
consume common root guidance, and prevents private scratch from leaking
across sibling focus. Explicit contradiction edges carry task and/or verified
work-root anchors, so two applicable pinned work records fail packet
construction just as pinned task records do. Work-anchored contradiction
events enter the project/root/current-run feeds, are delivered as typed
`work_next` changes, and remain part of feed-integrity verification.

### Work source snapshot

A `WorkSourceSnapshot` records adapter kind, canonical external reference,
captured time, source revision/fingerprint, projected fields, and canonical
payload hash plus bounded extension data. Import activates only a typed,
hash-verified canonical snapshot, maps selected fields into a new local work
revision, and keeps the snapshot as provenance. Refresh creates a proposed
local revision; it never overwrites local state or implicitly
reopens/completes work.

### Claim and resource lease

These are deliberately different:

- A **work claim** reserves responsibility for a work item or run. It prevents
  duplicate execution and supports assignment, heartbeat, handoff, expiry,
  and recovery. It does not authorize file or external mutation.
- A **resource lease** is fenced authority over canonical path or logical
  subjects in the project namespace. Conflict detection and fence continuity
  span every task in that project, while the lease's task id remains binding
  and audit metadata. A mutation grant must bind the live lease fences. It says
  nothing about whether the overall work item is complete.

A session normally needs the work claim before an ordinary execution turn and
the relevant resource lease immediately before mutation. Shared analysis may
need a claim but no exclusive resource lease. Root-level observation and
communication may instead use `RootExecution` membership; membership never
authorizes mutation or completion of another executor's run.

### Completion seal and report assembly claim

Every completed run produces an immutable `CompletionSeal`. Root completion
additionally binds every required child seal or an explicit
`CompletionWaiver`-authorized omission with the child's exact disposed
revision, plus the `RootExecution` roster, contributions, decisions, and
attributed waivers. Cancelling or superseding a required child never satisfies
the barrier by itself or through an unrelated replacement. Sealing terminalizes
the run's `WorkClaim` and releases or transfers every dependent
`ResourceLease`; completed execution authority is never kept alive for report
work.

Optional report assembly therefore uses a distinct post-completion authority:

```text
ReportAssembly {
  assembly_id, root_id, root_execution_id, completion_seal_hash
  generation, state, revision
}

ReportAssemblyClaim {
  claim_id, assembly_id, generation, holder
  expires_at, revision, fence
}
```

The assembly claim is not a work claim or resource lease and does not permit
ordinary workspace mutation. A finalizer grant binds the completion-seal
hash, assembly generation and revision, and live assembly-claim fence.
Handoff, expiry, and recovery advance that fence. Reaching `report_ready`
terminalizes the claim and freezes the report bytes.

## Lifecycle and derived readiness

Engram does not squeeze planning, availability, execution, and publication
into one status field.

The shipped alpha lifecycle is:

```text
proposed -> open -> completed
              |\-> cancelled
              \--> superseded

completed --reopen--> open with a new WorkRun generation
```

The target controlled-completion lifecycle inserts `completion_pending`
between `open` and `completed` only when Engram must drain mediated actions or
linked resource leases. That state is not emitted by the shipped zero-linked-
state completion path.

Availability is a derived projection over the open item:

- `ready`: admitted, not deferred, required prerequisites complete, required
  parent constraints satisfied, and no active claim.
- `claimed`: a live claim exists but no first execution checkpoint has landed.
- `active`: claimed and execution has checkpointed progress.
- `blocked`: at least one typed blocker remains. Blockers may be a prerequisite
  work item, a required human decision, missing external input, policy, or a
  manually recorded condition.
- `deferred`: a future wake time or explicit wake condition is active.
- `waiting`: work is intentionally waiting for a named event while retaining
  responsibility; unlike `blocked`, it is not advertised for reassignment.

Operational indexes cover open/closed/proposed work, assignments, labels,
blocked/stale/orphaned items, statistics, and preflight integrity. Deferral
has an explicit time or event wake condition; reaching it only recomputes
readiness and does not auto-claim or start a process.

Completion is local and final for that run. Report readiness and external
publication are separate projections; a work item can be completed with no
report or target, and publication failure never makes completed work active
again.

Every completed run has a `CompletionSeal`: accepted work revision, run and
claim fences, dense completion-cut position, executor checkpoint state,
reconciled action outcomes, released/transferred resource leases, acceptance
results, and evidence hashes. The shipped `work_complete` requires the linked
action-outcome and resource-lease drain sets to be empty, terminalizes the work
claim, and seals atomically. A root seal also consumes each required child seal
or explicit grant-backed disposed-child waiver and all root contributions.
Before that seal can land, every descendant claim and handoff offer must be
released, completed, cancelled, or expired. Unfinished optional children are
recorded in the seal and remain non-executable audit records under the closed
root. They must be disposed leaf-first before the completed root can reopen;
the new root execution never adopts their old runs implicitly.
Nonempty drain/reconciliation will use the planned `completion_pending`
protocol; it is refused today rather than silently accepted. Optional report
assembly consumes the root seal under a
`ReportAssemblyClaim` and never performs a second execution drain.

## Graph invariants

Every graph-changing command is one SQLite transaction with an expected work
revision and idempotency key.

- Parent/child edges must form a forest. Explicit prerequisite edges plus the
  implicit completion edge `parent requires required-child` form one
  **completion-dependency graph**, which must remain acyclic. Every hierarchy,
  required/optional, or prerequisite mutation checks that union in the same
  transaction. Optional children create no implicit completion edge. The API
  names explicit edge direction as `work requires prerequisite`; it never
  exposes an ambiguous `blocks(A, B)` verb.
- Required children must be completed or explicitly waived before their
  parent can complete. Optional children may remain open but are surfaced in
  the completion receipt. Root sealing refuses live descendant claims or
  handoff offers, and root reopen refuses unresolved open descendants, so an
  old child run can never cross into the next root-execution generation.
- A completion binds the accepted work revision, run generation, claim fence,
  latest checkpoint cursor, acceptance results, and evidence hashes. Any
  change to those facts makes an unconsumed completion decision stale.
- A run has one ordinary executor and at most one live ordinary work claim. A
  session may inspect or hold claims on multiple items under policy, but each
  ordinary turn grant binds exactly one focused, claimed work item. Parallel
  executors claim distinct child runs. Changing focus never releases a claim.
- Claim handoff and recovery increment a monotonic claim fence. Old sessions
  cannot complete or mutate after transfer even if their process resumes.
- Work created below a parent inherits its project, root, sensitivity floor,
  authority ceiling, non-waivable constraints, and publication restrictions.
  A child cannot relax its parent.
- Decomposition requires the parent's claim or a bounded planning delegation.
  Model-created non-leaf work is proposed by default; evidence-backed leaf
  children within the parent's authority and decomposition budget may activate
  immediately. A one-child "decomposition" revises the parent instead.
- Root completion, waivers, cancellation, reopen, policy changes, and external
  publication require explicit authority named by policy. Model identity by
  itself grants none of them.
- Exact duplicate creation is prevented by idempotency. A normalized
  parent/outcome fingerprint surfaces likely semantic duplicates before
  admission; it warns or creates a proposal rather than silently merging.
- Decomposition is bounded by policy: maximum depth, children per atomic plan,
  and open descendants per root. Hitting a bound returns a typed directive to
  consolidate or request an attributed override.
- Cross-project hierarchy and prerequisites are out of V1. An external or
  cross-project dependency is represented as a typed blocker with provenance,
  not a fake local edge.
- A resource lease can be issued only to a session with a live claim covering
  that work. Releasing, handing off, or recovering the claim increments its
  fence and revokes or transfers its dependent leases transactionally.

Every admitted change appends a canonical `WorkEvent`. Project, root-work, and
run-execution feeds each allocate a dense per-feed position in the event
transaction; delivery pages have their own dense per-session sequence. A
position is always carried with its feed kind and id. A delivery position is
separate from the vector of source-feed positions represented by that
delivery. A global database row id is not a cursor. Object hashes reproduce
content but never order changes.

## Agent-native protocol

Models are primary protocol users, so the surface optimizes for few calls,
bounded responses, stable reason codes, and no redundant identifier shuttling.
The host supplies the bound project, session, actor context, and current work
where unambiguous. A model receives short references and only supplies an
explicit id when changing focus or referring to another graph node.

The hot agent protocol has six operations:

| Operation | Purpose |
| --- | --- |
| `work_next` | Return selected compact focus, ready, catalog, and change sections under a 12 KiB ceiling; an exact staged change-summary page replays until explicitly acknowledged |
| `work_focus` | Select/inspect one item as the ambient binding and return bounded acceptance, relations, memory index, history count/tail, and allowed-next state; never claim or release implicitly |
| `work_propose` | Open a root or atomically create a bounded decomposition and prerequisites; each result is active, proposed, duplicate, or refused |
| `work_update` | Apply a typed union such as claim/release, checkpoint, block/unblock, defer, revise, assign, or dependency change to ambient work |
| `work_complete` | Evaluate acceptance and complete ambient work under current revision/run/claim fences; an optional capture records evidence and its final checkpoint in the same high-level call |
| `work_handoff` | Couple an outgoing checkpoint to an offered/accepted claim transfer |

This six-operation slice is shipped through one `LocalWorkService` used by
both CLI and MCP. The ambient SQLite row binds only project, session, focused
work, and the processed project-feed cursor. It never stores authority. A host
or operator starts the service with one immutable `WorkAuthorityGrant` hash;
every mutation resolves that canonical grant and its live revocation marker
inside the lifecycle transaction. `work_focus` accepts a short ref or UUID,
while update, completion, and handoff infer the current revision, run, claim,
fence, evidence set, and unique matching offer. `work_next` exposes an optional
section selector over `focus`, `ready`, `catalog`, and `changes`; excluding
`changes` performs no delivery staging. For change delivery it verifies the
canonical source objects, projects explicit compact summaries, and stages only
the largest dense prefix that fits the change byte budget. Full canonical
snapshots and memory bodies are not ambient protocol payloads. Each summary
retains its source position and hash, but the source hash intentionally does
not bind the summary bytes. Restricted and out-of-focus entries retain their
positions as typed omission markers. Planning/lifecycle events, checkpoints,
and evidence summaries are project-visible coordination state across roots;
work-memory summaries are visible only within the focused root, and exact-item
private scratch never enters the shared feed. It replays that same interval even if
concurrent sessions append more work, and
advances the confirmed cursor only when the caller acknowledges the exact
`delivered_through` value with the opaque `delivery_token` returned by that
page. Repeating an acknowledged-and-fetch call after a lost response replays
the newer staged page and its stable token rather than losing it. The tentative
cursor and token are host-internal until a page is actually returned; a
response with no change section has neither field. Every successful agent work
response is at most 12,288 serialized JSON bytes. Advisory truncation is
declared through a typed omission manifest, and catalog continuation points at
the last item actually emitted.

A session must clear a staged change page before an operation changes ambient
focus, because replay authorization depends on that focus. The safe recovery is
explicit: first call `work_next` with `changes` selected and no acknowledgement
to replay the pending page, then pass the `delivered_through` and
`delivery_token` actually received as `acknowledge_through` and
`acknowledge_token` while selecting sections that exclude `changes`, such as
`focus`. The typed `work_delivery_pending` refusal explains both calls but never
discloses the host-only tentative cursor or token, so a response lost after
staging cannot be acknowledged unseen or by guessing its cursor.
The delta interval is the authoritative delivery cut. Focus, ready, and
catalog sections are advisory refreshed views and may observe a newer
concurrent commit; lifecycle mutations always revalidate their revision,
claim, lease, authority, and canonical projection basis under the write lock.
The exact projected change page and its byte-budget omission count are stored
canonically beside the tentative cursor and opaque token. A restart or later
legacy-task rebind therefore replays the same page; only acknowledgement of
that exact cursor/token pair clears all four staged values.
Staging itself compare-and-swaps the confirmed cursor, empty pending slot,
focused work, and legacy-task binding under the SQLite write lock. Concurrent
calls for one session either return that one winning page or replay it; a focus
or task rebind that commits first forces projection to restart on the new read
basis.

All model-originated mutations require caller-stable idempotency keys. Their
durable attempts bind both caller intent and the exact focused work/claim/
handoff basis. A lost-response retry may replay a committed result, but an
interrupted attempt must revalidate live authority and cannot follow a changed
ambient focus into another work item.

`work_focus` is the explicit drill-down surface. It carries an exact history
event count and only the newest bounded event summaries, plus body-free memory
index entries. Active blockers include their id, type, and compact detail so an
agent can construct `unblock`; when exactly one blocker is active the id may be
omitted and Engram infers it. Authorized memory bodies remain available on
demand through their version hash. `work_update` and `work_handoff` never rebuild this history:
their success envelopes contain only the operation, compact receipt, current
obligations, and `allowed_next`, so hundreds of historical events cannot grow a
mutation response.

`work_complete` accepts either previously recorded evidence and checkpoint
state or an optional `capture { summary, refs }`. The capture form records one
generic evidence object, checkpoints the exact completion evidence set, and attempts
the seal as one model-level operation while retaining each durable lifecycle
event and fence check. All caller-controlled acceptance shape, satisfaction,
and evidence references are validated before either capture substep commits;
the completion transaction then revalidates the same rules against current
run state. If the process stops after evidence or checkpoint
commit, retry loads that canonical substep's original timestamp so its core
idempotency hash replays exactly; any still-uncommitted substep uses the retry's
current time and therefore cannot bypass an expired claim.

Typed `verification_evidence` and `environment_evidence` are different from
that generic capture. Only the host-private control checkpoint may mint them,
and each is bound to the exact root/work/run/claim fence and source revision.
Verification derives its result, check fingerprint, producer session, and
timestamps from a canonical execution observation; agent prose cannot promote
itself into verification. The agent protocol can only attach an existing typed
hash through
`work_update { kind: "evidence", attach: { evidence: <hash> }, ... }`.
Attach is a validated reference operation and never duplicates the canonical
object or its project/root/run feed entries. Focus and delta summaries expose
the typed kind and compact binding fields without granting the agent a minting
surface. A later mutation at the evaluated run-feed cut makes older
verification stale even when it came from another workspace with the same
previous content fingerprint.

Every source-changing execution observation also creates one immutable
built-in test obligation on the run, independent of action outcome and source
basis availability. Definitions and their later satisfied/waived resolutions
are direct dense feed objects; `work_run_obligations` is only their verified
query projection. Satisfaction is evaluated against the latest mutation at an
exact run-feed cut. A passed test for a later basis-bearing mutation may close
earlier open definitions, but a basisless latest mutation makes the open set
waiver-only until a newer basis-bearing mutation and passed test arrive.

Focus and delta packets expose bounded obligation identity, rule, requirement,
state, and evidence. Agents cannot mint or waive them. Waiver is restricted to
the host/operator `engram authority waive-obligation` CLI under a dedicated
`ObligationWaiver` authority operation; neither MCP nor `work_update` accepts
that operation, and agent projections redact its authority hash and reason.
The cut-aware open-obligation query is intentionally prepared for the A3
completion-seal gate rather than being presented as a shipped completion rule
in A2.

The operator-only `engram authority grant|revoke` commands are the current
host boundary for local use. They are deliberately absent from agent-facing
MCP. `engram mcp --work-authority-grant <hash>` fixes the grant for one MCP
process; `engram work --authority-grant <hash> ...` does the same for a shell
operation. Grant text remains asserted context in this slice, not
authenticated identity. The Host Enforcement SDK replaces manual process
binding and couples the work envelope to turn/action authority in the next
slice.

`work_propose` is the low-ceremony decomposition path: an agent can submit a
small plan in one call and either all children/edges appear or none do. The
decomposition receipt returns the complete ordered child identity set as
fixed-size `work_id`/`short_ref`/revision records plus an exact child count;
full child details are obtained by focusing a returned short ref. This keeps
even the maximum 64-child admitted plan below the agent response ceiling and
makes the exact durable replay returnable after restart.

Administrative CLI/query views additionally expose search, history, stats,
stale/orphan/preflight checks, approval decisions, import/export, and cursor
changes. They are indexes over the same core, not extra lifecycle verbs in the
normal model loop.

A normal `work_ready` candidate is compact:

```text
ReadyWork {
  work_ref, revision, title, outcome, priority
  why_ready[], acceptance_summary[]
  claim_requirement, resource_hints[]
  context_digest, context_changed_since?
  allowed_next[]
}
```

Ranking is deterministic and inspectable: authority-set priority, dependency
unblocking value, age, and stable id tie-break. Engram returns candidates and
reasons; the host or model chooses among them unless policy assigned one.
There is no unbounded startup injection of the full backlog.

The session's project, actor, current focus, and cursors are ambient. Updates,
completion, and handoff omit a work id unless intentionally changing focus.
`work_focus` is navigation only; `work_update { claim | release }` is an
explicit fenced authority transition and never happens as a side effect of
viewing another item.
Model-visible success is terse—often only changed obligations—while the host
retains the full durable receipt. Every refusal includes a stable code and a
satisfiable next action.

## Human authority and model autonomy

Authority is operation-specific, not a single `human`/`agent` trust bit.
A project policy names who may create roots, admit proposed children, change
priority, waive acceptance, cancel, recover claims, complete roots, and
publish externally. Actor text supplied by a host remains asserted context
unless a stronger mechanism verifies it.

The useful default for a local coding project is:

- a host may seed a user-requested root with an authority reference to that
  request;
- an agent may create bounded children, dependencies, checkpoints, and
  evidence within the root's inherited envelope;
- an agent may complete leaves when objective acceptance predicates pass and
  the completion records whether acceptance was self-attested or independent;
- root completion becomes a durable proposal for human approval by default;
  a bounded, expiring standing delegation may authorize autonomous root
  completion without changing the global policy;
- required-child waivers, destructive non-leaf cancellation/reopen, authority
  changes, and all external publication require explicit human authority.

Engram must show `allowed_next` and a typed recovery directive rather than
making a model infer permissions by trial and error.

`allowed_next` entries name the exact tool and tagged operation, for example
`work_update:claim`, `work_propose:decompose`, or `work_handoff:accept`.
`work_update:claim(recovery_reason_required)` means a different prior holder is
still unaccounted and the caller must submit the `claim` variant with an
attributed `recovery_reason`; the host grant must also include
`claim_recovery`. A prior contribution or persisted participant waiver makes
the holder accounted, so a successor receives ordinary `work_update:claim`
instead of being asked to waive the same omission twice.

`work_update:waive_required_child` appears only when at least one direct,
required, cancelled-or-superseded, not-yet-waived child is covered by the
current grant against that exact child target. `work_focus` carries a bounded
typed `waivable_required_children` list with the executable child short refs;
parent-scoped authority alone never produces false guidance.

The implemented authority boundary uses canonical host-installed
`WorkAuthorityGrant` objects. Agent-visible lifecycle and planning requests
carry only a grant hash; the SQLite transaction resolves that durable object
and checks project, policy, subject actor, assurance, operation, scope,
issuance, expiry, and the grant-owned planning budget. Children copy the
parent's authority policy exactly. Root and child creation events retain the
admission grant hash, and host revocation creates an immutable attributed
revocation object before all later live use fails closed, regardless of a
caller-supplied backdated operation time; future-dated revocations are refused.
Installing or revoking a
grant is a Host Enforcement SDK operation, not an MCP capability.

Until action outcomes and resource leases are linked to `WorkRun`, V1 accepts
only a grant-backed **zero-linked-state** completion-drain attestation. An
agent cannot complete by supplying arbitrary action hashes or lease names.
The later host-enforcement slice replaces that temporary empty attestation
with exact reconciled action and released/transferred lease projections.

## Behavioral-control integration

Work calls describe intent; the host-private channel carries authority.
Before a model turn, the Host Enforcement SDK resolves the selected work and
asks Engram for a grant. A grant binds:

```text
work_id + work_revision + run_id
claim_id + claim_fence
resource lease ids + fences
project policy epoch + work admission epoch
basis feed positions[] + session delivery position
capability envelope + expiry
```

`turn_begin` rechecks that basis immediately before prompt dispatch.
`action_authorize` rechecks it again for a material capability. A model-facing
MCP call can propose, query, checkpoint, or request a transition; it cannot
mint or consume the host's grant.

The SDK removes ceremony from the model loop:

1. `before_turn` revalidates the optional portable writer epoch when due,
   chooses or validates the bound work, synchronizes changed context,
   obtains/begins a grant, and injects one bounded `WorkEnvelope`.
2. `before_action` maps a host tool call to effects/resources and obtains a
   single-use action grant when required.
3. `after_action` records the minimal outcome receipt even if the model turn
   later fails.
4. `after_turn` persists the model's structured checkpoint and reconciles
   claims/leases before another turn.

Unchanged work context is represented by a small cursor/hash receipt, not
repeated prose. Refusals name the exact condition and safe recovery operation.
The TermAl adapter is the first full integration target; generic wrappers can
only claim coverage for processes and tools they actually mediate. Native
runtime adapters and the custom-agent library share the same conformance
suite and assurance labels.

No replayable grant token appears in an agent-visible MCP response. Grants and
their consumption stay on the private host channel; the model sees only
references, obligations, reasons, and allowed next operations. Human
authorization is a distinct attributed object or delegation, never a prose
`reason` field that the model can manufacture.

Work verbs do not add another admission round trip. Pre-turn delivery carries
focus, delta, obligations, and current fences in one bounded envelope; the
host uses that basis on the private channel. Material actions create outcome
metadata at the action boundary. Semantic checkpoints add meaning and
evidence, but mutation is never conditioned on the model first writing a
status sentence.

## Optional external intake, storage, and publication

Adapters have independent capability families:

```text
WorkSourceAdapter {
  capabilities()
  normalize_ref(input)
  fetch_snapshot(ref, projection) -> WorkSourceSnapshot
  search(query, cursor) -> [SourceCandidate]
}

BackupAdapter {
  put_snapshot(project, manifest, bytes) -> BackupReceipt
  get_snapshot(project, snapshot_id) -> RecoverySnapshot
  list_snapshots(project, cursor) -> [SnapshotMetadata]
}

PortableStoreAdapter {
  read_head(project) -> PortableHead
  fetch_snapshot(project, head_hash) -> RecoverySnapshot
  publish(project, expected_parent, active_snapshot) -> PortableReceipt
  release_writer(project, expected_active_head, released_snapshot) -> PortableReceipt
  acquire_writer(project, expected_released_head, active_manifest) -> PortableReceipt
  recover_writer(project, expected_head, recovery_intent, active_manifest) -> PortableReceipt
  validate_writer(project, writer_instance_id, writer_epoch) -> WriterValidation
}

PublicationAdapter {
  capabilities()
  publish_report(target, frozen_report, idempotency_key) -> Receipt
  publish_work?(target, work_projection, idempotency_key) -> Receipt
}
```

No adapter is required to initialize or finish local work. A configured
portable store supports sequential host handoff; a later live `Sync` backend
is distinct from both portable replication and snapshot backup. Import never grants
the external source authority to overwrite local execution facts. Publication
requires a frozen payload, a durable intent, an explicit target, an
idempotency key, and authority; retry sends identical bytes. Publishing work
state and publishing a final report are distinct optional capabilities.
Beads migration must be round-trip: previewed import and explicit export retain
ids, hierarchy, prerequisites, fields, and provenance well enough to return to
the source without silent loss. This is interoperability, not mirroring.

## Durability modes and integrity

Engram reports durability separately from work authority and from external
publication:

| Mode | Off-host copy | Writer model | Normal remote reads |
| --- | --- | --- | --- |
| `local` | No | Concurrent sessions on one host | None |
| `local_backed_up` | Verified restore snapshot | Concurrent sessions on one host | Explicit restore only |
| `portable` | Transferable working snapshot | Release/acquire enforces clean handoff; forced takeover is detected within a bounded validation window | Head/epoch validation plus explicit restore/handoff |
| `synchronized` | Shared working state | Concurrent hosts | Live synchronization |

All modes use the same local work semantics. External storage is optional,
and local-only mode is a valid source of truth when the user accepts its
recovery boundary. `portable` is the V1 cross-machine target: a configured
daemon or session hook publishes on a cadence and at clean session end;
`engram doctor` reports the last verified remote head, unpushed event/byte/age
lag, and a visible degraded state after failure. Moving machines is an
explicit project-level release/acquire around restore of that exact head. A
portable manifest binds its parent, consistent source cut, feed heads,
export-policy hash, writer instance, and monotonic writer epoch. Release
checkpoints/exits local sessions, makes unfinished claims recoverable, releases
leases, invalidates grants/delivery authority, CAS-publishes a `released`
manifest, and makes the old store mutation-read-only. Acquire CAS-publishes a
new active instance/epoch before enabling local writes. Crash takeover is an
attributed recovery. The remote is not read as a second live database during
active execution.

Restore/acquire refuses unless its destination is empty or exactly at the
expected manifest with no unpushed local tail; a divergent destination is
preserved as a recovery bundle. Portable process/session start and crash
resume perform a bounded remote head/epoch validation before any mutation
grant, and a configured cadence revalidates it. That metadata check is writer
authority validation, not a remote work-database read. Mismatch, expiry, or
unavailability makes the store mutation-read-only and invalidates grants;
local-mode projects are unaffected.

A portable push compare-and-swaps the expected remote parent manifest. A
changed remote head is `portable_diverged`: Engram refuses to push or merge
and directs the operator to `engram portable reconcile`. Reconciliation
previews both immutable lineages and either continues one while retaining the
other as a recovery bundle/proposed import, or forks a new project identity;
it never silently renumbers dense feeds or drops a lineage. This preserves a
single dense feed sequence in portable mode. Live concurrent synchronization
must instead add per-origin ordering or a server sequencer and remains later.

Portable/backup payloads contain canonical shared objects, the local work
graph, feed ordering, evidence references, schemas, and a manifest. They never
restore a live `WorkClaim`, `ResourceLease`, control session, delivery state,
or grant. Immutable claim lifecycle facts may remain for audit, but an
unfinished old-host claim restores as `recoverable`; a new host performs an
attributed recovery and advances the generation/fence. Resource leases must
be reacquired. Inert lease lifecycle audit events may cross, but never rebuild
an active lease. Agent-private scratch never enters a portable payload.

The projection must be closed under executable shared-state references. Every
object required to rebuild the work graph, readiness, policy, root context,
acceptance/evidence, completion, and behavior-affecting feed history is either
included or release fails `portable_projection_incomplete`. Provenance-only
references into excluded content use a separately hashed `ExclusionStub` that
names the original hash/kind, reason, and export-policy hash without pretending
to be that object. Excluded non-semantic feed payloads leave typed placeholders
at their original dense positions. Export passes or excludes an existing
canonical object; it never rewrites its bytes under the old hash. `doctor`
distinguishes missing/corrupt from deliberately excluded, reports coverage,
and claims `portable` only for a complete shared-state closure. Acquire refuses
an export-policy hash mismatch. If policy forbids even stub metadata, the
result may be a marked-truncated backup but not a portable working store.

Before Engram claims Beads-equivalent off-host durability, it should ship:

- deterministic, human-readable work-graph recovery snapshots that can be
  committed or copied off-host without copying a live SQLite file;
- manifest hashes and a previewed restore path, exercised in CI;
- referential-integrity verification for work/events/edges/evidence plus
  projection rebuild-and-compare checks, not only canonical object hashes;
- crash tests proving event/cursor ordering and atomic packet/head snapshots;
- configured-backup/portable freshness surfaced by `engram doctor`; and
- a documented recovery-point objective for each durability mode, with no
  implication that optional report publication protects the local backlog.

SQLite remains the live canonical store on the active host in V1. A configured
recovery snapshot is a restore artifact; a portable head is a sequentially
transferable canonical projection and cannot be mounted concurrently as a
second writer. The reference Git transport should use a dedicated plumbing
ref such as `refs/engram/<project-id>/<scope>`, never a checked-out branch or
the working tree. The port also permits a private repository or internal
object store. At organization scale, Engram must not create hundreds of refs
in a shared code repository: execution state needs an access-controlled store
whose lifetime and privacy are independent of the code remote. Cross-host live
coordination remains a later optional mode.

## Beads replacement and interoperability

Engram must cover the daily local workflow before claiming Beads replacement:

| Beads workflow | Engram equivalent |
| --- | --- |
| `bd create`, parent/child | `work_propose` |
| `bd ready` | `work_next` with typed readiness reasons |
| `bd show`, search/list | `work_focus` plus query views |
| `bd dep add`, blocked | prerequisite edges and typed blockers |
| assignee vs. `bd update --claim` | durable assignment vs. fenced live claim; resource mutation still needs leases |
| notes/design/acceptance | typed work fields plus work-scoped shared/private memory and evidence |
| comments and handoff | one checkpoint feeding deltas, handoff, and report input |
| `bd close`, reopen, supersede | one-call evidence capture/seal when compact, or explicit evidence/checkpoint steps; audited reopen/cancel/supersede events |
| `bd remember` | Engram's typed durable memory, not a pseudo-task |
| `bd stats`, stale/orphans/preflight | rebuildable operational indexes and integrity diagnostics |
| Dolt cross-machine sync | V1 sequential `portable` handoff; later concurrent Engram `Sync` backend |

A Beads adapter imports selected issues as source snapshots and may optionally
publish an explicit projection. It does not make `.beads`, Dolt, or `bd` a
runtime dependency. The accurate V1 claim is "local-first Beads replacement
with optional sequential portability"; teams needing a concurrently writable
multi-machine backlog still need an external system or later Engram sync.

Work-targeted `memory_note` calls bind directly to the persisted local work
focus: shared notes enter project/root/run feeds and are root-visible to
participating peers, while private notes remain exact-item actor-local scratch
and never enter peer delivery. When a session is also task-bound, the caller
must explicitly choose `target: work` or `target: task`; Engram does not change
the capture scope implicitly. Work
focus returns a bounded, actor-filtered memory summary, so the work graph and
its execution memory do not require a legacy tracker task as an identity shim.
Every `memory_note` mutation requires a caller-stable `idempotency_key`; an
exact retry returns the first receipt and the same key with different prose is
a conflict.
Restricted-sensitivity bodies are omitted from task and work search/focus
views and remain unavailable through direct show in V1. `work_next` also
replaces restricted work memory and memory outside the currently focused root
with a typed omission marker, preserving its exact dense feed position without
exposing protected content.

## Delivery boundary

The smallest coherent implementation sequence is:

1. Define/migrate dense per-feed cursor identity, then add canonical work
   items/events, graph projections, local roots, hierarchy, prerequisites,
   ready queries, claims, and evidence-gated completion.
2. Bind the existing task/run, memory feed, resource leases, and turn grants to
   work revisions and claim fences.
3. Ship the six-operation agent protocol plus administrative CLI/query views,
   assignment, deferral, human decisions, and use Engram to track its own
   local implementation work.
4. Add the TermAl Host Enforcement SDK adapter and conformance suite; follow
   with the generic wrapper and native/custom integrations.
5. Add deterministic recovery snapshot/restore, scheduled portable push plus
   explicit cross-machine handoff/divergence refusal, integrity tests, and
   round-trip Beads import/export for migration and dogfooding.
6. Add optional report/work publication adapters. Revisit cross-host sync only
   when one project must coordinate live work concurrently across machines.

V1 does not include an autonomous planner, automatic external polling,
cross-project dependency solving, fairness scheduling, concurrent cross-host
sync, or an LLM-based duplicate/priority oracle. Models propose plans;
deterministic core rules admit, order, explain, and enforce them.

## Decisions still needed

The design can proceed with recommended defaults, but product policy must
eventually select:

- the scope and expiry defaults for standing root-completion delegations;
- the default decomposition depth/open-descendant budgets;
- the completed-work retention and compaction period when nothing is
  published;
- the recovery snapshot format and acceptable recovery-point objective;
- the first portable substrate (recommended: private dedicated Git ref, with
  internal object storage for organization scale); and
- the trigger and substrate for concurrent cross-host synchronization.
