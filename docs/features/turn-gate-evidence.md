# Turn gate evidence

> The one-page summary is the [turn gate assessment](turn-gate-assessment.md).
> Normative reference: [spec §2.7](../spec.md#27-execution-control).
> Related briefs: [behavioral control plane](behavioral-control-plane.md),
> [acceptance evaluation](acceptance-evaluation.md),
> [control session inspection](control-session-inspection.md),
> [host readiness](host-readiness.md), and the
> [host checklist](../host-checklist.md).

This page holds the full evidence behind the
[turn gate assessment](turn-gate-assessment.md): a snapshot, taken on 25
September 2026, of what the turn-gated control channel does in practice with
TermAl as its host. It gives, for each question TermAl asks, what the answer
allows or blocks, the TermAl call sites and code references, what the live
stores recorded, and what switching the gate off would lose. The
[behavioral control plane](behavioral-control-plane.md) brief remains the design;
this page records how much of it is used.

The same day, the grant's context page, the recovery turns and the parts
nothing used were removed, as [decided](#decisions) below. The questions and
refusals below describe the gate after those changes. The figures, examples and call sites are from the
snapshot.

Figures come from the two live stores, opened read-only on 25 September 2026
around 15:30 UTC: the Engram store (records from 2 September) and the
PhoenixCodeNav store (records from 23 September). Engram code references with
line numbers are at commit 7c1ba5d, before the change; the rest name a
function. TermAl references are to its working tree on the same day and will
drift.

## Summary

TermAl checks with Engram three times around every agent turn:

1. may this prompt go;
2. it is going now;
3. here is how the turn ended.

In 23 days the Engram store recorded 1,142 turn decisions. 29 of them were
refusals. None of the refusals stopped anything risky; all were bookkeeping.
The one part that nothing else can replace is the turn report. It lets Engram
record what the host itself saw: whether the source changed, and which test
runs passed.

| Measure | Engram store | PhoenixCodeNav store |
| --- | ---: | ---: |
| Sessions bound | 913 | 150 |
| Sessions that ever asked for a turn | 271 | not counted |
| Turn decisions | 1,142 | 201 |
| Granted | 1,113 | 201 |
| Refused | 29 | 0 |
| Turns started | 1,112 | 201 |
| Turn reports | 1,111 | 201 |

## The questions TermAl asks, in order

A session is bound once. After that, every prompt goes through a permission
check and a start, and every turn ends with a report. During a turn TermAl asks
nothing, because there is no check on individual tool calls.

### Once per session

**Session binding (`session_bind`).** "This is session X, working on this
claim. Register it."

- **Answer:** a routing token and the effects Engram will mediate. The session
  is `ready` at once, so its first turn can be granted straight away. Without
  a binding, no turn can be requested.
- **Blocks:** nothing by itself. If the claim moved on, Engram answers
  `stale_fence`, and TermAl reads the claim again and rebinds once.
- **Use:** 913 sessions bound in the Engram store, 150 in PhoenixCodeNav. Only
  271 of the 913 ever asked for a turn: delegations are bound when they are
  created, whether or not they run. Binding on the first prompt instead would
  cut most binds, and most of the rebinding after a TermAl restart.
- **Example:** session-7284 bound as `termal:session:session-7284`, assurance
  `turn_gated`, effects observe, communicate and mutate_local.
- **TermAl call sites:** `engram_host_adapter.rs` 6078–6158 (first prompt),
  5347–5388 (delegation) and 3214–3342 (restart).

**Status check (`session_status`).** "Is anything left open from before?"

- **Answer:** the session's phase, its policy epoch and any grant still open.
  TermAl uses it before rebinding and after a restart, to close a turn that was
  cut off. It also uses it when a turn start is refused with
  `grant_scope_mismatch`, to check that Engram retired the grant before asking
  for permission again.
- **Blocks:** nothing. It only reads, though it expires a grant that has timed
  out.
- **Use:** not stored, so there is no count. What it returned for
  session-7284 was phase `ready`, policy epoch 2 and no open grant.
- **TermAl call sites:** `engram_host_adapter.rs` 5523–5580, and
  `engram_queued_admission.rs` 711–720 and 310–345
  (`engram_issued_grant_was_retired`).

### Before every prompt

**Turn permission (`turn_evaluate`).** "May this prompt go to the model?"

- **Yes:** a grant, valid for 30 seconds, naming the allowed effects and the
  claim it is bound to. It carries no context: agents get their work context
  from `next` (see [context delivery](#context-delivery-inside-the-grant)).
- **No:** a refusal code, which TermAl's control card shows after "Reason:".
  TermAl rebinds and asks once more for `stale_fence`, and for
  `control_assurance_insufficient` when the session was bound before it
  declared the effect it now requests. Any other refusal, or a second
  refusal after that rebind, drops the prompt with "Engram did not authorize
  this turn for runtime delivery". After `turn_already_open` TermAl also
  rebinds the session before its next prompt. The
  [refusal table](#refusals-a-user-can-see-in-termal) lists every code a user
  can meet.
- **Use:** 1,142 decisions in the Engram store, 1,113 of them grants and 29
  refusals. PhoenixCodeNav had 201, all granted. The check takes 6 to 8 ms.
- **Example:** on 2026-09-25 at 15:33:03 UTC session-7284 was granted a turn
  requesting observe, communicate and mutate_local, bound to its claim and
  expiring 30 seconds later. On 2026-09-23 at 00:57:55 UTC session-5132 was
  refused with `recovery_required`, a refusal that no longer occurs.
- **TermAl call sites:** `turn_dispatch.rs` 220 and 1456, which call
  `engram_host_adapter.rs` 6235–6251. The purpose is always "ordinary".

**Turn start (`turn_begin`).** "The prompt is leaving now."

- **Yes:** right before the prompt goes, Engram checks the grant again
  (`evaluate_turn_begin` in `control.rs`):
  - it is still issued and has not expired, and its session phase and task are
    unchanged;
  - the session is still a participant on the task, and the task's control
    anchor still exists;
  - the policy epoch, the task's admission epoch, the capability map, the work
    revision and the claim fence are unchanged;
  - the request names no delivery token, since no grant carries one.

  It then marks the grant begun, and TermAl hands the prompt to the model.
- **No:** one of those checks failed. For an expired grant, a changed epoch or
  `stale_fence` (after a rebind), and for `grant_scope_mismatch` when a
  session status shows Engram already retired the grant (no open grant, phase
  `ready`), TermAl asks for permission again, once. Any other refusal drops
  the prompt, as a refused permission does. Engram retires the issued grant
  and returns the session to `ready` when the start is refused for an
  expired grant, a changed epoch, a stale fence, or a lost anchor or
  membership (`task_unbound`, `task_access_denied`); before 25 September a
  refusal for the anchor or membership left the grant issued until it
  expired.
- **Use:** 1,112 starts in the Engram store, 201 in PhoenixCodeNav. The start
  is what separates "allowed" from "ran", and the counts show both gaps: 1,113
  turns were granted and 1,112 started, because one grant expired without
  starting. And 1,112 started but 1,111 reported, because one turn started and
  never reported. That second case is what the restart path exists for.
- **Example:** on 2026-09-25 at 15:33:03 UTC session-7284 began its turn. The
  grant became begun. The session was already in phase `turn_open`, which the
  permission sets when it issues the grant.
- **TermAl call sites:** `api.rs` 236–240, which calls
  `engram_host_adapter.rs` 3998–4115.

### After every turn

**Turn report (`turn_checkpoint`).** "The turn ended like this. Here is what I
saw."

- **Answer:** Engram records the outcome, whether the source changed, and any
  test run the host watched, with its result. A source change opens a "tests
  have not run" obligation on the claimed work. `done` then marks the change
  untested unless the host saw a passing run.
- **Blocks:** a refused report for a turn that never started leaves an issued
  grant, which the next rebind expires. A refused report for a turn that did
  start holds the session: Engram will not rebind it until a report succeeds
  (`control_runtime.rs` 231–237).
- **Use:** 1,111 reports in the Engram store, 201 in PhoenixCodeNav. Since
  24 September they recorded 37 observations, 17 of them with changed source.
  Those opened 27 test obligations, 12 now resolved, and recorded 10 test runs
  the host saw: 1 passed and 9 were indeterminate. This is the only evidence in
  Engram that an agent cannot write for itself.
- **Example:** on 2026-09-25 at 15:32:05 UTC session-7284 reported a turn
  that succeeded without changing the source, at source revision
  `48b897c9…`, with next intent `wait`.
- **TermAl call sites:** most reports go through `engram_host_adapter.rs`
  `checkpoint_engram_turn_off_lock`, with the checks gathered in
  `engram_turn_checks.rs`. Its callers are, in `turn_lifecycle.rs`,
  `finish_turn_ok_if_runtime_matches_guarded` (a successful turn, through
  `checkpoint_successful_engram_turn_off_lock`),
  `fail_turn_if_runtime_matches_and_report`,
  `fail_turn_and_clear_runtime_atomically`,
  `mark_turn_error_if_runtime_matches_guarded` and
  `handle_runtime_exit_if_matches_guarded` (failed turns, errors and runtime
  exits); in `session_lifecycle.rs`, `stop_local_session_with_options` (a
  stopped turn) and `kill_session`; and in `session_crud.rs`,
  `shutdown_revoked_engram_mcp_runtimes`.
  A turn cut off by a restart is reported from `engram_host_adapter.rs`
  `bind_engram_target_uncoordinated_off_lock` and `engram_queued_admission.rs`
  `restore_queued_engram_target` instead. A turn Engram had begun but TermAl
  then did not deliver is closed from `prepare_engram_turn_delivery_off_lock`:
  the prompt was superseded or stopped, a project reset or settings change
  intervened, or its record could not be saved. That report, and those for a
  project reset (`checkpoint_for_project_reset_off_lock`, called from
  `session_crud.rs`), a deleted session (`kill_session` in
  `session_lifecycle.rs`) and revoked tool access
  (`shutdown_revoked_engram_mcp_runtimes` in `session_crud.rs`), carry next
  intent `exit`.

### Outside turns

TermAl also runs a few one-off commands. These are settings checks, not turn
gating:

- `readiness`, when you verify or save the project settings;
- `doctor`, for a full audit;
- `control-session-inspect`, during a strict save;
- `authority revoke`;
- `control-policy show`, for the acceptance settings.

## What surrounds the questions

### Context delivery inside the grant

Removed. A grant carries no context, and neither turn start nor the turn
report delivers anything. The `<engram-work-context>` block agents see comes
from a separate `engram work next --peek` call (`engram_mcp_config.rs`
429–459, `turn_dispatch.rs` 67–71), which never depended on the gate. `next`
keeps the work session's own position, through its own stage-then-confirm
cycle, and no gate call moves it. Each turn report still adds one event to the
task's change index, which is now only an audit trail: nothing delivers it
and no decision reads it.

Before the change, Engram built a page of work changes and context for every
grant it could (`control_runtime.rs` 553–617); the host could not supply one.
Turn start recorded the page as delivered tentatively, and the turn report
confirmed it. TermAl read only the page's position and token
(`engram_host_adapter.rs` 680–697) and never showed the content to the agent.
The page advanced the control session's own cursors, separate from `next`'s.
So removing it changed nothing agents see. It did change the design:
[spec §2.7](../spec.md#27-execution-control) bound a grant to a context packet
and to exact, host-confirmed delivery positions, and it now names `next` as
the only delivery. The control session's cursor columns stay in the store,
unused, until the next planned migration drops them.

### Holding prompts when Engram does not answer

If Engram is slow or unreachable, TermAl keeps the prompt and shows "Engram:
Waiting/Unknown. Original prompt retained; resume to retry or cancel." It does
not retry by itself; the user presses Resume. After three transport failures in
a row it stops trying for a while. This is what makes the record complete: no
prompt reaches a model without Engram knowing. It is also why a stuck Engram
stops sessions.

When a request went unanswered because of a timeout or a lost connection,
resending it once with the same key is safe. Engram answers a repeated key
with the decision it already made. Resending automatically before asking the
user to press Resume would spare a click on every passing blip. A refusal
should still go straight to the user.

### Recovery turns

Removed, with the page they existed for. A session is `ready` after a bind or
a rebind, after an issued grant expires, and after a refused turn start. A
turn begun before a restart stays open until the host reports it. No turn is
refused for falling behind its task feed; agents catch up through `next`.

Before the change, a session that fell behind its task feed had to catch up
before an ordinary turn. It fell behind after a rebind or restart, a turn
report that trailed other sessions' events, an issued grant that expired or
was replaced, or a refused turn start. Engram then answered
`recovery_required` to an ordinary turn in two cases (`control.rs` 821–846 and
1013–1021, `control_runtime.rs` 553–617):

- **The backlog was longer than one page.** Engram built a partial page, and
  only a recovery turn could consume it, one page at a time.
- **Engram built no page at all while catching up.** That happened when the
  context packet was over its pinned budget or the page was over the size
  limit.

TermAl never sent a recovery turn. It always asked for an ordinary turn and
had no code for this refusal. So the prompt was dropped, the session went to
Error, and every retry got the same answer until the state changed so that
Engram could build a single page that fit. A partial backlog never cleared
without a recovery turn.

22 of the 29 refusals were this. All came from one session on 23 September,
between 00:57 and 01:29 UTC, right after a stale-claim rebind. Its next turn
was granted at 02:20, with a page attached. The store does not show which
cause applied or what changed in between.

The one real recovery case, a turn that started and never reported, is still
handled by the status check and a report after the restart.

### Designed but never used

At the snapshot these parts existed but nothing used them. They were removed
the same day, following the [decision](#decisions) below:

- Checks around individual actions: `action_authorize`, `action_begin` and
  `action_complete`. Also `control_bootstrap`, `delivery_ack`,
  `session_heartbeat` and `session_exit`. None was ever wired to the host
  channel, and neither store has a table for action grants, heartbeats or
  delivery acknowledgements. The unused action-start check in `control.rs` is
  gone. They stay listed as not built under
  [planned interfaces](behavioral-control-plane.md#planned-interfaces).
- The "defer" answer, which Engram never gave. Turn decisions are grant or
  refuse.
- Session phases storage never wrote: `unbound`, `checkpoint_required`,
  `recovery_open`, `handoff_pending`, `contribution_required` and
  `participant_ready`. The live phases are `ready`, `turn_open` and `exited`.
  A session last written as `sync_required`, before the grant's page was
  removed, is treated as `ready`.
- Refusal codes no path produced: `control_unavailable`, `store_corrupt`,
  `control_policy_missing`, `action_outcome_unknown`, `missing_authority`,
  `resource_remapped`, `checkpoint_required`, `lifecycle_hold` and
  `participant_not_ready`. A turn start whose task
  anchor has gone now answers `task_unbound`, as permission does. It used to
  answer `lifecycle_hold`, or `task_access_denied` when the anchor had moved
  to another project, because it checked membership first.

Some codes stay although nothing has produced them so far:
`unknown_control_schema`, for a stored grant whose control schema this build
does not know; `task_admission_epoch_changed`, though nothing raises a task's
admission epoch yet; and the defensive `task_unbound` and `task_access_denied`,
for an anchor or membership that storage never deletes. The codes from the page
and recovery turns (`recovery_required`, `delta_required`,
`pinned_budget_exceeded`, `delivery_invalid`, `context_required` and
`turn_purpose_mismatch`) stay only so that refusals stored before those were
removed can still be read. So does `lease_required`, for refusals stored while
resource leases existed; see [full store migration](full-store-migration.md).

## Refusals a user can see in TermAl

A refused turn shows "Engram did not authorize this turn for runtime delivery",
drops the prompt and puts the session in Error. Its control card names the
code after "Reason:". The first table lists every code TermAl can meet from
the turn permission or turn start, and what gets the session out. The store
so far holds only `recovery_required` (22), which no longer occurs, and
`stale_fence` (7).

| Reason on the card | Why | How the session gets out |
| --- | --- | --- |
| `stale_fence` | The claim or work the session is bound to changed: the claim lapsed, was released or taken over, or the work was revised. | TermAl rebinds to the current claim and asks once more, so the user rarely sees this. If the second answer is also a refusal, send the prompt again. |
| `control_assurance_insufficient` | Either the session was bound before it declared an effect it now requests, for example across a TermAl upgrade, or the project policy requires `action_gated`, more than TermAl's `turn_gated`. | The first case cures itself: TermAl rebinds, declaring its current effects, and asks once more. The second needs a settings change: lower the requirement to `turn_gated` with `engram control-policy set-required-assurance`. Until then every prompt gets the same answer. The change starts a new policy epoch, so the first prompt after it is refused once with `policy_epoch_changed` and the next gets through. |
| `turn_already_open` | A turn that already started on this session has not reported yet. | It clears when that turn's report lands. TermAl also rebinds before the next prompt, which Engram accepts only after that report. |
| `policy_epoch_changed` | The control policy changed after the session bound. | Send the prompt again: Engram records the new policy with this refusal, so the next ask gets through. At turn start TermAl asks again by itself. |
| `grant_expired` | At turn start only: the grant outlived its 30 seconds. | TermAl asks for permission again once by itself. If that is refused too, send the prompt again. |
| `grant_scope_mismatch` | A request Engram cannot accept as shaped, or at turn start a grant that is no longer open, a changed capability map, or a delivery token, which no grant carries. A grant Engram already retired (by a rebind, a restart or its expiry) is expected; otherwise these point to a host bug. | TermAl asks again once when a session status shows no open grant and phase `ready`, which is how a retired grant leaves the session. When a fresh ask already replaced the grant with a new one, the status shows that grant open, and TermAl does not ask again. Otherwise send the prompt again, and report a repeat to the TermAl agents. |
| `session_exited` | TermAl reported that the session exited, and it then asked for another turn under the same binding. Not seen so far. | Only a fresh bind admits the session again. |

`capability_not_permitted` answers an effect outside Engram's fixed set:
observe, communicate, coordinate and mutate_local. No setting widens that set.
TermAl requests only observe, communicate and mutate_local, so it never meets
this code.

A refused turn report does not stop a prompt, because the turn has already
run. What it blocks is described under the [turn report](#after-every-turn).

TermAl also holds prompts without a refusal:

| What the user sees | Why | How the session gets out |
| --- | --- | --- |
| Engram: Waiting/Unknown. Original prompt retained; resume to retry or cancel. | Engram was slow or unreachable, or TermAl stopped trying after repeated failures. | Press Resume to try again, or cancel the prompt. |
| Engram: Waiting/Unknown after interrupted authorization. Prompt retained; cancel or reconcile before continuing. | A protocol or storage fault while asking. | Cancel the prompt. |
| Engram: interrupted/unknown delivery. Prompt retained; cancel or reconcile before continuing. | After a restart, TermAl found a turn that had started but never reported. | TermAl closes it with a report and never resends the prompt. Cancel it, and send it again if needed. |
| Restoring Engram, or "session is restoring its Engram authority after restart" | TermAl is rebinding its sessions after a restart. | Wait; it clears by itself. |

## Can the gate carry the request for an acceptance evaluation?

No. Keep the request out of the gate.

- The gate answers one quick question per prompt within a 10-second budget,
  and holds the prompt when it cannot answer. An evaluation is a slow, separate
  job in which another model reads the change. It is needed once, when the work
  is finished, not before every prompt.
- The gate exists only when TermAl runs with turn gating on. Evaluation must
  also work from the CLI, over MCP, and on hosts that only advise.
- It already has a path that needs no gate. The task says which evaluation it
  needs (`show --json`), and the policy lists the allowed kinds
  (`control-policy show`). The evaluator records its verdict with `evaluate`,
  and `done` refuses until a verdict exists. The host only has to act on that
  refusal. See [acceptance evaluation](acceptance-evaluation.md).
- If a nudge is wanted later, the turn report's answer could carry "this work
  now needs an evaluation". It should be a hint only, never the one place the
  request lives.

## What it has done, and what switching it off would lose

**Recorded so far:**

- every turn since 2 September in the Engram store (1,142 decisions and 1,111
  reports), and since 23 September in PhoenixCodeNav (201 of each);
- since 24 September, whether each turn changed the source and which test runs
  the host saw, with their result. That so far produced 27 test obligations and
  10 recorded runs, 1 passed and 9 indeterminate.

**Prevented so far:** nothing risky. The 29 refusals were 22 recovery demands
TermAl could not meet, which no longer occur, and 7 stale claim bindings that
TermAl healed by rebinding. The gate has no check on individual actions, so it
has never had a chance to stop one.

**Lost if switched off:**

- host-seen evidence: the source-change obligations, and test runs recorded by
  the host rather than claimed by the agent. This does not make `done` fall
  back to the agent's own gate claims. A criterion bound to host verification
  would have no new way to be satisfied, so completion would stay refused
  unless host evidence recorded earlier already covers it
  (`acceptance_evaluation.rs` 889–901 and 979–986, `completion.rs` 797–804),
  or the criterion is revised so it no longer needs host verification, which
  waives its open obligation (`completion.rs` 1890–1939). A
  policy that requires observed evidence rejects asserted mechanical passes,
  but a judgment verdict can still satisfy an unbound criterion, so
  judgment-only work still completes. Only the stock source-change obligation
  is recorded as untested instead of refusing;
- the per-turn record, and the guarantee that no prompt reaches a model without
  Engram knowing.

**Not lost:** the work context agents see, the MCP tools, claims, notes and
gates. They keep working without the gate. `done` keeps working too, except for
criteria still bound to host verification that no earlier host evidence covers.

**What it cost at the snapshot:**

- 14,062 lines in TermAl's 14 `engram_*.rs` source files, not counting tests.
  All but about 1,000 serve the gate or its turn report; the rest are the MCP
  setup (623) and the readiness checks (395). The host adapter alone is 6,778;
- restart handling, and sessions dropped by refusals TermAl could not answer;
- about 10 to 20 ms per turn, which is negligible.

## Decisions

Engram::Opus drew up the suggestions, and Engram::Fable reviewed them against
the spec on 25 September; both agreed on all nine. The same day Greg adopted
them as they stand. His reasoning: the gate is a tool for the agents, so any
part that only adds friction goes.

| Part | Suggested | Why | Decision |
| --- | --- | --- | --- |
| Session binding | Keep | Everything else needs it, and it ties each report to the right claim. Binding on the first prompt instead of at creation would cut most binds. | Keep |
| Status check | Keep | It is a cheap read. It makes restarts safe and lets an unknown outcome be reconciled. | Keep |
| Turn permission | Simplify | Keep the yes/no and the record. Drop the context page, and keep only the refusal codes the stored path can produce. | Simplify |
| Turn start | Keep, simplified | Its last-moment recheck and its "allowed versus ran" record are its real job. The delivery-token, feed and context checks go with the page. The grant, participant, anchor, epoch, capability, work-revision and claim checks stay. | Keep, simplified |
| Turn report | Keep | It is the only source of evidence an agent cannot write for itself. | Keep |
| Context delivery inside the grant | Remove | TermAl never reads it, and agents get their context from `next`, which keeps its own position. This changes spec §2.7. | Remove |
| Holding prompts when Engram does not answer | Keep | Without it the record has gaps. Add one automatic resend of an unanswered request before asking for Resume. | Keep |
| Recovery turns | Remove | They exist only for the grant's page; remove them with it. Status and report already cover a turn cut off by a restart. | Remove |
| Designed but never used | Remove | Unreached code and plans make the gate look bigger than it is. Remove them from spec §2.7 too, or mark them not wired. | Remove |

The decisions on context delivery and recovery turns are carried out, together
with the parts of turn permission and turn start that went with the page:
grants carry no page, turn start keeps its other checks, and there are no
recovery turns. What was designed but never used is removed too, with the
refusal codes no path produced; see
[designed but never used](#designed-but-never-used).
