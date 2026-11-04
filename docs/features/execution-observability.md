# Execution Observability

> Normative references: [spec §2.7](../spec.md#27-execution-observability) and
> [spec §8](../spec.md#8-interfaces).
> Related briefs: [local work system](local-work-system.md),
> [acceptance evaluation](acceptance-evaluation.md),
> [CLI & MCP](cli-and-mcp.md), [security & trust](security-and-trust.md),
> [execution pipeline](execution-pipeline.md) and
> [verification applicability](verification-applicability.md).

Execution Observability is the host feature that records what happens while
agents work: which work session holds which claim, what the host saw run, how
the source changed, and which registered checks passed or failed. The host
records these facts; Engram stores them, links them to the work item and run
they belong to, and uses them for source-change obligations and completion.
Nothing here admits, delays or refuses a model turn, and nothing here grants
filesystem, tool or external-action authority.

The terms used throughout:

- **work session**: the host's ordinary connection for one asserted actor and
  session, started with `engram control --work-session`.
- **assignment** and **claim**: an assignment is future intent on a work item;
  a claim schedules live execution of one run under a fence. Neither grants
  file or external authority.
- **host observations**: the execution, source and test facts the host
  records, through `execution_observe`.
- **registered verification**: a host-run check, registered before it ran
  and bound to an item, recorded through `verification_record` and credited
  to further items through `verification_bind`.
- **remote verification**: a test run on another host, delivered as its
  launcher's completion facts and a stated destination, through
  `remote_verification_record`.
- **acceptance evaluation**: the evidence-based judgement of an item's
  criteria before completion. It is **temporarily off** until Execution
  Observability is implemented; see
  [acceptance evaluation](acceptance-evaluation.md).

## Project ON/OFF

Execution Observability is switched on or off per project in the host. In
TermAl it is the project setting labelled "Execution Observability" (JSON key
`executionObservability`), on by default. The host records automatically
when Engram is enabled for the project and the setting is on.

Off suppresses only the automatic observations: the turn, command, source and
check envelopes the host would send through `execution_observe`. It never
affects the host's admission, queue, Stop or reset, claims, root naming, the
work-session services, or explicit registered verification. Ordinary work is
unaffected either way: work words, claims, notes, gates and completion behave
exactly as they do with it on. Engram has no setting of its own for the
feature: it stores what the host sends and never asks for it.

## The work session

`engram control --work-session --actor-id ACTOR --session-id SESSION` serves
the host's work session over a private JSON-lines channel. The project,
asserted actor and session are fixed by the process arguments. Startup
rotates the connection fence, so a still-running predecessor for the same
session is superseded.

`work_session_read` takes no fields beyond `operation` and returns
`project_id`, `session_id`, `work_session: true` and capabilities
`host_work_v1`, `verification_record_v1` and
`remote_verification_record_v1`. It writes nothing. It also
advertises `verification_evidence_git_tree_applicability_v1: true` only while
the serving connection observes the exact current
[git-tree applicability barrier](verification-applicability.md#schema-barrier-and-cutover).
A missing or malformed advertisement grants no restricted verification
credit.

The channel accepts `control_connection_read`, `named_root_bind`,
`named_root_read`, `named_root_sighting_read`, `acceptance_binding_read`,
`acceptance_verification_read`, `execution_observe`, `verification_record`,
`verification_bind` and `remote_verification_record`. Connection fencing,
project/work/run/claim association, source validation, exact operation
retries and evidence eligibility apply to every request. Naming a fresh root
requires the live claim holder and fence; ending a root and recording an
observation follow their historical rules below. None of these operations is
exposed through agent-facing MCP.

Operation receipts use the work-operation result store, scoped by project,
session and caller key, retaining the frozen intent and the unchanged
external receipt. An exact retry returns the original receipt; the same key
with a changed intent returns `control_operation_idempotency_conflict`.
Doctor and migration import validate the same canonical receipt associations.

### Reading which store a connection serves

`control_connection_read` takes no fields: `{"operation":"control_connection_read"}`.
The serving process answers it itself, for its own current connection, from
the moment its store is open and its connection resumed.

The read is a fail-closed contract. It would return the main database file
SQLite has open, as `database_path`, with the `project_id` and `session_id`
the process was started with, only where that file can be proved to be the
one at the reported path. No platform qualifies today. A path observed just
before and just after SQLite opens it can be redirected during the open,
through a renamed file, a symbolic link or a directory junction, and no safe
interface exposes the file SQLite holds. So on every platform the read
refuses with `control_connection_identity_unavailable`, and never returns a
path that might name another file. Should a sound proof exist, the answer's
`database_path` will be canonicalized and spelled exactly as
`engram readiness --json` and `engram doctor` spell a database path
(symbolic links and junctions resolved; on Windows without the `\\?\` prefix,
`\\?\UNC\server\share` becoming `\\server\share`), never taken from the
process's arguments, home or configuration.

The connection check comes first: a connection a newer one for the same
session has superseded refuses with `control_connection_superseded`. Other
storage and deadline errors keep their codes. A host treats every refusal and
a malformed reply as unproven identity, never as a match, so its recovery
fails closed. The read locks nothing and writes nothing to the store; like
every operation, it may write the usual host-private diagnostics for a
refusal or a slow reply.

## Named source roots

### Binding and ending a root

The host-private `named_root_bind` operation records a claim's source-root
selection or explicit end. It is not an agent word. Its request carries
`claim_id`, `claim_fence`, `workspace_id`, positive `generation`, `named_at`,
`kind` (`bound` or `ended`), `idempotency_key`, and `end_reason` only for
`ended` (`explicit_clear`, `session_gone_at_restore`, or `root_invalid`). The
host derives a stable retry key from `(claim_id, generation, kind)`.
`workspace_id` is exactly the canonical identity returned by the host's
source capture, including a Windows verbatim prefix when present; Engram does
not normalize it. The response names the immutable event's record id, dense
run-feed position, workspace, generation and kind. An exact same-intent retry
returns that receipt; reusing its key for different content returns
`control_operation_idempotency_conflict`.

Engram takes the reporting session from the work-session connection. It
derives work, run and root execution from the claim in the store, whatever
the session is focused on. A `bound` event requires the named claim to be
current and active. An `ended` event must repeat the recorded bound
generation's workspace and `named_at`, the time of the original naming.
Explicit clear uses the naming session's connection; restore or invalid-root
ends may use any work session of this project and record that reporter.
`claim_fence` is retained for disclosure, not compared with a renewed claim
fence. New claim ids never inherit a root. Peers receive each `bound` event
in `next` as a `source_root_named` delta and each `ended` event as a
`source_root_ended` delta, naming the workspace and generation.

A binding lasts as long as its claim. It survives a change of holder on the
same claim: a renewal, a recovery by the same or another holder, and an
accepted handoff. It ends with an `ended` event, or when the claim ends: its
release (`update --release`), or cancellation, supersession, detach or
completion of the run. Engram reuses a claim id when a released run is
claimed again, so a binding recorded before the claim's latest release is
never active again; the claim is unbound until a later name mints a fresh
generation. A host that missed the release and goes on stating the old
generation as the claim's root is refused: its root basis or closing sighting
no longer matches the claim's named-root state at the capture cut, so the
observation returns `execution_observation_basis_mismatch` and records
nothing. No observation is admitted as an unbound sighting of that
generation. The host reads the claim's `named_root` state and names the root
again. The event's run-feed position
orders durable events; `named_at` is audit time, not an ordering. A
duplicate, stale generation, invalid lifecycle transition, wrong session or
malformed field returns `named_root_binding_refused` with the reason.
Generation history is never reset by a release: a re-claimed claim names only
a generation larger than any it recorded before. A new `workspace_id` holding
a control or bidirectional formatting character, as
[source text](#source-text) defines it, is also refused as
`named_root_binding_refused`, the reason naming `workspace_id`, since the
host treats that code as definitive (an `ended` request repeats the stored
workspace and is not checked). Connection refusals keep
`control_connection_superseded`. The host is the identity and policy
enforcement point; the connection alone is not cryptographic authentication.

When a reporter's outcome is unknown and it cannot replay, the current holder
settles it from the claim's `named_root` state: the read establishes the
effect, no receipt is inferred from it, and a fresh name takes a generation
above the claim's latest event. The reporting session is part of a
`named_root_bind` intent and of its replay key, and connection checks run
before replay, so only the reporting session, on a current connection, can
replay; another session's call is a new event. An `ended` with
`explicit_clear` stays the naming session's operation and a handoff does not
transfer it; `session_gone_at_restore` and `root_invalid` are sent only when
true: Engram cannot inspect the host, so it validates only the end's
structure and, for `explicit_clear`, its reporter.

### A claim's root state

A claim's `named_root` state is derived from its recorded `named_root_bind`
events and the claim's own lifecycle, never from path text or a fence
change. It is the authoritative read a host uses to decide when to name a
root again. It has three states:

- `state: none`: no root is bound. None was named on the claim, its latest
  root was ended, or the run completed or was cancelled, superseded or
  detached after the name.
- `state: bound`, with `workspace_id`, `generation` and `named_at`: the
  claim's latest name, still bound.
- `state: unbound_by_release`, with `last_generation` and
  `released_at_position`: the claim was released after its latest name, at
  that run-feed position. A re-claim of the same claim id keeps this state
  until the host names a fresh generation.

When the claim's newest named-root event names a root and the run has not
finished since, of that name and the claim's newest release the later one
decides. An ended root reads `none`, even when a release follows it. A
handoff or a recovery records no release, so the state stays `bound`; the
claim's `revision` and fence advance on those too, so neither tells a host
when to name again. The variant set is closed, and each state carries
exactly its own fields: wherever Engram decodes a state, in a request, a
stored receipt or a stored observation, a field its state does not name, a
field of another state, or a repeated key is refused rather than dropped.

A host reads any claim's root with the host-private `named_root_read`
operation, including a claim whose holder is gone or whose run has finished.
Its request carries `run_id` and `claim_id`; it takes no idempotency key,
fence or cut. Any work session of the project may read. Engram checks the
session's connection, that the run exists, that the claim is that run's and
that both belong to the project, and then reads, all from one snapshot of the
store:

- `project_id`, `work_id`, `root_execution_id`, `run_id` and `claim_id`;
- `run`: its `state` and `generation`. `state` is the `WorkRunState` wire
  value: `open`, `claimed`, `active`, `completed` or `cancelled`. A `claimed`
  or `active` run is the normal in-progress case. `run.state` is distinct
  from the work item's lifecycle and is never an alias of it;
- `claim`: its `state`, `holder`, `expires_at`, `revision` and `fence`, as
  stored. An expired claim is not reported as ended or released: expiry is
  disclosed, never derived into another state;
- `named_root`: the claim's state, derived exactly as above, at the read's
  cut;
- `latest_event`: the claim's newest recorded `named_root_bind` event at the
  cut, bound or ended, by its real record id (`event`), run-feed `position`,
  `generation`, `kind`, `workspace_id`, `named_at` and, for an ended one,
  `end_reason`. It stays named when the state is `none` because the root
  ended or the run finished, so a host tells those from a claim never named,
  for which it is `null`. It is a read of a recorded event, not a binding
  receipt;
- `read_cut`: the run-feed position everything above was read at.

The read writes nothing: it records no event, changes no revision and
consumes no idempotency key, so a host may repeat it freely. A still-bound
root whose holder is gone reads `bound`. An unknown run, a claim that is not
the run's, or a run of another project returns `named_root_read_refused`,
with `error.reason` respectively `run_unknown`, `claim_not_current_for_run`
or `other_project`. Hosts match the code and reason without parsing the
message text. The reason identifies a refusal, not a named-root state. A
record that fails its canonical association returns a storage error. A
refused or failed read is never a `none` state. A claim's absence from one
session's `work core held` list says nothing about that claim's binding.

### Reading a root's initial sighting before an evaluation

Recording an acceptance evaluation at a cut under a named root is refused
with `no_initial_sighting` while the root has no sighting at or before that
cut. A host can ask first, with the host-private `named_root_sighting_read`
operation, and spawn no evaluator that would be refused. Its request carries
`work_ref` (the item's short reference or full id, within the project the
work session serves), `run_id` and an optional `run_cut`; without one the
read takes the run-feed head. It needs no live claim holder and takes no
idempotency key.

Engram resolves the item, checks that the run is one of its runs and that
the cut lies between 0 and the head, and then reads, from one snapshot, the
root recording would select at the cut and whether it has a sighting there.
It uses recording's own functions: the run's current claim's newest binding
after its latest release, when that binding names a root, and the root's
newest source record at or before the cut in its workspace, at its
generation, with the root named. A host observation counts when it was
accounted as a change, a repeat or a source sighting; one accounted as no
change does not. A sighting recorded by an earlier build's turn checkpoint
counts as recorded. A
record after the cut is never a sighting at it. The result has exactly three
shapes:

<!-- named-root-sighting-read:begin -->
| Field | Type | Meaning |
| --- | --- | --- |
| `schema_version` | integer | This result's version, 1. |
| `project_id` | string | The routed project. |
| `work_id` | string | The item `work_ref` named. |
| `run_id` | string | The run read. |
| `read_cut` | integer | The run-feed position read at: the requested cut, or the head. |
| `head_cut` | integer | The run-feed head in the same snapshot. |
| `current_binding` | string or null | The binding event of the root recording would select at the head; null when it would select none. |
| `binding_changed` | boolean | Whether the binding selected at `read_cut` differs from `current_binding`. |
| `root.state` | `none` or `bound` | Whether a root is bound at `read_cut`. |
| `root.workspace_id` | string | The bound root's workspace; `bound` only. |
| `root.generation` | integer | The bound root's generation; `bound` only. |
| `root.binding_event` | string | The binding event's record id; `bound` only. |
| `root.binding_position` | integer | The binding event's run-feed position; `bound` only. |
| `root.sighting.state` | `absent` or `present` | Whether the root has a sighting at or before `read_cut`; `bound` only. |
| `root.sighting.record` | string | The sighting record's id; `present` only. |
| `root.sighting.position` | integer | Its run-feed position; `present` only. |
| `root.sighting.revision` | string | The source revision it sighted; `present` only. |
<!-- named-root-sighting-read:end -->

A host acts on it this way:

- Only a valid result with a `bound` root whose sighting is `absent` shows
  that recording at `read_cut` would be refused for want of a sighting.
- An unsupported `schema_version`, a malformed or unknown shape, a refusal
  or a failed call is unknown, never absent.
- `binding_changed` true means the root moved after the cut: recording at
  that cut is refused as moved before the sighting is looked at, so the host
  reads again at a newer cut.
- A read at the head returns `read_cut` equal to `head_cut`; the host carries
  that cut as the floor of the evaluator's basis under the same binding, and
  never uses it to justify an earlier cut.
- The read takes any run of the item, but recording evaluates only the
  item's active run, so only a read of that run predicts recording.
- The read is not a verdict. Recording an evaluation still decides, against
  everything recorded since, and the host revalidates before it spawns.

A work reference that names no item of the project returns
`named_root_sighting_read_invalid_work_ref`, and an ambiguous one keeps
`work_reference_ambiguous`. An unknown run, or one of another item, returns
`named_root_sighting_read_wrong_run`; a negative cut or one past the head
returns `named_root_sighting_read_invalid_cut`; a result larger than 16 KiB
returns `named_root_sighting_read_response_too_large`. The read writes
nothing: it refreshes no connection, renews no claim and records nothing, so
a host may repeat it freely.

### Sightings against a root

A sighting's generation, not its time or feed position, places it against a
binding at generation g. The sighting is before that binding when its
`source_root_generation` is absent or smaller than g, and after it otherwise.
Under a later generation g2, a sighting stated under an earlier, still-bound
generation g counts as before g2 only when it was recorded in g's own root
workspace; a sighting already foreign to g's root stays a foreign change under
a bound name, across renames and across a release, a reclaim and a new name,
even when the later name is that very workspace. After an end or a release the
claim is unbound again: a sighting recorded on the run feed after its
generation ended or was released, or stated `ended`, is unbound, neither
judged inside a root nor displaced, and the rules for a claim without a root
apply to its change: a later matching check satisfies it, and without one the
stock rule waives it as untested at `done` while an operator-selected rule
keeps it open for an operator waiver. At completion, a source-change
obligation is displaced only when its trigger sighting is in another
workspace and before the active binding in that sense. Displacement depends
only on where and when the trigger was sighted, never on which rule opened the
obligation: an operator-selected rule, even one that pins its check, is
displaced and disclosed like the stock rule. A foreign change recorded under a
bound name stays open until an explicit human waiver. Neither a later name nor
a check made while the claim is unbound, after that root ended or the claim
was released, discharges it, and `done` on an unbound claim refuses it too. A
source change that carries no source basis has no generation: its run-feed
position places it against the binding event, and it is never displaced.
Recorded while a root was bound, it needs a fresh check in the claim's active
named root or an explicit human waiver, even after that root ended, was
released or was renamed, and `done` on an unbound claim refuses it.

After a claim has named a root, the optional positive
`source_root_generation` and `source_root_state` (`named` or `ended`) travel
together on a sighting. They describe the claim's root lifecycle at capture,
even when the sighting measured its ordinary workdir. Both are absent on a
sighting taken before the claim's first name. A stated generation must
already have a matching host event for that claim; a missing or mismatched
event refuses the observation.

## Host observations

A host records what it saw run, how the source changed and which checks it
observed with the host-private `execution_observe` operation. The record is
attributed history, never authority: it renews no claim and credits no
check. Its request carries:

- `idempotency_key` (trimmed, nonblank, at most 128 bytes);
- `binding`: the exact `root_execution_id`, `work_id`, `run_id`,
  `work_revision`, `claim_id` and `claim_fence` observed, never whatever the
  session is focused on now;
- `root_basis`: the `capture_run_cut` (a run-feed position), the claim's
  `latest_event` record id (or `null`) and its `state`, exactly as
  `named_root_read` reports them at that cut;
- `observed_interval`: `from` and `through`, the window actually observed;
- `occurrence`, tagged by `kind`: `unadmitted_turn` (`host_turn_ref`,
  optional `outcome`, `source_change` or `null`, and `observed_checks`, at
  most 16), `inter_turn_change` (`source_change`), `observed_check`
  (`host_turn_ref` and one `check`), or `source_sighting` (`source_basis`
  and `observed_at`), a measured look at the source that reports no change;
- `causality`: `{"kind": "unknown"}` or `{"kind": "host_assertion",
  "claimed_actor": …, "basis": …}`. There is no verified cause;
- `policy_basis`: `{"mode": "audit_only"}`, or `account_if_eligible` with the
  project's current `project_policy_epoch`, `policy` and
  `obligation_rule_set`.

The wire names `unadmitted_turn` and `admission: "unadmitted"` say only that
Engram admitted nothing; they are the names of every observed turn.

Only `unadmitted_turn` accepts `outcome`: `succeeded`, `failed`, `cancelled`
or `unknown`, as reported by the host's terminal lifecycle owner. Explicit
Stop cancellation is `cancelled`; an exit without a known terminal result
is `unknown`. Omission remains omission in stored history and readback;
it is never inferred to mean success or replaced with `unknown`. Null and
unrecognized values are refused. This fact is provider-neutral and conveys
no verification credit.

A `source_change` is tagged by `detection`, and every kind names its
`workspace_id`: `content_comparison` adds a `baseline` (`workspace_id`,
`source_revision`, `observed_at`) and a `sighting` (`source_basis`,
`observed_at`) of that one workspace at two different revisions;
`assumed_missing_baseline` adds a `sighting` only; and `watcher_only` adds
`observed_at` and no revision.
`source_change: null` means only that no change is reported, never that the
source is unchanged. The detection value is the host's word about its own
method, stored as said; the core cannot check it. A check carries
`host_check_id`, `check_kind`, `observed_result`, optional `started_at` and
`finished_at`, `observed_at`, an optional `source_basis` and an optional
opaque `host_evidence_ref` (at most 2048 bytes, never fetched). A passed or
failed result needs `finished_at`; an unfinished check is `indeterminate`.
Every time lies inside the observed window, and the window ends no later than
the record time.

A `source_basis` is `{ workspace_id, source_revision,
source_root_generation?, source_root_state? }`. `source_revision` is a
host-computed fingerprint of the complete relevant content state, including
committed and dirty content. `workspace_id` is the host-recorded workspace
identity, compared byte for byte; Engram never derives it from path text.

#### Source text

New host source text, a `workspace_id` or `source_revision`, may not hold a
C0 or C1 control character or a character of Unicode's `Bidi_Control`
property (U+061C, U+200E, U+200F, U+202A–U+202E, U+2066–U+2069). Other format
characters, such as the zero-width joiners, variation selectors and the soft
hyphen, and Windows extended paths are kept byte for byte. Text already
stored keeps reading as stored. These fields must also be trimmed.

#### Admission and accounting

Engram checks the session's connection, replays an identical earlier request
under the same session and key, and otherwise checks the request against the
store's history at the capture cut: a canonical event at or before the cut
must record that claim epoch on the bound run, whoever held it, and the root
basis and any root generation a source basis names must be what the claim
had recorded by then. A claim that has since expired or moved on keeps that
history valid; the observing session need not be the claim's holder. A
closing sighting that states a root generation must agree with the root
basis: `named` at generation g needs the claim bound at g at the cut, and
`ended` at g needs no root there with g's end as the claim's newest root
event; a check may name any generation the claim had recorded by the cut.
The request is bounded to 64 KiB before it is decoded and its receipt to
16 KiB before it commits. Nothing is truncated. A frame over 64 KiB,
malformed JSON, an unknown field at any depth (including inside
`root_basis.state` and `claimed_actor`) or an unknown variant is refused as
`invalid_request`. A decoded request that breaks a rule above, an asserted
actor with any assurance but `asserted` or an oversized field, a receipt
over 16 KiB, and new source text that breaks [the rule above](#source-text)
are refused as `execution_observation_invalid`, the reason naming the field
by the path its request carries (`check.source_basis` for a single observed
check, `observed_checks[i].source_basis` for an unadmitted turn's checks). A
binding, cut or root basis the store does not hold is refused as
`execution_observation_basis_mismatch`, and an `account_if_eligible` basis
that does not name the project's current policy epoch, policy and obligation
rule set as `execution_observation_policy_basis_mismatch`. Each refusal
records nothing. The same key with any changed fact returns
`control_operation_idempotency_conflict`; an identical retry, including one
over a new connection for the same session, returns the original receipt.

An `audit_only` request is kept as a fact and accounts for nothing
(`{"kind": "audit_only", "reason": "explicit_audit"}`). An
`account_if_eligible` request is decided on the store's history just before
the record's own run-feed position, never renewing the claim. The lifecycle
decides first: a run finished by then keeps it as `audit_only` with reason
`finished_run`; a binding that is no longer the run's newest claim epoch (a
newer claim or fence, a released claim, or an item revised or moved to
another run) as `historical_binding`, while an epoch that merely expired
still accounts; and a named-root state or newest root event that changed
since the capture cut as `root_basis_moved`. Then a request with no reported
change, `source_change: null` or a standalone `observed_check`, is
`no_source_change`: it clears nothing and moves no freshness, and the sources
of the checks it carries never count as sightings of the source. An eligible
`source_sighting`, rechecked against the claim's current epoch, root and
policy, records where the source was: it counts as a sighting of the source,
opens no obligation and credits no check. A measured
change whose revision equals that of the newest change recorded in its
workspace, recorded under the same claim, with no other revision seen in that
workspace since (the run's repeat rule, scoped to the workspace, and to the
named root when one is bound), is a `repeat` naming that change in
`source_change`; it opens nothing. A revision seen in another workspace in
between does not make it a new change, but a change with no located source
in between does, since nothing shows it was elsewhere. A watcher-only change
carries no revision and no located source, so it is unlocated (the workspace
it names is kept for display) and is never a repeat, and a change that
returns to an earlier revision after another one (A, then B, then A) is a new
change. Any other change is a `source_change`: it accounts at its own
position even when other records arrived since the capture, opens the
selected rule set's obligations triggered by the record at that position,
moves source freshness, and is classified against the claim's named root.
The stored record anchors a new change to itself by leaving the id out; the
receipt names the record's own id.

An observed change records where the source was when the host saw it, which
may be older than sightings recorded since, so it is a barrier rather than a
pin. A check accounts for an obligation it triggered, and for the run's
latest change when that change is an observed one, only when the check's
producer and its verification are both recorded after the change and the
check completed no earlier than the change was recorded: an old result whose
records arrive late does not count. Without a named root, the revision such a
check must carry is that of the newest measured sighting at the assessment
cut in the change's workspace, not the change's own; audit-only records and
checks nested in an observed turn never count. Under a named root the root's
newest sighting decides. So a late report of an old revision asks for a fresh
check without moving the source back, and checks run before it may need
rerunning. A bound criterion names no source change, so its check is held to
the floors of every accounted observed change on the run at the cut, a
watcher-only one included (under a named root, every one not sighted outside
it), whichever change is the latest: when its binding's obligation is
satisfied, when completion checks the newest check of its kind, and when an
evaluation cites it ([F8](acceptance-evaluation.md#freshness)). These rest on
the host's asserted measurements and times; they do not prove unseen
filesystem state or physical order.

The receipt says `decision: "recorded"` and carries the `observation` record
id, its run-feed `position`, the `observing_session`, the `binding`,
`admission: "unadmitted"`, the `causality`, the `policy_basis`, the
`accounting`, the `opened_obligations` (the definition ids of the
obligations a new change opened, otherwise none) and, for each check, its
`host_check_id` with `credit: "uncredited"`. The record sits on the project,
root and run feeds. A peer's `next` shows it as `unadmitted_observation`,
leading with its cause (`cause unknown` or `unverified cause`), its
uncredited checks and its accounting; `show REF --observations` lists it with
its window, its cause, and every check as `observed check, uncredited`. No
check in it can satisfy an obligation, mint verification evidence, count as
a gate, be cited by an evaluation or linked at completion, or enter a seal's
tested set: observed checks are facts without test credit. Doctor and
migration import hold an accounted record to the accounting its history
decides: the named policy version must hold the named epoch and rule set, the
accounting is decided again on the history before the record's position, and
a new change must hold exactly the obligations its rule set calls for.

## Registered verification

The host-private `verification_record` operation records the terminal results
of a durably registered launcher run. Registration is **asserted by the host,
not independently proven**. The host calls only for its durable
registrations, never an unregistered command or an artifact found later.
Engram validates the connection's project and session and the pinned current
work revision, claim, fence and named root; later focus cannot retarget a
result.

`work_session_read` advertises `verification_record_v1` in its capabilities.
Missing means unsupported: retain the result without sending. Install the
producer before enabling host emission. Old builds refuse the unknown
operation without writing; an installed binary's identity does not establish
support in a connected process still running an older build.

```text
operation_key: termal-run:<host_run_id>
registration: {host_run_id, request_identity, registered_at,
  initiating_session_id, owning_session_id?, binding, root}
root: {workspace_id, generation, state: named}
input: {kind: commit, commit} | {kind: staged_tree, tree, base_commit}
run: {mode, command, started_at, finished_at, outcome}
checks: [{stage, check_kind, command, check_fingerprint, started_at?,
  completed_at?, outcome, summary?, refs?, counts?}]
post_run: {outcome, clean_before, clean_after, dirty_paths?, disposition?}
```

Ordinary input names its full commit; staged input names its exact tree,
with base commit as provenance. Only input clean before and after execution
is admitted here. A dirty-input disposition does not waive this rule or
change landing requirements.

An immutable `registered_host_run` preserves the aggregate and individual
stage results. Completed checks enter canonical verification with that
producer, using existing feeds and the matcher. Unrun stages mint no
verification. An interrupted stage without a known completion time stays
indeterminate in the producer and mints none. A passing stage cannot turn a
failed run into a passed gate.

The receipt is `{operation, producer, verifications: [{stage, record_id}],
run_outcome}`. The host persists one exact request and retries the same
body/key after an unknown transport outcome or restart, never rerunning the
test for settlement. Accepted exact replay returns the original ids even
after claim movement. Changed intent conflicts. A new submission against a
moved association returns `verification_record_refused` with
`details.field` and `details.reason`, recording nothing; the host retains it
as not credited, without changing the key or substituting a later claim.

`verification_bind` credits one registered check to several items: the check
runs once, on one item's run, and binding gives every other item a record of
it on its own run; see
[binding one check to several items](acceptance-evaluation.md#binding-one-check-to-several-items).

Only a typed, passed verification for the required check, exact run, and
latest source revision may satisfy a verification obligation. The built-in
rule leaves `check_fingerprint` empty, so it accepts any passed test. An
operator-selected typed V1 set may instead pin the exact check. A
requirement never names an environment, and a requirement that names
`required_environment` is refused by name. The matcher still requires the
evidence to follow the latest mutation at the evaluated run-feed cut, so a
later source mutation reopens the requirement. Generic agent-recorded
`work_evidence` remains useful context but never verifies a check.

## Remote verification

The host-private `remote_verification_record` operation records a test run on
another host, launched by that host's native root and delivered over the
existing mailbox. Its facts are **asserted by the host, not independently
proven**: the host correlates them with its retained native run and the original
request, and mailbox text saying PASS or exit 0 is never evidence. There is no
log, results bundle, file hash, source remeasurement, extra capture or agent
import step.

`work_session_read` advertises `remote_verification_record_v1`. **The first
delivery requires every consumer of the store on a build that has this
operation.** The change adds no durable schema, table, column or index, so a
store is ready and the binary is installed as usual, and import needs no
conversion. But an older build cannot read the two record kinds: its delivery
page stops at them, it cannot read a verification they produced, and its doctor
reports them. The capability alone does not prove this, because a control
sidecar can advertise it while an MCP server still runs the older build until
the host restarts. A host holds its first delivery until that restart has
happened. Once a store holds a delivery, reinstalling an older build is unsafe.

```text
operation_key
completion: {schema_version: 1, origin: {host_id, runner_session_id}, run_id,
  mode: full|focused,
  result: {verdict: PASS|FAIL|INTERRUPTED|RUNNING|UNKNOWN, terminal, exit, reason},
  input: {commit?, tree?, before_fingerprint?, after_fingerprint?},
  finished_at?}
association: {binding, root, use}
use: {kind: committed_input, expected_commit, purpose: verification|landing_gate}
   | {kind: authorized_test_ref, ref_name, expected_tree, purpose: landing_gate}
```

`exit` must be stated and may be null; other optional members are omitted when
missing, never null. Missing facts stay missing and are never guessed. A
malformed value or an unknown member answers `invalid_request` and records
nothing. A well-formed finish time ahead of this host's clock is not malformed:
it stays provisional, visible and uncredited, with a null origin. If the earlier
qualification reasons below do not apply, it receives `result_not_pass` and the
time in `detail`. A later delivery can settle it under a new key.

**Send the complete retained facts the first time.** The first authoritative facts
are the run's terminal, so a later delivery of the same run with more facts, such
as a fingerprint or finish time left out at first, is `result_conflict`. Only a
provisional delivery can be followed by fuller facts.

The receipt is `{operation, origin, verifications: [{stage: full, record_id}],
qualification: {credited, reason?, detail?}}`. `operation` names the delivery's
record, and `origin` names the run's terminal record or is null. An exact retry
under the same key returns the same receipt before its destination is judged
again, even after the claim or root has moved; a changed request under the same
key conflicts. The same result delivered to the same destination for the same use
under another key returns the existing delivery, so one result earns one credit
there.

### One terminal, many deliveries

A `remote_terminal_origin` holds one run's terminal facts, once per project,
host and run. Its uniqueness is a `remote_verification_origin` row in
`work_operation_results`, keyed by project, host and run. Only `terminal` true
with verdict PASS, FAIL or INTERRUPTED, and no finish time ahead of this host's
clock, settles it. A missing finish time does not prevent settlement, but it
prevents credit. A running, unknown or inferred snapshot never fixes the terminal,
and a later real terminal of the same run settles without conflict. The launcher
really emits UNKNOWN with `terminal` true (`input-unverified`) and INTERRUPTED
with `terminal` false (`heartbeat-stale`); both stay provisional. Two different
terminals for one run are `result_conflict`, and the first stays.

A `remote_verification_association` records one delivery: the request as stated,
the origin, the decision and the local `associated_at`. The run's own
`finished_at` stays apart from that time and is what every time guard reads. The
delivery stands on its item's project, root and run feeds, and `show` lists it
among the item's observations with both times, where it cannot be cited as run
evidence. A stale or unsupported delivery goes on the project feed only, and
writes nothing in that item's scope, so `show` does not list it: its durable
visibility is its receipt, which an exact retry returns, and one change delivery
to each session. A result can serve several items, as one origin with several
deliveries.

### What earns credit

One decision serves every use. Reasons apply in this order:

1. `producer_unsupported`: this build does not read the completion's
   `schema_version`.
2. `association_stale`: the destination's item, revision, claim, fence, run or
   named root has moved.
3. `result_conflict`: the run already has different terminal facts.
4. `not_full_mode`: a focused pass stays visible author evidence only.
5. `result_not_pass`: a finish time ahead of this host's clock.
6. `untracked_debris_pending_disposition`: an authoritative terminal native FAIL
   whose reason is `new-untracked-only`.
7. `result_not_pass`: anything else but a terminal PASS with exit 0 and a finish
   time. The debris case above is the exception checked before these ordinary
   result predicates.
8. `commit_mismatch`: for `committed_input`, the tested commit is missing or
   another commit, even with the same tree.
9. `tree_mismatch`: for `authorized_test_ref`, the tested tree is missing or
   another tree.
10. `fingerprint_missing_or_changed`: for `landing_gate` only, the before and after
    fingerprints are not both present and equal.

`detail` names missing data; there are no further reasons. An ordinary
`verification` has no fingerprint rule at all, not even for differing optional
fingerprints. The launcher's native reason is kept unchanged. A debris result
counts toward landing only through the integration owner's existing disposition
note and run-referenced gate record; neither that gate nor a mailbox message makes
it verification.

Only a qualifying delivery mints verification: one `test` record, written by the
same writer as any other, whose producer is the delivery. `committed_input` is the
input `git-commit:<expected_commit>` and `authorized_test_ref` is
`git-tree:<expected_tree>`, so it enters the same
[registered Git input](acceptance-evaluation.md#registered-git-input-evidence)
decision, and the same root, run, cut, time and mutation guards. The wire names no
check, so its `check_fingerprint` is derived from the host, run and mode: it
satisfies a requirement that leaves the check open, and a requirement pinning
another check stays unmet. It never goes through a local producer, grant, turn,
sighting or `execution_observe`. Purpose and ref tags grant no authority, and
credit is not landing authority. Platform coverage stays the integration owner's
judgment.

**Cross-host time ordering assumes comparable clocks.** The time guards compare the
remote finish time with local times, and nothing attests that time. A remote clock
running behind can withhold credit. A remote clock running ahead can make an old
run appear to follow a later local change: a run that ended at T, a local change at
T+30 and a delivery at T+60 pass every guard if the origin's clock is 45 seconds
ahead. Credit names the exact commit or tree the run tested. Evaluation compares
that input exactly, independently of clock ordering. When a bound completion
criterion selects registered Git verification, completion with a declared input
compares it with that verification. Without a declaration, unevaluated completion
uses that registered verification's own input for this comparison; it does not
independently check the checkout's input. The applicable time and position guards
still apply. A criterion that selects no registered Git verification does not run
this declared-Git-input check. Non-registered verification, including a bound copy,
keeps its source-revision matching against the available root sightings, measured
sightings and latest source changes.

For a new evaluation, a passed registered check in the evaluated run can
corroborate the exact declared Git input and named root even when an unbound
judgment cites only notes. The evaluation then stores that check as its source
witness; it does not assign the check to a criterion. A bound criterion keeps
its required observed citation. Quiet content sightings while the evaluator
runs do not change this Git input. A measured edit after the check still needs
a later check, including after a measured revert. An edit and revert before the
next measurement remain within the Git cleanliness lane's stated limits.
Existing evaluations keep their recorded source interpretation; a stored
witness is validated at its original cut and never replaced by another check.
A quiet sighting of a different Git input cannot corroborate a new witness
or leave it current after its cut. Content fingerprints alone do not compare
with Git identity; the source-change observations remain the change guards.
Witness selection corroborates identity only: it grants no criterion credit
and does not hide a newer failed check from the evaluator.

`verification_bind` can copy a credited remote check through its ordinary bound
branch, under its usual measurement rules. Crediting another item with the result
itself is a new delivery of the same origin.

## Obligations from source changes

The active immutable `ControlPolicy` selects a canonical `ObligationRuleSet`
by record id. The built-in set contains the typed
`source_mutation_requires_test` rule, which evaluates every work-bound
observation accounted as a new source change, regardless of outcome or
whether a source basis is present. For a host observation that is the
`source_change` accounting described under
[admission and accounting](#admission-and-accounting); a report of no change,
a sighting and a `repeat` open no obligation.

Observations an earlier build admitted from a turn are history, read as
stored. Each keeps the `source_changed` reading recorded with it, and only
one recorded as true opened an obligation. That build read a reported change
as true unless its `source_revision` equaled the revision of the run's newest
recorded source change and no host record on the run since that change
carried another revision (under a named root, within the reporting
workspace); when the report or that newest change carried no revision, or the
run had recorded no change, the host's report stood. No new observation is
read this way.

An observation records the rule-set id its `policy_basis` named, and every
resulting
`WorkObligation` repeats that exact selection. Activating another set
affects only later observations and cannot reinterpret a prior trigger,
definition, or completion cut.

Each match appends one immutable `work_obligation` definition directly to the
project, root-work, and run-execution feeds. A passed `test` verification
appends a separate `work_obligation_resolution` only when it matches the exact
run and the newest source basis visible at the evaluated run-feed cut. That
evidence may satisfy older still-open mutation obligations as well as the
newest one. For a claim without a named root, if the newest mutation has no
source basis, no verification can match it: all open obligations remain
waiver-only until a later basis-bearing mutation and passed test establish a
newer verifiable state. Under a named root, a change with no source basis
never becomes the root's newest mutation; a fresh check in the root that ran
after it accounts for it instead.

On a finished run, records are audit only: an observation opens no
obligation, a verification satisfies none, and no record becomes a
contributor or contribution of the root, whose sealed accounting already
closed over the run. One rule decides that a run is finished, the same one by
which readers show a finished run's obligations as history. Obligations an
earlier build opened on a finished run stay stored and readable as they are.

The stock rule records rather than blocks. A
`source_mutation_requires_test` obligation may stay open because no matching
passing test followed a change. Completion then resolves that obligation as a
waiver attributed to the completing actor. The waiver's host-private reason
names the change and its source revision. The waiver is appended inside the
sealed cut, so the seal still binds only terminal obligations. A completion
refused for another reason rolls the waiver back with the rest. Its recovery
page is read from the state before the waivers, so it never names a waiver
that rollback discards. On that waived obligation, the `obligation_page`
entry carries `untested_change`: the host's observation id, source revision,
and observation time. An operator waiver of the same rule carries it too. The
page's `untested_total` counts every such change on the run, including those
the bounded page leaves out. Agent `done` and `show` print one
`untested source change:` line per named change, then the exact count of any
not shown, so the changes stay visible after completion. A change a named
root displaced is disclosed the same way: its obligation page entry carries
`displaced_change` (the host's observation id, workspace and source
revision), the page's `displaced_total` counts every such change, and `done`
and `show` print one `foreign workspace change:` line per named change, then
the exact count of any not shown. Peers receive each untested change in
`next` as an `untested_source_change` delta, and each displaced change as a
`foreign_workspace_change` delta, never as an untested waiver. Only the exact
stock definition behaves this way: the stock id at version 1, triggered by a
source change, requiring an unpinned test. Obligations from an acceptance
binding (`--bind`) keep blocking completion, and so do obligations from any
other operator-selected rule, including one that reuses the stock id with
another version or a pinned check. Each needs a matching verification or an
operator waiver, unless a named root displaces its trigger.

Definitions and resolutions are canonical feed objects; the mutable
obligation row is only a verified projection. `work_focus`, nested
`work_next.focus`, `work_update`, and both completion outcomes use one count-
and byte-bounded `obligation_page` with explicit omission count, immutable
identities, state, rule-set identity, rule, requirement, trigger, terminal
evidence/resolution, and deterministic typed guidance. On a run that
completed or that a seal binds, the page carries `historical: true`: its rows
are read as stored, its `open_total` is 0 and its guidance is `none`. Generic
readiness strings remain separate. A fresh session reconstructs the same
summaries from canonical history rather than trusting a prior response.

A waiver may be requested through the operator CLI, never through MCP or a
direct `work_update` waiver variant. The shell request carries an attributed
reason; its actor and `waived_by` human are asserted, not authenticated.
Revising or dropping an acceptance binding still resolves its open obligation
with retained history. Agent-facing pages omit the reason. Completion first
records the stock rule's open obligations as waivers, as above, unless a
named root, active now or bound when the change was recorded, changes that
disposition: it displaces the pre-binding foreign changes of any
source-change rule and refuses the changes a root held open, even after it
ended or the claim was released. It then evaluates the cut-aware open set at
the exact pre-seal run-feed cut; any obligation still open refuses. Terminal
definitions are frozen into the seal as exact definition/resolution id pairs
under obligation schema V1, and completion success reconstructs its page from
that sealed basis. New seals separately declare environment schema V1 and
bind the sorted, distinct environment-evidence ids at or before the same
dense run-feed cut; the seal carries ids only and refuses more than 64
records.

## Project policy

`ControlPolicy` and its `policy_epoch` are project-scoped; the per-project
SQLite store is the selection scope. The policy selects the obligation rule
set and the acceptance-evaluation policy. `engram init` installs a versioned
built-in policy and atomically selects its record id; on an existing current
store it preserves the selected policy. Any missing, different-build, or
corrupt schema or active policy fails store open for every service surface.
`doctor --recover-policy` may inspect the policy family read-only but cannot
return a usable store or enable mutation. On a cold store, core DDL, host
path-policy binding, and the canonical policy selector and history commit in
the same immediate transaction, so a crash cannot leave an empty policy table
that later resembles established state.

Policy history is ordered exclusively by `policy_epoch`. `activated_at` and
authority `decided_at` are attribution timestamps; clock skew does not reorder
the immutable chain or block activation. Store open, policy activation,
`doctor`, and integrity verification traverse and verify the complete
authority-bound predecessor chain. Every object must use the current
supported schema.

The operator-only `engram control-policy set-obligation-rule-set` command may
append an attributed successor under an epoch/id compare-and-swap. It accepts
bounded, strict JSON inline or through `@file`, and activates only the fully
re-supplied typed set; rollback never trusts a record id alone. The command
is not exposed through MCP or the work session. V1 rule sets are bounded
typed data, not a natural-language rule engine; unknown schemas, nested
fields, duplicate rule identities, and unknown triggers fail closed. Its
operator identity is asserted host context; policy administration is not
authenticated.

## Host integration contract

A conforming host adapter must:

1. treat Engram notifications only as doorbells and fetch state by cursor;
2. where the runtime lets the host learn that a context compaction happened,
   make the agent's next prompt re-orient through a non-advancing read
   (`next` with peek, and `memories`), never advancing delivery on the
   agent's behalf;
3. surface refusal codes and recovery actions to the human and agent;
4. after an uncertain outcome (a timeout or a lost reply), resend the same
   request under the same idempotency key; whenever the request changes,
   change the key;
5. keep the record ids a receipt returns and compare them with the evidence
   sent; when it records less evidence than the work produced, tell the
   holder which evidence was not recorded;
6. never attribute a source change to a claim or run that was not live when
   the host saw it;
7. bind a named source root with `named_root_bind` before stating its
   generation on a sighting, and treat a binding whose outcome is unknown as
   no binding: state its generation on no sighting, and give no check credit
   and no evaluation source under that generation.

Engram refuses a request whole and never asks a host to drop part of it. A
host that answers a refusal by sending a smaller request does so by its own
policy, and requirement 5 applies.

### Request allowance

The work-session producer gives each input frame a fixed 15,000-millisecond
cooperative allowance, measured from its first available byte. It takes no
new request field. Admission checks the same monotonic allowance before
dispatch, before `BEGIN IMMEDIATE`, after acquisition, and before `COMMIT`.
The connection's current SQLite busy timeout is capped by the remaining
allowance and restored after the attempt; this does not make a native wait
or filesystem call preemptible.

`control_request_expired` certifies that this invocation committed no
changes: the transaction was not entered, or explicit rollback succeeded
before `COMMIT`. Retry only the same retained request and idempotency key on
the current connection. It does not clear uncertainty about an earlier
attempt with that key or prove that the request was unsent.
A busy store is not certified. When `BEGIN IMMEDIATE`, or a statement before
`COMMIT`, returns `SQLITE_BUSY` or `SQLITE_LOCKED` during a work-session
write, the reply is the generic `storage_error`, as are failed rollback and
failed `COMMIT`. Its outcome is unresolved: keep the request and its original
key, and reconcile by exact retry.

An exact retry can expire again: the operation's own work may exceed the
allowance, so retrying cannot be assumed to make progress. Hosts must bound
their automatic retries. If expiry repeats within that bound, retain the
request, its original key and any earlier uncertainty, then escalate for
diagnosis instead of retrying without limit. An exhausted retry bound does
not discard the request or settle an earlier attempt.

Once `COMMIT` starts, the producer lets it settle and returns that result
even when the allowance elapsed. If the host deadline expires without a
reply, the outcome remains unknown until exact-key reconciliation succeeds;
the host must never infer `control_request_expired` from its own timeout. A
late or lost successful response is recovered from the original stored
operation result. This is a cooperative pre-commit bound and eventual
settlement contract, not a promise of delivery within 15 seconds under
arbitrary OS or I/O stalls. Startup before the first frame is outside this
allowance.

`redaction_refused` answers a request whose text the pre-write redactor
refused. It is a definitive refusal: nothing was recorded, and the same
payload is refused again. The shipped development redactor never refuses.

### Terminal startup failures

An open or connection-resume failure answers at most the first bounded,
nonempty input frame with one flushed JSON line, then exits nonzero. There
is no unsolicited startup handshake. The frame is consumed but its operation
is not dispatched, including a mutating request. A malformed bounded frame
or a bounded partial frame ending at EOF receives the same refusal. Empty
EOF, a read error or an oversized first frame produces no response; startup
stops at the size bound without draining the rest of the line. Healthy
servers retain their existing oversized-frame drain and continue behavior.

The envelope has exactly `status` and `error`; `error` has exactly `code`
and `message`. The two reserved terminal codes and fixed messages are:

```json
{"status":"error","error":{"code":"control_startup_busy","message":"Engram control startup could not acquire the store."}}
{"status":"error","error":{"code":"control_startup_refused","message":"Engram control startup was refused; inspect the startup diagnostic."}}
```

Only a typed SQLite `DatabaseBusy` error selects `control_startup_busy`.
Other startup errors select `control_startup_refused`; their original
diagnostic stays on stderr. These codes are never emitted by a live server.
The normal SQLite wait remains bounded at five seconds. The host bounds the
wait for the first input frame with its request deadline.

The host must evict the process after either code. Busy permits bounded
re-admission by starting a new process; refused fails closed. Neither code
means the process never started or that nothing mutated: earlier startup
work may already have committed. Do not reopen blindly around connection
generation rotation or replay a request whose execution is uncertain.
Ordinary `storage_error` replies from a healthy server remain nonterminal
request failures.

### Opt-in phase trace

`ENGRAM_MCP_PHASE_TRACE=1` in the environment of `engram control` when it
starts turns on a phase trace. It shows where startup and each input frame
spent their time, for a host that diagnoses a slow or expired exchange. A
process keeps the setting it started with. Unset, stderr phase tracing and
detailed SQLite profiling are disabled; the default bounded
[operational diagnostics](diagnostics.md) remain active.

The trace changes no response and adds nothing to stdout. Each record is one
JSON line on stderr, at most 4,096 bytes with its newline, keyed
`engram_control_phase_trace` (the format version, now 1).

Every line carries:
- `pid`, which is diagnostic only, since a host keys records by its own
  identity for each process it spawned;
- `seq`;
- a per-`seq` `ordinal`;
- `kind`: `startup` or `frame`;
- `state`.

Numbering:
- `seq` 0 is startup.
- `seq` N is the Nth input frame in physical order. It is numbered when its
  first byte arrives; waiting idle for that byte is not part of a frame.
- A blank, malformed, unknown-operation or oversize frame takes a number like
  any other, and an oversize frame's drain is part of its own number.
- An unterminated last frame takes one; a bare end of input takes none.
- Frame N is normally answered by the Nth response line. A failed read or
  write, or a kill, can leave it without one, so a host matches records by
  `seq` and its own attempt state, never by counting lines.

States:
- `in_flight`, for progress. A startup or frame still running after one
  second gets a progress line, then one every two seconds, at most ten. Each
  names the `phase` in progress, `elapsed_ms` and `phase_elapsed_ms`, and,
  while a `BEGIN IMMEDIATE` or `COMMIT` runs, that `statement` and its
  elapsed time. The last one says where execution was last observed, not
  necessarily where it stayed.
- `complete`: startup reached ready, or the response's newline was written
  and flushed. A protocol error is still complete, with its `outcome` code.
- `write_failed`: writing the response failed.
- `incomplete`: processing stopped otherwise where the process could say so.
- At most one terminal line follows the progress lines, and a stale progress
  line never follows it. A killed process may leave none.

Fields:
- A frame names its `operation`, the protocol label, or `invalid` for a frame
  that does not parse, from the moment it is parsed, before its handler
  runs.
- A terminal line carries `total_ms` and `phases`, the time of each phase
  reached. A phase not reached is left out, never shown as zero.
  - Startup phases: `starting` (from `main` until the command is known),
    `project_resolve`, `host_path_probe`, `connection_open`,
    `schema_check`, `host_path_policy`, `policy_history_verify`,
    `schema_init`, `connection_resume`. A startup that fails ends
    `incomplete`: with the store error's code when the store cannot open,
    otherwise `startup_failed` or `invalid_session_id`.
  - Frame phases: `handler_total`, `response_serialize`,
    `response_write_flush`. `handler_total` runs from the frame's first byte
    until its response is ready: reading the rest of it, parsing and
    handling it. That is wider than the MCP trace's handler time, which
    begins after the request is parsed.
- `begin_immediate` and `commit` give count, total and longest time. A
  `BEGIN IMMEDIATE` call can include a wait for the write lock, and a
  `COMMIT` can include I/O; neither is pure lock wait.
- Phases name the instrumented scope, not a proven cause. They nest and are
  not summed.
- No line carries a request or response body, an error message, SQL, a path
  or a token.

Delivery:
- Lines go through a bounded queue to their own writer thread. A stderr
  nobody drains drops lines, counted in `dropped_lines`, and never stalls a
  request; the losses of a killed process are unknown.
- While the trace is on, the process's own error and warning lines go to
  stderr through the same queue, after the trace lines, and its exit waits
  for them only briefly. A stderr nobody drains can then cost those lines
  too, but never blocks the exit. A panic message is still written directly.
- Startup is measured from the process's `main`, so a host's own timings
  cover spawn and the time before it.
- A host that captures the lines drains stderr independently of stdout. It
  tolerates other stderr text, gaps and unknown versions, and never lets a
  trace line decide retry or credit.

## Records from earlier builds

Turn gating, the per-turn grants an earlier build issued (turn grants:
Evaluate/Begin) together with its session admission, settlement and
recovery, was removed. No operation, setting or flag can turn it back on.

Its canonical records, the checkpoint events and the observations,
verification and environment evidence a checkpoint carried, remain readable
as stored, and an obligation or seal that cites them keeps its recorded
meaning. Such an observation keeps the `grant_id` it was stored with; a read
that returns it returns that id as passive data, which names no current grant
(see [identifiers on the work-session channel](cli-and-mcp.md#identifiers-and-fingerprints-on-the-work-session-channel)).
The completed store migration retired `control_sessions`,
`control_turn_results`, `control_turn_grants` and
`control_turn_grant_supersessions`, and the `control_policy_state` columns
`required_assurance`, `supported_effects_json` and `grant_ttl_seconds`, by name,
reporting each with its count; the backup holds their original rows. The
converter has been removed after all live stores were converted. Current
import refuses those tables and the removed policy-selector columns by name;
converting an older backup requires the retained converter build. The anchor,
change-index and operation-result tables stay as passive history. Policy
versions written before the removal stay readable as stored; a new policy
version omits their assurance, effect
and grant-lifetime fields. Reading any of these never restores the
operations that wrote them.
