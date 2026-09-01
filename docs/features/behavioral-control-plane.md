# Behavioral & Coordination Control Plane

> Normative references: [spec §2.7](../spec.md#27-execution-control) and
> [spec §8](../spec.md#8-interfaces).
> Related briefs: [context packets](context-packets.md),
> [local work system](local-work-system.md),
> [local tasks & reports](local-tasks-and-reports.md),
> [CLI & MCP](cli-and-mcp.md),
> [security & trust](security-and-trust.md), and
> [execution pipeline](execution-pipeline.md).

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
| Engram | Local work graph and readiness, task-bound execution admission, context obligations, claims, participant coordination, scoped leases, checkpoints, handoffs, evidence, completion, and publication intents | Model/process supervision or source-control policy |
| Host runtime | Session/process lifecycle, prompt injection, tool interception, wake-up delivery, user approvals | Engram's durable task truth or policy decisions |
| Agent | Reasoning and proposed work within a granted envelope | Self-authorizing a turn, lease, acknowledgement, or external side effect |
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
| `action_gated` | The host also intercepts every configured material capability and requires an action grant | Engram controls turn admission and the declared material capability set |

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
by host assurance, while evaluation and lease acquisition refuse effects above
that cap with `control_assurance_insufficient`. Assurance refusals at both
boundaries are policy decisions carrying an effect-naming `ControlDirective`,
never transport error envelopes. Mediation-envelope refusals also return the
declared and assurance-capped effective effect sets so one host renderer can
explain the same decision at turn and lease boundaries. A
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
run's completion state and the optional report state:

```text
unbound → sync_required → ready → turn_open → checkpoint_required → ready
              ↑            │             │              │
              │            ├── wait ─────┘              ├── handoff_pending
              │            └── policy change ───────────┘
              └──── restart / expired grant / stale epoch

sync_required → recovery_open → checkpoint_required → sync_required
                                                        └─ reevaluate → ready

handoff_pending ── accepted/released ──→ exited
        └───────── canceled/expired ───→ sync_required

ready → completion_required → participant_ready → exited
                                  └── optional finalizer_open → exited
```

- `unbound`: the session has no active task reference. Only binding and
  diagnostic operations are available.
- `sync_required`: required context has not been host-acknowledged, peer
  deltas are outstanding, a project-policy or work-admission epoch changed, or
  the host
  restarted. Only recovery capabilities are available.
- `ready`: all blocking directives are acknowledged and required leases are
  either held or can be requested with the intended action.
- `turn_open`: one unexpired turn grant exists. Replaying the same turn intent
  returns the same grant; a different intent under the same idempotency key is
  a conflict.
- `checkpoint_required`: the model turn ended. Material action outcomes and a
  structured turn checkpoint must be reconciled before another ordinary turn.
- `recovery_open`: one `purpose: recovery` grant permits only the exact
  directive ids and capabilities named by the refusal. Its checkpoint returns
  to `sync_required`; Engram reevaluates durable state before `ready`.
- `handoff_pending`: the session has declared the work transferable but the
  lease transition and handoff checkpoint have not both completed.
- `completion_required`: local completion requires the session to checkpoint,
  release or transfer its leases, satisfy evidence obligations, and
  contribute.
- `participant_ready`: the contribution is frozen and the participant is no
  longer permitted to perform ordinary execution mutations. The designated
  report assembler may later acquire a separate `ReportAssemblyClaim` and
  receive a narrowly scoped finalizer grant;
  an attributed completion abort before the seal sends participants back to
  `sync_required`, while a later report-assembly abort does not reopen work.
- `finalizer_open`: one designated participant may assemble and polish the
  report from the immutable completion seal under a fenced
  `ReportAssemblyClaim` without reopening ordinary execution.
- `exited`: no active grant or lease remains. Rejoining begins at
  `sync_required`.

`blocked` is not a catch-all session phase. A refusal names the precise
blocking condition and the operations that remain safe. This avoids states
that cannot explain how to recover.

## Host-facing control protocol

The substrate-neutral core exposes a small lifecycle protocol. Transports may
batch operations, but may not weaken their semantics.

### 1. Register and bind

`session_bind` records the asserted actor, host session, control-assurance
level, declared mediated capabilities, and an optional exact work binding:
`root_execution_id`, `work_id`, `run_id`, `work_revision`, `claim_id`, and
`claim_fence`. Storage verifies that tuple against the session's live claim and
copies it into each grant. The six-operation work protocol supplies this tuple
directly as `work_update:claim.receipt.control_binding` and
`work_focus.control_binding`; the focus run section also names its root
execution and work item. Here `work_revision` is the work item's revision
returned at the top of the claim receipt, not the claim object's revision. A host
may first seed a local root from a user request or an optional external
snapshot. Binding never requires an external reference. V1 has one ordinary
executor per `WorkRun`; an ordinary turn additionally requires that session's
live claim on the focused run. Other root members may inspect permitted root
memory and communicate, or claim distinct child runs for parallel work, but
membership does not authorize mutation/completion of another executor's run.
Initial roles come from a user/host authority reference or the active control
policy. While active, the root coordinator may change the expected roster
under that authority and its live root-run coordination lease. Joining or
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
cursor, policy, membership, and lease facts come from Engram's durable state;
caller fields are never accepted as proof that a precondition is true.

Feed and delivery positions have different identities:

```text
FeedPosition {
  kind: project | root_work | run_execution
  id, position
}

DeliveryPosition { session_id, position }

FeedRange {
  kind: project | root_work | run_execution
  id, from_position, to_position, observed_head_position
}
```

The decision is a grant, defer, or refusal:

```text
TurnGrant {
  grant_id, intent_hash, work_id, work_revision, run_id, session_id, turn_id
  purpose: ordinary | recovery | finalizer
  authority_basis:
    ordinary { work_claim_id, work_claim_fence }
    recovery { directive_ids[], authority_refs[] }
    finalizer { completion_seal_hash, assembly_id, assembly_generation,
                assembly_revision, assembly_claim_id, assembly_claim_fence }
  basis_feed_positions[]        # FeedPosition
  basis_delivery_position       # DeliveryPosition
  basis_project_policy_epoch, basis_work_admission_epoch
  basis_portable_writer_epoch?, portable_writer_valid_until?
  packet_hash
  required_injections[]          # exact delivery token + bounded payload
  capability_envelope[]
  resource_lease_fences[]        # lease_id + fencing_epoch + expiry
  expires_at
  directives[]
}

TurnRefusal {
  intent_hash, code, message
  current_feed_positions[]      # FeedPosition
  current_delivery_position     # DeliveryPosition
  current_project_policy_epoch, current_work_admission_epoch
  blocking_directives[]
  recovery_capabilities[]
  recovery_turn_grant?          # a TurnGrant with purpose: recovery
}

TurnDefer {
  intent_hash, code, message
  retry_after_ms?, wake_on_feed_position?
  blocking_directives[]
}
```

The normal pre-turn result is a grant **with the required context and peer
delta already attached**, not a refusal telling the agent to call another
tool. Immediately before dispatching the prompt, the host calls
`turn_begin(grant_id, delivery_tokens[])`. In one transaction Engram rechecks
grant expiry, project-policy epoch, work-admission epoch, work revision, the
purpose-specific authority basis, resource-lease fences, optional portable
writer epoch/validation deadline, the session blocking watermark, and each
exact token, then records a **tentative** delivery position
and source-feed progress vector and activates the grant. Without that transition the host must
not deliver the prompt.
Standalone delivery calls remain available for preloading and recovery, but
freshness is not delegated back to voluntary MCP.

Tentative delivery is not proof that prompt dispatch succeeded. The last
checkpointed delivery position and source-feed progress vector remain
separate. A successful turn checkpoint promotes both atomically. Restart
invalidates an unbegun grant and returns it to `sync_required`; an uncertain
begun grant remains `turn_open` and checkpoint-required. The one safely
replayable case is a begun observe-only partial recovery page:
`session_status.recoverable_grant` returns its exact frozen delta so the new
host can redeliver it before checkpointing, while the confirmed cursor remains
unchanged. Other begun turns expose only their id because replaying a prompt
with possible effects would be unsafe.
A fresh evaluation key likewise atomically supersedes an issued-but-unbegun
grant. The same transaction records an immutable supersession transition that
binds the old grant and request key to the replacement request and decision,
with a typed reason and timestamp. It never replaces a begun grant: that
session stays `turn_open`, reports `open_grant_state: begun`, and refuses the
evaluation with `turn_already_open` until checkpoint/reconciliation completes.

A grant is an immutable, fingerprinted operational record while live. It is
bound to one task, session, turn intent, both control epochs, capability
envelope, and lease fences; it is not a bearer token transferable to another
session. Restart invalidates issued-but-unbegun authority. A begun grant stays
durable only until checkpoint/reconciliation, including the narrowly
redeliverable recovery case above, so completed, expired, and superseded grants
still need not become permanent canonical memory. State-changing grant
supersession, delivery, checkpoint, lease, action, handoff, and finalization
transitions emit immutable canonical events.

Evaluation order is fixed so refusals are deterministic and do not leak later
state through an earlier failure:

1. verify store integrity plus recognized event, object, and control-policy
   schemas;
2. verify durable root-execution membership, focused work/run binding, and
   asserted host control declaration;
3. verify that session/run lifecycle states allow the requested activity and,
   for an ordinary turn, that this session holds the run's live work claim;
4. construct applicable pinned context without overflow or unresolved policy
   contradiction;
5. attach the next required bounded context/delta delivery not already
   acknowledged, preserving dense task-local cursor ranges and an omission
   manifest; partial pages authorize only recovery turns, and the final page
   attaches the context snapshot at the delivered head;
6. verify required resource ownership, fence, and expiry for the requested
   capability envelope; and
7. verify no checkpoint, unknown action, handoff, contribution, or
   finalization obligation is outstanding.

Only then can a turn grant be minted. Missing deliverable context normally
creates `required_injections`, not denial. A refusal or defer is reserved for
an unsafe packet, unreconciled recovery, lifecycle hold, unknown prior action,
unavailable required authority, or a condition that cannot fit safely in the
bounded delivery. Time is an explicit evaluator input, not a hidden wall-clock
read, so replay and boundary tests are deterministic.

Refusal does not mean the host must deadlock or bypass the gate. Directives are
classified by executor:

- **host-automatic** — deliver context, apply a delta page, wait, report an
  already-known action outcome, renew a safe lease;
- **human-only** — supply missing authority, choose a policy exception,
  reconcile an ambiguous non-idempotent side effect;
- **agent recovery turn** — reason about a pinned conflict, choose a resource
  scope, prepare a handoff, or produce a contribution.

For the last class, the reevaluation may produce a short-lived `TurnGrant` with
`purpose: recovery`, bound to exact directive ids and only the read/capture or
coordination capabilities needed to resolve them. It cannot authorize
ordinary workspace mutation, a new external effect, or unrelated reasoning.
Its result must be checkpointed before admission is reevaluated.

### 3. Acknowledge deliveries

The shipped turn channel stages the supplied packet/delta at `turn_begin`, and
checkpoint makes it durable. A future standalone `delivery_ack` may perform
the same transition without a model turn. Neither operation claims that the
model understood the content. Each
delivery has an independent per-session sequence and exact source ranges:

```text
ContextDelivery {
  token
  delivery_position             # DeliveryPosition
  source_ranges[]               # FeedRange
  has_more_by_feed[]
  content_digest, packet_hash?
  basis_project_policy_epoch, basis_work_admission_epoch
}
```

The acknowledgement cites that token, host session, expected prior delivery
position, and idempotency key.

Blocking directives are typed, addressed, and carry a satisfaction mode:

```text
Directive {
  directive_id, kind
  audience: host | agent | human
  satisfaction: delivery_ack | state_predicate | authority_ref | none
  parameters
}
```

Kinds include:

- `bind_task`
- `load_context`
- `apply_delta`
- `resolve_pinned_conflict`
- `acquire_lease` / `renew_lease`
- `checkpoint_turn`
- `release_or_handoff`
- `wait`
- `contribute`
- `finalize`

Initial context uses the same delivery protocol as deltas. The shipped
`turn_begin` stages only the exact page frozen into its grant;
`turn_checkpoint` promotes the contiguous session cursor in one transaction.
For bounded backlogs, `has_more` pages carry deltas only and grant observe-only
recovery turns; the final page carries the context packet. Every step uses the persisted
grant and expected state. A caller cannot jump beyond the task head, skip or
reorder pages, or use a partial page for an ordinary turn. A future standalone
`delivery_ack` will reuse these semantics.

Other directives are satisfied only by their dedicated atomic operations—for
example, claim, resolve, handoff, checkpoint, contribute, or finalize.
Engram reevaluates their state predicates; acknowledging text never substitutes
for making the required transition. Only informational directives use
`satisfaction: none`.

### 4. Authorize a material action

`action_authorize(ActionIntent) -> ActionDecision` is called by an
action-gated host before a material tool call:

```text
ActionIntent {
  grant_id, action_id, idempotency_key
  capability, effect_class
  resource_subjects[]
  request_fingerprint
  authority_refs[]
}
```

An `ActionGrant` is single-use and bound to the request fingerprint. The
authorization transaction rechecks work revision/run state, grant expiry, project-policy
epoch, work-admission epoch, optional portable writer epoch/validation
deadline, the purpose-specific authority basis,
resource-lease fencing epochs and expiry, session/run status, the
session-specific blocking watermark, and outstanding checkpoint obligations.
It denies an action when relevant state changed after the turn began.

Effect classes are deliberately small and host-neutral:

| Effect | Examples | Default control |
| --- | --- | --- |
| `observe` | Read files, search memory, inspect status | Allowed after required policy context is loaded |
| `communicate` | Root-work-shared capture, contributor message | Requires root-execution membership; becomes an ordered event |
| `coordinate` | Acquire an Engram-internal coordination lease | Requires `turn_gated` assurance and is not a model-turn capability |
| `mutate_local` | Write a mediated workspace or run a mutating local tool | Requires a compatible execution lease on the supplied resource subject |
| `mutate_shared` | Change shared work/run coordination state | Requires membership plus the appropriate scoped lease |
| `external_side_effect` | Publish a report or invoke an external write adapter | Requires an explicit durable intent, user/policy authority, and idempotency key |
| `lifecycle` | Handoff, waive, complete, finalize | Requires the named lifecycle capability and barrier preconditions; finalization additionally binds a `ReportAssemblyClaim` |

An authority reference identifies a durable user approval, host-policy grant,
or coordination authority that Engram can verify as bound to this work/run,
effect, request fingerprint, and validity window. Free-form claims of approval
do not satisfy it. Neither `ControlPolicy` nor a pinned memory can widen a
user/host denial.

V1 uses one canonical structured subject model:

```text
ResourceSubject =
  Path { project_id, segments[], coverage: exact | tree }
  Logical { namespace, segments[], coverage: exact | tree }
```

The host maps tool targets into subjects; the core validates and compares
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

Filesystem `action_gated` assurance additionally requires an execution-bound
`ResolutionBinding`. Before authorization, the host starts from an already
registered project-root directory handle, traverses existing components
without following an unexamined link or reparse point, and retains the
resulting file/directory handles through `action_begin` and invocation. The
binding records the root identity, normalized subject, identities of resolved
components (or the nearest existing ancestor), unresolved tail, and host
capability-map revision. Creation uses the retained ancestor handle and creates
each tail component without link traversal; rename retains and binds both
parent handles. The `ActionGrant` contains the binding digest, and
`action_begin` rejects a changed mapping as `resource_remapped`. POSIX
`openat`/no-follow traversal and the equivalent Windows handle/reparse-safe
operations are examples; the protocol requires the property, not a particular
OS API.

A host that cannot retain handle-relative resolution through invocation may
re-resolve immediately before the call and compare the binding, but that does
not close the final check/use race. Its filesystem coverage is therefore
`detection_only`, not `action_gated`; policy must deny higher-risk path effects
or disclose that downgrade. A post-turn workspace fingerprint detects
out-of-band or unmediated writes but never upgrades detection into prevention.

`exact` overlaps only the same normalized subject; `tree` overlaps itself and
component-boundary descendants. Logical namespaces use the same segment and
coverage rules. Multiple worktrees therefore conflict on the same logical
project path instead of silently diverging through absolute paths. Host
identity and handle retention remain asserted context at V1 assurance, but
normalization, binding comparison, and conflict semantics are core logic and
are exhaustively fixture-tested.

If a generic shell command's write set cannot be conservatively mapped,
policy requires a broad `Path { segments: [], coverage: tree }` workspace
lease or refuses the mutation.
Because not every host can prevent out-of-band writes, the host records a
workspace fingerprint at turn boundaries and reports changed logical paths.
Changes outside held lease subjects create an attributed reconciliation event.
Where prevention is impossible, the assurance claim is detection, not control.

### 5. Begin and record the action

`action_begin(ActionGrant)` atomically consumes the single-use grant and
creates an `in_flight` record immediately before the host invokes the
capability. In that same transaction it rechecks the complete authorization
basis: parent turn and grant state/expiry, project-policy and work-admission
epochs, session blocking watermark, work/run/participant phase, mediated
capability-map revision, request fingerprint, every authority reference, and
each lease's subject/holder/fence/expiry. For filesystem effects it also
compares the execution-bound `ResolutionBinding` digest and requires the host
to invoke through the retained handles. A stale basis refuses without
consuming the grant. Transports may combine authorize-and-begin in one call but
may not omit the begin-time checks. `action_complete(ActionOutcome)` records success, failure, a
durable external receipt, or `outcome_unknown`. The stored evidence is minimal
and redacted: effect class, tool/capability name, resource subjects, request
fingerprint, timestamps, status, and explicit artifact references. Raw
command output and transcripts are not persisted by default.

If a process dies after `action_begin` but before completion, a non-idempotent
action remains `outcome_unknown`. A merely issued grant can expire safely;
Engram never claims the action ran. Engram blocks blind replay of an unknown
outcome until the host or adapter reconciles it. An external side effect can
retry only with the same durable intent and payload fingerprint.

### 6. Checkpoint the turn

`turn_checkpoint(TurnCheckpoint)` closes a turn only after all material action
grants are reconciled. It contains:

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
  lease_disposition
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
canonical observation hash or by an `observation_id` in the same request.
Storage derives the run/source binding, command fingerprint, result, producer
session, and times from that observation; a missing producer is a named
`verification_producer_not_found` request fault. Verification may also cite an
environment object by hash or by same-request index. That object must name the
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
grant's frozen project-policy epoch and records its hash on the
`ExecutionObservation`; every resulting `WorkObligation` repeats that exact
selection. Activating another set affects only observations from later policy
epochs and cannot reinterpret a prior trigger, definition, or completion cut.
Every observation carries the exact selected rule-set hash.

Each match appends one immutable `work_obligation` definition directly to the
project, root-work, and run-execution feeds. A passed `test` verification
appends a separate `work_obligation_resolution` only when it matches the exact
run and the newest source basis visible at the evaluated run-feed cut. That
evidence may satisfy older still-open mutation obligations as well as the
newest one. If the newest mutation has no source basis, no verification can
match it: all open obligations remain waiver-only until a later basis-bearing
mutation and passed test establish a newer verifiable state.

Definitions and resolutions are canonical feed objects; the mutable obligation
row is only a verified projection. `work_focus`, nested `work_next.focus`,
`work_update`, and both completion outcomes use one count- and byte-bounded
`obligation_page` with explicit omission count, immutable identities, state,
rule-set identity, rule, requirement, trigger, terminal evidence/resolution,
and deterministic typed guidance. Generic readiness strings remain separate.
A fresh session reconstructs the same summaries from canonical history rather
than trusting a prior response.

A waiver requires a dedicated
`WorkAuthorityOperation::ObligationWaiver` grant. It may be requested through
the operator CLI or the host-private `obligation_waive` JSON-lines operation,
never through MCP or `work_update`. The private request is bound to the
session's exact live `WorkRun` and expected definition. Its canonical
resolution records the server-fixed session actor beside the asserted
`waived_by` human; neither that assertion nor the grant is authentication.
Policy refusals are typed as `waiver_not_admitted`, `obligation_not_open`, or
`definition_changed`, and exact retries replay exactly. Agent-facing pages and
the host receipt omit the authority grant and reason. Completion evaluates the
cut-aware open set at the exact pre-seal run-feed cut. Terminal definitions are
frozen into the seal as exact definition/resolution hash pairs under obligation
schema V1, and completion success reconstructs its page from that sealed basis.
New seals separately declare environment schema V1 and bind the sorted,
distinct environment-evidence hashes at or before the same dense run-feed cut.
The seal carries hashes only, refuses more than 64 records, and never copies
toolchain, sandbox, or image bytes. Every accepted seal carries the current
environment-schema binding.

An observation effect outside the frozen grant is rejected as
`observation_scope_mismatch`. A checkpoint against an issued-but-not-begun
grant returns `grant_not_begun` with host bind/recovery guidance;
`grant_scope_mismatch` remains the general frozen-basis mismatch. An
exact retry replays the same observation and typed-evidence hashes, while a
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

Coordination transitions and structured action evidence are captured
automatically. Decisions, constraints, facts, and promotion candidates enter
the ordinary typed-memory write path; a checkpoint may cite those captures but
cannot bypass classification, redaction, scope, or promotion policy. One
capture continues to feed peer deltas, handoffs, and report input.

The host should collect a `TurnResult` beside the ordinary model response and
submit one checkpoint operation, not require a second bookkeeping dialogue.
It pre-populates tool/action receipts; the agent adds typed captures, blocker
references, a bounded next-intent enum, and lease disposition. Meaningful
progress prose is captured once as typed memory and cited by hash; the
checkpoint is not a second status ledger. No raw reasoning trace or transcript
is required. If the structured result is absent or invalid, Engram keeps the
session at `checkpoint_required` and returns a repair directive before another
work turn.

Capture is not a quota and never gates an ordinary edit on creating a memory.
`capture_hashes` may be empty. Requiring a note before mutation would be
satisfied by low-value prose and poison the corpus. Engram automatically
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

Every turn grant records `basis_feed_positions[]`, the independent
`basis_delivery_position`, and both epochs.
Ordinary peer notes advance the root-work feed but do not revoke an
already-running local action. A
project-policy or work-admission epoch change invalidates affected grants
immediately. `action_authorize` always rechecks both, so a newly arrived hard
constraint cannot be bypassed by a long-lived turn grant. Lease ownership
changes are fenced independently; expiry invalidates the relevant grant
without advancing either epoch.

The packet hash reproduces delivered content, dense named feed positions order
changes, the project-policy epoch invalidates global rules, the work-admission
epoch invalidates work/run rules and lifecycle, the claim fence invalidates
stale responsibility, and a resource-lease fence invalidates old mutation
authority. None substitutes for another.

Every work/run event stores intrinsic type plus optional audience and normalized
resource selectors. A named, versioned built-in classifier derives effective
admission impact separately for each session and records a monotonic
session-specific `blocking_watermark`:

| Impact | Examples | Effect |
| --- | --- | --- |
| `blocking` | Applicable pinned change/contradiction, claim or lease recovery, completion pending, addressed handoff | Must be injected or reconciled before the affected ordinary turn/action |
| `advisory` | Peer decision or finding relevant to the session's resource intent | Bundled into the next turn when budget permits; omission is visible and may cross a configured threshold |
| `informational` | Unrelated progress and routine lifecycle facts | Delivered opportunistically; never blocks by count alone |

Staleness is based on behavioral relevance, not arithmetic cursor lag. Engram
guarantees only host-asserted delivery. A host-reported context compaction
invalidates packet delivery state and requires the pinned tier to be injected
again, even when the event cursor did not change.

`turn_begin` and `action_authorize` both re-evaluate the classifier and refuse
when the session's blocking watermark is newer than the grant basis and not
satisfied by the begun delivery. Thus a mid-turn addressed handoff or resource
recovery can stop a later action without globally revoking unrelated sessions.
Advisory omissions accumulate bounded count/age/bytes. Crossing the configured
threshold creates a `delta_backlog` obligation that blocks the **next**
ordinary turn until delivery; it does not revoke an in-flight action unless a
matching resource event is independently classified blocking.

## Durable control records

Control uses two explicit persistence tiers:

- **canonical state-changing events** — delivery acknowledgements,
  checkpoints, lease ownership, action begin/receipts, handoffs, policy
  activation, finalization, and recovery. They follow ordinary work retention
  and are the source for rebuilding projections;
- **bounded operational records** — live/expired turn and action grants plus
  allow/refusal diagnostics. They are immutable while live, idempotently
  addressable, invalidated on restart where specified, and pruned after their
  terminal retention window. They are not canonical memory or peer context.

The minimum records are:

- `ControlPolicy`: version and hash, control mode, mediated effect classes,
  project epoch, classifier version, synchronization rules, grant TTLs,
  degraded-envelope rules, portable writer-validation maximum age, and
  lease-conflict policy. Machine
  policy is never inferred from documentation prose.
- `ParticipantRecord` and `SessionProgress`: asserted actor and role, expected
  contribution, join/leave state, durable phase, staged/tentative/checkpointed
  delivery positions, checkpointed source-feed progress vector, blocking
  watermark, outstanding delivery, current claims, and last checkpoint.
- `ContextDelivery` and `DeliveryAcknowledgement`: the exact page bounds,
  named source ranges, per-session delivery position, `has_more`, digest,
  token, and CAS advancement described above.
- `WorkClaim`: work/run holder, assignment reference, expiry, revision,
  monotonic claim fence, and transfer/recovery lifecycle.
- `ResourceLease` and `HandoffOffer`: canonical subject set, mode, holder, expiry,
  revision, fencing epoch, and transfer lifecycle.
- `ReportAssembly` and `ReportAssemblyClaim`: root completion-seal hash,
  assembly generation/state/revision, designated holder, expiry, revision,
  monotonic fence, and handoff/recovery lifecycle. This is post-completion
  authority and is never a substitute for a work claim or resource lease.
- `PortableWriterState`: configured mode, lineage/head, local store instance,
  writer state/epoch, last remote validation time/result, maximum validation
  age, and released/read-only state. Remote mismatch or validation expiry
  advances the local admission epoch before another mutation-capable grant.
- `TurnGrant`, `ActionGrant`, and `ActionReceipt`: immutable intent bindings,
  expiry, one-use state, and minimal outcome metadata; receipts and action
  state transitions are canonical, while terminal grants are operational.
- `RequestKeyTombstone`: compact durable binding of request kind, key,
  session/work/run, intent hash, terminal state, and optional result hash. It
  outlives a pruned grant through the work retention boundary and can never
  mint authority.
- `DegradedEnvelope` and `DegradedActionDebt`: bounded cached degradation
  authority and typed host-spooled reconciliation evidence.
- `ParticipantContribution`, `CompletionBarrier`, and `FrozenReport`: source
  hashes, validation evidence, checkpoint cursor, roster/waivers, immutable
  report bytes, and publication intent.

Every safety-relevant projection can be rebuilt from canonical transitions;
restart deliberately discards any authority that existed only in a live
grant. High-volume allow and refusal diagnostics stay in bounded operational
storage unless they change peer behavior; they are not echoed into context or
any external adapter.

### Policy bootstrap and precedence

`ControlPolicy` and its `policy_epoch` are project-scoped in V1; task-specific
policy languages are out of scope. The per-project SQLite store is the V1
selection scope. `engram init --required-assurance advisory|turn_gated|action_gated`
with `--authorized-by <actor> --reason <text>` installs a versioned built-in
safe policy, records the explicit operator choice as asserted attribution, and
atomically selects its hash. `turn_gated` is the default; plain `engram init`
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
coordination lease. V1 records the operator identity as asserted host context;
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
an exactly replayable no-op receipt. Every
`turn_begin` and future `action_authorize` reads that
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

The bind/evaluate/begin/lease hot path verifies the selected version's
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
attributed successor under an epoch/hash compare-and-swap. It accepts bounded,
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
one ordinary executor and one claim to each `WorkRun`; multi-session
parallelism comes from distinct child runs. Within each claimed run, scoped
resource leases keep work responsibility separate from mutation authority:

- **intent lease** — an advisory reservation taken before work so peers do not
  independently plan the same resource change;
- **exclusive execution lease** — converts intent on first mutation and
  authorizes one session to mutate the resource;
- **coordination lease** — authorizes exclusive task-level transitions such as
  roster changes, waivers, or beginning completion while the run is live;
- **shared analysis** — needs membership and synchronization but no exclusive
  mutation lease.

Leases name one or more canonical `ResourceSubject`s, mode, holder, revision,
expiry, and retry key. Resource identity, collision detection, and fence
history are project-wide rather than task-local, so distinct tasks cannot both
authorize the same path. For different holders, any overlapping `intent` or
`exclusive` subject conflicts:

| Existing \ requested | `intent` | `exclusive` |
| --- | --- | --- |
| `intent` | conflict | conflict |
| `exclusive` | conflict | conflict |

The same holder may idempotently repeat a claim. Converting its intent to
exclusive is one compare-and-swap transaction that rechecks every subject and
keeps the ownership fence. Multi-subject acquisition or conversion is sorted
canonically and all-or-nothing; no partial lease set is observable.
Independent subjects can run in parallel when their executors hold distinct
child-run claims. Work claims prevent duplicate execution of a local work
item; scoped leases separately authorize resource mutation. Neither may be
inferred from the other.

Renewal, release, handoff, expiry recovery, and force recovery are
compare-and-swap transitions. Ownership transfer or recovery increments a
monotonic fencing epoch; renewal may increment a record revision without
changing that epoch. Each transition emits an immutable event. Grants bind
`lease_id + fencing_epoch`; revision exists only for
compare-and-swap updates, while current holder and expiry are rechecked at
action authorization. Normal renewal does not invalidate an otherwise valid
grant merely because its CAS revision changed. A handoff is complete only when
the outgoing checkpoint and claim/lease transfer agree; the incoming executor
starts at `sync_required` and cannot inherit the previous session's
unacknowledged delivery position, source-feed progress vector, or turn grant.
A stale holder's old fence is
rejected even if its process resumes later.

Successful mediated actions implicitly heartbeat their leases. A host-reported
pause suspends expiry up to a policy-bounded `max_suspension`; pause remains a
host fact, not an Engram process state. After that bound the lease becomes
`recoverable`, not silently free. Takeover requires an attributed recovery and
increments the fence; a returning holder receives a blocking recovery event
and cannot mutate until it reconciles. The small wait-for graph is checked on
multi-resource acquisition; a cycle refuses the younger request with a
rescope/handoff directive rather than adding scheduling or fairness policy.

## Failure and recovery behavior

A single global fail-open/fail-closed switch is unsafe and unusable. The
built-in policy applies a capability-specific matrix; deployments may make it
stricter but cannot weaken non-overridable cells:

| Failure | Observe | Communicate | Reversible local mutation | Shared mutation | External effect / lifecycle |
| --- | --- | --- | --- | --- | --- |
| Decision service unreachable or deadline exceeded | Open | Closed | `degraded_open` only inside a cached envelope | Closed | Closed |
| Store corruption or unknown safety schema | Diagnostic-only | Closed | Closed | Closed | Closed |
| Portable writer epoch unknown/stale/expired | Open | Closed | Closed | Closed | Closed |
| Unsafe pinned packet or policy contradiction | Recovery-only | Recovery capture only | Closed | Closed | Closed |
| Lease conflict, expiry, or stale fence | Open | Capture allowed | Defer | Defer or deny | Closed |
| Unknown prior action outcome | Open | Unrelated capture only | Unrelated work only | Closed when related | Closed when related |
| User/host denial or missing authority | As host permits | Closed for denied capability | Closed for denied capability | Closed | Closed |

`degraded_open` is never silent fail-open. While Engram is healthy it may issue
a cached `DegradedEnvelope` bound to session, policy hash/epoch, capability and
resource bounds, lease ids/fences, expiry, maximum actions/bytes, and the host
mediation map. The host may use it only for policy-designated reversible local
work; without a valid envelope it fails closed. The host does not independently
reconstruct or widen Engram policy while the service is unavailable.
Envelope expiry may not exceed any basis grant or lease expiry, is capped by a
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
a scoped recovery turn may capture any durable semantic finding through the
ordinary typed-memory path.

On recovery the session returns to `sync_required`, uploads debt idempotently,
replays deltas, verifies current policy and leases, fingerprints touched
resources, and records `accepted | conflict | operator_required` reconciliation
for each entry. Shared/external/lifecycle actions remain unavailable until all
debt is terminal.

Recovery capabilities remain available where disclosure permits: inspect the
refusal, load context, read deltas, acknowledge delivery, resolve a
contradiction, reconcile an unknown action, renew or release a lease, wait,
contribute, and request an attributed human exception. A refusal distinguishes
`defer` (contention with retry/wake conditions) from `deny` (authority or
safety prohibition), so the host does not turn normal contention into an
unsatisfiable refusal loop.

A break-glass exception is an explicit, scoped, expiring, human-attributed
event naming the denied capability and reason. It cannot make an advisory host
action-gated or turn asserted identity into authenticated identity. Store
corruption, an unknown safety schema, a user/host denial, missing mediation,
and unverifiable external authority are non-overridable. Break-glass is
limited to policy-authorized coordination exceptions after those invariants
pass.

## Crash, restart, and replay

- Host or Engram restart invalidates unexpired in-memory delivery assumptions;
  the durable session resumes at `sync_required`.
- Turn, lease, delivery, action, and checkpoint requests use independent
  idempotency keys bound to canonical intent fingerprints and, for begin and
  checkpoint, the exact grant id.
- Exact decision retries while the result is retained return that result.
  A refused `lease_acquire` is therefore sticky under its key even after an
  assurance or policy-epoch change; a fresh key re-evaluates current policy,
  while changing intent under the old key is a conflict. Acquisition intent
  includes the current session-bind generation, so an old key conflicts after
  rebind instead of replaying obsolete authority. Within one bind generation,
  a host must not reuse a successful acquisition key after terminal release;
  it mints a fresh key for a new reservation.
  After grant/result pruning, the durable request-key tombstone returns
  `expired_request`; it never treats the old key as fresh. Reuse with a
  different intent is always a conflict until the task's explicit retention
  boundary. Canonical action/publication receipts keep their durable retry
  semantics.
- Expired unbegun grants write a terminal tombstone and never resurrect. Lease ownership transfer or recovery
  increments the fencing epoch and invalidates grants bound to the old fence;
  an ordinary renewal revision does not.
- Only an `in_flight` action created by a successful `action_begin` can become
  `outcome_unknown`. An issued but unbegun grant expires unused. Idempotent
  adapters reconcile unknown outcomes by their durable request key; other
  effects require an attributed operator decision.
- Ordered events allow a replacement host process to reconstruct session and
  task projections before issuing another grant.

## Completion and optional finalization under control

In the target controlled-completion path, `work_complete` enters
`completion_pending`. That transaction freezes the
executor checkpoint obligation, advances the work admission epoch, and denies
new ordinary execution mutation grants. Engram then drains in-flight actions,
releases or transfers every resource lease, and terminalizes the work claim
while allowing reconciliation and abort. Root completion additionally
freezes the `RootExecution` contributor roster and every required child seal
or explicit grant-backed disposed-child waiver. Only
after the drain succeeds does a
`completion_seal` transaction capture a dense run-feed cut and bind the work
revision, run/claim fences, executor checkpoint, action outcomes, acceptance
results, and evidence hashes. A root seal also binds required child seals or
disposed-child waivers, contributions, decisions, and attributed participant
waivers. It makes the work completed; an
attributed abort before the seal returns it to `open`. The shipped alpha only
accepts the zero-linked-state path: it requires empty action-outcome and
resource-lease drain sets and creates the seal atomically. It refuses nonempty
drains until this control integration ships.

Before any new seal is frozen, every obligation definition whose trigger is at
or before the completion cut must have a satisfied or host-authorized waived
resolution at or before that same cut. The final checkpoint must acknowledge
the typed verification evidence. Required child seals are decoded and checked
recursively; every accepted child seal carries the exact current obligation
and environment schema bindings.

An executor may seal its run only after its last turn is checkpointed, all
material outcomes are known, its host-confirmed source-feed progress reaches
the frozen cut, and its resource leases are released or explicitly
transferred. A root contributor may mark ready only after its claimed child
run has sealed or its omission is authorized. Contribution and readiness
events before the root seal do not move the cut.
Discovery of new execution work atomically aborts completion, invalidates the
barrier, advances the work admission epoch, and requires a later fresh cut.

Optional report finalization consumes the immutable completion seal; it does
not drain execution a second time. Engram creates a `ReportAssembly` anchored
to the root seal and issues a fenced `ReportAssemblyClaim` to the designated
finalizer. A narrowly scoped `finalizer` turn grant binds the seal hash,
assembly generation/revision, and live assembly-claim fence. It cannot reopen
ordinary workspace mutation, and it requires no completed-run work claim or
resource lease. Final report freeze requires that grant, the seal, and the
assembly claim; it terminalizes the claim.
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
3. call `turn_evaluate` and inject all blocking directives before each turn;
4. prevent ordinary prompts while the session is not `ready`, while allowing
   only a matching recovery or finalizer prompt under its scoped grant;
5. in action-gated mode, intercept every declared material capability and
   require and begin a matching single-use action grant;
6. report action outcomes even when the model turn later fails;
7. request and persist a turn checkpoint before starting the next turn;
8. checkpoint before context compaction and reconcile or exit before ending a
   session;
9. treat Engram notifications only as doorbells and fetch state by cursor;
10. resume after restart through `sync_required`, never from cached permission;
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

The exact transport can evolve, but the shared core operations are:

```text
control_bootstrap
session_bind
turn_evaluate
turn_begin
delivery_ack
action_authorize
action_begin
action_complete
turn_checkpoint
session_heartbeat
session_exit
```

Agent-facing MCP exposes exactly `next`, `ls`, `show`, `add`, `claim`,
`update`, `note`, `done`, `search`, and `handoff`. The host control
surface uses the stable spellings defined by `ControlRefusalCode`:
`control_unavailable`, `store_corrupt`, `unknown_control_schema`,
`control_policy_missing`, `control_assurance_insufficient`,
`capability_not_permitted`, `task_unbound`, `task_access_denied`,
`policy_epoch_changed`, `task_admission_epoch_changed`,
`pinned_contradiction`, `pinned_budget_exceeded`, `lease_required`,
`context_required`, `delta_required`, `delivery_invalid`,
`checkpoint_required`, `recovery_required`, `turn_already_open`,
`turn_purpose_mismatch`, `lifecycle_hold`, `participant_not_ready`,
`action_outcome_unknown`, `missing_authority`, `grant_expired`,
`grant_not_begun`, `grant_scope_mismatch`, `stale_fence`, `resource_remapped`, and
`session_exited`. The current persisted alpha cannot yet emit
`action_outcome_unknown` or `missing_authority`: action-outcome tracking and
organizational authority mediation are not wired, and effects requiring them
remain outside the supported policy envelope. Lease contention is a typed
`defer` decision rather than a refusal code.

## Preconditions in the current implementation

The Phase 0 shadow evaluator and integrity-checked observation log remain.
The current host-control alpha additionally ships a built-in safe policy,
durable control sessions, optional exact `WorkRun` claim bindings,
transactional context snapshots, persisted turn decisions and short-lived
grants, begin-time freshness/delivery rechecks, canonical execution
observations, and canonical checkpoint events. A separate `engram control` JSON-lines process
implements `session_bind`, `session_status`, `turn_evaluate`, `turn_begin`, and
`turn_checkpoint`, plus scoped `lease_acquire` and `lease_release`; none is
exposed through agent-facing MCP. Exact retry
evidence survives process restart, while unbegun authority is invalidated and
the session returns to `sync_required`. Each open rotates an internal
connection generation so a still-running predecessor is fenced. Begun grants
remain checkpoint-required and are discoverable through session status;
observe-only partial recovery grants include their exact safely redeliverable
payload.
`doctor` verifies canonical intent/result bytes plus their redundant row
bindings.

The alpha grants `observe`, `communicate`, and lease-backed, turn-gated
`mutate_local`. An exclusive execution lease covers a normalized path or
logical resource tree, becomes an immutable task-feed event, and contributes
its id, subject, expiry, and monotonic overlap fence to a turn grant. Begin
rechecks that exact live basis. Conflicting acquisition defers; release and
later acquisition advance the fence. Once a mutation grant is begun, every
lease in its basis remains pinned until checkpoint: explicit release refuses,
and an overlapping acquisition continues to defer after nominal expiry with
`checkpoint_required: true`. Restart preserves that pin. Checkpoint closes the
uncertain turn atomically, after which an expired or released scope can move to
the next holder under a higher fence. The store persists one host path policy
on the first open that resolved the project root's filesystem identity
(host-supplied or probed on the real filesystem) and refuses later resolved
openers with different path semantics; an opener that could not resolve the
identity refuses path leases rather than guessing. The
storage boundary binds path subjects to the session project, NFC-normalizes
them, applies that persisted case policy, and under Windows rules rejects
trailing-dot/space, alternate-data-stream, reserved-device, and 8.3-shaped
aliases. Host adapters remain responsible for resolving symlink/hard-link
identity before constructing a resource subject. Engram also rejects a task
rebind while old active leases remain. Every task event is treated as
begin-blocking until the impact classifier ships, which is conservative but
can over-deliver. `action_gated` declarations and shared/external/lifecycle
turn effects are rejected. The decision service becomes a real `turn_gated`
deployment only when an embedding host makes it mandatory and injects the
attached context before prompt dispatch; this repository does not yet ship
that runtime hook.
`engram doctor` verifies the immutable active-policy chain and reports its
hash, epoch, required assurance, built-in effect envelope, live turn counts,
and explicitly discloses that action gating, organizational authority
mediation, and action-outcome reconciliation are unavailable. Selecting an
`action_gated` requirement is therefore a deliberate fail-closed
configuration: no current host may bind at that level. The required
per-host-tool mediation map is still outstanding.

Broader enforcement must remain disabled until Phase 1 makes these invariants
true in the core, not only in wrappers:

- context contents, task/work-root contradictions, packet hash, stamped task
  head, persisted work focus, project/root/run work-feed heads, the
  project-visible context revision, and the owner-private context revision
  come from one consistent SQLite read transaction; begin rechecks that basis
  and rejects drift before execution without putting private object identities
  on a shared feed;
- every behaviorally relevant task/memory/lease/lifecycle transition advances
  the authoritative feed and its head projection, and delta requests cannot
  jump or acknowledge undelivered ranges;
- capture, claim, task transition, and publication entry points validate work
  existence/state, root-execution membership, focused-run claim, and
  applicable grant/lease/assembly-claim rules;
- the minimum scoped execution lease is extended with renew, explicit handoff,
  intent conversion, durable expiry/recovery, suspension, and host path-remap
  identity binding; and
- work/run states, contributions, completion seal, report freeze, durable
  publication intent, and dummy publication receipt are wired on a real
  restart-safe path.

Until those are process-tested, the existing MCP loop remains advisory. The
host-control alpha can authorize only a declared local-mutation *turn* under a
live lease; it cannot authorize an individual tool action, shared/external
effects, lifecycle transitions, or finalization.

## Delivery sequence

| Phase | Deliverable | Honest control claim |
| --- | --- | --- |
| 0 — observe and replay | Safe policy bootstrap, daemon/thin client, host mediation map, decision log, latency/false-refusal baseline; control decisions are shadow-only and never weaken existing user/host denials or shipped packet safety errors | Advisory observation only |
| 1 — repair prerequisites | Transactional context snapshots, consistent task cursor, real task transitions, scoped lease lifecycle, contribution barrier, durable dummy publication path | No new control claim |
| 2 — freshness mediation | Impact-classed events, durable tentative/checkpointed delivery, recovery grants, pre-turn inline packet/delta, compaction re-delivery, checkpoint; enforce only unknown-schema, unsafe-packet, and failed-required-injection refusals | `turn_gated` delivery plus the minimal non-overridable refusal set |
| 3 — scoped coordination | Normalized resource leases, intent/exclusive modes, suspension, handoff, recovery/fencing, out-of-band detection | `turn_gated` coordination |
| 4 — widen refusal and action gate | Enable the broader replay-proven closed turn-refusal set, degraded-envelope matrix, and action authorization/begin/outcome mediator | `action_gated` for the declared capability set |
| 5 — controlled completion/finalization | Stable completion cut, recovery/finalizer grants, contribution barrier, optional report freeze and receipted dummy publication | End-to-end controlled local loop |

The current implementation deliberately process-tests a narrow
`observe`/`communicate` lifecycle plus lease-backed local-mutation turns while
Phase 1 is still incomplete. That validates the host protocol, restart
semantics, overlap fencing, and the first coordination path early; it does not
skip action mediation, the complete lease lifecycle, report, or entry-point
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
