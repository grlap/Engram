---
name: team-coordination
description: Coordinate a multi-session engineering team when work must continue through gates, handoffs, and resource limits. Use for dispatch, merger queues, stalled lanes, or throughput recovery; not for ordinary single-session coding.
---

# Team coordination

Treat a coordinator as a scheduler and recovery owner. It must not become a
serial approval point. Scheduling the serial gate and the landing windows
stays its job; that is scheduling, not approval.

## Keep work continuous

Every active lane has exactly one current state recorded in durable host state:

- a runnable next action, with its owner; or
- a concrete blocker and a machine-checkable release condition.

Mailbox prose, a remembered plan, or a coordinator's intention is not a
durable lane state. When a lane finishes a turn with an executable next action,
the host must re-wake it when its release condition holds. Do not require a
human status request or a coordinator's manual "go".

If an owner becomes idle with an unapplied fix or another executable action,
first recover the lane. Then record a product follow-up for the missing
dispatch obligation; a one-off wake is containment, not a fix.

## Use capacity deliberately

Separate source concurrency from build concurrency:

- Dispatch independent coding, failing-test (RED) design, carry preparation
  and documentation work in separate worktrees whenever their scopes do not
  overlap. Reviewers are read-only leaves on the frozen input and need no
  worktree of their own.
- Respect the declared build/backpressure limits. Do not infer that an idle
  source worker can start a build.
- A lane waiting for a build slot continues useful non-build work when one is
  available: inspect a review finding, prepare a carry, write the exact test
  plan, or make the next patch on an input that is not frozen. While the
  lane's own input is in its gate or review, it does only non-mutating
  preparation of the next item: no worktree or index edits and no change of
  claim or focus. Mutating work goes to another session with its own claim
  and worktree.
- If many items are ready but builds are serialized, state that the build cap
  is the bottleneck. Do not describe this as a lack of work or worker capacity.

At scheduling boundaries, publish a compact map: **active** (owner and action),
**blocked** (release condition), **candidates** (carry/merge order), and the
next gate. Run this scan without waiting for the user to request status.

## Run a merge queue

Keep two or three independent candidates moving in parallel when the backlog
allows it. Parallelize implementation, tests, review and carry preparation.
Carry candidates onto the current mainline one at a time, then freeze and
validate the exact combined tree. A completed review or gate applies only to
the frozen tree it names, except that a clean review round may be adopted
across a base-only carry as `/review-changes` section 6a allows; a gate never
carries. Re-carry after mainline movement when required.

Choose candidates with non-overlapping owned paths where possible. When their
surfaces overlap, serialize the implementation or explicitly give one owner
the shared boundary. The Merger owns queue order and conflict resolution; it
does not replace the required project validation or reviews.

### Keep the Merger non-authoring

The Merger only receives worker-delivered patches, resolves actual merge
conflicts, carries candidates onto the selected mainline, freezes the tree,
coordinates validation and reviews, and lands a result that meets the
project's landing conditions (in Engram, the Authority and Git section of
AGENTS.md). It never
edits product code, tests, fixtures, documentation, or configuration as a
fix, even for a one-line correction.

When a merge, gate, review, or landing reveals a defect, the Merger
classifies each failure, records the gate as the project requires, and
reports the classification, the exact failure, the affected paths and the
frozen input to the owning implementer or the coordinator. A
claimed implementation worker fixes it in its own worktree and returns patch
and evidence. The Merger resumes only from that worker-delivered result.

Batch two or three path-disjoint candidates when the project can attribute
validation and acceptance to the whole frozen batch. If the host or tracker
can only credit one item per run, keep the carry preparation parallel but do
not pretend that one gate proves every candidate; file the attribution gap.

## Match implementation work to the model

Use the project's designated strong coding model for product, test, fixture,
documentation, and configuration edits. Do not assign those edits to a
lightweight or medium-reasoning session merely because it is idle. Such a
session may perform bounded read-only inspection, coordination, evidence
collection, or merger duties.

Before transferring a repair after a role or model concern, verify the target
session's effective model and reasoning level from the host. Preserve an
earlier lightweight draft only as evidence; the replacement worker derives
and tests its own patch from the frozen input and failure evidence.

## Centralize test execution

Use the host's supported runner for gates and focused checks. Implementers
should receive pass/fail metadata and only the bounded diagnostics needed to
fix a failure, rather than routine full logs. Keep full logs as runner
artifacts for the coordinator and reviewer when inspection is necessary.

## Use architects sparingly

Ask an architect only when a change crosses an architecture boundary, changes
durable meaning or format, or leaves a real design conflict unresolved.
Routine decomposition, local implementation choices, and bounded repair
decisions belong to the coordinator and implementer so coding capacity stays
available.

## Observe before reporting

Do not report a last mailbox update as a current execution state. Before
claiming that a lane is active, inspect the host's current session/work status.
Classify an apparent idle lane as one of: completed, running a detached
operation, resource-blocked, awaiting an external decision, or dispatchable.
Wake every dispatchable lane immediately.

When a host's projection disagrees with observed session state, report the
discrepancy with session id, owner, last durable next action and time. Do not
paper over it with a status summary.

## Preserve authority boundaries

Questions for the user belong in design when a choice genuinely changes scope.
Where authority permits, continue reversible work under the recorded default;
acts reserved to the user still wait for the user's word. Do not let an
unanswered question block unrelated work. Keep explicit user pauses, build
caps, installation/restart rules and project-specific landing gates intact.

For cross-host work, keep the control plane and task-state model separate:
TermAl can centralize mailbox/control delivery, while Engram task text moves
through the selected Git task-text exchange (planned; today only its
authorized pilot). Do not silently turn either into replicated SQLite or an
automatic merge service.
