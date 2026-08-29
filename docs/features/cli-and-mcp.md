# CLI & MCP Surface

> Normative reference: [spec §8](../spec.md#8-interfaces). Related briefs:
> [context packets](context-packets.md),
> [local work system](local-work-system.md),
> [local tasks & reports](local-tasks-and-reports.md), and
> [behavioral control plane](behavioral-control-plane.md).

One core library owns classification, object storage, scope authorization,
task binding, deltas, and context-packet construction. The CLI and MCP server
are thin faces over it; transport code does not redefine memory policy.

The shipped agent-facing MCP interface is **advisory**. A model can omit an
MCP call, so that surface alone cannot enforce synchronization, ownership, or
finalization. A separate host-private JSON-lines channel now implements the
first turn lifecycle; grants are not exposed as tools with which the agent can
authorize itself.

## Shipped CLI

```bash
export ENGRAM_HOME=/absolute/host-local/path
engram init --required-assurance advisory \
  --authorized-by host-operator \
  --reason "bootstrap this project for an advisory host"
engram doctor

# Host/operator boundary: activate a new immutable policy version. The
# optional expected hash is the `id=` reported by doctor and prevents a stale
# operator from overwriting a concurrent policy update.
engram control-policy set-required-assurance turn_gated \
  --authorized-by host-operator \
  --reason "enable mandatory host turn mediation" \
  --expected-policy-hash <active-policy-hash>

engram mcp \
  --actor-id codex \
  --session-id session-unique-id \
  --source-skill engram-repo
engram control \
  --actor-id codex \
  --session-id session-unique-id \
  --source-skill engram-repo

# Host/operator boundary: mint a bounded, expiring grant for one exact actor.
GRANT=$(engram authority grant \
  --subject-actor-id codex \
  --issued-by host-operator | jq -r .grant)

# Read-only work operations need no grant. Mutations resolve this host-bound hash.
engram work --actor-id codex --session-id session-unique-id next
engram work --actor-id codex --session-id session-unique-id \
  --authority-grant "$GRANT" focus <short-ref>
engram mcp --actor-id codex --session-id session-unique-id \
  --work-authority-grant "$GRANT"
```

`--project-file` defaults to the tracked `.engram-project`. Its stable project
identity resolves to the same opaque SQLite path for every worktree and
session on the host. `doctor` verifies every canonical object plus
hash-bound control record, reports the active immutable policy hash, epoch,
required assurance, built-in effect envelope, and live
issued/begun turns, and visibly warns that action gating, organizational
authority mediation, and action-outcome reconciliation are unavailable. V1's
development no-op redactor provides no secret or PII protection.

On a fresh store, plain `engram init` defaults to `turn_gated`;
`--required-assurance` may instead select `advisory`, `turn_gated`, or
`action_gated` for that first policy and requires `--authorized-by` plus
`--reason`; the resulting epoch-one authority object records that operator
choice as asserted context. Plain `engram init` remains an
idempotent create-or-migrate operation and preserves any existing active
policy. Explicitly passing a different bootstrap value for an existing store
fails instead of silently changing policy.
`engram control-policy set-required-assurance` is the only shipped
reconfiguration path: it records asserted operator attribution and a
reason, creates immutable authority and policy objects, atomically advances
the active policy hash and epoch, and supports an optional compare-and-swap
hash. Reapplying the active level is idempotent. Issued grants from the prior
epoch fail begin with `policy_epoch_changed` and require one fresh evaluation;
if the new requirement exceeds the host's declaration, that fresh evaluation
instead fails `control_assurance_insufficient` because assurance is checked
before epoch adoption. Already-begun grants remain checkpointable under their
frozen basis. Selecting `action_gated` through either initialization or the
setter prints a warning that no V1 host can bind at that level plus the
`set-required-assurance turn_gated` recovery command. The operator identity is
asserted host context, not authenticated administration.

`engram work` exposes `next`, `focus`, `propose`, `update`, `complete`, and
`handoff`. Mutation payloads accept an inline JSON object or `@path` to a JSON
file. All six call the same service core as MCP. The current operator CLI also
installs and irreversibly revokes canonical work-authority grants; those
commands are not MCP tools. Search/stats/import/export and the remaining broad
administrative CLI in the specification are still planned.

The same host boundary can waive one exact open run obligation, but only with
a separately admitted grant:

```bash
WAIVER_GRANT=$(engram authority grant \
  --subject-actor-id host-operator \
  --issued-by project-owner \
  --allow-obligation-waiver | jq -r .grant)

engram authority waive-obligation \
  --obligation-id <uuid> \
  --expected-definition <hash> \
  --authority-grant "$WAIVER_GRANT" \
  --waived-by host-operator \
  --reason "accepted without the required test" \
  --idempotency-key <retry-key>
```

The equivalent native-host operation is `obligation_waive` on the private
JSON-lines channel. Its request names the routing token, exact obligation and
definition hashes, dedicated grant, asserted `waived_by` human, bounded reason,
and idempotency key. The control session must be bound to that obligation's
live run. Policy outcomes are typed as `waiver_not_admitted`,
`obligation_not_open`, or `definition_changed`; transport, token, and
same-key/different-intent faults remain request errors. A committed retry
replays the exact result. The canonical resolution keeps the server-fixed
session actor beside the asserted human attribution, while the receipt omits
the authority hash and reason.

Both waiver surfaces are host/operator private. MCP and `work_update` cannot
request a waiver, and agent-facing projections omit its authority grant and
reason.

## Shipped MCP tools

| Tool | Purpose |
| --- | --- |
| `task_start` | Start or ref-idempotently bind a local task |
| `task_join` | Join the same task using only its external reference |
| `memory_note` | Capture prose once with inferred classification and a required retry key; `target: task|work` is required when both contexts are active |
| `memory_contradict` | Link two visible task/work shared version hashes as explicitly incompatible |
| `memory_context` | Build a bounded packet for the session's active task and persisted work focus |
| `memory_delta` | Return only task changes after a processed cursor |
| `memory_search` | Full-text search visible memory |
| `memory_show` | Verify and inspect an authorized memory by the receipt's version hash |
| `context_explain` | Reproduce the inclusion and omission reasons for a packet |
| `task_claim` | Acquire an expiring, safely retryable task lease with typed conflict details |
| `work_next` | Return selected compact focus/catalog/ready/change sections under a 12 KiB wire ceiling; an exact staged summary page replays until acknowledged |
| `work_focus` | Select by short ref/UUID and inspect bounded acceptance, graph, claim, blocker ids, evidence, handoff, memory index, history count/tail, exact waivable-child candidates, and allowed-next state without claiming |
| `work_propose` | Create a root or atomically add bounded children/prerequisites to focused work; decomposition receipts return the complete compact child identity set under the 12 KiB ceiling |
| `work_update` | Apply claim/release, checkpoint, evidence, blocker, revision/assignment/deferral, dependency, or reopen updates under inferred fences; unblock may omit its id when exactly one blocker is active |
| `work_complete` | Seal focused work under inferred fences, optionally recording evidence and its final checkpoint in the same high-level call |
| `work_handoff` | Offer, accept, or cancel the unique checkpoint-coupled handoff without passing offer/claim ids |

Every result is structured JSON. Expected execution failures are MCP tool
errors with stable error codes and details—for example, a live claim conflict
returns `task_claim_held` with holder plus epoch and ISO-8601 expiry. Repeating
a claim with the same key and TTL returns the original lease even though
wall-clock time advanced; changing the TTL under that key is an idempotency
conflict. A private memory hash does not become a capability: `memory_show`
repeats project/task/participant/owner authorization, verifies the persisted
work focus, and returns `memory_access_denied` to a peer.

The shipped `task_claim` is a whole-task advisory lease and predates the local
work design. The target does not silently reinterpret it: a fenced work claim
schedules one work item, while a separately versioned resource-lease operation
authorizes mutation over canonical subjects. The legacy tool is deprecated
after both replacement paths ship.

## Current advisory agent loop

1. The first session calls `task_start` with the external tracker reference;
   peers call `task_join` with that reference alone. Engram persists each
   session's active binding, including across MCP process restart.
2. Call `memory_context` once and retain its `event_cursor`. Inject the pinned
   and index tiers into the working context. This remains a convention in the
   direct advisory slice. The shipped host-private control protocol instead
   persists exact deliveries and requires their acknowledgement before issuing
   a turn grant.
3. Call `memory_note` when a decision, constraint, finding, evidence pointer,
   or handoff fact becomes worth sharing. Supply a caller-stable
   `idempotency_key`; retry the exact call after a lost response, and generate a
   new key for different prose. Natural-language prefixes such as
   `Decided:` help classification but flags are not required. Natural rules
   beginning with `Never`, `Always`, `Must`, `Do not`, or `Only` become firm
   pinned constraints without flags. The receipt
   returns the inferred fields, their basis, canonical hashes, cursor, and
   idempotency key.
4. Before the next work turn or after a host wake, call `memory_delta` with the
   retained cursor. Advance it only after processing the returned changes.
5. Use `private: true` only for incomplete scratch. It remains searchable and
   inspectable by the owning agent but never enters peer packets or deltas.
6. When two shared records cannot both guide action, call
   `memory_contradict` with their version hashes and a reason. Both become
   visibly contested. If both are applicable firm/hard pinned records,
   `memory_context` fails with `pinned_contradiction` and names the edge and
   versions instead of asking the agent to invent precedence.

One capture powers peer context and the ordered feed. The host may use a
mailbox as a doorbell, but must not relay full state or make the agent repeat
the same fact into another status ledger.

## Shipped agent-native work protocol

The normal model surface is six ambient operations: `work_next`, `work_focus`,
`work_propose`, `work_update`, `work_complete`, and `work_handoff`. Session
binding supplies project, actor, current work, and cursors, so update/complete/
handoff do not repeatedly shuttle ids. Ambient state contains no authority;
the host fixes a canonical grant hash for the MCP process and each mutation
rechecks that grant and revocation state. `work_next` returns only the selected
`focus`, `ready`, `catalog`, and/or `changes` sections; omitting `sections`
selects all four. CLI callers use `--sections focus,ready,catalog,changes` and
MCP callers pass a string array. Selecting no `changes` section never stages or
advances project delivery, including when a prior page remains pending.
Hash-verified source changes retain dense positions and explicit compact
summaries instead of canonical work snapshots or memory bodies. A change's
`object_hash` identifies the verified canonical source and intentionally is not
a hash of its summary. Restricted
work memory, and work memory outside the session's currently focused verified
root, is replaced by a typed `omission` marker at its original dense position;
the protected body and structured fields never cross the agent boundary. The
largest dense prefix fitting the fixed change budget is staged durably and
replayed until
the caller returns its exact `delivered_through` value and opaque
`delivery_token` as `acknowledge_through` and `acknowledge_token`; agent-facing
change projections are stored canonically with that pair and replay byte-for-
byte even if focus or legacy-task binding changes. Concurrent appends wait for
the next page. Both acknowledgement fields
are absent when changes were not delivered, so an undisclosed or guessed cursor
cannot be acknowledged. Every
successful work response is at most 12,288 serialized JSON bytes; typed
`omissions` report advisory sections shortened by count or byte budget. A repeated
acknowledge-and-fetch request safely replays any newer staged page after a lost
response. `work_focus` is navigation only and never claims/releases as a side
effect. It returns an exact history count with a bounded newest-event summary
tail, the latest run even after completion, and a body-free actor-filtered
memory index whose version hash is the key for authorized `memory_show`.
Before changing ambient focus, callers first replay any pending page by calling
`work_next` with `changes` selected and no acknowledgement. They then pass the
`delivered_through` and `delivery_token` actually received as
`acknowledge_through` and `acknowledge_token` while selecting sections that
exclude `changes`. The typed `work_delivery_pending` response explains this
sequence without exposing the host-only tentative cursor or token.
`work_propose`
atomically handles roots and bounded decomposition. `work_update` carries a
typed transition such as claim/release, checkpoint, blocker, cancel,
supersede, deferral, assignment, revision, or prerequisite change. Update and
handoff success responses contain only a compact receipt, one bounded
`obligation_page`, generic readiness `obligations`, and `allowed_next`;
their size does not grow with item history. Each entry
names the exact tool and tagged operation. For example,
`allowed_next: ["work_update:claim(recovery_reason_required)"]` directs the
agent to submit the `work_update` claim variant with an attributed
`recovery_reason` and a host grant that includes `claim_recovery`; ordinary
claiming is `work_update:claim`. A successful claim receipt includes
`control_binding { root_execution_id, work_id, run_id, work_revision,
claim_id, claim_fence }`, ready to pass unchanged to host-private
`session_bind`. The same live tuple appears as `focus.control_binding`;
`focus.run` also exposes `root_execution_id` and `work_id`, while
`focus.claim` exposes the claim and fence components. `work_revision` is the
focused work item's revision—the claim receipt's top-level `revision`—not the
claim projection's own revision counter.

`work_complete` can consume evidence/checkpoint state created through explicit
`work_update` calls, or accept `capture { summary, refs }` to record evidence,
checkpoint its exact evidence set, and seal in one model-level call. Completion
is refused while blockers, prerequisites, required child seals or explicit
completion waivers, live handoffs, capture requirements, or run obligations
remain unresolved. Open obligations are not an MCP error envelope.
`work_complete` returns a typed `open_work_obligations` result with the same
`obligation_page` used by `work_focus`, nested `work_next.focus`, and
`work_update`. A completed receipt also returns that page reconstructed from
the exact terminal obligation hashes bound into the seal. Each page is count-
and byte-bounded, reports an explicit `omitted_count`, and carries immutable
obligation/definition identities, state, rule, requirement, trigger, terminal
resolution/evidence when present, and deterministic typed guidance. An open
verification requirement directs the caller to record matching host
verification, checkpoint it, then complete, or request a host/operator waiver.
Generic readiness strings remain a separate compatibility field. The typed
completion result is durably replayable under the same idempotency key.
An interrupted capture-backed completion recovers committed evidence or
checkpoint timestamps for exact replay, while missing substeps use current
time and recheck the live claim.

The MCP schemas for `work_propose`, `work_update`, `work_complete`, and
`work_handoff` expose their typed discriminated inputs rather than opaque JSON.
Every mutating branch requires a caller-stable `idempotency_key`. Durable
attempts bind caller intent separately from the current focused work/claim/
handoff basis, so committed retries replay while interrupted attempts
revalidate authority and cannot mutate a newly focused item.

Work search/lifecycle filters, paged catalog results, and item history ship in
the ambient query/focus views. Stats, stale/orphan diagnostics, approval
decisions, import/export, and report publication remain administrative tools
over the same core. Successful model responses are terse; refusals return a
stable code plus a satisfiable remedy. Full durable receipts go to the host. No
replayable turn/action grant token appears in model-visible MCP output. The
work-authority hash is a host process argument, not a request field or tool
result.

## Shipped host-private turn channel

`engram control` is a long-lived stdio process. It accepts one JSON object per
line and returns one `{ "status": "ok", "result": ... }` or typed error line.
The runtime session and asserted actor are fixed by process arguments. The
shipped operations are:

| Operation | Durable effect |
| --- | --- |
| `session_bind` | Start/join the compatibility task, optionally bind an exact live `WorkRun` claim, rotate a routing token, reset to `sync_required` |
| `session_status` | Read current phase, cursors, epochs, mediation declaration, optional work binding, revision, and any safely redeliverable partial recovery grant |
| `lease_acquire` | Atomically grant or defer a normalized resource lease and append its fenced task event |
| `lease_release` | Release a lease held by this session and append its fenced task event |
| `turn_evaluate` | Derive membership/context/head/policy from SQLite and persist a decision plus optional grant |
| `turn_begin` | Recheck freshness and exact delivery token, then consume the issued grant |
| `turn_checkpoint` | Promote tentative delivery, atomically append bound execution observations, complete the grant, and append a canonical control checkpoint event |
| `obligation_waive` | Resolve one exact open obligation on the session's bound run under dedicated human-attributed waiver authority |

The bind response supplies the `routing_token` used on later calls. A granted
turn carries an exact dense task delta under `grant.delivery.delta`. The final
page also carries the bounded context packet under `grant.delivery.context`;
earlier bounded pages set `context` to `null`, `has_more` to `true`, and grant
only an observe-only `recovery` turn. The host must inject the supplied payload
and cite `grant.delivery.page.delivery_token` in `turn_begin` before
dispatching the prompt. Checkpointing a partial page leaves the session
`sync_required`; finite recovery pages drain the backlog before an ordinary
turn can grant. A single canonical task event is size-limited at capture, so a
page always makes progress. Exact retry evidence remains canonical across
process restart, but a newly opened control connection invalidates unbegun
grants and returns the session to `sync_required`; old results never resurrect
authority. The new connection also fences a still-live predecessor, whose next
operation fails with `control_connection_superseded`. A begun grant is not
silently replayed or discarded: `session_status.open_grant_id` identifies the
required checkpoint. When the begun grant contains an observe-only partial
recovery page, `session_status.recoverable_grant` returns the exact canonical
grant and delivery bytes; the replacement host redelivers that payload and
then checkpoints the already-begun grant. The confirmed cursor does not move
until that checkpoint. Other begun turns expose no replayable prompt because
their outcome may be uncertain. Reusing a key for a different intent fails.

A native local-work bind supplies `work_binding` with
`root_execution_id`, `work_id`, `run_id`, `work_revision`, `claim_id`, and
`claim_fence`. Storage verifies that exact tuple against the session's live
claim and copies it into every turn-grant basis. Ownership means
`claim.holder == session_id` for the MCP actor session that claimed the run;
the asserted `actor_id` is audit context and never substitutes for that holder
check. A malformed, cross-project, or currently peer-owned bind fails as
`work_claim_mismatch`. A tuple that canonical history proves belonged to this
session but whose revision, fence, claim, handoff, run, root execution, or
expiry moved before bind fails as `stale_fence`, telling the adapter to reread
and rebind. The same movement after bind refuses evaluation or begin with
`stale_fence`. Omitting `work_binding` retains the compatibility task-only
channel, but that session cannot append run execution observations.

`turn_checkpoint.observations` accepts at most 64 host facts containing
`observation_id`, `action_fingerprint`, `effect`, `outcome`, and
`source_changed`. An observation may also carry
`source_basis { workspace_id, source_revision }` and `observed_at`.
`source_revision` is the host's fingerprint of the full relevant content,
including committed and dirty bytes. `workspace_id` is retained for audit but
does not participate in anti-stale equality. Engram supplies the authoritative
project, frozen work binding, session, grant, actor, and recording timestamp,
then appends each canonical observation to the project, root-work, and
run-execution feeds in the same transaction as the control checkpoint.

`turn_checkpoint.verification_evidence` accepts at most 16 host-minted checks.
Each entry supplies `producer_observation` as either
`{ kind: "object_hash", object_hash }` or
`{ kind: "observation_id", observation_id }`, plus `check_kind`, optional
`summary`, and bounded `refs`. Storage derives the check fingerprint, outcome,
source/run/session binding, and timestamps from the producer; an unknown
producer returns `verification_producer_not_found`. Up to four
`environment_evidence` entries bind an environment identity to an exact source
basis. The legacy form may retain only an opaque host-produced fingerprint.
The component form adds
`components { toolchain, sandbox?, workspace_id, capability_map_revision }`;
Engram derives the canonical component fingerprint, requires the component
workspace to equal the source workspace, and requires the capability-map
revision to equal the bound session. Component strings are trimmed,
nonempty, limited to 256 bytes, inspected by the configured redactor, and are
asserted host context rather than attestation. Do not place secrets in them.

A verification may cite an environment as
`{ kind: "object_hash", object_hash }` or
`{ kind: "index", index }`, where the index addresses the same request's
ordered environment list. The referenced object must belong to the same run
and source revision. The current built-in test requirement does not require a
particular environment, but its optional link is retained for audit and future
typed policies. Mismatched derived bytes, a missing reference, or a run/source/
session basis mismatch return `environment_fingerprint_mismatch`,
`environment_evidence_not_found`, or `environment_basis_mismatch`.

The receipt returns all three typed hash lists. An exact retry returns those
hashes without another feed append; changing any ordered list under the same
checkpoint key fails with `control_operation_idempotency_conflict`.

Agent-facing `work_update:evidence` retains its legacy generic form. It also
accepts the attach-only form
`{ kind: "evidence", attach: { evidence: <typed-hash> }, idempotency_key }`.
Attach validates that the hash is verification/environment evidence on the
focused run and does not mint another canonical object or feed entry. Generic
evidence can be cited for context and completion, but cannot satisfy a typed
verification requirement.

Every work-bound observation with `source_changed: true` atomically opens one
built-in test obligation, irrespective of `outcome` and irrespective of
whether `source_basis` is present. A passed typed test satisfies open
obligations only against the newest mutation source revision at the evaluated
run-feed cut. Thus a newest basisless mutation makes the open set waiver-only
until a later basis-bearing mutation plus passed test arrives; that later test
may satisfy both the earlier and newer definitions. `work_focus` exposes the
canonical bounded `obligation_page`, the same field appears inside
`work_next.focus`, and `work_next` deltas use
`obligation_opened`, `obligation_satisfied`, or `obligation_waived` without
leaking host authority.

Every new completion seal declares obligation schema V1 and freezes the exact
definition/resolution hash pairs applicable at its dense pre-seal cut. The
final checkpoint must acknowledge the matching typed verification evidence.
A new seal also declares environment schema V1 and cites the sorted, distinct
environment-evidence hashes at or before that cut. It refuses more than 64
environment records and never copies the component payload into the seal.
A new parent verifies required child seals transitively and refuses a legacy
child seal whose run already had obligation definitions; existing legacy
terminal seals remain readable and are surfaced as legacy by diagnostics.
Pre-environment seals likewise decode with no environment bindings and are
reported as legacy rather than rewritten.

An observation effect absent from the frozen grant is a request error with
code `observation_scope_mismatch`. `grant_scope_mismatch` instead identifies a
checkpoint request whose grant was issued but never begun.
Checkpointing an already-begun grant compares its frozen session/grant binding
but deliberately does not recheck claim expiry or live ownership: begin already
consumed authority, and checkpoint records what happened rather than granting a
new action.
This control operation is deliberately named `turn_checkpoint`; the local-work
lifecycle operation `checkpoint_work` remains the separate run-progress and
evidence checkpoint.

The built-in alpha policy grants `observe`, `communicate`, Engram-internal
`coordinate` leases, and lease-backed `mutate_local` to a session whose
declared assurance first meets the active project requirement, whose
effect-specific assurance floor is then met, and whose mediated effects cover
the request. `coordinate` is accepted only at the lease boundary and is not a
model-turn capability. The bind receipt exposes
`effective_mediated_effects`: the declared set capped by the host's assurance.
`observe` and `communicate` may remain effective for an advisory host;
`coordinate` and `mutate_local` require at least `turn_gated` even when the
project floor is advisory. Before reserving anything, `lease_acquire` applies
that same policy ladder: the project floor, then the effect floor, the declared
and assurance-capped mediation sets, the active policy's supported effects,
and the session policy epoch. A project-floor refusal carries no effect; an
effect-floor refusal names the first failing effect and its intrinsic floor. A
stale epoch returns `policy_epoch_changed` and is adopted atomically; retrying
that exact key still returns the stored refusal, while a fresh key re-evaluates
and may proceed. Acquisition fingerprints have their own schema version and
include the current bind generation, so a pre-upgrade retry or reuse after
`session_bind` is an explicit idempotency conflict. Within one bind, do not
reuse a successful acquisition key after release; mint a new key for a new
reservation. A local-mutation intent must name one or more
normalized resources, all covered by a live exclusive `execution` lease held
by that session. The grant freezes the lease fence and `turn_begin` rechecks
it. Shared mutation, external, and lifecycle requests fail closed, and an
`action_gated` bind is rejected because per-tool action authorization is not
shipped. A decision-service process does not by itself prove control: the
embedding runtime may claim `turn_gated` only when it withholds every prompt
until this sequence succeeds.

For a repository path, the lease subject uses the stable project id and path
segments, for example:

```json
{
  "operation": "lease_acquire",
  "routing_token": "from-session_bind",
  "kind": "execution",
  "mode": "exclusive",
  "subject": {
    "kind": "path",
    "project_id": "value-from-.engram-project",
    "segments": ["src"],
    "coverage": "tree"
  },
  "ttl_seconds": 60,
  "idempotency_key": "claim-src-for-turn-42"
}
```

The matching `turn_evaluate` supplies exact or tree `resource_intents` beneath
that subject. The core rejects a different embedded project id and
NFC-normalizes every segment. The first store opener atomically persists the
host filesystem identity policy: Windows and macOS defaults case-fold, Linux
defaults case-sensitive, and Windows rejects reserved names, alternate data
stream syntax, trailing-dot/space aliases, and known 8.3 aliases. Later openers
must present the same policy. Lease expiry immediately removes authority;
releasing and later reacquiring an overlapping subject advances the
project-wide resource fence even when the new holder belongs to another task.
A session must release active leases before rebinding to another task; expired
rows are audited and terminalized automatically, and an `Exit` checkpoint
releases live rows. The
alpha conservatively treats all overlapping leases from different holders as
conflicts even when `mode: shared`; mutation grants still require
`mode: exclusive`. Renewal, intent-to-exclusive
conversion, explicit handoff, suspension, and audited recovery remain planned.

Action authorization/begin/completion, standalone delivery acknowledgement,
lease renewal/handoff/recovery, heartbeat, and independent exit remain
planned protocol operations.

`memory_note` never makes an ambient scope switch silently. If exactly one of
the legacy task binding or local work focus exists, that context is the
default. If both exist, the caller must select `target: task` or
`target: work`; omission is a typed `invalid_argument` refusal. Task notes
reach peers through `memory_delta`. Work notes retain the focused item as
their provenance subject, enter project/root/run feeds, and reach work peers
through `work_next` and focused context. Capture, search, and show re-read the
persisted focus at the storage boundary, so a stale or caller-supplied work id
cannot redirect access.

Hooks can integrate the shipped turn boundary. Full action gating needs a wrapper,
gateway, or native host integration around every declared material tool. If a
shell or network path remains unmediated, the session must not claim
`action_gated` assurance. See the
[control-plane host contract](behavioral-control-plane.md#host-integration-contract).

## Host configuration

Build an executable and configure one stdio MCP process per agent session:

```json
{
  "mcpServers": {
    "engram": {
      "command": "/absolute/path/to/engram",
      "args": [
        "--project-file",
        "/absolute/project/.engram-project",
        "--home",
        "/absolute/host-local/engram-data",
        "mcp",
        "--actor-id",
        "codex",
        "--session-id",
        "replace-with-this-runtime-session-id",
        "--source-skill",
        "engram-repo",
        "--work-authority-grant",
        "replace-with-host-installed-grant-hash"
      ]
    }
  }
}
```

The proprietary runtime supplies actor/session/tool/skill instruction context.
V1 records it with `asserted` assurance; configuration text is not
authentication. Distinct concurrent sessions need distinct `--session-id`
values. The database is shared; the MCP processes are not.

## Dogfood contract

`scripts/mcp-dogfood.test.mjs` launches two real stdio MCP processes against a
fresh home and checks ref-only rendezvous, flag-free classification, context,
single-item delta, private-scope non-disclosure (including a raw-hash probe),
restart durability, provenance/explain, idempotent retry/conflict, and lease
contention/expiry/retry. It also verifies zero-flag natural constraint
classification and explicit pinned-contradiction fail-closed behavior. It now
also installs operator grants, drives a real two-session work root through
claim, evidence, checkpoint-coupled handoff, recipient checkpoint, and sealed
completion without shuttling lifecycle ids, and drives the same lifecycle
through the CLI translation. It is part of `scripts/check.sh`.

`scripts/control-dogfood.test.mjs` launches the real bounded JSON-lines service,
bootstraps an advisory policy, activates a turn-gated successor through the
operator CLI, verifies both versions through `doctor`, binds, evaluates,
restarts before begin, proves the old grant cannot begin,
resynchronizes, checkpoints, checks mutation denial, and probes a wrong
routing token. Host action control,
report finalization/publication, scoped lease renewal/handoff/release, review
actions, history, explicit contradiction resolution, and the remaining
administrative CLI remain planned surfaces. They must reuse this core rather
than fork its semantics.
