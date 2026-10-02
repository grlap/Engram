# Action gates

> Normative reference: [spec §2.7](../spec.md#27-execution-control).
> Related briefs: [behavioral control plane](behavioral-control-plane.md),
> whose [planned interfaces](behavioral-control-plane.md#planned-interfaces)
> this brief designs, the [turn gate assessment](turn-gate-assessment.md)
> and its [evidence](turn-gate-evidence.md),
> [security & trust](security-and-trust.md), [CLI & MCP](cli-and-mcp.md) and
> the [host checklist](../host-checklist.md).
>
> Status: design. Nothing in this brief is built, and no implementation is
> scheduled: see [When to build it](#when-to-build-it).

Today a host asks Engram once per model turn: may this prompt go. Whatever
the agent then does inside the turn is decided by that one answer. An action
gate adds a second, narrower question: before one declared material action
runs, may this action run, and afterwards, what happened. `action_gated` is
the assurance level a host may claim when it asks that question for every
action in a declared set and can show that nothing in that set runs without
it.

## The honest value now

Evidence from this project's history, read on 2026-10-02. The incidents come
from the project's recorded notes; where an incident's source was not
verified beyond such a note, the table says so.

| What happened | Would an Engram grant per action have stopped it? |
| --- | --- |
| 2026-09-26: an agent's cleanup script failed to assign a variable and then deleted most of the operator's user profile | Not by itself. The tool call carried a script that named a variable, not a path, so a decision on that call had no resolved target to judge and may well have admitted it. What stops it is a boundary on what the agent's processes can write, or a typed delete tool that resolves its target before it acts. The operator decided that day that the host will not block dangerous operations, and that decision stands |
| 2026-10-01, twice: an agent edited a worktree while that worktree's gate was running | No. The deciding fact is that the tree is a frozen gate input. The host holds that fact; Engram does not |
| Build folders created in the main checkout changed its fingerprint and voided two gate runs (from a project note; not verified further) | No, for the same reason |
| 2026-09-27: a landing went ahead without one of the standing approval's conditions, readiness on one live store (from a project note) | Partly. This is the one class where recorded Engram state could decide. It is rare, not sensitive to latency, and better served by a check the lander runs than by interception |

The rows marked as from a project note are attributed reports, not an
independent reconstruction. On this evidence, none of these incidents is
shown to be one that a general Engram decision per action would have
prevented, and approving the text of a command cannot establish what the
command is confined to.

One measurement of the Engram store's turn decisions from 2 September to
2 October 2026, taken on a copy on 2026-10-02: 6,044 decisions, of which 30
were refusals: 22 demands of a mechanism since removed and 8 stale claim
bindings. A refused stale binding is a real control result; the counts
neither show nor rule out that the turn gate prevented harm, and they are
given only for scale.

So the case for a general action gate is thin. The recommendation of this
brief is to keep the contract below as the design of record and to build
nothing now.

## What it reuses and what it adds

| Reused as shipped | Added |
| --- | --- |
| Session binding, the connection generation and the routing token | A versioned mediation map of the host's tools |
| The turn grant: policy epoch, capability map revision, claim and fence | Three host operations that change state, `action_authorize`, `action_begin` and `action_complete`, and one read, `action_status` |
| Exact replay of an intent under its idempotency key | A durable action grant and its outcome |
| Canonical execution observations and the turn checkpoint | An outcome that can be reported after its turn has closed |
| Typed refusals that name a directive | Reconciliation of unknown outcomes before a dependent action or completion |

Not reused: resource leases, which were removed, and context packets, which
are not built. A material action never runs under a cached permission while
Engram cannot be reached; a second permission mechanism for it would be a
second place to be wrong.

Two things the shipped code does not yet give, which a build must add and
must not assume:

- Subject normalization is lexical. It validates and normalizes path
  segments and holds no operating-system handle, so it cannot bind a path
  to the object an action touches. That binding is the host's, as
  [Paths are bound where the action runs](#paths-are-bound-where-the-action-runs)
  says.
- Completion today refuses any reconciled action outcome, because outcomes
  are not linked to runs. Pending outcomes linked to a run, and a completion
  check over them, are new work.

The action grant and its outcome are durable rows the store does not have
today. Adding them changes the store format, so every live store moves
through migration export and import once when this is built. That is the
operator's decision at that time.

## The mediation map and the capability set

A host does not declare abstract capabilities. It declares its actual tools.
The **mediation map** is a versioned list with one entry per tool route the
agent can reach:

| An entry states | Meaning |
| --- | --- |
| Tool route | The tool as the agent calls it, and the runtime it belongs to |
| Effect class | `observe`, `communicate`, `mutate_local`, `mutate_shared`, `external_side_effect` or `lifecycle`, as the control plane defines them |
| Material | Whether an action grant is required for it |
| Interception | Whether the host sees each call before it runs and can refuse it |
| Request shape | What the host can know about a call before it runs: typed targets, or only text |
| Observable outcomes | What the host can know afterwards |
| Bypasses and containment | Known ways around the interception, and what bounds the tool's processes |

The **material set** is small and set by policy. It is defined by the
resolved effect, whatever route produced it:

- a file write, replace, delete or move whose resolved target lies outside
  the file-system bounds the operator approved for the session, or that
  removes a tree;
- a change to shared state outside the store, such as a Git push;
- an external side effect through a network connector or a publication
  adapter;
- an install or another change to the machine outside the project.

The approved bounds come from the host and the operator. A work claim
grants no authority to write, and neither does its named source root: the
root is where evidence is measured. It may narrow the bounds, never widen
them, and without approved bounds there is no default exemption. The bounds
name a concrete workspace by its identity, under one policy and map
revision, so that another worktree or the main checkout, which share the
project's logical paths, cannot stand in for it.

An edit by a typed tool inside the approved bounds stays under the turn
grant and makes no call. A route whose effect the host cannot resolve, such
as an unrestricted shell, does not fall into that exemption: while such a
route exists the host cannot claim `action_gated` at all, as the next
section says.

A tool the map does not list is refused only under an action-gated profile
that the operator enabled. Under `turn_gated` nothing changes.

## What a host may claim

`action_gated` is a claim about the declared material set, and it is true
only when every route to an effect in that set is closed or intercepted:
the typed tool, a shell, Git, a hook, a connector, and the child processes
of any of them. A route the host does not intercept is switched off or
confined by a boundary at execution. It is never counted as harmless
because its tool's name looks harmless, and it cannot be set aside as
outside the claim: as [security & trust](security-and-trust.md) says, no
deployment may claim action gating while an unmediated write, shell or
network path remains available. Three rules keep the claim honest:

- A general shell is one tool whose request is text. Permission to run a
  command is not knowledge of what the command will touch. While an agent
  has a shell, or a network path, that is not confined by something other
  than the host reading the command, the host is not `action_gated`.
- A pilot that gates typed file tools while a shell stays unrestricted is
  still `turn_gated`. It states its tool coverage and nothing more.
- A tool that writes to its own cache or store does so as part of that
  tool's trusted route. An agent command that merely mentions the tool
  earns no exception.

The claim is the host's assertion and is recorded as one. Engram checks a
binding against the map and the profile it is given. It cannot verify that
the host intercepts what the map says, and it never reports `action_gated`
as verified.

### TermAl today, and what it would have to close

These are facts TermAl's architect reported on 2026-10-02 from a reading of
TermAl's source and briefs. They describe one host on one day, and are not
a contract.

- The only decision point before execution is where the agent's own runtime
  chooses to ask the host. The host decides from the request it is shown.
  It contains nothing at execution: the runtime, not the host, starts the
  tool's processes, and there is no control of the network.
- What reaches the host differs by runtime:

  | Runtime | What asks the host before it runs |
  | --- | --- |
  | Claude | Shell, native edits and MCP calls, each with its full input. In an ordinary session the user's allow rules and hooks answer first, so only the read-only mode sends every tool to the host |
  | Codex | Nothing under TermAl's default approval policy; the host sees commands as reported events afterwards. No request covers an MCP call. Codex's own sandbox is the one boundary at the operating-system level, and it is the vendor's; its mode that confines writes to a workspace is not verified on Windows |
  | Kimi | Shell, edits and MCP calls, without their arguments; reads and network fetches do not ask. The host takes the arguments from streamed text and can detect a mismatch only afterwards |
  | Cursor, Gemini, OpenCode | The request carries no authenticated tool identity, so the host cannot judge what is asked. Which of their tools ask at all is not verified |

- A typed edit request from Claude carries an absolute path before it runs,
  so a path rule is decidable there. The files a shell command will write
  are not derivable from its text.
- A commit, a push or an install that an agent runs reaches the host only
  as a shell command. Typed Git events exist only for the host's own user
  interface.
- A process an agent starts in the background belongs to the runtime; the
  host has no handle on it.
- No measurement of a permission round trip was supplied. A turn admission
  that times out already parks a session until a person resumes it, and a
  check per action would multiply that exposure. There is no automatic
  repair: a refusal is a message the model reads.

To claim `action_gated` for a material set, TermAl would have to close all
of these: every runtime sends every tool in the set to the host before it
runs, with the tool's identity and its typed input; shells and the network
are confined at execution by a boundary of the operating system, or are
withheld; background processes are tracked to their end; a late answer
never parks a session; and the host, not the model, acts on a refusal. None
of that is proposed.

## The action grant

```text
action_authorize(routing_token, turn_grant, idempotency_key, action intent)
  → granted(action_grant) | refused(directive)
action_begin(routing_token, action_grant, resolved scope)
  → begun | refused(directive)
action_complete(routing_token, action_grant, outcome, observation)
  → recorded | refused(directive)
action_reconcile(routing_token, action_grant, idempotency_key, follows,
                 resolution)
  → recorded | refused(directive)
action_status(routing_token, action intent | session)
  → the stored decision and state of that intent, or the session's pending actions
```

Each operation is replayed exactly under its key: `authorize` under its
idempotency key, `begin` and `complete` under the grant's id with the
operation's name, and each `reconcile` under its own idempotency key. A
repeated `begin` or
`complete` returns the stored receipt and causes nothing: the host
dispatches at most once for a grant, and a second, different begin on the
same grant is refused. The states survive a restart and stay distinct: issued, begun,
and settled with its outcome. `action_status` is the read a host uses
after a lost reply or a restart, in place of guessing.

**Authorize.** The intent names the map entry, the exact invocation as a
fingerprint, and the resource subjects in the control plane's subject model.
For an external side effect it also cites the durable authority reference
the spec requires for an external action: bound to the work and run, the
effect, the payload's fingerprint and a validity window.
Engram checks that the turn is begun, that the session, connection, claim
fence, policy epoch and map revision are current, and that the entry is
material and permitted. A grant is single-use and short-lived, and it binds
all of those. The same intent under the same key returns the same answer; a
different intent under that key is a conflict.

**Begin.** The host calls it immediately before it dispatches. Engram
rechecks the same basis, and compares the scope the host actually resolved
with the scope that was authorized. The host records its own intent to
execute before the effect. A grant that was never begun expires and may be
asked for again. A granted begin is permission, not proof that the action
ran.

**Complete.** The host reports `succeeded`, `failed`, `partial` or `unknown`
with an execution observation, even when the model turn has failed or has
already been checkpointed. The outcome is recorded against the grant's
original basis, whatever the claim or the policy has become since.

`complete` is the first report for a begun grant, and the only one that
`complete` can make. It may come from a successor host session when the
session that began the action is gone: Engram checks that the reporter
holds a current private connection bound to the grant's project, keeps the
action's original actor and basis, and records the reporter beside them.
Reporting grants no execution and no claim authority. A grant that was
issued and never begun has nothing to report: `complete` and
`action_reconcile` are refused for it, and it expires.

An outcome may report a source change after its turn, and after the claim
was handed off. An action is grant-backed, so its outcome is not execution
observed without admission, where a superseded claim makes a record audit
only. While the grant's original run is open, the change accounts on that
run, under the obligation rule set frozen in the grant and against the
named root recorded in the grant's basis, as an admitted turn's change
does. What it borrows from the rule for
[execution observed without admission](behavioral-control-plane.md#5b-record-execution-observed-without-admission)
is the mechanics of a late report: the change accounts at the position
where it is appended, as a barrier that a later check must follow in
position and in completion time, not as a pin on the revision. On a
finished run the record is audit only and never rewrites sealed accounting.
A reconciliation accounts only a source change it newly reports; one that
repeats a change already reported for the grant is a repeat and opens
nothing.

**Reconcile.** An `unknown` report, and a `partial` report whose result is
not known, are not the last word, and `complete` cannot say more: it replays
its first receipt. `action_reconcile` is the write that says more, and only
for a begun action that already has a report and is still unresolved. It
names the grant and, as `follows`, the newest observation recorded for that
grant, and it carries its own idempotency key and a resolution. The key
alone cannot order two different reconciliations; `follows` does. In one
transaction Engram compares `follows` with the newest observation, appends
the new record linked to it, and changes the action's state. An exact
repeat under the same key returns its stored receipt before anything else
is checked, so a retry still succeeds after later reports and after
settlement. Another payload under that key is a conflict. A different
request whose `follows` is no longer the newest is refused, so of two
competing reconciliations one is recorded and the other is told to read the
state again. Nothing recorded earlier is rewritten, and a replay of the
original `complete` still returns the original receipt.

A resolution has one of two typed bases:

- `observed`: a new outcome and observation from the host. `succeeded`,
  `failed`, and `partial` with a known result settle the action. `unknown`
  records what was learned and settles nothing.
- `operator_decision`: an explicit, attributed decision by the operator to
  treat the action as settled although its effect stays unknown, with the
  decision, its reason and a durable reference. It records no result: the
  observed fact stays `unknown` beside the decision. It arrives only by the
  operator's route. On the host channel a resolution is always `observed`,
  so a host cannot make that choice by writing an operator's name into a
  report, and the rule that a host never waives an unresolved effect
  stands. The operator is asserted context, as everywhere.

Settlement is terminal: a settled action accepts no further reconciliation,
so it cannot be reopened as unknown or settled a second way. It lifts this
action's blocks on overlapping subjects and on completion, in the
transaction that records it, and nothing else: not another action's blocks,
and no source or test obligation. Like `complete`, a reconciliation is
recorded against the grant's original basis and may come from a successor
host session bound to the grant's project, with the reporter recorded
beside the original actor.

A process that an action started is not finished when the tool call
returns. While such a process is contained and its lifetime is tracked, the
action is pending until the process ends, not succeeded because it is
contained. Without containment and tracking the outcome is `unknown`.

## Paths are bound where the action runs

A path string, a working directory and a normalized subject are labels. They
say what was asked, not what will be touched. For a file action the host
resolves at the actuator:

- an existing target, and the parent of a target that does not yet exist,
  through the handle the action will use;
- both ends of a move;
- every descendant of a recursive operation.

It keeps that identity through the execution and refuses when a link or a
reparse point changed between authorization and use, or when the path is an
alias the project's path policy does not admit. `action_begin` carries the
resolved scope, and Engram refuses a scope that is not inside what was
authorized.

## Unresolved outcomes

- An action is settled when its resulting state is known and any repair it
  needs is done. A `succeeded` or `failed` outcome is settled. A `partial`
  outcome whose result is fully known is settled too.
- An `unknown` outcome, a `partial` one whose result is not known, a pending
  process, and a begun action with no report are unresolved.
- While an action is unresolved, no write may touch a subject that overlaps
  its scope, through any route and in any run: not through another tool,
  and not through an ordinary edit that would otherwise make no call. The
  host enforces that block on the pending subjects, or requires mediation
  for them. When the scope of the unresolved action is itself unknown, the
  whole capability is barred until it is resolved.
- Completion of the work is refused while one of its run's actions is
  unresolved. Engram derives that from the run's recorded actions; it does
  not rely on the host attesting an empty list.
- Reads, a turn in which the agent diagnoses what happened, and actions
  that are demonstrably independent go on.
- An unresolved action is never run again under its old grant, and never
  treated as failed. The host inspects the effect and reports it, with
  `complete` when the grant has no report yet and with `action_reconcile`
  after one, or the operator records a decision. Asking again under a new
  key or with a new deadline never clears an unresolved action and never
  goes around it.
- A change of claim, policy or map after a begin stops new grants. It does
  not reach back into an action that is already running.
- A session whose binding went stale while an action is unresolved, because
  the work was revised, the fence moved or the policy epoch changed, binds
  again the ordinary way; without that no turn could be admitted, the
  diagnosis turn included. The new binding settles nothing. The unresolved
  action keeps its original basis, its block on overlapping subjects and on
  completion follows the run and not the binding, and its outcome is still
  reported against that original basis. When the claim moved to another
  holder, the block is the new holder's to see and diagnose, and the host
  that began the action still reports its outcome.

## What fails closed

| Situation | Material action | Everything else |
| --- | --- | --- |
| Engram does not answer within the deadline | Not dispatched | Unaffected: it makes no call |
| The session, connection, fence, epoch or map revision is stale | Refused with the directive for that cause | As turn gating decides today |
| The resolved scope is outside the authorized scope | Refused | Unaffected |
| An unresolved outcome on an overlapping subject | Refused | A write on that subject is refused too; the rest is unaffected |
| A tool the map does not list, under an action-gated profile | Refused | Unaffected |

## Budget and repair

A gate that stalls ordinary work is worse than none. The budget below is a
set of new targets, to be measured before anything is switched on. There is
no evidence that anything shipped meets them:

- no call and no delay for an action that is not material;
- authorize and begin together within 20 ms at the 99th percentile on the
  host, measured end to end on the supported platforms;
- a hard deadline of 250 ms before dispatch, after which the action is not
  dispatched and the answer is an explicit refusal, never a hang;
- no routine prompt to a human.

Every refusal names, in a form a host can act on, the next safe step. That
is not a promise that every refusal clears by itself: a true denial stays
denied.

| Refusal | The next safe step |
| --- | --- |
| A changed policy epoch or map revision, an expired grant | The host asks once more with fresh authority, once, inside the same deadline, and only when the action has certainly not begun |
| A stale fence | The host binds again and asks once more only if the session still holds valid authority of its own. A claim that moved to another holder is never taken back or recovered just to retry: the action is refused and the agent is told who holds the work |
| No answer within the deadline | The action is not dispatched. Afterwards the host reads `action_status` for the exact intent only to settle its own record of what Engram stored: a grant that was issued and never begun expires unused. To run the action after all, the host asks again as a new intent under a new key, with a new deadline; it never dispatches on a guess |
| An unresolved outcome | The host inspects the effect and reports it: `complete` for a begun action with no report, `action_reconcile` after one. An effect it cannot learn stays unknown, and the refusal says what evidence or operator decision would settle it |
| A denial by the operator or the host's own policy | None. A denial stays a denial |
| A resolved scope outside the authorized scope | None. This is a real stop: the agent is told what was authorized and what the path resolved to |
| A tool that is not in the map | None. The operator adds the tool to the map or the action does not run |

A retry happens only before a begin and inside the deadline, or as an exact
replay of the protocol exchange. An execution is never repeated after an
unknown result. A new
binding never clears an unresolved effect, and the host never waives one and
never widens a path to get a grant.

Before activation a pilot reports the median, 95th and 99th percentile of
the added delay, the count of false refusals, the time each repair took and
the count of interruptions that needed a human, on representative work.

## Acceptance tests

- **Single use.** A grant is begun once: a second, different begin is
  refused, and a replay of the same begin returns the stored receipt. The
  same authorize intent under the same key returns the same grant; another
  intent under that key is a conflict.
- **Recheck at begin.** A claim handed off, a policy change and a map change
  between authorize and begin each refuse the begin with their own code, and
  the host's single automatic retry succeeds only when the action has not
  begun.
- **Bound path.** A link swapped between authorize and begin, a move whose
  destination resolves outside the authorized scope, and a recursive delete
  with a descendant outside it are each refused at begin.
- **Outcome after the turn.** An outcome reported after its turn was
  checkpointed is recorded against the original basis.
- **Unresolved outcome.** After a begun action with no outcome, a write on an
  overlapping subject is refused through the same tool, through another
  tool and as an ordinary edit, and completion of the work is refused; an
  independent action and a diagnosis turn are admitted; the action is not
  dispatched again under its old grant across a host restart. A `partial`
  outcome whose result is known does not block.
- **Lost replies and crashes.** A lost reply to `begin`, a crash just before
  and just after the launch, and a lost reply to `complete` each end with
  one dispatch at most, and `action_status` shows the stored state. A
  repeated `begin` or `complete` returns the stored receipt.
- **Begun, no report.** After a crash before the first report, and after a
  lost reply to a `begin` that was committed, `action_status` shows the
  grant begun. A successor host session reports the first `complete`, and
  the reporter is recorded beside the original actor. An `action_reconcile`
  for that grant is refused until a report exists. `complete` and
  `action_reconcile` for a grant that was issued and never begun are
  refused. A new authorize under a new key on an overlapping subject is
  refused while that action is unresolved.
- **Reconciliation.** After `complete` recorded `unknown`, an
  `action_reconcile` with an `observed`, settled outcome is appended and
  linked to that observation; the first observation is unchanged, a replay
  of the original `complete` still returns its original receipt, and this
  action's blocks on overlapping subjects and on completion lift in the
  transaction that records the reconciliation, while another unresolved
  action's blocks and every source and test obligation stay. A
  reconciliation that still says `unknown` lifts nothing.
- **Reconciliation, competing and replayed.** When the reply to a
  reconciliation is lost, `action_status` shows it recorded; the same
  request under the same key returns the stored receipt and records nothing
  more, also after later reports and after settlement; another payload
  under that key is a conflict. Of two different reconciliations with
  different keys that follow the same newest observation, one is recorded
  and the other is refused. A new reconciliation of a settled action is
  refused.
- **Operator decision.** An `operator_decision` settles an action whose
  observed outcome stays `unknown`, records the decision, its reason and
  its reference as the operator's, and leaves the observation unchanged.
  The host channel refuses a resolution of that basis.
- **Late source change.** An outcome that reports a source change after the
  claim was handed off accounts on the grant's original open run, under the
  grant's rule set, as a barrier at its own position; the same report on a
  finished run is audit only and leaves the seal unchanged; and a
  reconciliation that repeats a change already reported for the grant opens
  nothing.
- **Stale binding with a pending action.** With an action unresolved, the
  work is revised, the fence moves and the policy epoch changes, each in
  turn. The session binds again and a diagnosis turn is admitted; the block
  on overlapping subjects and on completion is still in force under the new
  binding; and the outcome, reported afterwards, is recorded against the
  action's original basis.
- **Deadline.** An authorize that does not return within the deadline is
  not dispatched. A status read afterwards finds the stored decision and
  dispatches nothing, an issued grant that was never begun expires, and
  asking again takes a new key and a new deadline. The one automatic retry
  after a changed epoch, a changed map revision or an expired grant happens
  only inside the original deadline.
- **Non-cooperative agent.** A test agent that calls a material tool
  directly, and one that replays a grant from another session, are stopped
  before anything executes; reporting `unknown` afterwards does not pass
  these two. A process that outlives its call is pending while tracked and
  `unknown` otherwise.
- **Honest claim.** A host whose map lists an unintercepted and unconfined
  shell, write route or network route cannot bind as `action_gated`,
  whatever else it gates; it binds as `turn_gated` and its status states
  its tool coverage.
- **No cost for the rest.** A turn with no material action makes exactly the
  calls it makes today.

## How this reconciles the earlier texts

| Earlier text | This design |
| --- | --- |
| Behavioral control plane, planned interfaces: `action_authorize`, `action_begin`, `action_complete` | Designed here, with `action_status` and `action_reconcile` added, `partial` added to the outcomes, and an outcome and its reconciliation that may arrive after the turn's checkpoint |
| Behavioral control plane, execution observed without admission: a reported source change accounts at its own position as a barrier, and a superseded claim makes the record audit only | A late action outcome borrows the barrier mechanics and differs in eligibility: it is grant-backed, so it accounts on its open original run also after a handoff, under the rule set frozen in the grant. A finished run stays audit only |
| Behavioral control plane, host integration contract, points 5 and 6 | Kept, with the mediation map as the thing a host declares |
| Spec §2.7 and the behavioral control plane's failure matrix: `degraded_open` inside a cached envelope for policy-designated reversible local work | Left as those texts state it for work that is not material. A material action never runs under a degraded envelope: with no answer it is not dispatched |
| Behavioral control plane, latency contract: under 10 ms for an uncached local allow, under 1 ms inside a live scoped grant | The 10 ms target stays for turn mediation. For actions the budget in this brief replaces the 1 ms figure: no call inside the turn grant, and 20 ms for authorize and begin together. That paragraph now says so |
| Behavioral control plane, delivery sequence, phase 4 | This brief is that phase's action mediator. It is deferred |
| Security & trust: no action-gated claim with an unmediated write, shell or network route | Kept, and made concrete by the three rules under "What a host may claim" |

## When to build it

Not now. An action gate is the host refusing an agent's action. The
operator decided on 2026-09-26 that the host will not block dangerous
operations. This design is therefore not scheduled, not enabled and not
presented as planned until the operator explicitly reverses that decision.

Two developments would be reasons to put the question to the operator
again. Neither is a trigger by itself:

- Engram gains its first external write adapter, such as publication. A
  durable intent, an idempotency key and an outcome that is never replayed
  blindly are then needed whatever the host intercepts. The designed
  [off-host backup](off-host-backup.md) already has that shape for its own
  writes: a pending-attempt record before each `put`, a receipt after it,
  and an unknown outcome resolved before anything else. It is an operator's
  command, not an agent's action, so it needs no gate.
- A host can show that every route to a declared material set is closed or
  intercepted, with the processes of its tools contained.

If it is ever scheduled, the work is in four parts: the protocol and its state
machine with crash and replay tests, including the store-format change; the
host's interception and containment with path-race tests; outcomes and
reconciliation joined to work completion; and a measured pilot on one
declared set that the operator opts into.

## Outside this design

Two things bear on the failures in the table above. Neither is an action
gate, and neither is proposed here.

- A boundary that confines an agent's processes to the repository and its
  worktrees. It would have stopped the deletion of 2026-09-26, because it
  acts on the resolved path at execution and not on the text of a command.
  The operator declined host blocking of dangerous operations on that day;
  this brief does not reopen that.
- A check that a lander runs before a push, which reads the recorded
  conditions of the standing approval and says which are missing. It is a
  tool for the agent, not interception, and it would be a separate small
  design.
