# Turn gate assessment

> Normative reference: [spec §2.7](../spec.md#27-execution-control). Call
> sites, line references, history and the full explanations are in the
> [turn gate evidence](turn-gate-evidence.md). Related:
> [behavioral control plane](behavioral-control-plane.md),
> [acceptance evaluation](acceptance-evaluation.md) and the
> [host checklist](../host-checklist.md).

What the turn gate does with TermAl as its host, and what it is worth, on 25
September 2026, from the Engram store (since 2 September) and the
PhoenixCodeNav store (since 23 September). On 25 September the grant's context
page and recovery turns were removed; this page describes the gate after that.

## The questions TermAl asks

TermAl asks three questions around every agent turn: may this prompt go, it is
going now, and here is how it ended. It also binds each session once, and
checks the session's status before a rebind, after a restart, and when a turn
start is refused with `grant_scope_mismatch`. It asks nothing during a turn:
there is no check on individual tool calls.

| When | Question | A yes allows | A no blocks | Real example |
| --- | --- | --- | --- | --- |
| Once per session | **Bind** (`session_bind`): register this session on this claim. | Turns for that claim. The session is ready at once. | Nothing. A moved claim answers `stale_fence`, and TermAl rebinds once. | session-7284 bound `turn_gated`, effects observe, communicate and mutate_local. |
| Before a rebind, after a restart, after a refused start | **Status** (`session_status`): is anything left open? | Closing a turn a restart cut off, or asking again once Engram has retired the grant. | Nothing. It reads, and expires a grant that has timed out. | session-7284: `ready`, policy epoch 2, no open grant. |
| Before every prompt | **Permission** (`turn_evaluate`): may this prompt go? | A 30-second grant for the named effects and claim. | The prompt; see [refusals](#refusals-a-user-sees). | 2026-09-25 15:33:03 UTC: session-7284 granted observe, communicate and mutate_local. |
| As the prompt leaves | **Start** (`turn_begin`): it is going now. | The prompt, after a last recheck of the grant. | The prompt, or TermAl asks once more. | 15:33:03 UTC: session-7284's grant became begun. |
| After every turn | **Report** (`turn_checkpoint`): here is how it ended. | A record of the outcome, source changes and test runs the host saw. | A refused report on a started turn holds the session until a report lands. | 15:32:05 UTC: session-7284 reported success, source unchanged. |

## What TermAl calls, and what nothing calls

TermAl calls those five, and a few settings commands: `readiness`, `doctor`,
`control-session-inspect`, `authority revoke` and `control-policy show`.

| | Engram store | PhoenixCodeNav |
| --- | ---: | ---: |
| Sessions bound | 913 | 150 |
| Turn decisions (granted / refused) | 1,142 (1,113 / 29) | 201 (201 / 0) |
| Turns started / reported | 1,112 / 1,111 | 201 / 201 |

Status checks are not stored, so they have no count.

The main TermAl call sites, by function in its working tree on 25 September,
all in `engram_host_adapter.rs` unless another file is named:

- bind: `ensure_engram_session_bound_with_budget_off_lock` and
  `bind_engram_target_uncoordinated_off_lock`;
- status: `bind_engram_target_uncoordinated_off_lock`, and
  `restore_queued_engram_target` and `engram_issued_grant_was_retired` in
  `engram_queued_admission.rs`;
- permission: `evaluate_engram_turn_off_lock`, called from `turn_dispatch.rs`;
- start: `prepare_engram_turn_delivery_off_lock`, called from
  `deliver_turn_dispatch_now` in `api.rs`;
- report: `checkpoint_engram_turn_off_lock`, for turns that succeeded, failed,
  were cancelled or were stopped. After a TermAl restart,
  `bind_engram_target_uncoordinated_off_lock` and
  `restore_queued_engram_target` close a cut-off turn with a report.

No host calls the action checks (`action_authorize`, `action_begin`,
`action_complete`), `control_bootstrap`, `delivery_ack`, `session_heartbeat`
or `session_exit`, and Engram never answers "defer". Engram's host channel
(`src/host.rs`) accepts none of these operations, and TermAl's request type
(`EngramControlRequest`) has only the five above. Neither store has a table
for action grants, heartbeats or delivery acknowledgements. Spec §2.7 still
describes them.

## Refusals a user sees

A refused turn shows "Engram did not authorize this turn for runtime
delivery", drops the prompt and puts the session in Error. The control card
names the code after "Reason:".

| Code | Cause | How the session gets out |
| --- | --- | --- |
| `stale_fence` | The claim or work the session is bound to changed. | TermAl rebinds and asks once more. If refused again, send the prompt again. |
| `control_assurance_insufficient` | The session was bound before it declared an effect, or the policy requires `action_gated`. | The first cures itself by a rebind. The second needs `engram control-policy set-required-assurance` down to `turn_gated`; the next prompt is then refused once with `policy_epoch_changed`. |
| `turn_already_open` | A started turn has not reported yet. | It clears when that report lands. |
| `policy_epoch_changed` | The policy changed after the bind. | Send the prompt again. At turn start TermAl asks again itself. |
| `grant_expired` | The grant outlived its 30 seconds before the start. | TermAl asks again once. If refused, send the prompt again. |
| `grant_scope_mismatch` | A malformed request, or at the start a grant no longer open, a changed capability map or a delivery token. | TermAl asks again once when status shows no open grant and phase `ready`. Otherwise send again and report a repeat to the TermAl agents. |
| `session_exited` | A turn asked for after the session reported its exit. | Only a fresh bind. |

`recovery_required`, 22 of the 29 stored refusals, came from the grant's
context page. Once an Engram build without the page is installed, it no longer
occurs: a session left in `sync_required` is admitted as `ready`, so sending
the prompt again gets through. On the older build, retries got the same answer
until Engram could build a page that fit.

TermAl also holds a prompt without any refusal:

| What the user sees | Why | How the session gets out |
| --- | --- | --- |
| Engram: Waiting/Unknown | Engram was slow or unreachable, or TermAl paused asking after repeated failures. | Press Resume, or cancel the prompt. |
| Engram: Waiting/Unknown after interrupted authorization | A protocol or storage fault while asking. | There is no Resume: cancel the prompt. |
| Engram: interrupted/unknown delivery | After a restart, a turn that started but never reported. | TermAl closes it with a report and never resends it; cancel it, and send it again if needed. |
| Readiness: Restoring Engram, in the session tab's tooltip | TermAl is rebinding its sessions after a restart. | Wait; it clears by itself. |

## Can the gate carry the request for an acceptance evaluation?

No. The gate answers one quick question per prompt within 10 seconds, and
only when turn gating is on. An evaluation is slow, needed once at the end,
and must also work from the CLI and MCP. It has its own path: the evaluator
records a verdict with `evaluate`, and `done` refuses until one exists.

## What it has done, and what switching it off would lose

- **Recorded:** every turn, and since 24 September whether each turn changed
  the source and which test runs the host saw pass: 27 test obligations and
  10 host-seen runs. This is the only evidence an agent cannot write itself.
- **Prevented:** nothing risky. The 29 refusals were bookkeeping: 22 recovery
  demands TermAl could not meet and 7 stale claim bindings it healed.
- **Lost if switched off:** the host-seen evidence, the per-turn record, and
  the guarantee that no prompt reaches a model without Engram knowing. A
  criterion bound to host verification would have no new way to be
  satisfied; only host evidence recorded earlier, or revising the criterion,
  would let its work complete.
- **Not lost:** the work context agents see through `next`, claims, notes,
  gates, and `done` for every other criterion.
- **Cost:** about 13,000 of the 14,000 lines in TermAl's Engram host code,
  restart handling, and 10 to 20 ms a turn.

## Decisions

Greg adopted all nine suggestions on 25 September: the gate is a tool for the
agents, so any part that only adds friction goes.

| Part | Decision | Status |
| --- | --- | --- |
| Session binding | Keep | Kept |
| Status check | Keep | Kept |
| Turn permission | Simplify: no context page, only the refusals it can produce | Page removed; refusal codes still to cut |
| Turn start | Keep, simplified | Done |
| Turn report | Keep | Kept |
| Context delivery inside the grant | Remove | Done |
| Holding prompts when Engram does not answer | Keep, adding one automatic resend | The resend is TermAl's to add |
| Recovery turns | Remove | Done |
| Designed but never used | Remove | Still to do |
