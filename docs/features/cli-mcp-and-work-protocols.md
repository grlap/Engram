# MCP and Work Protocol Reference

[CLI and MCP index](cli-and-mcp.md) · [Architecture](../architecture.md)

### MCP tools

`engram mcp` registers the fifteen work tools below (fourteen words plus
`search`) and the separate `task_sync_settings` configuration tool.
Its [settings contract](task-planning-text.md#project-opt-in-setting) uses
`action: status|enable|disable`; project and attribution are connection-bound.

| Tool | Purpose |
| --- | --- |
| `next` | What is ready, what this session holds, and the changes since its previous call |
| `ls` | Open items with `search`, `ready` or `blocked`, `mine`, `all`, `label`, direct-parent `under` and `optional`/`required` filters, plus non-confidential `after` continuation |
| `show` | One item in safe agent detail; changes neither focus nor claims |
| `add` | A root from a title, or one child with `under`; `optional` permits a peer proposal beneath a foreign-held parent; `notes` records ordered initial observations atomically; outcome and acceptance default from the title |
| `claim` | Hold an item; later calls default to it |
| `update` | One `action`: `release`, `blocked`, `unblock` (optional `blocker` selector), `revise`, `cancel`, `reject`, `after`, `drop_after`, `waive`, `detach`, or `supersede` |
| `gate` | Record one bounded pass/fail observation; completed work accepts it as a late finding without a claim or reopen |
| `evaluate` | Record attributed acceptance verdicts for the held item; independent evaluation remains host-owned |
| `note` | Record evidence and checkpoint open work; completed work records only late evidence, both keyless |
| `done` | Complete the held item. A source change with no later matching passing test is recorded and disclosed as untested; any other open obligation returns the typed `open_work_obligations` result |
| `search` | `ls` over every lifecycle |
| `handoff` | `offer`, `accept`, or `cancel` the unique checkpoint-coupled handoff |
| `remember` | Create or explicitly revise an attributed episode under one permanent key; retain history |
| `memories` | List/search current rows or read one current/historical revision of a live key |
| `forget` | Append an attributed terminal tombstone; never erase or reuse the key |
| `task_sync_settings` | Read or explicitly enable/disable the connection-bound project's host-local task-sync opt-in setting |

Task-text exchange remains an explicit CLI/shared-service operation, separate
from that configuration tool. `task-sync setup-materialize MAIN` selects
`--setup-operation`, `--prepared-operation`, `--progress-directory` and
`--candidate-directory`; it publishes retained raw preparation bytes through
owned local recovery, without opening SQLite, staging Git or sending a network
request. New preparation/export/import/materialization require project opt-in;
an already admitted pending operation can finish after opt-out. See
[selected materialization](task-planning-text.md#materializing-a-selected-preparation)
for admission, refusal, ownership, filesystem and recovery limits.

#### Read-only mode

`engram mcp --read-only`, with the same identity arguments, serves a host
child that must not mutate the store. It lists the read words `next`, `ls`,
`search`, `show` and `memories`, plus `task_sync_settings` with `action: status`,
and admits only their reading forms:

- `next` only with `peek` exactly `true`, which stages no delivery and
  records nothing;
- `memories` only without `context_generation`: the argument present at all,
  even as `null`, is refused, because a listing that carries one records it;
- `task_sync_settings` only with `action: status`; its enable and disable
  actions are refused before any setting path is created;
- each tool only with the arguments it declares.

Any other call, a tool that is not a read word, a writing form of a read
word, or an undeclared argument, is refused before the tool runs, as an MCP
tool error (`isError: true`) whose JSON reads
`{"error": {"code": "mcp_read_only_refused", "message": "MCP read-only mode
refused TOOL: …", "details": {"mode": "read_only", "tool": TOOL,
"restriction": R}, "reminders": […], "next": […]}}`, where `R` is
`tool_not_admitted`, `argument_not_admitted`, `next_without_peek` or
`memories_with_context_generation`, or `settings_mutation`; the code is stable. Like every tool
error Engram itself returns, it carries `reminders`, one line naming the
restriction, and `next`, the read to make instead: `engram work next
--peek` after a tool that is not a read word or a `next` without the peek,
`engram work memories` after a listing with a generation, and nothing after
undeclared arguments; `engram task-sync status --json` follows a settings
mutation refusal. The read words open the store read-only for each call, and the connection never opens or holds
the writable one, so nothing it does writes the database or its WAL; SQLite
may still coordinate through the shared-memory file. A missing or
uninitialized store refuses and is not created. Ordinary `engram mcp`
lists all sixteen tools and marks none of them read-only,
since `next` and `memories` have writing forms there. The text a read word
returns may still suggest a writing command; in this mode such a call is
refused. `initialize` gives a read-only connection its own instructions,
naming the five read words, the separate configuration status read and their
admitted forms. The settings status read creates no files and opens no SQLite.
The process still writes bounded [operational diagnostics](diagnostics.md)
outside SQLite under the selected home's `diagnostics/` directory, including
refused calls and its exit summary. Read-only mode restricts store operations;
it does not promise a filesystem with no writes. An unwritable diagnostic
destination preserves responses and may lose diagnostics as documented.

A read-only child cannot record a memories listing, so when its host passes
a context generation, the peek's direction to run `memories` with that
generation never settles: the child lists `memories` without it and reads
the entries it needs, and the direction stays standing.

Every agent tool result keeps its structured shape and adds two fields.
`reminders` holds words only, derived by a fixed table from the readiness
`obligations` strings, open `obligation_page` items, active blockers, and the
claim holder. `next` holds literal `engram work …` commands derived by a fixed
table from `allowed_next`: at most one dead-prerequisite `--drop-after`
recovery followed by lifecycle moves in priority order (`handoff --accept`,
`claim`, `note`, `done`, and for an item's only active blocker the exact
`update REF --unblock --blocker SELECTOR`, or `show REF` when several are
active), with three commands total and
one trailing `show REF`, so no receipt lists more than four. Other planning
edits (`--blocked`, `--release`, `handoff --to`, `add --under`, `--title`,
`--cancel`, `--after`, `--waive`, `--supersede-with`) are not synthesized as
general next commands; their tags stay in `allowed_next` on the structured
receipt. A successful `add --under PARENT --optional` beneath a foreign-held
parent lists `show CHILD` and `show PARENT` as inspection guidance and does
not suggest execution; own-parent and root `add` receipts keep the
ordinary claim guidance. A required-child completion refusal supplies the exact waiver command.
The host-only reopen operation remains structured-only. A holder-only mutation
against completed work instead names `note` as the late-finding path and never
suggests reopening merely to record evidence.
Errors keep their stable code and details and add the same two fields. The
shell prints a one-line receipt followed by `reminders:` and `next:`; `--json`
prints the structured receipt plus `effective_session_id` only for a successful
default-session mutation. In the baseline `add → claim → done` lifecycle,
text output contains no full record id (32 or 64 lowercase hex), fence number,
or idempotency key. `scripts/parity.test.mjs` checks that on a fresh store and
counts three commands and at most three agent-supplied fields. Scoped
note/detail navigation and evidence locators may expose full record ids,
including read commands for clipped status notes.

`ls --mine` returns items assigned to the actor plus the session's focused
item when this session holds it; claims on other items are visible through
`show`. `add --under` selects the parent and submits one required child
through `work_propose:decompose`; adding `--optional` instead records an
optional child that is shown as such and does not gate parent completion (a
decomposition admits one through 16 children). An omitted `--priority`
(MCP `priority`) follows one creation rule for `add --under`, a core
decomposition and an atomic plan alike: a required child gets its parent's
priority, an optional child the project default (1), as a root does; an
explicit priority wins. The rule applies only when an item is created;
existing items keep their stored priority. Either form then focuses that
child exactly as a root `add` focuses the new root; a bare word that follows
while this session holds other work is refused, as described under
[reading and focus](cli-agent-usage.md#using-engram-as-an-agent). On open work, `note`
records evidence and then checkpoints the run's current evidence set. On
completed work, `note` records only attributed late evidence after the frozen
completion cut; it creates no checkpoint, does not reopen or reseal the run,
and cannot enter the existing `CompletionSeal`. Repeated commands follow the
[session and intent retry rule](local-work-system.md#agent-native-protocol);
not every keyless repeat is a replay.
An identical child `add --under` in the same session replays the original
creation after decomposition's own parent revision advance, including its
proven restored-run bootstrap. Changed child intent creates new work; other
parent or authority changes refuse instead of silently creating another child.
The `work_decomposition_retry_conflict` refusal names the parent and its changed
state without exposing a server key. Inspect the parent and its existing
children, reuse an already-created child when present, and add new work only
for a genuinely different child intent.

### Work protocol contract

The lifecycle words use the six ambient work operations: `work_next`,
`work_focus`, `work_propose`, `work_update`, `work_complete`, and
`work_handoff`. `remember`, `memories`, and `forget` are a separate thin
project-memory surface, not new work-core operations, and `evaluate` is the
service's separate evaluation entry (`work_evaluate_on`), also not a core
operation. Hosts and operators may call the six work operations directly
through `engram work core`; they are not additional MCP tools, and `evaluate`
is reachable only as a word. Session binding supplies
project, actor, current work,
and cursors, so update/complete/handoff do not repeatedly shuttle ids. Ambient
state contains no authority token; each mutation rechecks the project, item,
claim, and fence state. `work_next` returns only the selected
`focus`, `ready`, `catalog`, `changes`, `memories`, `assigned`, and/or
`participated` sections; omitting `sections` selects all seven. Core CLI callers use
`--sections focus,ready,catalog,changes,memories,assigned,participated` and
MCP callers pass a string array. Selecting no `changes` section never stages or
advances project delivery, including when a prior page remains pending.
Ready and catalog candidates are filtered and limited by maintained SQLite
projections before their compact item rows are decoded. Assignment and label
filters use NFC plus full Unicode case folding, and catalog text search uses a
trigram index over the short reference, title, outcome, labels, and active
blocker detail, combined with complete public note/gate text and refs from the
transactionally maintained note index in the same read snapshot. Search trusts
that projection; explicit detail reads and doctor retain canonical validation,
as described in the [search contract](cli-agent-usage.md#using-engram-as-an-agent). These views remain advisory; lifecycle mutations revalidate
their canonical work-event basis under the write lock.
Core `next --search` includes canonical note and gate matches, but its catalog
rows omit the match details. To find why an item matched, run
`engram work ls --all --search QUERY`, follow its continuations to the returned
work ref, and read its `note_match` locator with `engram work show REF --note LOCATOR`.
This listing is a new read and may observe newer state. `--all` broadens lifecycle
coverage; it does not recreate every core catalog filter.
The `--blocked`/`blocked_only` filter is independent of derived availability:
it returns work with an active blocker or incomplete prerequisite even when
the item is deferred, but excludes completed, cancelled and superseded work.
Proposed work retains its existing treatment. Historical blockers remain
inspectable through `show` and unfiltered `ls --all`; filtering writes no
events and changes no stored rows.
Source changes retain dense positions and explicit compact summaries instead
of canonical work snapshots or memory bodies. A change's `object_id` is the
id of the source record the summary was projected from; it names that record
and says nothing about the summary's content. Restricted
work memory, and work memory outside the session's currently focused verified
root, is replaced by a typed `omission` marker at its original dense position;
the protected body and structured fields never cross the agent boundary. The
largest dense prefix fitting the fixed change budget is staged durably; each
call returns the changes since the session's previous call, and the previous
page counts as delivered when the session asks again. A response lost between
Engram and the agent is therefore not redelivered; the section is advisory and
canonical state stays readable through focus and catalog views. A host that
needs exact delivery acknowledges explicitly by returning the exact
`delivered_through` value and opaque `delivery_token` as `acknowledge_through`
and `acknowledge_token`; both fields are absent when changes were not
delivered. A pair matching neither the pending page and token nor the already
confirmed cursor is refused. To recover after an invalid ACK or lost response,
serialize this session's delivery and focus-changing calls, then run
`engram work core next --sections focus` without ACK fields. It reports
`session.confirmed_project_cursor` and `session.pending_delivery` without
staging or acknowledging a page (not necessarily without session writes).
Run `engram work core next --sections changes --acknowledge-through
<confirmed_project_cursor>` with that cursor and no token: acknowledging the
confirmed cursor is a no-op, so any retained page is returned with the same
change payload, `delivered_through` and `delivery_token`, even if later data
exists. Dynamic advisory fields are not part of that exact replay. After
delivering the page, acknowledge its returned pair. This may stage the next
page; repeating the previous ACK does not acknowledge that next page.

Do not drop ACK fields on a changes call to recover: that implicitly
acknowledges the pending page. The whole recovery sequence requires host-side
serialization for the same session; a stale cursor may refuse and require a
fresh cursor read. No exact-replay guarantee covers concurrent advancement or
a focus change discarding the page. Agent `peek` is a different interface,
not the host cursor read. See the complete
[host recovery recipe](../host-checklist.md#recover-a-host-delivery-after-an-invalid-ack).
Concurrent appends wait for the next page. Every successful work response is at
most 12,288 serialized JSON bytes; typed `omissions` report advisory sections
shortened by count or byte budget. A `staged` changes omission instead means
those dense entries remain unconsumed for the next page; it is not a
byte-budget discard. `work_focus` is navigation only and never claims/releases
as a side effect. Its host-only core result returns an exact history count with
a bounded newest-event summary tail, the latest run even after completion, and
a body-free actor-filtered memory index. The `show` word projects that result
into the [safe agent detail view](cli-agent-usage.md#using-engram-as-an-agent); event summaries name what
happened instead of repeating only the transition kind and title. A staged
page never blocks a focus change or a mutation; changing focus discards the
un-delivered page and the next call recomputes the same interval under the new
focus.
`work_propose` atomically handles roots and bounded decomposition. `work_update`
carries a typed transition such as claim/release, checkpoint, blocker, cancel,
supersede, deferral, assignment, revision, or prerequisite change. Update and
handoff success responses contain only a compact receipt, one bounded
`obligation_page`, generic readiness `obligations`, and `allowed_next`;
their size does not grow with item history. A successful holder note, update,
evidence, checkpoint, or handoff on open work advances claim expiry to at
least one hour after that mutation without shortening a longer explicit TTL;
successful completion terminalizes it. A completed `note` or `gate` instead
appends attributed evidence without a live claim, claim renewal, checkpoint,
reopen, or reseal. A holder mutation on a lapsed claim is
refused with exactly one next command: `engram work claim <ref>`. Once the work
is ready, that ordinary claim command retakes the same holder's claim, advances
the fence, preserves an active run, and needs
no recovery reason. Every handoff offer expires no later than its source claim.
A different prior holder still requires the explicit recovery path. Each entry
names the exact tool and tagged operation. For example,
`allowed_next: ["work_update:claim(recovery_reason_required)"]` directs the
agent to submit the `work_update` claim variant with an attributed
`recovery_reason`; ordinary
claiming is `work_update:claim`.

While a live offer is pending, fresh holder notes, checkpoints, evidence,
release, renewal and claim-bound planning are refused. Fresh action lists and
next commands omit them and suggest cancelling the offer. The refusal's
details include `offer_id`; its reason code and existing remedy are unchanged.
Record status before offering, cancel to resume holder writes, or use the
host's peer channel while waiting. Non-holder observations remain admitted;
successful historical retries keep their original receipts.

A successful claim receipt includes
`control_binding { root_execution_id, work_id, run_id, work_revision,
claim_id, claim_fence }`, the held-work binding the host-private
work-session operations accept, such as `execution_observe` and
`verification_record`. The same live tuple appears as `focus.control_binding`;
`focus.run` also exposes `root_execution_id` and `work_id`, while
`focus.claim` exposes the claim and fence components. `work_revision` is the
focused work item's revision—the claim receipt's top-level `revision`—not the
claim projection's own revision counter. A binding appears only when the
work-session operations would accept it. The calling session's live claim
proposes the tuple, and the same validation those operations run decides. A claim with a pending
handoff offer therefore shows no binding, though `focus.claim` still shows the
claim; so would a claim under a closed ancestor or outside the active root
execution. No binding while `focus.claim` shows the calling session's live
claim means the session still holds the claim but the work-session
operations would refuse it now; an earlier binding for it is refused as stale
until a later read shows the binding again. This holds when an answer is built: a claim
retried with the same idempotency key returns the stored original receipt,
binding included, so a host reads the current binding with `work core inspect`,
not from a replayed receipt.

A host that must read a claim's binding without moving the agent uses
`work core inspect <ref>`. It returns the same bounded view as `work core
focus`, `control_binding` included, read in one snapshot on a read-only
connection: it refuses a missing or uninitialized store rather than creating
one, selects no focus, stages or discards no delivery page, appends nothing,
registers no session, and carries no focus-bound memory index. Selecting a
different item with `work core focus` is not a side-effect-free read: it
discards a staged delivery page and changes the item that bare agent words act
on. Core focus, the focus nested in core next, and
inspect always carry `control_binding`: the
binding when the calling session holds the item's live claim and the
work-session operations would accept it, and an explicit `null` otherwise, never a
missing key. Because only the calling session's
claim yields a binding, the host must use the agent's own session id.
Null and an absent key never mean different things; these reads always emit
the key, with null meaning no bindable claim. The binding goes stale when the
holder's planning update changes the item
revision or when a lapsed claim is retaken with a new fence; the host then reads
it again before its next work-session request. A pending handoff offer hides the binding.
Cancelling the offer restores the same binding, and so does letting it expire
while the claim is still live. An offer never outlives its claim, so the two
can lapse together; then no binding returns, and retaking the claim gives a new
fence and a new binding. Accepting the offer moves the claim to the recipient
under a new fence, so the offering session gets no binding back and the
recipient gets a new one.

A host that must choose which held claim to name uses `work core held`. It
lists every item in the project on which the calling session holds a live
claim: `work_id`, `short_ref`, `claim_id`, `claim_fence`, `expires_at`,
`claimed_at` (when this session acquired the claim, by claiming it or by
accepting a handoff; renewals leave it unchanged), `focused` (whether the item
is the session's focus), and `control_binding`, the binding the work-session
operations would accept, computed as focus and inspect compute it, or an
explicit `null`.
A row with a null binding is still a held claim, for example one with a
pending handoff offer. Only the holder may revise a claimed item, and its
revision re-accepts the claim at the new revision, so the row shows a fresh
binding and an earlier one is stale. Rows come newest claim
first, then by work id, at most 16 of them; `total` counts every held claim,
and `omitted` counts those left out. `focused_work_id` names the session's
focus, or `null`, whether or not it is held. Like inspect, it reads one
snapshot on a read-only connection: it refuses a missing or uninitialized store,
selects no focus, touches no delivery page, appends nothing, and registers no
session.

`work_complete` can consume evidence/checkpoint state created through explicit
`work_update` calls, or accept `capture { summary, refs }` to record evidence,
checkpoint its exact evidence set, and seal in one model-level call. Completion
is refused while blockers, prerequisites, required child seals or explicit
completion waivers, live handoffs, capture requirements, or run obligations
remain unresolved. Open obligations are not an MCP error envelope.
When a satisfied bound criterion is contradicted by a newer relevant failed
or indeterminate check, or its newest check fails the existing verification
freshness rule, completion returns `WorkBoundVerificationRefused`. Its
`work_completion_refused` code, human message, CLI exit 1 and MCP error status
are unchanged. Native CLI JSON and MCP `error.details` add `cause`, containing
the one-based `criterion`, `requirement` (check kind and optional command
fingerprint), `mismatch`, selected `verification`, original `satisfied_by`,
`producer_observation`, actual `result`, and typed `remedy`. A
`stale_source_revision` mismatch adds `cause.stale_source`, the record that
decided it, in the shape the verification detail uses. A word reminder after
the remedy names it as one sentence. The message keeps its words.
The remedy is `run_current_check` for a matcher mismatch or
`run_passing_check_after` for a non-passing result. `error.details.remedy`
and the word reminders format that action from the cause, without parsing
the reason. Candidate selection, the completion-only recording-order
fallback, and admissible bindings and waivers retain their existing rules.
This is an error, not an owed-result completion receipt. The storage
completion transaction rolls back temporary waivers and all completion
effects on refusal; service preparation such as capture and checkpoint
remains recorded as before. The refusal preserves session focus.

`work_complete` returns a typed `open_work_obligations` result with the same
`obligation_page` used by `work_focus`, nested `work_next.focus`, and
`work_update`. A completed receipt also returns that page reconstructed from
the exact terminal obligation ids bound into the seal. Each page is count-
and byte-bounded, reports an explicit `omitted_count`, and carries immutable
obligation/definition identities, the required exact rule-set id, state,
rule, requirement, trigger, terminal
resolution/evidence when present, and deterministic typed guidance. An open
verification requirement directs the caller to record matching host
verification, checkpoint it, then complete, or request a host/operator waiver.
Trimming retains open obligations before satisfied, waived or displaced
history and keeps
deterministic trigger/resolution ordering within those state groups. In focus
evidence, a visible verification summary keeps its referenced environment
summary ahead of it. Count and byte trimming remove
unrelated or dependent evidence before breaking that visible typed closure.
Generic readiness strings remain a separate compatibility field. A successful
completion result is durably replayable under the same idempotency key;
recoverable refusals are recomputed from current state. The pending attempt
retains the original request and work target, so a lost refusal cannot redirect
the same caller key after focus moves. An interrupted
capture-backed completion reuses committed evidence, and reuses an exact
checkpoint while its acknowledged run-feed cut remains current. If the feed
advances, the retry writes a new checkpoint under a cut-derived substep key.
Cut selection and that checkpoint append share one SQLite write transaction.

For a cancelled or superseded required child without a seal, qualifying
[sibling successor resolution](local-work-system.md#gates-prerequisites-supersession-and-project-memories),
or waiver, the refusal names that lifecycle and returns the runnable agent command
`engram work update PARENT --waive CHILD --reason "why"`. The matching MCP
update uses `action: "waive"`, `child`, and `reason`; both translate into the
existing typed `work_update:waive_required_child` operation. The project-bound
session records the reason-attributed, audited waiver, after which retrying
`done` re-evaluates the current completion barrier. The parent and all its
ancestors must be Open. An explicit waiver under a terminal ancestor returns
`work_invalid`, explains the refusal, and offers `engram work show PARENT` in
`error.next`. Follow that parent's admitted detach or resolve-first guidance,
or file an independent root follow-up.

Recoverable completion refusals add `recovery { cause, item, command }` to the
receipt. `cause` is a tagged value for `open_obligation`,
`required_child_unsealed`, `missing_contribution`, or `missing_acceptance`,
including the exact blocker identity. `item` carries the
affected full id, short ref, title, and lifecycle-backed state. `command` is
deliberately a single next command, and the `done` verb exposes exactly that
one entry in its `next` list. Beside an `open_obligation` cause, never inside
it, `recovery.open_obligation_check` names the check the obligation waits for.
It is read at the completion cut:
- `none_followed` when no passed check of its kind was recorded after the
  obligation opened;
- otherwise `newest`, with that newest passed check's `verification` record
  and `position`, and its `mismatch` or `left_out` reason. A stale mismatch
  adds the `stale_source` that decided it.
A reminder after the cause's unchanged words says the same. For a `newest`
check, `next` adds `show REF --note RECORD`, which reads that check's
detail. Recovery guidance is not a replayable result: it
is rebuilt from a coherent current snapshot so a retry observes a child,
contribution, obligation, or acceptance barrier that moved. Native `done` and
the fifteen MCP work tools return this as a typed refusal receipt. The JSON
core prints the same typed refusal receipt on stdout and exits with status 1;
it does not wrap the refusal in an error envelope. Short-ref ambiguity likewise
returns a stable
`work_reference_ambiguous` error with up to eight ordered candidates, an exact
`more` count, and full-id retry guidance on every JSON front door.

The host-only JSON core schemas for `work_propose`, `work_update`,
`work_complete`, and `work_handoff` use typed discriminated inputs.
Each accepts an optional `work_ref`; the target is resolved and bound inside
the mutation, so a concurrent focus change by the same session cannot redirect
it, and it becomes the ambient focus as a side effect. `idempotency_key` is
optional on every mutating branch; the
[session and intent retry rule](local-work-system.md#agent-native-protocol)
defines automatic keys, explicit overrides, and current replay limits.
Durable attempts bind caller intent separately
from the current focused work/claim/handoff basis. A pending refused completion
may refresh its live claim basis only after the original target binding is
verified; committed successes replay, and an interrupted attempt cannot mutate
a newly focused item. Omitting
`work_complete.acceptance` asserts every current criterion with the note
`accepted by <actor_id> via work done` (or the supplied `note`) and leaves each
criterion's evidence empty unless explicit `links` and `link_basis` bind
existing records to selected positions. Those links cannot be combined with
explicit `acceptance`. Explicit per-criterion citations pass through and
must belong to the completion evidence set; work-level evidence is never
automatically assigned to individual criteria. Omitting
`work_update:checkpoint.evidence` acknowledges every evidence object already on
the live run.

Work search/lifecycle filters, paged catalog results, and item history ship in
the ambient query/focus views. Stats, stale/orphan diagnostics, approval
decisions, import/export, and report publication remain administrative tools
over the same core. Successful model responses are terse; refusals return a
stable code plus a satisfiable remedy. Full durable receipts go to the host. No
host-private value appears in model-visible MCP output. Agent-facing work has
no grant token.

One capture powers peer context and the ordered feed. The host may use a
mailbox as a doorbell, but must not relay full state or make the agent repeat
the same fact into another status ledger.

### Host-private work-session channel

`engram control --work-session` is a long-lived stdio process for the host's
work session. It accepts one JSON object per line and returns one
`{ "status": "ok", "result": ... }` or typed error line. An open or
connection-resume failure consumes at most one bounded first frame without
dispatching it, replies with terminal `control_startup_busy` or
`control_startup_refused`, and exits nonzero. The host must evict that worker;
only busy permits bounded safe re-admission. Neither code proves that startup
made no durable changes. See [Terminal startup failures](execution-observability.md#terminal-startup-failures)
for the exact frames and input bounds. The runtime session and asserted actor
are fixed by process arguments: `--actor-id`, `--session-id`, and the optional
`--actor-context` (or `ENGRAM_ACTOR_CONTEXT`) that the work words and the MCP
server also accept. A host passes one context to every channel of a session;
the work-session connection normalizes it the same way and records it on its
attribution, never on the principal.

Startup rotates the connection fence, so a still-live predecessor for the same
session fails its next operation with `control_connection_superseded`.
`work_session_read` takes only `{"operation":"work_session_read"}` and returns
the fixed `project_id`, `session_id`, `work_session: true` and capabilities
`host_work_v1`, `verification_record_v1` and
`remote_verification_record_v1`. It also returns
`verification_evidence_git_tree_applicability_v1: true` only when the serving
connection observes the exact current applicability barrier. This read writes
nothing. Work, claim, source and connection
fences apply to every operation. Nothing on this channel admits, delays or
refuses a model turn; see [Execution Observability](execution-observability.md).

The shipped operations are:

| Operation | Durable effect |
| --- | --- |
| `work_session_read` | None. Read the work session's identity and evidence capabilities, including the conditional git-tree applicability advertisement. |
| `control_connection_read` | None. Takes no fields. Read which store this serving connection opened. A fail-closed contract: no platform can yet prove the opened file, so it refuses with `control_connection_identity_unavailable` after checking the connection is current; see [Reading which store a connection serves](execution-observability.md#reading-which-store-a-connection-serves) |
| `named_root_bind` | Append a claim's named source-root event, `bound` or `ended`, to the project, root and run feeds |
| `named_root_read` | None. Read one claim's named root on its run, for any run and claim of the project: identities, run and claim lifecycle, the derived `named_root` state, the newest root event by its real id, and the run-feed cut, from one snapshot. A `named_root_read_refused` error carries `reason`: `run_unknown`, `claim_not_current_for_run` or `other_project`; its message and the successful claim block are unchanged |
| `named_root_sighting_read` | None. Read, for an item's run at a cut (the head by default), the named root recording an evaluation would select there and whether it has the initial sighting recording requires, with the binding event, the sighting record and whether the binding moved since the cut, from one snapshot; it needs no live holder; see [Reading a root's initial sighting before an evaluation](execution-observability.md#reading-a-roots-initial-sighting-before-an-evaluation) |
| `execution_observe` | Append one host observation (a turn the host observed, a change between turns, a check inside such a turn, or a measured source sighting) to the project, root and run feeds. It renews no claim and credits no check. Under `account_if_eligible`, naming the project's current policy, an eligible reported change accounts at its own position and opens the selected rule set's obligations; an ineligible one is kept as `audit_only` with its reason; see [Host observations](execution-observability.md#host-observations) |
| `verification_record` | Record the terminal results of a durably registered host run as typed verification, one record per completed stage, with the run's Git input and outcomes; see [Registered verification](execution-observability.md#registered-verification) |
| `remote_verification_record` | Record one delivery of a test run on another host: its immutable terminal facts, once per project, host and run, and the destination and use the host states. Only a qualifying result mints one typed verification; any other stays visible with a named reason, and a malformed request answers `invalid_request` and records nothing; see [Remote verification](execution-observability.md#remote-verification) |
| `acceptance_binding_read` | None. Read, for one item on its active run, what satisfied each bound criterion: the obligation completion selects, its recorded resolution, and the original verification with its producer, in pages pinned to the run-feed cut the first page captures; see [Reading what satisfied a bound criterion](acceptance-evaluation.md#reading-what-satisfied-a-bound-criterion) |
| `acceptance_verification_read` | None. List, for one criterion of an item on its active run, every host verification of its bound kind up to a caller-named cut that must be the run's head, with each record's id, kind, fingerprint, result, source basis, position and producer, in run-feed order and in pages with exact counts; it judges nothing; see [Listing the candidate verifications of a criterion](acceptance-evaluation.md#listing-the-candidate-verifications-of-a-criterion) |
| `verification_bind` | Bind one native passed check to 1 to 16 other open items the session holds on the same named-root workspace, in one transaction: one verification record per target on its own run, with a typed `bound_from`, satisfying only what that target's own obligation rules allow. Any failure refuses the whole request with `verification_bind_refused` and typed `details`, writing nothing; an exact retry replays the stored receipt. It creates no observation or focus change; see [Binding one check to several items](acceptance-evaluation.md#binding-one-check-to-several-items) |

The `named_root` state, `none`, `bound` or `unbound_by_release`, is the
authoritative read a host uses to decide when to name a root again; see
[A claim's root state](execution-observability.md#a-claims-root-state).

Exact retry results are stored as operational records and survive a process
restart. Reusing a key for a different intent fails with
`control_operation_idempotency_conflict`.

Work-session writes do not certify a busy store. When `BEGIN IMMEDIATE`, or
a statement before `COMMIT`, returns `SQLITE_BUSY` or `SQLITE_LOCKED`, the
reply is the generic `storage_error`, like `COMMIT` failures, rollback
failures and other SQLite errors, and it carries no guarantee that nothing
was committed. A `SQLITE_LOCKED` error need not wait for the busy timeout.
Treat the outcome as unknown: retry the exact retained operation and
unchanged payload, with its original idempotency key if it has one, and let
exact replay or reconciliation settle it. Preserve any earlier uncertainty
about that key.

`redaction_refused` answers a request whose text the pre-write redactor
refused. It is definitive: the invocation recorded nothing, and the same
payload is refused again, so a host does not replay it unchanged. Its message
carries the redactor's own reason, to which Engram adds none of the refused
text. The shipped development redactor never refuses, so no host request
produces this code today.

`control_request_expired` can be returned by every work-session operation.
Reads check the allowance only before dispatch. The code certifies only that
this invocation committed no changes, because no transaction started or
explicit rollback succeeded before `COMMIT`. Retry only the exact retained
operation and unchanged payload, with its original idempotency key if it has
one, on the current connection. It does not resolve an earlier uncertain
attempt, prove the request was unsent or mean that no process started. A host
timeout without this response remains unknown. For the cooperative 15-second
allowance, late commit settlement and bounded escalation after repeated
expiry, see [Request allowance](execution-observability.md#request-allowance).

Agent-facing `work_update:evidence` retains its generic form. It also
accepts the attach-only form
`{ kind: "evidence", attach: { evidence: <typed-record-id> }, idempotency_key }`.
Attach validates that the id names verification/environment evidence on the
focused run and does not mint another canonical object or feed entry. Generic
evidence can be cited for context and completion, but cannot satisfy a typed
verification requirement.

Every accounted work-bound observation with `source_changed: true` atomically
opens one built-in test obligation, irrespective of `outcome` and
irrespective of whether `source_basis` is present. The recorded
`source_changed` is the core's reading of the host's report, not the host's
literal flag; observations stored before this rule keep the host's flag and
are read as stored. It is true when the host reported a change, unless the
reported `source_revision` equals the revision of the run's newest recorded
source change and no host record on the run since that change carried another
revision. Such a repeat is recorded as `source_changed: false` and opens
nothing, so writes under git-ignored paths that leave the content unchanged do
not count. When the report or that newest change carries no revision, the
host's report stands, so a later change with a revision still re-anchors
obligations that a revision-less change left waiver-only. For a claim without
a named root, a passed typed test satisfies open obligations only against the
newest mutation source revision at the evaluated run-feed cut. Thus a newest
basisless mutation makes the open set waiver-only until a later basis-bearing
mutation plus passed test arrives; that later test may satisfy both the
earlier and newer definitions. For the stock rule that waiver comes at
`done`: completion records each still-open stock obligation as an untested
change instead of refusing. Under a named root, a fresh check in the root that
ran after a basisless change accounts for it, and `done` refuses the changes a
named root holds open, now or since it ended or the claim was released,
instead of recording them as untested (see
[sightings against a root](execution-observability.md#sightings-against-a-root)).
`work_focus` exposes the canonical bounded `obligation_page`, the same field
appears inside `work_next.focus`, and `work_next` deltas use
`obligation_opened`, `obligation_satisfied`, `obligation_waived`, or, for a
waived stock obligation, `untested_source_change` naming the change and its
source revision, or, for a displaced one, `foreign_workspace_change` naming
its workspace, without leaking host authority. A host's named-root events
arrive as `source_root_named` and `source_root_ended`, naming the workspace
and generation. The page's `untested_total` counts every untested change on
the run, and its items name those that fit; likewise `displaced_total` counts
every displaced change, and an item's `displaced_change` names its
observation id, workspace and source revision. Its `open_total` counts every
open obligation on the run before count and byte trimming, and is 0 for an
item that has no run yet, such as one restored from a work-graph snapshot and
not yet claimed. When it exceeds the open items shown, an open obligation was
left out, and the agent reminder says more obligations are open than shown.
Only a page stored before this count existed lacks it, and that page keeps
its original reminder.

A page whose run completed, or that a completion seal binds, carries
`historical: true`; the field is absent otherwise. Its rows are history and
owe nothing. A row whose state is `open` there is no longer actionable: on a
live read it was opened after the run finished, for example by an observation
a host recorded late; in a stored receipt replayed after the run finished, it
is the row as the receipt recorded it while the run was live. `historical`
alone does not say when a row was opened. Such a page's `open_total` is 0,
every item's guidance is `{"action": "none"}`, and no reminder, evaluation
guidance or mutation `obligations.open` count offers it as owed work,
including a stored receipt replayed after its run finished. `show` says
"historical (run completed)" and reports how many such rows the bounded page
shows as `historical_open_obligations`. A completion's sealed page lists only
the obligations its seal binds, read back at the seal's cut: a row the seal
does not bind is accepted only when it was opened after that cut, and never
breaks the readback.

Every new completion seal declares obligation schema V1 and freezes the exact
definition/resolution id pairs applicable at its dense pre-seal cut. A later
`note` or `gate` from any project-bound session is marked in the evidence
actor's existing provenance chain and appended after that cut. It is visible
in `show` notes and peer `next` changes but never changes the frozen seal or
adds a completion barrier. A new seal also declares environment schema V1 and
cites the sorted, distinct environment-evidence ids at or before that cut. It
refuses more than 64 environment records and never copies the component
payload into the seal. A parent verifies required child seals transitively;
every accepted seal carries the current obligation and environment schema
bindings.

Path-bearing host commands (`init`, `doctor`, `control`, `authority`,
`control-policy`, `readiness`) resolve the project root's filesystem identity
before opening the store: `--host-path-policy case_fold|case_sensitive`
(or `ENGRAM_HOST_PATH_POLICY`) when the host knows it, otherwise a probe that
looks the project file up under the opposite ASCII case: nothing there means
the root tells the spellings apart, and when something is there the root's
listing says whether it is the same entry. The probe only reads. A project
file name with no ASCII letter (or not Unicode text), a name that reaches the
file through a wider alias than ASCII case (such as a non-ASCII case variant,
a short 8.3 name, a trailing dot or space, or another Unicode normalization),
a project file that changes while it is probed, or a failed
lookup or listing leaves the identity unresolved. Agent work words, MCP
startup, graph, backup, restore and import do not run that probe; they still
perform their ordinary store and file I/O. The first resolved writable opener
persists that policy; read-only [readiness](host-readiness.md) never binds it
and explicitly reports an unbound or unresolved identity. Later resolved
openers must present the same one, and a mismatch names both. An opener that
could not resolve the identity (a project file the probe cannot test, or a
lookup that fails) still reads and tracks work, but path-bearing host requests
are refused with `host_path_identity_unresolved` instead of guessing. Windows
alias rules (reserved names, alternate data stream syntax, trailing-dot/space
aliases, known 8.3 aliases) follow the running operating system. `doctor`
reports the persisted and resolved policy.

Records written by the removed turn gating stay readable as stored; see
[records from earlier builds](execution-observability.md#records-from-earlier-builds).

#### Identifiers and fingerprints on the work-session channel

Every identifier or fingerprint that crosses the work-session channel, in a
request, a result or an error's `details`, has one class, and its field names
the class. A host reads the class from the field, never from the value's
length: a record id stored before ids were minted and a SHA-256 fingerprint
are both 64 lowercase hex digits, and only the field tells them apart. A host
keeps one validator per class and returns every identifier exactly as it
received it. It never pads, truncates, rehashes or relinks one, and never
parses a token. A fingerprint compares content and is never used as an id.
Engram's tests check every host receipt they produce against this table, and
fail on a value shaped like an identifier in a field the table does not list.
Request fields are listed here too; their producers' admission checks them.

| Class | Fields | Form | Minted by | Compared | Scope |
| --- | --- | --- | --- | --- | --- |
| Record id | `execution_observations`, `verification_evidence`, `environment_evidence`, `event`, `latest_event`, `binding_event`, `current_binding`, `claim_epoch_event`, `observation`, `opened_obligations`, `original` in a `verification_bind` request or receipt, `verification`, `sighting`, `record`, `records`, `definition`, `triggering_observation`, `producer_observation`, `resolution`, `satisfied_by`, `environment`, `evidence`, `evidence_basis`, `evaluation`, `supersedes`, `named_root_binding`, `root_binding`, `completion_seal`, `required_child_seals`, `policy`, `previous_policy`, `obligation_rule_set`, `rule_set`, `authority`, `producer`, `record_id`, `object_id`, `source_change` where it names a record (a `repeat` accounting), `origin` in a remote verification receipt (null when no terminal settled), and `operation` in a registered or remote verification receipt (beside `verifications`). A remote completion's `origin` is instead an object of host ids | 32 lowercase hex digits for a record stored now; 64 for a record stored before ids were minted, kept as stored | Engram, when it stores the record | Exact string equality | The project's store; a run's records belong to that run |
| Typed identity | `root_execution_id`, `work_id`, `run_id`, `run`, `claim_id`, `obligation_id`, `root_id`, `memory_id`, and an actor context's `run_id` when Engram records the actor (a bind's `binder`, a registered run) | A lowercase, hyphenated UUID. Engram always sends this form; on a request it also parses other UUID spellings, which a host must not rely on | Engram | Exact equality | The project's store; a run, claim or obligation within its work |
| Work reference | `work_ref` | `w-` and 12 lowercase hex digits. A refused `verification_bind` target whose item is not found, or belongs to another project, carries the requested work id instead. On a request, a short reference or a work id | Engram | Exact string equality | The project's store |
| Host fingerprint | `intent_fingerprint`, `action_fingerprint`, `environment_fingerprint`, `check_fingerprint` | A host sends the 64-digit lowercase SHA-256 of canonical bytes. Engram today also admits 32 lowercase hex digits and returns the value unchanged wherever it echoes it (a check's `check_fingerprint`) | The host | Exact equality of content, never as an id | None: equal content has equal fingerprints anywhere |
| Content fingerprint | `content_digest`, `work_revision_hash`, `accepted_work_revision_hash` | The 64-digit lowercase SHA-256 of canonical bytes | Engram | Exact equality of content, never as an id | None: equal content has equal fingerprints anywhere |
| Rule id | `rule_id`, `obligations_satisfied` | Non-empty text equal to its own trim, at most 128 bytes: a built-in rule's id (`source_mutation_requires_test`, or `acceptance_criterion_requires_verification:` and a criterion number) or one an operator's obligation rule set names | Engram for its built-in rules; the operator for a rule set it installs | Exact string equality; a rule's identity is its `rule_id` with its `rule_version` | The obligation rule set the active policy selects |
| Host source value | `source_revision`, `workspace_id`, a named root sighting's `revision` | Non-empty host text equal to its own trim (no leading or trailing whitespace), at most 512 bytes, without control or bidirectional-formatting characters. TermAl sends `content-v1:` and 64 hex digits as a revision; for a registered run Engram writes `git-commit:` or `git-tree:` and the Git object | The host | Exact string equality; Engram never parses or recomputes it. It names what the host measured, not which source a check or an evaluation saw: that is the Git commit or tree | The workspace the host measured |
| Git tree | `git_tree`, `tree`, `expected_tree` | 40 or 64 lowercase hex digits, as Git spells the tree | The host | Exact string equality; Engram never normalizes it | The workspace the host measured |
| Git commit | `commit`, `base_commit`, `expected_commit` | 40 lowercase hex digits, the only form registered verification admits | The host | Exact string equality; Engram never resolves or normalizes it | The repository the host measured |
| Host id | `session_id`, `initiating_session_id`, `observing_session`, `holder`, `owning_session_id`, `host_run_id`, `request_identity`, `operation_key`, `idempotency_key`, `host_check_id`, `host_turn_ref`, `host_evidence_ref`, `host_id`, `runner_session_id`, and a remote completion's `run_id` | Non-empty host text, kept unchanged and never parsed; inner whitespace is allowed. Each must equal its own trim (no leading or trailing whitespace), except a session id, which is checked for length only, and the `named_root_bind` key, which is kept as sent. A session id is at most 64 bytes; `owning_session_id` is text of at most 512. `host_run_id` starts with `test-`, and it and `request_identity` are at most 128 bytes; `operation_key` is exactly `termal-run:` and the host run id on `verification_record`, and bounded safe text of at most 512 bytes on `remote_verification_record`, whose `host_id` and `runner_session_id` are at most 512 and whose completion `run_id` is at most 128. An `idempotency_key` is at most 128 bytes on `execution_observe` and 512 on `verification_bind`. `host_check_id` and `host_turn_ref` are at most 128 bytes, `host_evidence_ref` 2048 | The host: a session id by its host integration, a host run id and request identity by its test runs, a key by its idempotent caller | Exact string equality | A session id: the host and its sessions; a host run id or request identity: that host's runs; a key: one operation's retries |
| Host text | `project_id`, `actor_id`, `agent_id`, `namespace`, `stage`, `ref_name`, `before_fingerprint`, `after_fingerprint`, a provenance link's `source` and `reference`, and an actor context's `run_id` when the host asserts the actor (`causality.claimed_actor`) | Non-empty text, compared exactly and never parsed; its form is whatever the host or its configuration gives | The host, or for `project_id` the project marker | Exact string equality | The project, or the assertion it is part of |
| Counter or cursor | `position`, `fence`, `revision` as a number, `generation`, `epoch`, cuts, criterion positions, `after`, `continuation` | A number, or opaque continuation text Engram issues | Engram | Within its own feed, run or page only | The feed, run or read it came from |
| Historical token | `grant_id`, only inside an execution observation an earlier build stored, when a read returns that record | Non-empty opaque text, kept as stored and never parsed | Engram, by the removed turn gating, when it stored the record | Exact string equality, as passive data only: it names no current grant and authorizes nothing, and no request carries it | The stored observation it belongs to |

A refused target of `verification_bind` carries `expected` and `actual`
values whose class depends on its `reason`:
- for `run_not_active` and `root_execution_moved`, typed identities;
- for `sighting_not_newest`, record ids;
- for `workspace_differs` and `sighting_revision_differs`, host source values;
- for every other reason, a counter, other host text or a time.

In that refusal, `original` and `request` are reason codes, such as
`claim_not_held`, not record ids.

A feed position names its feed as `{"kind", "id"}`. For `root_work` and
`run_execution`, the `id` is a typed identity; for `project`, it is the
project's asserted text.

A local receipt carries no authority on another host.

### Host configuration

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
        "--actor-context",
        "model=opus-4.1;reasoning=high",
        "--source-skill",
        "engram-repo"
      ]
    }
  }
}
```

The proprietary runtime supplies actor/session/tool/skill instruction context.
This process exposes fifteen work tools and the separate settings tool. V1 records host
context with `asserted` assurance; configuration text is not authentication.
Distinct concurrent sessions need distinct `--session-id` values. The database
is shared; the MCP processes are not.

### Dogfood contract

`scripts/parity.test.mjs` runs the real binary against a fresh home with
`engram init` as host setup outside the count,
then drives `add → claim → done` and fails if the agent needed more than three
commands or three supplied fields, typed JSON, or saw a full record id
(32 or 64 lowercase hex), fence, or key in text output. It also checks that an
unheld `note` records a marked observation without execution credit, while an
unnoted `done` supplies its resolving command even when observations exist.

`scripts/mcp-dogfood.test.mjs` launches real stdio MCP processes against a
fresh home. Its main lifecycle uses only the agent-facing MCP tools: one session
creates, claims, blocks/unblocks, notes, and offers a root; a peer accepts the
checkpoint-coupled handoff, notes, and seals it with `done`. Keyless replay,
`reminders`/`next` derivation, catalog and search filters, cancellation,
compact completion, child creation under a parent, and field revision are
asserted along the way. The receipt checks in this lifecycle reject full
record ids (32 or 64 lowercase hex), fences, and keys in `reminders` and
`next`; separate status-recovery tests preserve the scoped note-locator
exceptions. The CLI path drives the same lifecycle through the words in
text and `--json` modes and keeps one
`engram work core focus` call. Both scripts are part of `scripts/check.sh`.

The shared [test launcher](../development.md#test-launcher) drives these gates;
`scripts/test-launcher.test.mjs` checks its execution, diagnostics and completion
delivery behavior alongside the review-fingerprint suite.

`scripts/control-dogfood.test.mjs` launches the real bounded JSON-lines work
session and exercises its protocol end to end: host observations and
registered verification against a live claim. Report finalization/publication,
review
actions, history, explicit contradiction resolution, and the remaining
administrative CLI remain planned surfaces. They must reuse this core rather
than fork its semantics.
