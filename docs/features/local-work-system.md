# Local Work System

> Normative references: [spec §2.6](../spec.md#26-local-work-graph--execution)
> and [spec §9](../spec.md#9-local-work-reports--external-systems).
> Related briefs: [behavioral control plane](behavioral-control-plane.md),
> [local tasks & reports](local-tasks-and-reports.md),
> [tracker adapter](tracker-adapter.md),
> [CLI & MCP](cli-and-mcp.md),
> [write policy & review](write-policy-and-review.md),
> [security & trust](security-and-trust.md), and
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
  revision, created_by, created_at
}
```

`short_ref` is human- and model-friendly display syntax; `work_id` is the
stable collision-resistant identity. Titles, outcomes, priority, and
acceptance criteria change through attributed revision events rather than
in-place history loss. A corrupted or imported store that contains a short-ref
collision fails selection with a typed candidate list containing each full
work id, short ref, title, and lifecycle-backed `state`. CLI/MCP guidance names
up to eight ordered candidates, reports how many additional matches exist, and
requires the caller to retry with one full id; it never picks a candidate
implicitly.

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
runs, required child `CompletionSeal` hashes, reason-attributed waivers for
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

This separates durable planning identity from live execution authority.
Protocol records bind both `work_id` and `run_id`.

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

A successful note, update, evidence, checkpoint, or handoff by the current
holder slides the work-claim expiry to one hour after that mutation. Successful
completion instead terminalizes the claim at the completion timestamp. A
holder mutation on a lapsed claim is refused with one recovery command:
`engram work claim <ref>`. When the work is ready, that ordinary claim command
retakes the same holder's claim under the stable project/session binding,
advances the fence, preserves an active run, and needs no recovery reason.
Every handoff offer expires no later than its source claim. Taking over from a
different, unaccounted holder still requires attributed recovery.

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

Every new seal also declares completion-obligation schema V1 and records the
exact `(definition, terminal resolution)` pairs applicable at its pre-seal
dense run-feed cut. An open obligation refuses sealing before any terminal
work mutation. A required child seal is decoded and checked recursively, and
every accepted seal carries the current obligation-schema binding.

New seals also declare environment schema V1 and bind the exact sorted,
distinct set of environment-evidence hashes visible at the same dense cut.
The set is capped at 64 and contains hashes only: canonical toolchain,
sandbox/image, workspace, and capability-map components remain in their own
evidence objects. Every accepted seal carries the current environment-schema
binding. Environment identity is currently audit
evidence; the built-in test obligation does not yet require a particular
environment hash.

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
blocked/stale/orphaned items, statistics, and preflight integrity. The ambient
ready view ranks admitted work by priority, the number of open dependants it
can unblock, age, and stable id. Its candidate limit is applied in SQLite before
item projections are decoded. Catalog assignment and label keys use NFC plus
full Unicode case folding; trigram FTS covers title, outcome, labels, short
reference, and active-blocker detail. Deferral has an explicit time or event
wake condition; reaching it only recomputes readiness and does not auto-claim
or start a process.

Completion is local and final for that run. Report readiness and external
publication are separate projections; a work item can be completed with no
report or target, and publication failure never makes completed work active
again.

Every completed run has a `CompletionSeal`: accepted work revision, run and
claim fences, dense completion-cut position, executor checkpoint state,
reconciled action outcomes, released/transferred resource leases, acceptance
results, evidence hashes, and the exact terminal obligation basis. The shipped
seal also carries the exact bounded environment-evidence hash set at that cut.
`work_complete` requires the linked
action-outcome and resource-lease drain sets to be empty, terminalizes the work
claim, and seals atomically. A root seal also consumes each required child seal
or explicit reason-attributed disposed-child waiver and all root contributions.
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
- Claim handoff, same-holder retake, and foreign-holder recovery increment a
  monotonic claim fence. Old sessions cannot complete or mutate after transfer
  even if their process resumes.
- Work created below a parent inherits its project, root, sensitivity floor,
  authority ceiling, non-waivable constraints, and publication restrictions.
  A child cannot relax its parent.
- Decomposition requires the parent's claim or the ordinary project-bound
  planning path. Children activate under the same project lifecycle rules. A
  one-child "decomposition" revises the parent instead.
- Project-bound sessions may complete, waive, cancel, reopen, and recover
  local work; exception paths retain attributed reasons and immutable audit
  events. External publication still requires an explicit human decision, and
  an optional host control plane may independently mediate turns or actions.
- Exact duplicate creation is prevented by idempotency. A normalized
  parent/outcome fingerprint surfaces likely semantic duplicates before
  admission; it warns or creates a proposal rather than silently merging.
- Decomposition is bounded in code: maximum depth, children per atomic plan,
  and open descendants per root. Hitting a bound returns a typed directive to
  consolidate the plan; there is no grant or override token.
- Cross-project hierarchy and prerequisites are out of V1. An external or
  cross-project dependency is represented as a typed blocker with provenance,
  not a fake local edge.
- A resource lease can be issued only to a session with a live claim covering
  that work. Releasing, handing off, or recovering the claim increments its
  fence and revokes or transfers its dependent leases transactionally.

Every admitted change appends a canonical `WorkEvent`. New events bind the
complete post-transition prerequisite and active-blocker basis by hash, so a
claim-validated mutation can verify the current relation projection without
replaying the item's whole history. Project, root-work, and run-execution
feeds each allocate a dense per-feed position in the event transaction; their
work-event entries carry a verified item id for bounded exact-item lookup. The
item projection also retains the latest event hash; operational reads require
it to equal the newest indexed feed entry, and a schema trigger prevents any
work-event append without an item id. Delivery pages have their own dense
per-session sequence. A position is always carried with its feed kind and id.
A delivery position is separate from the vector of source-feed positions
represented by that delivery. A global database row id is not a cursor. Object
hashes reproduce content but never order changes. `engram doctor` and recovery
still replay retained history exhaustively and compare it with
those bindings. The serial scale regression covers claim, evidence,
checkpoint, revision, block/unblock, handoff, completion, and `work_next` over
500 items and 5,000 events, including one 500-event item, with a fixed
canonical-decode budget.

## Agent-native protocol

Models are primary protocol users, so the surface optimizes for few calls,
bounded responses, stable reason codes, and no redundant identifier shuttling.
The host supplies the bound project, session, actor context, and current work
where unambiguous. A model receives short references and only supplies an
explicit id when changing focus or referring to another graph node.

The hot agent protocol has six operations:

| Operation | Purpose |
| --- | --- |
| `work_next` | Return selected compact focus, ready, catalog, change, and content-free project-memory advisory sections under a 12 KiB ceiling; each call returns the changes since the session's previous call |
| `work_focus` | Select/inspect one item as the ambient binding and return bounded acceptance, relations, memory index, history count/tail, and allowed-next state; never claim or release implicitly |
| `work_propose` | Open a root or atomically create a bounded decomposition and prerequisites; each result is active, proposed, duplicate, or refused |
| `work_update` | Apply a typed union such as claim/release, checkpoint, block/unblock, defer, revise, assign, or dependency change to ambient work |
| `work_complete` | Evaluate acceptance and complete ambient work under current revision/run/claim fences; an optional capture records evidence and its final checkpoint in the same high-level call |
| `work_handoff` | Couple an outgoing checkpoint to an offered/accepted claim transfer |

This six-operation slice is shipped through one `LocalWorkService` used by
both CLI and MCP. The long-lived MCP server retains one service instance for
the process lifetime and shares it across the fourteen MCP tools. That instance
lazily retains one SQLite connection; cloning a
service explicitly creates an independent connection so concurrent delivery
and CAS behavior remains real rather than process-local serialization. The
serial scale benchmark samples that same retained-service lifecycle. The
ambient SQLite row binds only project, session, focused work, and the
processed project-feed cursor. It never stores authority. Agent-facing work is
bound by the stable project plus asserted actor/session context and carries no
grant token or grant timeout. `work_focus` accepts a short ref or UUID,
while update, completion, and handoff infer the current revision, run, claim,
fence, evidence set, and unique matching offer. `work_next` exposes an optional
section selector over `focus`, `ready`, `catalog`, `changes`, and `memories`;
excluding `changes` performs no delivery staging. Ready and catalog candidate selection
uses bounded, maintained SQLite projections and decodes only the rows selected
by the requested limit and filters. Those two sections are advisory: lifecycle
mutations still verify the exact hash-bound item, run, claim, lease,
and relation basis they consume under their write transaction, while `engram
doctor` exhaustively verifies the derived catalog and relation indexes against
retained canonical history. For change delivery it verifies
the canonical source objects, projects explicit compact summaries, and stages
only the largest dense prefix that fits the change byte budget. Full canonical
snapshots and memory bodies are not ambient protocol payloads. Each summary
retains its source position and hash, but the source hash intentionally does
not bind the summary bytes. Restricted and out-of-focus entries retain their
positions as typed omission markers. Planning/lifecycle events, checkpoints,
and evidence summaries are project-visible coordination state across roots;
work-memory summaries are visible only within the focused root, and exact-item
private scratch never enters the shared feed. Each call returns the dense
interval after the session's confirmed cursor, and the page returned by the
previous call counts as delivered when the same session asks again. An agent
never acknowledges anything, and that is a deliberate trade: the change section
is advisory, canonical state is always readable through focus and catalog
views, and a response lost between Engram and the agent is not redelivered.
Concurrent calls from one session return the same staged page rather than
skipping one. A host that needs exact delivery acknowledges explicitly by
returning the `delivered_through` value with the opaque `delivery_token`; a
guessed cursor or token is refused without disclosing either. The tentative
cursor and token are host-internal until a page is actually returned; a
response with no change section has neither field. Every successful agent work
response is at most 12,288 serialized JSON bytes. Advisory truncation is
declared through a typed omission manifest, and catalog continuation points at
the last item actually emitted.

A staged page never blocks anything. Changing focus discards the un-delivered
page, because its omission decisions were made under the previous focus; the
next call recomputes the same interval under the new visibility basis and the
confirmed cursor does not move. The delta interval is the authoritative
delivery cut. Focus, ready, and catalog sections are advisory refreshed views
and may observe a newer concurrent commit; lifecycle mutations always
revalidate their revision, claim, lease, authority, and canonical projection
basis under the write lock. The exact projected change page and its staged
omission count are stored canonically beside the tentative cursor and opaque
token; that count names entries left unconsumed for the next page, not entries
discarded from the current response by its byte budget. Staging
compare-and-swaps the confirmed cursor, empty pending slot, focused work, and
task binding under the SQLite write lock; a focus or task rebind that commits
first forces projection to restart on the new read basis.

Model-originated mutations may supply a caller-stable idempotency key. When
none is supplied the server derives one from the session, operation, focused
work, the item's current claim/handoff basis, and the canonical intent: an
identical call replays its receipt while nothing about the item changed, and
becomes a new attempt once the item moved (so a repeated keyless claim after
expiry claims again). Claiming work the session already holds returns the
live claim, and a fresh completion call against work that is already sealed
returns its seal, so the common retries are never refusals. An interrupted
completion remains bound to its original work and run instead of adopting a
later generation's seal. Every mutation may also name its
target by `work_ref`; the target is resolved and bound inside the mutation, so
a concurrent focus change by the same session cannot redirect it, and it
becomes the ambient focus as a side effect. Durable attempts bind both caller
intent and the exact focused work/claim/handoff basis. A lost-response retry
may replay a committed result, but an interrupted attempt must revalidate live
authority and cannot follow a changed ambient focus into another work item.
The retry-stable basis deliberately ignores only sliding claim expiry and
claim revision. It retains the canonical work head and claim fence, which
distinguish work and claim epochs. A recoverable refusal keeps that durable
request/target binding while a later same-holder claim epoch refreshes the live
basis by compare-and-swap; a different focus or holder conflicts. Every resumed
substep also re-reads evidence and revalidates the live claim inside its commit
transaction.

`work_focus` is the explicit drill-down surface. It carries an exact history
event count and only the newest bounded event summaries, plus body-free memory
index entries. Active blockers include their id, type, and compact detail so an
agent can construct `unblock`; when exactly one blocker is active the id may be
omitted and Engram infers it. Authorized memory bodies remain available on
demand through their version hash. `work_update` and `work_handoff` never
rebuild this history: their success envelopes contain only the operation,
compact receipt, one bounded `obligation_page`, generic readiness obligations,
and `allowed_next`, so hundreds of historical events cannot grow a mutation
response. The same page field appears on `work_focus`, nested
`work_next.focus`, and both completion outcomes. Its item count and canonical
byte size are bounded independently, with an explicit `omitted_count`. Open
obligations sort ahead of terminal history under both count and byte trimming.
The sibling focus-evidence selection retains environments required by visible
open obligations and keeps each visible verification's referenced environment
before that verification, so bounded summaries do not expose dangling typed
evidence links.

`work_complete` accepts either previously recorded evidence and checkpoint
state or an optional `capture { summary, refs }`. The capture form records one
generic evidence object, checkpoints the exact completion evidence set, and attempts
the seal as one model-level operation while retaining each durable lifecycle
event and fence check. All caller-controlled acceptance shape, satisfaction,
and evidence references are validated before either capture substep commits;
the completion transaction then revalidates the same rules against current
run state. If the process stops after evidence or checkpoint
commit, retry loads that canonical substep's original timestamp so its core
idempotency hash replays exactly. Capture identity includes the work revision,
run, claim, and fence, so a legitimate later holder epoch can record its own
evidence without colliding with the earlier capture. The checkpoint's
cut-derived key is selected in the same write transaction that appends it; any
still-uncommitted substep uses the retry's current time and therefore cannot
bypass an expired claim.

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

Every execution observation freezes the canonical obligation-rule-set hash
selected by the begun grant's project-policy epoch. The built-in set turns each
source-changing observation into one immutable test obligation on the run,
independent of action outcome and source-basis availability. Each definition
repeats the exact rule-set hash, rule identity/version, trigger, and requirement;
changing the active policy affects only later observations and never
reinterprets an existing definition. Definitions and their later
satisfied/waived resolutions
are direct dense feed objects; `work_run_obligations` is only their verified
query projection. Satisfaction is evaluated against the latest mutation at an
exact run-feed cut. A passed test for a later basis-bearing mutation may close
earlier open definitions, but a basisless latest mutation makes the open set
waiver-only until a newer basis-bearing mutation and passed test arrive.

The page exposes immutable obligation and definition identities, the required
selected rule-set hash, rule, requirement, trigger, state, terminal
evidence/resolution, and deterministic typed guidance. Neither MCP nor
`work_update` accepts a waiver. The `engram authority waive-obligation` shell
command is an operator-intended convention, not an authenticated boundary: it
has no grant token or run-binding check, so any local process with the binary
and store access can invoke it. The host-private `obligation_waive` operation
is the enforced alternative; its native control session must be bound to the
same live run. Canonical resolution records asserted `waived_by` attribution
and either the shell caller's asserted actor or the private session's
server-fixed actor, while agent pages and the receipt omit the reason. At the
exact pre-seal cut, every applicable definition must
have a satisfied or waived resolution at or before that cut. Otherwise
`work_complete` returns the typed `open_work_obligations` result with the
shared page and remedy: record matching host verification, checkpoint it, then
complete; or request a host/operator waiver. A successful seal stores only
canonical hashes; its page and a fresh session's later focus are reconstructed
from that immutable basis.

Every recoverable completion refusal also carries a typed `recovery` object.
It identifies the exact cause, affected item's full id/short ref/title/current
state, and one `command` string. Shipped causes cover the first exact open
obligation, an unsealed required child, an unaccounted root participant, and
the first missing acceptance criterion. The singular command
keeps CLI and MCP recovery deterministic while the surrounding typed cause
retains the obligation, child, participant, or criterion identity.
Recovery guidance is not persisted as a replay result. It is rebuilt from one
coherent current snapshot, including the bounded obligation page, so a retry
observes a barrier that moved; only a successful completion receipt and
committed capture/checkpoint substeps replay. The refused attempt row remains
pending solely to retain caller-intent and target binding; it is not a frozen
refusal receipt.
See [CLI and MCP](cli-and-mcp.md#work-protocol-contract) for the
retry contract. Missing-contribution recovery hands the root to the named
participant; the participant must then checkpoint/handoff or complete their own
work rather than relying on a no-op claim by the current holder.

Agent-facing MCP and shell work use only the stable project plus non-empty
asserted actor/session binding. There is no work grant file, hash, environment
variable, flag, validity window, or revocation operation. The host-private
behavioral-control channel may still use its separate turn/action grants; those
tokens never authorize or appear in the local-work word surface.

`work_propose` is the low-ceremony decomposition path: an agent can submit a
small plan in one call and either all children/edges appear or none do. The
decomposition receipt returns the complete ordered child identity set as
fixed-size `work_id`/`short_ref`/revision records plus an exact child count;
full child details are obtained by focusing a returned short ref. This keeps
even the maximum 16-child admitted plan below the agent response ceiling and
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

### Gates, prerequisites, supersession, and project memories

The shipped `gate` word, prerequisite/supersession update flags, and project
memory words make the agent surface strictly stronger than the tracker it
replaced. These additions add no canonical object kind,
completion barrier, or review queue: they ride the existing evidence, graph,
dispose, episode-memory, and idempotency machinery at schema marker 1. The
agent-facing syntax is summarized in
[CLI & MCP](cli-and-mcp.md#using-engram-as-an-agent); the memory rules
cross-link [security & trust](security-and-trust.md).

**Gate results become auditable evidence.** `gate NAME [--failed FAILURE]...
[--ref opaque-reference]` records exactly one bounded pass or fail evidence
entry on the focused item you hold (otherwise the typed claim guidance) —
gate name, the bounded failure-label list, any `--ref` — through the ordinary
`WorkEvidence` path. Consecutive identical results under the same claim
generation replay after a lost response without renewing the claim: replay is
the recorded fact again — it records no new evidence and does not renew the
claim. Release,
handoff, or recovery creates a new claim identity, so even the same result
becomes a fresh observation; the
same result after a different state is likewise fresh. Pass → fail → pass
therefore preserves all three observations. That is all it does:
no extra completion barrier, no children, no obligation, no waiver — the
entry rides the ordinary evidence feed, contribution, and `CompletionSeal`
binding like any other evidence, and completion semantics do not change. The
workflow
rule stays where it belongs, in the instruction files: the agent classifies
every failure, and a product defect gets a required child through the ordinary
`add` command with kind `bug`, label
`gate`, and the failing test as acceptance — existing required-child
machinery enforces that work; test and environment classifications go into
the durable note with their evidence. A structural typed field on
`WorkEvidence` keeps test boundaries exact and prevents generic note prose
from acquiring gate semantics; agent-facing projections retain a typed gate
discriminator beside bounded rendered words. The receipt echoes the gate name, result, failure count,
and whether an evidence reference was present, not the potentially
escape-expanded input.

Bare `gate NAME` always records a pass. Every failure supplies at least one
bounded `--failed` label; when the check has no named test, the label is the
check command or check name (for example, `--failed "cargo fmt --check"`).

The verb binds the selected work id through the core call, so a concurrent
same-session focus change cannot redirect the evidence or its receipt. The
storage transaction derives a stable attempt identity from the normalized
observation and previous distinct transition, reserves the protocol attempt,
and appends the evidence atomically. A crash before receipt completion
therefore resumes the pending attempt instead of appending another result.
The same transaction uses a rebuildable partial expression index to narrow
the same-run, same-name evidence candidates, then derives the latest
observation from their canonical run-feed positions. No mutable head can
redirect an immutable `previous` link. Runs are short-lived and their
same-name evidence count is bounded in practice; a query-plan regression pins
the indexed search without a scan or temporary sort, and the project-scale
gate measures the canonical-decode cost alongside the other claim-validated
mutations.

The shared domain/storage boundary owns gate normalization and bounds; the
agent verb calls that same constructor early only to return concise guidance.
Conservative raw byte ceilings run before Unicode normalization so an
oversized MCP string cannot force unbounded normalization work. The name is
trimmed, NFC-normalized, case-folded, and NFC-normalized once when raw input is
admitted; read validation checks the stored canonical fields without applying
that pipeline or mutable Unicode category policy again. This preserves
previously admitted bounded text if a later Unicode table reclassifies a code
point; agent-facing rendering remains bounded and whitespace-collapsed. Each
failure label is trimmed and NFC-normalized with case kept (test and check
identifiers are case-sensitive) and duplicates are
deduplicated; unsafe control and format characters are refused. Then the name
must fit 128 UTF-8 bytes and the failure set
4096 total bytes — the dominant bound — with a 256-byte per-entry sanity
cap, at most 256 supplied entries, and at most 64 distinct normalized
failures. `--ref` is a bounded opaque reference — a path or URL by convention,
not a shape-validated locator — and never ingests log bytes; oversize input is refused with the
one-aggregate-entry remedy. Tests cover the bounds, normalization, MCP shape,
and pass/fail evidence receipt.

Every explicit agent word resolves the exact target and binds it through the
core mutation, so a concurrent focus change by the same session cannot retarget
`claim`, `update`, `note`, `done`, `gate`, or `handoff`. One `note` commits its
evidence and acknowledging checkpoint in one storage transaction and replays
that pair as one operation.

**Prerequisites between arbitrary items.** `update REF --after OTHER` records
that `REF` must not become ready until `OTHER` is complete; `--drop-after
OTHER` removes it. The relation and its readiness semantics already exist
in the graph (`ls --blocked` reports "one or more prerequisites are
incomplete"), but exposing the word adds real core validation: `REF` must
be open for add and drop, `OTHER` must be open for add — proposed,
cancelled, or superseded targets are refused with the item named, while a
completed target gets the typed "already satisfied; no edge needed" refusal —
`OTHER` must not be `REF`, both must share the project, and cycle
prevention rejects any prerequisite cycle and simply refuses an `OTHER`
that is an ancestor of `REF`, keeping decomposition deadlocks impossible
without graph gymnastics. Dropping stays allowed after `OTHER` becomes
terminal so stale edges can be cleaned, and dropping an
absent edge is an idempotent no-op. Re-adding an existing edge after `OTHER`
becomes terminal returns the same typed terminal refusal; only exact protocol
replay of the original add returns its recorded result. One shared one-hop
classifier labels each edge `satisfied`, `pending`, or `dead`: completed and
superseded-to-completed
edges are satisfied; live replacements remain pending; cancelled prerequisites
and superseded prerequisites whose immediate replacement cannot complete are
dead. `REF` stays blocked on pending and dead edges, and the blocked reason plus
`next`/`show` guidance carries `update REF --drop-after OTHER` for a dead edge —
a guide, not a refusal.
All dead prerequisites are prioritized ahead of pending and satisfied edges in
the bounded focus relation page; class-specific omission counts say which
relations did not fit.
At most one removal command precedes ordinary lifecycle suggestions, keeping
both prerequisite recovery and a lifecycle action available under relation
and command limits.
Each flag takes one ref and is mutually exclusive with every other `update`
action. An edge is not a required child: a prerequisite orders readiness and
can be dropped again, while a required
child binds parent completion and is accounted only by seal or waiver. The
admission, cycle, and refusal boundaries above are covered in the core and
agent-surface tests.

**Supersession.** `update REF --supersede-with NEW --reason "why"` exposes
the existing dispose-as-superseded path with the caller's attributed reason —
the CLI/MCP contract requires `--reason` and never invents one: `REF` leaves
the ready list, `show REF` names its successor, `ls --all` still lists it,
the caller's own claim on `REF` is released with that audited reason.
`REF`-side refusals: not open, open descendants, held by another session.
`NEW`-side refusals, as storage already enforces: `NEW` is `REF` itself, lives
in another project, or is cancelled or superseded; a completed `NEW` is
allowed. Exposing the word also adds one core admission check: the implicit
`REF` → `NEW` completion dependency joins required-child and prerequisite edges
in the union-cycle validation, so direct and transitive replacement deadlocks
are refused. The shared update action group allows `--reason` with release or
supersession, and the action-enumeration error names all three new flags.
Superseding a
required child never satisfies its parent by itself: the parent's `done`
still reports the unsealed required child, and the deliberate replacement
is accounted by the existing reason-attributed required-child waiver. Automatic
successor accounting is not in this cut; tests cover the REF/NEW refusal
matrix and the front-end translation.

**Project memories.** `remember "text" [--key KEY]` stores one attributed,
retrievable project note — an ordinary Episode in the existing memory
model: soft authority, `internal` sensitivity, on-demand delivery, no
automatic decay in V1 (episodic compaction stays a V1.x roadmap item),
never a rule or a fact authority, active through the
existing episode exception (see
[write policy & review](write-policy-and-review.md)). There is no Proposed
slot, no review queue, and no host review operation: what you write is
what project peers can list, attributed to your session. A note stays full
until an explicit `forget`: `memories` lists it and `--full` returns the
full body until the tombstone; a retired key answers with the typed
`memory_retired` and the satisfiable next action to pick or list another
key. Reads and writes in V1 use cooperative project binding: any session
asserted-bound to the same stable project may list, read, remember, and forget.
Before persistence, the mutation path validates a non-empty consistent
actor/session binding. `memory_binding_invalid` means that binding is absent
or inconsistent.
This is asserted host context rather than authenticated identity,
with no per-note ownership or separate memory-policy operation. Writes pass the
configured Redactor, which in V1 is the visibly labeled no-op
`DevelopmentNoopRedactor` that filters nothing; see
[security & trust](security-and-trust.md#redaction-the-real-control).
Engram promises no automatic secret prevention, so credentials and secrets
must never be placed in a memory body, and a write that policy would make
undeliverable is refused before persistence — no write-only sink. The key
is a safe token: 1–64 ASCII bytes matching `[a-z0-9][a-z0-9._-]*` — no
leading dash, no control or shell metacharacters — supplied explicitly or
defaulted to a slug of the first words, so generated commands are always
safe. An identical retry from the same actor/session replays; any other
defaulted slug collision with a live or tombstoned key is refused and the
guidance names an explicit `--key`. `forget KEY` appends an
attributed tombstone — not erasure, the version history stays canonical —
and is idempotent; the key is then retired for good. A raw body is at most
8 KiB (8192 UTF-8 bytes) and is accepted only when both the exact structured
full-read envelope and terminal-safe shell rendering fit the 12 KiB ceiling;
anything larger is rejected before persistence, with escape-heavy boundary
tests among the named targets.

The mutation contract is create/refuse/forget — no update-in-place:
`remember` creates only when the safe key has never been used; a live key
is the typed `memory_exists` refusal, and a tombstoned key is the typed
`memory_retired` refusal whose satisfiable next action is choosing another
key (`memories` lists what is taken) — tombstoned keys are permanently
reserved, never recreated. `forget` tombstones idempotently. The durable
representation is the simplest one: the safe key lives in the existing
`MemoryVersion` shape, changed in place at marker 1, unique per project,
with `Tombstoned` as the terminal status. The existing durable memory head
carries that terminal state and is verified against canonical history; key
uniqueness is rebuildable from canonical versions, and no new canonical object
kind exists. Keyed project memories cannot be contradiction endpoints; their
narrow lifecycle is remember, optional full read, then forget. With one create
and one idempotent tombstone per key
forever, ordinary argument-derived operation idempotency is generation-safe
by construction; `next` installs no per-key session state, and no hash or
id is ever model-visible. Tests cover create/refuse/forget, listing
continuation, escape-heavy size boundaries, and ordinary idempotency.

`memories` is the source of truth; `next` only advertises. The positional
argument is a query unless `--full` names a key, and `--after` always
takes a key. Unfiltered `memories` lists compact rows (key, bounded first
line, remembered-at) in key order and may continue with the shell-safe
`memories --after KEY`; an exhausted listing says so. Filtered `memories
QUERY` returns a bounded set of top matches only and never emits a
continuation — its omission note tells the agent to refine the query. Final
structured and terminal-safe list receipts also share the 12 KiB agent
response ceiling; rows are shed with an omission count or continuation before
either representation can exceed it. The filtered path uses the Unicode-aware
memory full-text index for keys/titles
and bodies rather than SQLite's ASCII-only `lower()` matching. Search input is
bounded before FTS expansion to 256 raw UTF-8 bytes and 16 normalized tokens.
Neither returns bodies; `memories KEY --full` resolves exactly one key —
typed `memory_not_found`, `memory_binding_invalid`, or `memory_retired` for
a tombstoned key — and returns the full body as a dedicated response, at most
8 KiB only when both its structured and terminal-safe envelopes remain under
the 12 KiB ceiling, never inlined into `next`. The `next.memories` signal is content-free: a
count of retained project notes and a changed-since-last-call flag, read in
O(1) from a rebuildable per-project count and change position — no
keys, no first lines, no body-derived text. When even that does not fit, the
signal is omitted without acknowledgement and reannounces later. Delivery is
advisory: no
per-session authoritative delivery stream, no dedicated acknowledgement
token, no exactly-once guarantee; a host-passed `context_generation` marks
a fresh or compacted context and may reannounce the count. Only a
domain-separated digest of that asserted value is persisted; its raw text is
never retained. The discardable
acknowledgement table is bounded per project; evicting an old session can only
cause one harmless reannouncement. SQLite busy/locked contention while writing
that advisory acknowledgement does not fail `next`; the signal simply
reannounces. An agent that
wants the notes runs `memories`. The existing `work_next` decode, latency,
and 12 KiB response targets stay as acceptance tests. Rules that must
survive every session belong in the instruction files; memories are for
attributed notes and observations that change more often than the files
do.

## Audited waivers and model autonomy

Agent-facing local-work words are not grant-gated. A stable project binding,
non-empty asserted actor/session context, the current lifecycle, and fenced
claim/handoff state determine whether a word can run. Actor text remains
asserted context unless a stronger host mechanism verifies it.

The default local planning envelope is bounded in code: depth 4, 128 open
descendants per root, and 16 children per decomposition. Agents may create and
revise work, claim/recover, cancel/reopen, complete, and record explicit
waivers within those lifecycle rules. Recovery, cancellation, reopen, and
waiver paths require attributed reasons where their audit contracts call for
one. The project-bound session may record the waiver; the reason and immutable
event make the exception attributable and auditable, not permission-bearing.
External publication still requires an explicit human decision. A host that
runs the optional behavioral-control plane may independently raise the bar for
model turns or material external actions.

Engram must show `allowed_next` and a typed recovery directive rather than
making a model infer permissions by trial and error.

`allowed_next` entries name the exact tool and tagged operation, for example
`work_update:claim`, `work_propose:decompose`, or `work_handoff:accept`.
`work_update:claim(recovery_reason_required)` means a different prior holder is
still unaccounted and the caller must submit the `claim` variant with an
attributed `recovery_reason`. A prior contribution or persisted participant waiver makes
the holder accounted, so a successor receives ordinary `work_update:claim`
instead of being asked to waive the same omission twice. Every agent-facing
claim reminder and runnable claim command derives from that exact
`allowed_next` tag; generic readiness wording never independently upgrades an
ordinary claim into attributed recovery. Catalog-only `next` and `ls` rows
route through `show` before suggesting a claim because they do not carry that
session-specific action set.

`work_update:waive_required_child` appears only when at least one direct,
required, cancelled-or-superseded, not-yet-waived child exists.
`work_focus` carries a bounded typed `waivable_required_children` list with
the executable child short refs. The mutation rechecks the exact parent and
child state before recording the attributed waiver.

Until action outcomes and resource leases are linked to `WorkRun`, V1 accepts
only a **zero-linked-state** completion-drain attestation. An
agent cannot complete by supplying arbitrary action hashes or lease names.
The later host-enforcement slice replaces that temporary empty attestation
with exact reconciled action and released/transferred lease projections.

## Behavioral-control integration

Work calls describe intent. Separately, the host-private behavioral-control
channel may mediate a model turn or material external action with its own
short-lived grant. That control-plane grant binds:

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

Engram reports durability separately from behavioral control and from external
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

## Tracker replacement and interoperability

The local workflow dogfood has cut this repository and one migrated project
over to Engram as their only writable local tracker. The broader replacement
claim still waits for the off-host durability and control-binding
[roadmap](../roadmap.md#v1--close-the-loop) gates. The mapping below is the
daily workflow it covers, kept for anyone arriving from the previous tracker
(Beads):

| Beads workflow | Engram equivalent |
| --- | --- |
| `bd create`, parent/child | `work_propose` |
| `bd ready` | `work_next` with typed readiness reasons |
| `bd show`, search/list | `work_focus` plus query views |
| `bd dep add`, blocked | prerequisite edges and typed blockers through `update --after` / `--drop-after` |
| assignee vs. `bd update --claim` | durable assignment vs. fenced live claim; resource mutation still needs leases |
| notes/design/acceptance | typed work fields plus work-scoped shared/private memory and evidence |
| comments and handoff | one checkpoint feeding deltas, handoff, and report input |
| `bd close`, reopen, supersede | one-call evidence capture/seal when compact, or explicit evidence/checkpoint steps; audited reopen/cancel/supersede events through `update --supersede-with` |
| `bd remember` | Engram's typed durable memory, not a pseudo-task (`remember`/`memories`/`forget`) |
| `bd stats`, stale/orphans/preflight | rebuildable operational indexes and integrity diagnostics |
| Dolt cross-machine sync | V1 sequential `portable` handoff; later concurrent Engram `Sync` backend |

A Beads adapter imports selected issues as source snapshots and may optionally
publish an explicit projection. It does not make `.beads`, Dolt, or `bd` a
runtime dependency. The accurate current claim matches the
[roadmap](../roadmap.md#v1--close-the-loop): a writable local tracker in a
running dogfood, with the broader replacement declared only after the
off-host durability and control-binding gates; teams needing a concurrently
writable multi-machine backlog still need an external system or later
Engram sync.

The agent-facing `note` tool binds directly to persisted local work focus. It
requires the session's exact live claim and records one shared finding plus its
evidence/checkpoint state for peers, handoff, and report assembly. The service
derives the idempotency key; an exact retry returns the first receipt while
changed prose is a new intent. Generic task memory and private scratch are not
separate MCP tools. Work focus still returns a bounded, actor-filtered memory
summary built from authorized canonical state, so the work graph and its
execution memory do not require an external tracker identity shim.
Restricted-sensitivity bodies are omitted from task and work search/focus
views and remain unavailable through direct show in V1. `work_next` also
replaces restricted work memory and memory outside the currently focused root
with a typed omission marker, preserving its exact dense feed position without
exposing protected content.

## Delivery boundary

The smallest coherent implementation sequence is:

1. Define dense per-feed cursor identity, then add canonical work
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

The design can proceed with the fixed project-bound work limits described
above, but product policy must eventually select:

- the completed-work retention and compaction period when nothing is
  published;
- the recovery snapshot format and acceptable recovery-point objective;
- the first portable substrate (recommended: private dedicated Git ref, with
  internal object storage for organization scale); and
- the trigger and substrate for concurrent cross-host synchronization.
