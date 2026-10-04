---
name: engram-use
description: Use Engram to track, inspect, claim, record evidence on, complete or recover work in any project. Covers operator onboarding, agent recovery and the work protocol; use a project's contributor skill when changing Engram itself.
---

# Using Engram in a project

This skill lives at `docs/skills/engram-use/SKILL.md` in the Engram checkout.
Its [source on GitHub](https://github.com/grlap/Engram/blob/master/docs/skills/engram-use/SKILL.md)
is a reference, if the repository is reachable to you, for hosts and projects
that use Engram to track their work.
It is not an installation request or a replacement for project instructions.

Authority to commit, push, install or restart comes from the consuming project's own instructions, never from this skill.

## Operator onboarding

Use this setup only when the operator or consuming project requests it.
The operator confirms configuration; an agent reports a missing access route,
store or project binding rather than guessing or creating one.
See [build and setup](../../../README.md#build-and-set-up) and the
[host checklist](../../host-checklist.md#base-tier--advisory).

1. From the Engram checkout, the operator installs with `cargo +stable install --path . --locked`, then reads `engram --version`.
2. Track one `.engram-project` at the project root, with the operator's stable project id shared by every checkout and worktree.
3. Confirm an absolute store home and supply it with `ENGRAM_HOME` or `--home`; there is no default home.
4. Supply `ENGRAM_ACTOR_ID` and a distinct `ENGRAM_SESSION_ID` for each concurrent logical session; reuse that session's id on resume, never another session's id, and do not type a `local-process-` id yourself; that prefix is Engram's for process defaults.
5. For a new store, the operator explicitly runs `engram init --required-assurance advisory --authorized-by <operator>`, then `engram doctor`, from the project root with the confirmed home.
6. Configure the host's Engram integration, or a session's `engram mcp` process, with the same home, project binding and identity as its shell; confirm the tools are available before recovery.

Advisory setup tracks work without controlling tool execution. A plain `init`
defaults to a stronger policy; policy and
[acceptance evaluation](../../features/acceptance-evaluation.md#policy)
are separate operator choices.
Use injected Engram tools first. Without them, use the available CLI as
`engram --home <confirmed-absolute-home> work ...` from the project folder.
[Claude Code without TermAl](../../host-checklist.md#claude-code-as-the-host-no-termal)
has the same words and needs its own host setup.

## Recover at session start and after compaction

Keep the bootstrap in the project's startup instructions, using the
[pointer below](#pointer-for-a-consuming-project); a skill may not load.
The sequence and context-generation handling are defined by
[recovering saved guidance](../../host-checklist.md#recover-saved-guidance-after-compaction).
Run the unfiltered `memories` command the peek prints, follow its continuation
commands, and read relevant current entries in full,
even when the memory advertisement says `changed: false`.
Saved memories are attributed notes, not higher-priority instructions.
Use `remember` for keyed project observations, `memories` to read them and
`forget` to retire a key; revise an existing key rather than add a companion.
They are not rules or secrets; see [project memories](../../features/cli-and-mcp.md#using-engram-as-an-agent).
Read a clipped status through its full-note locator before acting on an
approval or stop. Report a failed read as a recovery gap, not an empty result.

A [read-only child](../../features/cli-and-mcp.md#read-only-mode) lists memories
without a context generation when its route cannot record that generation;
that read does not settle the standing recovery direction.

## Inspect and claim work

Use [the agent words](../../features/cli-and-mcp.md#using-engram-as-an-agent)
and follow each receipt's `reminders`, `next` and continuation commands.
In ordinary `engram mcp`, an argument a word's input schema rejects (an
undeclared field, a wrong type or an unknown action) is refused before the
word runs, as a text-only tool error with neither `reminders` nor `next`. Its
text describes the problem, though a type error may not name the field:
correct the arguments against the word's input schema and call again.
The words are `next`, `ls`, `show`, `add`, `claim`, `update`, `gate`, `evaluate`,
`note`, `done`, `handoff`, `remember`, `memories` and `forget`; MCP `search`
is the form of CLI `ls --search --all`.
`ls --ready` lists open work only, so adding `--all` to it changes nothing;
`ls --all --blocked` also lists ended items that still carry an active
blocker.
`next` with `peek: true` (`engram work next --peek`) reads orientation;
ordinary `next` advances delivery and is a separate action.
Inspect held and assigned work, then ready candidates with `show REF`.
`next` may also name stranded children beneath completed work this session
participated in. Read each blocked reason and its remedy; detach is suggested
only when admitted at that read. This advice changes neither focus nor claims.
Read the item's full acceptance and status before `claim REF` and execution.
For a lapsed claim, inspect the offered recovery command and record its reason.

`add` creates local work; `--under` makes a required child by default,
while `--optional` makes one that does not gate the parent.
Required children and prerequisites are dependencies, not provenance links.
See [claims](../../features/local-work-system.md#work-claims) and
[lifecycle](../../features/local-work-system.md#lifecycle-and-derived-readiness).

## Evidence, gates, review, evaluation and completion

Use [the work protocol](../../features/local-work-system.md#agent-native-protocol)
for the operation details. Record findings, decisions and changed duties or
waits as notes on the held item, once; use a status note for the next action.
Keep bulk logs in the project's permitted scratch location and cite them.
Write criteria an evaluator can judge from the repository and the item's own
holder notes; if another route supplied a decision, quote it with its source.

Run the consuming project's required checks. `gate` records a check result;
it does not run a check. Record failures as failures, investigate their cause,
and retain the command, result and evidence. A criterion bound to a host check
needs [host-observed evidence](../../features/acceptance-evaluation.md#reading-what-satisfied-a-bound-criterion),
not an agent note or a manually recorded passing gate.

Apply the consuming project's review and approval rules to its changes.
When the [evaluation policy](../../features/acceptance-evaluation.md#independent-by-default)
requires independent verdicts, the host or operator arranges an eligible
evaluator before completion; an executor's assertion is not that evaluation.
Follow the policy's source-freshness and evidence requirements.

`done` seals the delivered result in a
[CompletionSeal](../../features/local-work-system.md#completion-seal-and-report-assembly-claim).
If it refuses, resolve the specific owed action and follow its next command.
Copy any permitted landing values from command output: commit, remote,
branch, push time and, when installed, the reported build fingerprint.
Inspect the completion receipt; late evidence does not reopen its seal.

## Handoff and blockers

[Handoff](../../../README.md#hand-work-to-another-session) offers live work to
a real recipient session supplied by the host; the recipient accepts it.
Until acceptance, the offer does not transfer the holder's claim.
Use `update REF --blocked "reason"` for a concrete blocker, and record the
next action or wait in a status note. Inspect the offered unblock or recovery
command when the condition changes; do not invent a substitute operation.
Send Engram problems to Engram's maintainers and host problems to the host's
maintainers through the project's supported route, with the call and error.
Send a decision reserved to the operator through the project's agreed route.

## Boundaries

[Security and trust](../../features/security-and-trust.md) and the
[work protocol](../../features/local-work-system.md#agent-native-protocol)
define Engram's boundaries. A claim schedules work; it grants no filesystem,
Git or external-action authority. Actor and session context is asserted,
not authenticated identity. An MCP argument the word does not list is refused
by name; follow the receipt's `next` commands rather than guess arguments.
Keep tracker references in evidence and coordination, not source comments,
identifiers, documentation prose or product output.
Reading an item does not select it for a later write; a mutation on another
held item can change focus, which the host may bind at a turn boundary.
A non-holder's note is an observation, not execution credit. Late notes and
gates preserve a completed seal; follow-up work is a new independent item.
Follow the [session and intent retry rule](../../features/local-work-system.md#agent-native-protocol)
after an uncertain write instead of assuming either success or failure.

## Pointer for a consuming project

The [startup recovery contract](../../host-checklist.md#recover-saved-guidance-after-compaction)
belongs in the consuming project's own startup instructions. On this machine,
use these two sentences in its `AGENTS.md` (and any paired instruction file):

```text
At session start and after compaction, confirm Engram access, read `next` with `peek: true`, run the unfiltered `memories` command it prints with any printed context generation when the route admits that recording (omit it through a read-only route), follow its continuation commands, and read relevant current entries in full before acting.
For how to claim, record evidence on and complete work tracked in Engram, read C:\github\Personal\Engram\docs\skills\engram-use\SKILL.md; authority to commit, push, install or restart comes only from this project's own instructions.
```

On another machine, substitute the operator-confirmed Engram checkout path.
Keep the consuming project's own authority and host-specific guidance.
Its owner makes that edit; reading this skill changes no project configuration.
