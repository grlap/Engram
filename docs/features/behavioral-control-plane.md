# Behavioral & Coordination Control Plane

> Normative references: [spec §2.7](../spec.md#27-execution-control) and
> [spec §8](../spec.md#8-interfaces).
> Related briefs: [context packets](context-packets.md),
> [local work system](local-work-system.md),
> [local tasks & reports](local-tasks-and-reports.md),
> [CLI & MCP](cli-and-mcp.md),
> [security & trust](security-and-trust.md),
> [execution pipeline](execution-pipeline.md), and the
> [turn gate assessment](turn-gate-assessment.md) of what is used today.

This brief specifies the **target V1 architecture**. Engram is not complete
when it merely offers coordination tools that a coding
agent may choose to call. It must be the decision authority for the execution
protocol around an active task: whether a turn may begin, which capabilities
that turn may exercise, which peer changes must be processed first, what
ownership is required, and what must be checkpointed before the session can
continue, hand off, or finalize.

The host runtime remains the **actuator**. It starts, pauses, wakes, and stops
agent processes and mediates their tools. Engram is the **policy decision
point**. It evaluates durable task and session state and returns typed grants,
refusals, and required directives. A host integration is the **policy
enforcement point**. It withholds prompts and material tool calls unless
Engram granted them.

This split keeps the core runtime-neutral without pretending that an MCP tool
an agent can ignore is enforcement.

## Product boundary

| System | Owns | Does not own |
| --- | --- | --- |
| Engram | Local work graph and readiness, task-bound execution admission, context obligations, claims, participant coordination, checkpoints, handoffs, evidence, completion, and publication intents | Model/process supervision or source-control policy |
| Host runtime | Session/process lifecycle, prompt injection, tool interception, wake-up delivery, user approvals | Engram's durable task truth or policy decisions |
| Agent | Reasoning and proposed work within a granted envelope | Self-authorizing a turn, acknowledgement, or external side effect |
| External adapter | Optional immutable intake snapshot and explicitly authorized publication | Live local work truth, silent refresh, or process coordination |

Engram derives and ranks ready local work. The host or model chooses among the
bounded candidates unless policy made an assignment; the host still chooses
the model and controls its process. Engram then decides whether that selected
session is sufficiently synchronized and authorized to act. It returns
corrective directives; it does not silently invent a plan or start a process.

A useful boundary test is temporal: if a fact decides what local work exists,
its decomposition, readiness, acceptance, or execution authority, it belongs
to Engram; if it starts, pauses, wakes, or stops a process, it belongs to the
host; if it crosses a system boundary, it belongs to an explicit adapter
intent. Engram has a ready-work view and deterministic ranking, but no process
lifecycle signal or autonomous model scheduler.

## Control assurance

Every session records the strongest control assurance actually provided by
its host adapter:

| Level | Meaning | Permitted claim |
| --- | --- | --- |
| `advisory` | The agent can call Engram, but prompts or tools may bypass it | Memory and coordination guidance only |
| `turn_gated` | The host obtains a grant before each model turn and injects every blocking directive | Engram controls turn admission; mid-turn side effects may still escape |
| `action_gated` | Not built: the host would also intercept every configured material capability and require an action grant (see [planned interfaces](#planned-interfaces)) | Engram would control turn admission and the declared material capability set |

These levels describe asserted host mediation, not cryptographic attestation.
The adapter declares its coverage and Engram records it with the existing
identity-assurance context. A deployment may not describe itself as
`action_gated` when the agent retains an unmediated shell, network path, or
write-capable tool. A future trusted gateway or signer can add stronger
assurance without changing the protocol.

Effect classes also carry a non-configurable minimum assurance. `observe` and
`communicate` require `advisory`; internal `coordinate`, `mutate_local`,
`mutate_shared`, `external_side_effect`, and `lifecycle` require at least
`turn_gated`. The effective requirement for a turn is the stronger of the
project policy floor and every requested effect floor. Binding remains
observable in shadow mode:
the bind receipt reports `effective_mediated_effects`, the declared set capped
by host assurance, while turn evaluation refuses effects above
that cap with `control_assurance_insufficient`. Assurance refusals are policy
decisions carrying an effect-naming `ControlDirective`, never transport error
envelopes. Mediation-envelope refusals also return the declared and
assurance-capped effective effect sets so one host renderer can
explain the turn decision. A
`capability_not_permitted` directive names the excluded effect but omits
`required_assurance`: raising assurance cannot add an effect that the active
policy does not support.

The target `engram doctor` reports a versioned mediation map: every host tool
surface, its effect classification, whether it is intercepted, and any unmediated
write-capable path. Unmapped tools default to `external_side_effect` in
enforcement mode. `session_bind` also mints a host-held routing token required
on control requests; it prevents accidental cross-session mix-ups but is not
authentication or containment. Anyone able to bypass the host or write the
store remains outside this coordination boundary.

## Components

```text
                      chooses model / starts process
┌────────────────┐       ┌───────────────────────────┐
│ Optional source│ snap  │ Host runtime              │
│ adapter        │──────►│ prompt + tool mediator    │
└──────┬─────────┘       └──────┬───────────┬────────┘
       │                         │ control   │ agent-facing
       ▼              before turn│ protocol  │ local-work tools
┌────────────────────────────────┴───────────▼────────┐
│ Engram core                                         │
│ work graph · control evaluator · memory/report      │
└──────────────────────────┬──────────────────────────┘
                           ▼
              canonical SQLite + projections
                           │ optional publication intent
                           ▼
                  External publication adapter
```

The control evaluator is domain logic. CLI, host hooks, a wrapper, and MCP
translate requests but do not reproduce the rules. The agent-facing MCP
surface remains useful for capture, search, and explanation; host-only grant
and acknowledgement operations are not presented as tools the agent can use
to authorize itself.

## Session protocol

A root-execution member has a durable session phase in addition to the focused
run's completion state and the optional report state. Storage writes three:

```text
bind → ready → turn_open → ready
         ↑         │
         │         └── report with next intent exit → exited
         └── grant expired or superseded, refused start, restart before start
```

- `ready`: the session may ask for a turn, subject to the current capability
  and work-claim checks. A `sync_required` row written before grants stopped
  carrying a delivery page admits a turn the same way.
- `turn_open`: one grant is issued or begun. Replaying the same turn intent
  returns the same grant; a different intent under the same idempotency key is
  a conflict. A begun turn stays open until the host reports it.
- `exited`: the host reported the session's exit. Only a fresh bind admits it
  again.

There are no other phases and no recovery turns.

`blocked` is not a catch-all session phase. A refusal names the precise
blocking condition and the operations that remain safe. This avoids states
that cannot explain how to recover.

## Host-facing control protocol

The substrate-neutral core exposes a small lifecycle protocol. Transports may
batch operations, but may not weaken their semantics.

### 1. Register and bind

The shipped host `session_bind(external_ref, title)` resolves a shared control
anchor by project and external reference. `control_sessions` holds the binding;
there is no separate compatibility-task lifecycle, participant roster or join
event. The retained wire field `task_id` identifies this scope, and
`control_changes` is its write-only audit index: each turn report appends one
event, which no grant delivers and no decision reads. Binding itself emits no
event. Exact local-work claims remain a separate authority boundary.

`session_bind` records the asserted actor, host session, control-assurance
level, declared mediated capabilities, and an optional exact work binding:
`root_execution_id`, `work_id`, `run_id`, `work_revision`, `claim_id`, and
`claim_fence`. Storage verifies that tuple against the session's live claim and
copies it into each grant. The six-operation work protocol supplies this tuple
directly as `work_update:claim.receipt.control_binding` and
`work_focus.control_binding`, and `work core inspect` returns it without
selecting focus, or an explicit `null` when there is none, including while the
caller still holds the claim but bind would refuse it. Each shows the tuple
only when `session_bind` would accept it, because it runs the same
validation, so a claim with a pending handoff offer shows none. That
holds when the answer is built; a replayed claim receipt returns its stored
original, so a host reads the current binding with `work core inspect`.
`work core held` lists the claims the session holds, newest first and at most
16 with an exact omitted count, each with the same bindable tuple or `null`, so
a host can choose which claim to bind without moving focus. The
focus run section also names its root execution and work
item. Here `work_revision` is the work item's revision
returned at the top of the claim receipt, not the claim object's revision. A host
may first seed a local root from a user request or an optional external
snapshot. Local work never requires an external reference. V1 has one ordinary
executor per `WorkRun`; an ordinary turn additionally requires that session's
live claim on the focused run. Other root members may inspect permitted root
memory and communicate, or claim distinct child runs for parallel work, but
membership does not authorize mutation/completion of another executor's run.
Initial roles come from a user/host authority reference or the active control
policy. While active, the root coordinator may change the expected roster
under that authority and its live root-run work claim. Joining or
changing the roster during `completion_pending` requires an attributed abort
and a new synchronization cut.

### 2. Evaluate a turn

`turn_evaluate(TurnIntent) -> ControlDecision` is called before a prompt is
delivered to the model. The intent contains:

```text
TurnIntent {
  root_execution_id, work_id, run_id
  session_id, session_token, turn_id, idempotency_key
  requested_capabilities[]
  resource_intents[]
  authority_refs[]
  expected_work_revision?, expected_session_revision?
}
```

The expected revision is only an optimistic-concurrency guard. Context,
cursor, policy, membership, and claim facts come from Engram's durable state;
caller fields are never accepted as proof that a precondition is true.

The decision is a grant or a refusal:

```text
TurnGrant {
  grant_id, intent_hash, work_id, work_revision, run_id, session_id, turn_id
  work_claim_id, work_claim_fence
  basis_project_policy_epoch, basis_work_admission_epoch
  basis_portable_writer_epoch?, portable_writer_valid_until?
  capability_envelope[]
  expires_at
  directives[]
}

TurnRefusal {
  intent_hash, code, message
  current_project_policy_epoch, current_work_admission_epoch
  blocking_directives[]
}
```

A grant carries no context or peer delta. The work context an agent sees
comes from the `next` word, which keeps its own staged delivery and does not
depend on the gate. Immediately before dispatching the prompt, the host calls
`turn_begin(grant_id)`. In one transaction Engram rechecks the grant's state and
expiry, the session's phase, the task's control anchor and the session's
membership, the project-policy and
work-admission epochs, the capability map, the work revision and the claim
fence, then activates the grant. Without that transition the host must not
deliver the prompt.

Restart invalidates an unbegun grant and returns the session to `ready`; an
uncertain begun grant remains `turn_open` until the host reports it. Nothing is
redelivered, because replaying a prompt with possible effects would be unsafe.
A fresh evaluation key likewise atomically supersedes an issued-but-unbegun
grant. The same transaction records an immutable supersession transition that
binds the old grant and request key to the replacement request and decision,
with a typed reason and timestamp. It never replaces a begun grant: that
session stays `turn_open`, reports `open_grant_state: begun`, and refuses the
evaluation with `turn_already_open` until checkpoint/reconciliation completes.

A grant is an immutable, fingerprinted operational record while live. It is
bound to one task, session, turn intent, both control epochs, capability
envelope, and work-claim fence; it is not a bearer token transferable to another
session. Restart invalidates issued-but-unbegun authority. A begun grant stays
durable only until checkpoint/reconciliation, so completed, expired, and
superseded grants still need not become permanent canonical memory. State-changing grant
supersession, checkpoint and handoff transitions emit immutable canonical
events.

Evaluation order is fixed so refusals are deterministic and do not leak later
state through an earlier failure:

1. verify the control schema;
2. verify that the session's work binding is still current (work revision,
   claim fence and run state);
3. verify the host's assurance and mediated effects against the policy and
   the requested effects;
4. verify the task's control anchor, the session's membership and its phase;
5. verify the project-policy and work-admission epochs; and
6. verify the request's shape and normalize supplied resource intents (which
   confer no exclusive ownership).

Only then can a turn grant be minted. Time is an explicit evaluator input, not
a hidden wall-clock read, so replay and boundary tests are deterministic.

Refusal does not mean the host must deadlock or bypass the gate. Almost every
refusal names a host-automatic repair: rebind, re-evaluate, wait for an open
turn's report, or change a setting. The historical page-era refusals named an
agent recovery turn, which no longer exists; repair that needs model reasoning
happens in an ordinary turn.

### 3. Deliveries

Grants carry no delivery, so `turn_begin` stages nothing and `turn_checkpoint`
promotes nothing. A begin that names a delivery token is refused with
`grant_scope_mismatch`. The planned standalone `delivery_ack` is not built.

Blocking directives are typed, addressed, and carry a satisfaction mode:

```text
Directive {            # design shape; the wire sends target host | agent
  directive_id, kind   # and satisfaction host_transition | recovery_checkpoint
  audience: host | agent | human
  satisfaction: state_predicate | authority_ref | none
  parameters
}
```

Kinds include:

- `bind_task`
- `resolve_pinned_conflict`
- `checkpoint_turn`
- `release_or_handoff`
- `wait`
- `contribute`
- `finalize`

Directives are satisfied only by their dedicated atomic operations—for
example, claim, resolve, handoff, checkpoint, contribute, or finalize.
Engram reevaluates their state predicates; acknowledging text never substitutes
for making the required transition. Only informational directives use
`satisfaction: none`.

### 4. Effects and resource subjects

There is no check on individual actions: every effect is decided once per turn
(see [planned interfaces](#planned-interfaces)).

Effect classes are deliberately small and host-neutral:

| Effect | Examples | Default control |
| --- | --- | --- |
| `observe` | Read files, search memory, inspect status | Allowed after required policy context is loaded |
| `communicate` | Root-work-shared capture, contributor message | Requires root-execution membership; becomes an ordered event |
| `coordinate` | Reserved internal coordination effect | Not a model-turn capability; no resource-lease API |
| `mutate_local` | Write a mediated workspace or run a mutating local tool | Requires host/user authority and adequate declared mediation; resource intents do not reserve ownership |
| `mutate_shared` | Change shared work/run coordination state | Requires membership and explicit authority; not shipped in the turn policy |
| `external_side_effect` | Publish a report or invoke an external write adapter | Requires an explicit durable intent, user/policy authority, and idempotency key |
| `lifecycle` | Handoff, waive, complete, finalize | Requires the named lifecycle capability and barrier preconditions; finalization additionally binds a `ReportAssemblyClaim` |

A turn's resource intents use one canonical structured subject model:

```text
ResourceSubject =
  Path { project_id, segments[], coverage: exact | tree }
  Logical { namespace, segments[], coverage: exact | tree }
```

The host maps tool targets into subjects; the core normalizes and validates
them. Path segments are project-relative, `/`-separated, Unicode NFC, and
normalized under the project's immutable `sensitive | case_folded` path
policy selected at initialization; folded mode uses locale-independent Unicode
Default Case Folding after NFC. Absolute paths, individual empty segments,
`.`, `..`, NUL, and arbitrary globs are rejected. A zero-segment path is valid
only with `coverage: tree` to mean the registered project root. Existing
symlinks resolve to their
canonical target and must remain inside a registered project root; a
nonexistent target resolves its nearest existing parent before validated tail
segments are appended. Rename requires both source and destination subjects.

`exact` describes one normalized subject; `tree` describes it and its
component-boundary descendants. Logical namespaces use the same segment and
coverage vocabulary. The shipped core does not compute subject overlap or
coverage authority. Multiple worktrees therefore describe the same logical
project path rather than unrelated absolute paths. This is not a resource
lock. Host identity remains asserted context at V1 assurance, but normalization
is core logic.

Because not every host can prevent out-of-band writes, the host records a
workspace fingerprint at turn boundaries and reports changed logical paths.
Changes outside the declared subjects create an attributed reconciliation event.
Where prevention is impossible, the assurance claim is detection, not control.

### 5. Checkpoint the turn

`turn_checkpoint(TurnCheckpoint)` closes a turn. The block below is the design
shape. On the wire a checkpoint carries only the routing token that
`session_bind` returned, the grant, the next intent, the execution
observations, verification and environment evidence, and an idempotency key.
The wire accepts the next intents `continue`, `wait` and `exit`; the design's
`handoff` and `contribute` intents are not built. The delivery positions and
source-feed vector went with the grant's page. Action outcome hashes need
action checks, which are not built. Capture hashes and blocker references are
not sent: agents record captures and blockers with `note` and `update`.

```text
TurnCheckpoint {
  grant_id, turn_id, idempotency_key
  expected_checkpointed_delivery_position  # DeliveryPosition CAS basis
  promote_through_delivery_position         # DeliveryPosition
  source_feed_positions[]                   # resulting FeedPosition vector
  action_outcome_hashes[]
  execution_observations[]               # bounded host facts for the bound run
  verification_evidence[]                # <= 16 host-minted checks
  environment_evidence[]                 # <= 4 source-bound environment identities
  capture_hashes[]
  blocker_refs[]
  next_intent: continue | wait | handoff | contribute | exit
}
```

The shipped path accepts execution observations directly on the host-private
`turn_checkpoint` request. Each observation names an action fingerprint,
effect, outcome, and whether source state changed. It may also carry
`source_basis { workspace_id, source_revision }` and `observed_at`.
`source_revision` is a host-computed fingerprint of the complete relevant
content state, including committed and dirty content; `workspace_id` is audit
context and is not an equality requirement, so equal revisions in different
workspaces remain comparable. Storage supplies and freezes the exact
grant/session/work binding plus the recording time, then appends the object to
the project, root, and run feeds atomically with the checkpoint receipt.

The same private checkpoint may mint up to 16 `verification_evidence` objects
and four `environment_evidence` objects. Verification cites its producer by
canonical observation id or by an `observation_id` in the same request.
Storage derives the run/source binding, command fingerprint, result, producer
session, and times from that observation; a missing producer is a named
`verification_producer_not_found` request fault. Verification may also cite an
environment object by id or by same-request index. That object must name the
same run and source revision.

Environment evidence may use the opaque fingerprint form. The structured
form supplies a closed, bounded component identity: `toolchain`, optional
`sandbox` or image label, `workspace_id`, and the session's
`capability_map_revision`. Engram derives the RFC 8785/SHA-256 fingerprint from
those components, requires the workspace to match the source basis and the
capability revision to match the bound session, and persists the components as
canonical evidence. `environment_fingerprint_mismatch`,
`environment_evidence_not_found`, and `environment_basis_mismatch` identify
the three repairable failures. Component strings are asserted host context,
not authenticated attestation; the V1 no-op redactor is visible in
diagnostics, and hosts must not place credentials or secrets in them.

Only a typed, passed verification for the required check, exact run, and
latest source revision may satisfy a verification obligation. The built-in rule
leaves `check_fingerprint` and `required_environment` empty, so it accepts any
passed test and treats an environment link as audit provenance. An
operator-selected typed V1 set may instead pin both exact hashes. The matcher
still requires the evidence to follow the latest mutation at the evaluated
run-feed cut, so a later source mutation reopens the requirement. Generic
agent-recorded `work_evidence` remains useful context but never verifies a
check.

The active immutable `ControlPolicy` selects a canonical
`ObligationRuleSet` by hash. The built-in set contains the typed
`source_mutation_requires_test` rule, which evaluates every work-bound
observation with `source_changed=true`, regardless of outcome or whether a
source basis is present. The checkpoint resolves the rule set from the begun
grant's frozen project-policy epoch and records its id on the
`ExecutionObservation`; every resulting `WorkObligation` repeats that exact
selection. Activating another set affects only observations from later policy
epochs and cannot reinterpret a prior trigger, definition, or completion cut.
Every observation carries the exact selected rule-set id.

Each match appends one immutable `work_obligation` definition directly to the
project, root-work, and run-execution feeds. A passed `test` verification
appends a separate `work_obligation_resolution` only when it matches the exact
run and the newest source basis visible at the evaluated run-feed cut. That
evidence may satisfy older still-open mutation obligations as well as the
newest one. If the newest mutation has no source basis, no verification can
match it: all open obligations remain waiver-only until a later basis-bearing
mutation and passed test establish a newer verifiable state.

The stock rule records rather than blocks. The final checkpoint may leave a
`source_mutation_requires_test` obligation open because no matching passing
test followed a change. Completion then resolves that obligation as a waiver
attributed to the completing actor. The waiver's host-private reason names the
change and its source revision. The waiver is appended after that checkpoint
and inside the sealed cut, so the seal still binds only terminal obligations.
A completion refused for another reason rolls the waiver back with the rest.
Its recovery page is read from the state before the waivers, so it never names
a waiver that rollback discards.
On that waived obligation, the `obligation_page` entry carries
`untested_change`: the host's observation id, source revision, and observation
time. An operator waiver of the same rule carries it too. The page's
`untested_total` counts every such change on the run, including those the
bounded page leaves out. Agent `done` and `show` print one
`untested source change:` line per named change, then the exact count of any
not shown, so the changes stay visible after completion. Peers receive each
change in `next` as an `untested_source_change` delta. Only the exact stock
definition behaves this way: the stock id at version 1, triggered by a source
change, requiring an unpinned test. Obligations from an acceptance binding
(`--bind`) keep blocking completion, and so do obligations from any other
operator-selected rule, including one that reuses the stock id with another
version or a pinned check or environment. Each needs a matching verification
or an operator waiver.

Definitions and resolutions are canonical feed objects; the mutable obligation
row is only a verified projection. `work_focus`, nested `work_next.focus`,
`work_update`, and both completion outcomes use one count- and byte-bounded
`obligation_page` with explicit omission count, immutable identities, state,
rule-set identity, rule, requirement, trigger, terminal evidence/resolution,
and deterministic typed guidance. Generic readiness strings remain separate.
A fresh session reconstructs the same summaries from canonical history rather
than trusting a prior response.

A waiver may be requested through the operator CLI, never through the removed
host-private operation, MCP or a direct `work_update` waiver variant.
The shell request carries an attributed reason but no work grant; its actor
and `waived_by` human are asserted, not authenticated. Revising or dropping an
acceptance binding still resolves its open obligation with retained history.
Agent-facing pages omit the reason. Completion first records the stock rule's
open obligations as waivers, as above, then evaluates the cut-aware open set at
the exact pre-seal run-feed cut; any obligation still open refuses. Terminal definitions are
frozen into the seal as exact definition/resolution id pairs under obligation
schema V1, and completion success reconstructs its page from that sealed basis.
New seals separately declare environment schema V1 and bind the sorted,
distinct environment-evidence ids at or before the same dense run-feed cut.
The seal carries ids only, refuses more than 64 records, and never copies
toolchain, sandbox, or image bytes. Every accepted seal carries the current
environment-schema binding.

An observation effect outside the frozen grant is rejected as
`observation_scope_mismatch`. A checkpoint against an issued-but-not-begun
grant returns `grant_not_begun` with host bind/recovery guidance;
`grant_scope_mismatch` remains the general frozen-basis mismatch. An
exact retry replays the same observation and typed-evidence ids, while a
different ordered input under the same checkpoint key is an idempotency
conflict. Task-only sessions cannot append run observations or typed run
evidence, and another session cannot bind or reuse a peer's claim: the claim
holder must equal the control `session_id`, never merely the asserted
`actor_id`. A historically owned tuple that moved before bind reports
`stale_fence`; a malformed or peer-owned tuple reports `work_claim_mismatch`.
After begin, checkpoint compares the frozen session/grant tuple without
regranting or rechecking live claim expiry, because its job is to record the
already-consumed turn. `turn_checkpoint` closes control authority;
`checkpoint_work` is the distinct local-work lifecycle operation that records
run progress and evidence.

Coordination transitions and the host's execution observations are captured
automatically. Decisions, constraints, facts, and promotion candidates enter
the ordinary typed-memory write path, never the checkpoint, so they cannot
bypass classification, redaction, scope, or promotion policy. One capture
continues to feed peer deltas, handoffs, and report input.

The host should collect a `TurnResult` beside the ordinary model response and
submit one checkpoint operation, not require a second bookkeeping dialogue.
It pre-populates the execution observations and the test or build evidence it
saw (per-action receipts are not built) and sets the bounded next intent.
The agent records findings and blockers with `note` and `update` as it works,
so meaningful progress prose is captured once as typed memory and the
checkpoint is not a second status ledger. No raw reasoning trace or transcript
is required. If the structured result is absent or invalid, Engram refuses the
report, and the begun turn stays open until a valid report lands.

Capture is not a quota and never gates an ordinary edit on creating a memory.
Requiring a note before mutation would be satisfied by low-value prose and
poison the corpus. Engram automatically
records structural metadata (effect class, touched resource subjects, exit status,
artifact fingerprints), prompts for semantic capture at handoff, resolved
denial, contradiction resolution, irreversible boundary, and freeze, and
requires semantic content only in the existing finalization contribution.

## Freshness without constant revocation

Dense positions in named project, root-work, and run-execution feeds order
peer-visible changes. Two separate
epochs invalidate grants without conflating scope:

- project `policy_epoch` changes when the active `ControlPolicy` or mediated
  capability declaration changes;
- work `admission_epoch` changes when authorization-relevant work/run state
  changes, including an applicable pinned rule being added, superseded,
  retracted, or contested; participant access changing; or work/run/report state
  changing.

Every turn grant records both epochs.
Ordinary peer notes advance the root-work feed but do not revoke an
already-running local action. A
project-policy or work-admission epoch change invalidates affected grants
immediately: `turn_begin` rechecks both, so a newly arrived hard constraint
cannot be bypassed by an issued grant. Work-claim ownership changes are fenced
independently.

Dense named feed positions order changes, the project-policy epoch invalidates
global rules, the work-admission epoch invalidates work/run rules and
lifecycle, the claim fence invalidates stale responsibility. None substitutes
for another.

Turn grants deliver no peer changes, so no per-session blocking watermark
holds a turn. Agents read peer changes through `next`. The planned impact
classifier (`blocking`, `advisory`, `informational`) and its `delta_backlog`
obligation are not built.

## Durable control records

Control uses two explicit persistence tiers:

- **canonical state-changing events** — checkpoints, handoffs, policy
  activation, finalization, and recovery. They follow ordinary work retention
  and are the source for rebuilding projections;
- **bounded operational records** — live/expired turn grants plus
  allow/refusal diagnostics. They are immutable while live, idempotently
  addressable, invalidated on restart where specified, and pruned after their
  terminal retention window. They are not canonical memory or peer context.

The minimum records are:

- `ControlPolicy`: version and hash, control mode, mediated effect classes,
  project epoch, classifier version, synchronization rules, grant TTLs,
  degraded-envelope rules and portable writer-validation maximum age. Machine
  policy is never inferred from documentation prose.
- `ParticipantRecord` and `SessionProgress`: asserted actor and role, expected
  contribution, join/leave state, durable phase, current claims, and last
  checkpoint. The shipped `control_sessions` table still has
  `confirmed_cursor`, `tentative_cursor` and `blocking_watermark` columns. No
  decision reads them; the loader only checks they are non-negative. Only a
  bind writes them, as zero or null, so a row written earlier keeps its last
  values until it is rebound. They are retained only so the schema stays
  unchanged until the next planned migration drops them.
- `WorkClaim`: work/run holder, assignment reference, expiry, revision,
  monotonic claim fence, and transfer/recovery lifecycle.
- `HandoffOffer`: exact work-claim handoff, recipient, expiry, and transfer lifecycle.
- `ReportAssembly` and `ReportAssemblyClaim`: root completion-seal id,
  assembly generation/state/revision, designated holder, expiry, revision,
  monotonic fence, and handoff/recovery lifecycle. This is post-completion
  authority and is never a substitute for a work claim.
- `PortableWriterState`: configured mode, lineage/head, local store instance,
  writer state/epoch, last remote validation time/result, maximum validation
  age, and released/read-only state. Remote mismatch or validation expiry
  advances the local admission epoch before another mutation-capable grant.
- `TurnGrant`: immutable intent binding, expiry and one-use state; terminal
  grants are operational. Action grants and receipts are not built.
- `RequestKeyTombstone`: compact durable binding of request kind, key,
  session/work/run, intent fingerprint, terminal state, and optional result id. It
  outlives a pruned grant through the work retention boundary and can never
  mint authority.
- `DegradedEnvelope` and `DegradedActionDebt`: bounded cached degradation
  authority and typed host-spooled reconciliation evidence.
- Deferred report/finalization records — `ParticipantContribution`,
  `CompletionBarrier`, and `FrozenReport`: source hashes, validation evidence,
  checkpoint cursor, roster/waivers, immutable report bytes, and publication
  intent. These are target contracts, not shipped Rust types or tables.

Every safety-relevant projection can be rebuilt from canonical transitions;
restart deliberately discards any authority that existed only in a live
grant. High-volume allow and refusal diagnostics stay in bounded operational
storage unless they change peer behavior; they are not echoed into context or
any external adapter.

### Policy bootstrap and precedence

`ControlPolicy` and its `policy_epoch` are project-scoped in V1; task-specific
policy languages are out of scope. The per-project SQLite store is the V1
selection scope. `engram init --required-assurance advisory|turn_gated|action_gated`
with `--authorized-by <actor>` installs a versioned built-in safe policy,
records the explicit operator choice as asserted attribution, and atomically
selects its hash. `turn_gated` is the default; plain `engram init`
uses synthetic system attribution because no operator choice was made. Plain
`engram init` preserves the selected policy on an existing current store. Any
missing, different-build, or corrupt schema or active policy fails store open for
every service surface; it never falls back to advisory memory or issues a
grant. `doctor --recover-policy` may inspect the policy family read-only but
cannot return a usable store or enable mutation.
On a cold store, core/control DDL, host path-policy binding, and the canonical
policy selector/history commit in the same immediate transaction, so a crash
cannot leave an empty policy table that later resembles established state.

A shipped assurance update runs through
`engram control-policy set-required-assurance`. It creates a new immutable
policy version plus an
attributed canonical authority-decision object—not one arbitrary task's
work claim. V1 records the operator identity as asserted host context;
authenticated `project_policy_admin` mediation remains unavailable and is
reported as an unavailable authority-mediation capability by `doctor`; the
setter also warns that the specific supplied identity is asserted rather than
authenticated. Selecting the hash and incrementing the project epoch is one
SQLite transaction with an optional expected-policy-hash compare and swap.
Both host/operator policy setters require a store-scoped idempotency key. The
normalized intent deliberately excludes the caller's retry-time clock, while
the exact receipt retains the originally committed activation timestamp. The
receipt commits with the policy activation and replays after restart or an
uncertain response before the expected-hash check; same-key different-intent
reuse is refused. Reapplying the active assurance under a fresh key persists
an exactly replayable no-op receipt. Every `turn_begin` reads that
project epoch plus the bound task's `admission_epoch`, so a project mismatch
invalidates issued grants across all active tasks without a non-atomic
row-by-row update; the refused session adopts the new epoch and must evaluate
once again. When the new requirement exceeds the host's declared assurance,
the assurance check runs first and fresh evaluation refuses with
`control_assurance_insufficient` instead of `policy_epoch_changed`. Selecting
`action_gated` warns immediately that no current V1 host can bind at that
level and prints the `set-required-assurance turn_gated` recovery command. A
begun grant remains checkpointable under its frozen basis so durable progress
is not lost. Notifications are only doorbells.

Policy history is ordered exclusively by `policy_epoch`. `activated_at` and
authority `decided_at` are attribution timestamps; clock skew does not reorder
the immutable chain or block activation.

The bind/evaluate/begin hot path verifies the selected version's
canonical hash and projection bytes, matches its selector scalars, and uses an
indexed successor probe to refuse a rolled-back head. It deliberately does not
walk predecessor objects because prior versions do not participate in a
live decision. Store open, policy activation, `doctor`, and integrity
verification additionally traverse and verify the complete authority-bound
predecessor chain. Every object must use the current supported schema. Epoch
one uses the built-in envelope, and `set_required_assurance` may change only
`required_assurance`, preserving supported effects, grant TTL, and the selected
obligation rule set.

Current policy state requires one canonical obligation-rule-set selection. The
operator-only
`engram control-policy set-obligation-rule-set` command may append an
attributed successor under an epoch/id compare-and-swap. It accepts bounded,
strict JSON inline or through `@file`, and activates only the fully re-supplied
typed set; rollback never trusts a hash alone. The command is not exposed
through MCP or the host turn protocol. V1 rule sets are bounded typed data,
not a natural-language rule engine; unknown schemas, nested fields, duplicate
rule identities, and unknown triggers fail closed. General conditions,
additional trigger/evidence vocabularies, configurable blocking phases, and
waiver-authority rules remain deferred.
Host and user authority is the ceiling; `ControlPolicy` configures mediation,
synchronization, TTL, and conflict behavior below that ceiling; task-applicable
hard/firm pinned rules may further restrict execution but never grant a denied
capability. Unknown versions or ambiguous precedence fail closed.

## Coordination and parallel work

Root-execution membership grants visibility, not write ownership. V1 assigns
one ordinary executor and one fenced work claim to each `WorkRun`; parallel
sessions claim distinct child runs. Claims schedule execution and protect
work/run mutations. They do not authorize filesystem or external writes:
those remain subject to host/user authority.

Resource leases, acquisition/release operations, and lease fences have been
removed. `resource_intents: []` remains valid; supplied subjects are normalized
but do not reserve paths or prevent overlapping edits. Sessions coordinate
their source ownership explicitly, including while a test/review input is frozen.

A handoff transfers work responsibility under the claim fence. The incoming
session must synchronize independently; it never inherits the previous
session's unacknowledged delivery or live turn grant. Stale claims cannot
mutate the run after handoff or recovery.

## Failure and recovery behavior

A single global fail-open/fail-closed switch is unsafe and unusable. The
built-in policy applies a capability-specific matrix; deployments may make it
stricter but cannot weaken non-overridable cells:

| Failure | Observe | Communicate | Reversible local mutation | Shared mutation | External effect / lifecycle |
| --- | --- | --- | --- | --- | --- |
| Decision service unreachable or deadline exceeded | Open | Closed | `degraded_open` only inside a cached envelope | Closed | Closed |
| Store corruption or unknown safety schema | Diagnostic-only | Closed | Closed | Closed | Closed |
| Portable writer epoch unknown/stale/expired | Open | Closed | Closed | Closed | Closed |
| Stale work-claim fence | Open | As the work contract permits | Refuse the bound turn | Refuse | Closed |
| Unknown prior action outcome (not built: needs action checks) | Open | Unrelated capture only | Unrelated work only | Closed when related | Closed when related |
| User/host denial or missing authority (not built: needs action checks) | As host permits | Closed for denied capability | Closed for denied capability | Closed | Closed |

`degraded_open` is never silent fail-open. While Engram is healthy it may issue
a cached `DegradedEnvelope` bound to session, policy hash/epoch, capability and
resource bounds, work-claim basis, expiry, maximum actions/bytes, and the host
mediation map. The host may use it only for policy-designated reversible local
work; without a valid envelope it fails closed. The host does not independently
reconstruct or widen Engram policy while the service is unavailable.
Envelope expiry may not exceed any basis grant expiry, is capped by a
short built-in maximum, and is invalidated by any project/task epoch
notification the host receives. An unavailable service cannot extend it or
suspend its clock.

Portable writer validation is not covered by `degraded_open`. In portable
mode, process/session start and crash resume must read only the remote
manifest head/epoch before enabling mutation; a bounded cadence repeats that
authority check. This metadata read is not context retrieval and does not make
the remote a live work database. Mismatch, expiry, or unavailability leaves
disclosure-authorized reads/diagnostics open but makes the local store
mutation-read-only and invalidates affected grants. Local-mode projects never
perform this check.

Each use appends a host-local `DegradedActionDebt` containing envelope id,
policy basis, prior fences, monotonic and wall timestamps, request/action
fingerprint, resource subjects, status, and idempotency key. The spool is
append-only, crash-durable, owner-restricted where the OS supports it, and
protected to the same declared level as other host control state; if those
conditions cannot be met, degradation is unavailable. Communication remains
closed rather than inventing a second offline message ledger; after recovery,
an ordinary turn may capture any durable semantic finding through the
ordinary typed-memory path.

On recovery the session returns to `ready`, uploads debt idempotently,
verifies current policy and claims, fingerprints touched
resources, and records `accepted | conflict | operator_required` reconciliation
for each entry. Shared/external/lifecycle actions remain unavailable until all
debt is terminal.

Recovery capabilities remain available where disclosure permits: inspect the
refusal, read work changes through `next`, resolve a contradiction, reconcile
an unknown action (not built: needs action checks), wait,
contribute, and request an attributed human exception. Each refusal names the
repair it needs, so the host does not turn normal contention into an
unsatisfiable refusal loop.

A break-glass exception is an explicit, scoped, expiring, human-attributed
event naming the denied capability and reason. It cannot make an advisory host
action-gated or turn asserted identity into authenticated identity. Store
corruption, an unknown safety schema, a user/host denial, missing mediation,
and unverifiable external authority are non-overridable. Break-glass is
limited to policy-authorized coordination exceptions after those invariants
pass.

## Crash, restart, and replay

- Host or Engram restart invalidates unbegun grants; the durable session
  resumes at `ready`, and a begun turn stays open until the host reports it.
- Turn, begin and checkpoint requests use independent idempotency keys
  bound to canonical intent fingerprints and, for begin and checkpoint, the
  exact grant id.
- Exact decision retries while the result is retained return that result.
  A policy change needs a fresh evaluation key; changing intent under an old
  key is a conflict.
  After grant/result pruning, the durable request-key tombstone returns
  `expired_request`; it never treats the old key as fresh. Reuse with a
  different intent is always a conflict until the task's explicit retention
  boundary. Stored operation receipts keep their durable retry semantics;
  per-action and publication receipts are not built.
- Expired unbegun grants never resurrect. Work-claim transfer or recovery
  advances its fence; old claims cannot authorize new turns.
- An issued but unbegun grant expires unused. A begun turn whose outcome is
  unknown after a restart stays open until the host reports it.
- Ordered events allow a replacement host process to reconstruct session and
  task projections before issuing another grant.

## Completion and optional finalization under control

In the target controlled-completion path, `work_complete` enters
`completion_pending`. That transaction freezes the
executor checkpoint obligation, advances the work admission epoch, and denies
new ordinary execution mutation grants. Engram then drains in-flight actions,
terminalizes the work claim
while allowing reconciliation and abort. Root completion additionally
freezes the `RootExecution` contributor roster and every required child seal
or explicit reason-attributed disposed-child waiver. Only
after the drain succeeds does a
`completion_seal` transaction capture a dense run-feed cut and bind the work
revision, run/claim fences, executor checkpoint, action outcomes, acceptance
results, and evidence ids. A root seal also binds required child seals or
disposed-child waivers, contributions, decisions, and attributed participant
waivers. It makes the work completed; an
attributed abort before the seal returns it to `open`. The shipped alpha only
accepts the zero-linked-state path: it requires empty action-outcome drains
and creates the seal atomically. Nonempty action drains remain unsupported.
The historical resource-lease drain field remains empty; the lease engine
and its live authority have been removed.

Before any new seal is frozen, every obligation definition whose trigger is at
or before the completion cut must have a satisfied or operator-attributed waived
resolution at or before that same cut. The final checkpoint must acknowledge
the typed verification evidence. Required child seals are decoded and checked
recursively; every accepted child seal carries the exact current obligation
and environment schema bindings.

An executor may seal its run only after its last turn is checkpointed, all
material outcomes are known, its host-confirmed source-feed progress reaches
the frozen cut. A root contributor may mark ready only after its claimed child
run has sealed or its omission is authorized. Contribution and readiness
events before the root seal do not move the cut.
Discovery of new execution work atomically aborts completion, invalidates the
barrier, advances the work admission epoch, and requires a later fresh cut.

Optional report finalization consumes the immutable completion seal; it does
not drain execution a second time. Engram creates a `ReportAssembly` anchored
to the root seal and issues a fenced `ReportAssemblyClaim` to the designated
assembler. This planned assembly claim binds the seal and assembly generation,
requires no completed-run work claim, and cannot authorize workspace mutation.
Report freeze terminalizes the assembly claim. There is no finalizer turn purpose
or phase in the host protocol.
Publication remains a separately authorized external side effect. Its intent
binds the frozen report hash, target, and idempotency key. An attributed report
abort before `report_ready` abandons only assembly; it does not reopen the
completed run. `report_ready` remains the irreversible bytes/hash freeze
boundary, and revisions supersede rather than reopen the frozen report.

No final assistant message, process exit, or external tracker closure
implicitly completes Engram work or marks a report published.

## Host integration contract

A conforming host adapter must:

1. declare its mediated capability set and control assurance honestly;
2. bind a unique durable host session and selected local work run before
   delivering task prompts;
3. call `turn_evaluate` and surface any blocking directive before each turn;
4. prevent prompts while the session is not `ready`;
5. (not built) in action-gated mode, intercept every declared material
   capability and require and begin a matching single-use action grant;
6. (not built) report action outcomes even when the model turn later fails;
7. request and persist a turn checkpoint before starting the next turn;
8. checkpoint before context compaction and reconcile or exit before ending a
   session;
9. treat Engram notifications only as doorbells and fetch state by cursor;
10. resume after restart through a fresh bind, never from cached permission;
11. surface refusal codes and recovery actions to the human and agent.

Hooks are sufficient for `turn_gated` control. `action_gated` control requires
a runtime wrapper, gateway, or native host integration around tools. MCP alone
is agent-facing and therefore advisory unless the host itself invokes a
separate, non-agent-exposed control channel and enforces its decisions.

Mandatory hot-path mediation also creates a latency contract: target p99 is
under 10 ms for an uncached local allow and under 1 ms for a client-side check
inside a live scoped grant. The target runtime is one long-lived host-local
`engram serve` process per project store with thin hook clients, not a fresh
SQLite/process startup per tool call. Deadlines always produce an explicit
fail-mode result; a hung hook is not an acceptable control mechanism.

## Planned interfaces

The shipped host channel is `session_bind`, `session_status`,
`turn_evaluate`, `turn_begin` and `turn_checkpoint`. The design named seven
more. None is built, and no host calls them:

- **`action_authorize`, `action_begin` and `action_complete` (not built).**
  They would put a single-use grant around each material tool call. A host
  could then stop one risky action rather than a whole turn, and record each
  action's outcome. An action whose outcome is unknown would never be replayed
  blindly. Filesystem effects would add a handle-bound check that the path
  still resolves to what was authorized. This is what `action_gated` assurance
  would mean; today every effect is decided once per turn.
- **`control_bootstrap`, `session_heartbeat` and `session_exit` (not
  built).** They would give a session an explicit lifecycle around binding.
  Today `session_bind` starts it and the turn report's `exit` intent ends it.
- **`delivery_ack` (not built).** It would acknowledge a delivered context page
  without a model turn. Grants no longer carry a page, so there is nothing to
  acknowledge.

Engram never answers `defer`; a turn is granted or refused.

The host control surface uses the stable spellings defined by
`ControlRefusalCode`: `unknown_control_schema`,
`control_assurance_insufficient`, `capability_not_permitted`,
`task_unbound`, `task_access_denied`, `policy_epoch_changed`,
`task_admission_epoch_changed`, `turn_already_open`, `grant_expired`,
`grant_not_begun`, `grant_scope_mismatch`, `stale_fence` and
`session_exited`. `recovery_required`, `delta_required`,
`turn_purpose_mismatch`, `context_required`, `delivery_invalid` and
`pinned_budget_exceeded` are kept only to read refusals stored before grants
stopped carrying a page, and `lease_required` only to read refusals stored
while resource leases existed; current evaluation never produces them.

## Preconditions in the current implementation

The pure evaluator remains; the obsolete persisted shadow-observation log is removed.
The current host-control alpha additionally ships a built-in safe policy,
durable control sessions, optional exact `WorkRun` claim bindings,
persisted turn decisions and short-lived grants that carry no delivery page,
begin-time rechecks, canonical execution observations, and canonical checkpoint
events. A separate `engram control` JSON-lines process
implements `session_bind`, `session_status`, `turn_evaluate`, `turn_begin`, and
`turn_checkpoint`; none is
exposed through agent-facing MCP. Exact retry
evidence survives process restart, while unbegun authority is invalidated and
the session returns to `ready`. Each open rotates an internal
connection generation so a still-running predecessor is fenced. Begun grants
stay open until reported and are discoverable through session status; no
payload is redelivered.
`doctor` verifies canonical intent/result bytes plus their redundant row
bindings.

The alpha grants `observe`, `communicate`, and turn-gated `mutate_local`.
It checks declared mediation, assurance floors, and any exact work-claim binding.
Resource leases and host obligation waiver are removed. Supplied resource intents
are project-bound and normalized; an empty list is valid and no list reserves
exclusive ownership. The persisted host path policy refuses unresolved or
inconsistent filesystem identity rather than guessing path semantics.
The host remains responsible for path resolution and actual mutation authority.
Each turn report appends one event to the bound task's change index, a
write-only audit trail that no grant delivers and no decision reads.
`action_gated` declarations and shared/external/lifecycle turn effects are
rejected. The decision service becomes a real `turn_gated` deployment only when
an embedding host makes it mandatory, as TermAl does.
`engram doctor` verifies the immutable active-policy chain and reports its
hash, epoch, required assurance, built-in effect envelope, live turn counts,
and explicitly discloses that action gating, organizational authority
mediation, and action-outcome reconciliation are unavailable. Selecting an
`action_gated` requirement is therefore a deliberate fail-closed
configuration: no current host may bind at that level. The required
per-host-tool mediation map is still outstanding.

Broader enforcement must remain disabled until Phase 1 makes these invariants
true in the core, not only in wrappers:

- every behaviorally relevant task/memory/lifecycle transition advances
  the authoritative feed and its head projection, and delta requests cannot
  jump or acknowledge undelivered ranges;
- capture, claim, task transition, and publication entry points validate work
  existence/state, root-execution membership, focused-run claim, and
  applicable grant/assembly-claim rules;
- per-action mediation must retain execution-bound path resolution; and
- work/run states, contributions and completion seal are wired on a real
  restart-safe path. The optional report freeze, durable publication intent
  and adapter receipt remain deferred; any future publication capability must
  be proved end to end before it can claim controlled finalization.

Until those are process-tested, the existing MCP loop remains advisory. The
host-control alpha can authorize only a declared local-mutation *turn*; it
cannot authorize an individual tool action, shared/external effects, lifecycle
transitions, or finalization.

## Delivery sequence

| Phase | Deliverable | Honest control claim |
| --- | --- | --- |
| 0 — observe and replay | Safe policy bootstrap, daemon/thin client, host mediation map, decision log, latency/false-refusal baseline; control decisions are shadow-only and never weaken existing user/host denials or shipped packet safety errors | Advisory observation only |
| 1 — repair prerequisites | Transactional context snapshots, consistent task cursor, work transitions, contribution barrier; optional publication remains deferred | No new control claim |
| 2 — freshness mediation | Impact-classed events, durable tentative/checkpointed delivery, recovery grants, pre-turn inline packet/delta, compaction re-delivery, checkpoint; enforce only unknown-schema, unsafe-packet, and failed-required-injection refusals | `turn_gated` delivery plus the minimal non-overridable refusal set |
| 3 — scoped coordination | Fenced work claims, handoff/recovery, and explicit source ownership | Work scheduling, not resource locking |
| 4 — widen refusal and action gate | Enable the broader replay-proven closed turn-refusal set, degraded-envelope matrix, and action authorization/begin/outcome mediator | `action_gated` for the declared capability set |
| 5 — controlled completion/finalization | Stable completion cut, recovery grants, contribution barrier; any future optional report freeze and receipted publication must be proved end to end | End-to-end controlled local loop |

Phase 2's pre-turn packet/delta delivery, compaction re-delivery and recovery
grants, and phase 5's recovery grants, are dropped: turn grants carry no
delivery page, and there are no recovery turns (see the
[turn gate assessment](turn-gate-assessment.md)).

The current implementation deliberately process-tests a narrow
`observe`/`communicate` lifecycle plus turn-gated local-mutation turns while
Phase 1 is still incomplete. That validates the host protocol, restart
semantics and work-claim fencing; it does not
skip action mediation, report, or entry-point
prerequisites or widen the honest deployment claim.

Each phase needs process-level tests with a deliberately non-cooperative test
agent. A passing happy-path MCP script proves usability; control tests must
also prove that direct turns, stale grants, wrong-session replay, unmediated
declared actions, unknown outcomes, and premature finalization are refused.
Policy changes replay against recorded structural traffic before activation;
tests and telemetry measure mediation coverage, false-refusal rate, time to
clear a directive, degraded-mode debt, contention prevented, and refusal-loop
incidents. A rule language, scheduling queue, semantic capture obligation,
and broader denial set remain out of scope until replay evidence justifies
them.
