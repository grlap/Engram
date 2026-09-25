# Turn gate assessment

> Normative reference: [spec §2.7](../spec.md#27-execution-control).
> Related briefs: [behavioral control plane](behavioral-control-plane.md),
> [acceptance evaluation](acceptance-evaluation.md),
> [control session inspection](control-session-inspection.md),
> [host readiness](host-readiness.md), and the
> [host checklist](../host-checklist.md).

This is a snapshot, taken on 25 September 2026, of what the turn-gated control
channel does in practice with TermAl as its host. It lists the questions TermAl
asks, what each answer allows or blocks, what the live stores recorded, and
what switching the gate off would lose. The
[behavioral control plane](behavioral-control-plane.md) brief remains the design;
this page records how much of it is used.

Figures come from the two live stores, opened read-only on 25 September 2026
around 15:30 UTC: the Engram store (records from 2 September) and the
PhoenixCodeNav store (records from 23 September). Engram code references are
at commit 7c1ba5d. TermAl references are to its working tree on the same day
and will drift.

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

- **Answer:** a routing token and the effects Engram will mediate. Without it,
  no turn can be requested.
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

- **Answer:** the session's phase, cursors and any grant still open. TermAl
  uses it before rebinding and after a restart, to close a turn that was cut off.
- **Blocks:** nothing. It only reads, though it expires a grant that has timed
  out.
- **Use:** not stored, so there is no count. What it returns for session-7284
  is phase `ready`, confirmed cursor 353, policy epoch 2 and no open grant.
- **TermAl call sites:** `engram_host_adapter.rs` 5523–5580 and
  `engram_queued_admission.rs` 711–720.

### Before every prompt

**Turn permission (`turn_evaluate`).** "May this prompt go to the model?"

- **Yes:** a grant, valid for 30 seconds, naming the allowed effects and the
  claim it is bound to. It also carries a page of work changes for the agent
  (see [context delivery](#context-delivery-inside-the-grant)).
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
  requesting observe, communicate and mutate_local, bound to its claim,
  expiring 30 seconds later, with nothing new to deliver (cursor 353). On
  2026-09-23 at 00:57:55 UTC session-5132 was refused with
  `recovery_required`.
- **TermAl call sites:** `turn_dispatch.rs` 220 and 1456, which call
  `engram_host_adapter.rs` 6235–6251. The purpose is always "ordinary".

**Turn start (`turn_begin`).** "The prompt is leaving now, with the context you
gave me."

- **Yes:** right before the prompt goes, Engram checks the grant again (`control.rs`
  396–475, `control_runtime.rs` 842–865):
  - it has not expired, and its session phase and task are unchanged;
  - the session is still a participant on the task, and the task's control
    anchor still exists;
  - the policy epoch, the task's admission epoch, the capability map, the work
    revision and the claim fence are unchanged;
  - the delivery tokens match the page, and the task feed and context have not
    moved on since the page was built.

  It then marks the grant begun and records the page as delivered, but only
  tentatively; the turn report confirms it. TermAl hands the prompt to the
  model.
- **No:** one of those checks failed. A feed or context that moved on answers
  `delta_required` and returns the session to catching up. For an expired
  grant, a changed epoch, `delta_required` or `stale_fence` (after a rebind),
  and for `grant_scope_mismatch` when a session status shows Engram already
  retired the grant (no open grant, phase `sync_required`), TermAl asks for
  permission again, once. Any other refusal drops the prompt, as a refused
  permission does.
- **Use:** 1,112 starts in the Engram store, 201 in PhoenixCodeNav. The start
  is what separates "allowed" from "ran", and the counts show both gaps: 1,113
  turns were granted and 1,112 started, because one grant expired without
  starting. And 1,112 started but 1,111 reported, because one turn started and
  never reported. That second case is what the restart path exists for.
- **Example:** on 2026-09-25 at 15:33:03 UTC session-7284 began its turn. The
  grant became begun, with tentative cursor 353. The session was already in
  phase `turn_open`, which the permission sets when it issues the grant.
- **TermAl call sites:** `api.rs` 236–240, which calls
  `engram_host_adapter.rs` 3998–4115.

### After every turn

**Turn report (`turn_checkpoint`).** "The turn ended like this. Here is what I
saw."

- **Answer:** Engram records the outcome, whether the source changed, and any
  test run the host watched pass. A source change opens a "tests have not run"
  obligation on the claimed work. `done` then marks the change untested unless
  the host saw a passing run.
- **Blocks:** it confirms the page that turn start recorded. A refused report
  for a turn that never started leaves an issued grant, which the next rebind
  expires. A refused report for a turn that did start holds the session:
  Engram will not rebind it until a report succeeds (`control_runtime.rs`
  231–237).
- **Use:** 1,111 reports in the Engram store, 201 in PhoenixCodeNav. Since
  24 September they recorded 37 observations, 17 of them with changed source.
  Those opened 27 test obligations, 12 now resolved, and recorded 10 test runs
  the host saw. This is the only evidence in Engram that an agent cannot write
  for itself.
- **Example:** on 2026-09-25 at 15:32:05 UTC session-7284 reported a turn
  that succeeded without changing the source, at source revision
  `48b897c9…`, with next intent `wait`.
- **TermAl call sites:** `engram_host_adapter.rs` 3971–3996,
  `turn_lifecycle.rs` 835–842, 1857 and 2043–2052, and `engram_turn_checks.rs`.

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

Engram builds a page of work changes and context for every grant it can
(`control_runtime.rs` 553–617); the host cannot supply one. Turn start records
the page as delivered tentatively, and the turn report confirms it. TermAl
reads only the page's position and token
(`engram_host_adapter.rs` 680–697) and never shows the content to the agent.
The `<engram-work-context>` block agents see comes from a separate
`engram work next --peek` call (`engram_mcp_config.rs` 429–459,
`turn_dispatch.rs` 67–71), which does not depend on the gate.

The two deliveries keep separate positions. The grant's page advances the
control session's cursors. `next` advances the work session's project cursor,
through its own stage-then-confirm cycle. So removing the grant's page cannot
change what agents see. It is still a change to the design, not just dead code
removal. [Spec §2.7](../spec.md#27-execution-control) binds a grant to a context
packet and to exact, host-confirmed delivery positions. After the change,
`next` would be the only delivery, and §2.7 would stop describing the grant as
one.

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

A session that falls behind its task feed must catch up before an ordinary
turn. It can fall behind in several ways:

- a rebind or restart;
- a turn report that trailed other sessions' events;
- an issued grant that expired or was replaced;
- a refused turn start.

Engram answers `recovery_required` to an ordinary turn in two cases
(`control.rs` 821–846 and 1013–1021, `control_runtime.rs` 553–617):

- **The backlog is longer than one page.** Engram builds a partial page, and
  only a recovery turn may consume it, one page at a time.
- **Engram built no page at all while catching up.** That happens when the
  context packet is over its pinned budget or the page is over the size limit.

A third cause, the phase `recovery_open`, cannot happen, because storage never
writes that phase.

TermAl never sends a recovery turn. It always asks for an ordinary turn and has
no code for this refusal. So the prompt is dropped, the session goes to Error,
and every retry gets the same answer. Retries get through only when the state
changes so that Engram can build a single page that fits, for example when the
context packet shrinks under its budget. A partial backlog never clears
without a recovery turn.

22 of the 29 refusals were this. All came from one session on 23 September,
between 00:57 and 01:29 UTC, right after a stale-claim rebind. Its next turn
was granted at 02:20, with a page attached. The store does not show which
cause applied or what changed in between.

Recovery turns exist only because of the grant's page: every cause above is
about building or consuming it. Without the page there is nothing to catch up
on. The one real recovery case left, a turn that started and never reported,
is already handled by the status check and a report after the restart.

### Designed but never used

- Checks around individual actions: `action_authorize`, `action_begin` and
  `action_complete`. Also `control_bootstrap`, `delivery_ack`,
  `session_heartbeat` and `session_exit`. These are listed as
  [planned interfaces](behavioral-control-plane.md#planned-interfaces) and were
  never wired to the host channel. A pure action-start check exists in
  `control.rs` but nothing calls it.
- The "defer" answer. TermAl can read it; Engram never gives it.
- Session phases storage never writes: `unbound`, `checkpoint_required`,
  `recovery_open`, `handoff_pending`, `contribution_required` and
  `participant_ready`. Storage writes only `sync_required`, `ready`,
  `turn_open` and `exited`.
- Refusal codes the stored checks never produce:
  - `control_unavailable`, `store_corrupt`, `unknown_control_schema`,
    `control_policy_missing`, `action_outcome_unknown` and `missing_authority`.
    The stored input fixes the schema, health, policy, action outcome and
    authority.
  - `lease_required`, which is historical.
  - `resource_remapped`, which belongs to action checks.
  - `checkpoint_required` and `participant_not_ready`, whose phases are never
    written.
  - `task_admission_epoch_changed`. Nothing ever raises a task's admission
    epoch.
  - `task_unbound`, `task_access_denied` and `lifecycle_hold`. They check that
    the task's control anchor and the session's own row exist, which storage
    never deletes, or come from phases storage never writes.
  - `context_required`. Every state that would reach it is refused earlier
    for the page's budget.
- Turns with purpose "recovery". Engram supports them; no host sends one, so
  `turn_purpose_mismatch`, which answers only a recovery turn in the wrong
  phase, never occurs either.

[Spec §2.7](../spec.md#27-execution-control) still describes action grants, the
"defer" answer and recovery turns. No host runs any of them.

## Refusals a user can see in TermAl

A refused turn shows "Engram did not authorize this turn for runtime delivery",
drops the prompt and puts the session in Error. Its control card names the
code after "Reason:". The first table lists every code TermAl can meet from
the turn permission or turn start, and what gets the session out. The store
so far holds only `recovery_required` (22) and `stale_fence` (7).

| Reason on the card | Why | How the session gets out |
| --- | --- | --- |
| `stale_fence` | The claim or work the session is bound to changed: the claim lapsed, was released or taken over, or the work was revised. | TermAl rebinds to the current claim and asks once more, so the user rarely sees this. If the second answer is also a refusal, send the prompt again. |
| `control_assurance_insufficient` | Either the session was bound before it declared an effect it now requests, for example across a TermAl upgrade, or the project policy requires `action_gated`, more than TermAl's `turn_gated`. | The first case cures itself: TermAl rebinds, declaring its current effects, and asks once more. The second needs a settings change: lower the requirement to `turn_gated` with `engram control-policy set-required-assurance`. Until then every prompt gets the same answer. The change starts a new policy epoch, so the first prompt after it is refused once with `policy_epoch_changed` and the next gets through. |
| `turn_already_open` | A turn that already started on this session has not reported yet. | It clears when that turn's report lands. TermAl also rebinds before the next prompt, which Engram accepts only after that report. |
| `policy_epoch_changed` | The control policy changed after the session bound. | Send the prompt again: Engram records the new policy with this refusal, so the next ask gets through. At turn start TermAl asks again by itself. |
| `grant_expired`, `delta_required` | At turn start only: the grant outlived its 30 seconds, or the task feed or context moved on after the grant was issued. | TermAl asks for permission again once by itself. If that is refused too, send the prompt again. |
| `pinned_budget_exceeded` | The context Engram builds for the grant is over its pinned budget. This shows only while the session is `ready`. While it is `sync_required` (listed below the table) the same problem shows as `recovery_required`. | Sending again gets the same answer until the context shrinks under the budget. |
| `delivery_invalid` | When asking: the grant's page is over its size limit. Like `pinned_budget_exceeded`, this shows only while the session is `ready`; while it is `sync_required` it shows as `recovery_required`. At turn start: the delivery tokens TermAl sent do not match the grant, a host bug. | When asking, it clears only when the context shrinks enough for the page to fit. At turn start, send the prompt again, and report a repeat to the TermAl agents. |
| `grant_scope_mismatch` | A request Engram cannot accept as shaped, or at turn start a grant that is no longer open or no longer matches the session's capability map. A grant Engram already retired (by a rebind, a restart or its expiry) is expected; otherwise both point to a host bug. | TermAl asks again once when a session status shows no open grant and phase `sync_required`. A grant a fresh ask superseded can leave the session `ready`, and then it is not asked again. Otherwise send the prompt again, and report a repeat to the TermAl agents. |
| `session_exited` | TermAl reported that the session exited, and it then asked for another turn under the same binding. Not seen so far. | Only a fresh bind admits the session again. |
| `recovery_required` | The session fell behind its task feed (after a rebind, for example), and either the backlog is longer than one page or Engram could build no page within its budgets. | Engram expects a recovery turn, which TermAl cannot send. A backlog longer than one page never clears this way. The budget case clears only when the context shrinks enough for a page to fit. |

For the `pinned_budget_exceeded` and `delivery_invalid` rows, a session is
`sync_required` after any of these:

- a bind;
- a restart or expiry that retired an unused grant;
- a fresh ask that replaced an unused grant while the session was behind its
  task feed;
- a report that found other sessions' events;
- most refused turn starts.

`capability_not_permitted` answers an effect outside Engram's fixed set:
observe, communicate, coordinate and mutate_local. No setting widens that set.
TermAl requests only observe, communicate and mutate_local, so it never meets
this code.

A refused turn report does not stop a prompt, because the turn has already
run. What it blocks is described under the [turn report](#after-every-turn).

TermAl also holds prompts without a refusal:

| What the user sees | Why | How the session gets out |
| --- | --- | --- |
| Engram: Waiting/Unknown. Original prompt retained; resume to retry or cancel. | Engram was slow or unreachable, TermAl stopped trying after repeated failures, or Engram deferred. | Press Resume to try again, or cancel the prompt. |
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
  the host saw pass. That so far produced 27 test obligations and 10 recorded
  runs.

**Prevented so far:** nothing risky. The 29 refusals were 22 recovery demands
TermAl could not meet and 7 stale claim bindings that TermAl healed by
rebinding. The gate has no check on individual actions, so it has never had a
chance to stop one.

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

**What it costs today:**

- 14,062 lines in TermAl's 14 `engram_*.rs` source files, not counting tests.
  All but about 1,000 serve the gate or its turn report; the rest are the MCP
  setup (623) and the readiness checks (395). The host adapter alone is 6,778;
- restart handling, and sessions dropped by refusals TermAl cannot answer;
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
